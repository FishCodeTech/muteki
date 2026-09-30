"""Completed-worker retirement and outcome folding."""

from __future__ import annotations

from muteki.solver.cli_launch_check import launch_failure_code


_MAX_CONSECUTIVE_WORKER_DISPATCH_FAILURES = 5


async def _observe_started_workers(self, state=None, solvers=()) -> None:
    """A real process-start receipt, rather than task creation, clears the streak."""
    from muteki.swarm.coordinator_state import emit_scheduler_bb

    scheduled = tuple(state.task_solvers.values()) if state is not None else ()
    for solver in (*scheduled, *tuple(solvers)):
        sid = str(getattr(solver, "solver_id", "") or "")
        if (not sid or bool(getattr(solver, "_muteki_worker_start_observed", False))
                or bool(getattr(solver, "_remote_start_uncertain", False))
                or not bool(getattr(solver, "_runtime_process_started", False))):
            continue
        solver._muteki_worker_start_observed = True
        previous = int(getattr(self, "_consecutive_worker_dispatch_failures", 0))
        self._consecutive_worker_dispatch_failures = 0
        if previous:
            fields = {"worker": sid, "previous_consecutive_failures": previous}
            if state is None:
                await self._emit_coord_bb("worker_dispatch_recovered", **fields)
            else:
                await emit_scheduler_bb(
                    self, state, "worker_dispatch_recovered", **fields,
                )


async def _record_worker_dispatch_failure(
    self, state=None, *, worker: str, engine: str, detail: str,
    solver=None, active_solvers=None,
) -> bool:
    """Count rejected/prestart dispatches; a running Worker failure is separate."""
    from muteki.swarm.coordinator_state import emit_scheduler_bb

    if (solver is not None
            and bool(getattr(solver, "_muteki_worker_dispatch_failure_recorded", False))):
        return bool(getattr(self, "_worker_dispatch_failure_limit_reached", False))
    active = (state.task_solvers if state is not None else active_solvers) or {}
    await _observe_started_workers(self, state, active.values())
    if solver is not None:
        solver._muteki_worker_dispatch_failure_recorded = True
    streak = int(getattr(self, "_consecutive_worker_dispatch_failures", 0)) + 1
    self._consecutive_worker_dispatch_failures = streak
    fields = {
        "worker": worker, "engine": engine,
        "consecutive_failures": streak,
        "stop_after": _MAX_CONSECUTIVE_WORKER_DISPATCH_FAILURES,
        "detail": detail,
    }
    if state is None:
        await self._emit_coord_bb("worker_dispatch_failed", **fields)
    else:
        await emit_scheduler_bb(self, state, "worker_dispatch_failed", **fields)
    if streak < _MAX_CONSECUTIVE_WORKER_DISPATCH_FAILURES:
        return False
    if state is not None:
        state.runtime_terminal_failure = True
    self._worker_dispatch_failure_limit_reached = True
    self._runtime_failure_code = "consecutive_worker_dispatch_failures"
    self._runtime_failure_phase = "worker_start"
    self._runtime_failure_detail = (
        f"{streak} consecutive Worker dispatches failed before process start; "
        f"last failure: {detail}"
    )
    for other, active_solver in list(active.items()):
        if not other.done():
            self._cancel_solver(active_solver)
            other.cancel()
    limit_fields = {
        "consecutive_failures": streak,
        "stop_after": _MAX_CONSECUTIVE_WORKER_DISPATCH_FAILURES,
        "detail": detail,
    }
    if state is None:
        await self._emit_coord_bb("worker_dispatch_failure_limit", **limit_fields)
    else:
        await emit_scheduler_bb(
            self, state, "worker_dispatch_failure_limit", **limit_fields,
        )
    return True


def _mark_ctf_batch_reaped(
    state, *, batch_id: int, is_review_task: bool, is_verifier_task: bool,
) -> None:
    if (
        is_review_task
        or is_verifier_task
    ):
        return
    if batch_id > 0 and batch_id == state.ctf_batch_id:
        state.ctf_batch_reaped += 1


