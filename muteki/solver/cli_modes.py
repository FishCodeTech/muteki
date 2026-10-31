"""CliSolver mode implementations. Moved mechanically from cli_solver.py."""
from __future__ import annotations

import asyncio
import tempfile
import time
from pathlib import Path
from typing import Awaitable, Callable, Optional

from muteki.core.events import EventType
from muteki.solver.cli_driver import CliResult
from muteki.solver.cli_results import ModelOutputTruncated
from muteki.solver.execution_budget import ExecutionBudget, ExecutionBudgetExpired
from muteki.solver.cli_prompts import (
    _CHECKPOINT_PROMPT,
    _CTF_WORKER_REMINDERS,
    _RESPOND_ASK_PROMPT,
    _RESPOND_MARK_FALSE_PROMPT,
    _RESPOND_WRITEUP_PROMPT,
    without_operator_input_capability,
)
from muteki.solver.result_codes import (
    RESULT_CANCELLED,
    RESULT_DEAD_END,
    RESULT_EXPLORED,
    RESULT_OOM,
    RESULT_OUTPUT_LIMIT,
    RESULT_DISK_LIMIT,
    RESULT_SOLVED,
    RESULT_STEERED,
    RESULT_TIMED_OUT,
)
from muteki.solver.types import SolveOutcome


_STEP_TOOL_CAP_CHECKPOINT_PROMPT = (
    "本 Step 工具调用步数已达上限，立即停止新增探测实验。"
    "按「本步目标 / 响应判别器（状态码、长度、关键报错） / 剩余缺口」收敛结论："
    "用 submit-fact 简明记录本步已确认的能力原语或实测响应判别器（若明确排除了某条有界假设，配合 mark-deadend 登记排除边界），"
    "随后调用 commit-step 收束本 Step。"
)


def _limit_flags(result: CliResult) -> tuple[bool, bool, bool]:
    """Return (oom, output_limit, disk_limit) for a CliResult-like object."""
    return (
        bool(getattr(result, "oom_killed", False)),
        bool(getattr(result, "output_limit", False)),
        bool(getattr(result, "disk_limit", False)),
    )


def _any_resource_limit(result: CliResult) -> bool:
    oom, out, disk = _limit_flags(result)
    return oom or out or disk


def _ctf_first_slice(self, execute_timeout: int) -> int:
    """Return the first execution lease before an optional soft reminder.

    Zero explicitly disables the reminder for one continuous direct Worker. The
    hard run deadline and cancellation boundary remain owned by the harness.
    """
    reminder_seconds = max(
        0, int(getattr(self, "soft_reminder_seconds", 600) or 0),
    )
    return (
        execute_timeout
        if reminder_seconds == 0
        else min(reminder_seconds, execute_timeout)
    )


def _checkpoint_prompt(self) -> str:
    """Render the forced same-session handoff at the execution lease boundary."""
    if getattr(self.challenge, "mode", "ctf") in {"ctf", "pentest"}:
        return "\n".join(_CTF_WORKER_REMINDERS)
    return _CHECKPOINT_PROMPT.format(
        intent_goal=(self.intent_goal or self._engagement_goal()),
        expected_observable=(self.expected_observable or "(not specified)"),
        stop_condition=(self.stop_condition or "all configured objectives complete"),
        ctx=(self._live_blackboard_context()
             or "(no additional shared state is currently visible)"),
    )


def _checkpoint_resumable(self, result: CliResult, session: Optional[str]) -> bool:
    """Resume only a session that completed at least one successful assistant turn.

    Some CLIs emit a session id before the first turn is persisted.  If that turn
    is steered or times out, resuming the advertised id fails immediately with
    ``No session found`` and turns a recoverable interruption into a Worker error.
    ``_mark_session_if_live`` is the single success fence for every driver.
    """
    if not session or self._control_secret_values:
        return False
    return bool(self._session_established)


async def _run_forced_checkpoint(
    self, *, result: CliResult, session: Optional[str], wd: Path,
    timeout: Optional[float] = None, prompt: Optional[str] = None,
) -> Optional[CliResult]:
    lease = float(self.conclude_timeout if timeout is None else timeout)
    if lease <= 0:
        return None
    if result.cancelled or _any_resource_limit(result):
        return None
    if not _checkpoint_resumable(self, result, session):
        await self._emit(
            EventType.REASONING_DELTA,
            text=f"[{self.driver.name}] checkpoint skipped: no resumable session.\n",
        )
        return None
    await self._emit(
        EventType.REASONING_DELTA,
        text=f"[{self.driver.name}] same-session checkpoint → publish handoff.\n",
    )
    # The previous invocation's monitor retired at its process boundary. Drain
    # once more so guidance that arrived during teardown is present in this exact
    # checkpoint prompt instead of waiting for another Worker.
    self._drain_control()
    checkpoint_prompt = prompt or _checkpoint_prompt(self)
    self._session_handoff_active = True
    try:
        argv, stdin_text = self._resume_invocation(
            checkpoint_prompt, str(session))
        checkpoint = await self._run_invocation(
            argv, cwd=str(wd),
            timeout=lease,
            stdin_text=stdin_text)
    finally:
        self._session_handoff_active = False
    await self._emit_bb(
        "worker_checkpoint",
        worker=self.solver_id,
        intent_id=getattr(self, "_intent_id", ""),
        session=str(session),
        timed_out=bool(checkpoint.timed_out),
    )
    return checkpoint


