"""CLI process/runtime control for CliSolver. Moved from cli_solver.py."""
from __future__ import annotations

import asyncio
import os
import signal
import stat
import threading
import time
from pathlib import Path
from typing import Any, Optional

from muteki.core.events import (
    EventType, tool_result_payload,
)
from muteki.solver.cli_driver import (
    CliResult, SecurePromptUnsupported, StreamStep,
    apply_runtime_argv, run_cli_streaming,
)
from muteki.solver.cli_protocol import _is_reasoning_replay
from muteki.solver.prompt_record import PromptArgv, PromptRecorder

_WORKER_HEARTBEAT_SECONDS = 15.0







def _on_proc(
    self, proc: Any, *, context_via_stdin: bool = False,
    context_reservations: "Optional[tuple[tuple[str, str], ...]]" = None,
) -> None:
    """Called by the streaming runner with the live Popen — track it so cancel()
    and the pause monitor can signal it. If a cancel already fired before the
    subprocess registered, kill it immediately. If the operator PAUSED this worker
    (M8) before this subprocess started — e.g. the pause arrived during the gap
    between the execute pass and the conclude-fallback subprocess — freeze the new
    process too, so pause state doesn't silently leak across the turn boundary and
    let a paused worker keep running."""
    self._runtime_process_started = True
    if self._runtime_started_at is None:
        self._runtime_started_at = time.monotonic()
    with self._procs_lock:
        self._live_procs.add(proc)
    runner_procs = getattr(self._runner_proc_local, "procs", None)
    if isinstance(runner_procs, set):
        runner_procs.add(proc)
    if not context_via_stdin:
        self._commit_control_context_delivery(
            actor="cli-popen-commit", reservations=context_reservations)
    if self._cancel_event.is_set():
        self._signal_proc(proc, getattr(signal, "SIGKILL", 9))
    elif self._paused:
        sig = getattr(signal, "SIGSTOP", None)
        if sig is not None:
            self._signal_proc(proc, sig)


def _on_proc_start_uncertain(
    self,
    reservations: "Optional[tuple[tuple[str, str], ...]]" = None,
) -> None:
    """Fail closed when a remote StartWorker was sent but no started ACK arrived.

    The supervisor may already have spawned the prompt-carrying process. Without
    a worker id there is no honest Popen binding or targeted kill proof, so mark
    every pending disclosure unknown and prevent generic pre-start cleanup from
    replaying it.
    """
    # Dispatch crossed the remote supervisor boundary.  Even without a started
    # ACK, a child may exist, so this intent is never pre-start replay-safe.
    self._runtime_process_started = True
    if self._runtime_started_at is None:
        self._runtime_started_at = time.monotonic()
    self._remote_start_uncertain = True
    self._mark_control_context_delivery_unknown(
        actor="rcp-start-uncertain",
        reason="remote StartWorker dispatched without started ACK",
        reservations=reservations,
    )


def _apply_runtime_argv(self, argv: list[str], env: dict) -> list[str]:
    """Apply profile/runtime options that must be command-line flags."""
    return apply_runtime_argv(argv, driver=self.driver, env=env)


