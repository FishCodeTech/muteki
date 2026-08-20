"""Host-only promotion and hardcoded provenance-gate commit authority."""

from __future__ import annotations

import json
from dataclasses import dataclass, field
from enum import Enum
from typing import Any

from muteki.epistemic.cas import CASIntegrityError, ReceiptCAS
from muteki.epistemic.broker import (
    GateInputReference,
    _canonical_active_launch,
    _canonical_permit_event,
)
from muteki.epistemic.contracts import canonical_digest
from muteki.epistemic.folds import RunExecution
from muteki.epistemic.sqlite_store import (
    CommandEvent,
    EpistemicSQLiteStore,
    FlagAcceptedOutboxV1,
    IntegrityError,
    OutboxIntent,
    ProjectionMutation,
)
from muteki.runtime.contracts import AttemptPermit
from muteki.solver.gate import flag_ok


class ReceiptTier(str, Enum):
    DIRECT_EXTRACTOR = "direct_extractor"
    DERIVATION_REPLAY = "derivation_replay"
    INDEPENDENT_REPRODUCTION = "independent_reproduction"
    HETEROGENEOUS_REVIEW = "heterogeneous_review"
    SAME_MODEL_REVIEW = "same_model_review"


@dataclass(frozen=True, slots=True)
class PromotionRequest:
    promotion_id: str
    claim_id: str
    observation_id: str
    observation_artifact_digest: str
    invocation_digest: str
    result_digest: str
    evidence_link_digest: str
    verifier_tier: ReceiptTier

    def validate(self) -> None:
        required = {
            "promotion_id": self.promotion_id, "claim_id": self.claim_id,
            "observation_id": self.observation_id,
            "observation_artifact_digest": self.observation_artifact_digest,
            "invocation_digest": self.invocation_digest,
            "result_digest": self.result_digest,
            "evidence_link_digest": self.evidence_link_digest,
        }
        missing = [name for name, value in required.items() if not value]
        if missing:
            raise ValueError(f"promotion lineage is incomplete: {', '.join(missing)}")
        if self.verifier_tier is ReceiptTier.SAME_MODEL_REVIEW:
            raise ValueError("same-model review alone cannot promote a claim")


class PromotionAuthority:
    def __init__(self, store: EpistemicSQLiteStore) -> None:
        self._store = store

    def promote(self, request: PromotionRequest, *, occurred_at_ns: int) -> str:
        request.validate()
        result = self._store.commit_command(
            command_id=f"promotion:{request.promotion_id}",
            idempotency_key=f"promotion:{request.promotion_id}",
            command_payload={
                "claim_id": request.claim_id,
                "evidence_link_digest": request.evidence_link_digest,
                "invocation_digest": request.invocation_digest,
                "observation_artifact_digest": request.observation_artifact_digest,
                "observation_id": request.observation_id,
                "promotion_id": request.promotion_id,
                "result_digest": request.result_digest,
                "verifier_tier": request.verifier_tier.value,
            },
            events=[CommandEvent(
                event_id=f"event:promotion:{request.promotion_id}",
                kind="CLAIM_PROMOTED", actor="promotion-authority",
                occurred_at_ns=occurred_at_ns,
                payload={
                    "claim_id": request.claim_id,
                    "evidence_link_digest": request.evidence_link_digest,
                    "observation_artifact_digest": request.observation_artifact_digest,
                    "observation_id": request.observation_id,
                    "promotion_id": request.promotion_id,
                    "verifier_tier": request.verifier_tier.value,
                },
            )],
            committed_at_ns=occurred_at_ns,
        )
        return result.receipt_digest


@dataclass(frozen=True, slots=True)
class GateDecision:
    accepted: bool
    flag: str
    snapshot_digest: str
    receipt_digest: str


@dataclass(frozen=True, slots=True)
class AcceptedFlagPublicationV1:
    publication_id: str
    evaluation_id: str
    attempt_digest: str
    candidate_id: str
    capture_event_digest: str
    lease_digest: str
    permit_digest: str
    policy_digest: str
    manifest_digest: str
    snapshot_digest: str
    flag: str = field(repr=False)
    flag_digest: str
    flag_object_digest: str = field(repr=False)
    flag_byte_count: int
    accepted_event_digest: str
    gate_receipt_digest: str


