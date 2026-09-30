"""Standby runtime, winner register, and shutdown. Moved from run_manager.py."""

from __future__ import annotations

import asyncio
import inspect
import os
import uuid
from typing import Any, Optional

from muteki.control import (
    ControlScope,
    WorkerRef,
)
from muteki.core.event_bus import EventBus
from muteki.core.events import Event, EventType

from apps.web.run_state import (  # noqa: F401
    LOG, Run, BoundRunConflictError, BoundRunStore, Driver,
    BOUND_RUN_ID_PREFIX, _safe_exception_detail, _runtime_error_id,
    _apply_blackboard_meta, _apply_operator_meta,
)

@staticmethod
def _standby_cancel_timeout() -> float:
    try:
        return max(0.01, float(os.environ.get(
            "MUTEKI_STANDBY_CANCEL_TIMEOUT", "2")))
    except (TypeError, ValueError):
        return 2.0


@staticmethod
def _main_runtime_cancel_timeout() -> float:
    """Time allowed to prove a live Coordinator and its workers have exited."""
    try:
        return max(0.01, float(os.environ.get(
            "MUTEKI_MAIN_RUNTIME_CANCEL_TIMEOUT", "15")))
    except (TypeError, ValueError):
        return 15.0


@staticmethod
def _standby_runtime_status(run: Run) -> Optional[bool]:
    query = run.standby_runtime_exited
    if not callable(query):
        return None
    try:
        return bool(query())
    except Exception:
        # A broken proof boundary is never proof of exit.
        return False


async def _settle_incomplete_runtime(
    self, run: Run, *, timeout: float,
) -> bool:
    """Boundedly wait/retry the retained main-runtime owner cleanup."""
    if not run.runtime_incomplete:
        return True
    task = run.runtime_cleanup_task
    if task is None or task.done():
        settle = run.runtime_settle
        if callable(settle):
            task = asyncio.create_task(
                settle(), name=f"runtime-owner-settle-{run.run_id}")
            run.runtime_cleanup_task = task
    if task is None:
        return False
    try:
        await asyncio.wait_for(
            asyncio.shield(task), timeout=max(0.01, float(timeout)))
    except asyncio.TimeoutError:
        return False
    except asyncio.CancelledError:
        raise
    except Exception:
        return False
    return not run.runtime_incomplete and run.runtime_owner is None


def _standby_scope_matches_winner(self, run: Run, target: str) -> bool:
    """Prove a finished-run selector includes the one resumable winner."""
    try:
        scope = ControlScope.parse(target or "global")
    except Exception:
        return False
    if scope.kind.value == "global":
        return True
    if scope.kind.value in {"run", "challenge"}:
        return scope.value == run.run_id
    winner = self.load_winner_continuation(run.run_id)
    if not winner:
        return False
    if scope.kind.value == "worker":
        persisted_worker = str(winner.get("worker_id") or "")
        return bool(persisted_worker and persisted_worker == scope.value)
    if scope.kind.value == "engine":
        return str(winner.get("engine") or "") == scope.value
    # Intent/lane identity is not persisted in continuation state; never widen it to
    # the winner merely because that is the only standby session available.
    return False


def _register_standby_winner(self, run: Run) -> None:
    """Project the persisted winner as the only valid finished-run mailbox."""
    try:
        winner = self.load_winner_continuation(run.run_id)
        worker_id = str(winner.get("worker_id") or "").strip()
        if not worker_id:
            return
        run.worker_registry.register(WorkerRef(
            worker_id=worker_id,
            engine=str(winner.get("engine") or ""),
            challenge_id=run.run_id,
            status="standby",
            metadata={"persisted_winner": True},
        ))
    except Exception:
        return


