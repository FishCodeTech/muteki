"""Atomic binding for one canonical cognitive-cycle selection.

This module is deliberately an admission guard, not a planner authority.  The
caller supplies typed planning inputs and a pure plan, while the store rebuilds
the exact resolver-owned fact inventory from the pre-admission receipt prefix
and reruns the canonical planner inside the admission transaction.

Only that resolver-fact inventory is store-resolved.  The candidate set, scalar
cost estimates, and remaining cost units stay frozen caller-supplied proposals.
An exact planner replay therefore proves only deterministic selection over those
inputs; it does not turn them into capability, accounting, or budget truth.
"""

from __future__ import annotations

from collections.abc import Mapping
from typing import Any

from muteki.epistemic.cognitive_events_v1 import (
    COGNITIVE_ASSIGNMENT_SCHEMA_ID,
    COGNITIVE_BINDING_ACTOR,
    COGNITIVE_EXPERIMENT_ASSIGNED,
)
from muteki.epistemic.contracts import (
    EventEnvelopeV2,
    canonical_digest,
    canonical_json_bytes,
)
from muteki.epistemic.receipt_objects import (
    CanonicalCommandReceiptResolverV1,
    ResolvedReceiptFieldV1,
    VerifiedEventReferenceV1,
    VerifiedReceiptPrefixV1,
)
from muteki.epistemic.sqlite_store import EpistemicSQLiteStore, IntegrityError
from muteki.runtime.canonical_cognitive_cycle_v1 import (
    CanonicalCognitiveCycleModeV1,
    CanonicalCognitiveCyclePlanV1,
    CanonicalCognitiveCycleRequestV1,
    ResolvedCognitiveFactV1,
    plan_canonical_cognitive_cycle_v1,
)
from muteki.runtime.cognitive_planning_contracts_v1 import (
    HypothesisMassV1,
    SuppliedCostEstimateV1,
)
from muteki.runtime.contracts import RuntimeEvaluationBindingV2
from muteki.runtime.cognitive_verification_resolver_v1 import (
    validate_cognitive_verification_resolution_against_store,
)
from muteki.runtime.hypothesis import (
    ActionClass,
    DiscriminatingExperiment,
    EffectClass,
    ExperimentPrediction,
    H5RecommendationRequestV1,
    Hypothesis,
    HypothesisSelector,
    PossibleDuplicateRecommendation,
    SemanticSignature,
)


COGNITIVE_CANONICAL_SELECTION_BOUND = "COGNITIVE_CANONICAL_SELECTION_BOUND"
COGNITIVE_CANONICAL_SELECTION_ACTOR = (
    "cognitive-canonical-selection-binding-v1-authority"
)
COGNITIVE_CANONICAL_SELECTION_SCHEMA_ID = (
    "muteki.cognitive-canonical-selection-bound.v1"
)
COGNITIVE_CANONICAL_REQUEST_BINDING_SCHEMA_ID = (
    "muteki.canonical-cognitive-cycle-request-binding.v1"
)
AUTHORITY_EFFECT_NONE = "NONE"
PRODUCTION_ENABLED = False
ACCEPTED_SET_CHANGE = False
AUTOMATIC_REDISPATCH_PERMITTED = False
PROVENANCE_GATE_ACCEPTED_SET = "UNCHANGED"
CANDIDATE_SET_AUTHORITY = "NONE_CALLER_SUPPLIED_FROZEN_TYPED_PROPOSAL"
COST_ESTIMATE_AUTHORITY = "NONE_CALLER_SUPPLIED_FROZEN_SCALAR_PROPOSAL"
REMAINING_COST_AUTHORITY = "NONE_CALLER_SUPPLIED_FROZEN_SCALAR_LIMIT"
RESOLVER_FACT_AUTHORITY = "STORE_RESOLVED_CANONICAL_RECEIPT_PREFIX"
SELECTION_SEMANTICS = "EXACT_PLANNER_OUTPUT_OVER_FROZEN_INPUTS"


def _input_authority_labels() -> dict[str, str]:
    return {
        "candidate_set": CANDIDATE_SET_AUTHORITY,
        "cost_estimates": COST_ESTIMATE_AUTHORITY,
        "remaining_cost_units": REMAINING_COST_AUTHORITY,
        "resolver_facts": RESOLVER_FACT_AUTHORITY,
    }


