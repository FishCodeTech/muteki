"""Web boundary coverage for durable operator control."""

from __future__ import annotations

import asyncio
import os
from pathlib import Path

import httpx
import pytest

from apps.web.control_adapter import QueueControlPort
from apps.web.run_manager import RunManager
from apps.web.server import create_app
from muteki.control import (
    ControlAction,
    ControlCommand,
    DecisionStatus,
    EffectState,
    RunControlState,
    StateConflict,
    WorkerRef,
)
from muteki.core.events import Event, EventType, hitl_request_payload


pytestmark = pytest.mark.asyncio


def _app(mgr: RunManager, monkeypatch: pytest.MonkeyPatch):
    monkeypatch.delenv("MUTEKI_WEB_PASSWORD", raising=False)
    return create_app(mgr)


async def _ack_one(run, *, detail: str = "effect confirmed") -> dict:
    wire = await asyncio.wait_for(run.hitl.get(), timeout=2)
    acknowledgement = wire.pop("_control_ack")
    acknowledgement.set_result({
        "state": "effect_observed",
        "detail": detail,
        "target_ids": [],
    })
    return wire


async def test_duplicate_start_cannot_overwrite_live_generation_owner(tmp_path):
    mgr = RunManager(sessions_root=tmp_path / "sessions")
    first_started = asyncio.Event()
    release_first = asyncio.Event()
    second_started = asyncio.Event()

    async def first_driver(_run):
        first_started.set()
        await release_first.wait()

    async def second_driver(_run):
        second_started.set()

    run = await mgr.start("run-duplicate-start", first_driver)
    await first_started.wait()
    first_task = run.task
    run.worker_registry.register(WorkerRef(
        worker_id="cli-live-owner", engine="claude"))

    with pytest.raises(StateConflict, match="already running"):
        await mgr.start(run.run_id, second_driver)
    assert run.task is first_task
    assert not second_started.is_set()
    assert {row.worker_id for row in run.worker_registry.snapshot()} == {
        "cli-live-owner"}

    release_first.set()
    await first_task
    await mgr.shutdown()


async def test_start_route_rejects_duplicate_before_mutating_rail(
        tmp_path, monkeypatch):
    mgr = RunManager(sessions_root=tmp_path / "sessions")
    app = _app(mgr, monkeypatch)
    async with httpx.AsyncClient(
        transport=httpx.ASGITransport(app=app), base_url="http://test",
    ) as client:
        first = await client.post(
            "/api/runs/run-route-duplicate/start",
            json={"kind": "idle", "challenge": {
                "name": "original name", "category": "web"}},
        )
        assert first.status_code == 200
        run = mgr.get("run-route-duplicate")
        assert run is not None
        run.flag = "flag{owned-by-live-generation}"
        run.flags = [run.flag]
        run.solved = True

        duplicate = await client.post(
            "/api/runs/run-route-duplicate/start",
            json={"kind": "idle", "challenge": {
                "name": "must not clobber", "category": "crypto"}},
        )
        assert duplicate.status_code == 409
        assert run.name == "original name"
        assert run.category == "web"
        assert run.flag == "flag{owned-by-live-generation}"
        assert run.solved is True
    await mgr.shutdown()


async def test_delete_fence_rejects_start_while_cancel_owner_unwinds(tmp_path):
    mgr = RunManager(sessions_root=tmp_path / "sessions")
    first_started = asyncio.Event()
    cancel_seen = asyncio.Event()
    release_cancel = asyncio.Event()
    replacement_started = asyncio.Event()

    async def first_driver(_run):
        first_started.set()
        try:
            await asyncio.Future()
        except asyncio.CancelledError:
            cancel_seen.set()
            await release_cancel.wait()
            raise

    async def replacement_driver(_run):
        replacement_started.set()

    run = await mgr.start("run-delete-start-race", first_driver)
    await first_started.wait()
    deletion = asyncio.create_task(mgr.delete(run.run_id))
    await cancel_seen.wait()

    with pytest.raises(StateConflict, match="transition is in progress"):
        await mgr.start(run.run_id, replacement_driver)
    assert not replacement_started.is_set()
    release_cancel.set()
    assert await deletion is True
    assert run.run_id not in mgr.runs


async def test_shutdown_fence_rejects_new_start_before_snapshot_settles(tmp_path):
    mgr = RunManager(sessions_root=tmp_path / "sessions")
    first_started = asyncio.Event()
    cancel_seen = asyncio.Event()
    release_cancel = asyncio.Event()
    replacement_started = asyncio.Event()

    async def first_driver(_run):
        first_started.set()
        try:
            await asyncio.Future()
        except asyncio.CancelledError:
            cancel_seen.set()
            await release_cancel.wait()
            raise

    async def replacement_driver(_run):
        replacement_started.set()

    await mgr.start("run-shutdown-start-race", first_driver)
    await first_started.wait()
    shutdown = asyncio.create_task(mgr.shutdown())
    await cancel_seen.wait()

    with pytest.raises(StateConflict, match="shutting down"):
        await mgr.start("run-after-shutdown", replacement_driver)
    assert not replacement_started.is_set()
    release_cancel.set()
    await shutdown


async def test_resolve_driver_crash_persists_terminal_event_for_rehydrate(
        tmp_path, monkeypatch):
    import apps.web.drivers as web_drivers

    sessions = tmp_path / "sessions"
    mgr = RunManager(sessions_root=sessions)
    run = mgr.create("run-resolve-crash")
    await run.bus.emit(Event(
        event_type=EventType.RUN_STARTED, run_id=run.run_id,
        payload={"challenge": {"name": "resolve crash", "category": "web"}},
    ))
    await run.bus.emit(Event(
        event_type=EventType.RUN_FINISHED, run_id=run.run_id,
        payload={"solved": False, "reason": "initial attempt"},
    ))
    await run.bus.close()

    async def crashing_driver(_run):
        raise RuntimeError("injected resolve driver crash")

    monkeypatch.setattr(
        web_drivers, "build_driver", lambda _body, *, mgr: crashing_driver)
    assert await mgr.resolve(run.run_id, {}) is True
    resolve_task = run.task
    assert resolve_task is not None
    await resolve_task

    events = [event async for event in run.store.replay(run.run_id)]
    assert events[-2].event_type is EventType.RUN_REOPENED
    assert events[-1].event_type is EventType.RUN_FINISHED
    assert events[-1].payload["reason"] == "runtime_failure"
    assert run.finished is True

    rehydrated = RunManager(sessions_root=sessions)
    restored = rehydrated.get(run.run_id)
    assert restored is not None
    assert restored.started is True
    assert restored.finished is True
    await mgr.shutdown()
    await rehydrated.shutdown()


async def test_retention_reports_delete_only_after_destructive_commit(
        tmp_path, monkeypatch):
    mgr = RunManager(sessions_root=tmp_path / "sessions")
    run = mgr.create("run-retention-owned")
    run.started = True
    mgr.set_archived(run.run_id, True, now=10.0)
    monkeypatch.setattr(mgr, "_last_activity", lambda _run: 1.0)

    async def _refuse_delete(_run_id):
        return False

    monkeypatch.setattr(mgr, "delete", _refuse_delete)
    result = await mgr.retention_sweep(
        now=100.0, archive_after_s=10.0, delete_after_s=20.0)
    assert result["deleted"] == []
    assert run.run_id in mgr.runs
    await mgr.shutdown()


async def test_control_api_persists_idempotently_and_enforces_cas(tmp_path, monkeypatch):
    mgr = RunManager(sessions_root=tmp_path / "sessions")
    run = mgr.create("run-control")
    run.task = asyncio.create_task(asyncio.sleep(30))
    app = _app(mgr, monkeypatch)
    consumer = asyncio.create_task(_ack_one(run))

    body = {
        "command_id": "C-fixed",
        "action": "pause",
        "target": "global",
    }
    async with httpx.AsyncClient(
        transport=httpx.ASGITransport(app=app), base_url="http://test",
    ) as client:
        first = await client.post("/api/runs/run-control/control", json=body)
        assert first.status_code == 200
        assert first.json()["status"] == "persisted"
        assert first.json()["command_id"] == "C-fixed"

        wire = await consumer
        assert wire["command_id"] == "C-fixed"
        assert wire["action"] == "pause"
        await run.control_actor.join()

        duplicate = await client.post("/api/runs/run-control/control", json=body)
        assert duplicate.status_code == 200
        assert duplicate.json()["status"] == "effect_observed"
        assert run.hitl.empty(), "an idempotent retry must not route twice"

        conflict = await client.post(
            "/api/runs/run-control/control",
            json={**body, "action": "freeze"},
        )
        assert conflict.status_code == 409

        stale = await client.post(
            "/api/runs/run-control/control",
            json={"command_id": "C-stale", "action": "resume",
                  "expected_generation": 0},
        )
        assert stale.status_code == 409
        assert stale.json()["status"] == "rejected"
        assert stale.json()["code"] == "generation_conflict"

    db = mgr.coordinator_control_dir(run.run_id) / "control.db"
    assert db.exists()
    assert run.control_journal.current_state().generation == 1
    run.task.cancel()
    await mgr.shutdown()


