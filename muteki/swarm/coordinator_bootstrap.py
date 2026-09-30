"""Coordinator admission, race transition, and main-loop preparation."""

from __future__ import annotations

import asyncio
import hashlib
import json
import time
from functools import partial

from muteki.solver.worker_profiles import worker_identity_event_fields
from muteki.swarm.graph_defs import EV_FACT_ADDED
from muteki.swarm.coordinator_state import (
    CoordinatorRunState,
    emit_scheduler_bb,
    running_engines as scheduler_running_engines,
    stop_for_budget as scheduler_stop_for_budget,
)
from muteki.swarm.coordinator_worker_reap import _record_worker_dispatch_failure
from muteki.swarm.swarm_support import (
    ControlShutdownIncomplete,
    SwarmOutcome,
    WorkerBudgetExhausted,
    WorkerDispatchFailureLimit,
    WorkerRuntimeUnavailable,
    WorkerSpawnRejected,
)


async def abort_preloop_acquisitions(
    self, state: CoordinatorRunState,
) -> None:
    for task, solver in list(state.task_solvers.items()):
        if not task.done():
            self._cancel_solver(solver)
            task.cancel()
    if state.tasks:
        await asyncio.gather(*state.tasks.keys(), return_exceptions=True)
    released_ids: set[str] = set()
    for task, solver in state.task_solvers.items():
        sid = str(getattr(solver, "solver_id", "") or "")
        if sid and sid not in released_ids:
            await self._retire_worker_account(
                solver, intent_id=str(
                    state.task_intents.get(task)
                    or getattr(solver, "intent_id_assigned", "")
                    or getattr(solver, "_intent_id", "") or ""),
                reason="coordinator acquisition aborted",
                lane_key=str(state.task_lanes.get(task) or ""),
            )
            released_ids.add(sid)
    if state.hitl_task is not None:
        state.hitl_task.cancel()
        await asyncio.gather(state.hitl_task, return_exceptions=True)
    if self._shutdown_owners_incomplete():
        self._retain_control_shutdown_owner(
            winner=state.winner, flag=state.flag, goal_complete=state.goal_complete,
            per_solver=state.per_solver)
        raise ControlShutdownIncomplete(
            "control shutdown incomplete; runtime owner retained")
    try:
        await self._finalize_coordinator_run(
            winner=state.winner, flag=state.flag, goal_complete=state.goal_complete,
            per_solver=state.per_solver)
    except Exception:
        pass


