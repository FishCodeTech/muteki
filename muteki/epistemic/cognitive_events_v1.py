"""Store-bound contracts for the default-off cognitive evaluation lifecycle.

These contracts freeze recommendation inputs and bind them to the existing v2
evaluation admission/terminal commands.  They deliberately do not decide whether
an experiment result is true, independent, or learning-eligible.  In particular,
``COGNITIVE_EXECUTION_OBSERVED`` is only a structural runtime observation; a future
independent V7 authority must resolve verification from canonical receipts.
"""

from __future__ import annotations

from collections.abc import Mapping
from dataclasses import dataclass
from typing import Any

from muteki.epistemic.contracts import (
    canonical_digest,
    canonical_json_bytes,
    freeze_json,
)


COGNITIVE_EXPERIMENT_ASSIGNED = "COGNITIVE_EXPERIMENT_ASSIGNED"
COGNITIVE_EXECUTION_OBSERVED = "COGNITIVE_EXECUTION_OBSERVED"
COGNITIVE_VERIFICATION_RESOLVED = "COGNITIVE_VERIFICATION_RESOLVED"

COGNITIVE_BINDING_ACTOR = "cognitive-evaluation-binding-v1-authority"
COGNITIVE_VERIFIER_ACTOR = "cognitive-independent-verifier-v1-authority"

COGNITIVE_ASSIGNMENT_SCHEMA_ID = "muteki.cognitive-experiment-assigned.v1"
COGNITIVE_RUNTIME_CONTEXT_ASSIGNMENT_SCHEMA_ID = (
    "muteki.cognitive-experiment-assigned.runtime-context.v1"
)
COGNITIVE_RUNTIME_EXECUTABLE_ASSIGNMENT_SCHEMA_ID = (
    "muteki.cognitive-experiment-assigned.runtime-context-executable.v1"
)
COGNITIVE_RUNTIME_REPRODUCTION_ASSIGNMENT_SCHEMA_ID = (
    "muteki.cognitive-experiment-assigned.runtime-context-reproduction.v1"
)
COGNITIVE_EXECUTION_SCHEMA_ID = "muteki.cognitive-execution-observed.v1"
COGNITIVE_RUNTIME_EXECUTION_SCHEMA_ID = (
    "muteki.cognitive-execution-observed.runtime-context.v1"
)
COGNITIVE_VERIFICATION_SCHEMA_ID = "muteki.cognitive-verification-resolved.v1"

COGNITIVE_MODE = "shadow"
COGNITIVE_RUNTIME_CONTEXT_MODE = "runtime_context_shadow"
COGNITIVE_RUNTIME_REPRODUCTION_MODE = "runtime_context_reproduction_shadow"
COGNITIVE_ACCEPTED_SET_CHANGE = False
RUNTIME_H5_SELECTOR_VERSION = "muteki.runtime-h5-selector.v3"
RUNTIME_H5_REQUEST_VERSION = "muteki.runtime-h5-recommendation-request.v2"

# These are admission/resource ceilings, not accounting claims.  They keep the
# default-off v1 binding bounded before semantic replay, while measured host CPU,
# serialization work, and CAS storage remain prerequisites for any capability
# promotion study.
COGNITIVE_H5_MAX_ASSIGNMENT_CANONICAL_BYTES_V1 = 16_384
COGNITIVE_H5_MAX_EXPERIMENT_CANONICAL_BYTES_V1 = 32_768
COGNITIVE_H5_MAX_REQUEST_CANONICAL_BYTES_V1 = 524_288
COGNITIVE_H5_MAX_PLAN_CANONICAL_BYTES_V1 = 262_144
COGNITIVE_H5_MAX_BINDING_CANONICAL_BYTES_V1 = 786_432
COGNITIVE_H5_MAX_HYPOTHESES_V1 = 64
COGNITIVE_H5_MAX_CANDIDATES_V1 = 64
COGNITIVE_H5_MAX_PRIOR_EXPERIMENTS_V1 = 128
COGNITIVE_H5_MAX_PREDICTIONS_PER_EXPERIMENT_V1 = 64

_ASSIGNMENT_BODY_FIELDS = frozenset(
    {
        "coverage_adjusted_distinction_mass",
        "direction_fingerprint",
        "experiment_digest",
        "new_distinction_mass",
        "profile_id",
        "purpose",
        "reason_codes",
        "supplied_cost_estimate_units",
        "utility_ppm",
    }
)
_EXPERIMENT_BODY_FIELDS = frozenset(
    {
        "context_packet_digest",
        "estimated_cost_units",
        "experiment_id",
        "hypothesis_digests",
        "predictions",
        "scope_digest",
        "semantic_signature",
        "version",
    }
)
_ASSIGNMENT_EVENT_FIELDS = frozenset(
    {
        "accepted_set_change",
        "assignment_binding_digest",
        "assignment_body",
        "assignment_digest",
        "attempt_digest",
        "attempt_id",
        "attempt_role_binding_digest",
        "base_event_id",
        "base_payload_digest",
        "decision_cutoff_seq",
        "decision_head_event_digest",
        "decision_prefix_digest",
        "evaluation_sidecar_event_id",
        "evaluation_sidecar_payload_digest",
        "experiment_body",
        "experiment_digest",
        "h5_request_body",
        "h5_request_digest",
        "h5_selection_plan_body",
        "h5_selection_plan_digest",
        "mode",
        "permit_digest",
        "permit_id",
        "schema_id",
        "scope_digest",
        "world_epoch_digest",
    }
)
_RUNTIME_CONTEXT_ASSIGNMENT_EVENT_FIELDS = frozenset(
    {
        "accepted_set_change",
        "assignment_body",
        "assignment_digest",
        "attempt_digest",
        "attempt_id",
        "base_event_id",
        "base_payload_digest",
        "context_packet_binding_body",
        "context_packet_binding_digest",
        "decision_cutoff_seq",
        "decision_head_event_digest",
        "decision_prefix_digest",
        "experiment_body",
        "experiment_digest",
        "h5_request_body",
        "h5_request_digest",
        "h5_selection_plan_body",
        "h5_selection_plan_digest",
        "mode",
        "permit_digest",
        "permit_id",
        "planner_policy_selection_proven",
        "schema_id",
        "scope_digest",
        "world_epoch_digest",
    }
)
_RUNTIME_EXECUTABLE_ASSIGNMENT_EVENT_FIELDS = frozenset(
    {
        *_RUNTIME_CONTEXT_ASSIGNMENT_EVENT_FIELDS,
        "executable_experiment_binding_body",
        "executable_experiment_binding_digest",
    }
)
_RUNTIME_REPRODUCTION_ASSIGNMENT_EVENT_FIELDS = frozenset(
    {
        *_RUNTIME_EXECUTABLE_ASSIGNMENT_EVENT_FIELDS,
        "assignment_role",
        "automatic_redispatch_permitted",
        "learning_eligible",
        "max_reproduction_count",
        "reproduction_kernel_digest",
        "required_reproducer_profile_digest",
        "source_assignment_event_digest",
        "source_assignment_event_receipt_digest",
        "source_claim_digest",
        "source_executable_spec_digest",
        "source_observation_event_digest",
        "source_observation_event_receipt_digest",
        "source_reproduction_kernel_digest",
        "verification_policy_version",
        "withheld_source_digest_set",
        "withheld_source_digest_set_digest",
    }
)
_CONTEXT_PACKET_BINDING_FIELDS = frozenset(
    {
        "accepted_set_change",
        "compilation_event_receipt_digest",
        "compiler_receipt_digest",
        "compiler_version",
        "cutoff_seq",
        "decision_id",
        "decision_receipt_digest",
        "feature_state_digest",
        "manifest_digest",
        "packet_digest",
        "target_attempt_id",
    }
)
_EXECUTION_EVENT_FIELDS = frozenset(
    {
        "accepted_set_change",
        "assignment_event_digest",
        "attempt_digest",
        "attempt_id",
        "base_event_id",
        "base_payload_digest",
        "budget_event_id",
        "budget_event_kind",
        "budget_payload_digest",
        "epistemic_classification",
        "evaluation_terminal_event_id",
        "evaluation_terminal_payload_digest",
        "execution_outcome",
        "experiment_execution_claim",
        "experiment_digest",
        "experiment_materialization_status",
        "learning_eligible",
        "mode",
        "observed_partition_digest",
        "permit_digest",
        "permit_id",
        "schema_id",
        "scope_digest",
        "usage_status",
        "world_epoch_digest",
    }
)


def is_sha256(value: object) -> bool:
    return (
        type(value) is str
        and len(value) == 64
        and all(character in "0123456789abcdef" for character in value)
    )


def _digest(value: object, name: str) -> str:
    if not is_sha256(value):
        raise ValueError(f"{name} must be a lowercase sha256 digest")
    return str(value)