def _ensure_standby_context_cleanup(
    self, run: Run, *, owner: str,
    reservations: list[tuple[str, str]],
) -> Optional[asyncio.Task]:
    """Retain/retry standby reservation release until SQLite proves terminal."""
    if not owner or not reservations or run.control_journal is None:
        return None
    run.standby_context_cleanup_owner = str(owner)
    run.standby_context_cleanup_reservations = list(dict.fromkeys([
        *run.standby_context_cleanup_reservations,
        *((str(a), str(b)) for a, b in reservations),
    ]))
    current = run.standby_context_cleanup_task
    if current is not None and not current.done():
        return current

    async def _cleanup() -> None:
        journal = run.control_journal
        assert journal is not None
        pending = list(run.standby_context_cleanup_reservations)
        while pending:
            remaining: list[tuple[str, str]] = []
            for context_id, reservation_id in pending:
                released = False
                try:
                    released = bool(journal.release_context_reservation(
                        str(context_id), worker_id=str(owner),
                        reservation_id=str(reservation_id)))
                except Exception:
                    released = False
                if not released:
                    try:
                        # bound/unknown/already-active are terminal postconditions;
                        # only an actually reserved row still needs retry.
                        released = (
                            journal.context_delivery_status(str(context_id))
                            != "reserved")
                    except Exception:
                        released = False
                if not released:
                    remaining.append((str(context_id), str(reservation_id)))
            pending = remaining
            run.standby_context_cleanup_reservations = list(remaining)
            if pending:
                await asyncio.sleep(0.05)
        run.standby_context_cleanup_owner = ""

    task = asyncio.create_task(
        _cleanup(), name=f"standby-context-release-{run.run_id}")
    run.standby_context_cleanup_task = task

    def _done(done: asyncio.Task) -> None:
        try:
            done.result()
        except BaseException:
            pass
        if run.standby_context_cleanup_task is done:
            run.standby_context_cleanup_task = None

    task.add_done_callback(_done)
    return task


async def _settle_standby_runtime(self, run: Run, *, timeout: float) -> bool:
    """Boundedly drive standby teardown while retaining its kill owner.

    Returns only proof: no live wrapper and no independently-live runtime.  A
    timeout never cancels the autonomous reaper; callers must keep the Run and
    its callbacks so cleanup remains retryable.
    """
    loop = asyncio.get_running_loop()
    deadline = loop.time() + max(0.01, float(timeout))
    if (run.standby_context_cleanup_reservations
            and (run.standby_context_cleanup_task is None
                 or run.standby_context_cleanup_task.done())):
        self._ensure_standby_context_cleanup(
            run, owner=run.standby_context_cleanup_owner,
            reservations=list(run.standby_context_cleanup_reservations))
    if self._standby_busy(run):
        await self._cancel_standby(
            run, timeout=max(0.01, deadline - loop.time()))
    cleanup = run.standby_runtime_cleanup_task
    if cleanup is not None and not cleanup.done():
        remaining = deadline - loop.time()
        if remaining > 0:
            try:
                await asyncio.wait_for(
                    asyncio.shield(cleanup), timeout=remaining)
            except asyncio.TimeoutError:
                return False
            except asyncio.CancelledError:
                raise
            except Exception:
                return False
    setup = run.standby_setup_task
    if setup is not None and not setup.done():
        remaining = deadline - loop.time()
        if remaining > 0:
            try:
                await asyncio.wait_for(
                    asyncio.shield(setup), timeout=remaining)
            except asyncio.TimeoutError:
                return False
            except asyncio.CancelledError:
                raise
            except Exception:
                # A failed acquisition is settled only after its owning wrapper
                # has run the teardown/finally path below.
                pass
    context_cleanup = run.standby_context_cleanup_task
    if context_cleanup is not None and not context_cleanup.done():
        remaining = deadline - loop.time()
        if remaining > 0:
            try:
                await asyncio.wait_for(
                    asyncio.shield(context_cleanup), timeout=remaining)
            except asyncio.TimeoutError:
                return False
            except asyncio.CancelledError:
                raise
            except Exception:
                return False
    task_live = run.standby_task is not None and not run.standby_task.done()
    runtime_live = self._standby_runtime_status(run) is False
    cleanup_live = (
        run.standby_runtime_cleanup_task is not None
        and not run.standby_runtime_cleanup_task.done()
    )
    setup_live = (
        run.standby_setup_task is not None
        and not run.standby_setup_task.done()
    )
    context_cleanup_live = (
        run.standby_context_cleanup_task is not None
        and not run.standby_context_cleanup_task.done()
    ) or bool(run.standby_context_cleanup_reservations)
    return (not task_live and not runtime_live and not cleanup_live
            and not setup_live and not context_cleanup_live)


