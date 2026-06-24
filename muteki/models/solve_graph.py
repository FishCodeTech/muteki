"""CTF solve-graph state model (Appendix B).

Core entities: Challenge, Evidence, Hypothesis, SolveGraph. `to_summary()` is
load-bearing — it's both what L2 reads to decide the next move and what the
context compactor substitutes for history when the window fills up. So it must
be genuinely concise: confirmed evidence + active hypotheses only.
"""

from __future__ import annotations

import re
from enum import Enum
from typing import Literal, Optional

from pydantic import BaseModel, Field

Category = Literal["web", "pwn", "reverse", "crypto", "forensics", "misc"]


class HypothesisStatus(str, Enum):
    PROPOSED = "proposed"  # raised, not yet tested
    TESTING = "testing"  # under verification
    CONFIRMED = "confirmed"  # confirmed effective
    REFUTED = "refuted"  # disproven (dead-end, prune across swarm)


class Evidence(BaseModel):
    source: str  # which code/tool produced it
    fact: str  # objective fact (no speculation)
    artifact_path: Optional[str] = None
    artifact_id: Optional[str] = None  # peek the raw output
    # -- P-B provenance fields (PCSG witness) -------------------------------
    # `verified` is set by the provenance gate when the evidence enters the
    # shared graph: NOT "artifact is readable" (too weak) but "the fact's
    # content is locatable inside the artifact" via a witness check.
    verified: bool = False
    source_solver: str = ""  # which solver/worker produced it (swarm attribution)
    confidence: float = 1.0  # 1.0 when verified; downweighted otherwise
    # witness: HOW the fact was located in the artifact (substring/regex/sha256).
    # Empty when unverified. Keeps the proof, not just the claim.
    witness: Optional[str] = None  # e.g. "substring", "regex:flag\\{", "sha256"
    verifier: str = ""  # which verifier-registry fn judged it (P-B)


class Hypothesis(BaseModel):
    id: str
    statement: str  # "this is a ret2libc-exploitable stack overflow"
    rationale: str  # why it was proposed
    status: HypothesisStatus = HypothesisStatus.PROPOSED
    priority: float = 0.5  # parallel-verification ordering (RAG prior may init)
    refuted_reason: Optional[str] = None


class Challenge(BaseModel):
    id: str
    name: str
    category: Category
    points: int = 0
    description: str = ""
    attachments: list[str] = Field(default_factory=list)
    target: Optional[str] = None  # remote target address (host:port or URL)
    flag_format: str = r"flag\{.*?\}"
    # Human-readable flag shape for prompting (e.g. "WMCTF{...}"). The hard gate
    # still uses flag_format; this only keeps the model from hunting the wrong
    # wrapper when the operator supplied a custom CTF prefix.
    flag_format_hint: str = ""
    flag_format_wrapper: str = ""
    # multi-flag: how many DISTINCT flags must be collected before the run is
    # solved. Default 1 → every single-flag code path stays byte-identical (the
    # run stops on the first flag, exactly as before). >1 means keep solving until
    # N distinct flags pass the gate. Set at dispatch time (operator / frontend).
    expected_flags: int = 1
    # multi-flag MODE: decouples "save a flag" from "finish the run". Default False
    # = single-flag (the first gated flag finishes the run). True = collect mode:
    # flags are still SAVED + displayed, but a saved flag does NOT finish the run —
    # completion is `expected_flags` distinct flags (if >1) OR, when the count is
    # unknown (expected_flags<=1), never by count: the run ends on operator STOP or
    # the coordinator's no-progress pause. Lets a ladder/collection challenge gather
    # every flag instead of stopping on the first. Set at dispatch time.
    multi_flag: bool = False
    # ── rate-limited verifier (submission gate) ───────────────────────────────
    # True when the TARGET provides a scoring verifier with a per-attempt cooldown
    # or burn-lockout that punishes wrong/concurrent submissions (e.g. BreachLab
    # Specter: 3 wrong attempts / 30 min → 30 min per-player server-side lock).
    # When set, the swarm SERIALIZES submissions (one verifier run at a time across
    # all workers) and backs off globally when a lockout is seen, instead of N
    # workers each independently burning the shared per-player budget. Default
    # False → every existing CTF path is byte-identical (no gate, free concurrent
    # submission). Opt-in at dispatch time for chained-submit / rate-limited tracks.
    verifier_rate_limited: bool = False
    # ── engagement mode (Origin/Goal/Hints framing, BE-pentest-mode) ──────────
    # "ctf" (default): the goal is to recover a flag — completion is the hardcoded
    # provenance gate (_flag_ok). "pentest": the goal is operator-defined (find +
    # prove vulnerabilities in scope, produce a findings report) — completion is
    # GOAL-driven (Reason judges the goal met), with findings kept honest by the
    # SAME witness gate (muteki/swarm/verifier.py). mode="ctf" leaves every CTF
    # code path byte-identical (the pentest branches only fire when mode=="pentest").
    mode: Literal["ctf", "pentest"] = "ctf"
    goal: str = ""    # pentest: the engagement objective (drives Reason completion)
    scope: str = ""   # pentest: in-scope targets / authorization boundary


