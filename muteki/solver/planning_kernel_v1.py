"""Pure planning kernel for Muteki planning swarms (no I/O, no env knobs).

Three structurally different planning mechanisms share this module's data
shapes and deterministic helpers so unit tests can exercise the planning
layer without spawning workers or touching the production coordinator.
"""

from __future__ import annotations

import json
import re
from dataclasses import dataclass, field
from enum import Enum
from typing import Any, Iterable, Optional


class UnitStatus(str, Enum):
    OPEN = "open"
    ACTIVE = "active"
    DONE = "done"
    FAILED = "failed"
    ABANDONED = "abandoned"
    BLOCKED = "blocked"


class HypothesisStatus(str, Enum):
    OPEN = "open"
    TESTING = "testing"
    SUPPORTED = "supported"
    FALSIFIED = "falsified"
    ABANDONED = "abandoned"


class Phase(str, Enum):
    RECON = "recon"
    ANALYZE = "analyze"
    EXPLOIT = "exploit"
    VERIFY = "verify"
    DONE = "done"


@dataclass
class Subgoal:
    id: str
    goal: str
    rationale: str = ""
    depends_on: list[str] = field(default_factory=list)
    status: UnitStatus = UnitStatus.OPEN
    receipt: str = ""
    attempts: int = 0
    max_attempts: int = 2


@dataclass
class Hypothesis:
    id: str
    claim: str
    test: str
    status: HypothesisStatus = HypothesisStatus.OPEN
    evidence_for: list[str] = field(default_factory=list)
    evidence_against: list[str] = field(default_factory=list)
    attempts: int = 0
    max_attempts: int = 2


@dataclass
class WorkingMemory:
    """Compressed planner-visible state (context firewall)."""

    challenge_brief: str = ""
    facts: list[str] = field(default_factory=list)
    dead_ends: list[str] = field(default_factory=list)
    receipts: list[str] = field(default_factory=list)
    open_questions: list[str] = field(default_factory=list)
    flags: list[str] = field(default_factory=list)
    max_facts: int = 12
    max_dead_ends: int = 8
    max_receipts: int = 6

    def add_fact(self, fact: str) -> None:
        text = _clip(fact, 220)
        if not text or text in self.facts:
            return
        self.facts.append(text)
        if len(self.facts) > self.max_facts:
            self.facts = self.facts[-self.max_facts :]

    def add_dead_end(self, note: str) -> None:
        text = _clip(note, 180)
        if not text or text in self.dead_ends:
            return
        self.dead_ends.append(text)
        if len(self.dead_ends) > self.max_dead_ends:
            self.dead_ends = self.dead_ends[-self.max_dead_ends :]

    def add_receipt(self, receipt: str) -> None:
        text = _clip(receipt, 280)
        if not text:
            return
        self.receipts.append(text)
        if len(self.receipts) > self.max_receipts:
            self.receipts = self.receipts[-self.max_receipts :]

    def render(self) -> str:
        lines = ["[working-memory]"]
        if self.challenge_brief:
            lines.append(f"challenge: {_clip(self.challenge_brief, 400)}")
        if self.flags:
            # Never put plaintext flags into planner/executor prompts.
            lines.append(f"flags_recovered: {min(4, len(self.flags))}")
        if self.facts:
            lines.append("facts:")
            for i, f in enumerate(self.facts, 1):
                lines.append(f"  {i}. {f}")
        if self.dead_ends:
            lines.append("dead_ends:")
            for i, d in enumerate(self.dead_ends, 1):
                lines.append(f"  {i}. {d}")
        if self.receipts:
            lines.append("recent_receipts:")
            for i, r in enumerate(self.receipts, 1):
                lines.append(f"  {i}. {r}")
        if self.open_questions:
            lines.append("open_questions:")
            for i, q in enumerate(self.open_questions[:5], 1):
                lines.append(f"  {i}. {_clip(q, 160)}")
        return "\n".join(lines)


@dataclass
class MissionPlan:
    subgoals: list[Subgoal] = field(default_factory=list)
    abandon_ids: list[str] = field(default_factory=list)
    parallel_max: int = 2
    notes: str = ""


@dataclass
class HypothesisPlan:
    hypotheses: list[Hypothesis] = field(default_factory=list)
    abandon_ids: list[str] = field(default_factory=list)
    parallel_max: int = 2
    notes: str = ""


def _clip(text: str, n: int) -> str:
    body = re.sub(r"\s+", " ", str(text or "")).strip()
    if len(body) <= n:
        return body
    return body[: max(0, n - 1)] + "…"


