"""Runtime evaluation validators moved from sqlite_store.py."""

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
    _C6_EVAL_BINDING_FIELDS,
    _C6_EVAL_BINDING_SCHEMA_ID,
    _C6_EVAL_BINDING_V2_SCHEMA_ID,
    _C6_EVAL_V2_COMMON_FIELDS,
    _C6_EVAL_V2_ROOT_BUDGET_FIELDS,
    _is_sha256,
)
from muteki.epistemic.sqlite_validation_runtime_observation import (  # noqa: F401
    _validate_runtime_cognitive_execution_mutation,
)


def _validate_c6_eval_binding_mutation(
    self, kind: str, payload: Mapping[str, Any]
) -> None:
    p = dict(payload)
    common = {
        "attempt_digest",
        "attempt_id",
        "base_event_id",
        "base_payload_digest",
        "evaluation_binding_digest",
        "permit_digest",
        "permit_id",
        "phase",
        "schema_id",
        "scope_digest",
    }
    phase_contract = {
        "c6_eval_attempt_bind_guard": (
            "attempt",
            "ATTEMPT_ADMITTED",
            common | {"evaluation_binding"},
            None,
            None,
        ),
        "c6_eval_launch_bind_guard": (
            "launch",
            "WORKER_LAUNCH_PREPARED",
            common | {"attempt_binding_event_digest"},
            "C6_EVAL_ATTEMPT_BOUND",
            "attempt_binding_event_digest",
        ),
        "c6_eval_terminal_bind_guard": (
            "terminal",
            ("WORKER_TERMINAL", "WORKER_UNKNOWN"),
            common | {"launch_binding_event_digest"},
            "C6_EVAL_LAUNCH_BOUND",
            "launch_binding_event_digest",
        ),
    }
    if kind not in phase_contract:
        raise IntegrityError("unknown C6 evaluation binding mutation")
    phase, base_kind, fields, parent_kind, parent_field = phase_contract[kind]
    if set(p) != fields:
        raise IntegrityError("C6 evaluation binding payload shape is not versioned")
    if p["schema_id"] != _C6_EVAL_BINDING_SCHEMA_ID or p["phase"] != phase:
        raise IntegrityError("C6 evaluation binding schema/phase diverged")
    for name in (
        "attempt_digest",
        "base_payload_digest",
        "evaluation_binding_digest",
        "permit_digest",
        "scope_digest",
    ):
        if not _is_sha256(p[name]):
            raise IntegrityError(f"C6 evaluation {name} is malformed")
    base = self._conn.execute(
        "SELECT command_id,kind,payload_json FROM events WHERE event_id=?",
        (p["base_event_id"],),
    ).fetchone()
    allowed_base_kinds = base_kind if type(base_kind) is tuple else (base_kind,)
    if base is None or base[1] not in allowed_base_kinds:
        raise IntegrityError("C6 evaluation sidecar has no exact base event")
    sidecar = self._conn.execute(
        "SELECT command_id FROM events WHERE kind=? AND payload_json=?",
        (
            {
                "attempt": "C6_EVAL_ATTEMPT_BOUND",
                "launch": "C6_EVAL_LAUNCH_BOUND",
                "terminal": "C6_EVAL_TERMINAL_BOUND",
            }[phase],
            canonical_json_bytes(p).decode(),
        ),
    ).fetchone()
    if sidecar is None or sidecar[0] != base[0]:
        raise IntegrityError("C6 evaluation sidecar is not atomic with its base")
    base_payload = json.loads(base[2])
    if canonical_digest(base_payload) != p["base_payload_digest"]:
        raise IntegrityError("C6 evaluation base payload digest is false")
    for name in (
        "attempt_digest",
        "attempt_id",
        "permit_digest",
        "permit_id",
        "scope_digest",
    ):
        if base_payload.get(name) != p[name]:
            raise IntegrityError("C6 evaluation base lineage diverged")
    if phase == "attempt":
        binding = p["evaluation_binding"]
        if (
            type(binding) is not dict
            or set(binding) != _C6_EVAL_BINDING_FIELDS
            or binding.get("mode") != "shadow"
            or binding.get("split") != "fresh_holdout"
            or binding.get("accepted_set_change") is not False
            or canonical_digest(binding) != p["evaluation_binding_digest"]
            or binding.get("run_manifest_digest")
            != canonical_digest(
                {
                    "binding": {
                        name: value
                        for name, value in binding.items()
                        if name != "run_manifest_digest"
                    },
                    "schema_id": "muteki.c6-eval-run-manifest.v1",
                }
            )
            or any(
                not _is_sha256(value)
                for name, value in binding.items()
                if name.endswith("_digest")
            )
        ):
            raise IntegrityError("C6 evaluation binding body is false")
    else:
        if not _is_sha256(p[parent_field]):
            raise IntegrityError("C6 evaluation parent sidecar digest is malformed")
        parent = self._conn.execute(
            "SELECT payload_json FROM events WHERE event_digest=? AND kind=?",
            (p[parent_field], parent_kind),
        ).fetchone()
        if parent is None:
            raise IntegrityError("C6 evaluation sidecar parent is absent")
        parent_payload = json.loads(parent[0])
        if any(
            parent_payload.get(name) != p[name]
            for name in (
                "attempt_digest",
                "attempt_id",
                "evaluation_binding_digest",
                "permit_digest",
                "permit_id",
                "scope_digest",
            )
        ):
            raise IntegrityError("C6 evaluation sidecar parent lineage diverged")


