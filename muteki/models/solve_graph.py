"""CTF solve-graph state model (Appendix B).

Core entities: Challenge, Evidence, Hypothesis, SolveGraph. `to_summary()` is
load-bearing — it's both what L2 reads to decide the next move and what the
context compactor substitutes for history when the window fills up. So it must
be genuinely concise: confirmed evidence + active hypotheses only.
"""

from __future__ import annotations

import re
from enum import Enum
from typing import Any, Literal, Mapping, Never, Optional

from pydantic import BaseModel, Field, field_validator, model_validator

from muteki.solver.gate import normalize_flag_contract, normalize_flag_format

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


QuantityKind = Literal["first", "collect", "recon"]
CompletionKind = Literal["outcome", "count", "coverage"]


_GOAL_CN_NUMBERS = {
    "一": 1, "二": 2, "两": 2, "三": 3, "四": 4, "五": 5,
    "六": 6, "七": 7, "八": 8, "九": 9, "十": 10,
}
_GOAL_COUNT_RE = re.compile(
    r"(?<![\d.])(\d+|[一二两三四五六七八九十]+)(?![\d.])"
    r"(?=[^0-9\n，。；:]{0,40}(?:报告|finding|findings|flag|flags))",
    re.IGNORECASE,
)


def _goal_count(text: str) -> Optional[int]:
    match = _GOAL_COUNT_RE.search(text or "")
    if not match:
        return None
    token = match.group(1)
    if token.isdigit():
        return max(1, int(token))
    if token in _GOAL_CN_NUMBERS:
        return _GOAL_CN_NUMBERS[token]
    if token.startswith("十") and len(token) == 2 and token[1] in _GOAL_CN_NUMBERS:
        return 10 + _GOAL_CN_NUMBERS[token[1]]
    if token.endswith("十") and len(token) == 2 and token[0] in _GOAL_CN_NUMBERS:
        return _GOAL_CN_NUMBERS[token[0]] * 10
    if "十" in token and len(token) == 3:
        left, right = token.split("十", 1)
        if left in _GOAL_CN_NUMBERS and right in _GOAL_CN_NUMBERS:
            return _GOAL_CN_NUMBERS[left] * 10 + _GOAL_CN_NUMBERS[right]
    return None


class InstructionSpan(BaseModel):
    """A byte-independent character range in ``TaskContract.raw_instruction``."""

    start: int = 0
    end: int = 0
    text: str = ""


class DerivedTaskField(BaseModel):
    """Audit receipt for one field derived from the operator instruction."""

    value: Any = None
    source: Literal[
        "explicit", "planner", "heuristic", "confirmed", "default", "empty",
    ] = "empty"
    confidence: float = 0.0
    span: Optional[InstructionSpan] = None
    detail: str = ""


class TaskAttachment(BaseModel):
    path: str
    name: str
    size: int = 0
    summary: str = ""


class CompletionContract(BaseModel):
    """The only completion contract consumed by the product gates."""

    kind: Literal["ctf_flag", "outcome", "count", "coverage"] = "ctf_flag"
    goal: str = ""
    task_type: str = ""
    quantity: Optional[int] = None
    flag_format: str = ""
    flag_format_hint: str = ""
    expected_flags: int = 1
    multi_flag: bool = False
    finding_class: str = "generic"
    outcome_predicate: str = "first_valid_report"
    collect_until_coverage: bool = False


class TaskContract(BaseModel):
    """Canonical interpretation of one conversational dispatch.

    ``raw_instruction`` remains intact. Structured fields are operator-supplied
    projections, not parser guesses.
    """

    raw_instruction: str
    mode: Literal["ctf", "pentest"] = "ctf"
    title: str = ""
    category: Optional[Category] = None
    attachments: list[TaskAttachment] = Field(default_factory=list)
    execution_target: Optional[str] = None
    authorization_scope: str = ""
    completion_contract: CompletionContract = Field(default_factory=CompletionContract)
    # Kept so old event dumps still validate; new contracts leave this empty.
    derived_fields: dict[str, DerivedTaskField] = Field(default_factory=dict)


