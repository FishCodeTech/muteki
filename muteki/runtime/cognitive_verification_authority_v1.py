"""Default-off canonical checker execution and CHECKED authority.

The cognitive verifier is a real, separately admitted PURE attempt.  Its exact
input is committed before ``WORKER_LAUNCH_PREPARED``; its deterministic output is
sealed while the attempt is live; and ``COGNITIVE_VERIFICATION_CHECKED`` is emitted
only after one worker terminal and one budget terminal exist.  CHECKED is still not
learning authority.  A separate resolver must inspect its canonical receipt chain.
"""

from __future__ import annotations

from collections.abc import Mapping
from dataclasses import dataclass
from typing import Any

from muteki.epistemic.cas import ReceiptCAS
from muteki.epistemic.cognitive_events_v1 import (
    COGNITIVE_EXECUTION_OBSERVED,
    COGNITIVE_EXPERIMENT_ASSIGNED,
)
from muteki.epistemic.contracts import canonical_digest, canonical_json_bytes
from muteki.epistemic.sqlite_store import (
    CommandEvent,
    EpistemicSQLiteStore,
    IntegrityError,
    ProjectionMutation,
)
from muteki.runtime.contracts import AttemptPermit, EffectClass
from muteki.runtime.cognitive_verification_checker_v1 import (
    COGNITIVE_VERIFICATION_CHECKER_VERSION,
    DeterministicCognitiveVerificationCheckV1,
    check_cognitive_reproduction_v1,
)


COGNITIVE_VERIFICATION_CHECK_INPUT_COMMITTED = (
    "COGNITIVE_VERIFICATION_CHECK_INPUT_COMMITTED"
)
COGNITIVE_VERIFICATION_CHECK_OUTPUT_SEALED = (
    "COGNITIVE_VERIFICATION_CHECK_OUTPUT_SEALED"
)
COGNITIVE_VERIFICATION_CHECKED = "COGNITIVE_VERIFICATION_CHECKED"

COGNITIVE_VERIFICATION_CHECKER_ACTOR = "cognitive-verification-checker-authority"
COGNITIVE_VERIFICATION_CHECK_INPUT_SCHEMA_ID = (
    "muteki.cognitive-verification-check-input.v1"
)
COGNITIVE_VERIFICATION_CHECK_OUTPUT_SCHEMA_ID = (
    "muteki.cognitive-verification-check-output.v1"
)

PRODUCTION_ENABLED = False
ACCEPTED_SET_CHANGE = False
LEARNING_ELIGIBLE = False
AUTOMATIC_REDISPATCH_PERMITTED = False


def _text(value: object, name: str) -> str:
    if type(value) is not str or not value or value != value.strip():
        raise ValueError(f"{name} must be exact non-empty text")
    return value


def _digest(value: object, name: str) -> str:
    result = _text(value, name)
    if len(result) != 64 or any(
        character not in "0123456789abcdef" for character in result
    ):
        raise ValueError(f"{name} must be a lowercase sha256 digest")
    return result


def _row_for_digest(
    store: EpistemicSQLiteStore, *, kind: str, event_digest: str
) -> dict[str, Any]:
    matches = tuple(
        row
        for row in store.event_rows(kind=kind)
        if row["event_digest"] == event_digest
    )
    if len(matches) != 1:
        raise IntegrityError(f"{kind} occurrence is absent or ambiguous")
    return matches[0]


def _rows_for_attempt(
    store: EpistemicSQLiteStore, *, kinds: tuple[str, ...], attempt_id: str
) -> tuple[dict[str, Any], ...]:
    return tuple(
        row
        for kind in kinds
        for row in store.event_rows(kind=kind)
        if row["payload"].get("attempt_id") == attempt_id
    )


def _assignment_for_observation(
    store: EpistemicSQLiteStore, observation: Mapping[str, Any]
) -> dict[str, Any]:
    return _row_for_digest(
        store,
        kind=COGNITIVE_EXPERIMENT_ASSIGNED,
        event_digest=_digest(
            observation.get("assignment_event_digest"),
            "assignment_event_digest",
        ),
    )


