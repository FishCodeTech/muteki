"""Coordinator finalize and control shutdown. Moved from coordinator_flags.py."""

from __future__ import annotations

import asyncio
import shutil
import subprocess
from typing import TYPE_CHECKING, Any, Optional

if TYPE_CHECKING:
    pass

from muteki.solver.types import SolveOutcome
from muteki.solver.workspace import cleanup_worker_scratch
from muteki.swarm.swarm_support import (
    ControlShutdownIncomplete,
    SwarmOutcome,
)

async def _finalize_coordinator_run(
    self, *, winner: "Optional[str]", flag: "Optional[str]",
    goal_complete: bool, per_solver: "dict[str, SolveOutcome]",
    terminal_reason: str = "") -> None:
    """M11: persist the winner, close the shared graph (release the SQLite WAL/-shm
    handles), and emit the single run-level RUN_FINISHED (which also sweeps
    non-winner worker scratch dirs). Idempotent via _run_finalized — safe to call
    from BOTH the normal-return path and the coordinator's finally, so a cancelled
    / errored run still frees its DB handle and cleans scratch instead of leaking
    them (the cleanup used to sit AFTER the finally, on the normal path only)."""
    if self._run_finalized:
        return
    if self.worker_backend == "container" or self._container_handle is not None:
        from muteki.solver.container_exec import teardown_container
        try:
            removed = await asyncio.to_thread(
                teardown_container, self.run_id, remove=True,
                container_scope=self.worker_container_scope,
                bootstrap_root=(str(self.container_bootstrap_root)
                                if self.container_bootstrap_root is not None else None))
        except Exception:
            removed = False
        if removed is not True:
            self._mark_shutdown_incomplete("container_absence")
            self._retain_control_shutdown_owner(
                winner=winner, flag=flag, goal_complete=goal_complete,
                per_solver=per_solver)
            raise ControlShutdownIncomplete("container teardown could not be proven")
    self._run_finalized = True
    # L3: detach the coordinator's bus sinks so a reused bus doesn't keep them.
    if self.bus is not None and self._coord_sinks:
        for sink in self._coord_sinks:
            try:
                self.bus.remove_sink(sink)
            except Exception:
                pass
        self._coord_sinks = []
    if winner is not None:
        self._persist_winner(
            per_solver.get(winner), flag, worker_id=str(winner or ""))
    tmux_socket = getattr(self, "_ctf_tmux_socket", None)
    if tmux_socket is not None:
        tmux_bin = shutil.which("tmux")
        try:
            if tmux_bin:
                await asyncio.to_thread(
                    subprocess.run,
                    [tmux_bin, "-S", str(tmux_socket), "kill-server"],
                    capture_output=True,
                    timeout=5,
                )
        except (OSError, subprocess.SubprocessError):
            pass
        tmux_socket.unlink(missing_ok=True)
        self._ctf_tmux_socket = None
    pentest_product = getattr(self.challenge, "mode", "ctf") == "pentest"
    if pentest_product:
        # A final Worker may publish the decisive Fact exactly as worker
        # admission closes. Give Decide one evidence-only pass before closing
        # the graph; no further Worker is admitted or Step dispatched.
        if self.shared_graph is not None and not self._findings_complete():
            from muteki.pentest.judgement import evaluate
            contract = getattr(self.challenge, "pentest_contract", None)
            have_evidence = bool(
                contract and evaluate(self.shared_graph.events(), contract)["findings"]
            )
            if have_evidence and (self._worker_admission_closed or self._budget_exhausted_kind):
                await self._run_reason(max_intents=0)
                rr = getattr(self, "_last_reason", None)
                if rr is not None and getattr(rr, "verdict", "") == "complete":
                    committed = self.shared_graph.record_pentest_goal_completion(
                        fact_seqs=list(getattr(rr, "goal_evidence_facts", None) or []),
                        reason=str(getattr(rr, "complete_why", "") or ""),
                    )
                    if committed > 0 and not self._operator_stop:
                        terminal_reason = "goal_met"
        solved = bool(goal_complete) or self._findings_complete()
        if (not solved and self.shared_graph is not None
                and not self._operator_stop and not self._budget_exhausted_kind):
            from muteki.pentest.judgement import submitted_reports
            contract = getattr(self.challenge, "pentest_contract", None)
            if contract is not None and contract.version >= 2:
                blocked = [
                    item for item in submitted_reports(self.shared_graph.events(), contract)
                    if item.get("review_status") == "pending"
                    and (item.get("review_error") or {}).get("blocked") is True
                ]
                if blocked:
                    self._runtime_failure_code = "pentest_review_unavailable"
                    self._runtime_failure_phase = "pentest_review"
                    self._runtime_failure_detail = (
                        "Coordinator review could not complete for submitted reports: "
                        + ", ".join(item["id"] for item in blocked)
                    )
    else:
        solved = winner is not None or goal_complete or self._flags_complete()
    reason = (terminal_reason or "").strip()
    if not reason:
        if solved:
            if pentest_product:
                reason = "goal_met"
            else:
                reason = "solved" if winner is not None or self._flags_complete() else "goal_met"
        elif self._operator_stop:
            reason = "operator_stop"
        elif getattr(self, "_coverage_exhausted", False):
            reason = "coverage_complete"
        elif self._budget_exhausted_kind:
            reason = "budget_exhausted"
        elif not str(getattr(self, "_runtime_failure_detail", "") or "").strip():
            reason = "no_progress"
        else:
            reason = "runtime_failure"
    if pentest_product and self.shared_graph is not None:
        from muteki.pentest.judgement import generate_report
        contract = getattr(self.challenge, "pentest_contract", None)
        try:
            if contract is None or self.llm is None:
                raise RuntimeError("pentest report model is unavailable")
            generated = await asyncio.wait_for(
                generate_report(
                    self.llm, self.reason_model,
                    self.shared_graph.events(), contract,
                    terminal_reason=reason,
                    run_id=self.run_id,
                    challenge_id=self.challenge.id,
                ),
                timeout=120.0,
            )
            self.shared_graph.record_pentest_report(
                payload=generated,
            )
        except Exception as exc:
            self.shared_graph.record_pentest_report(
                payload={
                    "code": "report_generation_failed",
                    "error_type": type(exc).__name__,
                    "detail": str(exc),
                    "raw_response": str(getattr(exc, "raw_response", "") or ""),
                },
                error=True,
            )
    if self.shared_graph is not None:
        try:
            snap = self.shared_graph.snapshot()
            self._record_flags(*getattr(snap, "flags", []))
            self._record_findings(*list(getattr(snap, "findings", []) or []))
        except Exception:
            pass
        finalize_reason = (
            reason if reason in {
                "solved", "goal_met", "operator_stop", "budget_exhausted",
                "runtime_failure", "coverage_complete", "no_progress",
            }
            else ("solved" if solved else "runtime_failure"))
        try:
            # Runtime resources outlive the Worker that created them, but never
            # the Run.  Close paths/capabilities before the final graph drain so
            # observers see the same terminal state as the process supervisor.
            if getattr(self.challenge, "mode", "ctf") != "ctf":
                await asyncio.to_thread(
                    self.shared_graph.stop_runtime_resources,
                    actor="coordinator",
                )
            fin = self.shared_graph.release_claims_for_finalize(  # type: ignore[attr-defined]
                reason=finalize_reason)
            # 刀2: mirror the resume/closed transition onto the bus BEFORE close()
            # so the deck doesn't keep rendering these intents as live work.
            await self._emit_finalize_lifecycle_deltas(fin, finalize_reason)
            await self._drain_graph_to_bus(emit_bb=self._emit_bb_bus)
        except Exception:
            pass
        try:
            self.shared_graph.close()
        except Exception:
            pass
    if not pentest_product:
        solved = winner is not None or goal_complete or self._flags_complete()
    if solved and (not terminal_reason or reason == "runtime_failure"):
        if pentest_product:
            reason = "goal_met"
        else:
            reason = "solved" if winner is not None or self._flags_complete() else "goal_met"
    if pentest_product:
        # The graph closes before coordinator_outcome returns. Keep the same
        # terminal decision for the caller as the run.finished event.
        self._pentest_final_outcome = (solved, reason)
    finish_flag = self._found_flags[0] if self._found_flags else (
        flag if winner is not None else None)
    await self._emit_run_finished(flag=finish_flag, solved=solved,
                                  reason=reason)


