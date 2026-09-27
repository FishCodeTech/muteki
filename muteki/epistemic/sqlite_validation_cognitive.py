"""Cognitive experiment validators moved from sqlite_store.py."""
from __future__ import annotations

import json
from collections.abc import Mapping
from typing import Any

from muteki.epistemic.contracts import (
    canonical_digest,
    canonical_json_bytes,
)

from muteki.epistemic.sqlite_types import (
    IntegrityError,
)

def _validate_cognitive_assignment_mutation(
    self, payload: Mapping[str, Any]
) -> None:
    from muteki.epistemic.cognitive_events_v1 import (
        COGNITIVE_ASSIGNMENT_SCHEMA_ID,
        COGNITIVE_EXPERIMENT_ASSIGNED,
        COGNITIVE_RUNTIME_CONTEXT_ASSIGNMENT_SCHEMA_ID,
        COGNITIVE_RUNTIME_EXECUTABLE_ASSIGNMENT_SCHEMA_ID,
        COGNITIVE_RUNTIME_REPRODUCTION_ASSIGNMENT_SCHEMA_ID,
        validate_assignment_payload_shape,
    )

    schema_id = payload.get("schema_id")
    if schema_id in {
        COGNITIVE_RUNTIME_CONTEXT_ASSIGNMENT_SCHEMA_ID,
        COGNITIVE_RUNTIME_EXECUTABLE_ASSIGNMENT_SCHEMA_ID,
        COGNITIVE_RUNTIME_REPRODUCTION_ASSIGNMENT_SCHEMA_ID,
    }:
        self._validate_runtime_context_cognitive_assignment_mutation(payload)
        return
    if schema_id != COGNITIVE_ASSIGNMENT_SCHEMA_ID:
        raise IntegrityError("cognitive assignment schema is not recognized")
    try:
        validate_assignment_payload_shape(payload)
    except (TypeError, ValueError) as exc:
        raise IntegrityError("cognitive assignment payload is false") from exc
    p = dict(payload)
    base = self._conn.execute(
        "SELECT command_id,kind,payload_json FROM events WHERE event_id=?",
        (p["base_event_id"],),
    ).fetchone()
    sidecar = self._conn.execute(
        "SELECT command_id,kind,payload_json FROM events WHERE event_id=?",
        (p["evaluation_sidecar_event_id"],),
    ).fetchone()
    own = self._conn.execute(
        "SELECT command_id FROM events WHERE kind=? AND payload_json=?",
        (
            COGNITIVE_EXPERIMENT_ASSIGNED,
            canonical_json_bytes(p).decode(),
        ),
    ).fetchone()
    if (
        base is None
        or base[1] != "ATTEMPT_ADMITTED"
        or sidecar is None
        or sidecar[1] != "C6_EVAL_V2_ATTEMPT_BOUND"
        or own is None
        or base[0] != sidecar[0]
        or base[0] != own[0]
    ):
        raise IntegrityError("cognitive assignment is not atomic with v2 admission")
    base_payload = json.loads(base[2])
    sidecar_payload = json.loads(sidecar[2])
    if (
        canonical_digest(base_payload) != p["base_payload_digest"]
        or canonical_digest(sidecar_payload)
        != p["evaluation_sidecar_payload_digest"]
        or sidecar_payload.get("base_event_id") != p["base_event_id"]
        or sidecar_payload.get("base_payload_digest") != p["base_payload_digest"]
        or sidecar_payload.get("role") != "executor"
    ):
        raise IntegrityError("cognitive assignment base binding is false")
    lineage = {
        "assignment_binding_digest": "assignment_binding_digest",
        "attempt_digest": "attempt_digest",
        "attempt_id": "attempt_id",
        "attempt_role_binding_digest": "attempt_role_binding_digest",
        "permit_digest": "permit_digest",
        "permit_id": "permit_id",
        "scope_digest": "scope_digest",
    }
    for cognitive_name, sidecar_name in lineage.items():
        if p[cognitive_name] != sidecar_payload.get(sidecar_name):
            raise IntegrityError("cognitive assignment v2 lineage diverged")
    for name in (
        "attempt_digest",
        "attempt_id",
        "permit_digest",
        "permit_id",
        "scope_digest",
    ):
        if p[name] != base_payload.get(name):
            raise IntegrityError("cognitive assignment admission lineage diverged")
    binding = self._runtime_evaluation_binding_from_sidecar(sidecar_payload)
    if binding.role != "executor":
        raise IntegrityError("cognitive assignment requires an executor role")

    # This is the actual semantic CAS.  The DTO's prefix identities are not
    # trusted: recompute the complete prefix from this store while the same
    # BEGIN IMMEDIATE still owns the pre-admission projection head.
    state = self._state()
    if (
        p["decision_cutoff_seq"] != state.head_seq
        or p["decision_head_event_digest"] != state.head_event_digest
    ):
        raise IntegrityError("cognitive assignment used a stale decision head")
    resolver = self.receipt_field_resolver(cutoff_seq=state.head_seq)
    prefix = resolver.verify_complete_through(state.head_seq)
    if (
        prefix.digest != p["decision_prefix_digest"]
        or prefix.head_event_digest != p["decision_head_event_digest"]
        or prefix.cutoff_seq != p["decision_cutoff_seq"]
    ):
        raise IntegrityError("cognitive assignment prefix is not store-owned")

    rows = self._conn.execute(
        "SELECT payload_json FROM events WHERE kind=? ORDER BY seq",
        (COGNITIVE_EXPERIMENT_ASSIGNED,),
    ).fetchall()
    decoded = [json.loads(row[0]) for row in rows]
    if (
        sum(item.get("attempt_id") == p["attempt_id"] for item in decoded) != 1
        or sum(
            item.get("assignment_digest") == p["assignment_digest"]
            for item in decoded
        )
        != 1
    ):
        raise IntegrityError("cognitive assignment identity is not unique")