def extract_json_object(text: str) -> dict[str, Any]:
    """Best-effort JSON object extraction from an LLM reply."""
    raw = (text or "").strip()
    if not raw:
        return {}
    # fenced
    fence = re.search(r"```(?:json)?\s*(\{.*?\})\s*```", raw, re.DOTALL | re.IGNORECASE)
    if fence:
        raw = fence.group(1)
    else:
        start = raw.find("{")
        end = raw.rfind("}")
        if start >= 0 and end > start:
            raw = raw[start : end + 1]
    try:
        data = json.loads(raw)
    except json.JSONDecodeError:
        return {}
    return data if isinstance(data, dict) else {}


def parse_mission_plan(text: str, *, default_parallel: int = 2) -> MissionPlan:
    data = extract_json_object(text)
    subs: list[Subgoal] = []
    for i, row in enumerate(data.get("subgoals") or []):
        if not isinstance(row, dict):
            continue
        goal = str(row.get("goal") or "").strip()
        if not goal:
            continue
        sid = str(row.get("id") or f"S{i + 1}").strip() or f"S{i + 1}"
        deps = row.get("depends_on") or []
        if not isinstance(deps, list):
            deps = []
        subs.append(
            Subgoal(
                id=sid,
                goal=goal[:500],
                rationale=str(row.get("rationale") or "")[:240],
                depends_on=[str(d) for d in deps if str(d).strip()],
            )
        )
    abandon = [str(x) for x in (data.get("abandon") or []) if str(x).strip()]
    try:
        parallel = int(data.get("parallel_max") or default_parallel)
    except (TypeError, ValueError):
        parallel = default_parallel
    return MissionPlan(
        subgoals=subs,
        abandon_ids=abandon,
        parallel_max=max(1, min(4, parallel)),
        notes=str(data.get("notes") or "")[:300],
    )


def parse_hypothesis_plan(text: str, *, default_parallel: int = 2) -> HypothesisPlan:
    data = extract_json_object(text)
    hyps: list[Hypothesis] = []
    for i, row in enumerate(data.get("hypotheses") or []):
        if not isinstance(row, dict):
            continue
        claim = str(row.get("claim") or "").strip()
        test = str(row.get("test") or "").strip()
        if not claim or not test:
            continue
        hid = str(row.get("id") or f"H{i + 1}").strip() or f"H{i + 1}"
        hyps.append(
            Hypothesis(
                id=hid,
                claim=claim[:300],
                test=test[:500],
            )
        )
    abandon = [str(x) for x in (data.get("abandon") or []) if str(x).strip()]
    try:
        parallel = int(data.get("parallel_max") or default_parallel)
    except (TypeError, ValueError):
        parallel = default_parallel
    return HypothesisPlan(
        hypotheses=hyps,
        abandon_ids=abandon,
        parallel_max=max(1, min(4, parallel)),
        notes=str(data.get("notes") or "")[:300],
    )


def merge_subgoals(
    existing: list[Subgoal], incoming: list[Subgoal], abandon_ids: Iterable[str]
) -> list[Subgoal]:
    """Merge planner output into the live subgoal ledger.

    - abandon_ids mark matching open/active units abandoned
    - new ids append; existing open units keep status/attempts
    - duplicate goals (normalized) are skipped
    """
    abandon = {str(x) for x in abandon_ids}
    by_id = {s.id: s for s in existing}
    for sid in abandon:
        if sid in by_id and by_id[sid].status in {
            UnitStatus.OPEN,
            UnitStatus.ACTIVE,
            UnitStatus.BLOCKED,
        }:
            by_id[sid].status = UnitStatus.ABANDONED
    seen_goals = {_norm_goal(s.goal) for s in by_id.values()}
    for sub in incoming:
        if sub.id in by_id:
            old = by_id[sub.id]
            if old.status in {UnitStatus.DONE, UnitStatus.ABANDONED}:
                continue
            # refresh goal text if still open
            if old.status == UnitStatus.OPEN:
                old.goal = sub.goal
                old.rationale = sub.rationale
                old.depends_on = list(sub.depends_on)
            continue
        g = _norm_goal(sub.goal)
        if g in seen_goals:
            continue
        by_id[sub.id] = sub
        seen_goals.add(g)
    return list(by_id.values())


