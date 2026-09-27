"""Reason phase — global planning over host-admitted Facts.

The reason phase reads the active Fact graph and proposes non-overlapping
Intents for the swarm to claim. Unadmitted Worker claims are Observations and
stay outside this planning input; explicit verifier work may inspect one when a
bounded challenge would change the next action.

Form:
- runs on a CHEAP model (flash); the expensive model runs explore/solve.
- runs after a semantic solve-graph change, with concurrent writes coalesced.
- emits typed Intents to the shared graph; a scheduler/solver claims them.

This module is intentionally LLM-agnostic and side-effect-light so it's unit-
testable with a ScriptedLLM (no API key needed).
"""

from __future__ import annotations

import hashlib
import json
import os
import re
import uuid
from difflib import SequenceMatcher
from dataclasses import dataclass, field, replace
from enum import Enum
from typing import Any, Final, Optional

import httpx

from muteki.core.prompt_assembly import estimate_host_tokens
from muteki.swarm.graph_defs import WORKER_RUNTIME_CAPABILITY_KEYS

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
    expected_observable: str = ""
    stop_condition: str = ""
    coverage_key: str = ""
    priority: str = "normal"  # high | normal | low
    value_claim: dict[str, Any] = field(default_factory=dict)
    requires_capabilities: list[str] = field(default_factory=list)
    required_pocs: list[str] = field(default_factory=list)
    requested_priority: str = "normal"
    priority_reason: str = ""
    # Explicit initial-step marker from the planner payload. Exempts a
    # source-less intent from the orphan filter in dispatch_intents (cold
    # start is the other exemption). Absent/False is the ordinary case.
    initial: bool = False
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
        if self.expected_observable:
            payload["expected_observable"] = self.expected_observable
        if self.stop_condition:
            payload["stop_condition"] = self.stop_condition
        if self.coverage_key:
            payload["coverage_key"] = self.coverage_key
        payload["priority"] = self.priority
        payload["requested_priority"] = self.requested_priority or self.priority
        if self.priority_reason:
            payload["priority_reason"] = self.priority_reason
        if self.value_claim:
            payload["value_claim"] = self.value_claim
            if self.value_claim.get("novelty_key"):
                payload["novelty_key"] = self.value_claim["novelty_key"]
        if self.requires_capabilities:
            payload["requires_capabilities"] = self.requires_capabilities
        if self.required_pocs:
            payload["required_pocs"] = self.required_pocs
        if self.initial:
            payload["initial"] = True
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
    TIMEOUT = "planner_timeout"
    EXCEPTION = "planner_exception"
    INVALID_PLAN = "invalid_plan"
    EMPTY_PLAN = "empty_plan"
    NEEDS_NEW_INFORMATION = "needs_new_information"


@dataclass(frozen=True)
class PlannerFailure:
    kind: PlannerFailureKind
    detail: str = ""


@dataclass
class ReasonDiagnostics:
    """Exact outcome of one planner call, independent of dispatch results."""

    response_status: str = "not_started"
    timed_out: bool = False
    finish_reason: str = ""
    raw_response: str = ""
    raw_response_sha256: str = ""
    response_chars: int = 0
    parse_status: str = "not_run"
    parse_detail: str = ""
    raw_intent_count: int = 0
    parsed_intent_count: int = 0
    input_tokens: int = 0
    output_tokens: int = 0


@dataclass
class ReasonResult:
    goal_met: bool
    intents: list[Intent]
    audit_notes: list[Any]  # bounded fact checks requested by DECIDE
    verdict: str = VERDICT_EXPLORE  # complete | course_correct | explore
    drift: str = ""  # if course_correct: what went wrong + the fix
    complete_why: str = ""  # if complete: why the goal is already met
    progress_summary: str = ""  # operator-facing synthesis from this Reason pass
    progress_sections: dict[str, list[str]] = field(default_factory=dict)
    semantic_dedupe_available: bool = False
    pinned_facts: list[int] = field(default_factory=list)
    supersede_intents: list[str] = field(default_factory=list)
    supersede_why: str = ""
    reprioritize_intents: dict[str, str] = field(default_factory=dict)
    planner_failure: PlannerFailure | None = None
    cognitive_hypotheses: list[CognitiveHypothesisDraft] = field(default_factory=list)
    diagnostics: ReasonDiagnostics = field(default_factory=ReasonDiagnostics)
    intent_parse_rejections: list[dict[str, Any]] = field(default_factory=list)


REASON_SYSTEM = """你负责读取完整 Fact-Goal-Step 图并决定下一步，不执行任务。

只返回 JSON：
{"verdict":"explore|complete","goal_met":false,"complete_why":"",
"progress":{"summary":""},"intents":[
{"id":"I1","from":[],"goal":"当前可执行的方向","priority":"high|normal|low"}]}

规则：
- Goal 已由图中的 Fact 或平台结果满足时返回 complete。
- 尚未完成时创建最多 {max_intents} 个当前可执行、尽量互补的 Step。
- Step 只描述要向目标取的那条信息，不写分步方法；一个 Step 只产出一个可独立交接的结果。
- 同批 Step 没有先后顺序，全部前置必须已经是 Fact。后续动作依赖尚未形成 Fact 的共享入口时，留到下一轮再创建。
- from 可以引用相关 Fact id；冷启动时可以为空。
- 不创建能力、资源、访问路径、风险、路线、覆盖、审计或抢占合同。
- progress.summary 使用简体中文概括当前进展和下一步。
- 只输出 JSON。"""

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
  complete exploit report through the Blackboard Skill, a different report
  reproducer confirms it, and a host-side value check rejects self-XSS /
  attacker-only / informational issues. Planner prose, this complete_why text,
  and Review summaries are not reports.
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
	  "progress": {
	    "summary": "<2-4 plain-language sentences: current judgment, key basis, blocker, and next move>",
	    "confirmed": ["<synthesized confirmed conclusion, cite [#N] when useful>"],
	    "active": ["<what is being tested and why>"],
	    "blocked": ["<ruled-out direction or current blocker>"],
	    "next": ["<highest-value next action and expected result>"]
	  },
	  "intents": [
	    {"id": "I1", "from": [3, 7], "goal": "<one concrete, independent next direction>",
	     "worker_class": "code", "route_hash": "web:login:sqli", "branch_id": "",
	     "priority": "high|normal|low",
	     "lane_key": "", "risk_class": "",
	     "depends_on": [], "rationale": "<why>", "dup_of": null,
	     "reopen_because": "",
	     "expected_observable": "<objective output that proves success or failure>",
	     "stop_condition": "<when this step is finished — do not expand further>",
	     "coverage_key": "", "requires_capabilities": [],
	     "value_claim": {"effect": "terminal|capability_advance|shared_enablement|branch_resolution|coverage",
	       "capability_before": [], "capability_after": [], "unblocks": [],
	       "consumer_capabilities": [], "novelty_key": "<stable new-state key>",
	       "cost_class": "small|medium|large", "risk_class": "read|mutating|exclusive",
	       "critical_path": false}}
	  ],
	  "supersede_intents": ["<exact open or claimed intent id made obsolete by new evidence>"],
	  "supersede_why": "<cite the new fact(s) that make those queued steps obsolete>",
	  "pinned_facts": [3, 7],
	  "audit": ["<finding text you do NOT trust and why>"]
	}

Rules:
- `progress` is the operator-facing status report from this Reason pass. Write it
  in concise Simplified Chinese, keep technical identifiers verbatim, and synthesize
  the state instead of copying graph rows. Keep each list to at most 3 items.