async def _retire_finished_worker(self, state, t):
    import asyncio
    from functools import partial
    from muteki.swarm.coordinator_state import emit_scheduler_bb
    from muteki.swarm.swarm_support import ControlShutdownIncomplete

    emit_bb = partial(emit_scheduler_bb, self, state)
    is_review_task = t in self._active_review_tasks
    is_verifier_task = t in self._active_verifier_tasks
    engine = state.tasks.pop(t)
    scheduled_intent_id = state.task_intents.pop(t, None)
    solver = state.task_solvers.pop(t, None)
    # Retire the Step currently owned by the Worker.
    intent_id = (
        str(getattr(solver, "_intent_id", "") or "")
        or scheduled_intent_id
    )
    ctf_batch_id = int(getattr(solver, "_ctf_batch_id", 0) or 0)
    state.task_started_at.pop(t, None)
    state.task_prog_ckpt.pop(t, None)
    state.task_tool_count.pop(t, None)
    state.task_last_tool_t.pop(t, None)
    was_fruitless_interrupt = t in state.fruitless_interrupt_tasks
    state.fruitless_interrupt_tasks.discard(t)
    was_stall_reclaim = bool(getattr(solver, "_stall_reclaim", False))
    lane_key = state.task_lanes.get(t, "")
    # Round-6: interrupt victims get a longer retire settle; a
    # late exit proof must not abort the swarm before
    # worker_finished / Reason / rebootstrap can run.
    # Stage-4b: the fruitless-interrupt settle knob is an experimental
    # hook; default (no hook) is the historical module-absent 20.0s.
    retire_timeout: float | None = None
    if was_fruitless_interrupt:
        retire_timeout = 20.0
    retired_ok = await self._retire_worker_account(
        solver, intent_id=str(
            intent_id
            or getattr(solver, "intent_id_assigned", "")
            or getattr(solver, "_intent_id", "") or ""),
        reason="worker wrapper finished",
        lane_key=lane_key,
        timeout=retire_timeout,
    )
    sid = getattr(solver, "solver_id", None) or f"cli-{engine}"
    if was_stall_reclaim:
        self._reopen_stalled_intent(
            solver,
            intent_id=str(
                intent_id
                or getattr(solver, "intent_id_assigned", "")
                or getattr(solver, "_intent_id", "") or ""),
        )
    retire_deferred = False
    if not retired_ok:
        supervised_retirement = (
            self._worker_retirement_is_supervised(solver)
        )
        soft_continue = False
        if was_fruitless_interrupt:
            # Stage-4b: the fruitless-interrupt soft-continue consult is an
            # experimental hook; default (no hook) preserves the historical
            # module-absent behavior (True).
            soft_continue = True
        if soft_continue and supervised_retirement:
            await emit_bb(
                "fruitless_interrupt_retire_deferred",
                worker=sid,
                settle_s=retire_timeout,
                detail=(
                    "runtime exit proof deferred to reaper; "
                    "continuing replan"
                ),
            )
            # Surface finish + force Reason even without
            # synchronous retirement. Do NOT cancel siblings
            # or raise ControlShutdownIncomplete here.
            try:
                t.result()
            except asyncio.CancelledError:
                pass
            except Exception:
                pass
            await emit_bb(
                "worker_finished",
                worker=sid,
                result="fruitless_interrupt",
                retire_deferred=True,
            )
            if intent_id and self.shared_graph is not None:
                try:
                    self.shared_graph.conclude_intent(
                        actor="coordinator",
                        intent_id=str(intent_id),
                        result="explored",
                        result_detail=(
                            "fruitless_interrupt: retire "
                            "deferred; replan continues"
                        ),
                    )
                    await emit_bb(
                        "intent_concluded",
                        intent_id=str(intent_id),
                        result="explored",
                        reason="fruitless_interrupt",
                    )
                except Exception:
                    pass
            state.task_lanes.pop(t, None)
            if t in self._active_verifier_tasks:
                self._active_verifier_tasks.discard(t)
            if not is_review_task and not is_verifier_task:
                state.completed_for_review_n += 1
            _mark_ctf_batch_reaped(
                state,
                batch_id=ctf_batch_id,
                is_review_task=is_review_task,
                is_verifier_task=is_verifier_task,
            )
            state.reaped_n += 1
            state.force_reason_after_fruitless_interrupt = True
            return None
        if supervised_retirement:
            retire_deferred = True
            await emit_bb(
                "worker_retire_deferred",
                worker=sid,
                settle_s=retire_timeout,
                detail=(
                    "runtime exit proof remains owned by the "
                    "autonomous reaper"
                ),
            )
        else:
            # No live reaper owns this runtime.  This remains a
            # fail-closed control failure: stop siblings before an
            # untracked process can overlap later work.
            for other, other_solver in list(state.task_solvers.items()):
                if not other.done():
                    self._cancel_solver(other_solver)
                    other.cancel()
            raise ControlShutdownIncomplete(
                "worker wrapper exited without a supervised "
                "runtime retirement owner")
    # key per-solver outcomes by the worker's UNIQUE solver_id (e.g.
    # cli-claude-2), not the bare engine — otherwise two same-engine
    # workers (race + a later explore) clobber each other's record.
    lane_key = state.task_lanes.pop(t, "")
    if (lane_key and self.shared_graph is not None
            and not retire_deferred):
        try:
            rel = getattr(
                solver, "_muteki_lane_release_result", None)
            if not isinstance(rel, dict):
                rel = self.shared_graph.release_lane(  # type: ignore[attr-defined]
                    actor="coordinator", lane_key=lane_key,
                    by_worker=sid)
            await self._consume_lane_release(rel, emit_bb=emit_bb)
        except Exception:
            pass
    if (
        getattr(self.challenge, "mode", "ctf") != "ctf"
        and self.shared_graph is not None
        and not retire_deferred
    ):
        # A proven-dead Worker cannot keep owning a shared target resource. The
        # old lease-only cleanup left successors blocked for several minutes
        # after the process had exited. Release only rows fenced to this exact
        # solver id; deferred/uncertain retirement deliberately keeps ownership.
        try:
            resource_locks = list(
                self.shared_graph.active_resource_locks() or [])
        except Exception:
            resource_locks = []
        for lock in resource_locks:
            if str(lock.get("owner_worker") or "") != sid:
                continue
            try:
                self.shared_graph.release_resource_lock(
                    actor="coordinator",
                    lock_id=str(lock.get("lock_id") or ""),
                    by_worker=sid,
                )
            except Exception:
                pass
    return {
        "t": t,
        "is_review_task": is_review_task,
        "is_verifier_task": is_verifier_task,
        "engine": engine,
        "intent_id": intent_id,
        "solver": solver,
        "was_fruitless_interrupt": was_fruitless_interrupt,
        "retire_deferred": retire_deferred,
        "sid": sid,
        "lane_key": lane_key,
        "ctf_batch_id": ctf_batch_id,
    }


