"""C6 packet and production-context authority checks."""

from __future__ import annotations

from collections.abc import Sequence

from muteki.epistemic.sqlite_authority_types import (
    _AuthorityCapabilities,
    _AuthorityInventory,
)
from muteki.epistemic.sqlite_types import (
    CommandEvent,
    IntegrityError,
    ProjectionMutation,
    _is_sha256,
)


def _validate_c6_packet_authority(
    inventory: _AuthorityInventory,
    capabilities: _AuthorityCapabilities,
) -> None:
    events = inventory.events
    mutations = inventory.mutations
    event_kinds = inventory.event_kinds
    c6_decision_authorized = capabilities.c6_decision_authorized
    c6_packet_authority_events = event_kinds & {
        "DECISION_NEED_REGISTERED",
        "C6_PACKET_COMPILED",
    }
    if c6_packet_authority_events:
        if not c6_decision_authorized:
            raise IntegrityError(
                "C6 packet authority event requires its host-only capability"
            )
        if len(events) != 1 or mutations:
            raise IntegrityError(
                "C6 packet authority command must be one inert canonical event"
            )
        authority_event = next(
            item for item in events if item.kind in c6_packet_authority_events
        )
        assignment_digest = authority_event.payload.get("assignment_binding_digest")
        if authority_event.kind == "DECISION_NEED_REGISTERED":
            decision_id = authority_event.payload.get("decision_id")
            if type(decision_id) is not str:
                raise IntegrityError("C6 decision identity is malformed")
            expected_event_id = (
                f"event:c6-decision:{decision_id.removeprefix('decision:')}"
            )
        else:
            receipt_digest = authority_event.payload.get("compiler_receipt_digest")
            if not _is_sha256(receipt_digest):
                raise IntegrityError("C6 packet compilation receipt is malformed")
            expected_event_id = f"event:c6-packet-compiled:{assignment_digest}"
        if (
            authority_event.actor != "c6-packet-compiler-authority-v2"
            or authority_event.event_id != expected_event_id
            or not _is_sha256(assignment_digest)
        ):
            raise IntegrityError("C6 packet authority identity diverged")


def _validate_production_context_authority(
    inventory: _AuthorityInventory,
    capabilities: _AuthorityCapabilities,
) -> None:
    events = inventory.events
    mutations = inventory.mutations
    event_kinds = inventory.event_kinds
    cognitive_context_authorized = capabilities.cognitive_context_authorized
    cognitive_context_events = event_kinds & {
        "RUNTIME_CONTEXT_DECISION_REGISTERED",
        "CONTEXT_PACKET_COMPILED",
        "CONTEXT_PACKET_UNADMITTED",
        "CONTEXT_PROMPT_STAGED",
        "CONTEXT_PROMPT_INVOCATION_BOUND",
        "CONTEXT_PROMPT_LAUNCH_CLAIMED",
        "CONTEXT_PROMPT_RELEASED",
        "CONTEXT_PROMPT_PRELAUNCH_ABORTED",
        "CONTEXT_PROMPT_UNKNOWN",
    }
    if cognitive_context_events:
        if not cognitive_context_authorized:
            raise IntegrityError(
                "production context event requires its host-only capability"
            )
        authority_event = next(
            item for item in events if item.kind in cognitive_context_events
        )
        if authority_event.kind == "CONTEXT_PROMPT_LAUNCH_CLAIMED":
            if (
                len(events) != 1
                or len(mutations) != 1
                or mutations[0].kind != "attempt_io_guard"
                or mutations[0].payload.get("action") != "c6_launch"
            ):
                raise IntegrityError(
                    "C6 host launch claim requires one exact active-owner guard"
                )
        elif len(events) != 1 or mutations:
            raise IntegrityError(
                "production context command must be one inert canonical event"
            )
        payload = authority_event.payload
        expected_event_id, digest_fields = _context_event_contract(
            authority_event, mutations
        )
        if authority_event.event_id != expected_event_id or any(
            not _is_sha256(payload.get(name)) for name in digest_fields
        ):
            raise IntegrityError("production context lineage is malformed")


