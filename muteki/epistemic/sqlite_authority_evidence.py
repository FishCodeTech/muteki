"""Reproduction and verification authority checks."""

from __future__ import annotations

from muteki.epistemic.contracts import canonical_json_bytes
from muteki.epistemic.sqlite_authority_types import (
    _AuthorityCapabilities,
    _AuthorityInventory,
)
from muteki.epistemic.sqlite_types import IntegrityError


def _validate_reproduction_authority(
    inventory: _AuthorityInventory,
    capabilities: _AuthorityCapabilities,
) -> None:
    events = inventory.events
    mutations = inventory.mutations
    event_kinds = inventory.event_kinds
    mutation_kinds = inventory.mutation_kinds
    cognitive_reproduction_declaration_authorized = (
        capabilities.cognitive_reproduction_declaration_authorized
    )
    cognitive_reproduction_launch_witness_authorized = (
        capabilities.cognitive_reproduction_launch_witness_authorized
    )
    from muteki.runtime.cognitive_reproduction_evidence_v1 import (
        COGNITIVE_REPRODUCTION_DECLARATION_ACTOR,
        COGNITIVE_REPRODUCTION_LAUNCH_WITNESSED,
        COGNITIVE_REPRODUCTION_LAUNCH_WITNESS_ACTOR,
        COGNITIVE_REPRODUCTION_PRELAUNCH_DECLARED,
        validate_launch_witness_payload_shape,
        validate_prelaunch_declaration_payload_shape,
    )

    reproduction_contracts = {
        COGNITIVE_REPRODUCTION_PRELAUNCH_DECLARED: (
            cognitive_reproduction_declaration_authorized,
            COGNITIVE_REPRODUCTION_DECLARATION_ACTOR,
            "cognitive_reproduction_prelaunch_declare_guard",
            validate_prelaunch_declaration_payload_shape,
        ),
        COGNITIVE_REPRODUCTION_LAUNCH_WITNESSED: (
            cognitive_reproduction_launch_witness_authorized,
            COGNITIVE_REPRODUCTION_LAUNCH_WITNESS_ACTOR,
            "cognitive_reproduction_launch_witness_guard",
            validate_launch_witness_payload_shape,
        ),
    }
    present_reproduction = event_kinds & set(reproduction_contracts)
    present_reproduction_mutations = set(mutation_kinds) & {
        item[2] for item in reproduction_contracts.values()
    }
    if (
        cognitive_reproduction_declaration_authorized
        or cognitive_reproduction_launch_witness_authorized
    ) and not (present_reproduction or present_reproduction_mutations):
        raise IntegrityError(
            "reproduction evidence capability requires its exact canonical event"
        )
    if present_reproduction or present_reproduction_mutations:
        if len(events) != 1 or len(mutations) != 1 or len(present_reproduction) != 1:
            raise IntegrityError(
                "reproduction evidence command must contain one event and one guard"
            )
        kind = next(iter(present_reproduction))
        authorized, expected_actor, expected_mutation, shape_validator = (
            reproduction_contracts[kind]
        )
        exact_event = events[0]
        if (
            not authorized
            or exact_event.kind != kind
            or exact_event.actor != expected_actor
            or mutations[0].kind != expected_mutation
            or canonical_json_bytes(exact_event.payload)
            != canonical_json_bytes(mutations[0].payload)
        ):
            raise IntegrityError("reproduction evidence capability or actor crossed")
        try:
            shape_validator(exact_event.payload)
        except (TypeError, ValueError) as exc:
            raise IntegrityError("reproduction evidence payload is false") from exc