@classmethod
def _standby_busy(cls, run: Run) -> bool:
    task_live = run.standby_task is not None and not run.standby_task.done()
    setup_live = (
        run.standby_setup_task is not None
        and not run.standby_setup_task.done()
    )
    runtime_status = cls._standby_runtime_status(run)
    context_cleanup_live = (
        run.standby_context_cleanup_task is not None
        and not run.standby_context_cleanup_task.done()
    ) or bool(run.standby_context_cleanup_reservations)
    return (task_live or setup_live or context_cleanup_live
            or runtime_status is False)


async def _cancel_standby(self, run: Run, *, timeout: float) -> dict[str, Any]:
    """Cancel a standby at both the runtime and asyncio boundaries.

    Calling ``Task.cancel`` alone only interrupts the coroutine waiting on
    ``asyncio.to_thread``; it does not stop the thread or the shelled CLI.
    Therefore an observed effect requires ALL THREE fences: successful delivery
    to the live worker cancel callback, wrapper task unwind, and CliSolver proof
    that every runner thread and process handle exited. A deadline can prove only
    a partial/unknown effect, never success.
    """
    task = run.standby_task
    initial_runtime_status = self._standby_runtime_status(run)
    task_live = task is not None and not task.done()
    if not task_live and initial_runtime_status is not False:
        return {
            "state": "unknown",
            "detail": "no live standby worker was available to cancel",
            "target_ids": [],
            "metadata": {
                "code": "no_live_standby",
                "worker_cancel_delivered": False,
                "task_done": bool(task is not None and task.done()),
                "runtime_exit_confirmed": initial_runtime_status is True,
            },
        }

    loop = asyncio.get_running_loop()
    deadline = loop.time() + max(0.01, float(timeout))
    cancel_callback = run.standby_cancel
    runtime_query = run.standby_runtime_exited
    runtime_wait = run.standby_wait_runtime_exit
    cancel_delivered = False
    cancel_error = ""
    timed_out = False

    # Order is intentional: signal the real process tree first.  Cancelling the
    # wrapper first can run driver.finally and lose the only live process handle.
    if callable(cancel_callback):
        try:
            callback_result = cancel_callback()
            if inspect.isawaitable(callback_result):
                remaining = max(0.001, deadline - loop.time())
                callback_result = await asyncio.wait_for(
                    callback_result, timeout=remaining)
            # A callback may explicitly return False when its runtime boundary
            # could not accept the signal.  Legacy ``worker.cancel`` returns
            # None, which means the call itself completed successfully.
            cancel_delivered = callback_result is not False
        except asyncio.TimeoutError:
            timed_out = True
            cancel_error = "worker cancellation callback timed out"
        except Exception as exc:  # noqa: BLE001 - recorded as effect evidence
            cancel_error = _safe_exception_detail(
                "worker cancellation callback failed", exc)

    if task is not None and not task.done():
        task.cancel()

    if task is not None and not task.done() and not timed_out:
        remaining = max(0.001, deadline - loop.time())
        try:
            await asyncio.wait_for(asyncio.shield(task), timeout=remaining)
        except asyncio.TimeoutError:
            timed_out = True
        except asyncio.CancelledError:
            # Expected when the INNER standby task acknowledged cancellation.
            # If it is not done, this CancelledError belongs to our own actor and
            # must propagate rather than being forged into a terminal receipt.
            if not task.done():
                raise
        except Exception:
            # A failed-but-done worker is still an observed termination once the
            # real cancel callback was delivered.
            pass

    task_done = bool(task is not None and task.done())
    runtime_exit_confirmed = False
    if callable(runtime_query):
        try:
            runtime_exit_confirmed = bool(runtime_query())
        except Exception:
            runtime_exit_confirmed = False

    # The wrapper may already be done while asyncio.to_thread continues. Spend
    # the remainder of the standby deadline waiting on the independent runtime
    # fence. The waiter itself never cancels the tracked runner tasks.
    if (task_done and not runtime_exit_confirmed and callable(runtime_wait)
            and not timed_out):
        remaining = deadline - loop.time()
        if remaining > 0:
            try:
                wait_result = runtime_wait(remaining)
                if inspect.isawaitable(wait_result):
                    wait_result = await asyncio.wait_for(
                        wait_result, timeout=remaining)
                runtime_exit_confirmed = bool(wait_result)
            except asyncio.TimeoutError:
                timed_out = True
            except asyncio.CancelledError:
                raise
            except Exception as exc:  # noqa: BLE001 - effect evidence
                cancel_error = (cancel_error + "; " if cancel_error else "") + (
                    _safe_exception_detail("runtime exit fence failed", exc))
        else:
            timed_out = True

    # Re-query after the await to close a boundary race where the waiter reached
    # its deadline just as the final process changed poll state.
    if callable(runtime_query):
        try:
            runtime_exit_confirmed = bool(runtime_query())
        except Exception:
            pass
    if not (task_done and runtime_exit_confirmed) and loop.time() >= deadline:
        timed_out = True

    metadata = {
        "worker_cancel_registered": callable(cancel_callback),
        "worker_cancel_delivered": cancel_delivered,
        "task_done": task_done,
        "runtime_exit_registered": (
            callable(runtime_query) and callable(runtime_wait)),
        "runtime_exit_confirmed": runtime_exit_confirmed,
        "timed_out": timed_out,
    }
    if cancel_error:
        metadata["cancel_error"] = cancel_error[:500]

    if cancel_delivered and task_done and runtime_exit_confirmed:
        return {
            "state": "effect_observed",
            "detail": (
                "standby worker cancellation, task unwind, and runtime exit confirmed"),
            "target_ids": [],
            "metadata": {**metadata, "effect": "standby_cancelled"},
        }
    if cancel_delivered or task_done or runtime_exit_confirmed:
        detail = (cancel_error or
                  "standby cancellation was requested but runtime exit was not fully confirmed")
        return {
            "state": "partial",
            "detail": detail,
            "target_ids": [],
            "metadata": {**metadata, "code": "standby_cancel_unconfirmed"},
        }
    return {
        "state": "unknown",
        "detail": (cancel_error or
                   "standby cancellation could not be confirmed before the deadline"),
        "target_ids": [],
        "metadata": {**metadata, "code": "standby_cancel_unknown"},
    }


