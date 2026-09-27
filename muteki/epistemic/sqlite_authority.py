"""Authority mutation validation dispatcher."""

from __future__ import annotations

from collections.abc import Sequence

from muteki.epistemic.sqlite_authority_cognitive import (
    _validate_canonical_continuation_authority,
    _validate_canonical_selection_authority,
    _validate_cognitive_authority,
)
from muteki.epistemic.sqlite_authority_context import (
    _validate_c6_packet_authority,
    _validate_production_context_authority,
)
from muteki.epistemic.sqlite_authority_evaluation import (
    _validate_canary_authority,
    _validate_evaluation_outcomes,
    _validate_evaluation_sidecars,
)
from muteki.epistemic.sqlite_authority_evidence import (
    _validate_reproduction_authority,
    _validate_verification_authority,
)
from muteki.epistemic.sqlite_authority_runtime import (
    _validate_attempt_effect_authority,
    _validate_io_worker_authority,
    _validate_lifecycle_authority,
)
from muteki.epistemic.sqlite_authority_types import (
    _AuthorityCapabilities,
    _AuthorityInventory,
)
from muteki.epistemic.sqlite_types import (
    CommandEvent,
    IntegrityError,
    OutboxIntent,
    ProjectionMutation,
)


def _require_authority_mutations(
    events: Sequence[CommandEvent],
    mutations: Sequence[ProjectionMutation],
    outbox: Sequence[OutboxIntent],
    *,
    gate_authorized: bool,
    lifecycle_authorized: bool,
    canary_authorized: bool,
    evaluation_authorized: bool,
    evaluation_v2_authorized: bool,
    cognitive_evaluation_authorized: bool,
    cognitive_runtime_context_assignment_authorized: bool,
    cognitive_canonical_selection_authorized: bool,
    cognitive_canonical_continuation_v2_authorized: bool,
    cognitive_runtime_output_authorized: bool,
    cognitive_runtime_observation_authorized: bool,
    cognitive_reproduction_declaration_authorized: bool,
    cognitive_reproduction_launch_witness_authorized: bool,
    cognitive_verification_checker_authorized: bool,
    cognitive_verification_resolver_authorized: bool,
    evaluation_checker_authorized: bool,
    c6_decision_authorized: bool,
    cognitive_context_authorized: bool,
) -> None:
    """Reserved authority events cannot be appended without their semantic CAS."""
    inventory = _AuthorityInventory(events, mutations, outbox)
    capabilities = _AuthorityCapabilities(
        gate_authorized=gate_authorized,
        lifecycle_authorized=lifecycle_authorized,
        canary_authorized=canary_authorized,
        evaluation_authorized=evaluation_authorized,
        evaluation_v2_authorized=evaluation_v2_authorized,
        cognitive_evaluation_authorized=cognitive_evaluation_authorized,
        cognitive_runtime_context_assignment_authorized=(
            cognitive_runtime_context_assignment_authorized
        ),
        cognitive_canonical_selection_authorized=(
            cognitive_canonical_selection_authorized
        ),
        cognitive_canonical_continuation_v2_authorized=(
            cognitive_canonical_continuation_v2_authorized
        ),
        cognitive_runtime_output_authorized=cognitive_runtime_output_authorized,
        cognitive_runtime_observation_authorized=(
            cognitive_runtime_observation_authorized
        ),
        cognitive_reproduction_declaration_authorized=(
            cognitive_reproduction_declaration_authorized
        ),
        cognitive_reproduction_launch_witness_authorized=(
            cognitive_reproduction_launch_witness_authorized
        ),
        cognitive_verification_checker_authorized=(
            cognitive_verification_checker_authorized
        ),
        cognitive_verification_resolver_authorized=(
            cognitive_verification_resolver_authorized
        ),
        evaluation_checker_authorized=evaluation_checker_authorized,
        c6_decision_authorized=c6_decision_authorized,
        cognitive_context_authorized=cognitive_context_authorized,
    )

    _validate_exclusive_authority(inventory)
    _validate_reproduction_authority(inventory, capabilities)
    _validate_verification_authority(inventory, capabilities)
    _validate_c6_packet_authority(inventory, capabilities)
    _validate_production_context_authority(inventory, capabilities)
    _validate_canonical_selection_authority(inventory, capabilities)
    _validate_canonical_continuation_authority(inventory, capabilities)
    if _validate_cognitive_authority(inventory, capabilities):
        return
    _validate_attempt_effect_authority(inventory, capabilities)
    _validate_io_worker_authority(inventory, capabilities)
    _validate_lifecycle_authority(inventory, capabilities)
    _validate_canary_authority(inventory, capabilities)
    _validate_evaluation_sidecars(inventory, capabilities)
    _validate_evaluation_outcomes(inventory, capabilities)


def _validate_exclusive_authority(inventory: _AuthorityInventory) -> None:
    events = inventory.events
    exclusive_groups = (
        {
            "BUDGET_PESSIMISTICALLY_SETTLED",
            "BUDGET_SETTLED",
            "BUDGET_USAGE_UNKNOWN",
        },
        {
            "EFFECT_DISPATCH_MAY_HAVE_STARTED",
            "EFFECT_OBSERVED",
            "EFFECT_CONFIRMED_NOT_APPLIED",
            "EFFECT_UNKNOWN",
        },
        {"FLAG_ACCEPTED", "FLAG_REJECTED"},
        {"WORKER_TERMINAL", "WORKER_UNKNOWN"},
        {"C6_EVAL_OUTCOME_VERIFIED", "C6_EVAL_OUTCOME_UNKNOWN"},
    )
    for group in exclusive_groups:
        if sum(event.kind in group for event in events) > 1:
            raise IntegrityError("reserved command contains contradictory events")
