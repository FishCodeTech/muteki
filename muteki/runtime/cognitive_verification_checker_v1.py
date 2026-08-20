"""Pure deterministic checker for one preregistered cognitive reproduction.

The checker deliberately owns no store, admission, budget, dispatch, resolver, or
gate capability.  It accepts already-canonical assignment/observation payloads,
reopens the sealed executable specifications and raw capture bytes from the CAS,
and independently reruns the prospective classifier.  Its result is data for a
later, separately accounted verification authority; it is not canonical truth.

Event existence, receipt lineage, and checker-budget settlement cannot be proven
from payloads alone.  A resolver must bind this result back to those canonical
records before any learning transition is considered.
"""

from __future__ import annotations

from collections.abc import Mapping
from dataclasses import dataclass
from enum import Enum
from typing import Any, TypeVar

from muteki.epistemic.cas import ReceiptCAS
from muteki.epistemic.cognitive_events_v1 import (
    COGNITIVE_RUNTIME_EXECUTABLE_ASSIGNMENT_SCHEMA_ID,
    COGNITIVE_RUNTIME_REPRODUCTION_ASSIGNMENT_SCHEMA_ID,
    validate_runtime_context_executable_assignment_payload_shape,
    validate_runtime_reproduction_assignment_payload_shape,
)
from muteki.epistemic.contracts import (
    FrozenJSON,
    canonical_digest,
    canonical_json_bytes,
    freeze_json,
)
from muteki.runtime.cognitive_runtime_observation_v1 import (
    validate_runtime_cognitive_observation_payload_shape,
)
from muteki.runtime.executable_experiment_v1 import (
    CapturedObservationV1,
    ClassificationStatus,
    DeterministicExperimentClassificationV1,
    ExecutableExperimentBindingV1,
    ObservationSource,
    classify_executable_experiment,
)


COGNITIVE_VERIFICATION_CHECK_SCHEMA_ID = (
    "muteki.cognitive-verification-deterministic-check.v1"
)
COGNITIVE_VERIFICATION_CHECKER_VERSION = (
    "muteki.cognitive-verification-checker.v1"
)
PRODUCTION_ENABLED = False
ACCEPTED_SET_CHANGE = False


class CognitiveVerificationRelationV1(str, Enum):
    SUPPORTED = "SUPPORTED"
    DISAGREEMENT = "DISAGREEMENT"
    UNKNOWN = "UNKNOWN"
    INVALID_SOURCE = "INVALID_SOURCE"


def _digest(value: object, name: str) -> str:
    if (
        type(value) is not str
        or len(value) != 64
        or any(character not in "0123456789abcdef" for character in value)
    ):
        raise ValueError(f"{name} must be a lowercase sha256 digest")
    return value


