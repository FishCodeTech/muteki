"""Attempt, I/O, worker, and lifecycle authority checks."""

from __future__ import annotations

from muteki.epistemic.contracts import canonical_json_bytes
from muteki.epistemic.sqlite_authority_types import (
    _AuthorityCapabilities,
    _AuthorityInventory,
)
from muteki.epistemic.sqlite_types import FlagAcceptedOutboxV1, IntegrityError


def _validate_attempt_effect_authority(
    inventory: _AuthorityInventory,
    capabilities: _AuthorityCapabilities,
) -> None:
    event_kinds = inventory.event_kinds
    event = inventory.event
    mutation = inventory.mutation
    require = inventory.require
    exact_binding = inventory.exact_binding
    evaluation_v2_authorized = capabilities.evaluation_v2_authorized
    if "ATTEMPT_ADMITTED" in event_kinds:
        require("attempt_admit")
        exact_binding("ATTEMPT_ADMITTED", "attempt_admit")
        admission_event = event("ATTEMPT_ADMITTED")
        admission_attempt_id = admission_event.payload.get("attempt_id")
        if (
            admission_event.actor != "search-admission"
            or type(admission_attempt_id) is not str
            or not admission_attempt_id
            or admission_event.event_id != f"event:attempt:admit:{admission_attempt_id}"
        ):
            raise IntegrityError("attempt admission authority or identity diverged")
    if "WORKER_LAUNCH_PREPARED" in event_kinds:
        require("attempt_launch")
        exact_binding("WORKER_LAUNCH_PREPARED", "attempt_launch")
    if "BUDGET_SETTLED" in event_kinds:
        require("budget_settle")
        exact_binding("BUDGET_SETTLED", "budget_settle")
    if "BUDGET_PESSIMISTICALLY_SETTLED" in event_kinds:
        if not evaluation_v2_authorized:
            raise IntegrityError(
                "pessimistic settlement requires v2 evaluation authority"
            )
        if not {
            "C6_EVAL_V2_TERMINAL_BOUND",
            "WORKER_TERMINAL",
        }.issubset(event_kinds):
            raise IntegrityError(
                "pessimistic settlement requires its atomic v2 worker terminal"
            )
        require("budget_pessimistic_settle")
        exact_binding(
            "BUDGET_PESSIMISTICALLY_SETTLED",
            "budget_pessimistic_settle",
        )
    if "BUDGET_USAGE_UNKNOWN" in event_kinds:
        require("budget_unknown")
        exact_binding("BUDGET_USAGE_UNKNOWN", "budget_unknown")
    if "EFFECT_PREPARED" in event_kinds:
        require("effect_prepare")
        exact_binding("EFFECT_PREPARED", "effect_prepare")
    if "EFFECT_RETRY_PREPARED" in event_kinds:
        require("effect_retry")
        exact_binding("EFFECT_RETRY_PREPARED", "effect_retry")
    if event_kinds & {
        "EFFECT_DISPATCH_MAY_HAVE_STARTED",
        "EFFECT_OBSERVED",
        "EFFECT_CONFIRMED_NOT_APPLIED",
        "EFFECT_UNKNOWN",
    }:
        require("effect_transition")
        transition_event = next(
            event(kind)
            for kind in event_kinds
            if kind
            in {
                "EFFECT_DISPATCH_MAY_HAVE_STARTED",
                "EFFECT_OBSERVED",
                "EFFECT_CONFIRMED_NOT_APPLIED",
                "EFFECT_UNKNOWN",
            }
        )
        if canonical_json_bytes(transition_event.payload) != canonical_json_bytes(
            mutation("effect_transition").payload
        ):
            raise IntegrityError(
                "effect transition event diverges from its semantic mutation"
            )


