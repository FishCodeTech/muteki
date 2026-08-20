"""Runtime-owned default-off bridge from verified facts to the next experiment.

The older cognitive-cycle prototype can consume only caller-supplied V7 shadow
records.  This module instead starts at a narrower boundary: a whole
``COGNITIVE_VERIFICATION_RESOLVED`` event that has already been resolved through
the lossless receipt reader.  Verification internals (engine registry, launcher
witness and checker accounting) remain upstream of the resolver and are opaque
here.  A digest-shaped certificate claim by itself is therefore never enough.

The bridge is deliberately a pure recommendation layer.  It cannot write the
epistemic store, admit or dispatch work, settle budget, retry UNKNOWN, or change
the hardcoded acceptance gate.  Its only state transition is a replayable belief
projection over resolver-owned facts.  Candidate scores remain model-conditioned
ranking heuristics because the hypothesis partitions and future costs are typed
proposals rather than verified information.
"""

from __future__ import annotations

from collections.abc import Mapping
from dataclasses import dataclass, field
from enum import Enum
from fractions import Fraction
from typing import Any

from muteki.epistemic.contracts import canonical_digest, canonical_json_bytes
from muteki.runtime.cognitive_planning_contracts_v1 import (
    COGNITIVE_MASS_TOTAL_UNITS,
    CausalProgramKeyV1,
    CognitiveAssignmentV1,
    CognitiveAssignmentPurpose,
    HypothesisMassV1,
    SuppliedCostEstimateV1,
    _direction_fingerprint,
    _distinction_pairs,
    _normalize_hypothesis_masses,
    _pair_mass,
    _typed_program_fingerprint,
    causal_program_key_v1,
)
from muteki.runtime.cognitive_observation_label_v1 import (
    ActiveHypothesisPredictionV1,
    CheckerDispositionV1,
    CognitiveInformationLabelV1,
    CognitiveObservationLabelRequestV1,
    CognitiveObservationLabelV1,
    ExperimentWorldSemanticKeyV1,
    ObservationExecutionStatusV1,
    PriorSupportedObservationV1,
    ReproductionDispositionV1,
    reduce_cognitive_observation_label_v1,
)
from muteki.runtime.hypothesis import (
    DiscriminatingExperiment,
    H5RecommendationRequestV1,
    HypothesisSelector,
    SelectionDisposition,
)
from muteki.runtime.cognitive_verification_resolution_v1 import (
    ACCEPTED_SET_CHANGE,
    AUTOMATIC_REDISPATCH_PERMITTED,
    CANONICAL_VERIFICATION_RESOLUTION_SCHEMA_ID,
    CANONICAL_VERIFICATION_RESOLVER_ACTOR,
    CANONICAL_VERIFICATION_RESOLVER_VERSION,
    COGNITIVE_VERIFICATION_RESOLVED as COGNITIVE_VERIFICATION_RESOLVED,
    PRODUCTION_ENABLED,
    PROVENANCE_GATE_ACCEPTED_SET,
    ResolvedCognitiveFactStatusV1,
    ResolvedCognitiveFactV1,
    canonical_verification_resolution_payload_v1,
)


CANONICAL_COGNITIVE_CYCLE_SCHEMA_ID = "muteki.canonical-cognitive-cycle.v1"
AUTHORITY_EFFECT_NONE = "NONE"
DISTINCTION_SCORE_AUTHORITY = "RECOMMENDATION_ONLY_MODEL_CONDITIONED"


type _ScoredCandidateV1 = tuple[
    Fraction,
    int,
    int,
    DiscriminatingExperiment,
    tuple[tuple[str, str], ...],
]


def _digest(value: object, name: str) -> str:
    if (
        type(value) is not str
        or len(value) != 64
        or any(character not in "0123456789abcdef" for character in value)
    ):
        raise ValueError(f"{name} must be a lowercase sha256 digest")
    return value


def _non_negative_int(value: object, name: str) -> int:
    if type(value) is not int or value < 0:
        raise ValueError(f"{name} must be a non-negative exact integer")
    return value


