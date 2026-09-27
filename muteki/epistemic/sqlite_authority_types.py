"""Shared inventory for SQLite authority validation."""

from __future__ import annotations

from collections.abc import Sequence
from dataclasses import dataclass, field

from muteki.epistemic.contracts import canonical_json_bytes
from muteki.epistemic.sqlite_types import (
    CommandEvent,
    IntegrityError,
    OutboxIntent,
    ProjectionMutation,
)


@dataclass(frozen=True, slots=True)
class _AuthorityInventory:
    events: Sequence[CommandEvent]
    mutations: Sequence[ProjectionMutation]
    outbox: Sequence[OutboxIntent]
    event_kinds: set[str] = field(init=False, repr=False)
    mutation_kinds: list[str] = field(init=False, repr=False)

    def __post_init__(self) -> None:
        object.__setattr__(self, "event_kinds", {event.kind for event in self.events})
        object.__setattr__(
            self,
            "mutation_kinds",
            [mutation.kind for mutation in self.mutations],
        )

    def event(self, kind: str) -> CommandEvent:
        matches = [item for item in self.events if item.kind == kind]
        if len(matches) != 1:
            raise IntegrityError(f"reserved command requires one {kind} event")
        return matches[0]

    def mutation(self, kind: str) -> ProjectionMutation:
        matches = [item for item in self.mutations if item.kind == kind]
        if len(matches) != 1:
            raise IntegrityError(
                f"reserved event requires exactly one {kind} semantic mutation"
            )
        return matches[0]

    def exact_binding(self, event_kind: str, mutation_kind: str) -> None:
        if canonical_json_bytes(self.event(event_kind).payload) != canonical_json_bytes(
            self.mutation(mutation_kind).payload
        ):
            raise IntegrityError(
                f"{event_kind} payload diverges from its semantic mutation"
            )

    def require(self, kind: str) -> None:
        if self.mutation_kinds.count(kind) != 1:
            raise IntegrityError(
                f"reserved event requires exactly one {kind} semantic mutation"
            )


@dataclass(frozen=True, slots=True)
class _AuthorityCapabilities:
    gate_authorized: bool
    lifecycle_authorized: bool
    canary_authorized: bool
    evaluation_authorized: bool
    evaluation_v2_authorized: bool
    cognitive_evaluation_authorized: bool
    cognitive_runtime_context_assignment_authorized: bool
    cognitive_canonical_selection_authorized: bool
    cognitive_canonical_continuation_v2_authorized: bool
    cognitive_runtime_output_authorized: bool
    cognitive_runtime_observation_authorized: bool
    cognitive_reproduction_declaration_authorized: bool
    cognitive_reproduction_launch_witness_authorized: bool
    cognitive_verification_checker_authorized: bool
    cognitive_verification_resolver_authorized: bool
    evaluation_checker_authorized: bool
    c6_decision_authorized: bool
    cognitive_context_authorized: bool
