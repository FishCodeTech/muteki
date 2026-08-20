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
import os
import re
from dataclasses import dataclass, field, replace
from enum import Enum
from typing import Any, Final, Optional

from muteki.models.solve_graph import SolveGraph


@dataclass
class Intent:
    """A claimable, typed task (PCSG-lite intent)."""

    intent_id: str
    goal: str
    worker_class: str = "code"  # code | shell_agent | verifier | review
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
    # Optional, shadow-only typed execution boundary.  The ordinary dispatcher
    # ignores these fields; an opt-in counterfactual adapter can evaluate them.
    cognitive_predictions: dict[str, str] = field(default_factory=dict)
    cognitive_capability: str = ""
    cognitive_supplied_cost_estimate_units: int | None = None
    cognitive_other_unknown_lane: bool = False
    # Schema'd declaration (round-14 seam, default-off): the proposer's typed
    # expected effects {"effect_types": [...], "expected_artifacts": [...],
    # "confidence": "low|medium|high"}. Absent/empty = no declaration — valid
    # first-class state, never a proposal failure. The ordinary dispatcher
    # does not read this; research-side instruments consume it offline.
    declared_effects: dict = field(default_factory=dict)

    def to_payload(self) -> dict:
        payload = {
            "worker_class": self.worker_class,
            "depends_on": self.depends_on,
            "rationale": self.rationale,
            "route_hash": self.route_hash,
            "branch_id": self.branch_id,
            "lane_key": self.lane_key,
            "risk_class": self.risk_class,
            "resource_key": self.resource_key,
            "dup_of": self.dup_of,
            "reopen_because": self.reopen_because,
        }
        if self.declared_effects:
            payload["declares"] = self.declared_effects
        return payload


@dataclass(frozen=True)
class CognitiveHypothesisDraft:
    """Shadow-only open hypothesis; never written as canonical evidence."""

    hypothesis_id: str
    claim: str
    rationale: str
    weight_units: int


COGNITIVE_SHADOW_ANNOTATION_SYSTEM = """You are a counterfactual cognition annotator.
You do not plan or execute work. The ordinary Reason planner has already produced a
FROZEN executable plan. You may only annotate that exact plan for an offline shadow
comparison. Output STRICT JSON with this shape:
{
  "baseline_digest": "<echo the supplied digest exactly>",
  "annotations": [
    {"id": "<exact frozen id>", "goal": "<exact frozen goal>",
     "cognitive_experiment": {
       "predictions": {"H1": "short_outcome_id", "H2": "other_outcome_id"},
       "capability": "short_capability_id",
       "supplied_cost_estimate_units": 1,
       "other_unknown_lane": true
     }}
  ],
  "cognitive_hypotheses": [
    {"id": "CH1", "claim": "short competing explanation",
     "rationale": "fact-bound reason", "weight_units": 45}
  ]
}

Rules:
- Echo every frozen intent exactly once, with byte-exact `id` and `goal`. Never add,
  remove, rename, reorder semantically, or rewrite executable work.
- The only allowed annotation field is `cognitive_experiment`. Omit that field for an
  open-ended discovery intent or when predictions are not concrete.
- A typed experiment needs at least two active hypothesis ids and at least two distinct
  short outcomes. Predict every active hypothesis explicitly. If an open-ended
  remainder cannot be enumerated, set `other_unknown_lane` true.
- `supplied_cost_estimate_units` is a coarse proposer estimate covering execution
  and checking. It is not measured usage, a reservation, or budget authority.
- Add 2-8 genuinely competing `cognitive_hypotheses` only when the graph has no active
  ids. These are shadow proposals, never facts.
- Output only the JSON object. This response has no dispatch, evidence, acceptance, or
  production authority.
"""


class CognitiveShadowAnnotationError(ValueError):
    """The shadow annotator failed to bind exactly to the frozen Reason plan."""


# Reason's verdict — a state-machine decision, not just a bool. The solver acts on
# this: `complete` → force conclude/extract now;
# `course_correct` → the run drifted, steer to a new direction; `explore` → keep
# going on the proposed intents.
VERDICT_COMPLETE = "complete"
VERDICT_COURSE_CORRECT = "course_correct"
VERDICT_EXPLORE = "explore"
_VALID_VERDICTS = (VERDICT_COMPLETE, VERDICT_COURSE_CORRECT, VERDICT_EXPLORE)


