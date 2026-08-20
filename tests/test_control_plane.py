from __future__ import annotations

import asyncio
import time

import pytest
from pydantic import ValidationError

from muteki.control import (
    ApplyResult,
    ContextKind,
    ContextResource,
    ContextTaint,
    AdmissionError,
    ControlAction,
    ControlActor,
    ControlAdmission,
    ControlCommand,
    ControlScope,
    DecisionAnswer,
    DecisionRequest,
    DecisionStatus,
    EffectReceipt,
    EffectState,
    IdempotencyConflict,
    InMemoryWorkerRegistry,
    InvalidEffectTransition,
    RunControlMode,
    RunControlState,
    SQLiteControlJournal,
    WorkerRef,
    context_resource_id_for_command,
    stable_decision_request_id,
)


def test_typed_scope_context_and_stable_decision_id():
    assert ControlScope.parse("solver:claude-1") == ControlScope(
        kind="worker", value="claude-1"
    )
    assert ControlScope.parse("solver:claude-1").as_legacy_target() == "solver:claude-1"
    with pytest.raises(ValueError):
        ControlScope.parse("mystery:value")

    secret = ContextResource(
        run_id="run-1",
        kind=ContextKind.SECRET_REF,
        content="secret://run-1/vps-password",
    )
    assert secret.taint is ContextTaint.SECRET_REFERENCE
    with pytest.raises(ValidationError):
        ContextResource(
            run_id="run-1", kind=ContextKind.SECRET_REF, content="plaintext-password"
        )

    kwargs = dict(
        run_id="run-1", worker_id="w-1", prompt="Need a token",
        kind="external_input",
    )
    assert stable_decision_request_id(**kwargs) == stable_decision_request_id(**kwargs)
    first_occurrence = stable_decision_request_id(
        **kwargs, execution_id="session-1", execution_occurrence="attempt-1",
        resolve_epoch=1)
    assert first_occurrence == stable_decision_request_id(
        **kwargs, execution_id="session-1", execution_occurrence="attempt-1",
        resolve_epoch=1)
    assert first_occurrence != stable_decision_request_id(
        **kwargs, execution_id="session-1", execution_occurrence="attempt-2",
        resolve_epoch=1)
    assert first_occurrence != stable_decision_request_id(
        **kwargs, execution_id="session-1", execution_occurrence="attempt-1",
        resolve_epoch=2)


@pytest.mark.asyncio
async def test_decision_answer_and_delivery_context_commit_atomically(tmp_path):
    journal = SQLiteControlJournal(tmp_path / "decision-atomic.db", run_id="run-1")
    journal.append_decision_request(DecisionRequest(
        request_id="DR-atomic", run_id="run-1", prompt="need token"))
    command_id = "C-answer-atomic"
    # Force the companion context insert to conflict. The decision close must roll
    # back in the same transaction, leaving the request answerable.
    journal.append_context(ContextResource(
        context_id=context_resource_id_for_command(command_id),
        run_id="run-1", content="preexisting conflicting context"))
    actor = ControlActor(run_id="run-1", journal=journal)
    receipt = await actor.submit_and_wait(ControlCommand(
        command_id=command_id, run_id="run-1",
        action=ControlAction.ANSWER_DECISION,
        payload={"request_id": "DR-atomic", "text": "actual token"},
    ))
    assert receipt.state is EffectState.FAILED
    assert [row.state for row in journal.effect_history(command_id)] == [
        EffectState.RECEIVED, EffectState.FAILED,
    ]
    assert journal.decision_status("DR-atomic") is DecisionStatus.OPEN
    await actor.close()
    journal.close()


def test_append_only_journal_idempotency_transitions_and_restart(tmp_path):
    path = tmp_path / "control.db"
    journal = SQLiteControlJournal(path, run_id="run-1")
    command = ControlCommand(
        command_id="C-fixed",
        run_id="run-1",
        action=ControlAction.HINT,
        scope="solver:w-1",
        payload={"text": "inspect /admin"},
        expected_generation=0,
    )
    appended = journal.append_command(command)
    assert appended.inserted and appended.accepted
    assert [r.state for r in appended.receipts] == [EffectState.RECEIVED]

    for state in (
        EffectState.PERSISTED,
        EffectState.ROUTED,
        EffectState.EFFECT_OBSERVED,
    ):
        journal.append_effect(EffectReceipt(
            command_id=command.command_id,
            run_id=command.run_id,
            state=state,
            scope=command.scope,
        ))
    assert [r.state for r in journal.effect_history(command.command_id)] == [
        EffectState.RECEIVED,
        EffectState.PERSISTED,
        EffectState.ROUTED,
        EffectState.EFFECT_OBSERVED,
    ]
    with pytest.raises(InvalidEffectTransition):
        journal.append_effect(EffectReceipt(
            command_id=command.command_id,
            run_id=command.run_id,
            state=EffectState.FAILED,
            scope=command.scope,
        ))

    duplicate = journal.append_command(command.model_copy(update={"created_at": time.time()}))
    assert not duplicate.inserted
    assert duplicate.latest_receipt.state is EffectState.EFFECT_OBSERVED
    with pytest.raises(IdempotencyConflict):
        journal.append_command(command.model_copy(update={"payload": {"text": "different"}}))

    journal.close()
    reopened = SQLiteControlJournal(path, run_id="run-1")
    assert reopened.get_command("C-fixed") == command
    assert reopened.latest_effect("C-fixed").state is EffectState.EFFECT_OBSERVED
    reopened.close()