class CanonicalCognitiveCycleModeV1(str, Enum):
    EXPERIMENT = "experiment"
    CONTINUE_DISTINCT_EXPERIMENT = "continue_distinct_experiment"
    TIE_REQUIRES_DIVERSITY = "tie_requires_diversity"
    HOLD_RECONCILIATION = "hold_reconciliation"
    EXPAND_HYPOTHESES = "expand_hypotheses"
    MODEL_RESOLUTION = "model_resolution"
    NO_AFFORDABLE_EXPERIMENT = "no_affordable_experiment"


@dataclass(frozen=True, slots=True)
class CanonicalCognitiveBeliefV1:
    masses: tuple[HypothesisMassV1, ...]
    label_digests: tuple[str, ...]
    learning_label_digests: tuple[str, ...]
    non_learning_label_digests: tuple[str, ...]
    held_fact_digests: tuple[str, ...]
    unexpected_fact_digests: tuple[str, ...]
    attempted_program_fingerprints: tuple[str, ...]
    model_conditioned_distinction_units: int

    @property
    def active_hypothesis_digests(self) -> tuple[str, ...]:
        return tuple(
            item.hypothesis_digest for item in self.masses if item.weight_units > 0
        )

    def canonical_body(self) -> dict[str, object]:
        return {
            "attempted_program_fingerprints": self.attempted_program_fingerprints,
            "distinction_score_authority": DISTINCTION_SCORE_AUTHORITY,
            "held_fact_digests": self.held_fact_digests,
            "label_digests": self.label_digests,
            "learning_label_digests": self.learning_label_digests,
            "masses": tuple(item.canonical_body() for item in self.masses),
            "model_conditioned_distinction_units": (
                self.model_conditioned_distinction_units
            ),
            "non_learning_label_digests": self.non_learning_label_digests,
            "unexpected_fact_digests": self.unexpected_fact_digests,
        }

    @property
    def digest(self) -> str:
        return canonical_digest(self.canonical_body())


