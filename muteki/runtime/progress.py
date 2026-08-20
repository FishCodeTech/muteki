"""Progress v0: activity, candidates, strong information, and goals stay distinct."""

from __future__ import annotations

from dataclasses import dataclass, field, replace
from enum import Enum
from typing import Mapping

from muteki.epistemic.contracts import canonical_digest
from muteki.epistemic.sqlite_store import CommandEvent, EpistemicSQLiteStore


class ProgressKind(str, Enum):
    ACTIVITY = "activity"
    CANDIDATE = "candidate"
    INFORMATION = "information"
    GOAL_UNIT = "goal_unit"


@dataclass(frozen=True, slots=True)
class ProgressOccurrence:
    occurrence_id: str
    branch_id: str
    attempt_id: str
    kind: ProgressKind
    basis_digest: str
    canonical_seq: int
    goal_unit: str = ""

    @property
    def dedupe_key(self) -> str:
        return canonical_digest({
            "attempt_id": self.attempt_id, "basis_digest": self.basis_digest,
            "branch_id": self.branch_id, "canonical_seq": self.canonical_seq,
            "goal_unit": self.goal_unit, "kind": self.kind.value,
            "occurrence_id": self.occurrence_id,
        })


@dataclass(frozen=True, slots=True)
class BranchProgress:
    information_head: int = 0
    activity_count: int = 0
    candidate_count: int = 0
    information_count: int = 0
    barren_attempts: int = 0


@dataclass(frozen=True, slots=True)
class ProgressProjection:
    branches: Mapping[str, BranchProgress] = field(default_factory=dict)
    seen_occurrences: frozenset[str] = frozenset()
    verified_goal_units: frozenset[str] = frozenset()
    expected_goal_units: int = 1

    @property
    def goal_complete(self) -> bool:
        return len(self.verified_goal_units) >= self.expected_goal_units

    def apply(self, occurrence: ProgressOccurrence) -> "ProgressProjection":
        key = occurrence.dedupe_key
        if key in self.seen_occurrences:
            return self
        branches = dict(self.branches)
        current = branches.get(occurrence.branch_id, BranchProgress())
        if occurrence.kind is ProgressKind.ACTIVITY:
            current = replace(current, activity_count=current.activity_count + 1)
        elif occurrence.kind is ProgressKind.CANDIDATE:
            current = replace(current, candidate_count=current.candidate_count + 1)
        elif occurrence.kind is ProgressKind.INFORMATION:
            if occurrence.canonical_seq > current.information_head:
                current = replace(
                    current, information_head=occurrence.canonical_seq,
                    information_count=current.information_count + 1,
                    barren_attempts=0,
                )
        elif occurrence.kind is ProgressKind.GOAL_UNIT:
            if not occurrence.goal_unit:
                raise ValueError("goal progress requires a distinct goal unit")
        branches[occurrence.branch_id] = current
        goal_units = self.verified_goal_units
        if occurrence.kind is ProgressKind.GOAL_UNIT:
            goal_units = goal_units | {occurrence.goal_unit}
        return ProgressProjection(
            branches=branches,
            seen_occurrences=self.seen_occurrences | {key},
            verified_goal_units=goal_units,
            expected_goal_units=self.expected_goal_units,
        )

    def mark_attempt_barren(self, branch_id: str) -> "ProgressProjection":
        branches = dict(self.branches)
        current = branches.get(branch_id, BranchProgress())
        branches[branch_id] = replace(
            current, barren_attempts=current.barren_attempts + 1)
        return replace(self, branches=branches)


class ProgressLedger:
    """Canonical write/rebuild wrapper for the pure progress projection."""

    def __init__(self, *, store: EpistemicSQLiteStore,
                 expected_goal_units: int = 1) -> None:
        self.store = store
        self.projection = ProgressProjection(
            expected_goal_units=expected_goal_units)
        for row in store.event_rows(kind="PROGRESS_RECORDED"):
            self.projection = self.projection.apply(self._from_payload(row["payload"]))
        for row in store.event_rows(kind="ATTEMPT_BARREN"):
            self.projection = self.projection.mark_attempt_barren(
                str(row["payload"]["branch_id"]))

    @staticmethod
    def _payload(occurrence: ProgressOccurrence) -> dict:
        return {
            "attempt_id": occurrence.attempt_id,
            "basis_digest": occurrence.basis_digest,
            "branch_id": occurrence.branch_id,
            "canonical_seq": occurrence.canonical_seq,
            "goal_unit": occurrence.goal_unit,
            "kind": occurrence.kind.value,
            "occurrence_id": occurrence.occurrence_id,
        }

    @staticmethod
    def _from_payload(payload: Mapping) -> ProgressOccurrence:
        return ProgressOccurrence(
            occurrence_id=str(payload["occurrence_id"]),
            branch_id=str(payload["branch_id"]),
            attempt_id=str(payload["attempt_id"]),
            kind=ProgressKind(str(payload["kind"])),
            basis_digest=str(payload["basis_digest"]),
            canonical_seq=int(payload["canonical_seq"]),
            goal_unit=str(payload.get("goal_unit") or ""),
        )

    def record(self, occurrence: ProgressOccurrence, *, occurred_at_ns: int) -> str:
        payload = self._payload(occurrence)
        result = self.store.commit_command(
            command_id=f"progress:{occurrence.dedupe_key}",
            idempotency_key=f"progress:{occurrence.dedupe_key}",
            command_payload=payload,
            events=[CommandEvent(
                f"event:progress:{occurrence.dedupe_key}", "PROGRESS_RECORDED",
                "progress-ledger", occurred_at_ns, payload)],
            committed_at_ns=occurred_at_ns,
        )
        self.projection = self.projection.apply(occurrence)
        return result.receipt_digest

    def mark_attempt_barren(self, *, branch_id: str, attempt_id: str,
                            occurred_at_ns: int) -> str:
        payload = {"attempt_id": attempt_id, "branch_id": branch_id}
        result = self.store.commit_command(
            command_id=f"progress:barren:{attempt_id}",
            idempotency_key=f"progress:barren:{attempt_id}",
            command_payload=payload,
            events=[CommandEvent(
                f"event:progress:barren:{attempt_id}", "ATTEMPT_BARREN",
                "progress-ledger", occurred_at_ns, payload)],
            committed_at_ns=occurred_at_ns,
        )
        self.projection = self.projection.mark_attempt_barren(branch_id)
        return result.receipt_digest
