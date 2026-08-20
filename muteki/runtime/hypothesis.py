"""Pure deterministic H5 hypothesis and experiment selection contracts.

This module is deliberately *not* an authority.  It has no store, provider,
worker, admission, budget, effect, verifier, or gate capability.  A caller may
use its result to make an admission request later, but the result itself is only
an immutable recommendation built from typed inputs.

The important asymmetries are intentional:

* a semantic resemblance never suppresses an experiment;
* suppression requires a bound deterministic equivalence/coverage proof;
* a possible duplicate remains eligible and is only labelled for diagnostics;
* a tombstone is narrow (same scope, hypotheses, and covered partitions) and a
  typed reopen trigger makes it inactive;
* UNKNOWN, tool, provider, and infrastructure outcomes cannot create a
  tombstone; and
* ordering is a pure greedy portfolio order over currently uncovered typed
  partitions.  There is no model-authored information-gain score in this API.
"""

from __future__ import annotations

from dataclasses import dataclass
from enum import Enum

from muteki.epistemic.contracts import canonical_digest


# v3 keeps v2's canonical prediction/reopen order and makes raw structural
# suppression an explicit research-fixture operation.  Production callers must
# use SearchKernel's default-off H5RecommendationGateV1, which refuses opaque
# proof/tombstone inputs altogether.
H5_SELECTOR_VERSION = "muteki.runtime-h5-selector.v3"
H5_RECOMMENDATION_REQUEST_VERSION = "muteki.runtime-h5-recommendation-request.v2"


def _text(value: object, name: str) -> str:
    if type(value) is not str or not value or value != value.strip():
        raise ValueError(f"{name} must be a non-empty canonical string")
    return value


def _identifier(value: object, name: str) -> str:
    result = _text(value, name)
    if len(result) > 160 or not result[0].isalnum():
        raise ValueError(f"{name} must be a bounded canonical identifier")
    if any(
        character
        not in "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789_.:-"
        for character in result
    ):
        raise ValueError(f"{name} must be a bounded canonical identifier")
    return result


def _digest(value: object, name: str) -> str:
    result = _text(value, name)
    if len(result) != 64 or any(character not in "0123456789abcdef" for character in result):
        raise ValueError(f"{name} must be a lowercase sha256 digest")
    return result


def _positive_int(value: object, name: str) -> int:
    if type(value) is not int or value <= 0:
        raise ValueError(f"{name} must be a positive exact integer")
    return value


def _non_negative_int(value: object, name: str) -> int:
    if type(value) is not int or value < 0:
        raise ValueError(f"{name} must be a non-negative exact integer")
    return value


def _digest_set(
    value: object,
    name: str,
    *,
    required: bool = False,
) -> tuple[str, ...]:
    if type(value) is not tuple:
        raise TypeError(f"{name} must be a built-in tuple")
    result = tuple(_digest(item, f"{name}[{index}]") for index, item in enumerate(value))
    if required and not result:
        raise ValueError(f"{name} must not be empty")
    if len(set(result)) != len(result):
        raise ValueError(f"{name} must not contain duplicates")
    return tuple(sorted(result))


class ActionClass(str, Enum):
    DISCOVERY_PROBE = "discovery_probe"
    DISCRIMINATING_EXPERIMENT = "discriminating_experiment"
    VERIFICATION_PROBE = "verification_probe"
    OPERATIONAL_STEP = "operational_step"


class EffectClass(str, Enum):
    PURE_COGNITIVE = "pure_cognitive"
    LOCAL_ISOLATED = "local_isolated"
    IDEMPOTENT = "idempotent"
    COMPENSATABLE = "compensatable"
    NON_IDEMPOTENT = "non_idempotent"
    UNKNOWN = "unknown"


class EquivalenceRelation(str, Enum):
    EXACT_EQUAL = "exact_equal"
    PRIOR_COVERS_CANDIDATE = "prior_covers_candidate"


class DuplicateDecision(str, Enum):
    NOT_PROVEN = "not_proven"
    POSSIBLE_DUPLICATE = "possible_duplicate"
    PROVEN_EQUIVALENT = "proven_equivalent"


class SelectionDisposition(str, Enum):
    ELIGIBLE = "eligible"
    SUPPRESSED_PROVEN_EQUIVALENT = "suppressed_proven_equivalent"
    BLOCKED_TOMBSTONE = "blocked_tombstone"


class ExecutionOutcome(str, Enum):
    SUCCEEDED = "succeeded"
    TOOL_FAILURE = "tool_failure"
    PROVIDER_FAILURE = "provider_failure"
    INFRA_FAILURE = "infra_failure"
    POLICY_BLOCKED = "policy_blocked"
    BUDGET_EXHAUSTED = "budget_exhausted"
    UNKNOWN = "unknown"


class EpistemicOutcome(str, Enum):
    NEW_INFORMATION = "new_information"
    REPEAT_INFORMATION = "repeat_information"
    NEGATIVE_INFORMATION = "negative_information"
    NO_INFORMATION = "no_information"
    INCONCLUSIVE = "inconclusive"
    NOT_EVALUATED = "not_evaluated"


class RecoveryDisposition(str, Enum):
    NONE = "none"
    DIAGNOSTIC_PROBE = "diagnostic_probe"
    HOLD_RECONCILIATION = "hold_reconciliation"


class ReopenPredicateKind(str, Enum):
    CONTRADICTORY_EVIDENCE = "contradictory_evidence"
    WORLD_EPOCH_CHANGED = "world_epoch_changed"
    TOOL_POLICY_CHANGED = "tool_policy_changed"
    MODEL_POLICY_CHANGED = "model_policy_changed"
    CAPABILITY_ADDED = "capability_added"
    PARAMETER_REGION_ENLARGED = "parameter_region_enlarged"
    VERIFIER_INVALIDATED = "verifier_invalidated"
    SCHEMA_VERSION_ADVANCED = "schema_version_advanced"
    REPAIR_POLICY_ADVANCED = "repair_policy_advanced"
    OPERATOR_OVERRIDE = "operator_override"


