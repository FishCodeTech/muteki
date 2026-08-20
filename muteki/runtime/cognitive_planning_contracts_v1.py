"""Neutral DTOs and deterministic helpers shared by cognitive planners.

These contracts carry caller-supplied model and scalar planning inputs.  They
provide shape, replay, and deterministic-ranking semantics only; they grant no
execution, accounting, budget, progress, or acceptance authority.
"""

from __future__ import annotations

from dataclasses import dataclass
from enum import Enum

from muteki.epistemic.contracts import canonical_digest
from muteki.runtime.hypothesis import DiscriminatingExperiment


COGNITIVE_MASS_TOTAL_UNITS = 10_000
CAUSAL_PROGRAM_KEY_SCHEMA_ID = "muteki.causal-program-key.v1"


type CausalProgramKeyV1 = tuple[
    str,
    str,
    str,
    str,
    str,
    str,
    str,
    str,
    str,
    str,
    str,
    str,
    tuple[str, ...],
    str,
    tuple[str, ...],
]


def _digest(value: object, name: str) -> str:
    if (
        type(value) is not str
        or len(value) != 64
        or any(character not in "0123456789abcdef" for character in value)
    ):
        raise ValueError(f"{name} must be a lowercase sha256 digest")
    return value


def _identifier(value: object, name: str) -> str:
    if type(value) is not str or not value or value != value.strip():
        raise ValueError(f"{name} must be a non-empty canonical string")
    if (
        len(value) > 160
        or not value[0].isalnum()
        or any(
            character
            not in "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789_.:-"
            for character in value
        )
    ):
        raise ValueError(f"{name} must be a bounded canonical identifier")
    return value


def _positive_int(value: object, name: str) -> int:
    if type(value) is not int or value <= 0:
        raise ValueError(f"{name} must be a positive exact integer")
    return value


def _non_negative_int(value: object, name: str) -> int:
    if type(value) is not int or value < 0:
        raise ValueError(f"{name} must be a non-negative exact integer")
    return value


class CognitiveAssignmentPurpose(str, Enum):
    DISCRIMINATION = "discrimination"
    MODEL_SINGLETON_CONFIRMATION = "model_singleton_confirmation"


@dataclass(frozen=True, slots=True)
class HypothesisMassV1:
    hypothesis_digest: str
    weight_units: int

    def __post_init__(self) -> None:
        object.__setattr__(
            self,
            "hypothesis_digest",
            _digest(self.hypothesis_digest, "hypothesis_digest"),
        )
        object.__setattr__(
            self,
            "weight_units",
            _non_negative_int(self.weight_units, "weight_units"),
        )

    def canonical_body(self) -> dict[str, object]:
        return {
            "hypothesis_digest": self.hypothesis_digest,
            "weight_units": self.weight_units,
        }


@dataclass(frozen=True, slots=True)
class SuppliedCostEstimateV1:
    """Caller-supplied scalar estimate; never settled usage or budget authority."""

    experiment_digest: str
    cost_units: int

    def __post_init__(self) -> None:
        object.__setattr__(
            self,
            "experiment_digest",
            _digest(self.experiment_digest, "experiment_digest"),
        )
        object.__setattr__(
            self,
            "cost_units",
            _positive_int(self.cost_units, "cost_units"),
        )

    def canonical_body(self) -> dict[str, object]:
        return {
            "cost_units": self.cost_units,
            "experiment_digest": self.experiment_digest,
        }


@dataclass(frozen=True, slots=True)
class CognitiveAssignmentV1:
    experiment_digest: str
    direction_fingerprint: str
    profile_id: str | None
    new_distinction_mass: int
    coverage_adjusted_distinction_mass: int
    supplied_cost_estimate_units: int
    utility_ppm: int
    reason_codes: tuple[str, ...]
    purpose: CognitiveAssignmentPurpose = CognitiveAssignmentPurpose.DISCRIMINATION

    def __post_init__(self) -> None:
        object.__setattr__(
            self,
            "experiment_digest",
            _digest(self.experiment_digest, "experiment_digest"),
        )
        object.__setattr__(
            self,
            "direction_fingerprint",
            _digest(self.direction_fingerprint, "direction_fingerprint"),
        )
        if self.profile_id is not None:
            object.__setattr__(
                self,
                "profile_id",
                _identifier(self.profile_id, "profile_id"),
            )
        object.__setattr__(
            self,
            "new_distinction_mass",
            _positive_int(self.new_distinction_mass, "new_distinction_mass"),
        )
        object.__setattr__(
            self,
            "coverage_adjusted_distinction_mass",
            _positive_int(
                self.coverage_adjusted_distinction_mass,
                "coverage_adjusted_distinction_mass",
            ),
        )
        if self.coverage_adjusted_distinction_mass > self.new_distinction_mass:
            raise ValueError(
                "coverage-adjusted mass cannot exceed raw distinction mass"
            )
        object.__setattr__(
            self,
            "supplied_cost_estimate_units",
            _positive_int(
                self.supplied_cost_estimate_units,
                "supplied_cost_estimate_units",
            ),
        )
        object.__setattr__(
            self,
            "utility_ppm",
            _non_negative_int(self.utility_ppm, "utility_ppm"),
        )
        if type(self.reason_codes) is not tuple or not self.reason_codes:
            raise ValueError("reason_codes must be a non-empty tuple")
        canonical_reasons = tuple(
            _identifier(item, f"reason_codes[{index}]")
            for index, item in enumerate(self.reason_codes)
        )
        if len(set(canonical_reasons)) != len(canonical_reasons):
            raise ValueError("reason_codes must be unique")
        object.__setattr__(self, "reason_codes", canonical_reasons)
        if type(self.purpose) is not CognitiveAssignmentPurpose:
            raise TypeError("purpose must be CognitiveAssignmentPurpose")

    def canonical_body(self) -> dict[str, object]:
        return {
            "coverage_adjusted_distinction_mass": self.coverage_adjusted_distinction_mass,
            "direction_fingerprint": self.direction_fingerprint,
            "supplied_cost_estimate_units": self.supplied_cost_estimate_units,
            "experiment_digest": self.experiment_digest,
            "new_distinction_mass": self.new_distinction_mass,
            "profile_id": self.profile_id,
            "purpose": self.purpose.value,
            "reason_codes": self.reason_codes,
            "utility_ppm": self.utility_ppm,
        }

    @property
    def digest(self) -> str:
        return canonical_digest(self.canonical_body())