def test_generation_cas_is_atomic_and_rejection_is_auditable(tmp_path):
    journal = SQLiteControlJournal(tmp_path / "control.db", run_id="run-1")
    owner = ControlCommand(
        command_id="C-owner", run_id="run-1", action="pause", expected_generation=0,
    )
    assert journal.append_command(owner).accepted
    journal.append_effect(EffectReceipt(
        command_id=owner.command_id, run_id="run-1", state="persisted"
    ))
    journal.append_state(RunControlState(
        run_id="run-1", generation=1, mode="quiesced",
        updated_by_command_id=owner.command_id,
    ))

    stale = ControlCommand(
        command_id="C-stale", run_id="run-1", action="resume", expected_generation=0,
    )
    result = journal.append_command(stale)
    assert result.inserted and not result.accepted
    assert [r.state for r in result.receipts] == [
        EffectState.RECEIVED, EffectState.REJECTED,
    ]
    assert result.latest_receipt.metadata["code"] == "generation_conflict"
    assert journal.current_state().generation == 1
    terminated = RunControlState(
        run_id="run-1", generation=2, mode="terminated",
        updated_by_command_id="C-owner",
    )
    journal.append_state(terminated)
    reopened = journal.reopen_state(reason="operator resolve")
    assert reopened.mode is RunControlMode.ACTIVE
    assert reopened.generation == 3
    journal.close()


def test_reopen_state_advances_generation_even_when_already_active(tmp_path):
    journal = SQLiteControlJournal(tmp_path / "control.db", run_id="run-active")
    assert journal.current_state().mode is RunControlMode.ACTIVE
    assert journal.current_state().generation == 0

    reopened = journal.reopen_state(reason="new execution generation")

    assert reopened.mode is RunControlMode.ACTIVE
    assert reopened.generation == 1
    assert journal.current_state().generation == 1
    journal.close()


def test_context_and_decision_journals_are_scoped_and_append_only(tmp_path):
    journal = SQLiteControlJournal(tmp_path / "control.db", run_id="run-1")
    context = ContextResource(
        context_id="CTX-1", run_id="run-1", content="Try the alternate endpoint",
        scope="solver:w-1", max_bindings=1,
    )
    assert journal.append_context(context) == context
    assert journal.append_context(context.model_copy(update={"created_at": time.time()})) == context
    assert journal.context_resources() == [context]
    assert journal.expire_context("CTX-1", reason="consumed")
    assert not journal.expire_context("CTX-1", reason="duplicate")
    assert journal.context_resources() == []
    assert journal.context_resources(active_only=False) == [context]

    bindable = ContextResource(
        context_id="CTX-bind", run_id="run-1", content="two deliveries",
        max_bindings=2,
    )
    journal.append_context(bindable)
    assert journal.bind_context("CTX-bind", worker_id="w-1") is True
    assert journal.bind_context("CTX-bind", worker_id="w-1") is False
    assert journal.context_resources() == [bindable]
    assert journal.bind_context("CTX-bind", worker_id="w-2") is True
    assert journal.context_bindings("CTX-bind") == ["w-1", "w-2"]
    assert journal.context_resources() == []

    reservable = ContextResource(
        context_id="CTX-reserve", run_id="run-1", content="one shot",
        max_bindings=1,
    )
    journal.append_context(reservable)
    reservation = journal.reserve_context("CTX-reserve", worker_id="w-starting")
    assert reservation
    assert journal.context_bindings("CTX-reserve") == []
    assert journal.context_resources() == []
    assert journal.reserve_context("CTX-reserve", worker_id="w-racing") is None
    assert journal.release_context_reservation(
        "CTX-reserve", worker_id="w-starting", reservation_id=reservation)
    assert journal.context_resources() == [reservable]
    retry = journal.reserve_context("CTX-reserve", worker_id="w-retry")
    assert retry
    assert journal.commit_context_binding(
        "CTX-reserve", worker_id="w-retry", reservation_id=retry)
    assert journal.context_bindings("CTX-reserve") == ["w-retry"]
    assert journal.context_resources() == []

    standing = ContextResource(
        context_id="CTX-standing", run_id="run-1", content="persistent rule",
        standing=True, max_bindings=None,
    )
    journal.append_context(standing)
    first_standing = journal.reserve_context("CTX-standing", worker_id="w-reused")
    assert first_standing and journal.commit_context_binding(
        "CTX-standing", worker_id="w-reused", reservation_id=first_standing)
    second_standing = journal.reserve_context("CTX-standing", worker_id="w-reused")
    assert second_standing and second_standing != first_standing
    assert journal.commit_context_binding(
        "CTX-standing", worker_id="w-reused", reservation_id=second_standing)
    assert journal.context_bindings("CTX-standing") == ["w-reused", "w-reused"]
    assert standing in journal.context_resources()

    request = DecisionRequest(
        request_id="DR-1", run_id="run-1", worker_id="w-1",
        prompt="Provide the lab credential", blocking_scope="solver:w-1",
    )
    journal.append_decision_request(request)
    assert journal.decision_status("DR-1") is DecisionStatus.OPEN
    assert journal.decision_requests(open_only=True) == [request]
    answer = DecisionAnswer(
        answer_id="DA-1", request_id="DR-1", run_id="run-1",
        answer="secret://run-1/lab-credential",
    )
    journal.append_decision_answer(answer)
    assert journal.decision_status("DR-1") is DecisionStatus.ANSWERED
    assert journal.decision_requests(open_only=True) == []
    journal.close()