_CHECK_INPUT_FIELDS = frozenset(
    {
        "accepted_set_change",
        "attempt_digest",
        "attempt_id",
        "automatic_redispatch_permitted",
        "checker_build_digest",
        "checker_implementation_digest",
        "checker_version",
        "learning_eligible",
        "permit_digest",
        "permit_id",
        "production_enabled",
        "reproduction_assignment_event_digest",
        "reproduction_assignment_event_receipt_digest",
        "reproduction_assignment_payload_digest",
        "reproduction_observation_event_digest",
        "reproduction_observation_event_receipt_digest",
        "reproduction_observation_payload_digest",
        "schema_id",
        "scope_digest",
        "source_assignment_event_digest",
        "source_assignment_event_receipt_digest",
        "source_assignment_payload_digest",
        "source_observation_event_digest",
        "source_observation_event_receipt_digest",
        "source_observation_payload_digest",
    }
)


_CHECK_OUTPUT_FIELDS = frozenset(
    {
        "accepted_set_change",
        "attempt_digest",
        "attempt_id",
        "automatic_redispatch_permitted",
        "byte_count",
        "check_body",
        "check_digest",
        "input_event_digest",
        "input_event_receipt_digest",
        "learning_eligible",
        "permit_digest",
        "permit_id",
        "production_enabled",
        "raw_digest",
        "schema_id",
        "scope_digest",
    }
)


def validate_cognitive_verification_check_input_shape(
    payload: Mapping[str, Any],
) -> None:
    if not isinstance(payload, Mapping) or set(payload) != _CHECK_INPUT_FIELDS:
        raise ValueError("cognitive verification check input is not versioned")
    if (
        payload["schema_id"] != COGNITIVE_VERIFICATION_CHECK_INPUT_SCHEMA_ID
        or payload["checker_version"] != COGNITIVE_VERIFICATION_CHECKER_VERSION
        or payload["production_enabled"] is not False
        or payload["accepted_set_change"] is not False
        or payload["learning_eligible"] is not False
        or payload["automatic_redispatch_permitted"] is not False
    ):
        raise ValueError("cognitive verification check input overclaims authority")
    for name in (
        "attempt_digest",
        "checker_build_digest",
        "checker_implementation_digest",
        "permit_digest",
        "reproduction_assignment_event_digest",
        "reproduction_assignment_event_receipt_digest",
        "reproduction_assignment_payload_digest",
        "reproduction_observation_event_digest",
        "reproduction_observation_event_receipt_digest",
        "reproduction_observation_payload_digest",
        "scope_digest",
        "source_assignment_event_digest",
        "source_assignment_event_receipt_digest",
        "source_assignment_payload_digest",
        "source_observation_event_digest",
        "source_observation_event_receipt_digest",
        "source_observation_payload_digest",
    ):
        _digest(payload[name], name)
    for name in ("attempt_id", "permit_id"):
        _text(payload[name], name)


def validate_cognitive_verification_check_output_shape(
    payload: Mapping[str, Any],
) -> None:
    if not isinstance(payload, Mapping) or set(payload) != _CHECK_OUTPUT_FIELDS:
        raise ValueError("cognitive verification check output is not versioned")
    if (
        payload["schema_id"] != COGNITIVE_VERIFICATION_CHECK_OUTPUT_SCHEMA_ID
        or payload["production_enabled"] is not False
        or payload["accepted_set_change"] is not False
        or payload["learning_eligible"] is not False
        or payload["automatic_redispatch_permitted"] is not False
    ):
        raise ValueError("cognitive verification check output overclaims authority")
    for name in (
        "attempt_digest",
        "check_digest",
        "input_event_digest",
        "input_event_receipt_digest",
        "permit_digest",
        "raw_digest",
        "scope_digest",
    ):
        _digest(payload[name], name)
    for name in ("attempt_id", "permit_id"):
        _text(payload[name], name)
    if type(payload["byte_count"]) is not int or payload["byte_count"] <= 0:
        raise ValueError("cognitive verification output byte_count is malformed")
    check = DeterministicCognitiveVerificationCheckV1.from_canonical(
        payload["check_body"]
    )
    if check.digest != payload["check_digest"]:
        raise ValueError("cognitive verification output check digest is false")


@dataclass(frozen=True, slots=True)
class CognitiveVerificationCheckInputRecordV1:
    event_digest: str
    event_receipt_digest: str
    payload: Mapping[str, Any]

    def __post_init__(self) -> None:
        _digest(self.event_digest, "event_digest")
        _digest(self.event_receipt_digest, "event_receipt_digest")
        validate_cognitive_verification_check_input_shape(self.payload)