def _validate_io_worker_authority(
    inventory: _AuthorityInventory,
    capabilities: _AuthorityCapabilities,
) -> None:
    mutations = inventory.mutations
    outbox = inventory.outbox
    event_kinds = inventory.event_kinds
    mutation_kinds = inventory.mutation_kinds
    event = inventory.event
    mutation = inventory.mutation
    require = inventory.require
    gate_authorized = capabilities.gate_authorized
    cognitive_runtime_output_authorized = (
        capabilities.cognitive_runtime_output_authorized
    )
    io_actions = [
        mutation.payload.get("action")
        for mutation in mutations
        if mutation.kind == "attempt_io_guard"
    ]
    if event_kinds & {"CAPTURE_CHUNK_SEALED", "CAPTURE_MANIFEST_ADVANCED"}:
        chunk_event = event("CAPTURE_CHUNK_SEALED")
        manifest_event = event("CAPTURE_MANIFEST_ADVANCED")
        cognitive_output = (
            chunk_event.actor == "cognitive-runtime-output-port-v1"
            or manifest_event.actor == "cognitive-runtime-output-port-v1"
        )
        expected_actor = (
            "cognitive-runtime-output-port-v1" if cognitive_output else "capture-port"
        )
        expected_action = "cognitive_capture" if cognitive_output else "capture"
        if (
            chunk_event.actor != expected_actor
            or manifest_event.actor != expected_actor
            or (cognitive_output and not cognitive_runtime_output_authorized)
        ):
            raise IntegrityError("capture event authority identity diverged")
        if io_actions.count(expected_action) != 1:
            raise IntegrityError("capture event requires its semantic I/O guard")
        chunk = chunk_event.payload
        manifest = manifest_event.payload
        if canonical_json_bytes(chunk) != canonical_json_bytes(manifest):
            raise IntegrityError("capture chunk and manifest payloads diverge")
        guard = next(
            item.payload
            for item in mutations
            if item.kind == "attempt_io_guard"
            and item.payload.get("action") == expected_action
        )
        for name in (
            "attempt_digest",
            "lease_digest",
            "manifest_digest",
            "permit_digest",
            "raw_digest",
        ):
            if chunk.get(name) != guard.get(name):
                raise IntegrityError("capture event diverges from its I/O guard")
    if "CANDIDATE_REPORTED" in event_kinds:
        if io_actions.count("candidate") != 1:
            raise IntegrityError("candidate event requires its semantic I/O guard")
        candidate = event("CANDIDATE_REPORTED").payload
        guard = next(
            item.payload
            for item in mutations
            if item.kind == "attempt_io_guard"
            and item.payload.get("action") == "candidate"
        )
        for name in ("lease_digest", "permit_digest"):
            if candidate.get(name) != guard.get(name):
                raise IntegrityError("candidate event diverges from its I/O guard")
    if event_kinds & {"FLAG_ACCEPTED", "FLAG_REJECTED"}:
        if not gate_authorized:
            raise IntegrityError(
                "gate decision requires the host-only GateAuthority capability"
            )
        if io_actions.count("gate") != 1:
            raise IntegrityError("gate event requires its semantic I/O guard")
        gate_kind = (
            "FLAG_ACCEPTED" if "FLAG_ACCEPTED" in event_kinds else "FLAG_REJECTED"
        )
        gate = event(gate_kind).payload
        if gate.get("accepted") is not (gate_kind == "FLAG_ACCEPTED"):
            raise IntegrityError("gate event kind and decision diverge")
        guard = next(
            item.payload
            for item in mutations
            if item.kind == "attempt_io_guard" and item.payload.get("action") == "gate"
        )
        for name in (
            "attempt_digest",
            "candidate_id",
            "capture_event_digest",
            "flag_digest",
            "flag_format_digest",
            "lease_digest",
            "manifest_digest",
            "permit_digest",
            "policy_digest",
            "raw_digest",
            "snapshot_digest",
        ):
            if gate.get(name) != guard.get(name):
                raise IntegrityError("gate event diverges from its I/O guard")
        if gate_kind == "FLAG_REJECTED":
            if outbox:
                raise IntegrityError("rejected gate cannot emit an immutable outbox")
        else:
            if len(outbox) != 1:
                raise IntegrityError(
                    "accepted gate requires exactly one immutable outbox intent"
                )
            item = outbox[0]
            try:
                accepted_outbox = FlagAcceptedOutboxV1.from_payload(item.payload)
            except ValueError as exc:
                raise IntegrityError("accepted gate outbox is malformed") from exc
            if (
                item.outbox_id != f"outbox:flag:{gate.get('evaluation_id')}"
                or item.topic != "flag.accepted"
                or accepted_outbox.attempt_digest != gate.get("attempt_digest")
                or accepted_outbox.candidate_id != gate.get("candidate_id")
                or accepted_outbox.evaluation_id != gate.get("evaluation_id")
                or accepted_outbox.flag_digest != gate.get("flag_digest")
                or accepted_outbox.snapshot_digest != gate.get("snapshot_digest")
            ):
                raise IntegrityError(
                    "accepted gate outbox diverges from its authority event"
                )
    if "CONTEXT_PROMPT_LAUNCH_CLAIMED" in event_kinds:
        if io_actions.count("c6_launch") != 1:
            raise IntegrityError("C6 host launch claim requires its semantic I/O guard")
        claim = event("CONTEXT_PROMPT_LAUNCH_CLAIMED").payload
        guard = next(
            item.payload
            for item in mutations
            if item.kind == "attempt_io_guard"
            and item.payload.get("action") == "c6_launch"
        )
        for name in (
            "attempt_digest",
            "attempt_id",
            "expires_at_ns",
            "lease_digest",
            "lease_id",
            "permit_digest",
            "permit_id",
            "scope_digest",
            "worker_launch_event_digest",
        ):
            if claim.get(name) != guard.get(name):
                raise IntegrityError(
                    "C6 host launch claim diverges from its active-owner guard"
                )
    if "WORKER_TERMINAL" in event_kinds:
        require("worker_terminal_guard")
        terminal = event("WORKER_TERMINAL")
        guard = mutation("worker_terminal_guard").payload
        if (
            guard.get("terminal_event_id") != terminal.event_id
            or any(
                terminal.payload.get(name) != value
                for name, value in guard.items()
                if name != "terminal_event_id"
            )
            or set(terminal.payload) != set(guard) - {"terminal_event_id"}
        ):
            raise IntegrityError("worker terminal event diverges from its owner guard")
    if "WORKER_UNKNOWN" in event_kinds:
        if (
            mutation_kinds.count("worker_terminal_guard")
            + mutation_kinds.count("orphan_reconcile_guard")
            != 1
        ):
            raise IntegrityError("worker UNKNOWN requires one terminal owner guard")
        unknown = event("WORKER_UNKNOWN")
        guard_kind = (
            "worker_terminal_guard"
            if mutation_kinds.count("worker_terminal_guard")
            else "orphan_reconcile_guard"
        )
        guard = mutation(guard_kind).payload
        event_id_key = (
            "terminal_event_id"
            if guard_kind == "worker_terminal_guard"
            else "worker_unknown_event_id"
        )
        if guard.get(event_id_key) != unknown.event_id:
            raise IntegrityError("worker UNKNOWN event id diverges from its guard")
        for name in (
            "attempt_digest",
            "attempt_id",
            "lease_digest",
            "lease_id",
            "permit_digest",
            "permit_id",
            "scope_digest",
        ):
            if name in guard and unknown.payload.get(name) != guard.get(name):
                raise IntegrityError("worker UNKNOWN lineage diverges from its guard")