@dataclass(frozen=True, slots=True)
class DeterministicCognitiveVerificationCheckV1:
    """Replayable, non-authoritative relation between two executions."""

    relation: CognitiveVerificationRelationV1
    source_assignment_payload_digest: str
    source_observation_payload_digest: str
    reproduction_assignment_payload_digest: str
    reproduction_observation_payload_digest: str
    source_reproduction_kernel_digest: str | None
    reproduction_reproduction_kernel_digest: str | None
    source_classification_body: FrozenJSON | None
    reproduction_classification_body: FrozenJSON | None
    reason_codes: tuple[str, ...]
    schema_id: str = COGNITIVE_VERIFICATION_CHECK_SCHEMA_ID
    checker_version: str = COGNITIVE_VERIFICATION_CHECKER_VERSION
    learning_eligible: bool = False
    accepted_set_change: bool = ACCEPTED_SET_CHANGE

    def __post_init__(self) -> None:
        if type(self.relation) is not CognitiveVerificationRelationV1:
            raise TypeError("relation must be CognitiveVerificationRelationV1")
        for name in (
            "source_assignment_payload_digest",
            "source_observation_payload_digest",
            "reproduction_assignment_payload_digest",
            "reproduction_observation_payload_digest",
        ):
            object.__setattr__(self, name, _digest(getattr(self, name), name))
        for name in (
            "source_reproduction_kernel_digest",
            "reproduction_reproduction_kernel_digest",
        ):
            value = getattr(self, name)
            if value is not None:
                object.__setattr__(self, name, _digest(value, name))
        for name in (
            "source_classification_body",
            "reproduction_classification_body",
        ):
            value = getattr(self, name)
            if value is not None:
                frozen = freeze_json(value, path=f"$.{name}")
                if not isinstance(frozen, Mapping):
                    raise TypeError(f"{name} must be a mapping or None")
                object.__setattr__(self, name, frozen)
        if (
            type(self.reason_codes) is not tuple
            or not self.reason_codes
            or any(
                type(item) is not str or not item or item != item.strip()
                for item in self.reason_codes
            )
            or self.reason_codes != tuple(sorted(set(self.reason_codes)))
        ):
            raise ValueError("reason_codes must be a canonical non-empty tuple")
        if (
            self.schema_id != COGNITIVE_VERIFICATION_CHECK_SCHEMA_ID
            or self.checker_version != COGNITIVE_VERIFICATION_CHECKER_VERSION
            or self.learning_eligible is not False
            or self.accepted_set_change is not False
        ):
            raise ValueError("deterministic check overclaims authority")

    @property
    def source_partition_digest(self) -> str | None:
        body = self.source_classification_body
        if not isinstance(body, Mapping):
            return None
        value = body.get("observed_partition_digest")
        return value if type(value) is str else None

    @property
    def reproduction_partition_digest(self) -> str | None:
        body = self.reproduction_classification_body
        if not isinstance(body, Mapping):
            return None
        value = body.get("observed_partition_digest")
        return value if type(value) is str else None

    def canonical_body(self) -> dict[str, Any]:
        return {
            "accepted_set_change": self.accepted_set_change,
            "checker_version": self.checker_version,
            "learning_eligible": self.learning_eligible,
            "reason_codes": self.reason_codes,
            "relation": self.relation.value,
            "reproduction_assignment_payload_digest": (
                self.reproduction_assignment_payload_digest
            ),
            "reproduction_classification_body": (
                self.reproduction_classification_body
            ),
            "reproduction_observation_payload_digest": (
                self.reproduction_observation_payload_digest
            ),
            "reproduction_reproduction_kernel_digest": (
                self.reproduction_reproduction_kernel_digest
            ),
            "schema_id": self.schema_id,
            "source_assignment_payload_digest": (
                self.source_assignment_payload_digest
            ),
            "source_classification_body": self.source_classification_body,
            "source_observation_payload_digest": (
                self.source_observation_payload_digest
            ),
            "source_reproduction_kernel_digest": (
                self.source_reproduction_kernel_digest
            ),
        }

    @property
    def digest(self) -> str:
        return canonical_digest(self.canonical_body())

    @classmethod
    def from_canonical(
        cls, value: Mapping[str, Any]
    ) -> "DeterministicCognitiveVerificationCheckV1":
        """Strictly reconstruct one checker output from captured canonical bytes."""

        if not isinstance(value, Mapping) or set(value) != {
            "accepted_set_change",
            "checker_version",
            "learning_eligible",
            "reason_codes",
            "relation",
            "reproduction_assignment_payload_digest",
            "reproduction_classification_body",
            "reproduction_observation_payload_digest",
            "reproduction_reproduction_kernel_digest",
            "schema_id",
            "source_assignment_payload_digest",
            "source_classification_body",
            "source_observation_payload_digest",
            "source_reproduction_kernel_digest",
        }:
            raise ValueError("deterministic checker body is not versioned")
        result = cls(
            relation=CognitiveVerificationRelationV1(value["relation"]),
            source_assignment_payload_digest=value[
                "source_assignment_payload_digest"
            ],
            source_observation_payload_digest=value[
                "source_observation_payload_digest"
            ],
            reproduction_assignment_payload_digest=value[
                "reproduction_assignment_payload_digest"
            ],
            reproduction_observation_payload_digest=value[
                "reproduction_observation_payload_digest"
            ],
            source_reproduction_kernel_digest=value[
                "source_reproduction_kernel_digest"
            ],
            reproduction_reproduction_kernel_digest=value[
                "reproduction_reproduction_kernel_digest"
            ],
            source_classification_body=value["source_classification_body"],
            reproduction_classification_body=value[
                "reproduction_classification_body"
            ],
            reason_codes=tuple(value["reason_codes"]),
            schema_id=value["schema_id"],
            checker_version=value["checker_version"],
            learning_eligible=value["learning_eligible"],
            accepted_set_change=value["accepted_set_change"],
        )
        if canonical_json_bytes(result.canonical_body()) != canonical_json_bytes(value):
            raise ValueError("deterministic checker body is not canonical")
        return result