def test_context_reservation_crash_recovery_is_fail_closed_and_append_only(tmp_path):
    db = tmp_path / "recovery-control.db"
    journal = SQLiteControlJournal(db, run_id="run-1")
    finite = ContextResource(
        context_id="CTX-crash", run_id="run-1", content="one disclosure",
        max_bindings=1,
    )
    standing = ContextResource(
        context_id="CTX-crash-standing", run_id="run-1", content="standing rule",
        standing=True, max_bindings=None,
    )
    journal.append_context(finite)
    journal.append_context(standing)
    assert journal.reserve_context("CTX-crash", worker_id="w-dead")
    assert journal.reserve_context("CTX-crash-standing", worker_id="w-dead")
    journal.close()

    reopened = SQLiteControlJournal(db, run_id="run-1")
    recovered = reopened.recover_context_reservations()
    assert {row["context_id"] for row in recovered} == {
        "CTX-crash", "CTX-crash-standing"}
    # Finite secret-like delivery is consumed UNKNOWN, never replayed and never
    # falsely presented as a confirmed binding.
    assert reopened.context_bindings("CTX-crash") == []
    assert reopened.reserve_context("CTX-crash", worker_id="w-replacement") is None
    assert finite not in reopened.context_resources()
    # Unlimited standing context survives the generation boundary after the stale
    # reservation is terminalised.
    assert standing in reopened.context_resources()
    assert reopened.reserve_context(
        "CTX-crash-standing", worker_id="w-replacement")
    reopened.close()


def test_post_popen_unknown_consumes_even_if_reservation_was_released(tmp_path):
    journal = SQLiteControlJournal(tmp_path / "popen-unknown.db", run_id="run-1")
    resource = ContextResource(
        context_id="CTX-popen-unknown", run_id="run-1",
        content="single disclosure", max_bindings=1,
    )
    journal.append_context(resource)
    reservation = journal.reserve_context(resource.context_id, worker_id="w-1")
    assert reservation
    # A concurrent revocation path may close the reservation just before the Popen
    # callback reports that argv already crossed the process boundary.
    assert journal.release_context_reservation(
        resource.context_id, worker_id="w-1", reservation_id=reservation)
    assert journal.mark_context_delivery_unknown(
        resource.context_id, worker_id="w-1", reservation_id=reservation)
    assert journal.context_delivery_status(resource.context_id) == "delivery_unknown"
    assert journal.reserve_context(resource.context_id, worker_id="w-2") is None
    assert journal.context_bindings(resource.context_id) == []
    journal.close()


def test_context_commit_rechecks_explicit_revocation_and_ttl(tmp_path):
    journal = SQLiteControlJournal(tmp_path / "revocation.db", run_id="run-1")
    explicit = ContextResource(
        context_id="CTX-explicit", run_id="run-1", content="revoke me",
        max_bindings=1,
    )
    ttl = ContextResource(
        context_id="CTX-ttl", run_id="run-1", content="short lived",
        max_bindings=1, created_at=1.0, expires_at=10.0,
    )
    journal.append_context(explicit)
    journal.append_context(ttl)
    explicit_reservation = journal.reserve_context(
        explicit.context_id, worker_id="w-explicit", now=5.0)
    ttl_reservation = journal.reserve_context(
        ttl.context_id, worker_id="w-ttl", now=5.0)
    assert explicit_reservation and ttl_reservation
    assert journal.expire_context(explicit.context_id, now=6.0)
    assert journal.commit_context_binding(
        explicit.context_id, worker_id="w-explicit",
        reservation_id=explicit_reservation, now=7.0) is False
    assert journal.commit_context_binding(
        ttl.context_id, worker_id="w-ttl",
        reservation_id=ttl_reservation, now=11.0) is False
    assert journal.context_bindings(explicit.context_id) == []
    assert journal.context_bindings(ttl.context_id) == []
    assert journal.context_resources(now=11.0) == []
    journal.close()