@dataclass(frozen=True, slots=True)
class CanonicalCognitiveCycleRequestV1:
    """Frozen planner inputs with deliberately narrow provenance semantics.

    Candidate programs, cost estimates, and remaining cost units are typed
    caller-supplied inputs, not verified capability, accounting, or budget
    facts.  A store-backed admission must separately reconstruct
    ``resolved_facts`` from its canonical receipt prefix.
    """

    h5_request: H5RecommendationRequestV1
    initial_masses: tuple[HypothesisMassV1, ...]
    resolved_facts: tuple[ResolvedCognitiveFactV1, ...]
    cost_estimates: tuple[SuppliedCostEstimateV1, ...]
    remaining_cost_units: int

    def __post_init__(self) -> None:
        if type(self.h5_request) is not H5RecommendationRequestV1:
            raise TypeError("h5_request must be H5RecommendationRequestV1")
        if self.h5_request.equivalence_proofs or self.h5_request.tombstones:
            raise ValueError(
                "canonical cycle refuses suppression inputs without a receipt resolver"
            )
        if type(self.initial_masses) is not tuple or not all(
            type(item) is HypothesisMassV1 for item in self.initial_masses
        ):
            raise TypeError("initial_masses must be a typed tuple")
        masses = tuple(
            sorted(self.initial_masses, key=lambda item: item.hypothesis_digest)
        )
        if len({item.hypothesis_digest for item in masses}) != len(masses):
            raise ValueError("initial_masses contains duplicate hypotheses")
        registered_hypotheses = {item.digest for item in self.h5_request.hypotheses}
        if {item.hypothesis_digest for item in masses} != registered_hypotheses:
            raise ValueError("initial masses must bind every registered hypothesis")
        if len(masses) < 2 or len(masses) > COGNITIVE_MASS_TOTAL_UNITS:
            raise ValueError("canonical cycle requires a bounded competing model set")
        object.__setattr__(
            self,
            "initial_masses",
            _normalize_hypothesis_masses(masses),
        )

        if type(self.resolved_facts) is not tuple or not all(
            type(item) is ResolvedCognitiveFactV1 for item in self.resolved_facts
        ):
            raise TypeError("resolved_facts must be a typed tuple")
        facts = tuple(
            sorted(
                self.resolved_facts,
                key=lambda item: (item.seq, item.event.digest),
            )
        )
        if len({item.event.digest for item in facts}) != len(facts):
            raise ValueError("resolved_facts contains a duplicate resolution event")
        if len({item.seq for item in facts}) != len(facts):
            raise ValueError("resolved facts cannot occupy the same canonical sequence")
        if facts and len({item.prefix.run_id for item in facts}) != 1:
            raise ValueError("resolved facts cannot splice distinct runs")
        if facts:
            longest = max(facts, key=lambda item: item.prefix.cutoff_seq).prefix
            for item in facts:
                prefix = item.prefix
                expected_events = longest.events[: prefix.cutoff_seq]
                if (
                    prefix.cutoff_seq > longest.cutoff_seq
                    or prefix.events != expected_events
                    or (
                        prefix.cutoff_seq
                        and prefix.head_event_digest != expected_events[-1].event_digest
                    )
                ):
                    raise ValueError(
                        "resolved facts do not share one contiguous canonical history"
                    )
        experiments = {
            item.digest: item
            for item in (
                *self.h5_request.prior_experiments,
                *self.h5_request.candidates,
            )
        }
        if any(
            item.source_experiment.digest not in experiments
            or canonical_json_bytes(
                experiments[item.source_experiment.digest].canonical_body()
            )
            != canonical_json_bytes(item.source_experiment.canonical_body())
            for item in facts
        ):
            raise ValueError("resolved fact references an unregistered experiment")
        object.__setattr__(self, "resolved_facts", facts)

        if type(self.cost_estimates) is not tuple or not all(
            type(item) is SuppliedCostEstimateV1 for item in self.cost_estimates
        ):
            raise TypeError("cost_estimates must be a typed tuple")
        costs = tuple(
            sorted(self.cost_estimates, key=lambda item: item.experiment_digest)
        )
        if len({item.experiment_digest for item in costs}) != len(costs):
            raise ValueError("cost_estimates contains duplicate experiments")
        candidate_digests = {item.digest for item in self.h5_request.candidates}
        if {item.experiment_digest for item in costs} != candidate_digests:
            raise ValueError("cost estimates must bind every candidate")
        object.__setattr__(self, "cost_estimates", costs)
        object.__setattr__(
            self,
            "remaining_cost_units",
            _non_negative_int(self.remaining_cost_units, "remaining_cost_units"),
        )


@dataclass(frozen=True, slots=True)
class CanonicalCognitiveCyclePlanV1:
    mode: CanonicalCognitiveCycleModeV1
    belief: CanonicalCognitiveBeliefV1
    labels: tuple[CognitiveObservationLabelV1, ...]
    next_assignment: CognitiveAssignmentV1 | None
    reason_codes: tuple[str, ...]
    authority_effect: str = field(default=AUTHORITY_EFFECT_NONE, init=False)
    production_enabled: bool = field(default=PRODUCTION_ENABLED, init=False)
    provenance_gate_accepted_set: str = field(
        default=PROVENANCE_GATE_ACCEPTED_SET,
        init=False,
    )
    automatic_redispatch_permitted: bool = field(
        default=AUTOMATIC_REDISPATCH_PERMITTED,
        init=False,
    )

    def __post_init__(self) -> None:
        if type(self.mode) is not CanonicalCognitiveCycleModeV1:
            raise TypeError("mode must be CanonicalCognitiveCycleModeV1")
        if type(self.belief) is not CanonicalCognitiveBeliefV1:
            raise TypeError("belief must be CanonicalCognitiveBeliefV1")
        if type(self.labels) is not tuple or not all(
            type(item) is CognitiveObservationLabelV1 for item in self.labels
        ):
            raise TypeError("labels must be a typed tuple")
        if self.next_assignment is not None and (
            type(self.next_assignment) is not CognitiveAssignmentV1
        ):
            raise TypeError("next_assignment must be CognitiveAssignmentV1 or None")
        assignment_modes = {
            CanonicalCognitiveCycleModeV1.EXPERIMENT,
            CanonicalCognitiveCycleModeV1.CONTINUE_DISTINCT_EXPERIMENT,
        }
        if (self.mode in assignment_modes) != (self.next_assignment is not None):
            raise ValueError(
                "only experiment or distinct-continuation mode may recommend "
                "one next assignment"
            )
        if (
            type(self.reason_codes) is not tuple
            or not self.reason_codes
            or any(type(item) is not str or not item for item in self.reason_codes)
        ):
            raise ValueError("reason_codes must be a non-empty tuple")

    @property
    def canonical_commands(self) -> tuple[()]:
        return ()

    def canonical_body(self) -> dict[str, Any]:
        return {
            "authority_effect": self.authority_effect,
            "automatic_redispatch_permitted": self.automatic_redispatch_permitted,
            "belief": self.belief.canonical_body(),
            "labels": tuple(item.canonical_body() for item in self.labels),
            "mode": self.mode.value,
            "next_assignment": (
                None
                if self.next_assignment is None
                else self.next_assignment.canonical_body()
            ),
            "production_enabled": self.production_enabled,
            "provenance_gate_accepted_set": self.provenance_gate_accepted_set,
            "reason_codes": self.reason_codes,
            "schema_id": CANONICAL_COGNITIVE_CYCLE_SCHEMA_ID,
        }

    @property
    def digest(self) -> str:
        return canonical_digest(self.canonical_body())


