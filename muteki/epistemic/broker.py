"""Candidate-only worker ingress and permit-bound raw capture authority."""

from __future__ import annotations

from dataclasses import dataclass
from typing import Any

from muteki.epistemic.cas import ReceiptCAS
from muteki.epistemic.contracts import canonical_digest
from muteki.epistemic.folds import RunExecution
from muteki.epistemic.sqlite_store import (
    CommandEvent,
    EpistemicSQLiteStore,
    OutboxIntent,
    ProjectionMutation,
)
from muteki.runtime.contracts import AttemptPermit, LeaseIdentity
from muteki.runtime.ports import CandidateEnvelope, CaptureChunk


class StaleLease(RuntimeError):
    pass


class CaptureIntegrityError(RuntimeError):
    """A capture cannot be bound unambiguously to live canonical authority."""


COGNITIVE_OBSERVATION_CAPTURE_PREFIX = "cognitive-observation:"
COGNITIVE_RUNTIME_OUTPUT_ACTOR = "cognitive-runtime-output-port-v1"


def _required_text(value: str, name: str) -> str:
    if type(value) is not str or not value or value != value.strip():
        raise ValueError(f"{name} must be a non-empty canonical string")
    return value


def _sha256(value: str, name: str) -> str:
    normalized = _required_text(value, name)
    if len(normalized) != 64 or any(
        character not in "0123456789abcdef" for character in normalized
    ):
        raise ValueError(f"{name} must be a lowercase sha256 digest")
    return normalized


def canonical_cognitive_observation_capture_id(
    *, permit_digest: str, spec_digest: str, observation_id: str
) -> str:
    """Derive the reserved capture id for one declared experiment observation."""

    return (
        COGNITIVE_OBSERVATION_CAPTURE_PREFIX
        + _sha256(permit_digest, "permit_digest")
        + ":"
        + _sha256(spec_digest, "spec_digest")
        + ":"
        + _required_text(observation_id, "observation_id")
    )


@dataclass(frozen=True, slots=True)
class GateInputReference:
    """Immutable capability naming one canonical, permit-bound gate input."""

    capture_id: str
    capture_event_digest: str
    manifest_digest: str
    permit_id: str
    permit_digest: str
    attempt_digest: str
    lease_digest: str
    candidate_id: str
    flag_digest: str
    flag_format_digest: str
    policy_digest: str
    ordinal: int
    raw_digest: str
    byte_count: int

    def __post_init__(self) -> None:
        for name in ("capture_id", "permit_id", "candidate_id"):
            object.__setattr__(self, name, _required_text(getattr(self, name), name))
        for name in (
            "capture_event_digest",
            "manifest_digest",
            "permit_digest",
            "attempt_digest",
            "lease_digest",
            "flag_digest",
            "flag_format_digest",
            "policy_digest",
            "raw_digest",
        ):
            object.__setattr__(self, name, _sha256(getattr(self, name), name))
        if type(self.ordinal) is not int or self.ordinal < 0:
            raise ValueError("ordinal must be a non-negative integer")
        if type(self.byte_count) is not int or self.byte_count < 0:
            raise ValueError("byte_count must be a non-negative integer")


def _canonical_permit_event(
    store: EpistemicSQLiteStore,
    *,
    permit_digest: str,
    permit_id: str,
    attempt_digest: str,
    lease_digest: str,
) -> dict[str, Any]:
    """Resolve one admission event and reject ambiguous or internally split lineage."""

    matches = [
        row
        for row in store.event_rows(kind="ATTEMPT_ADMITTED")
        if row["payload"].get("permit_digest") == permit_digest
    ]
    if len(matches) != 1:
        raise CaptureIntegrityError(
            "permit must resolve to exactly one canonical admission event"
        )
    payload = dict(matches[0]["payload"])
    permit_body = payload.get("permit")
    if not isinstance(permit_body, dict):
        raise CaptureIntegrityError("canonical admission is missing the permit body")
    if canonical_digest(permit_body) != permit_digest:
        raise CaptureIntegrityError("canonical admission permit body digest mismatch")
    exact = {
        "attempt_digest": attempt_digest,
        "lease_digest": lease_digest,
        "permit_id": permit_id,
    }
    if any(payload.get(name) != value for name, value in exact.items()):
        raise CaptureIntegrityError("canonical admission identity binding mismatch")
    if permit_body.get("permit_id") != permit_id:
        raise CaptureIntegrityError("canonical permit id is internally inconsistent")
    if permit_body.get("lease_digest") != lease_digest:
        raise CaptureIntegrityError("canonical permit lease is internally inconsistent")
    if permit_body.get("policy_digest") != payload.get("policy_digest"):
        raise CaptureIntegrityError("canonical permit policy is internally inconsistent")
    return matches[0]


