"""Flag/finding/goal completion predicates. Moved from coordinator_flags.py."""

from __future__ import annotations

from typing import TYPE_CHECKING, Optional

if TYPE_CHECKING:
    pass

from muteki.models.solve_graph import SolveGraph

def _expected_flags(self) -> int:
    return max(1, getattr(self.challenge, "expected_flags", 1) or 1)


def _multi_flag(self) -> bool:
    return bool(getattr(self.challenge, "multi_flag", False))


def _completing_flag_actor(self) -> str:
    """Return the Worker that wrote the newest still-accepted Flag."""
    if self.shared_graph is None:
        return ""
    accepted = set(self._found_flags)
    try:
        events = self.shared_graph.events_since(0, kinds=["flag_found"])
    except Exception:
        return ""
    for event in reversed(events):
        if (event.get("payload") or {}).get("flag") in accepted:
            return str(event.get("actor") or "")
    return ""


def _flags_complete(self) -> bool:
    """Is the run's flag objective met? This is the SAVE-vs-FINISH decoupling
    (run-10070): saving a flag (_record_flags) must not finish a collect-mode run
    the way it finishes a single-flag run.

    - single-flag (multi_flag=False, the default): `len >= expected_flags`, which
      with expected_flags=1 finishes on the first submitted Flag.
    - collect mode with a known count (multi_flag=True, expected_flags>1): finish
      once N distinct flags are collected.
    - collect mode with UNKNOWN count (multi_flag=True, expected_flags<=1): NEVER
      finish by count. Flags still save + display; the run ends only on operator
      STOP or the coordinator's no-progress pause. A saved flag is not a finish."""
    if bool(getattr(self.challenge, "platform_confirmation_required", False)):
        return False
    if self._multi_flag() and self._expected_flags() <= 1:
        return False
    return len(self._found_flags) >= self._expected_flags()


def _findings_complete(self) -> bool:
    """Host judgment over independently evidenced findings or finite coverage."""
    if getattr(self.challenge, "mode", "ctf") != "pentest":
        return False
    contract = getattr(self.challenge, "pentest_contract", None)
    if contract is None or self.shared_graph is None:
        return False
    from muteki.pentest.judgement import evaluate
    return evaluate(self.shared_graph.events(), contract)["objective_status"] == "met"


def _qualified_report_count(self) -> int:
    contract = getattr(self.challenge, "pentest_contract", None)
    if contract is not None and self.shared_graph is not None:
        from muteki.pentest.judgement import evaluate
        return int(evaluate(self.shared_graph.events(), contract)["qualified_findings"])
    return 0


def _pentest_product(self) -> bool:
    """Product pentest: success is gated findings, not flags."""
    return (
        getattr(self.challenge, "mode", "ctf") == "pentest"
    )


def _goal_satisfied(self) -> bool:
    """Stop predicate for the live coordinator."""
    if self._pentest_product():
        return self._findings_complete()
    return self._flags_complete()


def _record_findings(self, *findings: dict | None) -> list[dict]:
    fresh: list[dict] = []
    seen = {SolveGraph._finding_identity(f) for f in self._found_findings}
    for item in findings:
        if not item:
            continue
        key = SolveGraph._finding_identity(item)
        if not key or key in seen:
            continue
        self._found_findings.append(dict(item))
        seen.add(key)
        fresh.append(dict(item))
    return fresh


async def wait_for_workers_stage(self, state) -> None:
    import asyncio
    from functools import partial
    from muteki.swarm.coordinator_state import (
        emit_scheduler_bb,
        running_engines as scheduler_running_engines,
    )

    emit_bb = partial(emit_scheduler_bb, self, state)
    running_engines = partial(scheduler_running_engines, state)
    await self._reconcile_control_continuations()

    # Worker control belongs to the live control plane, so claim it
    # before any scheduler wait / operator-help / soft-pause branch.
    # Handling it only in the lower dispatch section leaves an explicit
    # spawn command unclaimed whenever the coordinator is paused or
    # awaiting input, even though that command correctly wakes the loop.
    await self._apply_worker_cmds(
        tasks=state.tasks, task_solvers=state.task_solvers, healthy=state.healthy,
        running_engines_fn=running_engines, emit_bb=emit_bb)
    waitables = set(state.tasks.keys())
    if state.reason_task is not None:
        waitables.add(state.reason_task)
    if waitables:
        finished, _pending = await asyncio.wait(
            waitables, timeout=self.config_poll_interval(),
            return_when=asyncio.FIRST_COMPLETED)
        state.done = finished.intersection(state.tasks)
    else:
        # Role policy can change which queued intents are runnable.
        # Keep the control plane responsive while re-evaluating an
        # otherwise idle coordinator epoch.
        await asyncio.sleep(self.config_poll_interval())
        state.done = set()

    # reap finished workers. reaped_n counts every worker that RAN to
    # an end (incl. errors — spent budget either way) for the barren
    # backpressure below; cancelled workers were killed, not fruitless.
    state.reaped_n = 0
    state.completed_for_review_n = 0




