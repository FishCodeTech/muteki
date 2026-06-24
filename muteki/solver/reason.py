"""Reason phase — global planner + anti-hallucination evidence audit (P-C).

The reason phase does two jobs. First, optimistic planning: it reads the graph
and proposes non-overlapping intents for the swarm to claim. Second — and the
part that matters most here — an EVIDENCE AUDIT: it scans candidate
(verified=false) evidence and refuses to build key intents on unverified facts.
This moves "verify only when refuting" forward to "question while planning".

Form:
- runs on a CHEAP model (flash); the expensive model runs explore/solve.
- triggered when the shared graph's fact/dead-end count changes (not every step).
- emits typed Intents to the shared graph; a scheduler/solver claims them.

This module is intentionally LLM-agnostic and side-effect-light so it's unit-
testable with a ScriptedLLM (no API key needed).
"""

from __future__ import annotations

import hashlib
import json
import re
from dataclasses import dataclass, field
from typing import Any, Optional

from muteki.models.solve_graph import SolveGraph


@dataclass
class Intent:
    """A claimable, typed task (PCSG-lite intent)."""
    intent_id: str
    goal: str
    worker_class: str = "code"           # code | shell_agent | verifier | review
    depends_on: list[str] = field(default_factory=list)
    rationale: str = ""
    from_facts: list[int] = field(default_factory=list)
    route_hash: str = ""
    branch_id: str = ""
    lane_key: str = ""
    risk_class: str = ""
    resource_key: str = ""
    dup_of: str = ""
    reopen_because: str = ""

    def to_payload(self) -> dict:
        return {"worker_class": self.worker_class, "depends_on": self.depends_on,
                "rationale": self.rationale, "route_hash": self.route_hash,
                "branch_id": self.branch_id, "lane_key": self.lane_key,
                "risk_class": self.risk_class, "resource_key": self.resource_key,
                "dup_of": self.dup_of, "reopen_because": self.reopen_because}


# Reason's verdict — a state-machine decision, not just a bool. The solver acts on
# this: `complete` → force conclude/extract now;
# `course_correct` → the run drifted, steer to a new direction; `explore` → keep
# going on the proposed intents.
VERDICT_COMPLETE = "complete"
VERDICT_COURSE_CORRECT = "course_correct"
VERDICT_EXPLORE = "explore"
_VALID_VERDICTS = (VERDICT_COMPLETE, VERDICT_COURSE_CORRECT, VERDICT_EXPLORE)


@dataclass
class ReasonResult:
    goal_met: bool
    intents: list[Intent]
    audit_notes: list[str]               # facts flagged as needing re-verification
    verdict: str = VERDICT_EXPLORE       # complete | course_correct | explore
    drift: str = ""                      # if course_correct: what went wrong + the fix
    complete_why: str = ""               # if complete: why the goal is already met
    semantic_dedupe_available: bool = False
    pinned_facts: list[int] = field(default_factory=list)