class EngagementGoal(BaseModel):
    """Pentest objective attached to Challenge.goal / Challenge.scope.

    Default: expected_findings=1, finding_class parsed from goal text or generic.
    success_predicate defaults to gated_report (report collection after
    independent reproduction and value judgment).
    """
    raw: str = ""
    finding_class: str = "generic"
    quantity: QuantityKind = "first"
    expected_findings: int = 1
    collect_until_coverage: bool = False
    success_predicate: str = "gated_report"
    completion_kind: CompletionKind = "outcome"
    outcome_predicate: str = "first_valid_report"


def parse_engagement_goal(raw: str) -> EngagementGoal:
    """Map operator goal text onto EngagementGoal. Unknown text stays generic."""
    text = (raw or "").strip()
    low = text.lower()
    finding_class = "generic"
    if any(h in text or h in low for h in (
        "rce", "远程代码", "命令注入", "command injection", "os command",
    )):
        finding_class = "rce"
    elif any(h in text or h in low for h in (
        "idor", "越权", "bola", "broken access", "未授权",
    )):
        finding_class = "idor"
    elif any(h in low for h in ("sqli", "sql注入", "sql injection")):
        finding_class = "sqli"
    elif any(h in low for h in ("xss", "跨站")):
        finding_class = "xss"
    elif "ssrf" in low:
        finding_class = "ssrf"
    quantity: QuantityKind = "first"
    expected = 1
    until_coverage = False
    completion_kind: CompletionKind = "outcome"
    outcome_predicate = "first_valid_report"
    if any(h in text or h in low for h in (
        "后台shell", "后台 shell", "getshell", "get shell", "shell access",
    )):
        finding_class = "rce"
        outcome_predicate = "shell_access"
    elif any(h in text or h in low for h in (
        "命令执行", "执行命令", "command execution", "command injection", "rce",
    )):
        finding_class = "rce"
        outcome_predicate = "command_execution"
    elif any(h in text or h in low for h in (
        "管理后台", "管理员后台", "后台权限", "admin access", "administrator access",
    )):
        outcome_predicate = "admin_access"
    if any(h in text or h in low for h in (
        "侦察", "recon", "测绘", "全面审计", "完整审计", "覆盖全部", "全面测试",
    )):
        quantity = "recon"
        until_coverage = True
        completion_kind = "coverage"
    elif any(h in text or h in low for h in ("收集", "全部", "所有", "collect")):
        quantity = "collect"
        requested_count = _goal_count(text)
        if requested_count is not None:
            expected = requested_count
            until_coverage = False
            completion_kind = "count"
        else:
            expected = 1
            until_coverage = True
            completion_kind = "coverage"
    else:
        requested_count = _goal_count(text)
        if requested_count is not None:
            expected = requested_count
            completion_kind = "count"
    return EngagementGoal(
        raw=text,
        finding_class=finding_class or "generic",
        quantity=quantity,
        expected_findings=max(1, expected),
        collect_until_coverage=until_coverage,
        success_predicate="gated_report",
        completion_kind=completion_kind,
        outcome_predicate=outcome_predicate,
    )


def engagement_goal_of(challenge: "Challenge") -> EngagementGoal:
    contract = getattr(challenge, "task_contract", None)
    if contract is not None and contract.mode == "pentest":
        completion = contract.completion_contract
        return EngagementGoal(
            raw=completion.goal,
            finding_class=completion.finding_class or "generic",
            quantity=(
                "recon" if completion.kind == "coverage"
                else "collect" if completion.kind == "count"
                else "first"
            ),
            expected_findings=max(1, int(completion.quantity or 1)),
            collect_until_coverage=completion.kind == "coverage",
            success_predicate="gated_report",
            completion_kind=(
                completion.kind
                if completion.kind in {"outcome", "count", "coverage"}
                else "outcome"
            ),
            outcome_predicate=completion.outcome_predicate,
        )
    stored = getattr(challenge, "engagement", None)
    if stored is not None:
        return stored
    if getattr(challenge, "mode", "ctf") != "pentest":
        return EngagementGoal(raw="", finding_class="", expected_findings=1)
    return parse_engagement_goal(getattr(challenge, "goal", "") or "")