async def _run_streaming(
    self, argv: list[str], *, cwd: str, timeout: int,
    stdin_text: "Optional[str]" = None,
    runtime_env: "Optional[dict]" = None,
) -> CliResult:
    """Run a CLI worker and stream each step (think / tool call / tool result)
    to the deck live. The subprocess blocks in a worker thread; its per-line
    callback schedules processing back onto THIS event loop. With no bus the
    event emission becomes a no-op, but the same Popen/on_proc delivery fence is
    still mandatory for context disclosure and cancellation correctness.

    Runtime control: the cancel_event lets the swarm kill the subprocess (winner
    found / abort); a daemon monitor thread polls the InsightBus inbox so HITL
    pause/resume (SIGSTOP/SIGCONT) and a sibling's FLAG reach this live worker."""
    env = dict(runtime_env) if runtime_env is not None else self._worker_env(cwd)
    prompt_record = PromptRecorder(self, argv, env, stdin_text)
    argv = self._apply_runtime_argv(argv, env)
    await prompt_record.prepare()
    loop = asyncio.get_running_loop()
    step_futures: "list[Any]" = []
    step_tail: "list[Any]" = []
    step_tasks: "set[asyncio.Task[Any]]" = set()
    step_ingress_lock = threading.Lock()
    step_ingress_open = True
    step_abort = False
    step_failure: "Optional[BaseException]" = None

    def on_step(step: StreamStep) -> None:
        # Called from one worker thread. Chain each coroutine to the prior step:
        # scheduling order alone does not preserve completion order when _emit_step
        # awaits a slow event sink, and attribution state must match stream order.
        nonlocal step_ingress_open
        with step_ingress_lock:
            if not step_ingress_open:
                return
            previous = step_tail[-1] if step_tail else None

            async def emit_in_order() -> None:
                nonlocal step_abort
                task = asyncio.current_task()
                if task is not None:
                    step_tasks.add(task)
                try:
                    if step_abort:
                        return
                    if previous is not None:
                        try:
                            await asyncio.wrap_future(previous)
                        except Exception:
                            # Ordinary workers are best effort: one failed step
                            # must not block its successors.
                            pass
                    if step_abort:
                        return
                    try:
                        await self._emit_step(step)
                    except BaseException as exc:
                        if str(getattr(exc, "code", "") or "") in {
                            "tool_artifact_persistence_failed",
                            "event_persistence_failed",
                            "blackboard_receipt_persistence_failed",
                        }:
                            step_abort = True
                        raise
                finally:
                    if task is not None:
                        step_tasks.discard(task)

            pending = emit_in_order()
            try:
                future = asyncio.run_coroutine_threadsafe(pending, loop)
            except RuntimeError:
                pending.close()
                return
            step_tail[:] = [future]
            step_futures.append(future)

    # Control queues belong to this event loop. Poll them in a loop-owned task;
    # the previous daemon thread submitted untracked drain coroutines that could
    # resume and mutate state after _run_streaming had already returned.
    monitor_stop = asyncio.Event()

    async def _monitor() -> None:
        while not monitor_stop.is_set():
            await self._drain_control_async()
            try:
                await asyncio.wait_for(monitor_stop.wait(), timeout=0.1)
            except asyncio.TimeoutError:
                pass

    monitor_task: "Optional[asyncio.Task[Any]]" = None
    # Drain any replayed InsightBus history while no subprocess is active. This
    # folds prior human hints into the prompt context without letting old guidance
    # kill the brand-new turn the moment it starts.
    self._turn_active = False
    self._drain_control()
    monitor_task = asyncio.create_task(_monitor())
    heartbeat_stop = asyncio.Event()

    async def _heartbeat() -> None:
        if _WORKER_HEARTBEAT_SECONDS <= 0:
            return
        while not heartbeat_stop.is_set():
            try:
                await asyncio.wait_for(
                    heartbeat_stop.wait(), timeout=_WORKER_HEARTBEAT_SECONDS)
                return
            except asyncio.TimeoutError:
                pass
            if self._turn_active and not self._paused_event.is_set():
                await self._emit_worker_status(
                    online=True, reason="busy", status="online")
            elif self._paused_event.is_set():
                await self._emit_lifecycle("phase_changed", paused=True)

    heartbeat_task = asyncio.create_task(_heartbeat())
    # P2 regression guard: clear any steer left set by history-backlog replay
    # BEFORE the subprocess starts (a fresh worker drains prior FLAGs/standing
    # while _turn_active was False — those folded into context but must not
    # kill the not-yet-started subprocess), then mark the turn active so real
    # in-turn redirects/flags are valid for the duration of THIS subprocess only.
    self._steer_event.clear()
    self._turn_active = True
    self._stalled_at = None
    self._idle_repeat_steered = False
    self._current_workdir = Path(cwd).resolve()
    context_via_stdin = stdin_text is not None
    context_reservations = self._context_delivery_reservation_snapshot()

    def on_proc(proc: Any) -> None:
        if not context_via_stdin:
            prompt_record.update("sent")
        self._on_proc(
            proc, context_via_stdin=context_via_stdin,
            context_reservations=context_reservations)

    def on_stdin_delivered() -> None:
        prompt_record.update("sent")
        self._commit_control_context_delivery(
            actor="cli-stdin-delivery", reservations=context_reservations)

    def on_stdin_uncertain() -> None:
        prompt_record.update("unknown")
        self._mark_control_context_delivery_unknown(
            actor="cli-stdin-uncertain",
            reason="prompt stdin handoff failed or did not finish",
            reservations=context_reservations,
        )

    def on_start_uncertain() -> None:
        prompt_record.update("unknown")
        self._on_proc_start_uncertain(context_reservations)

    result: "Optional[CliResult]" = None
    runner_failure: "Optional[tuple[BaseException, Any]]" = None
    caller_cancel: "Optional[tuple[asyncio.CancelledError, Any]]" = None
    try:
        result = await self._to_thread_with_cancel_cleanup(
            run_cli_streaming, self.driver, argv, cwd=cwd, timeout=timeout,
            on_step=on_step, env=env, inherit_env=False,
            cancel_event=self._cancel_event,
            on_proc=on_proc, steer_event=self._steer_event,
            on_start_uncertain=on_start_uncertain,
            on_stdin_delivered=(on_stdin_delivered
                                if context_via_stdin else None),
            on_stdin_uncertain=(on_stdin_uncertain
                                if context_via_stdin else None),
            paused_event=self._paused_event,
            container=self.container, stdin_text=stdin_text)
        if self._finish_event.is_set():
            # commit-step deliberately stops the current CLI process after its
            # graph transaction.  Normalize the process-level SIGKILL into the
            # logical Worker result instead of exposing it as a runtime error.
            result.finished = True
            result.cancelled = False
            result.steered = False
            result.timed_out = False
            result.error = ""
        self._last_runtime_status = getattr(result, "runtime_status", {}) or {}
    except asyncio.CancelledError as exc:
        current = asyncio.current_task()
        outcome = (exc, exc.__traceback__)
        if current is not None and current.cancelling():
            caller_cancel = outcome
        else:
            runner_failure = outcome
    except BaseException as exc:
        runner_failure = (exc, exc.__traceback__)

    # Close ingress before the first cleanup await. The blocking runner can outlive
    # cancellation briefly, but callbacks arriving after this fence own no state.
    with step_ingress_lock:
        step_ingress_open = False
        pending_steps = tuple(step_futures)

    def abort_steps() -> None:
        # Do not cancel the run_coroutine_threadsafe proxy futures: a cancelled
        # proxy can report done before its actual loop Task has stopped. The abort
        # bit prevents late-starting callbacks from mutating state; actual Tasks are
        # the cancellation handles and the untouched proxies remain completion
        # fences for callbacks that have not registered yet.
        nonlocal step_abort
        step_abort = True
        for task in tuple(step_tasks):
            if not task.done():
                task.cancel()
        if monitor_task is not None and not monitor_task.done():
            monitor_task.cancel()
        if not heartbeat_task.done():
            heartbeat_task.cancel()

    if caller_cancel is not None or runner_failure is not None:
        abort_steps()

    # Steers and helper tasks stop at the synchronous terminal fence. Keep the
    # workdir until callbacks retire because Cursor spill materialization needs it.
    self._turn_active = False
    monitor_stop.set()
    heartbeat_stop.set()
    if monitor_task is not None:
        monitor_task.cancel()
    heartbeat_task.cancel()

    async def retire_turn() -> None:
        nonlocal step_failure
        try:
            await prompt_record.finish()
            if pending_steps:
                step_results = await asyncio.gather(
                    *(asyncio.wrap_future(f) for f in pending_steps),
                    return_exceptions=True)
                for step_result in step_results:
                    if (isinstance(step_result, BaseException)
                            and str(getattr(step_result, "code", "") or "") in {
                            "tool_artifact_persistence_failed",
                            "event_persistence_failed",
                            "blackboard_receipt_persistence_failed",
                        }):
                        step_failure = step_result
                        break

            # Tasks can register after the proxy snapshot while their coroutine is
            # queued on the loop. Re-snapshot until every actual callback Task has
            # retired; repeated caller cancellation re-enters abort_steps().
            while True:
                live_steps = tuple(
                    task for task in step_tasks if not task.done())
                if not live_steps:
                    break
                if step_abort:
                    for task in live_steps:
                        task.cancel()
                await asyncio.gather(*live_steps, return_exceptions=True)
        finally:
            self._current_workdir = None
            # A steer is scoped to one turn; cancel remains permanent.
            self._steer_event.clear()
            self._prune_finished_procs()
            helpers = tuple(
                task for task in (heartbeat_task, monitor_task)
                if task is not None)
            if helpers:
                await asyncio.gather(*helpers, return_exceptions=True)

    # A retained teardown Task is the completion fence. Shield it repeatedly so a
    # second or third caller cancellation cannot expose partially retired state.
    retirement = asyncio.create_task(retire_turn())
    retirement_failure: "Optional[tuple[BaseException, Any]]" = None
    while not retirement.done():
        try:
            await asyncio.shield(retirement)
        except asyncio.CancelledError as exc:
            current = asyncio.current_task()
            if current is not None and current.cancelling():
                if caller_cancel is None:
                    caller_cancel = (exc, exc.__traceback__)
                abort_steps()
                continue
            retirement_failure = (exc, exc.__traceback__)
            break
    if retirement.done():
        try:
            retirement.result()
        except BaseException as exc:
            retirement_failure = (exc, exc.__traceback__)

    if caller_cancel is not None:
        exc, traceback = caller_cancel
        raise exc.with_traceback(traceback)
    blackboard_failure = getattr(self, "_blackboard_terminal_error", None)
    if blackboard_failure is not None:
        raise blackboard_failure
    if runner_failure is not None:
        exc, traceback = runner_failure
        raise exc.with_traceback(traceback)
    if retirement_failure is not None:
        exc, traceback = retirement_failure
        raise exc.with_traceback(traceback)
    if step_failure is not None:
        raise step_failure
    assert result is not None
    return result