class _RecordingPort:
    def __init__(self) -> None:
        self.calls: list[tuple[ControlCommand, tuple[str, ...], RunControlState]] = []
        self.active = 0
        self.max_active = 0

    async def apply(self, command, targets, desired):
        self.active += 1
        self.max_active = max(self.max_active, self.active)
        await asyncio.sleep(0.01)
        self.calls.append((command, tuple(w.worker_id for w in targets), desired))
        self.active -= 1
        if command.payload.get("explode"):
            raise RuntimeError("adapter exploded")
        return ApplyResult(
            state=EffectState.EFFECT_OBSERVED,
            target_ids=[w.worker_id for w in targets],
            detail="runtime acknowledged",
        )


class _SimulatedPostCommitCrash(BaseException):
    """Model process death, which normal actor exception handling cannot catch."""


class _CrashAfterCompanionJournal(SQLiteControlJournal):
    def append_persisted_with_companion(self, *args, **kwargs):
        result = super().append_persisted_with_companion(*args, **kwargs)
        raise _SimulatedPostCommitCrash("process died after SQLite commit")


async def _run_until_post_commit_crash(actor, command):
    submit = asyncio.create_task(actor.submit_and_wait(command))
    for _ in range(100):
        if actor._task is not None:  # deterministic test of the actor boundary
            break
        await asyncio.sleep(0)
    assert actor._task is not None
    with pytest.raises(_SimulatedPostCommitCrash):
        await actor._task
    submit.cancel()
    with pytest.raises(asyncio.CancelledError):
        await submit


@pytest.mark.asyncio
async def test_control_actor_is_single_writer_and_reports_real_effects(tmp_path):
    journal = SQLiteControlJournal(tmp_path / "control.db", run_id="run-1")
    registry = InMemoryWorkerRegistry()
    registry.register(WorkerRef(worker_id="w-claude", engine="claude"))
    registry.register(WorkerRef(worker_id="w-cursor", engine="cursor"))
    port = _RecordingPort()
    effects: list[EffectReceipt] = []
    actor = ControlActor(
        run_id="run-1", journal=journal, port=port, registry=registry,
        effect_sink=lambda receipt: effects.append(receipt),
    )
    first = ControlCommand(
        command_id="C-1", run_id="run-1", action="hint",
        scope="engine:claude", payload={"text": "try /admin"},
    )
    second = ControlCommand(
        command_id="C-2", run_id="run-1", action="focus",
        scope="engine:cursor", payload={"text": "focus on auth"},
    )
    r1, r2 = await asyncio.gather(
        actor.submit_and_wait(first), actor.submit_and_wait(second)
    )
    assert r1.state is r2.state is EffectState.EFFECT_OBSERVED
    assert port.max_active == 1
    assert [call[1] for call in port.calls] == [("w-claude",), ("w-cursor",)]
    assert [e.state for e in effects[:4]] == [
        EffectState.RECEIVED, EffectState.PERSISTED,
        EffectState.ROUTED, EffectState.EFFECT_OBSERVED,
    ]

    # Same command id is observational on retry and never reaches the port twice.
    before_projection_count = len(effects)
    duplicate_receipt = await actor.submit_and_wait(first)
    assert duplicate_receipt.state is EffectState.EFFECT_OBSERVED
    assert len(port.calls) == 2
    assert len(effects) == before_projection_count + 1
    assert effects[-1].receipt_id == duplicate_receipt.receipt_id
    await actor.close()
    journal.close()


@pytest.mark.asyncio
async def test_actor_desired_state_cas_deadline_and_failure_isolation(tmp_path):
    journal = SQLiteControlJournal(tmp_path / "control.db", run_id="run-1")
    port = _RecordingPort()
    actor = ControlActor(run_id="run-1", journal=journal, port=port)

    pause = ControlCommand(
        command_id="C-pause", run_id="run-1", action="pause", expected_generation=0,
    )
    assert (await actor.submit_and_wait(pause)).state is EffectState.EFFECT_OBSERVED
    assert actor.desired_state.mode is RunControlMode.QUIESCED
    assert actor.desired_state.generation == 1

    stale = ControlCommand(
        command_id="C-stale", run_id="run-1", action="resume", expected_generation=0,
    )
    assert (await actor.submit_and_wait(stale)).state is EffectState.REJECTED

    expired = ControlCommand(
        command_id="C-expired", run_id="run-1", action="hint",
        payload={"text": "late"}, deadline_at=time.time() - 1,
    )
    rejected = await actor.submit_and_wait(expired)
    assert rejected.state is EffectState.REJECTED
    assert rejected.metadata["code"] == "deadline_expired"

    broken = ControlCommand(
        command_id="C-broken", run_id="run-1", action="hint",
        payload={"text": "boom", "explode": True},
    )
    assert (await actor.submit_and_wait(broken)).state is EffectState.FAILED
    healthy = ControlCommand(
        command_id="C-healthy", run_id="run-1", action="hint",
        payload={"text": "continue"},
    )
    assert (await actor.submit_and_wait(healthy)).state is EffectState.EFFECT_OBSERVED

    await actor.close()
    journal.close()