def _retain_control_shutdown_owner(
    self, *, winner: "Optional[str]", flag: "Optional[str]",
    goal_complete: bool, per_solver: "dict[str, SolveOutcome]",
) -> None:
    """Persist finalization inputs while a fenced control orphan still owns state."""
    self._deferred_control_finalization = {
        "winner": winner,
        "flag": flag,
        "goal_complete": bool(goal_complete),
        "per_solver": dict(per_solver),
    }


async def settle_control_shutdown(self) -> None:
    """Wait for retained control owners, then perform the previously-forbidden teardown.

    This is intentionally separate from ``run()``: Web owns it as a durable
    cleanup task so the request loop remains available while a hostile callback
    takes an arbitrary amount of time to leave.
    """
    while True:
        context_pending = False
        for _key, owner in list(getattr(
                self, "_context_cleanup_owners", {}).items()):
            reservations, worker_id = owner
            if not self._release_typed_context_reservations(
                    list(reservations), str(worker_id)):
                context_pending = True
        control_owned = tuple(
            task for task in getattr(self, "_control_orphan_tasks", set())
            if not task.done())
        worker_owned: list[asyncio.Task[Any]] = []
        for _sid, owner in list(getattr(
                self, "_worker_runtime_owners", {}).items()):
            solver, intent_id, reason, lane_key = owner
            if self._worker_runtime_exit_confirmed(solver):
                if self._finish_worker_retirement(
                        solver, intent_id=intent_id, reason=reason,
                        lane_key=lane_key):
                    continue
            # A done/cancelled/failed task is not exit proof. Rebuild it from
            # the retained solver owner and wait for the real runtime fence.
            worker_owned.append(self._ensure_worker_runtime_reaper(
                solver, intent_id=intent_id, reason=reason,
                lane_key=lane_key))
        owned = (*control_owned, *worker_owned)
        if not owned and not context_pending:
            break
        if not owned:
            await asyncio.sleep(0.05)
            continue
        await asyncio.gather(
            *(asyncio.shield(task) for task in owned),
            return_exceptions=True)
    if getattr(self, "_worker_runtime_owners", {}):
        self._worker_runtime_incomplete = True
        self._mark_shutdown_incomplete("worker_runtime")
        raise ControlShutdownIncomplete(
            "worker runtime exit could not be proven")
    if getattr(self, "_context_cleanup_owners", {}):
        self._context_cleanup_incomplete = True
        self._mark_shutdown_incomplete("context_cleanup")
        raise ControlShutdownIncomplete(
            "context reservation release could not be proven")
    deferred = dict(getattr(
        self, "_deferred_control_finalization", {}) or {})
    # Container absence is part of the runtime exit proof, not post-finalize
    # housekeeping. Prove it before closing the graph or emitting RUN_FINISHED.
    if self.worker_backend == "container" or self._container_handle is not None:
        from muteki.solver.container_exec import teardown_container
        removed = await asyncio.to_thread(
            teardown_container, self.run_id, remove=True,
            container_scope=self.worker_container_scope,
            bootstrap_root=(str(self.container_bootstrap_root)
                            if self.container_bootstrap_root is not None else None))
        if removed is not True:
            self._mark_shutdown_incomplete("container_absence")
            raise ControlShutdownIncomplete(
                "container teardown could not be proven")
    self._shutdown_incomplete_causes.clear()
    self._control_shutdown_incomplete = False
    self._worker_runtime_incomplete = False
    self._context_cleanup_incomplete = False
    await self._finalize_coordinator_run(
        winner=deferred.get("winner"), flag=deferred.get("flag"),
        goal_complete=bool(deferred.get("goal_complete", False)),
        per_solver=dict(deferred.get("per_solver") or {}),
        terminal_reason="runtime_failure",
    )
    self._deferred_control_finalization = None