@dataclass(frozen=True, slots=True)
class CognitiveVerificationCheckOutputRecordV1:
    event_digest: str
    event_receipt_digest: str
    payload: Mapping[str, Any]

    def __post_init__(self) -> None:
        _digest(self.event_digest, "event_digest")
        _digest(self.event_receipt_digest, "event_receipt_digest")
        validate_cognitive_verification_check_output_shape(self.payload)

    @property
    def check(self) -> DeterministicCognitiveVerificationCheckV1:
        return DeterministicCognitiveVerificationCheckV1.from_canonical(
            self.payload["check_body"]
        )


@dataclass(frozen=True, slots=True)
class CognitiveVerificationCheckedRecordV1:
    event_digest: str
    event_receipt_digest: str
    checker_attempt_id: str
    scope_digest: str
    check: DeterministicCognitiveVerificationCheckV1

    def __post_init__(self) -> None:
        _digest(self.event_digest, "event_digest")
        _digest(self.event_receipt_digest, "event_receipt_digest")
        _text(self.checker_attempt_id, "checker_attempt_id")
        _digest(self.scope_digest, "scope_digest")
        if type(self.check) is not DeterministicCognitiveVerificationCheckV1:
            raise TypeError("check must be a deterministic verification check")

    @property
    def learning_eligible(self) -> bool:
        return False