async def test_concurrent_idempotent_secret_retries_mint_one_reference(
    tmp_path, monkeypatch,
):
    """Compilation is part of admission: retries must not race SecretStore.put."""
    mgr = RunManager(sessions_root=tmp_path / "sessions")
    run = mgr.create("run-concurrent-secret")
    run.task = asyncio.create_task(asyncio.sleep(30))
    consumer = asyncio.create_task(_ack_one(run))
    body = {
        "command_id": "C-concurrent-secret",
        "action": "hint",
        "target": "global",
        "text": "password=only-one-copy",
    }

    first, second = await asyncio.gather(
        mgr.post_control(run.run_id, dict(body)),
        mgr.post_control(run.run_id, dict(body)),
    )
    wire = await consumer
    await run.control_actor.join()

    assert first["command_id"] == second["command_id"] == body["command_id"]
    assert wire["command_id"] == body["command_id"]
    assert run.hitl.empty(), "one command_id must cross the routing boundary once"
    assert run.control_secrets is not None
    assert len(run.control_secrets.list()) == 1
    command = run.control_journal.get_command(body["command_id"])
    assert command is not None
    assert len(command.payload.get("secret_refs") or []) == 1

    run.task.cancel()
    await mgr.shutdown()


async def test_control_api_rejects_unknown_action_payload_and_run(tmp_path, monkeypatch):
    mgr = RunManager(sessions_root=tmp_path / "sessions")
    mgr.create("run-validation")
    app = _app(mgr, monkeypatch)
    async with httpx.AsyncClient(
        transport=httpx.ASGITransport(app=app), base_url="http://test",
    ) as client:
        bad_action = await client.post(
            "/api/runs/run-validation/control", json={"action": "freezezzz"})
        assert bad_action.status_code == 422
        bad_payload = await client.post(
            "/api/runs/run-validation/control",
            json={"action": "hint", "payload": ["not", "an", "object"]},
        )
        assert bad_payload.status_code == 422
        missing = await client.post(
            "/api/runs/no-such-run/control", json={"action": "pause"})
        assert missing.status_code == 404
    await mgr.shutdown()


async def test_control_timeout_is_unknown_not_applied_live(tmp_path, monkeypatch):
    monkeypatch.setenv("MUTEKI_CONTROL_ACK_TIMEOUT", "0.02")
    mgr = RunManager(sessions_root=tmp_path / "sessions")
    run = mgr.create("run-timeout")
    run.task = asyncio.create_task(asyncio.sleep(30))
    app = _app(mgr, monkeypatch)
    async with httpx.AsyncClient(
        transport=httpx.ASGITransport(app=app), base_url="http://test",
    ) as client:
        response = await client.post(
            "/api/runs/run-timeout/control",
            json={"command_id": "C-timeout", "action": "pause"},
        )
        assert response.status_code == 200
        assert response.json()["status"] == "persisted"
    await run.control_actor.join()
    latest = run.control_journal.latest_effect("C-timeout")
    assert latest is not None and latest.state is EffectState.UNKNOWN
    assert run.hitl.empty(), "a timed-out envelope must not execute on a later drain"
    assert "applied_live" not in str(run.store.load_all(run.run_id))
    run.task.cancel()
    await mgr.shutdown()


async def test_control_timeout_after_runtime_claim_waits_for_real_effect():
    """Once dequeued, a command may finish after the nominal ACK deadline, but
    the journal-facing result must wait for that effect instead of closing UNKNOWN
    and allowing a late mutation."""
    inbox: asyncio.Queue = asyncio.Queue()
    port = QueueControlPort(inbox=inbox, is_live=lambda: True, ack_timeout=0.01)
    command = ControlCommand(
        command_id="C-claimed-slow", run_id="run-claimed", action="pause")

    async def consume():
        wire = await inbox.get()
        wire["_control_started"] = True
        await asyncio.sleep(0.05)
        wire["_control_ack"].set_result({
            "state": "effect_observed", "detail": "runtime state changed"})
        inbox.task_done()

    consumer = asyncio.create_task(consume())
    result = await port.apply(
        command, (), RunControlState(run_id="run-claimed"))
    await consumer
    assert result.state is EffectState.EFFECT_OBSERVED
    assert result.detail == "runtime state changed"


async def test_queue_control_port_awaits_async_standby_boundary():
    observed: list[str] = []

    async def on_standby(wire):
        await asyncio.sleep(0)
        observed.append(wire["action"])
        return {
            "state": "effect_observed",
            "detail": "async standby boundary completed",
        }

    port = QueueControlPort(
        inbox=asyncio.Queue(), is_live=lambda: False,
        on_standby=on_standby,
    )
    result = await port.apply(
        ControlCommand(command_id="C-async-standby", run_id="run-async",
                       action="writeup"),
        (), RunControlState(run_id="run-async"),
    )
    assert observed == ["writeup"]
    assert result.state is EffectState.EFFECT_OBSERVED


async def test_stop_standby_calls_real_cancel_then_awaits_delayed_unwind(
    tmp_path, monkeypatch
):
    monkeypatch.setenv("MUTEKI_STANDBY_CANCEL_TIMEOUT", "0.5")
    mgr = RunManager(sessions_root=tmp_path / "sessions")
    run = mgr.create("run-stop-standby")
    entered = asyncio.Event()
    cleaned = asyncio.Event()
    order: list[str] = []

    async def standby():
        entered.set()
        try:
            await asyncio.Event().wait()
        except asyncio.CancelledError:
            order.append("task_cancelled")
            assert order[0] == "worker_cancel", "process boundary must be first"
            await asyncio.sleep(0.04)
            cleaned.set()
            raise

    run.standby_task = asyncio.create_task(standby())
    await entered.wait()

    def cancel_worker():
        order.append("worker_cancel")

    def runtime_exited():
        return cleaned.is_set()

    async def wait_runtime_exit(timeout):
        try:
            await asyncio.wait_for(cleaned.wait(), timeout=timeout)
        except asyncio.TimeoutError:
            return False
        return True

    run.standby_cancel = cancel_worker
    run.standby_runtime_exited = runtime_exited
    run.standby_wait_runtime_exit = wait_runtime_exit
    response = await mgr.post_control(
        run.run_id, {"command_id": "C-stop-standby", "action": "stop"})
    assert response["status"] == "persisted"
    await run.control_actor.join()

    effect = run.control_journal.latest_effect("C-stop-standby")
    assert effect is not None
    assert effect.state is EffectState.EFFECT_OBSERVED
    assert cleaned.is_set() and run.standby_task.done()
    assert order[:2] == ["worker_cancel", "task_cancelled"]
    run.standby_cancel = None
    run.standby_runtime_exited = None
    run.standby_wait_runtime_exit = None
    await mgr.shutdown()


async def test_stop_standby_timeout_is_partial_not_fake_success(
    tmp_path, monkeypatch
):
    monkeypatch.setenv("MUTEKI_STANDBY_CANCEL_TIMEOUT", "0.01")
    mgr = RunManager(sessions_root=tmp_path / "sessions")
    run = mgr.create("run-stop-timeout")
    entered = asyncio.Event()
    release_cleanup = asyncio.Event()
    process_cancelled = False

    async def standby():
        entered.set()
        try:
            await asyncio.Event().wait()
        except asyncio.CancelledError:
            assert process_cancelled
            await release_cleanup.wait()
            raise

    run.standby_task = asyncio.create_task(standby())
    await entered.wait()

    def cancel_worker():
        nonlocal process_cancelled
        process_cancelled = True

    def runtime_exited():
        return run.standby_task.done() if run.standby_task is not None else False

    async def wait_runtime_exit(timeout):
        deadline = asyncio.get_running_loop().time() + timeout
        while not runtime_exited() and asyncio.get_running_loop().time() < deadline:
            await asyncio.sleep(0.001)
        return runtime_exited()

    run.standby_cancel = cancel_worker
    run.standby_runtime_exited = runtime_exited
    run.standby_wait_runtime_exit = wait_runtime_exit
    await mgr.post_control(
        run.run_id, {"command_id": "C-stop-timeout", "action": "force_cancel"})
    await run.control_actor.join()

    effect = run.control_journal.latest_effect("C-stop-timeout")
    assert effect is not None
    assert effect.state is EffectState.PARTIAL
    assert effect.metadata["worker_cancel_delivered"] is True
    assert effect.metadata["task_done"] is False
    assert not run.standby_task.done()

    release_cleanup.set()
    await asyncio.gather(run.standby_task, return_exceptions=True)
    run.standby_cancel = None
    run.standby_runtime_exited = None
    run.standby_wait_runtime_exit = None
    await mgr.shutdown()


async def test_standby_cancel_exception_text_never_reaches_durable_state(
    tmp_path, monkeypatch,
):
    monkeypatch.setenv("MUTEKI_STANDBY_CANCEL_TIMEOUT", "0.2")
    mgr = RunManager(sessions_root=tmp_path / "sessions")
    run = mgr.create("run-secret-cancel-error")
    entered = asyncio.Event()
    raw_secret = "password=RUNTIME-EXCEPTION-SECRET"

    async def standby():
        entered.set()
        await asyncio.Event().wait()

    run.standby_task = asyncio.create_task(standby())
    await entered.wait()

    def broken_cancel():
        raise RuntimeError(raw_secret)

    run.standby_cancel = broken_cancel
    run.standby_runtime_exited = lambda: True

    async def wait_runtime_exit(_timeout):
        return True

    run.standby_wait_runtime_exit = wait_runtime_exit
    await mgr.post_control(run.run_id, {
        "command_id": "C-secret-cancel-error", "action": "stop",
    })
    await run.control_actor.join()

    effect = run.control_journal.latest_effect("C-secret-cancel-error")
    assert effect is not None and effect.state is EffectState.PARTIAL
    assert effect.metadata["cancel_error"] == (
        "worker cancellation callback failed (RuntimeError): "
        "password=<redacted>")
    assert raw_secret not in effect.model_dump_json()
    control_db = mgr.coordinator_control_dir(run.run_id) / "control.db"
    assert raw_secret.encode() not in control_db.read_bytes()
    persisted_events = b"".join(
        path.read_bytes() for path in (tmp_path / "sessions").rglob("*.jsonl")
    )
    assert raw_secret.encode() not in persisted_events

    run.standby_cancel = None
    run.standby_runtime_exited = None
    run.standby_wait_runtime_exit = None
    await mgr.shutdown()


