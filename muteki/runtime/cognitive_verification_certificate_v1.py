"""Pure replayable certificate for one cognitive reproduction check.

This module joins already-existing value contracts.  It does not read a store or
CAS, append an event, dispatch an attempt, update beliefs, route production, or
change the provenance gate's accepted set.  In particular, constructing a
``SUPPORTED`` value here is not a canonical verification transition: a future
store-owned resolver must independently bind every occurrence to the event log.

The reducer is deliberately conservative.  Complete accounting is derived from
the actual admission, terminal, and budget-event payloads.  There is no caller
``complete`` flag.  A clean settlement must contain exactly one observed
measurement for every (arbitrarily named) reserved runtime budget axis, with the
same ceiling and a value inside that ceiling.
"""

from __future__ import annotations

from collections.abc import Mapping, Sequence
from dataclasses import dataclass, field
from enum import Enum
from typing import Any

from muteki.epistemic.contracts import (
    FrozenJSON,
    canonical_digest,
    canonical_json_bytes,
    freeze_json,
)
from muteki.runtime.cognitive_engine_registry_v1 import (
    EngineIndependenceStateV1,
    RunPinnedEngineIdentityV1,
    reduce_engine_independence_v1,
)
from muteki.runtime.cognitive_reproduction_witness_v1 import (
    CognitiveReproductionWitnessV1,
    ReproductionEnvironmentEntryV1,
    ReproductionInputArtifactV1,
    ReproductionInputModeV1,
    ReproductionWitnessStatusV1,
    SourcePreOutcomeFenceV1,
    assess_cognitive_reproduction_witness,
)
from muteki.runtime.cognitive_verification_checker_v1 import (
    CognitiveVerificationRelationV1,
    DeterministicCognitiveVerificationCheckV1,
)


COGNITIVE_VERIFICATION_EVENT_REFERENCE_SCHEMA_ID = (
    "muteki.cognitive-verification-event-reference.v1"
)
COGNITIVE_VERIFICATION_PAYLOAD_OCCURRENCE_SCHEMA_ID = (
    "muteki.cognitive-verification-payload-occurrence.v1"
)
COGNITIVE_ENGINE_REGISTRY_PROVENANCE_SCHEMA_ID = (
    "muteki.cognitive-engine-registry-provenance.v1"
)
COGNITIVE_REPRODUCTION_WITNESS_PROVENANCE_SCHEMA_ID = (
    "muteki.cognitive-reproduction-witness-provenance.v1"
)
COGNITIVE_ATTEMPT_ACCOUNTING_SCHEMA_ID = (
    "muteki.cognitive-verification-attempt-accounting.v1"
)
COGNITIVE_VERIFICATION_CERTIFICATE_SCHEMA_ID = (
    "muteki.cognitive-verification-certificate.v1"
)
COGNITIVE_VERIFICATION_CERTIFICATE_REDUCER_VERSION = (
    "muteki.cognitive-verification-certificate-reducer.v1"
)

PRODUCTION_ENABLED = False
ACCEPTED_SET_CHANGE = False
LEARNING_AUTHORITY = False
STORE_WRITE_AUTHORITY = False
DISPATCH_AUTHORITY = False
AUTOMATIC_REDISPATCH_PERMITTED = False


def _text(value: object, name: str) -> str:
    if type(value) is not str or not value or value != value.strip():
        raise ValueError(f"{name} must be exact non-empty text")
    return value


def _digest(value: object, name: str) -> str:
    value = _text(value, name)
    if len(value) != 64 or any(character not in "0123456789abcdef" for character in value):
        raise ValueError(f"{name} must be a lowercase sha256 digest")
    return value


def _positive_int(value: object, name: str) -> int:
    if type(value) is not int or value < 1:
        raise ValueError(f"{name} must be a positive exact integer")
    return value


def _non_negative_int(value: object, name: str) -> int:
    if type(value) is not int or value < 0:
        raise ValueError(f"{name} must be a non-negative exact integer")
    return value


def _mapping(value: object, name: str) -> Mapping[str, Any]:
    if not isinstance(value, Mapping) or any(type(key) is not str for key in value):
        raise TypeError(f"{name} must be a string-keyed mapping")
    return value


def _exact_keys(value: Mapping[str, Any], expected: frozenset[str], name: str) -> None:
    if frozenset(value) != expected:
        raise ValueError(f"{name} has non-canonical fields")


def _sequence(value: object, name: str) -> Sequence[Any]:
    if type(value) not in {tuple, list}:
        raise TypeError(f"{name} must be a JSON sequence")
    return value


def _canonical_equal(left: object, right: object) -> bool:
    return canonical_json_bytes(left) == canonical_json_bytes(right)


def _reason_tuple(reasons: set[str] | list[str] | tuple[str, ...]) -> tuple[str, ...]:
    return tuple(sorted(set(reasons)))


@dataclass(frozen=True, slots=True)
class CanonicalCognitiveEventReferenceV1:
    """Exact identity of one canonical occurrence, without claiming it exists."""

    run_id: str
    run_scope_digest: str
    seq: int
    event_kind: str
    actor: str
    event_digest: str
    command_receipt_digest: str
    payload_digest: str
    attempt_id: str

    def __post_init__(self) -> None:
        object.__setattr__(self, "run_id", _text(self.run_id, "run_id"))
        object.__setattr__(
            self, "run_scope_digest", _digest(self.run_scope_digest, "run_scope_digest")
        )
        object.__setattr__(self, "seq", _positive_int(self.seq, "seq"))
        object.__setattr__(self, "event_kind", _text(self.event_kind, "event_kind"))
        object.__setattr__(self, "actor", _text(self.actor, "actor"))
        for name in ("event_digest", "command_receipt_digest", "payload_digest"):
            object.__setattr__(self, name, _digest(getattr(self, name), name))
        object.__setattr__(self, "attempt_id", _text(self.attempt_id, "attempt_id"))

    def canonical_body(self) -> dict[str, Any]:
        return {
            "actor": self.actor,
            "attempt_id": self.attempt_id,
            "command_receipt_digest": self.command_receipt_digest,
            "event_digest": self.event_digest,
            "event_kind": self.event_kind,
            "payload_digest": self.payload_digest,
            "run_id": self.run_id,
            "run_scope_digest": self.run_scope_digest,
            "schema_id": COGNITIVE_VERIFICATION_EVENT_REFERENCE_SCHEMA_ID,
            "seq": self.seq,
        }

    @property
    def digest(self) -> str:
        return canonical_digest(self.canonical_body())

    @classmethod
    def from_canonical(cls, value: Mapping[str, Any]) -> "CanonicalCognitiveEventReferenceV1":
        body = _mapping(value, "event reference")
        _exact_keys(
            body,
            frozenset(
                {
                    "actor",
                    "attempt_id",
                    "command_receipt_digest",
                    "event_digest",
                    "event_kind",
                    "payload_digest",
                    "run_id",
                    "run_scope_digest",
                    "schema_id",
                    "seq",
                }
            ),
            "event reference",
        )
        if body["schema_id"] != COGNITIVE_VERIFICATION_EVENT_REFERENCE_SCHEMA_ID:
            raise ValueError("event reference schema_id is unsupported")
        result = cls(
            run_id=body["run_id"],
            run_scope_digest=body["run_scope_digest"],
            seq=body["seq"],
            event_kind=body["event_kind"],
            actor=body["actor"],
            event_digest=body["event_digest"],
            command_receipt_digest=body["command_receipt_digest"],
            payload_digest=body["payload_digest"],
            attempt_id=body["attempt_id"],
        )
        if not _canonical_equal(result.canonical_body(), body):
            raise ValueError("event reference is not canonical")
        return result


@dataclass(frozen=True, slots=True)
class CanonicalRuntimePayloadOccurrenceV1:
    """One event reference paired with the exact payload used by this reducer."""

    reference: CanonicalCognitiveEventReferenceV1
    payload: FrozenJSON

    def __post_init__(self) -> None:
        if type(self.reference) is not CanonicalCognitiveEventReferenceV1:
            raise TypeError("reference must be exact CanonicalCognitiveEventReferenceV1")
        payload = freeze_json(self.payload, path="$.payload")
        if not isinstance(payload, Mapping):
            raise TypeError("payload must be a canonical mapping")
        object.__setattr__(self, "payload", payload)

    def canonical_body(self) -> dict[str, Any]:
        return {
            "payload": self.payload,
            "reference": self.reference.canonical_body(),
            "schema_id": COGNITIVE_VERIFICATION_PAYLOAD_OCCURRENCE_SCHEMA_ID,
        }

    @property
    def digest(self) -> str:
        return canonical_digest(self.canonical_body())

    @classmethod
    def from_canonical(cls, value: Mapping[str, Any]) -> "CanonicalRuntimePayloadOccurrenceV1":
        body = _mapping(value, "payload occurrence")
        _exact_keys(body, frozenset({"payload", "reference", "schema_id"}), "payload occurrence")
        if body["schema_id"] != COGNITIVE_VERIFICATION_PAYLOAD_OCCURRENCE_SCHEMA_ID:
            raise ValueError("payload occurrence schema_id is unsupported")
        result = cls(
            reference=CanonicalCognitiveEventReferenceV1.from_canonical(
                _mapping(body["reference"], "payload occurrence reference")
            ),
            payload=body["payload"],
        )
        if not _canonical_equal(result.canonical_body(), body):
            raise ValueError("payload occurrence is not canonical")
        return result