@dataclass(frozen=True, slots=True)
class Hypothesis:
    """A typed, open-ended explanation with an explicit OTHER/UNKNOWN lane."""

    hypothesis_id: str
    version: int
    scope_digest: str
    claim_digest: str
    prediction_partition_digests: tuple[str, ...]
    other_unknown_lane: bool = True

    def __post_init__(self) -> None:
        object.__setattr__(self, "hypothesis_id", _identifier(self.hypothesis_id, "hypothesis_id"))
        object.__setattr__(self, "version", _positive_int(self.version, "version"))
        object.__setattr__(self, "scope_digest", _digest(self.scope_digest, "scope_digest"))
        object.__setattr__(self, "claim_digest", _digest(self.claim_digest, "claim_digest"))
        object.__setattr__(
            self,
            "prediction_partition_digests",
            _digest_set(
                self.prediction_partition_digests,
                "prediction_partition_digests",
                required=True,
            ),
        )
        if type(self.other_unknown_lane) is not bool or not self.other_unknown_lane:
            raise ValueError("a hypothesis must preserve an OTHER/UNKNOWN lane")

    def canonical_body(self) -> dict[str, object]:
        return {
            "claim_digest": self.claim_digest,
            "hypothesis_id": self.hypothesis_id,
            "other_unknown_lane": self.other_unknown_lane,
            "prediction_partition_digests": self.prediction_partition_digests,
            "scope_digest": self.scope_digest,
            "version": self.version,
        }

    @property
    def digest(self) -> str:
        return canonical_digest(self.canonical_body())


@dataclass(frozen=True, slots=True)
class ExperimentPrediction:
    hypothesis_digest: str
    predicate_digest: str
    outcome_partition_digest: str

    def __post_init__(self) -> None:
        for name in (
            "hypothesis_digest",
            "predicate_digest",
            "outcome_partition_digest",
        ):
            object.__setattr__(self, name, _digest(getattr(self, name), name))

    def canonical_body(self) -> dict[str, object]:
        return {
            "hypothesis_digest": self.hypothesis_digest,
            "outcome_partition_digest": self.outcome_partition_digest,
            "predicate_digest": self.predicate_digest,
        }


@dataclass(frozen=True, slots=True)
class SemanticSignature:
    """Deterministic semantic input used only with an independent proof."""

    action_class: ActionClass
    tool_capability_digest: str
    resource_digest: str
    parameter_region_digest: str
    precondition_set_digest: str
    read_set_digest: str
    world_epoch_digest: str
    tool_policy_digest: str
    model_policy_digest: str
    prediction_partition_digests: tuple[str, ...]
    stop_condition_digests: tuple[str, ...]
    effect_class: EffectClass
    canonicalizer_version: str = H5_SELECTOR_VERSION

    def __post_init__(self) -> None:
        if type(self.action_class) is not ActionClass:
            raise TypeError("action_class must be ActionClass")
        if type(self.effect_class) is not EffectClass:
            raise TypeError("effect_class must be EffectClass")
        for name in (
            "tool_capability_digest",
            "resource_digest",
            "parameter_region_digest",
            "precondition_set_digest",
            "read_set_digest",
            "world_epoch_digest",
            "tool_policy_digest",
            "model_policy_digest",
        ):
            object.__setattr__(self, name, _digest(getattr(self, name), name))
        object.__setattr__(
            self,
            "prediction_partition_digests",
            _digest_set(
                self.prediction_partition_digests,
                "prediction_partition_digests",
                required=True,
            ),
        )
        object.__setattr__(
            self,
            "stop_condition_digests",
            _digest_set(self.stop_condition_digests, "stop_condition_digests", required=True),
        )
        object.__setattr__(
            self,
            "canonicalizer_version",
            _identifier(self.canonicalizer_version, "canonicalizer_version"),
        )
        if self.canonicalizer_version != H5_SELECTOR_VERSION:
            raise ValueError("unsupported semantic canonicalizer version")

    def canonical_body(self) -> dict[str, object]:
        return {
            "action_class": self.action_class.value,
            "canonicalizer_version": self.canonicalizer_version,
            "effect_class": self.effect_class.value,
            "model_policy_digest": self.model_policy_digest,
            "parameter_region_digest": self.parameter_region_digest,
            "prediction_partition_digests": self.prediction_partition_digests,
            "precondition_set_digest": self.precondition_set_digest,
            "read_set_digest": self.read_set_digest,
            "resource_digest": self.resource_digest,
            "stop_condition_digests": self.stop_condition_digests,
            "tool_capability_digest": self.tool_capability_digest,
            "tool_policy_digest": self.tool_policy_digest,
            "world_epoch_digest": self.world_epoch_digest,
        }

    @property
    def digest(self) -> str:
        return canonical_digest(self.canonical_body())


@dataclass(frozen=True, slots=True)
class DiscriminatingExperiment:
    """Host-validated typed execution boundary; still not an admission permit."""

    experiment_id: str
    version: int
    context_packet_digest: str
    scope_digest: str
    semantic_signature: SemanticSignature
    hypothesis_digests: tuple[str, ...]
    predictions: tuple[ExperimentPrediction, ...]
    estimated_cost_units: int

    def __post_init__(self) -> None:
        object.__setattr__(self, "experiment_id", _identifier(self.experiment_id, "experiment_id"))
        object.__setattr__(self, "version", _positive_int(self.version, "version"))
        object.__setattr__(
            self,
            "context_packet_digest",
            _digest(self.context_packet_digest, "context_packet_digest"),
        )
        object.__setattr__(self, "scope_digest", _digest(self.scope_digest, "scope_digest"))
        if type(self.semantic_signature) is not SemanticSignature:
            raise TypeError("semantic_signature must be SemanticSignature")
        if self.semantic_signature.action_class is not ActionClass.DISCRIMINATING_EXPERIMENT:
            raise ValueError("H5 experiment action must be discriminating_experiment")
        hypotheses = _digest_set(self.hypothesis_digests, "hypothesis_digests", required=True)
        if len(hypotheses) < 2:
            raise ValueError("a discriminating experiment requires at least two hypotheses")
        object.__setattr__(self, "hypothesis_digests", hypotheses)
        if type(self.predictions) is not tuple or not self.predictions or not all(
            type(item) is ExperimentPrediction for item in self.predictions
        ):
            raise TypeError("predictions must be a non-empty tuple of ExperimentPrediction")
        by_hypothesis: dict[str, ExperimentPrediction] = {}
        for prediction in self.predictions:
            if prediction.hypothesis_digest in by_hypothesis:
                raise ValueError("a discriminating experiment permits one prediction per hypothesis")
            by_hypothesis[prediction.hypothesis_digest] = prediction
        if set(by_hypothesis) != set(hypotheses):
            raise ValueError("every bound hypothesis requires one prospective prediction")
        predictions = tuple(by_hypothesis[digest] for digest in hypotheses)
        object.__setattr__(self, "predictions", predictions)
        if len({item.outcome_partition_digest for item in predictions}) < 2:
            raise ValueError("experiment does not prospectively distinguish hypotheses")
        prediction_partitions = tuple(
            sorted({item.outcome_partition_digest for item in predictions})
        )
        if prediction_partitions != self.semantic_signature.prediction_partition_digests:
            raise ValueError("semantic signature must bind exactly the predicted partitions")
        object.__setattr__(
            self,
            "estimated_cost_units",
            _positive_int(self.estimated_cost_units, "estimated_cost_units"),
        )

    @property
    def predicted_partition_digests(self) -> tuple[str, ...]:
        return self.semantic_signature.prediction_partition_digests

    def canonical_body(self) -> dict[str, object]:
        return {
            "context_packet_digest": self.context_packet_digest,
            "estimated_cost_units": self.estimated_cost_units,
            "experiment_id": self.experiment_id,
            "hypothesis_digests": self.hypothesis_digests,
            "predictions": tuple(item.canonical_body() for item in self.predictions),
            "scope_digest": self.scope_digest,
            "semantic_signature": self.semantic_signature.canonical_body(),
            "version": self.version,
        }

    @property
    def digest(self) -> str:
        return canonical_digest(self.canonical_body())