async def test_wrapper_done_with_live_proc_is_partial_then_force_cancel_retries(
    tmp_path, monkeypatch
):
    """The CliSolver cleanup window may be shorter than the control deadline.

    Even after the wrapper Task is DONE, a live runner/proc keeps STOP at PARTIAL.
    Because STOP moved desired state to TERMINATED, FORCE_CANCEL must remain an
    admissible cleanup follow-up and can later close the runtime-exit fence.
    """
    import threading

    from muteki.models.solve_graph import Challenge
    from muteki.solver.cli_solver import CliSolver

    monkeypatch.setenv("MUTEKI_CLI_CANCEL_CLEANUP_TIMEOUT", "0.01")
    monkeypatch.setenv("MUTEKI_STANDBY_CANCEL_TIMEOUT", "0.06")

    solver = CliSolver(
        None,
        Challenge(id="run-runtime-fence", name="fence", category="web"),
        kb=False,
    )
    started = threading.Event()
    release_runner = threading.Event()
    runner_finished = threading.Event()

    class _FakeProc:
        pid = 999999

        def __init__(self):
            self.alive = True
            self.killed = 0

        def kill(self):
            self.killed += 1

        def poll(self):
            return None if self.alive else 0

    proc = _FakeProc()

    def stubborn_runner():
        solver._on_proc(proc)
        started.set()
        # Deliberately ignore kill until the second durable cleanup command.
        release_runner.wait(timeout=2)
        proc.alive = False
        runner_finished.set()
        return "done"

    wrapper = asyncio.create_task(
        solver._to_thread_with_cancel_cleanup(stubborn_runner))
    while not started.is_set():
        await asyncio.sleep(0.001)
    wrapper.cancel()
    with pytest.raises(asyncio.CancelledError):
        await wrapper
    assert wrapper.done()
    assert not solver.runtime_exit_confirmed()
    assert proc.poll() is None and not runner_finished.is_set()

    mgr = RunManager(sessions_root=tmp_path / "sessions")
    run = mgr.create("run-runtime-fence")
    run.standby_task = wrapper
    cancel_calls = 0

    def cancel_runtime():
        nonlocal cancel_calls
        cancel_calls += 1
        solver.cancel()
        if cancel_calls >= 2:
            release_runner.set()

    run.standby_cancel = cancel_runtime
    run.standby_runtime_exited = solver.runtime_exit_confirmed
    run.standby_wait_runtime_exit = solver.wait_runtime_exit

    await mgr.post_control(
        run.run_id, {"command_id": "C-runtime-stop", "action": "stop"})
    await run.control_actor.join()
    first = run.control_journal.latest_effect("C-runtime-stop")
    assert first is not None and first.state is EffectState.PARTIAL
    assert first.metadata["task_done"] is True
    assert first.metadata["runtime_exit_confirmed"] is False
    assert proc.poll() is None and not runner_finished.is_set()

    # Desired state is already TERMINATED, but cleanup commands remain idempotently
    # admissible so the orphan cannot become impossible to signal.
    await mgr.post_control(
        run.run_id,
        {"command_id": "C-runtime-force", "action": "force_cancel"},
    )
    await run.control_actor.join()
    second = run.control_journal.latest_effect("C-runtime-force")
    assert second is not None and second.state is EffectState.EFFECT_OBSERVED
    assert second.metadata["runtime_exit_confirmed"] is True
    assert cancel_calls == 2
    assert runner_finished.is_set() and proc.poll() == 0
    assert solver.runtime_exit_confirmed()

    run.standby_cancel = None
    run.standby_runtime_exited = None
    run.standby_wait_runtime_exit = None
    await mgr.shutdown()


async def test_finished_run_worker_scope_never_widens_to_unrelated_winner_standby(
        tmp_path, monkeypatch):
    import json

    mgr = RunManager(sessions_root=tmp_path / "sessions")
    run = mgr.create("run-standby-scope")
    run.started = True
    run.finished = True
    run.task = None
    (mgr.workspace_dir(run.run_id) / "winner.json").write_text(json.dumps({
        "worker_id": "cli-claude-winner",
        "engine": "claude",
        "session": "winner-session",
    }))
    scheduled = []
    monkeypatch.setattr(
        mgr, "_ensure_standby",
        lambda run_id, wire: scheduled.append((run_id, wire)),
    )

    await mgr.post_control(run.run_id, {
        "command_id": "C-private-loser",
        "action": "hint",
        "target": "worker:cli-codex-loser",
        "text": "private correction",
    })
    await run.control_actor.join()
    receipt = run.control_journal.latest_effect("C-private-loser")
    assert receipt is not None and receipt.state is EffectState.UNKNOWN
    assert receipt.metadata["code"] == "standby_scope_unresolved"
    assert scheduled == []
    # A display label is not a durable execution identity.  Persisting this
    # private command as worker-scoped context would expose it to a later worker
    # that happens to reuse the same label.
    assert run.control_journal.context_resources(active_only=False) == []
    run.worker_registry.register(WorkerRef(
        worker_id="cli-codex-loser", engine="codex", challenge_id=run.run_id,
    ))
    assert run.control_journal.context_resources() == []
    await mgr.shutdown()


async def test_delete_and_shutdown_retain_unconfirmed_standby_runtime_owner(
        tmp_path, monkeypatch):
    monkeypatch.setenv("MUTEKI_STANDBY_CANCEL_TIMEOUT", "0.02")

    async def _attach_unconfirmed_runtime(mgr, run_id):
        run = mgr.create(run_id)
        run.started = True
        run.finished = True
        exited = asyncio.Event()
        run.standby_cancel = lambda: None
        run.standby_runtime_exited = exited.is_set

        async def _wait(timeout=None):
            try:
                await asyncio.wait_for(exited.wait(), timeout=float(timeout or 0.01))
            except asyncio.TimeoutError:
                return False
            return True

        run.standby_wait_runtime_exit = _wait

        async def _reaper():
            await exited.wait()
            run.standby_cancel = None
            run.standby_runtime_exited = None
            run.standby_wait_runtime_exit = None
            if run.standby_runtime_cleanup_task is asyncio.current_task():
                run.standby_runtime_cleanup_task = None

        cleanup = asyncio.create_task(_reaper())
        run.standby_runtime_cleanup_task = cleanup
        return run, exited, cleanup

    mgr = RunManager(sessions_root=tmp_path / "delete-sessions")
    run, exited, cleanup = await _attach_unconfirmed_runtime(mgr, "run-delete")
    artifact = mgr.workspace_dir(run.run_id) / "keep.txt"
    artifact.write_text("still owned")
    assert await mgr.delete(run.run_id) is False
    assert mgr.runs[run.run_id] is run
    assert artifact.exists()
    assert not cleanup.done(), "delete must not cancel the only runtime reaper"
    exited.set()
    await cleanup
    assert await mgr.delete(run.run_id) is True

    mgr2 = RunManager(sessions_root=tmp_path / "shutdown-sessions")
    run2, exited2, cleanup2 = await _attach_unconfirmed_runtime(
        mgr2, "run-shutdown")
    with pytest.raises(RuntimeError, match="shutdown incomplete"):
        await mgr2.shutdown()
    assert mgr2.runs[run2.run_id] is run2
    assert not cleanup2.done(), "failed shutdown must retain the kill owner"
    exited2.set()
    await cleanup2
    await mgr2.shutdown()


async def test_main_runtime_incomplete_fences_control_resolve_delete_and_shutdown(
        tmp_path, monkeypatch):
    monkeypatch.setenv("MUTEKI_STANDBY_CANCEL_TIMEOUT", "0.02")
    mgr = RunManager(sessions_root=tmp_path / "main-owner-sessions")
    run = mgr.create("run-main-owner")
    run.started = True
    run.finished = False
    run.runtime_incomplete = True
    run.runtime_owner = object()
    released = asyncio.Event()

    async def _settle():
        await released.wait()
        run.runtime_incomplete = False
        run.runtime_owner = None
        run.runtime_error = ""

    run.runtime_settle = _settle
    run.runtime_cleanup_task = asyncio.create_task(_settle())
    artifact = mgr.workspace_dir(run.run_id) / "keep-main-owner.txt"
    artifact.write_text("still owned")

    control = await mgr.post_control(
        run.run_id, {"command_id": "C-during-incomplete", "action": "pause"})
    assert control["ok"] is False
    assert control["code"] == "runtime_shutdown_incomplete"
    assert await mgr.resolve(run.run_id, {}) is False
    assert await mgr.delete(run.run_id) is False
    assert mgr.runs[run.run_id] is run and artifact.exists()
    with pytest.raises(RuntimeError, match="shutdown incomplete"):
        await mgr.shutdown()
    assert not run.runtime_cleanup_task.done()

    released.set()
    await run.runtime_cleanup_task
    assert await mgr.delete(run.run_id) is True