async def _run_campaign(
    self, *, initial_prompt: str, session: Optional[str], wd: Path,
    absorb: Callable[[CliResult], Awaitable[None]],
    step_scoped: bool = False,
) -> tuple[CliResult, Optional[str]]:
    """Continue one established CLI session within its original finite lease."""
    parent_budget = getattr(self, "execution_budget", None)
    if parent_budget is None:
        budget = ExecutionBudget.start(
            float(self.timeout) + float(self.conclude_timeout),
            conclude_seconds=float(self.conclude_timeout),
        )
    else:
        remaining_after_finalization = parent_budget.remaining_seconds(
            reserve_seconds=parent_budget.finalization_seconds,
        )
        if remaining_after_finalization <= 0:
            return CliResult(text="", timed_out=True), session
        parent_conclude = (
            float(parent_budget.conclude_seconds)
            if float(parent_budget.conclude_seconds or 0.0) > 0
            else float(self.conclude_timeout)
        )
        effective_conclude = min(
            float(self.conclude_timeout),
            parent_conclude,
            max(0.0, remaining_after_finalization / 3.0),
        )
        budget = ExecutionBudget(
            deadline_monotonic=parent_budget.deadline_monotonic,
            deadline_utc=parent_budget.deadline_utc,
            conclude_seconds=effective_conclude,
            finalization_seconds=parent_budget.finalization_seconds,
        )
    # Give every invocation and its tools the same physical execution lease,
    # including callers that did not supply a parent budget.
    if parent_budget is None:
        self.execution_budget = budget
    execution_reserve = budget.conclude_seconds + budget.finalization_seconds
    self._campaign_execution_deadline_unix = (
        min(
            budget.deadline_utc.timestamp(),
            time.time() + budget.remaining_seconds(),
        ) - budget.finalization_seconds)
    try:
        lease = budget.call_limit_seconds(
            _ctf_first_slice(self, self.timeout),
            reserve_seconds=execution_reserve,
        )
    except ExecutionBudgetExpired:
        return CliResult(text="", timed_out=True), session
    argv, stdin_text = self._execute_invocation(initial_prompt, session)
    continuations = 0
    last_context: Optional[str] = self._live_blackboard_context()
    while True:
        result = await self._run_invocation(
            argv, cwd=str(wd), timeout=lease, stdin_text=stdin_text,
        )
        await absorb(result)
        session = None if self._control_secret_values else (result.session or session)
        await self._note_cli_session(session)
        if self._cancel_event.is_set():
            result.cancelled = True
        if (result.cancelled or result.finished or _any_resource_limit(result)
                or self._step_committed or self._cancel_event.is_set()):
            return result, session
        # Controlled timeout/steer can have a nonzero exit. Provider/transport
        # errors surface with their real detail instead of an in-band retry that
        # could replay side effects or silently cold-start another session.
        if result.error:
            raise RuntimeError(result.error)
        if result.returncode not in (None, 0) and not (result.timed_out or result.steered):
            raise RuntimeError(f"{self.driver.name} campaign exited with code {result.returncode}")
        remaining = budget.remaining_seconds(reserve_seconds=execution_reserve)
        continuation_limit = int(getattr(self, "max_continuations", 0) or 0)
        conclude_requested = bool(getattr(self, "_conclude_requested", False))
        if (
            remaining <= 0
            or conclude_requested
            or (continuation_limit > 0 and continuations >= continuation_limit)
        ):
            await self._emit_bb(
                "worker_campaign_boundary", worker=self.solver_id,
                intent_id=self._intent_id, continuations=continuations,
                reason=(
                    "conclude_requested" if conclude_requested
                    else "execution_deadline" if remaining <= 0
                    else "continuation_limit"
                ),
            )
            break
        if not _checkpoint_resumable(self, result, session):
            if result.model_truncated:
                raise ModelOutputTruncated(
                    "Model output reached its token limit before a resumable CLI session was established"
                )
            raise RuntimeError("Campaign continuation requires an established CLI session; cold start is not permitted")
        self._drain_control()
        if self._cancel_event.is_set():
            result.cancelled = True
            return result, session
        if bool(getattr(self, "_conclude_requested", False)):
            break
        reminder_cfg = max(0, int(getattr(self, "soft_reminder_seconds", 600) or 0))
        pending_restart_notice = str(
            getattr(self, "_pending_target_restart_notice", "") or ""
        ).strip()
        use_worker_soft_reminder = bool(
            not pending_restart_notice
            and step_scoped
            and not result.model_truncated
            and not result.steered
            and getattr(self.challenge, "mode", "ctf") in {"ctf", "pentest"}
            and (
                (result.timed_out and reminder_cfg > 0 and not getattr(self, "_soft_reminder_sent", False))
                or not result.timed_out
            )
        )
        if pending_restart_notice:
            self._pending_target_restart_notice = ""
            prompt = pending_restart_notice
            context_reused = True
            context_to_send = ""
            context = ""
        elif use_worker_soft_reminder:
            if result.timed_out and reminder_cfg > 0 and not getattr(self, "_soft_reminder_sent", False):
                reminder_text = _CTF_WORKER_REMINDERS[1]
                self._soft_reminder_sent = True
            else:
                reminder_text = _CTF_WORKER_REMINDERS[0]
            prompt = reminder_text
            context_reused = True
            context_to_send = ""
            context = ""
        else:
            context = self._live_blackboard_context()
            context_reused = bool(context and context == last_context)
            context_to_send = (
                "Shared graph unchanged since the previous continuation. Use the graph "
                "already in this session, or call Blackboard context to inspect it again."
                if context_reused else context
            )
            last_context = context
            scope = (
                "Continue only the assigned Step in this existing session and workspace. "
                f"Step goal: {self.intent_goal}. "
                f"Expected observable: {self.expected_observable or '(not specified)'}. "
                f"Stop condition: {self.stop_condition or 'complete the assigned Step'}. "
                "Use actual tool results to choose the next action within this Step; "
                "use submit-fact + commit-step when a positive finding is confirmed, "
                "or mark-deadend + commit-step when the tested hypothesis is ruled out. "
                "A completed CLI turn alone does not conclude the assigned Step. "
                "Platform confirmation remains authoritative for Flag acceptance. "
                if step_scoped else
                "Continue the same authorized CTF challenge in this existing session and workspace. "
                "A completed CLI turn or an atomic checkpoint does not complete the challenge. "
                "Read current platform verdicts in the shared graph; local candidate registration is not acceptance. "
                "Preserve useful evidence and reusable PoCs, choose the next action from actual results, "
                "and continue until the platform confirms every required flag or the harness stops you. "
            )
            if step_scoped and getattr(self.challenge, "mode", "ctf") == "pentest":
                scope = scope.replace(
                    "Platform confirmation remains authoritative for Flag acceptance. ",
                    "Keep the engagement evidence and report requirements authoritative. ",
                )
            lease_line = (
                ""
                if (step_scoped and getattr(self.challenge, "mode", "ctf") == "ctf")
                else f"The original execution lease has {remaining:.3f} seconds left.\n\n"
            )
            prompt = (
                (
                    "The preceding model response reached its output token limit. "
                    "Continue in this existing session without repeating prior reasoning; "
                    "make the next tool call or give a concise answer. "
                    if result.model_truncated else ""
                )
                + scope
                + lease_line
                + context_to_send
            )
        argv, stdin_text = self._resume_invocation(prompt, str(session))
        continuations += 1
        try:
            lease = budget.call_limit_seconds(
                None,
                reserve_seconds=execution_reserve,
            )
        except ExecutionBudgetExpired:
            remaining = 0.0
            result.timed_out = True
            break
        if use_worker_soft_reminder:
            await self._emit_bb(
                "worker_soft_reminder",
                worker=self.solver_id,
                intent_id=self._intent_id,
                reminder_index=1,
                session=str(session),
                next_slice_s=lease,
            )
        await self._emit_bb(
            "worker_campaign_continued", worker=self.solver_id,
            intent_id=self._intent_id, continuation=continuations,
            session=str(session), remaining_seconds=lease,
            model_truncated=bool(result.model_truncated),
            context_reused=context_reused,
            context_bytes=len(context_to_send.encode("utf-8")),
            shared_graph_bytes=len(context.encode("utf-8")),
        )
    checkpoint_prompt = _CTF_WORKER_REMINDERS[0]
    avail_for_checkpoint = budget.remaining_seconds(
        reserve_seconds=budget.finalization_seconds,
    )
    checkpoint = await _run_forced_checkpoint(
        self, result=result, session=session, wd=wd,
        timeout=min(
            float(self.conclude_timeout),
            avail_for_checkpoint,
        ),
        prompt=checkpoint_prompt,
    )
    if checkpoint is not None:
        session = checkpoint.session or session
        await self._note_cli_session(session)
        await absorb(checkpoint)
        if remaining <= 0:
            checkpoint.timed_out = True
            checkpoint.steered = False
        result = checkpoint
    elif remaining <= 0:
        result.timed_out = True
        result.steered = False
    if result.model_truncated and not result.timed_out:
        raise ModelOutputTruncated(
            "Model output reached its token limit at the campaign continuation boundary"
        )
    return result, session