def _canonical_active_launch(
    store: EpistemicSQLiteStore,
    *,
    permit_digest: str,
    permit_id: str,
    attempt_id: str,
    attempt_digest: str,
    lease_digest: str,
    scope_digest: str,
) -> dict[str, Any]:
    launches = [
        row
        for row in store.event_rows(kind="WORKER_LAUNCH_PREPARED")
        if row["payload"].get("permit_id") == permit_id
    ]
    if len(launches) != 1:
        raise CaptureIntegrityError(
            "permit must resolve to exactly one canonical launch event"
        )
    payload = launches[0]["payload"]
    exact = {
        "attempt_digest": attempt_digest,
        "lease_digest": lease_digest,
        "permit_digest": permit_digest,
        "scope_digest": scope_digest,
    }
    if any(payload.get(name) != value for name, value in exact.items()):
        raise CaptureIntegrityError("canonical launch identity binding mismatch")
    terminal = [
        row
        for kind in ("WORKER_TERMINAL", "WORKER_UNKNOWN")
        for row in store.event_rows(kind=kind)
        if row["payload"].get("permit_id") == permit_id
    ]
    budget_terminal = [
        row
        for kind in ("BUDGET_SETTLED", "BUDGET_USAGE_UNKNOWN")
        for row in store.event_rows(kind=kind)
        if row["payload"].get("attempt_id") == attempt_id
    ]
    if terminal or budget_terminal:
        raise CaptureIntegrityError("attempt capture authority is terminal")
    return launches[0]