def _mark_session_if_live(self, res: "CliResult") -> None:
    """Record that the engine session is really established after a completed
    turn or non-empty assistant text. Only then is a later
    `build_resume` against it safe; before it, `-r <sid>` hits "No conversation
    found" (run-42598 claude spawn-death loop). A steered turn is resumable when
    the engine reported both a real session id and at least one completed turn."""
    if res is None:
        return
    if self._control_secret_values:
        # stdin secret invocations are intentionally non-persistent.  Some CLIs
        # still report an in-memory session id in their result envelope; treating
        # it as resumable would create a guaranteed failed/disk-unsafe follow-up.
        return
    if res.cancelled:
        return
    # Pi can persist many successful turns and then report a provider error on
    # the final model request.  The session is still valid in that case and is
    # exactly what the lifecycle checkpoint must resume to publish its Fact.
    # Treat the persisted-turn count as the success fence; a startup failure has
    # no completed turn and remains non-resumable.
    if (
        bool(getattr(res, "session", None))
        and int(getattr(res, "num_turns", 0) or 0) > 0
    ):
        self._session_established = True
    if res.steered:
        return
    if getattr(res, "error", ""):
        return
    if (
        res.timed_out
        and bool(getattr(res, "session", None))
    ):
        # A timed-out CLI may already have persisted its real session id. Keep it
        # available for non-CTF conclude handling.
        self._session_established = True
        return
    if getattr(res, "returncode", None) not in (None, 0):
        return
    if bool((res.text or "").strip()):
        self._session_established = True