class _InvalidSource(ValueError):
    def __init__(self, reason_code: str) -> None:
        super().__init__(reason_code)
        self.reason_code = reason_code


_T = TypeVar("_T")


def _stage(reason_code: str, operation: Any) -> _T:
    try:
        return operation()
    except _InvalidSource:
        raise
    except Exception as exc:
        raise _InvalidSource(reason_code) from exc


def _require(condition: bool, reason_code: str) -> None:
    if not condition:
        raise _InvalidSource(reason_code)


def _validate_assignment_observation_lineage(
    assignment: Mapping[str, Any],
    observation: Mapping[str, Any],
    binding: ExecutableExperimentBindingV1,
) -> None:
    expected = {
        "assignment_payload_digest": canonical_digest(assignment),
        "attempt_digest": assignment["attempt_digest"],
        "attempt_id": assignment["attempt_id"],
        "context_packet_digest": assignment["context_packet_binding_body"][
            "packet_digest"
        ],
        "executable_experiment_binding_digest": binding.digest,
        "executable_spec_digest": binding.spec.digest,
        "experiment_digest": assignment["experiment_digest"],
        "permit_digest": assignment["permit_digest"],
        "permit_id": assignment["permit_id"],
        "scope_digest": assignment["scope_digest"],
        "world_epoch_digest": assignment["world_epoch_digest"],
    }
    _require(
        all(observation.get(name) == value for name, value in expected.items()),
        "assignment_observation_lineage_invalid",
    )


def _validate_reproduction_source_lineage(
    source_assignment: Mapping[str, Any],
    source_observation: Mapping[str, Any],
    reproduction_assignment: Mapping[str, Any],
    source_binding: ExecutableExperimentBindingV1,
    reproduction_binding: ExecutableExperimentBindingV1,
) -> None:
    source_experiment = source_assignment["experiment_body"]
    reproduction_experiment = reproduction_assignment["experiment_body"]
    source_predictions = {
        item["hypothesis_digest"]: item["outcome_partition_digest"]
        for item in source_experiment["predictions"]
    }
    reproduction_predictions = {
        item["hypothesis_digest"]: item["outcome_partition_digest"]
        for item in reproduction_experiment["predictions"]
    }
    source_claim_digest = canonical_digest(
        {
            "experiment_digest": source_assignment["experiment_digest"],
            "observed_partition_digest": source_observation[
                "observed_partition_digest"
            ],
            "schema_id": "muteki.cognitive-observation-binding.v1",
        }
    )
    withheld = set(reproduction_assignment["withheld_source_digest_set"])
    required_withheld = {
        source_observation["classification_digest"],
        *(
            item["raw_digest"]
            for item in source_observation["capture_bindings"]
        ),
    }
    _require(
        source_assignment["attempt_id"] != reproduction_assignment["attempt_id"]
        and source_assignment["experiment_digest"]
        != reproduction_assignment["experiment_digest"]
        and source_assignment["scope_digest"]
        == reproduction_assignment["scope_digest"]
        and source_assignment["world_epoch_digest"]
        == reproduction_assignment["world_epoch_digest"]
        and source_experiment["hypothesis_digests"]
        == reproduction_experiment["hypothesis_digests"]
        and source_predictions == reproduction_predictions
        and reproduction_assignment["source_assignment_event_digest"]
        == source_observation["assignment_event_digest"]
        and reproduction_assignment["source_assignment_event_receipt_digest"]
        == source_observation["assignment_event_receipt_digest"]
        and reproduction_assignment["source_claim_digest"] == source_claim_digest
        and reproduction_assignment["source_executable_spec_digest"]
        == source_binding.spec.digest
        and reproduction_assignment["source_reproduction_kernel_digest"]
        == source_binding.spec.reproduction_kernel_digest
        and reproduction_assignment["reproduction_kernel_digest"]
        == reproduction_binding.spec.reproduction_kernel_digest
        and source_binding.spec.reproduction_kernel_digest
        == reproduction_binding.spec.reproduction_kernel_digest
        and required_withheld.issubset(withheld),
        "reproduction_source_lineage_invalid",
    )