def _identifier(value: object, name: str) -> str:
    if type(value) is not str or not value or value != value.strip():
        raise ValueError(f"{name} must be exact non-empty text")
    return value


def _positive_int(value: object, name: str) -> int:
    if type(value) is not int or value <= 0:
        raise ValueError(f"{name} must be a positive exact integer")
    return value


def _nonnegative_int(value: object, name: str) -> int:
    if type(value) is not int or value < 0:
        raise ValueError(f"{name} must be a non-negative exact integer")
    return value


def _digest_tuple(
    value: object, name: str, *, required: bool = False
) -> tuple[str, ...]:
    if type(value) is not tuple:
        raise TypeError(f"{name} must be a canonical sequence")
    result = tuple(
        _digest(item, f"{name}[{index}]") for index, item in enumerate(value)
    )
    if required and not result:
        raise ValueError(f"{name} must not be empty")
    if len(result) != len(set(result)) or result != tuple(sorted(result)):
        raise ValueError(f"{name} must be unique and canonicalized")
    return result


def _plain_mapping(value: Mapping[str, Any], name: str) -> Mapping[str, Any]:
    if not isinstance(value, Mapping) or not value:
        raise ValueError(f"{name} must be a non-empty canonical mapping")
    frozen = freeze_json(value, path=f"$.{name}")
    if not isinstance(frozen, Mapping):
        raise TypeError(f"{name} must be a mapping")
    return frozen


def _canonical_size_at_most(value: object, name: str, maximum: int) -> None:
    byte_count = len(canonical_json_bytes(value))
    if byte_count > maximum:
        raise ValueError(
            f"{name} exceeds {maximum} canonical bytes (observed {byte_count})"
        )


def _validate_binding_canonical_size(
    *,
    assignment: Mapping[str, Any],
    experiment: Mapping[str, Any],
    h5_request: Mapping[str, Any],
    h5_plan: Mapping[str, Any],
) -> None:
    _canonical_size_at_most(
        {
            "assignment_body": assignment,
            "experiment_body": experiment,
            "h5_request_body": h5_request,
            "h5_selection_plan_body": h5_plan,
        },
        "cognitive binding",
        COGNITIVE_H5_MAX_BINDING_CANONICAL_BYTES_V1,
    )


def _validate_assignment_body(body: Mapping[str, Any]) -> None:
    _canonical_size_at_most(
        body,
        "assignment_body",
        COGNITIVE_H5_MAX_ASSIGNMENT_CANONICAL_BYTES_V1,
    )
    if set(body) != _ASSIGNMENT_BODY_FIELDS:
        raise ValueError("assignment_body shape is not CognitiveAssignmentV1")
    _digest(body.get("experiment_digest"), "assignment experiment_digest")
    _digest(body.get("direction_fingerprint"), "assignment direction_fingerprint")
    new_mass = _positive_int(body.get("new_distinction_mass"), "new distinction mass")
    covered_mass = _positive_int(
        body.get("coverage_adjusted_distinction_mass"),
        "coverage-adjusted distinction mass",
    )
    if covered_mass > new_mass:
        raise ValueError("coverage-adjusted mass exceeds raw distinction mass")
    _positive_int(body.get("supplied_cost_estimate_units"), "supplied cost estimate")
    _nonnegative_int(body.get("utility_ppm"), "utility_ppm")
    if body.get("purpose") not in {
        "discrimination",
        "model_singleton_confirmation",
    }:
        raise ValueError("assignment purpose is unsupported")
    profile_id = body.get("profile_id")
    if profile_id is not None:
        _identifier(profile_id, "assignment profile_id")
    reasons = body.get("reason_codes")
    if type(reasons) is not tuple or not reasons:
        raise ValueError("assignment reason_codes must be non-empty")
    normalized = tuple(
        _identifier(reason, f"reason_codes[{index}]")
        for index, reason in enumerate(reasons)
    )
    if len(normalized) != len(set(normalized)):
        raise ValueError("assignment reason_codes contain duplicates")


def _validate_experiment_body(body: Mapping[str, Any]) -> None:
    _canonical_size_at_most(
        body,
        "experiment_body",
        COGNITIVE_H5_MAX_EXPERIMENT_CANONICAL_BYTES_V1,
    )
    if set(body) != _EXPERIMENT_BODY_FIELDS:
        raise ValueError("experiment_body shape is not DiscriminatingExperiment")
    _identifier(body.get("experiment_id"), "experiment_id")
    _positive_int(body.get("version"), "experiment version")
    _positive_int(body.get("estimated_cost_units"), "estimated_cost_units")
    _digest(body.get("context_packet_digest"), "context_packet_digest")
    _digest(body.get("scope_digest"), "experiment scope_digest")
    hypotheses = _digest_tuple(
        body.get("hypothesis_digests"),
        "hypothesis_digests",
        required=True,
    )
    if len(hypotheses) < 2:
        raise ValueError("a discriminating experiment needs two hypotheses")
    predictions = body.get("predictions")
    if type(predictions) is not tuple:
        raise ValueError("experiment predictions must be canonical")
    if len(predictions) > COGNITIVE_H5_MAX_PREDICTIONS_PER_EXPERIMENT_V1:
        raise ValueError(
            "experiment predictions exceed the v1 count ceiling "
            f"({COGNITIVE_H5_MAX_PREDICTIONS_PER_EXPERIMENT_V1})"
        )
    if len(predictions) != len(hypotheses):
        raise ValueError("experiment predictions must cover every hypothesis")
    normalized_predictions: list[tuple[str, str]] = []
    for index, raw in enumerate(predictions):
        if not isinstance(raw, Mapping) or set(raw) != {
            "hypothesis_digest",
            "outcome_partition_digest",
            "predicate_digest",
        }:
            raise ValueError(f"predictions[{index}] shape diverged")
        hypothesis = _digest(
            raw.get("hypothesis_digest"), f"predictions[{index}].hypothesis"
        )
        partition = _digest(
            raw.get("outcome_partition_digest"),
            f"predictions[{index}].partition",
        )
        _digest(raw.get("predicate_digest"), f"predictions[{index}].predicate")
        normalized_predictions.append((hypothesis, partition))
    if tuple(item[0] for item in normalized_predictions) != hypotheses:
        raise ValueError("experiment prediction order/membership diverged")
    partitions = tuple(sorted({item[1] for item in normalized_predictions}))
    if len(partitions) < 2:
        raise ValueError("experiment predictions do not discriminate")
    signature = body.get("semantic_signature")
    signature_fields = {
        "action_class",
        "canonicalizer_version",
        "effect_class",
        "model_policy_digest",
        "parameter_region_digest",
        "prediction_partition_digests",
        "precondition_set_digest",
        "read_set_digest",
        "resource_digest",
        "stop_condition_digests",
        "tool_capability_digest",
        "tool_policy_digest",
        "world_epoch_digest",
    }
    if not isinstance(signature, Mapping) or set(signature) != signature_fields:
        raise ValueError("experiment semantic signature shape diverged")
    if (
        signature.get("action_class") != "discriminating_experiment"
        or signature.get("canonicalizer_version") != RUNTIME_H5_SELECTOR_VERSION
        or signature.get("effect_class")
        not in {
            "pure_cognitive",
            "local_isolated",
            "idempotent",
            "compensatable",
            "non_idempotent",
            "unknown",
        }
    ):
        raise ValueError("experiment semantic signature policy diverged")
    for name in (
        "model_policy_digest",
        "parameter_region_digest",
        "precondition_set_digest",
        "read_set_digest",
        "resource_digest",
        "tool_capability_digest",
        "tool_policy_digest",
        "world_epoch_digest",
    ):
        _digest(signature.get(name), f"semantic_signature.{name}")
    signature_partitions = _digest_tuple(
        signature.get("prediction_partition_digests"),
        "semantic signature partitions",
        required=True,
    )
    _digest_tuple(
        signature.get("stop_condition_digests"),
        "semantic signature stop conditions",
        required=True,
    )
    if signature_partitions != partitions:
        raise ValueError("semantic signature prediction partitions diverged")