def _validate_runtime_context_cognitive_assignment_mutation(
    self, payload: Mapping[str, Any]
) -> None:
    """Semantic CAS for one ContextPacket-bound cognitive assignment."""

    from muteki.epistemic.cognitive_events_v1 import (
        COGNITIVE_EXPERIMENT_ASSIGNED,
        COGNITIVE_RUNTIME_EXECUTABLE_ASSIGNMENT_SCHEMA_ID,
        COGNITIVE_RUNTIME_REPRODUCTION_ASSIGNMENT_SCHEMA_ID,
        validate_runtime_context_assignment_payload_shape,
        validate_runtime_context_executable_assignment_payload_shape,
        validate_runtime_reproduction_assignment_payload_shape,
    )

    try:
        if (
            payload.get("schema_id")
            == COGNITIVE_RUNTIME_REPRODUCTION_ASSIGNMENT_SCHEMA_ID
        ):
            validate_runtime_reproduction_assignment_payload_shape(payload)
        elif (
            payload.get("schema_id")
            == COGNITIVE_RUNTIME_EXECUTABLE_ASSIGNMENT_SCHEMA_ID
        ):
            validate_runtime_context_executable_assignment_payload_shape(payload)
        else:
            validate_runtime_context_assignment_payload_shape(payload)
    except (TypeError, ValueError) as exc:
        raise IntegrityError(
            "runtime-context cognitive assignment payload is false"
        ) from exc
    p = dict(payload)
    base = self._conn.execute(
        "SELECT command_id,kind,payload_json FROM events WHERE event_id=?",
        (p["base_event_id"],),
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
        or own is None
        or base[0] != own[0]
    ):
        raise IntegrityError(
            "runtime-context cognitive assignment is not atomic with admission"
        )
    base_payload = json.loads(base[2])
    packet = p["context_packet_binding_body"]
    permit_body = base_payload.get("permit")
    permit_constraints = (
        permit_body.get("constraints") if isinstance(permit_body, dict) else None
    )
    if (
        canonical_digest(base_payload) != p["base_payload_digest"]
        or base_payload.get("context_packet") != packet
        or not isinstance(permit_constraints, dict)
        or permit_constraints.get("context_packet") != packet
        or canonical_digest(packet) != p["context_packet_binding_digest"]
    ):
        raise IntegrityError(
            "runtime-context cognitive assignment packet/admission binding is false"
        )
    for name in (
        "attempt_digest",
        "attempt_id",
        "permit_digest",
        "permit_id",
        "scope_digest",
    ):
        if p[name] != base_payload.get(name):
            raise IntegrityError(
                "runtime-context cognitive assignment admission lineage diverged"
            )

    try:
        compilation_receipt = self.resolve_receipt(
            packet["compilation_event_receipt_digest"]
        )
        compilation_rows = [
            row
            for row in self.event_rows(kind="CONTEXT_PACKET_COMPILED")
            if row["payload"].get("packet_digest") == packet["packet_digest"]
            and self.receipt_digest_for_event(row["event_digest"])
            == packet["compilation_event_receipt_digest"]
        ]
    except (IntegrityError, KeyError, TypeError, ValueError) as exc:
        raise IntegrityError(
            "runtime-context cognitive assignment packet receipt is absent"
        ) from exc
    if (
        len(compilation_rows) != 1
        or compilation_receipt.command_id != f"context:packet:{packet['decision_id']}"
        or compilation_rows[0]["payload"].get("compiler_receipt_digest")
        != packet["compiler_receipt_digest"]
        or compilation_rows[0]["payload"].get("decision_receipt_digest")
        != packet["decision_receipt_digest"]
        or compilation_rows[0]["payload"].get("feature_state_digest")
        != packet["feature_state_digest"]
        or compilation_rows[0]["payload"].get("manifest_digest")
        != packet["manifest_digest"]
        or compilation_rows[0]["payload"].get("target_attempt_id") != p["attempt_id"]
    ):
        raise IntegrityError(
            "runtime-context cognitive assignment packet lineage diverged"
        )

    # As in eval-v2, the DTO's prefix identities are not trusted.  Recompute
    # the complete pre-admission prefix while this BEGIN IMMEDIATE owns it.
    state = self._state()
    if (
        p["decision_cutoff_seq"] != state.head_seq
        or p["decision_head_event_digest"] != state.head_event_digest
    ):
        raise IntegrityError(
            "runtime-context cognitive assignment used a stale decision head"
        )
    resolver = self.receipt_field_resolver(cutoff_seq=state.head_seq)
    prefix = resolver.verify_complete_through(state.head_seq)
    if (
        prefix.digest != p["decision_prefix_digest"]
        or prefix.head_event_digest != p["decision_head_event_digest"]
        or prefix.cutoff_seq != p["decision_cutoff_seq"]
    ):
        raise IntegrityError(
            "runtime-context cognitive assignment prefix is not store-owned"
        )

    rows = self._conn.execute(
        "SELECT payload_json FROM events WHERE kind=? ORDER BY seq",
        (COGNITIVE_EXPERIMENT_ASSIGNED,),
    ).fetchall()
    decoded = [json.loads(row[0]) for row in rows]
    if (
        sum(item.get("attempt_id") == p["attempt_id"] for item in decoded) != 1
        or sum(
            item.get("assignment_digest") == p["assignment_digest"] for item in decoded
        )
        != 1
    ):
        raise IntegrityError(
            "runtime-context cognitive assignment identity is not unique"
        )

    if p["schema_id"] == COGNITIVE_RUNTIME_REPRODUCTION_ASSIGNMENT_SCHEMA_ID:
        self._validate_cognitive_reproduction_source_locked(p)


