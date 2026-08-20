"""Per-run append-only SQLite control journal.

The journal records commands, delivery/effect receipts, desired-state
generations, operator context, and decision requests.  No row is updated or
deleted by this module.  Materialized current state is obtained by folding the
latest append-only row.
"""

from __future__ import annotations

import hashlib
import json
import sqlite3
import threading
import time
import uuid
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Optional

from muteki.control.models import (
    ContextResource,
    DecisionAnswer,
    DecisionRequest,
    DecisionStatus,
    EFFECT_TRANSITIONS,
    EffectReceipt,
    EffectState,
    RunControlMode,
    RunControlState,
    ControlCommand,
)


_SCHEMA = """
CREATE TABLE IF NOT EXISTS commands (
    seq          INTEGER PRIMARY KEY AUTOINCREMENT,
    command_id   TEXT NOT NULL UNIQUE,
    run_id       TEXT NOT NULL,
    payload_hash TEXT NOT NULL,
    command_json TEXT NOT NULL,
    created_at   REAL NOT NULL
);
CREATE TABLE IF NOT EXISTS effects (
    seq          INTEGER PRIMARY KEY AUTOINCREMENT,
    receipt_id   TEXT NOT NULL UNIQUE,
    command_id   TEXT NOT NULL,
    run_id       TEXT NOT NULL,
    state        TEXT NOT NULL,
    payload_hash TEXT NOT NULL,
    receipt_json TEXT NOT NULL,
    created_at   REAL NOT NULL,
    FOREIGN KEY(command_id) REFERENCES commands(command_id)
);
CREATE INDEX IF NOT EXISTS idx_control_effects_command
    ON effects(command_id, seq);
CREATE TABLE IF NOT EXISTS state_events (
    seq          INTEGER PRIMARY KEY AUTOINCREMENT,
    run_id       TEXT NOT NULL,
    generation   INTEGER NOT NULL UNIQUE,
    command_id   TEXT,
    state_json   TEXT NOT NULL,
    created_at   REAL NOT NULL
);
CREATE TABLE IF NOT EXISTS context_events (
    seq          INTEGER PRIMARY KEY AUTOINCREMENT,
    event_id     TEXT NOT NULL UNIQUE,
    run_id       TEXT NOT NULL,
    context_id   TEXT NOT NULL,
    operation    TEXT NOT NULL,
    payload_hash TEXT NOT NULL,
    payload_json TEXT NOT NULL,
    created_at   REAL NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_control_context_id
    ON context_events(context_id, seq);
CREATE TABLE IF NOT EXISTS decision_requests (
    seq          INTEGER PRIMARY KEY AUTOINCREMENT,
    request_id   TEXT NOT NULL UNIQUE,
    run_id       TEXT NOT NULL,
    payload_hash TEXT NOT NULL,
    request_json TEXT NOT NULL,
    created_at   REAL NOT NULL
);
CREATE TABLE IF NOT EXISTS decision_answers (
    seq          INTEGER PRIMARY KEY AUTOINCREMENT,
    answer_id    TEXT NOT NULL UNIQUE,
    request_id   TEXT NOT NULL,
    run_id       TEXT NOT NULL,
    status       TEXT NOT NULL,
    payload_hash TEXT NOT NULL,
    answer_json  TEXT NOT NULL,
    created_at   REAL NOT NULL,
    FOREIGN KEY(request_id) REFERENCES decision_requests(request_id)
);
CREATE INDEX IF NOT EXISTS idx_control_decision_answers
    ON decision_answers(request_id, seq);
"""


class ControlJournalError(RuntimeError):
    pass


class IdempotencyConflict(ControlJournalError):
    pass


class InvalidEffectTransition(ControlJournalError):
    pass


class StateConflict(ControlJournalError):
    pass


@dataclass(frozen=True)
class AppendCommandResult:
    command: ControlCommand
    inserted: bool
    accepted: bool
    receipts: tuple[EffectReceipt, ...]

    @property
    def latest_receipt(self) -> EffectReceipt:
        return self.receipts[-1]


@dataclass(frozen=True)
class StandingClearOperation:
    """Durable, replayable retraction of standing operator guidance.

    The control journal and shared graph intentionally live in separate SQLite
    databases, so a clear command is an outbox record until the graph stores its
    own idempotency marker. ``cutoff_before`` is the clear command's own durable
    persistence fence. It prevents recovery from retracting source-less legacy
    guidance with unknown ordering. Typed guidance is
    ordered without clocks through the closed
    ``eligible_standing_command_ids`` set: unknown/new source ids are preserved.
    """

    command_id: str
    action: str
    actor: str
    text: str
    persisted_at: float
    cutoff_before: Optional[float] = None
    eligible_standing_command_ids: tuple[str, ...] = ()


def _hash_json(value: Any) -> str:
    raw = json.dumps(value, ensure_ascii=False, sort_keys=True,
                     separators=(",", ":"), default=str)
    return hashlib.sha256(raw.encode("utf-8")).hexdigest()