@dataclass(frozen=True, slots=True)
class EquivalenceProof:
    """Independent, receipt-bound proof that may suppress one candidate."""

    prior_experiment_digest: str
    candidate_experiment_digest: str
    prior_signature_digest: str
    candidate_signature_digest: str
    relation: EquivalenceRelation
    deterministic_checker_receipt_digest: str
    coverage_proof_receipt_digest: str | None = None

    def __post_init__(self) -> None:
        for name in (
            "prior_experiment_digest",
            "candidate_experiment_digest",
            "prior_signature_digest",
            "candidate_signature_digest",
            "deterministic_checker_receipt_digest",
        ):
            object.__setattr__(self, name, _digest(getattr(self, name), name))
        if self.prior_experiment_digest == self.candidate_experiment_digest:
            raise ValueError("an equivalence proof must relate distinct experiments")
        if type(self.relation) is not EquivalenceRelation:
            raise TypeError("relation must be EquivalenceRelation")
        if self.coverage_proof_receipt_digest is not None:
            object.__setattr__(
                self,
                "coverage_proof_receipt_digest",
                _digest(self.coverage_proof_receipt_digest, "coverage_proof_receipt_digest"),
            )
        if (
            self.relation is EquivalenceRelation.EXACT_EQUAL
            and self.prior_signature_digest != self.candidate_signature_digest
        ):
            raise ValueError("exact equality proof requires equal signatures")
        if (
            self.relation is EquivalenceRelation.PRIOR_COVERS_CANDIDATE
            and self.coverage_proof_receipt_digest is None
        ):
            raise ValueError("coverage suppression requires a coverage proof receipt")

    def canonical_body(self) -> dict[str, object]:
        return {
            "candidate_experiment_digest": self.candidate_experiment_digest,
            "candidate_signature_digest": self.candidate_signature_digest,
            "coverage_proof_receipt_digest": self.coverage_proof_receipt_digest,
            "deterministic_checker_receipt_digest": self.deterministic_checker_receipt_digest,
            "prior_experiment_digest": self.prior_experiment_digest,
            "prior_signature_digest": self.prior_signature_digest,
            "relation": self.relation.value,
        }

    @property
    def digest(self) -> str:
        return canonical_digest(self.canonical_body())


@dataclass(frozen=True, slots=True)
class PossibleDuplicateRecommendation:
    """Non-authoritative similarity diagnostic; it never blocks an experiment."""

    prior_signature_digest: str
    candidate_signature_digest: str
    recommender_profile_digest: str
    rationale_digest: str

    def __post_init__(self) -> None:
        for name in (
            "prior_signature_digest",
            "candidate_signature_digest",
            "recommender_profile_digest",
            "rationale_digest",
        ):
            object.__setattr__(self, name, _digest(getattr(self, name), name))

    def canonical_body(self) -> dict[str, object]:
        return {
            "candidate_signature_digest": self.candidate_signature_digest,
            "prior_signature_digest": self.prior_signature_digest,
            "rationale_digest": self.rationale_digest,
            "recommender_profile_digest": self.recommender_profile_digest,
        }

    @property
    def digest(self) -> str:
        return canonical_digest(self.canonical_body())


@dataclass(frozen=True, slots=True)
class ReopenTrigger:
    kind: ReopenPredicateKind
    observed_digest: str
    evidence_receipt_digest: str
    version: int = 0

    def __post_init__(self) -> None:
        if type(self.kind) is not ReopenPredicateKind:
            raise TypeError("kind must be ReopenPredicateKind")
        object.__setattr__(self, "observed_digest", _digest(self.observed_digest, "observed_digest"))
        object.__setattr__(
            self,
            "evidence_receipt_digest",
            _digest(self.evidence_receipt_digest, "evidence_receipt_digest"),
        )
        object.__setattr__(self, "version", _non_negative_int(self.version, "version"))

    def canonical_body(self) -> dict[str, object]:
        return {
            "evidence_receipt_digest": self.evidence_receipt_digest,
            "kind": self.kind.value,
            "observed_digest": self.observed_digest,
            "version": self.version,
        }

    @property
    def digest(self) -> str:
        return canonical_digest(self.canonical_body())


@dataclass(frozen=True, slots=True)
class ReopenPredicate:
    predicate_id: str
    kind: ReopenPredicateKind
    baseline_digest: str
    minimum_version: int = 0

    def __post_init__(self) -> None:
        object.__setattr__(self, "predicate_id", _identifier(self.predicate_id, "predicate_id"))
        if type(self.kind) is not ReopenPredicateKind:
            raise TypeError("kind must be ReopenPredicateKind")
        object.__setattr__(self, "baseline_digest", _digest(self.baseline_digest, "baseline_digest"))
        object.__setattr__(
            self,
            "minimum_version",
            _non_negative_int(self.minimum_version, "minimum_version"),
        )
        if self.kind in {
            ReopenPredicateKind.SCHEMA_VERSION_ADVANCED,
            ReopenPredicateKind.REPAIR_POLICY_ADVANCED,
        } and self.minimum_version == 0:
            raise ValueError("version reopen predicates require a positive minimum")

    def matches(self, trigger: ReopenTrigger) -> bool:
        return (
            type(trigger) is ReopenTrigger
            and trigger.kind is self.kind
            and trigger.observed_digest != self.baseline_digest
            and trigger.version >= self.minimum_version
        )

    def canonical_body(self) -> dict[str, object]:
        return {
            "baseline_digest": self.baseline_digest,
            "kind": self.kind.value,
            "minimum_version": self.minimum_version,
            "predicate_id": self.predicate_id,
        }

    @property
    def digest(self) -> str:
        return canonical_digest(self.canonical_body())


