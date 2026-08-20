"""Runtime-owned pure replay reducer from verified observations to labels.

This module deliberately accepts no proposer score, confidence, prose diagnosis,
or claimed information gain.  It derives one label from typed facts that must have
already crossed an independent verification boundary.  It has no store, runtime,
provider, filesystem, clock, gate, or production authority.

The reducer is conservative around negative evidence.  A failed execution or a
missing observation is never negative information.  ``NEGATIVE_INFORMATION`` is
available only when a deterministic witness enumerates a non-empty bounded domain,
checks that entire domain, and records no positive case.
"""

from __future__ import annotations

from collections.abc import Mapping, Sequence
from dataclasses import dataclass
from enum import Enum

from muteki.epistemic.contracts import canonical_digest


COGNITIVE_OBSERVATION_LABEL_SCHEMA_ID = "muteki.cognitive-observation-label.v1"
COGNITIVE_OBSERVATION_LABEL_REDUCER_VERSION = (
    "muteki.cognitive-observation-label-reducer.v1"
)

_DIGEST_LENGTH = 64


class CognitiveInformationLabelV1(str, Enum):
    NEW_INFORMATION = "new_information"
    REPEAT_INFORMATION = "repeat_information"
    NEGATIVE_INFORMATION = "negative_information"
    NO_INFORMATION = "no_information"
    INCONCLUSIVE = "inconclusive"


class ObservationExecutionStatusV1(str, Enum):
    SUCCEEDED = "succeeded"
    TOOL_FAILURE = "tool_failure"
    PROVIDER_FAILURE = "provider_failure"
    ENVIRONMENT_FAULT = "environment_fault"
    POLICY_BLOCKED = "policy_blocked"
    BUDGET_EXHAUSTED = "budget_exhausted"
    UNKNOWN = "unknown"


class CheckerDispositionV1(str, Enum):
    SUPPORTED = "supported"
    FALSIFIED = "falsified"
    DISAGREEMENT = "disagreement"
    UNKNOWN = "unknown"
    INCOMPLETE = "incomplete"


class ReproductionDispositionV1(str, Enum):
    REPRODUCED = "reproduced"
    NOT_REPRODUCED = "not_reproduced"
    DISAGREEMENT = "disagreement"
    UNKNOWN = "unknown"
    INCOMPLETE = "incomplete"


class ObservationReasonCodeV1(str, Enum):
    EXECUTION_UNKNOWN = "execution_unknown"
    EXECUTION_NOT_SUCCEEDED = "execution_not_succeeded"
    CHECKER_UNKNOWN = "checker_unknown"
    REPRODUCTION_UNKNOWN = "reproduction_unknown"
    VERIFICATION_INCOMPLETE = "verification_incomplete"
    VERIFICATION_DISAGREEMENT = "verification_disagreement"
    VERIFICATION_NOT_SUPPORTED = "verification_not_supported"
    PRIOR_SUPPORTED_PARTITION_CONFLICT = "prior_supported_partition_conflict"
    BOUNDED_NEGATIVE_COVERAGE_INCOMPLETE = "bounded_negative_coverage_incomplete"
    BOUNDED_NEGATIVE_PROVEN = "bounded_negative_proven"
    OBSERVED_PARTITION_MISSING = "observed_partition_missing"
    PREVIOUSLY_SUPPORTED_SAME_RESULT = "previously_supported_same_result"
    LIVE_HYPOTHESES_ELIMINATED = "live_hypotheses_eliminated"
    UNEXPECTED_PARTITION_OPEN_WORLD = "unexpected_partition_open_world"
    SUPPORTED_WITHOUT_DISTINCTION = "supported_without_distinction"


def _digest(value: object, name: str) -> str:
    if type(value) is not str:
        raise TypeError(f"{name} must be an exact string")
    if len(value) != _DIGEST_LENGTH or any(
        character not in "0123456789abcdef" for character in value
    ):
        raise ValueError(f"{name} must be a lowercase sha256 digest")
    return value