def _validate_h5_request_body(body: Mapping[str, Any]) -> None:
    if (
        set(body)
        != {
            "candidates",
            "covered_partition_digests",
            "duplicate_recommendations",
            "equivalence_proofs",
            "hypotheses",
            "prior_experiments",
            "reopen_triggers",
            "schema_version",
            "tombstones",
        }
        or body.get("schema_version") != RUNTIME_H5_REQUEST_VERSION
    ):
        raise ValueError("h5_request_body shape/version diverged")
    candidates = body.get("candidates")
    if type(candidates) is not tuple or not candidates:
        raise ValueError("H5 request candidates must be non-empty")
    if len(candidates) > COGNITIVE_H5_MAX_CANDIDATES_V1:
        raise ValueError(
            "H5 request candidates exceed the v1 count ceiling "
            f"({COGNITIVE_H5_MAX_CANDIDATES_V1})"
        )
    hypotheses = body.get("hypotheses")
    if type(hypotheses) is not tuple or len(hypotheses) < 2:
        raise ValueError("H5 hypotheses must be a canonical non-empty sequence")
    if len(hypotheses) > COGNITIVE_H5_MAX_HYPOTHESES_V1:
        raise ValueError(
            "H5 hypotheses exceed the v1 count ceiling "
            f"({COGNITIVE_H5_MAX_HYPOTHESES_V1})"
        )
    prior = body.get("prior_experiments")
    if type(prior) is not tuple:
        raise ValueError("H5 prior experiments must be canonical")
    if len(prior) > COGNITIVE_H5_MAX_PRIOR_EXPERIMENTS_V1:
        raise ValueError(
            "H5 prior experiments exceed the v1 count ceiling "
            f"({COGNITIVE_H5_MAX_PRIOR_EXPERIMENTS_V1})"
        )
    _canonical_size_at_most(
        body,
        "h5_request_body",
        COGNITIVE_H5_MAX_REQUEST_CANONICAL_BYTES_V1,
    )
    candidate_digests: list[str] = []
    for index, candidate in enumerate(candidates):
        if not isinstance(candidate, Mapping):
            raise ValueError(f"H5 candidate[{index}] is not a mapping")
        _validate_experiment_body(candidate)
        candidate_digests.append(canonical_digest(candidate))
    if candidate_digests != sorted(candidate_digests) or len(candidate_digests) != len(
        set(candidate_digests)
    ):
        raise ValueError("H5 request candidates are not canonicalized")
    hypothesis_digests: list[str] = []
    for index, hypothesis in enumerate(hypotheses):
        if not isinstance(hypothesis, Mapping) or set(hypothesis) != {
            "claim_digest",
            "hypothesis_id",
            "other_unknown_lane",
            "prediction_partition_digests",
            "scope_digest",
            "version",
        }:
            raise ValueError(f"H5 hypothesis[{index}] shape diverged")
        _digest(hypothesis.get("claim_digest"), "H5 hypothesis claim")
        _identifier(hypothesis.get("hypothesis_id"), "H5 hypothesis id")
        if hypothesis.get("other_unknown_lane") is not True:
            raise ValueError("H5 hypothesis lost the OTHER/UNKNOWN lane")
        _digest_tuple(
            hypothesis.get("prediction_partition_digests"),
            "H5 hypothesis partitions",
            required=True,
        )
        _digest(hypothesis.get("scope_digest"), "H5 hypothesis scope")
        _positive_int(hypothesis.get("version"), "H5 hypothesis version")
        hypothesis_digests.append(canonical_digest(hypothesis))
    if hypothesis_digests != sorted(hypothesis_digests) or len(
        hypothesis_digests
    ) != len(set(hypothesis_digests)):
        raise ValueError("H5 hypotheses are not canonicalized")
    hypothesis_by_digest = {
        canonical_digest(hypothesis): hypothesis for hypothesis in hypotheses
    }
    known_hypotheses = set(hypothesis_by_digest)
    for candidate in candidates:
        if not set(candidate["hypothesis_digests"]).issubset(known_hypotheses):
            raise ValueError("H5 candidate references an unknown hypothesis")
        for prediction in candidate["predictions"]:
            hypothesis = hypothesis_by_digest[prediction["hypothesis_digest"]]
            if hypothesis["scope_digest"] != candidate["scope_digest"]:
                raise ValueError("H5 candidate/hypothesis scope diverged")
            if (
                prediction["outcome_partition_digest"]
                not in hypothesis["prediction_partition_digests"]
            ):
                raise ValueError(
                    "H5 candidate prediction is outside its hypothesis partitions"
                )
    for item in prior:
        if not isinstance(item, Mapping):
            raise ValueError("H5 prior experiment is not a mapping")
        _validate_experiment_body(item)
    prior_digests = [canonical_digest(item) for item in prior]
    if prior_digests != sorted(prior_digests) or len(prior_digests) != len(
        set(prior_digests)
    ):
        raise ValueError("H5 prior experiments are not canonicalized")
    _digest_tuple(
        body.get("covered_partition_digests"),
        "H5 covered partitions",
    )

    def canonical_objects(
        value: object,
        name: str,
        validator: Any,
    ) -> None:
        if type(value) is not tuple:
            raise ValueError(f"{name} must be a canonical sequence")
        digests: list[str] = []
        for index, item in enumerate(value):
            if not isinstance(item, Mapping):
                raise ValueError(f"{name}[{index}] must be a mapping")
            validator(item, index)
            digests.append(canonical_digest(item))
        if digests != sorted(digests) or len(digests) != len(set(digests)):
            raise ValueError(f"{name} is not canonicalized")

    def equivalence(item: Mapping[str, Any], index: int) -> None:
        if set(item) != {
            "candidate_experiment_digest",
            "candidate_signature_digest",
            "coverage_proof_receipt_digest",
            "deterministic_checker_receipt_digest",
            "prior_experiment_digest",
            "prior_signature_digest",
            "relation",
        }:
            raise ValueError(f"H5 equivalence_proofs[{index}] shape diverged")
        for name in (
            "candidate_experiment_digest",
            "candidate_signature_digest",
            "deterministic_checker_receipt_digest",
            "prior_experiment_digest",
            "prior_signature_digest",
        ):
            _digest(item.get(name), f"H5 equivalence {name}")
        if item["candidate_experiment_digest"] == item["prior_experiment_digest"]:
            raise ValueError("H5 equivalence cannot self-relate")
        relation = item.get("relation")
        coverage = item.get("coverage_proof_receipt_digest")
        if relation not in {"exact_equal", "prior_covers_candidate"}:
            raise ValueError("H5 equivalence relation is unsupported")
        if coverage is not None:
            _digest(coverage, "H5 equivalence coverage receipt")
        if (
            relation == "exact_equal"
            and item["prior_signature_digest"] != item["candidate_signature_digest"]
        ):
            raise ValueError("H5 exact equivalence signatures diverged")
        if relation == "prior_covers_candidate" and coverage is None:
            raise ValueError("H5 coverage equivalence has no coverage receipt")

    def duplicate(item: Mapping[str, Any], index: int) -> None:
        if set(item) != {
            "candidate_signature_digest",
            "prior_signature_digest",
            "rationale_digest",
            "recommender_profile_digest",
        }:
            raise ValueError(f"H5 duplicate_recommendations[{index}] shape diverged")
        for name in item:
            _digest(item[name], f"H5 duplicate {name}")

    def trigger(item: Mapping[str, Any], index: int) -> None:
        if set(item) != {
            "evidence_receipt_digest",
            "kind",
            "observed_digest",
            "version",
        }:
            raise ValueError(f"H5 reopen_triggers[{index}] shape diverged")
        _digest(item.get("evidence_receipt_digest"), "H5 trigger receipt")
        _digest(item.get("observed_digest"), "H5 trigger observation")
        if item.get("kind") not in {
            "contradictory_evidence",
            "world_epoch_changed",
            "tool_policy_changed",
            "model_policy_changed",
            "capability_added",
            "parameter_region_enlarged",
            "verifier_invalidated",
            "schema_version_advanced",
            "repair_policy_advanced",
            "operator_override",
        }:
            raise ValueError("H5 reopen trigger kind is unsupported")
        _nonnegative_int(item.get("version"), "H5 reopen trigger version")

    def tombstone(item: Mapping[str, Any], index: int) -> None:
        if set(item) != {
            "closure_receipt_digest",
            "covered_partition_digests",
            "hypothesis_digests",
            "reopen_predicates",
            "scope_digest",
            "tombstone_id",
        }:
            raise ValueError(f"H5 tombstones[{index}] shape diverged")
        _digest(item.get("closure_receipt_digest"), "H5 tombstone closure")
        _digest_tuple(
            item.get("covered_partition_digests"),
            "H5 tombstone coverage",
            required=True,
        )
        _digest_tuple(
            item.get("hypothesis_digests"),
            "H5 tombstone hypotheses",
            required=True,
        )
        _digest(item.get("scope_digest"), "H5 tombstone scope")
        _identifier(item.get("tombstone_id"), "H5 tombstone id")
        predicates = item.get("reopen_predicates")
        if type(predicates) is not tuple:
            raise ValueError("H5 tombstone predicates must be canonical")
        for predicate_index, predicate in enumerate(predicates):
            if not isinstance(predicate, Mapping) or set(predicate) != {
                "baseline_digest",
                "kind",
                "minimum_version",
                "predicate_id",
            }:
                raise ValueError(
                    f"H5 tombstone predicate[{predicate_index}] shape diverged"
                )
            _digest(predicate.get("baseline_digest"), "H5 predicate baseline")
            _identifier(predicate.get("predicate_id"), "H5 predicate id")
            _nonnegative_int(predicate.get("minimum_version"), "H5 predicate version")
            if predicate.get("kind") not in {
                "contradictory_evidence",
                "world_epoch_changed",
                "tool_policy_changed",
                "model_policy_changed",
                "capability_added",
                "parameter_region_enlarged",
                "verifier_invalidated",
                "schema_version_advanced",
                "repair_policy_advanced",
                "operator_override",
            }:
                raise ValueError("H5 tombstone predicate kind is unsupported")

    canonical_objects(
        body.get("equivalence_proofs"), "H5 equivalence proofs", equivalence
    )
    canonical_objects(
        body.get("duplicate_recommendations"),
        "H5 duplicate recommendations",
        duplicate,
    )
    canonical_objects(body.get("reopen_triggers"), "H5 reopen triggers", trigger)
    canonical_objects(body.get("tombstones"), "H5 tombstones", tombstone)