class GateInputRejected(RuntimeError):
    """The supplied reference does not resolve to one exact canonical gate input."""


def _is_lower_sha256(value: object) -> bool:
    return (
        type(value) is str
        and len(value) == 64
        and all(character in "0123456789abcdef" for character in value)
    )


def _receipt_for_event(store: EpistemicSQLiteStore, event_digest: str):
    try:
        return store.resolve_receipt_for_event(event_digest)
    except (IntegrityError, KeyError) as exc:
        raise GateInputRejected(
            "accepted publication event receipt did not resolve"
        ) from exc


def resolve_accepted_flag_publication(
    *,
    store: EpistemicSQLiteStore,
    cas: ReceiptCAS,
    attempt_digest: str,
    flag: str,
) -> AcceptedFlagPublicationV1:
    """Resolve one exact accepted flag through one stable local read snapshot."""

    if not isinstance(store, EpistemicSQLiteStore):
        raise GateInputRejected("store must be EpistemicSQLiteStore")
    if not isinstance(cas, ReceiptCAS):
        raise GateInputRejected("cas must be ReceiptCAS")
    if not _is_lower_sha256(attempt_digest):
        raise GateInputRejected("attempt digest is malformed")
    if type(flag) is not str or not flag:
        raise GateInputRejected("flag must be exact non-empty text")
    with store.stable_read_snapshot():
        try:
            return _resolve_accepted_flag_publication(
                store=store,
                cas=cas,
                attempt_digest=attempt_digest,
                flag=flag,
            )
        except GateInputRejected:
            raise
        except (
            CASIntegrityError,
            IntegrityError,
            KeyError,
            OSError,
            TypeError,
            UnicodeError,
            ValueError,
        ) as exc:
            raise GateInputRejected(
                "accepted publication proof did not resolve"
            ) from exc