def merge_hypotheses(
    existing: list[Hypothesis],
    incoming: list[Hypothesis],
    abandon_ids: Iterable[str],
) -> list[Hypothesis]:
    abandon = {str(x) for x in abandon_ids}
    by_id = {h.id: h for h in existing}
    for hid in abandon:
        if hid in by_id and by_id[hid].status in {
            HypothesisStatus.OPEN,
            HypothesisStatus.TESTING,
        }:
            by_id[hid].status = HypothesisStatus.ABANDONED
    seen = {_norm_goal(h.claim) for h in by_id.values()}
    for hyp in incoming:
        if hyp.id in by_id:
            old = by_id[hyp.id]
            if old.status in {
                HypothesisStatus.SUPPORTED,
                HypothesisStatus.FALSIFIED,
                HypothesisStatus.ABANDONED,
            }:
                continue
            if old.status == HypothesisStatus.OPEN:
                old.claim = hyp.claim
                old.test = hyp.test
            continue
        key = _norm_goal(hyp.claim)
        if key in seen:
            continue
        by_id[hyp.id] = hyp
        seen.add(key)
    return list(by_id.values())


def _norm_goal(text: str) -> str:
    return re.sub(r"\s+", " ", (text or "").strip().lower())


def ready_subgoals(subgoals: list[Subgoal]) -> list[Subgoal]:
    """Subgoals whose dependencies are DONE and status is OPEN."""
    done = {s.id for s in subgoals if s.status == UnitStatus.DONE}
    out: list[Subgoal] = []
    for s in subgoals:
        if s.status != UnitStatus.OPEN:
            continue
        if s.attempts >= s.max_attempts:
            s.status = UnitStatus.FAILED
            continue
        if all(dep in done for dep in s.depends_on):
            out.append(s)
        else:
            s.status = UnitStatus.BLOCKED
    # unblock when deps met next call; reset BLOCKED→OPEN if deps now met
    for s in subgoals:
        if s.status == UnitStatus.BLOCKED and all(
            dep in done for dep in s.depends_on
        ):
            s.status = UnitStatus.OPEN
            if s not in out and s.attempts < s.max_attempts:
                out.append(s)
    return out


def select_dispatch(
    ready: list[Subgoal], *, parallel_max: int, avoid_goals: Optional[set[str]] = None
) -> list[Subgoal]:
    """Pick up to parallel_max ready subgoals, preferring unseen goals."""
    avoid = avoid_goals or set()
    ranked = sorted(
        ready,
        key=lambda s: (
            _norm_goal(s.goal) in avoid,
            s.attempts,
            len(s.depends_on),
            s.id,
        ),
    )
    return ranked[: max(1, parallel_max)]


def select_hypothesis_tests(
    hypotheses: list[Hypothesis], *, parallel_max: int
) -> list[Hypothesis]:
    open_h = [
        h
        for h in hypotheses
        if h.status == HypothesisStatus.OPEN and h.attempts < h.max_attempts
    ]
    open_h.sort(key=lambda h: (h.attempts, h.id))
    return open_h[: max(1, parallel_max)]


def apply_executor_receipt(
    memory: WorkingMemory,
    subgoal: Subgoal,
    *,
    new_facts: list[str],
    success: bool,
    summary: str,
    found_flag: str = "",
) -> None:
    """Fold an executor result into working memory + subgoal status."""
    for f in new_facts:
        memory.add_fact(f)
    if found_flag:
        if found_flag not in memory.flags:
            memory.flags.append(found_flag)
    subgoal.attempts += 1
    subgoal.receipt = _clip(summary, 280)
    memory.add_receipt(f"{subgoal.id}: {summary}")
    if success or found_flag:
        subgoal.status = UnitStatus.DONE
    elif subgoal.attempts >= subgoal.max_attempts:
        subgoal.status = UnitStatus.FAILED
        memory.add_dead_end(f"{subgoal.id} failed: {summary}")
    else:
        subgoal.status = UnitStatus.OPEN


def apply_hypothesis_result(
    memory: WorkingMemory,
    hyp: Hypothesis,
    *,
    verdict: str,
    evidence: str,
    new_facts: list[str],
) -> None:
    """verdict in {supported, falsified, inconclusive}."""
    for f in new_facts:
        memory.add_fact(f)
    hyp.attempts += 1
    note = _clip(evidence, 200)
    v = (verdict or "").strip().lower()
    if v == "supported":
        hyp.status = HypothesisStatus.SUPPORTED
        if note:
            hyp.evidence_for.append(note)
        memory.add_fact(f"HYP-SUPPORTED {hyp.claim}: {note}")
    elif v == "falsified":
        hyp.status = HypothesisStatus.FALSIFIED
        if note:
            hyp.evidence_against.append(note)
        memory.add_dead_end(f"HYP-FALSIFIED {hyp.claim}: {note}")
    else:
        if note:
            hyp.evidence_for.append(note)
        if hyp.attempts >= hyp.max_attempts:
            hyp.status = HypothesisStatus.ABANDONED
            memory.add_dead_end(f"HYP-ABANDONED {hyp.claim}: no conclusive test")
        else:
            hyp.status = HypothesisStatus.OPEN