def _ambiguity_mass(weights: Mapping[str, int]) -> int:
    active = [value for value in weights.values() if value > 0]
    return sum(
        left * right
        for index, left in enumerate(active)
        for right in active[index + 1 :]
    )


def _label_request(
    *,
    fact: ResolvedCognitiveFactV1,
    weights: Mapping[str, int],
    priors: tuple[PriorSupportedObservationV1, ...],
) -> CognitiveObservationLabelRequestV1:
    predictions = tuple(
        ActiveHypothesisPredictionV1(
            hypothesis_digest=item.hypothesis_digest,
            predicted_partition_digest=item.outcome_partition_digest,
        )
        for item in fact.source_experiment.predictions
        if weights.get(item.hypothesis_digest, 0) > 0
    )
    status = fact.status
    if status is ResolvedCognitiveFactStatusV1.VERIFIED_SUPPORTED:
        execution = ObservationExecutionStatusV1.SUCCEEDED
        checker = CheckerDispositionV1.SUPPORTED
        reproduction = ReproductionDispositionV1.REPRODUCED
    elif status is ResolvedCognitiveFactStatusV1.VERIFIED_DISAGREEMENT:
        execution = ObservationExecutionStatusV1.SUCCEEDED
        checker = CheckerDispositionV1.DISAGREEMENT
        reproduction = ReproductionDispositionV1.DISAGREEMENT
    elif status is ResolvedCognitiveFactStatusV1.HELD_UNKNOWN:
        execution = ObservationExecutionStatusV1.UNKNOWN
        checker = CheckerDispositionV1.UNKNOWN
        reproduction = ReproductionDispositionV1.UNKNOWN
    else:
        execution = ObservationExecutionStatusV1.SUCCEEDED
        checker = CheckerDispositionV1.INCOMPLETE
        reproduction = ReproductionDispositionV1.INCOMPLETE
    return CognitiveObservationLabelRequestV1(
        semantic_key=ExperimentWorldSemanticKeyV1(
            experiment_semantic_digest=fact.causal_kernel_digest,
            world_epoch_digest=(
                fact.source_experiment.semantic_signature.world_epoch_digest
            ),
        ),
        active_predictions=predictions,
        prior_supported_observations=priors,
        execution_status=execution,
        observed_partition_digest=fact.observed_partition_digest,
        checker_disposition=checker,
        reproduction_disposition=reproduction,
        verification_occurrence_digest=fact.verification_occurrence_digest,
        # A canonical bounded-domain authority does not exist yet.  Resolver
        # support must never be silently upgraded into negative coverage.
        bounded_negative_coverage=None,
    )