def _validate_verification_authority(
    inventory: _AuthorityInventory,
    capabilities: _AuthorityCapabilities,
) -> None:
    events = inventory.events
    mutations = inventory.mutations
    event_kinds = inventory.event_kinds
    mutation_kinds = inventory.mutation_kinds
    cognitive_verification_checker_authorized = (
        capabilities.cognitive_verification_checker_authorized
    )
    cognitive_verification_resolver_authorized = (
        capabilities.cognitive_verification_resolver_authorized
    )
    from muteki.runtime.cognitive_verification_authority_v1 import (
        COGNITIVE_VERIFICATION_CHECKED,
        COGNITIVE_VERIFICATION_CHECKER_ACTOR,
        COGNITIVE_VERIFICATION_CHECK_INPUT_COMMITTED,
        COGNITIVE_VERIFICATION_CHECK_OUTPUT_SEALED,
        validate_cognitive_verification_check_input_shape,
        validate_cognitive_verification_check_output_shape,
    )
    from muteki.runtime.cognitive_verification_checker_v1 import (
        DeterministicCognitiveVerificationCheckV1,
    )

    verification_checker_contracts = {
        COGNITIVE_VERIFICATION_CHECK_INPUT_COMMITTED: (
            "cognitive_verification_check_input_guard",
            validate_cognitive_verification_check_input_shape,
        ),
        COGNITIVE_VERIFICATION_CHECK_OUTPUT_SEALED: (
            "cognitive_verification_check_output_guard",
            validate_cognitive_verification_check_output_shape,
        ),
        COGNITIVE_VERIFICATION_CHECKED: (
            "cognitive_verification_checked_guard",
            DeterministicCognitiveVerificationCheckV1.from_canonical,
        ),
    }
    present_verification_checker = event_kinds & set(verification_checker_contracts)
    present_verification_checker_mutations = set(mutation_kinds) & {
        item[0] for item in verification_checker_contracts.values()
    }
    if cognitive_verification_checker_authorized and not (
        present_verification_checker or present_verification_checker_mutations
    ):
        raise IntegrityError(
            "verification checker capability requires its exact canonical event"
        )
    if present_verification_checker or present_verification_checker_mutations:
        if (
            not cognitive_verification_checker_authorized
            or len(events) != 1
            or len(mutations) != 1
            or len(present_verification_checker) != 1
        ):
            raise IntegrityError(
                "verification checker event requires its checker-only capability"
            )
        kind = next(iter(present_verification_checker))
        expected_mutation, shape_validator = verification_checker_contracts[kind]
        exact_event = events[0]
        if (
            exact_event.actor != COGNITIVE_VERIFICATION_CHECKER_ACTOR
            or mutations[0].kind != expected_mutation
            or canonical_json_bytes(exact_event.payload)
            != canonical_json_bytes(mutations[0].payload)
        ):
            raise IntegrityError("verification checker capability or actor crossed")
        try:
            shape_validator(exact_event.payload)
        except (TypeError, ValueError) as exc:
            raise IntegrityError("verification checker payload is false") from exc

    from muteki.runtime.cognitive_verification_resolver_v1 import (
        COGNITIVE_VERIFICATION_RESOLVED,
        COGNITIVE_VERIFICATION_RESOLVER_ACTOR,
        validate_cognitive_verification_resolution_payload_shape,
    )

    present_verification_resolver = COGNITIVE_VERIFICATION_RESOLVED in event_kinds
    present_verification_resolver_mutation = (
        "cognitive_verification_resolve_guard" in mutation_kinds
    )
    if cognitive_verification_resolver_authorized and not (
        present_verification_resolver or present_verification_resolver_mutation
    ):
        raise IntegrityError(
            "verification resolver capability requires its exact canonical event"
        )
    if present_verification_resolver or present_verification_resolver_mutation:
        if (
            not cognitive_verification_resolver_authorized
            or len(events) != 1
            or len(mutations) != 1
            or events[0].kind != COGNITIVE_VERIFICATION_RESOLVED
            or events[0].actor != COGNITIVE_VERIFICATION_RESOLVER_ACTOR
            or mutations[0].kind != "cognitive_verification_resolve_guard"
            or canonical_json_bytes(events[0].payload)
            != canonical_json_bytes(mutations[0].payload)
        ):
            raise IntegrityError(
                "verification resolution requires its resolver-only capability"
            )
        try:
            validate_cognitive_verification_resolution_payload_shape(events[0].payload)
        except (TypeError, ValueError) as exc:
            raise IntegrityError("verification resolution payload is false") from exc