class PlannerFailureKind(str, Enum):
    """Typed reason failure used by the coordinator's containment policy.

    A dry planner is not equivalent to an empty work queue.  Keeping the reason
    explicit prevents infrastructure/configuration failures from being laundered
    into a business decision to spawn another whole-challenge worker.
    """

    UNAVAILABLE = "planner_unavailable"
    EXCEPTION = "planner_exception"
    INVALID_PLAN = "invalid_plan"
    EMPTY_PLAN = "empty_plan"
    NEEDS_NEW_INFORMATION = "needs_new_information"


@dataclass(frozen=True)
class PlannerFailure:
    kind: PlannerFailureKind
    detail: str = ""


@dataclass
class ReasonResult:
    goal_met: bool
    intents: list[Intent]
    audit_notes: list[str]  # facts flagged as needing re-verification
    verdict: str = VERDICT_EXPLORE  # complete | course_correct | explore
    drift: str = ""  # if course_correct: what went wrong + the fix
    complete_why: str = ""  # if complete: why the goal is already met
    semantic_dedupe_available: bool = False
    pinned_facts: list[int] = field(default_factory=list)
    planner_failure: PlannerFailure | None = None
    cognitive_hypotheses: list[CognitiveHypothesisDraft] = field(default_factory=list)


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


# Round-14 declaration prompt addendum (default-OFF; only appended when
# MUTEKI_REASON_DECLARE_EFFECTS=1 — the stock REASON_SYSTEM stays
# byte-identical otherwise). Asks the planner to attach a typed expected-
# effects declaration to each intent. Declarations are optional metadata;
# the ordinary dispatcher never reads them.
DECLARE_EFFECTS_ADDENDUM: Final = """

DECLARATIONS (required for every intent): add a
"declares" object to each intent: {"effect_types": [...], "expected_artifacts":
[...], "confidence": "low|medium|high"}. effect_types must come from:
recover_secret (flag/key/password/plaintext), verify_hypothesis (confirm or
refute a specific claim), discover_artifact (find/extract/catalog files or
artifacts), analyze_mechanism (understand a protocol/cipher/binary),
exploit_chain (weaponize a vulnerability into an effect), eliminate_direction
(rule a path out), other. expected_artifacts: at most 3 short noun phrases
naming what should exist after success (e.g. "file-type mapping table",
"recovered key bytes", "verdict on deployment config"). If an effect cannot
be estimated, use effect_types ["other"], expected_artifacts [], confidence
"low". Never omit the object in this experimental mode."""

# Round-16 exact target/receipt seam.  This is a separate, default-off schema;
# v1 and v2 are mutually exclusive so a measurement cannot silently mix their
# truth conditions.  Target ids and receipt keys must come from the caller's
# bounded catalog.  The production dispatcher still treats this as inert JSON.
DECLARE_TARGET_RECEIPTS_V2_ADDENDUM: Final = """

EXACT DECLARATIONS V2 (required for every intent): add a "declares" object:
{"schema":"muteki.research.declaration-target-receipt.v2","targets":[
{"target_id":"<exact catalog id>","predicate":"fact_active|artifact_present|hypothesis_true|terminal_admitted|poststate_holds|direction_viable",
"polarity":"establish|retract","receipt":{"class":"verified_fact|structured_artifact|fact_review|admitted_flag|applied_poststate","key":"<exact catalog receipt key>"}}],
"effect_types":[...],"confidence":"low|medium|high"}. Use only target ids and
receipt keys explicitly present in the graph's target catalog. Never invent or
lexically approximate them. Every target is an auditable promise: retract
requires an explicit refuting review or applied poststate, never mere absence.
If no catalog target fits an intent, omit the declares object. Output remains
planning metadata only; it cannot prove progress or authorize dispatch."""


def _env_true(name: str) -> bool:
    raw = (os.environ.get(name) or "").strip().lower()
    return raw in {"1", "true", "yes", "on"}


def declare_effects_enabled_from_env() -> bool:
    return _env_true("MUTEKI_REASON_DECLARE_EFFECTS")


def declare_target_receipts_v2_enabled_from_env() -> bool:
    return _env_true("MUTEKI_REASON_DECLARE_TARGET_RECEIPTS_V2")


def _declaration_mode_from_env() -> str:
    v1 = declare_effects_enabled_from_env()
    v2 = declare_target_receipts_v2_enabled_from_env()
    if v1 and v2:
        raise ValueError("Reason declaration v1 and v2 gates are mutually exclusive")
    if v2:
        return "v2"
    if v1:
        return "v1"
    return ""