def apply_expected_findings(
    engagement: EngagementGoal, expected_findings: int,
) -> EngagementGoal:
    """Apply the operator's explicit report count from the dispatch form.

    A filled number is a known collection size, including N=1. Open-ended
    collect is only when the count field is left blank.
    """
    want = max(1, int(expected_findings))
    quantity = engagement.quantity
    if quantity == "recon":
        return engagement.model_copy(update={
            "expected_findings": want,
            "completion_kind": "count",
            "quantity": "collect",
            "collect_until_coverage": False,
        })
    if want > 1 or quantity == "collect":
        quantity = "collect"
    return engagement.model_copy(update={
        "expected_findings": want,
        "quantity": quantity,
        "collect_until_coverage": False,
        "completion_kind": "count",
    })


def engagement_reports_complete(
    engagement: EngagementGoal, n_accepted: int,
) -> bool:
    """True when the accepted report collection satisfies the engagement."""
    if engagement.success_predicate not in {"gated_finding", "gated_report"}:
        return False
    if engagement.completion_kind == "coverage":
        return False
    got = max(0, int(n_accepted))
    if engagement.completion_kind == "outcome":
        return got >= 1
    q = engagement.quantity
    if q == "recon":
        return False
    if q == "collect":
        if engagement.collect_until_coverage:
            return False
        return got >= max(1, int(engagement.expected_findings or 1))
    if q == "first":
        return got >= max(1, int(engagement.expected_findings or 1))
    _exhaustive: Never = q
    raise AssertionError(_exhaustive)


