"""Projection dispatch and event-to-mutation mapping."""

from __future__ import annotations

import json
from collections.abc import Mapping
from typing import Any

from muteki.epistemic.sqlite_projection_budget import (
    _apply_budget_projection_mutation,
)
from muteki.epistemic.sqlite_projection_effect_catalog import (
    _apply_effect_catalog_projection_mutation,
)
from muteki.epistemic.sqlite_projection_runtime import (
    _apply_runtime_projection_mutation,
)
from muteki.epistemic.sqlite_projection_worker import (
    _apply_worker_projection_mutation,
)
from muteki.epistemic.sqlite_types import (
    ProjectionMutation,
    _strict_nonnegative_int_map,
)


@staticmethod
def _json_map(raw: str) -> dict[str, int]:
    value = json.loads(raw)
    return _strict_nonnegative_int_map(value, name="stored budget dimensions")


def _apply_projection_mutation(
    self,
    mutation: ProjectionMutation,
    *,
    enforce_live_guards: bool = True,
) -> None:
    p = dict(mutation.payload)
    if self._apply_cognitive_projection_mutation(mutation, p):
        return
    if _apply_runtime_projection_mutation(
        self, mutation, p, enforce_live_guards=enforce_live_guards
    ):
        return
    if _apply_worker_projection_mutation(
        self, mutation, p, enforce_live_guards=enforce_live_guards
    ):
        return
    if _apply_budget_projection_mutation(self, mutation, p):
        return
    if _apply_effect_catalog_projection_mutation(
        self, mutation, p, enforce_live_guards=enforce_live_guards
    ):
        return
    raise ValueError(f"unsupported projection mutation: {mutation.kind}")


@staticmethod
def _mutation_from_event(
    kind: str, payload: Mapping[str, Any]
) -> ProjectionMutation | None:
    direct = {
        "BRANCH_CREATED": "branch_create",
        "BUDGET_ACCOUNT_CREATED": "budget_account_create",
        "ATTEMPT_ADMITTED": "attempt_admit",
        "BUDGET_PESSIMISTICALLY_SETTLED": "budget_pessimistic_settle",
        "BUDGET_SETTLED": "budget_settle",
        "BUDGET_USAGE_UNKNOWN": "budget_unknown",
        "EFFECT_PREPARED": "effect_prepare",
        "EFFECT_RETRY_PREPARED": "effect_retry",
        "DRAFT_CREATED": "draft_create",
        "DRAFT_ATTACHMENT_SEALED": "draft_attachment",
        "RUN_ID_ALLOCATED": "provision_begin",
        "RUN_MATERIALIZED": "provision_materialized",
        "RUN_SEALED": "provision_sealed",
        "CATALOG_ARCHIVE_REQUESTED": "archive_begin",
        "CATALOG_RUN_ARCHIVED": "archive_complete",
        "PURGE_PLAN_SEALED": "purge_begin",
        "PURGE_ITEM_ABSENT": "purge_item_absent",
        "PURGE_ITEM_UNKNOWN": "purge_item_unknown",
        "PURGE_COMPLETED": "purge_complete",
    }
    if kind in direct:
        return ProjectionMutation(direct[kind], payload)
    if kind == "BRANCH_STATE_CHANGED":
        return ProjectionMutation(
            "branch_state",
            {
                "branch_id": payload["branch_id"],
                "expected_state": payload["from"],
                "new_state": payload["to"],
            },
        )
    if kind == "WORKER_LAUNCH_PREPARED" and all(
        name in payload
        for name in (
            "attempt_id",
            "lease_id",
            "permit_id",
            "reservation_ids",
            "scope_digest",
        )
    ):
        return ProjectionMutation("attempt_launch", payload)
    if kind.startswith("EFFECT_") and kind not in {
        "EFFECT_PREPARED",
        "EFFECT_RETRY_PREPARED",
    }:
        return ProjectionMutation("effect_transition", payload)
    return None