async def _run_bootstrap(self) -> SolveOutcome:
    await self._emit(EventType.RUN_STARTED, challenge=self.challenge.model_dump())
    mode = "offline" if not self.web_access else "web"
    kb_note = " +KB" if self.kb else ""
    await self._emit(
        EventType.REASONING_DELTA,
        text=f"[{self.driver.name}] delegating to shelled CLI agent — full shell, "
             f"black-box, {mode}{kb_note}.\n")

    # Blackboard collaboration layer: a CLI worker takes the WHOLE challenge as
    # one intent it owns end-to-end. Planning architectures assign and persist an
    # intent before constructing the Worker; use that same id for claim, conclude,
    # and runtime retirement. Direct race Workers have no assigned id and retain
    # the per-worker fallback.
    self._intent_id = (
        getattr(self, "intent_id_assigned", "")
        or f"intent:{self.solver_id}"
    )
    self._last_fact_seq = -1
    # pentest mode → the operator's goal is the engagement objective; CTF mode
    # keeps the original "Solve {name} [{category}]" string byte-for-byte.
    goal = self.intent_goal or self._engagement_goal()
    if not self._record_intent_db(goal):
        raise RuntimeError(
            f"intent ownership unavailable before Worker start: {self._intent_id}")
    # SharedGraph publishes the proposal and claim events from the same committed
    # transaction.  Do not emit a second optimistic pair on the event bus.

    # per-solver scratch workdir (CLI cwd). Service challenges only need the
    # target URL; FILE challenges (crypto/rev/forensics/misc) need their
    # attachments present in the cwd so the agent can inspect them directly.
    wd = Path(self._workdir) if self._workdir else Path(
        tempfile.mkdtemp(prefix=f"muteki-cli-{self.solver_id}-"))
    if not self._workdir:
        self._owned_scratch = wd   # M9: ensure cleanup on ALL exit paths
    wd.mkdir(parents=True, exist_ok=True)
    self._staged_files = self._stage_attachments(wd)

    # CTF keeps one logical Worker/Pi session across its optional soft-reminder
    # boundary, configured execute boundary, and final conclude invocation.
    session = None if self._control_secret_values else self.driver.new_session()
    await self._note_cli_session(session)  # claude pre-seeds; codex stays None
    worker_timed_out = False
    worker_cancelled = False
    worker_steered = False
    all_text = ""
    runtime_error_after_session = ""
    accepted: "Optional[str]" = None
    res: CliResult = CliResult(text="")

    async def _absorb(r: CliResult) -> None:
        """Fold one subprocess result into cost, Blackboard requests, and transcript."""
        nonlocal all_text, accepted, runtime_error_after_session
        self._mark_session_if_live(r)
        await self._emit_empty_stderr_diagnostic(r)
        try:
            result_text = (
                r.text or ""
                if (getattr(self, "campaign_mode", False) and r.model_truncated
                    and not r.error and r.returncode in (None, 0))
                else self._result_text_with_stderr(r)
            )
        except RuntimeError as exc:
            if isinstance(exc, ModelOutputTruncated):
                raise
            if (
                getattr(self, "campaign_mode", False)
                or getattr(self.challenge, "mode", "ctf") not in {"ctf", "pentest"}
                or not self._session_established
            ):
                raise
            runtime_error_after_session = str(exc)
            result_text = r.text or ""
        all_text = (all_text + "\n" + result_text).strip()
        await self._stream_cost(r)
        await self._drain_blackboard_requests_once()
        concl = result_text.strip().splitlines()
        if concl:
            await self._emit(EventType.REASONING_DELTA,
                             text=f"[{self.driver.name}] ⮑ {concl[-1]}\n")
        if self._stream_accepted and accepted is None:
            accepted = self._stream_accepted[0]

    # The execute prompt carries its role-scoped graph projection directly.  Do
    # not materialize the full board in the Worker cwd: that would create a
    # second, unscoped context path outside ContextManifest accounting.
    self._remove_unscoped_board_file(wd)
    initial_prompt = self._build_prompt()
    if getattr(self, "campaign_mode", False):
        res, session = await _run_campaign(
            self, initial_prompt=initial_prompt, session=session, wd=wd,
            absorb=_absorb,
        )
        worker_cancelled = res.cancelled
        worker_steered = res.steered
        worker_timed_out = res.timed_out
        worker_oom_killed, worker_output_limit, worker_disk_limit = _limit_flags(res)
    elif getattr(self.challenge, "mode", "ctf") in {"ctf", "pentest"}:
        execute_timeout = max(1, int(self.timeout))
        first_slice = _ctf_first_slice(self, execute_timeout)
        worker_oom_killed = False
        worker_output_limit = False
        worker_disk_limit = False
        argv, stdin_text = self._execute_invocation(initial_prompt, session)
        res = await self._run_invocation(
            argv, cwd=str(wd), timeout=first_slice, stdin_text=stdin_text)
        await _absorb(res)
        session = None if self._control_secret_values else (res.session or session)
        await self._note_cli_session(session)
        worker_cancelled = res.cancelled
        worker_steered = res.steered
        worker_oom_killed = bool(getattr(res, "oom_killed", False))
        worker_output_limit = bool(getattr(res, "output_limit", False))
        worker_disk_limit = bool(getattr(res, "disk_limit", False))
        worker_timed_out = res.timed_out

        terminal = bool(
            self._step_committed or res.finished or res.cancelled
            or worker_oom_killed
            or worker_output_limit
            or worker_disk_limit
        )
        pending_restart_notice = str(
            getattr(self, "_pending_target_restart_notice", "") or ""
        ).strip()
        if not terminal and res.steered and pending_restart_notice:
            self._pending_target_restart_notice = ""
            argv, stdin_text = self._resume_or_execute_argv(
                pending_restart_notice,
                session,
                fresh_prompt=f"{initial_prompt}\n\n{pending_restart_notice}",
            )
            res = await self._run_invocation(
                argv, cwd=str(wd), timeout=first_slice,
                stdin_text=stdin_text)
            session = (
                None if self._control_secret_values else (res.session or session)
            )
            await self._note_cli_session(session)
            await _absorb(res)
            worker_cancelled = worker_cancelled or res.cancelled
            worker_steered = worker_steered or res.steered
            worker_oom_killed = worker_oom_killed or bool(
                getattr(res, "oom_killed", False))
            worker_output_limit = worker_output_limit or bool(
                getattr(res, "output_limit", False))
            worker_disk_limit = worker_disk_limit or bool(
                getattr(res, "disk_limit", False))
            worker_timed_out = res.timed_out
            terminal = bool(
                self._step_committed or res.finished or res.cancelled
                or worker_oom_killed
                or worker_output_limit
                or worker_disk_limit
            )
        if not terminal and res.timed_out and execute_timeout > first_slice:
            reminder = _CTF_WORKER_REMINDERS[1]
            await self._emit_bb(
                "worker_soft_reminder",
                worker=self.solver_id,
                intent_id=self._intent_id,
                reminder_index=1,
                session=str(session or ""),
                next_slice_s=execute_timeout - first_slice,
            )
            argv, stdin_text = self._resume_or_execute_argv(
                reminder,
                session,
                fresh_prompt=f"{initial_prompt}\n\n{reminder}",
            )
            res = await self._run_invocation(
                argv, cwd=str(wd), timeout=execute_timeout - first_slice,
                stdin_text=stdin_text)
            session = (
                None if self._control_secret_values else (res.session or session)
            )
            await self._note_cli_session(session)
            await _absorb(res)
            worker_cancelled = worker_cancelled or res.cancelled
            worker_steered = worker_steered or res.steered
            worker_oom_killed = worker_oom_killed or bool(
                getattr(res, "oom_killed", False))
            worker_output_limit = worker_output_limit or bool(
                getattr(res, "output_limit", False))
            worker_disk_limit = worker_disk_limit or bool(
                getattr(res, "disk_limit", False))
            worker_timed_out = res.timed_out
            terminal = bool(
                self._step_committed or res.finished or res.cancelled
                or worker_oom_killed
                or worker_output_limit
                or worker_disk_limit
            )

        if not terminal:
            checkpoint_prompt = _CTF_WORKER_REMINDERS[0]
            if res.timed_out:
                checkpoint_prompt += "\n" + _CTF_WORKER_REMINDERS[1]
            checkpoint = await _run_forced_checkpoint(
                self, result=res, session=session, wd=wd,
                timeout=self.conclude_timeout, prompt=checkpoint_prompt)
            if checkpoint is not None:
                res = checkpoint
                session = (
                    None if self._control_secret_values
                    else (checkpoint.session or session)
                )
                await self._note_cli_session(session)
                await _absorb(checkpoint)
                worker_cancelled = worker_cancelled or checkpoint.cancelled
                worker_steered = worker_steered or checkpoint.steered
                worker_oom_killed = worker_oom_killed or bool(
                    getattr(checkpoint, "oom_killed", False))
                worker_output_limit = worker_output_limit or bool(
                    getattr(checkpoint, "output_limit", False))
                worker_disk_limit = worker_disk_limit or bool(
                    getattr(checkpoint, "disk_limit", False))
                worker_timed_out = checkpoint.timed_out
    else:
        argv, stdin_text = self._execute_invocation(initial_prompt, session)
        res = await self._run_invocation(
            argv, cwd=str(wd),
            timeout=min(self.timeout, self.checkpoint_interval),
            stdin_text=stdin_text)
        session = None if self._control_secret_values else (res.session or session)
        await self._note_cli_session(session)
        await _absorb(res)
        worker_cancelled = res.cancelled
        worker_steered = res.steered
        worker_timed_out = res.timed_out
        # OOM-killed by the kernel (a sibling run's container starved the Docker VM —
        # no per-container --memory cap). NOT a timeout: the worker died early with an
        # empty transcript. Surface it separately so it is not misread as budget expiry.
        worker_oom_killed = getattr(res, "oom_killed", False)
        worker_output_limit = getattr(res, "output_limit", False)
        worker_disk_limit = getattr(res, "disk_limit", False)

        checkpoint = await _run_forced_checkpoint(
            self, result=res, session=session, wd=wd)
        if checkpoint is not None:
            await _absorb(checkpoint)
            worker_cancelled = worker_cancelled or checkpoint.cancelled
            worker_steered = worker_steered or checkpoint.steered
            worker_timed_out = checkpoint.timed_out
            worker_oom_killed = worker_oom_killed or bool(
                getattr(checkpoint, "oom_killed", False))
            worker_output_limit = worker_output_limit or bool(
                getattr(checkpoint, "output_limit", False))
            worker_disk_limit = worker_disk_limit or bool(
                getattr(checkpoint, "disk_limit", False))

    await self._drain_blackboard_requests_once()

    # persist the agent's full transcript as a provenance artifact.
    self._transcript_artifact_id = str(self.artifacts.put(
        all_text, suffix=".txt") or "")

    if getattr(self, "_step_committed", False):
        solved = self._flags_complete_for_worker()
        self._note_worker_stop("solved" if solved else "finished")
        found = self._accepted_flags_for_outcome()
        await self._emit_finished(
            flag=(found[0] if solved and found else None),
            flags=found,
            solved=solved,
        )
        return SolveOutcome(
            solved, found[0] if solved and found else None, 1, self.graph,
            f"{self.driver.name} CLI: committed",
            session=session, engine=self.driver.name, workdir=str(wd),
            flags=found, worker_result=self._last_worker_result,
        )

    if worker_steered and accepted is None:
        result_code = RESULT_STEERED
        self._note_worker_stop("steered")
        detail = "Worker was steered before a Flag submission."
        await self._commit_worker_result(status=result_code, result_detail=detail)
        await self._emit_bb("intent_concluded", intent_id=self._intent_id,
                            worker=self.solver_id, result=result_code,
                            result_detail=detail)
        partial_flags = list(self.graph.flags)
        await self._emit_finished(flag=None, flags=partial_flags, solved=False)
        return SolveOutcome(False, None, 1, self.graph,
                            f"{self.driver.name} CLI: {result_code}",
                            flags=partial_flags,
                            worker_result=self._last_worker_result)

    await self._drain_blackboard_requests_once()
    if self._stream_accepted and accepted is None:
        accepted = self._stream_accepted[0]

    if accepted is not None and self._flags_complete_for_worker():
        found = self._accepted_flags_for_outcome()
        # P1-B: conclude in DB unconditionally (was gated on lfs is not None,
        # which dropped the conclude when no fact seq was recorded → the intent
        # stayed status='claimed' and never showed as attempted). The atomic
        # worker-result commit concludes (accumulated observations/dead-ends/
        # pocs first, the conclusion LAST); to_fact_seq now rides inside
        # result_detail since commit_worker_result has no such parameter.
        detail = "Configured Flag count reached."
        await self._commit_worker_result(status=RESULT_SOLVED, result_detail=detail)
        lfs = self._last_fact_seq if self._last_fact_seq > 0 else None
        if lfs is not None:
            detail = f"{detail} to_fact_seq={lfs}"
        await self._emit_bb("intent_concluded", intent_id=self._intent_id,
                            worker=self.solver_id, result=RESULT_SOLVED,
                            to_fact_seq=lfs, result_detail=detail)
        self._note_worker_stop("solved")
        # flags were already accepted+broadcast in the loop; emit the terminal
        # lifecycle event ONCE here carrying all of them.
        await self._emit_finished(flag=accepted, flags=found, solved=True)
        return SolveOutcome(
            True, accepted, 1, self.graph, f"solved via {self.driver.name} CLI",
            session=session, engine=self.driver.name, workdir=str(wd),
            flags=found, worker_result=self._last_worker_result)
    if accepted is not None and not (
        getattr(self, "campaign_mode", False)
        and (worker_cancelled or worker_oom_killed or worker_output_limit or worker_disk_limit)
    ):
        found = self._accepted_flags_for_outcome()
        detail = (
            f"Flag submitted; {len(self._known_flags())}/"
            f"{self._expected_flags()} distinct Flags recorded."
        )
        await self._commit_worker_result(status=RESULT_EXPLORED, result_detail=detail)
        await self._emit_bb(
            "intent_concluded", intent_id=self._intent_id,
            worker=self.solver_id, result=RESULT_EXPLORED,
            result_detail=detail)
        self._note_worker_stop("finished")
        await self._emit_finished(flag=None, flags=found, solved=False)
        return SolveOutcome(
            False, None, 1, self.graph,
            f"{self.driver.name} CLI: partial Flag progress",
            session=session, engine=self.driver.name, workdir=str(wd),
            flags=found, worker_result=self._last_worker_result)

    if worker_cancelled:
        self._note_worker_stop("cancelled")
    elif worker_steered:
        self._note_worker_stop("steered")
    elif worker_oom_killed:
        self._note_worker_stop("oom")
    elif worker_output_limit:
        self._note_worker_stop("output_limit")
    elif worker_disk_limit:
        self._note_worker_stop("disk_limit")
    elif worker_timed_out:
        self._note_worker_stop("timeout")
    else:
        self._note_worker_stop("finished")
    deadends = list(getattr(self, "_pending_dead_ends", None) or [])
    if deadends:
        result_code = RESULT_DEAD_END
        detail = f"Worker explicitly ruled out: {deadends[0].reason}"
    elif worker_cancelled:
        result_code = RESULT_CANCELLED
        detail = "Worker was cancelled before a Flag submission."
    elif worker_steered:
        result_code = RESULT_STEERED
        detail = "Worker was steered before a Flag submission."
    elif worker_oom_killed:
        result_code = RESULT_OOM
        detail = "Killed by the OOM killer before a Flag submission."
    elif worker_output_limit:
        result_code = RESULT_OUTPUT_LIMIT
        detail = "Exceeded stdout/stderr output budget before a Flag submission."
    elif worker_disk_limit:
        result_code = RESULT_DISK_LIMIT
        detail = "Exceeded workdir disk budget before a Flag submission."
    elif worker_timed_out:
        result_code = RESULT_TIMED_OUT
        detail = "Timed out before a Flag submission."
    else:
        result_code = RESULT_EXPLORED
        if runtime_error_after_session:
            detail = (
                "Worker session ended with a runtime error and its final "
                f"checkpoint produced no Fact: {runtime_error_after_session}"
            )
        else:
            detail = "Explored without a Flag submission."
    # P1-B: conclude in the DB intents table too (was only _emit_bb). result
    # carries WHAT this whole-challenge attempt amounted to, so the next
    # bootstrap worker's board shows "this direction was already tried → <reason>"
    # instead of re-running the same recon. The commit lands the run's
    # accumulated observations/dead-ends/pocs in the same transaction.
    await self._commit_worker_result(status=result_code, result_detail=detail)
    await self._emit_bb("intent_concluded", intent_id=self._intent_id,
                        worker=self.solver_id, result=result_code,
                        result_detail=detail)
    partial_flags = list(self.graph.flags)
    await self._emit_finished(flag=None, flags=partial_flags, solved=False)
    # scratch cleanup is centralized in run()'s finally (M9) via _owned_scratch.
    return SolveOutcome(False, None, 1, self.graph,
                        f"{self.driver.name} CLI: no Flag submission",
                        flags=partial_flags,
                        worker_result=self._last_worker_result)


