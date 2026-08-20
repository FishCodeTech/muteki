"""Canonical host observation for one executable runtime cognitive experiment.

This module closes the gap between an experiment-specific prompt and a replayable
structural outcome.  It resolves only store-owned assignment, launch, capture,
terminal, budget, receipt, and CAS data.  A matching worker-controlled marker is
still not independently verified, so every record remains non-learning until a
separate checker/reproducer authority resolves it.
"""

from __future__ import annotations

from collections.abc import Mapping, Sequence
from dataclasses import dataclass
from typing import Any

from muteki.epistemic.broker import (
    COGNITIVE_RUNTIME_OUTPUT_ACTOR,
    canonical_cognitive_observation_capture_id,
)
from muteki.epistemic.cas import ReceiptCAS
from muteki.epistemic.cognitive_events_v1 import (
    COGNITIVE_EXECUTION_OBSERVED,
    COGNITIVE_EXPERIMENT_ASSIGNED,
    COGNITIVE_RUNTIME_EXECUTION_SCHEMA_ID,
    COGNITIVE_RUNTIME_EXECUTABLE_ASSIGNMENT_SCHEMA_ID,
    COGNITIVE_RUNTIME_REPRODUCTION_ASSIGNMENT_SCHEMA_ID,
)
from muteki.epistemic.contracts import canonical_digest, canonical_json_bytes
from muteki.epistemic.sqlite_store import (
    CommandEvent,
    EpistemicSQLiteStore,
    IntegrityError,
    ProjectionMutation,
)
from muteki.runtime.cognition import DeliveredContextPacketV1
from muteki.runtime.cognitive_materialization_v1 import (
    CognitiveExperimentMaterializationV1,
    CognitiveMaterializationStatusV1,
)
from muteki.runtime.contracts import AttemptPermit
from muteki.runtime.executable_experiment_v1 import (
    CapturedObservationV1,
    ClassificationStatus,
    DeterministicExperimentClassificationV1,
    ExecutableExperimentBindingV1,
    ObservationSource,
    classify_executable_experiment,
)


COGNITIVE_RUNTIME_OBSERVER_ACTOR = "cognitive-runtime-observer-v1-authority"
COGNITIVE_RUNTIME_OBSERVATION_MODE = "runtime_context_shadow"


def _text(value: object, name: str) -> str:
    if type(value) is not str or not value or value != value.strip():
        raise ValueError(f"{name} must be exact non-empty text")
    return value


def _digest(value: object, name: str) -> str:
    result = _text(value, name)
    if len(result) != 64 or any(
        character not in "0123456789abcdef" for character in result
    ):
        raise ValueError(f"{name} must be a lowercase sha256 digest")
    return result


def canonical_observation_capture_id(
    *, permit_digest: str, spec_digest: str, observation_id: str
) -> str:
    """Derive the only capture id allowed to represent one declared observation."""

    return canonical_cognitive_observation_capture_id(
        permit_digest=permit_digest,
        spec_digest=spec_digest,
        observation_id=observation_id,
    )


_CAPTURE_BINDING_FIELDS = frozenset(
    {
        "byte_count",
        "capture_event_digest",
        "capture_event_id",
        "capture_id",
        "capture_receipt_digest",
        "manifest_digest",
        "manifest_event_digest",
        "manifest_event_id",
        "manifest_receipt_digest",
        "observation_id",
        "ordinal",
        "raw_digest",
        "source",
        "terminal",
    }
)

_RUNTIME_OBSERVATION_FIELDS = frozenset(
    {
        "accepted_set_change",
        "assignment_event_digest",
        "assignment_event_receipt_digest",
        "assignment_payload_digest",
        "attempt_digest",
        "attempt_id",
        "automatic_redispatch_permitted",
        "budget_event_digest",
        "budget_event_id",
        "budget_event_kind",
        "budget_event_receipt_digest",
        "budget_payload_digest",
        "capture_bindings",
        "capture_inventory_complete",
        "classification_body",
        "classification_digest",
        "classification_status",
        "context_packet_digest",
        "epistemic_classification",
        "executable_experiment_binding_digest",
        "executable_spec_digest",
        "experiment_digest",
        "host_launch_proof_body",
        "host_launch_proof_digest",
        "learning_eligible",
        "mode",
        "observed_partition_digest",
        "permit_digest",
        "permit_id",
        "schema_id",
        "scope_digest",
        "terminal_event_digest",
        "terminal_event_id",
        "terminal_event_kind",
        "terminal_event_receipt_digest",
        "terminal_outcome",
        "terminal_payload_digest",
        "undeclared_capture_event_digests",
        "usage_status",
        "verification_resolved",
        "verified_prefix_cutoff_seq",
        "verified_prefix_digest",
        "verified_prefix_head_event_digest",
        "world_epoch_digest",
    }
)