def next_phase(
    current: Phase,
    *,
    fact_count: int,
    fruitless_rounds: int,
    has_candidate_flag: bool,
    phase_budget_exhausted: bool,
) -> Phase:
    """Deterministic phase transition for the phased architecture."""
    if current == Phase.DONE:
        return Phase.DONE
    if has_candidate_flag and current != Phase.VERIFY:
        return Phase.VERIFY
    if current == Phase.RECON:
        if fact_count >= 2 or fruitless_rounds >= 1 or phase_budget_exhausted:
            return Phase.ANALYZE
        return Phase.RECON
    if current == Phase.ANALYZE:
        if fact_count >= 4 or fruitless_rounds >= 2 or phase_budget_exhausted:
            return Phase.EXPLOIT
        return Phase.ANALYZE
    if current == Phase.EXPLOIT:
        if has_candidate_flag:
            return Phase.VERIFY
        if fruitless_rounds >= 3 or phase_budget_exhausted:
            # recycle to analyze with fresh questions rather than spin
            return Phase.ANALYZE
        return Phase.EXPLOIT
    if current == Phase.VERIFY:
        if has_candidate_flag:
            return Phase.DONE
        return Phase.EXPLOIT
    return current


def phase_parallel_budget(phase: Phase, *, max_workers: int) -> int:
    """How many workers a phase may run concurrently."""
    cap = max(1, int(max_workers))
    if phase == Phase.RECON:
        return min(cap, 2)
    if phase == Phase.ANALYZE:
        return min(cap, 2)
    if phase == Phase.EXPLOIT:
        return min(cap, 3)
    if phase == Phase.VERIFY:
        return 1
    return 1


def phase_timeout_seconds(phase: Phase, *, wall_remaining: float) -> int:
    """Short-horizon timeouts; never exceed remaining wall budget."""
    base = {
        Phase.RECON: 180,
        Phase.ANALYZE: 240,
        Phase.EXPLOIT: 300,
        Phase.VERIFY: 120,
        Phase.DONE: 60,
    }.get(phase, 240)
    remain = max(30.0, float(wall_remaining))
    return int(min(base, remain - 15.0))


def bootstrap_recon_goal(
    category: str,
    description: str,
    *,
    mode: str = "ctf",
    engagement_goal: str = "",
) -> str:
    if (mode or "ctf") == "pentest":
        goal = _clip(engagement_goal, 240) or "prove the engagement goal on the live target"
        return (
            "Pursue the engagement goal against the live in-scope HTTP origin. "
            f"Goal: {goal}. Record verified observations from real responses. "
            "Do not inventory challenge attachments, git objects, or host "
            "repository source."
        )
    cat = (category or "misc").strip().lower()
    desc = _clip(description, 240)
    if cat in {"crypto", "cry"}:
        return (
            "RECON only: inventory player files, identify cipher/protocol "
            f"family from headers and source, record concrete facts. {desc}"
        )
    if cat in {"forensics", "for"}:
        git_hint = ""
        low = desc.lower()
        if "git" in low or "fsck" in low or "sata" in low or "repo" in low:
            git_hint = (
                " If the hint mentions git/fsck: run `git fsck --unreachable "
                "-v`, inspect dangling blobs/trees/commits, and `git show`/"
                "`git cat-file -p` candidates — do not invent flag{sha1} from "
                "object ids."
            )
        return (
            "RECON only: file(1)/binwalk/strings on attachments, list "
            f"embedded streams and notable artifacts as facts.{git_hint} {desc}"
        )
    if cat in {"reverse", "rev"}:
        return (
            "RECON only: file type, arch, strings/interesting symbols, "
            f"entry behavior summary as facts — no full solve yet. {desc}"
        )
    return (
        "RECON only: inventory attachments, extract obvious structure, "
        f"write verified observations as facts. {desc}"
    )


