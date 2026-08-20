from __future__ import annotations

import asyncio

import pytest

from muteki.epistemic.sqlite_store import (
    CommandEvent,
    EpistemicSQLiteStore,
    IntegrityError,
    ProjectionMutation,
)
from muteki.runtime.admission import AdmissionRequest, SearchAdmission
from muteki.runtime.contracts import (
    AttemptIdentity,
    AttemptPermit,
    EffectClass,
    ExecutionScope,
    LeaseIdentity,
)
from muteki.runtime.controller import BootRecoveryCapability, LiveHealthGuard
from muteki.runtime.supervisor import LaunchRejected, RunSupervisor


def _runtime(tmp_path):
    store = EpistemicSQLiteStore.create(
        path=tmp_path / "supervisor.db",
        run_id="run-1",
        manifest_digest="a" * 64,
    )
    store.commit_command(
        command_id="ready",
        idempotency_key="ready",
        command_payload={},
        events=[CommandEvent("e-ready", "BOOT_READY", "host", 1)],
        committed_at_ns=1,
    )
    store.commit_command(
        command_id="start",
        idempotency_key="start",
        command_payload={},
        events=[
            CommandEvent(
                "e-start",
                "START_EXECUTION",
                "host",
                2,
                {"execution_generation": 1, "run_fence_epoch": 1},
            )
        ],
        projection_mutations=[ProjectionMutation(
            "execution_start_guard",
            {"execution_generation": 1, "run_fence_epoch": 1},
        )],
        authority_capability=store._lifecycle_commit_capability,
        committed_at_ns=2,
    )
    guard = LiveHealthGuard()
    capability = BootRecoveryCapability(1, 1, "owner")
    guard.begin_boot_finalize(capability)
    guard.open_admission(capability=capability, attestation_digest="b" * 64)
    admission = SearchAdmission(store=store, guard=guard)
    admission.create_branch(branch_id="b1", max_attempts=4, occurred_at_ns=3)
    admission.create_budget_account(
        account_id="global",
        limits={"attempts": 4, "tokens": 100},
        occurred_at_ns=4,
    )
    return store, admission


def _admit(
    admission,
    scope,
    *,
    attempt_id="a1",
    permit_id="p1",
    expires=100,
):
    attempt = AttemptIdentity(scope, "b1", attempt_id, 1)
    lease = LeaseIdentity(attempt, f"lease-{attempt_id}", 1, 1)
    return admission.admit(
        AdmissionRequest(
            attempt=attempt,
            lease=lease,
            permit_id=permit_id,
            account_id="global",
            requested_budget={"attempts": 1, "tokens": 20},
            conflict_keys=(),
            effect_class=EffectClass.PURE,
            fingerprint=f"fingerprint-{attempt_id}",
            policy_digest="c" * 64,
            expires_at_ns=expires,
        ),
        occurred_at_ns=5,
    )


def _fabricated_permit(scope):
    attempt = AttemptIdentity(scope, "b1", "fabricated", 1)
    lease = LeaseIdentity(attempt, "lease-fabricated", 1, 1)
    return AttemptPermit(
        "fabricated",
        lease,
        "c" * 64,
        ("fabricated:global",),
        EffectClass.PURE,
        100,
        {
            "account_id": "global",
            "conflict_keys": (),
            "fingerprint": "fingerprint-fabricated",
            "requested_budget": {"attempts": 1, "tokens": 20},
        },
    )


async def test_supervisor_resolves_canonical_permit_and_writes_terminal_receipt(
    tmp_path,
):
    store, admission = _runtime(tmp_path)
    scope = ExecutionScope("run-1", 1, 1)
    permit = _admit(admission, scope)
    supervisor = RunSupervisor(store=store, scope=scope)

    async def work():
        await asyncio.sleep(0)
        return 42

    task = supervisor.spawn_owned(permit, work, now_ns=10)
    assert await task == 42
    assert supervisor.active_count == 0
    kinds = [row["kind"] for row in store.event_rows()]
    assert kinds[-2:] == ["WORKER_LAUNCH_PREPARED", "WORKER_TERMINAL"]
    launch = store.event_rows(kind="WORKER_LAUNCH_PREPARED")[0]["payload"]
    assert launch["permit_digest"] == permit.digest
    assert (
        launch["admission_event_digest"]
        == store.event_rows(kind="ATTEMPT_ADMITTED")[0]["event_digest"]
    )


async def test_supervisor_rejects_stale_or_expired_canonical_permit(tmp_path):
    store, admission = _runtime(tmp_path)
    scope = ExecutionScope("run-1", 1, 1)
    expired = _admit(admission, scope, expires=10)
    supervisor = RunSupervisor(store=store, scope=scope)
    with pytest.raises(LaunchRejected, match="expired"):
        supervisor.spawn_owned(expired, lambda: asyncio.sleep(0), now_ns=10)

    current = _admit(
        admission,
        scope,
        attempt_id="a2",
        permit_id="p2",
        expires=100,
    )
    stale_scope = ExecutionScope("run-1", 2, 2)
    with pytest.raises(LaunchRejected, match="stale"):
        RunSupervisor(store=store, scope=stale_scope).spawn_owned(
            current, lambda: asyncio.sleep(0), now_ns=11
        )
    assert store.event_rows(kind="WORKER_LAUNCH_PREPARED") == ()