def _exact_sequence(value: object, name: str) -> tuple[object, ...]:
    if not isinstance(value, Sequence) or isinstance(value, (str, bytes, bytearray)):
        raise TypeError(f"{name} must be a canonical sequence")
    return tuple(value)


def _exact_keys(
    value: object,
    expected: set[str],
    name: str,
) -> Mapping[str, object]:
    if not isinstance(value, Mapping):
        raise TypeError(f"{name} must be a mapping")
    keys = set(value)
    if keys != expected:
        missing = sorted(expected - keys)
        extra = sorted(keys - expected)
        raise ValueError(f"{name} keys mismatch; missing={missing}, extra={extra}")
    if not all(type(key) is str for key in value):
        raise TypeError(f"{name} keys must be exact strings")
    return value


@dataclass(frozen=True, slots=True)
class ExperimentWorldSemanticKeyV1:
    experiment_semantic_digest: str
    world_epoch_digest: str

    def __post_init__(self) -> None:
        for name in ("experiment_semantic_digest", "world_epoch_digest"):
            object.__setattr__(self, name, _digest(getattr(self, name), name))

    def canonical_body(self) -> dict[str, str]:
        return {
            "experiment_semantic_digest": self.experiment_semantic_digest,
            "world_epoch_digest": self.world_epoch_digest,
        }


@dataclass(frozen=True, slots=True)
class ActiveHypothesisPredictionV1:
    hypothesis_digest: str
    predicted_partition_digest: str

    def __post_init__(self) -> None:
        for name in ("hypothesis_digest", "predicted_partition_digest"):
            object.__setattr__(self, name, _digest(getattr(self, name), name))

    def canonical_body(self) -> dict[str, str]:
        return {
            "hypothesis_digest": self.hypothesis_digest,
            "predicted_partition_digest": self.predicted_partition_digest,
        }


@dataclass(frozen=True, slots=True)
class PriorSupportedObservationV1:
    semantic_key: ExperimentWorldSemanticKeyV1
    observed_partition_digest: str
    verification_occurrence_digest: str

    def __post_init__(self) -> None:
        if type(self.semantic_key) is not ExperimentWorldSemanticKeyV1:
            raise TypeError("semantic_key must be ExperimentWorldSemanticKeyV1")
        for name in (
            "observed_partition_digest",
            "verification_occurrence_digest",
        ):
            object.__setattr__(self, name, _digest(getattr(self, name), name))

    def canonical_body(self) -> dict[str, object]:
        return {
            "observed_partition_digest": self.observed_partition_digest,
            "semantic_key": self.semantic_key.canonical_body(),
            "verification_occurrence_digest": self.verification_occurrence_digest,
        }


@dataclass(frozen=True, slots=True)
class BoundedNegativeCoverageWitnessV1:
    """Deterministic coverage facts for one finite, explicitly bound domain."""

    semantic_key: ExperimentWorldSemanticKeyV1
    tested_behavior_digest: str
    checker_algorithm_digest: str
    checker_receipt_digest: str
    bounded_case_digests: tuple[str, ...]
    checked_case_digests: tuple[str, ...]
    positive_case_digests: tuple[str, ...] = ()

    def __post_init__(self) -> None:
        if type(self.semantic_key) is not ExperimentWorldSemanticKeyV1:
            raise TypeError("semantic_key must be ExperimentWorldSemanticKeyV1")
        for name in (
            "tested_behavior_digest",
            "checker_algorithm_digest",
            "checker_receipt_digest",
        ):
            object.__setattr__(self, name, _digest(getattr(self, name), name))

        normalized: dict[str, tuple[str, ...]] = {}
        for name in (
            "bounded_case_digests",
            "checked_case_digests",
            "positive_case_digests",
        ):
            value = getattr(self, name)
            if type(value) is not tuple:
                raise TypeError(f"{name} must be a built-in tuple")
            checked = tuple(_digest(item, f"{name}[]") for item in value)
            if len(set(checked)) != len(checked):
                raise ValueError(f"{name} contains duplicate cases")
            normalized[name] = tuple(sorted(checked))
            object.__setattr__(self, name, normalized[name])

        if not self.bounded_case_digests:
            raise ValueError("bounded negative coverage requires a non-empty domain")
        bounded = set(self.bounded_case_digests)
        checked = set(self.checked_case_digests)
        positive = set(self.positive_case_digests)
        if not checked <= bounded:
            raise ValueError("checked cases must be inside the bounded domain")
        if not positive <= checked:
            raise ValueError("positive cases must have been checked")

    @property
    def proves_absence(self) -> bool:
        return (
            self.checked_case_digests == self.bounded_case_digests
            and not self.positive_case_digests
        )

    def canonical_body(self) -> dict[str, object]:
        return {
            "bounded_case_digests": self.bounded_case_digests,
            "checked_case_digests": self.checked_case_digests,
            "checker_algorithm_digest": self.checker_algorithm_digest,
            "checker_receipt_digest": self.checker_receipt_digest,
            "positive_case_digests": self.positive_case_digests,
            "semantic_key": self.semantic_key.canonical_body(),
            "tested_behavior_digest": self.tested_behavior_digest,
        }

    @property
    def digest(self) -> str:
        return canonical_digest(self.canonical_body())