def default_mission_seed(
    category: str,
    description: str,
    *,
    mode: str = "ctf",
    engagement_goal: str = "",
) -> MissionPlan:
    """Deterministic seed plan when the planner LLM fails."""
    if (mode or "ctf") == "pentest":
        return MissionPlan(
            subgoals=[
                Subgoal(
                    id="S1",
                    goal=bootstrap_recon_goal(
                        category, description,
                        mode="pentest", engagement_goal=engagement_goal,
                    ),
                    rationale="seed-goal",
                ),
            ],
            parallel_max=2,
            notes="pentest-seed",
        )
    recon = bootstrap_recon_goal(category, description)
    return MissionPlan(
        subgoals=[
            Subgoal(id="S1", goal=recon, rationale="seed-recon"),
            Subgoal(
                id="S2",
                goal=(
                    "Using verified facts, identify the next decisive "
                    "transform or decode step and execute it; record outputs."
                ),
                rationale="seed-chain",
                depends_on=["S1"],
            ),
            Subgoal(
                id="S3",
                goal=(
                    "Pursue the flag from the intermediate result; verify "
                    "format flag{...} from real command output."
                ),
                rationale="seed-finish",
                depends_on=["S2"],
            ),
        ],
        parallel_max=2,
        notes="deterministic-seed",
    )


def default_hypothesis_seed(category: str, description: str) -> HypothesisPlan:
    cat = (category or "misc").strip().lower()
    desc = _clip(description, 200)
    if cat in {"crypto", "cry"}:
        claims = [
            (
                "H1",
                "Classic classical/stream cipher with reused key or crib",
                "Check for XOR/OTP reuse, crib drag, or known-plaintext in files",
            ),
            (
                "H2",
                "Custom script encodes the flag with a recoverably weak primitive",
                "Read any .py/.c source and recover key/nonce from constants",
            ),
        ]
    elif cat in {"forensics", "for"}:
        claims = [
            (
                "H1",
                "Flag is embedded in a carved stream (pcap/zip/image)",
                "Carving + strings/binwalk on attachments; list recoveries",
            ),
            (
                "H2",
                "Flag requires reconstructing a multi-layer container chain",
                "Enumerate nested containers and decode each layer once",
            ),
        ]
    else:
        claims = [
            (
                "H1",
                "Binary/logic reveals flag via strings or simple decode",
                "strings + simple static analysis; record candidates",
            ),
            (
                "H2",
                "Flag requires one algorithmic transform after recon",
                "Identify the transform and execute it on extracted data",
            ),
        ]
    return HypothesisPlan(
        hypotheses=[
            Hypothesis(id=hid, claim=f"{claim}. {desc}", test=test)
            for hid, claim, test in claims
        ],
        parallel_max=2,
        notes="deterministic-seed",
    )


def fold_graph_facts(shared_graph: Any, *, limit: int = 12) -> list[str]:
    """Pull recent verified facts from SharedGraph into plain strings."""
    if shared_graph is None:
        return []
    out: list[str] = []
    try:
        rows = shared_graph.verified_evidence()
    except Exception:
        rows = []
    if not rows:
        try:
            snap = shared_graph.snapshot()
            rows = list(getattr(snap, "facts", None) or [])
        except Exception:
            rows = []
    for row in rows:
        if isinstance(row, dict):
            text = str(row.get("fact") or row.get("text") or "").strip()
        else:
            text = str(getattr(row, "fact", None) or getattr(row, "text", None) or row)
            text = str(text).strip()
        if text and text not in out:
            out.append(text)
        if len(out) >= limit:
            break
    return out


def planner_system_pex() -> str:
    return (
        "You are the Planner in a Planner–Executor CTF swarm. "
        "You NEVER run tools. You emit ONLY JSON with keys: "
        "subgoals (list of {id,goal,rationale,depends_on}), "
        "abandon (ids to drop), parallel_max (1-3), notes. "
        "Each subgoal must be ONE concrete executable step for an Executor "
        "(file ops, decode, reverse one function, one protocol parse). "
        "Prefer a short dependency chain over a vague 'solve the challenge'. "
        "Abandon subgoals contradicted by dead_ends. "
        "Do not invent flags."
    )


def planner_system_hypo() -> str:
    return (
        "You are a Hypothesis Ledger controller for a CTF swarm. "
        "Emit ONLY JSON with keys: hypotheses "
        "(list of {id,claim,test}), abandon (ids), parallel_max (1-3), notes. "
        "Each hypothesis must be falsifiable; test is the ONE experiment "
        "an Executor should run. Prefer discriminating tests. "
        "Abandon hypotheses contradicted by evidence. Do not invent flags."
    )


def planner_system_phase(phase: Phase) -> str:
    return (
        f"You are planning the {phase.value.upper()} phase of a CTF solve. "
        "Emit ONLY JSON with keys: subgoals "
        "(list of {id,goal,rationale,depends_on}), abandon, parallel_max, notes. "
        f"All subgoals must stay inside the {phase.value} phase — "
        "do not jump ahead. Keep goals short and tool-executable. "
        "Do not invent flags."
    )