@pytest.mark.parametrize("launch_kind", ["start", "resolve"])
async def test_generation_launch_drains_inflight_prior_control_epoch(
        tmp_path, monkeypatch, launch_kind):
    from muteki.control import RunControlMode

    monkeypatch.setenv("MUTEKI_CONTROL_EPOCH_DRAIN_TIMEOUT", "1")
    mgr = RunManager(sessions_root=tmp_path / launch_kind)
    run = mgr.create(f"run-control-epoch-{launch_kind}")
    run.started = True
    run.finished = True
    run.name = "epoch"
    run.category = "web"
    run.task = asyncio.create_task(asyncio.sleep(0))
    await run.task
    actor, journal, _secrets = mgr._ensure_control(run)
    persisted_blocked = asyncio.Event()
    release_old = asyncio.Event()
    original_sink = actor.effect_sink

    async def blocking_sink(receipt):
        if (receipt.command_id == "C-old-stop"
                and receipt.state is EffectState.PERSISTED):
            persisted_blocked.set()
            await release_old.wait()
        if original_sink is not None:
            await original_sink(receipt)

    actor.effect_sink = blocking_sink
    old_control = asyncio.create_task(mgr.post_control(run.run_id, {
        "command_id": "C-old-stop", "action": "stop", "target": "global",
    }))
    await persisted_blocked.wait()

    new_entered = asyncio.Event()
    finish_new = asyncio.Event()

    async def new_driver(_run):
        new_entered.set()
        await finish_new.wait()

    if launch_kind == "resolve":
        monkeypatch.setattr(
            "apps.web.drivers.build_driver",
            lambda _body, mgr=None: new_driver,
        )
        launch = asyncio.create_task(mgr.resolve(run.run_id, {}))
    else:
        launch = asyncio.create_task(mgr.start(run.run_id, new_driver))

    await asyncio.sleep(0.05)
    assert not new_entered.is_set(), "new generation must wait for old control"
    assert not launch.done()

    release_old.set()
    await old_control
    launched = await asyncio.wait_for(launch, timeout=1)
    assert launched is True if launch_kind == "resolve" else launched is run
    await asyncio.wait_for(new_entered.wait(), timeout=1)
    assert run.task is not None and not run.task.done()
    assert journal.current_state().mode is RunControlMode.ACTIVE
    assert journal.latest_effect("C-old-stop").state is EffectState.EFFECT_OBSERVED

    finish_new.set()
    await asyncio.wait_for(run.task, timeout=1)
    await mgr.shutdown()


async def test_delete_and_shutdown_rescan_main_runtime_owner_created_by_cancel(
        tmp_path, monkeypatch):
    """Cancellation itself may expose a still-live process/container owner."""
    monkeypatch.setenv("MUTEKI_STANDBY_CANCEL_TIMEOUT", "0.02")

    async def _attach_late_owner(mgr, run_id):
        run = mgr.create(run_id)
        release = asyncio.Event()
        started = asyncio.Event()

        async def _settle():
            await release.wait()
            run.runtime_incomplete = False
            run.runtime_owner = None
            run.runtime_error = ""

        async def _wrapper():
            started.set()
            try:
                await asyncio.Future()
            except asyncio.CancelledError:
                # Mirrors build_swarm_driver transferring an unconfirmed runtime
                # to its autonomous cleanup owner while cancellation unwinds.
                run.runtime_incomplete = True
                run.runtime_owner = object()
                run.runtime_error = "runtime exit unconfirmed"
                run.runtime_settle = _settle
                run.runtime_cleanup_task = asyncio.create_task(_settle())

        run.task = asyncio.create_task(_wrapper())
        await started.wait()
        return run, release

    delete_mgr = RunManager(sessions_root=tmp_path / "late-delete")
    delete_run, delete_release = await _attach_late_owner(
        delete_mgr, "run-late-delete")
    artifact = delete_mgr.workspace_dir(delete_run.run_id) / "owned.txt"
    artifact.write_text("keep until runtime exit")
    assert await delete_mgr.delete(delete_run.run_id) is False
    assert delete_mgr.runs[delete_run.run_id] is delete_run
    assert delete_run.runtime_incomplete is True
    assert artifact.exists()
    delete_release.set()
    await delete_run.runtime_cleanup_task
    assert await delete_mgr.delete(delete_run.run_id) is True

    shutdown_mgr = RunManager(sessions_root=tmp_path / "late-shutdown")
    shutdown_run, shutdown_release = await _attach_late_owner(
        shutdown_mgr, "run-late-shutdown")
    with pytest.raises(RuntimeError, match="shutdown incomplete"):
        await shutdown_mgr.shutdown()
    assert shutdown_mgr.runs[shutdown_run.run_id] is shutdown_run
    assert shutdown_run.runtime_incomplete is True
    shutdown_release.set()
    await shutdown_run.runtime_cleanup_task
    await shutdown_mgr.shutdown()

    # A pre-cancel timeout is not sticky: another wrapper's cancellation can let
    # the autonomous reaper prove exit during the gather window.  The final rescan
    # must close cleanly instead of retaining a stale ``unsettled`` id.
    settle_mgr = RunManager(sessions_root=tmp_path / "settles-during-cancel")
    owned = settle_mgr.create("run-owned-before-cancel")
    owner_release = asyncio.Event()

    async def _settle_existing_owner():
        await owner_release.wait()
        owned.runtime_incomplete = False
        owned.runtime_owner = None

    owned.runtime_incomplete = True
    owned.runtime_owner = object()
    owned.runtime_settle = _settle_existing_owner
    owned.runtime_cleanup_task = asyncio.create_task(_settle_existing_owner())

    trigger = settle_mgr.create("run-cancel-releases-owner")
    trigger_started = asyncio.Event()

    async def _release_owner_on_cancel():
        trigger_started.set()
        try:
            await asyncio.Future()
        except asyncio.CancelledError:
            owner_release.set()

    trigger.task = asyncio.create_task(_release_owner_on_cancel())
    await trigger_started.wait()
    await settle_mgr.shutdown()
    assert owned.runtime_incomplete is False


async def test_standby_redirect_binds_one_context_and_decrypts_only_authorized_resource(
        tmp_path, monkeypatch):
    import json
    import muteki.solver.cli_solver as cli_solver
    from muteki.solver.types import SolveOutcome

    captured = {}

    class _FakeCliSolver:
        def __init__(self, _spec, _challenge, **kwargs):
            self.solver_id = kwargs.get("solver_label") or "standby"
            self.hitl_cmd = kwargs.get("hitl_cmd") or {}
            captured["hitl_cmd"] = dict(self.hitl_cmd)
            self._pending_control_context_reservations = []
            self._context_committer = None
            self._context_releaser = None
            self._context_binding_worker_id = ""
            self._context_delivery_callback = None

        def cancel(self):
            return None

        def runtime_exit_confirmed(self):
            return True

        async def wait_runtime_exit(self, timeout=None):
            return True

        async def run(self):
            ok = True
            for context_id, reservation_id in list(
                    self._pending_control_context_reservations):
                ok = bool(self._context_committer(
                    context_id, worker_id=self._context_binding_worker_id,
                    reservation_id=reservation_id)) and ok
            if ok:
                self._pending_control_context_reservations = []
            if callable(self._context_delivery_callback):
                self._context_delivery_callback(ok)
            return SolveOutcome(False, None, 1, None, "standby done")

    monkeypatch.setattr(cli_solver, "CliSolver", _FakeCliSolver)
    mgr = RunManager(sessions_root=tmp_path / "sessions")
    mgr.worker_config.resolve = lambda _category: {
        "worker_backend": "local", "worker_profiles": [],
    }
    run = mgr.create("run-standby-secret")
    run.started = True
    run.finished = True
    (mgr.workspace_dir(run.run_id) / "winner.json").write_text(json.dumps({
        "worker_id": "cli-claude-winner", "engine": "claude",
        "session": "winner-session",
        "challenge": {"id": run.run_id, "name": "saved", "category": "web"},
    }))
    command_id = "C-standby-secret-redirect"
    await mgr.post_control(run.run_id, {
        "command_id": command_id,
        "action": "redirect",
        "target": "global",
        "text": "password=TEXT-SECRET",
        "url": "https://user:URL-SECRET@example.invalid/path",
    })
    await run.control_actor.join()
    receipt = run.control_journal.latest_effect(command_id)
    assert receipt is not None and receipt.state is EffectState.EFFECT_OBSERVED
    assert captured["hitl_cmd"]["url"] == (
        "https://user:URL-SECRET@example.invalid/path")
    assert "text" not in captured["hitl_cmd"]
    assert "TEXT-SECRET" not in str(captured["hitl_cmd"])
    context_id = next(
        row.context_id for row in run.control_journal.context_resources(active_only=False)
        if row.metadata.get("source_command_id") == command_id)
    assert run.control_journal.context_bindings(context_id) == [
        f"standby:{command_id}"]
    assert run.control_journal.context_delivery_status(context_id) == "bound"

    ask_command_id = "C-standby-secret-ask"
    await mgr.post_control(run.run_id, {
        "command_id": ask_command_id,
        "action": "ask",
        "target": "global",
        "text": "password=ASK-SECRET",
    })
    await run.control_actor.join()
    ask_receipt = run.control_journal.latest_effect(ask_command_id)
    assert ask_receipt is not None
    assert ask_receipt.state is EffectState.EFFECT_OBSERVED
    assert captured["hitl_cmd"]["text"] == "password=ASK-SECRET"
    assert captured["hitl_cmd"]["followup_id"]
    await mgr.shutdown()