@pytest.mark.asyncio
async def test_adapter_exception_plaintext_never_enters_control_journal(tmp_path):
    plaintext = "materialized-secret-in-exception"

    class _LeakyPort:
        async def apply(self, *_args, **_kwargs):
            raise RuntimeError(f"driver failed around {plaintext}")

    db_path = tmp_path / "redacted-adapter.db"
    journal = SQLiteControlJournal(db_path, run_id="run-1")
    actor = ControlActor(run_id="run-1", journal=journal, port=_LeakyPort())
    receipt = await actor.submit_and_wait(ControlCommand(
        command_id="C-adapter-secret", run_id="run-1", action="ask",
    ))
    assert receipt.state is EffectState.FAILED
    assert plaintext not in receipt.detail
    await actor.close()
    journal.close()
    for path in tmp_path.glob("redacted-adapter.db*"):
        assert plaintext.encode() not in path.read_bytes()


@pytest.mark.parametrize("last_state", [
    EffectState.RECEIVED,
    EffectState.PERSISTED,
    EffectState.ROUTED,
])
@pytest.mark.asyncio
async def test_actor_recovers_incomplete_command_as_unknown_without_reexecution(
    tmp_path, last_state,
):
    journal = SQLiteControlJournal(
        tmp_path / f"{last_state.value}.db", run_id="run-1")
    command = ControlCommand(
        command_id=f"C-recover-{last_state.value}", run_id="run-1",
        action="hint", payload={"text": "one shot"},
    )
    journal.append_command(command)
    if last_state in {EffectState.PERSISTED, EffectState.ROUTED}:
        journal.append_effect(EffectReceipt(
            command_id=command.command_id, run_id="run-1",
            state=EffectState.PERSISTED,
        ))
    if last_state is EffectState.ROUTED:
        journal.append_effect(EffectReceipt(
            command_id=command.command_id, run_id="run-1",
            state=EffectState.ROUTED,
        ))

    port = _RecordingPort()
    actor = ControlActor(run_id="run-1", journal=journal, port=port)
    recovered = await actor.submit_and_wait(
        command.model_copy(update={"created_at": time.time()}))
    assert recovered.state is EffectState.UNKNOWN
    assert recovered.metadata == {
        "code": "recovery_incomplete",
        "recovered_from": last_state.value,
        "reexecuted": False,
    }
    assert port.calls == []
    assert journal.effect_history(command.command_id)[-1] == recovered

    # A later retry observes the terminal UNKNOWN and neither appends nor executes.
    count = len(journal.effect_history(command.command_id))
    assert (await actor.submit_and_wait(command)).receipt_id == recovered.receipt_id
    assert len(journal.effect_history(command.command_id)) == count
    assert port.calls == []
    await actor.close()
    journal.close()


@pytest.mark.asyncio
async def test_atomic_persisted_standing_context_survives_post_commit_crash(
        tmp_path):
    path = tmp_path / "atomic-standing-crash.db"
    secret_ref = "secret://11111111111111111111111111111111"
    journal = _CrashAfterCompanionJournal(path, run_id="run-1")
    first_port = _RecordingPort()
    command = ControlCommand(
        command_id="C-crash-standing", run_id="run-1", action="hint",
        payload={"text": secret_ref, "standing": True},
    )
    actor = ControlActor(run_id="run-1", journal=journal, port=first_port)
    await _run_until_post_commit_crash(actor, command)

    assert journal.latest_effect(command.command_id).state is EffectState.PERSISTED
    rows = journal.context_resources()
    assert len(rows) == 1
    assert rows[0].content == secret_ref
    assert rows[0].standing is True
    assert first_port.calls == []
    journal.close()

    reopened = SQLiteControlJournal(path, run_id="run-1")
    retry_port = _RecordingPort()
    retry_actor = ControlActor(
        run_id="run-1", journal=reopened, port=retry_port)
    recovered = await retry_actor.submit_and_wait(command)
    assert recovered.state is EffectState.UNKNOWN
    assert recovered.metadata["reexecuted"] is False
    rows = reopened.context_resources()
    assert len(rows) == 1
    assert rows[0].content == secret_ref
    assert retry_port.calls == []
    await retry_actor.close()
    reopened.close()