def _validate_cognitive_execution_mutation(
    self, payload: Mapping[str, Any]
) -> None:
    from muteki.epistemic.cognitive_events_v1 import (
        COGNITIVE_EXECUTION_OBSERVED,
        COGNITIVE_EXPERIMENT_ASSIGNED,
        validate_execution_payload_shape,
    )

    try:
        validate_execution_payload_shape(payload)
    except (TypeError, ValueError) as exc:
        raise IntegrityError("cognitive execution payload is false") from exc
    p = dict(payload)
    assignment = self._conn.execute(
        "SELECT payload_json FROM events WHERE event_digest=? AND kind=?",
        (p["assignment_event_digest"], COGNITIVE_EXPERIMENT_ASSIGNED),
    ).fetchone()
    base = self._conn.execute(
        "SELECT command_id,kind,payload_json FROM events WHERE event_id=?",
        (p["base_event_id"],),
    ).fetchone()
    sidecar = self._conn.execute(
        "SELECT command_id,kind,payload_json FROM events WHERE event_id=?",
        (p["evaluation_terminal_event_id"],),
    ).fetchone()
    budget = self._conn.execute(
        "SELECT command_id,kind,payload_json FROM events WHERE event_id=?",
        (p["budget_event_id"],),
    ).fetchone()
    own = self._conn.execute(
        "SELECT command_id FROM events WHERE kind=? AND payload_json=?",
        (
            COGNITIVE_EXECUTION_OBSERVED,
            canonical_json_bytes(p).decode(),
        ),
    ).fetchone()
    if (
        assignment is None
        or base is None
        or base[1] not in {"WORKER_TERMINAL", "WORKER_UNKNOWN"}
        or sidecar is None
        or sidecar[1] != "C6_EVAL_V2_TERMINAL_BOUND"
        or budget is None
        or budget[1] != p["budget_event_kind"]
        or own is None
        or base[0] != sidecar[0]
        or base[0] != budget[0]
        or base[0] != own[0]
    ):
        raise IntegrityError(
            "cognitive execution is not atomic with v2 terminal accounting"
        )
    assignment_payload = json.loads(assignment[0])
    base_payload = json.loads(base[2])
    sidecar_payload = json.loads(sidecar[2])
    budget_payload = json.loads(budget[2])
    if (
        canonical_digest(base_payload) != p["base_payload_digest"]
        or canonical_digest(sidecar_payload)
        != p["evaluation_terminal_payload_digest"]
        or canonical_digest(budget_payload) != p["budget_payload_digest"]
        or sidecar_payload.get("base_event_id") != p["base_event_id"]
        or sidecar_payload.get("budget_event_id") != p["budget_event_id"]
        or sidecar_payload.get("budget_event_kind") != p["budget_event_kind"]
        or sidecar_payload.get("role") != "executor"
    ):
        raise IntegrityError("cognitive execution base binding is false")
    for name in (
        "attempt_digest",
        "attempt_id",
        "permit_digest",
        "permit_id",
        "scope_digest",
    ):
        if (
            p[name] != assignment_payload.get(name)
            or p[name] != base_payload.get(name)
            or p[name] != sidecar_payload.get(name)
        ):
            raise IntegrityError("cognitive execution lineage diverged")
    if (
        p["experiment_digest"] != assignment_payload.get("experiment_digest")
        or p["world_epoch_digest"] != assignment_payload.get("world_epoch_digest")
        or p["execution_outcome"] != base_payload.get("outcome")
        or p["execution_outcome"] != sidecar_payload.get("terminal_outcome")
        or budget_payload.get("attempt_id") != p["attempt_id"]
    ):
        raise IntegrityError("cognitive execution observation is rebound")
    rows = self._conn.execute(
        "SELECT payload_json FROM events WHERE kind=? ORDER BY seq",
        (COGNITIVE_EXECUTION_OBSERVED,),
    ).fetchall()
    decoded = [json.loads(row[0]) for row in rows]
    if (
        sum(
            item.get("assignment_event_digest") == p["assignment_event_digest"]
            for item in decoded
        )
        != 1
    ):
        raise IntegrityError("cognitive execution observation is not unique")