class CognitiveRegistryProvenanceGradeV1(str, Enum):
    RUN_FROZEN_CANONICAL_REFERENCE = "run_frozen_canonical_reference"
    UNPROVEN = "unproven"


@dataclass(frozen=True, slots=True)
class CognitiveEngineRegistryProvenanceV1:
    """References to prelaunch registrations and their frozen run manifest.

    The grade is recomputed by the certificate reducer.  The references do not prove
    their own existence; that remains the future resolver's responsibility.
    """

    source_registration: CanonicalCognitiveEventReferenceV1
    reproducer_registration: CanonicalCognitiveEventReferenceV1
    run_manifest_frozen: CanonicalCognitiveEventReferenceV1

    def __post_init__(self) -> None:
        for name in (
            "source_registration",
            "reproducer_registration",
            "run_manifest_frozen",
        ):
            if type(getattr(self, name)) is not CanonicalCognitiveEventReferenceV1:
                raise TypeError(f"{name} must be an exact canonical event reference")

    def canonical_body(self) -> dict[str, Any]:
        return {
            "reproducer_registration": self.reproducer_registration.canonical_body(),
            "run_manifest_frozen": self.run_manifest_frozen.canonical_body(),
            "schema_id": COGNITIVE_ENGINE_REGISTRY_PROVENANCE_SCHEMA_ID,
            "source_registration": self.source_registration.canonical_body(),
        }

    @property
    def digest(self) -> str:
        return canonical_digest(self.canonical_body())

    @classmethod
    def from_canonical(cls, value: Mapping[str, Any]) -> "CognitiveEngineRegistryProvenanceV1":
        body = _mapping(value, "engine registry provenance")
        _exact_keys(
            body,
            frozenset(
                {
                    "reproducer_registration",
                    "run_manifest_frozen",
                    "schema_id",
                    "source_registration",
                }
            ),
            "engine registry provenance",
        )
        if body["schema_id"] != COGNITIVE_ENGINE_REGISTRY_PROVENANCE_SCHEMA_ID:
            raise ValueError("engine registry provenance schema_id is unsupported")
        result = cls(
            source_registration=CanonicalCognitiveEventReferenceV1.from_canonical(
                _mapping(body["source_registration"], "source registration")
            ),
            reproducer_registration=CanonicalCognitiveEventReferenceV1.from_canonical(
                _mapping(body["reproducer_registration"], "reproducer registration")
            ),
            run_manifest_frozen=CanonicalCognitiveEventReferenceV1.from_canonical(
                _mapping(body["run_manifest_frozen"], "run manifest")
            ),
        )
        if not _canonical_equal(result.canonical_body(), body):
            raise ValueError("engine registry provenance is not canonical")
        return result


@dataclass(frozen=True, slots=True)
class CognitiveReproductionWitnessProvenanceV1:
    """Separate preregistration and launcher-owned actual-launch occurrences."""

    prelaunch_declaration: CanonicalCognitiveEventReferenceV1
    launcher_actual_witness: CanonicalCognitiveEventReferenceV1
    witness_digest: str

    def __post_init__(self) -> None:
        if type(self.prelaunch_declaration) is not CanonicalCognitiveEventReferenceV1:
            raise TypeError("prelaunch_declaration must be an exact event reference")
        if type(self.launcher_actual_witness) is not CanonicalCognitiveEventReferenceV1:
            raise TypeError("launcher_actual_witness must be an exact event reference")
        object.__setattr__(
            self,
            "witness_digest",
            _digest(self.witness_digest, "witness_digest"),
        )

    def canonical_body(self) -> dict[str, Any]:
        return {
            "launcher_actual_witness": self.launcher_actual_witness.canonical_body(),
            "prelaunch_declaration": self.prelaunch_declaration.canonical_body(),
            "schema_id": COGNITIVE_REPRODUCTION_WITNESS_PROVENANCE_SCHEMA_ID,
            "witness_digest": self.witness_digest,
        }

    @property
    def digest(self) -> str:
        return canonical_digest(self.canonical_body())

    @classmethod
    def from_canonical(cls, value: Mapping[str, Any]) -> "CognitiveReproductionWitnessProvenanceV1":
        body = _mapping(value, "reproduction witness provenance")
        _exact_keys(
            body,
            frozenset(
                {
                    "launcher_actual_witness",
                    "prelaunch_declaration",
                    "schema_id",
                    "witness_digest",
                }
            ),
            "reproduction witness provenance",
        )
        if body["schema_id"] != COGNITIVE_REPRODUCTION_WITNESS_PROVENANCE_SCHEMA_ID:
            raise ValueError("reproduction witness provenance schema_id is unsupported")
        result = cls(
            prelaunch_declaration=CanonicalCognitiveEventReferenceV1.from_canonical(
                _mapping(body["prelaunch_declaration"], "prelaunch declaration")
            ),
            launcher_actual_witness=CanonicalCognitiveEventReferenceV1.from_canonical(
                _mapping(body["launcher_actual_witness"], "launcher actual witness")
            ),
            witness_digest=body["witness_digest"],
        )
        if not _canonical_equal(result.canonical_body(), body):
            raise ValueError("reproduction witness provenance is not canonical")
        return result


class CognitiveAttemptAccountingRoleV1(str, Enum):
    SOURCE = "source"
    REPRODUCER = "reproducer"
    CHECKER = "checker"


class CognitiveAttemptAccountingStatusV1(str, Enum):
    COMPLETE_ACCOUNTED = "complete_accounted"
    HELD_UNKNOWN = "held_unknown"
    INVALID = "invalid"