def _event_from_resolved(value: ResolvedReceiptFieldV1) -> EventEnvelopeV2:
    body = value.value
    if not isinstance(body, Mapping):
        raise IntegrityError("resolved receipt event is not a mapping")
    try:
        event = EventEnvelopeV2(
            event_id=body["event_id"],
            run_id=body["run_id"],
            command_id=body["command_id"],
            ordinal=body["ordinal"],
            kind=body["kind"],
            actor=body["actor"],
            occurred_at_ns=body["occurred_at_ns"],
            payload=body["payload"],
            parent_event_digest=body["parent_event_digest"],
            schema_version=body["schema_version"],
        )
    except (KeyError, TypeError, ValueError) as exc:
        raise IntegrityError("resolved receipt event does not replay") from exc
    if (
        canonical_json_bytes(event.canonical_body()) != canonical_json_bytes(body)
        or event.digest != value.pointer.event_digest
        or event.command_id != value.pointer.command_id
        or event.ordinal != value.pointer.event_ordinal
    ):
        raise IntegrityError("resolved receipt event identity was rebound")
    return event


def _resolve_reference(
    *,
    resolver: CanonicalCommandReceiptResolverV1,
    prefix: VerifiedReceiptPrefixV1,
    reference: VerifiedEventReferenceV1,
) -> tuple[ResolvedReceiptFieldV1, EventEnvelopeV2]:
    entries = tuple(
        item
        for item in resolver.index.entries
        if item.receipt_digest == reference.receipt_digest
        and item.first_seq <= reference.seq <= item.last_seq
    )
    if len(entries) != 1:
        raise IntegrityError("verified event receipt boundary is ambiguous")
    entry = entries[0]
    ordinal = reference.seq - entry.first_seq
    pointer = resolver.pointer_for(
        reference.receipt_digest,
        f"events[{ordinal}]",
        cutoff_seq=prefix.cutoff_seq,
    )
    resolved = resolver.resolve(pointer, cutoff_seq=prefix.cutoff_seq)
    event = _event_from_resolved(resolved)
    if (
        event.digest != reference.event_digest
        or event.kind != reference.kind
        or canonical_digest(event.payload) != reference.payload_digest
        or resolved.event_global_seq != reference.seq
    ):
        raise IntegrityError("verified event reference differs from receipt bytes")
    return resolved, event


def _parse_hypothesis(body: Mapping[str, Any]) -> Hypothesis:
    return Hypothesis(
        hypothesis_id=body["hypothesis_id"],
        version=body["version"],
        scope_digest=body["scope_digest"],
        claim_digest=body["claim_digest"],
        prediction_partition_digests=tuple(body["prediction_partition_digests"]),
        other_unknown_lane=body["other_unknown_lane"],
    )


def _parse_experiment(body: Mapping[str, Any]) -> DiscriminatingExperiment:
    signature_body = body["semantic_signature"]
    if not isinstance(signature_body, Mapping):
        raise TypeError("experiment semantic signature must be a mapping")
    signature = SemanticSignature(
        action_class=ActionClass(signature_body["action_class"]),
        tool_capability_digest=signature_body["tool_capability_digest"],
        resource_digest=signature_body["resource_digest"],
        parameter_region_digest=signature_body["parameter_region_digest"],
        precondition_set_digest=signature_body["precondition_set_digest"],
        read_set_digest=signature_body["read_set_digest"],
        world_epoch_digest=signature_body["world_epoch_digest"],
        tool_policy_digest=signature_body["tool_policy_digest"],
        model_policy_digest=signature_body["model_policy_digest"],
        prediction_partition_digests=tuple(
            signature_body["prediction_partition_digests"]
        ),
        stop_condition_digests=tuple(signature_body["stop_condition_digests"]),
        effect_class=EffectClass(signature_body["effect_class"]),
        canonicalizer_version=signature_body["canonicalizer_version"],
    )
    return DiscriminatingExperiment(
        experiment_id=body["experiment_id"],
        version=body["version"],
        context_packet_digest=body["context_packet_digest"],
        scope_digest=body["scope_digest"],
        semantic_signature=signature,
        hypothesis_digests=tuple(body["hypothesis_digests"]),
        predictions=tuple(
            ExperimentPrediction(
                hypothesis_digest=item["hypothesis_digest"],
                predicate_digest=item["predicate_digest"],
                outcome_partition_digest=item["outcome_partition_digest"],
            )
            for item in body["predictions"]
        ),
        estimated_cost_units=body["estimated_cost_units"],
    )