@dataclass(frozen=True, slots=True)
class CognitiveObservationLabelRequestV1:
    semantic_key: ExperimentWorldSemanticKeyV1
    active_predictions: tuple[ActiveHypothesisPredictionV1, ...]
    prior_supported_observations: tuple[PriorSupportedObservationV1, ...]
    execution_status: ObservationExecutionStatusV1
    observed_partition_digest: str | None
    checker_disposition: CheckerDispositionV1
    reproduction_disposition: ReproductionDispositionV1
    verification_occurrence_digest: str
    bounded_negative_coverage: BoundedNegativeCoverageWitnessV1 | None = None

    def __post_init__(self) -> None:
        if type(self.semantic_key) is not ExperimentWorldSemanticKeyV1:
            raise TypeError("semantic_key must be ExperimentWorldSemanticKeyV1")
        if type(self.active_predictions) is not tuple or not all(
            type(item) is ActiveHypothesisPredictionV1
            for item in self.active_predictions
        ):
            raise TypeError(
                "active_predictions must be a tuple of ActiveHypothesisPredictionV1"
            )
        if not self.active_predictions:
            raise ValueError("active_predictions must not be empty")
        predictions: dict[str, ActiveHypothesisPredictionV1] = {}
        for item in self.active_predictions:
            if item.hypothesis_digest in predictions:
                raise ValueError("active_predictions contains duplicate hypotheses")
            predictions[item.hypothesis_digest] = item
        object.__setattr__(
            self,
            "active_predictions",
            tuple(predictions[key] for key in sorted(predictions)),
        )

        if type(self.prior_supported_observations) is not tuple or not all(
            type(item) is PriorSupportedObservationV1
            for item in self.prior_supported_observations
        ):
            raise TypeError(
                "prior_supported_observations must be a tuple of "
                "PriorSupportedObservationV1"
            )
        by_occurrence: dict[str, PriorSupportedObservationV1] = {}
        facts: set[tuple[str, str, str]] = set()
        for item in self.prior_supported_observations:
            occurrence = item.verification_occurrence_digest
            if occurrence in by_occurrence:
                raise ValueError(
                    "prior_supported_observations contains a duplicate occurrence"
                )
            fact = (
                item.semantic_key.experiment_semantic_digest,
                item.semantic_key.world_epoch_digest,
                item.observed_partition_digest,
            )
            if fact in facts:
                raise ValueError(
                    "prior_supported_observations contains a duplicate semantic fact"
                )
            facts.add(fact)
            by_occurrence[occurrence] = item
        object.__setattr__(
            self,
            "prior_supported_observations",
            tuple(
                sorted(
                    by_occurrence.values(),
                    key=lambda item: (
                        item.semantic_key.experiment_semantic_digest,
                        item.semantic_key.world_epoch_digest,
                        item.observed_partition_digest,
                        item.verification_occurrence_digest,
                    ),
                )
            ),
        )

        if not isinstance(self.execution_status, ObservationExecutionStatusV1):
            raise TypeError("execution_status must be ObservationExecutionStatusV1")
        if self.observed_partition_digest is not None:
            object.__setattr__(
                self,
                "observed_partition_digest",
                _digest(
                    self.observed_partition_digest,
                    "observed_partition_digest",
                ),
            )
        if not isinstance(self.checker_disposition, CheckerDispositionV1):
            raise TypeError("checker_disposition must be CheckerDispositionV1")
        if not isinstance(self.reproduction_disposition, ReproductionDispositionV1):
            raise TypeError(
                "reproduction_disposition must be ReproductionDispositionV1"
            )
        object.__setattr__(
            self,
            "verification_occurrence_digest",
            _digest(
                self.verification_occurrence_digest,
                "verification_occurrence_digest",
            ),
        )
        if self.verification_occurrence_digest in by_occurrence:
            raise ValueError(
                "the current verification occurrence cannot also be prior evidence"
            )
        if self.bounded_negative_coverage is not None:
            if (
                type(self.bounded_negative_coverage)
                is not BoundedNegativeCoverageWitnessV1
            ):
                raise TypeError(
                    "bounded_negative_coverage must be BoundedNegativeCoverageWitnessV1"
                )
            if self.bounded_negative_coverage.semantic_key != self.semantic_key:
                raise ValueError(
                    "bounded negative coverage is bound to another semantic key"
                )

    def canonical_body(self) -> dict[str, object]:
        return {
            "active_predictions": tuple(
                item.canonical_body() for item in self.active_predictions
            ),
            "bounded_negative_coverage": (
                None
                if self.bounded_negative_coverage is None
                else self.bounded_negative_coverage.canonical_body()
            ),
            "checker_disposition": self.checker_disposition.value,
            "execution_status": self.execution_status.value,
            "observed_partition_digest": self.observed_partition_digest,
            "prior_supported_observations": tuple(
                item.canonical_body() for item in self.prior_supported_observations
            ),
            "reproduction_disposition": self.reproduction_disposition.value,
            "schema_id": COGNITIVE_OBSERVATION_LABEL_SCHEMA_ID,
            "semantic_key": self.semantic_key.canonical_body(),
            "verification_occurrence_digest": self.verification_occurrence_digest,
        }

    @property
    def digest(self) -> str:
        return canonical_digest(self.canonical_body())