async def test_supervisor_rechecks_current_run_state_after_admission(tmp_path):
    store, admission = _runtime(tmp_path)
    scope = ExecutionScope("run-1", 1, 1)
    permit = _admit(admission, scope)
    store.commit_command(
        command_id="pause",
        idempotency_key="pause",
        command_payload={},
        events=[CommandEvent("event:pause", "SEARCH_PAUSED", "host", 9)],
        committed_at_ns=9,
    )
    invoked = False

    async def work():
        nonlocal invoked
        invoked = True

    with pytest.raises(LaunchRejected, match="forbids"):
        RunSupervisor(store=store, scope=scope).spawn_owned(permit, work, now_ns=10)
    await asyncio.sleep(0)
    assert invoked is False
    assert store.event_rows(kind="WORKER_LAUNCH_PREPARED") == ()


async def test_fabricated_permit_is_rejected_before_factory_is_invoked(tmp_path):
    store, _ = _runtime(tmp_path)
    scope = ExecutionScope("run-1", 1, 1)
    supervisor = RunSupervisor(store=store, scope=scope)
    invoked = False

    async def work():
        nonlocal invoked
        invoked = True

    with pytest.raises(LaunchRejected, match="canonical admission"):
        supervisor.spawn_owned(_fabricated_permit(scope), work, now_ns=10)
    await asyncio.sleep(0)
    assert invoked is False
    assert store.event_rows(kind="WORKER_LAUNCH_PREPARED") == ()


async def test_permit_body_mismatch_is_rejected_before_factory(tmp_path):
    store, admission = _runtime(tmp_path)
    scope = ExecutionScope("run-1", 1, 1)
    canonical = _admit(admission, scope)
    changed = AttemptPermit(
        canonical.permit_id,
        canonical.lease,
        "d" * 64,
        canonical.reservation_ids,
        canonical.effect_class,
        canonical.expires_at_ns,
        canonical.constraints,
    )
    invoked = False

    async def work():
        nonlocal invoked
        invoked = True

    with pytest.raises(LaunchRejected, match="differs"):
        RunSupervisor(store=store, scope=scope).spawn_owned(changed, work, now_ns=10)
    await asyncio.sleep(0)
    assert invoked is False


async def test_completed_or_unknown_attempt_cannot_launch(tmp_path):
    store, admission = _runtime(tmp_path)
    scope = ExecutionScope("run-1", 1, 1)
    completed = _admit(admission, scope)
    admission.settle(
        attempt_id="a1",
        actual_usage={"attempts": 1, "tokens": 1},
        settlement_revision=1,
        occurred_at_ns=6,
    )
    invoked = False

    async def work():
        nonlocal invoked
        invoked = True

    with pytest.raises(LaunchRejected, match="completed"):
        RunSupervisor(store=store, scope=scope).spawn_owned(completed, work, now_ns=10)

    unknown = _admit(admission, scope, attempt_id="a2", permit_id="p2", expires=100)
    admission.hold_unknown_usage(attempt_id="a2", revision=1, occurred_at_ns=7)
    with pytest.raises(LaunchRejected, match="UNKNOWN"):
        RunSupervisor(store=store, scope=scope).spawn_owned(unknown, work, now_ns=10)
    await asyncio.sleep(0)
    assert invoked is False


async def test_launch_claim_is_one_shot_across_supervisor_restart(tmp_path):
    store, admission = _runtime(tmp_path)
    scope = ExecutionScope("run-1", 1, 1)
    permit = _admit(admission, scope)
    first = RunSupervisor(store=store, scope=scope)

    async def work():
        return "done"

    assert await first.spawn_owned(permit, work, now_ns=10) == "done"
    invoked = False

    async def duplicate():
        nonlocal invoked
        invoked = True

    reopened = EpistemicSQLiteStore.open(store.path)
    try:
        with pytest.raises(LaunchRejected, match="already launched"):
            RunSupervisor(store=reopened, scope=scope).spawn_owned(
                permit, duplicate, now_ns=11
            )
        await asyncio.sleep(0)
        assert invoked is False
        assert len(reopened.event_rows(kind="WORKER_LAUNCH_PREPARED")) == 1
    finally:
        reopened.close()