def _runtime_evaluation_binding_from_sidecar(self, sidecar: Mapping[str, Any]) -> Any:
    from muteki.runtime.contracts import RuntimeEvaluationBindingV2

    body = sidecar.get("runtime_binding")
    if not isinstance(body, Mapping):
        raise IntegrityError("evaluation v2 runtime binding body is absent")
    body = json.loads(canonical_json_bytes(body).decode())
    try:
        binding = RuntimeEvaluationBindingV2.from_canonical(body)
    except (TypeError, ValueError) as exc:
        raise IntegrityError("evaluation v2 runtime binding body is false") from exc
    if (
        binding.digest != sidecar.get("runtime_binding_digest")
        or binding.assignment_binding_digest != sidecar.get("assignment_binding_digest")
        or binding.attempt_role_binding_digest
        != sidecar.get("attempt_role_binding_digest")
        or binding.attempt_id != sidecar.get("attempt_id")
        or binding.attempt_identity_digest != sidecar.get("attempt_digest")
        or binding.permit_id != sidecar.get("permit_id")
        or binding.permit_digest != sidecar.get("permit_digest")
        or binding.role != sidecar.get("role")
        or binding.scope_digest != sidecar.get("scope_digest")
        or binding.slot_id != sidecar.get("slot_id")
    ):
        raise IntegrityError("evaluation v2 runtime binding identity diverged")
    return binding


