"""Types, exceptions, and payload helpers moved from sqlite_store.py."""
from __future__ import annotations

from collections.abc import Mapping, Sequence
from dataclasses import dataclass, field
from typing import Any

from muteki.epistemic.contracts import (
    canonical_digest,
)

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
        if type(values.get("flag_byte_count")) is not int or values["flag_byte_count"] < 0:
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