async def _fold_finished_worker(self, state, retired) -> str:
    import asyncio
    from functools import partial
    from muteki.solver.cli_results import WorkerRuntimeUnavailable
    from muteki.solver.result_codes import RESULT_TIMED_OUT
    from muteki.solver.types import SolveOutcome
    from muteki.swarm.coordinator_state import emit_scheduler_bb
    from muteki.swarm.swarm_support import (
        ControlShutdownIncomplete,
        _is_control_failure,
    )

    emit_bb = partial(emit_scheduler_bb, self, state)
    t = retired["t"]
    is_review_task = retired["is_review_task"]
    is_verifier_task = retired["is_verifier_task"]
    engine = retired["engine"]
    intent_id = retired["intent_id"]
    solver = retired["solver"]
    was_fruitless_interrupt = retired["was_fruitless_interrupt"]
    retire_deferred = retired["retire_deferred"]
    sid = retired["sid"]
    lane_key = retired.get("lane_key", "")
    ctf_batch_id = int(retired.get("ctf_batch_id", 0) or 0)
    try:
        outcome = t.result()
    except asyncio.CancelledError:
        # Ordinary cancels (budget/stop/winner) stay silent. A
        # mid-flight fruitless interrupt must surface as a finished
        # worker so barren accounting + Reason replan can fire.
        if was_fruitless_interrupt:
            await emit_bb(
                "worker_finished",
                worker=sid,
                result="fruitless_interrupt",
            )
            if (intent_id and self.shared_graph is not None
                    # Skip when the worker's own run() cleanup already
                    # committed its structured result for this intent.
                    and not getattr(solver, "_worker_result_committed", False)):
                try:
                    self.shared_graph.conclude_intent(
                        actor="coordinator",
                        intent_id=str(intent_id),
                        result="explored",
                        result_detail=(
                            "fruitless_interrupt: no new verified "
                            "fact/flag before mid-flight threshold"
                        ),
                    )
                    await emit_bb(
                        "intent_concluded",
                        intent_id=str(intent_id),
                        result="explored",
                        reason="fruitless_interrupt",
                    )
                except Exception:
                    pass
            if not is_review_task and not is_verifier_task:
                state.completed_for_review_n += 1
            if t in self._active_verifier_tasks:
                self._active_verifier_tasks.discard(t)
            _mark_ctf_batch_reaped(
                state,
                batch_id=ctf_batch_id,
                is_review_task=is_review_task,
                is_verifier_task=is_verifier_task,
            )
            state.reaped_n += 1
            state.force_reason_after_fruitless_interrupt = True
        return "proceed"
    except Exception as e:
        await _observe_started_workers(self, state, (solver,))
        state.per_solver[sid] = SolveOutcome(
            False, None, 0, None, f"error: {e}")
        runtime_unavailable = isinstance(e, WorkerRuntimeUnavailable)
        launch_code = launch_failure_code(e)
        terminal_code = str(getattr(e, "code", "") or "")
        terminal_state_failure = terminal_code in {
            "event_persistence_failed",
            "tool_artifact_persistence_failed",
            "blackboard_receipt_persistence_failed",
            "required_poc_unavailable",
        }
        if (launch_code or terminal_state_failure) and not runtime_unavailable:
            runtime_unavailable = True
            e = WorkerRuntimeUnavailable(
                str(e), code=terminal_code if terminal_state_failure else launch_code
            )
        policy_refusal = (
            not runtime_unavailable
            and str(e).startswith("policy_refusal:")
        )
        if policy_refusal and not is_review_task and not is_verifier_task:
            role = "explore"
            refusal_key = (role, str(engine))
            refusals = state.engine_role_refusals.get(refusal_key, 0) + 1
            state.engine_role_refusals[refusal_key] = refusals
            if refusals >= 2:
                quarantined = state.role_quarantined_engines.setdefault(
                    role, set())
                first_quarantine = str(engine) not in quarantined
                quarantined.add(str(engine))
                if first_quarantine:
                    await emit_bb(
                        "worker_profile_role_quarantined",
                        engine=str(engine),
                        role=role,
                        refusal_count=refusals,
                        reason="repeated zero-action policy refusal",
                    )
        # Crash salvage: the worker died before its end-of-life commit. Land
        # whatever it accumulated as one handoff_missing commit (the solver's
        # own run() cleanup usually beats us here — the committed flag makes
        # this a no-op then). Never raise from this path.
        if solver is not None and not getattr(
                solver, "_worker_result_committed", True):
            try:
                await solver._commit_worker_result(
                    status="error", result_detail=str(e),
                    handoff_missing=True)
            except asyncio.CancelledError:
                pass
            except Exception:
                pass
        elif solver is None and self.shared_graph is not None:
            try:
                self.shared_graph.commit_worker_result(
                    actor="coordinator", worker_id=sid,
                    intent_id=str(intent_id or "") or None,
                    status="handoff_missing", handoff_missing=True,
                    result_detail=str(e))
            except Exception:
                pass
        if bool(getattr(solver, "_remote_start_uncertain", False)):
            # StartWorker crossed the reverse link but no worker id /
            # started ACK came back. The remote process may still own
            # workspace state, so quarantine the whole run container;
            # do not dispatch siblings until absence is proven.
            self._mark_shutdown_incomplete(
                "remote_start_uncertain")
            for other in state.tasks:
                if other is not t and not other.done():
                    self._cancel_solver(state.task_solvers.get(other))
                    other.cancel()
            raise ControlShutdownIncomplete(
                "remote worker start outcome is uncertain")
        dispatch_failure_limit = False
        if not bool(getattr(solver, "_runtime_process_started", True)):
            dispatch_failure_limit = await _record_worker_dispatch_failure(
                self, state, worker=sid, engine=str(engine), detail=str(e),
                solver=solver,
            )
        if runtime_unavailable:
            # A missing provider/model catalog is a deterministic runtime setup
            # failure. Reopening the semantic Intent cannot change it; with one
            # configured profile that produced an unbounded spawn/fail/reopen loop.
            state.runtime_terminal_failure = True
            self._runtime_failure_detail = (
                f"Worker execution failed ({e.code}): {e}"
            )
            self._runtime_failure_code = str(e.code)
            self._runtime_failure_phase = (
                "worker_execution" if terminal_state_failure else "worker_start"
            )
            for other, other_solver in list(state.task_solvers.items()):
                if not other.done():
                    self._cancel_solver(other_solver)
                    other.cancel()
            await emit_bb(
                "worker_runtime_unavailable",
                worker=sid,
                engine=str(engine),
                code=str(e.code),
                detail=str(e),
            )
            await emit_bb(
                "worker_finished", worker=sid, result="runtime_unavailable",
                retire_deferred=retire_deferred,
            )
            if t in self._active_review_tasks:
                self._active_review_tasks.discard(t)
            if t in self._active_verifier_tasks:
                self._active_verifier_tasks.discard(t)
            if not is_review_task and not is_verifier_task:
                state.completed_for_review_n += 1
            _mark_ctf_batch_reaped(
                state,
                batch_id=ctf_batch_id,
                is_review_task=is_review_task,
                is_verifier_task=is_verifier_task,
            )
            state.reaped_n += 1
            return "break"
        if dispatch_failure_limit:
            await emit_bb(
                "worker_finished", worker=sid,
                result="dispatch_failure_limit",
                retire_deferred=retire_deferred,
            )
            self._active_review_tasks.discard(t)
            self._active_verifier_tasks.discard(t)
            if not is_review_task and not is_verifier_task:
                state.completed_for_review_n += 1
            _mark_ctf_batch_reaped(
                state, batch_id=ctf_batch_id,
                is_review_task=is_review_task,
                is_verifier_task=is_verifier_task,
            )
            state.reaped_n += 1
            return "break"
        # A control-plane failure (the in-container supervisor died /
        # the reverse link dropped mid-worker) is NOT an ordinary
        # worker crash — surface it as runtime_degraded so the operator
        # sees the runtime broke (roadmap 972 / §8). We never silently
        # switch to local: the worker just failed, container-backed.
        if _is_control_failure(e):
            self._record_runtime_degraded(
                engine=engine, profile=None,
                reason=f"runtime supervisor/link failed mid-worker: {e}",
                requested_backend="container",
                fallback_backend="none")
        if (getattr(self.challenge, "mode", "ctf") != "ctf"
                and intent_id and not is_review_task and not is_verifier_task
                and self.shared_graph is not None):
            iid = str(intent_id)
            state.intent_failed_engines.setdefault(iid, set()).add(
                str(engine)
            )
            try:
                reopened = bool(self.shared_graph.reopen_intent(
                    actor="coordinator",
                    intent_id=iid,
                    reason=(
                        f"retry after worker execution failure on {engine}: "
                        f"{type(e).__name__}: {e}"
                    )[:500],
                ))
            except Exception:
                reopened = False
            if not reopened:
                try:
                    reopened = bool(self.shared_graph.release_intent_claim(
                        worker=sid,
                        intent_id=iid,
                        reason=(
                            f"retry after worker execution failure on {engine}"
                        ),
                    ))
                except Exception:
                    reopened = False
            if reopened:
                await emit_bb(
                    "intent_reopened",
                    intent_id=iid,
                    previous_result="handoff_missing",
                    failed_engine=str(engine),
                    retry_engine_selection="scheduler",
                )
        await emit_bb(
            "worker_finished", worker=sid, result="error",
            retire_deferred=retire_deferred,
        )
        if t in self._active_review_tasks:
            self._active_review_tasks.discard(t)
            await emit_bb("review_finished", worker=sid,
                           result="error")
        if t in self._active_verifier_tasks:
            self._active_verifier_tasks.discard(t)
        if not is_review_task and not is_verifier_task:
            state.completed_for_review_n += 1
        _mark_ctf_batch_reaped(
            state,
            batch_id=ctf_batch_id,
            is_review_task=is_review_task,
            is_verifier_task=is_verifier_task,
        )
        state.reaped_n += 1
        return "proceed"
    _mark_ctf_batch_reaped(
        state,
        batch_id=ctf_batch_id,
        is_review_task=is_review_task,
        is_verifier_task=is_verifier_task,
    )
    state.reaped_n += 1
    if not is_review_task and not is_verifier_task:
        state.completed_for_review_n += 1
        if intent_id:
            iid = str(intent_id)
            result_status = str(
                getattr(getattr(outcome, "worker_result", None), "status", "")
                or ""
            )
            has_step_contract = bool(
                str(getattr(solver, "expected_observable", "") or "").strip()
                and str(getattr(solver, "stop_condition", "") or "").strip()
            )
            retry_incomplete_timeout = bool(
                getattr(self.challenge, "mode", "ctf") != "ctf"
                and result_status == RESULT_TIMED_OUT
                and (lane_key or has_step_contract)
                and self.shared_graph is not None
            )
            if retry_incomplete_timeout:
                state.intent_failed_engines.setdefault(iid, set()).add(
                    str(engine)
                )
                try:
                    reopened = bool(self.shared_graph.reopen_intent(
                        actor="coordinator",
                        intent_id=iid,
                        reason=(
                            "retry incomplete contracted step after lease "
                            f"timeout on {engine}"
                        ),
                    ))
                except Exception:
                    reopened = False
                if reopened:
                    try:
                        checkpoint_wm = int(
                            self.shared_graph.semantic_graph_watermark() or 0
                        )
                    except Exception:
                        checkpoint_wm = 0
                    state.checkpoint_replan_wm[iid] = checkpoint_wm
                    state.reason_next_trigger = "worker_checkpoint_timeout"
                    state.decide_followup_pending = True
                    await emit_bb(
                        "intent_reopened",
                        intent_id=iid,
                        previous_result=RESULT_TIMED_OUT,
                        failed_engine=str(engine),
                        retry_with_different_engine=True,
                        dispatch_deferred_until_reason=True,
                        checkpoint_watermark=checkpoint_wm,
                    )
            else:
                state.intent_failed_engines.pop(iid, None)
    state.per_solver[outcome_id := sid] = outcome
    await emit_bb(
        "worker_finished", worker=outcome_id,
        result="solved" if outcome.solved else "done",
        retire_deferred=retire_deferred,
    )
    if t in self._active_review_tasks:
        self._active_review_tasks.discard(t)
        await emit_bb("review_finished", worker=outcome_id,
                       result="done")
    if t in self._active_verifier_tasks:
        self._active_verifier_tasks.discard(t)
    # multi-flag: tally every flag this worker produced. The run is
    # done only once we hold expected_flags — until then a flag is
    # NOT a stop signal; the loop keeps spawning/exploring to find
    # the rest (re-bootstrap naturally continues; the new workers'
    # prompts carry the already-found list via _record_flags →
    # standing injection below).
    self._record_flags(*(outcome.flags or
                         ([outcome.flag] if outcome.flag else [])))
    pentest_product = getattr(self.challenge, "mode", "ctf") == "pentest"
    if pentest_product:
        self._sync_findings_from_graph()
        if self._findings_complete():
            state.goal_complete = True
            await emit_bb(
                "goal_complete",
                why="host_verified_objective",
                reports=0,
                findings=self._qualified_report_count())
            for other in state.tasks:
                self._cancel_solver(state.task_solvers.get(other))
                other.cancel()
            return "break"
    if self._flags_complete() and state.winner is None and not pentest_product:
        state.winner, state.flag = outcome_id, self._found_flags[0]
        try:
            await self.insight.all_flags_found(
                "coordinator", count=len(self._found_flags))
        except Exception:
            pass
        # kill the losing workers' subprocesses, not just their tasks
        for other in state.tasks:
            self._cancel_solver(state.task_solvers.get(other))
            other.cancel()
    return "proceed"


async def reap_workers_stage(self, state) -> None:
    await _observe_started_workers(self, state)
    for task in state.done:
        retired = await _retire_finished_worker(self, state, task)
        if retired is None:
            continue
        action = await _fold_finished_worker(self, state, retired)
        if action == "break":
            break
