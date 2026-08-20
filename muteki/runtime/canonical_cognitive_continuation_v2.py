"""Atomic admission companion for a distinct experiment after HELD_UNKNOWN.

This is a versioned, default-off admission guard.  It does not turn the
canonical planner into an authority: candidates, cost estimates and the scalar
remaining-cost limit are still frozen caller proposals.  The store owns only
the exact receipt prefix and resolver facts, then reruns the pure planner over
those inputs before it accepts one ordinary ContextPacket-bound assignment.

The v1 EXPERIMENT sidecar deliberately remains separate.  This module accepts
only ``CONTINUE_DISTINCT_EXPERIMENT`` and preserves every HELD_UNKNOWN fact as
non-learning history.  A successful commit grants no dispatch, effect, gate,
retry, or production authority.
"""

from __future__ import annotations

from collections.abc import Mapping
from typing import Any

from muteki.epistemic.cognitive_events_v1 import (
    COGNITIVE_BINDING_ACTOR,
    COGNITIVE_EXPERIMENT_ASSIGNED,
)
from muteki.epistemic.contracts import canonical_digest, canonical_json_bytes
from muteki.epistemic.receipt_objects import VerifiedReceiptPrefixV1
from muteki.epistemic.sqlite_store import EpistemicSQLiteStore, IntegrityError
from muteki.runtime.canonical_cognitive_cycle_v1 import (
    CanonicalCognitiveCycleModeV1,
    CanonicalCognitiveCyclePlanV1,
    CanonicalCognitiveCycleRequestV1,
    ResolvedCognitiveFactV1,
    ResolvedCognitiveFactStatusV1,
    plan_canonical_cognitive_cycle_v1,
)
from muteki.runtime.canonical_cognitive_selection_v1 import (
    CANDIDATE_SET_AUTHORITY,
    COST_ESTIMATE_AUTHORITY,
    REMAINING_COST_AUTHORITY,
    RESOLVER_FACT_AUTHORITY,
    _current_context_packet_digest_from_assignment,
    _is_digest,
    _request_from_binding_body,
    canonical_cycle_request_binding_body_v1,
    reconstruct_resolved_cognitive_facts_v1,
)
from muteki.runtime.cognitive_planning_contracts_v1 import (
    _typed_program_fingerprint,
)
from muteki.runtime.hypothesis import HypothesisSelector


COGNITIVE_CANONICAL_CONTINUATION_BOUND_V2 = "COGNITIVE_CANONICAL_CONTINUATION_BOUND_V2"
COGNITIVE_CANONICAL_CONTINUATION_ACTOR_V2 = (
    "cognitive-canonical-continuation-binding-v2-authority"
)
COGNITIVE_CANONICAL_CONTINUATION_SCHEMA_ID_V2 = (
    "muteki.cognitive-canonical-continuation-bound.v2"
)
COGNITIVE_CANONICAL_CONTINUATION_MUTATION_V2 = (
    "cognitive_canonical_continuation_bind_guard_v2"
)
AUTHORITY_EFFECT_NONE = "NONE"
PRODUCTION_ENABLED = False
ACCEPTED_SET_CHANGE = False
AUTOMATIC_REDISPATCH_PERMITTED = False
PROVENANCE_GATE_ACCEPTED_SET = "UNCHANGED"
CONTINUATION_SEMANTICS = (
    "EXACT_DISTINCT_TYPED_PROGRAM_AFTER_STORE_RESOLVED_HELD_UNKNOWN"
)
HELD_FACT_SEMANTICS = "PRESERVED_NON_LEARNING"


def _input_authority_labels() -> dict[str, str]:
    return {
        "candidate_set": CANDIDATE_SET_AUTHORITY,
        "cost_estimates": COST_ESTIMATE_AUTHORITY,
        "remaining_cost_units": REMAINING_COST_AUTHORITY,
        "resolver_facts": RESOLVER_FACT_AUTHORITY,
    }


def _held_unknown_facts(
    request: CanonicalCognitiveCycleRequestV1,
    plan: CanonicalCognitiveCyclePlanV1,
) -> tuple[ResolvedCognitiveFactV1, ...]:
    held_digests = set(plan.belief.held_fact_digests)
    held = tuple(item for item in request.resolved_facts if item.digest in held_digests)
    if (
        not held
        or {item.digest for item in held} != held_digests
        or any(
            item.status is not ResolvedCognitiveFactStatusV1.HELD_UNKNOWN
            or item.learning_eligible
            for item in held
        )
    ):
        raise ValueError(
            "continuation requires exact store-resolved HELD_UNKNOWN non-learning facts"
        )
    return held