async def reconcile_completion_stage(self, state) -> str:
    from functools import partial
    from muteki.swarm.coordinator_state import emit_scheduler_bb

    emit_bb = partial(emit_scheduler_bb, self, state)
    if state.winner is None:
        self._sync_flags_from_graph()
        pentest_product = getattr(self.challenge, "mode", "ctf") == "pentest"
        if pentest_product:
            if self._findings_complete():
                state.goal_complete = True
                await emit_bb(
                    "goal_complete",
                    why="model_goal_with_evidence",
                    reports=0,
                    findings=self._qualified_report_count())
                for other in state.tasks:
                    self._cancel_solver(state.task_solvers.get(other))
                    other.cancel()
        elif self._flags_complete():
            import time

            producer = _completing_flag_actor(self)
            state.winner = producer or "coordinator"
            state.flag = self._found_flags[0] if self._found_flags else None
            state.goal_complete = True
            producer_task = None
            producer_solver = None
            for task, solver in state.task_solvers.items():
                if (
                    producer
                    and str(getattr(solver, "solver_id", "") or "") == producer
                    and not task.done()
                ):
                    producer_task = task
                    producer_solver = solver
                    continue
                self._cancel_solver(solver)
                task.cancel()
            try:
                await self.insight.all_flags_found(
                    producer or "coordinator", count=len(self._found_flags))
            except Exception:
                pass
            await emit_bb(
                "all_flags_found",
                flags=len(self._found_flags),
                worker=producer,
            )
            if producer_task is not None and producer_solver is not None:
                conclude_timeout = max(
                    1.0,
                    float(getattr(producer_solver, "conclude_timeout", 0.0) or 0.0),
                )
                state.termination_conclude_reason = "goal_complete"
                state.termination_conclude_worker = producer
                state.termination_conclude_deadline = time.monotonic() + conclude_timeout
                await emit_bb(
                    "goal_conclude_window",
                    worker=producer,
                    conclude_timeout=int(conclude_timeout),
                )

    if state.termination_conclude_reason:
        return "proceed"
    if state.winner is not None or state.goal_complete or self._coverage_exhausted:
        return "break"
    return "proceed"


def _sync_findings_from_graph(self) -> list[dict]:
    if self.shared_graph is None:
        return []
    try:
        snap = self.shared_graph.snapshot()
        graph_findings = list(getattr(snap, "findings", []) or [])
        invalidated = set()
        if hasattr(self.shared_graph, "invalidated_findings"):
            invalidated = self.shared_graph.invalidated_findings()
    except Exception:
        return []
    if invalidated:
        self._found_findings = [
            f for f in self._found_findings if SolveGraph._finding_identity(f) not in invalidated
        ]
    return self._record_findings(*(
        f for f in graph_findings if SolveGraph._finding_identity(f) not in invalidated
    ))


def _record_flags(self, *flags: Optional[str]) -> list[str]:
    """Add flags to the dedup set; return the ones that were NEW (so the caller
    can broadcast each exactly once)."""
    fresh: list[str] = []
    for f in flags:
        if f is not None and f not in self._found_flags:
            self._found_flags.append(f)
            fresh.append(f)
    return fresh


def _sync_flags_from_graph(self) -> list[str]:
    """Reconcile the in-memory flag set with the AUTHORITATIVE shared-graph
    snapshot, returning the flags that were newly absorbed (for one-time
    broadcast). This is the fix for the run-75379 split-brain (BUG②).

    Every worker writes each submitted Flag to the shared graph via _accept_flag
    → shared_graph.flag_found, and the graph snapshot is what the UI / planner /
    finalize already trust. But _found_flags (the in-memory list _flags_complete
    reads) is fed ONLY from reaped `outcome.flags`, so a flag that reached the
    graph via a path that never delivered a clean outcome — a worker cancelled
    after it accepted a flag (reaped as CancelledError, line ~3615), an
    error-reaped worker, or the live-broadcast/DB-bridge path — stays invisible
    to the completion check. In run-75379 the graph held 4 valid flags (5 found,
    1 operator-invalidated) while _found_flags was stuck at 2, so _flags_complete()
    never fired and the run spawned ~55 post-solve waves until operator stop.

    Reconciling against snapshot().flags makes the graph the single source of
    truth for completion:
      - ADD any flag the graph holds but _found_flags is missing.
      - DROP any flag the operator explicitly INVALIDATED (snapshot already
        excludes it), so a blacklisted false positive (e.g. 090099b7) can never
        count toward expected_flags (BUG③ cross-check).
    Absent-from-snapshot-but-not-invalidated flags are left in place while the
    graph is being reconciled."""
    if self.shared_graph is None:
        return []
    try:
        graph_flags = list(getattr(self.shared_graph.snapshot(), "flags", []) or [])
        invalidated = self.shared_graph.invalidated_flags()
    except Exception:
        return []
    # DROP operator-invalidated flags from the in-memory set (and never let one
    # back in below). reopen_after_false_positive removes it from the snapshot
    # too, so this only matters for a flag already absorbed before invalidation.
    if invalidated:
        self._found_flags = [f for f in self._found_flags if f not in invalidated]
    # ADD any authoritative flag the in-memory set is missing.
    fresh = self._record_flags(*(f for f in graph_flags if f not in invalidated))
    return fresh