@dataclass(frozen=True, slots=True)
class ExperimentOutcome:
    """Settled outcome classification usable for conservative tombstone creation."""

    experiment_digest: str
    execution: ExecutionOutcome
    epistemic: EpistemicOutcome
    covered_partition_digests: tuple[str, ...] = ()
    deterministic_checker_receipt_digest: str | None = None

    def __post_init__(self) -> None:
        object.__setattr__(self, "experiment_digest", _digest(self.experiment_digest, "experiment_digest"))
        if type(self.execution) is not ExecutionOutcome:
            raise TypeError("execution must be ExecutionOutcome")
        if type(self.epistemic) is not EpistemicOutcome:
            raise TypeError("epistemic must be EpistemicOutcome")
        object.__setattr__(
            self,
            "covered_partition_digests",
            _digest_set(self.covered_partition_digests, "covered_partition_digests"),
        )
        if self.deterministic_checker_receipt_digest is not None:
            object.__setattr__(
                self,
                "deterministic_checker_receipt_digest",
                _digest(
                    self.deterministic_checker_receipt_digest,
                    "deterministic_checker_receipt_digest",
                ),
            )
        if self.execution is ExecutionOutcome.SUCCEEDED:
            if self.epistemic is EpistemicOutcome.NOT_EVALUATED:
                raise ValueError("successful execution requires an epistemic classification")
        elif self.epistemic is not EpistemicOutcome.NOT_EVALUATED:
            raise ValueError("non-success execution cannot masquerade as epistemic evidence")
        if self.epistemic is EpistemicOutcome.NEGATIVE_INFORMATION:
            if not self.covered_partition_digests:
                raise ValueError("negative information requires bounded coverage")
            if self.deterministic_checker_receipt_digest is None:
                raise ValueError("negative information requires a deterministic checker receipt")
        elif self.covered_partition_digests or self.deterministic_checker_receipt_digest is not None:
            raise ValueError("only verified negative information can carry branch coverage")

    @property
    def recovery_disposition(self) -> RecoveryDisposition:
        if self.execution is ExecutionOutcome.UNKNOWN:
            return RecoveryDisposition.HOLD_RECONCILIATION
        if self.execution in {
            ExecutionOutcome.TOOL_FAILURE,
            ExecutionOutcome.PROVIDER_FAILURE,
            ExecutionOutcome.INFRA_FAILURE,
        }:
            return RecoveryDisposition.DIAGNOSTIC_PROBE
        return RecoveryDisposition.NONE

    @property
    def closure_eligible(self) -> bool:
        return (
            self.execution is ExecutionOutcome.SUCCEEDED
            and self.epistemic is EpistemicOutcome.NEGATIVE_INFORMATION
            and bool(self.covered_partition_digests)
            and self.deterministic_checker_receipt_digest is not None
        )

    def canonical_body(self) -> dict[str, object]:
        return {
            "covered_partition_digests": self.covered_partition_digests,
            "deterministic_checker_receipt_digest": self.deterministic_checker_receipt_digest,
            "epistemic": self.epistemic.value,
            "execution": self.execution.value,
            "experiment_digest": self.experiment_digest,
            "recovery_disposition": self.recovery_disposition.value,
        }

    @property
    def digest(self) -> str:
        return canonical_digest(self.canonical_body())