async def _run_explore(self) -> SolveOutcome:
    """Claim one intent and run its scoped exploration Worker."""
    await self._emit(EventType.RUN_STARTED, challenge=self.challenge.model_dump())
    mode_str = "offline" if not self.web_access else "web"
    kb_note = " +KB" if self.kb else ""
    await self._emit(
        EventType.REASONING_DELTA,
        text=f"[{self.driver.name}] explore mode — intent: {self.intent_goal}, "
             f"{mode_str}{kb_note}\n")

    self._intent_id = getattr(self, "intent_id_assigned", "") or f"intent:{self.solver_id}"
    self._last_fact_seq = -1
    await self._emit_bb("intent_claimed", intent_id=self._intent_id,
                        worker=self.solver_id)

    wd = Path(self._workdir) if self._workdir else Path(
        tempfile.mkdtemp(prefix=f"muteki-explore-{self.solver_id}-"))
    if not self._workdir:
        self._owned_scratch = wd   # M9: ensure cleanup on ALL exit paths
    wd.mkdir(parents=True, exist_ok=True)
    self._staged_files = self._stage_attachments(wd)

    session = None if self._control_secret_values else self.driver.new_session()
    await self._note_cli_session(session)  # claude pre-seeds; codex stays None
    worker_cancelled = False
    worker_steered = False
    worker_oom_killed = False
    worker_output_limit = False
    worker_disk_limit = False
    worker_timed_out = False
    all_text = ""
    runtime_error_after_handoff = ""
    res: CliResult = CliResult(text="")

    async def _absorb(r: CliResult) -> None:
        nonlocal all_text, runtime_error_after_handoff
        self._mark_session_if_live(r)
        await self._emit_empty_stderr_diagnostic(r)
        await self._stream_cost(r)
        await self._drain_blackboard_requests_once()
        try:
            result_text = (
                r.text or ""
                if (getattr(self, "same_session_continuation", False)
                    and r.model_truncated and not r.error
                    and r.returncode in (None, 0))
                else self._result_text_with_stderr(r)
            )
        except RuntimeError as exc:
            if isinstance(exc, ModelOutputTruncated):
                raise
            resumable_or_handed_off = bool(
                self._session_established
                or int(getattr(self, "_last_fact_seq", -1) or -1) > 0
                or getattr(self, "_stream_accepted", None)
                or getattr(self, "_pending_dead_ends", None)
                or getattr(self, "_pending_pocs", None)
            )
            if (
                getattr(self, "same_session_continuation", False)
                or getattr(self.challenge, "mode", "ctf") not in {"ctf", "pentest"}
                or not resumable_or_handed_off
            ):
                raise
            runtime_error_after_handoff = str(exc)
            result_text = r.text or ""
        all_text = (all_text + "\n" + result_text).strip()

    self._remove_unscoped_board_file(wd)
    initial_prompt = self._build_explore_prompt()
    if getattr(self, "same_session_continuation", False):
        res, session = await _run_campaign(
            self, initial_prompt=initial_prompt, session=session, wd=wd,
            absorb=_absorb, step_scoped=True,
        )
        worker_cancelled = res.cancelled
        worker_steered = res.steered
        worker_timed_out = res.timed_out
        worker_oom_killed, worker_output_limit, worker_disk_limit = _limit_flags(res)
    elif getattr(self.challenge, "mode", "ctf") in {"ctf", "pentest"}:
        execute_timeout = max(1, int(self.timeout))
        first_slice = _ctf_first_slice(self, execute_timeout)
        argv, stdin_text = self._execute_invocation(initial_prompt, session)
        res = await self._run_invocation(
            argv, cwd=str(wd), timeout=first_slice, stdin_text=stdin_text)
        await _absorb(res)
        session = None if self._control_secret_values else (res.session or session)
        await self._note_cli_session(session)
        worker_cancelled = res.cancelled
        worker_steered = res.steered
        worker_oom_killed = bool(getattr(res, "oom_killed", False))
        worker_output_limit = bool(getattr(res, "output_limit", False))
        worker_disk_limit = bool(getattr(res, "disk_limit", False))
        worker_timed_out = res.timed_out

        terminal = bool(
            self._step_committed or res.finished or res.cancelled
            or worker_oom_killed
            or worker_output_limit
            or worker_disk_limit
        )
        pending_restart_notice = str(
            getattr(self, "_pending_target_restart_notice", "") or ""
        ).strip()
        if not terminal and res.steered and pending_restart_notice:
            self._pending_target_restart_notice = ""
            argv, stdin_text = self._resume_or_execute_argv(
                pending_restart_notice,
                session,
                fresh_prompt=f"{initial_prompt}\n\n{pending_restart_notice}",
            )
            res = await self._run_invocation(
                argv, cwd=str(wd), timeout=first_slice,
                stdin_text=stdin_text)
            session = (
                None if self._control_secret_values else (res.session or session)
            )
            await self._note_cli_session(session)
            await _absorb(res)
            worker_cancelled = worker_cancelled or res.cancelled
            worker_steered = worker_steered or res.steered
            worker_oom_killed = worker_oom_killed or bool(
                getattr(res, "oom_killed", False))
            worker_output_limit = worker_output_limit or bool(
                getattr(res, "output_limit", False))
            worker_disk_limit = worker_disk_limit or bool(
                getattr(res, "disk_limit", False))
            worker_timed_out = res.timed_out
            terminal = bool(
                self._step_committed or res.finished or res.cancelled
                or worker_oom_killed
                or worker_output_limit
                or worker_disk_limit
            )
        if not terminal and res.timed_out and execute_timeout > first_slice:
            reminder = _CTF_WORKER_REMINDERS[1]
            await self._emit_bb(
                "worker_soft_reminder",
                worker=self.solver_id,
                intent_id=self._intent_id,
                reminder_index=1,
                session=str(session or ""),
                next_slice_s=execute_timeout - first_slice,
            )
            argv, stdin_text = self._resume_or_execute_argv(
                reminder,
                session,
                fresh_prompt=f"{initial_prompt}\n\n{reminder}",
            )
            res = await self._run_invocation(
                argv, cwd=str(wd), timeout=execute_timeout - first_slice,
                stdin_text=stdin_text)
            session = (
                None if self._control_secret_values else (res.session or session)
            )
            await self._note_cli_session(session)
            await _absorb(res)
            worker_cancelled = worker_cancelled or res.cancelled
            worker_steered = worker_steered or res.steered
            worker_oom_killed = worker_oom_killed or bool(
                getattr(res, "oom_killed", False))
            worker_output_limit = worker_output_limit or bool(
                getattr(res, "output_limit", False))
            worker_disk_limit = worker_disk_limit or bool(
                getattr(res, "disk_limit", False))
            worker_timed_out = res.timed_out
            terminal = bool(
                self._step_committed or res.finished or res.cancelled
                or worker_oom_killed
                or worker_output_limit
                or worker_disk_limit
            )

        if not terminal:
            checkpoint_prompt = _CTF_WORKER_REMINDERS[0]
            if res.timed_out:
                checkpoint_prompt += "\n" + _CTF_WORKER_REMINDERS[1]
            checkpoint = await _run_forced_checkpoint(
                self, result=res, session=session, wd=wd,
                timeout=self.conclude_timeout, prompt=checkpoint_prompt)
            if checkpoint is not None:
                res = checkpoint
                session = (
                    None if self._control_secret_values
                    else (checkpoint.session or session)
                )
                await self._note_cli_session(session)
                await _absorb(checkpoint)
                worker_cancelled = worker_cancelled or checkpoint.cancelled
                worker_steered = worker_steered or checkpoint.steered
                worker_oom_killed = worker_oom_killed or bool(
                    getattr(checkpoint, "oom_killed", False))
                worker_output_limit = worker_output_limit or bool(
                    getattr(checkpoint, "output_limit", False))
                worker_disk_limit = worker_disk_limit or bool(
                    getattr(checkpoint, "disk_limit", False))
                worker_timed_out = checkpoint.timed_out
    else:
        argv, stdin_text = self._execute_invocation(initial_prompt, session)
        res = await self._run_invocation(
            argv, cwd=str(wd),
            timeout=min(self.timeout, self.checkpoint_interval),
            stdin_text=stdin_text)
        session = None if self._control_secret_values else (res.session or session)
        await self._note_cli_session(session)
        await _absorb(res)
        worker_cancelled = res.cancelled
        worker_steered = res.steered
        worker_oom_killed = bool(getattr(res, "oom_killed", False))
        worker_output_limit = bool(getattr(res, "output_limit", False))
        worker_disk_limit = bool(getattr(res, "disk_limit", False))
        worker_timed_out = res.timed_out

        checkpoint = await _run_forced_checkpoint(
            self, result=res, session=session, wd=wd)
        if checkpoint is not None:
            await _absorb(checkpoint)
            worker_cancelled = worker_cancelled or checkpoint.cancelled
            worker_steered = worker_steered or checkpoint.steered
            worker_oom_killed = worker_oom_killed or bool(
                getattr(checkpoint, "oom_killed", False))
            worker_output_limit = worker_output_limit or bool(
                getattr(checkpoint, "output_limit", False))
            worker_disk_limit = worker_disk_limit or bool(
                getattr(checkpoint, "disk_limit", False))
            worker_timed_out = checkpoint.timed_out

    self._transcript_artifact_id = str(self.artifacts.put(
        all_text, suffix=".txt") or "")
    accepted = self._stream_accepted[0] if self._stream_accepted else None
    deadends = list(getattr(self, "_pending_dead_ends", None) or [])

    if getattr(self, "_step_committed", False):
        solved = self._flags_complete_for_worker()
        self._note_worker_stop("solved" if solved else "finished")
        found = self._accepted_flags_for_outcome()
        await self._emit_finished(
            flag=(found[0] if solved and found else None),
            flags=found,
            solved=solved,
        )
        return SolveOutcome(
            solved, found[0] if solved and found else None, 1, self.graph,
            f"{self.driver.name} explore: committed",
            session=session, engine=self.driver.name, workdir=str(wd),
            flags=found, worker_result=self._last_worker_result,
        )

    if accepted is not None and self._flags_complete_for_worker():
        # #13: conclude UNCONDITIONALLY (was gated on `lfs is not None`). An explore
        # worker that solved but recorded no fact-seq (lfs is None) used to leave
        # its intent status='claimed'; the lease then expired and _open_intents
        # re-dispatched the already-solved direction to a fresh worker. The atomic
        # worker-result commit concludes (owner-fenced; result="solved"
        # intentionally bypasses the owner fence per conclude_intent), with the
        # run's accumulated claims landing in the same transaction. Mirrors the
        # dead-end exit below, which already concludes unconditionally.
        detail = "Configured Flag count reached."
        await self._commit_worker_result(status=RESULT_SOLVED, result_detail=detail)
        lfs = self._last_fact_seq if self._last_fact_seq > 0 else None
        if lfs is not None:
            detail = f"{detail} to_fact_seq={lfs}"
        await self._emit_bb("intent_concluded", intent_id=self._intent_id,
                            worker=self.solver_id, result=RESULT_SOLVED,
                            to_fact_seq=lfs, result_detail=detail)
        self._note_worker_stop("solved")
        found = self._accepted_flags_for_outcome()
        await self._emit_finished(flag=accepted, flags=found, solved=True)
        return SolveOutcome(
            True, accepted, 1, self.graph,
            f"solved via {self.driver.name} explore",
            session=session, engine=self.driver.name, workdir=str(wd),
            flags=found, worker_result=self._last_worker_result)
    if accepted is not None:
        found = self._accepted_flags_for_outcome()
        detail = (
            f"Flag submitted; {len(self._known_flags())}/"
            f"{self._expected_flags()} distinct Flags recorded."
        )
        if runtime_error_after_handoff:
            detail += " Worker runtime ended after the shared handoff."
        await self._commit_worker_result(status=RESULT_EXPLORED, result_detail=detail)
        await self._emit_bb(
            "intent_concluded", intent_id=self._intent_id,
            worker=self.solver_id, result=RESULT_EXPLORED,
            result_detail=detail)
        self._note_worker_stop("finished")
        await self._emit_finished(flag=None, flags=found, solved=False)
        return SolveOutcome(
            False, None, 1, self.graph,
            f"{self.driver.name} explore: partial Flag progress",
            session=session, engine=self.driver.name, workdir=str(wd),
            flags=found, worker_result=self._last_worker_result)

    if worker_cancelled:
        self._note_worker_stop("cancelled")
    elif worker_steered:
        self._note_worker_stop("steered")
    elif worker_oom_killed:
        self._note_worker_stop("oom")
    elif worker_output_limit:
        self._note_worker_stop("output_limit")
    elif worker_disk_limit:
        self._note_worker_stop("disk_limit")
    elif worker_timed_out:
        self._note_worker_stop("timeout")
    else:
        self._note_worker_stop("finished")
    # An explicit dead-end is the Worker's bounded conclusion for this step.
    # The lease expiring during the same turn must not overwrite that durable
    # result with ``timed_out`` and cause the Coordinator to reopen work that
    # was already conclusively exhausted.
    if deadends:
        result_label = RESULT_DEAD_END
        result_detail = f"Worker explicitly ruled out: {deadends[0].reason}"
    elif worker_cancelled:
        result_label = RESULT_CANCELLED
        result_detail = "Worker was cancelled before finishing this intent."
    elif worker_steered:
        result_label = RESULT_STEERED
        result_detail = "Worker was steered before finishing this intent."
    elif worker_oom_killed:
        result_label = RESULT_OOM
        result_detail = "Worker was terminated by OOM before finishing this intent."
    elif worker_output_limit:
        result_label = RESULT_OUTPUT_LIMIT
        result_detail = (
            "Worker exceeded the stdout/stderr output budget before finishing "
            "this intent."
        )
    elif worker_disk_limit:
        result_label = RESULT_DISK_LIMIT
        result_detail = (
            "Worker exceeded the workdir disk budget before finishing this intent."
        )
    elif worker_timed_out:
        result_label = RESULT_TIMED_OUT
        result_detail = "Timed out before finishing this intent."
    elif runtime_error_after_handoff:
        result_label = "error"
        result_detail = (
            "Worker session ended with a runtime error and its final "
            "checkpoint produced no Fact: "
            f"{runtime_error_after_handoff}"
        )
    else:
        result_label = RESULT_EXPLORED
        result_detail = "Explored this intent and produced no explicit dead-end."
    # ALWAYS flip the intent to done on exit — even when this worker recorded NO
    # new fact (lfs is None). Previously the DB conclude was gated on `lfs is not
    # None`, so a "need operator" / no-fact dead-end left the intent status=
    # 'claimed'; its lease then expired and _open_intents re-dispatched the SAME
    # stale direction to a fresh worker, forever (run-11190: 93/173 claimed
    # intents never concluded → 238-worker churn on L2). The owner-fence in
    # conclude_intent makes a late conclude safe (a re-dispatched intent owned by
    # a newer worker is not clobbered). Concluding a no-fact intent retires the
    # direction so it stops resurrecting. The commit also lands this run's
    # accumulated observations/dead-ends/pocs in the same transaction; the
    # to_fact_seq pointer now rides inside result_detail (commit_worker_result
    # has no such parameter).
    await self._commit_worker_result(status=result_label,
                                     result_detail=result_detail)
    lfs = self._last_fact_seq if self._last_fact_seq > 0 else None
    if lfs is not None:
        result_detail = f"{result_detail} to_fact_seq={lfs}"
    await self._emit_bb("intent_concluded", intent_id=self._intent_id,
                        worker=self.solver_id, result=result_label,
                        to_fact_seq=lfs, result_detail=result_detail)
    partial_flags = list(self.graph.flags)
    await self._emit_finished(flag=None, flags=partial_flags, solved=False)
    # scratch cleanup is centralized in run()'s finally (M9) via _owned_scratch.
    return SolveOutcome(False, None, 1, self.graph,
                        f"{self.driver.name} explore: {result_label}",
                        flags=partial_flags,
                        worker_result=self._last_worker_result)