def _normalize_hypothesis_masses(
    masses: tuple[HypothesisMassV1, ...],
) -> tuple[HypothesisMassV1, ...]:
    """Apply a label-invariant uniform prior.

    Integer mass is a ranking lattice, not probability authority.  Any remainder is
    deliberately left unallocated: assigning it to hypotheses in digest order lets an
    ID-only relabel move weight and change the selected causal program whenever the
    hypothesis count does not divide the lattice size.
    """

    if any(item.weight_units <= 0 for item in masses):
        raise ValueError(
            "every registered hypothesis starts live under uniform prior v1"
        )
    if len(masses) > COGNITIVE_MASS_TOTAL_UNITS:
        raise ValueError("too many live hypotheses for the canonical mass lattice")
    base = COGNITIVE_MASS_TOTAL_UNITS // len(masses)
    return tuple(
        HypothesisMassV1(item.hypothesis_digest, base)
        for item in masses
    )


def _distinction_pairs(
    experiment: DiscriminatingExperiment,
    weights: dict[str, int],
) -> tuple[tuple[str, str], ...]:
    predictions = {
        item.hypothesis_digest: item.outcome_partition_digest
        for item in experiment.predictions
        if weights.get(item.hypothesis_digest, 0) > 0
    }
    hypotheses = sorted(predictions)
    return tuple(
        (left, right)
        for index, left in enumerate(hypotheses)
        for right in hypotheses[index + 1 :]
        if predictions[left] != predictions[right]
    )


def _pair_mass(
    pairs: tuple[tuple[str, str], ...],
    weights: dict[str, int],
) -> int:
    return sum(weights[left] * weights[right] for left, right in pairs)


def _direction_fingerprint(pairs: tuple[tuple[str, str], ...]) -> str:
    return canonical_digest({"hypothesis_distinction_pairs": pairs})


def _typed_program_fingerprint(experiment: DiscriminatingExperiment) -> str:
    """Retry-suppression identity for one typed causal operation.

    A new ContextPacket may carry a fresher view, but rebinding the same action,
    predictions, scope, and execution signature to that packet is not by itself a
    distinct experiment after UNKNOWN.  Context identity remains bound by the
    assignment and admission receipts; it is deliberately excluded here so a caller
    cannot turn a retry into a new program by recompiling the prompt.  A future reopen
    policy may authorize that operation only with its own verified predicate.
    """

    return canonical_digest(
        {
            "hypothesis_digests": experiment.hypothesis_digests,
            "predictions": tuple(
                item.canonical_body() for item in experiment.predictions
            ),
            "scope_digest": experiment.scope_digest,
            "semantic_signature": experiment.semantic_signature.canonical_body(),
        }
    )


def causal_program_key_v1(
    experiment: DiscriminatingExperiment,
) -> CausalProgramKeyV1:
    """Return the public, relabeling-invariant causal execution program.

    The key deliberately keeps fields that change what would be executed and
    the multiset of predicates that would be measured.  It deliberately drops
    experiment/version labels, hypothesis identities, and predicted outcome
    partition labels or mappings.  Those dropped proposer-controlled names may
    describe a score, but must never break an otherwise exact planning tie.

    This tuple is a structural comparison key, not an equivalence proof and not
    an authority receipt.  In particular, equality cannot suppress work.
    """

    if type(experiment) is not DiscriminatingExperiment:
        raise TypeError("experiment must be DiscriminatingExperiment")
    signature = experiment.semantic_signature
    return (
        CAUSAL_PROGRAM_KEY_SCHEMA_ID,
        experiment.context_packet_digest,
        experiment.scope_digest,
        signature.action_class.value,
        signature.tool_capability_digest,
        signature.resource_digest,
        signature.parameter_region_digest,
        signature.precondition_set_digest,
        signature.read_set_digest,
        signature.world_epoch_digest,
        signature.tool_policy_digest,
        signature.model_policy_digest,
        signature.stop_condition_digests,
        signature.effect_class.value,
        tuple(sorted(item.predicate_digest for item in experiment.predictions)),
    )


__all__ = [
    "CAUSAL_PROGRAM_KEY_SCHEMA_ID",
    "COGNITIVE_MASS_TOTAL_UNITS",
    "CausalProgramKeyV1",
    "CognitiveAssignmentPurpose",
    "CognitiveAssignmentV1",
    "HypothesisMassV1",
    "SuppliedCostEstimateV1",
    "causal_program_key_v1",
]