def _usage_contract(
    statement: "ResolvedCognitiveAttemptAccountingV1",
) -> tuple[CognitiveAttemptAccountingStatusV1, tuple[str, ...], dict[str, int], dict[str, int]]:
    """Recompute accounting status only from launch-lifecycle payload facts."""

    invalid: set[str] = set()
    held: set[str] = set()
    admission = statement.admission
    terminal = statement.terminal
    admission_payload = _mapping(admission.payload, "admission payload")
    terminal_payload = _mapping(terminal.payload, "terminal payload")

    occurrences = (admission, terminal, *statement.budget_occurrences)
    for occurrence in occurrences:
        ref = occurrence.reference
        if canonical_digest(occurrence.payload) != ref.payload_digest:
            invalid.add("event_payload_digest_mismatch")
        if (
            ref.run_id != admission.reference.run_id
            or ref.run_scope_digest != admission.reference.run_scope_digest
        ):
            invalid.add("accounting_cross_run_splice")
        if ref.attempt_id != admission.reference.attempt_id:
            invalid.add("accounting_attempt_reference_splice")

    if admission.reference.event_kind != "ATTEMPT_ADMITTED":
        invalid.add("admission_event_kind_invalid")
    if admission.reference.actor != "search-admission":
        invalid.add("admission_actor_invalid")
    if terminal.reference.event_kind not in {"WORKER_TERMINAL", "WORKER_UNKNOWN"}:
        invalid.add("terminal_event_kind_invalid")
    if terminal.reference.actor != "run-supervisor":
        invalid.add("terminal_actor_invalid")
    if admission.reference.seq >= terminal.reference.seq:
        invalid.add("terminal_not_after_admission")

    attempt_id = admission.reference.attempt_id
    for payload, label in ((admission_payload, "admission"), (terminal_payload, "terminal")):
        if payload.get("attempt_id") != attempt_id:
            invalid.add(f"{label}_attempt_id_mismatch")
    attempt_digest = admission_payload.get("attempt_digest")
    if not _is_digest(attempt_digest):
        invalid.add("admission_attempt_digest_invalid")
    elif terminal_payload.get("attempt_digest") != attempt_digest:
        invalid.add("terminal_attempt_digest_mismatch")
    if terminal_payload.get("admission_event_digest") != admission.reference.event_digest:
        invalid.add("terminal_admission_lineage_mismatch")
    outcome = terminal_payload.get("outcome")
    if terminal.reference.event_kind == "WORKER_UNKNOWN":
        if outcome != "unknown":
            invalid.add("unknown_terminal_outcome_mismatch")
        else:
            held.add("worker_terminal_unknown")
    elif type(outcome) is not str or not outcome or outcome == "unknown":
        invalid.add("terminal_outcome_invalid")

    reserved: dict[str, int] = {}
    requested = admission_payload.get("requested_budget")
    if not isinstance(requested, Mapping) or not requested:
        invalid.add("reserved_budget_missing")
    else:
        for axis_value, ceiling_value in requested.items():
            try:
                axis = _text(axis_value, "budget axis")
                ceiling = _non_negative_int(ceiling_value, f"reserved[{axis}]")
            except (TypeError, ValueError):
                invalid.add("reserved_budget_invalid")
                continue
            if axis in reserved:
                invalid.add("reserved_budget_axis_duplicate")
            reserved[axis] = ceiling

    reservation_ids_value = admission_payload.get("reservation_ids")
    reservation_ids: tuple[str, ...] = ()
    if type(reservation_ids_value) not in {tuple, list} or not reservation_ids_value:
        invalid.add("reservation_ids_missing")
    else:
        try:
            reservation_ids = tuple(
                _text(item, "reservation_id") for item in reservation_ids_value
            )
            if len(set(reservation_ids)) != len(reservation_ids):
                invalid.add("reservation_ids_duplicate")
        except (TypeError, ValueError):
            invalid.add("reservation_ids_invalid")

    if not statement.budget_occurrences:
        held.add("budget_settlement_missing")
        return _accounting_result(invalid, held, reserved, {})
    if len(statement.budget_occurrences) != 1:
        invalid.add("budget_terminal_occurrence_not_unique")
        return _accounting_result(invalid, held, reserved, {})

    budget = statement.budget_occurrences[0]
    budget_payload = _mapping(budget.payload, "budget payload")
    if budget.reference.seq <= admission.reference.seq:
        invalid.add("budget_event_not_after_admission")
    if budget.reference.actor != "search-admission":
        invalid.add("budget_actor_invalid")
    if budget_payload.get("attempt_id") != attempt_id:
        invalid.add("budget_attempt_id_mismatch")
    if budget.reference.event_kind == "BUDGET_USAGE_UNKNOWN":
        held.add("budget_usage_unknown")
        return _accounting_result(invalid, held, reserved, {})
    if budget.reference.event_kind != "BUDGET_SETTLED":
        invalid.add("budget_event_kind_invalid")
        return _accounting_result(invalid, held, reserved, {})

    if budget_payload.get("reservation_ids") != reservation_ids:
        invalid.add("settlement_reservation_ids_mismatch")
    if type(budget_payload.get("settlement_revision")) is not int or budget_payload.get(
        "settlement_revision", 0
    ) < 1:
        invalid.add("settlement_revision_invalid")

    report = budget_payload.get("usage_report")
    if not isinstance(report, Mapping) or frozenset(report) != {"measurements"}:
        invalid.add("usage_report_invalid")
        return _accounting_result(invalid, held, reserved, {})
    if canonical_digest(report) != budget_payload.get("usage_report_digest"):
        invalid.add("usage_report_digest_mismatch")
    measurements = report.get("measurements")
    if type(measurements) not in {tuple, list}:
        invalid.add("usage_measurements_invalid")
        return _accounting_result(invalid, held, reserved, {})

    axes: list[str] = []
    observed: dict[str, int] = {}
    charged: dict[str, int] = {}
    for raw in measurements:
        if not isinstance(raw, Mapping) or frozenset(raw) != {
            "axis",
            "observed_so_far",
            "reserved_ceiling",
            "status",
        }:
            invalid.add("usage_measurement_shape_invalid")
            continue
        try:
            axis = _text(raw.get("axis"), "usage axis")
            amount = _non_negative_int(raw.get("observed_so_far"), "observed usage")
            ceiling = _non_negative_int(raw.get("reserved_ceiling"), "usage ceiling")
        except (TypeError, ValueError):
            invalid.add("usage_measurement_value_invalid")
            continue
        status = raw.get("status")
        axes.append(axis)
        if axis in observed:
            invalid.add("usage_axis_duplicate")
        observed[axis] = amount
        if reserved.get(axis) != ceiling:
            invalid.add("usage_ceiling_mismatch")
        if amount > ceiling:
            invalid.add("usage_exceeds_reservation")
        if status == "observed":
            charged[axis] = amount
        elif status in {"partial", "unknown"}:
            charged[axis] = max(amount, ceiling)
            held.add("usage_axis_not_fully_observed")
        else:
            invalid.add("usage_status_invalid")

    if axes != sorted(axes) or len(set(axes)) != len(axes):
        invalid.add("usage_axes_not_canonical")
    if set(axes) != set(reserved):
        invalid.add("usage_axes_do_not_cover_reservation")
    actual_usage = budget_payload.get("actual_usage")
    if not isinstance(actual_usage, Mapping):
        invalid.add("actual_usage_invalid")
    else:
        try:
            actual = {
                _text(axis, "actual usage axis"): _non_negative_int(
                    amount, "actual usage amount"
                )
                for axis, amount in actual_usage.items()
            }
        except (TypeError, ValueError):
            invalid.add("actual_usage_invalid")
        else:
            if actual != charged:
                invalid.add("actual_usage_does_not_match_report")

    return _accounting_result(invalid, held, reserved, observed)


def _accounting_result(
    invalid: set[str],
    held: set[str],
    reserved: dict[str, int],
    observed: dict[str, int],
) -> tuple[CognitiveAttemptAccountingStatusV1, tuple[str, ...], dict[str, int], dict[str, int]]:
    if invalid:
        return (
            CognitiveAttemptAccountingStatusV1.INVALID,
            _reason_tuple(invalid | held),
            dict(sorted(reserved.items())),
            dict(sorted(observed.items())),
        )
    if held:
        return (
            CognitiveAttemptAccountingStatusV1.HELD_UNKNOWN,
            _reason_tuple(held),
            dict(sorted(reserved.items())),
            dict(sorted(observed.items())),
        )
    return (
        CognitiveAttemptAccountingStatusV1.COMPLETE_ACCOUNTED,
        ("complete_accounting_derived",),
        dict(sorted(reserved.items())),
        dict(sorted(observed.items())),
    )


def _is_digest(value: object) -> bool:
    return (
        type(value) is str
        and len(value) == 64
        and all(character in "0123456789abcdef" for character in value)
    )


@dataclass(frozen=True, slots=True)
class ResolvedCognitiveAttemptAccountingV1:
    """Replayable attempt accounting; completeness is never supplied by a caller."""

    role: CognitiveAttemptAccountingRoleV1
    admission: CanonicalRuntimePayloadOccurrenceV1
    terminal: CanonicalRuntimePayloadOccurrenceV1
    budget_occurrences: tuple[CanonicalRuntimePayloadOccurrenceV1, ...]

    def __post_init__(self) -> None:
        if type(self.role) is not CognitiveAttemptAccountingRoleV1:
            raise TypeError("role must be exact CognitiveAttemptAccountingRoleV1")
        if type(self.admission) is not CanonicalRuntimePayloadOccurrenceV1:
            raise TypeError("admission must be exact CanonicalRuntimePayloadOccurrenceV1")
        if type(self.terminal) is not CanonicalRuntimePayloadOccurrenceV1:
            raise TypeError("terminal must be exact CanonicalRuntimePayloadOccurrenceV1")
        if type(self.budget_occurrences) is not tuple or any(
            type(item) is not CanonicalRuntimePayloadOccurrenceV1
            for item in self.budget_occurrences
        ):
            raise TypeError("budget_occurrences must be an immutable typed tuple")

    @property
    def _assessment(
        self,
    ) -> tuple[CognitiveAttemptAccountingStatusV1, tuple[str, ...], dict[str, int], dict[str, int]]:
        return _usage_contract(self)

    @property
    def status(self) -> CognitiveAttemptAccountingStatusV1:
        return self._assessment[0]

    @property
    def reason_codes(self) -> tuple[str, ...]:
        return self._assessment[1]

    @property
    def reserved_usage(self) -> dict[str, int]:
        return self._assessment[2]

    @property
    def observed_usage(self) -> dict[str, int]:
        return self._assessment[3]

    @property
    def complete_accounted(self) -> bool:
        return self.status is CognitiveAttemptAccountingStatusV1.COMPLETE_ACCOUNTED

    @property
    def attempt_id(self) -> str:
        return self.admission.reference.attempt_id

    def canonical_body(self) -> dict[str, Any]:
        return {
            "admission": self.admission.canonical_body(),
            "automatic_redispatch_permitted": AUTOMATIC_REDISPATCH_PERMITTED,
            "budget_occurrences": tuple(
                item.canonical_body() for item in self.budget_occurrences
            ),
            "complete_accounted": self.complete_accounted,
            "observed_usage": self.observed_usage,
            "reason_codes": self.reason_codes,
            "reserved_usage": self.reserved_usage,
            "role": self.role.value,
            "schema_id": COGNITIVE_ATTEMPT_ACCOUNTING_SCHEMA_ID,
            "status": self.status.value,
            "terminal": self.terminal.canonical_body(),
        }

    @property
    def digest(self) -> str:
        return canonical_digest(self.canonical_body())

    @classmethod
    def from_canonical(cls, value: Mapping[str, Any]) -> "ResolvedCognitiveAttemptAccountingV1":
        body = _mapping(value, "attempt accounting")
        _exact_keys(
            body,
            frozenset(
                {
                    "admission",
                    "automatic_redispatch_permitted",
                    "budget_occurrences",
                    "complete_accounted",
                    "observed_usage",
                    "reason_codes",
                    "reserved_usage",
                    "role",
                    "schema_id",
                    "status",
                    "terminal",
                }
            ),
            "attempt accounting",
        )
        if body["schema_id"] != COGNITIVE_ATTEMPT_ACCOUNTING_SCHEMA_ID:
            raise ValueError("attempt accounting schema_id is unsupported")
        result = cls(
            role=CognitiveAttemptAccountingRoleV1(body["role"]),
            admission=CanonicalRuntimePayloadOccurrenceV1.from_canonical(
                _mapping(body["admission"], "accounting admission")
            ),
            terminal=CanonicalRuntimePayloadOccurrenceV1.from_canonical(
                _mapping(body["terminal"], "accounting terminal")
            ),
            budget_occurrences=tuple(
                CanonicalRuntimePayloadOccurrenceV1.from_canonical(
                    _mapping(item, "accounting budget occurrence")
                )
                for item in _sequence(body["budget_occurrences"], "budget occurrences")
            ),
        )
        if not _canonical_equal(result.canonical_body(), body):
            raise ValueError("attempt accounting is not a replay of its payload facts")
        return result


