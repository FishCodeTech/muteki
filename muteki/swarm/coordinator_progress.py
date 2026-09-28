"""Coordinator budget, stall reopen, and winner persistence. Moved from coordinator_loop.py."""

from __future__ import annotations

from pathlib import Path
from typing import TYPE_CHECKING, Any, Optional

if TYPE_CHECKING:
    pass

from muteki.solver.types import SolveOutcome
from muteki.swarm.graph_defs import SEMANTIC_GRAPH_KINDS


_STALL_RECLAIM_S = 120.0


def _planning_input_pending(self, state) -> bool:
    """Whether Reason has not yet seen a material SharedGraph change."""
    graph = getattr(self, "shared_graph", None)
    if graph is None:
        return False
    try:
        watermark = int(graph.semantic_graph_watermark() or 0)
        after = int(state.last_decided_wm)
        if watermark <= after:
            return False
        from muteki.swarm.coordinator_reason import _is_external_planning_input

        return any(
            _is_external_planning_input(event)
            for event in graph.events_since(
                after, kinds=list(SEMANTIC_GRAPH_KINDS))
        )
    except Exception:
        return False


def _planner_owns_next_free_slot(self, state) -> bool:
    """Keep low-priority Review behind an active or not-yet-applied plan."""
    return bool(
        state.reason_task is not None
        or state.reason_result_ready
        or _planning_input_pending(self, state)
    )

def _budget_elapsed(self, started_at: float, *, now: Optional[float] = None) -> float:
    """Wall-clock charged to the run, excluding emergency freeze only.

    Soft pause/quiesce deliberately keeps consuming the configured offline
    budget; a true SIGSTOP freeze suspends it because neither workers nor lease
    owners can make progress.
    """
    import time
    current = time.monotonic() if now is None else float(now)
    suspended = float(getattr(self, "_budget_suspended_total", 0.0) or 0.0)
    freeze_started = getattr(self, "_budget_suspend_started", None)
    if freeze_started is not None:
        suspended += max(0.0, current - float(freeze_started))
    return max(0.0, current - float(started_at) - suspended)


def _reopen_stalled_intent(self, solver: Any, *, intent_id: str) -> None:
    iid = str(intent_id or "").strip()
    if not iid or self.shared_graph is None:
        return
    sid = str(getattr(solver, "solver_id", "") or "")
    try:
        state = {}
        reader = getattr(self.shared_graph, "intent_claim_state", None)
        if callable(reader):
            state = dict(reader(iid) or {})
        status = str(state.get("status") or "")
        if status == "done" and hasattr(self.shared_graph, "reopen_intent"):
            self.shared_graph.reopen_intent(
                actor="coordinator", intent_id=iid, reason="stall reclaim")
        elif status == "claimed" and sid and hasattr(
                self.shared_graph, "release_intent_claim"):
            self.shared_graph.release_intent_claim(
                worker=sid, intent_id=iid, reason="stall reclaim")
    except Exception:
        return


def config_poll_interval(self) -> float:
    """How long asyncio.wait blocks before re-checking stall/intents. Short
    enough to be responsive, long enough not to busy-spin."""
    return 2.0


def _active_worker_conclude_reserve(state) -> float:
    """Largest existing Worker checkpoint allowance in the active set."""
    return max(
        (
            float(getattr(solver, "conclude_timeout", 0.0) or 0.0)
            for task, solver in state.task_solvers.items()
            if task in state.tasks and not task.done()
        ),
        default=0.0,
    )


def _request_active_worker_conclusion(state) -> int:
    """End active turns so each Worker enters its existing checkpoint path."""
    requested = 0
    target_worker = str(
        getattr(state, "termination_conclude_worker", "") or ""
    )
    for task, solver in state.task_solvers.items():
        if task not in state.tasks or task.done():
            continue
        if (
            target_worker
            and str(getattr(solver, "solver_id", "") or "") != target_worker
        ):
            continue
        if task in state.termination_conclude_requested:
            continue
        if not bool(getattr(solver, "_turn_active", False)):
            continue
        if bool(getattr(solver, "_session_handoff_active", False)):
            continue
        steer_event = getattr(solver, "_steer_event", None)
        if steer_event is None or not hasattr(steer_event, "set"):
            continue
        steer_event.set()
        state.termination_conclude_requested.add(task)
        requested += 1
    return requested


