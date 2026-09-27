"""Cognitive assignment and observation authority checks."""

from __future__ import annotations

from muteki.epistemic.contracts import canonical_json_bytes
from muteki.epistemic.sqlite_authority_types import (
    _AuthorityCapabilities,
    _AuthorityInventory,
)
from muteki.epistemic.sqlite_types import IntegrityError


def _validate_canonical_selection_authority(
    inventory: _AuthorityInventory,
    capabilities: _AuthorityCapabilities,
) -> None:
    from muteki.epistemic.cognitive_events_v1 import (
        COGNITIVE_EXPERIMENT_ASSIGNED,
        COGNITIVE_RUNTIME_CONTEXT_ASSIGNMENT_SCHEMA_ID,
    )
    from muteki.runtime.canonical_cognitive_selection_v1 import (
        COGNITIVE_CANONICAL_SELECTION_ACTOR,
        COGNITIVE_CANONICAL_SELECTION_BOUND,
        validate_canonical_selection_sidecar_shape,
    )

    events = inventory.events
    mutations = inventory.mutations
    mutation_kinds = inventory.mutation_kinds
    cognitive_canonical_selection_authorized = (
        capabilities.cognitive_canonical_selection_authorized
    )
    selection_events = [
        item for item in events if item.kind == COGNITIVE_CANONICAL_SELECTION_BOUND
    ]
    selection_mutations = [
        item
        for item in mutations
        if item.kind == "cognitive_canonical_selection_bind_guard"
    ]
    if cognitive_canonical_selection_authorized and not (
        selection_events or selection_mutations
    ):
        raise IntegrityError(
            "canonical selection capability requires its exact inert sidecar"
        )
    if selection_events or selection_mutations:
        exact_inventory = (
            tuple(item.kind for item in events)
            == (
                "ATTEMPT_ADMITTED",
                COGNITIVE_EXPERIMENT_ASSIGNED,
                COGNITIVE_CANONICAL_SELECTION_BOUND,
            )
            and events[1].payload.get("schema_id")
            == COGNITIVE_RUNTIME_CONTEXT_ASSIGNMENT_SCHEMA_ID
            and tuple(mutation_kinds)
            == (
                "attempt_admit",
                "cognitive_experiment_assign_guard",
                "cognitive_canonical_selection_bind_guard",
            )
        )
        if (
            not cognitive_canonical_selection_authorized
            or len(selection_events) != 1
            or len(selection_mutations) != 1
            or not exact_inventory
        ):
            raise IntegrityError(
                "canonical selection requires one exact atomic admission inventory"
            )
        selection_event = selection_events[0]
        if (
            selection_event.actor != COGNITIVE_CANONICAL_SELECTION_ACTOR
            or selection_event.event_id
            != (
                f"event:{COGNITIVE_CANONICAL_SELECTION_BOUND}:"
                f"{selection_event.payload.get('attempt_id')}"
            )
            or canonical_json_bytes(selection_event.payload)
            != canonical_json_bytes(selection_mutations[0].payload)
        ):
            raise IntegrityError("canonical selection sidecar authority diverged")
        try:
            validate_canonical_selection_sidecar_shape(selection_event.payload)
        except (TypeError, ValueError) as exc:
            raise IntegrityError("canonical selection sidecar is malformed") from exc