def _parse_h5_request(body: Mapping[str, Any]) -> H5RecommendationRequestV1:
    # The existing runtime assignment schema already rejects unresolved
    # equivalence/tombstone/reopen inputs.  Recheck here before typed replay.
    if (
        body.get("equivalence_proofs")
        or body.get("tombstones")
        or body.get("reopen_triggers")
    ):
        raise IntegrityError(
            "canonical selection refuses unresolved H5 suppression inputs"
        )
    return H5RecommendationRequestV1(
        hypotheses=tuple(_parse_hypothesis(item) for item in body["hypotheses"]),
        candidates=tuple(_parse_experiment(item) for item in body["candidates"]),
        prior_experiments=tuple(
            _parse_experiment(item) for item in body["prior_experiments"]
        ),
        covered_partition_digests=tuple(body["covered_partition_digests"]),
        duplicate_recommendations=tuple(
            PossibleDuplicateRecommendation(
                prior_signature_digest=item["prior_signature_digest"],
                candidate_signature_digest=item["candidate_signature_digest"],
                recommender_profile_digest=item["recommender_profile_digest"],
                rationale_digest=item["rationale_digest"],
            )
            for item in body["duplicate_recommendations"]
        ),
        schema_version=body["schema_version"],
    )


def reconstruct_resolved_cognitive_facts_v1(
    *,
    store: EpistemicSQLiteStore,
    resolver: CanonicalCommandReceiptResolverV1,
    prefix: VerifiedReceiptPrefixV1,
    scope_digest: str,
) -> tuple[ResolvedCognitiveFactV1, ...]:
    """Rebuild the exact in-scope resolver fact inventory from one prefix."""

    if type(store) is not EpistemicSQLiteStore:
        raise TypeError("store must be exactly EpistemicSQLiteStore")
    if type(resolver) is not CanonicalCommandReceiptResolverV1:
        raise TypeError("resolver must be CanonicalCommandReceiptResolverV1")
    if type(prefix) is not VerifiedReceiptPrefixV1:
        raise TypeError("prefix must be VerifiedReceiptPrefixV1")
    if not _is_digest(scope_digest):
        raise ValueError("scope_digest must be a lowercase sha256 digest")
    facts: list[ResolvedCognitiveFactV1] = []
    references = {item.event_digest: item for item in prefix.events}
    for resolution_reference in (
        item for item in prefix.events if item.kind == "COGNITIVE_VERIFICATION_RESOLVED"
    ):
        resolved, resolution_event = _resolve_reference(
            resolver=resolver,
            prefix=prefix,
            reference=resolution_reference,
        )
        source_digest = resolution_event.payload.get("source_assignment_event_digest")
        source_reference = references.get(source_digest)
        if (
            source_reference is None
            or source_reference.kind != COGNITIVE_EXPERIMENT_ASSIGNED
        ):
            raise IntegrityError(
                "resolver fact source assignment is outside the exact prefix"
            )
        _, source_event = _resolve_reference(
            resolver=resolver,
            prefix=prefix,
            reference=source_reference,
        )
        if source_event.actor != COGNITIVE_BINDING_ACTOR:
            raise IntegrityError("resolver fact source assignment actor diverged")
        # The run log may contain independent cognitive cycles.  Their facts
        # cannot poison this admission's model set.  Within one scope every fact
        # is independently replayed and none may be omitted by the caller.
        if source_event.payload.get("scope_digest") != scope_digest:
            continue
        validate_cognitive_verification_resolution_against_store(
            store, resolution_event.payload
        )
        experiment_body = source_event.payload.get("experiment_body")
        if not isinstance(experiment_body, Mapping):
            raise IntegrityError("resolver fact source experiment body is absent")
        try:
            source_experiment = _parse_experiment(experiment_body)
            fact = ResolvedCognitiveFactV1(
                prefix=prefix,
                resolved_event=resolved,
                source_experiment=source_experiment,
            )
        except (KeyError, TypeError, ValueError) as exc:
            raise IntegrityError("resolver fact does not replay from history") from exc
        facts.append(fact)
    return tuple(sorted(facts, key=lambda item: (item.seq, item.event.digest)))