def _reduce_belief(
    request: CanonicalCognitiveCycleRequestV1,
) -> tuple[
    CanonicalCognitiveBeliefV1,
    tuple[CognitiveObservationLabelV1, ...],
]:
    weights = {
        item.hypothesis_digest: item.weight_units for item in request.initial_masses
    }
    labels: list[CognitiveObservationLabelV1] = []
    learning: list[str] = []
    non_learning: list[str] = []
    held: list[str] = []
    unexpected: list[str] = []
    distinction = 0
    prior_by_fact: dict[tuple[str, str, str], PriorSupportedObservationV1] = {}
    attempted_programs = tuple(
        sorted(
            {
                _typed_program_fingerprint(item.source_experiment)
                for item in request.resolved_facts
            }
        )
    )

    for fact in request.resolved_facts:
        prior_tuple = tuple(prior_by_fact[key] for key in sorted(prior_by_fact))
        label = reduce_cognitive_observation_label_v1(
            _label_request(fact=fact, weights=weights, priors=prior_tuple)
        )
        labels.append(label)
        if label.label is CognitiveInformationLabelV1.INCONCLUSIVE:
            held.append(fact.digest)
            non_learning.append(label.digest)
            continue
        if not fact.learning_eligible:
            # Defensive invariant: a non-supported resolver status can never
            # pass the label reducer as learning evidence.
            held.append(fact.digest)
            non_learning.append(label.digest)
            continue
        assert label.observed_partition_digest is not None
        semantic_fact = (
            label.semantic_key.experiment_semantic_digest,
            label.semantic_key.world_epoch_digest,
            label.observed_partition_digest,
        )
        prior_by_fact.setdefault(
            semantic_fact,
            PriorSupportedObservationV1(
                semantic_key=label.semantic_key,
                observed_partition_digest=label.observed_partition_digest,
                verification_occurrence_digest=label.verification_occurrence_digest,
            ),
        )
        if (
            label.label is CognitiveInformationLabelV1.NEW_INFORMATION
            and label.eliminated_hypothesis_digests
        ):
            before = _ambiguity_mass(weights)
            for hypothesis_digest in label.eliminated_hypothesis_digests:
                weights[hypothesis_digest] = 0
            distinction += max(0, before - _ambiguity_mass(weights))
            learning.append(label.digest)
        else:
            non_learning.append(label.digest)
        if (
            label.label is CognitiveInformationLabelV1.NEW_INFORMATION
            and not label.eliminated_hypothesis_digests
        ):
            unexpected.append(fact.digest)

    belief = CanonicalCognitiveBeliefV1(
        masses=tuple(HypothesisMassV1(key, weights[key]) for key in sorted(weights)),
        label_digests=tuple(item.digest for item in labels),
        learning_label_digests=tuple(learning),
        non_learning_label_digests=tuple(non_learning),
        held_fact_digests=tuple(held),
        unexpected_fact_digests=tuple(unexpected),
        attempted_program_fingerprints=attempted_programs,
        model_conditioned_distinction_units=distinction,
    )
    return belief, tuple(labels)


