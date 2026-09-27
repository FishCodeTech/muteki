"""Execution-generation lifecycle extracted from run_control."""

from __future__ import annotations

import asyncio
import os
from typing import Any

from muteki.control import StateConflict
from muteki.core.events import Event, EventType

from apps.web.run_state import (
    LOG,
    Driver,
    Run,
    _runtime_error_id,
    _safe_exception_detail,
)

def _retire_worker_command_epoch(self, run: Run) -> None:
    """Clear execution-local selectors and close queued command receipts."""
    run.worker_registry.clear()
    while True:
        try:
            stale_worker_cmd = run.worker_cmds.get_nowait()
        except asyncio.QueueEmpty:
            break
        if isinstance(stale_worker_cmd, dict):
            stale_ack = stale_worker_cmd.get("_control_ack")
            if isinstance(stale_ack, asyncio.Future) and not stale_ack.done():
                stale_ack.set_result({
                    "state": "unknown",
                    "detail": "stale worker command retired at execution epoch",
                    "target_ids": [],
                    "metadata": {"code": "stale_execution_epoch"},
                })
        run.worker_cmds.task_done()


def _retire_hitl_epoch(self, run: Run, *, terminal: bool) -> None:
    """Remove commands that no longer belong to a live execution generation."""
    while True:
        try:
            stale = run.hitl.get_nowait()
        except asyncio.QueueEmpty:
            break
        if isinstance(stale, dict):
            acknowledgement = stale.get("_control_ack")
            action = str(stale.get("action") or "")
            if (isinstance(acknowledgement, asyncio.Future)
                    and not acknowledgement.done()):
                observed_stop = terminal and action in {
                    "stop", "complete", "force_cancel"}
                acknowledgement.set_result({
                    "state": (
                        "effect_observed" if observed_stop else "unknown"),
                    "detail": (
                        "run generation terminated" if observed_stop
                        else "stale command retired at execution generation"),
                    "target_ids": [],
                    "metadata": {
                        "effect": (
                            "run_terminated" if observed_stop
                            else "stale_execution_generation"),
                    },
                })
        run.hitl.task_done()


def _launch_generation(self, run: Run, driver: Driver) -> asyncio.Task[Any]:
    """Create one owner-token-fenced execution wrapper.

    Caller holds ``_lifecycle_lock`` and has completed admission. The wrapper
    is shared by fresh start and resolve so both synthesize a terminal event on
    crash/cancel and neither stale generation can close a replacement bus.
    """
    previous_generation = run.execution_generation
    generation = previous_generation + 1
    if run.title_task is not None and not run.title_task.done():
        run.title_task.cancel()
    run.execution_generation = generation
    run.finished = False
    run.runtime_error = ""
    run.control_ready.clear()
    run.worker_control_ready.clear()

    async def _go() -> None:
        failure_detail = ""
        try:
            await driver(run)
        except Exception as exc:
            LOG.exception(
                "run driver failed before terminal receipt: run_id=%s generation=%s",
                run.run_id, generation,
            )
            failure_detail = _safe_exception_detail("driver failed", exc)
            if self._execution_owned(
                    run, generation, asyncio.current_task()):
                run.runtime_error = failure_detail
        finally:
            current = asyncio.current_task()
            # Release commands waiting for a consumer even when driver setup
            # failed before the Coordinator existed.  QueueControlPort checks
            # liveness again after this fence and returns a terminal result.
            if self._execution_owned(run, generation, current):
                run.control_ready.set()
                run.worker_control_ready.set()
            # A driver may transfer cleanup to a separately fenced runtime
            # owner. A stale wrapper or transferred owner performs no generation
            # finalization here. Avoid returning from ``finally`` so cancellation
            # and unexpected base exceptions retain their normal semantics.
            if (self._execution_owned(run, generation, current)
                    and not run.runtime_incomplete):
                # If the driver exited without a terminal receipt, synthesize the
                # generation's sole terminal event before closing its bus.
                if not run.finished:
                    try:
                        reason = run.termination_reasons.pop(
                            generation, "runtime_failure")
                        detail = failure_detail
                        if reason == "operator_stop" and not detail:
                            detail = "Operator requested run termination"
                        error_id = _runtime_error_id(
                            run.run_id, generation, detail or reason)
                        payload = {
                            "flag": run.flag,
                            "flags": list(run.flags),
                            "expected_flags": run.expected_flags,
                            "multi_flag": run.multi_flag,
                            "solved": run.solved,
                            "reason": reason,
                            "failure_code": (
                                "operator_stop"
                                if reason == "operator_stop"
                                else "runtime_driver_failed"),
                            "failure_phase": "runtime",
                            "error_id": error_id,
                            "detail": detail,
                        }
                        await run.bus.emit(Event(
                            event_type=EventType.RUN_FINISHED,
                            run_id=run.run_id,
                            payload=payload,
                        ))
                    except Exception:
                        pass
                # Event sinks may await. Re-check ownership before final close.
                if self._execution_owned(run, generation, current):
                    self._retire_hitl_epoch(run, terminal=True)
                    self._retire_worker_command_epoch(run)
                    title_task = run.title_task
                    if (title_task is not None and title_task is not current
                            and not title_task.done()):
                        title_task.cancel()
                        await asyncio.gather(
                            title_task, return_exceptions=True)
                    if run.progress_publisher is not None:
                        await run.progress_publisher.publish_pending(
                            trigger="terminal")
                    run.finished = True
                    await run.bus.close()

    coroutine = _go()
    try:
        task = asyncio.create_task(
            coroutine, name=f"run-{run.run_id}-generation-{generation}")
    except BaseException:
        coroutine.close()
        run.execution_generation = previous_generation
        raise
    run.task = task
    return task