@pytest.mark.asyncio
async def test_atomic_persisted_expiration_survives_post_commit_crash(tmp_path):
    path = tmp_path / "atomic-expiration-crash.db"
    journal = _CrashAfterCompanionJournal(path, run_id="run-1")
    standing = ContextResource(
        context_id="CTX-before-clear", run_id="run-1",
        content="standing instruction", standing=True, max_bindings=None,
        metadata={"source_command_id": "C-standing-source"},
    )
    journal.append_context(standing)
    command = ControlCommand(
        command_id="C-crash-clear", run_id="run-1",
        action="clear_standing",
    )
    first_port = _RecordingPort()
    actor = ControlActor(run_id="run-1", journal=journal, port=first_port)
    await _run_until_post_commit_crash(actor, command)

    persisted = journal.latest_effect(command.command_id)
    assert persisted.state is EffectState.PERSISTED
    assert persisted.metadata["expired_context_count"] == 1
    assert persisted.metadata["matched_source_command_ids"] == [
        "C-standing-source"]
    assert journal.context_resources() == []
    assert first_port.calls == []
    journal.close()

    reopened = SQLiteControlJournal(path, run_id="run-1")
    retry_port = _RecordingPort()
    retry_actor = ControlActor(
        run_id="run-1", journal=reopened, port=retry_port)
    recovered = await retry_actor.submit_and_wait(command)
    assert recovered.state is EffectState.UNKNOWN
    assert recovered.metadata["expired_context_count"] == 1
    assert recovered.metadata["matched_source_command_ids"] == [
        "C-standing-source"]
    assert reopened.context_resources() == []
    assert retry_port.calls == []
    await retry_actor.close()
    reopened.close()


@pytest.mark.asyncio
async def test_atomic_persisted_decision_answer_survives_post_commit_crash(
        tmp_path):
    path = tmp_path / "atomic-decision-crash.db"
    secret_ref = "secret://22222222222222222222222222222222"
    journal = _CrashAfterCompanionJournal(path, run_id="run-1")
    journal.append_decision_request(DecisionRequest(
        request_id="DR-crash-answer", run_id="run-1",
        worker_id="w-old", prompt="Need input",
        blocking_scope="solver:w-old",
    ))
    command = ControlCommand(
        command_id="C-crash-answer", run_id="run-1",
        action="answer_decision", scope="solver:w-old",
        payload={"request_id": "DR-crash-answer", "answer": secret_ref},
    )
    first_port = _RecordingPort()
    actor = ControlActor(run_id="run-1", journal=journal, port=first_port)
    await _run_until_post_commit_crash(actor, command)

    assert journal.latest_effect(command.command_id).state is EffectState.PERSISTED
    assert journal.decision_status("DR-crash-answer") is DecisionStatus.ANSWERED
    contexts = journal.context_resources()
    assert len(contexts) == 1
    assert contexts[0].content == secret_ref
    assert first_port.calls == []
    journal.close()

    reopened = SQLiteControlJournal(path, run_id="run-1")
    retry_port = _RecordingPort()
    retry_actor = ControlActor(
        run_id="run-1", journal=reopened, port=retry_port)
    recovered = await retry_actor.submit_and_wait(command)
    assert recovered.state is EffectState.UNKNOWN
    assert recovered.metadata["decision_closed"] is True
    assert reopened.decision_status(
        "DR-crash-answer") is DecisionStatus.ANSWERED
    contexts = reopened.context_resources()
    assert len(contexts) == 1
    assert contexts[0].content == secret_ref
    assert retry_port.calls == []
    await retry_actor.close()
    reopened.close()


@pytest.mark.asyncio
async def test_graceful_drain_sets_quiesced_desired_state(tmp_path):
    journal = SQLiteControlJournal(tmp_path / "control.db", run_id="run-1")
    actor = ControlActor(run_id="run-1", journal=journal, port=_RecordingPort())
    result = await actor.submit_and_wait(ControlCommand(
        run_id="run-1", action="graceful_drain", expected_generation=0,
    ))
    assert result.state is EffectState.EFFECT_OBSERVED
    assert actor.desired_state.mode is RunControlMode.QUIESCED
    assert actor.desired_state.generation == 1
    await actor.close()
    journal.close()


def test_admission_enforces_scope_identity_and_run_wide_actions():
    state = RunControlState(run_id="run-1")
    admission = ControlAdmission(challenge_id="challenge-1")
    with pytest.raises(AdmissionError, match="run scope"):
        admission.admit(ControlCommand(
            run_id="run-1", action="hint", scope="run:other",
            payload={"text": "x"}), state)
    with pytest.raises(AdmissionError, match="challenge scope"):
        admission.admit(ControlCommand(
            run_id="run-1", action="hint", scope="challenge:other",
            payload={"text": "x"}), state)

    for action in (
        "pause", "resume", "stop", "complete", "graceful_drain",
        "clear_standing", "reset_guidance", "mark_false",
    ):
        with pytest.raises(AdmissionError, match="run-scoped action"):
            admission.admit(ControlCommand(
                run_id="run-1", action=action, scope="solver:w-1"), state)

    assert admission.admit(ControlCommand(
        run_id="run-1", action="pause", scope="challenge:challenge-1"), state
    ).accepted
    assert admission.admit(ControlCommand(
        run_id="run-1", action="freeze", scope="solver:w-1"), state
    ).accepted
    assert admission.admit(ControlCommand(
        run_id="run-1", action="thaw", scope="solver:w-1"), state
    ).accepted

    terminated = RunControlState(
        run_id="run-1", mode=RunControlMode.TERMINATED)
    for command in (
        ControlCommand(run_id="run-1", action="clear_standing"),
        ControlCommand(run_id="run-1", action="reset_guidance"),
        ControlCommand(
            run_id="run-1", action="expire_context",
            payload={"context_id": "CTX-old"}),
    ):
        assert admission.admit(command, terminated).accepted


