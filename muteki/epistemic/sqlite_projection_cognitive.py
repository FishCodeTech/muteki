"""Cognitive projection guards moved from sqlite_store.py."""

from __future__ import annotations

from typing import Any


from muteki.epistemic.sqlite_types import (
    ProjectionMutation,
)


def _apply_cognitive_projection_mutation(
    self,
    mutation: ProjectionMutation,
    p: dict[str, Any],
) -> bool:
    """Handle cognitive projection guards. Returns True if consumed."""
    if mutation.kind == "cognitive_experiment_assign_guard":
        self._validate_cognitive_assignment_mutation(p)
        return True
    if mutation.kind == "cognitive_canonical_selection_bind_guard":
        from muteki.runtime.canonical_cognitive_selection_v1 import (
            validate_canonical_selection_against_store,
        )

        validate_canonical_selection_against_store(self, p)
        return True
    if mutation.kind == "cognitive_canonical_continuation_bind_guard_v2":
        from muteki.runtime.canonical_cognitive_continuation_v2 import (
            validate_canonical_continuation_against_store_v2,
        )

        validate_canonical_continuation_against_store_v2(self, p)
        return True
    if mutation.kind == "cognitive_execution_observe_guard":
        self._validate_cognitive_execution_mutation(p)
        return True
    if mutation.kind == "cognitive_runtime_execution_observe_guard":
        self._validate_runtime_cognitive_execution_mutation(p)
        return True
    if mutation.kind == "cognitive_reproduction_prelaunch_declare_guard":
        self._validate_cognitive_reproduction_prelaunch_mutation(p)
        return True
    if mutation.kind == "cognitive_reproduction_launch_witness_guard":
        self._validate_cognitive_reproduction_launch_witness_mutation(p)
        return True
    if mutation.kind == "cognitive_verification_check_input_guard":
        from muteki.runtime.cognitive_verification_authority_v1 import (
            validate_cognitive_verification_check_input_against_store,
        )

        validate_cognitive_verification_check_input_against_store(self, p)
        return True
    if mutation.kind == "cognitive_verification_check_output_guard":
        from muteki.runtime.cognitive_verification_authority_v1 import (
            validate_cognitive_verification_check_output_against_store,
        )

        validate_cognitive_verification_check_output_against_store(self, p)
        return True
    if mutation.kind == "cognitive_verification_checked_guard":
        from muteki.runtime.cognitive_verification_authority_v1 import (
            validate_cognitive_verification_checked_against_store,
        )

        validate_cognitive_verification_checked_against_store(self, p)
        return True
    if mutation.kind == "cognitive_verification_resolve_guard":
        from muteki.runtime.cognitive_verification_resolver_v1 import (
            validate_cognitive_verification_resolution_against_store,
        )

        validate_cognitive_verification_resolution_against_store(self, p)
        return True
    return False