@staticmethod
def _control_epoch_drain_timeout() -> float:
    try:
        return max(0.05, float(os.environ.get(
            "MUTEKI_CONTROL_EPOCH_DRAIN_TIMEOUT", "35")))
    except (TypeError, ValueError):
        return 35.0


async def _drain_control_before_launch(self, run_id: str, run: Run) -> bool:
    """Fence and drain every pre-launch control command to a terminal receipt.

    ``_launching_runs`` is published before this method is called, so new
    submissions fail closed. Acquiring the compile/submit lock waits for a
    request that crossed admission but has not yet reached the actor; actor
    ``join`` then drains commands already queued or executing. The wait is
    bounded—failure leaves the old epoch intact and launches nothing.
    """
    submit_lock = self._control_submit_locks.setdefault(
        run_id, asyncio.Lock())
    async with submit_lock:
        actor = run.control_actor
        if actor is None:
            return True
        try:
            await asyncio.wait_for(
                actor.join(), timeout=self._control_epoch_drain_timeout())
        except asyncio.TimeoutError:
            LOG.error(
                "refusing to launch %s: prior control epoch did not drain",
                run_id,
            )
            return False
        return True


async def start(self, run_id: str, driver: Driver) -> Run:
    """Admit and launch one fresh execution generation.

    Duplicate live starts are conflicts; they never overwrite the only task
    handle. Finished generations may be explicitly restarted on the same run
    id, with a fresh bus/control epoch and cleared terminal projection.
    """
    async with self._lifecycle_lock:
        if self._shutting_down:
            raise StateConflict("run manager is shutting down")
        if run_id in self._closing_runs or run_id in self._launching_runs:
            raise StateConflict(f"run {run_id} lifecycle transition is in progress")
        run = self.create(run_id)
        if run.runtime_incomplete:
            raise StateConflict(
                f"run {run_id} still has an unsettled runtime owner")
        if run.task is not None and not run.task.done():
            raise StateConflict(f"run {run_id} is already running")
        if self._standby_busy(run):
            raise StateConflict(f"run {run_id} standby runtime is still active")
        self._launching_runs.add(run_id)
    try:
        if not await self._drain_control_before_launch(run_id, run):
            raise StateConflict(
                f"run {run_id} prior control epoch is still draining")
        async with self._lifecycle_lock:
            if (self._shutting_down or self.runs.get(run_id) is not run
                    or run_id in self._closing_runs
                    or run.runtime_incomplete
                    or (run.task is not None and not run.task.done())
                    or self._standby_busy(run)):
                raise StateConflict(
                    f"run {run_id} lifecycle changed before launch")
            self._fresh_bus(run)
            self._retire_hitl_epoch(run, terminal=False)
            self._retire_worker_command_epoch(run)
            if run.started:
                _actor, journal, _secrets = self._ensure_control(run)
                state = journal.reopen_state(
                    reason="explicit start generation")
                run.control_generation = state.generation
            self._launch_generation(run, driver)
            run.finished = False
            run.solved = False
            run.flag = None
            run.flags = []
            run.paused = False
            run.started = True
            return run
    finally:
        async with self._lifecycle_lock:
            self._launching_runs.discard(run_id)



async def resolve(self, run_id: str, body: dict[str, Any] | None = None) -> bool:
    """Fence and launch an explicit continuation generation."""
    async with self._lifecycle_lock:
        run = self.runs.get(run_id)
        if (run is None or self._shutting_down
                or run_id in self._closing_runs
                or run_id in self._launching_runs):
            return False
        if run.task is not None and not run.task.done():
            return False
        self._launching_runs.add(run_id)
    try:
        return await self._resolve_launching(run_id, run, body)
    finally:
        async with self._lifecycle_lock:
            self._launching_runs.discard(run_id)


