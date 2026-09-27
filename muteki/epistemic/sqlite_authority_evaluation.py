"""Canary and C6 evaluation authority checks."""

from __future__ import annotations

from muteki.epistemic.contracts import canonical_digest
from muteki.epistemic.sqlite_authority_types import (
    _AuthorityCapabilities,
    _AuthorityInventory,
)
from muteki.epistemic.sqlite_types import IntegrityError


def _validate_canary_authority(
    inventory: _AuthorityInventory,
    capabilities: _AuthorityCapabilities,
) -> None:
    event_kinds = inventory.event_kinds
    exact_binding = inventory.exact_binding
    canary_authorized = capabilities.canary_authorized
    if "CANARY_ADMITTED" in event_kinds:
        if not canary_authorized:
            raise IntegrityError(
                "canary admission requires the catalog-only canary capability"
            )
        exact_binding("CANARY_ADMITTED", "canary_commit_guard")


def _validate_evaluation_sidecars(
    inventory: _AuthorityInventory,
    capabilities: _AuthorityCapabilities,
) -> None:
    events = inventory.events
    event_kinds = inventory.event_kinds
    exact_binding = inventory.exact_binding
    evaluation_authorized = capabilities.evaluation_authorized
    evaluation_v2_authorized = capabilities.evaluation_v2_authorized
    evaluation_sidecars = {
        "C6_EVAL_ATTEMPT_BOUND": (
            "ATTEMPT_ADMITTED",
            "c6_eval_attempt_bind_guard",
            "attempt",
        ),
        "C6_EVAL_LAUNCH_BOUND": (
            "WORKER_LAUNCH_PREPARED",
            "c6_eval_launch_bind_guard",
            "launch",
        ),
        "C6_EVAL_TERMINAL_BOUND": (
            ("WORKER_TERMINAL", "WORKER_UNKNOWN"),
            "c6_eval_terminal_bind_guard",
            "terminal",
        ),
    }
    evaluation_v2_sidecars = {
        "C6_EVAL_V2_ATTEMPT_BOUND": (
            "ATTEMPT_ADMITTED",
            "c6_eval_v2_attempt_bind_guard",
            "attempt",
        ),
        "C6_EVAL_V2_LAUNCH_BOUND": (
            "WORKER_LAUNCH_PREPARED",
            "c6_eval_v2_launch_bind_guard",
            "launch",
        ),
        "C6_EVAL_V2_TERMINAL_BOUND": (
            ("WORKER_TERMINAL", "WORKER_UNKNOWN"),
            "c6_eval_v2_terminal_bind_guard",
            "terminal",
        ),
    }
    present_sidecars = event_kinds & set(evaluation_sidecars)
    present_v2_sidecars = event_kinds & set(evaluation_v2_sidecars)
    if present_sidecars and present_v2_sidecars:
        raise IntegrityError(
            "one command cannot combine C6 v1 and v2 evaluation sidecars"
        )
    if present_sidecars:
        if not evaluation_authorized:
            raise IntegrityError(
                "C6 evaluation binding requires the host-only evaluation capability"
            )
        if len(present_sidecars) != 1 or len(events) != 2:
            raise IntegrityError(
                "C6 evaluation binding must be one exact atomic sidecar"
            )
        sidecar_kind = next(iter(present_sidecars))
        base_kind, mutation_kind, phase = evaluation_sidecars[sidecar_kind]
        allowed_base_kinds = base_kind if type(base_kind) is tuple else (base_kind,)
        if events[0].kind not in allowed_base_kinds or events[1].kind != sidecar_kind:
            raise IntegrityError("C6 evaluation sidecar ordinal/base kind diverged")
        if events[1].actor != "c6-evaluation-binding-authority":
            raise IntegrityError("C6 evaluation sidecar actor is not authoritative")
        if events[1].payload.get("phase") != phase:
            raise IntegrityError("C6 evaluation sidecar phase diverged")
        identity_name = (
            events[1].payload.get("attempt_id")
            if phase == "attempt"
            else events[1].payload.get("permit_id")
        )
        if (
            events[1].payload.get("base_event_id") != events[0].event_id
            or events[1].payload.get("base_payload_digest")
            != canonical_digest(events[0].payload)
            or events[1].event_id != f"event:{sidecar_kind}:{identity_name}"
        ):
            raise IntegrityError("C6 evaluation sidecar/base identity diverged")
        exact_binding(sidecar_kind, mutation_kind)
    if present_v2_sidecars:
        if not evaluation_v2_authorized:
            raise IntegrityError(
                "C6 evaluation v2 binding requires the host-only v2 evaluation capability"
            )
        if len(present_v2_sidecars) != 1:
            raise IntegrityError(
                "C6 evaluation v2 binding must be one exact atomic sidecar"
            )
        sidecar_kind = next(iter(present_v2_sidecars))
        base_kind, mutation_kind, phase = evaluation_v2_sidecars[sidecar_kind]
        cognitive_companion = (
            "COGNITIVE_EXECUTION_OBSERVED"
            if phase == "terminal"
            else "COGNITIVE_EXPERIMENT_ASSIGNED"
        )
        expected_event_count = (3 if phase == "terminal" else 2) + int(
            cognitive_companion in event_kinds
        )
        sidecar_ordinal = 2 if phase == "terminal" else 1
        if len(events) != expected_event_count:
            raise IntegrityError(
                "C6 evaluation v2 binding must be one exact atomic sidecar"
            )
        allowed_base_kinds = base_kind if type(base_kind) is tuple else (base_kind,)
        sidecar_event = events[sidecar_ordinal]
        if (
            events[0].kind not in allowed_base_kinds
            or sidecar_event.kind != sidecar_kind
        ):
            raise IntegrityError("C6 evaluation v2 sidecar ordinal/base kind diverged")
        if sidecar_event.actor != "c6-evaluation-binding-v2-authority":
            raise IntegrityError("C6 evaluation v2 sidecar actor is not authoritative")
        if sidecar_event.payload.get("phase") != phase:
            raise IntegrityError("C6 evaluation v2 sidecar phase diverged")
        identity_name = (
            sidecar_event.payload.get("attempt_id")
            if phase == "attempt"
            else sidecar_event.payload.get("permit_id")
        )
        if (
            sidecar_event.payload.get("base_event_id") != events[0].event_id
            or sidecar_event.payload.get("base_payload_digest")
            != canonical_digest(events[0].payload)
            or sidecar_event.event_id != f"event:{sidecar_kind}:{identity_name}"
        ):
            raise IntegrityError("C6 evaluation v2 sidecar/base identity diverged")
        if phase == "terminal":
            budget_event = events[1]
            if budget_event.kind not in {
                "BUDGET_PESSIMISTICALLY_SETTLED",
                "BUDGET_USAGE_UNKNOWN",
            }:
                raise IntegrityError(
                    "C6 evaluation v2 terminal requires atomic budget closure"
                )
            expected_budget_kind = (
                "BUDGET_USAGE_UNKNOWN"
                if events[0].kind == "WORKER_UNKNOWN"
                else "BUDGET_PESSIMISTICALLY_SETTLED"
            )
            if budget_event.kind != expected_budget_kind:
                raise IntegrityError(
                    "C6 evaluation v2 worker and budget outcomes diverged"
                )
            if (
                budget_event.payload.get("attempt_id")
                != events[0].payload.get("attempt_id")
                or sidecar_event.payload.get("budget_event_id") != budget_event.event_id
                or sidecar_event.payload.get("budget_event_kind") != budget_event.kind
                or sidecar_event.payload.get("budget_payload_digest")
                != canonical_digest(budget_event.payload)
            ):
                raise IntegrityError(
                    "C6 evaluation v2 terminal budget lineage diverged"
                )
        exact_binding(sidecar_kind, mutation_kind)