def plan_canonical_cognitive_cycle_v1(
    request: CanonicalCognitiveCycleRequestV1,
) -> CanonicalCognitiveCyclePlanV1:
    """Replay verified facts and recommend at most one next experiment."""

    if type(request) is not CanonicalCognitiveCycleRequestV1:
        raise TypeError("request must be CanonicalCognitiveCycleRequestV1")
    belief, labels = _reduce_belief(request)
    held_fact_digests = set(belief.held_fact_digests)
    held_statuses = {
        fact.status
        for fact in request.resolved_facts
        if fact.digest in held_fact_digests
    }
    can_continue_after_unknown = (
        bool(held_fact_digests)
        and held_statuses == {ResolvedCognitiveFactStatusV1.HELD_UNKNOWN}
        and not belief.unexpected_fact_digests
        and len(belief.active_hypothesis_digests) >= 2
    )
    if belief.held_fact_digests and not can_continue_after_unknown:
        return CanonicalCognitiveCyclePlanV1(
            mode=CanonicalCognitiveCycleModeV1.HOLD_RECONCILIATION,
            belief=belief,
            labels=labels,
            next_assignment=None,
            reason_codes=(
                "unresolved_fact_cannot_trigger_learning_or_model_resolution",
                "automatic_redispatch_forbidden",
            ),
        )
    if belief.unexpected_fact_digests:
        return CanonicalCognitiveCyclePlanV1(
            mode=CanonicalCognitiveCycleModeV1.EXPAND_HYPOTHESES,
            belief=belief,
            labels=labels,
            next_assignment=None,
            reason_codes=("verified_open_world_observation_requires_model_expansion",),
        )
    if len(belief.active_hypothesis_digests) <= 1:
        return CanonicalCognitiveCyclePlanV1(
            mode=CanonicalCognitiveCycleModeV1.MODEL_RESOLUTION,
            belief=belief,
            labels=labels,
            next_assignment=None,
            reason_codes=("at_most_one_named_hypothesis_remains",),
        )

    h5_plan = HypothesisSelector.recommend(request.h5_request)
    eligible = {
        item.experiment_digest
        for item in h5_plan.decisions
        if item.disposition is SelectionDisposition.ELIGIBLE
    }
    weights = {item.hypothesis_digest: item.weight_units for item in belief.masses}
    costs = {item.experiment_digest: item.cost_units for item in request.cost_estimates}
    attempted = set(belief.attempted_program_fingerprints)
    scored: list[_ScoredCandidateV1] = []
    has_unattempted = False
    has_affordable = False
    for experiment in request.h5_request.candidates:
        if (
            experiment.digest not in eligible
            or _typed_program_fingerprint(experiment) in attempted
        ):
            continue
        has_unattempted = True
        cost = costs[experiment.digest]
        if cost > request.remaining_cost_units:
            continue
        has_affordable = True
        pairs = _distinction_pairs(experiment, weights)
        mass = _pair_mass(pairs, weights)
        if mass <= 0:
            continue
        scored.append(
            (
                Fraction(mass, cost),
                mass,
                cost,
                experiment,
                pairs,
            )
        )

    if not scored:
        if can_continue_after_unknown:
            return CanonicalCognitiveCyclePlanV1(
                mode=CanonicalCognitiveCycleModeV1.HOLD_RECONCILIATION,
                belief=belief,
                labels=labels,
                next_assignment=None,
                reason_codes=(
                    "held_unknown_preserved_without_learning",
                    "no_affordable_positive_distinction_typed_program_remains",
                    "automatic_redispatch_forbidden",
                ),
            )
        if has_affordable:
            mode = CanonicalCognitiveCycleModeV1.EXPAND_HYPOTHESES
            reason = "no_unattempted_candidate_separates_current_active_hypotheses"
        elif has_unattempted:
            mode = CanonicalCognitiveCycleModeV1.NO_AFFORDABLE_EXPERIMENT
            reason = "no_unattempted_candidate_remains_within_complete_budget"
        else:
            mode = CanonicalCognitiveCycleModeV1.EXPAND_HYPOTHESES
            reason = "all_candidates_already_attempted_or_proven_ineligible"
        return CanonicalCognitiveCyclePlanV1(
            mode=mode,
            belief=belief,
            labels=labels,
            next_assignment=None,
            reason_codes=(reason,),
        )

    primary_score = max((item[0], item[1], -item[2]) for item in scored)
    top = tuple(
        item for item in scored if (item[0], item[1], -item[2]) == primary_score
    )
    public_tie_resolved = len(top) > 1
    exact_alias_representation_resolved = False
    if public_tie_resolved:
        # The old final key was the whole experiment digest.  That let an
        # experiment ID, a hypothesis ID, or an arbitrary outcome-partition
        # label change the selected action without changing the public score.
        # Compare only the causal execution program.  A remaining equality with
        # different prediction programs is intentionally non-dispatching.  Exact
        # typed aliases may choose a deterministic receipt representation because
        # every such representation executes the same causal operation.
        causal_groups: dict[CausalProgramKeyV1, list[_ScoredCandidateV1]] = {}
        for item in top:
            causal_groups.setdefault(causal_program_key_v1(item[3]), []).append(item)
        selected_causal_key = min(causal_groups)
        selected_group = causal_groups[selected_causal_key]
        if len(selected_group) > 1:
            exact_programs = {
                _typed_program_fingerprint(item[3]) for item in selected_group
            }
            if len(exact_programs) != 1:
                return CanonicalCognitiveCyclePlanV1(
                    mode=CanonicalCognitiveCycleModeV1.TIE_REQUIRES_DIVERSITY,
                    belief=belief,
                    labels=labels,
                    next_assignment=None,
                    reason_codes=(
                        "exact_public_primary_score_tie",
                        "causal_program_indistinguishable_representations",
                        "recommendation_only_no_dispatch_authority",
                    ),
                )
            # Exact typed aliases execute the same causal operation.  Choose one
            # concrete receipt representation deterministically so duplicate labels
            # cannot deadlock useful work.  The chosen experiment digest may change
            # when aliases are renamed, but the selected causal program cannot.
            selected = min(selected_group, key=lambda item: item[3].digest)
            exact_alias_representation_resolved = True
        else:
            selected = selected_group[0]
    else:
        selected = top[0]
    utility, mass, cost, experiment, pairs = selected
    assignment = CognitiveAssignmentV1(
        experiment_digest=experiment.digest,
        direction_fingerprint=_direction_fingerprint(pairs),
        profile_id=None,
        new_distinction_mass=mass,
        coverage_adjusted_distinction_mass=mass,
        supplied_cost_estimate_units=cost,
        utility_ppm=(utility.numerator * 1_000_000) // utility.denominator,
        reason_codes=(
            "current_active_hypothesis_distinction_per_supplied_cost",
            *(
                (
                    (
                        "canonical_representation_resolves_exact_typed_alias_tie"
                        if exact_alias_representation_resolved
                        else "public_causal_program_order_resolves_exact_primary_tie"
                    ),
                )
                if public_tie_resolved
                else ()
            ),
            "attempted_typed_programs_excluded_including_unknown",
            "recommendation_only_no_dispatch_authority",
        ),
        purpose=CognitiveAssignmentPurpose.DISCRIMINATION,
    )
    return CanonicalCognitiveCyclePlanV1(
        mode=(
            CanonicalCognitiveCycleModeV1.CONTINUE_DISTINCT_EXPERIMENT
            if can_continue_after_unknown
            else CanonicalCognitiveCycleModeV1.EXPERIMENT
        ),
        belief=belief,
        labels=labels,
        next_assignment=assignment,
        reason_codes=(
            (
                "held_unknown_preserved_choose_exact_typed_program_nonidentity"
                if can_continue_after_unknown
                else (
                    "choose_canonical_exact_typed_alias_representation"
                    if exact_alias_representation_resolved
                    else (
                        "choose_public_causal_program_order_after_exact_primary_tie"
                        if public_tie_resolved
                        else "choose_highest_current_distinction_per_supplied_cost"
                    )
                )
            ),
            *(
                ("automatic_redispatch_forbidden",)
                if can_continue_after_unknown
                else ()
            ),
        ),
    )


__all__ = [
    "ACCEPTED_SET_CHANGE",
    "AUTHORITY_EFFECT_NONE",
    "AUTOMATIC_REDISPATCH_PERMITTED",
    "CANONICAL_COGNITIVE_CYCLE_SCHEMA_ID",
    "CANONICAL_VERIFICATION_RESOLUTION_SCHEMA_ID",
    "CANONICAL_VERIFICATION_RESOLVER_ACTOR",
    "CANONICAL_VERIFICATION_RESOLVER_VERSION",
    "COGNITIVE_VERIFICATION_RESOLVED",
    "CanonicalCognitiveBeliefV1",
    "CanonicalCognitiveCycleModeV1",
    "CanonicalCognitiveCyclePlanV1",
    "CanonicalCognitiveCycleRequestV1",
    "PRODUCTION_ENABLED",
    "PROVENANCE_GATE_ACCEPTED_SET",
    "ResolvedCognitiveFactStatusV1",
    "ResolvedCognitiveFactV1",
    "canonical_verification_resolution_payload_v1",
    "plan_canonical_cognitive_cycle_v1",
]