REASON_SYSTEM = """You are the REASON phase of an autonomous CTF-solving swarm. \
You do NOT execute — you read the shared solve-graph and DECIDE the swarm's next \
move. Become an expert in whatever domain this challenge is in, judge the state \
honestly, and output STRICT JSON.

First decide a `verdict` (the most important field):
- "complete": the Goal is ALREADY satisfied by a CONFIRMED (verified) fact in the
  graph — e.g. a real flag has appeared in actual execution output. Only choose
  this when it is genuinely done; do not declare victory on a guess.
- "course_correct": the run has DRIFTED — solvers are repeating, stuck on a dead
  angle, or chasing unverified assumptions, and the current intents won't reach the
  Goal. Say what went wrong and propose a corrected direction.
- "explore": still making progress; propose the next high-value directions.

Output JSON:
{
  "verdict": "explore",
  "goal_met": false,
  "complete_why": "<only if verdict=complete: why the goal is already proven>",
  "drift": "<only if verdict=course_correct: what's going wrong + the correct direction>",
	  "intents": [
	    {"id": "I1", "from": [3, 7], "goal": "<one concrete, independent next direction>",
	     "worker_class": "code", "route_hash": "web:login:sqli", "branch_id": "",
	     "lane_key": "", "risk_class": "",
	     "depends_on": [], "rationale": "<why>", "dup_of": null,
	     "reopen_because": ""}
	  ],
	  "pinned_facts": [3, 7],
	  "audit": ["<fact text you do NOT trust and why>"]
	}

Rules:
- Each intent MUST include a "from" array of fact sequence numbers (the [#N] tags
  in the evidence list) that motivated this direction. Use the exact numbers.
- Propose at most {max_intents} INDEPENDENT, NON-OVERLAPPING intents (distinct
  directions, not minor variations of one). Each should be a clear high-value
  direction — focus on the core insight, do not over-specify the steps; trust the
  executor to be the expert.
- If a proposed intent is the same direction as an existing open/claimed/attempted
  intent shown in the graph, set dup_of to that existing intent id. Only leave
  dup_of null for genuinely new directions. Set reopen_because only when new
  verified evidence materially changes an attempted route.
- worker_class is "code" by default. Use "shell_agent" ONLY for a long-chain task
  a single code call can't do. Use "verifier" for a narrow proof task. Use
  "review" only when the swarm needs arbitration: repeated route loops, conflicting
  assumptions, challenged facts, or ignored dead-ends.
- If you know the semantic route, include route_hash as category:surface:technique
  (for example web:login:sqli, web:jwt:forge, web:upload:svg-parser).
- For destructive or exclusive work (remote RCE exploit, service-crashing PoC,
  reverse-shell listener, relay/responder, or an exclusive shell session), include
  lane_key and risk_class. lane_key is resource-only:
  risk_class:transport:port@host, such as destructive:tcp:445@172.22.11.45.
  Do NOT include the exploit technique in lane_key.
- Facts under "Candidates / needs verification" are UNVERIFIED: do NOT build a key
  intent that ASSUMES such a fact is true. If an intent needs it, make the intent
  VERIFY it first, and list the fact text in "audit".
- The "Fact retention index" is for retention judgment. Put fact seqs in
  `pinned_facts` only when the fact is semantically reusable later (credentials,
  non-English clues, topology constraints, exploit preconditions, scope constraints,
  or durable discoveries). Do NOT pin routine host:port strings, URLs, headers, or
  generic key:value text unless the surrounding meaning makes it important.
- The graph may carry "Open intents (directions in flight)" and "Already attempted
  (concluded intents)" sections. Do NOT propose an intent that is the SAME
  DIRECTION as any entry there — a reworded/paraphrased goal is still the same
  direction. Re-open an attempted direction ONLY when NEW verified evidence
  materially changes it (name that fact in "rationale"). If every direction you
  can think of is already listed, output an EMPTY "intents" array (or verdict
  "course_correct" with a genuinely different angle) — never re-word old goals.
- If the graph carries a "Flags already captured" section, NEVER propose an intent
  to re-recover a flag listed there — that direction is DONE. Propose intents only
  for flags NOT yet captured (or other goal-advancing evidence).
- Reflect before proposing: if the Goal is not reached, ask WHY, whether the run
  drifted, and whether a course-correction beats proposing yet more intents.
- Respect Review directives, Challenged facts, Suppressed routes, and Open branches:
  do not rely on a challenged fact except in verifier work; do not propose a
  suppressed route unless new evidence/review reopened it; keep incompatible branch
  assumptions separated with branch_id.
- Preserve execution topology. Do not assume the operator's Mac, the public VPS,
  the entry host, and internal pivot hosts can reach the same networks. If the graph
  or operator standing guidance does not prove where a command must run from, create
  a verifier intent to establish the execution site/network path before planning
  lateral movement.
- Output ONLY the JSON object, nothing else."""