def _resolve_accepted_flag_publication(
    *,
    store: EpistemicSQLiteStore,
    cas: ReceiptCAS,
    attempt_digest: str,
    flag: str,
) -> AcceptedFlagPublicationV1:
    """Resolve an accepted flag while the caller holds a stable store snapshot."""
    flag_digest = canonical_digest(flag)
    matches = [
        row
        for row in store.event_rows(kind="FLAG_ACCEPTED")
        if row["payload"].get("attempt_digest") == attempt_digest
        and row["payload"].get("flag_digest") == flag_digest
    ]
    if len(matches) != 1:
        raise GateInputRejected(
            "accepted flag must resolve to one exact same-attempt gate event"
        )
    gate = matches[0]
    payload = gate["payload"]
    required = {
        "accepted",
        "attempt_digest",
        "candidate_id",
        "capture_event_digest",
        "evaluation_id",
        "flag_digest",
        "flag_format_digest",
        "lease_digest",
        "manifest_digest",
        "permit_digest",
        "policy_digest",
        "raw_digest",
        "snapshot_digest",
    }
    if (
        set(payload) != required
        or payload.get("accepted") is not True
        or store.actor_for_event(gate["event_digest"]) != "hardcoded-gate"
    ):
        raise GateInputRejected("accepted gate event is malformed")
    for name in (
        "attempt_digest",
        "capture_event_digest",
        "evaluation_id",
        "flag_digest",
        "flag_format_digest",
        "lease_digest",
        "manifest_digest",
        "permit_digest",
        "policy_digest",
        "raw_digest",
        "snapshot_digest",
    ):
        if not _is_lower_sha256(payload.get(name)):
            raise GateInputRejected(f"accepted gate {name} is malformed")
    evaluation_id = payload["evaluation_id"]
    command_id = f"gate:{evaluation_id}"
    if gate.get("event_id") != f"event:gate:{evaluation_id}":
        raise GateInputRejected("accepted gate event identity is false")
    try:
        receipt = store.resolve_receipt_for_event(gate["event_digest"])
    except (IntegrityError, KeyError) as exc:
        raise GateInputRejected("accepted gate receipt did not resolve") from exc
    if (
        receipt.command_id != command_id
        or receipt.payload.get("event_digests") != (gate["event_digest"],)
    ):
        raise GateInputRejected("accepted gate receipt is not exact")
    rows = store._conn.execute(
        "SELECT ordinal,outbox_id,topic,payload_json,payload_digest "
        "FROM immutable_outbox WHERE command_id=? ORDER BY ordinal",
        (command_id,),
    ).fetchall()
    if len(rows) != 1:
        raise GateInputRejected(
            "accepted gate must have one immutable publication handoff"
        )
    ordinal, outbox_id, topic, payload_json, payload_digest = rows[0]
    try:
        raw_outbox = json.loads(payload_json)
        outbox = FlagAcceptedOutboxV1.from_payload(raw_outbox)
    except (TypeError, ValueError, json.JSONDecodeError) as exc:
        raise GateInputRejected("accepted flag outbox is malformed") from exc
    if (
        type(ordinal) is not int
        or ordinal != 0
        or outbox_id != f"outbox:flag:{evaluation_id}"
        or topic != "flag.accepted"
        or payload_digest != canonical_digest(raw_outbox)
        or outbox.attempt_digest != attempt_digest
        or outbox.candidate_id != payload["candidate_id"]
        or outbox.evaluation_id != evaluation_id
        or outbox.flag_digest != flag_digest
        or outbox.snapshot_digest != payload["snapshot_digest"]
    ):
        raise GateInputRejected(
            "accepted flag outbox diverges from its authority event"
        )
    try:
        flag_bytes = cas.read_verified(outbox.flag_object_digest)
        decoded_flag = flag_bytes.decode(outbox.flag_encoding, errors="strict")
    except (CASIntegrityError, OSError, UnicodeError, ValueError) as exc:
        raise GateInputRejected("accepted flag object failed verification") from exc
    if (
        len(flag_bytes) != outbox.flag_byte_count
        or decoded_flag != flag
        or canonical_digest(decoded_flag) != flag_digest
    ):
        raise GateInputRejected("accepted flag object is rebound or malformed")
    capture_rows = [
        row
        for row in store.event_rows(kind="CAPTURE_CHUNK_SEALED")
        if row["event_digest"] == payload["capture_event_digest"]
    ]
    if len(capture_rows) != 1:
        raise GateInputRejected("accepted gate capture did not resolve uniquely")
    capture_row = capture_rows[0]
    capture = capture_row["payload"]
    if (
        capture_row.get("kind") != "CAPTURE_CHUNK_SEALED"
        or store.actor_for_event(capture_row["event_digest"]) != "capture-port"
        or any(
            capture.get(name) != payload.get(name)
            for name in (
                "attempt_digest",
                "candidate_id",
                "flag_digest",
                "flag_format_digest",
                "lease_digest",
                "manifest_digest",
                "permit_digest",
                "policy_digest",
                "raw_digest",
            )
        )
    ):
        raise GateInputRejected("accepted gate capture lineage is false")
    capture_required = {
        "attempt_digest",
        "byte_count",
        "candidate_id",
        "capture_id",
        "flag_digest",
        "flag_format_digest",
        "lease_digest",
        "manifest_digest",
        "ordinal",
        "permit_digest",
        "policy_digest",
        "previous_manifest_digest",
        "raw_digest",
        "stream",
        "terminal",
    }
    if (
        set(capture) != capture_required
        or capture.get("stream") != "gate_input"
        or capture.get("terminal") is not False
        or type(capture.get("ordinal")) is not int
        or capture["ordinal"] < 0
        or type(capture.get("byte_count")) is not int
        or capture["byte_count"] < 0
        or not _is_lower_sha256(capture.get("raw_digest"))
    ):
        raise GateInputRejected("accepted gate capture is malformed")
    manifest_body = {
        name: capture[name]
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
    if canonical_digest(manifest_body) != capture["manifest_digest"]:
        raise GateInputRejected("accepted gate capture manifest digest is false")
    capture_command_id = (
        f"capture:{capture['permit_digest']}:{capture['ordinal']}"
    )
    if (
        capture_row.get("event_id") != f"event:{capture_command_id}:chunk"
        or _receipt_for_event(store, capture_row["event_digest"]).command_id
        != capture_command_id
    ):
        raise GateInputRejected("accepted gate capture authority identity is false")
    manifest_rows = [
        row
        for row in store.event_rows(kind="CAPTURE_MANIFEST_ADVANCED")
        if row["payload"].get("manifest_digest") == capture["manifest_digest"]
    ]
    if len(manifest_rows) != 1:
        raise GateInputRejected("accepted gate manifest did not resolve uniquely")
    manifest_row = manifest_rows[0]
    if (
        manifest_row["payload"] != capture
        or manifest_row.get("event_id") != f"event:{capture_command_id}:manifest"
        or manifest_row.get("seq") != capture_row.get("seq") + 1
        or store.actor_for_event(manifest_row["event_digest"]) != "capture-port"
        or _receipt_for_event(store, manifest_row["event_digest"]).digest
        != _receipt_for_event(store, capture_row["event_digest"]).digest
    ):
        raise GateInputRejected("accepted gate manifest authority identity is false")

    candidate_id = payload["candidate_id"]
    if (
        type(candidate_id) is not str
        or not candidate_id
        or candidate_id != candidate_id.strip()
    ):
        raise GateInputRejected("accepted gate candidate identity is malformed")
    expected_evaluation_id = canonical_digest(
        {
            "candidate_id": candidate_id,
            "flag_digest": flag_digest,
            "manifest_digest": capture["manifest_digest"],
            "permit_digest": capture["permit_digest"],
            "version": 1,
        }
    )
    if evaluation_id != expected_evaluation_id:
        raise GateInputRejected("accepted gate evaluation identity is false")
    try:
        raw_output = cas.read_verified(capture["raw_digest"])
    except (CASIntegrityError, OSError, ValueError) as exc:
        raise GateInputRejected("accepted gate input object failed verification") from exc
    if len(raw_output) != capture["byte_count"]:
        raise GateInputRejected("accepted gate input byte count is false")
    decoded = raw_output.decode("utf-8", errors="replace")
    snapshot = {
        "artifact_policy": "inline-capture-only-v1",
        "attempt_digest": attempt_digest,
        "candidate_id": candidate_id,
        "capture_event_digest": capture_row["event_digest"],
        "decoded_gate_input_digest": canonical_digest(decoded),
        "decoder": "utf-8-errors-replace-v1",
        "flag_digest": flag_digest,
        "lease_digest": capture["lease_digest"],
        "manifest_digest": capture["manifest_digest"],
        "permit_digest": capture["permit_digest"],
        "policy_digest": capture["policy_digest"],
        "raw_capture_digest": capture["raw_digest"],
        "raw_capture_size": capture["byte_count"],
    }
    if payload["snapshot_digest"] != canonical_digest(snapshot):
        raise GateInputRejected("accepted gate snapshot digest is false")

    admission_rows = [
        row
        for row in store.event_rows(kind="ATTEMPT_ADMITTED")
        if row["payload"].get("permit_digest") == payload["permit_digest"]
    ]
    if len(admission_rows) != 1:
        raise GateInputRejected("accepted gate permit did not resolve uniquely")
    admission_row = admission_rows[0]
    admission = admission_row["payload"]
    permit = admission.get("permit")
    attempt_id = admission.get("attempt_id")
    if (
        type(permit) is not dict
        or canonical_digest(permit) != payload["permit_digest"]
        or admission.get("attempt_digest") != attempt_digest
        or admission.get("lease_digest") != payload["lease_digest"]
        or admission.get("policy_digest") != payload["policy_digest"]
        or type(attempt_id) is not str
        or not attempt_id
        or admission_row.get("event_id") != f"event:attempt:admit:{attempt_id}"
        or store.actor_for_event(admission_row["event_digest"]) != "search-admission"
        or _receipt_for_event(store, admission_row["event_digest"]).command_id
        != f"attempt:admit:{attempt_id}"
    ):
        raise GateInputRejected("accepted gate permit lineage is false")
    return AcceptedFlagPublicationV1(
        publication_id=str(outbox_id),
        evaluation_id=evaluation_id,
        attempt_digest=attempt_digest,
        candidate_id=payload["candidate_id"],
        capture_event_digest=payload["capture_event_digest"],
        lease_digest=payload["lease_digest"],
        permit_digest=payload["permit_digest"],
        policy_digest=payload["policy_digest"],
        manifest_digest=payload["manifest_digest"],
        snapshot_digest=payload["snapshot_digest"],
        flag=decoded_flag,
        flag_digest=flag_digest,
        flag_object_digest=outbox.flag_object_digest,
        flag_byte_count=outbox.flag_byte_count,
        accepted_event_digest=gate["event_digest"],
        gate_receipt_digest=receipt.digest,
    )


class GateAuthority:
    """Resolve a sealed gate input, then call the unchanged hardcoded gate."""

    def __init__(self, *, store: EpistemicSQLiteStore, cas: ReceiptCAS,
                 artifacts: Any) -> None:
        self._store = store
        self._cas = cas
        self._artifacts = artifacts

    @staticmethod
    def evaluation_id_for(gate_input: GateInputReference) -> str:
        if type(gate_input) is not GateInputReference:
            raise TypeError("gate_input must be GateInputReference")
        return canonical_digest({
            "candidate_id": gate_input.candidate_id,
            "flag_digest": gate_input.flag_digest,
            "manifest_digest": gate_input.manifest_digest,
            "permit_digest": gate_input.permit_digest,
            "version": 1,
        })

    @staticmethod
    def _manifest_body(payload: dict[str, Any]) -> dict[str, Any]:
        try:
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
        except KeyError as exc:
            raise GateInputRejected("canonical gate-input manifest is incomplete") from exc

    def _resolve_gate_input(
        self,
        *,
        gate_input: GateInputReference,
        permit: AttemptPermit,
        candidate_id: str,
        flag: str,
        flag_format: str,
        policy_digest: str,
        occurred_at_ns: int,
    ) -> bytes:
        if type(gate_input) is not GateInputReference:
            raise TypeError("gate authority accepts only GateInputReference")
        if type(permit) is not AttemptPermit:
            raise TypeError("gate authority requires an AttemptPermit")
        if type(occurred_at_ns) is not int or occurred_at_ns < 0:
            raise ValueError("occurred_at_ns must be a non-negative integer")
        if not str(candidate_id or "").strip():
            raise GateInputRejected("candidate_id is required")
        if not str(flag or "").strip():
            raise GateInputRejected("flag is required")
        if not str(flag_format or "").strip():
            raise GateInputRejected("flag_format is required")

        exact = {
            "attempt_digest": permit.lease.attempt.digest,
            "candidate_id": str(candidate_id or "").strip(),
            "flag_digest": canonical_digest(flag),
            "flag_format_digest": canonical_digest(flag_format),
            "lease_digest": permit.lease.digest,
            "permit_digest": permit.digest,
            "permit_id": permit.permit_id,
            "policy_digest": policy_digest,
        }
        if any(not value for value in exact.values()):
            raise GateInputRejected("gate evaluation identity is incomplete")
        if any(getattr(gate_input, name) != value for name, value in exact.items()):
            raise GateInputRejected("gate-input reference was rebound to another identity")
        if policy_digest != permit.policy_digest:
            raise GateInputRejected("gate policy differs from the admitted permit")
        if occurred_at_ns >= permit.expires_at_ns:
            raise GateInputRejected("gate permit is expired")

        state = self._store.verify()
        scope = permit.lease.attempt.scope
        if (
            state.run_id != scope.run_id
            or state.run_fence_epoch != scope.run_fence_epoch
            or state.execution_generation != scope.execution_generation
            or state.run_execution is not RunExecution.RUNNING
        ):
            raise GateInputRejected("gate input is outside the current running scope")

        try:
            admission = _canonical_permit_event(
                self._store,
                permit_digest=permit.digest,
                permit_id=permit.permit_id,
                attempt_digest=permit.lease.attempt.digest,
                lease_digest=permit.lease.digest,
            )
        except RuntimeError as exc:
            raise GateInputRejected(str(exc)) from exc
        admission_payload = admission["payload"]
        if canonical_digest(admission_payload["permit"]) != canonical_digest(
            permit.canonical_body()
        ):
            raise GateInputRejected("supplied permit differs from canonical admission")
        if admission_payload.get("scope_digest") != scope.digest:
            raise GateInputRejected("canonical gate scope binding mismatch")
        try:
            _canonical_active_launch(
                self._store,
                permit_digest=permit.digest,
                permit_id=permit.permit_id,
                attempt_id=permit.lease.attempt.attempt_id,
                attempt_digest=permit.lease.attempt.digest,
                lease_digest=permit.lease.digest,
                scope_digest=scope.digest,
            )
        except RuntimeError as exc:
            raise GateInputRejected(str(exc)) from exc

        capture_matches = [
            row
            for row in self._store.event_rows(kind="CAPTURE_CHUNK_SEALED")
            if row["event_digest"] == gate_input.capture_event_digest
        ]
        if len(capture_matches) != 1:
            raise GateInputRejected("capture event did not resolve uniquely")
        capture_payload = dict(capture_matches[0]["payload"])
        expected_capture = {
            "attempt_digest": gate_input.attempt_digest,
            "byte_count": gate_input.byte_count,
            "candidate_id": gate_input.candidate_id,
            "capture_id": gate_input.capture_id,
            "flag_digest": gate_input.flag_digest,
            "flag_format_digest": gate_input.flag_format_digest,
            "lease_digest": gate_input.lease_digest,
            "manifest_digest": gate_input.manifest_digest,
            "ordinal": gate_input.ordinal,
            "permit_digest": gate_input.permit_digest,
            "policy_digest": gate_input.policy_digest,
            "raw_digest": gate_input.raw_digest,
            "stream": "gate_input",
            "terminal": False,
        }
        if any(
            capture_payload.get(name) != value
            for name, value in expected_capture.items()
        ):
            raise GateInputRejected("capture event differs from gate-input reference")
        if canonical_digest(self._manifest_body(capture_payload)) != gate_input.manifest_digest:
            raise GateInputRejected("capture manifest digest mismatch")

        manifest_matches = [
            row
            for row in self._store.event_rows(kind="CAPTURE_MANIFEST_ADVANCED")
            if row["payload"].get("manifest_digest") == gate_input.manifest_digest
        ]
        if len(manifest_matches) != 1:
            raise GateInputRejected("capture manifest did not resolve uniquely")
        if manifest_matches[0]["payload"] != capture_matches[0]["payload"]:
            raise GateInputRejected("capture event and manifest payloads disagree")

        raw_output = self._cas.read_verified(gate_input.raw_digest)
        if len(raw_output) != gate_input.byte_count:
            raise GateInputRejected("CAS byte count differs from canonical capture")
        return raw_output

    def evaluate(
        self,
        *,
        evaluation_id: str,
        candidate_id: str,
        flag: str,
        gate_input: GateInputReference,
        permit: AttemptPermit,
        flag_format: str,
        policy_digest: str,
        occurred_at_ns: int,
    ) -> GateDecision:
        if evaluation_id != self.evaluation_id_for(gate_input):
            raise GateInputRejected(
                "evaluation_id must be derived from the canonical gate input"
            )
        raw_output = self._resolve_gate_input(
            gate_input=gate_input,
            permit=permit,
            candidate_id=candidate_id,
            flag=flag,
            flag_format=flag_format,
            policy_digest=policy_digest,
            occurred_at_ns=occurred_at_ns,
        )
        decoded = raw_output.decode("utf-8", errors="replace")
        # Protocol 2 does not trust the legacy mutable ArtifactStore. Until an
        # artifact is itself captured into this permit's CAS manifest, only the
        # exact sealed gate-input bytes may establish provenance.
        accepted = flag_ok(
            flag, decoded, flag_format=flag_format, artifacts=None)
        snapshot = {
            "artifact_policy": "inline-capture-only-v1",
            "attempt_digest": gate_input.attempt_digest,
            "candidate_id": candidate_id,
            "capture_event_digest": gate_input.capture_event_digest,
            "decoded_gate_input_digest": canonical_digest(decoded),
            "decoder": "utf-8-errors-replace-v1",
            "flag_digest": canonical_digest(flag),
            "lease_digest": gate_input.lease_digest,
            "manifest_digest": gate_input.manifest_digest,
            "permit_digest": gate_input.permit_digest,
            "policy_digest": policy_digest,
            "raw_capture_digest": gate_input.raw_digest,
            "raw_capture_size": gate_input.byte_count,
        }
        snapshot_digest = canonical_digest(snapshot)
        outbox = []
        if accepted:
            try:
                flag_bytes = flag.encode("utf-8", errors="strict")
            except UnicodeEncodeError as exc:
                raise GateInputRejected("accepted flag is not strict UTF-8") from exc
            sealed_flag = self._cas.seal_bytes(flag_bytes)
            outbox.append(OutboxIntent(
                outbox_id=f"outbox:flag:{evaluation_id}", topic="flag.accepted",
                payload=FlagAcceptedOutboxV1(
                    attempt_digest=gate_input.attempt_digest,
                    candidate_id=candidate_id,
                    evaluation_id=evaluation_id,
                    flag_digest=canonical_digest(flag),
                    flag_object_digest=sealed_flag.digest,
                    flag_byte_count=sealed_flag.byte_count,
                    flag_encoding="utf-8",
                    snapshot_digest=snapshot_digest,
                ).canonical_payload(),
            ))
        result = self._store.commit_command(
            command_id=f"gate:{evaluation_id}",
            idempotency_key=f"gate:{evaluation_id}",
            command_payload={**snapshot, "accepted": accepted},
            events=[CommandEvent(
                event_id=f"event:gate:{evaluation_id}",
                kind="FLAG_ACCEPTED" if accepted else "FLAG_REJECTED",
                actor="hardcoded-gate", occurred_at_ns=occurred_at_ns,
                payload={
                    "accepted": accepted,
                    "attempt_digest": gate_input.attempt_digest,
                    "candidate_id": candidate_id,
                    "capture_event_digest": gate_input.capture_event_digest,
                    "evaluation_id": evaluation_id,
                    "flag_digest": canonical_digest(flag),
                    "flag_format_digest": gate_input.flag_format_digest,
                    "lease_digest": gate_input.lease_digest,
                    "manifest_digest": gate_input.manifest_digest,
                    "permit_digest": gate_input.permit_digest,
                    "policy_digest": policy_digest,
                    "raw_digest": gate_input.raw_digest,
                    "snapshot_digest": snapshot_digest,
                },
            )],
            outbox=outbox,
            projection_mutations=[ProjectionMutation(
                "attempt_io_guard",
                {
                    "action": "gate",
                    "attempt_digest": permit.lease.attempt.digest,
                    "attempt_id": permit.lease.attempt.attempt_id,
                    "candidate_id": candidate_id,
                    "capture_event_digest": gate_input.capture_event_digest,
                    "flag_digest": gate_input.flag_digest,
                    "flag_format_digest": gate_input.flag_format_digest,
                    "lease_digest": permit.lease.digest,
                    "lease_id": permit.lease.lease_id,
                    "manifest_digest": gate_input.manifest_digest,
                    "permit_digest": permit.digest,
                    "permit_id": permit.permit_id,
                    "policy_digest": policy_digest,
                    "raw_digest": gate_input.raw_digest,
                    "scope_digest": permit.lease.attempt.scope.digest,
                    "snapshot_digest": snapshot_digest,
                },
            )],
            authority_capability=self._store._gate_commit_capability,
            committed_at_ns=occurred_at_ns,
        )
        return GateDecision(
            accepted=accepted,
            flag=flag,
            snapshot_digest=snapshot_digest,
            receipt_digest=result.receipt_digest,
        )