def _validate_c6_eval_v2_binding_mutation(
    self, kind: str, payload: Mapping[str, Any]
) -> None:
    p = dict(payload)
    phase_contract = {
        "c6_eval_v2_attempt_bind_guard": (
            "attempt",
            "ATTEMPT_ADMITTED",
            _C6_EVAL_V2_COMMON_FIELDS | {"runtime_binding", "root_budget_reservation"},
            None,
            None,
        ),
        "c6_eval_v2_launch_bind_guard": (
            "launch",
            "WORKER_LAUNCH_PREPARED",
            _C6_EVAL_V2_COMMON_FIELDS
            | {
                "attempt_binding_event_digest",
                "prerequisite_terminal_event_digests",
            },
            "C6_EVAL_V2_ATTEMPT_BOUND",
            "attempt_binding_event_digest",
        ),
        "c6_eval_v2_terminal_bind_guard": (
            "terminal",
            ("WORKER_TERMINAL", "WORKER_UNKNOWN"),
            _C6_EVAL_V2_COMMON_FIELDS
            | {
                "budget_event_id",
                "budget_event_kind",
                "budget_payload_digest",
                "launch_binding_event_digest",
                "terminal_outcome",
            },
            "C6_EVAL_V2_LAUNCH_BOUND",
            "launch_binding_event_digest",
        ),
    }
    if kind not in phase_contract:
        raise IntegrityError("unknown evaluation v2 binding mutation")
    phase, base_kind, fields, parent_kind, parent_field = phase_contract[kind]
    if set(p) != fields:
        raise IntegrityError("evaluation v2 sidecar shape is not versioned")
    if p["schema_id"] != _C6_EVAL_BINDING_V2_SCHEMA_ID or p["phase"] != phase:
        raise IntegrityError("evaluation v2 sidecar schema/phase diverged")
    digest_fields = (
        "assignment_binding_digest",
        "attempt_role_binding_digest",
        "attempt_digest",
        "base_payload_digest",
        "permit_digest",
        "runtime_binding_digest",
        "scope_digest",
    )
    if phase == "terminal":
        digest_fields += ("budget_payload_digest",)
    for name in digest_fields:
        if not _is_sha256(p[name]):
            raise IntegrityError(f"evaluation v2 {name} is malformed")
    if p["role"] not in {"observer", "executor"} or p["slot_id"] != p["role"]:
        raise IntegrityError("evaluation v2 role/slot identity diverged")
    base = self._conn.execute(
        "SELECT command_id,kind,payload_json FROM events WHERE event_id=?",
        (p["base_event_id"],),
    ).fetchone()
    allowed_base_kinds = base_kind if type(base_kind) is tuple else (base_kind,)
    if base is None or base[1] not in allowed_base_kinds:
        raise IntegrityError("evaluation v2 sidecar has no exact base event")
    sidecar = self._conn.execute(
        "SELECT command_id FROM events WHERE kind=? AND payload_json=?",
        (
            {
                "attempt": "C6_EVAL_V2_ATTEMPT_BOUND",
                "launch": "C6_EVAL_V2_LAUNCH_BOUND",
                "terminal": "C6_EVAL_V2_TERMINAL_BOUND",
            }[phase],
            canonical_json_bytes(p).decode(),
        ),
    ).fetchone()
    if sidecar is None or sidecar[0] != base[0]:
        raise IntegrityError("evaluation v2 sidecar is not atomic with its base")
    base_payload = json.loads(base[2])
    if canonical_digest(base_payload) != p["base_payload_digest"]:
        raise IntegrityError("evaluation v2 base payload digest is false")
    for name in (
        "attempt_digest",
        "attempt_id",
        "permit_digest",
        "permit_id",
        "scope_digest",
    ):
        if base_payload.get(name) != p[name]:
            raise IntegrityError("evaluation v2 base lineage diverged")
    if phase == "attempt":
        self._validate_runtime_evaluation_v2_attempt_body(p, base_payload)
        return
    if not _is_sha256(p[parent_field]):
        raise IntegrityError("evaluation v2 parent sidecar digest is malformed")
    parent = self._conn.execute(
        "SELECT payload_json FROM events WHERE event_digest=? AND kind=?",
        (p[parent_field], parent_kind),
    ).fetchone()
    if parent is None:
        raise IntegrityError("evaluation v2 sidecar parent is absent")
    parent_payload = json.loads(parent[0])
    lineage_fields = (
        "assignment_binding_digest",
        "attempt_role_binding_digest",
        "attempt_digest",
        "attempt_id",
        "permit_digest",
        "permit_id",
        "role",
        "runtime_binding_digest",
        "scope_digest",
        "slot_id",
    )
    if any(parent_payload.get(name) != p[name] for name in lineage_fields):
        raise IntegrityError("evaluation v2 sidecar parent lineage diverged")
    if phase == "launch":
        binding = self._runtime_evaluation_binding_from_sidecar(parent_payload)
        if binding.split not in {
            "architecture_search",
            "development",
            "fresh_holdout",
        }:
            raise IntegrityError("sealed_final has no runtime launch authority")
        prereqs = p["prerequisite_terminal_event_digests"]
        if prereqs != list(binding.prerequisite_terminal_event_digests):
            raise IntegrityError("evaluation v2 launch prerequisites diverged")
        self.validate_runtime_evaluation_v2_prerequisite_lineage(binding)
        return
    terminal_outcome = p["terminal_outcome"]
    if terminal_outcome != base_payload.get("outcome"):
        raise IntegrityError("evaluation v2 terminal outcome diverged")
    allowed = (
        {"proposal", "unknown"}
        if p["role"] == "observer"
        else {
            "observed",
            "unknown",
        }
    )
    if terminal_outcome not in allowed:
        raise IntegrityError("evaluation v2 role terminal outcome is forbidden")
    budget = self._conn.execute(
        "SELECT command_id,kind,payload_json FROM events WHERE event_id=?",
        (p["budget_event_id"],),
    ).fetchone()
    if (
        budget is None
        or budget[0] != base[0]
        or budget[1] != p["budget_event_kind"]
        or budget[1]
        not in {
            "BUDGET_PESSIMISTICALLY_SETTLED",
            "BUDGET_USAGE_UNKNOWN",
        }
    ):
        raise IntegrityError("evaluation v2 terminal budget event is not atomic")
    budget_payload = json.loads(budget[2])
    if (
        canonical_digest(budget_payload) != p["budget_payload_digest"]
        or budget_payload.get("attempt_id") != p["attempt_id"]
        or budget[1]
        != (
            "BUDGET_USAGE_UNKNOWN"
            if base[1] == "WORKER_UNKNOWN"
            else "BUDGET_PESSIMISTICALLY_SETTLED"
        )
    ):
        raise IntegrityError("evaluation v2 terminal budget lineage is false")