def _validate_lifecycle_authority(
    inventory: _AuthorityInventory,
    capabilities: _AuthorityCapabilities,
) -> None:
    events = inventory.events
    mutations = inventory.mutations
    event_kinds = inventory.event_kinds
    exact_binding = inventory.exact_binding
    lifecycle_authorized = capabilities.lifecycle_authorized
    lifecycle_bindings = {
        "START_EXECUTION": "execution_start_guard",
        "GOAL_COMPLETED": "goal_commit_guard",
        "EXECUTION_STOP_REQUESTED": "execution_stop_guard",
        "EXECUTION_SCOPE_DRAINED": "execution_drain_guard",
        "PROJECTION_REBUILD_VERIFIED": "projection_verify_guard",
        "S4E_CLOSURE_ATTESTED": "s4e_closure_guard",
    }
    lifecycle_events = event_kinds & set(lifecycle_bindings)
    if lifecycle_events:
        if not lifecycle_authorized:
            raise IntegrityError(
                "lifecycle event requires the host-only lifecycle capability"
            )
        if len(lifecycle_events) != 1:
            raise IntegrityError(
                "one command cannot combine multiple lifecycle authority events"
            )
        lifecycle_event = next(iter(lifecycle_events))
        exact_binding(lifecycle_event, lifecycle_bindings[lifecycle_event])
        if lifecycle_event == "S4E_CLOSURE_ATTESTED" and (
            len(events) != 1
            or len(mutations) != 1
            or mutations[0].kind != "s4e_closure_guard"
        ):
            raise IntegrityError(
                "S4-E closure must be the sole event and sole mutation in its command"
            )