@dataclass(frozen=True, slots=True)
class CognitiveObservationLabelV1:
    semantic_key: ExperimentWorldSemanticKeyV1
    label: CognitiveInformationLabelV1
    reason_codes: tuple[ObservationReasonCodeV1, ...]
    eliminated_hypothesis_digests: tuple[str, ...]
    observed_partition_digest: str | None
    verification_occurrence_digest: str
    source_request_digest: str
    bounded_negative_witness_digest: str | None = None

    def __post_init__(self) -> None:
        if type(self.semantic_key) is not ExperimentWorldSemanticKeyV1:
            raise TypeError("semantic_key must be ExperimentWorldSemanticKeyV1")
        if not isinstance(self.label, CognitiveInformationLabelV1):
            raise TypeError("label must be CognitiveInformationLabelV1")
        if type(self.reason_codes) is not tuple or not self.reason_codes:
            raise ValueError("reason_codes must be a non-empty tuple")
        if not all(
            isinstance(item, ObservationReasonCodeV1) for item in self.reason_codes
        ):
            raise TypeError("reason_codes must contain ObservationReasonCodeV1")
        reasons = tuple(sorted(set(self.reason_codes), key=lambda item: item.value))
        if reasons != self.reason_codes:
            raise ValueError("reason_codes must be unique and canonical")

        if type(self.eliminated_hypothesis_digests) is not tuple:
            raise TypeError("eliminated_hypothesis_digests must be a built-in tuple")
        eliminated = tuple(
            _digest(item, "eliminated_hypothesis_digests[]")
            for item in self.eliminated_hypothesis_digests
        )
        if tuple(sorted(set(eliminated))) != eliminated:
            raise ValueError(
                "eliminated_hypothesis_digests must be unique and canonical"
            )
        if self.label is not CognitiveInformationLabelV1.NEW_INFORMATION and eliminated:
            raise ValueError("only new information may eliminate hypotheses")
        if (
            self.label is CognitiveInformationLabelV1.NEW_INFORMATION
            and not eliminated
            and ObservationReasonCodeV1.UNEXPECTED_PARTITION_OPEN_WORLD
            not in self.reason_codes
        ):
            raise ValueError(
                "new information without elimination must be open-world novelty"
            )
        if self.observed_partition_digest is not None:
            object.__setattr__(
                self,
                "observed_partition_digest",
                _digest(
                    self.observed_partition_digest,
                    "observed_partition_digest",
                ),
            )
        for name in ("verification_occurrence_digest", "source_request_digest"):
            object.__setattr__(self, name, _digest(getattr(self, name), name))
        if self.bounded_negative_witness_digest is not None:
            object.__setattr__(
                self,
                "bounded_negative_witness_digest",
                _digest(
                    self.bounded_negative_witness_digest,
                    "bounded_negative_witness_digest",
                ),
            )
        if self.label is CognitiveInformationLabelV1.NEGATIVE_INFORMATION:
            if self.bounded_negative_witness_digest is None:
                raise ValueError(
                    "negative information requires a bounded negative witness"
                )
        elif self.bounded_negative_witness_digest is not None:
            raise ValueError(
                "only negative information may bind a bounded negative witness"
            )

    def canonical_body(self) -> dict[str, object]:
        return {
            "bounded_negative_witness_digest": (self.bounded_negative_witness_digest),
            "eliminated_hypothesis_digests": self.eliminated_hypothesis_digests,
            "label": self.label.value,
            "observed_partition_digest": self.observed_partition_digest,
            "reason_codes": tuple(item.value for item in self.reason_codes),
            "reducer_version": COGNITIVE_OBSERVATION_LABEL_REDUCER_VERSION,
            "schema_id": COGNITIVE_OBSERVATION_LABEL_SCHEMA_ID,
            "semantic_key": self.semantic_key.canonical_body(),
            "source_request_digest": self.source_request_digest,
            "verification_occurrence_digest": self.verification_occurrence_digest,
        }

    @property
    def digest(self) -> str:
        return canonical_digest(self.canonical_body())