async def _run_review(self) -> SolveOutcome:
    """Review-Arbiter: audit a scoped projection and emit narrow fact-review
    proposals/findings. It never plans, accepts flags, or marks the run solved."""
    await self._emit(EventType.RUN_STARTED, challenge=self.challenge.model_dump())
    await self._emit(
        EventType.REASONING_DELTA,
        text=f"[{self.driver.name}] review-arbiter mode — auditing swarm trajectory.\n")

    self._intent_id = getattr(self, "intent_id_assigned", "") or f"review:{self.solver_id}"
    self._last_fact_seq = -1
    await self._emit_bb("intent_claimed", intent_id=self._intent_id,
                        worker=self.solver_id, worker_class="review")

    wd = Path(self._workdir) if self._workdir else Path(
        tempfile.mkdtemp(prefix=f"muteki-review-{self.solver_id}-"))
    if not self._workdir:
        self._owned_scratch = wd
    wd.mkdir(parents=True, exist_ok=True)
    self._staged_files = self._stage_attachments(wd)

    session = None if self._control_secret_values else self.driver.new_session()
    await self._note_cli_session(session)
    self._remove_unscoped_board_file(wd)
    prompt = self._build_review_prompt()
    argv, stdin_text = self._execute_invocation(prompt, session)
    res = await self._run_invocation(
        argv, cwd=str(wd), timeout=self.timeout, stdin_text=stdin_text)
    session = None if self._control_secret_values else (res.session or session)
    self._mark_session_if_live(res)
    await self._note_cli_session(session)
    await self._emit_empty_stderr_diagnostic(res)
    text = self._result_text_with_stderr(res)
    await self._stream_cost(res)
    await self._drain_blackboard_requests_once()

    safe_text = text
    aid = self.artifacts.put(safe_text, suffix=".txt")
    self._transcript_artifact_id = str(aid or "")
    applied = int(self._accepted_blackboard_requests)
    controlled_stop = self._controlled_result_stop(res)
    if controlled_stop is not None:
        result_code, stop_tag, detail = controlled_stop
        self._note_worker_stop(stop_tag)
        await self._commit_worker_result(status=result_code, result_detail=detail)
        await self._emit_bb(
            "intent_concluded", intent_id=self._intent_id,
            worker=self.solver_id, result=result_code,
            result_detail=detail, artifact_id=aid,
        )
        partial_flags = list(self.graph.flags)
        await self._emit_finished(
            flag=None, flags=partial_flags, solved=False)
        return SolveOutcome(
            False, None, 1, self.graph,
            f"{self.driver.name} review: {result_code}",
            session=session, engine=self.driver.name,
            workdir=str(wd), flags=partial_flags,
            worker_result=self._last_worker_result,
        )
    if applied == 0:
        try:
            seq = self.shared_graph.add_review_proposal(
                actor=self.solver_id, marker="REVIEW_FINDING",
                payload={"kind": "no_action", "severity": "info",
                         "summary": "Review completed without a Skill operation."},
            ) if self.shared_graph is not None else 0
            await self._emit_bb(
                "review_proposal", seq=seq, marker="REVIEW_FINDING",
                tier="tier1", severity="info",
                summary="Review completed without a Skill operation.")
            applied += 1
        except Exception:
            pass

    result = f"reviewed: {applied} proposal(s)"
    # Review proposals already persisted incrementally above; the commit here
    # carries no observations and only concludes the review intent.
    await self._commit_worker_result(status=result)
    await self._emit_bb("intent_concluded", intent_id=self._intent_id,
                        worker=self.solver_id, result=result,
                        artifact_id=aid)
    self._note_worker_stop("finished")
    await self._emit_finished(flag=None, flags=list(self.graph.flags), solved=False)
    return SolveOutcome(False, None, 1, self.graph,
                        f"{self.driver.name} review: {result}",
                        session=session, engine=self.driver.name,
                        workdir=str(wd), flags=list(self.graph.flags),
                        worker_result=self._last_worker_result)