def _cleanup_finished_worker_dirs(self) -> None:
    """Remove failed/finished worker scratch while preserving durable run data.

    The workspace root keeps shared/, inputs/, final/, manifest.json,
    and winner.json. Only non-winner worker cwd directories under workers/ are
    removed at run finish to avoid long coordinator runs accumulating hundreds
    of duplicate scratch trees.
    """
    if self.worker_root is None:
        return
    winner_workdir_name = str(
        getattr(self, "_winner_workdir_name", "") or ""
    ).strip()
    keep = [winner_workdir_name] if winner_workdir_name else []
    cleanup_worker_scratch(self.worker_root, keep=keep)


async def teardown_coordinator_stage(self, state) -> None:
    from functools import partial
    from muteki.swarm.coordinator_state import emit_scheduler_bb

    emit_bb = partial(emit_scheduler_bb, self, state)
    review_task = state.pentest_review_task
    if review_task is not None:
        if not review_task.done():
            review_task.cancel()
        await asyncio.gather(review_task, return_exceptions=True)
        state.pentest_review_task = None
    reason_task = state.reason_task
    if reason_task is not None:
        was_running = not reason_task.done()
        if was_running:
            reason_task.cancel()
        await asyncio.gather(reason_task, return_exceptions=True)
        state.reason_task = None
        self._reason_max_intents_override = None
        if was_running:
            try:
                await emit_bb(
                    "reason_cancelled",
                    trigger=state.reason_started_trigger,
                    watermark=state.reason_started_wm,
                    requested_intents=state.reason_started_max_intents,
                    reason="coordinator epoch ended",
                )
            except Exception:
                pass
    if "__help__" in self._freeze_suspensions:
        try:
            self._end_operator_help_freeze(
                reason="coordinator leaving operator help wait")
        except Exception:
            # Retain graph/runtime ownership instead of finalizing beneath
            # an un-restored help suspension.
            self._mark_shutdown_incomplete("help_suspension")
    # Dispatcher-only state cannot outlive its coordinator epoch. A wall
    # budget or other terminal edge may win the same scheduling turn as a
    # RESUME, so balance the projection here as the final authority. An
    # explicit process FREEZE is deliberately excluded: it remains true
    # until its OS/lease fence is thawed or terminal teardown proves the
    # process owner absent.
    if self._operator_paused and not self._control_frozen:
        self._operator_paused = False
        try:
            await emit_bb(
                "operator_resumed",
                reason="coordinator epoch ended; dispatcher latch retired",
            )
        except Exception:
            pass
    if self._operator_draining:
        self._operator_draining = False
        try:
            await emit_bb(
                "operator_drain_completed",
                reason="coordinator epoch ended after draining in-flight work",
            )
        except Exception:
            pass
    leftover = [t for t in state.tasks if not t.done()]
    for t in leftover:
        self._cancel_solver(state.task_solvers.get(t))
        t.cancel()
    if leftover:
        await asyncio.gather(*leftover, return_exceptions=True)
    # The normal reap path releases profile/account/registry ownership, but
    # cancel/error exits can jump straight here with live WorkerRefs still
    # published. Release every constructed solver idempotently so a later
    # resolve never targets stale runtime objects.
    released_ids: set[str] = set()
    for task, solver in state.task_solvers.items():
        sid = str(getattr(solver, "solver_id", "") or "")
        if not sid or sid in released_ids:
            continue
        await self._retire_worker_account(
            solver, intent_id=str(
                state.task_intents.get(task)
                or getattr(solver, "intent_id_assigned", "")
                or getattr(solver, "_intent_id", "") or ""),
            reason="coordinator shutdown",
            lane_key=str(state.task_lanes.get(task) or ""),
        )
        released_ids.add(sid)
    if state.task_lanes and self.shared_graph is not None:
        for t, lane_key in list(state.task_lanes.items()):
            solver = state.task_solvers.get(t)
            if solver is None:
                # Its wrapper was popped by the normal reap path, but a
                # retained runtime reaper still owns this lane. Never use an
                # empty by_worker selector, which would bypass owner fencing.
                continue
            sid = getattr(solver, "solver_id", "") or ""
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
            state.task_lanes.pop(t, None)
    if state.hitl_task is not None:
        state.hitl_task.cancel()
        await asyncio.gather(state.hitl_task, return_exceptions=True)
    if self._shutdown_owners_incomplete():
        # Do not close the graph or emit a false terminal lifecycle while a
        # fenced handler still owns an in-flight mutation. The orphan set on
        # the Swarm is the retained ownership record for diagnostics/reap.
        self._retain_control_shutdown_owner(
            winner=state.winner, flag=state.flag, goal_complete=state.goal_complete,
            per_solver=state.per_solver)
        raise ControlShutdownIncomplete(
            "control shutdown incomplete; runtime owner retained")
    # M11: if we are leaving via cancel/exception, finalize HERE so the shared
    # graph handle is closed and worker scratch is swept even on the error path
    # (the post-finally finalize below only runs on a clean return). Idempotent.
    try:
        await self._finalize_coordinator_run(
            winner=state.winner, flag=state.flag, goal_complete=state.goal_complete,
            per_solver=state.per_solver)
    except Exception:
        pass


async def coordinator_outcome(self, state) -> SwarmOutcome:
    await self._finalize_coordinator_run(
        winner=state.winner, flag=state.flag, goal_complete=state.goal_complete, per_solver=state.per_solver)
    if getattr(self.challenge, "mode", "ctf") == "pentest":
        solved, reason = self._pentest_final_outcome
        return SwarmOutcome(solved, None, None, state.per_solver, reason)
    if state.winner is not None:
        return SwarmOutcome(True, state.flag, state.winner, state.per_solver, "solved",
                            flags=list(self._found_flags))
    if state.goal_complete:
        return SwarmOutcome(True,
                            self._found_flags[0] if self._found_flags else None,
                            None, state.per_solver, "goal_met",
                            flags=list(self._found_flags))
    if self._coverage_exhausted:
        return SwarmOutcome(False, None, None, state.per_solver, "coverage_complete")
    if self._budget_exhausted_kind:
        return SwarmOutcome(False, None, None, state.per_solver,
                            "budget_exhausted")
    return SwarmOutcome(False, None, None, state.per_solver,
                        "coordinator: no verified flag")
