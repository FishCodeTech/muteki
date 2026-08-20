"""Single-writer atomic command/event/fold/outbox store for Protocol 2."""

from __future__ import annotations

import json
import os
import sqlite3
import threading
import time
from contextlib import contextmanager
from collections.abc import Callable, Iterator, Mapping, Sequence
from dataclasses import dataclass, field, replace
from pathlib import Path
from typing import Any

from muteki.epistemic.contracts import (
    CanonicalReceipt,
    EventEnvelopeV2,
    canonical_digest,
    canonical_json_bytes,
)
from muteki.epistemic.folds import CanonicalState, apply_event, initial_state


class IdempotencyConflict(RuntimeError):
    pass


class IntegrityError(RuntimeError):
    pass


FLAG_ACCEPTED_OUTBOX_SCHEMA_ID = "muteki.flag-accepted-outbox.v1"
_FLAG_ACCEPTED_OUTBOX_FIELDS = frozenset(
    {
        "attempt_digest",
        "candidate_id",
        "evaluation_id",
        "flag_byte_count",
        "flag_digest",
        "flag_encoding",
        "flag_object_digest",
        "schema_id",
        "snapshot_digest",
    }
)


@dataclass(frozen=True, slots=True)
class FlagAcceptedOutboxV1:
    attempt_digest: str
    candidate_id: str
    evaluation_id: str
    flag_digest: str
    flag_object_digest: str
    flag_byte_count: int
    flag_encoding: str
    snapshot_digest: str
    schema_id: str = FLAG_ACCEPTED_OUTBOX_SCHEMA_ID

    @classmethod
    def from_payload(cls, payload: Mapping[str, Any]) -> "FlagAcceptedOutboxV1":
        if not isinstance(payload, Mapping) or set(payload) != _FLAG_ACCEPTED_OUTBOX_FIELDS:
            raise ValueError("accepted flag outbox payload has an unexpected shape")
        values = dict(payload)
        for name in (
            "attempt_digest",
            "evaluation_id",
            "flag_digest",
            "flag_object_digest",
            "snapshot_digest",
        ):
            if not _is_sha256(values.get(name)):
                raise ValueError(f"accepted flag outbox {name} is malformed")
        candidate_id = values.get("candidate_id")
        if (
            type(candidate_id) is not str
            or not candidate_id
            or candidate_id != candidate_id.strip()
        ):
            raise ValueError("accepted flag outbox candidate_id is malformed")
        if type(values.get("flag_byte_count")) is not int or values["flag_byte_count"] <= 0:
            raise ValueError("accepted flag outbox byte count is malformed")
        if values.get("flag_encoding") != "utf-8":
            raise ValueError("accepted flag outbox encoding is unsupported")
        if values.get("schema_id") != FLAG_ACCEPTED_OUTBOX_SCHEMA_ID:
            raise ValueError("accepted flag outbox schema is unsupported")
        return cls(**values)

    def canonical_payload(self) -> dict[str, Any]:
        payload = {
            "attempt_digest": self.attempt_digest,
            "candidate_id": self.candidate_id,
            "evaluation_id": self.evaluation_id,
            "flag_byte_count": self.flag_byte_count,
            "flag_digest": self.flag_digest,
            "flag_encoding": self.flag_encoding,
            "flag_object_digest": self.flag_object_digest,
            "schema_id": self.schema_id,
            "snapshot_digest": self.snapshot_digest,
        }
        type(self).from_payload(payload)
        return payload


# One explicit legal transition table shared by the API and projection. A retry
# is a separate operation, not a transition edge from the terminal attempt.
EFFECT_LEGAL_TRANSITIONS: Mapping[str, frozenset[str]] = {
    "prepared": frozenset({"dispatch_may_have_started", "confirmed_not_applied"}),
    "dispatch_may_have_started": frozenset(
        {
            "observed",
            "confirmed_not_applied",
            "unknown",
        }
    ),
    "unknown": frozenset({"observed", "confirmed_not_applied"}),
    "observed": frozenset(),
    "confirmed_not_applied": frozenset(),
}

_C6_EVAL_BINDING_SCHEMA_ID = "muteki.c6-eval-binding-sidecar.v1"
_C6_EVAL_BINDING_FIELDS = frozenset(
    {
        "arm_config_digest",
        "arm_id",
        "assignment_digest",
        "assignment_receipt_digest",
        "budget_point_digest",
        "checker_commitment_digest",
        "compiler_digest",
        "context_digest",
        "context_packet_digest",
        "environment_digest",
        "evaluator_ledger_anchor_digest",
        "feature_version",
        "feature_state_receipt_digest",
        "mode",
        "offline_policy_digest",
        "price_table_digest",
        "randomization_receipt_digest",
        "run_manifest_digest",
        "source_registry_digest",
        "source_registry_receipt_digest",
        "split",
        "study_manifest_digest",
        "worktree_digest",
        "accepted_set_change",
    }
)
_C6_EVAL_BINDING_V2_SCHEMA_ID = "muteki.c6-eval-binding-sidecar.v2"
_C6_EVAL_V2_COMMON_FIELDS = frozenset(
    {
        "assignment_binding_digest",
        "attempt_role_binding_digest",
        "attempt_digest",
        "attempt_id",
        "base_event_id",
        "base_payload_digest",
        "permit_digest",
        "permit_id",
        "phase",
        "role",
        "runtime_binding_digest",
        "schema_id",
        "scope_digest",
        "slot_id",
    }
)
_C6_EVAL_V2_ROOT_BUDGET_FIELDS = frozenset(
    {
        "assignment_binding_digest",
        "first_reservation",
        "root_budget_digest",
    }
)
_C6_EVAL_OUTCOME_SCHEMA_ID = "muteki.c6-eval-outcome.v1"
_C6_EVAL_OUTCOME_COMMON_FIELDS = frozenset(
    {
        "assignment_digest",
        "evaluation_binding_digest",
        "result",
        "run_id",
        "schema_id",
        "scope_digest",
        "terminal_binding_event_digests",
    }
)
_C6_EVAL_OUTCOME_VERIFIED_FIELDS = _C6_EVAL_OUTCOME_COMMON_FIELDS | frozenset(
    {
        "artifact_manifest_digest",
        "checker_build_digest",
        "checker_input_manifest_digest",
        "checker_output_digest",
        "checker_policy_digest",
        "complete_accounting_digest",
    }
)
_C6_EVAL_OUTCOME_UNKNOWN_FIELDS = _C6_EVAL_OUTCOME_COMMON_FIELDS | frozenset(
    {"reason_digest"}
)


def require_positive_effect_revision(revision: int) -> None:
    if type(revision) is not int or revision <= 0:
        raise ValueError("revision must be greater than zero")


def _is_sha256(value: Any) -> bool:
    return (
        type(value) is str
        and len(value) == 64
        and value == value.lower()
        and all(character in "0123456789abcdef" for character in value)
    )


def _strict_nonnegative_int_map(value: Any, *, name: str) -> dict[str, int]:
    try:
        items = dict(value).items()
    except (TypeError, ValueError) as exc:
        raise ValueError(f"{name} must be a mapping") from exc
    result: dict[str, int] = {}
    for key, amount in items:
        if type(key) is not str or not key.strip():
            raise ValueError(f"{name} axes must be non-empty strings")
        if type(amount) is not int or amount < 0:
            raise ValueError(f"{name} must contain non-negative integers")
        result[key] = amount
    if not result:
        raise ValueError(f"{name} is required")
    return result