def _continuation_identity(
    request: CanonicalCognitiveCycleRequestV1,
    plan: CanonicalCognitiveCyclePlanV1,
) -> tuple[tuple[ResolvedCognitiveFactV1, ...], str]:
    if (
        plan.mode is not CanonicalCognitiveCycleModeV1.CONTINUE_DISTINCT_EXPERIMENT
        or plan.next_assignment is None
    ):
        raise ValueError(
            "canonical v2 continuation accepts only CONTINUE_DISTINCT_EXPERIMENT"
        )
    held = _held_unknown_facts(request, plan)
    selected = next(
        (
            item
            for item in request.h5_request.candidates
            if item.digest == plan.next_assignment.experiment_digest
        ),
        None,
    )
    if selected is None:
        raise ValueError("continuation selected experiment is absent")
    selected_fingerprint = _typed_program_fingerprint(selected)
    attempted = set(plan.belief.attempted_program_fingerprints)
    if selected_fingerprint in attempted or any(
        selected_fingerprint == _typed_program_fingerprint(item.source_experiment)
        for item in held
    ):
        raise ValueError("continuation selected an attempted typed-program alias")
    return held, selected_fingerprint


def canonical_continuation_sidecar_payload_v2(
    *,
    request: CanonicalCognitiveCycleRequestV1,
    plan: CanonicalCognitiveCyclePlanV1,
    prefix: VerifiedReceiptPrefixV1,
    admission_payload: Mapping[str, Any],
    assignment_payload: Mapping[str, Any],
    assignment_event_digest: str,
) -> dict[str, Any]:
    """Build the inert v2 companion for one exact continuation assignment."""

    if type(request) is not CanonicalCognitiveCycleRequestV1:
        raise TypeError("request must be CanonicalCognitiveCycleRequestV1")
    if type(plan) is not CanonicalCognitiveCyclePlanV1:
        raise TypeError("plan must be CanonicalCognitiveCyclePlanV1")
    replay = plan_canonical_cognitive_cycle_v1(request)
    if replay != plan:
        raise ValueError("canonical continuation plan must replay exactly")
    held, selected_fingerprint = _continuation_identity(request, plan)
    request_body = canonical_cycle_request_binding_body_v1(
        request=request,
        prefix=prefix,
    )
    if request_body.get("input_authority") != _input_authority_labels():
        raise ValueError("canonical request authority labels diverged")
    plan_body = plan.canonical_body()
    assert plan.next_assignment is not None
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
        "attempted_program_fingerprints": (plan.belief.attempted_program_fingerprints),
        "authority_effect": AUTHORITY_EFFECT_NONE,
        "automatic_redispatch_permitted": AUTOMATIC_REDISPATCH_PERMITTED,
        "canonical_plan_body": plan_body,
        "canonical_plan_digest": plan.digest,
        "canonical_request_body": request_body,
        "canonical_request_digest": canonical_digest(request_body),
        "continuation_semantics": CONTINUATION_SEMANTICS,
        "dispatch_authority": False,
        "effect_authority": False,
        "gate_authority": False,
        "held_fact_digests": tuple(item.digest for item in held),
        "held_fact_semantics": HELD_FACT_SEMANTICS,
        "held_source_experiment_digests": tuple(
            item.source_experiment.digest for item in held
        ),
        "permit_id": admission_payload["permit_id"],
        "planner_command_count": 0,
        "production_enabled": PRODUCTION_ENABLED,
        "provenance_gate_accepted_set": PROVENANCE_GATE_ACCEPTED_SET,
        "recommendation_only": True,
        "schema_id": COGNITIVE_CANONICAL_CONTINUATION_SCHEMA_ID_V2,
        "scope_digest": admission_payload["scope_digest"],
        "selected_assignment_body": selected_body,
        "selected_assignment_digest": plan.next_assignment.digest,
        "selected_program_fingerprint": selected_fingerprint,
    }
    validate_canonical_continuation_sidecar_shape_v2(payload)
    return payload


_SIDECAR_FIELDS_V2 = frozenset(
    {
        "accepted_set_change",
        "admission_event_id",
        "admission_payload_digest",
        "assignment_event",
        "attempt_id",
        "attempted_program_fingerprints",
        "authority_effect",
        "automatic_redispatch_permitted",
        "canonical_plan_body",
        "canonical_plan_digest",
        "canonical_request_body",
        "canonical_request_digest",
        "continuation_semantics",
        "dispatch_authority",
        "effect_authority",
        "gate_authority",
        "held_fact_digests",
        "held_fact_semantics",
        "held_source_experiment_digests",
        "permit_id",
        "planner_command_count",
        "production_enabled",
        "provenance_gate_accepted_set",
        "recommendation_only",
        "schema_id",
        "scope_digest",
        "selected_assignment_body",
        "selected_assignment_digest",
        "selected_program_fingerprint",
    }
)