def _persist_winner(
    self, outcome: "Optional[SolveOutcome]", flag: "Optional[str]",
    *, worker_id: str = "",
) -> None:
    """Persist the winner's CLI continuation handle for human follow-ups.

    The Web driver installs a coordinator-only writer. ``winner.json`` remains
    a compatibility artifact in the Worker workspace and carries no profile,
    credential endpoint or backend authority. Best-effort: a write failure must
    never fail a solved run.

    Needs graph_dir (web runs) — winner.json lands beside graph/ (a sibling of
    the sandbox root, so sandbox.shutdown_all()'s rmtree can't delete it). TUI
    / test runs without graph_dir simply skip persistence (no standby there)."""
    if self._graph_dir is None or outcome is None:
        return
    session = getattr(outcome, "session", None)
    # only CLI workers carry a session; without one there's nothing to resume.
    if not session:
        return
    try:
        import json
        workdir = getattr(outcome, "workdir", "") or ""
        self._winner_workdir_name = Path(workdir).name if workdir else ""
        agent_state_dir = ""
        workspace_root = getattr(self, "workspace_root", None)
        if workspace_root is not None and worker_id:
            candidate = Path(workspace_root) / ".muteki-agent-state" / worker_id
            if candidate.is_dir():
                agent_state_dir = str(candidate.resolve())
        self._winner_agent_state_dir = agent_state_dir
        trusted_payload = {
            "engine": getattr(outcome, "engine", "") or "",
            "worker_id": str(worker_id or ""),
            "session": session,
            "workdir": workdir,
            "agent_state_dir": agent_state_dir,
            "flag": flag or outcome.flag or "",
            # multi-flag: every flag the run collected (the run's authoritative
            # set, not just this one worker's). `flag` stays the first.
            "flags": list(self._found_flags) or (
                [flag] if flag else (outcome.flags or [])),
            "challenge": self.challenge.model_dump(),
            "profile": dict(getattr(outcome, "runtime_profile", {}) or {}),
            **self._runtime_metadata_for(outcome),
        }
        writer = getattr(self, "_winner_continuation_writer", None)
        if callable(writer):
            writer(dict(trusted_payload))
        profile = trusted_payload.get("profile") or {}
        payload = {
            key: trusted_payload[key]
            for key in (
                "engine", "worker_id", "session", "workdir", "flag",
                "flags", "challenge",
            )
        }
        if isinstance(profile, dict):
            payload["profile_id"] = str(
                profile.get("id") or profile.get("name") or ""
            )
        dest = self._graph_dir.parent / "winner.json"
        dest.parent.mkdir(parents=True, exist_ok=True)
        dest.write_text(json.dumps(payload, ensure_ascii=False, indent=2))
    except Exception:
        pass