async def test_standby_rejects_expired_plaintext_context_before_scheduling(
        tmp_path, monkeypatch):
    mgr = RunManager(sessions_root=tmp_path / "sessions")
    run = mgr.create("run-standby-expired")
    run.started = True
    run.finished = True
    scheduled = []
    monkeypatch.setattr(
        mgr, "_ensure_standby",
        lambda run_id, wire: scheduled.append((run_id, wire)) or True,
    )
    command_id = "C-standby-expired"
    await mgr.post_control(run.run_id, {
        "command_id": command_id, "action": "hint", "target": "global",
        "text": "already revoked", "ttl_s": 0,
    })
    await run.control_actor.join()
    receipt = run.control_journal.latest_effect(command_id)
    assert receipt is not None and receipt.state is EffectState.UNKNOWN
    assert receipt.metadata["code"] == "standby_context_unavailable"
    assert scheduled == []
    await mgr.shutdown()


async def test_standby_context_release_failure_keeps_retry_owner(tmp_path, monkeypatch):
    from muteki.control import ContextResource

    mgr = RunManager(sessions_root=tmp_path / "sessions")
    run = mgr.create("run-standby-context-cleanup")
    _actor, journal, _secrets = mgr._ensure_control(run)
    journal.append_context(ContextResource(
        context_id="CTX-standby-cleanup", run_id=run.run_id,
        content="one-shot standby context", max_bindings=1,
    ))
    reservation_id = journal.reserve_context(
        "CTX-standby-cleanup", worker_id="standby:test")
    assert reservation_id
    original_release = journal.release_context_reservation
    attempts = 0

    def _flaky_release(*args, **kwargs):
        nonlocal attempts
        attempts += 1
        if attempts == 1:
            raise RuntimeError("transient context journal failure")
        return original_release(*args, **kwargs)

    monkeypatch.setattr(journal, "release_context_reservation", _flaky_release)
    cleanup = mgr._ensure_standby_context_cleanup(
        run, owner="standby:test",
        reservations=[("CTX-standby-cleanup", reservation_id)],
    )
    assert cleanup is not None
    assert run.standby_context_cleanup_reservations
    await asyncio.wait_for(cleanup, timeout=1)

    assert attempts >= 2
    assert run.standby_context_cleanup_reservations == []
    assert journal.context_delivery_status("CTX-standby-cleanup") == "active"
    await mgr.shutdown()


async def test_secret_input_is_reference_only_in_journal_and_events(tmp_path, monkeypatch):
    monkeypatch.setenv("MUTEKI_CONTROL_ACK_TIMEOUT", "0.02")
    mgr = RunManager(sessions_root=tmp_path / "sessions")
    run = mgr.create("run-secret")
    run.task = asyncio.create_task(asyncio.sleep(30))
    app = _app(mgr, monkeypatch)
    raw_secret = "super-secret-token-9347"
    async with httpx.AsyncClient(
        transport=httpx.ASGITransport(app=app), base_url="http://test",
    ) as client:
        body = {"command_id": "C-secret", "action": "hint",
                "text": f"password={raw_secret}"}
        response = await client.post(
            "/api/runs/run-secret/control",
            json=body,
        )
        assert response.status_code == 200
        retry = await client.post("/api/runs/run-secret/control", json=body)
        assert retry.status_code == 200
    await run.control_actor.join()

    command = run.control_journal.get_command("C-secret")
    assert command is not None
    assert raw_secret not in command.model_dump_json()
    assert str(command.payload["text"]).startswith("secret://")
    events = (tmp_path / "sessions" / "run-secret.jsonl").read_text()
    assert raw_secret not in events
    assert "[redacted operator secret]" in events
    control_dir = mgr.coordinator_control_dir(run.run_id)
    for path in control_dir.glob("control.db*"):
        assert raw_secret.encode() not in path.read_bytes()
    secret_files = list((control_dir / "secrets").glob("*.secret"))
    assert len(secret_files) == 1
    assert oct(secret_files[0].stat().st_mode & 0o777) == "0o600"
    async with httpx.AsyncClient(
        transport=httpx.ASGITransport(app=app), base_url="http://test",
    ) as client:
        changed = await client.post(
            "/api/runs/run-secret/control",
            json={**body, "text": "password=different"},
        )
        assert changed.status_code == 409
    assert len(list((control_dir / "secrets").glob("*.secret"))) == 1
    run.task.cancel()
    await mgr.shutdown()


async def test_control_state_is_outside_worker_mount_and_never_worker_chowned(
    tmp_path, monkeypatch,
):
    """The privileged journal boundary must not inherit workspace ownership.

    Exercise the real container command builder and recursive chown path. This
    catches both ways the boundary previously leaked: being a descendant of the
    workspace bind mount and being swept into its uid/gid rewrite.
    """
    mgr = RunManager(sessions_root=tmp_path / "sessions")
    run = mgr.create("run-private-control")

    import muteki.solver.container_exec as ce
    import muteki.solver.control_receiver as cr

    # Install the chown spy before either privileged file is created. The only
    # expected calls happen later, when ensure_container walks host_workspace.
    monkeypatch.setattr(os, "geteuid", lambda: 0)
    monkeypatch.setattr(ce, "_worker_uid_gid", lambda _image: (1234, 1235))
    chowned: list[Path] = []
    monkeypatch.setattr(
        os, "chown", lambda path, _uid, _gid: chowned.append(Path(path).resolve()))

    _actor, journal, secrets = mgr._ensure_control(run)
    reference = secrets.put("private-value")
    secret_path = secrets.root / f"{reference.removeprefix('secret://')}.secret"
    db_path = Path(journal.db_path)
    workspace = mgr.workspace_dir(run.run_id)
    private_dir = mgr.coordinator_control_dir(run.run_id)

    assert not private_dir.resolve().is_relative_to(workspace.resolve())
    assert not db_path.resolve().is_relative_to(workspace.resolve())
    assert not secret_path.resolve().is_relative_to(workspace.resolve())
    assert not (workspace / "control").exists()

    docker_calls: list[tuple] = []

    def fake_docker(*args, **_kwargs):
        docker_calls.append(args)
        if args[:2] == ("image", "inspect"):
            return type("R", (), {"returncode": 0, "stdout": "", "stderr": ""})()
        if args[:2] == ("inspect", "-f"):
            return type("R", (), {"returncode": 1, "stdout": "", "stderr": "no"})()
        if args and args[0] == "inspect":
            return type(
                "R", (),
                {"returncode": 1, "stdout": "", "stderr": "No such object"})()
        return type("R", (), {"returncode": 0, "stdout": "cid\n", "stderr": ""})()

    class FakeReceiver:
        def forget(self, _run_id):
            return None

        def expect(self, _run_id, _token):
            return None

    monkeypatch.setattr(ce, "_docker", fake_docker)
    monkeypatch.setattr(ce, "_USE_DOCKEREXEC", False)
    monkeypatch.setattr(ce, "_await_supervisor", lambda _handle: None)
    monkeypatch.setattr(
        cr.ControlReceiver, "instance", classmethod(lambda _cls: FakeReceiver()))

    ce.ensure_container(run.run_id, str(workspace), image="img", network="bridge")

    run_call = next(call for call in docker_calls if call and call[0] == "run")
    mount_specs = [
        run_call[index + 1]
        for index, value in enumerate(run_call[:-1])
        if value == "--mount"
    ]
    mount_sources = [
        Path(part.removeprefix("source=")).resolve()
        for spec in mount_specs
        for part in spec.split(",")
        if part.startswith("source=")
    ]
    assert workspace.resolve() in mount_sources
    assert all(not private_dir.resolve().is_relative_to(source)
               for source in mount_sources)
    assert db_path.resolve() not in chowned
    assert secret_path.resolve() not in chowned
    assert all(not path.is_relative_to(private_dir.resolve()) for path in chowned)
    assert await mgr.delete(run.run_id) is True
    assert not private_dir.exists(), "delete must scrub an out-of-tree control root"


async def test_control_root_configuration_cannot_point_inside_worker_workspace(
    tmp_path,
):
    sessions = tmp_path / "sessions"
    unsafe = sessions / "some-run" / "workspace" / "private-control"
    with pytest.raises(ValueError, match="inside a worker workspace"):
        RunManager(sessions_root=sessions, control_root=unsafe)


async def test_distinct_run_ids_never_share_workspace_or_control_state(tmp_path):
    mgr = RunManager(sessions_root=tmp_path / "sessions")
    first = mgr.create("foo..bar")
    second = mgr.create("foo_bar")

    assert mgr.workspace_dir(first.run_id) != mgr.workspace_dir(second.run_id)
    assert mgr.coordinator_control_dir(first.run_id) != mgr.coordinator_control_dir(
        second.run_id)
    assert mgr._ensure_control(first)[1].db_path != mgr._ensure_control(second)[1].db_path
    await mgr.shutdown()


async def test_secret_retry_is_semantic_across_mapping_key_order(tmp_path, monkeypatch):
    monkeypatch.setenv("MUTEKI_CONTROL_ACK_TIMEOUT", "0.01")
    mgr = RunManager(sessions_root=tmp_path / "sessions")
    run = mgr.create("run-secret-order")
    run.task = asyncio.create_task(asyncio.sleep(30))
    app = _app(mgr, monkeypatch)
    first = {
        "command_id": "C-secret-order", "action": "hint",
        "payload": {"text": "inspect auth", "password": "same-pw", "token": "same-token"},
    }
    reordered = {
        "command_id": "C-secret-order", "action": "hint",
        "payload": {"token": "same-token", "password": "same-pw", "text": "inspect auth"},
    }
    async with httpx.AsyncClient(
        transport=httpx.ASGITransport(app=app), base_url="http://test",
    ) as client:
        assert (await client.post(
            "/api/runs/run-secret-order/control", json=first)).status_code == 200
        await run.control_actor.join()
        retry = await client.post(
            "/api/runs/run-secret-order/control", json=reordered)
        assert retry.status_code == 200
        assert retry.json()["command_id"] == "C-secret-order"
    secret_files = list(
        (mgr.coordinator_control_dir(run.run_id) / "secrets").glob("*.secret"))
    assert len(secret_files) == 2
    run.task.cancel()
    await mgr.shutdown()


