"""EffectOperation + ordinal EffectAttempt with conservative UNKNOWN holds."""

from __future__ import annotations

from collections.abc import Sequence

from muteki.epistemic.sqlite_store import (
    CommandEvent,
    EFFECT_LEGAL_TRANSITIONS,
    EpistemicSQLiteStore,
    ProjectionMutation,
    require_positive_effect_revision,
)
from muteki.runtime.contracts import EffectClass


def _require_legal_effect_transition(from_state: str, to_state: str) -> None:
    allowed = EFFECT_LEGAL_TRANSITIONS.get(from_state, frozenset())
    if to_state not in allowed:
        raise ValueError(
            f"illegal effect transition {from_state!r} -> {to_state!r}"
        )


class EffectLedger:
    def __init__(self, store: EpistemicSQLiteStore) -> None:
        self._store = store

    def prepare(self, *, operation_id: str, attempt_id: str,
                effect_class: EffectClass, conflict_keys: Sequence[str],
                occurred_at_ns: int) -> str:
        payload = {"operation_id": operation_id, "attempt_id": attempt_id,
                   "effect_class": effect_class.value,
                   "conflict_keys": tuple(conflict_keys)}
        result = self._store.commit_command(
            command_id=f"effect:prepare:{operation_id}",
            idempotency_key=f"effect:prepare:{operation_id}",
            command_payload=payload,
            events=[CommandEvent(
                f"event:effect:prepare:{operation_id}", "EFFECT_PREPARED",
                "effect-ledger", occurred_at_ns, payload)],
            projection_mutations=[ProjectionMutation("effect_prepare", payload)],
            committed_at_ns=occurred_at_ns,
        )
        return result.receipt_digest

    def transition(self, *, operation_id: str, expected_state: str,
                   new_state: str, revision: int, occurred_at_ns: int) -> str:
        require_positive_effect_revision(revision)
        _require_legal_effect_transition(expected_state, new_state)
        if expected_state == "unknown":
            raise ValueError(
                "UNKNOWN effect reconciliation requires an independent "
                "observer receipt"
            )
        payload = {"operation_id": operation_id, "expected_state": expected_state,
                   "new_state": new_state, "revision": revision}
        result = self._store.commit_command(
            command_id=f"effect:transition:{operation_id}:{revision}",
            idempotency_key=f"effect:transition:{operation_id}:{revision}",
            command_payload=payload,
            events=[CommandEvent(
                f"event:effect:transition:{operation_id}:{revision}",
                "EFFECT_" + new_state.upper(), "effect-ledger",
                occurred_at_ns, payload)],
            projection_mutations=[ProjectionMutation("effect_transition", payload)],
            committed_at_ns=occurred_at_ns,
        )
        return result.receipt_digest

    def retry_confirmed_not_applied(self, *, operation_id: str, revision: int,
                                    occurred_at_ns: int) -> str:
        require_positive_effect_revision(revision)
        del operation_id, occurred_at_ns
        raise ValueError(
            "effect retry requires a fresh admitted attempt, permit, policy, "
            "and budget reservation"
        )