async def progress_guard_stage(self, state) -> str:
    import time
    from functools import partial
    from muteki.swarm.coordinator_state import (
        emit_scheduler_bb,
        stop_for_budget as scheduler_stop_for_budget,
    )

    emit_bb = partial(emit_scheduler_bb, self, state)
    stop_for_budget = partial(scheduler_stop_for_budget, self, state)
    ctf_mode = getattr(self.challenge, "mode", "ctf") == "ctf"
    # ── progress tracking (for the reason graph-change trigger) ──
    now = time.monotonic()
    fc = self._verified_fact_count()
    if fc > state.last_fact_count:
        state.last_fact_count = fc

    # ── barren backpressure accounting (ALL modes) ───────────────
    # Per finished worker: did the board grow at all since the last
    # completion? Candidates count (engagement ≠ fruitless); flags
    # count; dead-ends deliberately do NOT — the run-11190 spike
    # workers wrote dead-ends while burning 238 slots on a solved wall.
    if state.reaped_n and self.barren_limit > 0:
        # Only canonical/verified information clears stagnation.
        # Candidate text and activity are useful telemetry, but letting
        # them reset this cursor creates an infinite self-reward loop.
        information_count = self._verified_fact_count()
        grew = (information_count > state.prog_fact_ckpt
                or len(self._found_flags) > state.prog_flag_ckpt)
        state.fruitless_workers = (0 if grew
                             else state.fruitless_workers + state.reaped_n)
        if grew:
            state.reason_retry_pending = False
            state.reason_retry_count = 0
            state.reason_retry_not_before = 0.0
            state.reason_retry_failure = ""
            state.last_progress_t = time.monotonic()  # H: reset no-progress timer
        state.prog_fact_ckpt = max(state.prog_fact_ckpt, information_count)
        state.prog_flag_ckpt = max(state.prog_flag_ckpt, len(self._found_flags))

    if state.completed_for_review_n:
        self._completed_workers_since_review += state.completed_for_review_n

    elapsed = self._budget_elapsed(state.t0, now=now)
    finite_wall_budget = self.wall_clock_budget != float("inf")

    if self._operator_stop and not state.termination_conclude_reason:
        reserve = _active_worker_conclude_reserve(state)
        state.termination_conclude_reason = "operator_stop"
        state.termination_conclude_deadline = now + reserve
        self._operator_draining = True
        await emit_bb(
            "operator_conclude_window",
            active_workers=len(state.tasks),
            conclude_timeout=int(reserve),
        )

    if (
        not state.termination_conclude_reason
        and finite_wall_budget
        and state.tasks
    ):
        reserve = _active_worker_conclude_reserve(state)
        if reserve > 0 and elapsed >= max(0.0, self.wall_clock_budget - reserve):
            state.termination_conclude_reason = "wall_clock_budget"
            state.termination_conclude_deadline = now + max(
                0.0, self.wall_clock_budget - elapsed
            )
            self._budget_exhausted_kind = "wall_clock_budget_exhausted"
            await emit_bb(
                "budget_conclude_window",
                active_workers=len(state.tasks),
                elapsed=int(elapsed),
                conclude_timeout=int(reserve),
            )

    if state.termination_conclude_reason:
        goal_conclude = state.termination_conclude_reason == "goal_complete"
        hard_deadline_reached = bool(
            finite_wall_budget and elapsed >= self.wall_clock_budget
        )
        if state.termination_conclude_reason in {"operator_stop", "goal_complete"}:
            hard_deadline_reached = bool(
                hard_deadline_reached
                or now >= state.termination_conclude_deadline
            )
        target_active = any(
            str(getattr(solver, "solver_id", "") or "")
            == state.termination_conclude_worker
            for task, solver in state.task_solvers.items()
            if task in state.tasks and not task.done()
        )
        if (
            state.tasks
            and not hard_deadline_reached
            and (not goal_conclude or target_active)
        ):
            _request_active_worker_conclusion(state)
            return "continue"

        if goal_conclude:
            await emit_bb(
                "goal_conclude_timeout" if target_active else "goal_conclude_complete",
                worker=state.termination_conclude_worker,
                flags=len(self._found_flags),
            )
        elif self._operator_stop:
            await emit_bb("operator_stopped", flags=len(self._found_flags))
        else:
            await emit_bb("budget_exhausted", elapsed=int(elapsed))
        for other in state.tasks:
            self._cancel_solver(state.task_solvers.get(other))
            other.cancel()
        state.termination_conclude_reason = ""
        state.termination_conclude_worker = ""
        state.termination_conclude_deadline = 0.0
        state.termination_conclude_requested.clear()
        self._operator_draining = False
        return "break"

    if finite_wall_budget and elapsed >= self.wall_clock_budget:
        self._budget_exhausted_kind = "wall_clock_budget_exhausted"
        await emit_bb("budget_exhausted", elapsed=int(elapsed))
        for other in state.tasks:
            self._cancel_solver(state.task_solvers.get(other))
            other.cancel()
        return "break"

    budget_kind = self._budget_exhausted()
    if budget_kind:
        terminal = await stop_for_budget(budget_kind)
        return "break" if terminal else "continue"

    if self._operator_draining:
        if not state.tasks:
            await emit_bb("operator_drain_complete")
            return "break"
        # Loop back through asyncio.wait/reap only. No Reason, review,
        # dynamic command, or bootstrap path below may create work.
        return "continue"

    # Sanctioned deterministic triggers: a fact refuted by fresh verified
    # evidence, and near-duplicate verified facts piling up on one route.
    if not ctf_mode:
        await self._drain_evidence_conflicts()
        self._maybe_queue_semantic_duplicate_review()

        queued_trigger = str(
            (
                self._queued_review_requests[0]
                if self._queued_review_requests
                else {}
            ).get("trigger", "")
        )
        low_priority_review = queued_trigger in {
            "candidate_spike", "duplicate_intents", "fruitless_workers",
        }
        if not (
            low_priority_review and _planner_owns_next_free_slot(self, state)
        ):
            if await self._maybe_run_queued_review(
                healthy=state.healthy,
                tasks=state.tasks,
                task_solvers=state.task_solvers,
                emit_bb=emit_bb,
            ):
                return "continue"

        if await self._drain_review_proposals(emit_bb=emit_bb):
            return "continue"

        await self._drain_resource_locks(emit_bb=emit_bb)
    await self._drain_graph_to_bus(emit_bb=emit_bb)

    if (
        not ctf_mode
        and self.review_policy.get("on_candidate_spike", True)
        and not _planner_owns_next_free_slot(self, state)
    ):
        candidate_count = self._candidate_fact_count()
        threshold = int(self.review_policy.get("candidate_spike_threshold") or 0)
        threshold = max(1, threshold)
        candidate_delta = candidate_count - self._last_candidate_review_count
        if (candidate_delta >= threshold
                and await self._maybe_start_review(
                    trigger="candidate_spike",
                    directive=(
                        f"{candidate_delta} new unverified candidate facts accumulated "
                        "since the last review. Audit semantic duplicates, challenge weak "
                        "facts, merge duplicate facts, and record search gaps as "
                        "REVIEW_FINDING entries for Reason."
                    ),
                    healthy=state.healthy, tasks=state.tasks,
                    task_solvers=state.task_solvers, emit_bb=emit_bb)):
            return "continue"

    every_completed = int(self.review_policy.get("every_completed_workers") or 0)
    if (not ctf_mode
            and every_completed > 0
            and self._completed_workers_since_review >= every_completed
            and await self._maybe_start_review(
                trigger="every_completed_workers",
                directive=(
                    f"{self._completed_workers_since_review} ordinary workers completed "
                    "since the last review. Audit the current facts and record any repeated "
                    "work or missing search coverage as REVIEW_FINDING entries for Reason."
                ),
                healthy=state.healthy, tasks=state.tasks,
                task_solvers=state.task_solvers, emit_bb=emit_bb)):
        return "continue"
    return "proceed"