def _context_event_contract(
    authority_event: CommandEvent,
    mutations: Sequence[ProjectionMutation],
) -> tuple[str, tuple[str, ...]]:
    payload = authority_event.payload
    decision_id = payload.get("decision_id")
    target_attempt_id = payload.get("preallocated_attempt_id")
    if authority_event.kind != "RUNTIME_CONTEXT_DECISION_REGISTERED":
        target_attempt_id = payload.get("target_attempt_id")
    if (
        authority_event.actor != "cognitive-context-authority-v1"
        or type(target_attempt_id) is not str
        or not target_attempt_id
        or payload.get("accepted_set_change") is not False
    ):
        raise IntegrityError("production context authority identity diverged")
    if authority_event.kind == "RUNTIME_CONTEXT_DECISION_REGISTERED":
        if type(decision_id) is not str or not decision_id:
            raise IntegrityError("production context decision identity is malformed")
        expected_event_id = f"event:context-decision:{decision_id}"
        digest_fields = (
            "attempt_digest",
            "context_digest",
            "feature_state_digest",
            "scope_digest",
        )
    elif authority_event.kind == "CONTEXT_PACKET_COMPILED":
        if type(decision_id) is not str or not decision_id:
            raise IntegrityError("production context decision identity is malformed")
        expected_event_id = f"event:context-packet:{decision_id}"
        digest_fields = (
            "build_request_digest",
            "compiler_receipt_digest",
            "decision_receipt_digest",
            "feature_state_digest",
            "manifest_digest",
            "packet_digest",
            "scope_digest",
        )
    elif authority_event.kind == "CONTEXT_PACKET_UNADMITTED":
        if type(decision_id) is not str or not decision_id:
            raise IntegrityError("production context decision identity is malformed")
        expected_event_id = f"event:context-packet-unadmitted:{decision_id}"
        digest_fields = (
            "compilation_event_receipt_digest",
            "compiler_receipt_digest",
            "feature_state_digest",
            "manifest_digest",
            "packet_digest",
            "reason_digest",
            "scope_digest",
        )
    elif authority_event.kind == "CONTEXT_PROMPT_STAGED":
        stage_id = payload.get("stage_id")
        if type(stage_id) is not str or not stage_id.startswith("stage-"):
            raise IntegrityError("production context stage identity is malformed")
        if payload.get("transport") != "argv":
            raise IntegrityError("strict C6 staging requires argv transport")
        if (
            type(payload.get("prompt_byte_count")) is not int
            or payload["prompt_byte_count"] <= 0
        ):
            raise IntegrityError("production context prompt byte count is malformed")
        expected_event_id = f"event:context-stage:{stage_id}"
        digest_fields = (
            "assembly_digest",
            "compilation_event_receipt_digest",
            "compiler_receipt_digest",
            "context_block_digest",
            "feature_state_digest",
            "manifest_digest",
            "packet_digest",
            "permit_digest",
            "prompt_artifact_digest",
            "scope_digest",
        )
    elif authority_event.kind == "CONTEXT_PROMPT_INVOCATION_BOUND":
        invocation_id = payload.get("invocation_id")
        if (
            type(invocation_id) is not str
            or not invocation_id.startswith("invocation-")
            or payload.get("transport") != "argv"
            or payload.get("prompt_argument_count") != 1
            or type(payload.get("argv_byte_count")) is not int
            or payload["argv_byte_count"] <= 0
        ):
            raise IntegrityError("production context invocation is malformed")
        expected_event_id = f"event:context-invocation:{invocation_id}"
        digest_fields = (
            "argv_artifact_digest",
            "assembly_digest",
            "feature_state_digest",
            "packet_digest",
            "permit_digest",
            "prompt_stage_event_digest",
            "prompt_stage_receipt_digest",
            "scope_digest",
            "worker_launch_event_digest",
        )
    elif authority_event.kind == "CONTEXT_PROMPT_LAUNCH_CLAIMED":
        claim_id = payload.get("claim_id")
        if (
            type(claim_id) is not str
            or not claim_id.startswith("claim-")
            or type(payload.get("expires_at_ns")) is not int
            or payload["expires_at_ns"] < 0
            or type(payload.get("invocation_id")) is not str
            or not payload["invocation_id"].startswith("invocation-")
            or payload.get("transport") != "argv"
        ):
            raise IntegrityError("production C6 host launch claim is malformed")
        expected_event_id = f"event:context-launch-claim:{claim_id}"
        digest_fields = (
            "feature_state_digest",
            "launch_material_digest",
            "packet_digest",
            "permit_digest",
            "profile_digest",
            "prompt_invocation_event_digest",
            "prompt_invocation_receipt_digest",
            "prompt_stage_event_digest",
            "prompt_stage_receipt_digest",
            "scope_digest",
            "worker_launch_event_digest",
        )
        guard = mutations[0].payload
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
            if guard.get(name) != payload.get(name):
                raise IntegrityError(
                    "C6 host launch claim diverges from its active-owner guard"
                )
    elif authority_event.kind == "CONTEXT_PROMPT_RELEASED":
        stage_id = payload.get("stage_id")
        if type(stage_id) is not str or not stage_id.startswith("stage-"):
            raise IntegrityError("production context stage identity is malformed")
        if (
            payload.get("transport") != "argv"
            or payload.get("transport_backend") != "host_popen"
            or type(payload.get("expires_at_ns")) is not int
            or payload["expires_at_ns"] < 0
            or type(payload.get("process_id")) is not int
            or payload["process_id"] <= 0
            or type(payload.get("invocation_id")) is not str
            or not payload["invocation_id"].startswith("invocation-")
        ):
            raise IntegrityError("strict C6 release requires argv transport")
        expected_event_id = f"event:context-release:{stage_id}"
        digest_fields = (
            "feature_state_digest",
            "launch_material_digest",
            "prompt_invocation_event_digest",
            "prompt_invocation_receipt_digest",
            "prompt_launch_claim_event_digest",
            "prompt_launch_claim_receipt_digest",
            "packet_digest",
            "permit_digest",
            "profile_digest",
            "prompt_stage_event_digest",
            "prompt_stage_receipt_digest",
            "scope_digest",
            "start_observation_digest",
            "worker_launch_event_digest",
        )
    elif authority_event.kind == "CONTEXT_PROMPT_PRELAUNCH_ABORTED":
        stage_id = payload.get("stage_id")
        if (
            type(stage_id) is not str
            or not stage_id.startswith("stage-")
            or type(payload.get("claim_id")) is not str
            or not payload["claim_id"].startswith("claim-")
            or type(payload.get("expires_at_ns")) is not int
            or payload["expires_at_ns"] < 0
            or payload.get("transport") != "argv"
        ):
            raise IntegrityError("production C6 prelaunch abort is malformed")
        expected_event_id = f"event:context-prelaunch-aborted:{stage_id}"
        digest_fields = (
            "feature_state_digest",
            "launch_material_digest",
            "packet_digest",
            "permit_digest",
            "profile_digest",
            "prompt_launch_claim_event_digest",
            "prompt_launch_claim_receipt_digest",
            "reason_digest",
            "scope_digest",
            "worker_launch_event_digest",
        )
    else:
        stage_id = payload.get("stage_id")
        if type(stage_id) is not str or not stage_id.startswith("stage-"):
            raise IntegrityError("production context stage identity is malformed")
        expected_event_id = f"event:context-unknown:{stage_id}"
        digest_fields = (
            "feature_state_digest",
            "prompt_invocation_event_digest",
            "prompt_invocation_receipt_digest",
            "packet_digest",
            "permit_digest",
            "prompt_stage_receipt_digest",
            "reason_digest",
            "scope_digest",
        )
    return expected_event_id, digest_fields