def _validate_evaluation_outcomes(
    inventory: _AuthorityInventory,
    capabilities: _AuthorityCapabilities,
) -> None:
    events = inventory.events
    mutations = inventory.mutations
    event_kinds = inventory.event_kinds
    mutation_kinds = inventory.mutation_kinds
    event = inventory.event
    exact_binding = inventory.exact_binding
    evaluation_checker_authorized = capabilities.evaluation_checker_authorized
    evaluation_outcomes = {
        "C6_EVAL_OUTCOME_VERIFIED": "c6_eval_outcome_guard",
        "C6_EVAL_OUTCOME_UNKNOWN": "c6_eval_outcome_unknown_guard",
    }
    present_outcomes = event_kinds & set(evaluation_outcomes)
    present_outcome_mutations = set(mutation_kinds) & set(evaluation_outcomes.values())
    if present_outcomes or present_outcome_mutations:
        if not evaluation_checker_authorized:
            raise IntegrityError(
                "C6 checker outcome requires its separate checker capability"
            )
        if (
            len(present_outcomes) != 1
            or len(present_outcome_mutations) != 1
            or len(events) != 1
            or len(mutations) != 1
        ):
            raise IntegrityError(
                "C6 checker outcome must be the sole event and sole mutation"
            )
        outcome_kind = next(iter(present_outcomes))
        mutation_kind = evaluation_outcomes[outcome_kind]
        if mutations[0].kind != mutation_kind:
            raise IntegrityError("C6 checker outcome mutation kind diverged")
        outcome = event(outcome_kind)
        assignment_digest = outcome.payload.get("assignment_digest")
        if (
            outcome.actor != "c6-evaluation-checker-authority"
            or outcome.event_id != f"event:C6_EVAL_OUTCOME:{assignment_digest}"
        ):
            raise IntegrityError("C6 checker outcome authority identity diverged")
        exact_binding(outcome_kind, mutation_kind)