def _fresh_bus(self, run: Run) -> None:
    """Replace a run's CLOSED bus with a live one (same sinks) so a standby
    worker's events reach a freshly-opened SSE stream. After the main run
    ended, run.bus was close()d — its subscribers got the end sentinel and the
    browser's EventSource reconnected, but the closed bus won't fan out to new
    subscribers. A new bus, re-wired to the SessionStore + rail meta sinks,
    keeps the durable JSONL append-only and the rail metadata fresh."""
    durable_seq = run.store.last_stream_seq(run.run_id)
    self._bump_bus_seq(run.bus, durable_seq)
    if not getattr(run.bus, "_closed", False):
        return  # still open (live run) — keep it
    new_bus = EventBus()
    new_bus.add_filter(self._generation_filter_for(run))
    new_bus.add_sink(run.store.sink, required=True)
    for sink in self._product_event_sinks:
        new_bus.add_sink(sink)
    if run.progress_publisher is not None:
        new_bus.add_sink(run.progress_publisher.observe)
    new_bus.add_sink(self._meta_sink_for(run))
    # carry the seq forward so SSE Last-Event-ID continuity holds across runs
    self._bump_bus_seq(new_bus, max(getattr(run.bus, "_seq", 0), durable_seq))
    run.bus = new_bus
    run.cost.bus = new_bus  # cost updates emit onto the live bus too