def _fact_binding_body(fact: ResolvedCognitiveFactV1) -> dict[str, Any]:
    source_matches = tuple(
        item
        for item in fact.prefix.events
        if item.event_digest == fact.source_assignment_event_digest
        and item.kind == COGNITIVE_EXPERIMENT_ASSIGNED
    )
    if len(source_matches) != 1:
        raise ValueError("fact prefix lacks its exact source assignment")
    source = source_matches[0]
    return {
        "fact_body": fact.canonical_body(),
        "fact_digest": fact.digest,
        "resolution_event_digest": fact.event.digest,
        "resolution_event_id": fact.event.event_id,
        "resolution_event_payload": fact.event.payload,
        "resolution_event_payload_digest": canonical_digest(fact.event.payload),
        "resolution_event_receipt_digest": (fact.resolved_event.pointer.receipt_digest),
        "resolution_event_seq": fact.seq,
        "source_assignment_event_digest": source.event_digest,
        "source_assignment_event_receipt_digest": source.receipt_digest,
        "source_assignment_event_seq": source.seq,
        "source_experiment_body": fact.source_experiment.canonical_body(),
        "source_experiment_digest": fact.source_experiment.digest,
    }


def canonical_cycle_request_binding_body_v1(
    *,
    request: CanonicalCognitiveCycleRequestV1,
    prefix: VerifiedReceiptPrefixV1,
) -> dict[str, Any]:
    if type(request) is not CanonicalCognitiveCycleRequestV1:
        raise TypeError("request must be CanonicalCognitiveCycleRequestV1")
    if type(prefix) is not VerifiedReceiptPrefixV1:
        raise TypeError("prefix must be VerifiedReceiptPrefixV1")
    if any(fact.prefix.digest != prefix.digest for fact in request.resolved_facts):
        raise ValueError("every resolver fact must bind the exact decision prefix")
    masses = tuple(item.canonical_body() for item in request.initial_masses)
    facts = tuple(_fact_binding_body(item) for item in request.resolved_facts)
    costs = tuple(item.canonical_body() for item in request.cost_estimates)
    prefix_identity = {
        "cutoff_seq": prefix.cutoff_seq,
        "head_event_digest": prefix.head_event_digest,
        "prefix_digest": prefix.digest,
        "run_id": prefix.run_id,
    }
    return {
        "input_authority": _input_authority_labels(),
        "cost_estimates": costs,
        "cost_estimates_digest": canonical_digest(costs),
        "h5_request_body": request.h5_request.canonical_body(),
        "h5_request_digest": request.h5_request.digest,
        "initial_masses": masses,
        "initial_masses_digest": canonical_digest(masses),
        "pre_admission_prefix": prefix_identity,
        "remaining_cost_units": request.remaining_cost_units,
        "resolution_fact_set_digest": canonical_digest(facts),
        "resolution_facts": facts,
        "schema_id": COGNITIVE_CANONICAL_REQUEST_BINDING_SCHEMA_ID,
    }


def canonical_selection_sidecar_payload_v1(
    *,
    request: CanonicalCognitiveCycleRequestV1,
    plan: CanonicalCognitiveCyclePlanV1,
    prefix: VerifiedReceiptPrefixV1,
    admission_payload: Mapping[str, Any],
    assignment_payload: Mapping[str, Any],
    assignment_event_digest: str,
) -> dict[str, Any]:
    if type(plan) is not CanonicalCognitiveCyclePlanV1:
        raise TypeError("plan must be CanonicalCognitiveCyclePlanV1")
    if plan.mode is not CanonicalCognitiveCycleModeV1.EXPERIMENT:
        raise ValueError("canonical v1 sidecar accepts only EXPERIMENT mode")
    replay = plan_canonical_cognitive_cycle_v1(request)
    if replay != plan or plan.next_assignment is None:
        raise ValueError("canonical plan must replay to one next assignment")
    request_body = canonical_cycle_request_binding_body_v1(
        request=request, prefix=prefix
    )
    plan_body = plan.canonical_body()
    selected_body = plan.next_assignment.canonical_body()
    assignment_event = {
        "actor": COGNITIVE_BINDING_ACTOR,
        "event_digest": assignment_event_digest,
        "event_id": assignment_payload["base_event_id"].replace(
            "event:attempt:admit:", f"event:{COGNITIVE_EXPERIMENT_ASSIGNED}:"
        ),
        "kind": COGNITIVE_EXPERIMENT_ASSIGNED,
        "ordinal": 1,
        "payload": assignment_payload,
        "payload_digest": canonical_digest(assignment_payload),
    }
    payload = {
        "accepted_set_change": ACCEPTED_SET_CHANGE,
        "admission_event_id": assignment_payload["base_event_id"],
        "admission_payload_digest": canonical_digest(admission_payload),
        "assignment_event": assignment_event,
        "attempt_id": admission_payload["attempt_id"],
        "authority_effect": AUTHORITY_EFFECT_NONE,
        "automatic_redispatch_permitted": AUTOMATIC_REDISPATCH_PERMITTED,
        "canonical_plan_body": plan_body,
        "canonical_plan_digest": plan.digest,
        "canonical_request_body": request_body,
        "canonical_request_digest": canonical_digest(request_body),
        "dispatch_authority": False,
        "effect_authority": False,
        "gate_authority": False,
        "permit_id": admission_payload["permit_id"],
        "production_enabled": PRODUCTION_ENABLED,
        "provenance_gate_accepted_set": PROVENANCE_GATE_ACCEPTED_SET,
        "schema_id": COGNITIVE_CANONICAL_SELECTION_SCHEMA_ID,
        "scope_digest": admission_payload["scope_digest"],
        "selection_semantics": SELECTION_SEMANTICS,
        "selected_assignment_body": selected_body,
        "selected_assignment_digest": plan.next_assignment.digest,
    }
    validate_canonical_selection_sidecar_shape(payload)
    return payload