async def test_decision_answer_carries_request_id_and_clears_only_one(tmp_path, monkeypatch):
    mgr = RunManager(sessions_root=tmp_path / "sessions")
    run = mgr.create("run-decisions")
    run.task = asyncio.create_task(asyncio.sleep(30))
    await run.bus.emit(Event(
        event_type=EventType.HITL_REQUEST,
        run_id=run.run_id,
        payload=hitl_request_payload(
            "worker-a", "need token A", request_id="H-one",
            need_kind="external_blocker"),
    ))
    await run.bus.emit(Event(
        event_type=EventType.HITL_REQUEST,
        run_id=run.run_id,
        payload=hitl_request_payload(
            "worker-b", "need token B", request_id="H-two",
            need_kind="external_blocker"),
    ))
    app = _app(mgr, monkeypatch)
    consumer = asyncio.create_task(_ack_one(run))
    async with httpx.AsyncClient(
        transport=httpx.ASGITransport(app=app), base_url="http://test",
    ) as client:
        response = await client.post(
            "/api/runs/run-decisions/control",
            json={"command_id": "C-answer", "action": "answer_decision",
                  "request_id": "H-one", "text": "token-a"},
        )
        assert response.status_code == 200
        assert response.json()["status"] == "persisted"
    wire = await consumer
    assert wire["request_id"] == "H-one"
    assert set(run.pending_help) == {"H-two"}
    assert run.control_journal.decision_status("H-one") is DecisionStatus.ANSWERED
    assert run.control_journal.decision_status("H-two") is DecisionStatus.OPEN
    run.task.cancel()
    await mgr.shutdown()


async def test_restart_reconciles_jsonl_decision_gap_before_answer(tmp_path):
    from muteki.core.events import hitl_request_payload
    from muteki.core.session_store import SessionStore

    sessions = tmp_path / "sessions"
    run_id = "run-decision-jsonl-gap"
    store = SessionStore(root=sessions)
    await store.append(Event(
        event_type=EventType.RUN_STARTED,
        run_id=run_id,
        payload={"challenge": {"name": "gap", "category": "web"}},
    ))
    await store.append(Event(
        event_type=EventType.HITL_REQUEST,
        run_id=run_id,
        solver_id="worker-gap",
        payload=hitl_request_payload(
            "worker-gap", "need exact operator input",
            request_id="DR-jsonl-gap",
            need_kind="external_blocker",
            execution_id="exec-gap",
            execution_occurrence="occ-2",
            resolve_epoch="resolve-7",
            intent_id="I-gap",
            engine="claude",
        ),
    ))

    # Simulate restart after SessionStore committed but before control.db did.
    mgr = RunManager(sessions_root=sessions)
    run = mgr.get(run_id)
    assert run is not None
    _actor, journal, _secrets = mgr._ensure_control(run)
    request = journal.get_decision_request("DR-jsonl-gap")
    assert request is not None
    assert request.execution_id == "exec-gap"
    assert request.execution_occurrence == "occ-2"
    assert request.resolve_epoch == "resolve-7"
    assert request.metadata["intent_id"] == "I-gap"

    result = await mgr.post_control(run_id, {
        "command_id": "C-answer-jsonl-gap",
        "action": "answer_decision",
        "request_id": "DR-jsonl-gap",
        "text": "operator supplied recovery context",
    })
    assert result["status"] == "persisted"
    await run.control_actor.join()
    assert journal.decision_status("DR-jsonl-gap") is DecisionStatus.ANSWERED
    context = next(
        row for row in journal.context_resources(active_only=False)
        if row.metadata.get("source_command_id") == "C-answer-jsonl-gap")
    assert context.metadata["request_id"] == "DR-jsonl-gap"
    await mgr.shutdown()


async def test_live_answer_repairs_transient_decision_journal_sink_gap(tmp_path):
    mgr = RunManager(sessions_root=tmp_path / "sessions")
    run = mgr.create("run-live-decision-gap")
    run.task = asyncio.create_task(asyncio.sleep(30))
    _actor, journal, _secrets = mgr._ensure_control(run)
    original_append = journal.append_decision_request
    attempts = 0

    def fail_once(request):
        nonlocal attempts
        attempts += 1
        if attempts == 1:
            raise RuntimeError("transient decision journal failure")
        return original_append(request)

    journal.append_decision_request = fail_once  # type: ignore[method-assign]
    await run.bus.emit(Event(
        event_type=EventType.HITL_REQUEST,
        run_id=run.run_id,
        solver_id="worker-live-gap",
        payload=hitl_request_payload(
            "worker-live-gap", "need a live recovery answer",
            request_id="DR-live-gap", need_kind="external_blocker"),
    ))
    assert journal.decision_status("DR-live-gap") is None
    consumer = asyncio.create_task(_ack_one(run))

    result = await mgr.post_control(run.run_id, {
        "command_id": "C-answer-live-gap",
        "action": "answer_decision",
        "request_id": "DR-live-gap",
        "text": "recovered without restart",
    })
    assert result["status"] == "persisted"
    await consumer
    await run.control_actor.join()
    assert attempts >= 2
    assert journal.decision_status("DR-live-gap") is DecisionStatus.ANSWERED

    run.task.cancel()
    await mgr.shutdown()


async def test_legacy_hitl_compiles_to_same_journal(tmp_path, monkeypatch):
    mgr = RunManager(sessions_root=tmp_path / "sessions")
    run = mgr.create("run-legacy")
    run.task = asyncio.create_task(asyncio.sleep(30))
    app = _app(mgr, monkeypatch)
    consumer = asyncio.create_task(_ack_one(run))
    async with httpx.AsyncClient(
        transport=httpx.ASGITransport(app=app), base_url="http://test",
    ) as client:
        response = await client.post(
            "/api/runs/run-legacy/hitl",
            json={"target": "global", "action": "hint", "text": "try /admin"},
        )
        assert response.status_code == 200 and response.json()["ok"] is True
    wire = await consumer
    command = run.control_journal.get_command(wire["command_id"])
    assert command is not None and command.action is ControlAction.HINT
    run.task.cancel()
    await mgr.shutdown()


async def test_legacy_hitl_dedupe_uses_full_semantics_and_infers_one_decision(
        tmp_path, monkeypatch):
    mgr = RunManager(sessions_root=tmp_path / "sessions")
    run = mgr.create("run-legacy-dedupe")
    run.task = asyncio.create_task(asyncio.sleep(30))

    await run.bus.emit(Event(
        event_type=EventType.HITL_REQUEST, run_id=run.run_id,
        payload=hitl_request_payload(
            "worker-a", "need token A", request_id="H-one",
            need_kind="external_blocker"),
    ))

    wires = []

    async def consume(count):
        for _ in range(count):
            wire = await asyncio.wait_for(run.hitl.get(), timeout=2)
            acknowledgement = wire.pop("_control_ack")
            wires.append(wire)
            acknowledgement.set_result({"state": "effect_observed"})

    consumer = asyncio.create_task(consume(5))
    assert await mgr.post_hitl(run.run_id, "global", "hint", text="same clue")
    assert wires[0]["request_id"] == "H-one", "one pending request is unambiguous"

    # Two pending requests means legacy input must not guess either id.
    for rid, worker in (("H-two", "worker-b"), ("H-three", "worker-c")):
        await run.bus.emit(Event(
            event_type=EventType.HITL_REQUEST, run_id=run.run_id,
            payload=hitl_request_payload(
                worker, f"need {rid}", request_id=rid,
                need_kind="external_blocker"),
        ))
    assert await mgr.post_hitl(run.run_id, "global", "hint", text="different clue")
    assert "request_id" not in wires[1]

    # request_id, standing, and preemption are all semantic dedupe dimensions.
    assert await mgr.post_hitl(
        run.run_id, "global", "hint", text="scoped clue", request_id="H-two")
    assert await mgr.post_hitl(
        run.run_id, "global", "hint", text="scoped clue", request_id="H-three")
    assert await mgr.post_hitl(
        run.run_id, "global", "hint", text="scoped clue", request_id="H-three",
        standing=True, preempt_policy="graceful_drain")
    await consumer
    assert len(wires) == 5
    # Exact resend is the only duplicate and therefore never reaches the queue.
    assert await mgr.post_hitl(
        run.run_id, "global", "hint", text="scoped clue", request_id="H-three",
        standing=True, preempt_policy="graceful_drain")
    assert run.hitl.empty()

    run.task.cancel()
    await mgr.shutdown()


async def test_legacy_dedupe_signature_is_recorded_only_after_persistence(
        tmp_path, monkeypatch):
    mgr = RunManager(sessions_root=tmp_path / "sessions")
    run = mgr.create("run-retryable")
    # Empty hint fails admission before routing and must remain retryable.
    assert await mgr.post_hitl(run.run_id, "global", "hint", text="") is False
    assert run._last_hitl_sig is None
    assert await mgr.post_hitl(run.run_id, "global", "hint", text="") is False
    assert run._last_hitl_sig is None
    assert len(run.control_journal.command_history()) == 2
    await mgr.shutdown()