def _ensure_standby(self, run_id: str, cmd: dict[str, Any]) -> bool:
    """Spin up a standby worker to serve `cmd`, unless one is already running
    (serialized — one standby per run). Fire-and-forget; events stream live."""
    run = self.runs.get(run_id)
    if run is None:
        return False
    if (self._shutting_down or run_id in self._closing_runs
            or run_id in self._launching_runs):
        return False
    if self._standby_busy(run):
        return False  # a standby is already serving this run — don't pile on
    action = str(cmd.get("action") or "").lower()
    if action in {"ask", "writeup"}:
        followup_id = str(
            cmd.get("followup_id")
            or cmd.get("command_id")
            or uuid.uuid4().hex
        )
        cmd["followup_id"] = followup_id
        run.active_followups.add(followup_id)
    # A prior driver clears these only after the runtime-exit fence. Clear stale
    # registrations defensively before publishing the next worker instance.
    run.standby_cancel = None
    run.standby_runtime_exited = None
    run.standby_wait_runtime_exit = None
    cleanup_task = run.standby_runtime_cleanup_task
    if cleanup_task is not None and not cleanup_task.done():
        cleanup_task.cancel()
    run.standby_runtime_cleanup_task = None
    if bool(getattr(run.bus, "_closed", False)):
        self._fresh_bus(run)
    from apps.web.drivers import build_standby_driver
    driver = build_standby_driver(cmd, mgr=self)

    async def _go() -> None:
        followup_terminal = False

        async def _note_followup_terminal(ev: Event) -> None:
            nonlocal followup_terminal
            if ev.event_type not in {
                EventType.FOLLOWUP_COMPLETED,
                EventType.FOLLOWUP_FAILED,
            }:
                return
            wanted = str(cmd.get("followup_id") or "")
            event_id = str(ev.payload.get("followup_id") or "")
            if not wanted or event_id == wanted:
                followup_terminal = True

        async def _emit_followup_failed(detail: str, *, code: str = "standby_failed") -> None:
            nonlocal followup_terminal
            if followup_terminal:
                return
            try:
                if bool(getattr(run.bus, "_closed", False)):
                    self._fresh_bus(run)
                await run.bus.emit(Event(
                    event_type=EventType.FOLLOWUP_FAILED,
                    run_id=run_id,
                    payload={
                        "followup_id": cmd.get("followup_id"),
                        "kind": action,
                        "detail": detail,
                        "code": code,
                    },
                ))
                followup_terminal = True
            except Exception:
                pass

        run.bus.add_sink(_note_followup_terminal)
        try:
            LOG.info("standby worker starting for %s action=%s",
                     run_id, cmd.get("action"))
            await driver(run)
            LOG.info("standby worker finished for %s action=%s",
                     run_id, cmd.get("action"))
        except asyncio.CancelledError:
            if action in {"ask", "writeup"}:
                await _emit_followup_failed("后续操作已取消")
            raise
        except Exception as exc:
            from apps.web.run_recovery import WorkerRuntimePolicyUnavailable
            detail = _safe_exception_detail("standby worker failed", exc)
            failure_code = (exc.code if isinstance(exc, WorkerRuntimePolicyUnavailable)
                            else "standby_failed")
            # Do not log the traceback here: exception messages from worker
            # boundaries may contain materialised operator secrets.
            LOG.error("standby worker failed for %s action=%s error_type=%s",
                      run_id, cmd.get("action"), type(exc).__name__)
            try:
                await _emit_followup_failed(detail, code=failure_code)
                if action == "mark_false":
                    # ``mark_false`` reopens the run before launching this
                    # one-shot worker.  If that worker fails, close the reopened
                    # generation so the rail never shows a live run with no
                    # runtime owner.  Internal runtime failures are not operator
                    # decisions and therefore must not become HITL cards.
                    await run.bus.emit(Event(
                        event_type=EventType.RUN_FINISHED,
                        run_id=run_id,
                        payload={
                            "solved": False,
                            "flag": run.flag,
                            "flags": list(run.flags),
                            "reason": "standby_failed",
                            "failure_code": failure_code,
                            "failure_phase": "runtime",
                            "error_id": _runtime_error_id(
                                run_id, run.execution_generation, detail),
                            "detail": detail,
                        },
                    ))
            except Exception:
                pass
        finally:
            run.bus.remove_sink(_note_followup_terminal)
            if action in {"ask", "writeup"} and not followup_terminal:
                await _emit_followup_failed("后续操作已中断")
            # Do not close the bus; retain the completed task as an observable
            # receipt. `_ensure_standby` checks `.done()` and replaces it on the
            # next command, so this does not block subsequent follow-ups.
            cancel_boundary = run.standby_cancel
            if callable(cancel_boundary):
                try:
                    cancel_result = cancel_boundary()
                    if inspect.isawaitable(cancel_result):
                        await cancel_result
                except Exception as exc:
                    # Runtime callbacks may embed materialised prompt/credential
                    # values in exception messages. Log only the local type.
                    LOG.error(
                        "standby final cancel boundary failed for %s "
                        "error_type=%s",
                        run_id, type(exc).__name__,
                    )
            delivery_ack = cmd.get("_standby_delivery_ack")
            if isinstance(delivery_ack, asyncio.Future) and not delivery_ack.done():
                # The CLI process-start hook can run on a worker thread and
                # publishes its positive ACK with call_soon_threadsafe().  If a
                # very short-lived worker returns in the same tick, give that
                # already-queued callback one chance to land before recording a
                # negative pre-start outcome here.
                await asyncio.sleep(0)
            if isinstance(delivery_ack, asyncio.Future) and not delivery_ack.done():
                delivery_ack.set_result(False)
            owner = str(cmd.get("_control_context_owner") or "")
            reservations = [
                (str(context_id), str(reservation_id))
                for context_id, reservation_id in list(
                    cmd.get("_control_context_reservations") or [])
            ]
            self._ensure_standby_context_cleanup(
                run, owner=owner, reservations=reservations)
            followup_id = str(cmd.get("followup_id") or "")
            if followup_id:
                run.active_followups.discard(followup_id)

    run.standby_task = asyncio.create_task(_go())
    return True