class CognitiveVerificationCheckerAuthorityV1:
    """Checker-only authority; it cannot resolve or update beliefs."""

    def __init__(self, *, store: EpistemicSQLiteStore, cas: ReceiptCAS) -> None:
        if type(store) is not EpistemicSQLiteStore:
            raise TypeError("store must be exactly EpistemicSQLiteStore")
        if not isinstance(cas, ReceiptCAS):
            raise TypeError("cas must be ReceiptCAS")
        self._store = store
        self._cas = cas

    def commit_input(
        self,
        *,
        permit: AttemptPermit,
        source_observation_event_digest: str,
        reproduction_observation_event_digest: str,
        checker_implementation_digest: str,
        checker_build_digest: str,
        occurred_at_ns: int,
    ) -> CognitiveVerificationCheckInputRecordV1:
        if type(permit) is not AttemptPermit:
            raise TypeError("permit must be exact AttemptPermit")
        if permit.effect_class is not EffectClass.PURE:
            raise IntegrityError("verification checker attempt must be PURE")
        if type(occurred_at_ns) is not int or occurred_at_ns < 0:
            raise ValueError("occurred_at_ns must be non-negative")
        source_observation = _row_for_digest(
            self._store,
            kind=COGNITIVE_EXECUTION_OBSERVED,
            event_digest=_digest(
                source_observation_event_digest,
                "source_observation_event_digest",
            ),
        )
        reproduction_observation = _row_for_digest(
            self._store,
            kind=COGNITIVE_EXECUTION_OBSERVED,
            event_digest=_digest(
                reproduction_observation_event_digest,
                "reproduction_observation_event_digest",
            ),
        )
        source_assignment = _assignment_for_observation(
            self._store, source_observation["payload"]
        )
        reproduction_assignment = _assignment_for_observation(
            self._store, reproduction_observation["payload"]
        )
        if (
            reproduction_assignment["payload"].get(
                "source_observation_event_digest"
            )
            != source_observation["event_digest"]
            or source_observation["payload"].get("scope_digest")
            != permit.lease.attempt.scope.digest
            or reproduction_observation["payload"].get("scope_digest")
            != permit.lease.attempt.scope.digest
        ):
            raise IntegrityError("checker input crosses reproduction or run scope")
        if _rows_for_attempt(
            self._store,
            kinds=("WORKER_LAUNCH_PREPARED", "WORKER_TERMINAL", "WORKER_UNKNOWN"),
            attempt_id=permit.lease.attempt.attempt_id,
        ):
            raise IntegrityError("checker input must be committed before launch")

        def receipt(row: Mapping[str, Any]) -> str:
            return self._store.resolve_receipt_for_event(row["event_digest"]).digest

        payload = {
            "accepted_set_change": False,
            "attempt_digest": permit.lease.attempt.digest,
            "attempt_id": permit.lease.attempt.attempt_id,
            "automatic_redispatch_permitted": False,
            "checker_build_digest": _digest(
                checker_build_digest, "checker_build_digest"
            ),
            "checker_implementation_digest": _digest(
                checker_implementation_digest, "checker_implementation_digest"
            ),
            "checker_version": COGNITIVE_VERIFICATION_CHECKER_VERSION,
            "learning_eligible": False,
            "permit_digest": permit.digest,
            "permit_id": permit.permit_id,
            "production_enabled": False,
            "reproduction_assignment_event_digest": reproduction_assignment[
                "event_digest"
            ],
            "reproduction_assignment_event_receipt_digest": receipt(
                reproduction_assignment
            ),
            "reproduction_assignment_payload_digest": canonical_digest(
                reproduction_assignment["payload"]
            ),
            "reproduction_observation_event_digest": reproduction_observation[
                "event_digest"
            ],
            "reproduction_observation_event_receipt_digest": receipt(
                reproduction_observation
            ),
            "reproduction_observation_payload_digest": canonical_digest(
                reproduction_observation["payload"]
            ),
            "schema_id": COGNITIVE_VERIFICATION_CHECK_INPUT_SCHEMA_ID,
            "scope_digest": permit.lease.attempt.scope.digest,
            "source_assignment_event_digest": source_assignment["event_digest"],
            "source_assignment_event_receipt_digest": receipt(source_assignment),
            "source_assignment_payload_digest": canonical_digest(
                source_assignment["payload"]
            ),
            "source_observation_event_digest": source_observation["event_digest"],
            "source_observation_event_receipt_digest": receipt(source_observation),
            "source_observation_payload_digest": canonical_digest(
                source_observation["payload"]
            ),
        }
        validate_cognitive_verification_check_input_shape(payload)
        result = self._store.commit_command(
            command_id=f"cognitive-verification-check-input:{permit.permit_id}",
            idempotency_key=f"cognitive-verification-check-input:{permit.permit_id}",
            command_payload=payload,
            events=(
                CommandEvent(
                    event_id=(
                        "event:cognitive-verification-check-input:"
                        f"{permit.permit_id}"
                    ),
                    kind=COGNITIVE_VERIFICATION_CHECK_INPUT_COMMITTED,
                    actor=COGNITIVE_VERIFICATION_CHECKER_ACTOR,
                    occurred_at_ns=occurred_at_ns,
                    payload=payload,
                ),
            ),
            projection_mutations=(
                ProjectionMutation(
                    "cognitive_verification_check_input_guard", payload
                ),
            ),
            authority_capability=(
                self._store._cognitive_verification_checker_commit_capability
            ),
            committed_at_ns=occurred_at_ns,
        )
        row = self._store.event_rows(
            kind=COGNITIVE_VERIFICATION_CHECK_INPUT_COMMITTED
        )[-1]
        return CognitiveVerificationCheckInputRecordV1(
            event_digest=row["event_digest"],
            event_receipt_digest=result.receipt_digest,
            payload=row["payload"],
        )

    def execute_committed_input(
        self,
        *,
        permit: AttemptPermit,
        input_event_digest: str,
        occurred_at_ns: int,
    ) -> CognitiveVerificationCheckOutputRecordV1:
        if type(permit) is not AttemptPermit:
            raise TypeError("permit must be exact AttemptPermit")
        if type(occurred_at_ns) is not int or occurred_at_ns < 0:
            raise ValueError("occurred_at_ns must be non-negative")
        input_row = _row_for_digest(
            self._store,
            kind=COGNITIVE_VERIFICATION_CHECK_INPUT_COMMITTED,
            event_digest=_digest(input_event_digest, "input_event_digest"),
        )
        payload = input_row["payload"]
        validate_cognitive_verification_check_input_shape(payload)
        if (
            payload["attempt_digest"] != permit.lease.attempt.digest
            or payload["permit_digest"] != permit.digest
        ):
            raise IntegrityError("checker input belongs to another permit")
        launches = _rows_for_attempt(
            self._store,
            kinds=("WORKER_LAUNCH_PREPARED",),
            attempt_id=permit.lease.attempt.attempt_id,
        )
        terminals = _rows_for_attempt(
            self._store,
            kinds=("WORKER_TERMINAL", "WORKER_UNKNOWN"),
            attempt_id=permit.lease.attempt.attempt_id,
        )
        if len(launches) != 1 or terminals:
            raise IntegrityError("checker output requires one live checker launch")
        source_assignment = _row_for_digest(
            self._store,
            kind=COGNITIVE_EXPERIMENT_ASSIGNED,
            event_digest=payload["source_assignment_event_digest"],
        )
        source_observation = _row_for_digest(
            self._store,
            kind=COGNITIVE_EXECUTION_OBSERVED,
            event_digest=payload["source_observation_event_digest"],
        )
        reproduction_assignment = _row_for_digest(
            self._store,
            kind=COGNITIVE_EXPERIMENT_ASSIGNED,
            event_digest=payload["reproduction_assignment_event_digest"],
        )
        reproduction_observation = _row_for_digest(
            self._store,
            kind=COGNITIVE_EXECUTION_OBSERVED,
            event_digest=payload["reproduction_observation_event_digest"],
        )
        check = check_cognitive_reproduction_v1(
            source_assignment_payload=source_assignment["payload"],
            source_observation_payload=source_observation["payload"],
            reproduction_assignment_payload=reproduction_assignment["payload"],
            reproduction_observation_payload=reproduction_observation["payload"],
            cas=self._cas,
        )
        raw = canonical_json_bytes(check.canonical_body())
        sealed = self._cas.seal_bytes(raw)
        output_payload = {
            "accepted_set_change": False,
            "attempt_digest": permit.lease.attempt.digest,
            "attempt_id": permit.lease.attempt.attempt_id,
            "automatic_redispatch_permitted": False,
            "byte_count": sealed.byte_count,
            "check_body": check.canonical_body(),
            "check_digest": check.digest,
            "input_event_digest": input_row["event_digest"],
            "input_event_receipt_digest": self._store.resolve_receipt_for_event(
                input_row["event_digest"]
            ).digest,
            "learning_eligible": False,
            "permit_digest": permit.digest,
            "permit_id": permit.permit_id,
            "production_enabled": False,
            "raw_digest": sealed.digest,
            "schema_id": COGNITIVE_VERIFICATION_CHECK_OUTPUT_SCHEMA_ID,
            "scope_digest": permit.lease.attempt.scope.digest,
        }
        validate_cognitive_verification_check_output_shape(output_payload)
        result = self._store.commit_command(
            command_id=f"cognitive-verification-check-output:{permit.permit_id}",
            idempotency_key=f"cognitive-verification-check-output:{permit.permit_id}",
            command_payload=output_payload,
            events=(
                CommandEvent(
                    event_id=(
                        "event:cognitive-verification-check-output:"
                        f"{permit.permit_id}"
                    ),
                    kind=COGNITIVE_VERIFICATION_CHECK_OUTPUT_SEALED,
                    actor=COGNITIVE_VERIFICATION_CHECKER_ACTOR,
                    occurred_at_ns=occurred_at_ns,
                    payload=output_payload,
                ),
            ),
            projection_mutations=(
                ProjectionMutation(
                    "cognitive_verification_check_output_guard", output_payload
                ),
            ),
            authority_capability=(
                self._store._cognitive_verification_checker_commit_capability
            ),
            committed_at_ns=occurred_at_ns,
        )
        row = self._store.event_rows(
            kind=COGNITIVE_VERIFICATION_CHECK_OUTPUT_SEALED
        )[-1]
        return CognitiveVerificationCheckOutputRecordV1(
            event_digest=row["event_digest"],
            event_receipt_digest=result.receipt_digest,
            payload=row["payload"],
        )

    def finalize_checked(
        self,
        *,
        permit: AttemptPermit,
        occurred_at_ns: int,
    ) -> CognitiveVerificationCheckedRecordV1:
        if type(permit) is not AttemptPermit:
            raise TypeError("permit must be exact AttemptPermit")
        attempt_id = permit.lease.attempt.attempt_id
        inputs = _rows_for_attempt(
            self._store,
            kinds=(COGNITIVE_VERIFICATION_CHECK_INPUT_COMMITTED,),
            attempt_id=attempt_id,
        )
        outputs = _rows_for_attempt(
            self._store,
            kinds=(COGNITIVE_VERIFICATION_CHECK_OUTPUT_SEALED,),
            attempt_id=attempt_id,
        )
        terminals = _rows_for_attempt(
            self._store,
            kinds=("WORKER_TERMINAL", "WORKER_UNKNOWN"),
            attempt_id=attempt_id,
        )
        budgets = _rows_for_attempt(
            self._store,
            kinds=("BUDGET_SETTLED", "BUDGET_USAGE_UNKNOWN"),
            attempt_id=attempt_id,
        )
        if any(len(items) != 1 for items in (inputs, outputs, terminals, budgets)):
            raise IntegrityError(
                "CHECKED requires unique input, output, terminal, and budget closure"
            )
        output_payload = outputs[0]["payload"]
        validate_cognitive_verification_check_output_shape(output_payload)
        raw = self._cas.read_verified(output_payload["raw_digest"])
        if len(raw) != output_payload["byte_count"]:
            raise IntegrityError("checker output CAS byte count diverged")
        check = DeterministicCognitiveVerificationCheckV1.from_canonical(
            output_payload["check_body"]
        )
        if raw != canonical_json_bytes(check.canonical_body()):
            raise IntegrityError("checker output CAS body diverged")
        if not (
            inputs[0]["seq"] < outputs[0]["seq"] < terminals[0]["seq"] < budgets[0]["seq"]
        ):
            raise IntegrityError("checker lifecycle order is not canonical")
        result = self._store.commit_command(
            command_id=f"cognitive-verification-checked:{check.digest}",
            idempotency_key=f"cognitive-verification-checked:{check.digest}",
            command_payload=check.canonical_body(),
            events=(
                CommandEvent(
                    event_id=f"event:cognitive-verification-checked:{check.digest}",
                    kind=COGNITIVE_VERIFICATION_CHECKED,
                    actor=COGNITIVE_VERIFICATION_CHECKER_ACTOR,
                    occurred_at_ns=occurred_at_ns,
                    payload=check.canonical_body(),
                ),
            ),
            projection_mutations=(
                ProjectionMutation(
                    "cognitive_verification_checked_guard", check.canonical_body()
                ),
            ),
            authority_capability=(
                self._store._cognitive_verification_checker_commit_capability
            ),
            forbid_prior_events=(
                (
                    COGNITIVE_VERIFICATION_CHECKED,
                    {
                        "source_observation_payload_digest": (
                            check.source_observation_payload_digest
                        ),
                        "reproduction_observation_payload_digest": (
                            check.reproduction_observation_payload_digest
                        ),
                    },
                ),
            ),
            committed_at_ns=occurred_at_ns,
        )
        row = self._store.event_rows(kind=COGNITIVE_VERIFICATION_CHECKED)[-1]
        return CognitiveVerificationCheckedRecordV1(
            event_digest=row["event_digest"],
            event_receipt_digest=result.receipt_digest,
            checker_attempt_id=attempt_id,
            scope_digest=permit.lease.attempt.scope.digest,
            check=check,
        )