async def test_finished_standby_command_idempotency_does_not_reschedule(
    tmp_path, monkeypatch
):
    mgr = RunManager(sessions_root=tmp_path / "sessions")
    run = mgr.create("run-finished-idempotent")
    run.started = True
    run.finished = True
    scheduled: list[dict] = []
    def _schedule(_run_id, wire):
        scheduled.append(dict(wire))
        wire["_standby_delivery_ack"].set_result(True)
        run.standby_task = asyncio.create_task(asyncio.sleep(30))
        return True
    monkeypatch.setattr(mgr, "_ensure_standby", _schedule)
    body = {"command_id": "C-writeup-once", "action": "writeup"}
    first = await mgr.post_control(run.run_id, body)
    assert first["status"] == "persisted"
    await run.control_actor.join()
    second = await mgr.post_control(run.run_id, body)
    assert second["status"] == "effect_observed"
    assert len(scheduled) == 1
    await mgr.shutdown()


async def test_negative_standby_delivery_ack_is_conservatively_unknown(
        tmp_path, monkeypatch):
    mgr = RunManager(sessions_root=tmp_path / "sessions")
    run = mgr.create("run-standby-negative-ack")
    run.started = True
    run.finished = True

    def _schedule(_run_id, wire):
        # False can be emitted after Popen when stdin/context commit is uncertain;
        # it is not proof that the process never started.
        wire["_standby_delivery_ack"].set_result(False)
        run.standby_task = asyncio.create_task(asyncio.sleep(30))
        return True

    monkeypatch.setattr(mgr, "_ensure_standby", _schedule)
    result = await mgr.post_control(
        run.run_id, {"command_id": "C-writeup-unknown", "action": "writeup"})
    assert result["status"] == "persisted"
    await run.control_actor.join()
    receipt = run.control_journal.latest_effect("C-writeup-unknown")
    assert receipt is not None
    assert receipt.state is EffectState.UNKNOWN
    assert receipt.metadata["effect"] == "delivery_unknown"
    assert receipt.metadata["process_start_unknown"] is True
    assert "may have crossed" in receipt.detail
    await mgr.shutdown()


async def test_control_receipt_get_reconciles_terminal_state(tmp_path, monkeypatch):
    mgr = RunManager(sessions_root=tmp_path / "sessions")
    run = mgr.create("run-receipt-get")
    run.task = asyncio.create_task(asyncio.sleep(30))
    app = _app(mgr, monkeypatch)
    consumer = asyncio.create_task(_ack_one(run))
    async with httpx.AsyncClient(
        transport=httpx.ASGITransport(app=app), base_url="http://test",
    ) as client:
        response = await client.post(
            "/api/runs/run-receipt-get/control",
            json={"command_id": "C-get", "action": "pause"},
        )
        assert response.status_code == 200
        await consumer
        await run.control_actor.join()
        receipt = await client.get(
            "/api/runs/run-receipt-get/control/C-get")
        assert receipt.status_code == 200
        body = receipt.json()
        assert body["command_id"] == "C-get"
        assert body["receipt_id"].startswith("E-")
        assert body["status"] == "effect_observed"
        assert body["terminal"] is True
    run.task.cancel()
    await mgr.shutdown()


async def test_legacy_rejected_stop_never_cancels_live_task(tmp_path):
    mgr = RunManager(sessions_root=tmp_path / "sessions")
    run = mgr.create("run-rejected-stop")
    run.task = asyncio.create_task(asyncio.sleep(30))
    consumer = asyncio.create_task(_ack_one(run))
    await mgr.post_control(run.run_id, {
        "command_id": "C-first-pause", "action": "pause",
        "expected_generation": 0,
    })
    await consumer
    await run.control_actor.join()
    assert run.control_journal.current_state().generation == 1

    accepted = await mgr.post_hitl(
        run.run_id, "global", "stop",
        command_id="C-stale-stop", expected_generation=0,
    )

    assert accepted is False
    assert run.task is not None and not run.task.done()
    assert run.control_journal.latest_effect(
        "C-stale-stop").state is EffectState.REJECTED
    run.task.cancel()
    await mgr.shutdown()


async def test_acknowledged_stop_waits_for_main_runtime_exit(tmp_path):
    mgr = RunManager(sessions_root=tmp_path / "sessions")
    run = mgr.create("run-acknowledged-stop")
    entered = asyncio.Event()
    cleanup_finished = asyncio.Event()

    async def main_runtime():
        entered.set()
        try:
            await asyncio.Future()
        except asyncio.CancelledError:
            await asyncio.sleep(0.01)
            cleanup_finished.set()
            raise

    run.task = asyncio.create_task(main_runtime())
    await entered.wait()
    consumer = asyncio.create_task(_ack_one(
        run, detail="coordinator termination latch observed"))

    result = await mgr.post_control(run.run_id, {
        "command_id": "C-acknowledged-stop",
        "action": "stop",
        "target": "global",
    })
    await consumer
    await run.control_actor.join()

    receipt = run.control_journal.latest_effect("C-acknowledged-stop")
    assert result["status"] == "persisted"
    assert receipt is not None
    assert receipt.state is EffectState.EFFECT_OBSERVED
    assert receipt.metadata["coordinator_effect"] == "effect_observed"
    assert receipt.metadata["runtime_exit_confirmed"] is True
    assert cleanup_finished.is_set()
    assert run.task.done()
    await mgr.shutdown()


async def test_partial_stop_ack_still_cancels_main_runtime(tmp_path):
    mgr = RunManager(sessions_root=tmp_path / "sessions")
    run = mgr.create("run-partial-stop")
    entered = asyncio.Event()

    async def main_runtime():
        entered.set()
        await asyncio.Future()

    async def acknowledge_partial():
        wire = await asyncio.wait_for(run.hitl.get(), timeout=2)
        acknowledgement = wire.pop("_control_ack")
        acknowledgement.set_result({
            "state": "partial",
            "detail": "coordinator requested only part of the cleanup",
            "target_ids": ["worker-a"],
        })

    run.task = asyncio.create_task(main_runtime())
    await entered.wait()
    consumer = asyncio.create_task(acknowledge_partial())
    await mgr.post_control(run.run_id, {
        "command_id": "C-partial-stop",
        "action": "stop",
        "target": "global",
    })
    await consumer
    await run.control_actor.join()

    receipt = run.control_journal.latest_effect("C-partial-stop")
    assert receipt is not None
    assert receipt.state is EffectState.EFFECT_OBSERVED
    assert receipt.metadata["coordinator_effect"] == "partial"
    assert receipt.metadata["runtime_exit_confirmed"] is True
    assert run.task.cancelled()
    await mgr.shutdown()


async def test_active_global_force_cancel_timeout_never_terminates_run(
        tmp_path, monkeypatch):
    monkeypatch.setenv("MUTEKI_CONTROL_ACK_TIMEOUT", "0.01")
    mgr = RunManager(sessions_root=tmp_path / "sessions")
    run = mgr.create("run-active-force-cancel")
    run.task = asyncio.create_task(asyncio.sleep(30))

    result = await mgr.post_control(run.run_id, {
        "command_id": "C-active-force-cancel",
        "action": "force_cancel",
        "target": "global",
    })
    assert result["status"] == "persisted"
    await run.control_actor.join()

    receipt = run.control_journal.latest_effect("C-active-force-cancel")
    assert receipt is not None and receipt.state is EffectState.UNKNOWN
    assert receipt.metadata["code"] == "ack_timeout"
    assert run.control_journal.current_state().mode.value == "active"
    assert run.task is not None and not run.task.done()
    assert run.finished is False

    run.task.cancel()
    await mgr.shutdown()


async def test_finished_clear_standing_expires_context_and_graph(
    tmp_path, monkeypatch,
):
    from muteki.control import ContextResource
    from muteki.models.solve_graph import Challenge
    from muteki.swarm.shared_graph import SQLiteSharedGraph

    mgr = RunManager(sessions_root=tmp_path / "sessions")
    run = mgr.create("run-offline-clear")
    run.started = True
    run.finished = True
    _actor, journal, _secrets = mgr._ensure_control(run)
    resource = ContextResource(
        context_id="CTX-standing", run_id=run.run_id,
        content="old standing clue", standing=True,
    )
    journal.append_context(resource)
    graph_db = mgr.workspace_dir(run.run_id) / "graph" / "shared_graph.db"
    graph = SQLiteSharedGraph.open(
        db_path=graph_db,
        challenge=Challenge(id=run.run_id, name="offline", category="web"),
    )
    graph.add_operator_directive(
        actor="operator", action="hint", text="old standing clue",
        standing=True,
    )
    graph.close()

    result = await mgr.post_control(run.run_id, {
        "command_id": "C-offline-clear", "action": "clear_standing",
    })
    assert result["status"] == "persisted"
    await run.control_actor.join()
    assert journal.latest_effect(
        "C-offline-clear").state is EffectState.EFFECT_OBSERVED
    assert journal.context_delivery_status(resource.context_id) == "expired"
    graph = SQLiteSharedGraph.open(
        db_path=graph_db,
        challenge=Challenge(id=run.run_id, name="offline", category="web"),
    )
    assert not [row for row in graph.operator_directives(active_only=True)
                if row.get("standing")]
    graph.close()
    await mgr.shutdown()