async def _resolve_launching(
    self, run_id: str, run: Run, body: dict[str, Any] | None = None,
) -> bool:
    """"继续做题" — relaunch the FULL coordinator swarm on a finished run.

    Unlike a standby (one cold-started worker resuming the winner's session to
    answer a follow-up), this reopens the run and re-runs the real Swarm:
    bootstrap workers + reason/explore scaling, reusing the SAME workspace_dir
    so the persisted shared_graph (verified facts / dead-ends) carries straight
    over — the swarm builds ON the prior evidence instead of from scratch.

    The challenge is reconstructed from coordinator-owned continuation state,
    falling back to durable lifecycle events and rail metadata. Caller-supplied
    `body` fields win
    (e.g. an operator hint folded into the description, a new target)."""
    if run.runtime_incomplete and not await self._settle_incomplete_runtime(
            run, timeout=self._standby_cancel_timeout()):
        LOG.error(
            "refusing to resolve %s: main runtime owner is still unsettled",
            run_id)
        return False
    if run.task is not None and not run.task.done():
        return False  # already live — nothing to relaunch (use HITL instead)
    if self._standby_busy(run):
        # A resumed winner and a fresh coordinator may share session/workspace/
        # container state. Resolve is allowed only after the real standby runtime
        # (not merely its asyncio wrapper) crosses the exit fence.
        if not await self._settle_standby_runtime(
                run, timeout=self._standby_cancel_timeout()):
            LOG.error(
                "refusing to resolve %s: standby runtime exit is unconfirmed",
                run_id)
            return False

    if not await self._drain_control_before_launch(run_id, run):
        return False

    continuation = self.load_winner_continuation(run_id)
    ch = continuation.get("challenge") or {}
    if not ch:
        try:
            async for ev in run.store.replay(run_id):
                if ev.event_type in {
                    EventType.RUN_PREPARING, EventType.RUN_STARTED,
                }:
                    ch = (ev.payload or {}).get("challenge") or {}
                    break
        except Exception:
            ch = {}
    if not ch:  # degrade to rail metadata
        ch = {"name": run.name or run_id, "category": run.category or "web",
              "expected_flags": run.expected_flags,
              "multi_flag": run.multi_flag}
    merged = {"challenge": ch, **(body or {})}
    if body and body.get("challenge"):
        merged["challenge"] = {**ch, **body["challenge"]}
    # "继续做题" 跳过 race-scout 竞速层：竞速是"从空图并行单发初探"，只在冷启动有意义。
    # resolve 复用同一个 workspace_dir，shared_graph 已满是 verified facts / dead-ends,
    # 应直接进主协调器循环(规划/派发)在已有证据上续做,而不是再竞速一轮
    # 从头探(浪费一轮 + 把已死方向重提)。操作者显式传 race_scout 仍可覆盖。
    # cold_start=False 是 run-75379 BUG④ 的显式信号：协调器内部以此为不变量直接跳过竞速
    # (race_scout=False 现为冗余保险)。即便某条复跑路径忘了传，Swarm 还有图状态兜底。
    merged.setdefault("race_scout", False)
    merged.setdefault("cold_start", False)

    from apps.web.drivers import build_driver
    try:
        driver = build_driver(merged, mgr=self)
    except Exception:
        LOG.exception("failed to build resolve driver for %s", run_id)
        return False

    # Destructive/visible commit: revalidate the exact Run and shutdown fence,
    # then reopen control state, bus, and execution owner as one admission
    # transaction. Holding the lifecycle lock across the replayable bus emit is
    # acceptable; it is a short local sink operation and prevents shutdown from
    # observing a half-reopened generation.
    async with self._lifecycle_lock:
        if (self._shutting_down or self.runs.get(run_id) is not run
                or run_id in self._closing_runs
                or (run.task is not None and not run.task.done())):
            return False
        try:
            _actor, control_journal, _secrets = self._ensure_control(run)
            state = control_journal.reopen_state(reason="operator resolve")
            run.control_generation = state.generation
        except Exception:
            LOG.exception("failed to reopen control epoch for %s", run_id)
            return False
        self._fresh_bus(run)
        self._retire_hitl_epoch(run, terminal=False)
        self._retire_worker_command_epoch(run)
        run.finished = False
        run.solved = False
        run.paused = False
        await run.bus.emit(Event(
            event_type=EventType.RUN_REOPENED, run_id=run_id,
            payload={
                "reason": "resolve",
                "execution_generation": run.execution_generation + 1,
                "control_generation": run.control_generation,
            }))
        self._launch_generation(run, driver)
        return True