_SIDECAR_FIELDS = frozenset(
    {
        "accepted_set_change",
        "admission_event_id",
        "admission_payload_digest",
        "assignment_event",
        "attempt_id",
        "authority_effect",
        "automatic_redispatch_permitted",
        "canonical_plan_body",
        "canonical_plan_digest",
        "canonical_request_body",
        "canonical_request_digest",
        "dispatch_authority",
        "effect_authority",
        "gate_authority",
        "permit_id",
        "production_enabled",
        "provenance_gate_accepted_set",
        "schema_id",
        "scope_digest",
        "selection_semantics",
        "selected_assignment_body",
        "selected_assignment_digest",
    }
)


def _is_digest(value: object) -> bool:
    return (
        type(value) is str
        and len(value) == 64
        and all(character in "0123456789abcdef" for character in value)
    )


def _current_context_packet_digest_from_assignment(
    store: EpistemicSQLiteStore,
    assignment_payload: Mapping[str, Any],
) -> str:
    """Resolve the packet from either ordinary or eval-v2 admission lineage."""

    packet_binding = assignment_payload.get("context_packet_binding_body")
    packet_digest = (
        packet_binding.get("packet_digest")
        if isinstance(packet_binding, Mapping)
        else None
    )
    if _is_digest(packet_digest):
        return packet_digest
    if assignment_payload.get("schema_id") != COGNITIVE_ASSIGNMENT_SCHEMA_ID:
        raise IntegrityError("canonical cognitive assignment has no current packet")
    sidecar_id = assignment_payload.get("evaluation_sidecar_event_id")
    sidecar_payload_digest = assignment_payload.get(
        "evaluation_sidecar_payload_digest"
    )
    rows = tuple(
        row
        for row in store.event_rows(kind="C6_EVAL_V2_ATTEMPT_BOUND")
        if row["event_id"] == sidecar_id
    )
    if len(rows) != 1:
        raise IntegrityError("canonical evaluation sidecar is absent or ambiguous")
    sidecar = rows[0]["payload"]
    runtime_body = sidecar.get("runtime_binding")
    if (
        canonical_digest(sidecar) != sidecar_payload_digest
        or not isinstance(runtime_body, Mapping)
    ):
        raise IntegrityError("canonical evaluation sidecar payload was rebound")
    try:
        runtime = RuntimeEvaluationBindingV2.from_canonical(runtime_body)
    except (TypeError, ValueError) as exc:
        raise IntegrityError("canonical evaluation runtime binding is false") from exc
    if (
        runtime.digest != sidecar.get("runtime_binding_digest")
        or runtime.role != "executor"
        or runtime.assignment_binding_digest
        != assignment_payload.get("assignment_binding_digest")
        or runtime.attempt_role_binding_digest
        != assignment_payload.get("attempt_role_binding_digest")
        or runtime.attempt_id != assignment_payload.get("attempt_id")
        or runtime.permit_digest != assignment_payload.get("permit_digest")
        or runtime.scope_digest != assignment_payload.get("scope_digest")
    ):
        raise IntegrityError("canonical evaluation runtime binding is cross-spliced")
    input_spec = runtime.attempt_role_body.get("input_spec")
    if (
        not isinstance(input_spec, Mapping)
        or input_spec.get("kind") != "candidate_context_packet"
        or not _is_digest(input_spec.get("context_packet_digest"))
    ):
        raise IntegrityError("canonical evaluation executor lacks a candidate packet")
    return str(input_spec["context_packet_digest"])