def _execute_invocation(
    self, prompt: str, session: "Optional[str]",
) -> "tuple[list[str], Optional[str]]":
    """Build one invocation, selecting the stdin-only capability for secrets."""
    self._finalize_control_context_prompt_manifest(prompt)
    if self._control_secret_values:
        preflight = getattr(self.driver, "secure_prompt_preflight", None)
        if not callable(preflight):
            raise SecurePromptUnsupported(
                f"{self.driver.name} has no secure prompt preflight")
        supported, detail = preflight()
        if not supported:
            raise SecurePromptUnsupported(
                detail or f"{self.driver.name} secure prompt transport unavailable")
        argv = self.driver.build_execute_stdin(
            prompt, session, web_access=self.web_access,
            kb_access=self.kb, stream=True)
        return PromptArgv(argv, prompt, None, "execute"), prompt
    argv = self.driver.build_execute(
        prompt, session, web_access=self.web_access,
        kb_access=self.kb, stream=True)
    return PromptArgv(argv, prompt, session, "execute"), None


def _resume_invocation(
    self, prompt: str, session: str,
) -> "tuple[list[str], Optional[str]]":
    """Build a resume only after the same final-prompt delivery fence.

    Resume used to bypass ``_execute_invocation`` entirely.  A pending typed
    context could therefore be absent from the resume prompt yet still be in the
    reservation snapshot committed by ``_on_proc``.  Reconcile first; if an
    included secret remains, deliberately fall back to a fresh ephemeral stdin
    invocation because persisted sessions are not a valid secret boundary.
    """
    self._finalize_control_context_prompt_manifest(prompt)
    if self._control_secret_values:
        return self._execute_invocation(prompt, None)
    argv = self.driver.build_resume(
        prompt, session, web_access=self.web_access,
        kb_access=self.kb, stream=True)
    return PromptArgv(argv, prompt, session, "resume"), None