class Challenge(BaseModel):
    id: str
    name: str
    category: Category
    points: int = 0
    description: str = ""
    attachments: list[str] = Field(default_factory=list)
    target: Optional[str] = None  # remote target address (host:port or URL)
    flag_format: str = ""
    # Human-readable Flag shape for prompting (e.g. "WMCTF{...}"). It never
    # validates or changes a value submitted by the model.
    flag_format_hint: str = ""
    flag_format_wrapper: str = ""

    @model_validator(mode="before")
    @classmethod
    def _normalize_flag_format_and_wrapper(cls, value: Any) -> Any:
        # Direct callers can bypass the Web contract/Driver.  Resolve the
        # separate custom-wrapper shape here with the same precedence as the
        # dispatch contract: custom/empty uses its wrapper, explicit formats and
        # token mode remain authoritative.
        if isinstance(value, Mapping):
            contract = normalize_flag_contract(
                value.get("flag_format"),
                value.get("flag_format_wrapper"),
            )
            normalized = dict(value)
            normalized["flag_format"] = contract.flag_format
            normalized["flag_format_wrapper"] = contract.flag_format_wrapper
            if contract.flag_format_wrapper and not str(
                normalized.get("flag_format_hint") or ""
            ).strip():
                normalized["flag_format_hint"] = contract.flag_format_wrapper
            return normalized
        return value

    @field_validator("flag_format", mode="before")
    @classmethod
    def _normalize_flag_format_contract(cls, value: Any) -> str:
        # Challenge is constructed by the Web, Command/Gateway, competition and
        # continuation paths.  Keep their persisted runtime contract identical
        # even when a caller supplied a human example such as ``flag{...}``.
        return normalize_flag_format(value)
    # multi-flag: how many DISTINCT flags must be collected before the run is
    # solved. Default 1 → every single-flag code path stays byte-identical (the
    # run stops on the first flag, exactly as before). >1 means keep solving until
    # N distinct model-submitted Flags are recorded. Set at dispatch time.
    expected_flags: int = 1
    # Flags already accepted by an external platform before this execution
    # starts. They count toward the same expected_flags total and are projected
    # to the shared graph so Decide and every Worker begin from the real progress.
    initial_flags: list[str] = Field(default_factory=list)
    # Competition-bound runs are stopped by the platform after remote verdicts
    # confirm the configured flag count. Model submissions remain visible as
    # candidates but never complete the local coordinator early.
    platform_confirmation_required: bool = False
    # multi-flag MODE: decouples "save a flag" from "finish the run". Default False
    # = single-flag (the first submitted Flag finishes the run). True = collect mode:
    # flags are still SAVED + displayed, but a saved flag does NOT finish the run —
    # completion is `expected_flags` distinct flags (if >1) OR, when the count is
    # unknown (expected_flags<=1), never by count: the run ends on operator STOP or
    # the coordinator's no-progress pause. Lets a ladder/collection challenge gather
    # every flag instead of stopping on the first. Set at dispatch time.
    multi_flag: bool = False
    # Platform-scoped autonomous runs may remove the operator-input capability
    # from the Worker prompt, Skill projection, CLI, and host ingress.
    allow_operator_input: bool = True
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
    # "ctf" (default): the goal is to recover a Flag — completion follows explicit
    # model submission. "pentest": the goal is operator-defined (find +
    # prove vulnerabilities in scope). Product success is gated_report (accepted
    # reports after independent reproduction and value judgment);
    # Reason verdict=complete is a planning signal only. mode="ctf" leaves every CTF
    # code path byte-identical (the pentest branches only fire when mode=="pentest").
    mode: Literal["ctf", "pentest"] = "ctf"
    goal: str = ""    # pentest: the engagement objective (drives Reason planning)
    scope: str = ""   # pentest: in-scope targets / authorization boundary
    engagement: Optional[EngagementGoal] = None
    # Eval-only bypass (tsecbench-style ranges: pentest prompt shape, but there IS
    # a flag and a judge). Product default False — not a product success condition.
    # When True, Reason complete may end the run only after a provenance-admitted
    # flag is in the store (salvage from verified evidence is attempted first).
    pentest_flag_required: bool = False
    # Canonical conversational dispatch contract. Older persisted runs do not
    # carry it; their legacy fields remain readable and the UI synthesizes a
    # read-only task-understanding card during replay.
    task_contract: Optional[TaskContract] = None


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
    findings: list[dict] = Field(default_factory=list)
    rejected_findings: list[str] = Field(default_factory=list)
    vuln_reports: list[dict] = Field(default_factory=list)

    def add_flag(self, flag: str) -> bool:
        """Record a flag if not already present (dedup, exact-match). Keeps the
        `flag`/`flags` invariant. A flag the operator rejected as a false positive
        is permanently refused here too — so a re-derivation after reopen cannot
        slip back in via this path. Returns True if it was new."""
        if flag is None or flag in self.flags or flag in self.rejected_flags:
            return False
        self.flags.append(flag)
        if self.flag is None:
            self.flag = self.flags[0]
        return True

    def reject_flag(self, flag: Optional[str]) -> None:
        """Mark a flag as a false positive: drop it from the accepted set and
        remember it permanently so it can't be re-accepted. Idempotent."""
        if flag is None:
            return
        if flag in self.flags:
            self.flags = [f for f in self.flags if f != flag]
            self.flag = self.flags[0] if self.flags else None
        if flag not in self.rejected_flags:
            self.rejected_flags.append(flag)

    @staticmethod
    def _finding_identity(finding: dict) -> str:
        cls = str((finding or {}).get("finding_class") or "").strip().lower()
        resource = str((finding or {}).get("resource_id") or "").strip()
        a = str((finding or {}).get("identity_a") or "").strip()
        b = str((finding or {}).get("identity_b") or "").strip()
        ids = tuple(sorted((a, b), key=str.casefold))
        return f"{cls}::{resource}::{ids[0]}::{ids[1]}"

    def add_finding(self, finding: dict) -> bool:
        """Record a gated finding if its key is new and not operator-rejected."""
        if not finding:
            return False
        key = self._finding_identity(finding)
        if not key or key in self.rejected_findings:
            return False
        if any(self._finding_identity(f) == key for f in self.findings):
            return False
        self.findings.append(dict(finding))
        return True

    def reject_finding(self, finding: dict | str) -> None:
        key = finding if isinstance(finding, str) else self._finding_identity(finding or {})
        if not key:
            return
        self.findings = [f for f in self.findings if self._finding_identity(f) != key]
        if key not in self.rejected_findings:
            self.rejected_findings.append(key)

    def add_vuln_report(self, report: dict) -> bool:
        if not report:
            return False
        rid = str(report.get("report_id") or "").strip()
        if not rid:
            rid = self._finding_identity(report)
        if not rid:
            return False
        if any(str(item.get("report_id") or "") == rid for item in self.vuln_reports):
            return False
        row = dict(report)
        row["report_id"] = rid
        self.vuln_reports.append(row)
        return True

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

        if self.flag is not None:
            lines.append(f"\n## FLAG: {self.flag}")
        if self.findings:
            lines.append("\n## Gated findings")
            for f in self.findings:
                cls = f.get("finding_class", "")
                res = f.get("resource_id", "")
                lines.append(f"- {cls} {res} ({f.get('identity_a','')} / {f.get('identity_b','')})")
        return "\n".join(lines)