def validate_canonical_selection_sidecar_shape(
    payload: Mapping[str, Any],
) -> None:
    if not isinstance(payload, Mapping) or set(payload) != _SIDECAR_FIELDS:
        raise ValueError("canonical selection sidecar shape is not versioned")
    if (
        payload["schema_id"] != COGNITIVE_CANONICAL_SELECTION_SCHEMA_ID
        or payload["authority_effect"] != AUTHORITY_EFFECT_NONE
        or payload["accepted_set_change"] is not False
        or payload["automatic_redispatch_permitted"] is not False
        or payload["production_enabled"] is not False
        or payload["provenance_gate_accepted_set"] != PROVENANCE_GATE_ACCEPTED_SET
        or payload["dispatch_authority"] is not False
        or payload["effect_authority"] is not False
        or payload["gate_authority"] is not False
        or payload["selection_semantics"] != SELECTION_SEMANTICS
    ):
        raise ValueError("canonical selection sidecar overclaims authority")
    for name in (
        "admission_payload_digest",
        "canonical_plan_digest",
        "canonical_request_digest",
        "scope_digest",
        "selected_assignment_digest",
    ):
        if not _is_digest(payload[name]):
            raise ValueError(f"{name} must be a lowercase sha256 digest")
    for name in ("admission_event_id", "attempt_id", "permit_id"):
        if type(payload[name]) is not str or not payload[name]:
            raise ValueError(f"{name} must be exact non-empty text")
    assignment = payload["assignment_event"]
    if not isinstance(assignment, Mapping) or set(assignment) != {
        "actor",
        "event_digest",
        "event_id",
        "kind",
        "ordinal",
        "payload",
        "payload_digest",
    }:
        raise ValueError("canonical selection assignment identity is malformed")
    if (
        assignment["actor"] != COGNITIVE_BINDING_ACTOR
        or assignment["kind"] != COGNITIVE_EXPERIMENT_ASSIGNED
        or assignment["ordinal"] != 1
        or not _is_digest(assignment["event_digest"])
        or not _is_digest(assignment["payload_digest"])
        or canonical_digest(assignment["payload"]) != assignment["payload_digest"]
    ):
        raise ValueError("canonical selection assignment identity diverged")
    request_body = payload["canonical_request_body"]
    if (
        not isinstance(request_body, Mapping)
        or request_body.get("schema_id")
        != COGNITIVE_CANONICAL_REQUEST_BINDING_SCHEMA_ID
        or canonical_digest(request_body) != payload["canonical_request_digest"]
        or canonical_digest(payload["canonical_plan_body"])
        != payload["canonical_plan_digest"]
        or canonical_digest(payload["selected_assignment_body"])
        != payload["selected_assignment_digest"]
        or request_body.get("input_authority") != _input_authority_labels()
    ):
        raise ValueError("canonical selection request/plan digest is false")


def _request_from_binding_body(
    body: Mapping[str, Any],
    *,
    facts: tuple[ResolvedCognitiveFactV1, ...],
) -> CanonicalCognitiveCycleRequestV1:
    try:
        return CanonicalCognitiveCycleRequestV1(
            h5_request=_parse_h5_request(body["h5_request_body"]),
            initial_masses=tuple(
                HypothesisMassV1(
                    hypothesis_digest=item["hypothesis_digest"],
                    weight_units=item["weight_units"],
                )
                for item in body["initial_masses"]
            ),
            resolved_facts=facts,
            cost_estimates=tuple(
                SuppliedCostEstimateV1(
                    experiment_digest=item["experiment_digest"],
                    cost_units=item["cost_units"],
                )
                for item in body["cost_estimates"]
            ),
            remaining_cost_units=body["remaining_cost_units"],
        )
    except (KeyError, TypeError, ValueError) as exc:
        raise IntegrityError(
            "canonical selection request body does not replay"
        ) from exc