def _digest_tuple(value: object, name: str, *, nonempty: bool) -> tuple[str, ...]:
    if type(value) not in {tuple, list} or (nonempty and not value):
        raise ValueError(f"{name} must be a canonical digest tuple")
    if any(not _is_digest(item) for item in value):
        raise ValueError(f"{name} contains a non-digest")
    return tuple(value)


def validate_canonical_continuation_sidecar_shape_v2(
    payload: Mapping[str, Any],
) -> None:
    if not isinstance(payload, Mapping) or set(payload) != _SIDECAR_FIELDS_V2:
        raise ValueError("canonical continuation sidecar shape is not v2")
    if (
        payload["schema_id"] != COGNITIVE_CANONICAL_CONTINUATION_SCHEMA_ID_V2
        or payload["authority_effect"] != AUTHORITY_EFFECT_NONE
        or payload["accepted_set_change"] is not False
        or payload["automatic_redispatch_permitted"] is not False
        or payload["production_enabled"] is not False
        or payload["provenance_gate_accepted_set"] != PROVENANCE_GATE_ACCEPTED_SET
        or payload["dispatch_authority"] is not False
        or payload["effect_authority"] is not False
        or payload["gate_authority"] is not False
        or payload["recommendation_only"] is not True
        or payload["planner_command_count"] != 0
        or payload["continuation_semantics"] != CONTINUATION_SEMANTICS
        or payload["held_fact_semantics"] != HELD_FACT_SEMANTICS
    ):
        raise ValueError("canonical continuation sidecar overclaims authority")
    for name in (
        "admission_payload_digest",
        "canonical_plan_digest",
        "canonical_request_digest",
        "scope_digest",
        "selected_assignment_digest",
        "selected_program_fingerprint",
    ):
        if not _is_digest(payload[name]):
            raise ValueError(f"{name} must be a lowercase sha256 digest")
    for name in ("admission_event_id", "attempt_id", "permit_id"):
        if type(payload[name]) is not str or not payload[name]:
            raise ValueError(f"{name} must be exact non-empty text")
    _digest_tuple(
        payload["attempted_program_fingerprints"],
        "attempted_program_fingerprints",
        nonempty=True,
    )
    held = _digest_tuple(
        payload["held_fact_digests"], "held_fact_digests", nonempty=True
    )
    sources = _digest_tuple(
        payload["held_source_experiment_digests"],
        "held_source_experiment_digests",
        nonempty=True,
    )
    if len(held) != len(sources):
        raise ValueError("held fact/source identity counts diverged")
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
        raise ValueError("canonical continuation assignment identity is malformed")
    if (
        assignment["actor"] != COGNITIVE_BINDING_ACTOR
        or assignment["kind"] != COGNITIVE_EXPERIMENT_ASSIGNED
        or assignment["ordinal"] != 1
        or not _is_digest(assignment["event_digest"])
        or not _is_digest(assignment["payload_digest"])
        or canonical_digest(assignment["payload"]) != assignment["payload_digest"]
    ):
        raise ValueError("canonical continuation assignment identity diverged")
    request_body = payload["canonical_request_body"]
    plan_body = payload["canonical_plan_body"]
    if (
        not isinstance(request_body, Mapping)
        or request_body.get("schema_id")
        != "muteki.canonical-cognitive-cycle-request-binding.v1"
        or request_body.get("input_authority") != _input_authority_labels()
        or not isinstance(plan_body, Mapping)
        or plan_body.get("mode")
        != CanonicalCognitiveCycleModeV1.CONTINUE_DISTINCT_EXPERIMENT.value
        or canonical_digest(request_body) != payload["canonical_request_digest"]
        or canonical_digest(plan_body) != payload["canonical_plan_digest"]
        or canonical_digest(payload["selected_assignment_body"])
        != payload["selected_assignment_digest"]
    ):
        raise ValueError("canonical continuation request/plan digest is false")