async def _run_invocation(
    self, argv: list[str], *, cwd: str, timeout: int,
    stdin_text: "Optional[str]" = None,
) -> CliResult:
    """Run an invocation without perturbing legacy/mock runner signatures."""
    if stdin_text is None:
        result = await self._run_streaming(argv, cwd=cwd, timeout=timeout)
    else:
        result = await self._run_streaming(
            argv, cwd=cwd, timeout=timeout, stdin_text=stdin_text)
    await self._drain_blackboard_requests_once()
    return result


def _resume_or_execute_argv(self, prompt: str, session: "Optional[str]", *,
                            fresh_prompt: "Optional[str]" = None
                            ) -> "tuple[list[str], Optional[str]]":
    """Build a resume argv when the session is known-established; otherwise fall
    back to a FRESH execute (the prior turn never seated the session, so `-r`
    would 0-token-die). The fallback still carries the solve context: callers
    pass the resume prompt (which already folds in board + peer/guidance), or a
    richer `fresh_prompt` to use when starting clean."""
    if self._control_secret_values:
        # A secret-bearing turn is explicitly non-persistent, so there is no
        # session that may be resumed safely.  Start another ephemeral stdin
        # turn and require callers to provide the full fresh context when the
        # short follow-up prompt alone would be insufficient.
        return self._execute_invocation(
            fresh_prompt if fresh_prompt is not None else prompt, None)
    if self._session_established and session:
        return self._resume_invocation(prompt, session)
    new_sid = self.driver.new_session()
    return self._execute_invocation(
        fresh_prompt if fresh_prompt is not None else prompt, new_sid)


async def _drain_control_async(self) -> None:
    """Loop-thread wrapper around _drain_control (Queue.get_nowait must run on
    the loop that owns the queue)."""
    self._drain_control()


async def _emit_step(self, step: StreamStep) -> None:
    """Map one live StreamStep onto the deck's event stream (reasoning bubble /
    tool bubble / tool-result lane). Rendering errors are best effort;
    authoritative persistence errors propagate into the run."""
    try:
        if step.kind == "reasoning" and step.text:
            incoming = step.text
            if _is_reasoning_replay(self._reasoning_turn_acc, incoming):
                await self._emit(EventType.REASONING_DELTA, text="", turn_end=True)
                self._reasoning_turn_acc = ""
            else:
                self._stream_reasoning_chars += len(incoming)
                # Thinking deltas are displayed but not accumulated: the
                # message_end snapshot repeats only the answer text, so the
                # accumulator must hold answer text only, otherwise the
                # snapshot becomes a suffix the replay check cannot seal.
                if not getattr(step, "thinking", False):
                    self._reasoning_turn_acc = (self._reasoning_turn_acc or "") + incoming
                await self._emit(EventType.REASONING_DELTA, text=incoming)
        elif step.kind == "tool":
            self._reasoning_turn_acc = ""
            self._stream_tool_starts += 1
            label = f"{step.tool}: {step.text}" if step.text else step.tool
            await self._emit(EventType.TOOL_CALL_START, tool=label[:200])
        # Capture tool invocations for provenance. Text in commands, reasoning,
        # tool output, and final replies never mutates shared state; only a
        # host-validated Skill request does so.
        if step.kind == "tool":
            # Keep command/output identity when the engine exposes a call id. The
            # FIFO fallback covers older stream shapes that preserve order only.
            command = (step.text or "")[:2000]
            self._pending_tool_calls.append({
                "call_id": step.call_id,
                "command": command,
                "intent_id": str(
                    getattr(self, "intent_id_assigned", "")
                    or getattr(self, "_intent_id", "")
                    or f"intent:{self.solver_id}"
                ),
                "target_epoch": str(
                    getattr(self, "_target_epoch", "") or "1"
                ),
                "target": str(self._target() or ""),
            })
        if step.kind == "tool_result":
            raw = step.raw or step.text
            evidence_record = None
            spill_status: "Optional[dict[str, Any]]" = None
            if step.spill_path:
                spilled, spill_status = self._materialize_tool_result_spill(step)
                if spilled:
                    raw = spilled
            match_idx = None
            if step.call_id:
                match_idx = next((
                    idx for idx, pending in enumerate(self._pending_tool_calls)
                    if pending.get("call_id") == step.call_id
                ), None)
            if (match_idx is None and not step.call_id
                    and self._pending_tool_calls):
                match_idx = 0
            attributed = match_idx is not None
            pending = self._pending_tool_calls[match_idx] if attributed else {}
            command = pending.get("command", "")
            if raw:
                evidence_record = self._persist_raw_tool_output(
                    raw, command=command, call_id=step.call_id,
                    attributed=attributed,
                    intent_id=pending.get("intent_id", ""),
                    target_epoch=pending.get("target_epoch", ""),
                    target=pending.get("target", ""),
                )
            if attributed:
                self._pending_tool_calls.pop(match_idx)
            result_text = raw[:600] if raw else ""
            result_view: "dict[str, Any]" = {"condensed": result_text}
            if spill_status is not None:
                result_view["spill"] = spill_status
            if result_text or spill_status is not None:
                tool_event = await self._emit(
                    EventType.TOOL_CALL_RESULT,
                    **tool_result_payload(
                        self.driver.name, result_view,
                        artifact_id=(
                            evidence_record.artifact_id
                            if evidence_record is not None else None),
                        truncated=len(raw) > len(result_text)),
                    tool_call_id=step.call_id,
                    intent_id=(
                        evidence_record.intent_id
                        if evidence_record is not None else ""),
                    target_epoch=(
                        evidence_record.target_epoch
                        if evidence_record is not None else ""),
                    artifact_sha256=(
                        evidence_record.artifact_sha256
                        if evidence_record is not None else ""),
                    observed_at=(
                        evidence_record.observed_at
                        if evidence_record is not None else 0.0),
                )
                if evidence_record is not None:
                    self._bind_tool_evidence_event(evidence_record, tool_event)
            await self._drain_blackboard_requests_once()
    except Exception as exc:
        if str(getattr(exc, "code", "") or "") in {
            "tool_artifact_persistence_failed",
            "event_persistence_failed",
            "blackboard_receipt_persistence_failed",
        }:
            raise
        # Stream rendering is best effort: one malformed step must not kill the
        # worker turn.
        pass