def validate_cognitive_verification_check_input_against_store(
    store: EpistemicSQLiteStore, payload: Mapping[str, Any]
) -> None:
    """Store semantic guard used only by the checker input capability."""

    validate_cognitive_verification_check_input_shape(payload)
    admissions = _rows_for_attempt(
        store, kinds=("ATTEMPT_ADMITTED",), attempt_id=payload["attempt_id"]
    )
    launches = _rows_for_attempt(
        store,
        kinds=("WORKER_LAUNCH_PREPARED", "WORKER_TERMINAL", "WORKER_UNKNOWN"),
        attempt_id=payload["attempt_id"],
    )
    if len(admissions) != 1 or launches:
        raise IntegrityError("checker input must bind one unlaunched admission")
    admission = admissions[0]["payload"]
    if (
        admission.get("attempt_digest") != payload["attempt_digest"]
        or admission.get("permit_digest") != payload["permit_digest"]
        or admission.get("permit_id") != payload["permit_id"]
        or admission.get("scope_digest") != payload["scope_digest"]
        or admission.get("effect_class") != EffectClass.PURE.value
    ):
        raise IntegrityError("checker input admission binding diverged")
    for prefix, kind in (
        ("source_assignment", COGNITIVE_EXPERIMENT_ASSIGNED),
        ("source_observation", COGNITIVE_EXECUTION_OBSERVED),
        ("reproduction_assignment", COGNITIVE_EXPERIMENT_ASSIGNED),
        ("reproduction_observation", COGNITIVE_EXECUTION_OBSERVED),
    ):
        row = _row_for_digest(
            store,
            kind=kind,
            event_digest=payload[f"{prefix}_event_digest"],
        )
        if (
            canonical_digest(row["payload"])
            != payload[f"{prefix}_payload_digest"]
            or store.resolve_receipt_for_event(row["event_digest"]).digest
            != payload[f"{prefix}_event_receipt_digest"]
            or row["seq"] >= store.state().head_seq + 1
        ):
            raise IntegrityError("checker input predecessor binding diverged")