class CognitiveVerificationCertificateStatusV1(str, Enum):
    SUPPORTED = "SUPPORTED"
    HELD_UNKNOWN = "HELD_UNKNOWN"
    DISAGREEMENT = "DISAGREEMENT"
    INVALID = "INVALID"


class CognitiveVerificationCheckerGradeV1(str, Enum):
    """The pure checker is a deterministic replay, not semantic independence."""

    DETERMINISTIC_REPLAY_ONLY = "deterministic_replay_only"


def reproduction_witness_declaration_digest_v1(
    witness: CognitiveReproductionWitnessV1,
) -> str:
    """Digest only the material that must exist before reproduction launch."""

    if type(witness) is not CognitiveReproductionWitnessV1:
        raise TypeError("witness must be exact CognitiveReproductionWitnessV1")
    return canonical_digest(
        {
            "declared_blackboard_cutoff_seq": witness.declared_blackboard_cutoff_seq,
            "declared_environment_digest": witness.declared_environment_digest,
            "declared_home_identity_digest": witness.declared_home_identity_digest,
            "declared_input_manifest_digest": witness.declared_input_manifest_digest,
            "declared_launch_cwd_digest": witness.declared_launch_cwd_digest,
            "declared_launch_material_digest": witness.declared_launch_material_digest,
            "declared_launch_profile_digest": witness.declared_launch_profile_digest,
            "declared_memory_cutoff_seq": witness.declared_memory_cutoff_seq,
            "declared_prompt_template_digest": witness.declared_prompt_template_digest,
            "declared_session_identity_digest": witness.declared_session_identity_digest,
            "declared_workspace_identity_digest": witness.declared_workspace_identity_digest,
            "environment_allowlist_digest": witness.environment_allowlist_digest,
            "input_mode": witness.input_mode.value,
            "schema_id": "muteki.cognitive-reproduction-prelaunch-declaration.v1",
            "source_fence_digest": witness.source_fence.digest,
        }
    )


def reproduction_witness_actual_digest_v1(
    witness: CognitiveReproductionWitnessV1,
) -> str:
    """Digest launcher-owned actual material and bind it to its declaration."""

    declaration_digest = reproduction_witness_declaration_digest_v1(witness)
    return canonical_digest(
        {
            "actual_blackboard_cutoff_seq": witness.actual_blackboard_cutoff_seq,
            "actual_environment_digest": witness.actual_environment_digest,
            "actual_home_identity_digest": witness.actual_home_identity_digest,
            "actual_input_manifest_digest": witness.actual_input_manifest_digest,
            "actual_launch_binding_digest": witness.actual_launch_binding_digest,
            "actual_launch_cwd_digest": witness.actual_launch_cwd_digest,
            "actual_launch_material_digest": witness.actual_launch_material_digest,
            "actual_launch_profile_digest": witness.actual_launch_profile_digest,
            "actual_memory_cutoff_seq": witness.actual_memory_cutoff_seq,
            "actual_prompt_template_digest": witness.actual_prompt_template_digest,
            "actual_session_identity_digest": witness.actual_session_identity_digest,
            "actual_workspace_identity_digest": witness.actual_workspace_identity_digest,
            "prelaunch_declaration_digest": declaration_digest,
            "resumed_from_session_digest": witness.resumed_from_session_digest,
            "schema_id": "muteki.cognitive-reproduction-launcher-actual-witness.v1",
        }
    )


def _registry_provenance_grade(
    certificate: "CognitiveVerificationCertificateV1",
) -> CognitiveRegistryProvenanceGradeV1:
    provenance = certificate.engine_registry_provenance
    if provenance is None:
        return CognitiveRegistryProvenanceGradeV1.UNPROVEN
    source = provenance.source_registration
    reproducer = provenance.reproducer_registration
    manifest = provenance.run_manifest_frozen
    source_engine = certificate.source_engine_identity
    reproducer_engine = certificate.reproducer_engine_identity
    # The run-frozen manifest may bind only configuration known before either
    # attempt launches.  Actual launch material, workspace and session identities
    # are witnessed later by the launcher-owned per-attempt registrations.  Putting
    # those post-launch values in this manifest would make the contract impossible
    # to satisfy without inventing future evidence.
    expected_manifest = canonical_digest(
        {
            "reproducer_source_group_digest": reproducer_engine.source_group_digest,
            "reproducer_source_identity_fingerprint_digest": (
                reproducer_engine.source_identity_fingerprint_digest
            ),
            "run_scope_digest": certificate.source_assignment.run_scope_digest,
            "schema_id": "muteki.cognitive-engine-registry-run-manifest.v2",
            "source_source_group_digest": source_engine.source_group_digest,
            "source_source_identity_fingerprint_digest": (
                source_engine.source_identity_fingerprint_digest
            ),
        }
    )
    if (
        all(
            ref.run_id == certificate.source_assignment.run_id
            and ref.run_scope_digest == certificate.source_assignment.run_scope_digest
            and ref.actor == "cognitive-engine-registry-authority"
            for ref in (source, reproducer, manifest)
        )
        and source.event_kind == "COGNITIVE_ENGINE_IDENTITY_REGISTERED"
        and reproducer.event_kind == "COGNITIVE_ENGINE_IDENTITY_REGISTERED"
        and manifest.event_kind == "COGNITIVE_ENGINE_REGISTRY_RUN_FROZEN"
        and source.attempt_id == certificate.source_assignment.attempt_id
        and reproducer.attempt_id == certificate.reproduction_assignment.attempt_id
        and manifest.attempt_id == "run-manifest"
        and source.payload_digest == source_engine.digest
        and reproducer.payload_digest == reproducer_engine.digest
        and manifest.payload_digest == expected_manifest
        and manifest.seq < certificate.source_assignment.seq
        and certificate.source_assignment.seq
        < source.seq
        < certificate.source_observation.seq
        and certificate.reproduction_assignment.seq
        < reproducer.seq
        < certificate.reproduction_observation.seq
    ):
        return CognitiveRegistryProvenanceGradeV1.RUN_FROZEN_CANONICAL_REFERENCE
    return CognitiveRegistryProvenanceGradeV1.UNPROVEN