def _meta_sink_for(self, run: Run):
    """The rail-metadata sink bound to a specific Run (used when rebuilding a
    fresh bus). Mirrors the inline _meta_sink in create()."""
    async def _meta_sink(ev: Event) -> None:
        self._seq += 1
        run.updated_seq = self._seq
        run.updated_at = ev.ts
        if ev.event_type is EventType.HITL_REQUEST:
            self._record_decision_request(run, ev)
        if _apply_operator_meta(run, ev):
            return
        if ev.event_type in {EventType.RUN_PREPARING, EventType.RUN_STARTED}:
            ch = ev.payload.get("challenge", {}) or {}
            run.started = True
            if ch.get("name"):
                run.name = ch["name"]
            run.category = ch.get("category", run.category) or run.category
            if ch.get("mode") in {"ctf", "pentest"}:
                run.mode = ch["mode"]
            if ch.get("expected_flags"):
                run.expected_flags = int(ch["expected_flags"])
            run.merge_flags(ch.get("initial_flags") or [])
            if "multi_flag" in ch:
                run.multi_flag = bool(ch["multi_flag"])
        elif ev.event_type is EventType.RUN_REOPENED:
            run.finished = False
            run.solved = False
            run.paused = False
            if ev.payload.get("reason") == "resolve":
                return
            run.invalidate_flag(ev.payload.get("flag"))
        elif ev.event_type is EventType.RUN_FINISHED:
            run.finished = True
            run.paused = False
            run.awaiting_help = False
            run.help_text = ""
            run.pending_help.clear()
            incoming_flags = (
                ev.payload.get("flags")
                if "flags" in ev.payload else ev.payload.get("flag")
            )
            incoming_values = (
                incoming_flags if isinstance(incoming_flags, list)
                else [incoming_flags]
            )
            had_flag_payload = any(value is not None for value in incoming_values)
            valid_incoming = run.valid_incoming_flags(incoming_flags)
            run.merge_flags(incoming_flags)
            if bool(ev.payload.get("solved")):
                run.solved = bool(valid_incoming) if had_flag_payload else True
            if ev.payload.get("expected_flags"):
                run.expected_flags = int(ev.payload["expected_flags"])
            if "multi_flag" in ev.payload:
                run.multi_flag = bool(ev.payload["multi_flag"])
        else:
            _apply_blackboard_meta(run, ev)
    return _meta_sink