def _inconclusive(
    request: CognitiveObservationLabelRequestV1,
    reasons: set[ObservationReasonCodeV1],
) -> CognitiveObservationLabelV1:
    return CognitiveObservationLabelV1(
        semantic_key=request.semantic_key,
        label=CognitiveInformationLabelV1.INCONCLUSIVE,
        reason_codes=tuple(sorted(reasons, key=lambda item: item.value)),
        eliminated_hypothesis_digests=(),
        observed_partition_digest=request.observed_partition_digest,
        verification_occurrence_digest=request.verification_occurrence_digest,
        source_request_digest=request.digest,
    )


def reduce_cognitive_observation_label_v1(
    request: CognitiveObservationLabelRequestV1,
) -> CognitiveObservationLabelV1:
    """Derive a label without consulting a proposer-authored score or prose."""

    if type(request) is not CognitiveObservationLabelRequestV1:
        raise TypeError("request must be CognitiveObservationLabelRequestV1")

    blockers: set[ObservationReasonCodeV1] = set()
    if request.execution_status is ObservationExecutionStatusV1.UNKNOWN:
        blockers.add(ObservationReasonCodeV1.EXECUTION_UNKNOWN)
    elif request.execution_status is not ObservationExecutionStatusV1.SUCCEEDED:
        blockers.add(ObservationReasonCodeV1.EXECUTION_NOT_SUCCEEDED)

    if request.checker_disposition is CheckerDispositionV1.UNKNOWN:
        blockers.add(ObservationReasonCodeV1.CHECKER_UNKNOWN)
    if request.reproduction_disposition is ReproductionDispositionV1.UNKNOWN:
        blockers.add(ObservationReasonCodeV1.REPRODUCTION_UNKNOWN)
    if (
        request.checker_disposition is CheckerDispositionV1.INCOMPLETE
        or request.reproduction_disposition is ReproductionDispositionV1.INCOMPLETE
    ):
        blockers.add(ObservationReasonCodeV1.VERIFICATION_INCOMPLETE)
    if (
        request.checker_disposition is CheckerDispositionV1.DISAGREEMENT
        or request.reproduction_disposition is ReproductionDispositionV1.DISAGREEMENT
    ):
        blockers.add(ObservationReasonCodeV1.VERIFICATION_DISAGREEMENT)
    if (
        request.checker_disposition is not CheckerDispositionV1.SUPPORTED
        or request.reproduction_disposition is not ReproductionDispositionV1.REPRODUCED
    ) and not blockers:
        blockers.add(ObservationReasonCodeV1.VERIFICATION_NOT_SUPPORTED)

    same_key_priors = tuple(
        item
        for item in request.prior_supported_observations
        if item.semantic_key == request.semantic_key
    )
    prior_partitions = {item.observed_partition_digest for item in same_key_priors}
    if len(prior_partitions) > 1:
        blockers.add(ObservationReasonCodeV1.PRIOR_SUPPORTED_PARTITION_CONFLICT)
    elif (
        prior_partitions
        and request.observed_partition_digest is not None
        and request.observed_partition_digest not in prior_partitions
    ):
        blockers.add(ObservationReasonCodeV1.PRIOR_SUPPORTED_PARTITION_CONFLICT)

    witness = request.bounded_negative_coverage
    if witness is not None and not witness.proves_absence:
        blockers.add(ObservationReasonCodeV1.BOUNDED_NEGATIVE_COVERAGE_INCOMPLETE)
    if blockers:
        return _inconclusive(request, blockers)

    if witness is not None:
        return CognitiveObservationLabelV1(
            semantic_key=request.semantic_key,
            label=CognitiveInformationLabelV1.NEGATIVE_INFORMATION,
            reason_codes=(ObservationReasonCodeV1.BOUNDED_NEGATIVE_PROVEN,),
            eliminated_hypothesis_digests=(),
            observed_partition_digest=request.observed_partition_digest,
            verification_occurrence_digest=request.verification_occurrence_digest,
            source_request_digest=request.digest,
            bounded_negative_witness_digest=witness.digest,
        )

    observed = request.observed_partition_digest
    if observed is None:
        return _inconclusive(
            request,
            {ObservationReasonCodeV1.OBSERVED_PARTITION_MISSING},
        )
    if observed in prior_partitions:
        return CognitiveObservationLabelV1(
            semantic_key=request.semantic_key,
            label=CognitiveInformationLabelV1.REPEAT_INFORMATION,
            reason_codes=(ObservationReasonCodeV1.PREVIOUSLY_SUPPORTED_SAME_RESULT,),
            eliminated_hypothesis_digests=(),
            observed_partition_digest=observed,
            verification_occurrence_digest=request.verification_occurrence_digest,
            source_request_digest=request.digest,
        )

    matching = {
        item.hypothesis_digest
        for item in request.active_predictions
        if item.predicted_partition_digest == observed
    }
    if not matching:
        return CognitiveObservationLabelV1(
            semantic_key=request.semantic_key,
            label=CognitiveInformationLabelV1.NEW_INFORMATION,
            reason_codes=(ObservationReasonCodeV1.UNEXPECTED_PARTITION_OPEN_WORLD,),
            eliminated_hypothesis_digests=(),
            observed_partition_digest=observed,
            verification_occurrence_digest=request.verification_occurrence_digest,
            source_request_digest=request.digest,
        )

    eliminated = tuple(
        sorted(
            item.hypothesis_digest
            for item in request.active_predictions
            if item.hypothesis_digest not in matching
        )
    )
    if eliminated:
        return CognitiveObservationLabelV1(
            semantic_key=request.semantic_key,
            label=CognitiveInformationLabelV1.NEW_INFORMATION,
            reason_codes=(ObservationReasonCodeV1.LIVE_HYPOTHESES_ELIMINATED,),
            eliminated_hypothesis_digests=eliminated,
            observed_partition_digest=observed,
            verification_occurrence_digest=request.verification_occurrence_digest,
            source_request_digest=request.digest,
        )
    return CognitiveObservationLabelV1(
        semantic_key=request.semantic_key,
        label=CognitiveInformationLabelV1.NO_INFORMATION,
        reason_codes=(ObservationReasonCodeV1.SUPPORTED_WITHOUT_DISTINCTION,),
        eliminated_hypothesis_digests=(),
        observed_partition_digest=observed,
        verification_occurrence_digest=request.verification_occurrence_digest,
        source_request_digest=request.digest,
    )