# Pentest variant (BE-pentest-mode): the SAME planner, but completion is judged
# against the operator's ENGAGEMENT GOAL (verified findings), not a flag. Used only
# when challenge.mode == "pentest"; the CTF path keeps REASON_SYSTEM byte-identical.
REASON_SYSTEM_PENTEST = """You are the REASON phase of an autonomous \
penetration-testing / security-audit swarm. You do NOT execute — you read the \
shared findings-graph and DECIDE the swarm's next move. Become an expert in this \
target's stack, judge the state honestly, and output STRICT JSON.

First decide a `verdict` (the most important field):
- "complete": the ENGAGEMENT GOAL is ALREADY satisfied by CONFIRMED (verified)
  findings in the graph — the required vulnerabilities are proven with real
  evidence from actual execution. Only choose this when it is genuinely done; do
  NOT declare victory on an unverified claim.
- "course_correct": the run has DRIFTED — workers are repeating, stuck on a dead
  angle, or chasing unverified assumptions, and the current intents won't reach the
  goal. Say what went wrong and propose a corrected direction.
- "explore": still making progress; propose the next high-value directions.

Output JSON:
{
  "verdict": "explore",
  "goal_met": false,
  "complete_why": "<only if verdict=complete: why the engagement goal is proven>",
  "drift": "<only if verdict=course_correct: what's going wrong + the correct direction>",
	  "intents": [
	    {"id": "I1", "from": [3, 7], "goal": "<one concrete, independent next direction>",
	     "worker_class": "code", "route_hash": "web:login:sqli", "branch_id": "",
	     "lane_key": "", "risk_class": "",
	     "depends_on": [], "rationale": "<why>", "dup_of": null,
	     "reopen_because": ""}
	  ],
	  "pinned_facts": [3, 7],
	  "audit": ["<finding text you do NOT trust and why>"]
	}

Rules:
- Each intent MUST include a "from" array of fact sequence numbers (the [#N] tags
  in the evidence list) that motivated this direction. Use the exact numbers.
- Propose at most {max_intents} INDEPENDENT, NON-OVERLAPPING intents (distinct
  directions, not minor variations of one). Each should be a clear high-value
  direction — focus on the core insight, do not over-specify the steps; trust the
  executor to be the expert.
- If a proposed intent is the same direction as an existing open/claimed/attempted
  intent shown in the graph, set dup_of to that existing intent id. Only leave
  dup_of null for genuinely new directions. Set reopen_because only when new
  verified evidence materially changes an attempted route.
- worker_class is "code" by default. Use "shell_agent" ONLY for a long-chain task
  a single code call can't do. Use "verifier" for a narrow proof task. Use
  "review" only when the swarm needs arbitration: repeated route loops, conflicting
  assumptions, challenged findings, or ignored dead-ends.
- If you know the semantic route, include route_hash as category:surface:technique
  (for example web:login:sqli, web:jwt:forge, cloud:iam:privilege).
- For destructive or exclusive work (remote RCE exploit, service-crashing PoC,
  reverse-shell listener, relay/responder, or an exclusive shell session), include
  lane_key and risk_class. lane_key is resource-only:
  risk_class:transport:port@host, such as destructive:tcp:445@172.22.11.45.
  Do NOT include the exploit technique in lane_key.
- Findings under "Candidates / needs verification" are UNVERIFIED: do NOT report a
  vulnerability as proven on such a fact. If an intent needs it, make the intent
  VERIFY it first, and list the fact text in "audit".
- The "Fact retention index" is for retention judgment. Put fact seqs in
  `pinned_facts` only when the finding/fact is semantically reusable later
  (credentials, non-English clues, topology constraints, exploit preconditions,
  scope constraints, or durable discoveries). Do NOT pin routine host:port strings,
  URLs, headers, or generic key:value text unless the surrounding meaning makes it
  important.
- The graph may carry "Open intents (directions in flight)" and "Already attempted
  (concluded intents)" sections. Do NOT propose an intent that is the SAME
  DIRECTION as any entry there — a reworded/paraphrased goal is still the same
  direction. Re-open an attempted direction ONLY when NEW verified evidence
  materially changes it (name that fact in "rationale"). If every direction you
  can think of is already listed, output an EMPTY "intents" array (or verdict
  "course_correct" with a genuinely different angle) — never re-word old goals.
- If the graph carries a "Flags already captured" section, NEVER propose an intent
  to re-recover a flag listed there — that direction is DONE. Propose intents only
  for flags NOT yet captured (or other goal-advancing evidence).
- Stay within the engagement scope; do not propose out-of-scope actions.
- Respect Review directives, Challenged facts, Suppressed routes, and Open branches:
  do not rely on a challenged finding except in verifier work; do not propose a
  suppressed route unless new evidence/review reopened it; keep incompatible branch
  assumptions separated with branch_id.
- Preserve execution topology. Do not assume the operator's Mac, the public VPS,
  the entry host, and internal pivot hosts can reach the same networks. If the graph
  or operator standing guidance does not prove where a command must run from, create
  a verifier intent to establish the execution site/network path before planning
  lateral movement.
- Output ONLY the JSON object, nothing else."""