@dataclass(frozen=True, slots=True)
class DeadEndTombstone:
    """Narrow branch closure.  It is inactive once a typed reopen matches."""

    tombstone_id: str
    scope_digest: str
    hypothesis_digests: tuple[str, ...]
    covered_partition_digests: tuple[str, ...]
    closure_receipt_digest: str
    reopen_predicates: tuple[ReopenPredicate, ...]

    def __post_init__(self) -> None:
        object.__setattr__(self, "tombstone_id", _identifier(self.tombstone_id, "tombstone_id"))
        object.__setattr__(self, "scope_digest", _digest(self.scope_digest, "scope_digest"))
        object.__setattr__(
            self,
            "hypothesis_digests",
            _digest_set(self.hypothesis_digests, "hypothesis_digests", required=True),
        )
        object.__setattr__(
            self,
            "covered_partition_digests",
            _digest_set(
                self.covered_partition_digests,
                "covered_partition_digests",
                required=True,
            ),
        )
        object.__setattr__(
            self,
            "closure_receipt_digest",
            _digest(self.closure_receipt_digest, "closure_receipt_digest"),
        )
        if type(self.reopen_predicates) is not tuple or not self.reopen_predicates or not all(
            type(item) is ReopenPredicate for item in self.reopen_predicates
        ):
            raise TypeError("reopen_predicates must be a non-empty tuple of ReopenPredicate")
        if len({item.predicate_id for item in self.reopen_predicates}) != len(
            self.reopen_predicates
        ):
            raise ValueError("reopen predicate ids must be unique")
        object.__setattr__(
            self,
            "reopen_predicates",
            tuple(sorted(self.reopen_predicates, key=lambda item: item.predicate_id)),
        )

    @classmethod
    def from_settled_negative_outcomes(
        cls,
        *,
        tombstone_id: str,
        scope_digest: str,
        hypothesis_digests: tuple[str, ...],
        outcomes: tuple[ExperimentOutcome, ...],
        closure_receipt_digest: str,
        reopen_predicates: tuple[ReopenPredicate, ...],
    ) -> "DeadEndTombstone":
        """Create closure only from independently checked, settled negatives.

        UNKNOWN, tool, provider, and infrastructure outcomes all fail this method;
        callers must reconcile or diagnose them through a separately admitted path.
        """

        if type(outcomes) is not tuple or not outcomes or not all(
            type(item) is ExperimentOutcome for item in outcomes
        ):
            raise TypeError("outcomes must be a non-empty tuple of ExperimentOutcome")
        if len({item.experiment_digest for item in outcomes}) != len(outcomes):
            raise ValueError("tombstone outcomes must not duplicate an experiment")
        if not all(item.closure_eligible for item in outcomes):
            raise ValueError("only settled verified negative outcomes can close a branch")
        coverage = tuple(
            sorted(
                {
                    partition
                    for outcome in outcomes
                    for partition in outcome.covered_partition_digests
                }
            )
        )
        return cls(
            tombstone_id=tombstone_id,
            scope_digest=scope_digest,
            hypothesis_digests=hypothesis_digests,
            covered_partition_digests=coverage,
            closure_receipt_digest=closure_receipt_digest,
            reopen_predicates=reopen_predicates,
        )

    @property
    def digest(self) -> str:
        return canonical_digest(self.canonical_body())

    def canonical_body(self) -> dict[str, object]:
        return {
            "closure_receipt_digest": self.closure_receipt_digest,
            "covered_partition_digests": self.covered_partition_digests,
            "hypothesis_digests": self.hypothesis_digests,
            "reopen_predicates": tuple(
                item.canonical_body() for item in self.reopen_predicates
            ),
            "scope_digest": self.scope_digest,
            "tombstone_id": self.tombstone_id,
        }

    def is_reopened(self, triggers: tuple[ReopenTrigger, ...]) -> bool:
        if type(triggers) is not tuple or not all(type(item) is ReopenTrigger for item in triggers):
            raise TypeError("reopen_triggers must be a tuple of ReopenTrigger")
        return any(
            predicate.matches(trigger)
            for predicate in self.reopen_predicates
            for trigger in triggers
        )

    def blocks(
        self,
        experiment: DiscriminatingExperiment,
        *,
        reopen_triggers: tuple[ReopenTrigger, ...],
    ) -> bool:
        """Block only an already-covered experiment in the exact closed branch."""

        if type(experiment) is not DiscriminatingExperiment:
            raise TypeError("experiment must be DiscriminatingExperiment")
        if self.is_reopened(reopen_triggers):
            return False
        return (
            self.scope_digest == experiment.scope_digest
            and self.hypothesis_digests == experiment.hypothesis_digests
            and set(experiment.predicted_partition_digests).issubset(
                self.covered_partition_digests
            )
        )


@dataclass(frozen=True, slots=True)
class SelectionScore:
    """Deterministic portfolio score for one greedy selection position."""

    uncovered_partition_digests: tuple[str, ...]
    total_partition_count: int
    hypothesis_count: int
    estimated_cost_units: int

    def __post_init__(self) -> None:
        object.__setattr__(
            self,
            "uncovered_partition_digests",
            _digest_set(self.uncovered_partition_digests, "uncovered_partition_digests"),
        )
        object.__setattr__(
            self,
            "total_partition_count",
            _positive_int(self.total_partition_count, "total_partition_count"),
        )
        object.__setattr__(
            self,
            "hypothesis_count",
            _positive_int(self.hypothesis_count, "hypothesis_count"),
        )
        object.__setattr__(
            self,
            "estimated_cost_units",
            _positive_int(self.estimated_cost_units, "estimated_cost_units"),
        )
        if len(self.uncovered_partition_digests) > self.total_partition_count:
            raise ValueError("uncovered partition count exceeds the experiment partition count")

    @property
    def uncovered_partition_count(self) -> int:
        return len(self.uncovered_partition_digests)

    def canonical_body(self) -> dict[str, object]:
        return {
            "estimated_cost_units": self.estimated_cost_units,
            "hypothesis_count": self.hypothesis_count,
            "total_partition_count": self.total_partition_count,
            "uncovered_partition_digests": self.uncovered_partition_digests,
        }


@dataclass(frozen=True, slots=True)
class SelectionDecision:
    experiment_digest: str
    disposition: SelectionDisposition
    duplicate_decision: DuplicateDecision
    equivalence_proof_digest: str | None = None
    tombstone_digest: str | None = None
    score: SelectionScore | None = None
    rank_ordinal: int | None = None

    def __post_init__(self) -> None:
        object.__setattr__(self, "experiment_digest", _digest(self.experiment_digest, "experiment_digest"))
        if type(self.disposition) is not SelectionDisposition:
            raise TypeError("disposition must be SelectionDisposition")
        if type(self.duplicate_decision) is not DuplicateDecision:
            raise TypeError("duplicate_decision must be DuplicateDecision")
        for name in ("equivalence_proof_digest", "tombstone_digest"):
            value = getattr(self, name)
            if value is not None:
                object.__setattr__(self, name, _digest(value, name))
        if self.score is not None and type(self.score) is not SelectionScore:
            raise TypeError("score must be SelectionScore or None")
        if self.rank_ordinal is not None:
            object.__setattr__(self, "rank_ordinal", _positive_int(self.rank_ordinal, "rank_ordinal"))

        if self.disposition is SelectionDisposition.ELIGIBLE:
            if self.score is None or self.rank_ordinal is None:
                raise ValueError("eligible decision requires a deterministic score and rank")
            if self.equivalence_proof_digest is not None or self.tombstone_digest is not None:
                raise ValueError("eligible decision cannot carry a suppression authority")
            if self.duplicate_decision is DuplicateDecision.PROVEN_EQUIVALENT:
                raise ValueError("proven equivalence cannot remain eligible")
        elif self.disposition is SelectionDisposition.SUPPRESSED_PROVEN_EQUIVALENT:
            if (
                self.duplicate_decision is not DuplicateDecision.PROVEN_EQUIVALENT
                or self.equivalence_proof_digest is None
                or self.score is not None
                or self.rank_ordinal is not None
                or self.tombstone_digest is not None
            ):
                raise ValueError("suppression requires a proof and no rank")
        elif self.disposition is SelectionDisposition.BLOCKED_TOMBSTONE:
            if (
                self.tombstone_digest is None
                or self.score is not None
                or self.rank_ordinal is not None
                or self.equivalence_proof_digest is not None
            ):
                raise ValueError("tombstone block requires only its tombstone authority")

    def canonical_body(self) -> dict[str, object]:
        return {
            "disposition": self.disposition.value,
            "duplicate_decision": self.duplicate_decision.value,
            "equivalence_proof_digest": self.equivalence_proof_digest,
            "experiment_digest": self.experiment_digest,
            "rank_ordinal": self.rank_ordinal,
            "score": None if self.score is None else self.score.canonical_body(),
            "tombstone_digest": self.tombstone_digest,
        }

    @property
    def digest(self) -> str:
        return canonical_digest(self.canonical_body())