_LABEL_BODY_KEYS = {
    "bounded_negative_witness_digest",
    "eliminated_hypothesis_digests",
    "label",
    "observed_partition_digest",
    "reason_codes",
    "reducer_version",
    "schema_id",
    "semantic_key",
    "source_request_digest",
    "verification_occurrence_digest",
}


def cognitive_observation_label_from_canonical_v1(
    value: object,
) -> CognitiveObservationLabelV1:
    """Strictly reconstruct an immutable reducer output from canonical JSON."""

    body = _exact_keys(value, _LABEL_BODY_KEYS, "label")
    if body["schema_id"] != COGNITIVE_OBSERVATION_LABEL_SCHEMA_ID:
        raise ValueError("label schema_id is not supported")
    if body["reducer_version"] != COGNITIVE_OBSERVATION_LABEL_REDUCER_VERSION:
        raise ValueError("label reducer_version is not supported")
    key_body = _exact_keys(
        body["semantic_key"],
        {"experiment_semantic_digest", "world_epoch_digest"},
        "semantic_key",
    )
    reason_values = _exact_sequence(body["reason_codes"], "reason_codes")
    eliminated_values = _exact_sequence(
        body["eliminated_hypothesis_digests"],
        "eliminated_hypothesis_digests",
    )
    try:
        label = CognitiveInformationLabelV1(body["label"])
        reasons = tuple(ObservationReasonCodeV1(item) for item in reason_values)
    except (TypeError, ValueError) as error:
        raise ValueError("label contains an unknown enum value") from error
    return CognitiveObservationLabelV1(
        semantic_key=ExperimentWorldSemanticKeyV1(
            experiment_semantic_digest=key_body["experiment_semantic_digest"],
            world_epoch_digest=key_body["world_epoch_digest"],
        ),
        label=label,
        reason_codes=reasons,
        eliminated_hypothesis_digests=tuple(eliminated_values),
        observed_partition_digest=body["observed_partition_digest"],
        verification_occurrence_digest=body["verification_occurrence_digest"],
        source_request_digest=body["source_request_digest"],
        bounded_negative_witness_digest=body["bounded_negative_witness_digest"],
    )