async def soft_pause_stage(self, state) -> str:
    import asyncio
    from functools import partial
    from muteki.solver.worker_profiles import worker_identity_event_fields
    from muteki.swarm.coordinator_state import (
        emit_scheduler_bb,
        running_engines as scheduler_running_engines,
        stop_for_budget as scheduler_stop_for_budget,
    )
    from muteki.swarm.swarm_support import (
        WorkerBudgetExhausted,
        WorkerSpawnRejected,
    )

    emit_bb = partial(emit_scheduler_bb, self, state)
    running_engines = partial(scheduler_running_engines, state)
    stop_for_budget = partial(scheduler_stop_for_budget, self, state)
    if self._operator_paused:
        self._operator_event.clear()
        event_wait = asyncio.create_task(
            self._operator_event.wait(), name="operator-pause-wait")
        timeout = None
        if (self.wall_clock_budget != float("inf")
                and not self._control_frozen):
            timeout = max(
                0.0,
                self.wall_clock_budget - self._budget_elapsed(state.t0),
            )
        state.done, _pending = await asyncio.wait(
            {event_wait, *state.tasks},
            timeout=timeout,
            return_when=asyncio.FIRST_COMPLETED,
        )
        if event_wait not in state.done:
            event_wait.cancel()
            await asyncio.gather(event_wait, return_exceptions=True)
        if not state.done:
            # L6: balance the paused-state bracket so the FE clears its
            # "awaiting operator / paused" banner on this exit too.
            self._budget_exhausted_kind = "wall_clock_budget_exhausted"
            self._operator_paused = False
            await emit_bb("operator_resumed")
            await emit_bb("budget_exhausted",
                           elapsed=int(self._budget_elapsed(state.t0)))
            for other in state.tasks:
                self._cancel_solver(state.task_solvers.get(other))
                other.cancel()
            return "break"
        if self._operator_stop:
            # L6: balance the paused-state bracket (see above).
            self._operator_paused = False
            await emit_bb("operator_resumed")
            await emit_bb("operator_stopped",
                           flags=len(self._found_flags))
            for other in state.tasks:
                self._cancel_solver(state.task_solvers.get(other))
                other.cancel()
            return "break"
        if self._operator_paused:
            # A hint/answer may wake the loop for bookkeeping, but only
            # RESUME/THAW owns this latch. Reap any completed tasks at the
            # next loop top without emitting resumed or spawning work.
            return "continue"
        state.reason_retry_pending = False
        state.reason_retry_count = 0
        state.reason_retry_not_before = 0.0
        state.reason_retry_failure = ""
        await emit_bb("operator_resumed")
        # same `while tasks:` guard as the other resume paths: if every
        # worker finished while paused, seed one bootstrap so the loop
        # lives on instead of falling out of `while tasks:`.
        if not state.tasks:
            try:
                engine = self._pick_engine(running_engines(), state.healthy, role="bootstrap")
            except RuntimeError as exc:
                await emit_bb("worker_spawn_rejected", reason=str(exc),
                               phase="resume_bootstrap")
                return "continue"
            try:
                w = self._make_cli_worker(
                    engine, mode="bootstrap",
                    intent_goal=self._retry_goal())
            except WorkerSpawnRejected as exc:
                await emit_bb("worker_spawn_rejected", reason=str(exc),
                               engine=str(engine), phase="resume_bootstrap")
                return "break"
            except WorkerBudgetExhausted as exc:
                terminal = await stop_for_budget(str(exc))
                return "break" if terminal else "continue"
            t = await self._schedule_control_worker(
                w, name=f"resume-bootstrap-{engine}")
            state.tasks[t] = engine
            state.task_solvers[t] = w
            await emit_bb("worker_spawned", worker=w.solver_id,
                           phase="resume_bootstrap", worker_role="worker",
                           **worker_identity_event_fields(w))
        return "continue"
    return "proceed"