@dataclass(frozen=True, slots=True)
class SelectionPlan:
    """Pure output; a later host must independently admit any selected entry."""

    decisions: tuple[SelectionDecision, ...]
    ranked_experiment_digests: tuple[str, ...]
    selector_version: str = H5_SELECTOR_VERSION

    def __post_init__(self) -> None:
        if type(self.decisions) is not tuple or not all(
            type(item) is SelectionDecision for item in self.decisions
        ):
            raise TypeError("decisions must be a tuple of SelectionDecision")
        if tuple(sorted(item.experiment_digest for item in self.decisions)) != tuple(
            item.experiment_digest for item in self.decisions
        ):
            raise ValueError("decisions must be canonicalized by experiment digest")
        if len({item.experiment_digest for item in self.decisions}) != len(self.decisions):
            raise ValueError("decisions must not duplicate an experiment")
        if type(self.ranked_experiment_digests) is not tuple:
            raise TypeError("ranked_experiment_digests must be a built-in tuple")
        ranked = tuple(
            _digest(item, f"ranked_experiment_digests[{index}]")
            for index, item in enumerate(self.ranked_experiment_digests)
        )
        if len(set(ranked)) != len(ranked):
            raise ValueError("ranked_experiment_digests must not contain duplicates")
        expected_ranked = tuple(
            item.experiment_digest
            for item in sorted(
                (
                    item
                    for item in self.decisions
                    if item.disposition is SelectionDisposition.ELIGIBLE
                ),
                key=lambda item: item.rank_ordinal,
            )
        )
        eligible_ordinals = tuple(
            item.rank_ordinal
            for item in sorted(
                (
                    item
                    for item in self.decisions
                    if item.disposition is SelectionDisposition.ELIGIBLE
                ),
                key=lambda item: item.rank_ordinal,
            )
        )
        if eligible_ordinals != tuple(range(1, len(eligible_ordinals) + 1)):
            raise ValueError("eligible decision ranks must be contiguous and unique")
        if ranked != expected_ranked:
            raise ValueError("ranked experiments must match the deterministic decision rank")
        object.__setattr__(self, "ranked_experiment_digests", ranked)
        if self.selector_version != H5_SELECTOR_VERSION:
            raise ValueError("unsupported H5 selector version")

    def decision_for(self, experiment: DiscriminatingExperiment) -> SelectionDecision:
        if type(experiment) is not DiscriminatingExperiment:
            raise TypeError("experiment must be DiscriminatingExperiment")
        for decision in self.decisions:
            if decision.experiment_digest == experiment.digest:
                return decision
        raise KeyError(experiment.digest)

    @property
    def ordered_eligible_digests(self) -> tuple[str, ...]:
        return tuple(
            item.experiment_digest
            for item in sorted(
                (
                    item
                    for item in self.decisions
                    if item.disposition is SelectionDisposition.ELIGIBLE
                ),
                key=lambda item: item.rank_ordinal,
            )
        )

    def canonical_body(self) -> dict[str, object]:
        return {
            "decisions": tuple(item.canonical_body() for item in self.decisions),
            "ranked_experiment_digests": self.ranked_experiment_digests,
            "selector_version": self.selector_version,
        }

    @property
    def digest(self) -> str:
        return canonical_digest(self.canonical_body())


@dataclass(frozen=True, slots=True)
class H5RecommendationRequestV1:
    """Immutable input to a recommendation-only H5 selector invocation.

    This request deliberately contains only typed epistemic inputs.  It carries no
    attempt identity, permit, budget reservation, effect handle, verifier handle,
    worker, store, or gate authority.  Canonicalizing the input collections makes
    the same decision snapshot replay identically regardless of caller order.
    """

    hypotheses: tuple[Hypothesis, ...]
    candidates: tuple[DiscriminatingExperiment, ...]
    prior_experiments: tuple[DiscriminatingExperiment, ...] = ()
    covered_partition_digests: tuple[str, ...] = ()
    equivalence_proofs: tuple[EquivalenceProof, ...] = ()
    duplicate_recommendations: tuple[PossibleDuplicateRecommendation, ...] = ()
    tombstones: tuple[DeadEndTombstone, ...] = ()
    reopen_triggers: tuple[ReopenTrigger, ...] = ()
    schema_version: str = H5_RECOMMENDATION_REQUEST_VERSION

    def __post_init__(self) -> None:
        object.__setattr__(
            self,
            "hypotheses",
            self._canonical_objects(self.hypotheses, Hypothesis, "hypotheses"),
        )
        object.__setattr__(
            self,
            "candidates",
            self._canonical_objects(
                self.candidates,
                DiscriminatingExperiment,
                "candidates",
            ),
        )
        object.__setattr__(
            self,
            "prior_experiments",
            self._canonical_objects(
                self.prior_experiments,
                DiscriminatingExperiment,
                "prior_experiments",
            ),
        )
        object.__setattr__(
            self,
            "covered_partition_digests",
            _digest_set(self.covered_partition_digests, "covered_partition_digests"),
        )
        object.__setattr__(
            self,
            "equivalence_proofs",
            self._canonical_objects(
                self.equivalence_proofs,
                EquivalenceProof,
                "equivalence_proofs",
            ),
        )
        object.__setattr__(
            self,
            "duplicate_recommendations",
            self._canonical_objects(
                self.duplicate_recommendations,
                PossibleDuplicateRecommendation,
                "duplicate_recommendations",
            ),
        )
        object.__setattr__(
            self,
            "tombstones",
            self._canonical_objects(
                self.tombstones,
                DeadEndTombstone,
                "tombstones",
            ),
        )
        object.__setattr__(
            self,
            "reopen_triggers",
            self._canonical_objects(
                self.reopen_triggers,
                ReopenTrigger,
                "reopen_triggers",
            ),
        )
        object.__setattr__(
            self,
            "schema_version",
            _identifier(self.schema_version, "schema_version"),
        )
        if self.schema_version != H5_RECOMMENDATION_REQUEST_VERSION:
            raise ValueError("unsupported H5 recommendation request version")

    @staticmethod
    def _canonical_objects(
        value: object,
        expected: type[object],
        name: str,
    ) -> tuple[object, ...]:
        if type(value) is not tuple or not all(type(item) is expected for item in value):
            raise TypeError(f"{name} must be a tuple of {expected.__name__}")
        by_digest: dict[str, object] = {}
        for item in value:
            digest = item.digest
            if digest in by_digest:
                raise ValueError(f"{name} must not contain duplicate digests")
            by_digest[digest] = item
        return tuple(by_digest[digest] for digest in sorted(by_digest))

    def canonical_body(self) -> dict[str, object]:
        return {
            "candidates": tuple(item.canonical_body() for item in self.candidates),
            "covered_partition_digests": self.covered_partition_digests,
            "duplicate_recommendations": tuple(
                item.canonical_body() for item in self.duplicate_recommendations
            ),
            "equivalence_proofs": tuple(
                item.canonical_body() for item in self.equivalence_proofs
            ),
            "hypotheses": tuple(item.canonical_body() for item in self.hypotheses),
            "prior_experiments": tuple(
                item.canonical_body() for item in self.prior_experiments
            ),
            "reopen_triggers": tuple(
                item.canonical_body() for item in self.reopen_triggers
            ),
            "schema_version": self.schema_version,
            "tombstones": tuple(item.canonical_body() for item in self.tombstones),
        }

    @property
    def digest(self) -> str:
        return canonical_digest(self.canonical_body())