# Pentest variant: SAME planner. Success is finding_ok (evidence predicate).
# verdict=complete is a planning/coverage signal, not goal_met by itself.
REASON_SYSTEM_PENTEST = """You are the REASON phase of an autonomous \
penetration-testing / security-audit swarm. You do NOT execute — you read the \
shared findings-graph and DECIDE the swarm's next move. Become an expert in this \
target's stack, judge the state honestly, and output STRICT JSON.

First decide a `verdict` (planning signal only — it does NOT accept a report):
- "complete": every direction that still matters has been proposed; use this so
  the coordinator can check coverage. A `complete` verdict does NOT mark the
  engagement successful. Success is the report collection: a Worker submits a
  complete exploit report (SUBMIT_REPORT=path), a different Verifier reproduces
  it, and a host-side value check rejects self-XSS / attacker-only / informational
  issues. VERIFIED_FACT lines, this complete_why text, and Review summaries are
  not reports.
- "course_correct": the run has DRIFTED — workers are repeating, stuck on a dead
  angle, or chasing unverified assumptions, and the current intents won't reach the
  goal. Say what went wrong and propose a corrected direction.
- "explore": still making progress; propose the next high-value directions.
  Prefer directions that can yield a distinct, independently reproducible report
  (SQL injection, command injection, stored XSS affecting other users, IDOR).
  Do not propose reflected self-XSS.

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


def resolve_declaration_mode(declaration_mode: Optional[str] = None) -> str:
    """Resolve declaration mode: explicit override wins; else env (may be cleared)."""
    if declaration_mode is None or not str(declaration_mode).strip():
        return _declaration_mode_from_env()
    mode = str(declaration_mode).strip().lower()
    if mode in {"off", "none", "0"}:
        return ""
    if mode not in {"v1", "v2"}:
        raise ValueError(f"unsupported declaration_mode: {declaration_mode!r}")
    return mode


def build_reason_prompt(
    summary: str,
    max_intents: int = 4,
    *,
    fact_index: str = "",
    goal: Optional[str] = None,
    mode: str = "ctf",
    scope: Optional[str] = None,
    cognitive_shadow: bool = False,
    declaration_target_catalog_v2: frozenset[
        tuple[str, str, str, str, str]
    ] | None = None,
    declaration_mode: Optional[str] = None,
) -> list[dict]:
    if cognitive_shadow:
        raise CognitiveShadowAnnotationError(
            "ordinary Reason prompts cannot carry cognitive shadow metadata; "
            "use the separate frozen-plan annotation call"
        )
    retention = ""
    if (fact_index or "").strip():
        idx = fact_index.strip()
        if "Fact retention index" not in idx:
            idx = "## Fact retention index (model decides pinned_facts)\n" + idx
        retention = f"\n\n{idx}"
    # pentest → goal-driven planner (the operator's engagement goal anchors the
    # `complete` verdict). CTF (default) keeps the original prompt byte-for-byte.
    if mode == "pentest":
        user = f"Engagement goal:\n{goal}\n\n" if (goal or "").strip() else ""
        if (scope or "").strip():
            user += f"Engagement scope:\n{scope.strip()}\n\n"
        user += (
            f"Shared findings-graph:\n\n{summary}{retention}\n\n"
            "Output the planning JSON."
        )
        system = REASON_SYSTEM_PENTEST.replace("{max_intents}", str(max_intents))
        return [
            {"role": "system", "content": system},
            {"role": "user", "content": user},
        ]
    system = REASON_SYSTEM.replace("{max_intents}", str(max_intents))
    resolved_mode = resolve_declaration_mode(declaration_mode)
    if resolved_mode == "v1":
        system += DECLARE_EFFECTS_ADDENDUM
    elif resolved_mode == "v2":
        # Class-side mode must arm the addendum even when catalog refresh failed
        # (catalog=None). Empty frozenset keeps the stock addendum byte-identical.
        system += DECLARE_TARGET_RECEIPTS_V2_ADDENDUM
        if declaration_target_catalog_v2:
            try:
                from muteki.frameworks.f01_declared_effects.catalog import (
                    catalog_prompt_block,
                )

                system += "\n\n" + catalog_prompt_block(declaration_target_catalog_v2)
            except Exception:
                pass
    return [
        {"role": "system", "content": system},
        {
            "role": "user",
            "content": f"Shared solve-graph:\n\n{summary}{retention}\n\n"
            "Output the planning JSON.",
        },
    ]


def _intent_execution_body(intent: Intent) -> dict[str, object]:
    """Everything the ordinary dispatcher can observe, excluding shadow fields."""

    return {
        "intent_id": intent.intent_id,
        "goal": intent.goal,
        "worker_class": intent.worker_class,
        "depends_on": tuple(intent.depends_on),
        "rationale": intent.rationale,
        "from_facts": tuple(intent.from_facts),
        "route_hash": intent.route_hash,
        "branch_id": intent.branch_id,
        "lane_key": intent.lane_key,
        "risk_class": intent.risk_class,
        "resource_key": intent.resource_key,
        "dup_of": intent.dup_of,
        "reopen_because": intent.reopen_because,
    }


def cognitive_shadow_baseline_digest(result: ReasonResult) -> str:
    """Bind a shadow annotation to one exact, already-produced Reason plan."""

    if type(result) is not ReasonResult:
        raise TypeError("result must be ReasonResult")
    body = {
        "schema": "muteki.reason-frozen-shadow-baseline.v1",
        "intents": tuple(_intent_execution_body(item) for item in result.intents),
    }
    return hashlib.sha256(
        json.dumps(
            body,
            ensure_ascii=False,
            sort_keys=True,
            separators=(",", ":"),
        ).encode("utf-8")
    ).hexdigest()


def build_cognitive_shadow_annotation_prompt(
    *,
    graph_summary: str,
    baseline_result: ReasonResult,
) -> list[dict[str, str]]:
    """Build a non-dispatching annotation request over frozen ordinary intents."""

    digest = cognitive_shadow_baseline_digest(baseline_result)
    frozen = tuple(_intent_execution_body(item) for item in baseline_result.intents)
    user = {
        "baseline_digest": digest,
        "frozen_intents": frozen,
        "shared_graph_summary": graph_summary,
    }
    return [
        {"role": "system", "content": COGNITIVE_SHADOW_ANNOTATION_SYSTEM},
        {
            "role": "user",
            "content": json.dumps(
                user,
                ensure_ascii=False,
                sort_keys=True,
                separators=(",", ":"),
            ),
        },
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


def _cognitive_meta(raw: dict) -> dict:
    value = raw.get("cognitive_experiment")
    return value if isinstance(value, dict) else {}


def _parse_cognitive_predictions(raw: dict) -> dict[str, str]:
    value = _cognitive_meta(raw).get("predictions")
    if not isinstance(value, dict):
        return {}
    predictions: dict[str, str] = {}
    for hypothesis_id, outcome in value.items():
        hypothesis_id = str(hypothesis_id).strip()
        outcome = str(outcome).strip()
        if (
            hypothesis_id
            and outcome
            and len(hypothesis_id) <= 80
            and len(outcome) <= 80
        ):
            predictions[hypothesis_id] = outcome
    return predictions


# Round-14 declaration taxonomy (production-local copy of the frozen
# research taxonomy in muteki/research/declaration_effect_precision_v1.py —
# production must not import research, so the 7 type NAMES are mirrored as
# data here; any taxonomy change is a new measurement round, not an edit).
DECLARED_EFFECT_TYPES = frozenset({
    "recover_secret",
    "verify_hypothesis",
    "discover_artifact",
    "analyze_mechanism",
    "exploit_chain",
    "eliminate_direction",
    "other",
})


def _parse_declares(raw: dict) -> dict:
    """Validate an optional v1 ``declares`` object; keep the intent on error."""
    value = raw.get("declares")
    if not isinstance(value, dict):
        return {}
    raw_types = value.get("effect_types")
    raw_artifacts = value.get("expected_artifacts", [])
    if not isinstance(raw_types, list) or not isinstance(raw_artifacts, list):
        return {}
    effect_types: list[str] = []
    for item in raw_types:
        if not isinstance(item, str):
            return {}
        effect_type = item.strip()
        if effect_type not in DECLARED_EFFECT_TYPES:
            continue
        if effect_type not in effect_types:
            effect_types.append(effect_type)
    if not effect_types:
        return {}
    artifacts: list[str] = []
    for item in raw_artifacts[:3]:
        if not isinstance(item, str):
            return {}
        text = item.strip()
        if text and text not in artifacts:
            artifacts.append(text[:120])
    confidence = str(value.get("confidence") or "").strip().lower()
    if confidence not in ("low", "medium", "high"):
        confidence = ""
    declares: dict = {"effect_types": effect_types}
    if artifacts:
        declares["expected_artifacts"] = artifacts
    if confidence:
        declares["confidence"] = confidence
    return declares


_V2_DECLARATION_SCHEMA: Final = "muteki.research.declaration-target-receipt.v2"
_V2_PREDICATES: Final = frozenset({
    "fact_active", "artifact_present", "hypothesis_true", "terminal_admitted",
    "poststate_holds", "direction_viable",
})
_V2_POLARITIES: Final = frozenset({"establish", "retract"})
_V2_RECEIPT_CLASSES: Final = frozenset({
    "verified_fact", "structured_artifact", "fact_review", "admitted_flag",
    "applied_poststate",
})
_V2_COMPATIBLE_RECEIPTS: Final = {
    ("fact_active", "establish"): frozenset({"verified_fact"}),
    ("fact_active", "retract"): frozenset({"fact_review", "applied_poststate"}),
    ("artifact_present", "establish"): frozenset({"structured_artifact"}),
    ("artifact_present", "retract"): frozenset({"applied_poststate"}),
    ("hypothesis_true", "establish"): frozenset({"fact_review"}),
    ("hypothesis_true", "retract"): frozenset({"fact_review"}),
    ("terminal_admitted", "establish"): frozenset({"admitted_flag"}),
    ("terminal_admitted", "retract"): frozenset({"fact_review"}),
    ("poststate_holds", "establish"): frozenset({"applied_poststate"}),
    ("poststate_holds", "retract"): frozenset({"applied_poststate"}),
    ("direction_viable", "establish"): frozenset({"fact_review"}),
    ("direction_viable", "retract"): frozenset({"fact_review"}),
}


def _v2_text(value: object, limit: int) -> str:
    if not isinstance(value, str):
        return ""
    text = value.strip()
    if not text or len(text) > limit or any(ord(char) < 32 for char in text):
        return ""
    return text


def _parse_declares_v2(
    raw: dict,
    *,
    target_catalog: frozenset[tuple[str, str, str, str, str]] | None,
) -> dict:
    """Strict v2 parser with caller-owned exact catalog membership."""

    # A prompt-visible string is not authority. Without a separately supplied
    # catalog, optional declaration metadata is dropped while the intent stays.
    if target_catalog is None:
        return {}
    value = raw.get("declares")
    if not isinstance(value, dict) or set(value) - {
        "schema", "targets", "effect_types", "confidence",
    }:
        return {}
    if value.get("schema") != _V2_DECLARATION_SCHEMA:
        return {}
    raw_targets = value.get("targets")
    if not isinstance(raw_targets, list) or not 1 <= len(raw_targets) <= 8:
        return {}
    targets: list[dict[str, object]] = []
    seen: set[tuple[str, str]] = set()
    for target in raw_targets:
        if not isinstance(target, dict) or set(target) != {
            "target_id", "predicate", "polarity", "receipt",
        }:
            return {}
        receipt = target.get("receipt")
        if not isinstance(receipt, dict) or set(receipt) != {"class", "key"}:
            return {}
        target_id = _v2_text(target.get("target_id"), 160)
        predicate = _v2_text(target.get("predicate"), 40)
        polarity = _v2_text(target.get("polarity"), 20)
        receipt_class = _v2_text(receipt.get("class"), 40)
        receipt_key = _v2_text(receipt.get("key"), 160)
        identity = (target_id, predicate)
        if (
            not target_id or predicate not in _V2_PREDICATES
            or polarity not in _V2_POLARITIES
            or receipt_class not in _V2_RECEIPT_CLASSES or not receipt_key
            or receipt_class not in _V2_COMPATIBLE_RECEIPTS[(predicate, polarity)]
            or identity in seen
        ):
            return {}
        if (
            target_id,
            predicate,
            polarity,
            receipt_class,
            receipt_key,
        ) not in target_catalog:
            return {}
        seen.add(identity)
        targets.append({
            "target_id": target_id,
            "predicate": predicate,
            "polarity": polarity,
            "receipt": {"class": receipt_class, "key": receipt_key},
        })
    targets.sort(key=lambda item: (
        str(item["target_id"]), str(item["predicate"]), str(item["polarity"]),
        str(item["receipt"]),
    ))
    raw_effects = value.get("effect_types", [])
    if not isinstance(raw_effects, list):
        return {}
    effects: list[str] = []
    for item in raw_effects:
        if not isinstance(item, str) or item.strip() not in DECLARED_EFFECT_TYPES:
            return {}
        effect = item.strip()
        if effect not in effects:
            effects.append(effect)
    confidence = str(value.get("confidence") or "").strip().lower()
    if confidence not in ("", "low", "medium", "high"):
        return {}
    declares: dict[str, object] = {
        "schema": _V2_DECLARATION_SCHEMA,
        "targets": targets,
    }
    if effects:
        declares["effect_types"] = effects
    if confidence:
        declares["confidence"] = confidence
    return declares


def _parse_cognitive_capability(raw: dict) -> str:
    return str(_cognitive_meta(raw).get("capability") or "").strip()[:80]


def _parse_cognitive_cost_estimate(raw: dict) -> int | None:
    value = _cognitive_meta(raw).get("supplied_cost_estimate_units")
    if type(value) is int and 0 < value <= 1_000_000:
        return value
    return None


def _parse_cognitive_other_lane(raw: dict) -> bool:
    return _cognitive_meta(raw).get("other_unknown_lane") is True


def _parse_cognitive_hypotheses(value: object) -> list[CognitiveHypothesisDraft]:
    if not isinstance(value, list):
        return []
    drafts: list[CognitiveHypothesisDraft] = []
    seen: set[str] = set()
    for raw in value[:8]:
        if not isinstance(raw, dict):
            continue
        hypothesis_id = str(raw.get("id") or "").strip()[:80]
        claim = str(raw.get("claim") or "").strip()[:500]
        rationale = str(raw.get("rationale") or "").strip()[:500]
        weight = raw.get("weight_units")
        if (
            hypothesis_id
            and hypothesis_id not in seen
            and claim
            and type(weight) is int
            and 0 < weight <= 1_000_000
        ):
            seen.add(hypothesis_id)
            drafts.append(
                CognitiveHypothesisDraft(hypothesis_id, claim, rationale, weight)
            )
    return drafts


def validate_cognitive_shadow_annotation(
    baseline_result: ReasonResult,
    annotated_result: ReasonResult,
) -> None:
    """Reject any annotation that changes the frozen executable plan.

    The coordinator dispatches ``baseline_result`` regardless, but validating the
    complete execution body here also prevents a future caller from accidentally
    treating the annotation response as a substitute plan.
    """

    if (
        type(baseline_result) is not ReasonResult
        or type(annotated_result) is not ReasonResult
    ):
        raise TypeError("baseline_result and annotated_result must be ReasonResult")

    def by_id(result: ReasonResult, label: str) -> dict[str, Intent]:
        out: dict[str, Intent] = {}
        for item in result.intents:
            if item.intent_id in out:
                raise CognitiveShadowAnnotationError(
                    f"{label} contains duplicate intent id {item.intent_id!r}"
                )
            out[item.intent_id] = item
        return out

    baseline = by_id(baseline_result, "baseline")
    annotated = by_id(annotated_result, "annotation")
    if set(annotated) != set(baseline):
        raise CognitiveShadowAnnotationError(
            "annotation intent ids do not exactly match the frozen baseline"
        )
    for intent_id, baseline_intent in baseline.items():
        if _intent_execution_body(annotated[intent_id]) != _intent_execution_body(
            baseline_intent
        ):
            raise CognitiveShadowAnnotationError(
                f"annotation changed frozen executable intent {intent_id!r}"
            )


def parse_cognitive_shadow_annotation_reply(
    text: str,
    *,
    baseline_result: ReasonResult,
) -> ReasonResult:
    """Parse annotations while copying executable fields only from the baseline."""

    if not baseline_result.intents:
        raise CognitiveShadowAnnotationError("cannot annotate an empty baseline plan")
    payload = _extract_json(text)
    if not payload:
        raise CognitiveShadowAnnotationError("annotation reply is not a JSON object")
    expected_digest = cognitive_shadow_baseline_digest(baseline_result)
    if payload.get("baseline_digest") != expected_digest:
        raise CognitiveShadowAnnotationError(
            "annotation baseline digest does not match the frozen plan"
        )
    raw_annotations = payload.get("annotations")
    if not isinstance(raw_annotations, list):
        raise CognitiveShadowAnnotationError("annotations must be a JSON array")

    annotations: dict[str, dict] = {}
    for raw in raw_annotations:
        if not isinstance(raw, dict):
            raise CognitiveShadowAnnotationError("every annotation must be an object")
        unexpected = set(raw) - {"id", "goal", "cognitive_experiment"}
        if unexpected:
            raise CognitiveShadowAnnotationError(
                "annotation attempted non-shadow fields: "
                + ",".join(sorted(unexpected))
            )
        intent_id = raw.get("id")
        if not isinstance(intent_id, str) or intent_id in annotations:
            raise CognitiveShadowAnnotationError(
                "annotation ids must be unique exact strings"
            )
        annotations[intent_id] = raw

    baseline_by_id = {item.intent_id: item for item in baseline_result.intents}
    if len(baseline_by_id) != len(baseline_result.intents):
        raise CognitiveShadowAnnotationError("baseline contains duplicate intent ids")
    if set(annotations) != set(baseline_by_id):
        raise CognitiveShadowAnnotationError(
            "annotation must echo every frozen intent exactly once"
        )

    annotated_intents: list[Intent] = []
    for baseline_intent in baseline_result.intents:
        raw = annotations[baseline_intent.intent_id]
        if raw.get("goal") != baseline_intent.goal:
            raise CognitiveShadowAnnotationError(
                f"annotation changed frozen goal for {baseline_intent.intent_id!r}"
            )
        annotated_intents.append(
            replace(
                baseline_intent,
                cognitive_predictions=_parse_cognitive_predictions(raw),
                cognitive_capability=_parse_cognitive_capability(raw),
                cognitive_supplied_cost_estimate_units=(
                    _parse_cognitive_cost_estimate(raw)
                ),
                cognitive_other_unknown_lane=_parse_cognitive_other_lane(raw),
            )
        )

    result = replace(
        baseline_result,
        intents=annotated_intents,
        cognitive_hypotheses=_parse_cognitive_hypotheses(
            payload.get("cognitive_hypotheses")
        ),
    )
    validate_cognitive_shadow_annotation(baseline_result, result)
    return result


def parse_reason_reply(
    text: str,
    *,
    max_intents: int = 4,
    allow_declares: bool = False,
    allow_declares_v2: bool = False,
    declaration_target_catalog_v2: frozenset[
        tuple[str, str, str, str, str]
    ] | None = None,
) -> ReasonResult:
    if allow_declares and allow_declares_v2:
        raise ValueError("Reason declaration v1 and v2 parsers are mutually exclusive")
    d = _extract_json(text)
    plan_is_valid = bool(d) and isinstance(d.get("intents"), list)
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
        intents.append(
            Intent(
                intent_id=str(raw.get("id") or f"I{i + 1}"),
                goal=goal,
                worker_class=wc,
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
                cognitive_predictions=_parse_cognitive_predictions(raw),
                cognitive_capability=_parse_cognitive_capability(raw),
                cognitive_supplied_cost_estimate_units=_parse_cognitive_cost_estimate(
                    raw
                ),
                cognitive_other_unknown_lane=_parse_cognitive_other_lane(raw),
                declared_effects=(
                    _parse_declares_v2(
                        raw,
                        target_catalog=declaration_target_catalog_v2,
                    )
                    if allow_declares_v2
                    else _parse_declares(raw) if allow_declares else {}
                ),
            )
        )
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
    planner_failure = None
    if not plan_is_valid:
        planner_failure = PlannerFailure(
            PlannerFailureKind.INVALID_PLAN,
            "planner reply did not contain the required JSON intents array",
        )
    elif not intents and verdict != VERDICT_COMPLETE:
        planner_failure = PlannerFailure(
            PlannerFailureKind.EMPTY_PLAN,
            "planner returned no executable intents",
        )
    return ReasonResult(
        goal_met=goal_met,
        intents=intents,
        audit_notes=audit,
        verdict=verdict,
        drift=drift,
        complete_why=complete_why,
        semantic_dedupe_available=plan_is_valid,
        pinned_facts=pinned_facts,
        planner_failure=planner_failure,
        cognitive_hypotheses=_parse_cognitive_hypotheses(d.get("cognitive_hypotheses")),
    )


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
    scope: Optional[str] = None,
    cognitive_shadow: bool = False,
    declaration_target_catalog_v2: frozenset[
        tuple[str, str, str, str, str]
    ] | None = None,
    declaration_mode: Optional[str] = None,
) -> ReasonResult:
    """Call the cheap planner model and parse its intents + audit. `mode`/`goal`
    select the CTF (default, byte-identical) vs pentest (goal-driven) prompt.

    ``declaration_mode`` is an optional class-side override (``\"v1\"``/``\"v2\"``).
    When omitted, falls back to env gates (which A/B harnesses clear per cell).
    """
    if cognitive_shadow:
        raise CognitiveShadowAnnotationError(
            "ordinary Reason cannot produce shadow metadata; run the separate "
            "frozen-plan annotation call"
        )
    messages = build_reason_prompt(
        graph_summary,
        max_intents=max_intents,
        fact_index=fact_index,
        goal=goal,
        mode=mode,
        scope=scope,
        cognitive_shadow=cognitive_shadow,
        declaration_target_catalog_v2=declaration_target_catalog_v2,
        declaration_mode=declaration_mode,
    )
    # No max_tokens cap: deepseek-v4-pro is a reasoning model — tokens are spent on
    # reasoning_content FIRST, so any cap risks truncating the JSON answer before it
    # is emitted (observed in run-7349: the reply cut off mid-thought, _extract_json
    # got {}, 0 intents → the coordinator fell into an endless retry_bootstrap loop
    # because Explore never had an intent to claim). The planner's context is large;
    # let the API use the model's own maximum.
    resp = await llm.chat(
        model=model,
        messages=messages,
        temperature=0.3,
        max_tokens=None,
        stream=False,
        run_id=run_id,
        challenge_id=challenge_id,
        solver_id="reason",
    )
    resolved_mode = resolve_declaration_mode(declaration_mode)
    return parse_reason_reply(
        getattr(resp, "content", "") or "",
        max_intents=max_intents,
        allow_declares=resolved_mode == "v1",
        allow_declares_v2=resolved_mode == "v2",
        declaration_target_catalog_v2=declaration_target_catalog_v2,
    )


async def run_cognitive_shadow_annotation(
    *,
    llm: Any,
    model: str,
    graph_summary: str,
    baseline_result: ReasonResult,
    run_id: Optional[str] = None,
    challenge_id: Optional[str] = None,
) -> ReasonResult:
    """Make a second, normally metered LLM call with zero dispatch authority.

    ``LLMClient.chat`` records usage through the same CostController as every
    other call. A distinct solver id keeps the shadow spend attributable instead
    of hiding it inside ordinary planning cost.
    """

    messages = build_cognitive_shadow_annotation_prompt(
        graph_summary=graph_summary,
        baseline_result=baseline_result,
    )
    response = await llm.chat(
        model=model,
        messages=messages,
        temperature=0.0,
        max_tokens=None,
        stream=False,
        run_id=run_id,
        challenge_id=challenge_id,
        solver_id="reason-cognitive-shadow",
    )
    return parse_cognitive_shadow_annotation_reply(
        getattr(response, "content", "") or "",
        baseline_result=baseline_result,
    )


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
    "that these those please try attempt now next using use".split()
)


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


def _propose_one(
    shared_graph: Any,
    it: Intent,
    *,
    actor: str,
    depends_on: list[str] | None = None,
) -> Optional[dict[str, Any]]:
    iid = _unique_intent_id(it.intent_id, it.goal)
    payload = it.to_payload()
    if depends_on is not None:
        payload["depends_on"] = depends_on
    seq = shared_graph.propose_intent(
        actor=actor,
        intent_id=iid,
        goal=it.goal,
        payload=payload,
        from_fact_seqs=it.from_facts or None,
    )
    if seq == -1:
        return None
    return {
        "intent_id": iid,
        "created_seq": int(seq),
        "goal": it.goal,
        "worker_class": it.worker_class,
        "from_facts": it.from_facts,
        "route_hash": it.route_hash,
        "branch_id": it.branch_id,
        "declares": dict(it.declared_effects) if it.declared_effects else None,
    }


def dispatch_intents(
    shared_graph: Any, result: ReasonResult, *, actor: str = "reason"
) -> list[dict[str, Any]]:
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
    raw_to_unique = {
        it.intent_id: _unique_intent_id(it.intent_id, it.goal)
        for it in result.intents
    }
    semantic_dedupe = bool(getattr(result, "semantic_dedupe_available", False))
    for it in result.intents:
        if semantic_dedupe:
            if it.dup_of and not it.reopen_because:
                continue
        if not it.reopen_because and _near_duplicate(it.goal, existing):
            continue
        route = _route_key(shared_graph, it.route_hash)
        if route and it.worker_class not in {"verifier", "review"}:
            if route in active_routes or route in batch_routes:
                continue
        depends_on = [
            raw_to_unique.get(dep, dep)
            for dep in it.depends_on
            if str(dep or "").strip()
        ]
        row = _propose_one(shared_graph, it, actor=actor, depends_on=depends_on)
        if row:
            proposed.append(row)
            existing.append(it.goal)
            if route and it.worker_class not in {"verifier", "review"}:
                batch_routes.add(route)
    return proposed