def _validate_h5_plan_body(body: Mapping[str, Any]) -> None:
    if (
        set(body)
        != {
            "decisions",
            "ranked_experiment_digests",
            "selector_version",
        }
        or body.get("selector_version") != RUNTIME_H5_SELECTOR_VERSION
    ):
        raise ValueError("h5_selection_plan_body shape/version diverged")
    ranked = body.get("ranked_experiment_digests")
    decisions = body.get("decisions")
    if type(ranked) is not tuple or type(decisions) is not tuple:
        raise ValueError("H5 plan decisions/ranking must be canonical sequences")
    if (
        len(ranked) > COGNITIVE_H5_MAX_CANDIDATES_V1
        or len(decisions) > COGNITIVE_H5_MAX_CANDIDATES_V1
    ):
        raise ValueError(
            "H5 plan entries exceed the v1 candidate count ceiling "
            f"({COGNITIVE_H5_MAX_CANDIDATES_V1})"
        )
    _canonical_size_at_most(
        body,
        "h5_selection_plan_body",
        COGNITIVE_H5_MAX_PLAN_CANONICAL_BYTES_V1,
    )
    for index, digest in enumerate(ranked):
        _digest(digest, f"ranked_experiment_digests[{index}]")
    if len(ranked) != len(set(ranked)):
        raise ValueError("H5 ranked experiments contain duplicates")
    decision_digests: list[str] = []
    eligible: list[tuple[int, str]] = []
    for index, decision in enumerate(decisions):
        if not isinstance(decision, Mapping) or set(decision) != {
            "disposition",
            "duplicate_decision",
            "equivalence_proof_digest",
            "experiment_digest",
            "rank_ordinal",
            "score",
            "tombstone_digest",
        }:
            raise ValueError(f"H5 decision[{index}] shape diverged")
        digest = _digest(
            decision.get("experiment_digest"), f"H5 decision[{index}] digest"
        )
        decision_digests.append(digest)
        disposition = decision.get("disposition")
        duplicate_decision = decision.get("duplicate_decision")
        if duplicate_decision not in {
            "not_proven",
            "possible_duplicate",
            "proven_equivalent",
        }:
            raise ValueError("H5 duplicate decision is unsupported")
        if disposition == "eligible":
            rank = _positive_int(
                decision.get("rank_ordinal"), f"H5 decision[{index}] rank"
            )
            score = decision.get("score")
            if not isinstance(score, Mapping) or set(score) != {
                "estimated_cost_units",
                "hypothesis_count",
                "total_partition_count",
                "uncovered_partition_digests",
            }:
                raise ValueError("eligible H5 decision score diverged")
            _positive_int(score.get("estimated_cost_units"), "H5 score cost")
            _positive_int(score.get("hypothesis_count"), "H5 score hypotheses")
            _positive_int(score.get("total_partition_count"), "H5 score partitions")
            _digest_tuple(
                score.get("uncovered_partition_digests"),
                "H5 score uncovered partitions",
            )
            if (
                decision.get("equivalence_proof_digest") is not None
                or decision.get("tombstone_digest") is not None
                or decision.get("duplicate_decision") == "proven_equivalent"
            ):
                raise ValueError("eligible H5 decision claims suppression authority")
            eligible.append((rank, digest))
        elif disposition == "suppressed_proven_equivalent":
            if (
                duplicate_decision != "proven_equivalent"
                or decision.get("score") is not None
                or decision.get("rank_ordinal") is not None
                or decision.get("tombstone_digest") is not None
            ):
                raise ValueError("H5 equivalence suppression shape diverged")
            _digest(
                decision.get("equivalence_proof_digest"),
                "H5 equivalence suppression proof",
            )
        elif disposition == "blocked_tombstone":
            if (
                decision.get("score") is not None
                or decision.get("rank_ordinal") is not None
                or decision.get("equivalence_proof_digest") is not None
            ):
                raise ValueError("H5 tombstone decision shape diverged")
            _digest(
                decision.get("tombstone_digest"),
                "H5 tombstone decision receipt",
            )
        else:
            raise ValueError("H5 selection disposition is unsupported")
    if decision_digests != sorted(decision_digests) or len(decision_digests) != len(
        set(decision_digests)
    ):
        raise ValueError("H5 decisions are not canonicalized")
    if tuple(digest for _rank, digest in sorted(eligible)) != ranked or tuple(
        rank for rank, _digest_value in sorted(eligible)
    ) != tuple(range(1, len(eligible) + 1)):
        raise ValueError("H5 plan eligible ranking diverged")