@dataclass(slots=True)
class CaptureSession:
    """Host capture port scoped to one canonically admitted attempt permit."""

    store: EpistemicSQLiteStore
    cas: ReceiptCAS
    permit: AttemptPermit

    def __post_init__(self) -> None:
        if type(self.permit) is not AttemptPermit:
            raise TypeError("capture authority requires an AttemptPermit")
        self._require_canonical_permit()
        self._require_live_scope(occurred_at_ns=None)
        self._manifest_head()

    @property
    def lease(self) -> LeaseIdentity:
        return self.permit.lease

    def _require_canonical_permit(self) -> dict[str, Any]:
        self.store.verify()
        row = _canonical_permit_event(
            self.store,
            permit_digest=self.permit.digest,
            permit_id=self.permit.permit_id,
            attempt_digest=self.permit.lease.attempt.digest,
            lease_digest=self.permit.lease.digest,
        )
        payload = row["payload"]
        if canonical_digest(payload["permit"]) != canonical_digest(
            self.permit.canonical_body()
        ):
            raise CaptureIntegrityError("supplied permit differs from canonical admission")
        if payload.get("scope_digest") != self.permit.lease.attempt.scope.digest:
            raise CaptureIntegrityError("canonical admission scope binding mismatch")
        return row

    def _require_live_scope(self, *, occurred_at_ns: int | None) -> None:
        state = self.store.state()
        scope = self.permit.lease.attempt.scope
        if (
            state.run_id != scope.run_id
            or state.run_fence_epoch != scope.run_fence_epoch
            or state.execution_generation != scope.execution_generation
            or state.run_execution is not RunExecution.RUNNING
        ):
            raise CaptureIntegrityError("capture scope is not the current running scope")
        if occurred_at_ns is not None:
            if type(occurred_at_ns) is not int or occurred_at_ns < 0:
                raise ValueError("occurred_at_ns must be a non-negative integer")
            if occurred_at_ns >= self.permit.expires_at_ns:
                raise CaptureIntegrityError("capture permit is expired")

    def _require_active_launch(self) -> None:
        _canonical_active_launch(
            self.store,
            permit_digest=self.permit.digest,
            permit_id=self.permit.permit_id,
            attempt_id=self.permit.lease.attempt.attempt_id,
            attempt_digest=self.permit.lease.attempt.digest,
            lease_digest=self.permit.lease.digest,
            scope_digest=self.permit.lease.attempt.scope.digest,
        )

    @staticmethod
    def _manifest_body(payload: dict[str, Any]) -> dict[str, Any]:
        return {
            name: payload[name]
            for name in (
                "attempt_digest",
                "byte_count",
                "candidate_id",
                "capture_id",
                "flag_digest",
                "flag_format_digest",
                "lease_digest",
                "ordinal",
                "permit_digest",
                "policy_digest",
                "previous_manifest_digest",
                "raw_digest",
                "stream",
                "terminal",
            )
        }

    def _manifest_head(self) -> tuple[int, str, bool]:
        rows = [
            row
            for row in self.store.event_rows(kind="CAPTURE_MANIFEST_ADVANCED")
            if row["payload"].get("permit_digest") == self.permit.digest
        ]
        expected_ordinal = 0
        previous_digest = ""
        terminal = False
        for row in rows:
            payload = dict(row["payload"])
            if terminal:
                raise CaptureIntegrityError("capture exists after terminal manifest")
            if payload.get("ordinal") != expected_ordinal:
                raise CaptureIntegrityError("capture manifest ordinals are not contiguous")
            if payload.get("previous_manifest_digest") != previous_digest:
                raise CaptureIntegrityError("capture manifest chain is discontinuous")
            if payload.get("lease_digest") != self.permit.lease.digest:
                raise CaptureIntegrityError("capture manifest lease binding mismatch")
            try:
                manifest_body = self._manifest_body(payload)
            except KeyError as exc:
                raise CaptureIntegrityError("capture manifest is incomplete") from exc
            manifest_digest = canonical_digest(manifest_body)
            if payload.get("manifest_digest") != manifest_digest:
                raise CaptureIntegrityError("capture manifest digest mismatch")
            previous_digest = manifest_digest
            terminal = payload.get("terminal") is True
            expected_ordinal += 1
        return expected_ordinal, previous_digest, terminal

    def _seal(
        self,
        *,
        capture_id: str,
        stream: str,
        data: bytes,
        occurred_at_ns: int,
        terminal: bool,
        candidate_id: str = "",
        flag_digest: str = "",
        flag_format_digest: str = "",
        policy_digest: str = "",
        actor: str = "capture-port",
        guard_action: str = "capture",
        authority_capability: object | None = None,
    ) -> tuple[CaptureChunk, dict[str, Any]]:
        capture_id = _required_text(capture_id, "capture_id")
        if type(data) is not bytes:
            raise TypeError("capture data must be exact bytes")
        if type(terminal) is not bool:
            raise TypeError("terminal must be an exact boolean")
        self._require_canonical_permit()
        self._require_live_scope(occurred_at_ns=occurred_at_ns)
        self._require_active_launch()
        ordinal, previous_manifest_digest, manifest_terminal = self._manifest_head()
        if manifest_terminal:
            raise CaptureIntegrityError("capture manifest is already terminal")
        if any(
            row["payload"].get("capture_id") == capture_id
            for row in self.store.event_rows(kind="CAPTURE_CHUNK_SEALED")
        ):
            raise CaptureIntegrityError("capture_id has already been sealed")

        sealed = self.cas.seal_bytes(data)
        manifest_body = {
            "attempt_digest": self.permit.lease.attempt.digest,
            "byte_count": sealed.byte_count,
            "candidate_id": candidate_id,
            "capture_id": capture_id,
            "flag_digest": flag_digest,
            "flag_format_digest": flag_format_digest,
            "lease_digest": self.permit.lease.digest,
            "ordinal": ordinal,
            "permit_digest": self.permit.digest,
            "policy_digest": policy_digest,
            "previous_manifest_digest": previous_manifest_digest,
            "raw_digest": sealed.digest,
            "stream": stream,
            "terminal": terminal,
        }
        manifest_digest = canonical_digest(manifest_body)
        payload = {**manifest_body, "manifest_digest": manifest_digest}
        guard_payload = {
            "action": guard_action,
            "attempt_digest": self.permit.lease.attempt.digest,
            "attempt_id": self.permit.lease.attempt.attempt_id,
            "lease_digest": self.permit.lease.digest,
            "lease_id": self.permit.lease.lease_id,
            "manifest_digest": manifest_digest,
            "permit_digest": self.permit.digest,
            "permit_id": self.permit.permit_id,
            "raw_digest": sealed.digest,
            "scope_digest": self.permit.lease.attempt.scope.digest,
        }
        command_id = f"capture:{self.permit.digest}:{ordinal}"
        chunk_event_id = f"event:{command_id}:chunk"
        self.store.commit_command(
            command_id=command_id,
            idempotency_key=command_id,
            command_payload=payload,
            events=[
                CommandEvent(
                    event_id=chunk_event_id,
                    kind="CAPTURE_CHUNK_SEALED",
                    actor=actor,
                    occurred_at_ns=occurred_at_ns,
                    payload=payload,
                ),
                CommandEvent(
                    event_id=f"event:{command_id}:manifest",
                    kind="CAPTURE_MANIFEST_ADVANCED",
                    actor=actor,
                    occurred_at_ns=occurred_at_ns,
                    payload=payload,
                ),
            ],
            projection_mutations=[
                ProjectionMutation("attempt_io_guard", guard_payload)
            ],
            authority_capability=authority_capability,
            committed_at_ns=occurred_at_ns,
        )
        event_rows = [
            row
            for row in self.store.event_rows(kind="CAPTURE_CHUNK_SEALED")
            if row["event_id"] == chunk_event_id
        ]
        if len(event_rows) != 1:
            raise CaptureIntegrityError("sealed capture event did not resolve uniquely")
        chunk = CaptureChunk(
            capture_id=capture_id,
            lease=self.permit.lease,
            stream=stream,
            ordinal=ordinal,
            raw_digest=sealed.digest,
            byte_count=sealed.byte_count,
            terminal=terminal,
        )
        return chunk, {
            **payload,
            "capture_event_digest": event_rows[0]["event_digest"],
        }

    def capture(
        self,
        *,
        capture_id: str,
        stream: str,
        data: bytes,
        occurred_at_ns: int,
        terminal: bool = False,
    ) -> CaptureChunk:
        if stream not in {"stdout", "stderr", "tool_result"}:
            raise ValueError("unsupported capture stream")
        if capture_id.startswith(COGNITIVE_OBSERVATION_CAPTURE_PREFIX):
            raise CaptureIntegrityError(
                "canonical cognitive output ids are reserved for the audited runner"
            )
        chunk, _ = self._seal(
            capture_id=capture_id,
            stream=stream,
            data=data,
            occurred_at_ns=occurred_at_ns,
            terminal=terminal,
        )
        return chunk

    def _capture_cognitive_output(
        self,
        *,
        capture_id: str,
        stream: str,
        data: bytes,
        occurred_at_ns: int,
        terminal: bool,
    ) -> CaptureChunk:
        """Seal output only for the exact C6 audited-runner callback.

        Ordinary callers cannot mint the reserved id namespace through ``capture``.
        The store additionally requires its separate host-only output capability and
        reserved actor before accepting these otherwise familiar capture receipts.
        """

        if stream not in {"stdout", "stderr"}:
            raise ValueError("cognitive runtime output must be stdout or stderr")
        if not capture_id.startswith(COGNITIVE_OBSERVATION_CAPTURE_PREFIX):
            raise CaptureIntegrityError("cognitive output capture id is not canonical")
        chunk, _ = self._seal(
            capture_id=capture_id,
            stream=stream,
            data=data,
            occurred_at_ns=occurred_at_ns,
            terminal=terminal,
            actor=COGNITIVE_RUNTIME_OUTPUT_ACTOR,
            guard_action="cognitive_capture",
            authority_capability=(
                self.store._cognitive_runtime_output_commit_capability
            ),
        )
        return chunk

    def seal_gate_input(
        self,
        *,
        capture_id: str,
        candidate_id: str,
        flag: str,
        flag_format: str,
        policy_digest: str,
        data: bytes,
        occurred_at_ns: int,
    ) -> GateInputReference:
        candidate_id = _required_text(candidate_id, "candidate_id")
        flag = _required_text(flag, "flag")
        flag_format = _required_text(flag_format, "flag_format")
        policy_digest = _sha256(policy_digest, "policy_digest")
        if policy_digest != self.permit.policy_digest:
            raise CaptureIntegrityError("gate policy differs from admitted permit policy")
        chunk, record = self._seal(
            capture_id=capture_id,
            stream="gate_input",
            data=data,
            occurred_at_ns=occurred_at_ns,
            terminal=False,
            candidate_id=candidate_id,
            flag_digest=canonical_digest(flag),
            flag_format_digest=canonical_digest(flag_format),
            policy_digest=policy_digest,
        )
        return GateInputReference(
            capture_id=chunk.capture_id,
            capture_event_digest=record["capture_event_digest"],
            manifest_digest=record["manifest_digest"],
            permit_id=self.permit.permit_id,
            permit_digest=self.permit.digest,
            attempt_digest=self.permit.lease.attempt.digest,
            lease_digest=self.permit.lease.digest,
            candidate_id=candidate_id,
            flag_digest=record["flag_digest"],
            flag_format_digest=record["flag_format_digest"],
            policy_digest=policy_digest,
            ordinal=chunk.ordinal,
            raw_digest=chunk.raw_digest,
            byte_count=chunk.byte_count,
        )