@dataclass(frozen=True, slots=True)
class CognitiveVerificationCertificateV1:
    """Inert reducer output.  It is neither a store command nor learning evidence."""

    source_assignment: CanonicalCognitiveEventReferenceV1
    source_observation: CanonicalCognitiveEventReferenceV1
    reproduction_assignment: CanonicalCognitiveEventReferenceV1
    reproduction_observation: CanonicalCognitiveEventReferenceV1
    checker_checked: CanonicalCognitiveEventReferenceV1
    source_accounting: ResolvedCognitiveAttemptAccountingV1
    reproduction_accounting: ResolvedCognitiveAttemptAccountingV1
    checker_accounting: ResolvedCognitiveAttemptAccountingV1
    deterministic_check: DeterministicCognitiveVerificationCheckV1
    reproduction_witness: CognitiveReproductionWitnessV1
    reproduction_witness_provenance: CognitiveReproductionWitnessProvenanceV1 | None
    source_engine_identity: RunPinnedEngineIdentityV1
    reproducer_engine_identity: RunPinnedEngineIdentityV1
    engine_registry_provenance: CognitiveEngineRegistryProvenanceV1 | None
    checker_implementation_digest: str
    checker_build_digest: str
    status: CognitiveVerificationCertificateStatusV1 = field(init=False)
    reason_codes: tuple[str, ...] = field(init=False)
    source_partition_digest: str | None = field(init=False)

    def __post_init__(self) -> None:
        for name in (
            "source_assignment",
            "source_observation",
            "reproduction_assignment",
            "reproduction_observation",
            "checker_checked",
        ):
            if type(getattr(self, name)) is not CanonicalCognitiveEventReferenceV1:
                raise TypeError(f"{name} must be an exact canonical event reference")
        for name, role in (
            ("source_accounting", CognitiveAttemptAccountingRoleV1.SOURCE),
            ("reproduction_accounting", CognitiveAttemptAccountingRoleV1.REPRODUCER),
            ("checker_accounting", CognitiveAttemptAccountingRoleV1.CHECKER),
        ):
            value = getattr(self, name)
            if type(value) is not ResolvedCognitiveAttemptAccountingV1:
                raise TypeError(f"{name} must be exact ResolvedCognitiveAttemptAccountingV1")
            if value.role is not role:
                raise ValueError(f"{name} has the wrong accounting role")
        if type(self.deterministic_check) is not DeterministicCognitiveVerificationCheckV1:
            raise TypeError("deterministic_check must be exact deterministic checker output")
        if type(self.reproduction_witness) is not CognitiveReproductionWitnessV1:
            raise TypeError("reproduction_witness must be exact CognitiveReproductionWitnessV1")
        if self.reproduction_witness_provenance is not None and (
            type(self.reproduction_witness_provenance)
            is not CognitiveReproductionWitnessProvenanceV1
        ):
            raise TypeError("reproduction_witness_provenance has the wrong type")
        for name in ("source_engine_identity", "reproducer_engine_identity"):
            if type(getattr(self, name)) is not RunPinnedEngineIdentityV1:
                raise TypeError(f"{name} must be exact RunPinnedEngineIdentityV1")
        if self.engine_registry_provenance is not None and (
            type(self.engine_registry_provenance)
            is not CognitiveEngineRegistryProvenanceV1
        ):
            raise TypeError("engine_registry_provenance has the wrong type")
        object.__setattr__(
            self,
            "checker_implementation_digest",
            _digest(self.checker_implementation_digest, "checker_implementation_digest"),
        )
        object.__setattr__(
            self,
            "checker_build_digest",
            _digest(self.checker_build_digest, "checker_build_digest"),
        )
        status, reasons, partition = _reduce_certificate(self)
        object.__setattr__(self, "status", status)
        object.__setattr__(self, "reason_codes", reasons)
        object.__setattr__(self, "source_partition_digest", partition)

    @property
    def occurrence_digest(self) -> str:
        return canonical_digest(
            {
                "ordered_occurrences": tuple(
                    item.digest
                    for item in (
                        self.source_assignment,
                        self.source_observation,
                        self.reproduction_assignment,
                        self.reproduction_observation,
                        self.checker_checked,
                    )
                ),
                "schema_id": "muteki.cognitive-verification-occurrence-set.v1",
            }
        )

    @property
    def component_digests(self) -> dict[str, str]:
        witness_assessment = assess_cognitive_reproduction_witness(
            self.reproduction_witness
        )
        independence = reduce_engine_independence_v1(
            self.source_engine_identity, self.reproducer_engine_identity
        )
        checker_identity_digest = canonical_digest(
            {
                "build_digest": self.checker_build_digest,
                "implementation_digest": self.checker_implementation_digest,
                "schema_id": "muteki.cognitive-verification-checker-identity.v1",
            }
        )
        return {
            "checker_accounting_digest": self.checker_accounting.digest,
            "checker_checked_occurrence_digest": self.checker_checked.digest,
            "checker_identity_digest": checker_identity_digest,
            "deterministic_check_digest": self.deterministic_check.digest,
            "engine_independence_assessment_digest": independence.digest,
            "engine_registry_provenance_digest": canonical_digest(
                None
                if self.engine_registry_provenance is None
                else self.engine_registry_provenance.canonical_body()
            ),
            "occurrence_digest": self.occurrence_digest,
            "reproducer_engine_identity_digest": self.reproducer_engine_identity.digest,
            "reproducer_registry_receipt_digest": self.reproducer_engine_identity.registry_receipt_digest,
            "reproduction_accounting_digest": self.reproduction_accounting.digest,
            "reproduction_assignment_occurrence_digest": self.reproduction_assignment.digest,
            "reproduction_observation_occurrence_digest": self.reproduction_observation.digest,
            "reproduction_witness_assessment_digest": witness_assessment.digest,
            "reproduction_witness_actual_digest": reproduction_witness_actual_digest_v1(
                self.reproduction_witness
            ),
            "reproduction_witness_declaration_digest": (
                reproduction_witness_declaration_digest_v1(self.reproduction_witness)
            ),
            "reproduction_witness_digest": self.reproduction_witness.digest,
            "reproduction_witness_provenance_digest": (
                canonical_digest(
                    None
                    if self.reproduction_witness_provenance is None
                    else self.reproduction_witness_provenance.canonical_body()
                )
            ),
            "source_accounting_digest": self.source_accounting.digest,
            "source_assignment_occurrence_digest": self.source_assignment.digest,
            "source_engine_identity_digest": self.source_engine_identity.digest,
            "source_observation_occurrence_digest": self.source_observation.digest,
            "source_registry_receipt_digest": self.source_engine_identity.registry_receipt_digest,
        }

    def canonical_body(self) -> dict[str, Any]:
        return {
            "accepted_set_change": ACCEPTED_SET_CHANGE,
            "automatic_redispatch_permitted": AUTOMATIC_REDISPATCH_PERMITTED,
            "checker_accounting": self.checker_accounting.canonical_body(),
            "checker_build_digest": self.checker_build_digest,
            "checker_checked": self.checker_checked.canonical_body(),
            "checker_implementation_digest": self.checker_implementation_digest,
            "component_digests": self.component_digests,
            "deterministic_check": self.deterministic_check.canonical_body(),
            "dispatch_authority": DISPATCH_AUTHORITY,
            "engine_registry_provenance": (
                None
                if self.engine_registry_provenance is None
                else self.engine_registry_provenance.canonical_body()
            ),
            "checker_grade": CognitiveVerificationCheckerGradeV1.DETERMINISTIC_REPLAY_ONLY.value,
            "learning_authority": LEARNING_AUTHORITY,
            "occurrence_digest": self.occurrence_digest,
            "production_enabled": PRODUCTION_ENABLED,
            "reason_codes": self.reason_codes,
            "reducer_version": COGNITIVE_VERIFICATION_CERTIFICATE_REDUCER_VERSION,
            "registry_provenance_grade": _registry_provenance_grade(self).value,
            "reproducer_engine_identity": self.reproducer_engine_identity.canonical_body(),
            "reproduction_accounting": self.reproduction_accounting.canonical_body(),
            "reproduction_assignment": self.reproduction_assignment.canonical_body(),
            "reproduction_observation": self.reproduction_observation.canonical_body(),
            "reproduction_witness": self.reproduction_witness.canonical_body(),
            "reproduction_witness_provenance": (
                None
                if self.reproduction_witness_provenance is None
                else self.reproduction_witness_provenance.canonical_body()
            ),
            "schema_id": COGNITIVE_VERIFICATION_CERTIFICATE_SCHEMA_ID,
            "source_accounting": self.source_accounting.canonical_body(),
            "source_assignment": self.source_assignment.canonical_body(),
            "source_engine_identity": self.source_engine_identity.canonical_body(),
            "source_observation": self.source_observation.canonical_body(),
            "source_partition_digest": self.source_partition_digest,
            "status": self.status.value,
            "store_write_authority": STORE_WRITE_AUTHORITY,
        }

    @property
    def digest(self) -> str:
        return canonical_digest(self.canonical_body())

    @classmethod
    def from_canonical(cls, value: Mapping[str, Any]) -> "CognitiveVerificationCertificateV1":
        body = _mapping(value, "verification certificate")
        expected = frozenset(
            {
                "accepted_set_change",
                "automatic_redispatch_permitted",
                "checker_accounting",
                "checker_build_digest",
                "checker_checked",
                "checker_grade",
                "checker_implementation_digest",
                "component_digests",
                "deterministic_check",
                "dispatch_authority",
                "engine_registry_provenance",
                "learning_authority",
                "occurrence_digest",
                "production_enabled",
                "reason_codes",
                "reducer_version",
                "registry_provenance_grade",
                "reproducer_engine_identity",
                "reproduction_accounting",
                "reproduction_assignment",
                "reproduction_observation",
                "reproduction_witness",
                "reproduction_witness_provenance",
                "schema_id",
                "source_accounting",
                "source_assignment",
                "source_engine_identity",
                "source_observation",
                "source_partition_digest",
                "status",
                "store_write_authority",
            }
        )
        _exact_keys(body, expected, "verification certificate")
        if body["schema_id"] != COGNITIVE_VERIFICATION_CERTIFICATE_SCHEMA_ID:
            raise ValueError("verification certificate schema_id is unsupported")
        result = cls(
            source_assignment=CanonicalCognitiveEventReferenceV1.from_canonical(
                _mapping(body["source_assignment"], "source assignment")
            ),
            source_observation=CanonicalCognitiveEventReferenceV1.from_canonical(
                _mapping(body["source_observation"], "source observation")
            ),
            reproduction_assignment=CanonicalCognitiveEventReferenceV1.from_canonical(
                _mapping(body["reproduction_assignment"], "reproduction assignment")
            ),
            reproduction_observation=CanonicalCognitiveEventReferenceV1.from_canonical(
                _mapping(body["reproduction_observation"], "reproduction observation")
            ),
            checker_checked=CanonicalCognitiveEventReferenceV1.from_canonical(
                _mapping(body["checker_checked"], "checker checked")
            ),
            source_accounting=ResolvedCognitiveAttemptAccountingV1.from_canonical(
                _mapping(body["source_accounting"], "source accounting")
            ),
            reproduction_accounting=ResolvedCognitiveAttemptAccountingV1.from_canonical(
                _mapping(body["reproduction_accounting"], "reproduction accounting")
            ),
            checker_accounting=ResolvedCognitiveAttemptAccountingV1.from_canonical(
                _mapping(body["checker_accounting"], "checker accounting")
            ),
            deterministic_check=_parse_check(
                _mapping(body["deterministic_check"], "deterministic check")
            ),
            reproduction_witness=_parse_witness(
                _mapping(body["reproduction_witness"], "reproduction witness")
            ),
            reproduction_witness_provenance=(
                None
                if body["reproduction_witness_provenance"] is None
                else CognitiveReproductionWitnessProvenanceV1.from_canonical(
                    _mapping(body["reproduction_witness_provenance"], "reproduction witness provenance")
                )
            ),
            source_engine_identity=_parse_engine_identity(
                _mapping(body["source_engine_identity"], "source engine identity")
            ),
            reproducer_engine_identity=_parse_engine_identity(
                _mapping(body["reproducer_engine_identity"], "reproducer engine identity")
            ),
            engine_registry_provenance=(
                None
                if body["engine_registry_provenance"] is None
                else CognitiveEngineRegistryProvenanceV1.from_canonical(
                    _mapping(body["engine_registry_provenance"], "engine registry provenance")
                )
            ),
            checker_implementation_digest=body["checker_implementation_digest"],
            checker_build_digest=body["checker_build_digest"],
        )
        if not _canonical_equal(result.canonical_body(), body):
            raise ValueError("verification certificate is not a deterministic replay")
        return result