def _materialize_tool_result_spill(
    self, step: StreamStep,
) -> "tuple[str, dict[str, Any]]":
    """Read a bounded Cursor spill without following symlinks.

    Metadata remains observable in the result event, but rejected paths and
    partial/oversized bytes never enter provenance or fact verification.
    """
    audit: "dict[str, Any]" = {
        "path": str(step.spill_path or ""),
        "status": "rejected",
        "reason": "invalid_path",
        "declared_size_bytes": int(step.spill_size_bytes),
        "declared_line_count": int(step.spill_line_count),
    }
    mapped = self._spill_host_path(step.spill_path)
    if mapped is None:
        return "", audit
    cwd, candidate = mapped
    try:
        rel = candidate.relative_to(cwd)
    except ValueError:
        return "", audit
    if not rel.parts or any(part in {"", ".", ".."} for part in rel.parts):
        return "", audit
    flags = os.O_RDONLY | getattr(os, "O_CLOEXEC", 0)
    nofollow = getattr(os, "O_NOFOLLOW", 0)
    directory = getattr(os, "O_DIRECTORY", 0)
    dir_fd: "Optional[int]" = None
    file_fd: "Optional[int]" = None
    try:
        dir_fd = os.open(str(cwd), flags | directory | nofollow)
        for part in rel.parts[:-1]:
            next_fd = os.open(part, flags | directory | nofollow, dir_fd=dir_fd)
            os.close(dir_fd)
            dir_fd = next_fd
        file_fd = os.open(rel.parts[-1], flags | nofollow, dir_fd=dir_fd)
        info = os.fstat(file_fd)
        if not stat.S_ISREG(info.st_mode):
            audit["reason"] = "not_regular"
            return "", audit
        if info.st_size > self._SPILL_OUTPUT_BYTE_CAP:
            audit["reason"] = "oversized"
            audit["actual_size_bytes"] = int(info.st_size)
            return "", audit
        chunks: "list[bytes]" = []
        remaining = self._SPILL_OUTPUT_BYTE_CAP + 1
        while remaining > 0:
            chunk = os.read(file_fd, min(1024 * 1024, remaining))
            if not chunk:
                break
            chunks.append(chunk)
            remaining -= len(chunk)
        data = b"".join(chunks)
        audit["actual_size_bytes"] = len(data)
        if len(data) > self._SPILL_OUTPUT_BYTE_CAP:
            audit["reason"] = "oversized"
            return "", audit
        audit["status"] = "loaded"
        audit["reason"] = ""
        return data.decode("utf-8", errors="replace"), audit
    except OSError as exc:
        audit["reason"] = f"os_error:{exc.errno or 0}"
        return "", audit
    finally:
        if file_fd is not None:
            os.close(file_fd)
        if dir_fd is not None:
            os.close(dir_fd)