async def pending_help_stage(self, state) -> str:
    import asyncio
    from functools import partial
    from muteki.solver.worker_profiles import worker_identity_event_fields
    from muteki.swarm.coordinator_state import (
        emit_scheduler_bb,
        running_engines as scheduler_running_engines,
        stop_for_budget as scheduler_stop_for_budget,
    )
    from muteki.swarm.swarm_support import (
        WorkerBudgetExhausted,
        WorkerSpawnRejected,
    )

    emit_bb = partial(emit_scheduler_bb, self, state)
    running_engines = partial(scheduler_running_engines, state)
    stop_for_budget = partial(scheduler_stop_for_budget, self, state)
    # ── operator-blocked: a worker raised its hand (NEED_INPUT / env_down)
    # — pause HERE, at the top of the loop body, BEFORE any spawning. The
    # old pause lived only in the "fully idle" branch (not tasks and not
    # open_intents), but the never-give-up Reason engine keeps minting
    # intents, so the swarm is never idle and the pause never fired
    # (run-11189: 3 NEED_INPUTs, 0 awaiting_operator, ~30 min hurling fresh
    # workers at the same no-dashboard-token wall). Pausing here is
    # phase-independent: as long as an ask is outstanding we wait for the
    # operator instead of spawning more doomed workers. The wait is
    # interruptible — _drain_hitl sets _operator_event on ANY operator
    # command (and clears _pending_help), and STOP wakes us to break.
    if self._pending_help and self.bus is not None:
        # Per-ask clip raised 120→300 (+ ellipsis on truncation) so the
        # amber "awaiting operator" banner shows enough of each ask to be
        # actionable. The full text rides the HITL_REQUEST card, which the
        # worker now emits without truncation; this is just the summary.
        def _clip(s: str) -> str:
            s = str(s)
            return s if len(s) <= 300 else (s[:300] + " …")
        needs = "; ".join(
            _clip(h.get("need", "")) for h in self._pending_help[-3:])
        # LOST-WAKEUP FIX: clear the event BEFORE emitting / awaiting.
        # _drain_hitl runs concurrently; if the operator answers during the
        # `await` of the awaiting_operator emit below, it sets _operator_event
        # and clears _pending_help. Clearing the event here (after that set)
        # would swallow the answer and — under the live inf budget — block
        # forever (the run-11189 deadlock class). So clear first, emit, then
        # re-check _pending_help: if the operator already answered, skip the
        # wait entirely.
        self._operator_event.clear()
        await emit_bb("awaiting_operator", reason=needs,
                       count=len(self._pending_help))
        if not self._pending_help or self._operator_stop:
            # answered (or stopped) in the emit window — don't wait/freeze.
            self._operator_paused = False
            # L6: balance the awaiting_operator bracket so the FE clears its
            # "awaiting operator" banner even on this no-wait exit.
            await emit_bb("operator_resumed")
            if self._operator_stop:
                await emit_bb("operator_stopped",
                               flags=len(self._found_flags))
                for other in state.tasks:
                    self._cancel_solver(state.task_solvers.get(other))
                    other.cancel()
                return "break"
            return "continue"
        # NEED_INPUT uses the same process/lease/budget transaction as an
        # explicit FREEZE. A raw InsightBus SIGSTOP would let intent leases
        # expire and could silently fail while the UI claimed a pause.
        help_freeze_owned = False
        try:
            help_freeze_owned = self._begin_operator_help_freeze()
        except Exception as exc:
            await emit_bb(
                "operator_freeze_failed", reason=str(exc)[:300])
            for other in state.tasks:
                self._cancel_solver(state.task_solvers.get(other))
                other.cancel()
            return "break"
        # A worker explicitly asked for help, so blocking is correct —
        # but an unattended run with a finite wall_clock_budget (offline
        # eval) must still be able to exhaust its budget rather than hang
        # forever waiting for an operator who isn't there (same reasoning
        # as the barren pause below). With an infinite budget (the live
        # default) we block indefinitely, exactly as before.
        if (self.wall_clock_budget == float("inf")
                or (self._control_frozen and not help_freeze_owned)):
            await self._operator_event.wait()  # blocks until operator acts
        else:
            remaining = self.wall_clock_budget - self._budget_elapsed(state.t0)
            try:
                await asyncio.wait_for(self._operator_event.wait(),
                                       timeout=max(0.0, remaining))
            except asyncio.TimeoutError:
                if help_freeze_owned:
                    try:
                        self._end_operator_help_freeze(
                            reason="operator help wait timed out")
                    except Exception as exc:
                        await emit_bb(
                            "operator_thaw_failed", reason=str(exc)[:300])
                # L6: balance the paused-state bracket so the FE clears its
                # "awaiting operator / paused" banner on this exit too.
                self._budget_exhausted_kind = "wall_clock_budget_exhausted"
                self._operator_paused = False
                await emit_bb("operator_resumed")
                await emit_bb("budget_exhausted",
                               elapsed=int(self._budget_elapsed(state.t0)))
                for other in state.tasks:
                    self._cancel_solver(state.task_solvers.get(other))
                    other.cancel()
                return "break"
        if self._operator_stop:
            if help_freeze_owned:
                try:
                    self._end_operator_help_freeze(
                        reason="operator stopped help wait")
                except Exception as exc:
                    await emit_bb(
                        "operator_thaw_failed", reason=str(exc)[:300])
            # L6: balance the paused-state bracket (see above).
            self._operator_paused = False
            await emit_bb("operator_resumed")
            await emit_bb("operator_stopped",
                           flags=len(self._found_flags))
            for other in state.tasks:
                self._cancel_solver(state.task_solvers.get(other))
                other.cancel()
            return "break"
        # operator responded → transactionally restore processes, leases,
        # and active-time accounting before dispatch resumes.
        if help_freeze_owned:
            try:
                self._end_operator_help_freeze(
                    reason="operator answered help wait")
            except Exception as exc:
                await emit_bb(
                    "operator_thaw_failed", reason=str(exc)[:300])
                for other in state.tasks:
                    self._cancel_solver(state.task_solvers.get(other))
                    other.cancel()
                return "break"
        elif not self._control_frozen:
            self._operator_paused = False
        # _pending_help already cleared by _drain_hitl,
        # the standing hint folded into future workers. If every worker
        # finished while we were paused, `tasks` is now empty — and the
        # loop guard `while tasks:` (plus asyncio.wait, which rejects an
        # empty set) would END the run instead of resuming with the new
        # input. So when idle-after-wake, spawn a fresh bootstrap worker
        # (seeded with _retry_goal + the standing hint) BEFORE looping, to
        # keep `tasks` non-empty. If workers are still running, just
        # re-poll from the top.
        if self._control_frozen:
            await emit_bb(
                "operator_still_frozen",
                reason="help was answered; explicit freeze remains active")
            while self._control_frozen and not self._operator_stop:
                self._operator_event.clear()
                if self._control_frozen:
                    await self._operator_event.wait()
            if self._operator_stop:
                await emit_bb(
                    "operator_stopped", flags=len(self._found_flags))
                for other in state.tasks:
                    self._cancel_solver(state.task_solvers.get(other))
                    other.cancel()
                return "break"
        await emit_bb("operator_resumed")
        if not state.tasks:
            try:
                engine = self._pick_engine(running_engines(), state.healthy, role="bootstrap")
            except RuntimeError as exc:
                await emit_bb("worker_spawn_rejected", reason=str(exc),
                               phase="resume_bootstrap")
                return "continue"
            try:
                w = self._make_cli_worker(
                    engine, mode="bootstrap",
                    intent_goal=self._retry_goal())
            except WorkerSpawnRejected as exc:
                await emit_bb("worker_spawn_rejected", reason=str(exc),
                               engine=str(engine), phase="resume_bootstrap")
                return "break"
            except WorkerBudgetExhausted as exc:
                terminal = await stop_for_budget(str(exc))
                return "break" if terminal else "continue"
            t = await self._schedule_control_worker(
                w, name=f"resume-bootstrap-{engine}")
            state.tasks[t] = engine
            state.task_solvers[t] = w
            await emit_bb("worker_spawned", worker=w.solver_id,
                           phase="resume_bootstrap", worker_role="worker",
                           **worker_identity_event_fields(w))
        return "continue"
    return "proceed"