def validate_runtime_evaluation_v2_prerequisite_lineage(
    self, runtime_binding: object
) -> None:
    from muteki.runtime.contracts import RuntimeEvaluationBindingV2

    if type(runtime_binding) is not RuntimeEvaluationBindingV2:
        raise IntegrityError(
            "evaluation v2 prerequisite requires an exact runtime binding"
        )
    inventories = zip(
        runtime_binding.prerequisite_attempt_ids,
        runtime_binding.prerequisite_attempt_binding_digests,
        runtime_binding.prerequisite_terminal_event_digests,
        strict=True,
    )
    for attempt_id, binding_digest, terminal_digest in inventories:
        rows = self._conn.execute(
            "SELECT payload_json FROM events "
            "WHERE kind='C6_EVAL_V2_ATTEMPT_BOUND' ORDER BY seq"
        ).fetchall()
        matching = [
            json.loads(row[0])
            for row in rows
            if json.loads(row[0]).get("attempt_role_binding_digest") == binding_digest
        ]
        if len(matching) != 1:
            raise IntegrityError(
                "evaluation v2 prerequisite attempt binding is not unique"
            )
        observer_sidecar = matching[0]
        observer = self._runtime_evaluation_binding_from_sidecar(observer_sidecar)
        if (
            observer.assignment_binding_digest
            != runtime_binding.assignment_binding_digest
            or observer.arm_id != runtime_binding.arm_id
            or observer.root_budget_digest != runtime_binding.root_budget_digest
            or observer.run_manifest_digest != runtime_binding.run_manifest_digest
            or observer.run_id != runtime_binding.run_id
            or observer.scope_digest != runtime_binding.scope_digest
            or observer.role != "observer"
            or observer.attempt_id != attempt_id
            or observer.attempt_role_binding_digest != binding_digest
        ):
            raise IntegrityError("evaluation v2 prerequisite is cross-spliced")
        terminal_rows = self._conn.execute(
            "SELECT event_id,payload_json FROM events "
            "WHERE event_digest=? AND kind='WORKER_TERMINAL'",
            (terminal_digest,),
        ).fetchall()
        if len(terminal_rows) != 1:
            raise IntegrityError(
                "evaluation v2 executor requires an observer proposal terminal"
            )
        terminal_event_id = str(terminal_rows[0][0])
        terminal_payload = json.loads(terminal_rows[0][1])
        if any(
            terminal_payload.get(name) != value
            for name, value in {
                "attempt_id": observer.attempt_id,
                "outcome": "proposal",
                "permit_digest": observer.permit_digest,
                "permit_id": observer.permit_id,
                "scope_digest": observer.scope_digest,
            }.items()
        ):
            raise IntegrityError("evaluation v2 observer terminal is rebound")
        terminal_sidecars = self._conn.execute(
            "SELECT payload_json FROM events "
            "WHERE kind='C6_EVAL_V2_TERMINAL_BOUND' ORDER BY seq"
        ).fetchall()
        terminal_matches = [
            json.loads(row[0])
            for row in terminal_sidecars
            if json.loads(row[0]).get("base_event_id") == terminal_event_id
        ]
        if len(terminal_matches) != 1:
            raise IntegrityError(
                "evaluation v2 observer terminal sidecar is not unique"
            )
        terminal_sidecar = terminal_matches[0]
        if any(
            terminal_sidecar.get(name) != value
            for name, value in {
                "assignment_binding_digest": observer.assignment_binding_digest,
                "attempt_id": observer.attempt_id,
                "attempt_role_binding_digest": binding_digest,
                "permit_digest": observer.permit_digest,
                "permit_id": observer.permit_id,
                "role": "observer",
                "runtime_binding_digest": observer.digest,
                "scope_digest": observer.scope_digest,
                "slot_id": observer.slot_id,
                "terminal_outcome": "proposal",
            }.items()
        ):
            raise IntegrityError(
                "evaluation v2 observer terminal sidecar is cross-spliced"
            )