def _reduce_certificate(
    certificate: CognitiveVerificationCertificateV1,
) -> tuple[CognitiveVerificationCertificateStatusV1, tuple[str, ...], str | None]:
    invalid: set[str] = set()
    held: set[str] = set()
    refs = (
        certificate.source_assignment,
        certificate.source_observation,
        certificate.reproduction_assignment,
        certificate.reproduction_observation,
        certificate.checker_checked,
    )
    expected_kinds = (
        "COGNITIVE_EXPERIMENT_ASSIGNED",
        "COGNITIVE_EXECUTION_OBSERVED",
        "COGNITIVE_EXPERIMENT_ASSIGNED",
        "COGNITIVE_EXECUTION_OBSERVED",
        "COGNITIVE_VERIFICATION_CHECKED",
    )
    if tuple(ref.event_kind for ref in refs) != expected_kinds:
        invalid.add("verification_event_kind_invalid")
    expected_actors = (
        "cognitive-evaluation-binding-v1-authority",
        "cognitive-runtime-observer-v1-authority",
        "cognitive-evaluation-binding-v1-authority",
        "cognitive-runtime-observer-v1-authority",
        "cognitive-verification-checker-authority",
    )
    if tuple(ref.actor for ref in refs) != expected_actors:
        invalid.add("verification_event_actor_invalid")
    if len({ref.run_id for ref in refs}) != 1 or len(
        {ref.run_scope_digest for ref in refs}
    ) != 1:
        invalid.add("verification_cross_run_splice")
    if tuple(ref.seq for ref in refs) != tuple(sorted(ref.seq for ref in refs)) or len(
        {ref.seq for ref in refs}
    ) != len(refs):
        invalid.add("verification_event_order_invalid")
    if certificate.source_assignment.attempt_id != certificate.source_observation.attempt_id:
        invalid.add("source_occurrence_attempt_mismatch")
    if (
        certificate.reproduction_assignment.attempt_id
        != certificate.reproduction_observation.attempt_id
    ):
        invalid.add("reproduction_occurrence_attempt_mismatch")

    attempts = {
        certificate.source_assignment.attempt_id,
        certificate.reproduction_assignment.attempt_id,
        certificate.checker_checked.attempt_id,
    }
    if len(attempts) != 3:
        invalid.add("verification_attempts_not_distinct")

    check = certificate.deterministic_check
    digest_bindings = (
        (check.source_assignment_payload_digest, certificate.source_assignment.payload_digest),
        (check.source_observation_payload_digest, certificate.source_observation.payload_digest),
        (
            check.reproduction_assignment_payload_digest,
            certificate.reproduction_assignment.payload_digest,
        ),
        (
            check.reproduction_observation_payload_digest,
            certificate.reproduction_observation.payload_digest,
        ),
        (check.digest, certificate.checker_checked.payload_digest),
    )
    if any(left != right for left, right in digest_bindings):
        invalid.add("checker_payload_reference_mismatch")

    accountings = (
        (certificate.source_accounting, certificate.source_assignment, certificate.source_observation),
        (
            certificate.reproduction_accounting,
            certificate.reproduction_assignment,
            certificate.reproduction_observation,
        ),
        (certificate.checker_accounting, certificate.checker_checked, certificate.checker_checked),
    )
    run_id = certificate.source_assignment.run_id
    run_scope = certificate.source_assignment.run_scope_digest
    for accounting, start, finish in accountings:
        if accounting.attempt_id != start.attempt_id:
            invalid.add(f"{accounting.role.value}_accounting_attempt_mismatch")
        accounting_refs = (
            accounting.admission.reference,
            accounting.terminal.reference,
            *(item.reference for item in accounting.budget_occurrences),
        )
        if any(ref.run_id != run_id or ref.run_scope_digest != run_scope for ref in accounting_refs):
            invalid.add(f"{accounting.role.value}_accounting_run_mismatch")
        if accounting.admission.reference.seq >= start.seq:
            invalid.add(f"{accounting.role.value}_admission_not_before_work")
        if accounting.terminal.reference.seq >= finish.seq:
            invalid.add(f"{accounting.role.value}_terminal_not_before_result")
        if any(item.reference.seq >= finish.seq for item in accounting.budget_occurrences):
            invalid.add(f"{accounting.role.value}_budget_not_before_result")
        if accounting.status is CognitiveAttemptAccountingStatusV1.INVALID:
            invalid.update(
                f"{accounting.role.value}_accounting:{reason}"
                for reason in accounting.reason_codes
            )
        elif accounting.status is CognitiveAttemptAccountingStatusV1.HELD_UNKNOWN:
            held.update(
                f"{accounting.role.value}_accounting:{reason}"
                for reason in accounting.reason_codes
            )

    witness = certificate.reproduction_witness
    witness_assessment = assess_cognitive_reproduction_witness(witness)
    if witness.source_fence.source_assignment_event_digest != certificate.source_assignment.event_digest:
        invalid.add("witness_source_assignment_reference_mismatch")
    if witness.source_fence.source_observation_seq != certificate.source_observation.seq:
        invalid.add("witness_source_observation_reference_mismatch")
    if witness_assessment.status is not ReproductionWitnessStatusV1.OUTCOME_BLIND:
        invalid.update(
            f"reproduction_witness:{reason.value}"
            for reason in witness_assessment.reason_codes
        )
    witness_provenance = certificate.reproduction_witness_provenance
    if witness_provenance is None:
        held.add("reproduction_witness_canonical_provenance_missing")
    else:
        declaration = witness_provenance.prelaunch_declaration
        actual_witness = witness_provenance.launcher_actual_witness
        if (
            declaration.event_kind != "COGNITIVE_REPRODUCTION_PRELAUNCH_DECLARED"
            or declaration.actor != "cognitive-reproduction-declaration-authority"
            or actual_witness.event_kind != "COGNITIVE_REPRODUCTION_LAUNCH_WITNESSED"
            or actual_witness.actor != "cognitive-launch-witness-authority"
        ):
            held.add("reproduction_witness_canonical_provenance_unproven")
        if any(
            ref.run_id != run_id
            or ref.run_scope_digest != run_scope
            or ref.attempt_id != certificate.reproduction_assignment.attempt_id
            for ref in (declaration, actual_witness)
        ):
            invalid.add("reproduction_witness_provenance_splice")
        if not (
            certificate.reproduction_assignment.seq
            < declaration.seq
            < actual_witness.seq
            < certificate.reproduction_observation.seq
        ):
            held.add("reproduction_witness_provenance_order_unproven")
        # Event-reference payload digests bind the complete declaration/witness
        # payloads, not the smaller component digests above.  The resolver must
        # inspect those canonical payloads and extract this witness digest.  A pure
        # certificate never pretends that a full event payload equals a component.
        if witness_provenance.witness_digest != witness.digest:
            invalid.add("reproduction_witness_component_mismatch")

    source_engine = certificate.source_engine_identity
    reproducer_engine = certificate.reproducer_engine_identity
    if source_engine.run_scope_digest != run_scope or reproducer_engine.run_scope_digest != run_scope:
        invalid.add("engine_registry_run_scope_mismatch")
    if (
        source_engine.workspace_digest
        != witness.source_fence.source_workspace_identity_digest
        or source_engine.session_digest
        != witness.source_fence.source_session_identity_digest
    ):
        invalid.add("source_engine_witness_identity_mismatch")
    if (
        reproducer_engine.launch_material_digest != witness.actual_launch_material_digest
        or reproducer_engine.actual_launch_profile_digest != witness.actual_launch_profile_digest
        or reproducer_engine.workspace_digest != witness.actual_workspace_identity_digest
        or reproducer_engine.session_digest != witness.actual_session_identity_digest
    ):
        invalid.add("reproducer_engine_witness_launch_mismatch")

    independence = reduce_engine_independence_v1(source_engine, reproducer_engine)
    if not independence.independent_reproducer_eligible:
        if independence.configured_source_disjoint is EngineIndependenceStateV1.UNPROVEN:
            held.add("configured_source_independence_unproven")
        else:
            invalid.add("configured_source_not_disjoint")

    registry_provenance = certificate.engine_registry_provenance
    if registry_provenance is None:
        held.add("engine_registry_canonical_provenance_missing")
    else:
        source_registration = registry_provenance.source_registration
        reproducer_registration = registry_provenance.reproducer_registration
        manifest = registry_provenance.run_manifest_frozen
        provenance_refs = (source_registration, reproducer_registration, manifest)
        if any(
            ref.run_id != run_id or ref.run_scope_digest != run_scope
            for ref in provenance_refs
        ):
            invalid.add("engine_registry_provenance_cross_run_splice")
        if (
            source_registration.event_kind != "COGNITIVE_ENGINE_IDENTITY_REGISTERED"
            or reproducer_registration.event_kind
            != "COGNITIVE_ENGINE_IDENTITY_REGISTERED"
            or manifest.event_kind != "COGNITIVE_ENGINE_REGISTRY_RUN_FROZEN"
            or any(
                ref.actor != "cognitive-engine-registry-authority"
                for ref in provenance_refs
            )
        ):
            held.add("engine_registry_canonical_provenance_unproven")
        if (
            source_registration.attempt_id != certificate.source_assignment.attempt_id
            or reproducer_registration.attempt_id
            != certificate.reproduction_assignment.attempt_id
            or manifest.attempt_id != "run-manifest"
        ):
            invalid.add("engine_registry_provenance_attempt_splice")
        expected_manifest_digest = canonical_digest(
            {
                "reproducer_source_group_digest": (
                    reproducer_engine.source_group_digest
                ),
                "reproducer_source_identity_fingerprint_digest": (
                    reproducer_engine.source_identity_fingerprint_digest
                ),
                "run_scope_digest": run_scope,
                "schema_id": "muteki.cognitive-engine-registry-run-manifest.v2",
                "source_source_group_digest": source_engine.source_group_digest,
                "source_source_identity_fingerprint_digest": (
                    source_engine.source_identity_fingerprint_digest
                ),
            }
        )
        if (
            source_registration.payload_digest != source_engine.digest
            or reproducer_registration.payload_digest != reproducer_engine.digest
            or manifest.payload_digest != expected_manifest_digest
        ):
            invalid.add("engine_registry_provenance_payload_mismatch")
        if not (
            manifest.seq < certificate.source_assignment.seq
            and certificate.source_assignment.seq
            < source_registration.seq
            < certificate.source_observation.seq
            and certificate.reproduction_assignment.seq
            < reproducer_registration.seq
            < certificate.reproduction_observation.seq
        ):
            held.add("engine_registry_temporal_binding_unproven")

    if certificate.checker_implementation_digest in {
        source_engine.driver_implementation_digest,
        reproducer_engine.driver_implementation_digest,
    }:
        invalid.add("checker_implementation_not_distinct")
    if certificate.checker_build_digest in {
        source_engine.driver_build_digest,
        reproducer_engine.driver_build_digest,
    }:
        invalid.add("checker_build_not_distinct")

    source_partition = check.source_partition_digest
    reproduction_partition = check.reproduction_partition_digest
    if check.relation in {
        CognitiveVerificationRelationV1.SUPPORTED,
        CognitiveVerificationRelationV1.DISAGREEMENT,
    }:
        if not _is_digest(source_partition) or not _is_digest(reproduction_partition):
            invalid.add("checker_partition_digest_invalid")
        if (
            check.source_reproduction_kernel_digest is None
            or check.source_reproduction_kernel_digest
            != check.reproduction_reproduction_kernel_digest
        ):
            invalid.add("checker_causal_kernel_mismatch")
    if check.relation is CognitiveVerificationRelationV1.SUPPORTED:
        if source_partition != reproduction_partition:
            invalid.add("supported_relation_partition_mismatch")
    elif check.relation is CognitiveVerificationRelationV1.DISAGREEMENT:
        if source_partition == reproduction_partition:
            invalid.add("disagreement_relation_partition_match")
    elif check.relation is CognitiveVerificationRelationV1.UNKNOWN:
        held.add("deterministic_checker_unknown")
    elif check.relation is CognitiveVerificationRelationV1.INVALID_SOURCE:
        invalid.add("deterministic_checker_invalid_source")

    if invalid:
        return CognitiveVerificationCertificateStatusV1.INVALID, _reason_tuple(invalid | held), None
    partition = source_partition if _is_digest(source_partition) else None
    if held:
        return CognitiveVerificationCertificateStatusV1.HELD_UNKNOWN, _reason_tuple(held), partition
    if check.relation is CognitiveVerificationRelationV1.DISAGREEMENT:
        return (
            CognitiveVerificationCertificateStatusV1.DISAGREEMENT,
            ("deterministic_checker_partition_disagreement",),
            partition,
        )
    if check.relation is CognitiveVerificationRelationV1.SUPPORTED:
        return (
            CognitiveVerificationCertificateStatusV1.SUPPORTED,
            ("verification_supported_by_replayable_components",),
            partition,
        )
    return CognitiveVerificationCertificateStatusV1.HELD_UNKNOWN, ("verification_unresolved",), partition