def build_reason_prompt(summary: str, max_intents: int = 4, *,
                        fact_index: str = "",
                        goal: Optional[str] = None, mode: str = "ctf") -> list[dict]:
    retention = ""
    if (fact_index or "").strip():
        idx = fact_index.strip()
        if "Fact retention index" not in idx:
            idx = "## Fact retention index (model decides pinned_facts)\n" + idx
        retention = f"\n\n{idx}"
    # pentest → goal-driven planner (the operator's engagement goal anchors the
    # `complete` verdict). CTF (default) keeps the original prompt byte-for-byte.
    if mode == "pentest":
        user = (f"Engagement goal:\n{goal}\n\n" if (goal or "").strip() else "")
        user += (f"Shared findings-graph:\n\n{summary}{retention}\n\n"
                 "Output the planning JSON.")
        return [
            {"role": "system",
             "content": REASON_SYSTEM_PENTEST.replace("{max_intents}", str(max_intents))},
            {"role": "user", "content": user},
        ]
    return [
        {"role": "system",
         "content": REASON_SYSTEM.replace("{max_intents}", str(max_intents))},
        {"role": "user", "content": f"Shared solve-graph:\n\n{summary}{retention}\n\n"
                                     "Output the planning JSON."},
    ]


def _extract_json(text: str) -> dict:
    """Pull the first JSON object out of a model reply (robust to prose/fences)."""
    if not text:
        return {}
    # strip ```json fences
    text = re.sub(r"```(?:json)?", "", text)
    m = re.search(r"\{.*\}", text, re.DOTALL)
    if not m:
        return {}
    try:
        return json.loads(m.group(0))
    except json.JSONDecodeError:
        return {}


def parse_reason_reply(text: str, *, max_intents: int = 4) -> ReasonResult:
    d = _extract_json(text)
    goal_met = bool(d.get("goal_met", False))
    intents: list[Intent] = []
    for i, raw in enumerate(d.get("intents", [])[:max_intents]):
        if not isinstance(raw, dict):
            continue
        goal = str(raw.get("goal", "")).strip()
        if not goal:
            continue
        wc = str(raw.get("worker_class", "code"))
        if wc not in ("code", "shell_agent", "verifier", "review"):
            wc = "code"
        from_raw = raw.get("from", [])
        from_facts = [int(x) for x in from_raw if isinstance(x, (int, float))]
        intents.append(Intent(
            intent_id=str(raw.get("id") or f"I{i+1}"),
            goal=goal, worker_class=wc,
            depends_on=[str(x) for x in raw.get("depends_on", []) if x],
            rationale=str(raw.get("rationale", "")),
            from_facts=from_facts,
            route_hash=str(raw.get("route_hash") or "").strip(),
            branch_id=str(raw.get("branch_id") or "").strip(),
            lane_key=str(raw.get("lane_key") or "").strip(),
            risk_class=str(raw.get("risk_class") or "").strip(),
            resource_key=str(raw.get("resource_key") or "").strip(),
            dup_of=str(raw.get("dup_of") or "").strip(),
            reopen_because=str(raw.get("reopen_because") or "").strip(),
        ))
    audit = [str(a) for a in d.get("audit", []) if a]
    pinned_facts: list[int] = []
    seen_pins: set[int] = set()
    for raw in d.get("pinned_facts", []):
        try:
            seq = int(raw)
        except (TypeError, ValueError):
            continue
        if seq <= 0 or seq in seen_pins:
            continue
        seen_pins.add(seq)
        pinned_facts.append(seq)
    drift = str(d.get("drift", "")).strip()
    complete_why = str(d.get("complete_why", "")).strip()
    # verdict: honor the model's explicit choice; else derive it (back-compat with
    # the old goal_met-only schema). goal_met → complete; otherwise → explore.
    verdict = str(d.get("verdict", "")).strip().lower()
    if verdict not in _VALID_VERDICTS:
        verdict = VERDICT_COMPLETE if goal_met else VERDICT_EXPLORE
    # keep goal_met and verdict consistent for downstream callers
    if verdict == VERDICT_COMPLETE:
        goal_met = True
    return ReasonResult(goal_met=goal_met, intents=intents, audit_notes=audit,
                        verdict=verdict, drift=drift, complete_why=complete_why,
                        semantic_dedupe_available=isinstance(d.get("intents"), list),
                        pinned_facts=pinned_facts)