class SolveGraph(BaseModel):
    challenge: Challenge
    evidence: list[Evidence] = Field(default_factory=list)
    hypotheses: list[Hypothesis] = Field(default_factory=list)
    intermediate_artifacts: list[str] = Field(default_factory=list)
    dead_ends: list[str] = Field(default_factory=list)  # swarm-shared dead paths
    # `flag` = the FIRST accepted flag (back-compat single-flag read); `flags` =
    # every distinct accepted flag in discovery order. Invariant: flag == flags[0]
    # when any flag exists. Single-flag runs keep flag set + flags == [flag].
    flag: Optional[str] = None
    flags: list[str] = Field(default_factory=list)
    # Flags an operator marked as FALSE POSITIVES. A value that was once accepted
    # then invalidated must NEVER be re-accepted — even after the producing intent
    # is reopened and a fresh worker re-derives it (run-75379: invalidate → reopen
    # → re-find → re-accept loop). This is the DURABLE reject memory: it is folded
    # from the append-only EV_FLAG_INVALIDATED log, so it survives worker respawn
    # (worker-local `_already_found` does not). Same permanence as a placeholder.
    rejected_flags: list[str] = Field(default_factory=list)

    def add_flag(self, flag: str) -> bool:
        """Record a flag if not already present (dedup, exact-match). Keeps the
        `flag`/`flags` invariant. A flag the operator rejected as a false positive
        is permanently refused here too — so a re-derivation after reopen cannot
        slip back in via this path. Returns True if it was new."""
        if not flag or flag in self.flags or flag in self.rejected_flags:
            return False
        self.flags.append(flag)
        if self.flag is None:
            self.flag = self.flags[0]
        return True

    def reject_flag(self, flag: str) -> None:
        """Mark a flag as a false positive: drop it from the accepted set and
        remember it permanently so it can't be re-accepted. Idempotent."""
        if not flag:
            return
        if flag in self.flags:
            self.flags = [f for f in self.flags if f != flag]
            self.flag = self.flags[0] if self.flags else None
        if flag not in self.rejected_flags:
            self.rejected_flags.append(flag)

    def _next_hid(self) -> str:
        """Derive the next H-id from existing hypotheses (no shared counter)."""
        used = []
        for h in self.hypotheses:
            m = re.fullmatch(r"H(\d+)", h.id)
            if m:
                used.append(int(m.group(1)))
        return f"H{(max(used) + 1) if used else 1}"

    # -- mutation helpers (return the created/changed object) -------------
    def add_evidence(
        self,
        source: str,
        fact: str,
        artifact_id: Optional[str] = None,
        artifact_path: Optional[str] = None,
        *,
        verified: bool = False,
        source_solver: str = "",
        confidence: float = 1.0,
        witness: Optional[str] = None,
        verifier: str = "",
    ) -> Evidence:
        ev = Evidence(
            source=source, fact=fact, artifact_id=artifact_id, artifact_path=artifact_path,
            verified=verified, source_solver=source_solver, confidence=confidence,
            witness=witness, verifier=verifier,
        )
        self.evidence.append(ev)
        return ev

    def add_hypothesis(
        self, statement: str, rationale: str, priority: float = 0.5, id: Optional[str] = None
    ) -> Hypothesis:
        hid = id or self._next_hid()
        h = Hypothesis(id=hid, statement=statement, rationale=rationale, priority=priority)
        self.hypotheses.append(h)
        return h

    def get_hypothesis(self, hid: str) -> Optional[Hypothesis]:
        return next((h for h in self.hypotheses if h.id == hid), None)

    def set_status(
        self, hid: str, status: HypothesisStatus, refuted_reason: Optional[str] = None
    ) -> Optional[Hypothesis]:
        h = self.get_hypothesis(hid)
        if h is None:
            return None
        h.status = status
        if status is HypothesisStatus.REFUTED:
            h.refuted_reason = refuted_reason
            if refuted_reason and refuted_reason not in self.dead_ends:
                self.dead_ends.append(refuted_reason)
        return h

    def mark_dead_end(self, reason: str) -> None:
        if reason not in self.dead_ends:
            self.dead_ends.append(reason)

    def active_hypotheses(self) -> list[Hypothesis]:
        return [
            h
            for h in self.hypotheses
            if h.status in (HypothesisStatus.PROPOSED, HypothesisStatus.TESTING)
        ]

    # -- the load-bearing summary -----------------------------------------
    def to_summary(self, max_evidence: int = 12, max_hypotheses: int = 8,
                   max_dead_ends: Optional[int] = None) -> str:
        """Concise state for LLM decisions + context compaction.

        Only confirmed evidence and active/confirmed hypotheses; dead-ends as a
        short prune list. Deliberately small.

        max_dead_ends: dead-ends historically rode on max_hypotheses' cap (8) —
        but a PLANNER must see every dead-end or it re-proposes long-ruled-out
        directions (the P1.5 un-blinding lifted the evidence cap for exactly
        this reason and missed this one). None keeps the legacy tied cap for
        compaction callers."""
        c = self.challenge
        lines: list[str] = [
            f"# Challenge: {c.name} [{c.category}] ({c.points} pts)",
        ]
        if c.target:
            lines.append(f"Target: {c.target}")
        if c.description:
            desc = c.description.strip().replace("\n", " ")
            lines.append(f"Brief: {desc[:300]}")
        if c.attachments:
            # player-facing files a real competitor was given — READ THESE FIRST.
            # For many challenges (esp. code-review) the attachment IS the input.
            lines.append("\n## Attached files (provided to you — inspect them FIRST)")
            for a in c.attachments:
                lines.append(f"- {a}")

        if self.evidence:
            # §8.5 PCSG: once the provenance gate has judged any evidence
            # (verifier set), split Verified vs Candidates so reason/solver do
            # NOT treat unverified facts as established. Before the gate is wired
            # (P-A), behave exactly as before — single "Confirmed evidence" block.
            gated = any(ev.verifier for ev in self.evidence)
            recent = self.evidence[-max_evidence:]
            if not gated:
                lines.append("\n## Confirmed evidence")
                for ev in recent:
                    tag = f" [peek:{ev.artifact_id}]" if ev.artifact_id else ""
                    lines.append(f"- ({ev.source}) {ev.fact}{tag}")
            else:
                verified = [ev for ev in recent if ev.verified]
                candidates = [ev for ev in recent if not ev.verified]
                if verified:
                    lines.append("\n## Verified evidence")
                    for ev in verified:
                        tag = f" [peek:{ev.artifact_id}]" if ev.artifact_id else ""
                        lines.append(f"- ({ev.source}) {ev.fact}{tag}")
                if candidates:
                    lines.append("\n## Candidates / needs verification "
                                 "(do NOT treat as established)")
                    for ev in candidates:
                        tag = f" [peek:{ev.artifact_id}]" if ev.artifact_id else ""
                        cf = f" conf={ev.confidence:.1f}"
                        lines.append(f"- [UNVERIFIED]{cf} ({ev.source}) {ev.fact}{tag}")

        active = self.active_hypotheses()
        confirmed = [h for h in self.hypotheses if h.status is HypothesisStatus.CONFIRMED]
        if active or confirmed:
            lines.append("\n## Hypotheses")
            shown = sorted(
                confirmed + active, key=lambda h: (-h.priority)
            )[:max_hypotheses]
            for h in shown:
                lines.append(f"- [{h.id} {h.status.value} p={h.priority:.2f}] {h.statement}")

        if self.dead_ends:
            dcap = max_hypotheses if max_dead_ends is None else max_dead_ends
            lines.append("\n## Dead-ends (do NOT retry)")
            for d in self.dead_ends[-dcap:]:
                lines.append(f"- {d}")

        if self.intermediate_artifacts:
            lines.append(
                f"\n## Intermediate artifacts: {len(self.intermediate_artifacts)} saved"
            )

        if self.flag:
            lines.append(f"\n## FLAG: {self.flag}")
        return "\n".join(lines)