def validate_canonical_continuation_against_store_v2(
    store: EpistemicSQLiteStore,
    payload: Mapping[str, Any],
) -> None:
    """Store-owned semantic CAS for the v2 continuation companion."""

    try:
        validate_canonical_continuation_sidecar_shape_v2(payload)
    except (TypeError, ValueError) as exc:
        raise IntegrityError("canonical continuation sidecar payload is false") from exc
    p = dict(payload)
    assignment_claim = p["assignment_event"]
    own_rows = tuple(
        row
        for row in store.event_rows(kind=COGNITIVE_CANONICAL_CONTINUATION_BOUND_V2)
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
        raise IntegrityError("canonical continuation atomic lineage is incomplete")
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
        raise IntegrityError("canonical continuation is not one atomic admission")
    for name in ("attempt_id", "permit_id", "scope_digest"):
        if p[name] != admission["payload"].get(name) or p[name] != assignment[
            "payload"
        ].get(name):
            raise IntegrityError("canonical continuation admission identity diverged")

    state = store._state()
    request_body = p["canonical_request_body"]
    prefix_claim = request_body.get("pre_admission_prefix")
    if not isinstance(prefix_claim, Mapping) or (
        prefix_claim.get("run_id") != store.run_id
        or prefix_claim.get("cutoff_seq") != state.head_seq
        or prefix_claim.get("head_event_digest") != state.head_event_digest
    ):
        raise IntegrityError("canonical continuation used a stale decision prefix")
    resolver = store.receipt_field_resolver(cutoff_seq=state.head_seq)
    prefix = resolver.verify_complete_through(state.head_seq)
    if prefix_claim.get("prefix_digest") != prefix.digest:
        raise IntegrityError("canonical continuation prefix is not store-owned")
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
            "canonical continuation hypotheses, candidates, and prior experiments "
            "must share the admission scope"
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
            "every continuation candidate must bind the current ContextPacket"
        )
    expected_request_body = canonical_cycle_request_binding_body_v1(
        request=request,
        prefix=prefix,
    )
    if canonical_json_bytes(expected_request_body) != canonical_json_bytes(
        request_body
    ):
        raise IntegrityError(
            "canonical continuation request or resolver fact inventory diverged"
        )
    plan = plan_canonical_cognitive_cycle_v1(request)
    try:
        held, selected_fingerprint = _continuation_identity(request, plan)
    except ValueError as exc:
        raise IntegrityError("canonical continuation is not distinct") from exc
    if (
        canonical_json_bytes(plan.canonical_body())
        != canonical_json_bytes(p["canonical_plan_body"])
        or plan.digest != p["canonical_plan_digest"]
        or plan.next_assignment is None
        or canonical_json_bytes(plan.next_assignment.canonical_body())
        != canonical_json_bytes(p["selected_assignment_body"])
        or plan.next_assignment.digest != p["selected_assignment_digest"]
        or tuple(item.digest for item in held) != p["held_fact_digests"]
        or tuple(item.source_experiment.digest for item in held)
        != p["held_source_experiment_digests"]
        or plan.belief.attempted_program_fingerprints
        != p["attempted_program_fingerprints"]
        or selected_fingerprint != p["selected_program_fingerprint"]
    ):
        raise IntegrityError("canonical continuation plan does not replay")
    h5_plan = HypothesisSelector.recommend(request.h5_request)
    selected_experiment = next(
        (
            item
            for item in request.h5_request.candidates
            if item.digest == plan.next_assignment.experiment_digest
        ),
        None,
    )
    if (
        selected_experiment is None
        or canonical_json_bytes(assignment_payload.get("assignment_body"))
        != canonical_json_bytes(plan.next_assignment.canonical_body())
        or assignment_payload.get("assignment_digest") != plan.next_assignment.digest
        or canonical_json_bytes(assignment_payload.get("experiment_body"))
        != canonical_json_bytes(selected_experiment.canonical_body())
        or assignment_payload.get("experiment_digest") != selected_experiment.digest
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
            "admitted continuation is not the exact canonical next assignment"
        )

    same_plan = tuple(
        row
        for row in store.event_rows(kind=COGNITIVE_CANONICAL_CONTINUATION_BOUND_V2)
        if row["payload"].get("canonical_plan_digest") == p["canonical_plan_digest"]
    )
    if len(same_plan) != 1 or same_plan[0]["event_digest"] != own["event_digest"]:
        raise IntegrityError("canonical continuation plan was already consumed")


__all__ = [
    "ACCEPTED_SET_CHANGE",
    "AUTHORITY_EFFECT_NONE",
    "AUTOMATIC_REDISPATCH_PERMITTED",
    "COGNITIVE_CANONICAL_CONTINUATION_ACTOR_V2",
    "COGNITIVE_CANONICAL_CONTINUATION_BOUND_V2",
    "COGNITIVE_CANONICAL_CONTINUATION_MUTATION_V2",
    "COGNITIVE_CANONICAL_CONTINUATION_SCHEMA_ID_V2",
    "CONTINUATION_SEMANTICS",
    "HELD_FACT_SEMANTICS",
    "PRODUCTION_ENABLED",
    "PROVENANCE_GATE_ACCEPTED_SET",
    "canonical_continuation_sidecar_payload_v2",
    "validate_canonical_continuation_against_store_v2",
    "validate_canonical_continuation_sidecar_shape_v2",
]