async def test_launch_marker_without_semantic_cas_is_rejected(tmp_path):
    store, admission = _runtime(tmp_path)
    scope = ExecutionScope("run-1", 1, 1)
    permit = _admit(admission, scope)
    with pytest.raises(IntegrityError, match="attempt_launch semantic mutation"):
        store.commit_command(
            command_id="legacy-launch-owner",
            idempotency_key="legacy-launch-owner",
            command_payload={"permit_id": permit.permit_id},
            events=[CommandEvent(
                "event:legacy-launch-owner", "WORKER_LAUNCH_PREPARED",
                "legacy-supervisor", 9, {"permit_id": permit.permit_id},
            )],
            committed_at_ns=9,
        )
    assert store.event_rows(kind="WORKER_LAUNCH_PREPARED") == ()


async def test_preentry_cancel_commits_exactly_one_unknown(tmp_path):
    store, admission = _runtime(tmp_path)
    scope = ExecutionScope("run-1", 1, 1)
    permit = _admit(admission, scope)
    supervisor = RunSupervisor(store=store, scope=scope)
    invoked = False

    async def attempts_uncancel():
        nonlocal invoked
        invoked = True
        current = asyncio.current_task()
        assert current is not None
        current.uncancel()
        return "must-not-run"

    task = supervisor.spawn_owned(permit, attempts_uncancel, now_ns=10)
    await supervisor.emergency_stop()
    with pytest.raises(asyncio.CancelledError):
        await task

    assert invoked is False
    assert supervisor.active_count == 0
    assert len(store.event_rows(kind="WORKER_UNKNOWN")) == 1
    assert len(store.event_rows(kind="WORKER_TERMINAL")) == 0


async def test_emergency_stop_produces_unknown_receipt(tmp_path):
    store, admission = _runtime(tmp_path)
    scope = ExecutionScope("run-1", 1, 1)
    permit = _admit(admission, scope)
    supervisor = RunSupervisor(store=store, scope=scope)

    async def forever():
        await asyncio.Event().wait()

    supervisor.spawn_owned(permit, forever, now_ns=10)
    await asyncio.sleep(0)
    await supervisor.emergency_stop()
    assert supervisor.active_count == 0
    assert len(store.event_rows(kind="WORKER_UNKNOWN")) == 1
    assert len(store.event_rows(kind="WORKER_TERMINAL")) == 0


async def test_emergency_stop_stays_unknown_when_worker_swallows_cancel(tmp_path):
    store, admission = _runtime(tmp_path)
    scope = ExecutionScope("run-1", 1, 1)
    permit = _admit(admission, scope)
    supervisor = RunSupervisor(store=store, scope=scope)
    entered = asyncio.Event()

    async def swallows_cancel():
        entered.set()
        try:
            await asyncio.Event().wait()
        except asyncio.CancelledError:
            return "swallowed-and-returned"

    task = supervisor.spawn_owned(permit, swallows_cancel, now_ns=10)
    await entered.wait()
    await supervisor.emergency_stop()

    assert task.cancelled()
    assert len(store.event_rows(kind="WORKER_UNKNOWN")) == 1
    assert len(store.event_rows(kind="WORKER_TERMINAL")) == 0


async def test_direct_cancel_latch_survives_worker_uncancel(tmp_path):
    store, admission = _runtime(tmp_path)
    scope = ExecutionScope("run-1", 1, 1)
    permit = _admit(admission, scope)
    supervisor = RunSupervisor(store=store, scope=scope)
    entered = asyncio.Event()

    async def uncancels_and_returns():
        entered.set()
        try:
            await asyncio.Event().wait()
        except asyncio.CancelledError:
            current = asyncio.current_task()
            assert current is not None
            assert current.uncancel() == 0
            return "uncancelled-and-returned"

    task = supervisor.spawn_owned(permit, uncancels_and_returns, now_ns=10)
    await entered.wait()
    assert task.cancel("external-owner-stop") is True

    with pytest.raises(asyncio.CancelledError):
        await task
    assert task.cancelled()
    assert len(store.event_rows(kind="WORKER_UNKNOWN")) == 1
    assert len(store.event_rows(kind="WORKER_TERMINAL")) == 0


async def test_terminal_receipt_failure_remains_a_drain_blocker(
    tmp_path, monkeypatch
):
    store, admission = _runtime(tmp_path)
    scope = ExecutionScope("run-1", 1, 1)
    permit = _admit(admission, scope)
    supervisor = RunSupervisor(store=store, scope=scope)
    original = store.commit_command

    def fail_terminal(**kwargs):
        if any(
            event.kind == "WORKER_TERMINAL"
            for event in kwargs.get("events", ())
        ):
            raise RuntimeError("terminal fault")
        return original(**kwargs)

    monkeypatch.setattr(store, "commit_command", fail_terminal)

    async def work():
        return 1

    task = supervisor.spawn_owned(permit, work, now_ns=10)
    with pytest.raises(RuntimeError, match="terminal fault"):
        await task
    assert supervisor.active_count == 0
    with pytest.raises(LaunchRejected, match="terminal receipt is unresolved"):
        await supervisor.drain()