async def run_reason(
    *,
    llm: Any,
    model: str,
    graph_summary: str,
    fact_index: str = "",
    max_intents: int = 4,
    run_id: Optional[str] = None,
    challenge_id: Optional[str] = None,
    goal: Optional[str] = None,
    mode: str = "ctf",
) -> ReasonResult:
    """Call the cheap planner model and parse its intents + audit. `mode`/`goal`
    select the CTF (default, byte-identical) vs pentest (goal-driven) prompt."""
    messages = build_reason_prompt(graph_summary, max_intents=max_intents,
                                   fact_index=fact_index,
                                   goal=goal, mode=mode)
    # No max_tokens cap: deepseek-v4-pro is a reasoning model — tokens are spent on
    # reasoning_content FIRST, so any cap risks truncating the JSON answer before it
    # is emitted (observed in run-7349: the reply cut off mid-thought, _extract_json
    # got {}, 0 intents → the coordinator fell into an endless retry_bootstrap loop
    # because Explore never had an intent to claim). The planner's context is large;
    # let the API use the model's own maximum.
    resp = await llm.chat(
        model=model, messages=messages, temperature=0.3, max_tokens=None,
        stream=False, run_id=run_id, challenge_id=challenge_id,
        solver_id="reason",
    )
    return parse_reason_reply(getattr(resp, "content", "") or "",
                              max_intents=max_intents)


# Near-duplicate goal filter (mechanical BACKSTOP for the prompt's no-re-proposal
# rule). Deliberately conservative: it only catches filler-word rewordings of a
# goal already in flight — true paraphrases ("Submit L1 flag" vs "Ask operator to
# submit L1 flag") are the PROMPT's job, because an aggressive mechanical filter
# is exactly the run-7349 failure shape (0 intents proposed → Explore starves →
# endless retry_bootstrap). It also checks ONLY open/claimed goals, never
# concluded ones: re-proposing an attempted direction under NEW evidence is
# legitimate and must stay a planner judgment call.
_GOAL_STOPWORDS = frozenset(
    "the a an to of for on in at and or with via then into from by it its this "
    "that these those please try attempt now next using use".split())


def _norm_goal(goal: str) -> str:
    toks = re.findall(r"[a-z0-9]+", (goal or "").lower())
    return " ".join(t for t in toks if t not in _GOAL_STOPWORDS)


def _near_duplicate(goal: str, existing: list[str]) -> bool:
    """True iff `goal`, after filler-word normalization, is (near-)identical to a
    goal already in `existing` — same token bag, or ≥0.9 character similarity."""
    import difflib
    g = _norm_goal(goal)
    if not g:
        return False
    gset = frozenset(g.split())
    for e in existing:
        en = _norm_goal(e)
        if not en:
            continue
        if g == en or gset == frozenset(en.split()):
            return True
        if difflib.SequenceMatcher(None, g, en).ratio() >= 0.9:
            return True
    return False