@dataclass(frozen=True, slots=True)
class CognitiveExperimentBindingV1:
    """Opaque, frozen planner output for explicit default-off assignment schemas.

    Evaluation-v2 and runtime-context admission may each consume this DTO through
    their distinct schemas.  The DTO proves only internal hash/shape consistency;
    it grants no assignment or admission authority and does not prove that the
    decision prefix or H5 objects came from an authoritative producer.  Each
    store semantic CAS must establish that separately.
    """

    assignment_body: Mapping[str, Any]
    experiment_body: Mapping[str, Any]
    h5_request_body: Mapping[str, Any]
    h5_selection_plan_body: Mapping[str, Any]
    decision_prefix_digest: str
    decision_cutoff_seq: int
    decision_head_event_digest: str

    def __post_init__(self) -> None:
        assignment = _plain_mapping(self.assignment_body, "assignment_body")
        experiment = _plain_mapping(self.experiment_body, "experiment_body")
        h5_request = _plain_mapping(self.h5_request_body, "h5_request_body")
        h5_plan = _plain_mapping(self.h5_selection_plan_body, "h5_selection_plan_body")
        _validate_binding_canonical_size(
            assignment=assignment,
            experiment=experiment,
            h5_request=h5_request,
            h5_plan=h5_plan,
        )
        _validate_assignment_body(assignment)
        _validate_experiment_body(experiment)
        _validate_h5_request_body(h5_request)
        _validate_h5_plan_body(h5_plan)
        experiment_digest = canonical_digest(experiment)
        if assignment.get("experiment_digest") != experiment_digest:
            raise ValueError("assignment is rebound to a different experiment")
        signature = experiment.get("semantic_signature")
        if not isinstance(signature, Mapping):
            raise ValueError("experiment semantic signature is absent")
        _digest(
            signature.get("world_epoch_digest"),
            "experiment world_epoch_digest",
        )
        self._validate_h5_membership(
            request=h5_request,
            plan=h5_plan,
            experiment_digest=experiment_digest,
        )
        for name in ("decision_prefix_digest", "decision_head_event_digest"):
            object.__setattr__(self, name, _digest(getattr(self, name), name))
        if type(self.decision_cutoff_seq) is not int or self.decision_cutoff_seq < 1:
            raise ValueError("decision_cutoff_seq must be a positive exact integer")
        object.__setattr__(self, "assignment_body", assignment)
        object.__setattr__(self, "experiment_body", experiment)
        object.__setattr__(self, "h5_request_body", h5_request)
        object.__setattr__(self, "h5_selection_plan_body", h5_plan)

    @staticmethod
    def _validate_h5_membership(
        *,
        request: Mapping[str, Any],
        plan: Mapping[str, Any],
        experiment_digest: str,
    ) -> None:
        candidates = request.get("candidates")
        if type(candidates) is not tuple:
            raise ValueError("H5 request candidates must be a canonical sequence")
        candidate_digests = tuple(canonical_digest(item) for item in candidates)
        if experiment_digest not in candidate_digests:
            raise ValueError("assigned experiment is absent from the H5 request")
        ranked = plan.get("ranked_experiment_digests")
        decisions = plan.get("decisions")
        if type(ranked) is not tuple or type(decisions) is not tuple:
            raise ValueError("H5 plan decisions/ranking must be canonical sequences")
        if experiment_digest not in ranked:
            raise ValueError("assigned experiment is not H5-eligible")
        expected_rank = ranked.index(experiment_digest) + 1
        matching = tuple(
            item
            for item in decisions
            if isinstance(item, Mapping)
            and item.get("experiment_digest") == experiment_digest
        )
        if (
            len(matching) != 1
            or matching[0].get("disposition") != "eligible"
            or matching[0].get("rank_ordinal") != expected_rank
        ):
            raise ValueError("assigned experiment H5 decision/rank diverged")
        decision_digests = tuple(
            item.get("experiment_digest")
            for item in decisions
            if isinstance(item, Mapping)
        )
        if set(decision_digests) != set(candidate_digests) or len(
            decision_digests
        ) != len(candidate_digests):
            raise ValueError("H5 plan decisions do not cover request candidates")
        if (
            request.get("equivalence_proofs")
            or request.get("tombstones")
            or request.get("reopen_triggers")
        ):
            raise ValueError(
                "canonical cognitive v1 refuses unresolved H5 suppression inputs"
            )

        # Recompute the supported H5 v1 portfolio rather than trusting caller
        # ranks/scores.  This is the same deterministic greedy key used by the
        # runtime selector for the deliberately narrow no-suppression lane.
        prior_signatures = {
            canonical_digest(item["semantic_signature"])
            for item in request.get("prior_experiments", ())
        }
        recommendations = request.get("duplicate_recommendations", ())
        projected_coverage = set(request.get("covered_partition_digests", ()))
        remaining = list(candidates)
        expected_by_digest: dict[str, dict[str, Any]] = {}
        expected_ranked: list[str] = []
        ordinal = 1
        while remaining:

            def candidate_key(
                candidate: Mapping[str, Any],
            ) -> tuple[int, int, int, int, str]:
                partitions = set(
                    candidate["semantic_signature"]["prediction_partition_digests"]
                )
                return (
                    -len(partitions - projected_coverage),
                    -len(partitions),
                    -len(candidate["hypothesis_digests"]),
                    int(candidate["estimated_cost_units"]),
                    canonical_digest(candidate),
                )

            selected = min(remaining, key=candidate_key)
            selected_digest = canonical_digest(selected)
            partitions = set(
                selected["semantic_signature"]["prediction_partition_digests"]
            )
            uncovered = tuple(sorted(partitions - projected_coverage))
            signature_digest = canonical_digest(selected["semantic_signature"])
            possible_duplicate = any(
                item.get("candidate_signature_digest") == signature_digest
                and item.get("prior_signature_digest") in prior_signatures
                for item in recommendations
            )
            expected_by_digest[selected_digest] = {
                "disposition": "eligible",
                "duplicate_decision": (
                    "possible_duplicate" if possible_duplicate else "not_proven"
                ),
                "equivalence_proof_digest": None,
                "experiment_digest": selected_digest,
                "rank_ordinal": ordinal,
                "score": {
                    "estimated_cost_units": selected["estimated_cost_units"],
                    "hypothesis_count": len(selected["hypothesis_digests"]),
                    "total_partition_count": len(partitions),
                    "uncovered_partition_digests": uncovered,
                },
                "tombstone_digest": None,
            }
            expected_ranked.append(selected_digest)
            projected_coverage.update(partitions)
            remaining.remove(selected)
            ordinal += 1
        expected_plan = {
            "decisions": tuple(
                expected_by_digest[digest] for digest in sorted(expected_by_digest)
            ),
            "ranked_experiment_digests": tuple(expected_ranked),
            "selector_version": RUNTIME_H5_SELECTOR_VERSION,
        }
        if canonical_digest(expected_plan) != canonical_digest(plan):
            raise ValueError("H5 selection plan does not replay from its request")

    @property
    def assignment_digest(self) -> str:
        return canonical_digest(self.assignment_body)

    @property
    def experiment_digest(self) -> str:
        return canonical_digest(self.experiment_body)

    @property
    def h5_request_digest(self) -> str:
        return canonical_digest(self.h5_request_body)

    @property
    def h5_selection_plan_digest(self) -> str:
        return canonical_digest(self.h5_selection_plan_body)

    @property
    def world_epoch_digest(self) -> str:
        signature = self.experiment_body["semantic_signature"]
        assert isinstance(signature, Mapping)
        return str(signature["world_epoch_digest"])


def cognitive_assignment_payload(
    *,
    binding: CognitiveExperimentBindingV1,
    admission_payload: Mapping[str, Any],
    evaluation_sidecar: Mapping[str, Any],
) -> dict[str, Any]:
    if type(binding) is not CognitiveExperimentBindingV1:
        raise TypeError("binding must be CognitiveExperimentBindingV1")
    if evaluation_sidecar.get("role") != "executor":
        raise ValueError("cognitive experiments can bind only executor attempts")
    payload = {
        "accepted_set_change": COGNITIVE_ACCEPTED_SET_CHANGE,
        "assignment_binding_digest": evaluation_sidecar.get(
            "assignment_binding_digest"
        ),
        "assignment_body": binding.assignment_body,
        "assignment_digest": binding.assignment_digest,
        "attempt_digest": admission_payload.get("attempt_digest"),
        "attempt_id": admission_payload.get("attempt_id"),
        "attempt_role_binding_digest": evaluation_sidecar.get(
            "attempt_role_binding_digest"
        ),
        "base_event_id": evaluation_sidecar.get("base_event_id"),
        "base_payload_digest": evaluation_sidecar.get("base_payload_digest"),
        "decision_cutoff_seq": binding.decision_cutoff_seq,
        "decision_head_event_digest": binding.decision_head_event_digest,
        "decision_prefix_digest": binding.decision_prefix_digest,
        "evaluation_sidecar_event_id": (
            f"event:C6_EVAL_V2_ATTEMPT_BOUND:{admission_payload.get('attempt_id')}"
        ),
        "evaluation_sidecar_payload_digest": canonical_digest(evaluation_sidecar),
        "experiment_body": binding.experiment_body,
        "experiment_digest": binding.experiment_digest,
        "h5_request_body": binding.h5_request_body,
        "h5_request_digest": binding.h5_request_digest,
        "h5_selection_plan_body": binding.h5_selection_plan_body,
        "h5_selection_plan_digest": binding.h5_selection_plan_digest,
        "mode": COGNITIVE_MODE,
        "permit_digest": admission_payload.get("permit_digest"),
        "permit_id": admission_payload.get("permit_id"),
        "schema_id": COGNITIVE_ASSIGNMENT_SCHEMA_ID,
        "scope_digest": admission_payload.get("scope_digest"),
        "world_epoch_digest": binding.world_epoch_digest,
    }
    validate_assignment_payload_shape(payload)
    return payload


def validate_assignment_payload_shape(payload: Mapping[str, Any]) -> None:
    p = dict(payload)
    if set(p) != _ASSIGNMENT_EVENT_FIELDS:
        raise ValueError("cognitive assignment event shape is not versioned")
    if (
        p["schema_id"] != COGNITIVE_ASSIGNMENT_SCHEMA_ID
        or p["mode"] != COGNITIVE_MODE
        or p["accepted_set_change"] is not False
        or type(p["decision_cutoff_seq"]) is not int
        or p["decision_cutoff_seq"] < 1
    ):
        raise ValueError("cognitive assignment event policy diverged")
    for name in (
        "assignment_binding_digest",
        "assignment_digest",
        "attempt_digest",
        "attempt_role_binding_digest",
        "base_payload_digest",
        "decision_head_event_digest",
        "decision_prefix_digest",
        "evaluation_sidecar_payload_digest",
        "experiment_digest",
        "h5_request_digest",
        "h5_selection_plan_digest",
        "permit_digest",
        "scope_digest",
        "world_epoch_digest",
    ):
        _digest(p[name], name)
    assignment = _plain_mapping(p["assignment_body"], "assignment_body")
    experiment = _plain_mapping(p["experiment_body"], "experiment_body")
    h5_request = _plain_mapping(p["h5_request_body"], "h5_request_body")
    h5_plan = _plain_mapping(p["h5_selection_plan_body"], "h5_selection_plan_body")
    _validate_binding_canonical_size(
        assignment=assignment,
        experiment=experiment,
        h5_request=h5_request,
        h5_plan=h5_plan,
    )
    _validate_assignment_body(assignment)
    _validate_experiment_body(experiment)
    _validate_h5_request_body(h5_request)
    _validate_h5_plan_body(h5_plan)
    if canonical_digest(assignment) != p["assignment_digest"]:
        raise ValueError("cognitive assignment digest is false")
    if canonical_digest(experiment) != p["experiment_digest"]:
        raise ValueError("cognitive experiment digest is false")
    if assignment.get("experiment_digest") != p["experiment_digest"]:
        raise ValueError("cognitive assignment/experiment lineage diverged")
    if canonical_digest(h5_request) != p["h5_request_digest"]:
        raise ValueError("cognitive H5 request digest is false")
    if canonical_digest(h5_plan) != p["h5_selection_plan_digest"]:
        raise ValueError("cognitive H5 selection plan digest is false")
    CognitiveExperimentBindingV1._validate_h5_membership(
        request=h5_request,
        plan=h5_plan,
        experiment_digest=p["experiment_digest"],
    )
    signature = experiment.get("semantic_signature")
    if (
        not isinstance(signature, Mapping)
        or signature.get("world_epoch_digest") != p["world_epoch_digest"]
    ):
        raise ValueError("cognitive assignment world epoch diverged")
    if experiment.get("scope_digest") != p["scope_digest"]:
        raise ValueError("cognitive experiment/runtime scope diverged")
    _identifier(p["attempt_id"], "attempt_id")
    _identifier(p["permit_id"], "permit_id")
    _identifier(p["base_event_id"], "base_event_id")
    _identifier(p["evaluation_sidecar_event_id"], "evaluation_sidecar_event_id")