def _captures_from_cas(
    *,
    observation: Mapping[str, Any],
    binding: ExecutableExperimentBindingV1,
    cas: ReceiptCAS,
) -> tuple[CapturedObservationV1, ...]:
    expected = {
        item.observation_id: item for item in binding.spec.observations
    }
    captures: list[CapturedObservationV1] = []
    for item in observation["capture_bindings"]:
        declared = expected.get(item["observation_id"])
        _require(declared is not None, "capture_observation_unknown")
        assert declared is not None
        _require(
            item["source"] == declared.source.value,
            "capture_source_invalid",
        )
        raw = cas.read_verified(item["raw_digest"])
        _require(len(raw) == item["byte_count"], "capture_byte_count_invalid")
        captures.append(
            CapturedObservationV1(
                observation_id=declared.observation_id,
                source=ObservationSource(item["source"]),
                raw=raw,
                capture_event_digest=item["capture_event_digest"],
                manifest_digest=item["manifest_digest"],
            )
        )
    return tuple(sorted(captures, key=lambda item: item.observation_id))


_MATERIALIZATION_REASON_CODES = frozenset(
    {
        "materialization:incomplete",
        "materialization:not_assigned",
        "materialization:not_staged",
        "materialization:prelaunch_aborted",
        "materialization:unknown",
    }
)


def _validate_forced_inconclusive(
    *,
    observation: Mapping[str, Any],
    binding: ExecutableExperimentBindingV1,
    captures: tuple[CapturedObservationV1, ...],
) -> DeterministicExperimentClassificationV1:
    """Validate the structural answer when runtime state forced UNKNOWN.

    The payload omits the precise materialization status, so a pure checker cannot
    recreate that one diagnostic label.  It can still independently prove that no
    partition was accepted and that every predicate/capture binding is exact.
    """

    stored = observation["classification_body"]
    reasons = tuple(stored["reason_codes"])
    expected_reasons: list[str] = []
    if observation["host_launch_proof_body"] is None:
        materialization = tuple(
            item for item in reasons if item in _MATERIALIZATION_REASON_CODES
        )
        _require(
            len(materialization) == 1,
            "stored_classification_mismatch",
        )
        expected_reasons.append(materialization[0])
    if observation["terminal_event_kind"] == "WORKER_UNKNOWN":
        expected_reasons.append("worker_terminal_unknown")
    if observation["budget_event_kind"] == "BUDGET_USAGE_UNKNOWN":
        expected_reasons.append("budget_usage_unknown")
    if observation["capture_inventory_complete"] is False:
        expected_reasons.append("capture_inventory_incomplete")
    if not expected_reasons:
        expected_reasons.append("runtime_observation_inconclusive")
    expected = DeterministicExperimentClassificationV1(
        spec_digest=binding.spec.digest,
        status=ClassificationStatus.INCONCLUSIVE,
        observed_partition_digest=None,
        prospective_partition_digests=tuple(
            sorted(
                {
                    item.outcome_partition_digest
                    for item in binding.spec.predicates
                }
            )
        ),
        prospective_predicate_digests=tuple(
            sorted({item.predicate_digest for item in binding.spec.predicates})
        ),
        matched_predicate_digests=(),
        observation_bindings=tuple(item.canonical_body() for item in captures),
        reason_codes=tuple(expected_reasons),
    )
    _require(
        canonical_json_bytes(expected.canonical_body())
        == canonical_json_bytes(stored),
        "stored_classification_mismatch",
    )
    return expected