def validate_runtime_cognitive_observation_payload_shape(
    payload: Mapping[str, Any],
) -> None:
    """Validate the versioned data shape without granting semantic authority."""

    if not isinstance(payload, Mapping) or set(payload) != _RUNTIME_OBSERVATION_FIELDS:
        raise ValueError("runtime cognitive observation shape is not versioned")
    p = dict(payload)
    if (
        p["schema_id"] != COGNITIVE_RUNTIME_EXECUTION_SCHEMA_ID
        or p["mode"] != COGNITIVE_RUNTIME_OBSERVATION_MODE
        or p["accepted_set_change"] is not False
        or p["learning_eligible"] is not False
        or p["verification_resolved"] is not False
        or p["automatic_redispatch_permitted"] is not False
        or type(p["capture_inventory_complete"]) is not bool
    ):
        raise ValueError("runtime cognitive observation policy diverged")
    for name in (
        "assignment_event_digest",
        "assignment_event_receipt_digest",
        "assignment_payload_digest",
        "attempt_digest",
        "budget_event_digest",
        "budget_event_receipt_digest",
        "budget_payload_digest",
        "classification_digest",
        "context_packet_digest",
        "executable_experiment_binding_digest",
        "executable_spec_digest",
        "experiment_digest",
        "permit_digest",
        "scope_digest",
        "terminal_event_digest",
        "terminal_event_receipt_digest",
        "terminal_payload_digest",
        "verified_prefix_digest",
        "verified_prefix_head_event_digest",
        "world_epoch_digest",
    ):
        _digest(p[name], name)
    for name in (
        "attempt_id",
        "budget_event_id",
        "permit_id",
        "terminal_event_id",
        "terminal_outcome",
    ):
        _text(p[name], name)
    if p["budget_event_kind"] not in {"BUDGET_SETTLED", "BUDGET_USAGE_UNKNOWN"}:
        raise ValueError("runtime cognitive budget event kind is unsupported")
    if p["terminal_event_kind"] not in {"WORKER_TERMINAL", "WORKER_UNKNOWN"}:
        raise ValueError("runtime cognitive terminal event kind is unsupported")
    if p["usage_status"] not in {
        "complete",
        "pessimistically_accounted_partial",
        "unknown",
    }:
        raise ValueError("runtime cognitive usage status is unsupported")
    if p["classification_status"] not in {
        item.value for item in ClassificationStatus
    }:
        raise ValueError("runtime cognitive classification status is unsupported")
    if p["epistemic_classification"] not in {
        "structurally_observed_unverified",
        "inconclusive",
        "ambiguous",
        "execution_unknown",
    }:
        raise ValueError("runtime cognitive epistemic classification is unsupported")
    observed_partition = p["observed_partition_digest"]
    if observed_partition is not None:
        _digest(observed_partition, "observed_partition_digest")
    if (p["classification_status"] == ClassificationStatus.OBSERVED.value) != (
        observed_partition is not None
    ):
        raise ValueError("runtime cognitive partition/status diverged")
    cutoff = p["verified_prefix_cutoff_seq"]
    if type(cutoff) is not int or cutoff < 1:
        raise ValueError("runtime cognitive prefix cutoff must be positive")

    proof = p["host_launch_proof_body"]
    proof_digest = p["host_launch_proof_digest"]
    if proof is None:
        if proof_digest is not None:
            raise ValueError("absent host proof cannot carry a digest")
    elif not isinstance(proof, Mapping):
        raise TypeError("host launch proof body must be a mapping or None")
    else:
        _digest(proof_digest, "host_launch_proof_digest")
        if canonical_digest(proof) != proof_digest:
            raise ValueError("host launch proof digest is false")

    classification = p["classification_body"]
    if not isinstance(classification, Mapping):
        raise TypeError("classification body must be a mapping")
    if (
        canonical_digest(classification) != p["classification_digest"]
        or classification.get("status") != p["classification_status"]
        or classification.get("observed_partition_digest") != observed_partition
        or classification.get("learning_eligible") is not False
        or classification.get("accepted_set_change") is not False
        or classification.get("spec_digest") != p["executable_spec_digest"]
    ):
        raise ValueError("runtime cognitive classification body diverged")

    bindings = p["capture_bindings"]
    if type(bindings) not in {tuple, list}:
        raise TypeError("capture_bindings must be a canonical sequence")
    observation_ids: list[str] = []
    capture_event_digests: list[str] = []
    for item in bindings:
        if not isinstance(item, Mapping) or set(item) != _CAPTURE_BINDING_FIELDS:
            raise ValueError("runtime cognitive capture binding shape diverged")
        for name in (
            "capture_event_digest",
            "capture_receipt_digest",
            "manifest_digest",
            "manifest_event_digest",
            "manifest_receipt_digest",
            "raw_digest",
        ):
            _digest(item[name], f"capture_binding.{name}")
        for name in (
            "capture_event_id",
            "capture_id",
            "manifest_event_id",
            "observation_id",
        ):
            _text(item[name], f"capture_binding.{name}")
        if item["source"] not in {source.value for source in ObservationSource}:
            raise ValueError("runtime cognitive capture source is unsupported")
        if type(item["byte_count"]) is not int or item["byte_count"] < 0:
            raise ValueError("runtime cognitive capture byte count is malformed")
        if type(item["ordinal"]) is not int or item["ordinal"] < 0:
            raise ValueError("runtime cognitive capture ordinal is malformed")
        if type(item["terminal"]) is not bool:
            raise TypeError("runtime cognitive capture terminal must be boolean")
        observation_ids.append(item["observation_id"])
        capture_event_digests.append(item["capture_event_digest"])
    if observation_ids != sorted(set(observation_ids)):
        raise ValueError("runtime cognitive capture observations are not canonical")
    if len(capture_event_digests) != len(set(capture_event_digests)):
        raise ValueError("runtime cognitive capture events are duplicated")
    undeclared = p["undeclared_capture_event_digests"]
    if type(undeclared) not in {tuple, list}:
        raise TypeError("undeclared capture inventory must be a sequence")
    undeclared_tuple = tuple(
        _digest(item, "undeclared_capture_event_digest") for item in undeclared
    )
    if undeclared_tuple != tuple(sorted(set(undeclared_tuple))):
        raise ValueError("undeclared capture inventory is not canonical")