class HypothesisSelector:
    """Stateless deterministic H5 portfolio selector.

    The selector intentionally accepts typed historical data as values rather
    than fetching it from a store.  This makes replay and paired testing a direct
    function of the same canonical inputs.
    """

    @staticmethod
    def rank(
        *,
        hypotheses: tuple[Hypothesis, ...],
        candidates: tuple[DiscriminatingExperiment, ...],
        prior_experiments: tuple[DiscriminatingExperiment, ...] = (),
        covered_partition_digests: tuple[str, ...] = (),
        equivalence_proofs: tuple[EquivalenceProof, ...] = (),
        duplicate_recommendations: tuple[PossibleDuplicateRecommendation, ...] = (),
        tombstones: tuple[DeadEndTombstone, ...] = (),
        reopen_triggers: tuple[ReopenTrigger, ...] = (),
        research_fixture: bool = False,
    ) -> SelectionPlan:
        """Rank candidates without granting them execution authority.

        Digest-shaped proof/tombstone objects are useful for offline structural
        fixtures, but are not receipt-resolved evidence.  A raw caller must opt
        into that fixture-only behavior explicitly; the runtime seam never does.
        """

        if type(research_fixture) is not bool:
            raise TypeError("research_fixture must be an exact boolean")
        HypothesisSelector._validate_tuple(hypotheses, Hypothesis, "hypotheses")
        HypothesisSelector._validate_tuple(candidates, DiscriminatingExperiment, "candidates")
        HypothesisSelector._validate_tuple(
            prior_experiments,
            DiscriminatingExperiment,
            "prior_experiments",
        )
        HypothesisSelector._validate_tuple(
            equivalence_proofs,
            EquivalenceProof,
            "equivalence_proofs",
        )
        HypothesisSelector._validate_tuple(
            duplicate_recommendations,
            PossibleDuplicateRecommendation,
            "duplicate_recommendations",
        )
        HypothesisSelector._validate_tuple(tombstones, DeadEndTombstone, "tombstones")
        HypothesisSelector._validate_tuple(reopen_triggers, ReopenTrigger, "reopen_triggers")
        if (equivalence_proofs or tombstones) and not research_fixture:
            raise ValueError(
                "H5 selector refuses unresolved suppression outside a research fixture"
            )
        covered = set(_digest_set(covered_partition_digests, "covered_partition_digests"))

        hypothesis_by_digest = HypothesisSelector._unique_by_digest(hypotheses, "hypotheses")
        candidate_by_digest = HypothesisSelector._unique_by_digest(candidates, "candidates")
        prior_by_digest = HypothesisSelector._unique_by_digest(
            prior_experiments,
            "prior_experiments",
        )
        for candidate in candidates:
            HypothesisSelector._validate_candidate(candidate, hypothesis_by_digest)

        preliminary: dict[str, SelectionDecision] = {}
        eligible: list[DiscriminatingExperiment] = []
        for candidate in sorted(candidates, key=lambda item: item.digest):
            proof = HypothesisSelector._matching_proof(
                candidate,
                candidate_by_digest=candidate_by_digest,
                prior_by_digest=prior_by_digest,
                proofs=equivalence_proofs,
            )
            if proof is not None:
                preliminary[candidate.digest] = SelectionDecision(
                    experiment_digest=candidate.digest,
                    disposition=SelectionDisposition.SUPPRESSED_PROVEN_EQUIVALENT,
                    duplicate_decision=DuplicateDecision.PROVEN_EQUIVALENT,
                    equivalence_proof_digest=proof.digest,
                )
                continue

            blocking_tombstone = HypothesisSelector._blocking_tombstone(
                candidate,
                tombstones=tombstones,
                reopen_triggers=reopen_triggers,
            )
            if blocking_tombstone is not None:
                preliminary[candidate.digest] = SelectionDecision(
                    experiment_digest=candidate.digest,
                    disposition=SelectionDisposition.BLOCKED_TOMBSTONE,
                    duplicate_decision=DuplicateDecision.NOT_PROVEN,
                    tombstone_digest=blocking_tombstone.digest,
                )
                continue

            duplicate = (
                DuplicateDecision.POSSIBLE_DUPLICATE
                if HypothesisSelector._has_possible_duplicate(
                    candidate,
                    prior_experiments=prior_experiments,
                    recommendations=duplicate_recommendations,
                )
                else DuplicateDecision.NOT_PROVEN
            )
            preliminary[candidate.digest] = SelectionDecision(
                experiment_digest=candidate.digest,
                disposition=SelectionDisposition.ELIGIBLE,
                duplicate_decision=duplicate,
                score=SelectionScore(
                    uncovered_partition_digests=(),
                    total_partition_count=len(candidate.predicted_partition_digests),
                    hypothesis_count=len(candidate.hypothesis_digests),
                    estimated_cost_units=candidate.estimated_cost_units,
                ),
                rank_ordinal=1,
            )
            eligible.append(candidate)

        # Greedy selection maximizes coverage in the *portfolio*, not a model's
        # unverified self-reported information-gain.  A candidate remains eligible
        # even after it contributes no new partition; it simply ranks later.
        ranked: list[SelectionDecision] = []
        remaining = list(eligible)
        projected_coverage = set(covered)
        ordinal = 1
        while remaining:
            def candidate_key(experiment: DiscriminatingExperiment) -> tuple[int, int, int, int, str]:
                uncovered_count = len(
                    set(experiment.predicted_partition_digests) - projected_coverage
                )
                return (
                    -uncovered_count,
                    -len(experiment.predicted_partition_digests),
                    -len(experiment.hypothesis_digests),
                    experiment.estimated_cost_units,
                    experiment.digest,
                )

            selected = min(remaining, key=candidate_key)
            uncovered = tuple(
                sorted(set(selected.predicted_partition_digests) - projected_coverage)
            )
            earlier = preliminary[selected.digest]
            decision = SelectionDecision(
                experiment_digest=selected.digest,
                disposition=SelectionDisposition.ELIGIBLE,
                duplicate_decision=earlier.duplicate_decision,
                score=SelectionScore(
                    uncovered_partition_digests=uncovered,
                    total_partition_count=len(selected.predicted_partition_digests),
                    hypothesis_count=len(selected.hypothesis_digests),
                    estimated_cost_units=selected.estimated_cost_units,
                ),
                rank_ordinal=ordinal,
            )
            preliminary[selected.digest] = decision
            ranked.append(decision)
            projected_coverage.update(selected.predicted_partition_digests)
            remaining.remove(selected)
            ordinal += 1

        decisions = tuple(sorted(preliminary.values(), key=lambda item: item.experiment_digest))
        return SelectionPlan(
            decisions=decisions,
            ranked_experiment_digests=tuple(item.experiment_digest for item in ranked),
        )

    @staticmethod
    def recommend(
        request: H5RecommendationRequestV1,
        *,
        research_fixture: bool = False,
    ) -> SelectionPlan:
        """Return a pure plan from one immutable recommendation request."""

        if type(request) is not H5RecommendationRequestV1:
            raise TypeError("request must be H5RecommendationRequestV1")
        return HypothesisSelector.rank(
            hypotheses=request.hypotheses,
            candidates=request.candidates,
            prior_experiments=request.prior_experiments,
            covered_partition_digests=request.covered_partition_digests,
            equivalence_proofs=request.equivalence_proofs,
            duplicate_recommendations=request.duplicate_recommendations,
            tombstones=request.tombstones,
            reopen_triggers=request.reopen_triggers,
            research_fixture=research_fixture,
        )

    @staticmethod
    def _validate_tuple(value: object, expected: type[object], name: str) -> None:
        if type(value) is not tuple or not all(type(item) is expected for item in value):
            raise TypeError(f"{name} must be a tuple of {expected.__name__}")

    @staticmethod
    def _unique_by_digest(
        values: tuple[object, ...],
        name: str,
    ) -> dict[str, object]:
        result: dict[str, object] = {}
        for value in values:
            digest = value.digest  # All supported input values expose a canonical digest.
            if digest in result:
                raise ValueError(f"{name} must not register the same digest twice")
            result[digest] = value
        return result

    @staticmethod
    def _validate_candidate(
        candidate: DiscriminatingExperiment,
        hypotheses: dict[str, object],
    ) -> None:
        for prediction in candidate.predictions:
            hypothesis = hypotheses.get(prediction.hypothesis_digest)
            if hypothesis is None:
                raise ValueError("candidate references an unregistered hypothesis")
            if hypothesis.scope_digest != candidate.scope_digest:
                raise ValueError("candidate and hypotheses must share an exact scope")
            if (
                prediction.outcome_partition_digest
                not in hypothesis.prediction_partition_digests
            ):
                raise ValueError("candidate prediction is outside its hypothesis partition set")

    @staticmethod
    def _matching_proof(
        candidate: DiscriminatingExperiment,
        *,
        candidate_by_digest: dict[str, object],
        prior_by_digest: dict[str, object],
        proofs: tuple[EquivalenceProof, ...],
    ) -> EquivalenceProof | None:
        matches: list[EquivalenceProof] = []
        for proof in proofs:
            if proof.candidate_experiment_digest != candidate.digest:
                continue
            bound_candidate = candidate_by_digest.get(proof.candidate_experiment_digest)
            prior = prior_by_digest.get(proof.prior_experiment_digest)
            if bound_candidate is None or prior is None:
                continue
            if (
                bound_candidate.semantic_signature.digest != proof.candidate_signature_digest
                or prior.semantic_signature.digest != proof.prior_signature_digest
                or prior.scope_digest != bound_candidate.scope_digest
                or prior.context_packet_digest != bound_candidate.context_packet_digest
                or prior.hypothesis_digests != bound_candidate.hypothesis_digests
            ):
                continue
            if proof.relation is EquivalenceRelation.EXACT_EQUAL:
                if proof.prior_signature_digest != proof.candidate_signature_digest:
                    continue
            elif proof.relation is EquivalenceRelation.PRIOR_COVERS_CANDIDATE:
                if proof.coverage_proof_receipt_digest is None:
                    continue
            else:  # pragma: no cover - Enum construction is guarded above.
                continue
            matches.append(proof)
        return min(matches, key=lambda item: item.digest) if matches else None

    @staticmethod
    def _has_possible_duplicate(
        candidate: DiscriminatingExperiment,
        *,
        prior_experiments: tuple[DiscriminatingExperiment, ...],
        recommendations: tuple[PossibleDuplicateRecommendation, ...],
    ) -> bool:
        prior_signatures = {item.semantic_signature.digest for item in prior_experiments}
        return any(
            recommendation.candidate_signature_digest == candidate.semantic_signature.digest
            and recommendation.prior_signature_digest in prior_signatures
            for recommendation in recommendations
        )

    @staticmethod
    def _blocking_tombstone(
        candidate: DiscriminatingExperiment,
        *,
        tombstones: tuple[DeadEndTombstone, ...],
        reopen_triggers: tuple[ReopenTrigger, ...],
    ) -> DeadEndTombstone | None:
        matches = tuple(
            tombstone
            for tombstone in tombstones
            if tombstone.blocks(candidate, reopen_triggers=reopen_triggers)
        )
        return min(matches, key=lambda item: item.digest) if matches else None