- Each intent MUST include a "from" array of fact sequence numbers (the [#N] tags
  in the evidence list) that motivated this direction. Use the exact numbers.
- Propose at most {max_intents} INDEPENDENT, NON-OVERLAPPING intents (distinct
  directions, not minor variations of one). Prefer 1-2 high-value steps unless
  the board still has several clearly independent attack surfaces. Each should
  be a clear high-value direction — focus on the core insight, do not
  over-specify the steps; trust the executor to be the expert.
- One intent owns one coherent immediate stage. Bundle adjacent file reads,
  route variants, host probes, or configuration checks that use the same
  primitive and answer the same question; do not split them into parallel siblings.
- Fresh operational evidence has first claim on the next step. A credential,
  secret, key, execution primitive, or reachable service must be consumed directly
  before unrelated coverage expands. When such evidence is still a tool-backed
  candidate, use one bounded verify-then-use intent instead of a standalone verifier
  followed by a later consumer.
- Advance verified capabilities in order: discovery, read, controlled write,
  execution, identity/credentials, topology, pivot, objective. As soon as verified
  facts satisfy the prerequisites for the next capability, the first intent MUST be
  the smallest bounded promotion step; broad enumeration becomes low priority or is
  superseded. Do not keep searching for more instances of a capability already proven.
- Read the trailing "Immediate planning frontier" before choosing work. A concluded
  discovery intent closes only that exact step; it does not close a newly exposed
  child capability. When source, configuration, or tool output names a concrete
  promotion technique, validate that technique before speculative enumeration and
  give the child step its own route_hash and, when mutating, its required lane_key.
- `progress.next` and `intents` must agree. Every executable action listed in
  `progress.next` must have a matching intent in the same response, and the first
  next action must match the first intent. Before returning, replace any intent that
  duplicates an open, claimed, or attempted route; never leave the novel action only
  in progress prose.
- Use shell_agent for a proven multi-stage chain whose intermediate target state is
  volatile or expensive to recreate; let it reach the next stable, publishable
  handoff. Keep independent discovery and one-check experiments as code Workers.
- For a pivot, an address found in page text or configuration is a clue, not
  runtime topology. If current interfaces, routes, DNS, or neighbors are not yet
  established, map them first; target only a verified reachable endpoint afterward.
- On a cold start with no verified facts, propose at least 3 immediately
  executable, complementary surface-level steps when max_intents permits it.
  Use depends_on=[] for every cold-start step: do not queue a future step that
  waits for another proposed step. Whole-challenge race-scout intents are
  background scouts, not proof that a concrete surface is already covered.
- The Coordinator has already admitted the target and configured direct routing.
  Do not spend an intent solely checking VPN, proxy variables, shell availability,
  or basic reachability. A worker should report an actual access failure if one occurs.
- expected_observable is the objective output that would prove the step
  succeeded or failed. stop_condition says when the worker must conclude
  instead of expanding the step. coverage_key is an optional surface label;
  it is one part of the step contract, not a unique identity by itself.
- Every intent must include a structured value_claim and requires_capabilities.
  requires_capabilities may contain only exact keys already listed under Active
  Reusable Capabilities. Local shell, HTTP clients, admitted target routing,
  VPN/proxy setup, and Toolbox files are host-provided primitives: never list
  them as requirements. If a missing reusable capability is needed, propose an
  immediately executable shared_enablement Step with requires_capabilities=[]
  and declare that new key in capability_after; do not queue its consumer yet.
  A high request is accepted only when the claim cites active Fact evidence and
  would complete the goal, add a missing next capability, or establish one
  reusable capability for at least two downstream consumers. Repeated coverage
  and already-active capabilities are normal or low priority.
- When several later intents would repeat an expensive access procedure, open a
  shared_enablement intent. Describe the required reusable reach and operations;
  the Worker selects an implementation from ./toolbox and publishes the resulting
  AccessPath after an actual health check.
- Set priority="high" only for a step directly unlocked by new verified evidence
  or required to continue a proven exploit chain. Use "normal" for independent
  current work and "low" for broad fallback coverage.
- When new verified evidence makes an existing intent obsolete, list its exact id
  in supersede_intents and cite the replacing fact in supersede_why. A claimed/running
  intent may be listed only when the replacement is a high-priority step on the same
  exclusive lane_key; the Coordinator will stop that exact stale owner before the
  replacement runs. Never supersede work merely to reword or reprioritize it.
- Propose at most one new intent for each non-empty lane_key in a Reason pass.
- An exclusive lane is one coherent stage. Do not retry the same lane_key from
  the same from-fact set under a rewritten route or goal. Cite at least one new
  fact sequence when fresh evidence genuinely reopens that resource direction.
- After a verified credential, execution primitive, or pivot unlocks a concrete
  chain, supersede queued broad fallback scans that no longer deserve capacity.
  Only one open/claimed intent may own a coverage_key. reopen_because applies to
  completed attempts changed by new evidence; it never authorizes duplicate live work.
- Treat live race-scout assignments as active work when avoiding collisions.
  In particular, do not duplicate a state-mutating action already owned by a race
  scout even though its whole-surface assignment has no precise coverage_key.
- Start with supplied files and existing evidence. Do not require scanning or
  wordlist enumeration merely to fill coverage gaps. Record what remains
  unknown instead of automatically expanding the search.
- Step identity is the exact combination of route_hash, from_facts,
  coverage_key, expected_observable, and stop_condition. Do not re-propose the
  same five-field contract. When new verified evidence changes a step, cite its
  Fact id in from_facts and explain the change in reopen_because.
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
  Log poisoning and writes to a shared upload path are exclusive too; use stable
  keys such as destructive:http-log:80@host or destructive:http-upload:80@host.
  Do NOT include the exploit technique in lane_key.
- Rows under "Observations" are audit input and never support a vulnerability or
  an ordinary intent. Only active Facts with [#seq] identifiers may appear in
  from_facts. A useful Observation must first be reproduced with admitted tool
  evidence so the host creates a Fact.
- Findings in the graph carry [#seq] labels. Put fact seqs in
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


REASON_COMPACTION_RESERVE_TOKENS = 16_384
REASON_KEEP_RECENT_TOKENS = 20_000

REASON_COMPACTION_SYSTEM = """You compact historical state for a long-running CTF or pentest Reason planner.
Return a structured checkpoint that another Reason call can use instead of the older graph rows.
Do not propose new work, declare completion, or invent evidence. Preserve every cited [#seq], exact
credential, host, port, path, payload fragment, flag, scope restriction, negation, and unresolved
contradiction. Merge duplicate attempts by route while retaining why each route was ruled out.

Use exactly these sections:
## Goal Progress
## Durable Facts
## Ruled-Out Routes
## Constraints and Scope
## Open Questions
## Critical Exact Values
## Next Planning Context
"""


@dataclass(frozen=True)
class ReasonCompactionResult:
    summary: str
    input_tokens: int
    output_tokens: int


def estimate_reason_messages_tokens(messages: list[dict[str, Any]]) -> int:
    return sum(estimate_host_tokens(str(message.get("content") or "")) for message in messages)


def reason_failure_is_context_overflow(result: ReasonResult) -> bool:
    failure = getattr(result, "planner_failure", None)
    detail = str(getattr(failure, "detail", "") or "").lower()
    return any(token in detail for token in (
        "context_length_exceeded", "context length", "context window",
        "maximum context", "too many tokens", "prompt is too long",
    ))


async def compact_reason_context(
    *, llm: Any, model: str, graph_context: str,
    previous_summary: str = "", run_id: Optional[str] = None,
    challenge_id: Optional[str] = None,
) -> ReasonCompactionResult:
    prompt = f"<graph-context>\n{graph_context}\n</graph-context>\n"
    if previous_summary:
        prompt += (
            f"\n<previous-summary>\n{previous_summary}\n</previous-summary>\n"
            "Update the previous checkpoint with the graph context above. Preserve all still-valid "
            "information and remove only items explicitly superseded by cited graph state."
        )
    else:
        prompt += "\nCreate the first checkpoint from the graph context above."
    response = await llm.chat(
        model=model,
        messages=[
            {"role": "system", "content": REASON_COMPACTION_SYSTEM},
            {"role": "user", "content": prompt},
        ],
        reasoning_effort="none",
        max_tokens=REASON_COMPACTION_RESERVE_TOKENS,
        stream=False,
        run_id=run_id,
        challenge_id=challenge_id,
        solver_id="reason-compact",
    )
    finish_reason = str(getattr(response, "finish_reason", "") or "").lower()
    summary = str(getattr(response, "content", "") or "").strip()
    if finish_reason == "length" or not summary or getattr(response, "has_tool_calls", False):
        raise RuntimeError("reason context compaction returned an incomplete checkpoint")
    return ReasonCompactionResult(
        summary=summary,
        input_tokens=int(getattr(response, "input_tokens", 0) or 0),
        output_tokens=int(getattr(response, "output_tokens", 0) or 0),
    )


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
    # pentest → goal-driven planner (the operator's engagement goal anchors the
    # `complete` verdict). CTF uses the compact graph-planning contract above.
    if mode == "pentest":
        user = f"Engagement goal:\n{goal}\n\n" if (goal or "").strip() else ""
        if (scope or "").strip():
            user += f"Engagement scope:\n{scope.strip()}\n\n"
        user += (
            f"Shared findings-graph:\n\n{summary}\n\n"
            "Output the planning JSON."
        )
        system = REASON_SYSTEM_PENTEST.replace("{max_intents}", str(max_intents))
        return [
            {"role": "system", "content": system},
            {"role": "user", "content": user},
        ]
    system = REASON_SYSTEM.replace("{max_intents}", str(max_intents))
    return [
        {"role": "system", "content": system},
        {
            "role": "user",
            "content": f"共享求解图：\n\n{summary}\n\n只输出规划 JSON。",
        },
    ]


def _intent_execution_body(intent: Intent) -> dict[str, object]:
    """Everything the ordinary dispatcher can observe, excluding shadow fields."""

    body: dict[str, object] = {
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
        "priority": intent.priority,
        "value_claim": intent.value_claim,
        "requires_capabilities": tuple(intent.requires_capabilities),
    }
    if intent.expected_observable:
        body["expected_observable"] = intent.expected_observable
    if intent.stop_condition:
        body["stop_condition"] = intent.stop_condition
    if intent.coverage_key:
        body["coverage_key"] = intent.coverage_key
    return body


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


def _extract_json_diagnostic(text: str) -> tuple[dict, str, str]:
    """Pull the first JSON object and retain the exact parse outcome."""
    if not str(text or "").strip():
        return {}, "empty_response", "planner response was empty"
    # strip ```json fences
    text = re.sub(r"```(?:json)?", "", text)
    m = re.search(r"\{.*\}", text, re.DOTALL)
    if not m:
        return {}, "json_object_not_found", "no JSON object found in planner response"
    try:
        parsed = json.loads(m.group(0))
    except json.JSONDecodeError as exc:
        return {}, "json_decode_error", (
            f"{exc.msg} at line {exc.lineno} column {exc.colno}"
        )
    if not isinstance(parsed, dict):
        return {}, "json_root_not_object", "planner JSON root was not an object"
    return parsed, "parsed", ""


def _extract_json(text: str) -> dict:
    """Compatibility wrapper for callers that only need the decoded object."""
    parsed, _status, _detail = _extract_json_diagnostic(text)
    return parsed


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
    finish_reason: str = "",
    max_intents: int = 4,
    allow_declares: bool = False,
    allow_declares_v2: bool = False,
    declaration_target_catalog_v2: frozenset[
        tuple[str, str, str, str, str]
    ] | None = None,
) -> ReasonResult:
    if allow_declares and allow_declares_v2:
        raise ValueError("Reason declaration v1 and v2 parsers are mutually exclusive")
    raw_response = str(text or "")
    d, parse_status, parse_detail = _extract_json_diagnostic(raw_response)
    plan_is_valid = bool(d) and isinstance(d.get("intents"), list)
    if parse_status == "parsed" and not plan_is_valid:
        parse_status = "schema_invalid"
        parse_detail = "planner JSON did not contain the required intents array"
    goal_met = bool(d.get("goal_met", False))
    intents: list[Intent] = []
    parse_rejections: list[dict[str, Any]] = []
    raw_intents = d.get("intents", []) if isinstance(d.get("intents"), list) else []
    for i, raw in enumerate(raw_intents[:max_intents]):
        if not isinstance(raw, dict):
            parse_rejections.append({
                "raw_index": i,
                "model_intent_id": "",
                "goal": "",
                "outcome": "dropped",
                "stage": "parse",
                "reason_code": "intent_not_object",
            })
            continue
        goal = str(raw.get("goal", "")).strip()
        if not goal:
            parse_rejections.append({
                "raw_index": i,
                "model_intent_id": str(raw.get("id") or f"I{i + 1}"),
                "goal": "",
                "outcome": "dropped",
                "stage": "parse",
                "reason_code": "missing_goal",
            })
            continue
        wc = str(raw.get("worker_class", "code"))
        if wc not in ("code", "shell_agent", "verifier", "review"):
            wc = "code"
        priority = str(raw.get("priority") or "normal").strip().lower()
        if priority not in {"high", "normal", "low"}:
            priority = "normal"
        from_raw = raw.get("from", [])
        from_facts = [int(x) for x in from_raw if isinstance(x, (int, float))]
        raw_value_claim = raw.get("value_claim")
        value_claim = dict(raw_value_claim) if isinstance(raw_value_claim, dict) else {}
        if value_claim:
            value_claim["effect"] = str(value_claim.get("effect") or "").strip().lower()
            for key in ("capability_before", "capability_after", "unblocks",
                        "consumer_capabilities"):
                value_claim[key] = list(dict.fromkeys(
                    str(item).strip().casefold()
                    for item in (value_claim.get(key) or [])
                    if str(item).strip()
                ))[:32]
            value_claim["novelty_key"] = str(
                value_claim.get("novelty_key") or ""
            ).strip().casefold()[:300]
            value_claim["cost_class"] = str(
                value_claim.get("cost_class") or "medium"
            ).strip().lower()
            value_claim["risk_class"] = str(
                value_claim.get("risk_class") or "read"
            ).strip().lower()
            value_claim["critical_path"] = bool(value_claim.get("critical_path"))
        requires_capabilities = list(dict.fromkeys(
            str(item).strip().casefold()
            for item in (raw.get("requires_capabilities") or [])
            if str(item).strip()
        ))[:32]
        route_hash = str(raw.get("route_hash") or "").strip()
        coverage_key = str(raw.get("coverage_key") or "").strip()
        if not coverage_key and wc != "review":
            # Keep an otherwise-valid planner step schedulable while still
            # giving the durable deduper a stable key. The system prompt asks
            # for the more precise explicit value; route is the safe fallback.
            coverage_key = route_hash
        intents.append(
            Intent(
                intent_id=str(raw.get("id") or f"I{i + 1}"),
                goal=goal,
                worker_class=wc,
                depends_on=[str(x) for x in raw.get("depends_on", []) if x],
                rationale=str(raw.get("rationale", "")),
                from_facts=from_facts,
                route_hash=route_hash,
                branch_id=str(raw.get("branch_id") or "").strip(),
                lane_key=str(raw.get("lane_key") or "").strip(),
                risk_class=str(raw.get("risk_class") or "").strip(),
                resource_key=str(raw.get("resource_key") or "").strip(),
                dup_of=str(raw.get("dup_of") or "").strip(),
                reopen_because=str(raw.get("reopen_because") or "").strip(),
                expected_observable=str(raw.get("expected_observable") or "").strip(),
                stop_condition=str(raw.get("stop_condition") or "").strip(),
                coverage_key=coverage_key,
                priority=priority,
                value_claim=value_claim,
                requires_capabilities=requires_capabilities,
                requested_priority=priority,
                initial=bool(raw.get("initial")),
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
    for i, raw in enumerate(raw_intents[max_intents:], start=max_intents):
        parse_rejections.append({
            "raw_index": i,
            "model_intent_id": (
                str(raw.get("id") or f"I{i + 1}")
                if isinstance(raw, dict) else ""
            ),
            "goal": str(raw.get("goal") or "") if isinstance(raw, dict) else "",
            "outcome": "dropped",
            "stage": "parse",
            "reason_code": "max_intents_exceeded",
        })
    audit: list[dict[str, Any]] = []
    raw_audit = d.get("audit", [])
    if not isinstance(raw_audit, list):
        raw_audit = []
    for raw in raw_audit[:8]:
        if isinstance(raw, dict):
            try:
                fact_seq = int(raw.get("fact") or 0)
            except (TypeError, ValueError):
                fact_seq = 0
            reason = " ".join(str(raw.get("reason") or "").split())[:500]
            goal = " ".join(
                str(raw.get("verification_goal") or "").split()
            )[:1000]
        else:
            text = " ".join(str(raw or "").split())[:1000]
            match = re.search(r"\[#(\d+)\]", text)
            fact_seq = int(match.group(1)) if match else 0
            reason = text
            goal = f"Independently verify fact #{fact_seq}: {text}" if fact_seq else ""
        if fact_seq > 0 and reason and goal:
            audit.append({
                "fact": fact_seq,
                "reason": reason,
                "verification_goal": goal,
            })
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
    supersede_intents: list[str] = []
    raw_supersede = d.get("supersede_intents", [])
    if not isinstance(raw_supersede, list):
        raw_supersede = []
    for raw in raw_supersede:
        intent_id = str(raw or "").strip()[:180]
        if intent_id and intent_id not in supersede_intents:
            supersede_intents.append(intent_id)
        if len(supersede_intents) >= 24:
            break
    supersede_why = " ".join(
        str(d.get("supersede_why") or "").split()
    )[:1000]
    reprioritize_intents: dict[str, str] = {}
    raw_reprioritize = d.get("reprioritize_intents")
    if isinstance(raw_reprioritize, dict):
        for raw_id, raw_priority in raw_reprioritize.items():
            intent_id = str(raw_id or "").strip()[:180]
            priority = str(raw_priority or "").strip().lower()
            if intent_id and priority in {"high", "normal", "low"}:
                reprioritize_intents[intent_id] = priority
            if len(reprioritize_intents) >= 24:
                break
    raw_progress = d.get("progress")
    progress = raw_progress if isinstance(raw_progress, dict) else {}
    progress_summary = " ".join(
        str(progress.get("summary") or "").split()
    )[:1600]
    progress_sections: dict[str, list[str]] = {}
    for key in ("confirmed", "active", "blocked", "next"):
        rows: list[str] = []
        raw_rows = progress.get(key)
        if isinstance(raw_rows, list):
            for raw in raw_rows:
                text = " ".join(str(raw or "").split())[:500]
                if text and text not in rows:
                    rows.append(text)
                if len(rows) >= 3:
                    break
        progress_sections[key] = rows
    # Rolling upgrades and weaker JSON models may omit the new progress object.
    # Keep the fallback strictly within this Reason response; never substitute
    # raw graph rows and present them as a model-authored summary.
    audit_texts = [str(row.get("reason") or "") for row in audit]
    if not progress_summary:
        rationales = [
            " ".join(str(intent.rationale or "").split())
            for intent in intents if str(intent.rationale or "").strip()
        ]
        progress_summary = " ".join(
            value for value in (
                complete_why,
                drift,
                "；".join(rationales[:2]),
                "；".join(intent.goal for intent in intents[:2]),
                "；".join(audit_texts[:2]),
            ) if value
        )[:1600]
    if not progress_sections["blocked"]:
        progress_sections["blocked"] = (
            [drift[:500]] if drift else audit_texts[:3]
        )
    if not progress_sections["next"]:
        progress_sections["next"] = [
            " ".join(intent.goal.split())[:500] for intent in intents[:3]
        ]
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
        progress_summary=progress_summary,
        progress_sections=progress_sections,
        semantic_dedupe_available=plan_is_valid,
        pinned_facts=pinned_facts,
        supersede_intents=supersede_intents,
        supersede_why=supersede_why,
        reprioritize_intents=reprioritize_intents,
        planner_failure=planner_failure,
        cognitive_hypotheses=_parse_cognitive_hypotheses(d.get("cognitive_hypotheses")),
        diagnostics=ReasonDiagnostics(
            response_status="received" if raw_response.strip() else "empty",
            timed_out=False,
            finish_reason=str(finish_reason or ""),
            raw_response=raw_response,
            raw_response_sha256=hashlib.sha256(
                raw_response.encode("utf-8", errors="replace")
            ).hexdigest(),
            response_chars=len(raw_response),
            parse_status=parse_status,
            parse_detail=parse_detail,
            raw_intent_count=len(raw_intents),
            parsed_intent_count=len(intents),
        ),
        intent_parse_rejections=parse_rejections,
    )


async def run_reason(
    *,
    llm: Any,
    model: str,
    graph_summary: str,
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
        goal=goal,
        mode=mode,
        scope=scope,
        cognitive_shadow=cognitive_shadow,
        declaration_target_catalog_v2=declaration_target_catalog_v2,
        declaration_mode=declaration_mode,
    )
    # The planner returns a small structured decision. Keep the output cap absent,
    # and disable extended reasoning so an OpenAI-compatible provider cannot spend
    # its whole default generation allowance on reasoning_content before emitting
    # the JSON body. The coordinator owns retries for transport/parse failures.
    try:
        resp = await llm.chat(
            model=model,
            messages=messages,
            reasoning_effort="none",
            stream=False,
            emit_events=False,
            run_id=run_id,
            challenge_id=challenge_id,
            solver_id="reason",
        )
    except (TimeoutError, httpx.TimeoutException) as exc:
        detail = f"{type(exc).__name__}: {exc}"[:500]
        return ReasonResult(
            goal_met=False,
            intents=[],
            audit_notes=[],
            semantic_dedupe_available=False,
            planner_failure=PlannerFailure(
                PlannerFailureKind.TIMEOUT,
                detail,
            ),
            diagnostics=ReasonDiagnostics(
                response_status="timeout",
                timed_out=True,
                finish_reason="timeout",
                raw_response_sha256=hashlib.sha256(b"").hexdigest(),
                parse_status="not_run_timeout",
                parse_detail=detail,
            ),
        )
    except Exception as exc:
        detail = f"{type(exc).__name__}: {exc}"[:500]
        return ReasonResult(
            goal_met=False,
            intents=[],
            audit_notes=[],
            semantic_dedupe_available=False,
            planner_failure=PlannerFailure(
                PlannerFailureKind.EXCEPTION,
                detail,
            ),
            diagnostics=ReasonDiagnostics(
                response_status="error",
                finish_reason="exception",
                raw_response_sha256=hashlib.sha256(b"").hexdigest(),
                parse_status="not_run_exception",
                parse_detail=detail,
            ),
        )
    finish_reason = str(getattr(resp, "finish_reason", "") or "").strip()
    raw_response = str(getattr(resp, "content", "") or "")
    if finish_reason.lower() == "timeout":
        return ReasonResult(
            goal_met=False,
            intents=[],
            audit_notes=[],
            semantic_dedupe_available=False,
            planner_failure=PlannerFailure(
                PlannerFailureKind.TIMEOUT,
                "planner request exceeded its overall timeout",
            ),
            diagnostics=ReasonDiagnostics(
                response_status="timeout",
                timed_out=True,
                finish_reason=finish_reason,
                raw_response=raw_response,
                raw_response_sha256=hashlib.sha256(
                    raw_response.encode("utf-8", errors="replace")
                ).hexdigest(),
                response_chars=len(raw_response),
                parse_status="not_run_timeout",
                parse_detail="planner request exceeded its overall timeout",
            ),
        )
    resolved_mode = resolve_declaration_mode(declaration_mode)
    result = parse_reason_reply(
        raw_response,
        finish_reason=finish_reason,
        max_intents=max_intents,
        allow_declares=resolved_mode == "v1",
        allow_declares_v2=resolved_mode == "v2",
        declaration_target_catalog_v2=declaration_target_catalog_v2,
    )
    result.diagnostics.input_tokens = int(getattr(resp, "input_tokens", 0) or 0)
    result.diagnostics.output_tokens = int(getattr(resp, "output_tokens", 0) or 0)
    return result


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
        stream=False,
        run_id=run_id,
        challenge_id=challenge_id,
        solver_id="reason-cognitive-shadow",
    )
    return parse_cognitive_shadow_annotation_reply(
        getattr(response, "content", "") or "",
        baseline_result=baseline_result,
    )


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


def _lane_key(shared_graph: Any, lane_key: str) -> str:
    lane = str(lane_key or "").strip()
    if not lane:
        return ""
    norm = getattr(shared_graph, "normalize_lane_key", None)
    if callable(norm):
        try:
            return str(norm(lane) or "")
        except Exception:
            pass
    return lane.lower()


_CTF_EXCLUSIVE_LANE_RE = re.compile(
    r"^(destructive|exclusive_shell|listener_port|relay_service|rate_limited):"
    r"[^:@]+:[0-9*]+@[^@]+$"
)


def _sanitize_ctf_exclusive_lane(intent: Intent) -> None:
    """Ignore planner labels that are not actual exclusive resources.

    ``lane_key`` is a lock identity, not a semantic grouping label.  Treating
    values such as ``http:root`` as locks collapses otherwise independent
    cold-start work into one Worker.  Branch-owned lanes are copied later from
    durable graph state and therefore do not pass through this model-output
    guard.
    """
    lane = str(intent.lane_key or "").strip().lower()
    match = _CTF_EXCLUSIVE_LANE_RE.fullmatch(lane)
    if match is None:
        intent.lane_key = ""
        intent.risk_class = ""
        return
    intent.lane_key = lane
    intent.risk_class = match.group(1)


def _route_is_owned(route: str, owned_routes: set[str]) -> bool:
    """Return whether an active hierarchical route already owns this work.

    Planner route hashes follow ``category:surface:technique``.  A later
    ``:advanced`` or ``:verify`` suffix refines the same live direction and must
    wait for its current owner; sibling techniques remain independent.  Requiring
    three components keeps malformed broad labels such as ``web`` from blocking
    an entire category.
    """
    if not route:
        return False
    for owned in owned_routes:
        if route == owned:
            return True
        shorter, longer = (
            (route, owned) if len(route) < len(owned) else (owned, route)
        )
        if shorter.count(":") >= 2 and longer.startswith(shorter + ":"):
            return True
    return False


def _step_text_norm(value: Any) -> str:
    """strip + collapse whitespace + casefold — mirrors the graph-layer
    normalization in equivalent_step_keys so both sides build the same key."""
    return " ".join(str(value or "").split()).casefold()


def _same_active_step_stage(intent: Intent, row: dict[str, Any]) -> bool:
    """Return whether ``intent`` only restates or narrows a live Step.

    Coverage and route are the primary contract. A planner can still rename a
    subset by appending a suffix, so an identical expected receipt plus strongly
    overlapping bounded action also counts as the same stage. This comparison is
    domain-neutral and never infers a task-specific route.
    """
    new_coverage = _step_text_norm(intent.coverage_key)
    old_coverage = _step_text_norm(row.get("coverage_key"))
    new_route = _step_text_norm(intent.route_hash) or "__unspecified__"
    old_route = _step_text_norm(row.get("route_hash")) or "__unspecified__"
    if new_coverage and new_coverage == old_coverage and new_route == old_route:
        return True
    new_expected = _step_text_norm(intent.expected_observable)
    old_expected = _step_text_norm(row.get("expected_observable"))
    new_goal = _step_text_norm(intent.goal)
    old_goal = _step_text_norm(row.get("goal"))
    if not (new_expected and new_expected == old_expected and new_goal and old_goal):
        return False
    return SequenceMatcher(None, new_goal, old_goal).ratio() >= 0.84


def _structural_step_key(shared_graph: Any, it: Intent) -> tuple:
    """The single canonical identity used by dispatch and persistence.

    The model-provided id and goal wording are labels, not identity.  A step is
    defined by its evidence inputs, normalized route, coverage, expected
    observable, and stop condition.  Keeping this helper shared prevents the
    duplicate filter from disagreeing with the durable intent id.
    """
    return (
        _step_text_norm(_route_key(shared_graph, it.route_hash)),
        tuple(sorted({int(x) for x in (it.from_facts or [])})),
        _step_text_norm(it.coverage_key),
        _step_text_norm(it.expected_observable),
        _step_text_norm(it.stop_condition),
    )


def _unique_intent_id(shared_graph: Any, it: Intent) -> str:
    key = _structural_step_key(shared_graph, it)
    seed = json.dumps(key, ensure_ascii=False, separators=(",", ":"))
    digest = hashlib.sha256(seed.encode("utf-8")).hexdigest()[:16]
    return f"I-reason-{digest}"


_VALUE_EFFECTS = {
    "terminal", "capability_advance", "shared_enablement",
    "branch_resolution", "coverage",
}


def derive_intent_priority(
    shared_graph: Any,
    intent: Intent,
    *,
    verified_fact_seqs: set[int],
    cold_start: bool,
    active_capability_keys: set[str],
    occupied_novelty_keys: set[str],
) -> tuple[str, str]:
    """Host-authoritative priority derived from a structured ValueClaim."""
    requested = str(intent.priority or "normal").lower()
    intent.requested_priority = requested
    claim = intent.value_claim if isinstance(intent.value_claim, dict) else {}
    effect = str(claim.get("effect") or "").strip().lower()
    novelty = str(claim.get("novelty_key") or "").strip().casefold()
    after = {
        str(item).strip().casefold()
        for item in (claim.get("capability_after") or [])
        if str(item).strip()
    }
    required = {
        str(item).strip().casefold()
        for item in (intent.requires_capabilities or [])
        if str(item).strip()
    }
    if required - active_capability_keys:
        return "normal", "required_capability_not_active"
    source_ok = bool(set(intent.from_facts or []) & verified_fact_seqs)
    if not claim or effect not in _VALUE_EFFECTS:
        effective = "normal" if cold_start else "low"
        return effective, "missing_or_invalid_value_claim"
    if novelty and novelty in occupied_novelty_keys:
        return "low", "novelty_already_owned"
    if after and after.issubset(active_capability_keys):
        return "low", "capability_already_active"
    if effect == "coverage":
        return ("normal" if cold_start or requested != "low" else "low"), "coverage"
    if effect == "branch_resolution":
        critical = bool(claim.get("critical_path"))
        unblocks = {str(item) for item in (claim.get("unblocks") or []) if str(item)}
        if source_ok and critical and unblocks:
            return "high", "critical_branch_resolution"
        return "normal", "noncritical_branch_resolution"
    if effect == "shared_enablement":
        consumers = {
            str(item).strip().casefold()
            for key in ("unblocks", "consumer_capabilities")
            for item in (claim.get(key) or [])
            if str(item).strip()
        }
        if source_ok and novelty and after and len(consumers) >= 2:
            return "high", "validated_shared_enablement"
        return "normal", "shared_enablement_missing_consumers_or_evidence"
    if effect == "capability_advance":
        if source_ok and novelty and after:
            return "high", "validated_capability_advance"
        return "normal", "capability_advance_missing_evidence_or_novelty"
    if effect == "terminal":
        if source_ok and novelty:
            return "high", "validated_terminal_step"
        return "normal", "terminal_missing_evidence_or_novelty"
    return "low", "unsupported_value_claim"


def _propose_one(
    shared_graph: Any,
    it: Intent,
    *,
    actor: str,
    depends_on: list[str] | None = None,
) -> Optional[dict[str, Any]]:
    iid = _unique_intent_id(shared_graph, it)
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
        "depends_on": list(depends_on or []),
        "route_hash": it.route_hash,
        "branch_id": it.branch_id,
        "expected_observable": it.expected_observable,
        "stop_condition": it.stop_condition,
        "coverage_key": it.coverage_key,
        "lane_key": _lane_key(shared_graph, it.lane_key),
        "risk_class": it.risk_class,
        "resource_key": it.resource_key,
        "priority": it.priority,
        "requested_priority": it.requested_priority,
        "priority_reason": it.priority_reason,
        "value_claim": dict(it.value_claim),
        "requires_capabilities": list(it.requires_capabilities),
        "dup_of": it.dup_of,
        "reopen_because": it.reopen_because,
        "declares": dict(it.declared_effects) if it.declared_effects else None,
    }


def dispatch_intents(
    shared_graph: Any,
    result: ReasonResult,
    *,
    actor: str = "reason",
    decision_log: Optional[list[dict[str, Any]]] = None,
) -> list[dict[str, Any]]:
    """Push reason's intents into the shared graph as claimable tasks.

    Returns the list of intents actually proposed (id/goal/worker_class) so the
    caller can emit blackboard `intent_proposed` events. Dead-ends from audit are
    surfaced via the reason summary (not here).

    Duplicate decisions use the same five-field structural key that generates
    the durable Intent id. Goal wording and the model's temporary id never define
    identity."""
    proposed: list[dict[str, Any]] = []
    raw_indexes = {id(intent): index for index, intent in enumerate(result.intents)}

    def _record(
        intent: Intent,
        *,
        outcome: str,
        reason_code: str,
        created_seq: int = 0,
    ) -> None:
        if decision_log is None:
            return
        decision_log.append({
            "raw_index": raw_indexes.get(id(intent), -1),
            "model_intent_id": intent.intent_id,
            "resolved_intent_id": _unique_intent_id(shared_graph, intent),
            "outcome": outcome,
            "reason_code": reason_code,
            "stage": "dispatch",
            "goal": intent.goal,
            "worker_class": intent.worker_class,
            "depends_on": list(intent.depends_on),
            "rationale": intent.rationale,
            "dup_of": intent.dup_of,
            "reopen_because": intent.reopen_because,
            "route_hash": intent.route_hash,
            "branch_id": intent.branch_id,
            "priority": intent.priority,
            "requested_priority": intent.requested_priority,
            "priority_reason": intent.priority_reason,
            "value_claim": dict(intent.value_claim),
            "coverage_key": intent.coverage_key,
            "from_facts": list(intent.from_facts),
            "created_seq": int(created_seq or 0),
        })

    if shared_graph is None:
        for intent in result.intents:
            _record(intent, outcome="dropped", reason_code="graph_unavailable")
        return proposed
    ctf_mode = (
        getattr(getattr(shared_graph, "challenge", None), "mode", "ctf")
        == "ctf"
    )
    if ctf_mode:
        for raw_index, intent in enumerate(result.intents):
            if not all((
                str(intent.expected_observable or "").strip(),
                str(intent.stop_condition or "").strip(),
                str(intent.coverage_key or "").strip(),
            )):
                if decision_log is not None:
                    decision_log.append({
                        "raw_index": raw_index,
                        "model_intent_id": intent.intent_id,
                        "outcome": "dropped",
                        "reason_code": "missing_step_contract",
                        "stage": "dispatch",
                    })
                continue
            worker_class = (
                intent.worker_class
                if intent.worker_class in {"code", "shell_agent"}
                else "code"
            )
            priority = (
                intent.priority
                if intent.priority in {"high", "normal", "low"}
                else "normal"
            )
            intent_id = f"I-reason-{uuid.uuid4().hex[:16]}"
            seq = shared_graph.propose_intent(
                actor=actor,
                intent_id=intent_id,
                goal=intent.goal,
                payload={
                    "worker_class": worker_class,
                    "priority": priority,
                    "requested_priority": priority,
                    "expected_observable": intent.expected_observable,
                    "stop_condition": intent.stop_condition,
                    "coverage_key": intent.coverage_key,
                    "required_pocs": list(intent.required_pocs),
                },
                from_fact_seqs=intent.from_facts or None,
            )
            if seq < 0:
                continue
            row = {
                "intent_id": intent_id,
                "created_seq": int(seq),
                "goal": intent.goal,
                "worker_class": worker_class,
                "from_facts": list(intent.from_facts),
                "depends_on": [],
                "route_hash": "",
                "branch_id": "",
                "expected_observable": intent.expected_observable,
                "stop_condition": intent.stop_condition,
                "coverage_key": intent.coverage_key,
                "required_pocs": list(intent.required_pocs),
                "lane_key": "",
                "risk_class": "",
                "resource_key": "",
                "priority": priority,
                "requested_priority": priority,
                "priority_reason": "planner",
                "value_claim": {},
                "requires_capabilities": [],
                "dup_of": "",
                "reopen_because": "",
                "declares": None,
            }
            proposed.append(row)
            if decision_log is not None:
                decision_log.append({
                    "raw_index": raw_index,
                    "model_intent_id": intent.intent_id,
                    "resolved_intent_id": intent_id,
                    "outcome": "accepted",
                    "reason_code": "planner",
                    "stage": "dispatch",
                    "goal": intent.goal,
                    "worker_class": worker_class,
                    "priority": priority,
                    "from_facts": list(intent.from_facts),
                    "coverage_key": intent.coverage_key,
                    "created_seq": int(seq),
                })
        return proposed
    invalid_source_intents: set[int] = set()
    verified_fact_seqs: set[int] = set()
    if ctf_mode:
        try:
            active_fact_seqs = set(shared_graph._active_fact_seq_set() or set())
        except Exception:
            active_fact_seqs = None
        for intent in result.intents:
            try:
                raw_sources = {
                    int(seq)
                    for seq in list(intent.from_facts or [])
                    if int(seq) > 0
                }
            except (TypeError, ValueError):
                raw_sources = set()
                invalid_source_intents.add(id(intent))
            valid_sources = (
                raw_sources
                if active_fact_seqs is None
                else raw_sources & active_fact_seqs
            )
            if (active_fact_seqs is not None
                    and raw_sources - valid_sources):
                invalid_source_intents.add(id(intent))
            intent.from_facts = sorted(valid_sources)
        try:
            verified_fact_seqs = {
                int(row.get("fact_seq") or 0)
                for row in (shared_graph.verified_evidence() or [])
                if int(row.get("fact_seq") or 0) > 0
            }
            challenged_fact_seqs = {
                int(row.get("fact_seq") or 0)
                for row in (shared_graph.challenged_facts() or [])
                if int(row.get("fact_seq") or 0) > 0
            }
            verified_fact_seqs -= challenged_fact_seqs
        except Exception:
            verified_fact_seqs = set()
    try:
        cold_start = not shared_graph._active_fact_seq_set()
    except Exception:
        cold_start = True
    try:
        active_capability_keys = (
            set(shared_graph.active_capability_keys())
            | WORKER_RUNTIME_CAPABILITY_KEYS
        )
    except Exception:
        active_capability_keys = set(WORKER_RUNTIME_CAPABILITY_KEYS)
    try:
        occupied_novelty_keys = set(shared_graph.active_novelty_keys())
    except Exception:
        occupied_novelty_keys = set()
    accepted_high = 0
    for intent in result.intents:
        effective, reason = derive_intent_priority(
            shared_graph, intent,
            verified_fact_seqs=verified_fact_seqs,
            cold_start=cold_start,
            active_capability_keys=active_capability_keys,
            occupied_novelty_keys=occupied_novelty_keys,
        )
        if effective == "high":
            if accepted_high >= 2:
                effective = "normal"
                reason = "high_budget_exceeded"
            else:
                accepted_high += 1
        intent.priority = effective
        intent.priority_reason = reason
        novelty = str(intent.value_claim.get("novelty_key") or "").strip().casefold()
        if novelty and effective != "low":
            occupied_novelty_keys.add(novelty)
    try:
        active_routes = {
            _route_key(shared_graph, route)
            for route in shared_graph.open_route_hashes()
            if _route_key(shared_graph, route)
        }
    except Exception:
        active_routes = set()
    try:
        active_coverage = {
            _step_text_norm(key)
            for key in shared_graph.active_coverage_keys()
            if _step_text_norm(key)
        }
    except Exception:
        active_coverage = set()
    try:
        historical_coverage = {
            _step_text_norm(key)
            for key in shared_graph.open_coverage_keys()
            if _step_text_norm(key)
        }
    except Exception:
        historical_coverage = set(active_coverage)
    try:
        active_lane_rows = list(shared_graph.active_lane_intent_rows() or [])
    except Exception:
        active_lane_rows = []
    try:
        coverage_history_rows = [
            dict(row) for row in (shared_graph.coverage_intent_rows() or [])
        ]
        active_intent_rows = [
            row for row in coverage_history_rows
            if (
                str(row.get("status") or "") in {"open", "claimed"}
                and str(row.get("dispatch_state") or "") == "active"
            )
        ]
    except Exception:
        coverage_history_rows = []
        active_intent_rows = []
    active_intent_by_id = {
        str(row.get("intent_id") or ""): row
        for row in active_intent_rows
        if str(row.get("intent_id") or "")
    }
    active_coverage_methods: dict[str, set[str]] = {}
    historical_coverage_methods: dict[str, set[str]] = {}
    for row in coverage_history_rows:
        coverage = _step_text_norm(row.get("coverage_key"))
        if not coverage:
            continue
        method = _route_key(shared_graph, str(row.get("route_hash") or ""))
        method = method or "__unspecified__"
        historical_coverage_methods.setdefault(coverage, set()).add(method)
    for row in active_intent_rows:
        coverage = _step_text_norm(row.get("coverage_key"))
        if not coverage:
            continue
        method = _route_key(shared_graph, str(row.get("route_hash") or ""))
        method = method or "__unspecified__"
        active_coverage_methods.setdefault(coverage, set()).add(method)
    active_lanes = {
        _lane_key(shared_graph, row.get("lane_key", ""))
        for row in active_lane_rows
        if _lane_key(shared_graph, row.get("lane_key", ""))
    }
    active_branches = {
        str(row.get("branch_id") or "").strip()
        for row in active_intent_rows
        if str(row.get("branch_id") or "").strip()
    }
    active_branch_rows = {
        str(row.get("branch_id") or "").strip(): row
        for row in active_intent_rows
        if (
            str(row.get("branch_id") or "").strip()
            and str(row.get("status") or "") == "claimed"
        )
    }
    try:
        claimed_ids = {
            str(row.get("intent_id") or "")
            for row in active_intent_rows
            if (
                str(row.get("status") or "") == "claimed"
                and str(row.get("intent_id") or "")
            )
        }
        claimed_products = shared_graph._intent_products_map(
            claimed_ids, include_retired=True
        )
    except Exception:
        claimed_products = {}
    requested_supersede = {
        str(intent_id or "").strip()
        for intent_id in (getattr(result, "supersede_intents", []) or [])
        if str(intent_id or "").strip()
    }
    replaceable_claimed_lanes = {
        _lane_key(shared_graph, row.get("lane_key", ""))
        for row in active_lane_rows
        if (
            str(row.get("intent_id") or "") in requested_supersede
            and str(row.get("status") or "") == "claimed"
            and str(getattr(result, "supersede_why", "") or "").strip()
            and _lane_key(shared_graph, row.get("lane_key", ""))
            and any(
                intent.priority == "high"
                and _lane_key(shared_graph, intent.lane_key)
                == _lane_key(shared_graph, row.get("lane_key", ""))
                and any(
                    int(seq) in verified_fact_seqs
                    for seq in (intent.from_facts or [])
                )
                for intent in result.intents
            )
        )
    }
    active_lane_by_intent = {
        str(row.get("intent_id") or ""): row
        for row in active_lane_rows
        if str(row.get("intent_id") or "")
    }
    claimed_supersede_rows = {
        str(row.get("intent_id") or ""): {
            **row,
            **active_lane_by_intent.get(str(row.get("intent_id") or ""), {}),
        }
        for row in active_intent_rows
        if (
            str(row.get("intent_id") or "") in requested_supersede
            and str(row.get("status") or "") == "claimed"
        )
    }
    claimed_source_seqs: dict[str, set[int]] = {}
    for intent_id in claimed_supersede_rows:
        try:
            raw_sources = shared_graph._intent_sources_map(
                {intent_id}, include_retired=True
            ).get(intent_id, [])
            claimed_source_seqs[intent_id] = {
                int(seq) for seq in raw_sources if int(seq) > 0
            }
        except Exception:
            try:
                claimed_source_seqs[intent_id] = {
                    int(row.get("seq") or 0)
                    for row in (shared_graph.intent_source_facts(intent_id) or [])
                    if int(row.get("seq") or 0) > 0
                }
            except Exception:
                claimed_source_seqs[intent_id] = set()

    def _refreshes_claimed_context(intent: Intent) -> bool:
        old_intent_id = str(intent.dup_of or "").strip()
        old_intent = claimed_supersede_rows.get(old_intent_id, {})
        route = _route_key(shared_graph, intent.route_hash)
        coverage = _step_text_norm(intent.coverage_key)
        old_lane = _lane_key(shared_graph, str(old_intent.get("lane_key") or ""))
        new_lane = _lane_key(shared_graph, intent.lane_key)
        new_source_seqs = {int(seq) for seq in (intent.from_facts or [])}
        return bool(
            old_intent
            and str(getattr(result, "supersede_why", "") or "").strip()
            and intent.priority == "high"
            and intent.reopen_because.strip()
            and route
            and route == _route_key(
                shared_graph, str(old_intent.get("route_hash") or "")
            )
            and coverage
            and coverage in active_coverage
            and (not old_lane or new_lane == old_lane)
            and bool(new_source_seqs & verified_fact_seqs)
            and new_source_seqs - claimed_source_seqs.get(old_intent_id, set())
        )

    # Preserve the live consumer's existing evidence packet even when the model
    # cites only the newly unlocked capability. This is the context-refresh
    # postcondition: the replacement receives old prerequisites plus the new fact.
    for intent in result.intents:
        if not _refreshes_claimed_context(intent):
            continue
        old_intent_id = str(intent.dup_of or "").strip()
        intent.from_facts = sorted(
            claimed_source_seqs.get(old_intent_id, set())
            | {int(seq) for seq in (intent.from_facts or [])}
        )
    try:
        existing_step_keys = set(shared_graph.equivalent_step_keys() or set())
    except Exception:
        existing_step_keys = set()
    try:
        existing_lane_source_keys = set(
            shared_graph.equivalent_lane_source_keys() or set()
        )
    except Exception:
        existing_lane_source_keys = set()
    try:
        branch_contracts = {
            str(row.get("branch_id") or ""): dict(row)
            for row in (shared_graph.branches() or [])
            if (
                str(row.get("branch_id") or "")
                and str(row.get("status") or "open") == "open"
            )
        }
    except Exception:
        branch_contracts = {}
    branches_by_coverage: dict[str, list[dict[str, Any]]] = {}
    branches_by_route: dict[str, list[dict[str, Any]]] = {}
    for branch in branch_contracts.values():
        coverage = _step_text_norm(str(branch.get("coverage_key") or ""))
        route = _route_key(shared_graph, str(branch.get("route_hash") or ""))
        if coverage:
            branches_by_coverage.setdefault(coverage, []).append(branch)
        if route:
            branches_by_route.setdefault(route, []).append(branch)
    invalid_branch_intents: set[int] = set()
    # A Worker declaration is the ownership boundary for its child branch.
    # Reason may narrow the action, but it cannot silently discard or replace
    # the declared exclusive resource when materialising the next Step.
    for intent in result.intents:
        supplied_branch_id = str(intent.branch_id or "").strip()
        branch = branch_contracts.get(supplied_branch_id, {})
        if not branch:
            candidates: dict[str, dict[str, Any]] = {}
            coverage = _step_text_norm(intent.coverage_key)
            route = _route_key(shared_graph, intent.route_hash)
            for row in branches_by_coverage.get(coverage, []) if coverage else []:
                candidates[str(row.get("branch_id") or "")] = row
            for row in branches_by_route.get(route, []) if route else []:
                candidates[str(row.get("branch_id") or "")] = row
            if len(candidates) == 1:
                branch = next(iter(candidates.values()))
                intent.branch_id = str(branch.get("branch_id") or "")
            elif supplied_branch_id:
                invalid_branch_intents.add(id(intent))
        branch_lane = str(branch.get("lane_key") or "").strip()
        branch_risk = str(branch.get("risk_class") or "").strip()
        branch_resource = str(branch.get("resource_key") or "").strip()
        if branch_lane:
            intent.lane_key = branch_lane
        if branch_risk:
            intent.risk_class = branch_risk
        if branch_resource:
            intent.resource_key = branch_resource
    batch_routes: set[str] = set()
    batch_coverage: set[str] = set()
    batch_coverage_methods: dict[str, set[str]] = {}
    batch_lanes: set[str] = set()
    batch_branches: set[str] = set()
    batch_step_keys: set[tuple] = set()
    batch_producer_successors: set[str] = set()
    raw_to_unique = {
        it.intent_id: _unique_intent_id(shared_graph, it)
        for it in result.intents
    }
    remaining = list(result.intents)
    ordered: list[Intent] = []
    while remaining:
        remaining_ids = {it.intent_id for it in remaining}
        ready = [
            it for it in remaining
            if not any(
                str(dep or "").strip() in remaining_ids
                for dep in it.depends_on
            )
        ]
        if not ready:
            break  # cyclic same-batch dependencies are not executable
        ordered.extend(ready)
        ready_ids = {id(it) for it in ready}
        remaining = [it for it in remaining if id(it) not in ready_ids]
    for intent in remaining:
        _record(intent, outcome="dropped", reason_code="dependency_cycle")

    # A context refresh exists specifically because fresh operational evidence
    # changed a live chain. Keep its same-batch prerequisites and other high
    # priority work, but do not use the freed slot for unrelated fallback work.
    refresh_ids = {
        intent.intent_id for intent in ordered
        if _refreshes_claimed_context(intent)
    }
    focused_ids = set(refresh_ids)
    if focused_ids:
        by_id = {intent.intent_id: intent for intent in ordered}
        pending = list(focused_ids)
        while pending:
            current = by_id.get(pending.pop())
            if current is None:
                continue
            for raw_dep in current.depends_on:
                dependency = str(raw_dep or "").strip()
                if dependency in by_id and dependency not in focused_ids:
                    focused_ids.add(dependency)
                    pending.append(dependency)

    def _intent_exists(intent_id: str) -> bool:
        try:
            return bool(shared_graph.intent_claim_state(intent_id))
        except Exception:
            return False

    persisted_ids: set[str] = set()
    resolved_aliases: dict[str, str] = {}
    semantic_dedupe = bool(getattr(result, "semantic_dedupe_available", False))
    for it in ordered:
        if id(it) in invalid_source_intents:
            _record(it, outcome="dropped", reason_code="invalid_source_fact")
            continue
        if id(it) in invalid_branch_intents:
            _record(it, outcome="dropped", reason_code="branch_missing")
            continue
        if (
            refresh_ids
            and it.intent_id not in focused_ids
            and it.priority != "high"
        ):
            _record(
                it,
                outcome="dropped",
                reason_code="focused_chain_backpressure",
            )
            continue
        depends_on: list[str] = []
        unresolved_dependency = False
        dependency_not_ready = False
        for raw_dep in it.depends_on:
            dep = str(raw_dep or "").strip()
            if not dep:
                continue
            resolved = resolved_aliases.get(dep) or raw_to_unique.get(dep, dep)
            if resolved not in persisted_ids and not _intent_exists(resolved):
                unresolved_dependency = True
                break
            try:
                dependency_state = dict(
                    shared_graph.intent_claim_state(resolved) or {})
            except Exception:
                dependency_state = {}
            if not (
                str(dependency_state.get("status") or "") == "done"
                and str(dependency_state.get("dispatch_state") or "") == "closed"
            ):
                dependency_not_ready = True
                break
            depends_on.append(resolved)
        if unresolved_dependency:
            _record(it, outcome="dropped", reason_code="dependency_missing")
            continue
        if dependency_not_ready:
            # Reason runs again when the prerequisite publishes its checkpoint.
            # Persisting future steps here freezes a plan made before the actual
            # result exists and lets it bypass that new frontier.  Accept only
            # the immediately executable layer of the Branch DAG.
            _record(it, outcome="dropped", reason_code="dependency_not_ready")
            continue
        if not it.from_facts and not (cold_start or it.initial):
            # Reason intents must be evidence-grounded: a source-less step
            # re-plans from thin air. Initial intents are exempt — cold start,
            # an explicit `initial: true` payload, or a challenge-root
            # citation in `from` (any citation makes from_facts non-empty).
            _record(it, outcome="dropped", reason_code="orphan_no_source")
            continue
        if semantic_dedupe:
            if (it.dup_of and not it.reopen_because
                    and _intent_exists(it.dup_of)):
                resolved_aliases[it.intent_id] = it.dup_of
                _record(it, outcome="dropped", reason_code="declared_duplicate")
                continue
        route = _route_key(shared_graph, it.route_hash)
        coverage = _step_text_norm(it.coverage_key)
        lane = _lane_key(shared_graph, it.lane_key)
        branch_id = str(it.branch_id or "").strip()
        lane_source_key = (
            lane,
            tuple(sorted(set(int(seq) for seq in it.from_facts))),
        )
        replaces_claimed_lane = bool(
            lane
            and lane in replaceable_claimed_lanes
            and it.priority == "high"
            and it.from_facts
            and any(
                int(seq) in verified_fact_seqs
                for seq in it.from_facts
            )
        )
        refreshes_claimed_context = _refreshes_claimed_context(it)
        source_set = {
            int(seq) for seq in (it.from_facts or []) if int(seq) > 0
        }
        active_producers = {
            str(intent_id)
            for intent_id, products in claimed_products.items()
            if source_set & {int(seq) for seq in (products or [])}
        }
        same_stage_owner = next(
            (
                row for row in active_intent_rows
                if _same_active_step_stage(it, row)
            ),
            None,
        )
        if same_stage_owner and not refreshes_claimed_context:
            # New evidence enriches the live Worker's next checkpoint. It does
            # not manufacture a successor for the same question or a second
            # Worker with a renamed coverage key.
            _record(it, outcome="dropped", reason_code="active_step_stage")
            continue
        producer_predecessor = ""
        if ctf_mode and active_producers and not refreshes_claimed_context:
            # A producer may retain ownership only by taking one concrete child
            # Step.  Persist that child behind the current claim; at the next
            # checkpoint the graph atomically concludes the predecessor and hands
            # this exact successor to the same session.  Without a child, the Step
            # ends at its lease boundary and ownership is released.
            producer_predecessor = min(
                active_producers,
                key=lambda intent_id: (
                    -len(
                        source_set
                        & {int(seq) for seq in claimed_products.get(intent_id, [])}
                    ),
                    intent_id,
                ),
            )
            if producer_predecessor in batch_producer_successors:
                _record(
                    it,
                    outcome="dropped",
                    reason_code="producer_successor_queued",
                )
                continue
            producer_row = active_intent_by_id.get(producer_predecessor, {})
            try:
                existing_successor = shared_graph.claimed_intent_successor(
                    worker=str(producer_row.get("worker") or ""),
                    intent_id=producer_predecessor,
                )
            except Exception:
                existing_successor = {}
            if existing_successor:
                _record(
                    it,
                    outcome="dropped",
                    reason_code="producer_successor_exists",
                )
                continue
        branch_predecessor = (
            str(it.dup_of or "").strip()
            if refreshes_claimed_context else producer_predecessor
        )
        branch_owner = active_branch_rows.get(branch_id, {}) if branch_id else {}
        if (
            ctf_mode
            and branch_owner
            and not refreshes_claimed_context
            and str(branch_owner.get("intent_id") or "") != branch_predecessor
        ):
            # Branch ownership is the semantic boundary.  Renaming route_hash or
            # coverage_key must not turn the live owner's next action into a queued
            # successor or a duplicate Worker.
            _record(
                it,
                outcome="dropped",
                reason_code="active_branch_owned",
            )
            continue
        if branch_predecessor and branch_predecessor not in depends_on:
            depends_on.append(branch_predecessor)
            if branch_predecessor not in it.depends_on:
                it.depends_on.append(branch_predecessor)
        replaces_claimed = bool(
            replaces_claimed_lane
            or refreshes_claimed_context
            or branch_predecessor
        )
        if branch_id and branch_id in batch_branches:
            _record(it, outcome="dropped", reason_code="branch_successor_queued")
            continue
        if (branch_id and branch_id in active_branches
                and not branch_predecessor):
            _record(it, outcome="dropped", reason_code="active_branch")
            continue
        step_key = _structural_step_key(shared_graph, it)
        if step_key in existing_step_keys or step_key in batch_step_keys:
            _record(it, outcome="dropped", reason_code="equivalent_step")
            continue
        if (
            lane
            and it.worker_class != "review"
            and lane_source_key in existing_lane_source_keys
            and not replaces_claimed_lane
        ):
            _record(
                it,
                outcome="dropped",
                reason_code="repeated_lane_evidence",
            )
            continue
        if route and it.worker_class not in {"verifier", "review"}:
            if (
                _route_is_owned(route, batch_routes)
                or (
                    _route_is_owned(route, active_routes)
                    and not replaces_claimed
                )
            ):
                _record(it, outcome="dropped", reason_code="active_route")
                continue
        if coverage and it.worker_class != "review":
            method = route or "__unspecified__"
            if ctf_mode and not branch_id and not lane and not replaces_claimed:
                active_methods = (
                    active_coverage_methods.get(coverage, set())
                    | batch_coverage_methods.get(coverage, set())
                )
                historical_methods = historical_coverage_methods.get(
                    coverage, set()
                )
                if method in active_methods or len(active_methods) >= 2:
                    _record(
                        it, outcome="dropped",
                        reason_code="active_coverage_method",
                    )
                    continue
                if (
                    coverage in historical_coverage
                    and coverage not in active_coverage
                    and method in historical_methods
                    and not it.reopen_because
                ):
                    _record(
                        it, outcome="dropped", reason_code="covered_history",
                    )
                    continue
            else:
                if (
                    coverage in historical_coverage
                    and coverage not in active_coverage
                    and not it.reopen_because
                ):
                    _record(
                        it,
                        outcome="dropped",
                        reason_code="covered_history",
                    )
                    continue
                if coverage in batch_coverage or (
                    coverage in active_coverage and not replaces_claimed
                ):
                    _record(it, outcome="dropped", reason_code="active_coverage")
                    continue
        if lane and it.worker_class != "review":
            if lane in batch_lanes or (
                lane in active_lanes and not replaces_claimed
            ):
                _record(it, outcome="dropped", reason_code="active_lane")
                continue
        row = _propose_one(shared_graph, it, actor=actor, depends_on=depends_on)
        if row:
            row["refreshes_claimed_context"] = refreshes_claimed_context
            row["branch_successor_of"] = branch_predecessor
            row["producer_successor_of"] = producer_predecessor
            proposed.append(row)
            _record(
                it,
                outcome="accepted",
                reason_code="accepted",
                created_seq=int(row.get("created_seq") or 0),
            )
            persisted_ids.add(row["intent_id"])
            if producer_predecessor:
                batch_producer_successors.add(producer_predecessor)
            batch_step_keys.add(step_key)
            if route and it.worker_class not in {"verifier", "review"}:
                batch_routes.add(route)
            if coverage and it.worker_class != "review":
                batch_coverage.add(coverage)
                method = route or "__unspecified__"
                batch_coverage_methods.setdefault(coverage, set()).add(method)
            if lane and it.worker_class != "review":
                batch_lanes.add(lane)
            if branch_id and it.worker_class != "review":
                batch_branches.add(branch_id)
        else:
            _record(it, outcome="dropped", reason_code="storage_duplicate")
            intent_id = raw_to_unique.get(it.intent_id, "")
            if intent_id and _intent_exists(intent_id):
                persisted_ids.add(intent_id)
                resolved_aliases[it.intent_id] = intent_id
    return proposed