@dataclass(frozen=True, slots=True)
class RuntimeCognitiveObservationRecordV1:
    event_digest: str
    event_receipt_digest: str
    payload: Mapping[str, Any]

    def __post_init__(self) -> None:
        _digest(self.event_digest, "event_digest")
        _digest(self.event_receipt_digest, "event_receipt_digest")
        validate_runtime_cognitive_observation_payload_shape(self.payload)

    @property
    def classification_status(self) -> ClassificationStatus:
        return ClassificationStatus(self.payload["classification_status"])

    @property
    def observed_partition_digest(self) -> str | None:
        value = self.payload["observed_partition_digest"]
        return None if value is None else str(value)

    @property
    def learning_eligible(self) -> bool:
        return False


class CognitiveRuntimeObservationAuthorityV1:
    """Compare-and-append one structural observation from canonical runtime data."""

    def __init__(self, *, store: EpistemicSQLiteStore, cas: ReceiptCAS) -> None:
        if type(store) is not EpistemicSQLiteStore:
            raise TypeError("store must be exactly EpistemicSQLiteStore")
        if not isinstance(cas, ReceiptCAS):
            raise TypeError("cas must be ReceiptCAS")
        self._store = store
        self._cas = cas

    @staticmethod
    def _proof_body(proof: object) -> dict[str, Any]:
        fields = getattr(proof, "__dataclass_fields__", None)
        if not isinstance(fields, Mapping):
            raise TypeError("host launch proof is not a canonical dataclass")
        return {name: getattr(proof, name) for name in fields}

    @staticmethod
    def _forced_inconclusive(
        *,
        binding: ExecutableExperimentBindingV1,
        captures: tuple[CapturedObservationV1, ...],
        reasons: Sequence[str],
    ) -> DeterministicExperimentClassificationV1:
        spec = binding.spec
        return DeterministicExperimentClassificationV1(
            spec_digest=spec.digest,
            status=ClassificationStatus.INCONCLUSIVE,
            observed_partition_digest=None,
            prospective_partition_digests=tuple(
                sorted({item.outcome_partition_digest for item in spec.predicates})
            ),
            prospective_predicate_digests=tuple(
                sorted({item.predicate_digest for item in spec.predicates})
            ),
            matched_predicate_digests=(),
            observation_bindings=tuple(
                item.canonical_body()
                for item in sorted(captures, key=lambda row: row.observation_id)
            ),
            reason_codes=tuple(dict.fromkeys(reasons)),
        )

    @staticmethod
    def _usage_status(budget_kind: str, budget_payload: Mapping[str, Any]) -> str:
        if budget_kind == "BUDGET_USAGE_UNKNOWN":
            return "unknown"
        report = budget_payload.get("usage_report")
        measurements = report.get("measurements") if isinstance(report, Mapping) else None
        if type(measurements) not in {tuple, list} or not measurements:
            raise IntegrityError("budget event has no tagged usage measurements")
        if any(not isinstance(item, Mapping) for item in measurements):
            raise IntegrityError("budget usage statuses are malformed")
        statuses = {item.get("status") for item in measurements}
        if statuses == {"observed"}:
            return "complete"
        if "unknown" in statuses:
            raise IntegrityError("settled budget cannot contain UNKNOWN usage")
        return "pessimistically_accounted_partial"

    def _rows_for_permit(
        self, *, kind: str, permit: AttemptPermit
    ) -> tuple[dict[str, Any], ...]:
        return tuple(
            row
            for row in self._store.event_rows(kind=kind)
            if row["payload"].get("permit_digest") == permit.digest
        )

    def _capture_inventory(
        self,
        *,
        permit: AttemptPermit,
        binding: ExecutableExperimentBindingV1,
    ) -> tuple[
        tuple[CapturedObservationV1, ...],
        tuple[dict[str, Any], ...],
        tuple[str, ...],
        bool,
    ]:
        rows = self._rows_for_permit(kind="CAPTURE_CHUNK_SEALED", permit=permit)
        manifests = self._rows_for_permit(
            kind="CAPTURE_MANIFEST_ADVANCED", permit=permit
        )
        manifest_by_digest: dict[str, list[dict[str, Any]]] = {}
        for row in manifests:
            manifest_by_digest.setdefault(row["payload"].get("manifest_digest"), []).append(row)
        expected_by_id = {
            canonical_observation_capture_id(
                permit_digest=permit.digest,
                spec_digest=binding.spec.digest,
                observation_id=observation.observation_id,
            ): observation
            for observation in binding.spec.observations
        }
        declared_rows = [
            row
            for row in rows
            if row["payload"].get("capture_id") in expected_by_id
            and self._store.actor_for_event(row["event_digest"])
            == COGNITIVE_RUNTIME_OUTPUT_ACTOR
        ]
        undeclared = tuple(
            sorted(
                row["event_digest"]
                for row in rows
                if row["payload"].get("capture_id") not in expected_by_id
                or self._store.actor_for_event(row["event_digest"])
                != COGNITIVE_RUNTIME_OUTPUT_ACTOR
            )
        )
        captures: list[CapturedObservationV1] = []
        capture_bindings: list[dict[str, Any]] = []
        structurally_complete = not undeclared and len(declared_rows) == len(
            expected_by_id
        )
        seen_ids: set[str] = set()
        ordinals: list[int] = []
        for row in declared_rows:
            payload = row["payload"]
            capture_id = payload.get("capture_id")
            expected = expected_by_id[capture_id]
            if capture_id in seen_ids:
                raise IntegrityError("declared cognitive capture id is duplicated")
            seen_ids.add(capture_id)
            if (
                payload.get("stream") != expected.source.value
                or payload.get("attempt_digest") != permit.lease.attempt.digest
                or payload.get("lease_digest") != permit.lease.digest
                or payload.get("permit_digest") != permit.digest
                or type(payload.get("ordinal")) is not int
            ):
                raise IntegrityError("declared cognitive capture lineage diverged")
            paired = manifest_by_digest.get(payload.get("manifest_digest"), [])
            if (
                len(paired) != 1
                or paired[0]["payload"] != payload
                or paired[0]["seq"] != row["seq"] + 1
                or self._store.actor_for_event(paired[0]["event_digest"])
                != COGNITIVE_RUNTIME_OUTPUT_ACTOR
            ):
                raise IntegrityError("declared cognitive capture manifest is ambiguous")
            raw = self._cas.read_verified(payload["raw_digest"])
            if len(raw) != payload.get("byte_count"):
                raise IntegrityError("declared cognitive capture byte count diverged")
            capture_receipt = self._store.resolve_receipt_for_event(
                row["event_digest"]
            )
            manifest_receipt = self._store.resolve_receipt_for_event(
                paired[0]["event_digest"]
            )
            capture = CapturedObservationV1(
                observation_id=expected.observation_id,
                source=expected.source,
                raw=raw,
                capture_event_digest=row["event_digest"],
                manifest_digest=payload["manifest_digest"],
            )
            captures.append(capture)
            ordinals.append(payload["ordinal"])
            capture_bindings.append(
                {
                    "byte_count": payload["byte_count"],
                    "capture_event_digest": row["event_digest"],
                    "capture_event_id": row["event_id"],
                    "capture_id": capture_id,
                    "capture_receipt_digest": capture_receipt.digest,
                    "manifest_digest": payload["manifest_digest"],
                    "manifest_event_digest": paired[0]["event_digest"],
                    "manifest_event_id": paired[0]["event_id"],
                    "manifest_receipt_digest": manifest_receipt.digest,
                    "observation_id": expected.observation_id,
                    "ordinal": payload["ordinal"],
                    "raw_digest": payload["raw_digest"],
                    "source": expected.source.value,
                    "terminal": payload["terminal"],
                }
            )
        if declared_rows:
            ordered_rows = sorted(declared_rows, key=lambda item: item["payload"]["ordinal"])
            structurally_complete = structurally_complete and (
                tuple(ordinals) == tuple(sorted(ordinals))
                and tuple(row["payload"]["ordinal"] for row in ordered_rows)
                == tuple(range(len(rows)))
                and all(not row["payload"]["terminal"] for row in ordered_rows[:-1])
                and ordered_rows[-1]["payload"]["terminal"] is True
            )
        else:
            structurally_complete = False
        return (
            tuple(sorted(captures, key=lambda item: item.observation_id)),
            tuple(sorted(capture_bindings, key=lambda item: item["observation_id"])),
            undeclared,
            structurally_complete,
        )

    def _existing(
        self, *, delivered: DeliveredContextPacketV1, permit: AttemptPermit
    ) -> RuntimeCognitiveObservationRecordV1 | None:
        rows = self._rows_for_permit(kind=COGNITIVE_EXECUTION_OBSERVED, permit=permit)
        if not rows:
            return None
        if len(rows) != 1:
            raise IntegrityError("runtime cognitive observation is ambiguous")
        row = rows[0]
        payload = row["payload"]
        validate_runtime_cognitive_observation_payload_shape(payload)
        if (
            payload["attempt_digest"] != permit.lease.attempt.digest
            or payload["permit_digest"] != permit.digest
            or payload["scope_digest"] != permit.lease.attempt.scope.digest
            or payload["context_packet_digest"] != delivered.binding.packet_digest
        ):
            raise IntegrityError("runtime cognitive observation was rebound")
        assignment_rows = tuple(
            item
            for item in self._store.event_rows(kind=COGNITIVE_EXPERIMENT_ASSIGNED)
            if item["event_digest"] == payload["assignment_event_digest"]
        )
        if len(assignment_rows) != 1:
            raise IntegrityError("runtime cognitive replay assignment is absent")
        try:
            executable = ExecutableExperimentBindingV1.from_canonical(
                assignment_rows[0]["payload"][
                    "executable_experiment_binding_body"
                ]
            )
            executable.resolve(self._cas)
        except (KeyError, TypeError, ValueError) as exc:
            raise IntegrityError(
                "runtime cognitive replay executable binding failed"
            ) from exc
        replayed_captures: list[CapturedObservationV1] = []
        observation_by_id = {
            item.observation_id: item for item in executable.spec.observations
        }
        for binding in payload["capture_bindings"]:
            observation = observation_by_id.get(binding["observation_id"])
            if observation is None:
                raise IntegrityError(
                    "runtime cognitive replay capture observation is unknown"
                )
            raw = self._cas.read_verified(binding["raw_digest"])
            if len(raw) != binding["byte_count"]:
                raise IntegrityError(
                    "runtime cognitive replay capture byte count diverged"
                )
            replayed_captures.append(
                CapturedObservationV1(
                    observation_id=observation.observation_id,
                    source=observation.source,
                    raw=raw,
                    capture_event_digest=binding["capture_event_digest"],
                    manifest_digest=binding["manifest_digest"],
                )
            )
        positive_runtime = (
            payload["host_launch_proof_body"] is not None
            and payload["terminal_event_kind"] == "WORKER_TERMINAL"
            and payload["budget_event_kind"] == "BUDGET_SETTLED"
            and payload["capture_inventory_complete"] is True
        )
        if positive_runtime:
            replayed_classification = classify_executable_experiment(
                executable.spec,
                tuple(
                    sorted(
                        replayed_captures,
                        key=lambda item: item.observation_id,
                    )
                ),
            )
            if canonical_json_bytes(replayed_classification.canonical_body()) != (
                canonical_json_bytes(payload["classification_body"])
            ):
                raise IntegrityError(
                    "runtime cognitive replay classification diverged"
                )
        prefix = self._store.receipt_field_resolver(
            cutoff_seq=payload["verified_prefix_cutoff_seq"]
        ).verify_complete_through(payload["verified_prefix_cutoff_seq"])
        if (
            prefix.digest != payload["verified_prefix_digest"]
            or prefix.head_event_digest
            != payload["verified_prefix_head_event_digest"]
        ):
            raise IntegrityError("runtime cognitive replay prefix diverged")
        return RuntimeCognitiveObservationRecordV1(
            event_digest=row["event_digest"],
            event_receipt_digest=self._store.resolve_receipt_for_event(
                row["event_digest"]
            ).digest,
            payload=payload,
        )

    def observe(
        self,
        *,
        delivered: DeliveredContextPacketV1,
        permit: AttemptPermit,
        occurred_at_ns: int,
    ) -> RuntimeCognitiveObservationRecordV1:
        if type(delivered) is not DeliveredContextPacketV1:
            raise TypeError("delivered must be DeliveredContextPacketV1")
        if type(permit) is not AttemptPermit:
            raise TypeError("permit must be AttemptPermit")
        if type(occurred_at_ns) is not int or occurred_at_ns < 0:
            raise ValueError("occurred_at_ns must be a non-negative integer")
        existing = self._existing(delivered=delivered, permit=permit)
        if existing is not None:
            return existing

        materializer = CognitiveExperimentMaterializationV1(
            store=self._store,
            cas=self._cas,
        )
        assigned = materializer.resolve_assigned(
            delivered=delivered,
            permit=permit,
        )
        binding = assigned.executable_experiment
        if type(binding) is not ExecutableExperimentBindingV1:
            raise IntegrityError("runtime cognitive observation requires executable assignment")
        binding.resolve(self._cas)

        assignment_rows = self._rows_for_permit(
            kind=COGNITIVE_EXPERIMENT_ASSIGNED,
            permit=permit,
        )
        terminal_rows = tuple(
            row
            for kind in ("WORKER_TERMINAL", "WORKER_UNKNOWN")
            for row in self._rows_for_permit(kind=kind, permit=permit)
        )
        budget_rows = tuple(
            row
            for kind in ("BUDGET_SETTLED", "BUDGET_USAGE_UNKNOWN")
            for row in self._store.event_rows(kind=kind)
            if row["payload"].get("attempt_id") == permit.lease.attempt.attempt_id
        )
        if len(assignment_rows) != 1 or len(terminal_rows) != 1 or len(budget_rows) != 1:
            raise IntegrityError(
                "runtime cognitive observation requires unique assignment, terminal, and budget"
            )
        assignment_row = assignment_rows[0]
        if assignment_row["payload"].get("schema_id") not in {
            COGNITIVE_RUNTIME_EXECUTABLE_ASSIGNMENT_SCHEMA_ID,
            COGNITIVE_RUNTIME_REPRODUCTION_ASSIGNMENT_SCHEMA_ID,
        }:
            raise IntegrityError("runtime cognitive observation assignment schema diverged")
        terminal_row = terminal_rows[0]
        budget_row = budget_rows[0]
        if not (terminal_row["seq"] < budget_row["seq"]):
            raise IntegrityError("runtime cognitive terminal must precede budget closure")

        captures, capture_bindings, undeclared, capture_complete = (
            self._capture_inventory(permit=permit, binding=binding)
        )
        materialization = materializer.resolve_host_launch_only(
            delivered=delivered,
            permit=permit,
        )
        proof_body = (
            self._proof_body(materialization.proof)
            if materialization.proof is not None
            else None
        )
        proof_digest = None if proof_body is None else canonical_digest(proof_body)
        terminal_kind = terminal_row["kind"]
        budget_kind = budget_row["kind"]
        usage_status = self._usage_status(budget_kind, budget_row["payload"])
        positive_runtime = (
            materialization.status is CognitiveMaterializationStatusV1.HOST_LAUNCH_ONLY
            and terminal_kind == "WORKER_TERMINAL"
            and terminal_row["payload"].get("outcome") != "unknown"
            and budget_kind == "BUDGET_SETTLED"
            and capture_complete
        )
        if positive_runtime:
            classification = classify_executable_experiment(binding.spec, captures)
        else:
            reasons = []
            if materialization.status is not CognitiveMaterializationStatusV1.HOST_LAUNCH_ONLY:
                reasons.append(f"materialization:{materialization.status.value}")
            if terminal_kind == "WORKER_UNKNOWN":
                reasons.append("worker_terminal_unknown")
            if budget_kind == "BUDGET_USAGE_UNKNOWN":
                reasons.append("budget_usage_unknown")
            if not capture_complete:
                reasons.append("capture_inventory_incomplete")
            classification = self._forced_inconclusive(
                binding=binding,
                captures=captures,
                reasons=reasons or ("runtime_observation_inconclusive",),
            )
        if terminal_kind == "WORKER_UNKNOWN" or budget_kind == "BUDGET_USAGE_UNKNOWN":
            epistemic_classification = "execution_unknown"
        elif classification.status is ClassificationStatus.OBSERVED:
            epistemic_classification = "structurally_observed_unverified"
        elif classification.status is ClassificationStatus.AMBIGUOUS:
            epistemic_classification = "ambiguous"
        else:
            epistemic_classification = "inconclusive"

        assignment_receipt = self._store.resolve_receipt_for_event(
            assignment_row["event_digest"]
        )
        terminal_receipt = self._store.resolve_receipt_for_event(
            terminal_row["event_digest"]
        )
        budget_receipt = self._store.resolve_receipt_for_event(
            budget_row["event_digest"]
        )
        cutoff = self._store.state().head_seq
        prefix = self._store.receipt_field_resolver(
            cutoff_seq=cutoff
        ).verify_complete_through(cutoff)
        assignment_payload = assignment_row["payload"]
        payload = {
            "accepted_set_change": False,
            "assignment_event_digest": assignment_row["event_digest"],
            "assignment_event_receipt_digest": assignment_receipt.digest,
            "assignment_payload_digest": canonical_digest(assignment_payload),
            "attempt_digest": permit.lease.attempt.digest,
            "attempt_id": permit.lease.attempt.attempt_id,
            "automatic_redispatch_permitted": False,
            "budget_event_digest": budget_row["event_digest"],
            "budget_event_id": budget_row["event_id"],
            "budget_event_kind": budget_kind,
            "budget_event_receipt_digest": budget_receipt.digest,
            "budget_payload_digest": canonical_digest(budget_row["payload"]),
            "capture_bindings": capture_bindings,
            "capture_inventory_complete": capture_complete,
            "classification_body": classification.canonical_body(),
            "classification_digest": classification.digest,
            "classification_status": classification.status.value,
            "context_packet_digest": delivered.binding.packet_digest,
            "epistemic_classification": epistemic_classification,
            "executable_experiment_binding_digest": binding.digest,
            "executable_spec_digest": binding.spec.digest,
            "experiment_digest": assigned.experiment_digest,
            "host_launch_proof_body": proof_body,
            "host_launch_proof_digest": proof_digest,
            "learning_eligible": False,
            "mode": COGNITIVE_RUNTIME_OBSERVATION_MODE,
            "observed_partition_digest": classification.observed_partition_digest,
            "permit_digest": permit.digest,
            "permit_id": permit.permit_id,
            "schema_id": COGNITIVE_RUNTIME_EXECUTION_SCHEMA_ID,
            "scope_digest": permit.lease.attempt.scope.digest,
            "terminal_event_digest": terminal_row["event_digest"],
            "terminal_event_id": terminal_row["event_id"],
            "terminal_event_kind": terminal_kind,
            "terminal_event_receipt_digest": terminal_receipt.digest,
            "terminal_outcome": terminal_row["payload"]["outcome"],
            "terminal_payload_digest": canonical_digest(terminal_row["payload"]),
            "undeclared_capture_event_digests": undeclared,
            "usage_status": usage_status,
            "verification_resolved": False,
            "verified_prefix_cutoff_seq": prefix.cutoff_seq,
            "verified_prefix_digest": prefix.digest,
            "verified_prefix_head_event_digest": prefix.head_event_digest,
            "world_epoch_digest": assignment_payload["world_epoch_digest"],
        }
        validate_runtime_cognitive_observation_payload_shape(payload)
        result = self._store.commit_command(
            command_id=f"cognitive:runtime-observe:{permit.permit_id}",
            idempotency_key=f"cognitive:runtime-observe:{permit.permit_id}",
            command_payload=payload,
            events=(
                CommandEvent(
                    event_id=(
                        f"event:{COGNITIVE_EXECUTION_OBSERVED}:{permit.permit_id}"
                    ),
                    kind=COGNITIVE_EXECUTION_OBSERVED,
                    actor=COGNITIVE_RUNTIME_OBSERVER_ACTOR,
                    occurred_at_ns=occurred_at_ns,
                    payload=payload,
                ),
            ),
            projection_mutations=(
                ProjectionMutation(
                    "cognitive_runtime_execution_observe_guard",
                    payload,
                ),
            ),
            authority_capability=(
                self._store._cognitive_runtime_observation_commit_capability
            ),
            committed_at_ns=occurred_at_ns,
        )
        row = next(
            row
            for row in self._store.event_rows(kind=COGNITIVE_EXECUTION_OBSERVED)
            if row["event_id"]
            == f"event:{COGNITIVE_EXECUTION_OBSERVED}:{permit.permit_id}"
        )
        if canonical_json_bytes(row["payload"]) != canonical_json_bytes(payload):
            raise IntegrityError("runtime cognitive observation commit diverged")
        return RuntimeCognitiveObservationRecordV1(
            event_digest=row["event_digest"],
            event_receipt_digest=result.receipt_digest,
            payload=row["payload"],
        )


__all__ = [
    "COGNITIVE_RUNTIME_OBSERVATION_MODE",
    "COGNITIVE_RUNTIME_OBSERVER_ACTOR",
    "CognitiveRuntimeObservationAuthorityV1",
    "RuntimeCognitiveObservationRecordV1",
    "canonical_observation_capture_id",
    "validate_runtime_cognitive_observation_payload_shape",
]