def validate_cognitive_verification_check_output_against_store(
    store: EpistemicSQLiteStore, payload: Mapping[str, Any]
) -> None:
    validate_cognitive_verification_check_output_shape(payload)
    inputs = tuple(
        row
        for row in store.event_rows(
            kind=COGNITIVE_VERIFICATION_CHECK_INPUT_COMMITTED
        )
        if row["event_digest"] == payload["input_event_digest"]
    )
    launches = _rows_for_attempt(
        store, kinds=("WORKER_LAUNCH_PREPARED",), attempt_id=payload["attempt_id"]
    )
    terminals = _rows_for_attempt(
        store,
        kinds=("WORKER_TERMINAL", "WORKER_UNKNOWN"),
        attempt_id=payload["attempt_id"],
    )
    if len(inputs) != 1 or len(launches) != 1 or terminals:
        raise IntegrityError("checker output is outside one live checker attempt")
    if (
        inputs[0]["payload"].get("attempt_digest") != payload["attempt_digest"]
        or inputs[0]["payload"].get("permit_digest") != payload["permit_digest"]
        or store.resolve_receipt_for_event(inputs[0]["event_digest"]).digest
        != payload["input_event_receipt_digest"]
        or not (inputs[0]["seq"] < launches[0]["seq"])
    ):
        raise IntegrityError("checker output input/launch lineage diverged")