async def shutdown(self) -> None:
    """Cancel every live task on server shutdown so no swarm/standby coroutine —
    and its shelled CLI subprocess group — survives as a budget-eating zombie.
    Cancels BOTH run.task AND standby_task (the latter was leaking: a standby
    worker spun up to answer a post-solve follow-up kept running). The titler is a
    detached create_task with no stored handle, so it can't be cancelled here; it
    is short-lived and self-terminates."""
    # Publish the admission fence before the first snapshot/await. It remains
    # latched even when bounded cleanup reports incomplete; callers may retry
    # shutdown, but no new main or standby generation can race into the gap.
    async with self._lifecycle_lock:
        self._shutting_down = True
    pending: dict[asyncio.Task, str] = {}
    live_standbys = [
        run for run in list(self.runs.values())
        if self._standby_busy(run)
    ]
    main_unsettled: set[str] = set()
    standby_unsettled: set[str] = set()
    task_unsettled: set[str] = set()
    live_main_owners = [
        run for run in list(self.runs.values()) if run.runtime_incomplete
    ]
    if live_main_owners:
        main_results = await asyncio.gather(*(
            self._settle_incomplete_runtime(
                run, timeout=self._standby_cancel_timeout())
            for run in live_main_owners
        ), return_exceptions=True)
        main_unsettled.update(
            run.run_id for run, result in zip(
                live_main_owners, main_results)
            if result is not True
        )
    if live_standbys:
        results = await asyncio.gather(*(
            self._settle_standby_runtime(
                run, timeout=self._standby_cancel_timeout())
            for run in live_standbys
        ), return_exceptions=True)
        standby_unsettled.update({
            run.run_id for run, result in zip(live_standbys, results)
            if result is not True
        })
    for run in list(self.runs.values()):
        if run.run_id in main_unsettled or run.run_id in standby_unsettled:
            # The bounded runtime settler/reaper already owns cancellation.
            # A second raw cancel+gather can hang forever when the wrapper
            # suppresses CancelledError and would discard the retained owner.
            continue
        for t in (
            run.task, run.standby_task, run.recovery_task, run.title_task,
        ):
            if t is not None and not t.done():
                t.cancel()
                pending[t] = run.run_id
    if pending:
        done, still_live = await asyncio.wait(
            tuple(pending), timeout=self._standby_cancel_timeout())
        if done:
            await asyncio.gather(*done, return_exceptions=True)
        task_unsettled.update(pending[task] for task in still_live)
    # A driver may only learn that its subprocess/container survived while its
    # cancelled wrapper is unwinding.  Such ownership transfer happens after
    # the pre-cancel snapshot above, so settle/rescan before closing control
    # state.  Discard a prior timeout when the autonomous reaper has since
    # proved exit; preserve every still-unsettled owner.
    post_cancel_main = [
        run for run in list(self.runs.values())
        if run.runtime_incomplete or run.run_id in main_unsettled
    ]
    if post_cancel_main:
        post_results = await asyncio.gather(*(
            self._settle_incomplete_runtime(
                run, timeout=self._standby_cancel_timeout())
            for run in post_cancel_main
        ), return_exceptions=True)
        for run, result in zip(post_cancel_main, post_results):
            if result is True:
                main_unsettled.discard(run.run_id)
            else:
                main_unsettled.add(run.run_id)
    unsettled = main_unsettled | standby_unsettled | task_unsettled
    for run in list(self.runs.values()):
        if run.run_id in unsettled:
            # Keep its journal/actor/cleanup callbacks owned and retryable. The
            # method will fail loudly below instead of pretending shutdown was
            # clean while its kill boundary remains live.
            continue
        if run.progress_publisher is not None:
            run.progress_publisher.close()
        if run.control_actor is not None:
            try:
                await run.control_actor.close()
            except Exception:
                LOG.exception("failed to close control actor for %s", run.run_id)
        if run.control_journal is not None:
            try:
                run.control_journal.close()
            except Exception:
                LOG.exception("failed to close control journal for %s", run.run_id)
    if unsettled:
        joined = ", ".join(sorted(unsettled))
        raise RuntimeError(
            f"runtime owner exit unconfirmed; shutdown incomplete for: {joined}")