async def start_coordinator(
    self, state: CoordinatorRunState,
) -> SwarmOutcome | None:
    abort_preloop = partial(abort_preloop_acquisitions, self, state)
    try:
        pentest_contract = getattr(self.challenge, "pentest_contract", None)
        if pentest_contract is not None and self.shared_graph is not None:
            payload = pentest_contract.model_dump(mode="json")
            contract_digest = hashlib.sha256(json.dumps(
                payload, ensure_ascii=False, sort_keys=True, separators=(",", ":"),
            ).encode("utf-8")).hexdigest()[:16]
            self.shared_graph._append(
                "pentest_contract", "operator",
                payload,
                verified=True,
                # A resumed Run may return to an earlier contract.  Deduping
                # only by its content leaves the intervening contract as the
                # latest graph event, so reports read the wrong goal.
                dedupe_key=(
                    f"pentest-contract::{self.challenge.id}::"
                    f"g{self._execution_generation}::{pentest_contract.version}::{contract_digest}"
                ),
            )
    except BaseException:
        await abort_preloop()
        raise
    async def fail_budget_preflight(code: str, detail: str) -> SwarmOutcome:
        self._runtime_failure_code = code
        self._runtime_failure_phase = "worker_preflight"
        self._runtime_failure_detail = detail
        await self._emit_coord_bb(code, code=code, detail=detail)
        if state.hitl_task is not None:
            state.hitl_task.cancel()
            await asyncio.gather(state.hitl_task, return_exceptions=True)
        await self._finalize_coordinator_run(
            winner=None, flag=None, goal_complete=False,
            per_solver=state.per_solver,
        )
        return SwarmOutcome(False, None, None, state.per_solver, "runtime_failure")

    if (self.cost_budget_usd is not None and self.cost is not None
            and self.cost.snapshot()["unpriced_calls"]):
        return await fail_budget_preflight(
            "budget_coverage_incomplete",
            "该 Run 已有未定价用量，无法执行完整美元预算；请取消美元预算或新建 Run"
        )
    if self.cost_budget_usd is not None:
        metered_models = [("planner", self.reason_model)]
        if self.llm is not None:
            metered_models.append(("titler", self.titler_model))
        unpriced_models = [
            f"{role}: {model}"
            for role, model in metered_models
            if self.cost is None or self.cost.price_for(model) is None
        ]
        if unpriced_models:
            return await fail_budget_preflight(
                "pricing_unavailable",
                "美元预算所需模型未定价（" + ", ".join(unpriced_models)
                + "）；请在服务端价格表添加精确单价，或取消美元预算",
            )
    try:
        state.healthy = await self._healthy_engines_async()
    except BaseException:
        await abort_preloop()
        raise
    if not state.healthy:
        if getattr(self, "_pricing_blocked_all", False):
            return await fail_budget_preflight(
                "pricing_unavailable",
                "; ".join(getattr(self, "_pricing_unavailable_profiles", {}).values()),
            )
        if state.hitl_task is not None:
            state.hitl_task.cancel()
            await asyncio.gather(state.hitl_task, return_exceptions=True)
        await self._emit_coord_bb(
            "health_unavailable",
            reason="NoEligibleEngine",
            configured_engines=list(self.engines),
        )
        await self._finalize_coordinator_run(
            winner=None, flag=None, goal_complete=False,
            per_solver=state.per_solver,
        )
        return SwarmOutcome(
            False, None, None, state.per_solver,
            "paused: NoEligibleEngine (all configured worker health probes failed)",
        )

    # ── race-scout layer (DESIGN_race_scout_layer.md) ────────────────────
    # ONE round of fresh single-shot bootstrap workers (one per race engine).
    # Normal coordinator runs adopt them immediately so Reason and complementary
    # Explore work begin concurrently.  The legacy non-adoption path retains its
    # historical fast/slow completion behavior for callers that need a blocking
    # race. Disabled (race_scout=False) → byte-identical to the plain loop.
    #
    # run-75379 BUG④ — INVARIANT GUARD: race-scout only ever runs on a genuine
    # COLD start. On a reopen/resume of a populated graph (prior intents/flags) we
    # skip the race entirely and go straight to the Reason/Explore loop on the
    # existing evidence. _is_cold_start is the load-bearing guard (explicit
    # cold_start hint + a graph-state backstop), so a relaunch that forgot to pass
    # race_scout=False / cold_start=False is still protected — the web reopen path
    # (run_manager.resolve) keeps passing race_scout=False as harmless redundancy.
    try:
        state.cold_start = self._is_cold_start()
    except BaseException:
        await abort_preloop()
        raise
    state.race_missed = False
    state.race_reasoned_wm = -1
    ctf_decide_first = bool(
        getattr(self.challenge, "mode", "ctf") in {"ctf", "pentest"}
        and self._auto_dispatch_enabled()
    )
    if self.race_scout and state.cold_start and not ctf_decide_first:
        try:
            # Adopt still-running race verifiers into the main loop's task
            # maps instead of cancelling them at the race boundary (they
            # were just dispatched to reproduce submitted reports).
            race_winner, race_flag, race_solvers = await self._run_race_scout(
                state.healthy,
                adopt_verifiers=(state.tasks, state.task_solvers, state.task_intents),
                adopt_race_workers=(state.tasks, state.task_solvers, state.task_intents),
            )
        except (WorkerDispatchFailureLimit, WorkerRuntimeUnavailable):
            await abort_preloop()
            return SwarmOutcome(
                False, None, None, state.per_solver, "runtime_failure",
            )
        except BaseException:
            await abort_preloop()
            raise
        state.per_solver.update(race_solvers)
        if bool(getattr(self, "_race_scout_handed_off", False)):
            # Race is now a concurrent scout lane, not a blocking prelude.  The
            # existing coordinator task lifecycle owns these Workers from here;
            # start Reason immediately against the task and any live facts.
            state.race_missed = True
            state.race_reasoned_wm = -1
            state.reason_next_trigger = "race_concurrent_handoff"
            try:
                await self._emit_coord_bb(
                    "phase_transition",
                    **{"from": "race", "to": "coordinator"},
                    concurrent=True,
                    facts_seeded=self._total_fact_count(),
                    flags=len(self._found_flags),
                )
            except BaseException:
                await abort_preloop()
                raise
            return None
        # Race write-back gate (stage-1 invariant): _run_race_scout's finally
        # awaits EVERY race-worker task before it returns — cancelled ones
        # included (asyncio.gather over the whole tasks map, which only ever
        # grows) — and each worker commits its structured result to the shared
        # graph inside run() before its task completes (cancel/crash paths
        # commit handoff_missing from run()'s cleanup). task-done ⇒ result
        # committed, so the first post-race Reason below always plans over
        # fully written-back race evidence; no extra barrier is needed here.
        await self._emit_coord_bb(
            "race_writeback_complete",
            workers=len(race_solvers),
            committed=sum(
                1 for outcome in race_solvers.values()
                if getattr(outcome, "worker_result", None) is not None),
        )
        if race_winner is not None and self._flags_complete():
            # fast path: reuse the winner-exit shape (persist + close + RUN_FINISHED)
            # via the shared M11 finalizer (idempotent).
            state.winner, state.flag = race_winner, race_flag
            if state.hitl_task is not None:
                state.hitl_task.cancel()
                await asyncio.gather(state.hitl_task, return_exceptions=True)
            if self._shutdown_owners_incomplete():
                self._retain_control_shutdown_owner(
                    winner=state.winner, flag=state.flag, goal_complete=state.goal_complete,
                    per_solver=state.per_solver)
                raise ControlShutdownIncomplete(
                    "control shutdown incomplete; runtime owner retained")
            await self._finalize_coordinator_run(
                winner=state.winner, flag=state.flag, goal_complete=False, per_solver=state.per_solver)
            return SwarmOutcome(True, state.flag, state.winner, state.per_solver,
                                "solved via race-scout",
                                flags=list(self._found_flags))
        # slow path: facts already on the shared graph; fall through to the
        # main coordinator loop.
        state.race_missed = True
        try:
            await self._emit_coord_bb(
                "phase_transition", **{"from": "race", "to": "coordinator"},
                facts_seeded=self._total_fact_count(),
                flags=len(self._found_flags),
            )
            await self._emit_coord_bb(
                "coverage_gap",
                source="race_miss",
                detail="race completed without satisfying the goal",
            )
            # Planning over race evidence belongs to the live coordinator loop.
            # Mark the first single-flight Decide with its real trigger and enter
            # the loop immediately; a slow planner must not hold the transition.
            state.race_reasoned_wm = -1
            state.reason_next_trigger = "race_miss"
        except BaseException:
            await abort_preloop()
            raise
        if self.review_policy.get("after_race", False):
            try:
                await self._maybe_start_review(
                    trigger="after_race",
                    directive=(
                        "Race scout ended without completing the challenge. Audit "
                        "the scoped facts and challenged assumptions; merge duplicates, "
                        "challenge weak facts, and record any search gap as a "
                        "REVIEW_FINDING before the coordinator expands the search."
                    ),
                    healthy=state.healthy, tasks=state.tasks, task_solvers=state.task_solvers,
                    emit_bb=self._emit_coord_bb,
                )
            except BaseException:
                await abort_preloop()
                raise
    elif self.race_scout and not state.cold_start:
        # WARM START (race-scout configured on, but this is a resume/reopen of a
        # populated graph): skip the race AND its after_race review — there was no
        # race to audit. Just announce the warm entry so the deck/board reflects
        # that the coordinator picked up on existing evidence, then fall through to
        # the same Reason/Explore loop the slow path uses. The loop's own
        # graph-change Reason trigger plans from the carried-over facts on tick 1.
        try:
            await self._emit_coord_bb(
                "phase_transition", **{"from": "resume", "to": "coordinator"},
                facts_seeded=self._total_fact_count(),
                flags=len(self._found_flags),
            )
        except BaseException:
            await abort_preloop()
            raise
    return None