def validate_canonical_selection_against_store(
    store: EpistemicSQLiteStore,
    payload: Mapping[str, Any],
) -> None:
    """Store-owned semantic CAS for the canonical selection sidecar."""

    try:
        validate_canonical_selection_sidecar_shape(payload)
    except (TypeError, ValueError) as exc:
        raise IntegrityError("canonical selection sidecar payload is false") from exc
    p = dict(payload)
    claimed_plan = p.get("canonical_plan_body")
    if (
        not isinstance(claimed_plan, Mapping)
        or claimed_plan.get("mode") != CanonicalCognitiveCycleModeV1.EXPERIMENT.value
    ):
        raise IntegrityError("canonical v1 store guard accepts only EXPERIMENT mode")
    assignment_claim = p["assignment_event"]
    own_rows = tuple(
        row
        for row in store.event_rows(kind=COGNITIVE_CANONICAL_SELECTION_BOUND)
        if canonical_json_bytes(row["payload"]) == canonical_json_bytes(p)
    )
    assignment_rows = tuple(
        row
        for row in store.event_rows(kind=COGNITIVE_EXPERIMENT_ASSIGNED)
        if row["event_digest"] == assignment_claim["event_digest"]
    )
    admission_rows = tuple(
        row
        for row in store.event_rows(kind="ATTEMPT_ADMITTED")
        if row["event_id"] == p["admission_event_id"]
    )
    if len(own_rows) != 1 or len(assignment_rows) != 1 or len(admission_rows) != 1:
        raise IntegrityError("canonical selection atomic lineage is incomplete")
    own = own_rows[0]
    assignment = assignment_rows[0]
    admission = admission_rows[0]
    command_rows = store._conn.execute(
        "SELECT command_id,ordinal FROM events WHERE event_digest IN (?,?,?)",
        (admission["event_digest"], assignment["event_digest"], own["event_digest"]),
    ).fetchall()
    if (
        len(command_rows) != 3
        or len({row[0] for row in command_rows}) != 1
        or sorted(int(row[1]) for row in command_rows) != [0, 1, 2]
        or own["seq"] != assignment["seq"] + 1
        or assignment["seq"] != admission["seq"] + 1
        or assignment["event_id"] != assignment_claim["event_id"]
        or canonical_json_bytes(assignment["payload"])
        != canonical_json_bytes(assignment_claim["payload"])
        or canonical_digest(admission["payload"]) != p["admission_payload_digest"]
    ):
        raise IntegrityError("canonical selection is not atomic with exact admission")
    for name in ("attempt_id", "permit_id", "scope_digest"):
        if p[name] != admission["payload"].get(name) or p[name] != assignment[
            "payload"
        ].get(name):
            raise IntegrityError("canonical selection admission identity diverged")

    state = store._state()
    request_body = p["canonical_request_body"]
    prefix_claim = request_body.get("pre_admission_prefix")
    if not isinstance(prefix_claim, Mapping) or (
        prefix_claim.get("run_id") != store.run_id
        or prefix_claim.get("cutoff_seq") != state.head_seq
        or prefix_claim.get("head_event_digest") != state.head_event_digest
    ):
        raise IntegrityError("canonical selection used a stale pre-admission prefix")
    resolver = store.receipt_field_resolver(cutoff_seq=state.head_seq)
    prefix = resolver.verify_complete_through(state.head_seq)
    if prefix_claim.get("prefix_digest") != prefix.digest:
        raise IntegrityError("canonical selection prefix is not store-owned")
    facts = reconstruct_resolved_cognitive_facts_v1(
        store=store,
        resolver=resolver,
        prefix=prefix,
        scope_digest=p["scope_digest"],
    )
    request = _request_from_binding_body(request_body, facts=facts)
    h5_request = request.h5_request
    if (
        any(
            hypothesis.scope_digest != p["scope_digest"]
            for hypothesis in h5_request.hypotheses
        )
        or any(
            experiment.scope_digest != p["scope_digest"]
            for experiment in h5_request.candidates
        )
        or any(
            experiment.scope_digest != p["scope_digest"]
            for experiment in h5_request.prior_experiments
        )
    ):
        raise IntegrityError(
            "canonical H5 hypotheses, candidates, and prior experiments must "
            "share the admission scope"
        )
    assignment_payload = assignment["payload"]
    current_packet_digest = _current_context_packet_digest_from_assignment(
        store, assignment_payload
    )
    if any(
        candidate.context_packet_digest != current_packet_digest
        for candidate in h5_request.candidates
    ):
        raise IntegrityError(
            "every canonical H5 candidate must bind the current ContextPacket"
        )
    expected_request_body = canonical_cycle_request_binding_body_v1(
        request=request,
        prefix=prefix,
    )
    if canonical_json_bytes(expected_request_body) != canonical_json_bytes(
        request_body
    ):
        raise IntegrityError(
            "canonical selection request or resolver fact inventory diverged"
        )
    plan = plan_canonical_cognitive_cycle_v1(request)
    if (
        plan.mode is not CanonicalCognitiveCycleModeV1.EXPERIMENT
        or plan.next_assignment is None
    ):
        raise IntegrityError(
            "canonical cycle is not an EXPERIMENT assignment admissible by v1"
        )
    if (
        canonical_json_bytes(plan.canonical_body())
        != canonical_json_bytes(p["canonical_plan_body"])
        or plan.digest != p["canonical_plan_digest"]
        or canonical_json_bytes(plan.next_assignment.canonical_body())
        != canonical_json_bytes(p["selected_assignment_body"])
        or plan.next_assignment.digest != p["selected_assignment_digest"]
    ):
        raise IntegrityError("canonical selection plan does not replay")
    h5_plan = HypothesisSelector.recommend(request.h5_request)
    if (
        canonical_json_bytes(assignment_payload.get("assignment_body"))
        != canonical_json_bytes(plan.next_assignment.canonical_body())
        or assignment_payload.get("assignment_digest") != plan.next_assignment.digest
        or canonical_json_bytes(assignment_payload.get("h5_request_body"))
        != canonical_json_bytes(request.h5_request.canonical_body())
        or assignment_payload.get("h5_request_digest") != request.h5_request.digest
        or canonical_json_bytes(assignment_payload.get("h5_selection_plan_body"))
        != canonical_json_bytes(h5_plan.canonical_body())
        or assignment_payload.get("h5_selection_plan_digest") != h5_plan.digest
        or assignment_payload.get("decision_prefix_digest") != prefix.digest
        or assignment_payload.get("decision_cutoff_seq") != prefix.cutoff_seq
        or assignment_payload.get("decision_head_event_digest")
        != prefix.head_event_digest
    ):
        raise IntegrityError(
            "admitted assignment is not the exact canonical next assignment"
        )
    selected_experiment = next(
        (
            item
            for item in request.h5_request.candidates
            if item.digest == plan.next_assignment.experiment_digest
        ),
        None,
    )
    if selected_experiment is None or canonical_json_bytes(
        assignment_payload.get("experiment_body")
    ) != canonical_json_bytes(selected_experiment.canonical_body()):
        raise IntegrityError("canonical selected experiment body was rebound")

    # A pure plan is consumable once per run.  Idempotent retry sees only this
    # one row; a later fresh admission cannot silently redispatch the same plan.
    same_plan = tuple(
        row
        for row in store.event_rows(kind=COGNITIVE_CANONICAL_SELECTION_BOUND)
        if row["payload"].get("canonical_plan_digest") == p["canonical_plan_digest"]
    )
    if len(same_plan) != 1 or same_plan[0]["event_digest"] != own["event_digest"]:
        raise IntegrityError("canonical cognitive plan was already consumed")


__all__ = [
    "ACCEPTED_SET_CHANGE",
    "AUTHORITY_EFFECT_NONE",
    "AUTOMATIC_REDISPATCH_PERMITTED",
    "CANDIDATE_SET_AUTHORITY",
    "COGNITIVE_CANONICAL_REQUEST_BINDING_SCHEMA_ID",
    "COGNITIVE_CANONICAL_SELECTION_ACTOR",
    "COGNITIVE_CANONICAL_SELECTION_BOUND",
    "COGNITIVE_CANONICAL_SELECTION_SCHEMA_ID",
    "COST_ESTIMATE_AUTHORITY",
    "PRODUCTION_ENABLED",
    "PROVENANCE_GATE_ACCEPTED_SET",
    "REMAINING_COST_AUTHORITY",
    "RESOLVER_FACT_AUTHORITY",
    "SELECTION_SEMANTICS",
    "canonical_cycle_request_binding_body_v1",
    "canonical_selection_sidecar_payload_v1",
    "reconstruct_resolved_cognitive_facts_v1",
    "validate_canonical_selection_against_store",
    "validate_canonical_selection_sidecar_shape",
]