@pytest.mark.asyncio
async def test_clear_standing_expires_durable_context_rows(tmp_path):
    journal = SQLiteControlJournal(tmp_path / "clear.db", run_id="run-1")
    actor = ControlActor(run_id="run-1", journal=journal, port=_RecordingPort())
    await actor.submit_and_wait(ControlCommand(
        command_id="C-standing", run_id="run-1", action="hint",
        payload={"text": "persistent clue", "standing": True},
    ))
    resource = journal.context_resources()[0]
    assert resource.standing

    receipt = await actor.submit_and_wait(ControlCommand(
        command_id="C-clear", run_id="run-1", action="clear_standing"))

    assert receipt.state is EffectState.EFFECT_OBSERVED
    assert journal.context_resources() == []
    assert journal.context_delivery_status(resource.context_id) == "expired"
    await actor.close()
    journal.close()


@pytest.mark.asyncio
async def test_exact_clear_resolves_distinct_secret_refs_without_plaintext_persistence(
        tmp_path):
    db_path = tmp_path / "secret-exact-clear.db"
    old_ref = "secret://aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
    clear_ref = "secret://bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
    other_ref = "secret://cccccccccccccccccccccccccccccccc"
    missing_ref = "secret://dddddddddddddddddddddddddddddddd"
    values = {
        old_ref: "same sensitive standing value",
        clear_ref: "same sensitive standing value",
        other_ref: "different sensitive standing value",
    }

    def resolve(reference):
        return values[reference]

    journal = SQLiteControlJournal(db_path, run_id="run-1")
    actor = ControlActor(
        run_id="run-1", journal=journal, port=_RecordingPort(),
        secret_resolver=resolve,
    )
    await actor.submit_and_wait(ControlCommand(
        command_id="C-secret-standing", run_id="run-1", action="hint",
        payload={"text": old_ref, "standing": True},
    ))
    await actor.submit_and_wait(ControlCommand(
        command_id="C-other-standing", run_id="run-1", action="hint",
        payload={"text": other_ref, "standing": True},
    ))

    cleared = await actor.submit_and_wait(ControlCommand(
        command_id="C-secret-exact-clear", run_id="run-1",
        action="clear_standing", payload={"text": clear_ref},
    ))
    assert cleared.state is EffectState.EFFECT_OBSERVED
    assert cleared.metadata["expired_context_count"] == 1
    assert cleared.metadata["matched_source_command_ids"] == [
        "C-secret-standing"]
    active = journal.context_resources()
    assert [row.content for row in active] == [other_ref]
    operation = journal.standing_clear_operations()[-1]
    assert operation.eligible_standing_command_ids == ("C-secret-standing",)

    # Resolver failure is a fail-closed non-match, never a raw-ref comparison.
    failed_match = await actor.submit_and_wait(ControlCommand(
        command_id="C-secret-missing-clear", run_id="run-1",
        action="clear_standing", payload={"text": missing_ref},
    ))
    assert failed_match.metadata["expired_context_count"] == 0
    assert failed_match.metadata["matched_source_command_ids"] == []
    assert [row.content for row in journal.context_resources()] == [other_ref]

    await actor.close()
    journal.close()
    for path in tmp_path.glob("secret-exact-clear.db*"):
        raw = path.read_bytes()
        assert b"same sensitive standing value" not in raw
        assert b"different sensitive standing value" not in raw


@pytest.mark.asyncio
async def test_standing_clear_outbox_is_ordered_and_excludes_rejected_commands(
        tmp_path):
    journal = SQLiteControlJournal(tmp_path / "clear-outbox.db", run_id="run-1")
    actor = ControlActor(run_id="run-1", journal=journal, port=_RecordingPort())
    await actor.submit_and_wait(ControlCommand(
        command_id="C-before", run_id="run-1", action="hint",
        payload={"text": "old standing", "standing": True},
    ))
    await actor.submit_and_wait(ControlCommand(
        command_id="C-clear-outbox", run_id="run-1", action="clear_standing",
        payload={"text": "old standing"},
    ))
    await actor.submit_and_wait(ControlCommand(
        command_id="C-after", run_id="run-1", action="hint",
        payload={"text": "new standing", "standing": True},
    ))
    rejected = await actor.submit_and_wait(ControlCommand(
        command_id="C-rejected-clear", run_id="run-1",
        action="reset_guidance", expected_generation=99,
    ))
    assert rejected.state is EffectState.REJECTED

    operations = journal.standing_clear_operations()
    assert len(operations) == 1
    operation = operations[0]
    assert operation.command_id == "C-clear-outbox"
    assert operation.text == "old standing"
    assert operation.eligible_standing_command_ids == ("C-before",)
    assert operation.cutoff_before == operation.persisted_at
    later = next(
        row for row in journal.context_resources()
        if row.content == "new standing")
    assert later.created_at >= operation.cutoff_before

    await actor.close()
    journal.close()