async def test_finished_sensitive_exact_clear_uses_private_value_match_and_source_fence(
    tmp_path, monkeypatch,
):
    from muteki.control import ContextResource
    from muteki.models.solve_graph import Challenge
    from muteki.swarm.shared_graph import SQLiteSharedGraph

    mgr = RunManager(sessions_root=tmp_path / "sessions")
    run = mgr.create("run-offline-secret-clear")
    run.started = True
    run.finished = True
    _actor, journal, secrets = mgr._ensure_control(run)
    selected_value = "password=SELECTED-PRIVATE-VALUE"
    other_value = "password=OTHER-PRIVATE-VALUE"
    selected_ref = secrets.put(selected_value)
    other_ref = secrets.put(other_value)
    journal.append_context(ContextResource(
        context_id="CTX-secret-selected", run_id=run.run_id,
        content=selected_ref, standing=True,
        metadata={"source_command_id": "C-secret-selected"},
    ))
    journal.append_context(ContextResource(
        context_id="CTX-secret-other", run_id=run.run_id,
        content=other_ref, standing=True,
        metadata={"source_command_id": "C-secret-other"},
    ))

    graph_db = mgr.workspace_dir(run.run_id) / "graph" / "shared_graph.db"
    challenge = Challenge(
        id=run.run_id, name="offline", category="web")
    graph = SQLiteSharedGraph.open(db_path=graph_db, challenge=challenge)
    selected = graph.add_operator_directive(
        actor="operator", action="hint", text=selected_ref, standing=True,
        source_command_id="C-secret-selected",
    )
    other = graph.add_operator_directive(
        actor="operator", action="hint", text=other_ref, standing=True,
        source_command_id="C-secret-other",
    )
    graph.close()

    result = await mgr.post_control(run.run_id, {
        "command_id": "C-sensitive-exact-clear",
        "action": "clear_standing",
        "text": selected_value,
    })
    assert result["status"] == "persisted"
    await run.control_actor.join()

    receipt = journal.latest_effect("C-sensitive-exact-clear")
    assert receipt is not None and receipt.state is EffectState.EFFECT_OBSERVED
    assert receipt.metadata["matched_source_command_ids"] == [
        "C-secret-selected"]
    active_context_ids = {
        row.context_id for row in journal.context_resources(active_only=True)
    }
    assert "CTX-secret-selected" not in active_context_ids
    assert "CTX-secret-other" in active_context_ids

    graph = SQLiteSharedGraph.open(db_path=graph_db, challenge=challenge)
    active_directive_ids = {
        row["directive_id"] for row in graph.operator_directives(active_only=True)
    }
    assert selected["directive_id"] not in active_directive_ids
    assert other["directive_id"] in active_directive_ids
    graph.close()

    # Plaintext is confined to SecretStore files, never SQLite/event journals.
    control_db = mgr.coordinator_control_dir(run.run_id) / "control.db"
    assert selected_value.encode() not in control_db.read_bytes()
    assert selected_value.encode() not in graph_db.read_bytes()
    await mgr.shutdown()


async def test_finished_mark_false_acks_graph_mutation_not_prompt_start(
    tmp_path, monkeypatch,
):
    import json
    from muteki.models.solve_graph import Challenge
    from muteki.swarm.shared_graph import SQLiteSharedGraph

    mgr = RunManager(sessions_root=tmp_path / "sessions")
    run = mgr.create("run-offline-mark-false")
    run.started = True
    run.finished = True
    run.flag = "flag{bad}"
    run.flags = ["flag{bad}"]
    workspace = mgr.workspace_dir(run.run_id)
    (workspace / "winner.json").write_text(json.dumps({
        "worker_id": "cli-claude-winner", "engine": "claude",
        "session": "winner-session",
    }))
    graph_db = workspace / "graph" / "shared_graph.db"
    challenge = Challenge(id=run.run_id, name="offline", category="web")
    graph = SQLiteSharedGraph.open(db_path=graph_db, challenge=challenge)
    graph.flag_found(actor="worker", flag="flag{bad}")
    graph.close()
    scheduled: list[dict] = []
    monkeypatch.setattr(
        mgr, "_ensure_standby",
        lambda _run_id, wire: scheduled.append(dict(wire)) or True,
    )

    await mgr.post_control(run.run_id, {
        "command_id": "C-offline-mark", "action": "mark_false",
        "flag": "flag{bad}",
    })
    await run.control_actor.join()

    receipt = run.control_journal.latest_effect("C-offline-mark")
    assert receipt is not None and receipt.state is EffectState.EFFECT_OBSERVED
    assert receipt.metadata["effect"] == "flag_invalidated"
    graph = SQLiteSharedGraph.open(db_path=graph_db, challenge=challenge)
    assert "flag{bad}" not in graph.snapshot().flags
    graph.close()
    assert not run.flag
    assert scheduled and scheduled[0]["_control_mark_false_applied"] is True
    await mgr.shutdown()


async def test_shutdown_is_bounded_for_cancellation_suppressing_standby(
    tmp_path, monkeypatch,
):
    monkeypatch.setenv("MUTEKI_STANDBY_CANCEL_TIMEOUT", "0.02")
    mgr = RunManager(sessions_root=tmp_path / "sessions")
    run = mgr.create("run-shutdown-suppressed")
    entered = asyncio.Event()
    release = asyncio.Event()

    async def suppress_cancel() -> None:
        entered.set()
        while not release.is_set():
            try:
                await release.wait()
            except asyncio.CancelledError:
                continue

    run.standby_task = asyncio.create_task(suppress_cancel())
    await entered.wait()
    with pytest.raises(RuntimeError, match="shutdown incomplete"):
        await asyncio.wait_for(mgr.shutdown(), timeout=0.3)
    assert run.standby_task is not None and not run.standby_task.done()

    release.set()
    await asyncio.wait_for(run.standby_task, timeout=0.3)
    await mgr.shutdown()


async def test_delete_is_bounded_for_cancellation_suppressing_main_task(
        tmp_path, monkeypatch):
    monkeypatch.setenv("MUTEKI_STANDBY_CANCEL_TIMEOUT", "0.02")
    mgr = RunManager(sessions_root=tmp_path / "sessions")
    run = mgr.create("run-delete-suppressed-main")
    entered = asyncio.Event()
    release = asyncio.Event()

    async def stubborn_main():
        entered.set()
        try:
            await asyncio.Future()
        except asyncio.CancelledError:
            await release.wait()

    run.task = asyncio.create_task(stubborn_main())
    await entered.wait()
    artifact = mgr.workspace_dir(run.run_id) / "retained.txt"
    artifact.write_text("owned until task exits")

    deleted = await asyncio.wait_for(mgr.delete(run.run_id), timeout=0.5)
    assert deleted is False
    assert mgr.get(run.run_id) is run
    assert artifact.exists()
    assert run.runtime_incomplete is True
    assert run.runtime_owner

    release.set()
    await asyncio.wait_for(run.runtime_cleanup_task, timeout=0.5)
    assert run.runtime_incomplete is False
    assert await asyncio.wait_for(mgr.delete(run.run_id), timeout=0.5) is True
    assert mgr.get(run.run_id) is None
    assert not artifact.exists()


async def test_delete_wrapper_settler_never_clobbers_late_stronger_owner(
        tmp_path, monkeypatch):
    monkeypatch.setenv("MUTEKI_STANDBY_CANCEL_TIMEOUT", "0.02")
    mgr = RunManager(sessions_root=tmp_path / "sessions")
    run = mgr.create("run-delete-owner-transfer")
    entered = asyncio.Event()
    release_wrapper = asyncio.Event()
    release_runtime = asyncio.Event()
    stronger_owner = object()

    async def settle_stronger_runtime():
        await release_runtime.wait()
        run.runtime_incomplete = False
        run.runtime_owner = None
        run.runtime_error = ""
        run.runtime_settle = None

    async def wrapper():
        entered.set()
        try:
            await asyncio.Future()
        except asyncio.CancelledError:
            await release_wrapper.wait()
            # Mirrors a driver discovering a surviving process/container only
            # during its late cancellation unwind.
            run.runtime_incomplete = True
            run.runtime_owner = stronger_owner
            run.runtime_error = "underlying runtime exit unconfirmed"
            run.runtime_settle = settle_stronger_runtime
            run.runtime_cleanup_task = asyncio.create_task(
                settle_stronger_runtime(), name="stronger-runtime-owner")

    run.task = asyncio.create_task(wrapper())
    await entered.wait()
    artifact = mgr.workspace_dir(run.run_id) / "strong-owner.txt"
    artifact.write_text("retain")

    assert await asyncio.wait_for(mgr.delete(run.run_id), timeout=0.5) is False
    wrapper_settler = run.runtime_cleanup_task
    release_wrapper.set()
    await asyncio.wait_for(run.task, timeout=0.5)
    await asyncio.wait_for(wrapper_settler, timeout=0.5)

    assert run.runtime_incomplete is True
    assert run.runtime_owner is stronger_owner
    assert run.runtime_cleanup_task is not wrapper_settler
    assert not run.runtime_cleanup_task.done()
    assert await asyncio.wait_for(mgr.delete(run.run_id), timeout=0.5) is False
    assert artifact.exists()

    release_runtime.set()
    await asyncio.wait_for(run.runtime_cleanup_task, timeout=0.5)
    assert await asyncio.wait_for(mgr.delete(run.run_id), timeout=0.5) is True
    assert not artifact.exists()


async def test_overlong_unicode_run_id_is_clean_422(tmp_path, monkeypatch):
    from urllib.parse import quote

    mgr = RunManager(sessions_root=tmp_path / "sessions")
    app = _app(mgr, monkeypatch)
    run_id = "题" * 256
    async with httpx.AsyncClient(
        transport=httpx.ASGITransport(app=app), base_url="http://test",
    ) as client:
        response = await client.post(
            f"/api/runs/{quote(run_id, safe='')}/start",
            json={"kind": "idle"},
        )
    assert response.status_code == 422
    assert "too long" in response.json()["detail"]
    assert mgr.get(run_id) is None
    await mgr.shutdown()