def _independent_classification(
    *,
    observation: Mapping[str, Any],
    binding: ExecutableExperimentBindingV1,
    cas: ReceiptCAS,
) -> DeterministicExperimentClassificationV1:
    captures = _captures_from_cas(
        observation=observation,
        binding=binding,
        cas=cas,
    )
    positive_runtime = (
        observation["host_launch_proof_body"] is not None
        and observation["terminal_event_kind"] == "WORKER_TERMINAL"
        and observation["terminal_outcome"] != "unknown"
        and observation["budget_event_kind"] == "BUDGET_SETTLED"
        and observation["capture_inventory_complete"] is True
    )
    if positive_runtime:
        independent = classify_executable_experiment(binding.spec, captures)
        _require(
            canonical_json_bytes(independent.canonical_body())
            == canonical_json_bytes(observation["classification_body"]),
            "stored_classification_mismatch",
        )
        return independent
    return _validate_forced_inconclusive(
        observation=observation,
        binding=binding,
        captures=captures,
    )


def check_cognitive_reproduction_v1(
    *,
    source_assignment_payload: Mapping[str, Any],
    source_observation_payload: Mapping[str, Any],
    reproduction_assignment_payload: Mapping[str, Any],
    reproduction_observation_payload: Mapping[str, Any],
    cas: ReceiptCAS,
) -> DeterministicCognitiveVerificationCheckV1:
    """Independently compare a source execution with its one reproducer.

    Any malformed lineage, missing CAS object, or mismatch between a stored and
    independently replayed classification becomes ``INVALID_SOURCE``.  Only two
    complete, uniquely classified executions can produce ``SUPPORTED`` or
    ``DISAGREEMENT``; all honest inconclusive runtime states remain ``UNKNOWN``.
    """

    for name, payload in (
        ("source_assignment_payload", source_assignment_payload),
        ("source_observation_payload", source_observation_payload),
        ("reproduction_assignment_payload", reproduction_assignment_payload),
        ("reproduction_observation_payload", reproduction_observation_payload),
    ):
        if not isinstance(payload, Mapping):
            raise TypeError(f"{name} must be a canonical mapping")
    if not isinstance(cas, ReceiptCAS):
        raise TypeError("cas must be ReceiptCAS")

    payload_digests = {
        "source_assignment_payload_digest": canonical_digest(
            source_assignment_payload
        ),
        "source_observation_payload_digest": canonical_digest(
            source_observation_payload
        ),
        "reproduction_assignment_payload_digest": canonical_digest(
            reproduction_assignment_payload
        ),
        "reproduction_observation_payload_digest": canonical_digest(
            reproduction_observation_payload
        ),
    }
    source_binding: ExecutableExperimentBindingV1 | None = None
    reproduction_binding: ExecutableExperimentBindingV1 | None = None
    source_classification: DeterministicExperimentClassificationV1 | None = None
    reproduction_classification: DeterministicExperimentClassificationV1 | None = None

    def result(
        relation: CognitiveVerificationRelationV1,
        *reason_codes: str,
    ) -> DeterministicCognitiveVerificationCheckV1:
        return DeterministicCognitiveVerificationCheckV1(
            relation=relation,
            source_reproduction_kernel_digest=(
                None
                if source_binding is None
                else source_binding.spec.reproduction_kernel_digest
            ),
            reproduction_reproduction_kernel_digest=(
                None
                if reproduction_binding is None
                else reproduction_binding.spec.reproduction_kernel_digest
            ),
            source_classification_body=(
                None
                if source_classification is None
                else source_classification.canonical_body()
            ),
            reproduction_classification_body=(
                None
                if reproduction_classification is None
                else reproduction_classification.canonical_body()
            ),
            reason_codes=tuple(sorted(set(reason_codes))),
            **payload_digests,
        )

    try:
        _stage(
            "source_assignment_invalid",
            lambda: validate_runtime_context_executable_assignment_payload_shape(
                source_assignment_payload
            ),
        )
        _require(
            source_assignment_payload.get("schema_id")
            == COGNITIVE_RUNTIME_EXECUTABLE_ASSIGNMENT_SCHEMA_ID,
            "source_assignment_invalid",
        )
        _stage(
            "source_observation_invalid",
            lambda: validate_runtime_cognitive_observation_payload_shape(
                source_observation_payload
            ),
        )
        _stage(
            "reproduction_assignment_invalid",
            lambda: validate_runtime_reproduction_assignment_payload_shape(
                reproduction_assignment_payload
            ),
        )
        _require(
            reproduction_assignment_payload.get("schema_id")
            == COGNITIVE_RUNTIME_REPRODUCTION_ASSIGNMENT_SCHEMA_ID,
            "reproduction_assignment_invalid",
        )
        _stage(
            "reproduction_observation_invalid",
            lambda: validate_runtime_cognitive_observation_payload_shape(
                reproduction_observation_payload
            ),
        )

        source_binding = _stage(
            "source_executable_binding_invalid",
            lambda: ExecutableExperimentBindingV1.from_canonical(
                source_assignment_payload[
                    "executable_experiment_binding_body"
                ]
            ),
        )
        reproduction_binding = _stage(
            "reproduction_executable_binding_invalid",
            lambda: ExecutableExperimentBindingV1.from_canonical(
                reproduction_assignment_payload[
                    "executable_experiment_binding_body"
                ]
            ),
        )
        _stage(
            "source_executable_cas_invalid",
            lambda: source_binding.resolve(cas),
        )
        _stage(
            "reproduction_executable_cas_invalid",
            lambda: reproduction_binding.resolve(cas),
        )
        _validate_assignment_observation_lineage(
            source_assignment_payload,
            source_observation_payload,
            source_binding,
        )
        _validate_assignment_observation_lineage(
            reproduction_assignment_payload,
            reproduction_observation_payload,
            reproduction_binding,
        )
        _validate_reproduction_source_lineage(
            source_assignment_payload,
            source_observation_payload,
            reproduction_assignment_payload,
            source_binding,
            reproduction_binding,
        )

        source_classification = _stage(
            "source_capture_or_classification_invalid",
            lambda: _independent_classification(
                observation=source_observation_payload,
                binding=source_binding,
                cas=cas,
            ),
        )
        reproduction_classification = _stage(
            "reproduction_capture_or_classification_invalid",
            lambda: _independent_classification(
                observation=reproduction_observation_payload,
                binding=reproduction_binding,
                cas=cas,
            ),
        )
        _require(
            source_observation_payload["usage_status"] == "complete"
            and source_observation_payload["capture_inventory_complete"] is True
            and source_classification.status is ClassificationStatus.OBSERVED,
            "source_observation_not_reproducible",
        )

        if reproduction_observation_payload["usage_status"] != "complete":
            return result(
                CognitiveVerificationRelationV1.UNKNOWN,
                "reproduction_usage_not_complete",
            )
        if reproduction_classification.status is not ClassificationStatus.OBSERVED:
            return result(
                CognitiveVerificationRelationV1.UNKNOWN,
                "reproduction_not_uniquely_classified",
            )
        if (
            source_classification.observed_partition_digest
            == reproduction_classification.observed_partition_digest
        ):
            return result(
                CognitiveVerificationRelationV1.SUPPORTED,
                "independent_classifications_agree",
            )
        return result(
            CognitiveVerificationRelationV1.DISAGREEMENT,
            "independent_classifications_disagree",
        )
    except _InvalidSource as exc:
        return result(
            CognitiveVerificationRelationV1.INVALID_SOURCE,
            exc.reason_code,
        )


__all__ = [
    "ACCEPTED_SET_CHANGE",
    "COGNITIVE_VERIFICATION_CHECKER_VERSION",
    "COGNITIVE_VERIFICATION_CHECK_SCHEMA_ID",
    "PRODUCTION_ENABLED",
    "CognitiveVerificationRelationV1",
    "DeterministicCognitiveVerificationCheckV1",
    "check_cognitive_reproduction_v1",
]