def cognitive_runtime_context_assignment_payload(
    *,
    binding: CognitiveExperimentBindingV1,
    admission_payload: Mapping[str, Any],
) -> dict[str, Any]:
    """Bind one H5-eligible assignment to an ordinary C6 ContextPacket admission.

    This schema is deliberately disjoint from the evaluation-v2 assignment
    schema above.  The store must still recompute the supplied prefix and commit
    this payload atomically with the ordinary ``ATTEMPT_ADMITTED`` event; this
    constructor alone is not assignment authority.  The narrow H5 replay does
    not prove that a qualified runtime/V3 planner policy selected the assignment.
    """

    if type(binding) is not CognitiveExperimentBindingV1:
        raise TypeError("binding must be CognitiveExperimentBindingV1")
    packet_body = admission_payload.get("context_packet")
    if not isinstance(packet_body, Mapping):
        raise ValueError("runtime-context assignment requires a ContextPacket")
    packet = _plain_mapping(packet_body, "context_packet_binding_body")
    payload = {
        "accepted_set_change": COGNITIVE_ACCEPTED_SET_CHANGE,
        "assignment_body": binding.assignment_body,
        "assignment_digest": binding.assignment_digest,
        "attempt_digest": admission_payload.get("attempt_digest"),
        "attempt_id": admission_payload.get("attempt_id"),
        "base_event_id": (f"event:attempt:admit:{admission_payload.get('attempt_id')}"),
        "base_payload_digest": canonical_digest(admission_payload),
        "context_packet_binding_body": packet,
        "context_packet_binding_digest": canonical_digest(packet),
        "decision_cutoff_seq": binding.decision_cutoff_seq,
        "decision_head_event_digest": binding.decision_head_event_digest,
        "decision_prefix_digest": binding.decision_prefix_digest,
        "experiment_body": binding.experiment_body,
        "experiment_digest": binding.experiment_digest,
        "h5_request_body": binding.h5_request_body,
        "h5_request_digest": binding.h5_request_digest,
        "h5_selection_plan_body": binding.h5_selection_plan_body,
        "h5_selection_plan_digest": binding.h5_selection_plan_digest,
        "mode": COGNITIVE_RUNTIME_CONTEXT_MODE,
        "permit_digest": admission_payload.get("permit_digest"),
        "permit_id": admission_payload.get("permit_id"),
        "planner_policy_selection_proven": False,
        "schema_id": COGNITIVE_RUNTIME_CONTEXT_ASSIGNMENT_SCHEMA_ID,
        "scope_digest": admission_payload.get("scope_digest"),
        "world_epoch_digest": binding.world_epoch_digest,
    }
    validate_runtime_context_assignment_payload_shape(payload)
    return payload


def cognitive_runtime_context_executable_assignment_payload(
    *,
    binding: CognitiveExperimentBindingV1,
    admission_payload: Mapping[str, Any],
    executable_experiment: Any,
) -> dict[str, Any]:
    """Bind a CAS-sealed executable spec to the runtime-context assignment.

    The executable binding is data, not dispatch or verification authority.  The
    admission writer must separately resolve its CAS bytes before committing the
    atomic admission/assignment pair.
    """

    from muteki.runtime.executable_experiment_v1 import (
        ExecutableExperimentBindingV1,
    )

    if type(executable_experiment) is not ExecutableExperimentBindingV1:
        raise TypeError(
            "executable_experiment must be ExecutableExperimentBindingV1"
        )
    executable_experiment.spec.validate_against_body(binding.experiment_body)
    payload = cognitive_runtime_context_assignment_payload(
        binding=binding,
        admission_payload=admission_payload,
    )
    payload["schema_id"] = COGNITIVE_RUNTIME_EXECUTABLE_ASSIGNMENT_SCHEMA_ID
    payload["executable_experiment_binding_body"] = (
        executable_experiment.canonical_body()
    )
    payload["executable_experiment_binding_digest"] = executable_experiment.digest
    validate_runtime_context_executable_assignment_payload_shape(payload)
    return payload


def cognitive_runtime_reproduction_assignment_payload(
    *,
    binding: CognitiveExperimentBindingV1,
    admission_payload: Mapping[str, Any],
    executable_experiment: Any,
    source_assignment_event_digest: str,
    source_assignment_event_receipt_digest: str,
    source_assignment_payload: Mapping[str, Any],
    source_observation_event_digest: str,
    source_observation_event_receipt_digest: str,
    source_observation_payload: Mapping[str, Any],
    required_reproducer_profile_digest: str,
) -> dict[str, Any]:
    """Freeze one fresh execution as the sole reproducer of a prior observation.

    Admission is the pre-outcome registration point.  The function derives every
    claim and withheld-field identity from the already-canonical source payloads;
    the store independently resolves those events again before accepting it.
    """

    from muteki.runtime.executable_experiment_v1 import (
        ExecutableExperimentBindingV1,
    )

    if type(executable_experiment) is not ExecutableExperimentBindingV1:
        raise TypeError(
            "executable_experiment must be ExecutableExperimentBindingV1"
        )
    source_assignment = _plain_mapping(
        source_assignment_payload,
        "source_assignment_payload",
    )
    source_observation = _plain_mapping(
        source_observation_payload,
        "source_observation_payload",
    )
    if source_assignment.get("schema_id") != (
        COGNITIVE_RUNTIME_EXECUTABLE_ASSIGNMENT_SCHEMA_ID
    ):
        raise ValueError("reproduction source must be one ordinary executable assignment")
    validate_runtime_context_executable_assignment_payload_shape(source_assignment)
    if (
        source_observation.get("schema_id") != COGNITIVE_RUNTIME_EXECUTION_SCHEMA_ID
        or source_observation.get("assignment_event_digest")
        != source_assignment_event_digest
        or source_observation.get("classification_status") != "observed"
        or source_observation.get("observed_partition_digest") is None
        or source_observation.get("usage_status") != "complete"
        or source_observation.get("verification_resolved") is not False
        or source_observation.get("learning_eligible") is not False
    ):
        raise ValueError("reproduction source is not one complete unverified observation")
    source_executable = ExecutableExperimentBindingV1.from_canonical(
        source_assignment["executable_experiment_binding_body"]
    )
    source_executable.spec.validate_reproduction_kernel(executable_experiment.spec)
    source_experiment = source_assignment["experiment_body"]
    reproduction_experiment = binding.experiment_body
    source_predictions = {
        item["hypothesis_digest"]: item["outcome_partition_digest"]
        for item in source_experiment["predictions"]
    }
    reproduction_predictions = {
        item["hypothesis_digest"]: item["outcome_partition_digest"]
        for item in reproduction_experiment["predictions"]
    }
    if (
        source_assignment["attempt_id"] == admission_payload.get("attempt_id")
        or source_assignment["experiment_digest"] == binding.experiment_digest
        or source_assignment["scope_digest"] != admission_payload.get("scope_digest")
        or source_assignment["world_epoch_digest"] != binding.world_epoch_digest
        or source_experiment["hypothesis_digests"]
        != reproduction_experiment["hypothesis_digests"]
        or source_predictions != reproduction_predictions
    ):
        raise ValueError("reproducer does not preserve the source logical experiment")

    payload = cognitive_runtime_context_executable_assignment_payload(
        binding=binding,
        admission_payload=admission_payload,
        executable_experiment=executable_experiment,
    )
    payload["schema_id"] = COGNITIVE_RUNTIME_REPRODUCTION_ASSIGNMENT_SCHEMA_ID
    payload["mode"] = COGNITIVE_RUNTIME_REPRODUCTION_MODE
    payload.update(
        {
            # A distinct attempt/profile is a preregistered reproduction, not by
            # itself proof of source-family or environment independence.  A later
            # verifier must resolve those witnesses before learning.
            "assignment_role": "preregistered_reproducer",
            "automatic_redispatch_permitted": False,
            "learning_eligible": False,
            "max_reproduction_count": 1,
            "reproduction_kernel_digest": (
                executable_experiment.spec.reproduction_kernel_digest
            ),
            "required_reproducer_profile_digest": _digest(
                required_reproducer_profile_digest,
                "required_reproducer_profile_digest",
            ),
            "source_assignment_event_digest": _digest(
                source_assignment_event_digest,
                "source_assignment_event_digest",
            ),
            "source_assignment_event_receipt_digest": _digest(
                source_assignment_event_receipt_digest,
                "source_assignment_event_receipt_digest",
            ),
            "source_claim_digest": canonical_digest(
                {
                    "experiment_digest": source_assignment["experiment_digest"],
                    "observed_partition_digest": source_observation[
                        "observed_partition_digest"
                    ],
                    "schema_id": "muteki.cognitive-observation-binding.v1",
                }
            ),
            "source_executable_spec_digest": source_executable.spec.digest,
            "source_observation_event_digest": _digest(
                source_observation_event_digest,
                "source_observation_event_digest",
            ),
            "source_observation_event_receipt_digest": _digest(
                source_observation_event_receipt_digest,
                "source_observation_event_receipt_digest",
            ),
            "source_reproduction_kernel_digest": (
                source_executable.spec.reproduction_kernel_digest
            ),
            "verification_policy_version": "muteki-cognitive-verification.v1",
        }
    )
    withheld = tuple(
        sorted(
            {
                source_observation_event_digest,
                source_observation["classification_digest"],
                *(
                    item["raw_digest"]
                    for item in source_observation["capture_bindings"]
                ),
            }
        )
    )
    payload["withheld_source_digest_set"] = withheld
    payload["withheld_source_digest_set_digest"] = canonical_digest(withheld)
    validate_runtime_reproduction_assignment_payload_shape(payload)
    return payload