def validate_cognitive_verification_checked_against_store(
    store: EpistemicSQLiteStore, payload: Mapping[str, Any]
) -> None:
    check = DeterministicCognitiveVerificationCheckV1.from_canonical(payload)
    outputs = tuple(
        row
        for row in store.event_rows(
            kind=COGNITIVE_VERIFICATION_CHECK_OUTPUT_SEALED
        )
        if row["payload"].get("check_digest") == check.digest
    )
    if len(outputs) != 1:
        raise IntegrityError("CHECKED has no unique sealed checker output")
    attempt_id = outputs[0]["payload"]["attempt_id"]
    terminals = _rows_for_attempt(
        store,
        kinds=("WORKER_TERMINAL", "WORKER_UNKNOWN"),
        attempt_id=attempt_id,
    )
    budgets = _rows_for_attempt(
        store,
        kinds=("BUDGET_SETTLED", "BUDGET_USAGE_UNKNOWN"),
        attempt_id=attempt_id,
    )
    if len(terminals) != 1 or len(budgets) != 1:
        raise IntegrityError("CHECKED lacks unique terminal accounting")
    if (
        canonical_json_bytes(outputs[0]["payload"]["check_body"])
        != canonical_json_bytes(check.canonical_body())
        or not (outputs[0]["seq"] < terminals[0]["seq"] < budgets[0]["seq"])
    ):
        raise IntegrityError("CHECKED output/accounting lineage diverged")


__all__ = [
    "ACCEPTED_SET_CHANGE",
    "AUTOMATIC_REDISPATCH_PERMITTED",
    "COGNITIVE_VERIFICATION_CHECKED",
    "COGNITIVE_VERIFICATION_CHECKER_ACTOR",
    "COGNITIVE_VERIFICATION_CHECK_INPUT_COMMITTED",
    "COGNITIVE_VERIFICATION_CHECK_INPUT_SCHEMA_ID",
    "COGNITIVE_VERIFICATION_CHECK_OUTPUT_SCHEMA_ID",
    "COGNITIVE_VERIFICATION_CHECK_OUTPUT_SEALED",
    "LEARNING_ELIGIBLE",
    "PRODUCTION_ENABLED",
    "CognitiveVerificationCheckInputRecordV1",
    "CognitiveVerificationCheckOutputRecordV1",
    "CognitiveVerificationCheckedRecordV1",
    "CognitiveVerificationCheckerAuthorityV1",
    "validate_cognitive_verification_check_input_against_store",
    "validate_cognitive_verification_check_input_shape",
    "validate_cognitive_verification_check_output_against_store",
    "validate_cognitive_verification_check_output_shape",
    "validate_cognitive_verification_checked_against_store",
]