async def _run_respond(self) -> SolveOutcome:
    """Standby: Ask/Writeup use fresh snapshot-only turns.
    Only execution commands may resume the original solve session.

    ask/writeup are conversational — their output streams to the deck as the
    worker's reply and (writeup) is persisted; neither produces a Flag.
    mark_false re-opens the solve and waits for a new Skill submission."""
    action = (self.hitl_cmd.get("action") or "ask").lower()
    text = (self.hitl_cmd.get("text") or "").strip()
    followup_id = str(self.hitl_cmd.get("followup_id") or "")
    if action in {"ask", "writeup"}:
        await self._emit(
            EventType.FOLLOWUP_STARTED,
            followup_id=followup_id,
            kind=action,
            question=text,
        )
    else:
        await self._emit(EventType.RUN_STARTED, challenge=self.challenge.model_dump())
        await self._emit(
            EventType.REASONING_DELTA,
            text=f"[{self.driver.name}] standby — resuming session for "
                 f"{action}{(': ' + text) if text else ''}\n")

    # per-worker cwd: reuse the winner's workdir if it still exists (keeps any
    # files it downloaded), else a fresh scratch dir. Computed FIRST so we can
    # write a fresh board file into THIS worker's wd before building the prompt
    # (standby reuses a possibly-stale winner dir → rewrite, don't trust a
    # leftover board file).
    _reuse_winner = action not in {"ask", "writeup"} and bool(self._workdir and Path(self._workdir).exists())
    wd = Path(self._workdir) if _reuse_winner else Path(tempfile.mkdtemp(
        prefix=f"muteki-respond-{self.solver_id}-",
        dir=self._workdir if action in {"ask", "writeup"} and self.container is not None else None))
    if not _reuse_winner:
        self._owned_scratch = wd   # M10: respond mkdtemp was never cleaned before
    wd.mkdir(parents=True, exist_ok=True)
    if action in {"ask", "writeup"} and self.container is not None:
        from muteki.solver.container_exec import _chown_tree_to_worker
        await asyncio.to_thread(_chown_tree_to_worker, str(wd), image=self.container.image)
    if action not in {"ask", "writeup"}:
        self._write_board_file(wd)

    # build the prompt for this command
    if action == "mark_false":
        note = self._board_context() or ""
        prompt = _RESPOND_MARK_FALSE_PROMPT.format(
            flag=self.hitl_cmd.get("flag") or "(the reported flag)", note=note)
    elif action == "writeup":
        if str(getattr(self.challenge, "mode", "ctf")) == "pentest":
            raise RuntimeError("Pentest reports are generated from the shared graph; use the Pentest report endpoint")
        from muteki.solver.followup_report import report_snapshot
        prompt = _RESPOND_WRITEUP_PROMPT + await asyncio.to_thread(report_snapshot, self)
    elif action == "ask":
        from muteki.solver.followup_report import question_prompt, report_snapshot
        snapshot = await asyncio.to_thread(report_snapshot, self)
        prompt = question_prompt(text, snapshot)
    else:  # execution guidance retains its existing behavior
        question = text or "(no question text)"
        if action == "redirect":
            endpoint = str(self.hitl_cmd.get("url") or "").strip()
            # A redirect is not delivered merely because it reached
            # _target_override.  This one-shot standby path does not call the
            # normal solve prompt builder, so the resumed CLI must receive the
            # endpoint in this exact turn before the reservation can be
            # committed at Popen.
            if endpoint:
                question = (
                    "Continue the investigation against this new target "
                    f"endpoint: {endpoint}"
                    + (f"\nOperator note: {text}" if text else "")
                )
        prompt = _RESPOND_ASK_PROMPT.format(text=question)

    if not bool(getattr(self.challenge, "allow_operator_input", True)):
        prompt = without_operator_input_capability(prompt)

    # Ask/Writeup always use fresh observers. Other commands resume the winner's
    # session when available, or receive the normal board context.
    # MIGRATION NOTE (DESIGN_single_shot_migration.md, D-1): this is the ONLY
    # resume path the single-shot migration keeps. standby is a SINGLE cold
    # answer-turn, not a long-lived loop accumulating context across a solve —
    # so resuming the winner's session here doesn't reintroduce the bloat the
    # migration removed; it just gives the answer the winner's full memory.
    stdin_text: "Optional[str]" = None
    if action in {"ask", "writeup"}:
        # A fresh observer gets inline evidence, never the solve-oriented board
        # pointer or a session that can resume its unfinished investigation.
        argv, stdin_text = self._execute_invocation(prompt, None)
    elif self._control_secret_values:
        # The winner session may contain durable history and Cursor cannot make
        # it ephemeral.  A secret-bearing response is always a fresh stdin-only
        # turn with an explicit board snapshot instead of a resume.
        board = self._board_context()
        if board:
            prompt = prompt + "\n" + board
        argv, stdin_text = self._execute_invocation(prompt, None)
    elif self.resume_session:
        await self._note_cli_session(self.resume_session)
        argv, stdin_text = self._resume_invocation(
            prompt, self.resume_session)
    else:
        if action != "mark_false":
            # no session to resume → seed the conversational prompt with the
            # board so the worker still has the solve context.
            board = self._board_context()
            if board:
                prompt = prompt + "\n" + board
        session = None if self._control_secret_values else self.driver.new_session()
        await self._note_cli_session(session)
        argv, stdin_text = self._execute_invocation(prompt, session)

    # Conversational follow-ups should return promptly. A resumed model can
    # otherwise start another investigation and leave the deck pending for the
    # normal 40-minute solve timeout. mark_false remains a real re-solve and
    # retains the longer bound.
    respond_timeout = 300 if action in {"ask", "writeup"} else 1200
    res: CliResult = await self._run_invocation(
        argv, cwd=str(wd), timeout=min(self.timeout, respond_timeout),
        stdin_text=stdin_text)
    await self._emit_empty_stderr_diagnostic(res)
    await self._stream_cost(res)
    all_text = self._result_text_with_stderr(res)
    safe_all_text = all_text

    if action in {"ask", "writeup"}:
        observed_rc = res.returncode
        if observed_rc is None:
            runtime_rc = (res.runtime_status or {}).get("rc")
            try:
                observed_rc = int(runtime_rc) if runtime_rc is not None else None
            except (TypeError, ValueError):
                observed_rc = None
        if res.timed_out:
            raise RuntimeError(f"standby {action} worker timed out")
        if res.oom_killed:
            raise RuntimeError(f"standby {action} worker was terminated by OOM")
        if getattr(res, "output_limit", False):
            raise RuntimeError(f"standby {action} worker exceeded the output budget")
        if getattr(res, "disk_limit", False):
            raise RuntimeError(f"standby {action} worker exceeded the working directory budget")
        if res.cancelled or res.steered:
            raise RuntimeError(f"standby {action} worker did not complete")
        if observed_rc not in {None, 0}:
            raise RuntimeError(
                f"standby {action} worker exited with code {observed_rc}")
        if not safe_all_text.strip():
            raise RuntimeError(f"standby {action} worker returned an empty response")

    # stream the reply to the deck (the worker's answer / writeup body).
    if safe_all_text.strip() and action not in {"ask", "writeup"}:
        await self._emit(
            EventType.TEXT_MESSAGE_DELTA,
            text=safe_all_text.strip(),
            main_thread=action in {"ask", "writeup"},
        )

    if action == "mark_false":
        await self._drain_blackboard_requests_once()
        accepted = self._stream_accepted[0] if self._stream_accepted else None
        # Preserve the response as an artifact, but do not manufacture a
        # candidate from its transcript tail when the re-solve misses.
        self.artifacts.put(
            all_text, suffix=".txt")
        if accepted is not None and self._flags_complete_for_worker():
            self._note_worker_stop("solved")
            found = self._accepted_flags_for_outcome()
            await self._emit_finished(flag=accepted, flags=found, solved=True)
            return SolveOutcome(
                True, accepted, 1, self.graph,
                f"re-solved via {self.driver.name} standby",
                session=(None if self._control_secret_values
                         else res.session or self.resume_session),
                engine=self.driver.name, workdir=str(wd), flags=found,
                worker_result=self._last_worker_result)
        if accepted is not None:
            found = self._accepted_flags_for_outcome()
            await self._emit_finished(flag=None, flags=found, solved=False)
            return SolveOutcome(
                False, None, 1, self.graph,
                f"{self.driver.name} standby: partial Flag progress",
                session=(None if self._control_secret_values
                         else res.session or self.resume_session),
                engine=self.driver.name, workdir=str(wd), flags=found,
                worker_result=self._last_worker_result)
        # A timed-out/cancelled standby can still have emitted attributed tool
        # output shortly before its stop.  Harvest it above first; only then
        # classify the typed stop when no candidate survived the host gate.
        controlled_stop = self._controlled_result_stop(res)
        if controlled_stop is not None:
            result_code, stop_tag, _detail = controlled_stop
            self._note_worker_stop(stop_tag)
            self.artifacts.put(
                all_text, suffix=".txt")
            partial_flags = list(self.graph.flags)
            await self._emit_finished(
                flag=None, flags=partial_flags, solved=False)
            return SolveOutcome(
                False, None, 1, self.graph,
                f"{self.driver.name} standby: {result_code}",
                flags=partial_flags,
                worker_result=self._last_worker_result,
            )
        partial_flags = list(self.graph.flags)
        await self._emit_finished(flag=None, flags=partial_flags, solved=False)
        return SolveOutcome(False, None, 1, self.graph,
                            f"{self.driver.name} standby: still searching",
                            flags=partial_flags,
                            worker_result=self._last_worker_result)

    # ask / writeup: record a non-Flag artifact and no RUN_FINISHED solved-state
    # churn. The writeup body is persisted by the
    # standby driver (it owns the run dir); here we just surface the text.
    self._note_worker_stop("finished")
    return SolveOutcome(
        False, None, 1, self.graph, f"{self.driver.name} standby {action}",
        session=(None if self._control_secret_values or action in {"ask", "writeup"}
                 else res.session or self.resume_session),
        engine=self.driver.name, workdir=str(wd), reply=safe_all_text.strip(),
        flags=list(self.graph.flags),
        worker_result=self._last_worker_result)