def _validate_canonical_continuation_authority(
    inventory: _AuthorityInventory,
    capabilities: _AuthorityCapabilities,
) -> None:
    from muteki.epistemic.cognitive_events_v1 import (
        COGNITIVE_EXPERIMENT_ASSIGNED,
        COGNITIVE_RUNTIME_CONTEXT_ASSIGNMENT_SCHEMA_ID,
    )
    from muteki.runtime.canonical_cognitive_continuation_v2 import (
        COGNITIVE_CANONICAL_CONTINUATION_ACTOR_V2,
        COGNITIVE_CANONICAL_CONTINUATION_BOUND_V2,
        COGNITIVE_CANONICAL_CONTINUATION_MUTATION_V2,
        validate_canonical_continuation_sidecar_shape_v2,
    )

    events = inventory.events
    mutations = inventory.mutations
    mutation_kinds = inventory.mutation_kinds
    cognitive_canonical_selection_authorized = (
        capabilities.cognitive_canonical_selection_authorized
    )
    cognitive_canonical_continuation_v2_authorized = (
        capabilities.cognitive_canonical_continuation_v2_authorized
    )
    continuation_events = [
        item
        for item in events
        if item.kind == COGNITIVE_CANONICAL_CONTINUATION_BOUND_V2
    ]
    continuation_mutations = [
        item
        for item in mutations
        if item.kind == COGNITIVE_CANONICAL_CONTINUATION_MUTATION_V2
    ]
    if cognitive_canonical_continuation_v2_authorized and not (
        continuation_events or continuation_mutations
    ):
        raise IntegrityError(
            "canonical continuation capability requires its exact v2 companion"
        )
    if continuation_events or continuation_mutations:
        exact_inventory = (
            tuple(item.kind for item in events)
            == (
                "ATTEMPT_ADMITTED",
                COGNITIVE_EXPERIMENT_ASSIGNED,
                COGNITIVE_CANONICAL_CONTINUATION_BOUND_V2,
            )
            and events[1].payload.get("schema_id")
            == COGNITIVE_RUNTIME_CONTEXT_ASSIGNMENT_SCHEMA_ID
            and tuple(mutation_kinds)
            == (
                "attempt_admit",
                "cognitive_experiment_assign_guard",
                COGNITIVE_CANONICAL_CONTINUATION_MUTATION_V2,
            )
        )
        if (
            not cognitive_canonical_continuation_v2_authorized
            or cognitive_canonical_selection_authorized
            or len(continuation_events) != 1
            or len(continuation_mutations) != 1
            or not exact_inventory
        ):
            raise IntegrityError(
                "canonical continuation requires one exact atomic v2 inventory"
            )
        continuation_event = continuation_events[0]
        if (
            continuation_event.actor != COGNITIVE_CANONICAL_CONTINUATION_ACTOR_V2
            or continuation_event.event_id
            != (
                f"event:{COGNITIVE_CANONICAL_CONTINUATION_BOUND_V2}:"
                f"{continuation_event.payload.get('attempt_id')}"
            )
            or canonical_json_bytes(continuation_event.payload)
            != canonical_json_bytes(continuation_mutations[0].payload)
        ):
            raise IntegrityError("canonical continuation capability or actor crossed")
        try:
            validate_canonical_continuation_sidecar_shape_v2(continuation_event.payload)
        except (TypeError, ValueError) as exc:
            raise IntegrityError("canonical continuation sidecar is malformed") from exc