def reduce_cognitive_verification_certificate_v1(
    *,
    source_assignment: CanonicalCognitiveEventReferenceV1,
    source_observation: CanonicalCognitiveEventReferenceV1,
    reproduction_assignment: CanonicalCognitiveEventReferenceV1,
    reproduction_observation: CanonicalCognitiveEventReferenceV1,
    checker_checked: CanonicalCognitiveEventReferenceV1,
    source_accounting: ResolvedCognitiveAttemptAccountingV1,
    reproduction_accounting: ResolvedCognitiveAttemptAccountingV1,
    checker_accounting: ResolvedCognitiveAttemptAccountingV1,
    deterministic_check: DeterministicCognitiveVerificationCheckV1,
    reproduction_witness: CognitiveReproductionWitnessV1,
    reproduction_witness_provenance: CognitiveReproductionWitnessProvenanceV1 | None,
    source_engine_identity: RunPinnedEngineIdentityV1,
    reproducer_engine_identity: RunPinnedEngineIdentityV1,
    engine_registry_provenance: CognitiveEngineRegistryProvenanceV1 | None,
    checker_implementation_digest: str,
    checker_build_digest: str,
) -> CognitiveVerificationCertificateV1:
    """Reduce supplied immutable facts; grant no authority of any kind."""

    return CognitiveVerificationCertificateV1(
        source_assignment=source_assignment,
        source_observation=source_observation,
        reproduction_assignment=reproduction_assignment,
        reproduction_observation=reproduction_observation,
        checker_checked=checker_checked,
        source_accounting=source_accounting,
        reproduction_accounting=reproduction_accounting,
        checker_accounting=checker_accounting,
        deterministic_check=deterministic_check,
        reproduction_witness=reproduction_witness,
        reproduction_witness_provenance=reproduction_witness_provenance,
        source_engine_identity=source_engine_identity,
        reproducer_engine_identity=reproducer_engine_identity,
        engine_registry_provenance=engine_registry_provenance,
        checker_implementation_digest=checker_implementation_digest,
        checker_build_digest=checker_build_digest,
    )