def _validate_tagged_usage_payload(
    payload: Mapping[str, Any],
    *,
    reserved: Mapping[str, int],
    reservation_ids: Sequence[str],
    unknown_hold: bool,
    charge_key: str | None = None,
) -> dict[str, int]:
    raw_ids = payload.get("reservation_ids")
    if type(raw_ids) not in {list, tuple}:
        raise IntegrityError("usage payload has no reservation identities")
    supplied_ids = tuple(raw_ids)
    expected_ids = tuple(reservation_ids)
    if (
        any(type(item) is not str or not item for item in supplied_ids)
        or len(set(supplied_ids)) != len(supplied_ids)
        or set(supplied_ids) != set(expected_ids)
    ):
        raise IntegrityError("usage payload reservation identities diverged")

    report = payload.get("usage_report")
    if not isinstance(report, Mapping) or set(report) != {"measurements"}:
        raise IntegrityError("tagged usage report is missing or malformed")
    measurements = report["measurements"]
    if type(measurements) not in {list, tuple} or not measurements:
        raise IntegrityError("tagged usage measurements are missing")
    axes: list[str] = []
    charged: dict[str, int] = {}
    unknown_axes = 0
    for measurement in measurements:
        if not isinstance(measurement, Mapping) or set(measurement) != {
            "axis",
            "observed_so_far",
            "reserved_ceiling",
            "status",
        }:
            raise IntegrityError("tagged usage measurement is malformed")
        axis = measurement["axis"]
        observed = measurement["observed_so_far"]
        ceiling = measurement["reserved_ceiling"]
        status = measurement["status"]
        if type(axis) is not str or not axis or axis != axis.strip():
            raise IntegrityError("tagged usage axis is malformed")
        if type(observed) is not int or observed < 0:
            raise IntegrityError("tagged observed usage is malformed")
        if type(ceiling) is not int or ceiling < 0:
            raise IntegrityError("tagged usage ceiling is malformed")
        if status not in {"observed", "partial", "unknown"}:
            raise IntegrityError("tagged usage status is malformed")
        if axis not in reserved or ceiling != reserved[axis]:
            raise IntegrityError("tagged usage does not bind its reservation")
        axes.append(axis)
        charged[axis] = observed if status == "observed" else max(observed, ceiling)
        unknown_axes += int(status == "unknown")
    if axes != sorted(axes) or len(set(axes)) != len(axes):
        raise IntegrityError("tagged usage axes are not canonical")
    if set(axes) != set(reserved):
        raise IntegrityError("tagged usage axes do not cover the reservation")
    if unknown_hold != bool(unknown_axes):
        raise IntegrityError(
            "UNKNOWN usage must be held and non-UNKNOWN usage must settle"
        )
    report_body = {"measurements": list(measurements)}
    if payload.get("usage_report_digest") != canonical_digest(report_body):
        raise IntegrityError("tagged usage report digest mismatch")
    charged_key = charge_key or ("held_usage" if unknown_hold else "actual_usage")
    supplied_charge = _strict_nonnegative_int_map(
        payload.get(charged_key), name=charged_key.replace("_", " ")
    )
    if supplied_charge != charged:
        raise IntegrityError("tagged usage charge does not match its report")
    return charged


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

    event_kinds = {event.kind for event in events}
    mutation_kinds = [mutation.kind for mutation in mutations]
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
                raise IntegrityError(
                    "production context decision identity is malformed"
                )
            expected_event_id = f"event:context-decision:{decision_id}"
            digest_fields = (
                "attempt_digest",
                "context_digest",
                "feature_state_digest",
                "scope_digest",
            )
        elif authority_event.kind == "CONTEXT_PACKET_COMPILED":
            if type(decision_id) is not str or not decision_id:
                raise IntegrityError(
                    "production context decision identity is malformed"
                )
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
                raise IntegrityError(
                    "production context decision identity is malformed"
                )
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
                raise IntegrityError(
                    "production context prompt byte count is malformed"
                )
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
        if authority_event.event_id != expected_event_id or any(
            not _is_sha256(payload.get(name)) for name in digest_fields
        ):
            raise IntegrityError("production context lineage is malformed")

    def event(kind: str) -> CommandEvent:
        matches = [item for item in events if item.kind == kind]
        if len(matches) != 1:
            raise IntegrityError(f"reserved command requires one {kind} event")
        return matches[0]

    def mutation(kind: str) -> ProjectionMutation:
        matches = [item for item in mutations if item.kind == kind]
        if len(matches) != 1:
            raise IntegrityError(
                f"reserved event requires exactly one {kind} semantic mutation"
            )
        return matches[0]

    def exact_binding(event_kind: str, mutation_kind: str) -> None:
        if canonical_json_bytes(event(event_kind).payload) != canonical_json_bytes(
            mutation(mutation_kind).payload
        ):
            raise IntegrityError(
                f"{event_kind} payload diverges from its semantic mutation"
            )

    def require(kind: str) -> None:
        if mutation_kinds.count(kind) != 1:
            raise IntegrityError(
                f"reserved event requires exactly one {kind} semantic mutation"
            )

    from muteki.epistemic.cognitive_events_v1 import (
        COGNITIVE_ASSIGNMENT_SCHEMA_ID,
        COGNITIVE_BINDING_ACTOR,
        COGNITIVE_EXECUTION_OBSERVED,
        COGNITIVE_EXPERIMENT_ASSIGNED,
        COGNITIVE_RUNTIME_CONTEXT_ASSIGNMENT_SCHEMA_ID,
        COGNITIVE_RUNTIME_EXECUTION_SCHEMA_ID,
        COGNITIVE_RUNTIME_EXECUTABLE_ASSIGNMENT_SCHEMA_ID,
        COGNITIVE_RUNTIME_REPRODUCTION_ASSIGNMENT_SCHEMA_ID,
    )
    from muteki.runtime.cognitive_runtime_observation_v1 import (
        COGNITIVE_RUNTIME_OBSERVER_ACTOR,
    )
    from muteki.runtime.canonical_cognitive_selection_v1 import (
        COGNITIVE_CANONICAL_SELECTION_ACTOR,
        COGNITIVE_CANONICAL_SELECTION_BOUND,
        validate_canonical_selection_sidecar_shape,
    )
    from muteki.runtime.canonical_cognitive_continuation_v2 import (
        COGNITIVE_CANONICAL_CONTINUATION_ACTOR_V2,
        COGNITIVE_CANONICAL_CONTINUATION_BOUND_V2,
        COGNITIVE_CANONICAL_CONTINUATION_MUTATION_V2,
        validate_canonical_continuation_sidecar_shape_v2,
    )

    selection_events = [
        item for item in events if item.kind == COGNITIVE_CANONICAL_SELECTION_BOUND
    ]
    selection_mutations = [
        item
        for item in mutations
        if item.kind == "cognitive_canonical_selection_bind_guard"
    ]
    if cognitive_canonical_selection_authorized and not (
        selection_events or selection_mutations
    ):
        raise IntegrityError(
            "canonical selection capability requires its exact inert sidecar"
        )
    if selection_events or selection_mutations:
        exact_inventory = (
            tuple(item.kind for item in events)
            == (
                "ATTEMPT_ADMITTED",
                COGNITIVE_EXPERIMENT_ASSIGNED,
                COGNITIVE_CANONICAL_SELECTION_BOUND,
            )
            and events[1].payload.get("schema_id")
            == COGNITIVE_RUNTIME_CONTEXT_ASSIGNMENT_SCHEMA_ID
            and tuple(mutation_kinds)
            == (
                "attempt_admit",
                "cognitive_experiment_assign_guard",
                "cognitive_canonical_selection_bind_guard",
            )
        )
        if (
            not cognitive_canonical_selection_authorized
            or len(selection_events) != 1
            or len(selection_mutations) != 1
            or not exact_inventory
        ):
            raise IntegrityError(
                "canonical selection requires one exact atomic admission inventory"
            )
        selection_event = selection_events[0]
        if (
            selection_event.actor != COGNITIVE_CANONICAL_SELECTION_ACTOR
            or selection_event.event_id
            != (
                f"event:{COGNITIVE_CANONICAL_SELECTION_BOUND}:"
                f"{selection_event.payload.get('attempt_id')}"
            )
            or canonical_json_bytes(selection_event.payload)
            != canonical_json_bytes(selection_mutations[0].payload)
        ):
            raise IntegrityError("canonical selection sidecar authority diverged")
        try:
            validate_canonical_selection_sidecar_shape(selection_event.payload)
        except (TypeError, ValueError) as exc:
            raise IntegrityError("canonical selection sidecar is malformed") from exc

    continuation_events = [
        item
        for item in events
        if item.kind == COGNITIVE_CANONICAL_CONTINUATION_BOUND_V2
    ]
    continuation_mutations = [
        item
        for item in mutations
        if item.kind == COGNITIVE_CANONICAL_CONTINUATION_MUTATION_V2
    ]
    if cognitive_canonical_continuation_v2_authorized and not (
        continuation_events or continuation_mutations
    ):
        raise IntegrityError(
            "canonical continuation capability requires its exact v2 companion"
        )
    if continuation_events or continuation_mutations:
        exact_inventory = (
            tuple(item.kind for item in events)
            == (
                "ATTEMPT_ADMITTED",
                COGNITIVE_EXPERIMENT_ASSIGNED,
                COGNITIVE_CANONICAL_CONTINUATION_BOUND_V2,
            )
            and events[1].payload.get("schema_id")
            == COGNITIVE_RUNTIME_CONTEXT_ASSIGNMENT_SCHEMA_ID
            and tuple(mutation_kinds)
            == (
                "attempt_admit",
                "cognitive_experiment_assign_guard",
                COGNITIVE_CANONICAL_CONTINUATION_MUTATION_V2,
            )
        )
        if (
            not cognitive_canonical_continuation_v2_authorized
            or cognitive_canonical_selection_authorized
            or len(continuation_events) != 1
            or len(continuation_mutations) != 1
            or not exact_inventory
        ):
            raise IntegrityError(
                "canonical continuation requires one exact atomic v2 inventory"
            )
        continuation_event = continuation_events[0]
        if (
            continuation_event.actor != COGNITIVE_CANONICAL_CONTINUATION_ACTOR_V2
            or continuation_event.event_id
            != (
                f"event:{COGNITIVE_CANONICAL_CONTINUATION_BOUND_V2}:"
                f"{continuation_event.payload.get('attempt_id')}"
            )
            or canonical_json_bytes(continuation_event.payload)
            != canonical_json_bytes(continuation_mutations[0].payload)
        ):
            raise IntegrityError("canonical continuation capability or actor crossed")
        try:
            validate_canonical_continuation_sidecar_shape_v2(continuation_event.payload)
        except (TypeError, ValueError) as exc:
            raise IntegrityError("canonical continuation sidecar is malformed") from exc

    runtime_observation_event = next(
        (
            item
            for item in events
            if item.kind == COGNITIVE_EXECUTION_OBSERVED
            and item.payload.get("schema_id") == COGNITIVE_RUNTIME_EXECUTION_SCHEMA_ID
        ),
        None,
    )
    cognitive_contracts = {
        COGNITIVE_EXPERIMENT_ASSIGNED: "cognitive_experiment_assign_guard",
        COGNITIVE_EXECUTION_OBSERVED: (
            "cognitive_runtime_execution_observe_guard"
            if runtime_observation_event is not None
            else "cognitive_execution_observe_guard"
        ),
    }
    present_cognitive_events = event_kinds & set(cognitive_contracts)
    present_cognitive_mutations = set(mutation_kinds) & (
        set(cognitive_contracts.values())
        | {"cognitive_runtime_execution_observe_guard"}
    )
    if (
        cognitive_evaluation_authorized
        or cognitive_runtime_context_assignment_authorized
        or cognitive_runtime_observation_authorized
    ) and not (present_cognitive_events or present_cognitive_mutations):
        raise IntegrityError(
            "composite cognitive capability requires its exact cognitive sidecar"
        )
    if present_cognitive_events or present_cognitive_mutations:
        if not (
            cognitive_evaluation_authorized
            or cognitive_runtime_context_assignment_authorized
            or cognitive_runtime_observation_authorized
        ):
            raise IntegrityError(
                "cognitive evaluation event requires its composite v2 capability"
            )
        if len(present_cognitive_events) != 1 or len(present_cognitive_mutations) != 1:
            raise IntegrityError(
                "cognitive evaluation command requires one exact semantic sidecar"
            )
        cognitive_kind = next(iter(present_cognitive_events))
        mutation_kind = cognitive_contracts[cognitive_kind]
        if mutation_kind not in present_cognitive_mutations:
            raise IntegrityError("cognitive event/mutation kinds diverged")
        cognitive_event = event(cognitive_kind)
        expected_cognitive_actor = (
            COGNITIVE_RUNTIME_OBSERVER_ACTOR
            if runtime_observation_event is not None
            else COGNITIVE_BINDING_ACTOR
        )
        if cognitive_event.actor != expected_cognitive_actor:
            raise IntegrityError("cognitive event actor is not authoritative")
        if cognitive_kind == COGNITIVE_EXPERIMENT_ASSIGNED:
            schema_id = cognitive_event.payload.get("schema_id")
            if schema_id in {
                COGNITIVE_RUNTIME_CONTEXT_ASSIGNMENT_SCHEMA_ID,
                COGNITIVE_RUNTIME_EXECUTABLE_ASSIGNMENT_SCHEMA_ID,
                COGNITIVE_RUNTIME_REPRODUCTION_ASSIGNMENT_SCHEMA_ID,
            }:
                if not cognitive_runtime_context_assignment_authorized:
                    raise IntegrityError(
                        "runtime-context cognitive assignment requires its exact capability"
                    )
                has_canonical_companion = (
                    cognitive_canonical_selection_authorized
                    or cognitive_canonical_continuation_v2_authorized
                )
                expected_event_count = 3 if has_canonical_companion else 2
                if (
                    len(events) != expected_event_count
                    or events[0].kind != "ATTEMPT_ADMITTED"
                    or events[1].kind != cognitive_kind
                ):
                    raise IntegrityError(
                        "runtime-context cognitive assignment must be atomic with ordinary admission"
                    )
                if cognitive_canonical_selection_authorized:
                    expected_mutations = (
                        "attempt_admit",
                        "cognitive_experiment_assign_guard",
                        "cognitive_canonical_selection_bind_guard",
                    )
                elif cognitive_canonical_continuation_v2_authorized:
                    expected_mutations = (
                        "attempt_admit",
                        "cognitive_experiment_assign_guard",
                        COGNITIVE_CANONICAL_CONTINUATION_MUTATION_V2,
                    )
                else:
                    expected_mutations = (
                        "attempt_admit",
                        "cognitive_experiment_assign_guard",
                    )
                if tuple(mutation_kinds) != expected_mutations:
                    raise IntegrityError(
                        "runtime-context cognitive assignment mutation inventory is not exact"
                    )
            else:
                if not cognitive_evaluation_authorized:
                    raise IntegrityError(
                        "cognitive evaluation event requires its composite v2 capability"
                    )
                if schema_id != COGNITIVE_ASSIGNMENT_SCHEMA_ID:
                    raise IntegrityError(
                        "cognitive assignment schema is not recognized by eval-v2"
                    )
                exact_events = tuple(item.kind for item in events) == (
                    "ATTEMPT_ADMITTED",
                    "C6_EVAL_V2_ATTEMPT_BOUND",
                    cognitive_kind,
                )
                if not exact_events:
                    raise IntegrityError(
                        "cognitive assignment must be atomic with v2 attempt admission"
                    )
                exact_mutations = tuple(mutation_kinds) == (
                    "attempt_admit",
                    "c6_eval_v2_attempt_bind_guard",
                    "cognitive_experiment_assign_guard",
                )
                if not exact_mutations:
                    raise IntegrityError(
                        "cognitive assignment mutation inventory is not exact"
                    )
            identity = cognitive_event.payload.get("attempt_id")
        else:
            if runtime_observation_event is not None:
                if not cognitive_runtime_observation_authorized:
                    raise IntegrityError(
                        "runtime cognitive observation requires its exact capability"
                    )
                if len(events) != 1 or tuple(mutation_kinds) != (
                    "cognitive_runtime_execution_observe_guard",
                ):
                    raise IntegrityError(
                        "runtime cognitive observation command inventory is not exact"
                    )
                identity = cognitive_event.payload.get("permit_id")
                if cognitive_event.event_id != (f"event:{cognitive_kind}:{identity}"):
                    raise IntegrityError(
                        "runtime cognitive observation event identity diverged"
                    )
                exact_binding(cognitive_kind, mutation_kind)
                return
            if not cognitive_evaluation_authorized:
                raise IntegrityError(
                    "cognitive execution requires its composite v2 capability"
                )
            if (
                len(events) != 4
                or events[0].kind not in {"WORKER_TERMINAL", "WORKER_UNKNOWN"}
                or events[1].kind
                not in {
                    "BUDGET_PESSIMISTICALLY_SETTLED",
                    "BUDGET_USAGE_UNKNOWN",
                }
                or events[2].kind != "C6_EVAL_V2_TERMINAL_BOUND"
                or events[3].kind != cognitive_kind
            ):
                raise IntegrityError(
                    "cognitive execution must be atomic with v2 terminal accounting"
                )
            expected_budget_mutation = (
                "budget_unknown"
                if events[1].kind == "BUDGET_USAGE_UNKNOWN"
                else "budget_pessimistic_settle"
            )
            if tuple(mutation_kinds) not in {
                (
                    "worker_terminal_guard",
                    expected_budget_mutation,
                    "c6_eval_v2_terminal_bind_guard",
                    "cognitive_execution_observe_guard",
                ),
                (
                    "orphan_reconcile_guard",
                    expected_budget_mutation,
                    "c6_eval_v2_terminal_bind_guard",
                    "cognitive_execution_observe_guard",
                ),
            }:
                raise IntegrityError(
                    "cognitive execution mutation inventory is not exact"
                )
            identity = cognitive_event.payload.get("permit_id")
        if cognitive_event.event_id != f"event:{cognitive_kind}:{identity}":
            raise IntegrityError("cognitive evaluation event identity diverged")
        exact_binding(cognitive_kind, mutation_kind)

    if "ATTEMPT_ADMITTED" in event_kinds:
        require("attempt_admit")
        exact_binding("ATTEMPT_ADMITTED", "attempt_admit")
        admission_event = event("ATTEMPT_ADMITTED")
        admission_attempt_id = admission_event.payload.get("attempt_id")
        if (
            admission_event.actor != "search-admission"
            or type(admission_attempt_id) is not str
            or not admission_attempt_id
            or admission_event.event_id
            != f"event:attempt:admit:{admission_attempt_id}"
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

    if "CANARY_ADMITTED" in event_kinds:
        if not canary_authorized:
            raise IntegrityError(
                "canary admission requires the catalog-only canary capability"
            )
        exact_binding("CANARY_ADMITTED", "canary_commit_guard")

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
        expected_event_count = (
            (3 if phase == "terminal" else 2)
            + int(cognitive_companion in event_kinds)
        )
        sidecar_ordinal = (
            2 if phase == "terminal" else 1
        )
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


@dataclass(frozen=True, slots=True)
class CommandEvent:
    event_id: str
    kind: str
    actor: str
    occurred_at_ns: int
    payload: Mapping[str, Any] = field(default_factory=dict)


@dataclass(frozen=True, slots=True)
class OutboxIntent:
    outbox_id: str
    topic: str
    payload: Mapping[str, Any] = field(default_factory=dict)


@dataclass(frozen=True, slots=True)
class ProjectionMutation:
    kind: str
    payload: Mapping[str, Any] = field(default_factory=dict)


@dataclass(frozen=True, slots=True)
class CommandCommitResult:
    command_id: str
    receipt_digest: str
    first_seq: int
    last_seq: int
    state_checksum: str
    idempotent: bool = False


_SCHEMA = """
CREATE TABLE IF NOT EXISTS run_meta (
  singleton INTEGER PRIMARY KEY CHECK(singleton=1),
  run_id TEXT NOT NULL UNIQUE,
  protocol_version INTEGER NOT NULL CHECK(protocol_version=2),
  manifest_digest TEXT NOT NULL,
  durability_tier TEXT NOT NULL CHECK(durability_tier IN ('D0_PROCESS','D1_HOST'))
) STRICT;
CREATE TABLE IF NOT EXISTS commands (
  command_id TEXT PRIMARY KEY,
  run_id TEXT NOT NULL,
  idempotency_key TEXT NOT NULL UNIQUE,
  payload_digest TEXT NOT NULL,
  event_count INTEGER NOT NULL CHECK(event_count>0),
  first_seq INTEGER NOT NULL,
  last_seq INTEGER NOT NULL,
  event_set_digest TEXT NOT NULL,
  outbox_set_digest TEXT NOT NULL,
  receipt_json TEXT NOT NULL,
  receipt_digest TEXT NOT NULL,
  committed_at_ns INTEGER NOT NULL,
  FOREIGN KEY(run_id) REFERENCES run_meta(run_id)
) STRICT;
CREATE TABLE IF NOT EXISTS events (
  seq INTEGER PRIMARY KEY AUTOINCREMENT,
  event_id TEXT NOT NULL UNIQUE,
  run_id TEXT NOT NULL,
  command_id TEXT NOT NULL,
  ordinal INTEGER NOT NULL CHECK(ordinal>=0),
  kind TEXT NOT NULL,
  actor TEXT NOT NULL,
  occurred_at_ns INTEGER NOT NULL CHECK(occurred_at_ns>=0),
  payload_json TEXT NOT NULL,
  parent_event_digest TEXT NOT NULL,
  event_digest TEXT NOT NULL UNIQUE,
  UNIQUE(command_id, ordinal),
  FOREIGN KEY(command_id) REFERENCES commands(command_id) DEFERRABLE INITIALLY DEFERRED,
  FOREIGN KEY(run_id) REFERENCES run_meta(run_id)
) STRICT;
CREATE TABLE IF NOT EXISTS immutable_outbox (
  outbox_id TEXT PRIMARY KEY,
  command_id TEXT NOT NULL,
  ordinal INTEGER NOT NULL CHECK(ordinal>=0),
  topic TEXT NOT NULL,
  payload_json TEXT NOT NULL,
  payload_digest TEXT NOT NULL,
  UNIQUE(command_id, ordinal),
  FOREIGN KEY(command_id) REFERENCES commands(command_id) DEFERRABLE INITIALLY DEFERRED
) STRICT;
CREATE TABLE IF NOT EXISTS state_projection (
  singleton INTEGER PRIMARY KEY CHECK(singleton=1),
  head_seq INTEGER NOT NULL,
  state_json TEXT NOT NULL,
  checksum TEXT NOT NULL
) STRICT;
CREATE TABLE IF NOT EXISTS runtime_branches (
  branch_id TEXT PRIMARY KEY,
  state TEXT NOT NULL CHECK(state IN ('open','suspended','resolved','closed')),
  depends_on_json TEXT NOT NULL,
  max_attempts INTEGER NOT NULL CHECK(max_attempts>0),
  attempt_count INTEGER NOT NULL DEFAULT 0 CHECK(attempt_count>=0)
) STRICT;
CREATE TABLE IF NOT EXISTS budget_accounts (
  account_id TEXT PRIMARY KEY,
  parent_id TEXT,
  limits_json TEXT NOT NULL,
  settled_json TEXT NOT NULL,
  held_json TEXT NOT NULL,
  debt INTEGER NOT NULL DEFAULT 0 CHECK(debt IN (0,1)),
  FOREIGN KEY(parent_id) REFERENCES budget_accounts(account_id)
) STRICT;
CREATE TABLE IF NOT EXISTS runtime_attempts (
  attempt_id TEXT PRIMARY KEY,
  branch_id TEXT NOT NULL,
  permit_id TEXT NOT NULL UNIQUE,
  scope_digest TEXT NOT NULL,
  lease_id TEXT NOT NULL UNIQUE,
  lease_epoch INTEGER NOT NULL,
  worker_generation INTEGER NOT NULL,
  fingerprint TEXT NOT NULL,
  effect_class TEXT NOT NULL,
  state TEXT NOT NULL CHECK(state IN ('reserved','running','terminal','unknown')),
  FOREIGN KEY(branch_id) REFERENCES runtime_branches(branch_id)
) STRICT;
CREATE TABLE IF NOT EXISTS budget_reservations (
  reservation_id TEXT PRIMARY KEY,
  account_id TEXT NOT NULL,
  attempt_id TEXT NOT NULL,
  dimensions_json TEXT NOT NULL,
  state TEXT NOT NULL CHECK(state IN ('active','settled','unknown','released')),
  FOREIGN KEY(account_id) REFERENCES budget_accounts(account_id),
  FOREIGN KEY(attempt_id) REFERENCES runtime_attempts(attempt_id)
) STRICT;
CREATE TABLE IF NOT EXISTS effect_conflict_holds (
  conflict_key TEXT PRIMARY KEY,
  operation_id TEXT NOT NULL,
  state TEXT NOT NULL CHECK(state IN ('active','unknown'))
) STRICT;
CREATE TABLE IF NOT EXISTS effect_operations (
  operation_id TEXT PRIMARY KEY,
  attempt_id TEXT NOT NULL,
  effect_class TEXT NOT NULL,
  conflict_keys_json TEXT NOT NULL,
  state TEXT NOT NULL CHECK(state IN ('prepared','dispatch_may_have_started','observed','confirmed_not_applied','unknown')),
  current_ordinal INTEGER NOT NULL CHECK(current_ordinal>0),
  FOREIGN KEY(attempt_id) REFERENCES runtime_attempts(attempt_id)
) STRICT;
CREATE TABLE IF NOT EXISTS effect_attempts (
  operation_id TEXT NOT NULL,
  ordinal INTEGER NOT NULL CHECK(ordinal>0),
  state TEXT NOT NULL,
  PRIMARY KEY(operation_id,ordinal),
  FOREIGN KEY(operation_id) REFERENCES effect_operations(operation_id)
) STRICT;
CREATE TABLE IF NOT EXISTS catalog_drafts (
  draft_id TEXT PRIMARY KEY,
  policy_json TEXT NOT NULL,
  state TEXT NOT NULL CHECK(state IN ('open','provisioning','sealed','failed'))
) STRICT;
CREATE TABLE IF NOT EXISTS catalog_attachments (
  attachment_id TEXT PRIMARY KEY,
  draft_id TEXT NOT NULL,
  digest TEXT NOT NULL,
  byte_count INTEGER NOT NULL CHECK(byte_count>=0),
  FOREIGN KEY(draft_id) REFERENCES catalog_drafts(draft_id)
) STRICT;
CREATE TABLE IF NOT EXISTS provision_operations (
  operation_id TEXT PRIMARY KEY,
  draft_id TEXT NOT NULL,
  allocated_run_id TEXT NOT NULL UNIQUE,
  target_root TEXT NOT NULL,
  manifest_digest TEXT NOT NULL,
  owner_epoch INTEGER NOT NULL,
  state TEXT NOT NULL CHECK(state IN ('preparing','run_allocated','run_materialized','sealed','failed_seal')),
  FOREIGN KEY(draft_id) REFERENCES catalog_drafts(draft_id)
) STRICT;
CREATE TABLE IF NOT EXISTS catalog_runs (
  run_id TEXT PRIMARY KEY,
  operation_id TEXT NOT NULL UNIQUE,
  manifest_digest TEXT NOT NULL,
  anchor_digest TEXT,
  state TEXT NOT NULL CHECK(state IN ('allocating','sealed','failed_seal','archived','purged')),
  FOREIGN KEY(operation_id) REFERENCES provision_operations(operation_id)
) STRICT;
"""

_LIFECYCLE_SCHEMA = """
CREATE TABLE IF NOT EXISTS archive_operations (
  operation_id TEXT PRIMARY KEY,
  run_id TEXT NOT NULL UNIQUE,
  owner_epoch INTEGER NOT NULL CHECK(owner_epoch>0),
  state TEXT NOT NULL CHECK(state IN ('requested','archived')),
  run_receipt_digest TEXT NOT NULL DEFAULT '',
  archive_receipt_digest TEXT NOT NULL DEFAULT '',
  requested_at_ns INTEGER NOT NULL CHECK(requested_at_ns>=0),
  FOREIGN KEY(run_id) REFERENCES catalog_runs(run_id)
) STRICT;
CREATE TABLE IF NOT EXISTS purge_operations (
  operation_id TEXT PRIMARY KEY,
  run_id TEXT NOT NULL UNIQUE,
  owner_epoch INTEGER NOT NULL CHECK(owner_epoch>0),
  state TEXT NOT NULL CHECK(state IN ('purge_pending','purged','purge_failed','purge_unknown')),
  plan_digest TEXT NOT NULL,
  plan_receipt_digest TEXT NOT NULL,
  absence_receipt_digest TEXT NOT NULL DEFAULT '',
  requested_at_ns INTEGER NOT NULL CHECK(requested_at_ns>=0),
  FOREIGN KEY(run_id) REFERENCES catalog_runs(run_id)
) STRICT;
CREATE TABLE IF NOT EXISTS purge_plan_items (
  operation_id TEXT NOT NULL,
  ordinal INTEGER NOT NULL CHECK(ordinal>=0),
  locator TEXT NOT NULL,
  adapter TEXT NOT NULL,
  state TEXT NOT NULL CHECK(state IN ('pending','absent','unknown')),
  action_receipt_digest TEXT NOT NULL DEFAULT '',
  absence_receipt_digest TEXT NOT NULL DEFAULT '',
  PRIMARY KEY(operation_id,ordinal),
  UNIQUE(operation_id,locator),
  FOREIGN KEY(operation_id) REFERENCES purge_operations(operation_id)
) STRICT;
CREATE TABLE IF NOT EXISTS catalog_tombstones (
  run_id TEXT PRIMARY KEY,
  purge_operation_id TEXT NOT NULL UNIQUE,
  plan_digest TEXT NOT NULL,
  absence_receipt_digest TEXT NOT NULL,
  purged_at_ns INTEGER NOT NULL CHECK(purged_at_ns>=0),
  FOREIGN KEY(run_id) REFERENCES catalog_runs(run_id),
  FOREIGN KEY(purge_operation_id) REFERENCES purge_operations(operation_id)
) STRICT;
"""

_RECEIPT_OBJECT_SCHEMA = """
CREATE TABLE IF NOT EXISTS command_receipt_objects (
  receipt_digest TEXT PRIMARY KEY,
  command_id TEXT NOT NULL UNIQUE,
  first_seq INTEGER NOT NULL CHECK(first_seq>0),
  last_seq INTEGER NOT NULL CHECK(last_seq>=first_seq),
  object_digest TEXT NOT NULL,
  byte_count INTEGER NOT NULL CHECK(byte_count>0),
  state TEXT NOT NULL CHECK(state IN ('resolved','unresolved','unknown','rebound')),
  diagnostic_receipt_digest TEXT NOT NULL DEFAULT '',
  FOREIGN KEY(command_id) REFERENCES commands(command_id) DEFERRABLE INITIALLY DEFERRED
) STRICT;
"""


def _immutable_triggers(table: str) -> str:
    return f"""
CREATE TRIGGER IF NOT EXISTS {table}_no_update
BEFORE UPDATE ON {table} BEGIN SELECT RAISE(ABORT, '{table} is append-only'); END;
CREATE TRIGGER IF NOT EXISTS {table}_no_delete
BEFORE DELETE ON {table} BEGIN SELECT RAISE(ABORT, '{table} is append-only'); END;
"""


class EpistemicSQLiteStore:
    def __init__(self, path: Path, conn: sqlite3.Connection) -> None:
        self.path = path
        self._conn = conn
        self._lock = threading.RLock()
        self._gate_commit_capability = object()
        self._lifecycle_commit_capability = object()
        self._canary_commit_capability = object()
        self._evaluation_commit_capability = object()
        self._evaluation_v2_commit_capability = object()
        # Composite capability: callers must already satisfy every v2 evaluation
        # guard and additionally own the default-off cognitive sidecar boundary.
        # The ordinary v2 capability can never emit a cognitive event.
        self._evaluation_v2_cognitive_commit_capability = object()
        self._evaluation_checker_commit_capability = object()
        self._c6_decision_commit_capability = object()
        self._cognitive_context_commit_capability = object()
        # Composite capability for the explicit default-off runtime-context seam.
        # It can only atomically add one canonical cognitive assignment to one
        # ordinary ContextPacket-bound admission; it is not a dispatch token.
        self._cognitive_context_assignment_commit_capability = object()
        # Strictly stronger composite capability for the explicit canonical
        # selection admission.  It must add the inert selection guard beside the
        # unchanged runtime-context assignment in the same command.
        self._cognitive_canonical_selection_commit_capability = object()
        # Separate versioned companion for one exact distinct experiment after
        # HELD_UNKNOWN.  It cannot emit or reinterpret the v1 EXPERIMENT sidecar.
        self._cognitive_canonical_continuation_v2_commit_capability = object()
        # The audited C6 Popen reader alone may seal reserved cognitive stdout/
        # stderr capture ids.  Ordinary CaptureSession callers cannot acquire this
        # actor/capability pair and therefore cannot substitute arbitrary bytes.
        self._cognitive_runtime_output_commit_capability = object()
        # Separate compare-and-append authority for one post-terminal runtime
        # structural observation.  It cannot assign, dispatch, verify, or learn.
        self._cognitive_runtime_observation_commit_capability = object()
        # Split pre-Popen reproduction evidence: a declaration cannot mint the
        # launcher-owned actual witness, and the launcher cannot backfill intent.
        self._cognitive_reproduction_declaration_commit_capability = object()
        self._cognitive_reproduction_launch_witness_commit_capability = object()
        # Checker input/output/CHECKED share one capability but cannot emit the
        # resolver-only RESOLVED event added at the next authority boundary.
        self._cognitive_verification_checker_commit_capability = object()
        # Separate compare-and-append authority for CHECKED -> RESOLVED.  It can
        # emit neither checker events nor any admission/dispatch/gate effect.
        self._cognitive_verification_resolver_commit_capability = object()
        # A C6 host launch has one deliberately narrow cross-process critical
        # section: final durable validation -> local Popen -> durable terminal
        # receipt.  ``commit_command`` normally owns a short transaction per
        # command; this tuple marks the one internal transaction that is allowed
        # to span that external observation boundary.  It is never a generic
        # transaction escape hatch.
        self._c6_host_launch_fence: tuple[int, str, str] | None = None

    @classmethod
    def create(
        cls,
        *,
        path: Path,
        run_id: str,
        manifest_digest: str,
        durability_tier: str = "D0_PROCESS",
    ) -> "EpistemicSQLiteStore":
        path = Path(path)
        path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
        os.chmod(path.parent, 0o700)
        conn = sqlite3.connect(path, isolation_level=None, check_same_thread=False)
        store = cls(path, conn)
        store._configure(durability_tier)
        conn.executescript(_SCHEMA)
        conn.executescript(_LIFECYCLE_SCHEMA)
        conn.executescript(_RECEIPT_OBJECT_SCHEMA)
        for table in (
            "run_meta",
            "commands",
            "events",
            "immutable_outbox",
            "command_receipt_objects",
        ):
            conn.executescript(_immutable_triggers(table))
        conn.execute(
            "INSERT INTO run_meta(singleton,run_id,protocol_version,manifest_digest,durability_tier) "
            "VALUES(1,?,2,?,?)",
            (run_id, manifest_digest, durability_tier),
        )
        state = initial_state(run_id)
        conn.execute(
            "INSERT INTO state_projection(singleton,head_seq,state_json,checksum) VALUES(1,0,?,?)",
            (canonical_json_bytes(state.as_dict()).decode(), state.checksum),
        )
        os.chmod(path, 0o600)
        return store

    @classmethod
    def open(cls, path: Path) -> "EpistemicSQLiteStore":
        path = Path(path)
        conn = sqlite3.connect(path, isolation_level=None, check_same_thread=False)
        row = conn.execute(
            "SELECT durability_tier FROM run_meta WHERE singleton=1"
        ).fetchone()
        if row is None:
            conn.close()
            raise IntegrityError("missing immutable run anchor")
        store = cls(path, conn)
        store._configure(str(row[0]))
        # Protocol 2 schemas are additive until a production cutover. Opening an
        # earlier catalog installs lifecycle projections before verification;
        # canonical events remain the authority and rebuild populates the tables.
        conn.executescript(_LIFECYCLE_SCHEMA)
        conn.executescript(_RECEIPT_OBJECT_SCHEMA)
        conn.executescript(_immutable_triggers("command_receipt_objects"))
        store.verify()
        return store

    def _configure(self, durability_tier: str) -> None:
        if durability_tier not in {"D0_PROCESS", "D1_HOST"}:
            raise ValueError("unsupported durability tier")
        self._conn.execute("PRAGMA foreign_keys=ON")
        self._conn.execute("PRAGMA journal_mode=WAL")
        self._conn.execute(
            "PRAGMA synchronous="
            + ("FULL" if durability_tier == "D1_HOST" else "NORMAL")
        )

    @contextmanager
    def stable_read_snapshot(self) -> Iterator[None]:
        """Hold one local SQLite read snapshot across a compound proof.

        This serializes same-process users of this store object and pins one WAL
        snapshot for cross-table resolution. It is not a distributed transaction
        and it does not cover CAS reads or external projections.
        """

        with self._lock:
            if self._conn.in_transaction:
                yield
                return
            self._conn.execute("BEGIN")
            try:
                # Establish the snapshot before caller code can perform its first
                # read; a deferred BEGIN alone does not pin WAL visibility.
                self._conn.execute(
                    "SELECT head_seq FROM state_projection WHERE singleton=1"
                ).fetchone()
                yield
            finally:
                self._conn.rollback()

    @property
    def run_id(self) -> str:
        row = self._conn.execute(
            "SELECT run_id FROM run_meta WHERE singleton=1"
        ).fetchone()
        if row is None:
            raise IntegrityError("missing run anchor")
        return str(row[0])

    def run_anchor(self) -> dict[str, str]:
        with self._lock:
            row = self._conn.execute(
                "SELECT run_id,manifest_digest,durability_tier FROM run_meta "
                "WHERE singleton=1"
            ).fetchone()
        if row is None:
            raise IntegrityError("missing run anchor")
        return {
            "run_id": str(row[0]),
            "manifest_digest": str(row[1]),
            "durability_tier": str(row[2]),
        }

    def close(self) -> None:
        self._conn.close()

    def _commit_c6_host_launch_fence_locked(self) -> None:
        """Commit the outer launch transaction (a narrow fault-injection seam)."""

        self._conn.commit()

    @contextmanager
    def c6_host_launch_fence(self, *, claim_id: str, stage_id: str) -> Iterator[None]:
        """Serialize one C6 final-Popen boundary with all SQLite writers.

        This is intentionally narrower than a host-ownership protocol.  It
        prevents a cooperative second SQLite writer from inserting UNKNOWN,
        budget closure, or BOOT state after the final durable claim check but
        before the local process-start receipt.  It does *not* make child
        creation transactional: a crash after Popen still rolls this transaction
        back and leaves the prior claim for fail-closed UNKNOWN recovery.

        The store's re-entrant lock is held across the context so same-process
        writers cannot bypass the SQLite writer lock either.  Only the exact
        terminal command for this claim/stage may use ``commit_command`` while
        the fence is active.
        """

        if type(claim_id) is not str or not claim_id:
            raise ValueError("claim_id must be exact non-empty text")
        if type(stage_id) is not str or not stage_id:
            raise ValueError("stage_id must be exact non-empty text")
        with self._lock:
            if self._c6_host_launch_fence is not None:
                raise IntegrityError("C6 host launch fence is already active")
            self._conn.execute("BEGIN IMMEDIATE")
            self._c6_host_launch_fence = (
                threading.get_ident(),
                claim_id,
                stage_id,
            )
            try:
                yield
            except BaseException:
                self._conn.rollback()
                raise
            else:
                try:
                    self._commit_c6_host_launch_fence_locked()
                except BaseException:
                    self._conn.rollback()
                    raise
            finally:
                self._c6_host_launch_fence = None

    def _assert_c6_host_launch_fence_command_locked(
        self, *, events: Sequence[CommandEvent]
    ) -> bool:
        """Return whether this command is nested in the active C6 fence."""

        active = self._c6_host_launch_fence
        if active is None:
            return False
        owner_thread_id, claim_id, stage_id = active
        if owner_thread_id != threading.get_ident():
            # The enclosing RLock should make this unreachable.  Keep the check
            # explicit so a future refactor cannot silently share an open SQLite
            # transaction with another host thread.
            raise IntegrityError("C6 host launch fence belongs to another thread")
        if len(events) != 1:
            raise IntegrityError(
                "C6 host launch fence permits exactly one terminal event"
            )
        event = events[0]
        if event.kind not in {
            "CONTEXT_PROMPT_RELEASED",
            "CONTEXT_PROMPT_PRELAUNCH_ABORTED",
            "CONTEXT_PROMPT_UNKNOWN",
        }:
            raise IntegrityError("C6 host launch fence permits only prompt terminals")
        payload = event.payload
        if payload.get("stage_id") != stage_id:
            raise IntegrityError("C6 host launch fence terminal stage diverged")
        if (
            event.kind != "CONTEXT_PROMPT_UNKNOWN"
            and payload.get("claim_id") != claim_id
        ):
            raise IntegrityError("C6 host launch fence terminal claim diverged")
        return True

    def _state(self) -> CanonicalState:
        row = self._conn.execute(
            "SELECT state_json,checksum FROM state_projection WHERE singleton=1"
        ).fetchone()
        if row is None:
            raise IntegrityError("missing state projection")
        data = json.loads(row[0])
        from muteki.epistemic.folds import KernelHealth, RunExecution, SearchControlMode

        state = CanonicalState(
            run_id=data["run_id"],
            head_seq=int(data["head_seq"]),
            head_event_digest=data["head_event_digest"],
            command_count=int(data["command_count"]),
            event_count=int(data["event_count"]),
            kernel_health=KernelHealth(data["kernel_health"]),
            run_execution=RunExecution(data["run_execution"]),
            search_mode=SearchControlMode(data["search_mode"]),
            run_fence_epoch=int(data["run_fence_epoch"]),
            execution_generation=int(data["execution_generation"]),
            completion_generation=int(data["completion_generation"]),
        )
        if state.checksum != row[1]:
            raise IntegrityError("state projection checksum mismatch")
        return state

    def state(self) -> CanonicalState:
        with self._lock:
            return self._state()

    def budget_ancestry(self, account_id: str) -> tuple[str, ...]:
        """Read-only narrow query; callers never receive the SQLite connection."""
        with self._lock:
            rows = self._conn.execute(
                "WITH RECURSIVE ancestry(account_id,parent_id) AS ("
                " SELECT account_id,parent_id FROM budget_accounts WHERE account_id=?"
                " UNION ALL SELECT b.account_id,b.parent_id FROM budget_accounts b"
                " JOIN ancestry a ON b.account_id=a.parent_id)"
                " SELECT account_id FROM ancestry",
                (account_id,),
            ).fetchall()
        return tuple(str(row[0]) for row in rows)

    def budget_remaining(self, account_id: str) -> dict[str, int]:
        """Return the current canonical budget left in one account.

        This is deliberately a projection read, not a forecast.  Callers that need
        to use it in an authority decision must snapshot the returned values into
        their own canonical event before any later admission can reserve budget.
        """

        if (
            type(account_id) is not str
            or not account_id
            or account_id != account_id.strip()
        ):
            raise ValueError("account_id must be exact non-empty text")
        with self._lock:
            row = self._conn.execute(
                "SELECT limits_json,settled_json,held_json,debt "
                "FROM budget_accounts WHERE account_id=?",
                (account_id,),
            ).fetchone()
        if row is None:
            raise KeyError(account_id)
        if int(row[3]) != 0:
            raise IntegrityError("budget account is in debt")
        limits = self._json_map(row[0])
        settled = self._json_map(row[1])
        held = self._json_map(row[2])
        if set(limits) != set(settled) or set(limits) != set(held):
            raise IntegrityError("budget account projection axes diverged")
        remaining = {
            axis: limits[axis] - settled[axis] - held[axis] for axis in sorted(limits)
        }
        if any(value < 0 for value in remaining.values()):
            raise IntegrityError("budget account remaining amount is negative")
        return remaining

    def _is_c6_v2_observer_attempt_locked(self, attempt_id: object) -> bool:
        if type(attempt_id) is not str or not attempt_id:
            return False
        rows = self._conn.execute(
            "SELECT payload_json FROM events WHERE kind='C6_EVAL_V2_ATTEMPT_BOUND'"
        ).fetchall()
        matches = [
            json.loads(row[0])
            for row in rows
            if json.loads(row[0]).get("attempt_id") == attempt_id
        ]
        if len(matches) > 1:
            raise IntegrityError("C6 evaluation v2 observer identity is ambiguous")
        return bool(matches and matches[0].get("role") == "observer")

    def _reject_c6_v2_observer_events_locked(
        self, events: Sequence[CommandEvent]
    ) -> None:
        for event in events:
            if event.kind not in {"PROGRESS_RECORDED", "ATTEMPT_BARREN"}:
                continue
            if self._is_c6_v2_observer_attempt_locked(event.payload.get("attempt_id")):
                raise IntegrityError("C6 evaluation v2 observer cannot update progress")

    def _require_c6_v2_terminal_accounting_locked(
        self, events: Sequence[CommandEvent]
    ) -> None:
        terminal_events = tuple(
            event
            for event in events
            if event.kind in {"WORKER_TERMINAL", "WORKER_UNKNOWN"}
        )
        if not terminal_events:
            return
        rows = self._conn.execute(
            "SELECT payload_json FROM events "
            "WHERE kind='C6_EVAL_V2_LAUNCH_BOUND' ORDER BY seq"
        ).fetchall()
        launch_payloads = tuple(json.loads(row[0]) for row in rows)
        cognitive_assignment_rows = self._conn.execute(
            "SELECT payload_json FROM events "
            "WHERE kind='COGNITIVE_EXPERIMENT_ASSIGNED' ORDER BY seq"
        ).fetchall()
        cognitive_assignments = tuple(
            json.loads(row[0]) for row in cognitive_assignment_rows
        )
        event_kinds = {event.kind for event in events}
        for terminal in terminal_events:
            permit_id = terminal.payload.get("permit_id")
            matching = tuple(
                payload
                for payload in launch_payloads
                if payload.get("permit_id") == permit_id
            )
            if len(matching) > 1:
                raise IntegrityError("evaluation v2 launch identity is ambiguous")
            if not matching:
                continue
            bound_cognitive = tuple(
                payload
                for payload in cognitive_assignments
                if payload.get("permit_id") == permit_id
            )
            if len(bound_cognitive) > 1:
                raise IntegrityError(
                    "v2 terminal has ambiguous cognitive assignment lineage"
                )
            if bound_cognitive and "COGNITIVE_EXECUTION_OBSERVED" not in event_kinds:
                raise IntegrityError(
                    "cognitive-bound v2 terminal requires its atomic observation"
                )
            if not bound_cognitive and "COGNITIVE_EXECUTION_OBSERVED" in event_kinds:
                raise IntegrityError(
                    "v2 terminal cannot mint an unassigned cognitive observation"
                )
            if "C6_EVAL_V2_TERMINAL_BOUND" not in event_kinds or not event_kinds & {
                "BUDGET_PESSIMISTICALLY_SETTLED",
                "BUDGET_SETTLED",
                "BUDGET_USAGE_UNKNOWN",
            }:
                raise IntegrityError(
                    "evaluation v2 terminal requires atomic budget and sidecar closure"
                )

    def commit_command(
        self,
        *,
        command_id: str,
        idempotency_key: str,
        command_payload: Mapping[str, Any],
        events: Sequence[CommandEvent],
        outbox: Sequence[OutboxIntent] = (),
        committed_at_ns: int,
        projection_mutations: Sequence[ProjectionMutation] = (),
        authority_capability: object | None = None,
        fault_hook: Callable[[str], None] | None = None,
        forbid_attempt_admission_id: str | None = None,
        required_prior_event: tuple[str, Mapping[str, Any]] | None = None,
        forbid_prior_events: Sequence[tuple[str, Mapping[str, Any]]] = (),
    ) -> CommandCommitResult:
        if not command_id or not idempotency_key or not events:
            raise ValueError(
                "command_id, idempotency_key and at least one event are required"
            )
        payload_digest = canonical_digest(command_payload)
        with self._lock:
            fenced = self._assert_c6_host_launch_fence_command_locked(events=events)
            savepoint = ""
            if fenced:
                # A terminal command is nested in the long-lived C6 launch
                # transaction.  It still needs an all-or-nothing boundary of its
                # own: a receipt/CAS/projection failure must not leave inserted
                # command/event rows for the outer fence to commit.  The caller
                # can then record UNKNOWN in a fresh savepoint, or leave the claim
                # unresolved for recovery, without corrupting replay state.
                savepoint = "c6_host_launch_command"
                self._conn.execute(f"SAVEPOINT {savepoint}")
            else:
                self._conn.execute("BEGIN IMMEDIATE")
            try:
                prior = self._conn.execute(
                    "SELECT command_id,payload_digest,receipt_json FROM commands "
                    "WHERE idempotency_key=?",
                    (idempotency_key,),
                ).fetchone()
                if prior is not None:
                    if prior[1] != payload_digest:
                        raise IdempotencyConflict(
                            "same idempotency key used with a different payload"
                        )
                    if fenced:
                        # A fence begins only after the exact claim was checked
                        # terminal-free.  Seeing an idempotence row here would
                        # mean a nested caller is attempting to reuse an already
                        # committed terminal rather than close this live boundary.
                        raise IntegrityError(
                            "C6 host launch fence encountered an existing terminal command"
                        )
                    receipt = json.loads(prior[2])
                    self._conn.rollback()
                    return CommandCommitResult(
                        command_id=str(prior[0]),
                        receipt_digest=str(receipt["receipt_digest"]),
                        first_seq=int(receipt["first_seq"]),
                        last_seq=int(receipt["last_seq"]),
                        state_checksum=str(receipt["state_checksum"]),
                        idempotent=True,
                    )

                if forbid_attempt_admission_id is not None:
                    if (
                        type(forbid_attempt_admission_id) is not str
                        or not forbid_attempt_admission_id
                    ):
                        raise ValueError(
                            "forbid_attempt_admission_id must be exact non-empty text"
                        )
                    admitted = any(
                        json.loads(row[0]).get("attempt_id")
                        == forbid_attempt_admission_id
                        for row in self._conn.execute(
                            "SELECT payload_json FROM events "
                            "WHERE kind='ATTEMPT_ADMITTED'"
                        ).fetchall()
                    )
                    if admitted:
                        raise IntegrityError(
                            "command must precede target attempt admission"
                        )

                if required_prior_event is not None:
                    if (
                        type(required_prior_event) is not tuple
                        or len(required_prior_event) != 2
                        or type(required_prior_event[0]) is not str
                        or not required_prior_event[0]
                        or not isinstance(required_prior_event[1], Mapping)
                        or not required_prior_event[1]
                    ):
                        raise ValueError(
                            "required_prior_event must be (kind, non-empty mapping)"
                        )
                    required_kind, required_fields = required_prior_event
                    matches = [
                        json.loads(row[0])
                        for row in self._conn.execute(
                            "SELECT payload_json FROM events WHERE kind=?",
                            (required_kind,),
                        ).fetchall()
                        if all(
                            json.loads(row[0]).get(name) == value
                            for name, value in required_fields.items()
                        )
                    ]
                    if len(matches) != 1:
                        raise IntegrityError(
                            "required canonical predecessor event is absent or ambiguous"
                        )

                if type(forbid_prior_events) is not tuple:
                    # The immutable command API uses tuples at authority boundaries
                    # so a caller cannot mutate the absence predicates while the
                    # transaction is being prepared.
                    raise TypeError("forbid_prior_events must be a built-in tuple")
                for predicate in forbid_prior_events:
                    if (
                        type(predicate) is not tuple
                        or len(predicate) != 2
                        or type(predicate[0]) is not str
                        or not predicate[0]
                        or not isinstance(predicate[1], Mapping)
                        or not predicate[1]
                    ):
                        raise ValueError(
                            "forbid_prior_events entries must be "
                            "(kind, non-empty mapping)"
                        )
                    forbidden_kind, forbidden_fields = predicate
                    matches = [
                        json.loads(row[0])
                        for row in self._conn.execute(
                            "SELECT payload_json FROM events WHERE kind=?",
                            (forbidden_kind,),
                        ).fetchall()
                        if all(
                            json.loads(row[0]).get(name) == value
                            for name, value in forbidden_fields.items()
                        )
                    ]
                    if matches:
                        raise IntegrityError(
                            "canonical absence predicate is no longer satisfied"
                        )

                closed = self._conn.execute(
                    "SELECT 1 FROM events WHERE kind='S4E_CLOSURE_ATTESTED' LIMIT 1"
                ).fetchone()
                if closed is not None:
                    raise IntegrityError(
                        "S4-E closure permanently seals the canonical run log"
                    )
                if (
                    any(event.kind == "S4E_CLOSURE_ATTESTED" for event in events)
                    and outbox
                ):
                    raise IntegrityError(
                        "S4-E closure command cannot emit outbox effects"
                    )
                if (
                    any(event.kind.startswith("C6_EVAL_") for event in events)
                    and outbox
                ):
                    raise IntegrityError(
                        "C6 evaluation authority command cannot emit outbox effects"
                    )
                if (
                    any(event.kind.startswith("COGNITIVE_") for event in events)
                    and outbox
                ):
                    raise IntegrityError(
                        "cognitive evaluation command cannot emit outbox effects"
                    )
                if (
                    any(
                        event.kind
                        in {
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
                        for event in events
                    )
                    and outbox
                ):
                    raise IntegrityError(
                        "production context authority command cannot emit outbox effects"
                    )
                self._reject_c6_v2_observer_events_locked(events)
                self._require_c6_v2_terminal_accounting_locked(events)

                _require_authority_mutations(
                    events,
                    projection_mutations,
                    outbox,
                    gate_authorized=(
                        authority_capability is self._gate_commit_capability
                    ),
                    lifecycle_authorized=(
                        authority_capability is self._lifecycle_commit_capability
                    ),
                    canary_authorized=(
                        authority_capability is self._canary_commit_capability
                    ),
                    evaluation_authorized=(
                        authority_capability is self._evaluation_commit_capability
                    ),
                    evaluation_v2_authorized=(
                        authority_capability is self._evaluation_v2_commit_capability
                        or authority_capability
                        is self._evaluation_v2_cognitive_commit_capability
                    ),
                    cognitive_evaluation_authorized=(
                        authority_capability
                        is self._evaluation_v2_cognitive_commit_capability
                    ),
                    cognitive_runtime_context_assignment_authorized=(
                        authority_capability
                        is self._cognitive_context_assignment_commit_capability
                        or authority_capability
                        is self._cognitive_canonical_selection_commit_capability
                        or authority_capability
                        is self._cognitive_canonical_continuation_v2_commit_capability
                    ),
                    cognitive_canonical_selection_authorized=(
                        authority_capability
                        is self._cognitive_canonical_selection_commit_capability
                    ),
                    cognitive_canonical_continuation_v2_authorized=(
                        authority_capability
                        is self._cognitive_canonical_continuation_v2_commit_capability
                    ),
                    cognitive_runtime_output_authorized=(
                        authority_capability
                        is self._cognitive_runtime_output_commit_capability
                    ),
                    cognitive_runtime_observation_authorized=(
                        authority_capability
                        is self._cognitive_runtime_observation_commit_capability
                    ),
                    cognitive_reproduction_declaration_authorized=(
                        authority_capability
                        is self._cognitive_reproduction_declaration_commit_capability
                    ),
                    cognitive_reproduction_launch_witness_authorized=(
                        authority_capability
                        is self._cognitive_reproduction_launch_witness_commit_capability
                    ),
                    cognitive_verification_checker_authorized=(
                        authority_capability
                        is self._cognitive_verification_checker_commit_capability
                    ),
                    cognitive_verification_resolver_authorized=(
                        authority_capability
                        is self._cognitive_verification_resolver_commit_capability
                    ),
                    evaluation_checker_authorized=(
                        authority_capability
                        is self._evaluation_checker_commit_capability
                    ),
                    c6_decision_authorized=(
                        authority_capability is self._c6_decision_commit_capability
                    ),
                    cognitive_context_authorized=(
                        authority_capability
                        is self._cognitive_context_commit_capability
                    ),
                )
                self._assert_c6_scope_deactivation_is_closed_locked(events=events)

                state = self._state()
                parent = state.head_event_digest
                envelopes: list[EventEnvelopeV2] = []
                for ordinal, spec in enumerate(events):
                    envelope = EventEnvelopeV2(
                        event_id=spec.event_id,
                        run_id=self.run_id,
                        command_id=command_id,
                        ordinal=ordinal,
                        kind=spec.kind,
                        actor=spec.actor,
                        occurred_at_ns=spec.occurred_at_ns,
                        payload=spec.payload,
                        parent_event_digest=parent,
                    )
                    envelopes.append(envelope)
                    parent = envelope.digest

                first_seq = state.head_seq + 1
                last_seq = state.head_seq + len(envelopes)
                outbox_rows = [
                    {
                        "ordinal": ordinal,
                        "outbox_id": item.outbox_id,
                        "payload_digest": canonical_digest(item.payload),
                        "topic": item.topic,
                    }
                    for ordinal, item in enumerate(outbox)
                ]
                next_state = state
                for offset, envelope in enumerate(envelopes):
                    next_state = apply_event(
                        next_state, envelope, seq=first_seq + offset
                    )
                next_state = replace(next_state, command_count=state.command_count + 1)
                receipt = CanonicalReceipt(
                    receipt_id=f"receipt:{command_id}",
                    run_id=self.run_id,
                    command_id=command_id,
                    kind="COMMAND_COMMITTED",
                    payload={
                        "command_payload_digest": payload_digest,
                        "event_digests": [event.digest for event in envelopes],
                        "first_seq": first_seq,
                        "last_seq": last_seq,
                        "outbox": outbox_rows,
                        "projection_mutation_digest": canonical_digest(
                            [
                                {"kind": mutation.kind, "payload": mutation.payload}
                                for mutation in projection_mutations
                            ]
                        ),
                        "state_checksum": next_state.checksum,
                    },
                )
                receipt_record = {
                    "canonical_receipt": receipt.canonical_body(),
                    "first_seq": first_seq,
                    "last_seq": last_seq,
                    "receipt_digest": receipt.digest,
                    "state_checksum": next_state.checksum,
                }
                # C6 never reconstructs omitted event/mutation bodies from hashes.
                # Seal the complete command boundary before the index row becomes
                # visible; a rollback may leave an unreachable CAS object, never a
                # falsely resolved receipt.
                from muteki.epistemic.cas import ReceiptCAS
                from muteki.epistemic.receipt_objects import (
                    CommandReceiptObjectV1,
                    ReceiptOutboxObjectV1,
                    ReceiptProjectionMutationV1,
                )

                receipt_object = CommandReceiptObjectV1(
                    receipt=receipt,
                    command_payload=json.loads(
                        canonical_json_bytes(command_payload).decode()
                    ),
                    events=tuple(envelopes),
                    outbox=tuple(
                        ReceiptOutboxObjectV1(
                            ordinal=ordinal,
                            outbox_id=item.outbox_id,
                            topic=item.topic,
                            payload=json.loads(
                                canonical_json_bytes(item.payload).decode()
                            ),
                            payload_digest=canonical_digest(item.payload),
                        )
                        for ordinal, item in enumerate(outbox)
                    ),
                    projection_mutations=tuple(
                        ReceiptProjectionMutationV1(
                            kind=item.kind,
                            payload=json.loads(
                                canonical_json_bytes(item.payload).decode()
                            ),
                        )
                        for item in projection_mutations
                    ),
                    committed_at_ns=committed_at_ns,
                )
                sealed_receipt_object = receipt_object.seal(
                    ReceiptCAS(self.path.parent / "receipt-objects-cas")
                )
                self._conn.execute(
                    "INSERT INTO commands(command_id,run_id,idempotency_key,payload_digest,"
                    "event_count,first_seq,last_seq,event_set_digest,outbox_set_digest,"
                    "receipt_json,receipt_digest,committed_at_ns) VALUES(?,?,?,?,?,?,?,?,?,?,?,?)",
                    (
                        command_id,
                        self.run_id,
                        idempotency_key,
                        payload_digest,
                        len(envelopes),
                        first_seq,
                        last_seq,
                        canonical_digest([e.digest for e in envelopes]),
                        canonical_digest(outbox_rows),
                        canonical_json_bytes(receipt_record).decode(),
                        receipt.digest,
                        int(committed_at_ns),
                    ),
                )
                for envelope in envelopes:
                    self._conn.execute(
                        "INSERT INTO events(event_id,run_id,command_id,ordinal,kind,actor,"
                        "occurred_at_ns,payload_json,parent_event_digest,event_digest) "
                        "VALUES(?,?,?,?,?,?,?,?,?,?)",
                        (
                            envelope.event_id,
                            envelope.run_id,
                            envelope.command_id,
                            envelope.ordinal,
                            envelope.kind,
                            envelope.actor,
                            envelope.occurred_at_ns,
                            canonical_json_bytes(envelope.payload).decode(),
                            envelope.parent_event_digest,
                            envelope.digest,
                        ),
                    )
                self._conn.execute(
                    "INSERT INTO command_receipt_objects("
                    "receipt_digest,command_id,first_seq,last_seq,object_digest,"
                    "byte_count,state,diagnostic_receipt_digest) "
                    "VALUES(?,?,?,?,?,?,'resolved','')",
                    (
                        receipt.digest,
                        command_id,
                        first_seq,
                        last_seq,
                        sealed_receipt_object.digest,
                        sealed_receipt_object.byte_count,
                    ),
                )
                if fault_hook:
                    fault_hook("after_events")
                for ordinal, item in enumerate(outbox):
                    self._conn.execute(
                        "INSERT INTO immutable_outbox(outbox_id,command_id,ordinal,topic,"
                        "payload_json,payload_digest) VALUES(?,?,?,?,?,?)",
                        (
                            item.outbox_id,
                            command_id,
                            ordinal,
                            item.topic,
                            canonical_json_bytes(item.payload).decode(),
                            canonical_digest(item.payload),
                        ),
                    )
                for mutation in projection_mutations:
                    self._apply_projection_mutation(mutation)
                self._conn.execute(
                    "UPDATE state_projection SET head_seq=?,state_json=?,checksum=? "
                    "WHERE singleton=1",
                    (
                        next_state.head_seq,
                        canonical_json_bytes(next_state.as_dict()).decode(),
                        next_state.checksum,
                    ),
                )
                if fault_hook:
                    fault_hook("before_commit")
                if fenced:
                    self._conn.execute(f"RELEASE SAVEPOINT {savepoint}")
                else:
                    self._conn.commit()
                return CommandCommitResult(
                    command_id=command_id,
                    receipt_digest=receipt.digest,
                    first_seq=first_seq,
                    last_seq=last_seq,
                    state_checksum=next_state.checksum,
                )
            except BaseException:
                if fenced:
                    self._conn.execute(f"ROLLBACK TO SAVEPOINT {savepoint}")
                    self._conn.execute(f"RELEASE SAVEPOINT {savepoint}")
                else:
                    self._conn.rollback()
                raise

    @staticmethod
    def _json_map(raw: str) -> dict[str, int]:
        value = json.loads(raw)
        return _strict_nonnegative_int_map(value, name="stored budget dimensions")

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
            or compilation_receipt.command_id
            != f"context:packet:{packet['decision_id']}"
            or compilation_rows[0]["payload"].get("compiler_receipt_digest")
            != packet["compiler_receipt_digest"]
            or compilation_rows[0]["payload"].get("decision_receipt_digest")
            != packet["decision_receipt_digest"]
            or compilation_rows[0]["payload"].get("feature_state_digest")
            != packet["feature_state_digest"]
            or compilation_rows[0]["payload"].get("manifest_digest")
            != packet["manifest_digest"]
            or compilation_rows[0]["payload"].get("target_attempt_id")
            != p["attempt_id"]
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
                item.get("assignment_digest") == p["assignment_digest"]
                for item in decoded
            )
            != 1
        ):
            raise IntegrityError(
                "runtime-context cognitive assignment identity is not unique"
            )

        if p["schema_id"] == COGNITIVE_RUNTIME_REPRODUCTION_ASSIGNMENT_SCHEMA_ID:
            self._validate_cognitive_reproduction_source_locked(p)

    def _validate_cognitive_reproduction_source_locked(
        self,
        payload: Mapping[str, Any],
    ) -> None:
        """Recompute one pre-outcome reproduction binding from canonical history."""

        from muteki.epistemic.cas import ReceiptCAS
        from muteki.epistemic.cognitive_events_v1 import (
            COGNITIVE_EXPERIMENT_ASSIGNED,
            COGNITIVE_RUNTIME_EXECUTABLE_ASSIGNMENT_SCHEMA_ID,
            CognitiveExperimentBindingV1,
            cognitive_runtime_reproduction_assignment_payload,
        )
        from muteki.runtime.executable_experiment_v1 import (
            ExecutableExperimentBindingV1,
        )

        p = dict(payload)
        source_assignment = self._conn.execute(
            "SELECT seq,event_digest,payload_json FROM events "
            "WHERE event_digest=? AND kind=?",
            (
                p["source_assignment_event_digest"],
                COGNITIVE_EXPERIMENT_ASSIGNED,
            ),
        ).fetchone()
        source_observation = self._conn.execute(
            "SELECT seq,event_digest,payload_json FROM events "
            "WHERE event_digest=? AND kind='COGNITIVE_EXECUTION_OBSERVED'",
            (p["source_observation_event_digest"],),
        ).fetchone()
        if source_assignment is None or source_observation is None:
            raise IntegrityError("cognitive reproduction source lineage is absent")
        source_assignment_payload = json.loads(source_assignment[2])
        source_observation_payload = json.loads(source_observation[2])
        if (
            source_assignment_payload.get("schema_id")
            != COGNITIVE_RUNTIME_EXECUTABLE_ASSIGNMENT_SCHEMA_ID
            or source_observation_payload.get("assignment_event_digest")
            != source_assignment[1]
            or source_assignment[0] >= source_observation[0]
            or source_observation[0] > self._state().head_seq
            or self.receipt_digest_for_event(source_assignment[1])
            != p["source_assignment_event_receipt_digest"]
            or self.receipt_digest_for_event(source_observation[1])
            != p["source_observation_event_receipt_digest"]
        ):
            raise IntegrityError("cognitive reproduction source lineage diverged")
        prior_reproductions = [
            item
            for item in self.event_rows(kind=COGNITIVE_EXPERIMENT_ASSIGNED)
            if item["payload"].get("source_observation_event_digest")
            == p["source_observation_event_digest"]
        ]
        if len(prior_reproductions) != 1:
            raise IntegrityError("one observation may have exactly one reproducer")

        reproduction = ExecutableExperimentBindingV1.from_canonical(
            p["executable_experiment_binding_body"]
        )
        binding = CognitiveExperimentBindingV1(
            assignment_body=p["assignment_body"],
            experiment_body=p["experiment_body"],
            h5_request_body=p["h5_request_body"],
            h5_selection_plan_body=p["h5_selection_plan_body"],
            decision_prefix_digest=p["decision_prefix_digest"],
            decision_cutoff_seq=p["decision_cutoff_seq"],
            decision_head_event_digest=p["decision_head_event_digest"],
        )
        base = self._conn.execute(
            "SELECT payload_json FROM events WHERE event_id=?",
            (p["base_event_id"],),
        ).fetchone()
        assert base is not None
        expected = cognitive_runtime_reproduction_assignment_payload(
            binding=binding,
            admission_payload=json.loads(base[0]),
            executable_experiment=reproduction,
            source_assignment_event_digest=source_assignment[1],
            source_assignment_event_receipt_digest=(
                p["source_assignment_event_receipt_digest"]
            ),
            source_assignment_payload=source_assignment_payload,
            source_observation_event_digest=source_observation[1],
            source_observation_event_receipt_digest=(
                p["source_observation_event_receipt_digest"]
            ),
            source_observation_payload=source_observation_payload,
            required_reproducer_profile_digest=(
                p["required_reproducer_profile_digest"]
            ),
        )
        if canonical_json_bytes(expected) != canonical_json_bytes(p):
            raise IntegrityError("cognitive reproduction assignment is not derived")

        proof = source_observation_payload.get("host_launch_proof_body")
        claim_digest = (
            proof.get("prompt_launch_claim_event_digest")
            if isinstance(proof, dict)
            else None
        )
        source_claim = self._conn.execute(
            "SELECT payload_json FROM events WHERE event_digest=? "
            "AND kind='CONTEXT_PROMPT_LAUNCH_CLAIMED'",
            (claim_digest,),
        ).fetchone()
        if source_claim is None:
            raise IntegrityError("cognitive reproduction source launch is absent")
        source_profile_digest = json.loads(source_claim[0]).get("profile_digest")
        if source_profile_digest == p["required_reproducer_profile_digest"]:
            raise IntegrityError("cognitive reproducer profile is not distinct")

        # Outcome blindness starts with a structural rule: O2 receives the exact
        # pre-O1 decision context, not caller-authored post-O1 prose.  Attempt and
        # packet identities are intentionally fresh, but every semantic context
        # field must replay byte-equivalent to the source decision occurrence.
        source_packet = source_assignment_payload["context_packet_binding_body"]
        reproduction_packet = p["context_packet_binding_body"]
        decision_rows = self._conn.execute(
            "SELECT seq,payload_json FROM events "
            "WHERE kind='RUNTIME_CONTEXT_DECISION_REGISTERED'"
        ).fetchall()

        def decision_for(decision_id: str) -> tuple[int, dict[str, Any]]:
            matches = [
                (int(row[0]), json.loads(row[1]))
                for row in decision_rows
                if json.loads(row[1]).get("decision_id") == decision_id
            ]
            if len(matches) != 1:
                raise IntegrityError(
                    "cognitive reproduction decision lineage is absent or ambiguous"
                )
            return matches[0]

        source_decision_seq, source_context = decision_for(source_packet["decision_id"])
        reproduction_decision_seq, reproduction_context = decision_for(
            reproduction_packet["decision_id"]
        )
        inherited_fields = (
            "acceptance_boundary",
            "decision_need",
            "effect_ambiguity",
            "feature_state_digest",
            "non_negotiable_policy",
            "objective",
            "remaining_budget",
            "scope_digest",
        )
        reproduction_assignment_seq = prior_reproductions[0]["seq"]
        if (
            source_decision_seq >= source_assignment[0]
            or reproduction_decision_seq >= reproduction_assignment_seq
            or any(
                source_context.get(name) != reproduction_context.get(name)
                for name in inherited_fields
            )
            or source_context.get("context_digest")
            != reproduction_context.get("context_digest")
        ):
            raise IntegrityError(
                "cognitive reproduction context is not inherited pre-outcome"
            )

        cas = ReceiptCAS(self.path.parent / "receipt-cas")
        packet_bytes = cas.read_verified(
            p["context_packet_binding_body"]["packet_digest"]
        )
        worker_view_bytes = reproduction.spec.worker_view_bytes
        for withheld in p["withheld_source_digest_set"]:
            marker = withheld.encode("ascii")
            if marker in packet_bytes or marker in worker_view_bytes:
                raise IntegrityError(
                    "cognitive reproduction leaked source outcome data"
                )

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

    def _validate_runtime_cognitive_execution_mutation(
        self, payload: Mapping[str, Any]
    ) -> None:
        """Semantic compare-and-append for one runtime structural observation."""

        from muteki.epistemic.cognitive_events_v1 import (
            COGNITIVE_EXECUTION_OBSERVED,
            COGNITIVE_EXPERIMENT_ASSIGNED,
            COGNITIVE_RUNTIME_EXECUTABLE_ASSIGNMENT_SCHEMA_ID,
            COGNITIVE_RUNTIME_REPRODUCTION_ASSIGNMENT_SCHEMA_ID,
        )
        from muteki.runtime.cognitive_runtime_observation_v1 import (
            canonical_observation_capture_id,
            validate_runtime_cognitive_observation_payload_shape,
        )
        from muteki.runtime.executable_experiment_v1 import (
            ClassificationStatus,
            DeterministicExperimentClassificationV1,
            ExecutableExperimentBindingV1,
        )

        try:
            validate_runtime_cognitive_observation_payload_shape(payload)
        except (TypeError, ValueError) as exc:
            raise IntegrityError(
                "runtime cognitive observation payload is false"
            ) from exc
        p = dict(payload)
        assignment = self._conn.execute(
            "SELECT seq,event_id,payload_json FROM events "
            "WHERE event_digest=? AND kind=?",
            (p["assignment_event_digest"], COGNITIVE_EXPERIMENT_ASSIGNED),
        ).fetchone()
        terminal = self._conn.execute(
            "SELECT seq,event_id,kind,payload_json FROM events "
            "WHERE event_digest=? AND kind=?",
            (p["terminal_event_digest"], p["terminal_event_kind"]),
        ).fetchone()
        budget = self._conn.execute(
            "SELECT seq,event_id,kind,payload_json FROM events "
            "WHERE event_digest=? AND kind=?",
            (p["budget_event_digest"], p["budget_event_kind"]),
        ).fetchone()
        own = self._conn.execute(
            "SELECT seq,command_id FROM events WHERE kind=? AND payload_json=?",
            (
                COGNITIVE_EXECUTION_OBSERVED,
                canonical_json_bytes(p).decode(),
            ),
        ).fetchone()
        if assignment is None or terminal is None or budget is None or own is None:
            raise IntegrityError("runtime cognitive observation lineage is incomplete")
        assignment_payload = json.loads(assignment[2])
        terminal_payload = json.loads(terminal[3])
        budget_payload = json.loads(budget[3])
        assignment_schema_id = assignment_payload.get("schema_id")
        if assignment_schema_id not in {
            COGNITIVE_RUNTIME_EXECUTABLE_ASSIGNMENT_SCHEMA_ID,
            COGNITIVE_RUNTIME_REPRODUCTION_ASSIGNMENT_SCHEMA_ID,
        }:
            raise IntegrityError(
                "runtime cognitive observation requires executable assignment"
            )
        try:
            executable = ExecutableExperimentBindingV1.from_canonical(
                assignment_payload["executable_experiment_binding_body"]
            )
        except (KeyError, TypeError, ValueError) as exc:
            raise IntegrityError(
                "runtime cognitive executable binding is false"
            ) from exc
        if (
            assignment_payload.get("executable_experiment_binding_digest")
            != executable.digest
            or p["executable_experiment_binding_digest"] != executable.digest
            or p["executable_spec_digest"] != executable.spec.digest
            or canonical_digest(assignment_payload) != p["assignment_payload_digest"]
            or assignment[1]
            != f"event:{COGNITIVE_EXPERIMENT_ASSIGNED}:{p['attempt_id']}"
            or terminal[1] != p["terminal_event_id"]
            or budget[1] != p["budget_event_id"]
        ):
            raise IntegrityError(
                "runtime cognitive assignment or event identity is rebound"
            )
        if (
            canonical_digest(terminal_payload) != p["terminal_payload_digest"]
            or canonical_digest(budget_payload) != p["budget_payload_digest"]
        ):
            raise IntegrityError("runtime cognitive terminal/budget binding is false")
        for name in (
            "attempt_digest",
            "attempt_id",
            "permit_digest",
            "permit_id",
            "scope_digest",
        ):
            if p[name] != assignment_payload.get(name) or p[
                name
            ] != terminal_payload.get(name):
                raise IntegrityError("runtime cognitive identity lineage diverged")
        if (
            budget_payload.get("attempt_id") != p["attempt_id"]
            or terminal_payload.get("outcome") != p["terminal_outcome"]
            or assignment_payload.get("experiment_digest") != p["experiment_digest"]
            or assignment_payload.get("world_epoch_digest") != p["world_epoch_digest"]
            or assignment_payload.get("context_packet_binding_body", {}).get(
                "packet_digest"
            )
            != p["context_packet_digest"]
            or not (assignment[0] < terminal[0] < budget[0] < own[0])
        ):
            raise IntegrityError("runtime cognitive semantic lineage diverged")
        for event_digest, expected_receipt in (
            (p["assignment_event_digest"], p["assignment_event_receipt_digest"]),
            (p["terminal_event_digest"], p["terminal_event_receipt_digest"]),
            (p["budget_event_digest"], p["budget_event_receipt_digest"]),
        ):
            if self.receipt_digest_for_event(event_digest) != expected_receipt:
                raise IntegrityError("runtime cognitive receipt lineage diverged")

        observation_by_id = {
            item.observation_id: item for item in executable.spec.observations
        }
        expected_capture_ids = {
            canonical_observation_capture_id(
                permit_digest=p["permit_digest"],
                spec_digest=p["executable_spec_digest"],
                observation_id=item.observation_id,
            ): item
            for item in executable.spec.observations
        }
        actual_capture_rows = self._conn.execute(
            "SELECT seq,event_id,event_digest,actor,payload_json FROM events "
            "WHERE kind='CAPTURE_CHUNK_SEALED' ORDER BY seq"
        ).fetchall()
        actual_capture_rows = tuple(
            row
            for row in actual_capture_rows
            if json.loads(row[4]).get("permit_digest") == p["permit_digest"]
        )
        bound_event_digests: set[str] = set()
        bound_capture_ids: set[str] = set()
        bound_ordinals: list[int] = []
        derived_classification_bindings: list[dict[str, Any]] = []
        for binding in p["capture_bindings"]:
            observation = observation_by_id.get(binding["observation_id"])
            if observation is None:
                raise IntegrityError(
                    "runtime cognitive capture references an undeclared observation"
                )
            expected_capture_id = canonical_observation_capture_id(
                permit_digest=p["permit_digest"],
                spec_digest=p["executable_spec_digest"],
                observation_id=observation.observation_id,
            )
            capture = self._conn.execute(
                "SELECT seq,event_id,actor,payload_json FROM events "
                "WHERE event_digest=? AND kind='CAPTURE_CHUNK_SEALED'",
                (binding["capture_event_digest"],),
            ).fetchone()
            manifest = self._conn.execute(
                "SELECT seq,event_id,actor,payload_json FROM events "
                "WHERE event_digest=? AND kind='CAPTURE_MANIFEST_ADVANCED'",
                (binding["manifest_event_digest"],),
            ).fetchone()
            if capture is None or manifest is None:
                raise IntegrityError("runtime cognitive capture pointer is absent")
            capture_payload = json.loads(capture[3])
            manifest_payload = json.loads(manifest[3])
            if (
                binding["capture_id"] != expected_capture_id
                or capture[2] != "cognitive-runtime-output-port-v1"
                or manifest[2] != "cognitive-runtime-output-port-v1"
                or capture_payload != manifest_payload
                or capture_payload.get("capture_id") != expected_capture_id
                or capture_payload.get("stream") != observation.source.value
                or capture_payload.get("raw_digest") != binding["raw_digest"]
                or capture_payload.get("byte_count") != binding["byte_count"]
                or capture_payload.get("manifest_digest") != binding["manifest_digest"]
                or capture_payload.get("ordinal") != binding["ordinal"]
                or capture_payload.get("terminal") is not binding["terminal"]
                or capture[1] != binding["capture_event_id"]
                or manifest[1] != binding["manifest_event_id"]
                or manifest[0] != capture[0] + 1
                or manifest[0] >= terminal[0]
                or self.receipt_digest_for_event(binding["capture_event_digest"])
                != binding["capture_receipt_digest"]
                or self.receipt_digest_for_event(binding["manifest_event_digest"])
                != binding["manifest_receipt_digest"]
            ):
                raise IntegrityError("runtime cognitive capture binding diverged")
            bound_event_digests.add(binding["capture_event_digest"])
            bound_capture_ids.add(binding["capture_id"])
            bound_ordinals.append(binding["ordinal"])
            derived_classification_bindings.append(
                {
                    "byte_count": binding["byte_count"],
                    "capture_event_digest": binding["capture_event_digest"],
                    "manifest_digest": binding["manifest_digest"],
                    "observation_id": binding["observation_id"],
                    "raw_digest": binding["raw_digest"],
                    "source": binding["source"],
                }
            )
        undeclared = tuple(
            sorted(
                row[2]
                for row in actual_capture_rows
                if json.loads(row[4]).get("capture_id") not in expected_capture_ids
                or row[3] != "cognitive-runtime-output-port-v1"
            )
        )
        if tuple(p["undeclared_capture_event_digests"]) != undeclared:
            raise IntegrityError(
                "runtime cognitive undeclared capture inventory is false"
            )
        expected_complete = (
            not undeclared
            and len(bound_capture_ids) == len(expected_capture_ids)
            and bound_capture_ids == set(expected_capture_ids)
            and len(actual_capture_rows) == len(expected_capture_ids)
            and tuple(sorted(bound_ordinals)) == tuple(range(len(expected_capture_ids)))
            and bool(p["capture_bindings"])
            and sum(bool(item["terminal"]) for item in p["capture_bindings"]) == 1
            and max(p["capture_bindings"], key=lambda item: item["ordinal"])["terminal"]
            is True
        )
        if p["capture_inventory_complete"] is not expected_complete:
            raise IntegrityError("runtime cognitive capture completeness is false")

        classification = p["classification_body"]
        try:
            reconstructed = DeterministicExperimentClassificationV1(
                spec_digest=classification["spec_digest"],
                status=ClassificationStatus(classification["status"]),
                observed_partition_digest=classification["observed_partition_digest"],
                prospective_partition_digests=tuple(
                    classification["prospective_partition_digests"]
                ),
                prospective_predicate_digests=tuple(
                    classification["prospective_predicate_digests"]
                ),
                matched_predicate_digests=tuple(
                    classification["matched_predicate_digests"]
                ),
                observation_bindings=tuple(classification["observation_bindings"]),
                reason_codes=tuple(classification["reason_codes"]),
                classifier_version=classification["classifier_version"],
                learning_eligible=classification["learning_eligible"],
                accepted_set_change=classification["accepted_set_change"],
            )
        except (KeyError, TypeError, ValueError) as exc:
            raise IntegrityError(
                "runtime cognitive classification cannot be reconstructed"
            ) from exc
        if (
            reconstructed.digest != p["classification_digest"]
            or tuple(reconstructed.observation_bindings)
            != tuple(
                sorted(
                    derived_classification_bindings,
                    key=lambda item: item["observation_id"],
                )
            )
            or reconstructed.prospective_partition_digests
            != tuple(
                sorted(
                    {
                        item.outcome_partition_digest
                        for item in executable.spec.predicates
                    }
                )
            )
            or reconstructed.prospective_predicate_digests
            != tuple(
                sorted({item.predicate_digest for item in executable.spec.predicates})
            )
        ):
            raise IntegrityError("runtime cognitive classification is rebound")
        try:
            executable.spec.validate_classification(reconstructed)
        except (TypeError, ValueError) as exc:
            raise IntegrityError(
                "runtime cognitive matched predicates do not prove the partition"
            ) from exc
        positive = reconstructed.status is ClassificationStatus.OBSERVED
        if positive and (
            not expected_complete
            or p["terminal_event_kind"] != "WORKER_TERMINAL"
            or p["budget_event_kind"] != "BUDGET_SETTLED"
            or p["host_launch_proof_body"] is None
            or p["epistemic_classification"] != "structurally_observed_unverified"
        ):
            raise IntegrityError(
                "runtime cognitive positive classification lacks complete runtime lineage"
            )
        proof = p["host_launch_proof_body"]
        if proof is not None and (
            proof.get("assignment_event_digest") != p["assignment_event_digest"]
            or proof.get("assignment_event_receipt_digest")
            != p["assignment_event_receipt_digest"]
            or proof.get("experiment_digest") != p["experiment_digest"]
            or proof.get("executable_spec_digest") != p["executable_spec_digest"]
            or proof.get("executable_worker_view_digest")
            != executable.spec.worker_view_digest
            or proof.get("packet_digest") != p["context_packet_digest"]
            or proof.get("attempt_digest") != p["attempt_digest"]
            or proof.get("permit_digest") != p["permit_digest"]
            or proof.get("scope_digest") != p["scope_digest"]
            or proof.get("learning_eligible") is not False
            or proof.get("verification_resolved") is not False
        ):
            raise IntegrityError("runtime cognitive host proof is rebound")
        if assignment_schema_id == COGNITIVE_RUNTIME_REPRODUCTION_ASSIGNMENT_SCHEMA_ID:
            if proof is None:
                raise IntegrityError(
                    "cognitive reproduction observation requires a host launch proof"
                )
            launch_claim = self._conn.execute(
                "SELECT payload_json FROM events WHERE event_digest=? "
                "AND kind='CONTEXT_PROMPT_LAUNCH_CLAIMED'",
                (proof.get("prompt_launch_claim_event_digest"),),
            ).fetchone()
            if launch_claim is None or json.loads(launch_claim[0]).get(
                "profile_digest"
            ) != assignment_payload.get("required_reproducer_profile_digest"):
                raise IntegrityError(
                    "cognitive reproduction used a non-preregistered launch profile"
                )

        state = self._state()
        if (
            p["verified_prefix_cutoff_seq"] != state.head_seq
            or p["verified_prefix_head_event_digest"] != state.head_event_digest
        ):
            raise IntegrityError("runtime cognitive observation used a stale prefix")
        prefix = self.receipt_field_resolver(
            cutoff_seq=state.head_seq
        ).verify_complete_through(state.head_seq)
        if (
            prefix.digest != p["verified_prefix_digest"]
            or prefix.cutoff_seq != p["verified_prefix_cutoff_seq"]
            or prefix.head_event_digest != p["verified_prefix_head_event_digest"]
        ):
            raise IntegrityError("runtime cognitive observation prefix is not complete")
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
            raise IntegrityError("runtime cognitive observation is not unique")

    def _validate_reproduction_launch_lineage_locked(
        self,
        *,
        payload: Mapping[str, Any],
        launch: Mapping[str, Any],
        own_seq: int,
    ) -> None:
        """Rebind a declared/actual snapshot to the existing C6 claim chain."""

        from muteki.epistemic.cas import ReceiptCAS

        lineage = launch["canonical_lineage"]
        stage = self._conn.execute(
            "SELECT seq,payload_json FROM events WHERE event_digest=? "
            "AND kind='CONTEXT_PROMPT_STAGED'",
            (lineage["stage_event_digest"],),
        ).fetchone()
        invocation = self._conn.execute(
            "SELECT seq,payload_json FROM events WHERE event_digest=? "
            "AND kind='CONTEXT_PROMPT_INVOCATION_BOUND'",
            (lineage["invocation_event_digest"],),
        ).fetchone()
        claim = self._conn.execute(
            "SELECT seq,payload_json FROM events WHERE event_digest=? "
            "AND kind='CONTEXT_PROMPT_LAUNCH_CLAIMED'",
            (lineage["claim_event_digest"],),
        ).fetchone()
        if stage is None or invocation is None or claim is None:
            raise IntegrityError("reproduction launch canonical lineage is absent")
        stage_payload = json.loads(stage[1])
        invocation_payload = json.loads(invocation[1])
        claim_payload = json.loads(claim[1])
        if not (
            stage[0]
            == lineage["stage_seq"]
            < invocation[0]
            == lineage["invocation_seq"]
            < claim[0]
            == lineage["claim_seq"]
            < own_seq
        ):
            raise IntegrityError("reproduction launch evidence ordering is false")
        for event_digest, expected_receipt in (
            (lineage["stage_event_digest"], lineage["stage_event_receipt_digest"]),
            (
                lineage["invocation_event_digest"],
                lineage["invocation_event_receipt_digest"],
            ),
            (lineage["claim_event_digest"], lineage["claim_event_receipt_digest"]),
        ):
            if self.receipt_digest_for_event(event_digest) != expected_receipt:
                raise IntegrityError("reproduction launch receipt lineage diverged")
        if (
            stage_payload.get("permit_digest") != payload["permit_digest"]
            or invocation_payload.get("permit_digest") != payload["permit_digest"]
            or claim_payload.get("permit_digest") != payload["permit_digest"]
            or claim_payload.get("permit_id") != payload["permit_id"]
            or stage_payload.get("stage_id") != launch["stage_id"]
            or invocation_payload.get("stage_id") != launch["stage_id"]
            or claim_payload.get("stage_id") != launch["stage_id"]
            or stage_payload.get("prompt_artifact_digest")
            != launch["prompt_artifact_digest"]
            or stage_payload.get("prompt_byte_count")
            != launch["full_prompt_byte_count"]
            or invocation_payload.get("invocation_id") != launch["invocation_id"]
            or claim_payload.get("invocation_id") != launch["invocation_id"]
            or invocation_payload.get("argv_artifact_digest")
            != launch["argv_artifact_digest"]
            or invocation_payload.get("argv_byte_count") != launch["argv_byte_count"]
            or claim_payload.get("claim_id") != launch["claim_id"]
            or claim_payload.get("launch_material_digest")
            != launch["launch_material_digest"]
            or claim_payload.get("profile_digest") != launch["launch_profile_digest"]
        ):
            raise IntegrityError("reproduction launch objects were rebound")
        cas = ReceiptCAS(self.path.parent / "receipt-cas")
        try:
            raw = cas.read_verified(launch["launch_material_digest"])
            material = json.loads(raw)
        except (TypeError, ValueError, json.JSONDecodeError) as exc:
            raise IntegrityError("reproduction launch material cannot replay") from exc
        if canonical_json_bytes(material) != raw:
            raise IntegrityError("reproduction launch material is non-canonical")
        if (
            material.get("argv_artifact_digest") != launch["argv_artifact_digest"]
            or material.get("cwd_digest") != launch["cwd_digest"]
            or tuple(material.get("environment") or ()) != tuple(launch["environment"])
            or material.get("profile_digest") != launch["launch_profile_digest"]
        ):
            raise IntegrityError(
                "reproduction launch snapshot is not the claim material"
            )
        prior_terminals = self._conn.execute(
            "SELECT seq FROM events WHERE kind IN "
            "('CONTEXT_PROMPT_RELEASED','CONTEXT_PROMPT_PRELAUNCH_ABORTED',"
            "'CONTEXT_PROMPT_UNKNOWN') AND json_extract(payload_json,'$.permit_digest')=? "
            "AND seq<?",
            (payload["permit_digest"], own_seq),
        ).fetchall()
        if prior_terminals:
            raise IntegrityError(
                "reproduction evidence was backfilled after launch terminal"
            )

    def _validate_cognitive_reproduction_prelaunch_mutation(
        self, payload: Mapping[str, Any]
    ) -> None:
        from muteki.epistemic.cas import ReceiptCAS
        from muteki.runtime.cognitive_reproduction_evidence_v1 import (
            COGNITIVE_REPRODUCTION_PRELAUNCH_DECLARED,
            validate_prelaunch_declaration_payload_shape,
        )

        try:
            validate_prelaunch_declaration_payload_shape(payload)
        except (TypeError, ValueError) as exc:
            raise IntegrityError("reproduction prelaunch declaration is false") from exc
        p = dict(payload)
        own = self._conn.execute(
            "SELECT seq,event_id FROM events WHERE kind=? AND payload_json=?",
            (
                COGNITIVE_REPRODUCTION_PRELAUNCH_DECLARED,
                canonical_json_bytes(p).decode(),
            ),
        ).fetchone()
        if own is None:
            raise IntegrityError("reproduction declaration event is absent")
        if p["run_id"] != self.run_id:
            raise IntegrityError("reproduction declaration crossed runs")
        reproduction = p["reproduction_assignment"]
        assignment = self._conn.execute(
            "SELECT seq,payload_json FROM events WHERE event_digest=? "
            "AND kind='COGNITIVE_EXPERIMENT_ASSIGNED'",
            (reproduction.get("assignment_event_digest"),),
        ).fetchone()
        source = p["source"]
        source_assignment = self._conn.execute(
            "SELECT seq,payload_json FROM events WHERE event_digest=? "
            "AND kind='COGNITIVE_EXPERIMENT_ASSIGNED'",
            (source.get("assignment_event_digest"),),
        ).fetchone()
        source_observation = self._conn.execute(
            "SELECT seq,payload_json FROM events WHERE event_digest=? "
            "AND kind='COGNITIVE_EXECUTION_OBSERVED'",
            (source.get("observation_event_digest"),),
        ).fetchone()
        source_claim = self._conn.execute(
            "SELECT seq,payload_json FROM events WHERE event_digest=? "
            "AND kind='CONTEXT_PROMPT_LAUNCH_CLAIMED'",
            (source.get("launch_claim_event_digest"),),
        ).fetchone()
        if (
            assignment is None
            or source_assignment is None
            or source_observation is None
            or source_claim is None
        ):
            raise IntegrityError("reproduction declaration source lineage is absent")
        assignment_payload = json.loads(assignment[1])
        source_assignment_payload = json.loads(source_assignment[1])
        source_observation_payload = json.loads(source_observation[1])
        source_claim_payload = json.loads(source_claim[1])
        if (
            assignment_payload.get("schema_id")
            != "muteki.cognitive-experiment-assigned.runtime-context-reproduction.v1"
            or assignment_payload.get("permit_digest") != p["permit_digest"]
            or assignment_payload.get("permit_id") != p["permit_id"]
            or assignment_payload.get("scope_digest") != p["scope_digest"]
            or assignment_payload.get("world_epoch_digest") != p["world_epoch_digest"]
            or assignment_payload.get("source_assignment_event_digest")
            != source["assignment_event_digest"]
            or assignment_payload.get("source_observation_event_digest")
            != source["observation_event_digest"]
            or reproduction.get("assignment_seq") != assignment[0]
            or reproduction.get("experiment_digest")
            != assignment_payload.get("experiment_digest")
            or reproduction.get("reproduction_kernel_digest")
            != assignment_payload.get("reproduction_kernel_digest")
            or self.receipt_digest_for_event(reproduction["assignment_event_digest"])
            != reproduction["assignment_event_receipt_digest"]
            or source.get("assignment_seq") != source_assignment[0]
            or source.get("observation_seq") != source_observation[0]
            or source_observation_payload.get("assignment_event_digest")
            != source["assignment_event_digest"]
            or source_claim_payload.get("launch_material_digest")
            != source.get("launch_material_digest")
            or source_claim_payload.get("profile_digest")
            != source.get("launch_profile_digest")
        ):
            raise IntegrityError("reproduction declaration semantic lineage diverged")
        for event_digest, expected_receipt in (
            (
                source["assignment_event_digest"],
                source["assignment_event_receipt_digest"],
            ),
            (
                source["observation_event_digest"],
                source["observation_event_receipt_digest"],
            ),
            (
                source["launch_claim_event_digest"],
                source["launch_claim_event_receipt_digest"],
            ),
        ):
            if self.receipt_digest_for_event(event_digest) != expected_receipt:
                raise IntegrityError("reproduction source receipt lineage diverged")
        fence = p["source_fence"]
        if (
            fence.get("source_assignment_event_digest")
            != source["assignment_event_digest"]
            or fence.get("cutoff_seq")
            != source_assignment_payload.get("decision_cutoff_seq")
            or fence.get("prefix_digest")
            != source_assignment_payload.get("decision_prefix_digest")
            or fence.get("prefix_head_event_digest")
            != source_assignment_payload.get("decision_head_event_digest")
            or fence.get("source_observation_seq") != source_observation[0]
        ):
            raise IntegrityError("reproduction source pre-outcome fence is false")
        try:
            source_material_raw = ReceiptCAS(
                self.path.parent / "receipt-cas"
            ).read_verified(source["launch_material_digest"])
            source_material = json.loads(source_material_raw)
        except (TypeError, ValueError, json.JSONDecodeError) as exc:
            raise IntegrityError("source launch material cannot replay") from exc
        if canonical_json_bytes(source_material) != source_material_raw or fence.get(
            "source_workspace_identity_digest"
        ) != source_material.get("cwd_digest"):
            raise IntegrityError("source workspace identity is false")
        source_names = {
            item.get("name")
            for item in source_material.get("environment", ())
            if isinstance(item, Mapping)
        }
        if source.get("home_identity_known") is (
            "HOME" not in source_names
        ) or source.get("session_identity_known") is (
            "MUTEKI_COGNITIVE_SESSION_ID" not in source_names
        ):
            raise IntegrityError("source session identity completeness is false")
        if not (
            source_assignment[0]
            < source_observation[0]
            < assignment[0]
            < p["declared_launch"]["canonical_lineage"]["stage_seq"]
            < own[0]
        ):
            raise IntegrityError("reproduction declaration event ordering is false")
        self._validate_reproduction_launch_lineage_locked(
            payload=p,
            launch=p["declared_launch"],
            own_seq=own[0],
        )
        duplicates = self._conn.execute(
            "SELECT COUNT(*) FROM events WHERE kind=? "
            "AND json_extract(payload_json,'$.permit_digest')=?",
            (COGNITIVE_REPRODUCTION_PRELAUNCH_DECLARED, p["permit_digest"]),
        ).fetchone()
        if duplicates is None or duplicates[0] != 1:
            raise IntegrityError("reproduction declaration is not unique")

    def _validate_cognitive_reproduction_launch_witness_mutation(
        self, payload: Mapping[str, Any]
    ) -> None:
        from muteki.runtime.cognitive_reproduction_evidence_v1 import (
            COGNITIVE_REPRODUCTION_LAUNCH_WITNESSED,
            COGNITIVE_REPRODUCTION_PRELAUNCH_DECLARED,
            reconstruct_reproduction_witness,
            validate_launch_witness_payload_shape,
            validate_prelaunch_declaration_payload_shape,
        )
        from muteki.runtime.cognitive_reproduction_witness_v1 import (
            ReproductionWitnessStatusV1,
            assess_cognitive_reproduction_witness,
        )

        try:
            validate_launch_witness_payload_shape(payload)
        except (TypeError, ValueError) as exc:
            raise IntegrityError("reproduction launch witness is false") from exc
        p = dict(payload)
        own = self._conn.execute(
            "SELECT seq FROM events WHERE kind=? AND payload_json=?",
            (
                COGNITIVE_REPRODUCTION_LAUNCH_WITNESSED,
                canonical_json_bytes(p).decode(),
            ),
        ).fetchone()
        declaration = self._conn.execute(
            "SELECT seq,payload_json FROM events WHERE event_digest=? AND kind=?",
            (
                p["declaration_event_digest"],
                COGNITIVE_REPRODUCTION_PRELAUNCH_DECLARED,
            ),
        ).fetchone()
        if own is None or declaration is None:
            raise IntegrityError(
                "reproduction declaration/witness occurrence is absent"
            )
        declared = json.loads(declaration[1])
        try:
            validate_prelaunch_declaration_payload_shape(declared)
        except (TypeError, ValueError) as exc:
            raise IntegrityError("witness predecessor declaration is false") from exc
        if (
            p["run_id"] != self.run_id
            or p["run_id"] != declared["run_id"]
            or p["permit_digest"] != declared["permit_digest"]
            or p["permit_id"] != declared["permit_id"]
            or p["scope_digest"] != declared["scope_digest"]
            or p["world_epoch_digest"] != declared["world_epoch_digest"]
            or canonical_digest(declared) != p["declaration_payload_digest"]
            or self.receipt_digest_for_event(p["declaration_event_digest"])
            != p["declaration_event_receipt_digest"]
            or declaration[0] >= own[0]
        ):
            raise IntegrityError("reproduction witness predecessor was rebound")
        self._validate_reproduction_launch_lineage_locked(
            payload=p,
            launch=p["actual_launch"],
            own_seq=own[0],
        )
        try:
            witness = reconstruct_reproduction_witness(
                source_fence_body=declared["source_fence"],
                declared_launch=declared["declared_launch"],
                actual_launch=p["actual_launch"],
            )
            assessment = assess_cognitive_reproduction_witness(witness)
        except (KeyError, TypeError, ValueError) as exc:
            raise IntegrityError(
                "canonical reproduction witness cannot replay"
            ) from exc
        reasons = [f"witness:{reason.value}" for reason in assessment.reason_codes]
        source = declared["source"]
        if source["home_identity_known"] is not True:
            reasons.append("source_home_identity_unknown")
        if source["session_identity_known"] is not True:
            reasons.append("source_session_identity_unknown")
        if p["actual_launch"]["home_identity_present"] is not True:
            reasons.append("reproduction_home_identity_missing")
        if p["actual_launch"]["session_identity_present"] is not True:
            reasons.append("reproduction_session_identity_missing")
        if p["actual_launch"]["input_snapshot"]["complete"] is not True:
            reasons.append("workspace_snapshot_incomplete")
        if p["actual_launch"]["input_channel_containment"] != "sealed_containment":
            reasons.append("external_input_channel_containment_unproven")
        if canonical_digest(p["actual_launch"]) != declared["declared_launch_digest"]:
            reasons.append("declared_actual_launch_material_changed")
        reason_codes = tuple(dict.fromkeys(reasons))
        expected_status = (
            "preregistered_exact_shadow"
            if not reason_codes
            and assessment.status is ReproductionWitnessStatusV1.OUTCOME_BLIND
            else "held_unknown"
        )
        if (
            witness.canonical_body() != p["witness_body"]
            or witness.digest != p["witness_digest"]
            or assessment.canonical_body() != p["witness_assessment"]
            or assessment.digest != p["witness_assessment_digest"]
            or reason_codes != tuple(p["policy_reason_codes"])
            or expected_status != p["evidence_status"]
        ):
            raise IntegrityError("reproduction witness or assessment was fabricated")
        duplicates = self._conn.execute(
            "SELECT COUNT(*) FROM events WHERE kind=? "
            "AND json_extract(payload_json,'$.permit_digest')=?",
            (COGNITIVE_REPRODUCTION_LAUNCH_WITNESSED, p["permit_digest"]),
        ).fetchone()
        if duplicates is None or duplicates[0] != 1:
            raise IntegrityError("reproduction launch witness is not unique")

    def _runtime_evaluation_binding_from_sidecar(
        self, sidecar: Mapping[str, Any]
    ) -> Any:
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
            or binding.assignment_binding_digest
            != sidecar.get("assignment_binding_digest")
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
                _C6_EVAL_V2_COMMON_FIELDS
                | {"runtime_binding", "root_budget_reservation"},
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
                if json.loads(row[0]).get("attempt_role_binding_digest")
                == binding_digest
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
            or dict(binding.role_budget)
            != dict(base_payload.get("requested_budget") or {})
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
            if (
                body.get("assignment_binding_digest")
                != binding.assignment_binding_digest
            ):
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

    def _validate_c6_eval_outcome_mutation(
        self, kind: str, payload: Mapping[str, Any]
    ) -> None:
        """Validate the checker-only, projection-free terminal declaration.

        This mutation intentionally writes no projection row.  Its purpose is to
        force the outcome event through a distinct store-local capability and to
        validate its complete transitive terminal-binding inventory atomically.
        The read-only evaluator still recomputes accounting and CAS closure later.
        """

        p = dict(payload)
        contracts = {
            "c6_eval_outcome_guard": (
                "C6_EVAL_OUTCOME_VERIFIED",
                "solved",
                _C6_EVAL_OUTCOME_VERIFIED_FIELDS,
            ),
            "c6_eval_outcome_unknown_guard": (
                "C6_EVAL_OUTCOME_UNKNOWN",
                "unknown",
                _C6_EVAL_OUTCOME_UNKNOWN_FIELDS,
            ),
        }
        if kind not in contracts:
            raise IntegrityError("unknown C6 checker outcome mutation")
        event_kind, required_result, fields = contracts[kind]
        if set(p) != fields:
            raise IntegrityError("C6 checker outcome payload shape is not versioned")
        if p.get("schema_id") != _C6_EVAL_OUTCOME_SCHEMA_ID:
            raise IntegrityError("C6 checker outcome schema diverged")
        if event_kind == "C6_EVAL_OUTCOME_VERIFIED":
            if p.get("result") not in {"solved", "clean_unsolved"}:
                raise IntegrityError("verified C6 checker outcome is not terminal")
        elif p.get("result") != required_result:
            raise IntegrityError("C6 checker UNKNOWN result diverged")
        if p.get("run_id") != self.run_id:
            raise IntegrityError("C6 checker outcome run identity diverged")
        digest_fields = {
            "assignment_digest",
            "evaluation_binding_digest",
            "scope_digest",
        }
        if event_kind == "C6_EVAL_OUTCOME_VERIFIED":
            digest_fields |= {
                "artifact_manifest_digest",
                "checker_build_digest",
                "checker_input_manifest_digest",
                "checker_output_digest",
                "checker_policy_digest",
                "complete_accounting_digest",
            }
        else:
            digest_fields.add("reason_digest")
        if any(not _is_sha256(p.get(name)) for name in digest_fields):
            raise IntegrityError("C6 checker outcome contains a malformed digest")

        terminal_digests = p.get("terminal_binding_event_digests")
        if (
            type(terminal_digests) is not list
            or not terminal_digests
            or terminal_digests != sorted(terminal_digests)
            or len(terminal_digests) != len(set(terminal_digests))
            or any(not _is_sha256(item) for item in terminal_digests)
        ):
            raise IntegrityError(
                "C6 checker outcome terminal binding inventory is not canonical"
            )

        current = self._state()
        current_scope = canonical_digest(
            {
                "execution_generation": current.execution_generation,
                "run_fence_epoch": current.run_fence_epoch,
                "run_id": current.run_id,
            }
        )
        if (
            current.run_execution.value != "running"
            or p["scope_digest"] != current_scope
        ):
            raise IntegrityError("C6 checker outcome is outside the running scope")

        payload_json = canonical_json_bytes(p).decode()
        outcome_rows = self._conn.execute(
            "SELECT command_id,event_id,actor,seq FROM events "
            "WHERE kind=? AND payload_json=?",
            (event_kind, payload_json),
        ).fetchall()
        expected_command = f"C6_EVAL_OUTCOME:{p['assignment_digest']}"
        expected_event = f"event:C6_EVAL_OUTCOME:{p['assignment_digest']}"
        if len(outcome_rows) != 1 or tuple(outcome_rows[0][:3]) != (
            expected_command,
            expected_event,
            "c6-evaluation-checker-authority",
        ):
            raise IntegrityError("C6 checker outcome command identity diverged")
        outcome_seq = int(outcome_rows[0][3])
        total_outcomes = self._conn.execute(
            "SELECT COUNT(*) FROM events WHERE kind IN "
            "('C6_EVAL_OUTCOME_VERIFIED','C6_EVAL_OUTCOME_UNKNOWN')"
        ).fetchone()
        if total_outcomes is None or int(total_outcomes[0]) != 1:
            raise IntegrityError("C6 checker outcome is not unique for the run")

        terminal_rows = self._conn.execute(
            "SELECT event_digest,payload_json,seq FROM events "
            "WHERE kind='C6_EVAL_TERMINAL_BOUND' ORDER BY event_digest"
        ).fetchall()
        if [str(row[0]) for row in terminal_rows] != terminal_digests:
            raise IntegrityError(
                "C6 checker outcome does not bind the complete terminal set"
            )
        attempt_count = self._conn.execute(
            "SELECT COUNT(*) FROM events WHERE kind='ATTEMPT_ADMITTED'"
        ).fetchone()
        if attempt_count is None or int(attempt_count[0]) != len(terminal_rows):
            raise IntegrityError("C6 checker outcome covers only part of the run")
        terminal_identity: set[tuple[str, str]] = set()
        manifest_digest = self._conn.execute(
            "SELECT manifest_digest FROM run_meta WHERE singleton=1"
        ).fetchone()
        if manifest_digest is None:
            raise IntegrityError("C6 checker outcome has no immutable run manifest")
        for terminal_digest, terminal_json, terminal_seq in terminal_rows:
            terminal = json.loads(terminal_json)
            if (
                int(terminal_seq) >= outcome_seq
                or terminal.get("evaluation_binding_digest")
                != p["evaluation_binding_digest"]
                or terminal.get("scope_digest") != p["scope_digest"]
            ):
                raise IntegrityError("C6 checker terminal lineage is rebound")
            launch_row = self._conn.execute(
                "SELECT payload_json FROM events WHERE event_digest=? "
                "AND kind='C6_EVAL_LAUNCH_BOUND'",
                (terminal.get("launch_binding_event_digest"),),
            ).fetchone()
            if launch_row is None:
                raise IntegrityError("C6 checker terminal has no launch parent")
            launch = json.loads(launch_row[0])
            attempt_row = self._conn.execute(
                "SELECT payload_json FROM events WHERE event_digest=? "
                "AND kind='C6_EVAL_ATTEMPT_BOUND'",
                (launch.get("attempt_binding_event_digest"),),
            ).fetchone()
            if attempt_row is None:
                raise IntegrityError("C6 checker launch has no attempt parent")
            attempt = json.loads(attempt_row[0])
            for name in (
                "attempt_digest",
                "attempt_id",
                "evaluation_binding_digest",
                "permit_digest",
                "permit_id",
                "scope_digest",
            ):
                if terminal.get(name) != launch.get(name) or launch.get(
                    name
                ) != attempt.get(name):
                    raise IntegrityError("C6 checker transitive lineage diverged")
            binding = attempt.get("evaluation_binding")
            if (
                type(binding) is not dict
                or binding.get("assignment_digest") != p["assignment_digest"]
                or binding.get("run_manifest_digest") != str(manifest_digest[0])
                or canonical_digest(binding) != p["evaluation_binding_digest"]
            ):
                raise IntegrityError("C6 checker assignment/run binding diverged")
            terminal_identity.add(
                (str(terminal.get("attempt_id")), str(terminal.get("permit_id")))
            )
        if len(terminal_identity) != len(terminal_rows):
            raise IntegrityError("C6 checker terminal identity is reused")

        forbidden = self._conn.execute(
            "SELECT kind FROM events WHERE kind IN "
            "('BUDGET_PESSIMISTICALLY_SETTLED','BUDGET_USAGE_UNKNOWN',"
            "'EFFECT_UNKNOWN','WORKER_UNKNOWN',"
            "'FLAG_ACCEPTED','GOAL_COMPLETED','EXECUTION_STOP_REQUESTED',"
            "'EXECUTION_SCOPE_DRAINED','S4E_CLOSURE_ATTESTED') LIMIT 1"
        ).fetchone()
        if forbidden is not None:
            raise IntegrityError(
                "C6 checker outcome cannot close UNKNOWN, accepted, or drained state"
            )
        for event_kind_required in (
            "BUDGET_SETTLED",
            "EFFECT_OBSERVED",
            "WORKER_TERMINAL",
        ):
            count = self._conn.execute(
                "SELECT COUNT(*) FROM events WHERE kind=? AND seq<?",
                (event_kind_required, outcome_seq),
            ).fetchone()
            if count is None or int(count[0]) != len(terminal_rows):
                raise IntegrityError(
                    "C6 checker outcome precedes complete terminal accounting"
                )

    def _assert_c6_claims_closed_before_worker_terminal_locked(
        self, *, permit_id: object, worker_terminal_event_id: object
    ) -> None:
        """Prevent every worker-terminal path from overtaking a host launch claim."""

        if (
            type(permit_id) is not str
            or not permit_id
            or type(worker_terminal_event_id) is not str
            or not worker_terminal_event_id
        ):
            raise IntegrityError("C6 terminal guard has no exact permit identity")
        worker_terminal = self._conn.execute(
            "SELECT seq FROM events WHERE event_id=? AND kind IN "
            "('WORKER_TERMINAL','WORKER_UNKNOWN')",
            (worker_terminal_event_id,),
        ).fetchone()
        if worker_terminal is None:
            raise IntegrityError("C6 terminal guard has no canonical worker terminal")
        worker_terminal_seq = int(worker_terminal[0])
        claims = self._conn.execute(
            "SELECT payload_json FROM events WHERE kind='CONTEXT_PROMPT_LAUNCH_CLAIMED'"
        ).fetchall()
        terminals = self._conn.execute(
            "SELECT kind,seq,payload_json FROM events WHERE kind IN "
            "('CONTEXT_PROMPT_RELEASED','CONTEXT_PROMPT_UNKNOWN',"
            "'CONTEXT_PROMPT_PRELAUNCH_ABORTED')"
        ).fetchall()
        decoded_terminals = [
            (str(kind), int(seq), json.loads(raw_payload))
            for kind, seq, raw_payload in terminals
        ]
        for raw_claim in claims:
            claim = json.loads(raw_claim[0])
            if claim.get("permit_id") != permit_id:
                continue
            stage_id = claim.get("stage_id")
            if type(stage_id) is not str or not stage_id:
                raise IntegrityError("C6 host launch claim has no stage identity")
            matching = [
                (kind, seq, payload)
                for kind, seq, payload in decoded_terminals
                if payload.get("stage_id") == stage_id and seq < worker_terminal_seq
            ]
            if len(matching) != 1:
                raise IntegrityError(
                    "worker terminal cannot overtake an unresolved C6 host claim"
                )

    def _assert_c6_claims_closed_before_attempt_state_change_locked(
        self, *, attempt_id: object
    ) -> None:
        """Keep budget owner closure behind every claimed host launch.

        The local C6 interlock holds its process-local lock across the final
        durable validation and ``Popen``.  Budget settlement/UNKNOWN commands use
        the canonical store instead, so they must not be allowed to turn the same
        attempt or its reservations terminal in the small interval after that
        validation.  A prompt terminal is the durable hand-off: only RELEASED,
        PRELAUNCH_ABORTED, or UNKNOWN may precede the attempt-state transition.
        """

        if type(attempt_id) is not str or not attempt_id:
            raise IntegrityError("C6 budget guard has no exact attempt identity")
        attempt = self._conn.execute(
            "SELECT permit_id,scope_digest FROM runtime_attempts WHERE attempt_id=?",
            (attempt_id,),
        ).fetchone()
        if attempt is None:
            raise IntegrityError("C6 budget guard has no canonical attempt owner")
        permit_id, scope_digest = str(attempt[0]), str(attempt[1])
        claims = self._conn.execute(
            "SELECT payload_json FROM events WHERE kind='CONTEXT_PROMPT_LAUNCH_CLAIMED'"
        ).fetchall()
        terminals = self._conn.execute(
            "SELECT kind,payload_json FROM events WHERE kind IN "
            "('CONTEXT_PROMPT_RELEASED','CONTEXT_PROMPT_UNKNOWN',"
            "'CONTEXT_PROMPT_PRELAUNCH_ABORTED')"
        ).fetchall()
        decoded_terminals = [
            (str(kind), json.loads(raw_payload)) for kind, raw_payload in terminals
        ]
        for raw_claim in claims:
            claim = json.loads(raw_claim[0])
            if claim.get("permit_id") != permit_id:
                continue
            if (
                claim.get("attempt_id") != attempt_id
                or claim.get("scope_digest") != scope_digest
            ):
                raise IntegrityError("C6 host launch claim owner identity diverged")
            stage_id = claim.get("stage_id")
            permit_digest = claim.get("permit_digest")
            if (
                type(stage_id) is not str
                or not stage_id
                or type(permit_digest) is not str
                or not permit_digest
            ):
                raise IntegrityError("C6 host launch claim has no budget identity")
            matching = [
                (kind, payload)
                for kind, payload in decoded_terminals
                if payload.get("stage_id") == stage_id
                and payload.get("permit_digest") == permit_digest
            ]
            if len(matching) != 1:
                raise IntegrityError(
                    "attempt state change cannot overtake an unresolved C6 host claim"
                )

    def _assert_c6_scope_deactivation_is_closed_locked(
        self, *, events: Sequence[CommandEvent]
    ) -> None:
        """Keep a state transition from racing a claimed host Popen.

        The host interlock rechecks the active scope immediately before Popen, but
        a separate store connection could otherwise commit a pause/degrade/stop in
        the interval after that read.  Every event that makes the current scope
        non-active is therefore rejected while it has a claimed prompt without one
        prior prompt terminal.  Supervisor-driven shutdown revokes the interlock
        and appends ``PRELAUNCH_ABORTED``/``UNKNOWN`` first, so normal shutdown
        remains ordered and replayable.
        """

        deactivating_kinds = {
            "AUTHORITY_DEGRADED",
            "BOOT_VERIFYING",
            "INTEGRITY_DEGRADED",
            "SEARCH_PAUSED",
            "GOAL_COMPLETED",
            "EXECUTION_STOP_REQUESTED",
            "EXECUTION_SCOPE_DRAINED",
            "GOAL_INVALIDATED",
            "RUN_ARCHIVE_REQUESTED",
            "RUN_ARCHIVED",
            "S4E_CLOSURE_ATTESTED",
        }
        if not any(event.kind in deactivating_kinds for event in events):
            return
        state = self._state()
        scope_digest = canonical_digest(
            {
                "execution_generation": state.execution_generation,
                "run_fence_epoch": state.run_fence_epoch,
                "run_id": state.run_id,
            }
        )
        claims = self._conn.execute(
            "SELECT payload_json FROM events WHERE kind='CONTEXT_PROMPT_LAUNCH_CLAIMED'"
        ).fetchall()
        terminals = self._conn.execute(
            "SELECT kind,payload_json FROM events WHERE kind IN "
            "('CONTEXT_PROMPT_RELEASED','CONTEXT_PROMPT_UNKNOWN',"
            "'CONTEXT_PROMPT_PRELAUNCH_ABORTED')"
        ).fetchall()
        decoded_terminals = [
            (str(kind), json.loads(raw_payload)) for kind, raw_payload in terminals
        ]
        for raw_claim in claims:
            claim = json.loads(raw_claim[0])
            if claim.get("scope_digest") != scope_digest:
                continue
            stage_id = claim.get("stage_id")
            permit_digest = claim.get("permit_digest")
            if (
                type(stage_id) is not str
                or not stage_id
                or type(permit_digest) is not str
                or not permit_digest
            ):
                raise IntegrityError("C6 host launch claim has no scope identity")
            matching = [
                (kind, payload)
                for kind, payload in decoded_terminals
                if payload.get("stage_id") == stage_id
                and payload.get("permit_digest") == permit_digest
            ]
            if len(matching) != 1:
                raise IntegrityError(
                    "scope transition cannot overtake an unresolved C6 host claim"
                )

        cognitive_closure_kinds = {
            "GOAL_COMPLETED",
            "EXECUTION_SCOPE_DRAINED",
            "RUN_ARCHIVE_REQUESTED",
            "RUN_ARCHIVED",
            "S4E_CLOSURE_ATTESTED",
        }
        if not any(event.kind in cognitive_closure_kinds for event in events):
            return

        # A launched default-off cognitive assignment is part of the same durable worker
        # lifecycle.  Restart must reconcile it to one structural observation
        # before any scope drain/closure can make terminalization impossible.
        assignments = self._conn.execute(
            "SELECT event_digest,payload_json FROM events "
            "WHERE kind='COGNITIVE_EXPERIMENT_ASSIGNED' ORDER BY seq"
        ).fetchall()
        observations = self._conn.execute(
            "SELECT payload_json FROM events "
            "WHERE kind='COGNITIVE_EXECUTION_OBSERVED' ORDER BY seq"
        ).fetchall()
        decoded_observations = [json.loads(row[0]) for row in observations]
        launches = self._conn.execute(
            "SELECT payload_json FROM events "
            "WHERE kind='C6_EVAL_V2_LAUNCH_BOUND' ORDER BY seq"
        ).fetchall()
        launched_permits = {json.loads(row[0]).get("permit_id") for row in launches}
        for assignment_digest, raw_assignment in assignments:
            assignment = json.loads(raw_assignment)
            if assignment.get("scope_digest") != scope_digest:
                continue
            if assignment.get("permit_id") not in launched_permits:
                # No worker/effect boundary was crossed.  The assignment stays
                # visibly unresolved for offline learning, but cannot suppress
                # a real degradation or make shutdown truth impossible.
                continue
            matching_observations = [
                item
                for item in decoded_observations
                if item.get("assignment_event_digest") == assignment_digest
            ]
            if len(matching_observations) != 1:
                raise IntegrityError(
                    "scope transition cannot overtake a pending cognitive assignment"
                )

    def _apply_projection_mutation(
        self,
        mutation: ProjectionMutation,
        *,
        enforce_live_guards: bool = True,
    ) -> None:
        p = dict(mutation.payload)
        if mutation.kind == "cognitive_experiment_assign_guard":
            self._validate_cognitive_assignment_mutation(p)
            return
        if mutation.kind == "cognitive_canonical_selection_bind_guard":
            from muteki.runtime.canonical_cognitive_selection_v1 import (
                validate_canonical_selection_against_store,
            )

            validate_canonical_selection_against_store(self, p)
            return
        if mutation.kind == "cognitive_canonical_continuation_bind_guard_v2":
            from muteki.runtime.canonical_cognitive_continuation_v2 import (
                validate_canonical_continuation_against_store_v2,
            )

            validate_canonical_continuation_against_store_v2(self, p)
            return
        if mutation.kind == "cognitive_execution_observe_guard":
            self._validate_cognitive_execution_mutation(p)
            return
        if mutation.kind == "cognitive_runtime_execution_observe_guard":
            self._validate_runtime_cognitive_execution_mutation(p)
            return
        if mutation.kind == "cognitive_reproduction_prelaunch_declare_guard":
            self._validate_cognitive_reproduction_prelaunch_mutation(p)
            return
        if mutation.kind == "cognitive_reproduction_launch_witness_guard":
            self._validate_cognitive_reproduction_launch_witness_mutation(p)
            return
        if mutation.kind == "cognitive_verification_check_input_guard":
            from muteki.runtime.cognitive_verification_authority_v1 import (
                validate_cognitive_verification_check_input_against_store,
            )

            validate_cognitive_verification_check_input_against_store(self, p)
            return
        if mutation.kind == "cognitive_verification_check_output_guard":
            from muteki.runtime.cognitive_verification_authority_v1 import (
                validate_cognitive_verification_check_output_against_store,
            )

            validate_cognitive_verification_check_output_against_store(self, p)
            return
        if mutation.kind == "cognitive_verification_checked_guard":
            from muteki.runtime.cognitive_verification_authority_v1 import (
                validate_cognitive_verification_checked_against_store,
            )

            validate_cognitive_verification_checked_against_store(self, p)
            return
        if mutation.kind == "cognitive_verification_resolve_guard":
            from muteki.runtime.cognitive_verification_resolver_v1 import (
                validate_cognitive_verification_resolution_against_store,
            )

            validate_cognitive_verification_resolution_against_store(self, p)
            return
        if mutation.kind.startswith("c6_eval_v2_") and mutation.kind.endswith(
            "_bind_guard"
        ):
            self._validate_c6_eval_v2_binding_mutation(mutation.kind, p)
            return
        if mutation.kind.startswith("c6_eval_") and mutation.kind.endswith(
            "_bind_guard"
        ):
            self._validate_c6_eval_binding_mutation(mutation.kind, p)
            return
        if mutation.kind in {
            "c6_eval_outcome_guard",
            "c6_eval_outcome_unknown_guard",
        }:
            self._validate_c6_eval_outcome_mutation(mutation.kind, p)
            return
        if mutation.kind == "branch_create":
            self._conn.execute(
                "INSERT INTO runtime_branches(branch_id,state,depends_on_json,max_attempts) "
                "VALUES(?,?,?,?)",
                (
                    p["branch_id"],
                    "open",
                    canonical_json_bytes(p.get("depends_on", [])).decode(),
                    int(p["max_attempts"]),
                ),
            )
            return
        if mutation.kind == "execution_start_guard":
            current = self._state()
            if enforce_live_guards and current.run_execution.value not in {
                "new",
                "stopped",
            }:
                raise IntegrityError("execution start owner is not quiescent")
            owners = self.lifecycle_owner_summary()
            if enforce_live_guards and any(owners.values()):
                raise IntegrityError("execution start has unresolved runtime owners")
            if (
                type(p.get("execution_generation")) is not int
                or type(p.get("run_fence_epoch")) is not int
                or p["execution_generation"] != current.execution_generation + 1
                or p["run_fence_epoch"] < current.run_fence_epoch + 1
            ):
                raise IntegrityError("execution start generation/fence is stale")
            return
        if mutation.kind == "execution_stop_guard":
            current = self._state()
            scope_digest = canonical_digest(
                {
                    "execution_generation": current.execution_generation,
                    "run_fence_epoch": current.run_fence_epoch,
                    "run_id": current.run_id,
                }
            )
            if enforce_live_guards and current.run_execution.value != "running":
                raise IntegrityError("execution stop owner is not running")
            if p != {"scope_digest": scope_digest}:
                raise IntegrityError("execution stop scope is stale or malformed")
            return
        if mutation.kind == "execution_drain_guard":
            current = self._state()
            scope_digest = canonical_digest(
                {
                    "execution_generation": current.execution_generation,
                    "run_fence_epoch": current.run_fence_epoch,
                    "run_id": current.run_id,
                }
            )
            if enforce_live_guards and current.run_execution.value not in {
                "quiescing",
                "reopen_required",
            }:
                raise IntegrityError("execution drain owner is not quiescing")
            if p != {"scope_digest": scope_digest}:
                raise IntegrityError("execution drain scope is stale or malformed")
            if enforce_live_guards and any(self.lifecycle_owner_summary().values()):
                raise IntegrityError("execution drain has unresolved runtime owners")
            return
        if mutation.kind == "goal_commit_guard":
            current = self._state()
            if enforce_live_guards and current.run_execution.value != "running":
                raise IntegrityError("goal completion owner is not running")
            if set(p) != {"gate_receipts"} or type(p["gate_receipts"]) not in {
                list,
                tuple,
            }:
                raise IntegrityError("goal completion gate receipts are malformed")
            bindings = tuple(p["gate_receipts"])
            if not bindings:
                raise IntegrityError("goal completion requires a gate receipt")
            normalized: list[tuple[str, str]] = []
            for item in bindings:
                if (
                    type(item) not in {list, tuple}
                    or len(item) != 2
                    or not _is_sha256(item[0])
                    or not _is_sha256(item[1])
                ):
                    raise IntegrityError("goal completion gate receipt is malformed")
                normalized.append((item[0], item[1]))
            if (
                normalized != sorted(normalized)
                or len(set(normalized)) != len(normalized)
                or len({item[0] for item in normalized}) != len(normalized)
            ):
                raise IntegrityError("goal completion gate receipts are not canonical")
            for flag_digest, receipt_digest in normalized:
                rows = self._conn.execute(
                    "SELECT e.payload_json FROM events e JOIN commands c "
                    "ON c.command_id=e.command_id "
                    "WHERE e.kind='FLAG_ACCEPTED' AND c.receipt_digest=?",
                    (receipt_digest,),
                ).fetchall()
                matching = [
                    json.loads(row[0])
                    for row in rows
                    if json.loads(row[0]).get("flag_digest") == flag_digest
                ]
                if len(matching) != 1:
                    raise IntegrityError(
                        "goal completion gate receipt does not resolve"
                    )
                accepted = matching[0]
                admissions = self._conn.execute(
                    "SELECT payload_json FROM events WHERE kind='ATTEMPT_ADMITTED'"
                ).fetchall()
                admitted_attempts = [
                    json.loads(row[0]).get("attempt_id")
                    for row in admissions
                    if json.loads(row[0]).get("attempt_digest")
                    == accepted.get("attempt_digest")
                ]
                if len(admitted_attempts) != 1:
                    raise IntegrityError("goal completion attempt does not resolve")
                progress_rows = self._conn.execute(
                    "SELECT payload_json FROM events WHERE kind='PROGRESS_RECORDED'"
                ).fetchall()
                progress_matches = [
                    json.loads(row[0])
                    for row in progress_rows
                    if (
                        json.loads(row[0]).get("kind") == "goal_unit"
                        and json.loads(row[0]).get("basis_digest") == receipt_digest
                        and json.loads(row[0]).get("goal_unit") == flag_digest
                        and json.loads(row[0]).get("attempt_id") == admitted_attempts[0]
                    )
                ]
                if len(progress_matches) != 1:
                    raise IntegrityError(
                        "goal completion lacks an exact verified progress occurrence"
                    )
            return
        if mutation.kind == "projection_verify_guard":
            current = self._state()
            scope_digest = canonical_digest(
                {
                    "execution_generation": current.execution_generation,
                    "run_fence_epoch": current.run_fence_epoch,
                    "run_id": current.run_id,
                }
            )
            if set(p) != {"after", "before", "equivalent", "scope_digest"}:
                raise IntegrityError("projection verification payload is malformed")
            current_digest = self.runtime_projection_digest()
            if (
                p["equivalent"] is not True
                or not _is_sha256(p["before"])
                or p["before"] != p["after"]
                or p["after"] != current_digest
                or p["scope_digest"] != scope_digest
            ):
                raise IntegrityError("projection verification is not equivalent")
            return
        if mutation.kind == "s4e_closure_guard":
            current = self._state()
            scope_digest = canonical_digest(
                {
                    "execution_generation": current.execution_generation,
                    "run_fence_epoch": current.run_fence_epoch,
                    "run_id": current.run_id,
                }
            )
            if set(p) != {
                "all_clean",
                "components",
                "invariants",
                "schema",
                "scope_digest",
                "solved",
            }:
                raise IntegrityError("S4-E closure payload is malformed")
            if type(p["all_clean"]) is not bool or type(p["solved"]) is not bool:
                raise IntegrityError("S4-E closure booleans are malformed")
            if p["scope_digest"] != scope_digest:
                raise IntegrityError("S4-E closure scope is stale")
            required_components = {
                "canonical_permit",
                "capture_manifest",
                "gate_input",
                "orphan_summary",
                "s4e_schema",
                "usage_closure",
            }
            components = p["components"]
            if (
                type(components) is not dict
                or set(components) != required_components
                or any(not _is_sha256(value) for value in components.values())
            ):
                raise IntegrityError("S4-E closure components are malformed")
            invariants = p["invariants"]
            required_invariants = {
                "capture_pairs",
                "effects_close",
                "gate_closes",
                "orphan_free",
                "usage_closes",
            }
            if (
                type(invariants) is not dict
                or set(invariants) != required_invariants
                or any(type(value) is not bool for value in invariants.values())
                or p["all_clean"] is not all(invariants.values())
                or (p["solved"] and not p["all_clean"])
            ):
                raise IntegrityError("S4-E closure invariants are inconsistent")
            schema = p["schema"]
            if (
                type(schema) is not dict
                or schema.get("name") != "muteki-s4e-closure"
                or schema.get("version") != 1
                or tuple(schema.get("required_components") or ())
                != (
                    "canonical_permit",
                    "capture_manifest",
                    "gate_input",
                    "orphan_summary",
                    "usage_closure",
                )
                or components["s4e_schema"] != canonical_digest(schema)
            ):
                raise IntegrityError("S4-E closure schema is malformed")
            return
        if mutation.kind == "canary_commit_guard":
            if set(p) != {"canary_digest", "level", "receipt_chain", "run_id"}:
                raise IntegrityError("canary admission payload is malformed")
            chain = p["receipt_chain"]
            if (
                not _is_sha256(p["canary_digest"])
                or p["level"] != "live_local"
                or type(p["run_id"]) is not str
                or not p["run_id"]
                or type(chain) is not dict
                or not chain
                or any(
                    type(name) is not str
                    or not name
                    or name != name.strip()
                    or not _is_sha256(digest)
                    for name, digest in chain.items()
                )
            ):
                raise IntegrityError("canary admission contract is malformed")
            run = self._conn.execute(
                "SELECT state FROM catalog_runs WHERE run_id=?", (p["run_id"],)
            ).fetchone()
            if enforce_live_guards and (run is None or run[0] != "sealed"):
                raise IntegrityError("canary admission has no sealed catalog run")
            return
        if mutation.kind == "branch_state":
            cur = self._conn.execute(
                "UPDATE runtime_branches SET state=? WHERE branch_id=? AND state=?",
                (p["new_state"], p["branch_id"], p["expected_state"]),
            )
            if cur.rowcount != 1:
                raise IntegrityError("branch state compare-and-set failed")
            return
        if mutation.kind == "budget_account_create":
            limits = _strict_nonnegative_int_map(p["limits"], name="budget limits")
            zeros = {key: 0 for key in limits}
            self._conn.execute(
                "INSERT INTO budget_accounts(account_id,parent_id,limits_json,settled_json,held_json) "
                "VALUES(?,?,?,?,?)",
                (
                    p["account_id"],
                    p.get("parent_id") or None,
                    canonical_json_bytes(limits).decode(),
                    canonical_json_bytes(zeros).decode(),
                    canonical_json_bytes(zeros).decode(),
                ),
            )
            return
        if mutation.kind == "attempt_admit":
            current_state = self._state()
            current_scope_digest = canonical_digest(
                {
                    "execution_generation": current_state.execution_generation,
                    "run_fence_epoch": current_state.run_fence_epoch,
                    "run_id": current_state.run_id,
                }
            )
            if enforce_live_guards and (
                current_state.kernel_health.value != "ready"
                or current_state.run_execution.value != "running"
                or current_state.search_mode.value != "active"
                or p.get("scope_digest") != current_scope_digest
            ):
                raise IntegrityError(
                    "attempt admission is outside the current active execution scope"
                )
            if self._conn.execute(
                "SELECT 1 FROM runtime_attempts WHERE fingerprint=?",
                (p["fingerprint"],),
            ).fetchone():
                raise IntegrityError("attempt fingerprint is already covered")
            branch = self._conn.execute(
                "SELECT state,depends_on_json,max_attempts,attempt_count FROM runtime_branches "
                "WHERE branch_id=?",
                (p["branch_id"],),
            ).fetchone()
            if (
                branch is None
                or branch[0] != "open"
                or int(branch[3]) >= int(branch[2])
            ):
                raise IntegrityError(
                    "branch is closed/suspended or attempt bound is exhausted"
                )
            for dependency in json.loads(branch[1]):
                row = self._conn.execute(
                    "SELECT state FROM runtime_branches WHERE branch_id=?",
                    (dependency,),
                ).fetchone()
                if row is None or row[0] != "resolved":
                    raise IntegrityError("attempt dependency is not resolved")
            for conflict_key in p.get("conflict_keys", []):
                if self._conn.execute(
                    "SELECT 1 FROM effect_conflict_holds WHERE conflict_key=?",
                    (conflict_key,),
                ).fetchone():
                    raise IntegrityError("effect conflict is already held")

            requested = _strict_nonnegative_int_map(
                p["requested_budget"], name="requested budget"
            )
            account_id = str(p["account_id"])
            ancestry: list[tuple[str, dict[str, int]]] = []
            seen: set[str] = set()
            current = account_id
            while current:
                if current in seen:
                    raise IntegrityError("budget ancestry cycle")
                seen.add(current)
                row = self._conn.execute(
                    "SELECT parent_id,limits_json,settled_json,held_json,debt "
                    "FROM budget_accounts WHERE account_id=?",
                    (current,),
                ).fetchone()
                if row is None or int(row[4]):
                    raise IntegrityError("budget account missing or in debt")
                limits = self._json_map(row[1])
                settled = self._json_map(row[2])
                held = self._json_map(row[3])
                if set(requested) != set(limits):
                    raise IntegrityError(
                        "requested budget must cover every enforced dimension"
                    )
                if set(settled) != set(limits) or set(held) != set(limits):
                    raise IntegrityError("budget account projection axes diverged")
                for dimension, amount in requested.items():
                    if (
                        settled[dimension] + held[dimension] + amount
                        > limits[dimension]
                    ):
                        raise IntegrityError(
                            "budget admission would oversell an ancestor"
                        )
                ancestry.append((current, held))
                current = str(row[0] or "")

            self._conn.execute(
                "INSERT INTO runtime_attempts(attempt_id,branch_id,permit_id,scope_digest,"
                "lease_id,lease_epoch,worker_generation,fingerprint,effect_class,state) "
                "VALUES(?,?,?,?,?,?,?,?,?,'reserved')",
                (
                    p["attempt_id"],
                    p["branch_id"],
                    p["permit_id"],
                    p["scope_digest"],
                    p["lease_id"],
                    int(p["lease_epoch"]),
                    int(p["worker_generation"]),
                    p["fingerprint"],
                    p["effect_class"],
                ),
            )
            for ancestor_id, held in ancestry:
                next_held = {key: held[key] + requested.get(key, 0) for key in held}
                self._conn.execute(
                    "UPDATE budget_accounts SET held_json=? WHERE account_id=?",
                    (canonical_json_bytes(next_held).decode(), ancestor_id),
                )
                reservation_id = f"{p['permit_id']}:{ancestor_id}"
                self._conn.execute(
                    "INSERT INTO budget_reservations(reservation_id,account_id,attempt_id,"
                    "dimensions_json,state) VALUES(?,?,?,?,'active')",
                    (
                        reservation_id,
                        ancestor_id,
                        p["attempt_id"],
                        canonical_json_bytes(requested).decode(),
                    ),
                )
            self._conn.execute(
                "UPDATE runtime_branches SET attempt_count=attempt_count+1 WHERE branch_id=?",
                (p["branch_id"],),
            )
            for conflict_key in p.get("conflict_keys", []):
                self._conn.execute(
                    "INSERT INTO effect_conflict_holds(conflict_key,operation_id,state) "
                    "VALUES(?,?,'active')",
                    (conflict_key, p["attempt_id"]),
                )
            return
        if mutation.kind == "attempt_launch":
            current_state = self._state()
            current_scope_digest = canonical_digest(
                {
                    "execution_generation": current_state.execution_generation,
                    "run_fence_epoch": current_state.run_fence_epoch,
                    "run_id": current_state.run_id,
                }
            )
            if enforce_live_guards and (
                current_state.kernel_health.value != "ready"
                or current_state.run_execution.value != "running"
                or current_state.search_mode.value != "active"
                or p.get("scope_digest") != current_scope_digest
            ):
                raise IntegrityError("launch is outside the current active scope")
            attempt_id = str(p["attempt_id"])
            attempt = self._conn.execute(
                "SELECT permit_id,scope_digest,lease_id,state "
                "FROM runtime_attempts WHERE attempt_id=?",
                (attempt_id,),
            ).fetchone()
            if attempt != (
                p["permit_id"],
                p["scope_digest"],
                p["lease_id"],
                "reserved",
            ):
                raise IntegrityError("launch permit projection compare-and-set failed")
            reservations = self._conn.execute(
                "SELECT reservation_id,state FROM budget_reservations "
                "WHERE attempt_id=?",
                (attempt_id,),
            ).fetchall()
            expected_ids = tuple(p["reservation_ids"])
            if (
                not reservations
                or len(reservations) != len(expected_ids)
                or {str(row[0]) for row in reservations} != set(expected_ids)
                or any(row[1] != "active" for row in reservations)
            ):
                raise IntegrityError("launch reservations are not exactly active")
            cur = self._conn.execute(
                "UPDATE runtime_attempts SET state='running' "
                "WHERE attempt_id=? AND state='reserved'",
                (attempt_id,),
            )
            if cur.rowcount != 1:
                raise IntegrityError("launch attempt compare-and-set failed")
            return
        if mutation.kind == "attempt_io_guard":
            action = p.get("action")
            if action not in {
                "candidate",
                "capture",
                "cognitive_capture",
                "gate",
                "c6_launch",
            }:
                raise IntegrityError("attempt I/O guard action is invalid")
            if action in {"candidate", "gate"} and (
                self._is_c6_v2_observer_attempt_locked(p.get("attempt_id"))
            ):
                raise IntegrityError(
                    "C6 evaluation v2 observer cannot reach candidate/gate authority"
                )
            current_state = self._state()
            current_scope_digest = canonical_digest(
                {
                    "execution_generation": current_state.execution_generation,
                    "run_fence_epoch": current_state.run_fence_epoch,
                    "run_id": current_state.run_id,
                }
            )
            if enforce_live_guards and (
                current_state.kernel_health.value != "ready"
                or current_state.run_execution.value != "running"
                or current_state.search_mode.value != "active"
                or p.get("scope_digest") != current_scope_digest
            ):
                raise IntegrityError("attempt I/O is outside the current active scope")
            attempt = self._conn.execute(
                "SELECT permit_id,scope_digest,lease_id,state "
                "FROM runtime_attempts WHERE attempt_id=?",
                (p["attempt_id"],),
            ).fetchone()
            if attempt != (p["permit_id"], p["scope_digest"], p["lease_id"], "running"):
                raise IntegrityError("attempt I/O owner is not active")
            reservations = self._conn.execute(
                "SELECT state FROM budget_reservations WHERE attempt_id=?",
                (p["attempt_id"],),
            ).fetchall()
            if not reservations or any(row[0] != "active" for row in reservations):
                raise IntegrityError("attempt I/O reservations are not active")
            launches = self._conn.execute(
                "SELECT event_digest,payload_json FROM events "
                "WHERE kind='WORKER_LAUNCH_PREPARED'"
            ).fetchall()
            matching_launches = [
                (str(row[0]), json.loads(row[1]))
                for row in launches
                if json.loads(row[1]).get("permit_id") == p["permit_id"]
            ]
            if len(matching_launches) != 1 or any(
                matching_launches[0][1].get(name) != p.get(name)
                for name in (
                    "attempt_digest",
                    "lease_digest",
                    "permit_digest",
                    "scope_digest",
                )
            ):
                raise IntegrityError("attempt I/O launch lineage is invalid")
            if action == "c6_launch":
                expires_at_ns = p.get("expires_at_ns")
                if type(expires_at_ns) is not int or expires_at_ns < 0:
                    raise IntegrityError("C6 host launch expiry is malformed")
                if matching_launches[0][0] != p.get("worker_launch_event_digest"):
                    raise IntegrityError(
                        "C6 host launch guard does not bind the exact worker launch"
                    )
                admissions = self._conn.execute(
                    "SELECT payload_json FROM events WHERE kind='ATTEMPT_ADMITTED'"
                ).fetchall()
                matching_admissions = [
                    json.loads(row[0])
                    for row in admissions
                    if json.loads(row[0]).get("permit_id") == p["permit_id"]
                ]
                if (
                    len(matching_admissions) != 1
                    or matching_admissions[0].get("expires_at_ns") != expires_at_ns
                    or matching_admissions[0].get("permit_digest")
                    != p.get("permit_digest")
                ):
                    raise IntegrityError(
                        "C6 host launch expiry diverges from admission"
                    )
                if enforce_live_guards and time.time_ns() >= expires_at_ns:
                    raise IntegrityError("C6 host launch permit is expired")
            terminal_rows = self._conn.execute(
                "SELECT payload_json FROM events "
                "WHERE kind IN ('WORKER_TERMINAL','WORKER_UNKNOWN')"
            ).fetchall()
            if any(
                json.loads(row[0]).get("permit_id") == p["permit_id"]
                for row in terminal_rows
            ):
                raise IntegrityError("attempt I/O occurs after worker terminal")
            if action in {"capture", "cognitive_capture"}:
                manifest_rows = self._conn.execute(
                    "SELECT payload_json FROM events "
                    "WHERE kind='CAPTURE_MANIFEST_ADVANCED' ORDER BY seq"
                ).fetchall()
                manifests = [
                    json.loads(row[0])
                    for row in manifest_rows
                    if json.loads(row[0]).get("permit_digest") == p["permit_digest"]
                ]
                previous = ""
                for ordinal, manifest in enumerate(manifests):
                    if (
                        manifest.get("ordinal") != ordinal
                        or manifest.get("previous_manifest_digest") != previous
                    ):
                        raise IntegrityError("capture manifest chain is discontinuous")
                    try:
                        body = {
                            name: manifest[name]
                            for name in (
                                "attempt_digest",
                                "byte_count",
                                "candidate_id",
                                "capture_id",
                                "flag_digest",
                                "flag_format_digest",
                                "lease_digest",
                                "ordinal",
                                "permit_digest",
                                "policy_digest",
                                "previous_manifest_digest",
                                "raw_digest",
                                "stream",
                                "terminal",
                            )
                        }
                    except KeyError as exc:
                        raise IntegrityError("capture manifest is incomplete") from exc
                    previous = canonical_digest(body)
                    if manifest.get("manifest_digest") != previous:
                        raise IntegrityError("capture manifest digest mismatch")
                if not manifests or manifests[-1].get("manifest_digest") != p.get(
                    "manifest_digest"
                ):
                    raise IntegrityError("capture append was not the manifest head")
            elif action == "gate":
                capture = self._conn.execute(
                    "SELECT payload_json FROM events WHERE event_digest=? "
                    "AND kind='CAPTURE_CHUNK_SEALED'",
                    (p["capture_event_digest"],),
                ).fetchone()
                if capture is None:
                    raise IntegrityError("gate capture event is missing")
                capture_payload = json.loads(capture[0])
                if any(
                    capture_payload.get(name) != p.get(name)
                    for name in (
                        "attempt_digest",
                        "candidate_id",
                        "flag_digest",
                        "flag_format_digest",
                        "lease_digest",
                        "manifest_digest",
                        "permit_digest",
                        "policy_digest",
                        "raw_digest",
                    )
                ):
                    raise IntegrityError("gate capture lineage is invalid")
            return
        if mutation.kind == "orphan_reconcile_guard":
            attempt = self._conn.execute(
                "SELECT permit_id,scope_digest,lease_id,state "
                "FROM runtime_attempts WHERE attempt_id=?",
                (p["attempt_id"],),
            ).fetchone()
            if attempt != (p["permit_id"], p["scope_digest"], p["lease_id"], "running"):
                raise IntegrityError("orphan owner is no longer in flight")
            launch = self._conn.execute(
                "SELECT event_digest,payload_json FROM events "
                "WHERE kind='WORKER_LAUNCH_PREPARED' AND event_digest=?",
                (p["launch_event_digest"],),
            ).fetchone()
            if launch is None:
                raise IntegrityError("orphan launch receipt is missing")
            launch_payload = json.loads(launch[1])
            if any(
                launch_payload.get(name) != p.get(name)
                for name in (
                    "attempt_digest",
                    "attempt_id",
                    "lease_digest",
                    "lease_id",
                    "permit_digest",
                    "permit_id",
                    "scope_digest",
                )
            ):
                raise IntegrityError("orphan launch lineage diverged")
            self._assert_c6_claims_closed_before_worker_terminal_locked(
                permit_id=p["permit_id"],
                worker_terminal_event_id=p["worker_unknown_event_id"],
            )
            terminals = self._conn.execute(
                "SELECT event_id,payload_json FROM events "
                "WHERE kind IN ('WORKER_TERMINAL','WORKER_UNKNOWN')"
            ).fetchall()
            matching_terminals = [
                (str(row[0]), json.loads(row[1]))
                for row in terminals
                if json.loads(row[1]).get("permit_id") == p["permit_id"]
            ]
            if (
                len(matching_terminals) != 1
                or matching_terminals[0][0] != p["worker_unknown_event_id"]
            ):
                raise IntegrityError("orphan terminal compare-and-append failed")
            budget_terminals = self._conn.execute(
                "SELECT event_id,payload_json FROM events "
                "WHERE kind IN ('BUDGET_PESSIMISTICALLY_SETTLED',"
                "'BUDGET_SETTLED','BUDGET_USAGE_UNKNOWN')"
            ).fetchall()
            matching_budget = [
                (str(row[0]), json.loads(row[1]))
                for row in budget_terminals
                if json.loads(row[1]).get("attempt_id") == p["attempt_id"]
            ]
            if (
                len(matching_budget) != 1
                or matching_budget[0][0] != p["budget_unknown_event_id"]
            ):
                raise IntegrityError("orphan budget compare-and-append failed")
            return
        if mutation.kind == "worker_terminal_guard":
            attempt = self._conn.execute(
                "SELECT permit_id,scope_digest,lease_id,state "
                "FROM runtime_attempts WHERE attempt_id=?",
                (p["attempt_id"],),
            ).fetchone()
            if (
                attempt is None
                or tuple(attempt[:3])
                != (p["permit_id"], p["scope_digest"], p["lease_id"])
                or attempt[3] not in {"running", "terminal", "unknown"}
            ):
                raise IntegrityError("worker terminal has no closed attempt owner")
            admission = self._conn.execute(
                "SELECT event_digest,payload_json FROM events "
                "WHERE kind='ATTEMPT_ADMITTED' AND event_digest=?",
                (p["admission_event_digest"],),
            ).fetchone()
            launch = self._conn.execute(
                "SELECT event_digest,payload_json FROM events "
                "WHERE kind='WORKER_LAUNCH_PREPARED' AND event_digest=?",
                (p["launch_event_digest"],),
            ).fetchone()
            if admission is None or launch is None:
                raise IntegrityError("worker terminal lineage is missing")
            admission_payload = json.loads(admission[1])
            launch_payload = json.loads(launch[1])
            for source in (admission_payload, launch_payload):
                if any(
                    source.get(name) != p.get(name)
                    for name in (
                        "attempt_digest",
                        "attempt_id",
                        "lease_digest",
                        "lease_id",
                        "permit_digest",
                        "permit_id",
                        "scope_digest",
                    )
                    if name in source
                ):
                    raise IntegrityError("worker terminal lineage diverged")
            self._assert_c6_claims_closed_before_worker_terminal_locked(
                permit_id=p["permit_id"],
                worker_terminal_event_id=p["terminal_event_id"],
            )
            terminal_rows = self._conn.execute(
                "SELECT event_id,payload_json FROM events "
                "WHERE kind IN ('WORKER_TERMINAL','WORKER_UNKNOWN')"
            ).fetchall()
            matching = [
                (str(row[0]), json.loads(row[1]))
                for row in terminal_rows
                if json.loads(row[1]).get("permit_id") == p["permit_id"]
            ]
            if len(matching) != 1 or matching[0][0] != p["terminal_event_id"]:
                raise IntegrityError("worker terminal compare-and-append failed")
            return
        if mutation.kind == "budget_settle":
            attempt_id = str(p["attempt_id"])
            require_positive_effect_revision(p["settlement_revision"])
            actual = _strict_nonnegative_int_map(p["actual_usage"], name="actual usage")
            attempt = self._conn.execute(
                "SELECT state FROM runtime_attempts WHERE attempt_id=?",
                (attempt_id,),
            ).fetchone()
            if attempt is None or attempt[0] not in {"reserved", "running"}:
                raise IntegrityError("attempt is not active for settlement")
            self._assert_c6_claims_closed_before_attempt_state_change_locked(
                attempt_id=attempt_id
            )
            rows = self._conn.execute(
                "SELECT reservation_id,account_id,dimensions_json,state FROM budget_reservations "
                "WHERE attempt_id=?",
                (attempt_id,),
            ).fetchall()
            if not rows:
                raise IntegrityError("attempt has no reservations")
            reserved_contract = self._json_map(rows[0][2])
            if any(self._json_map(row[2]) != reserved_contract for row in rows):
                raise IntegrityError("attempt reservation dimensions diverged")
            validated_charge = _validate_tagged_usage_payload(
                p,
                reserved=reserved_contract,
                reservation_ids=tuple(str(row[0]) for row in rows),
                unknown_hold=False,
            )
            if actual != validated_charge:
                raise IntegrityError("actual usage diverges from tagged usage")
            for reservation_id, account_id, dims_json, state in rows:
                if state != "active":
                    raise IntegrityError("reservation is not active")
                reserved = self._json_map(dims_json)
                if set(actual) != set(reserved):
                    raise IntegrityError(
                        "actual usage must cover the exact reserved dimensions"
                    )
                account = self._conn.execute(
                    "SELECT limits_json,settled_json,held_json FROM budget_accounts "
                    "WHERE account_id=?",
                    (account_id,),
                ).fetchone()
                if account is None:
                    raise IntegrityError("reservation budget account is missing")
                limits = self._json_map(account[0])
                settled = self._json_map(account[1])
                held = self._json_map(account[2])
                if not (set(limits) == set(settled) == set(held) == set(reserved)):
                    raise IntegrityError("budget settlement axes diverged")
                next_held = {key: held[key] - reserved[key] for key in held}
                if any(value < 0 for value in next_held.values()):
                    raise IntegrityError("budget held amount would become negative")
                next_settled = {key: settled[key] + actual[key] for key in settled}
                debt = int(any(next_settled[key] > limits[key] for key in limits))
                self._conn.execute(
                    "UPDATE budget_accounts SET settled_json=?,held_json=?,debt=? "
                    "WHERE account_id=?",
                    (
                        canonical_json_bytes(next_settled).decode(),
                        canonical_json_bytes(next_held).decode(),
                        debt,
                        account_id,
                    ),
                )
                self._conn.execute(
                    "UPDATE budget_reservations SET state='settled' WHERE reservation_id=?",
                    (reservation_id,),
                )
            cur = self._conn.execute(
                "UPDATE runtime_attempts SET state='terminal' WHERE attempt_id=? "
                "AND state IN ('reserved','running')",
                (attempt_id,),
            )
            if cur.rowcount != 1:
                raise IntegrityError("attempt settlement compare-and-set failed")
            # Admission-level conflict keys fence concurrent attempts. A terminal,
            # observed settlement releases them; UNKNOWN deliberately keeps its
            # hold in the separate budget_unknown path.
            self._conn.execute(
                "DELETE FROM effect_conflict_holds "
                "WHERE operation_id=? AND state='active'",
                (attempt_id,),
            )
            return
        if mutation.kind == "budget_pessimistic_settle":
            if set(p) != {
                "attempt_id",
                "charge_basis",
                "charged_usage",
                "reservation_ids",
                "settlement_revision",
                "usage_report",
                "usage_report_digest",
            }:
                raise IntegrityError(
                    "pessimistic settlement payload shape is not versioned"
                )
            attempt_id = str(p["attempt_id"])
            require_positive_effect_revision(p["settlement_revision"])
            if p["charge_basis"] != "unobserved_reservation_ceiling":
                raise IntegrityError("pessimistic settlement basis is false")
            charged = _strict_nonnegative_int_map(
                p["charged_usage"], name="pessimistically charged usage"
            )
            attempt = self._conn.execute(
                "SELECT state FROM runtime_attempts WHERE attempt_id=?",
                (attempt_id,),
            ).fetchone()
            if attempt is None or attempt[0] not in {"reserved", "running"}:
                raise IntegrityError("attempt is not active for pessimistic settlement")
            self._assert_c6_claims_closed_before_attempt_state_change_locked(
                attempt_id=attempt_id
            )
            rows = self._conn.execute(
                "SELECT reservation_id,account_id,dimensions_json,state "
                "FROM budget_reservations WHERE attempt_id=?",
                (attempt_id,),
            ).fetchall()
            if not rows or any(row[3] != "active" for row in rows):
                raise IntegrityError(
                    "pessimistic settlement reservations are not all active"
                )
            reserved_contract = self._json_map(rows[0][2])
            if any(self._json_map(row[2]) != reserved_contract for row in rows):
                raise IntegrityError("attempt reservation dimensions diverged")
            report = p.get("usage_report")
            measurements = (
                report.get("measurements") if isinstance(report, Mapping) else None
            )
            if (
                type(measurements) not in {list, tuple}
                or not measurements
                or any(
                    not isinstance(item, Mapping) or item.get("status") != "unknown"
                    for item in measurements
                )
            ):
                raise IntegrityError(
                    "pessimistic settlement must label every usage axis UNKNOWN"
                )
            validated_charge = _validate_tagged_usage_payload(
                p,
                reserved=reserved_contract,
                reservation_ids=tuple(str(row[0]) for row in rows),
                unknown_hold=True,
                charge_key="charged_usage",
            )
            if charged != validated_charge or charged != reserved_contract:
                raise IntegrityError(
                    "pessimistic settlement must charge the full reservation ceiling"
                )
            for reservation_id, account_id, dims_json, _state in rows:
                reserved = self._json_map(dims_json)
                account = self._conn.execute(
                    "SELECT limits_json,settled_json,held_json "
                    "FROM budget_accounts WHERE account_id=?",
                    (account_id,),
                ).fetchone()
                if account is None:
                    raise IntegrityError(
                        "pessimistic settlement budget account is missing"
                    )
                limits = self._json_map(account[0])
                settled = self._json_map(account[1])
                held = self._json_map(account[2])
                if not (set(limits) == set(settled) == set(held) == set(reserved)):
                    raise IntegrityError("pessimistic settlement budget axes diverged")
                next_held = {axis: held[axis] - reserved[axis] for axis in held}
                if any(value < 0 for value in next_held.values()):
                    raise IntegrityError("budget held amount would become negative")
                next_settled = {axis: settled[axis] + charged[axis] for axis in settled}
                debt = int(any(next_settled[axis] > limits[axis] for axis in limits))
                self._conn.execute(
                    "UPDATE budget_accounts SET settled_json=?,held_json=?,debt=? "
                    "WHERE account_id=?",
                    (
                        canonical_json_bytes(next_settled).decode(),
                        canonical_json_bytes(next_held).decode(),
                        debt,
                        account_id,
                    ),
                )
                self._conn.execute(
                    "UPDATE budget_reservations SET state='settled' "
                    "WHERE reservation_id=?",
                    (reservation_id,),
                )
            cur = self._conn.execute(
                "UPDATE runtime_attempts SET state='terminal' WHERE attempt_id=? "
                "AND state IN ('reserved','running')",
                (attempt_id,),
            )
            if cur.rowcount != 1:
                raise IntegrityError(
                    "pessimistic settlement attempt compare-and-set failed"
                )
            self._conn.execute(
                "DELETE FROM effect_conflict_holds "
                "WHERE operation_id=? AND state='active'",
                (attempt_id,),
            )
            return
        if mutation.kind == "budget_unknown":
            attempt_id = str(p["attempt_id"])
            require_positive_effect_revision(p["revision"])
            attempt = self._conn.execute(
                "SELECT state FROM runtime_attempts WHERE attempt_id=?",
                (attempt_id,),
            ).fetchone()
            if attempt is None or attempt[0] not in {"reserved", "running"}:
                raise IntegrityError("attempt is not active for UNKNOWN usage")
            self._assert_c6_claims_closed_before_attempt_state_change_locked(
                attempt_id=attempt_id
            )
            rows = self._conn.execute(
                "SELECT reservation_id,account_id,dimensions_json,state "
                "FROM budget_reservations WHERE attempt_id=?",
                (attempt_id,),
            ).fetchall()
            reservation_count = len(rows)
            active_count = sum(row[3] == "active" for row in rows)
            if reservation_count == 0 or active_count != reservation_count:
                raise IntegrityError("attempt reservations are not all active")
            reserved_contract = self._json_map(rows[0][2])
            if any(self._json_map(row[2]) != reserved_contract for row in rows):
                raise IntegrityError("attempt reservation dimensions diverged")
            held_charge = _validate_tagged_usage_payload(
                p,
                reserved=reserved_contract,
                reservation_ids=tuple(str(row[0]) for row in rows),
                unknown_hold=True,
            )
            for _reservation_id, account_id, dimensions_json, _state in rows:
                reserved = self._json_map(dimensions_json)
                account = self._conn.execute(
                    "SELECT limits_json,settled_json,held_json FROM budget_accounts "
                    "WHERE account_id=?",
                    (account_id,),
                ).fetchone()
                if account is None:
                    raise IntegrityError("UNKNOWN reservation account is missing")
                limits = self._json_map(account[0])
                settled = self._json_map(account[1])
                held = self._json_map(account[2])
                if not (set(limits) == set(settled) == set(held) == set(reserved)):
                    raise IntegrityError("UNKNOWN budget axes diverged")
                next_held = {
                    axis: held[axis] + held_charge[axis] - reserved[axis]
                    for axis in held
                }
                debt = int(
                    any(
                        settled[axis] + next_held[axis] > limits[axis]
                        for axis in limits
                    )
                )
                self._conn.execute(
                    "UPDATE budget_accounts SET held_json=?,debt=? WHERE account_id=?",
                    (canonical_json_bytes(next_held).decode(), debt, account_id),
                )
            self._conn.execute(
                "UPDATE budget_reservations SET dimensions_json=? "
                "WHERE attempt_id=? AND state='active'",
                (canonical_json_bytes(held_charge).decode(), attempt_id),
            )
            cur = self._conn.execute(
                "UPDATE budget_reservations SET state='unknown' "
                "WHERE attempt_id=? AND state='active'",
                (attempt_id,),
            )
            if cur.rowcount != reservation_count:
                raise IntegrityError("UNKNOWN reservation compare-and-set failed")
            cur = self._conn.execute(
                "UPDATE runtime_attempts SET state='unknown' WHERE attempt_id=? "
                "AND state IN ('reserved','running')",
                (attempt_id,),
            )
            if cur.rowcount != 1:
                raise IntegrityError("UNKNOWN attempt compare-and-set failed")
            return
        if mutation.kind == "effect_prepare":
            attempt = self._conn.execute(
                "SELECT effect_class,state FROM runtime_attempts WHERE attempt_id=?",
                (p["attempt_id"],),
            ).fetchone()
            if attempt is None or attempt[1] != "running":
                raise IntegrityError("effect requires a running admitted attempt")
            if attempt[0] != p["effect_class"]:
                raise IntegrityError("effect class does not match admitted permit")
            keys = tuple(str(key) for key in p.get("conflict_keys", []))
            admission_rows = self._conn.execute(
                "SELECT payload_json FROM events WHERE kind='ATTEMPT_ADMITTED'"
            ).fetchall()
            admissions = [
                json.loads(row[0])
                for row in admission_rows
                if json.loads(row[0]).get("attempt_id") == p["attempt_id"]
            ]
            if (
                len(admissions) != 1
                or set(admissions[0].get("conflict_keys", ())) != set(keys)
                or len(keys) != len(set(keys))
            ):
                raise IntegrityError(
                    "effect conflict keys do not match the admitted permit"
                )
            for key in keys:
                row = self._conn.execute(
                    "SELECT operation_id FROM effect_conflict_holds WHERE conflict_key=?",
                    (key,),
                ).fetchone()
                if row is not None and row[0] != p["attempt_id"]:
                    raise IntegrityError("effect conflict is held by another operation")
                self._conn.execute(
                    "INSERT INTO effect_conflict_holds(conflict_key,operation_id,state) "
                    "VALUES(?,?,'active') ON CONFLICT(conflict_key) DO UPDATE SET "
                    "operation_id=excluded.operation_id,state='active'",
                    (key, p["operation_id"]),
                )
            self._conn.execute(
                "INSERT INTO effect_operations(operation_id,attempt_id,effect_class,"
                "conflict_keys_json,state,current_ordinal) VALUES(?,?,?,?,'prepared',1)",
                (
                    p["operation_id"],
                    p["attempt_id"],
                    p["effect_class"],
                    canonical_json_bytes(keys).decode(),
                ),
            )
            self._conn.execute(
                "INSERT INTO effect_attempts(operation_id,ordinal,state) VALUES(?,1,'prepared')",
                (p["operation_id"],),
            )
            return
        if mutation.kind == "effect_transition":
            require_positive_effect_revision(p["revision"])
            operation_id = str(p["operation_id"])
            new_state = str(p["new_state"])
            row = self._conn.execute(
                "SELECT state,current_ordinal,conflict_keys_json,attempt_id "
                "FROM effect_operations "
                "WHERE operation_id=?",
                (operation_id,),
            ).fetchone()
            if row is None:
                raise IntegrityError("effect transition compare-and-set failed")
            owner = self._conn.execute(
                "SELECT state FROM runtime_attempts WHERE attempt_id=?",
                (row[3],),
            ).fetchone()
            if enforce_live_guards and (owner is None or owner[0] != "running"):
                raise IntegrityError(
                    "effect transition requires a running attempt owner"
                )
            if enforce_live_guards and row[0] == "unknown":
                raise IntegrityError(
                    "UNKNOWN effect requires an independent observer receipt"
                )
            # Enforce the FSM from the actual projected state, independent of CAS.
            if new_state not in EFFECT_LEGAL_TRANSITIONS.get(str(row[0]), frozenset()):
                raise IntegrityError(
                    f"illegal effect transition {row[0]!r} -> {new_state!r}"
                )
            if row[0] != p["expected_state"]:
                raise IntegrityError("effect transition compare-and-set failed")
            ordinal = int(row[1])
            self._conn.execute(
                "UPDATE effect_operations SET state=? WHERE operation_id=?",
                (new_state, operation_id),
            )
            self._conn.execute(
                "UPDATE effect_attempts SET state=? WHERE operation_id=? AND ordinal=?",
                (new_state, operation_id, ordinal),
            )
            keys = json.loads(row[2])
            if new_state == "unknown":
                for key in keys:
                    self._conn.execute(
                        "UPDATE effect_conflict_holds SET state='unknown' "
                        "WHERE conflict_key=? AND operation_id=?",
                        (key, operation_id),
                    )
            elif new_state in {"observed", "confirmed_not_applied"}:
                self._conn.execute(
                    "DELETE FROM effect_conflict_holds WHERE operation_id=?",
                    (operation_id,),
                )
            return
        if mutation.kind == "effect_retry":
            require_positive_effect_revision(p["revision"])
            if enforce_live_guards:
                raise IntegrityError(
                    "effect retry requires a fresh canonical admission"
                )
            operation_id = str(p["operation_id"])
            row = self._conn.execute(
                "SELECT state,current_ordinal,conflict_keys_json FROM effect_operations "
                "WHERE operation_id=?",
                (operation_id,),
            ).fetchone()
            if row is None or row[0] != "confirmed_not_applied":
                raise IntegrityError("only confirmed-not-applied effect may retry")
            ordinal = int(row[1]) + 1
            self._conn.execute(
                "UPDATE effect_operations SET state='prepared',current_ordinal=? "
                "WHERE operation_id=?",
                (ordinal, operation_id),
            )
            self._conn.execute(
                "INSERT INTO effect_attempts(operation_id,ordinal,state) "
                "VALUES(?,?,'prepared')",
                (operation_id, ordinal),
            )
            for key in json.loads(row[2]):
                self._conn.execute(
                    "INSERT INTO effect_conflict_holds(conflict_key,operation_id,state) "
                    "VALUES(?,?,'active')",
                    (key, operation_id),
                )
            return
        if mutation.kind == "draft_create":
            self._conn.execute(
                "INSERT INTO catalog_drafts(draft_id,policy_json,state) VALUES(?,?,'open')",
                (p["draft_id"], canonical_json_bytes(p["policy"]).decode()),
            )
            return
        if mutation.kind == "draft_attachment":
            draft = self._conn.execute(
                "SELECT state FROM catalog_drafts WHERE draft_id=?", (p["draft_id"],)
            ).fetchone()
            if draft is None or draft[0] != "open":
                raise IntegrityError("attachment requires an open draft")
            self._conn.execute(
                "INSERT INTO catalog_attachments(attachment_id,draft_id,digest,byte_count) "
                "VALUES(?,?,?,?)",
                (p["attachment_id"], p["draft_id"], p["digest"], int(p["byte_count"])),
            )
            return
        if mutation.kind == "provision_begin":
            draft = self._conn.execute(
                "SELECT state FROM catalog_drafts WHERE draft_id=?", (p["draft_id"],)
            ).fetchone()
            if draft is None or draft[0] != "open":
                raise IntegrityError("provision requires an open draft")
            self._conn.execute(
                "INSERT INTO provision_operations(operation_id,draft_id,allocated_run_id,"
                "target_root,manifest_digest,owner_epoch,state) "
                "VALUES(?,?,?,?,?,?,'run_allocated')",
                (
                    p["operation_id"],
                    p["draft_id"],
                    p["run_id"],
                    p["target_root"],
                    p["manifest_digest"],
                    int(p["owner_epoch"]),
                ),
            )
            self._conn.execute(
                "INSERT INTO catalog_runs(run_id,operation_id,manifest_digest,state) "
                "VALUES(?,?,?,'allocating')",
                (p["run_id"], p["operation_id"], p["manifest_digest"]),
            )
            self._conn.execute(
                "UPDATE catalog_drafts SET state='provisioning' WHERE draft_id=?",
                (p["draft_id"],),
            )
            return
        if mutation.kind == "provision_materialized":
            cur = self._conn.execute(
                "UPDATE provision_operations SET state='run_materialized' "
                "WHERE operation_id=? AND state='run_allocated' AND owner_epoch=?",
                (p["operation_id"], int(p["owner_epoch"])),
            )
            if cur.rowcount != 1:
                raise IntegrityError("provision owner/state fence failed")
            return
        if mutation.kind == "provision_sealed":
            cur = self._conn.execute(
                "UPDATE provision_operations SET state='sealed' "
                "WHERE operation_id=? AND state='run_materialized' AND owner_epoch=?",
                (p["operation_id"], int(p["owner_epoch"])),
            )
            if cur.rowcount != 1:
                raise IntegrityError("provision seal fence failed")
            self._conn.execute(
                "UPDATE catalog_runs SET state='sealed',anchor_digest=? "
                "WHERE run_id=? AND operation_id=? AND state='allocating'",
                (p["anchor_digest"], p["run_id"], p["operation_id"]),
            )
            self._conn.execute(
                "UPDATE catalog_drafts SET state='sealed' WHERE draft_id=("
                "SELECT draft_id FROM provision_operations WHERE operation_id=?)",
                (p["operation_id"],),
            )
            return
        if mutation.kind == "provision_failed":
            self._conn.execute(
                "UPDATE provision_operations SET state='failed_seal' "
                "WHERE operation_id=? AND owner_epoch=? AND state!='sealed'",
                (p["operation_id"], int(p["owner_epoch"])),
            )
            self._conn.execute(
                "UPDATE catalog_runs SET state='failed_seal' WHERE operation_id=?",
                (p["operation_id"],),
            )
            self._conn.execute(
                "UPDATE catalog_drafts SET state='failed' WHERE draft_id=("
                "SELECT draft_id FROM provision_operations WHERE operation_id=?)",
                (p["operation_id"],),
            )
            return
        if mutation.kind == "archive_assert_settled":
            active_attempts = int(
                self._conn.execute(
                    "SELECT COUNT(*) FROM runtime_attempts WHERE state IN ('reserved','running','unknown')"
                ).fetchone()[0]
            )
            active_reservations = int(
                self._conn.execute(
                    "SELECT COUNT(*) FROM budget_reservations WHERE state IN ('active','unknown')"
                ).fetchone()[0]
            )
            active_effects = int(
                self._conn.execute(
                    "SELECT COUNT(*) FROM effect_conflict_holds"
                ).fetchone()[0]
            )
            if active_attempts or active_reservations or active_effects:
                raise IntegrityError("run has unsettled attempt/effect/budget owners")
            return
        if mutation.kind == "archive_begin":
            run = self._conn.execute(
                "SELECT state FROM catalog_runs WHERE run_id=?", (p["run_id"],)
            ).fetchone()
            if run is None or run[0] != "sealed":
                raise IntegrityError("archive requires a sealed catalog run")
            self._conn.execute(
                "INSERT INTO archive_operations(operation_id,run_id,owner_epoch,state,requested_at_ns) "
                "VALUES(?,?,?,'requested',?)",
                (
                    p["operation_id"],
                    p["run_id"],
                    int(p["owner_epoch"]),
                    int(p["requested_at_ns"]),
                ),
            )
            return
        if mutation.kind == "archive_complete":
            cur = self._conn.execute(
                "UPDATE archive_operations SET state='archived',run_receipt_digest=?,"
                "archive_receipt_digest=? WHERE operation_id=? AND run_id=? "
                "AND owner_epoch=? AND state='requested'",
                (
                    p["run_receipt_digest"],
                    p["archive_receipt_digest"],
                    p["operation_id"],
                    p["run_id"],
                    int(p["owner_epoch"]),
                ),
            )
            if cur.rowcount != 1:
                raise IntegrityError("archive owner/state fence failed")
            cur = self._conn.execute(
                "UPDATE catalog_runs SET state='archived' WHERE run_id=? AND state='sealed'",
                (p["run_id"],),
            )
            if cur.rowcount != 1:
                raise IntegrityError("catalog archive transition failed")
            return
        if mutation.kind == "purge_begin":
            run = self._conn.execute(
                "SELECT state FROM catalog_runs WHERE run_id=?", (p["run_id"],)
            ).fetchone()
            if run is None or run[0] != "archived":
                raise IntegrityError("purge requires an archived catalog run")
            items = tuple(p.get("items") or ())
            if not items:
                raise IntegrityError("purge plan must contain at least one item")
            self._conn.execute(
                "INSERT INTO purge_operations(operation_id,run_id,owner_epoch,state,"
                "plan_digest,plan_receipt_digest,requested_at_ns) "
                "VALUES(?,?,?,'purge_pending',?,?,?)",
                (
                    p["operation_id"],
                    p["run_id"],
                    int(p["owner_epoch"]),
                    p["plan_digest"],
                    p["plan_receipt_digest"],
                    int(p["requested_at_ns"]),
                ),
            )
            for ordinal, item in enumerate(items):
                self._conn.execute(
                    "INSERT INTO purge_plan_items(operation_id,ordinal,locator,adapter,state) "
                    "VALUES(?,?,?,?,'pending')",
                    (p["operation_id"], ordinal, item["locator"], item["adapter"]),
                )
            return
        if mutation.kind == "purge_item_absent":
            cur = self._conn.execute(
                "UPDATE purge_plan_items SET state='absent',action_receipt_digest=?,"
                "absence_receipt_digest=? WHERE operation_id=? AND ordinal=? "
                "AND locator=? AND adapter=? AND state IN ('pending','unknown')",
                (
                    p["action_receipt_digest"],
                    p["absence_receipt_digest"],
                    p["operation_id"],
                    int(p["ordinal"]),
                    p["locator"],
                    p["adapter"],
                ),
            )
            if cur.rowcount != 1:
                raise IntegrityError("purge plan item identity/state fence failed")
            remaining_unknown = int(
                self._conn.execute(
                    "SELECT COUNT(*) FROM purge_plan_items WHERE operation_id=? AND state='unknown'",
                    (p["operation_id"],),
                ).fetchone()[0]
            )
            if not remaining_unknown:
                self._conn.execute(
                    "UPDATE purge_operations SET state='purge_pending' "
                    "WHERE operation_id=? AND state='purge_unknown'",
                    (p["operation_id"],),
                )
            return
        if mutation.kind == "purge_item_unknown":
            cur = self._conn.execute(
                "UPDATE purge_plan_items SET state='unknown',action_receipt_digest=? "
                "WHERE operation_id=? AND ordinal=? AND locator=? AND adapter=? "
                "AND state='pending'",
                (
                    p["action_receipt_digest"],
                    p["operation_id"],
                    int(p["ordinal"]),
                    p["locator"],
                    p["adapter"],
                ),
            )
            if cur.rowcount != 1:
                raise IntegrityError("purge unknown item identity/state fence failed")
            self._conn.execute(
                "UPDATE purge_operations SET state='purge_unknown' "
                "WHERE operation_id=? AND state='purge_pending'",
                (p["operation_id"],),
            )
            return
        if mutation.kind == "purge_complete":
            operation = self._conn.execute(
                "SELECT run_id,owner_epoch,plan_digest,state FROM purge_operations "
                "WHERE operation_id=?",
                (p["operation_id"],),
            ).fetchone()
            if (
                operation is None
                or operation[0] != p["run_id"]
                or int(operation[1]) != int(p["owner_epoch"])
                or operation[2] != p["plan_digest"]
                or operation[3] != "purge_pending"
            ):
                raise IntegrityError("purge owner/plan/state fence failed")
            pending = int(
                self._conn.execute(
                    "SELECT COUNT(*) FROM purge_plan_items WHERE operation_id=? AND state!='absent'",
                    (p["operation_id"],),
                ).fetchone()[0]
            )
            if pending:
                raise IntegrityError(
                    "purge cannot complete before every absence readback"
                )
            self._conn.execute(
                "UPDATE purge_operations SET state='purged',absence_receipt_digest=? "
                "WHERE operation_id=?",
                (p["absence_receipt_digest"], p["operation_id"]),
            )
            cur = self._conn.execute(
                "UPDATE catalog_runs SET state='purged' WHERE run_id=? AND state='archived'",
                (p["run_id"],),
            )
            if cur.rowcount != 1:
                raise IntegrityError("catalog purge transition failed")
            self._conn.execute(
                "INSERT INTO catalog_tombstones(run_id,purge_operation_id,plan_digest,"
                "absence_receipt_digest,purged_at_ns) VALUES(?,?,?,?,?)",
                (
                    p["run_id"],
                    p["operation_id"],
                    p["plan_digest"],
                    p["absence_receipt_digest"],
                    int(p["purged_at_ns"]),
                ),
            )
            return
        raise ValueError(f"unsupported projection mutation: {mutation.kind}")

    def lifecycle_owner_summary(self) -> dict[str, int]:
        """Canonical operational-owner readback used by archive/purge admission."""
        with self._lock:
            attempts = int(
                self._conn.execute(
                    "SELECT COUNT(*) FROM runtime_attempts WHERE state IN ('reserved','running','unknown')"
                ).fetchone()[0]
            )
            reservations = int(
                self._conn.execute(
                    "SELECT COUNT(*) FROM budget_reservations WHERE state IN ('active','unknown')"
                ).fetchone()[0]
            )
            effects = int(
                self._conn.execute(
                    "SELECT COUNT(*) FROM effect_conflict_holds"
                ).fetchone()[0]
            )
        return {"attempts": attempts, "reservations": reservations, "effects": effects}

    def draft_attachments(self, draft_id: str) -> tuple[dict[str, Any], ...]:
        with self._lock:
            rows = self._conn.execute(
                "SELECT attachment_id,digest,byte_count FROM catalog_attachments "
                "WHERE draft_id=? ORDER BY attachment_id",
                (draft_id,),
            ).fetchall()
        return tuple(
            {"attachment_id": row[0], "digest": row[1], "byte_count": int(row[2])}
            for row in rows
        )

    def provision_status(self, operation_id: str) -> dict[str, Any]:
        with self._lock:
            row = self._conn.execute(
                "SELECT draft_id,allocated_run_id,target_root,manifest_digest,owner_epoch,state "
                "FROM provision_operations WHERE operation_id=?",
                (operation_id,),
            ).fetchone()
        if row is None:
            raise KeyError(operation_id)
        return {
            "draft_id": row[0],
            "run_id": row[1],
            "target_root": row[2],
            "manifest_digest": row[3],
            "owner_epoch": int(row[4]),
            "state": row[5],
        }

    def catalog_run(self, run_id: str) -> dict[str, Any]:
        with self._lock:
            row = self._conn.execute(
                "SELECT operation_id,manifest_digest,anchor_digest,state FROM catalog_runs "
                "WHERE run_id=?",
                (run_id,),
            ).fetchone()
        if row is None:
            raise KeyError(run_id)
        return {
            "run_id": run_id,
            "operation_id": row[0],
            "manifest_digest": row[1],
            "anchor_digest": row[2] or "",
            "state": row[3],
        }

    def archive_status(self, operation_id: str) -> dict[str, Any]:
        with self._lock:
            row = self._conn.execute(
                "SELECT run_id,owner_epoch,state,run_receipt_digest,"
                "archive_receipt_digest,requested_at_ns FROM archive_operations "
                "WHERE operation_id=?",
                (operation_id,),
            ).fetchone()
        if row is None:
            raise KeyError(operation_id)
        return {
            "operation_id": operation_id,
            "run_id": row[0],
            "owner_epoch": int(row[1]),
            "state": row[2],
            "run_receipt_digest": row[3],
            "archive_receipt_digest": row[4],
            "requested_at_ns": int(row[5]),
        }

    def purge_status(self, operation_id: str) -> dict[str, Any]:
        with self._lock:
            row = self._conn.execute(
                "SELECT run_id,owner_epoch,state,plan_digest,plan_receipt_digest,"
                "absence_receipt_digest,requested_at_ns FROM purge_operations "
                "WHERE operation_id=?",
                (operation_id,),
            ).fetchone()
            items = (
                self._conn.execute(
                    "SELECT ordinal,locator,adapter,state,action_receipt_digest,"
                    "absence_receipt_digest FROM purge_plan_items WHERE operation_id=? "
                    "ORDER BY ordinal",
                    (operation_id,),
                ).fetchall()
                if row is not None
                else ()
            )
        if row is None:
            raise KeyError(operation_id)
        return {
            "operation_id": operation_id,
            "run_id": row[0],
            "owner_epoch": int(row[1]),
            "state": row[2],
            "plan_digest": row[3],
            "plan_receipt_digest": row[4],
            "absence_receipt_digest": row[5],
            "requested_at_ns": int(row[6]),
            "items": tuple(
                {
                    "ordinal": int(item[0]),
                    "locator": item[1],
                    "adapter": item[2],
                    "state": item[3],
                    "action_receipt_digest": item[4],
                    "absence_receipt_digest": item[5],
                }
                for item in items
            ),
        }

    def event_rows(self, *, kind: str = "") -> tuple[dict[str, Any], ...]:
        with self._lock:
            if kind:
                rows = self._conn.execute(
                    "SELECT seq,event_id,kind,payload_json,event_digest FROM events "
                    "WHERE kind=? ORDER BY seq",
                    (kind,),
                ).fetchall()
            else:
                rows = self._conn.execute(
                    "SELECT seq,event_id,kind,payload_json,event_digest FROM events "
                    "ORDER BY seq"
                ).fetchall()
        return tuple(
            {
                "seq": int(row[0]),
                "event_id": row[1],
                "kind": row[2],
                "payload": json.loads(row[3]),
                "event_digest": row[4],
            }
            for row in rows
        )

    def receipt_digest_for_event(self, event_digest: str) -> str:
        """Resolve the immutable command receipt that committed one event."""
        with self._lock:
            row = self._conn.execute(
                "SELECT c.receipt_digest FROM events e "
                "JOIN commands c ON c.command_id=e.command_id "
                "WHERE e.event_digest=?",
                (event_digest,),
            ).fetchone()
        if row is None:
            raise KeyError(event_digest)
        return str(row[0])

    def actor_for_event(self, event_digest: str) -> str:
        """Resolve the immutable actor bound into one canonical event envelope."""

        with self._lock:
            row = self._conn.execute(
                "SELECT actor FROM events WHERE event_digest=?",
                (event_digest,),
            ).fetchone()
        if row is None:
            raise KeyError(event_digest)
        return str(row[0])

    def resolve_receipt(self, receipt_digest: str) -> CanonicalReceipt:
        """Resolve and independently validate a fully persisted command receipt.

        Protocol 2 foundations written before the complete-receipt cutover remain
        replayable, but they deliberately fail this stronger C6 resolution port.
        Callers must never synthesize a complete object from the legacy summary.
        """

        if not _is_sha256(receipt_digest):
            raise IntegrityError("receipt digest must be lowercase sha256")
        with self._lock:
            row = self._conn.execute(
                "SELECT command_id,run_id,payload_digest,event_count,first_seq,last_seq,"
                "event_set_digest,outbox_set_digest,receipt_json,receipt_digest "
                "FROM commands "
                "WHERE receipt_digest=?",
                (receipt_digest,),
            ).fetchone()
            if row is None:
                raise KeyError(receipt_digest)
            record = json.loads(row[8])
            if type(record) is not dict or set(record) != {
                "canonical_receipt",
                "first_seq",
                "last_seq",
                "receipt_digest",
                "state_checksum",
            }:
                raise IntegrityError("complete canonical receipt is unavailable")
            body = record["canonical_receipt"]
            if type(body) is not dict or set(body) != {
                "command_id",
                "kind",
                "parent_digests",
                "payload",
                "receipt_id",
                "run_id",
                "schema_version",
            }:
                raise IntegrityError("canonical receipt body is malformed")
            try:
                receipt = CanonicalReceipt(
                    receipt_id=body["receipt_id"],
                    run_id=body["run_id"],
                    command_id=body["command_id"],
                    kind=body["kind"],
                    payload=body["payload"],
                    parent_digests=tuple(body["parent_digests"]),
                    schema_version=body["schema_version"],
                )
            except (KeyError, TypeError, ValueError) as exc:
                raise IntegrityError("canonical receipt body is invalid") from exc
            command_id = str(row[0])
            if (
                receipt.digest != receipt_digest
                or row[9] != receipt_digest
                or record["receipt_digest"] != receipt_digest
                or receipt.receipt_id != f"receipt:{command_id}"
                or receipt.command_id != command_id
                or receipt.run_id != row[1]
                or receipt.run_id != self.run_id
                or receipt.kind != "COMMAND_COMMITTED"
                or type(record["first_seq"]) is not int
                or type(record["last_seq"]) is not int
                or record["first_seq"] != row[4]
                or record["last_seq"] != row[5]
                or record["state_checksum"] != receipt.payload.get("state_checksum")
            ):
                raise IntegrityError("canonical receipt identity diverged")
            events = self._conn.execute(
                "SELECT event_digest FROM events WHERE command_id=? ORDER BY ordinal",
                (command_id,),
            ).fetchall()
            event_digests = [str(item[0]) for item in events]
            outbox_rows = self._conn.execute(
                "SELECT ordinal,outbox_id,payload_digest,topic FROM immutable_outbox "
                "WHERE command_id=? ORDER BY ordinal",
                (command_id,),
            ).fetchall()
            outbox = [
                {
                    "ordinal": int(item[0]),
                    "outbox_id": str(item[1]),
                    "payload_digest": str(item[2]),
                    "topic": str(item[3]),
                }
                for item in outbox_rows
            ]
            payload = receipt.payload
            if (
                set(payload)
                != {
                    "command_payload_digest",
                    "event_digests",
                    "first_seq",
                    "last_seq",
                    "outbox",
                    "projection_mutation_digest",
                    "state_checksum",
                }
                or type(payload["event_digests"]) is not tuple
                or type(payload["outbox"]) is not tuple
                or type(payload["first_seq"]) is not int
                or type(payload["last_seq"]) is not int
                or payload["command_payload_digest"] != row[2]
                or list(payload["event_digests"]) != event_digests
                or len(event_digests) != int(row[3])
                or canonical_digest(event_digests) != row[6]
                or payload["first_seq"] != int(row[4])
                or payload["last_seq"] != int(row[5])
                or list(payload["outbox"]) != outbox
                or canonical_digest(outbox) != row[7]
                or not _is_sha256(payload["projection_mutation_digest"])
                or not _is_sha256(payload["state_checksum"])
            ):
                raise IntegrityError("canonical receipt does not resolve its command")
            return receipt

    def resolve_receipt_for_event(self, event_digest: str) -> CanonicalReceipt:
        """Resolve an event only through its immutable complete command receipt."""

        return self.resolve_receipt(self.receipt_digest_for_event(event_digest))

    def receipt_object_index(self):
        """Build and seal the longest losslessly indexed command prefix.

        A legacy or missing first object yields an empty prefix.  Later objects are
        never spliced across that gap, so C6 cannot mistake partial migration for a
        complete decision-time history.
        """

        from muteki.epistemic.cas import ReceiptCAS
        from muteki.epistemic.receipt_objects import (
            CommandReceiptObjectIndexV1,
            ReceiptObjectIndexEntryV1,
            ReceiptObjectState,
        )

        with self._lock:
            rows = self._conn.execute(
                "SELECT c.command_id,c.receipt_digest,c.first_seq,c.last_seq,"
                "o.object_digest,o.byte_count,o.state,o.diagnostic_receipt_digest "
                "FROM commands c LEFT JOIN command_receipt_objects o "
                "ON o.command_id=c.command_id ORDER BY c.first_seq"
            ).fetchall()
            entries = []
            expected_first = 1
            for row in rows:
                if int(row[2]) != expected_first or row[4] is None:
                    break
                try:
                    state = ReceiptObjectState(str(row[6]))
                except ValueError:
                    break
                entry = ReceiptObjectIndexEntryV1(
                    run_id=self.run_id,
                    command_id=str(row[0]),
                    receipt_digest=str(row[1]),
                    first_seq=int(row[2]),
                    last_seq=int(row[3]),
                    state=state,
                    object_digest=str(row[4] or ""),
                    byte_count=int(row[5] or 0),
                    diagnostic_receipt_digest=str(row[7] or ""),
                )
                if state is not ReceiptObjectState.RESOLVED:
                    break
                entries.append(entry)
                expected_first = entry.last_seq + 1
            complete = entries[-1].last_seq if entries else 0
            head = ""
            if complete:
                head_row = self._conn.execute(
                    "SELECT event_digest FROM events WHERE seq=?", (complete,)
                ).fetchone()
                if head_row is None:
                    raise IntegrityError("receipt object prefix has no event head")
                head = str(head_row[0])
            index = CommandReceiptObjectIndexV1(
                run_id=self.run_id,
                complete_through_seq=complete,
                head_event_digest=head,
                entries=tuple(entries),
            )
        index.seal(ReceiptCAS(self.path.parent / "receipt-objects-cas"))
        return index

    def receipt_field_resolver(self, *, cutoff_seq: int | None = None):
        """Return a read-only resolver for a stable complete receipt prefix.

        When ``cutoff_seq`` is supplied, the resolver owns a truncated index whose
        digest cannot change as later commands append.  This is the only safe form
        for a replayable decision-time ContextPacket.
        """

        from muteki.epistemic.cas import ReceiptCAS
        from muteki.epistemic.receipt_objects import (
            CanonicalCommandReceiptResolverV1,
            CommandReceiptObjectIndexV1,
        )

        index = self.receipt_object_index()
        if cutoff_seq is not None:
            if type(cutoff_seq) is not int or cutoff_seq < 0:
                raise ValueError("cutoff_seq must be a non-negative exact integer")
            if cutoff_seq > index.complete_through_seq:
                raise IntegrityError("cutoff exceeds the complete receipt index")
            entries = tuple(
                entry for entry in index.entries if entry.last_seq <= cutoff_seq
            )
            if cutoff_seq and (not entries or entries[-1].last_seq != cutoff_seq):
                raise IntegrityError(
                    "cutoff must end at one complete command receipt boundary"
                )
            with self._lock:
                row = (
                    self._conn.execute(
                        "SELECT event_digest FROM events WHERE seq=?",
                        (cutoff_seq,),
                    ).fetchone()
                    if cutoff_seq
                    else None
                )
            if cutoff_seq and row is None:
                raise IntegrityError("cutoff has no canonical event head")
            index = CommandReceiptObjectIndexV1(
                run_id=self.run_id,
                complete_through_seq=cutoff_seq,
                head_event_digest=str(row[0]) if row is not None else "",
                entries=entries,
            )
            index.seal(ReceiptCAS(self.path.parent / "receipt-objects-cas"))
        return CanonicalCommandReceiptResolverV1(
            index=index,
            cas=ReceiptCAS(self.path.parent / "receipt-objects-cas"),
        )

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

    def runtime_projection_digest(self) -> str:
        tables = (
            "runtime_branches",
            "budget_accounts",
            "runtime_attempts",
            "budget_reservations",
            "effect_conflict_holds",
            "effect_operations",
            "effect_attempts",
            "catalog_drafts",
            "catalog_attachments",
            "provision_operations",
            "catalog_runs",
            "archive_operations",
            "purge_operations",
            "purge_plan_items",
            "catalog_tombstones",
        )
        snapshot: dict[str, list[list[Any]]] = {}
        with self._lock:
            for table in tables:
                rows = self._conn.execute(
                    f"SELECT * FROM {table} ORDER BY 1"  # fixed identifier allowlist
                ).fetchall()
                snapshot[table] = [list(row) for row in rows]
        return canonical_digest(snapshot)

    def rebuild_runtime_projections(self) -> str:
        """Delete disposable runtime/catalog projections and replay canonical events."""
        delete_order = (
            "catalog_tombstones",
            "purge_plan_items",
            "purge_operations",
            "archive_operations",
            "effect_attempts",
            "effect_operations",
            "effect_conflict_holds",
            "budget_reservations",
            "runtime_attempts",
            "runtime_branches",
            "budget_accounts",
            "catalog_runs",
            "provision_operations",
            "catalog_attachments",
            "catalog_drafts",
        )
        with self._lock:
            self._conn.execute("BEGIN IMMEDIATE")
            try:
                for table in delete_order:
                    self._conn.execute(f"DELETE FROM {table}")
                rows = self._conn.execute(
                    "SELECT kind,payload_json FROM events ORDER BY seq"
                ).fetchall()
                for kind, payload_json in rows:
                    mutation = self._mutation_from_event(kind, json.loads(payload_json))
                    if mutation is not None:
                        self._apply_projection_mutation(
                            mutation, enforce_live_guards=False
                        )
                digest = self.runtime_projection_digest()
                self._conn.commit()
                return digest
            except Exception:
                self._conn.rollback()
                raise

    def _replay(self) -> CanonicalState:
        commands = self._conn.execute(
            "SELECT command_id,event_count,first_seq,last_seq FROM commands ORDER BY first_seq"
        ).fetchall()
        state = initial_state(self.run_id)
        for command_id, expected_count, first_seq, last_seq in commands:
            rows = self._conn.execute(
                "SELECT seq,event_id,run_id,command_id,ordinal,kind,actor,occurred_at_ns,"
                "payload_json,parent_event_digest,event_digest FROM events "
                "WHERE command_id=? ORDER BY ordinal",
                (command_id,),
            ).fetchall()
            if len(rows) != expected_count or not rows:
                raise IntegrityError("incomplete command event group")
            if rows[0][0] != first_seq or rows[-1][0] != last_seq:
                raise IntegrityError("command prefix boundary mismatch")
            for expected_ordinal, row in enumerate(rows):
                if row[4] != expected_ordinal:
                    raise IntegrityError("event ordinal gap")
                envelope = EventEnvelopeV2(
                    event_id=row[1],
                    run_id=row[2],
                    command_id=row[3],
                    ordinal=row[4],
                    kind=row[5],
                    actor=row[6],
                    occurred_at_ns=row[7],
                    payload=json.loads(row[8]),
                    parent_event_digest=row[9],
                )
                if envelope.digest != row[10]:
                    raise IntegrityError("event digest mismatch")
                state = apply_event(state, envelope, seq=int(row[0]))
        return replace(state, command_count=len(commands))

    def rebuild_projection(self) -> CanonicalState:
        with self._lock:
            self._conn.execute("BEGIN IMMEDIATE")
            try:
                state = self._replay()
                self._conn.execute(
                    "UPDATE state_projection SET head_seq=?,state_json=?,checksum=? "
                    "WHERE singleton=1",
                    (
                        state.head_seq,
                        canonical_json_bytes(state.as_dict()).decode(),
                        state.checksum,
                    ),
                )
                self._conn.commit()
                return state
            except Exception:
                self._conn.rollback()
                raise

    def verify(self) -> CanonicalState:
        with self._lock:
            quick = self._conn.execute("PRAGMA quick_check").fetchone()
            if quick is None or quick[0] != "ok":
                raise IntegrityError("SQLite quick_check failed")
            replayed = self._replay()
            current = self._state()
            if replayed.checksum != current.checksum:
                raise IntegrityError("projection does not match legal event prefix")
            return current