def _unique_intent_id(raw_id: str, goal: str) -> str:
    """Make a cross-round-unique intent id.

    The Reason model is prompted with an example using id "I1", so almost every
    round it labels its intents I1..I4 — and propose_intent dedupes on
    `intent::{id}` (a UNIQUE key), so from round 2 on EVERY intent collides with
    round 1's and is silently dropped (seq=-1 → 0 proposed → Explore starves →
    the coordinator falls into endless retry_bootstrap). We suffix the id with a
    short hash of the GOAL text: two genuinely different directions get distinct
    ids (both proposed), while the exact same goal re-proposed keeps the same id
    (correctly deduped — no duplicate work)."""
    h = hashlib.sha1(goal.strip().encode("utf-8")).hexdigest()[:8]
    return f"{raw_id}-{h}"


def _route_key(shared_graph: Any, route_hash: str) -> str:
    route = (route_hash or "").strip()
    if not route:
        return ""
    norm = getattr(shared_graph, "normalize_route_hash", None)
    if callable(norm):
        try:
            return str(norm(route) or "")
        except Exception:
            pass
    return route.lower()


def _propose_one(shared_graph: Any, it: Intent, *, actor: str) -> Optional[dict[str, Any]]:
    iid = _unique_intent_id(it.intent_id, it.goal)
    seq = shared_graph.propose_intent(
        actor=actor, intent_id=iid, goal=it.goal,
        payload=it.to_payload(),
        from_fact_seqs=it.from_facts or None,
    )
    if seq == -1:
        return None
    return {"intent_id": iid, "goal": it.goal,
            "worker_class": it.worker_class,
            "from_facts": it.from_facts,
            "route_hash": it.route_hash,
            "branch_id": it.branch_id}


def dispatch_intents(shared_graph: Any, result: ReasonResult, *,
                     actor: str = "reason") -> list[dict[str, Any]]:
    """Push reason's intents into the shared graph as claimable tasks.

    Returns the list of intents actually proposed (id/goal/worker_class) so the
    caller can emit blackboard `intent_proposed` events. Dead-ends from audit are
    surfaced via the reason summary (not here).

    Near-duplicates of a goal already OPEN/CLAIMED on the graph (or proposed
    earlier in this same batch) are dropped — the goal-hash id only dedupes
    byte-identical goals, so a filler-word rewording used to slip through as a
    "new" intent and double the work."""
    proposed: list[dict[str, Any]] = []
    if shared_graph is None:
        return proposed
    try:
        active_goals: list[str] = list(shared_graph.open_goal_texts())
    except Exception:
        active_goals = []
    try:
        dispatchable_goals: list[str] = list(shared_graph.dispatchable_goal_texts())
    except Exception:
        dispatchable_goals = list(active_goals)
    existing: list[str] = list(active_goals)
    # P1 escape-valve: also dedup against CONCLUDED-but-BARREN directions (tried,
    # produced no fact/flag). Stops the planner re-proposing paraphrases of already-
    # attempted-and-empty directions ("重走老路" at the planner layer). The method
    # EXCLUDES concluded intents that produced a fact, so a productive direction can
    # still be re-proposed under new evidence — preserving the run-7349 anti-
    # starvation guarantee (no blanket concluded-dedup).
    try:
        existing += list(shared_graph.barren_concluded_goal_texts())
    except Exception:
        pass
    try:
        active_routes = set(shared_graph.open_route_hashes())
    except Exception:
        active_routes = set()
    batch_routes: set[str] = set()
    semantic_dedupe = bool(getattr(result, "semantic_dedupe_available", False))
    skipped: list[Intent] = []
    for it in result.intents:
        if semantic_dedupe:
            if it.dup_of and not it.reopen_because:
                skipped.append(it)
                continue
        elif _near_duplicate(it.goal, existing):
            skipped.append(it)
            continue
        route = _route_key(shared_graph, it.route_hash)
        if route and it.worker_class not in {"verifier", "review"}:
            if route in active_routes or route in batch_routes:
                skipped.append(it)
                continue
        row = _propose_one(shared_graph, it, actor=actor)
        if row:
            proposed.append(row)
            existing.append(it.goal)
            if route and it.worker_class not in {"verifier", "review"}:
                batch_routes.add(route)
    if not proposed and not dispatchable_goals and skipped:
        row = _propose_one(shared_graph, skipped[0], actor=actor)
        if row:
            proposed.append(row)
    return proposed