def _validate_runtime_evaluation_v2_attempt_body(
    self,
    sidecar: Mapping[str, Any],
    base_payload: Mapping[str, Any],
) -> None:
    binding = self._runtime_evaluation_binding_from_sidecar(sidecar)
    if binding.split not in {
        "architecture_search",
        "development",
        "fresh_holdout",
    }:
        raise IntegrityError("sealed_final has no runtime admission authority")
    reservation = sidecar.get("root_budget_reservation")
    if (
        type(reservation) is not dict
        or set(reservation) != _C6_EVAL_V2_ROOT_BUDGET_FIELDS
        or reservation.get("assignment_binding_digest")
        != binding.assignment_binding_digest
        or reservation.get("root_budget_digest") != binding.root_budget_digest
        or type(reservation.get("first_reservation")) is not bool
    ):
        raise IntegrityError("evaluation v2 root reservation is false")
    if (
        binding.run_manifest_digest != self.run_anchor()["manifest_digest"]
        or binding.run_id != self.run_id
        or binding.permit_digest != base_payload.get("permit_digest")
        or binding.permit_id != base_payload.get("permit_id")
        or binding.attempt_id != base_payload.get("attempt_id")
        or binding.attempt_identity_digest != base_payload.get("attempt_digest")
        or binding.scope_digest != base_payload.get("scope_digest")
        or binding.policy_digest != base_payload.get("policy_digest")
        or dict(binding.role_budget) != dict(base_payload.get("requested_budget") or {})
    ):
        raise IntegrityError("evaluation v2 attempt/permit/budget is rebound")
    prior_rows = self._conn.execute(
        "SELECT payload_json FROM events "
        "WHERE kind='C6_EVAL_V2_ATTEMPT_BOUND' ORDER BY seq"
    ).fetchall()
    prior_first = 0
    prior_slots: set[str] = set()
    for row in prior_rows:
        body = json.loads(row[0])
        if body.get("attempt_id") == binding.attempt_id:
            continue
        if body.get("assignment_binding_digest") != binding.assignment_binding_digest:
            raise IntegrityError("evaluation v2 run cannot mint a second root")
        prior_slots.add(str(body.get("slot_id")))
        root = body.get("root_budget_reservation")
        if type(root) is dict and root.get("first_reservation") is True:
            prior_first += 1
            if root.get("root_budget_digest") != binding.root_budget_digest:
                raise IntegrityError("evaluation v2 root budget diverged")
    if binding.slot_id in prior_slots:
        raise IntegrityError("evaluation v2 role slot was already admitted")
    if reservation["first_reservation"] is True:
        if prior_first:
            raise IntegrityError("evaluation v2 root may be reserved only once")
    elif prior_first != 1:
        raise IntegrityError("evaluation v2 role has no unique root reservation")
    self.validate_runtime_evaluation_v2_prerequisite_lineage(binding)
    if binding.role == "observer" and base_payload.get("effect_class") != "pure":
        raise IntegrityError("evaluation v2 observer admission must be effect-pure")