class CandidateBroker:
    """Workers may report candidates; this class has no promotion method."""

    def __init__(self, *, store: EpistemicSQLiteStore, permit: AttemptPermit) -> None:
        if type(permit) is not AttemptPermit:
            raise TypeError("candidate broker requires an AttemptPermit")
        self._store = store
        self._permit = permit
        self._lease = permit.lease
        _canonical_permit_event(
            store,
            permit_digest=permit.digest,
            permit_id=permit.permit_id,
            attempt_digest=permit.lease.attempt.digest,
            lease_digest=permit.lease.digest,
        )

    def submit_candidate(self, candidate: CandidateEnvelope, *, occurred_at_ns: int) -> str:
        if candidate.lease != self._lease:
            raise StaleLease("candidate lease is not current")
        result = self._store.commit_command(
            command_id=f"candidate:{candidate.candidate_id}",
            idempotency_key=f"candidate:{candidate.candidate_id}",
            command_payload={
                "artifact_digests": candidate.artifact_digests,
                "candidate_id": candidate.candidate_id,
                "kind": candidate.kind,
                "lease_digest": candidate.lease.digest,
                "permit_digest": self._permit.digest,
                "payload": candidate.payload,
            },
            events=[
                CommandEvent(
                    event_id=f"event:candidate:{candidate.candidate_id}",
                    kind="CANDIDATE_REPORTED",
                    actor="worker-broker",
                    occurred_at_ns=occurred_at_ns,
                    payload={
                        "artifact_digests": candidate.artifact_digests,
                        "candidate_id": candidate.candidate_id,
                        "kind": candidate.kind,
                        "lease_digest": candidate.lease.digest,
                        "permit_digest": self._permit.digest,
                        "payload": candidate.payload,
                    },
                )
            ],
            outbox=[
                OutboxIntent(
                    outbox_id=f"outbox:candidate:{candidate.candidate_id}",
                    topic="candidate.reported",
                    payload={"candidate_id": candidate.candidate_id},
                )
            ],
            projection_mutations=[ProjectionMutation(
                "attempt_io_guard",
                {
                    "action": "candidate",
                    "attempt_digest": self._permit.lease.attempt.digest,
                    "attempt_id": self._permit.lease.attempt.attempt_id,
                    "lease_digest": self._permit.lease.digest,
                    "lease_id": self._permit.lease.lease_id,
                    "permit_digest": self._permit.digest,
                    "permit_id": self._permit.permit_id,
                    "scope_digest": self._permit.lease.attempt.scope.digest,
                },
            )],
            committed_at_ns=occurred_at_ns,
        )
        return result.receipt_digest