def replay_cognitive_observation_label_v1(
    request: CognitiveObservationLabelRequestV1,
    supplied_output: object,
) -> CognitiveObservationLabelV1:
    """Recompute the decision and reject a modified or differently derived output."""

    expected = reduce_cognitive_observation_label_v1(request)
    supplied = cognitive_observation_label_from_canonical_v1(supplied_output)
    if supplied.digest != expected.digest:
        raise ValueError("supplied cognitive observation label diverges on replay")
    return expected


__all__ = [
    "COGNITIVE_OBSERVATION_LABEL_REDUCER_VERSION",
    "COGNITIVE_OBSERVATION_LABEL_SCHEMA_ID",
    "ActiveHypothesisPredictionV1",
    "BoundedNegativeCoverageWitnessV1",
    "CheckerDispositionV1",
    "CognitiveInformationLabelV1",
    "CognitiveObservationLabelRequestV1",
    "CognitiveObservationLabelV1",
    "ExperimentWorldSemanticKeyV1",
    "ObservationExecutionStatusV1",
    "ObservationReasonCodeV1",
    "PriorSupportedObservationV1",
    "ReproductionDispositionV1",
    "cognitive_observation_label_from_canonical_v1",
    "reduce_cognitive_observation_label_v1",
    "replay_cognitive_observation_label_v1",
]