def initialize_loop_state(self, state: CoordinatorRunState) -> None:
    state.t0 = time.monotonic()
    # Warm starts begin at the current strong-information head.  Historical
    # facts must not masquerade as progress made by the first resumed worker.
    state.last_fact_count = self._verified_fact_count()
    # ── (A) reason checkpoint: graph-change trigger ───────────────────────
    # Snapshot of (facts, open_intents) at the last reason. Reason fires when the
    # graph GREW (new fact) or open intents were CONSUMED — not on a fixed stall.
    # This is what lets the swarm keep producing intents → keep filling slots,
    # instead of idling at 2 workers while one slowly emits facts (the
    # "permanently 2 workers" bug: progress was SUPPRESSING expansion).
    state.reason_fact_ckpt = state.last_fact_count
    state.reason_open_intent_ckpt = 0
    state.last_decided_wm = int(state.race_reasoned_wm)
    state.last_consumed_wm = state.last_decided_wm
    state.decide_followup_pending = False
    # no-progress backpressure (run-10070: 48 barren Reason rounds; run-11190:
    # a 238-worker spike the old collect-only, idle-branch guardrail could not
    # reach because open intents kept the loop busy — same structural miss as
    # the run-11189 NEED_INPUT pause). Count CONSECUTIVE worker COMPLETIONS
    # that produced NO new fact (incl. candidates) AND NO new flag; at
    # barren_limit, soft-PAUSE for the operator at the TOP of the loop (fires
    # busy or idle). Keyed on zero-new-evidence per finished worker, NOT a
    # global-fact-stall timer, so it can't death-spiral a deep-exploit worker
    # that's mid-setup (run-7352 lesson: never time-based, never kill).
    state.fruitless_workers = 0
    state.prog_fact_ckpt = state.last_fact_count
    state.prog_flag_ckpt = len(self._found_flags)
    state.prog_report_ckpt = 0
    # Race-scout deliberately does not bootstrap again. If its first Decide pass
    # produced no claimable work, keep the coordinator alive by scheduling another
    # planning pass. Goal-incomplete runs never enter an autonomous operator wait.
    state.reason_retry_pending = bool(
        state.race_missed and not self._open_intents()
    )
    state.reason_retry_not_before = time.monotonic()
    state.last_pause_fruitless = -1
    # H: long-run compaction trigger. Track when the board last grew; if too long
    # passes with no progress (or fruitless workers pile up past 2× barren_limit),
    # compact the graph (retire stale closed intents) and reset the barren count.
    state.last_progress_t = time.monotonic()
    state.last_compact_t = 0.0
    state.compact_no_progress_s = float(getattr(self, "compact_no_progress_s", 1800.0))
    self._last_candidate_review_count = self._candidate_fact_count()