def validate_runtime_context_assignment_payload_shape(
    payload: Mapping[str, Any],
) -> None:
    """Validate only the runtime-context assignment schema, never eval-v2."""

    p = dict(payload)
    if set(p) != _RUNTIME_CONTEXT_ASSIGNMENT_EVENT_FIELDS:
        raise ValueError(
            "runtime-context cognitive assignment event shape is not versioned"
        )
    if (
        p["schema_id"] != COGNITIVE_RUNTIME_CONTEXT_ASSIGNMENT_SCHEMA_ID
        or p["mode"] != COGNITIVE_RUNTIME_CONTEXT_MODE
        or p["accepted_set_change"] is not False
        or p["planner_policy_selection_proven"] is not False
        or type(p["decision_cutoff_seq"]) is not int
        or p["decision_cutoff_seq"] < 1
    ):
        raise ValueError("runtime-context cognitive assignment policy diverged")
    for name in (
        "assignment_digest",
        "attempt_digest",
        "base_payload_digest",
        "context_packet_binding_digest",
        "decision_head_event_digest",
        "decision_prefix_digest",
        "experiment_digest",
        "h5_request_digest",
        "h5_selection_plan_digest",
        "permit_digest",
        "scope_digest",
        "world_epoch_digest",
    ):
        _digest(p[name], name)
    for name in ("attempt_id", "base_event_id", "permit_id"):
        _identifier(p[name], name)

    packet = _plain_mapping(
        p["context_packet_binding_body"], "context_packet_binding_body"
    )
    if set(packet) != _CONTEXT_PACKET_BINDING_FIELDS:
        raise ValueError("runtime-context ContextPacket binding shape diverged")
    if (
        packet.get("accepted_set_change") is not False
        or type(packet.get("cutoff_seq")) is not int
        or packet["cutoff_seq"] < 1
    ):
        raise ValueError("runtime-context ContextPacket policy diverged")
    for name in (
        "compilation_event_receipt_digest",
        "compiler_receipt_digest",
        "decision_receipt_digest",
        "feature_state_digest",
        "manifest_digest",
        "packet_digest",
    ):
        _digest(packet.get(name), f"context_packet.{name}")
    for name in (
        "compiler_version",
        "decision_id",
        "target_attempt_id",
    ):
        _identifier(packet.get(name), f"context_packet.{name}")
    if canonical_digest(packet) != p["context_packet_binding_digest"]:
        raise ValueError("runtime-context ContextPacket binding digest is false")
    if packet.get("target_attempt_id") != p["attempt_id"]:
        raise ValueError("runtime-context ContextPacket belongs to another attempt")

    assignment = _plain_mapping(p["assignment_body"], "assignment_body")
    experiment = _plain_mapping(p["experiment_body"], "experiment_body")
    h5_request = _plain_mapping(p["h5_request_body"], "h5_request_body")
    h5_plan = _plain_mapping(p["h5_selection_plan_body"], "h5_selection_plan_body")
    _validate_binding_canonical_size(
        assignment=assignment,
        experiment=experiment,
        h5_request=h5_request,
        h5_plan=h5_plan,
    )
    _validate_assignment_body(assignment)
    _validate_experiment_body(experiment)
    _validate_h5_request_body(h5_request)
    _validate_h5_plan_body(h5_plan)
    if canonical_digest(assignment) != p["assignment_digest"]:
        raise ValueError("runtime-context cognitive assignment digest is false")
    if canonical_digest(experiment) != p["experiment_digest"]:
        raise ValueError("runtime-context cognitive experiment digest is false")
    if assignment.get("experiment_digest") != p["experiment_digest"]:
        raise ValueError("runtime-context assignment/experiment lineage diverged")
    if experiment.get("context_packet_digest") != packet.get("packet_digest"):
        raise ValueError("runtime-context experiment/ContextPacket lineage diverged")
    if experiment.get("scope_digest") != p["scope_digest"]:
        raise ValueError("runtime-context experiment/runtime scope diverged")
    signature = experiment.get("semantic_signature")
    if (
        not isinstance(signature, Mapping)
        or signature.get("world_epoch_digest") != p["world_epoch_digest"]
    ):
        raise ValueError("runtime-context assignment world epoch diverged")
    if canonical_digest(h5_request) != p["h5_request_digest"]:
        raise ValueError("runtime-context H5 request digest is false")
    if canonical_digest(h5_plan) != p["h5_selection_plan_digest"]:
        raise ValueError("runtime-context H5 selection plan digest is false")
    CognitiveExperimentBindingV1._validate_h5_membership(
        request=h5_request,
        plan=h5_plan,
        experiment_digest=p["experiment_digest"],
    )


def validate_runtime_context_executable_assignment_payload_shape(
    payload: Mapping[str, Any],
) -> None:
    """Validate the v2 executable assignment without weakening v1."""

    from muteki.runtime.executable_experiment_v1 import (
        ExecutableExperimentBindingV1,
    )

    p = dict(payload)
    if set(p) != _RUNTIME_EXECUTABLE_ASSIGNMENT_EVENT_FIELDS:
        raise ValueError(
            "runtime executable cognitive assignment shape is not versioned"
        )
    if p.get("schema_id") != COGNITIVE_RUNTIME_EXECUTABLE_ASSIGNMENT_SCHEMA_ID:
        raise ValueError("runtime executable cognitive assignment schema diverged")
    base = {
        name: value
        for name, value in p.items()
        if name
        not in {
            "executable_experiment_binding_body",
            "executable_experiment_binding_digest",
        }
    }
    base["schema_id"] = COGNITIVE_RUNTIME_CONTEXT_ASSIGNMENT_SCHEMA_ID
    validate_runtime_context_assignment_payload_shape(base)
    executable = ExecutableExperimentBindingV1.from_canonical(
        p["executable_experiment_binding_body"]
    )
    if (
        executable.digest != p["executable_experiment_binding_digest"]
        or executable.spec.experiment_digest != p["experiment_digest"]
        or executable.spec.context_packet_digest
        != p["context_packet_binding_body"]["packet_digest"]
        or executable.spec.scope_digest != p["scope_digest"]
    ):
        raise ValueError("runtime executable experiment lineage diverged")
    executable.spec.validate_against_body(p["experiment_body"])


