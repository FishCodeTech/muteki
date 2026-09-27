"""Single-writer atomic command/event/fold/outbox store for Protocol 2."""

from __future__ import annotations

import json
import os
import sqlite3
import threading
from contextlib import contextmanager
from collections.abc import Callable, Iterator, Mapping, Sequence
from dataclasses import replace
from pathlib import Path
from typing import Any

from muteki.epistemic.contracts import (
    CanonicalReceipt,
    EventEnvelopeV2,
    canonical_digest,
    canonical_json_bytes,
)
from muteki.epistemic.folds import CanonicalState, apply_event, initial_state
from muteki.epistemic.sqlite_authority import _require_authority_mutations
from muteki.epistemic.sqlite_schema import (
    _immutable_triggers,
    _LIFECYCLE_SCHEMA,
    _RECEIPT_OBJECT_SCHEMA,
    _SCHEMA,
)
from muteki.epistemic.sqlite_types import (
    CommandCommitResult,
    CommandEvent,
    EFFECT_LEGAL_TRANSITIONS,  # noqa: F401 - compatibility export
    FlagAcceptedOutboxV1,  # noqa: F401 - compatibility export
    IdempotencyConflict,
    IntegrityError,
    OutboxIntent,
    ProjectionMutation,
    _is_sha256,
    require_positive_effect_revision,  # noqa: F401 - compatibility export
    FLAG_ACCEPTED_OUTBOX_SCHEMA_ID,  # noqa: F401 - compatibility export
)

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



from muteki.epistemic import sqlite_validation_runtime as _val_runtime  # noqa: E402
from muteki.epistemic import sqlite_validation_cognitive as _val_cognitive  # noqa: E402
from muteki.epistemic import sqlite_validation_evaluation as _val_evaluation  # noqa: E402
from muteki.epistemic import sqlite_validation_reproduction as _val_reproduction  # noqa: E402
from muteki.epistemic import sqlite_projection_core as _proj_core  # noqa: E402
from muteki.epistemic import sqlite_projection_cognitive as _proj_cognitive  # noqa: E402
EpistemicSQLiteStore._validate_c6_eval_binding_mutation = _val_runtime._validate_c6_eval_binding_mutation
EpistemicSQLiteStore._validate_runtime_context_cognitive_assignment_mutation = _val_runtime._validate_runtime_context_cognitive_assignment_mutation
EpistemicSQLiteStore._validate_runtime_cognitive_execution_mutation = _val_runtime._validate_runtime_cognitive_execution_mutation
EpistemicSQLiteStore._runtime_evaluation_binding_from_sidecar = _val_runtime._runtime_evaluation_binding_from_sidecar
EpistemicSQLiteStore._validate_c6_eval_v2_binding_mutation = _val_runtime._validate_c6_eval_v2_binding_mutation
EpistemicSQLiteStore.validate_runtime_evaluation_v2_prerequisite_lineage = _val_runtime.validate_runtime_evaluation_v2_prerequisite_lineage
EpistemicSQLiteStore._validate_runtime_evaluation_v2_attempt_body = _val_runtime._validate_runtime_evaluation_v2_attempt_body
EpistemicSQLiteStore._validate_cognitive_assignment_mutation = _val_cognitive._validate_cognitive_assignment_mutation
EpistemicSQLiteStore._validate_cognitive_execution_mutation = _val_cognitive._validate_cognitive_execution_mutation
EpistemicSQLiteStore._validate_c6_eval_outcome_mutation = _val_evaluation._validate_c6_eval_outcome_mutation
EpistemicSQLiteStore._is_c6_v2_observer_attempt_locked = _val_evaluation._is_c6_v2_observer_attempt_locked
EpistemicSQLiteStore._reject_c6_v2_observer_events_locked = _val_evaluation._reject_c6_v2_observer_events_locked
EpistemicSQLiteStore._require_c6_v2_terminal_accounting_locked = _val_evaluation._require_c6_v2_terminal_accounting_locked
EpistemicSQLiteStore._assert_c6_claims_closed_before_worker_terminal_locked = _val_evaluation._assert_c6_claims_closed_before_worker_terminal_locked
EpistemicSQLiteStore._assert_c6_claims_closed_before_attempt_state_change_locked = _val_evaluation._assert_c6_claims_closed_before_attempt_state_change_locked
EpistemicSQLiteStore._assert_c6_scope_deactivation_is_closed_locked = _val_evaluation._assert_c6_scope_deactivation_is_closed_locked
EpistemicSQLiteStore._validate_cognitive_reproduction_source_locked = _val_reproduction._validate_cognitive_reproduction_source_locked
EpistemicSQLiteStore._validate_reproduction_launch_lineage_locked = _val_reproduction._validate_reproduction_launch_lineage_locked
EpistemicSQLiteStore._validate_cognitive_reproduction_prelaunch_mutation = _val_reproduction._validate_cognitive_reproduction_prelaunch_mutation
EpistemicSQLiteStore._validate_cognitive_reproduction_launch_witness_mutation = _val_reproduction._validate_cognitive_reproduction_launch_witness_mutation
EpistemicSQLiteStore._json_map = _proj_core._json_map
EpistemicSQLiteStore._apply_projection_mutation = _proj_core._apply_projection_mutation
EpistemicSQLiteStore._mutation_from_event = _proj_core._mutation_from_event
EpistemicSQLiteStore._apply_cognitive_projection_mutation = _proj_cognitive._apply_cognitive_projection_mutation