class SQLiteControlJournal:
    """One local SQLite journal for one run.

    Writes are thread-safe for defensive use, but the intended production owner
    is one ``ControlActor``.  WAL permits observers to inspect state without
    delaying command admission.
    """

    def __init__(self, db_path: str | Path, *, run_id: str) -> None:
        run_id = str(run_id).strip()
        if not run_id:
            raise ValueError("run_id cannot be empty")
        self.db_path = str(db_path)
        self.run_id = run_id
        self._created_at = time.time()
        Path(self.db_path).parent.mkdir(parents=True, exist_ok=True)
        self._conn = sqlite3.connect(self.db_path, check_same_thread=False)
        self._lock = threading.Lock()
        cur = self._conn.cursor()
        cur.execute("PRAGMA journal_mode=WAL")
        cur.execute("PRAGMA busy_timeout=5000")
        cur.execute("PRAGMA synchronous=NORMAL")
        cur.execute("PRAGMA foreign_keys=ON")
        self._conn.executescript(_SCHEMA)
        self._conn.commit()

    @classmethod
    def open(cls, *, db_path: str | Path, run_id: str) -> "SQLiteControlJournal":
        return cls(db_path, run_id=run_id)

    def close(self) -> None:
        with self._lock:
            self._conn.close()

    def __enter__(self) -> "SQLiteControlJournal":
        return self

    def __exit__(self, *_args: Any) -> None:
        self.close()

    def _assert_run(self, run_id: str) -> None:
        if run_id != self.run_id:
            raise ControlJournalError(
                f"record run {run_id!r} does not match journal run {self.run_id!r}"
            )

    def _current_state_unlocked(self) -> RunControlState:
        row = self._conn.execute(
            "SELECT state_json FROM state_events WHERE run_id=? "
            "ORDER BY generation DESC LIMIT 1",
            (self.run_id,),
        ).fetchone()
        if row is None:
            return RunControlState(run_id=self.run_id, updated_at=self._created_at)
        return RunControlState.model_validate_json(row[0])

    def current_state(self) -> RunControlState:
        with self._lock:
            return self._current_state_unlocked()

    def state_history(self) -> list[RunControlState]:
        with self._lock:
            rows = self._conn.execute(
                "SELECT state_json FROM state_events WHERE run_id=? ORDER BY generation",
                (self.run_id,),
            ).fetchall()
        return [RunControlState.model_validate_json(row[0]) for row in rows]

    def _latest_effect_unlocked(self, command_id: str) -> Optional[EffectReceipt]:
        row = self._conn.execute(
            "SELECT receipt_json FROM effects WHERE command_id=? ORDER BY seq DESC LIMIT 1",
            (command_id,),
        ).fetchone()
        return EffectReceipt.model_validate_json(row[0]) if row else None

    def latest_effect(self, command_id: str) -> Optional[EffectReceipt]:
        with self._lock:
            return self._latest_effect_unlocked(command_id)

    def effect_history(self, command_id: str) -> list[EffectReceipt]:
        with self._lock:
            rows = self._conn.execute(
                "SELECT receipt_json FROM effects WHERE command_id=? ORDER BY seq",
                (command_id,),
            ).fetchall()
        return [EffectReceipt.model_validate_json(row[0]) for row in rows]

    def get_command(self, command_id: str) -> Optional[ControlCommand]:
        with self._lock:
            row = self._conn.execute(
                "SELECT command_json FROM commands WHERE command_id=?", (command_id,)
            ).fetchone()
        return ControlCommand.model_validate_json(row[0]) if row else None

    def command_history(self) -> list[ControlCommand]:
        with self._lock:
            rows = self._conn.execute(
                "SELECT command_json FROM commands WHERE run_id=? ORDER BY seq",
                (self.run_id,),
            ).fetchall()
        return [ControlCommand.model_validate_json(row[0]) for row in rows]

    def standing_clear_operations(self) -> list[StandingClearOperation]:
        """Return accepted CLEAR/RESET commands as a deterministic graph outbox.

        ``PERSISTED`` is the durable admission fence. A crash may happen directly
        after that receipt, after typed-context tombstones, or after the graph
        write; all three states are safe to replay because graph application is
        monotonic and keyed by ``command_id``. Commands rejected by CAS/admission
        never have a PERSISTED receipt and are therefore excluded.

        The clear's persisted receipt is a stable causal upper bound for legacy
        rows. Command ids provide exact ordering for typed resources; timestamps
        are retained only as a conservative compatibility fence for rows without
        source metadata.
        """
        with self._lock:
            rows = self._conn.execute(
                "SELECT c.seq, c.command_json, e.receipt_json "
                "FROM commands c JOIN effects e ON e.command_id=c.command_id "
                "WHERE c.run_id=? AND e.state=? ORDER BY c.seq",
                (self.run_id, EffectState.PERSISTED.value),
            ).fetchall()

        persisted: list[tuple[ControlCommand, EffectReceipt]] = []
        for _seq, command_json, receipt_json in rows:
            persisted.append((
                ControlCommand.model_validate_json(command_json),
                EffectReceipt.model_validate_json(receipt_json),
            ))

        def _creates_standing(command: ControlCommand) -> bool:
            payload = command.payload
            if command.action.value == "add_context":
                raw_context = payload.get("context")
                return (
                    isinstance(raw_context, dict)
                    and bool(raw_context.get("standing", False))
                )
            return (
                command.action.value in {
                    "ask", "hint", "focus", "redirect", "directive", "correction",
                }
                and bool(payload.get("standing", False))
            )

        operations: list[StandingClearOperation] = []
        eligible_standing: list[str] = []
        for command, receipt in persisted:
            if command.action.value in {"clear_standing", "reset_guidance"}:
                payload = command.payload
                exact_text = str(
                    payload.get("text") or payload.get("hint") or ""
                ).strip()
                matched = receipt.metadata.get("matched_source_command_ids")
                closed_set = (
                    matched if (exact_text
                                and isinstance(matched, (list, tuple)))
                    else eligible_standing
                )
                operations.append(StandingClearOperation(
                    command_id=command.command_id,
                    action=command.action.value,
                    actor=command.actor,
                    text=exact_text,
                    persisted_at=float(receipt.created_at),
                    cutoff_before=float(receipt.created_at),
                    eligible_standing_command_ids=tuple(
                        str(value) for value in closed_set
                        if str(value).strip()
                    ),
                ))
            if _creates_standing(command):
                eligible_standing.append(command.command_id)
        return operations

    def _insert_effect_unlocked(self, receipt: EffectReceipt) -> EffectReceipt:
        existing = self._conn.execute(
            "SELECT payload_hash, receipt_json FROM effects WHERE receipt_id=?",
            (receipt.receipt_id,),
        ).fetchone()
        if existing is not None:
            if existing[0] != receipt.semantic_hash():
                raise IdempotencyConflict(
                    f"receipt_id {receipt.receipt_id!r} was reused with different content"
                )
            return EffectReceipt.model_validate_json(existing[1])

        command_row = self._conn.execute(
            "SELECT 1 FROM commands WHERE command_id=? AND run_id=?",
            (receipt.command_id, self.run_id),
        ).fetchone()
        if command_row is None:
            raise ControlJournalError(f"unknown command_id {receipt.command_id!r}")
        previous = self._latest_effect_unlocked(receipt.command_id)
        previous_state = previous.state if previous is not None else None
        if receipt.state not in EFFECT_TRANSITIONS[previous_state]:
            raise InvalidEffectTransition(
                f"invalid effect transition {previous_state!r} -> {receipt.state.value!r}"
            )
        self._conn.execute(
            "INSERT INTO effects "
            "(receipt_id, command_id, run_id, state, payload_hash, receipt_json, created_at) "
            "VALUES (?,?,?,?,?,?,?)",
            (receipt.receipt_id, receipt.command_id, receipt.run_id,
             receipt.state.value, receipt.semantic_hash(),
             receipt.model_dump_json(), receipt.created_at),
        )
        return receipt

    def append_command(self, command: ControlCommand) -> AppendCommandResult:
        """Atomically append a command, initial receipt, and CAS rejection if any."""
        self._assert_run(command.run_id)
        command_hash = command.semantic_hash()
        with self._lock:
            try:
                self._conn.execute("BEGIN IMMEDIATE")
                existing = self._conn.execute(
                    "SELECT payload_hash, command_json FROM commands WHERE command_id=?",
                    (command.command_id,),
                ).fetchone()
                if existing is not None:
                    if existing[0] != command_hash:
                        raise IdempotencyConflict(
                            f"command_id {command.command_id!r} was reused with different content"
                        )
                    stored = ControlCommand.model_validate_json(existing[1])
                    receipts_rows = self._conn.execute(
                        "SELECT receipt_json FROM effects WHERE command_id=? ORDER BY seq",
                        (command.command_id,),
                    ).fetchall()
                    receipts = tuple(
                        EffectReceipt.model_validate_json(row[0]) for row in receipts_rows
                    )
                    self._conn.commit()
                    return AppendCommandResult(
                        command=stored, inserted=False,
                        accepted=not receipts[-1].state.terminal if receipts else False,
                        receipts=receipts,
                    )

                current = self._current_state_unlocked()
                self._conn.execute(
                    "INSERT INTO commands "
                    "(command_id, run_id, payload_hash, command_json, created_at) "
                    "VALUES (?,?,?,?,?)",
                    (command.command_id, command.run_id, command_hash,
                     command.model_dump_json(), command.created_at),
                )
                received = EffectReceipt(
                    command_id=command.command_id,
                    run_id=command.run_id,
                    state=EffectState.RECEIVED,
                    scope=command.scope,
                    observed_generation=current.generation,
                )
                self._insert_effect_unlocked(received)
                receipts: list[EffectReceipt] = [received]
                accepted = True
                if (command.expected_generation is not None
                        and command.expected_generation != current.generation):
                    rejected = EffectReceipt(
                        command_id=command.command_id,
                        run_id=command.run_id,
                        state=EffectState.REJECTED,
                        scope=command.scope,
                        observed_generation=current.generation,
                        detail=(f"expected generation {command.expected_generation}, "
                                f"observed {current.generation}"),
                        metadata={"code": "generation_conflict"},
                    )
                    self._insert_effect_unlocked(rejected)
                    receipts.append(rejected)
                    accepted = False
                self._conn.commit()
                return AppendCommandResult(
                    command=command, inserted=True, accepted=accepted,
                    receipts=tuple(receipts),
                )
            except Exception:
                self._conn.rollback()
                raise

    def append_effect(self, receipt: EffectReceipt) -> EffectReceipt:
        self._assert_run(receipt.run_id)
        with self._lock:
            try:
                self._conn.execute("BEGIN IMMEDIATE")
                result = self._insert_effect_unlocked(receipt)
                self._conn.commit()
                return result
            except Exception:
                self._conn.rollback()
                raise

    def append_persisted_with_companion(
        self,
        receipt: EffectReceipt,
        *,
        contexts: tuple[ContextResource, ...] = (),
        expiration_context_ids: tuple[str, ...] = (),
        expiration_actor: str = "operator",
        expiration_reason: str = "",
        decision_answer: Optional[DecisionAnswer] = None,
        decision_context: Optional[ContextResource] = None,
    ) -> tuple[EffectReceipt, int]:
        """Atomically publish PERSISTED and every typed durable companion.

        ``PERSISTED`` is the restart/idempotency fence used by ``ControlActor``.
        Consequently it must not become visible before a ContextResource,
        DecisionAnswer, or context-expiration promised by the same command.  A
        failure in any companion rolls the receipt back as well, so the caller
        can append FAILED directly after RECEIVED.
        """
        self._assert_run(receipt.run_id)
        if receipt.state is not EffectState.PERSISTED:
            raise ValueError("atomic companion receipt must be PERSISTED")
        for resource in contexts:
            self._assert_run(resource.run_id)
        if decision_answer is not None:
            self._assert_run(decision_answer.run_id)
        if decision_context is not None:
            self._assert_run(decision_context.run_id)
        with self._lock:
            try:
                self._conn.execute("BEGIN IMMEDIATE")
                persisted = self._insert_effect_unlocked(receipt)
                for resource in contexts:
                    self._insert_context_unlocked(resource)
                expired_count = 0
                at = receipt.created_at
                for context_id in expiration_context_ids:
                    if self._expire_context_unlocked(
                        context_id,
                        actor=expiration_actor,
                        reason=expiration_reason,
                        now=at,
                    ):
                        expired_count += 1
                if decision_answer is not None:
                    self._insert_decision_answer_with_context_unlocked(
                        decision_answer, context=decision_context)
                elif decision_context is not None:
                    raise ValueError(
                        "decision_context requires a decision_answer")
                self._conn.commit()
                return persisted, expired_count
            except Exception:
                self._conn.rollback()
                raise

    def append_state(self, state: RunControlState) -> RunControlState:
        """Append one desired-state generation; generation is strict current+1."""
        self._assert_run(state.run_id)
        with self._lock:
            try:
                self._conn.execute("BEGIN IMMEDIATE")
                current = self._current_state_unlocked()
                if current.mode is RunControlMode.TERMINATED:
                    raise StateConflict("terminated desired state is immutable")
                if state.generation != current.generation + 1:
                    raise StateConflict(
                        f"state generation must be {current.generation + 1}, "
                        f"got {state.generation}"
                    )
                if state.updated_by_command_id:
                    row = self._conn.execute(
                        "SELECT 1 FROM commands WHERE command_id=? AND run_id=?",
                        (state.updated_by_command_id, self.run_id),
                    ).fetchone()
                    if row is None:
                        raise StateConflict(
                            f"state references unknown command {state.updated_by_command_id!r}"
                        )
                self._conn.execute(
                    "INSERT INTO state_events "
                    "(run_id, generation, command_id, state_json, created_at) "
                    "VALUES (?,?,?,?,?)",
                    (self.run_id, state.generation,
                     state.updated_by_command_id or None,
                     state.model_dump_json(), state.updated_at),
                )
                self._conn.commit()
                return state
            except Exception:
                self._conn.rollback()
                raise

    def reopen_state(self, *, reason: str = "explicit resolve",
                     now: Optional[float] = None) -> RunControlState:
        """Start a new execution epoch at the next monotonic generation.

        TERMINATED is immutable within one execution attempt. ``resolve`` is the
        explicit boundary that permits a new ACTIVE generation without replaying
        old imperative commands.
        """
        at = time.time() if now is None else float(now)
        with self._lock:
            try:
                self._conn.execute("BEGIN IMMEDIATE")
                current = self._current_state_unlocked()
                state = RunControlState(
                    run_id=self.run_id, generation=current.generation + 1,
                    mode=RunControlMode.ACTIVE, updated_by_command_id="",
                    reason=str(reason or "explicit resolve"), updated_at=at,
                )
                self._conn.execute(
                    "INSERT INTO state_events "
                    "(run_id, generation, command_id, state_json, created_at) "
                    "VALUES (?,?,?,?,?)",
                    (self.run_id, state.generation, None,
                     state.model_dump_json(), state.updated_at),
                )
                self._conn.commit()
                return state
            except Exception:
                self._conn.rollback()
                raise

    # -- typed context -------------------------------------------------
    def _insert_context_unlocked(
        self, resource: ContextResource,
    ) -> ContextResource:
        payload_hash = resource.semantic_hash()
        row = self._conn.execute(
            "SELECT payload_hash, payload_json FROM context_events "
            "WHERE context_id=? AND operation='created' ORDER BY seq LIMIT 1",
            (resource.context_id,),
        ).fetchone()
        if row is not None:
            if row[0] != payload_hash:
                raise IdempotencyConflict(
                    f"context_id {resource.context_id!r} was reused with different content"
                )
            return ContextResource.model_validate_json(row[1])
        self._conn.execute(
            "INSERT INTO context_events "
            "(event_id, run_id, context_id, operation, payload_hash, "
            " payload_json, created_at) VALUES (?,?,?,?,?,?,?)",
            (f"CTXE-{uuid.uuid4().hex}", self.run_id, resource.context_id,
             "created", payload_hash, resource.model_dump_json(), resource.created_at),
        )
        return resource

    def append_context(self, resource: ContextResource) -> ContextResource:
        self._assert_run(resource.run_id)
        with self._lock:
            try:
                self._conn.execute("BEGIN IMMEDIATE")
                result = self._insert_context_unlocked(resource)
                self._conn.commit()
                return result
            except Exception:
                self._conn.rollback()
                raise

    def _expire_context_unlocked(
        self, context_id: str, *, actor: str, reason: str, now: float,
    ) -> bool:
        created = self._conn.execute(
            "SELECT 1 FROM context_events WHERE context_id=? "
            "AND operation='created'",
            (context_id,),
        ).fetchone()
        if created is None:
            raise ControlJournalError(f"unknown context_id {context_id!r}")
        latest = self._conn.execute(
            "SELECT operation FROM context_events WHERE context_id=? "
            "ORDER BY seq DESC LIMIT 1",
            (context_id,),
        ).fetchone()
        if latest and latest[0] == "expired":
            return False
        payload = {"actor": actor, "reason": reason, "created_at": now}
        self._conn.execute(
            "INSERT INTO context_events "
            "(event_id, run_id, context_id, operation, payload_hash, "
            " payload_json, created_at) VALUES (?,?,?,?,?,?,?)",
            (f"CTXE-{uuid.uuid4().hex}", self.run_id, context_id,
             "expired", _hash_json(payload), json.dumps(payload), now),
        )
        return True

    def expire_context(self, context_id: str, *, actor: str = "operator",
                       reason: str = "", now: Optional[float] = None) -> bool:
        now = time.time() if now is None else now
        with self._lock:
            try:
                self._conn.execute("BEGIN IMMEDIATE")
                result = self._expire_context_unlocked(
                    context_id, actor=actor, reason=reason, now=now)
                self._conn.commit()
                return result
            except Exception:
                self._conn.rollback()
                raise

    def _context_delivery_state_unlocked(
        self, context_id: str,
    ) -> tuple[list[str], dict[str, str]]:
        """Fold committed bindings and still-live delivery reservations."""
        rows = self._conn.execute(
            "SELECT operation, payload_json FROM context_events "
            "WHERE context_id=? AND operation IN "
            "('reserved','bound','released','delivery_unknown') "
            "ORDER BY seq",
            (context_id,),
        ).fetchall()
        bound_workers: list[str] = []
        reservations: dict[str, str] = {}
        for operation, raw in rows:
            try:
                payload = json.loads(raw)
            except Exception:
                payload = {}
            worker_id = str(payload.get("worker_id") or "")
            reservation_id = str(payload.get("reservation_id") or "")
            if operation == "reserved" and reservation_id and worker_id:
                reservations[reservation_id] = worker_id
            elif operation == "released" and reservation_id:
                reservations.pop(reservation_id, None)
            elif operation == "bound":
                if worker_id:
                    bound_workers.append(worker_id)
                if reservation_id:
                    reservations.pop(reservation_id, None)
            elif operation == "delivery_unknown":
                # Recovery cannot prove whether argv crossed the process boundary.
                # Consume finite disclosure capacity without claiming a binding.
                if worker_id:
                    bound_workers.append(worker_id)
                if reservation_id:
                    reservations.pop(reservation_id, None)
        return bound_workers, reservations

    def reserve_context(self, context_id: str, *, worker_id: str,
                        now: Optional[float] = None) -> Optional[str]:
        """Atomically claim prompt material before it is read or injected.

        A reservation consumes ``max_bindings`` capacity until it is committed at
        the real subprocess-start boundary or explicitly released.  This closes the
        build-two-workers/read-first TOCTOU that could disclose a one-shot secret to
        multiple prompts while recording only one binding.
        """
        context_id = str(context_id or "").strip()
        worker_id = str(worker_id or "").strip()
        if not context_id or not worker_id:
            raise ValueError("context_id and worker_id are required")
        at = time.time() if now is None else float(now)
        with self._lock:
            try:
                self._conn.execute("BEGIN IMMEDIATE")
                created = self._conn.execute(
                    "SELECT payload_json FROM context_events WHERE context_id=? "
                    "AND operation='created' ORDER BY seq LIMIT 1",
                    (context_id,),
                ).fetchone()
                if created is None:
                    raise ControlJournalError(f"unknown context_id {context_id!r}")
                resource = ContextResource.model_validate_json(created[0])
                if resource.is_expired(at):
                    self._conn.commit()
                    return None
                expired = self._conn.execute(
                    "SELECT 1 FROM context_events WHERE context_id=? "
                    "AND operation='expired' LIMIT 1",
                    (context_id,),
                ).fetchone()
                if expired is not None:
                    self._conn.commit()
                    return None
                bound_workers, reservations = self._context_delivery_state_unlocked(
                    context_id)
                # Finite resources are idempotent per worker identity. Unlimited
                # standing resources intentionally cross process generations; UI
                # worker labels can repeat after a server restart, so a historical
                # binding must not suppress delivery to the fresh execution.
                if (resource.max_bindings is not None
                        and worker_id in bound_workers):
                    self._conn.commit()
                    return None
                for reservation_id, owner in reservations.items():
                    if owner == worker_id:
                        self._conn.commit()
                        return reservation_id
                used = len(bound_workers) + len(reservations)
                if resource.max_bindings is not None and used >= resource.max_bindings:
                    self._conn.commit()
                    return None
                reservation_id = f"CTXR-{uuid.uuid4().hex}"
                payload = {
                    "reservation_id": reservation_id,
                    "worker_id": worker_id,
                    "reserved_at": at,
                }
                self._conn.execute(
                    "INSERT INTO context_events "
                    "(event_id, run_id, context_id, operation, payload_hash, "
                    " payload_json, created_at) VALUES (?,?,?,?,?,?,?)",
                    (reservation_id, self.run_id, context_id, "reserved",
                     _hash_json(payload), json.dumps(payload), at),
                )
                self._conn.commit()
                return reservation_id
            except Exception:
                self._conn.rollback()
                raise

    def commit_context_binding(
        self, context_id: str, *, worker_id: str, reservation_id: str,
        now: Optional[float] = None,
    ) -> bool:
        """Commit a reserved delivery once the prompt-carrying process exists."""
        context_id = str(context_id or "").strip()
        worker_id = str(worker_id or "").strip()
        reservation_id = str(reservation_id or "").strip()
        if not context_id or not worker_id or not reservation_id:
            raise ValueError("context_id, worker_id and reservation_id are required")
        at = time.time() if now is None else float(now)
        digest = hashlib.sha256(
            f"{self.run_id}:{context_id}:{reservation_id}:bound".encode("utf-8")
        ).hexdigest()[:32]
        event_id = f"CTXB-{digest}"
        with self._lock:
            try:
                self._conn.execute("BEGIN IMMEDIATE")
                created = self._conn.execute(
                    "SELECT payload_json FROM context_events WHERE context_id=? "
                    "AND operation='created' ORDER BY seq LIMIT 1",
                    (context_id,),
                ).fetchone()
                if created is None:
                    raise ControlJournalError(f"unknown context_id {context_id!r}")
                resource = ContextResource.model_validate_json(created[0])
                explicitly_expired = self._conn.execute(
                    "SELECT 1 FROM context_events WHERE context_id=? "
                    "AND operation='expired' LIMIT 1",
                    (context_id,),
                ).fetchone() is not None
                existing = self._conn.execute(
                    "SELECT 1 FROM context_events WHERE event_id=?", (event_id,)
                ).fetchone()
                if existing is not None:
                    self._conn.commit()
                    # Idempotent retry of the same Popen-bound commit is success:
                    # the durable delivery fact already exists.
                    return True
                _bound, reservations = self._context_delivery_state_unlocked(context_id)
                if reservations.get(reservation_id) != worker_id:
                    self._conn.commit()
                    return False
                if explicitly_expired or resource.is_expired(at):
                    # Revocation/TTL is authoritative at the final Popen fence, but
                    # argv already crossed the process boundary.  Kill the process
                    # and conservatively consume finite disclosure capacity; making
                    # this reservation active again could disclose a one-shot secret
                    # twice.
                    unknown_digest = hashlib.sha256(
                        f"{self.run_id}:{context_id}:{reservation_id}:unknown".encode(
                            "utf-8")
                    ).hexdigest()[:32]
                    unknown_event_id = f"CTXU-{unknown_digest}"
                    unknown_payload = {
                        "reservation_id": reservation_id,
                        "worker_id": worker_id,
                        "actor": "popen-commit",
                        "unknown_at": at,
                        "reason": "context revoked at process-start delivery fence",
                    }
                    self._conn.execute(
                        "INSERT OR IGNORE INTO context_events "
                        "(event_id, run_id, context_id, operation, payload_hash, "
                        " payload_json, created_at) VALUES (?,?,?,?,?,?,?)",
                        (unknown_event_id, self.run_id, context_id,
                         "delivery_unknown", _hash_json(unknown_payload),
                         json.dumps(unknown_payload), at),
                    )
                    self._conn.commit()
                    return False
                payload = {
                    "reservation_id": reservation_id,
                    "worker_id": worker_id,
                    "bound_at": at,
                }
                self._conn.execute(
                    "INSERT INTO context_events "
                    "(event_id, run_id, context_id, operation, payload_hash, "
                    " payload_json, created_at) VALUES (?,?,?,?,?,?,?)",
                    (event_id, self.run_id, context_id, "bound",
                     _hash_json(payload), json.dumps(payload), at),
                )
                self._conn.commit()
                return True
            except Exception:
                self._conn.rollback()
                raise

    def release_context_reservation(
        self, context_id: str, *, worker_id: str, reservation_id: str,
        now: Optional[float] = None,
    ) -> bool:
        """Release an unstarted prompt claim without erasing its audit trail."""
        context_id = str(context_id or "").strip()
        worker_id = str(worker_id or "").strip()
        reservation_id = str(reservation_id or "").strip()
        if not context_id or not worker_id or not reservation_id:
            raise ValueError("context_id, worker_id and reservation_id are required")
        at = time.time() if now is None else float(now)
        digest = hashlib.sha256(
            f"{self.run_id}:{context_id}:{reservation_id}:released".encode("utf-8")
        ).hexdigest()[:32]
        event_id = f"CTXL-{digest}"
        with self._lock:
            try:
                self._conn.execute("BEGIN IMMEDIATE")
                existing = self._conn.execute(
                    "SELECT 1 FROM context_events WHERE event_id=?", (event_id,)
                ).fetchone()
                if existing is not None:
                    self._conn.commit()
                    # Idempotent postcondition: this exact owner/reservation release
                    # is already durable. Retirement retries must distinguish that
                    # from a still-live or foreign reservation.
                    return True
                _bound, reservations = self._context_delivery_state_unlocked(context_id)
                if reservations.get(reservation_id) != worker_id:
                    self._conn.commit()
                    return False
                payload = {
                    "reservation_id": reservation_id,
                    "worker_id": worker_id,
                    "released_at": at,
                }
                self._conn.execute(
                    "INSERT INTO context_events "
                    "(event_id, run_id, context_id, operation, payload_hash, "
                    " payload_json, created_at) VALUES (?,?,?,?,?,?,?)",
                    (event_id, self.run_id, context_id, "released",
                     _hash_json(payload), json.dumps(payload), at),
                )
                self._conn.commit()
                return True
            except Exception:
                self._conn.rollback()
                raise

    def mark_context_delivery_unknown(
        self, context_id: str, *, worker_id: str, reservation_id: str,
        actor: str = "popen-commit", reason: str = "",
        now: Optional[float] = None,
    ) -> bool:
        """Terminalise a delivery whose argv crossed Popen without a commit ACK.

        Unlike :meth:`release_context_reservation`, this is valid after process
        creation and conservatively consumes finite ``max_bindings`` capacity.  It
        may follow a prior ``released`` event (for example a revocation race), but
        never double-counts an already-bound or already-unknown reservation.
        """
        context_id = str(context_id or "").strip()
        worker_id = str(worker_id or "").strip()
        reservation_id = str(reservation_id or "").strip()
        if not context_id or not worker_id or not reservation_id:
            raise ValueError("context_id, worker_id and reservation_id are required")
        at = time.time() if now is None else float(now)
        digest = hashlib.sha256(
            f"{self.run_id}:{context_id}:{reservation_id}:unknown".encode("utf-8")
        ).hexdigest()[:32]
        event_id = f"CTXU-{digest}"
        with self._lock:
            try:
                self._conn.execute("BEGIN IMMEDIATE")
                rows = self._conn.execute(
                    "SELECT operation, payload_json FROM context_events "
                    "WHERE context_id=? ORDER BY seq", (context_id,),
                ).fetchall()
                reserved_by_owner = False
                for operation, raw in rows:
                    try:
                        payload = json.loads(raw)
                    except Exception:
                        payload = {}
                    if str(payload.get("reservation_id") or "") != reservation_id:
                        continue
                    event_worker = str(payload.get("worker_id") or "")
                    if operation == "reserved" and event_worker == worker_id:
                        reserved_by_owner = True
                    elif operation in {"bound", "delivery_unknown"}:
                        self._conn.commit()
                        return False
                if not reserved_by_owner:
                    self._conn.commit()
                    return False
                payload = {
                    "reservation_id": reservation_id,
                    "worker_id": worker_id,
                    "actor": str(actor or "popen-commit"),
                    "unknown_at": at,
                    "reason": str(reason or
                                  "delivery commit unavailable after process start")[:500],
                }
                self._conn.execute(
                    "INSERT OR IGNORE INTO context_events "
                    "(event_id, run_id, context_id, operation, payload_hash, "
                    " payload_json, created_at) VALUES (?,?,?,?,?,?,?)",
                    (event_id, self.run_id, context_id, "delivery_unknown",
                     _hash_json(payload), json.dumps(payload), at),
                )
                self._conn.commit()
                return True
            except Exception:
                self._conn.rollback()
                raise

    def bind_context(self, context_id: str, *, worker_id: str,
                     now: Optional[float] = None) -> bool:
        """Compatibility helper: reserve and immediately commit a binding."""
        reservation_id = self.reserve_context(
            context_id, worker_id=worker_id, now=now)
        if reservation_id is None:
            return False
        return self.commit_context_binding(
            context_id, worker_id=worker_id,
            reservation_id=reservation_id, now=now)

    def recover_context_reservations(
        self, *, actor: str = "runtime-recovery",
        now: Optional[float] = None,
    ) -> list[dict[str, str]]:
        """Terminalise reservations owned by a prior runtime generation.

        A crash between reservation and Popen commit has an unknowable disclosure
        outcome.  Recovery therefore never replays it.  ``delivery_unknown`` removes
        the live reservation while conservatively consuming one finite binding slot;
        unlimited standing context remains available to the new generation.
        """
        at = time.time() if now is None else float(now)
        recovered: list[dict[str, str]] = []
        with self._lock:
            try:
                self._conn.execute("BEGIN IMMEDIATE")
                context_ids = [
                    str(row[0]) for row in self._conn.execute(
                        "SELECT DISTINCT context_id FROM context_events "
                        "WHERE run_id=? AND operation='reserved'",
                        (self.run_id,),
                    ).fetchall()
                ]
                for context_id in context_ids:
                    _used, reservations = self._context_delivery_state_unlocked(
                        context_id)
                    for reservation_id, worker_id in reservations.items():
                        digest = hashlib.sha256(
                            f"{self.run_id}:{context_id}:{reservation_id}:unknown".encode(
                                "utf-8")
                        ).hexdigest()[:32]
                        event_id = f"CTXU-{digest}"
                        payload = {
                            "reservation_id": reservation_id,
                            "worker_id": worker_id,
                            "actor": str(actor or "runtime-recovery"),
                            "recovered_at": at,
                            "reason": "delivery outcome unknown after runtime restart",
                        }
                        self._conn.execute(
                            "INSERT OR IGNORE INTO context_events "
                            "(event_id, run_id, context_id, operation, payload_hash, "
                            " payload_json, created_at) VALUES (?,?,?,?,?,?,?)",
                            (event_id, self.run_id, context_id, "delivery_unknown",
                             _hash_json(payload), json.dumps(payload), at),
                        )
                        recovered.append({
                            "context_id": context_id,
                            "reservation_id": reservation_id,
                            "worker_id": worker_id,
                        })
                self._conn.commit()
                return recovered
            except Exception:
                self._conn.rollback()
                raise

    def context_bindings(self, context_id: str) -> list[str]:
        with self._lock:
            rows = self._conn.execute(
                "SELECT payload_json FROM context_events WHERE context_id=? "
                "AND operation='bound' ORDER BY seq",
                (context_id,),
            ).fetchall()
        result: list[str] = []
        for (raw,) in rows:
            try:
                worker_id = str(json.loads(raw).get("worker_id") or "")
            except Exception:
                worker_id = ""
            if worker_id:
                result.append(worker_id)
        return result

    def context_delivery_status(
        self, context_id: str, *, now: Optional[float] = None,
    ) -> str:
        """Return the folded disclosure state without exposing context content."""
        at = time.time() if now is None else float(now)
        with self._lock:
            created = self._conn.execute(
                "SELECT payload_json FROM context_events WHERE context_id=? "
                "AND operation='created' ORDER BY seq LIMIT 1",
                (context_id,),
            ).fetchone()
            if created is None:
                return "missing"
            resource = ContextResource.model_validate_json(created[0])
            rows = self._conn.execute(
                "SELECT operation, payload_json FROM context_events "
                "WHERE context_id=? ORDER BY seq",
                (context_id,),
            ).fetchall()
        if resource.is_expired(at) or any(op == "expired" for op, _raw in rows):
            return "expired"
        reservations: set[str] = set()
        bound = 0
        unknown = 0
        for operation, raw in rows:
            try:
                reservation_id = str(json.loads(raw).get("reservation_id") or "")
            except Exception:
                reservation_id = ""
            if operation == "reserved" and reservation_id:
                reservations.add(reservation_id)
            elif operation in {"released", "bound", "delivery_unknown"}:
                reservations.discard(reservation_id)
                if operation == "bound":
                    bound += 1
                elif operation == "delivery_unknown":
                    unknown += 1
        if reservations:
            return "reserved"
        used = bound + unknown
        if resource.max_bindings is not None and used >= resource.max_bindings:
            return "delivery_unknown" if unknown else "bound"
        return "active"

    def context_resources(self, *, active_only: bool = True,
                          now: Optional[float] = None) -> list[ContextResource]:
        now = time.time() if now is None else now
        with self._lock:
            rows = self._conn.execute(
                "SELECT context_id, operation, payload_json FROM context_events "
                "WHERE run_id=? ORDER BY seq",
                (self.run_id,),
            ).fetchall()
        resources: dict[str, ContextResource] = {}
        expired: set[str] = set()
        bindings: dict[str, int] = {}
        reservations: dict[str, dict[str, str]] = {}
        for context_id, operation, payload_json in rows:
            if operation == "created":
                resources[context_id] = ContextResource.model_validate_json(payload_json)
            elif operation == "expired":
                expired.add(context_id)
            elif operation == "bound":
                bindings[context_id] = bindings.get(context_id, 0) + 1
                try:
                    reservation_id = str(json.loads(payload_json).get(
                        "reservation_id") or "")
                except Exception:
                    reservation_id = ""
                if reservation_id:
                    reservations.setdefault(context_id, {}).pop(
                        reservation_id, None)
            elif operation == "reserved":
                try:
                    payload = json.loads(payload_json)
                    reservation_id = str(payload.get("reservation_id") or "")
                    worker_id = str(payload.get("worker_id") or "")
                except Exception:
                    reservation_id = worker_id = ""
                if reservation_id and worker_id:
                    reservations.setdefault(context_id, {})[reservation_id] = worker_id
            elif operation == "released":
                try:
                    reservation_id = str(json.loads(payload_json).get(
                        "reservation_id") or "")
                except Exception:
                    reservation_id = ""
                if reservation_id:
                    reservations.setdefault(context_id, {}).pop(
                        reservation_id, None)
            elif operation == "delivery_unknown":
                bindings[context_id] = bindings.get(context_id, 0) + 1
                try:
                    reservation_id = str(json.loads(payload_json).get(
                        "reservation_id") or "")
                except Exception:
                    reservation_id = ""
                if reservation_id:
                    reservations.setdefault(context_id, {}).pop(
                        reservation_id, None)
        if not active_only:
            return list(resources.values())
        return [
            resource for context_id, resource in resources.items()
            if (context_id not in expired and not resource.is_expired(now)
                and (resource.max_bindings is None
                     or (bindings.get(context_id, 0)
                         + len(reservations.get(context_id, {})))
                     < resource.max_bindings))
        ]

    # -- scoped decisions ----------------------------------------------
    def append_decision_request(self, request: DecisionRequest) -> DecisionRequest:
        self._assert_run(request.run_id)
        payload_hash = request.semantic_hash()
        with self._lock:
            try:
                self._conn.execute("BEGIN IMMEDIATE")
                row = self._conn.execute(
                    "SELECT payload_hash, request_json FROM decision_requests "
                    "WHERE request_id=?",
                    (request.request_id,),
                ).fetchone()
                if row is not None:
                    if row[0] != payload_hash:
                        raise IdempotencyConflict(
                            f"request_id {request.request_id!r} was reused with different content"
                        )
                    self._conn.commit()
                    return DecisionRequest.model_validate_json(row[1])
                self._conn.execute(
                    "INSERT INTO decision_requests "
                    "(request_id, run_id, payload_hash, request_json, created_at) "
                    "VALUES (?,?,?,?,?)",
                    (request.request_id, self.run_id, payload_hash,
                     request.model_dump_json(), request.created_at),
                )
                self._conn.commit()
                return request
            except Exception:
                self._conn.rollback()
                raise

    def decision_status(self, request_id: str) -> Optional[DecisionStatus]:
        with self._lock:
            exists = self._conn.execute(
                "SELECT 1 FROM decision_requests WHERE request_id=? AND run_id=?",
                (request_id, self.run_id),
            ).fetchone()
            if exists is None:
                return None
            row = self._conn.execute(
                "SELECT status FROM decision_answers WHERE request_id=? "
                "ORDER BY seq DESC LIMIT 1",
                (request_id,),
            ).fetchone()
        return DecisionStatus(row[0]) if row else DecisionStatus.OPEN

    def get_decision_request(self, request_id: str) -> Optional[DecisionRequest]:
        """Return one immutable request by id without inferring from card order."""
        with self._lock:
            row = self._conn.execute(
                "SELECT request_json FROM decision_requests "
                "WHERE request_id=? AND run_id=?",
                (str(request_id), self.run_id),
            ).fetchone()
        return DecisionRequest.model_validate_json(row[0]) if row else None

    def append_decision_answer(self, answer: DecisionAnswer) -> DecisionAnswer:
        return self.append_decision_answer_with_context(answer, context=None)

    def _insert_decision_answer_with_context_unlocked(
        self,
        answer: DecisionAnswer,
        *,
        context: Optional[ContextResource],
    ) -> DecisionAnswer:
        payload = answer.model_dump(mode="json", exclude={"answer_id", "created_at"})
        payload_hash = _hash_json(payload)
        duplicate = self._conn.execute(
            "SELECT payload_hash, answer_json FROM decision_answers "
            "WHERE answer_id=?",
            (answer.answer_id,),
        ).fetchone()
        if duplicate is not None:
            if duplicate[0] != payload_hash:
                raise IdempotencyConflict(
                    f"answer_id {answer.answer_id!r} was reused with different content"
                )
            return DecisionAnswer.model_validate_json(duplicate[1])
        request = self._conn.execute(
            "SELECT 1 FROM decision_requests WHERE request_id=? AND run_id=?",
            (answer.request_id, self.run_id),
        ).fetchone()
        if request is None:
            raise ControlJournalError(
                f"unknown decision request {answer.request_id!r}"
            )
        prior = self._conn.execute(
            "SELECT status FROM decision_answers WHERE request_id=? "
            "ORDER BY seq DESC LIMIT 1",
            (answer.request_id,),
        ).fetchone()
        if prior is not None:
            raise StateConflict(
                f"decision {answer.request_id!r} is already {prior[0]}"
            )
        if context is not None:
            context_hash = context.semantic_hash()
            context_row = self._conn.execute(
                "SELECT payload_hash, payload_json FROM context_events "
                "WHERE context_id=? AND operation='created' ORDER BY seq LIMIT 1",
                (context.context_id,),
            ).fetchone()
            if context_row is not None and context_row[0] != context_hash:
                raise IdempotencyConflict(
                    f"context_id {context.context_id!r} was reused with different content"
                )
        else:
            context_row = None
        self._conn.execute(
            "INSERT INTO decision_answers "
            "(answer_id, request_id, run_id, status, payload_hash, "
            " answer_json, created_at) VALUES (?,?,?,?,?,?,?)",
            (answer.answer_id, answer.request_id, self.run_id,
             answer.status.value, payload_hash,
             answer.model_dump_json(), answer.created_at),
        )
        if context is not None and context_row is None:
            self._insert_context_unlocked(context)
        return answer

    def append_decision_answer_with_context(
        self,
        answer: DecisionAnswer,
        *,
        context: Optional[ContextResource],
    ) -> DecisionAnswer:
        """Atomically close a decision and enqueue its answer context.

        Without one transaction, a crash between the two appends could leave the
        request ANSWERED while no future worker could ever receive the answer.
        """
        self._assert_run(answer.run_id)
        if context is not None:
            self._assert_run(context.run_id)
        with self._lock:
            try:
                self._conn.execute("BEGIN IMMEDIATE")
                result = self._insert_decision_answer_with_context_unlocked(
                    answer, context=context)
                self._conn.commit()
                return result
            except Exception:
                self._conn.rollback()
                raise

    def decision_requests(self, *, open_only: bool = False) -> list[DecisionRequest]:
        with self._lock:
            rows = self._conn.execute(
                "SELECT request_json, request_id FROM decision_requests "
                "WHERE run_id=? ORDER BY seq",
                (self.run_id,),
            ).fetchall()
            if open_only:
                closed = {
                    row[0] for row in self._conn.execute(
                        "SELECT DISTINCT request_id FROM decision_answers WHERE run_id=?",
                        (self.run_id,),
                    ).fetchall()
                }
            else:
                closed = set()
        return [
            DecisionRequest.model_validate_json(raw)
            for raw, request_id in rows
            if not open_only or request_id not in closed
        ]