@pytest.mark.asyncio
async def test_actor_mirrors_scoped_operator_context_without_evidence_authority(tmp_path):
    journal = SQLiteControlJournal(tmp_path / "control.db", run_id="run-1")
    actor = ControlActor(run_id="run-1", journal=journal, port=_RecordingPort())
    command = ControlCommand(
        command_id="C-context", run_id="run-1", action="focus",
        scope="engine:cursor",
        payload={"text": "audit the auth boundary", "max_bindings": 2, "ttl_s": 60},
    )
    assert (await actor.submit_and_wait(command)).state is EffectState.EFFECT_OBSERVED
    resources = journal.context_resources()
    assert len(resources) == 1
    assert resources[0].kind is ContextKind.OBJECTIVE
    assert str(resources[0].scope) == "engine:cursor"
    assert resources[0].max_bindings == 2
    assert resources[0].metadata["source_command_id"] == "C-context"

    secret = ControlCommand(
        command_id="C-secret-context", run_id="run-1", action="hint",
        payload={"text": "secret://0123456789abcdef0123456789abcdef"},
    )
    assert (await actor.submit_and_wait(secret)).state is EffectState.EFFECT_OBSERVED
    secret_rows = [r for r in journal.context_resources()
                   if r.metadata.get("source_command_id") == "C-secret-context"]
    assert secret_rows[0].kind is ContextKind.SECRET_REF
    assert secret_rows[0].taint is ContextTaint.SECRET_REFERENCE
    await actor.close()
    journal.close()


@pytest.mark.asyncio
async def test_worker_context_uses_exact_continuation_and_secret_redirect_stays_endpoint(
        tmp_path):
    from muteki.control import continuation_intent_id_for_command

    journal = SQLiteControlJournal(tmp_path / "control.db", run_id="run-1")
    registry = InMemoryWorkerRegistry()
    registry.register(WorkerRef(
        worker_id="w-old", engine="claude", intent_id="I-old",
        challenge_id="run-1"))
    actor = ControlActor(
        run_id="run-1", journal=journal, registry=registry,
        port=_RecordingPort())

    add = ControlCommand(
        command_id="C-add-worker", run_id="run-1", action="add_context",
        scope="worker:w-old", payload={"context": {"content": "private note"}},
    )
    await actor.submit_and_wait(add)
    added = next(r for r in journal.context_resources()
                 if r.metadata.get("source_command_id") == add.command_id)
    assert str(added.scope) == (
        f"intent:{continuation_intent_id_for_command(add.command_id)}")

    redirect = ControlCommand(
        command_id="C-secret-redirect", run_id="run-1", action="redirect",
        scope="worker:w-old",
        payload={"url": "secret://0123456789abcdef0123456789abcdef"},
    )
    await actor.submit_and_wait(redirect)
    endpoint = next(r for r in journal.context_resources()
                    if r.metadata.get("source_command_id") == redirect.command_id)
    assert endpoint.kind is ContextKind.ENDPOINT
    assert endpoint.taint is ContextTaint.SECRET_REFERENCE
    assert str(endpoint.scope) == (
        f"intent:{continuation_intent_id_for_command(redirect.command_id)}")

    add_secret_endpoint = ControlCommand(
        command_id="C-add-secret-endpoint", run_id="run-1", action="add_context",
        scope="worker:w-old", payload={"context": {
            "kind": "endpoint",
            "content": "secret://abcdefabcdefabcdefabcdefabcdefab",
        }},
    )
    await actor.submit_and_wait(add_secret_endpoint)
    added_endpoint = next(
        r for r in journal.context_resources()
        if r.metadata.get("source_command_id") == add_secret_endpoint.command_id)
    assert added_endpoint.kind is ContextKind.ENDPOINT
    assert added_endpoint.taint is ContextTaint.SECRET_REFERENCE

    await actor.close()
    journal.close()


@pytest.mark.asyncio
async def test_unmatched_worker_context_never_survives_for_a_reused_label(tmp_path):
    journal = SQLiteControlJournal(tmp_path / "unmatched.db", run_id="run-1")
    registry = InMemoryWorkerRegistry()
    actor = ControlActor(
        run_id="run-1", journal=journal, registry=registry,
        port=_RecordingPort())
    command = ControlCommand(
        command_id="C-stale-worker", run_id="run-1", action="hint",
        scope="worker:cli-claude", payload={"text": "private old execution note"},
    )

    receipt = await actor.submit_and_wait(command)
    assert receipt.state is EffectState.EFFECT_OBSERVED
    assert journal.context_resources(active_only=False) == []

    registry.register(WorkerRef(worker_id="cli-claude", engine="claude"))
    assert journal.context_resources() == []
    await actor.close()
    journal.close()