def validate_runtime_reproduction_assignment_payload_shape(
    payload: Mapping[str, Any],
) -> None:
    """Validate the reproduction schema without claiming its source events exist."""

    from muteki.runtime.executable_experiment_v1 import (
        ExecutableExperimentBindingV1,
    )

    p = dict(payload)
    if set(p) != _RUNTIME_REPRODUCTION_ASSIGNMENT_EVENT_FIELDS:
        raise ValueError("runtime reproduction assignment shape is not versioned")
    if (
        p.get("schema_id") != COGNITIVE_RUNTIME_REPRODUCTION_ASSIGNMENT_SCHEMA_ID
        or p.get("mode") != COGNITIVE_RUNTIME_REPRODUCTION_MODE
        or p.get("assignment_role") != "preregistered_reproducer"
        or p.get("automatic_redispatch_permitted") is not False
        or p.get("learning_eligible") is not False
        or p.get("max_reproduction_count") != 1
        or p.get("verification_policy_version")
        != "muteki-cognitive-verification.v1"
    ):
        raise ValueError("runtime reproduction assignment policy diverged")
    base = {
        name: value
        for name, value in p.items()
        if name not in (_RUNTIME_REPRODUCTION_ASSIGNMENT_EVENT_FIELDS - _RUNTIME_EXECUTABLE_ASSIGNMENT_EVENT_FIELDS)
    }
    base["schema_id"] = COGNITIVE_RUNTIME_EXECUTABLE_ASSIGNMENT_SCHEMA_ID
    base["mode"] = COGNITIVE_RUNTIME_CONTEXT_MODE
    validate_runtime_context_executable_assignment_payload_shape(base)
    executable = ExecutableExperimentBindingV1.from_canonical(
        p["executable_experiment_binding_body"]
    )
    for name in (
        "reproduction_kernel_digest",
        "required_reproducer_profile_digest",
        "source_assignment_event_digest",
        "source_assignment_event_receipt_digest",
        "source_claim_digest",
        "source_executable_spec_digest",
        "source_observation_event_digest",
        "source_observation_event_receipt_digest",
        "source_reproduction_kernel_digest",
        "withheld_source_digest_set_digest",
    ):
        _digest(p[name], name)
    if (
        p["reproduction_kernel_digest"]
        != executable.spec.reproduction_kernel_digest
        or p["source_reproduction_kernel_digest"]
        != p["reproduction_kernel_digest"]
    ):
        raise ValueError("runtime reproduction kernel identity diverged")
    withheld = p["withheld_source_digest_set"]
    withheld_tuple = tuple(withheld) if type(withheld) in {tuple, list} else ()
    if (
        not withheld_tuple
        or withheld_tuple != tuple(sorted(set(withheld_tuple)))
    ):
        raise ValueError("withheld source digest set is not canonical")
    for index, value in enumerate(withheld_tuple):
        _digest(value, f"withheld_source_digest_set[{index}]")
    if canonical_digest(withheld_tuple) != p["withheld_source_digest_set_digest"]:
        raise ValueError("withheld source digest set digest is false")


def cognitive_execution_payload(
    *,
    assignment_event_digest: str,
    assignment_payload: Mapping[str, Any],
    terminal_payload: Mapping[str, Any],
    budget_event_id: str,
    budget_kind: str,
    budget_payload: Mapping[str, Any],
    evaluation_terminal_event_id: str,
    evaluation_terminal_sidecar: Mapping[str, Any],
) -> dict[str, Any]:
    validate_assignment_payload_shape(assignment_payload)
    usage_status = (
        "unknown" if budget_kind == "BUDGET_USAGE_UNKNOWN" else "pessimistic_ceiling"
    )
    payload = {
        "accepted_set_change": COGNITIVE_ACCEPTED_SET_CHANGE,
        "assignment_event_digest": _digest(
            assignment_event_digest, "assignment_event_digest"
        ),
        "attempt_digest": terminal_payload.get("attempt_digest"),
        "attempt_id": terminal_payload.get("attempt_id"),
        "base_event_id": evaluation_terminal_sidecar.get("base_event_id"),
        "base_payload_digest": evaluation_terminal_sidecar.get("base_payload_digest"),
        "budget_event_id": budget_event_id,
        "budget_event_kind": budget_kind,
        "budget_payload_digest": canonical_digest(budget_payload),
        "epistemic_classification": "not_resolved",
        "evaluation_terminal_event_id": evaluation_terminal_event_id,
        "evaluation_terminal_payload_digest": canonical_digest(
            evaluation_terminal_sidecar
        ),
        "execution_outcome": terminal_payload.get("outcome"),
        "experiment_execution_claim": False,
        "experiment_digest": assignment_payload["experiment_digest"],
        "experiment_materialization_status": "not_materialized",
        "learning_eligible": False,
        "mode": COGNITIVE_MODE,
        "observed_partition_digest": None,
        "permit_digest": terminal_payload.get("permit_digest"),
        "permit_id": terminal_payload.get("permit_id"),
        "schema_id": COGNITIVE_EXECUTION_SCHEMA_ID,
        "scope_digest": terminal_payload.get("scope_digest"),
        "usage_status": usage_status,
        "world_epoch_digest": assignment_payload["world_epoch_digest"],
    }
    validate_execution_payload_shape(payload)
    return payload


def validate_execution_payload_shape(payload: Mapping[str, Any]) -> None:
    p = dict(payload)
    if set(p) != _EXECUTION_EVENT_FIELDS:
        raise ValueError("cognitive execution event shape is not versioned")
    if (
        p["schema_id"] != COGNITIVE_EXECUTION_SCHEMA_ID
        or p["mode"] != COGNITIVE_MODE
        or p["accepted_set_change"] is not False
        or p["learning_eligible"] is not False
        or p["epistemic_classification"] != "not_resolved"
        or p["observed_partition_digest"] is not None
        or p["experiment_execution_claim"] is not False
        or p["experiment_materialization_status"] != "not_materialized"
        or p["execution_outcome"] not in {"observed", "unknown"}
        or p["usage_status"] not in {"pessimistic_ceiling", "unknown"}
    ):
        raise ValueError("cognitive execution event overclaims its authority")
    expected_budget_kind = (
        "BUDGET_USAGE_UNKNOWN"
        if p["usage_status"] == "unknown"
        else "BUDGET_PESSIMISTICALLY_SETTLED"
    )
    if p["budget_event_kind"] != expected_budget_kind:
        raise ValueError("cognitive execution usage/budget status diverged")
    for name in (
        "assignment_event_digest",
        "attempt_digest",
        "base_payload_digest",
        "budget_payload_digest",
        "evaluation_terminal_payload_digest",
        "experiment_digest",
        "permit_digest",
        "scope_digest",
        "world_epoch_digest",
    ):
        _digest(p[name], name)
    for name in (
        "attempt_id",
        "base_event_id",
        "budget_event_id",
        "evaluation_terminal_event_id",
        "permit_id",
    ):
        _identifier(p[name], name)


__all__ = [
    "COGNITIVE_ACCEPTED_SET_CHANGE",
    "COGNITIVE_ASSIGNMENT_SCHEMA_ID",
    "COGNITIVE_BINDING_ACTOR",
    "COGNITIVE_EXECUTION_OBSERVED",
    "COGNITIVE_EXECUTION_SCHEMA_ID",
    "COGNITIVE_EXPERIMENT_ASSIGNED",
    "COGNITIVE_MODE",
    "COGNITIVE_RUNTIME_CONTEXT_ASSIGNMENT_SCHEMA_ID",
    "COGNITIVE_RUNTIME_CONTEXT_MODE",
    "COGNITIVE_RUNTIME_EXECUTABLE_ASSIGNMENT_SCHEMA_ID",
    "COGNITIVE_RUNTIME_REPRODUCTION_ASSIGNMENT_SCHEMA_ID",
    "COGNITIVE_RUNTIME_REPRODUCTION_MODE",
    "COGNITIVE_RUNTIME_EXECUTION_SCHEMA_ID",
    "COGNITIVE_VERIFICATION_RESOLVED",
    "COGNITIVE_VERIFICATION_SCHEMA_ID",
    "COGNITIVE_VERIFIER_ACTOR",
    "COGNITIVE_H5_MAX_ASSIGNMENT_CANONICAL_BYTES_V1",
    "COGNITIVE_H5_MAX_BINDING_CANONICAL_BYTES_V1",
    "COGNITIVE_H5_MAX_CANDIDATES_V1",
    "COGNITIVE_H5_MAX_EXPERIMENT_CANONICAL_BYTES_V1",
    "COGNITIVE_H5_MAX_HYPOTHESES_V1",
    "COGNITIVE_H5_MAX_PLAN_CANONICAL_BYTES_V1",
    "COGNITIVE_H5_MAX_PREDICTIONS_PER_EXPERIMENT_V1",
    "COGNITIVE_H5_MAX_PRIOR_EXPERIMENTS_V1",
    "COGNITIVE_H5_MAX_REQUEST_CANONICAL_BYTES_V1",
    "CognitiveExperimentBindingV1",
    "cognitive_assignment_payload",
    "cognitive_execution_payload",
    "cognitive_runtime_context_assignment_payload",
    "cognitive_runtime_context_executable_assignment_payload",
    "cognitive_runtime_reproduction_assignment_payload",
    "validate_assignment_payload_shape",
    "validate_execution_payload_shape",
    "validate_runtime_context_assignment_payload_shape",
    "validate_runtime_context_executable_assignment_payload_shape",
    "validate_runtime_reproduction_assignment_payload_shape",
]