def _validate_cognitive_authority(
    inventory: _AuthorityInventory,
    capabilities: _AuthorityCapabilities,
) -> bool:
    from muteki.epistemic.cognitive_events_v1 import (
        COGNITIVE_ASSIGNMENT_SCHEMA_ID,
        COGNITIVE_BINDING_ACTOR,
        COGNITIVE_EXECUTION_OBSERVED,
        COGNITIVE_EXPERIMENT_ASSIGNED,
        COGNITIVE_RUNTIME_CONTEXT_ASSIGNMENT_SCHEMA_ID,
        COGNITIVE_RUNTIME_EXECUTION_SCHEMA_ID,
        COGNITIVE_RUNTIME_EXECUTABLE_ASSIGNMENT_SCHEMA_ID,
        COGNITIVE_RUNTIME_REPRODUCTION_ASSIGNMENT_SCHEMA_ID,
    )
    from muteki.runtime.canonical_cognitive_continuation_v2 import (
        COGNITIVE_CANONICAL_CONTINUATION_MUTATION_V2,
    )
    from muteki.runtime.cognitive_runtime_observation_v1 import (
        COGNITIVE_RUNTIME_OBSERVER_ACTOR,
    )

    events = inventory.events
    event_kinds = inventory.event_kinds
    mutation_kinds = inventory.mutation_kinds
    event = inventory.event
    exact_binding = inventory.exact_binding
    cognitive_evaluation_authorized = capabilities.cognitive_evaluation_authorized
    cognitive_runtime_context_assignment_authorized = (
        capabilities.cognitive_runtime_context_assignment_authorized
    )
    cognitive_runtime_observation_authorized = (
        capabilities.cognitive_runtime_observation_authorized
    )
    cognitive_canonical_selection_authorized = (
        capabilities.cognitive_canonical_selection_authorized
    )
    cognitive_canonical_continuation_v2_authorized = (
        capabilities.cognitive_canonical_continuation_v2_authorized
    )
    runtime_observation_event = next(
        (
            item
            for item in events
            if item.kind == COGNITIVE_EXECUTION_OBSERVED
            and item.payload.get("schema_id") == COGNITIVE_RUNTIME_EXECUTION_SCHEMA_ID
        ),
        None,
    )
    cognitive_contracts = {
        COGNITIVE_EXPERIMENT_ASSIGNED: "cognitive_experiment_assign_guard",
        COGNITIVE_EXECUTION_OBSERVED: (
            "cognitive_runtime_execution_observe_guard"
            if runtime_observation_event is not None
            else "cognitive_execution_observe_guard"
        ),
    }
    present_cognitive_events = event_kinds & set(cognitive_contracts)
    present_cognitive_mutations = set(mutation_kinds) & (
        set(cognitive_contracts.values())
        | {"cognitive_runtime_execution_observe_guard"}
    )
    if (
        cognitive_evaluation_authorized
        or cognitive_runtime_context_assignment_authorized
        or cognitive_runtime_observation_authorized
    ) and not (present_cognitive_events or present_cognitive_mutations):
        raise IntegrityError(
            "composite cognitive capability requires its exact cognitive sidecar"
        )
    if present_cognitive_events or present_cognitive_mutations:
        if not (
            cognitive_evaluation_authorized
            or cognitive_runtime_context_assignment_authorized
            or cognitive_runtime_observation_authorized
        ):
            raise IntegrityError(
                "cognitive evaluation event requires its composite v2 capability"
            )
        if len(present_cognitive_events) != 1 or len(present_cognitive_mutations) != 1:
            raise IntegrityError(
                "cognitive evaluation command requires one exact semantic sidecar"
            )
        cognitive_kind = next(iter(present_cognitive_events))
        mutation_kind = cognitive_contracts[cognitive_kind]
        if mutation_kind not in present_cognitive_mutations:
            raise IntegrityError("cognitive event/mutation kinds diverged")
        cognitive_event = event(cognitive_kind)
        expected_cognitive_actor = (
            COGNITIVE_RUNTIME_OBSERVER_ACTOR
            if runtime_observation_event is not None
            else COGNITIVE_BINDING_ACTOR
        )
        if cognitive_event.actor != expected_cognitive_actor:
            raise IntegrityError("cognitive event actor is not authoritative")
        if cognitive_kind == COGNITIVE_EXPERIMENT_ASSIGNED:
            schema_id = cognitive_event.payload.get("schema_id")
            if schema_id in {
                COGNITIVE_RUNTIME_CONTEXT_ASSIGNMENT_SCHEMA_ID,
                COGNITIVE_RUNTIME_EXECUTABLE_ASSIGNMENT_SCHEMA_ID,
                COGNITIVE_RUNTIME_REPRODUCTION_ASSIGNMENT_SCHEMA_ID,
            }:
                if not cognitive_runtime_context_assignment_authorized:
                    raise IntegrityError(
                        "runtime-context cognitive assignment requires its exact capability"
                    )
                has_canonical_companion = (
                    cognitive_canonical_selection_authorized
                    or cognitive_canonical_continuation_v2_authorized
                )
                expected_event_count = 3 if has_canonical_companion else 2
                if (
                    len(events) != expected_event_count
                    or events[0].kind != "ATTEMPT_ADMITTED"
                    or events[1].kind != cognitive_kind
                ):
                    raise IntegrityError(
                        "runtime-context cognitive assignment must be atomic with ordinary admission"
                    )
                if cognitive_canonical_selection_authorized:
                    expected_mutations = (
                        "attempt_admit",
                        "cognitive_experiment_assign_guard",
                        "cognitive_canonical_selection_bind_guard",
                    )
                elif cognitive_canonical_continuation_v2_authorized:
                    expected_mutations = (
                        "attempt_admit",
                        "cognitive_experiment_assign_guard",
                        COGNITIVE_CANONICAL_CONTINUATION_MUTATION_V2,
                    )
                else:
                    expected_mutations = (
                        "attempt_admit",
                        "cognitive_experiment_assign_guard",
                    )
                if tuple(mutation_kinds) != expected_mutations:
                    raise IntegrityError(
                        "runtime-context cognitive assignment mutation inventory is not exact"
                    )
            else:
                if not cognitive_evaluation_authorized:
                    raise IntegrityError(
                        "cognitive evaluation event requires its composite v2 capability"
                    )
                if schema_id != COGNITIVE_ASSIGNMENT_SCHEMA_ID:
                    raise IntegrityError(
                        "cognitive assignment schema is not recognized by eval-v2"
                    )
                exact_events = tuple(item.kind for item in events) == (
                    "ATTEMPT_ADMITTED",
                    "C6_EVAL_V2_ATTEMPT_BOUND",
                    cognitive_kind,
                )
                if not exact_events:
                    raise IntegrityError(
                        "cognitive assignment must be atomic with v2 attempt admission"
                    )
                exact_mutations = tuple(mutation_kinds) == (
                    "attempt_admit",
                    "c6_eval_v2_attempt_bind_guard",
                    "cognitive_experiment_assign_guard",
                )
                if not exact_mutations:
                    raise IntegrityError(
                        "cognitive assignment mutation inventory is not exact"
                    )
            identity = cognitive_event.payload.get("attempt_id")
        else:
            if runtime_observation_event is not None:
                if not cognitive_runtime_observation_authorized:
                    raise IntegrityError(
                        "runtime cognitive observation requires its exact capability"
                    )
                if len(events) != 1 or tuple(mutation_kinds) != (
                    "cognitive_runtime_execution_observe_guard",
                ):
                    raise IntegrityError(
                        "runtime cognitive observation command inventory is not exact"
                    )
                identity = cognitive_event.payload.get("permit_id")
                if cognitive_event.event_id != (f"event:{cognitive_kind}:{identity}"):
                    raise IntegrityError(
                        "runtime cognitive observation event identity diverged"
                    )
                exact_binding(cognitive_kind, mutation_kind)
                return True
            if not cognitive_evaluation_authorized:
                raise IntegrityError(
                    "cognitive execution requires its composite v2 capability"
                )
            if (
                len(events) != 4
                or events[0].kind not in {"WORKER_TERMINAL", "WORKER_UNKNOWN"}
                or events[1].kind
                not in {
                    "BUDGET_PESSIMISTICALLY_SETTLED",
                    "BUDGET_USAGE_UNKNOWN",
                }
                or events[2].kind != "C6_EVAL_V2_TERMINAL_BOUND"
                or events[3].kind != cognitive_kind
            ):
                raise IntegrityError(
                    "cognitive execution must be atomic with v2 terminal accounting"
                )
            expected_budget_mutation = (
                "budget_unknown"
                if events[1].kind == "BUDGET_USAGE_UNKNOWN"
                else "budget_pessimistic_settle"
            )
            if tuple(mutation_kinds) not in {
                (
                    "worker_terminal_guard",
                    expected_budget_mutation,
                    "c6_eval_v2_terminal_bind_guard",
                    "cognitive_execution_observe_guard",
                ),
                (
                    "orphan_reconcile_guard",
                    expected_budget_mutation,
                    "c6_eval_v2_terminal_bind_guard",
                    "cognitive_execution_observe_guard",
                ),
            }:
                raise IntegrityError(
                    "cognitive execution mutation inventory is not exact"
                )
            identity = cognitive_event.payload.get("permit_id")
        if cognitive_event.event_id != f"event:{cognitive_kind}:{identity}":
            raise IntegrityError("cognitive evaluation event identity diverged")
        exact_binding(cognitive_kind, mutation_kind)

    return False