def _parse_source_fence(value: Mapping[str, Any]) -> SourcePreOutcomeFenceV1:
    expected = frozenset(
        {
            "cutoff_seq",
            "prefix_digest",
            "prefix_head_event_digest",
            "source_assignment_event_digest",
            "source_home_identity_digest",
            "source_observation_seq",
            "source_session_identity_digest",
            "source_workspace_identity_digest",
        }
    )
    _exact_keys(value, expected, "source fence")
    return SourcePreOutcomeFenceV1(**dict(value))


def _parse_artifact(value: Mapping[str, Any]) -> ReproductionInputArtifactV1:
    _exact_keys(
        value,
        frozenset(
            {"availability_receipt_digest", "available_at_seq", "content_digest", "relative_path"}
        ),
        "input artifact",
    )
    return ReproductionInputArtifactV1(**dict(value))


def _parse_environment(value: Mapping[str, Any]) -> ReproductionEnvironmentEntryV1:
    _exact_keys(value, frozenset({"name", "value_digest"}), "environment entry")
    return ReproductionEnvironmentEntryV1(**dict(value))


def _parse_witness(value: Mapping[str, Any]) -> CognitiveReproductionWitnessV1:
    direct = {
        "declared_prompt_template_digest",
        "actual_prompt_template_digest",
        "declared_workspace_identity_digest",
        "actual_workspace_identity_digest",
        "declared_home_identity_digest",
        "actual_home_identity_digest",
        "declared_session_identity_digest",
        "actual_session_identity_digest",
        "resumed_from_session_digest",
        "declared_blackboard_cutoff_seq",
        "actual_blackboard_cutoff_seq",
        "declared_memory_cutoff_seq",
        "actual_memory_cutoff_seq",
        "declared_launch_material_digest",
        "actual_launch_material_digest",
        "declared_launch_cwd_digest",
        "actual_launch_cwd_digest",
        "declared_launch_profile_digest",
        "actual_launch_profile_digest",
    }
    derived = {
        "accepted_set_change",
        "actual_environment_digest",
        "actual_input_manifest_digest",
        "actual_launch_binding_digest",
        "declared_environment_digest",
        "declared_input_manifest_digest",
        "environment_allowlist_digest",
        "schema_id",
        "source_fence_digest",
    }
    expected = direct | derived | {
        "actual_environment",
        "actual_input_manifest",
        "declared_environment",
        "declared_input_manifest",
        "environment_allowlist",
        "input_mode",
        "source_fence",
    }
    _exact_keys(value, frozenset(expected), "reproduction witness")
    result = CognitiveReproductionWitnessV1(
        source_fence=_parse_source_fence(_mapping(value["source_fence"], "source fence")),
        input_mode=ReproductionInputModeV1(value["input_mode"]),
        declared_input_manifest=tuple(
            _parse_artifact(_mapping(item, "declared input artifact"))
            for item in _sequence(value["declared_input_manifest"], "declared input manifest")
        ),
        actual_input_manifest=tuple(
            _parse_artifact(_mapping(item, "actual input artifact"))
            for item in _sequence(value["actual_input_manifest"], "actual input manifest")
        ),
        environment_allowlist=tuple(
            _text(item, "environment allowlist item")
            for item in _sequence(value["environment_allowlist"], "environment allowlist")
        ),
        declared_environment=tuple(
            _parse_environment(_mapping(item, "declared environment entry"))
            for item in _sequence(value["declared_environment"], "declared environment")
        ),
        actual_environment=tuple(
            _parse_environment(_mapping(item, "actual environment entry"))
            for item in _sequence(value["actual_environment"], "actual environment")
        ),
        **{name: value[name] for name in direct},
    )
    if not _canonical_equal(result.canonical_body(), value):
        raise ValueError("reproduction witness is not canonical")
    return result


def _parse_engine_identity(value: Mapping[str, Any]) -> RunPinnedEngineIdentityV1:
    constructor_fields = {
        "run_scope_digest",
        "launch_material_digest",
        "actual_launch_profile_digest",
        "driver_implementation_digest",
        "driver_build_digest",
        "driver_executable_token_digest",
        "engine_identity_digest",
        "provider_identity_digest",
        "model_identity_digest",
        "tool_policy_digest",
        "session_digest",
        "workspace_digest",
        "source_group_digest",
        "provider_attestation_receipt_digest",
    }
    _exact_keys(
        value,
        frozenset(constructor_fields | {"schema_id", "source_identity_fingerprint_digest"}),
        "engine identity",
    )
    result = RunPinnedEngineIdentityV1(**{name: value[name] for name in constructor_fields})
    if not _canonical_equal(result.canonical_body(), value):
        raise ValueError("engine identity is not canonical")
    return result


def _parse_check(value: Mapping[str, Any]) -> DeterministicCognitiveVerificationCheckV1:
    fields = {
        "accepted_set_change",
        "checker_version",
        "learning_eligible",
        "reason_codes",
        "relation",
        "reproduction_assignment_payload_digest",
        "reproduction_classification_body",
        "reproduction_observation_payload_digest",
        "reproduction_reproduction_kernel_digest",
        "schema_id",
        "source_assignment_payload_digest",
        "source_classification_body",
        "source_observation_payload_digest",
        "source_reproduction_kernel_digest",
    }
    _exact_keys(value, frozenset(fields), "deterministic check")
    result = DeterministicCognitiveVerificationCheckV1(
        relation=CognitiveVerificationRelationV1(value["relation"]),
        source_assignment_payload_digest=value["source_assignment_payload_digest"],
        source_observation_payload_digest=value["source_observation_payload_digest"],
        reproduction_assignment_payload_digest=value["reproduction_assignment_payload_digest"],
        reproduction_observation_payload_digest=value["reproduction_observation_payload_digest"],
        source_reproduction_kernel_digest=value["source_reproduction_kernel_digest"],
        reproduction_reproduction_kernel_digest=value["reproduction_reproduction_kernel_digest"],
        source_classification_body=value["source_classification_body"],
        reproduction_classification_body=value["reproduction_classification_body"],
        reason_codes=tuple(_sequence(value["reason_codes"], "checker reasons")),
        schema_id=value["schema_id"],
        checker_version=value["checker_version"],
        learning_eligible=value["learning_eligible"],
        accepted_set_change=value["accepted_set_change"],
    )
    if not _canonical_equal(result.canonical_body(), value):
        raise ValueError("deterministic check is not canonical")
    return result


__all__ = [
    "ACCEPTED_SET_CHANGE",
    "AUTOMATIC_REDISPATCH_PERMITTED",
    "COGNITIVE_ATTEMPT_ACCOUNTING_SCHEMA_ID",
    "COGNITIVE_ENGINE_REGISTRY_PROVENANCE_SCHEMA_ID",
    "COGNITIVE_VERIFICATION_CERTIFICATE_REDUCER_VERSION",
    "COGNITIVE_VERIFICATION_CERTIFICATE_SCHEMA_ID",
    "COGNITIVE_VERIFICATION_EVENT_REFERENCE_SCHEMA_ID",
    "COGNITIVE_VERIFICATION_PAYLOAD_OCCURRENCE_SCHEMA_ID",
    "COGNITIVE_REPRODUCTION_WITNESS_PROVENANCE_SCHEMA_ID",
    "DISPATCH_AUTHORITY",
    "LEARNING_AUTHORITY",
    "PRODUCTION_ENABLED",
    "STORE_WRITE_AUTHORITY",
    "CanonicalCognitiveEventReferenceV1",
    "CanonicalRuntimePayloadOccurrenceV1",
    "CognitiveAttemptAccountingRoleV1",
    "CognitiveAttemptAccountingStatusV1",
    "CognitiveEngineRegistryProvenanceV1",
    "CognitiveRegistryProvenanceGradeV1",
    "CognitiveReproductionWitnessProvenanceV1",
    "CognitiveVerificationCheckerGradeV1",
    "CognitiveVerificationCertificateStatusV1",
    "CognitiveVerificationCertificateV1",
    "ResolvedCognitiveAttemptAccountingV1",
    "reduce_cognitive_verification_certificate_v1",
    "reproduction_witness_actual_digest_v1",
    "reproduction_witness_declaration_digest_v1",
]