async def prepare_main_loop(self, state: CoordinatorRunState) -> None:
    emit_bb = partial(emit_scheduler_bb, self, state)
    running_engines = partial(scheduler_running_engines, state)
    stop_for_budget = partial(scheduler_stop_for_budget, self, state)
    # ── 刀4: revive resume-parked intents on a CONTINUED run ─────────
    # A prior non-solved finalize parked this run's in-flight intents in
    # dispatch_state='resume'. No-op on a fresh run.
    if self.shared_graph is not None:
        revived = self.shared_graph.revive_resume_intents(
            actor="coordinator")
        if revived:
            await emit_bb(
                "intent_state_changed", intent_id=",".join(revived),
                dispatch_state="active")

    if (
        self.shared_graph is not None
        and getattr(self.challenge, "mode", "ctf") in {"ctf", "pentest"}
    ):
        has_origin = any(
            event.get("kind") == EV_FACT_ADDED
            and str((event.get("payload") or {}).get("source") or "") == "origin"
            for event in self.shared_graph.events()
        )
        if not has_origin:
            contract = getattr(self.challenge, "task_contract", None)
            instruction = str(
                getattr(contract, "raw_instruction", "") or ""
            ).strip() or str(self.challenge.description or "").strip()
            origin_lines = ["题目信息:", instruction]
            target = str(self.challenge.target or "").strip()
            if target and target not in instruction:
                origin_lines.extend(["", f"目标地址: {target}"])
            attachments = [
                str(getattr(item, "name", "") or "").strip()
                for item in (getattr(contract, "attachments", None) or [])
            ] or [
                str(item).strip()
                for item in (self.challenge.attachments or [])
                if str(item).strip()
            ]
            if attachments:
                origin_lines.append("附件: " + ", ".join(attachments))
            origin_fact = "\n".join(origin_lines).strip()
            origin_seq = self.shared_graph.add_evidence(
                actor="origin",
                source="origin",
                fact=origin_fact,
                verified=True,
                confidence=1.0,
            )
            if origin_seq > 0:
                await emit_bb(
                    "fact_added",
                    actor="origin",
                    fact_seq=origin_seq,
                    source="origin",
                    fact=origin_fact,
                )

    # Auto CTF/pentest dispatches concrete Steps. On a resumed graph, dispatch
    # revived Steps directly; if none remain, let Decide plan from the carried
    # evidence before admitting another Worker. Cold-start parallelism is still
    # supplied by the ordinary Step frontier and its available seats.
    planned_auto_run = bool(
        getattr(self.challenge, "mode", "ctf") in {"ctf", "pentest"}
        and self._auto_dispatch_enabled()
        and not state.race_missed
    )
    ctf_decide_first = bool(
        planned_auto_run
        and not self._open_intents()
    )
    ctf_planned = False
    if ctf_decide_first:
        requested_intents = self._ordinary_capacity_limit()
        await emit_bb(
            "reason_start",
            trigger=("ctf_cold_start" if state.cold_start else "ctf_resume"),
            requested_intents=requested_intents,
        )
        previous_override = getattr(self, "_reason_max_intents_override", None)
        self._reason_max_intents_override = requested_intents
        try:
            ctf_planned = bool(await self._run_reason())
        finally:
            self._reason_max_intents_override = previous_override
        await emit_bb(
            "reason_done",
            trigger=("ctf_cold_start" if state.cold_start else "ctf_resume"),
            **self._reason_event_fields(len(self._open_intents())),
        )
        decided_wm = int(
            self.shared_graph.semantic_graph_watermark() or 0
        )
        state.last_decided_wm = decided_wm
        state.last_consumed_wm = decided_wm
        state.reason_fact_ckpt = self._verified_fact_count()
        state.reason_open_intent_ckpt = len(self._open_intents())
        if not ctf_planned:
            # Retry transport/invalid-plan failures. A valid empty plan with
            # unchanged evidence is handled as no progress by idle_stage; an
            # identical immediate Decide cannot create new information.
            if getattr(self, "_last_planner_failure", None) is not None:
                state.reason_next_trigger = (
                    "ctf_cold_start_retry" if state.cold_start
                    else "ctf_resume_retry"
                )
    elif planned_auto_run and not state.cold_start:
        # A warm graph may contain Steps waiting on stale dependencies. Reason
        # reads the carried evidence while any ready Steps dispatch in parallel.
        state.reason_next_trigger = "ctf_resume"

    # Fixed dispatch retains its original bootstrap admission. Auto CTF and
    # pentest use only the concrete Step frontier on both cold and warm starts;
    # resuming a populated graph no longer starts broad duplicate campaigns.
    if planned_auto_run:
        initial_workers = 0
    elif state.race_missed:
        initial_workers = 0
    elif state.cold_start or self._flags_complete():
        initial_workers = min(self.start_workers, max(1, len(state.healthy)))
    else:
        initial_workers = max(1, len(state.healthy))
    for _i in range(initial_workers):
        if not self._ordinary_capacity_available(state.tasks):
            break
        try:
            engine = self._pick_engine(
                running_engines(), state.healthy, role="bootstrap")
        except RuntimeError as exc:
            await emit_bb(
                "worker_spawn_rejected", reason=str(exc), phase="bootstrap")
            break
        try:
            worker = self._make_cli_worker(engine, mode="bootstrap")
        except WorkerSpawnRejected as exc:
            await emit_bb(
                "worker_spawn_rejected", reason=str(exc),
                engine=str(engine), phase="bootstrap")
            if await _record_worker_dispatch_failure(
                self, state, worker="", engine=str(engine), detail=str(exc),
            ):
                break
            state.reason_retry_pending = True
            state.reason_retry_not_before = time.monotonic()
            break
        except WorkerBudgetExhausted as exc:
            await stop_for_budget(str(exc))
            break
        task = await self._schedule_control_worker(
            worker, name=f"bootstrap-{engine}")
        state.tasks[task] = engine
        state.task_solvers[task] = worker
        await emit_bb(
            "worker_spawned", worker=worker.solver_id,
            phase="bootstrap", worker_role="worker",
            **worker_identity_event_fields(worker))
