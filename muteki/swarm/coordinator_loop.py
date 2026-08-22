"""The coordinator main loop plus winner persistence.

Split out of ``swarm.py`` (code-health G1) as a mixin of ``Swarm``. Every method
body is byte-for-byte the original; the mixin is composed back into ``Swarm`` so
behavior and the public surface are unchanged. Instance state built in
``Swarm.__init__`` is resolved through the composed class at runtime.
"""

# ruff: noqa: F401
from __future__ import annotations

import asyncio
import hashlib
import json
import os
import re
from dataclasses import dataclass, field
from pathlib import Path
from typing import TYPE_CHECKING, Any, Optional

if TYPE_CHECKING:
    from muteki.learning.distill import TemplateStore

from muteki.core.cost import CostController
from muteki.core.event_bus import EventBus
from muteki.core.runtime_env import is_web_container
from muteki.core.events import Event, EventType, blackboard_delta_payload
from muteki.core.llm import LLMClient, ModelSpec
from muteki.models.solve_graph import Challenge
from muteki.sandbox.manager import SandboxManager
from muteki.solver.result import ArtifactStore
from muteki.solver.types import SolverConfig, SolveOutcome
from muteki.solver.credential_accounts import runtime_env_for_engine
from muteki.solver.worker_profiles import (
    base_engine_for_profile,
    coerce_nonneg_int,
    normalize_profile_roster,
    normalize_worker_profiles,
    profile_names,
    worker_identity_event_fields,
)
from muteki.solver.workspace import cleanup_worker_scratch, ensure_workspace
from muteki.swarm.insight_bus import InsightBus
from muteki.swarm.stage_policy import StagePolicy
from muteki.swarm.shared_graph import SharedGraph, SQLiteSharedGraph, canonicalize_lane
from muteki.swarm.swarm_support import (
    _STANDING_MAX,
    _PENDING_HELP_MAX,
    WorkerBudgetExhausted,
    WorkerSpawnRejected,
    RequiredContextUnavailable,
    ControlShutdownIncomplete,
    SwarmOutcome,
    _CONTAINER_BLACKBOARD_SKILL,
    _BLACKBOARD_SKILL_LINKS,
    _ensure_blackboard_skill_links,
    _HEALTH_PROBE_CACHE,
    _HEALTH_FAILURE_TTL_FRACTION,
    _health_cache_get,
    _health_cache_put,
    _health_cache_clear,
    _is_control_failure,
)


_STALL_RECLAIM_S = 120.0


class _CoordinatorLoopMixin:
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

    async def _run_coordinator(self) -> SwarmOutcome:
        """The evidence-driven plan / dispatch loop. See class header."""
        import time

        # event the coordinator waits on when it pauses for operator help; set by
        # _drain_hitl on any operator command.
        self._operator_event = asyncio.Event()
        # sink: capture workers' HITL_REQUEST (NEED_INPUT / env_down) off the shared
        # bus so the coordinator knows a direction is blocked on the operator. Mirror
        # of RunManager's meta sink. Best-effort; never raises into a worker's emit.
        if self.bus is not None:
            async def _help_sink(ev: Event) -> None:
                if ev.event_type is EventType.HITL_REQUEST:
                    # M6: dedup on (worker, need) and cap the list. The per-worker
                    # marker dedup is per-worker, so the SAME blocker raised by N
                    # workers (or re-emitted by a re-bootstrapped worker) used to
                    # append N entries — inflating the awaiting_operator `count`,
                    # pushing the earliest (often most important) asks past the
                    # [-3:] summary window, and growing unbounded on a never-give-up
                    # run. Keyed dedup + cap fixes all three.
                    payload = dict(ev.payload or {})
                    need_kind = str(payload.get("need_kind") or "external_blocker")
                    need_text = str(payload.get("need", "")).strip()
                    worker = str(payload.get("worker", ""))
                    need_kind = self._rechecked_need_kind(need_text, need_kind)
                    payload["need_kind"] = need_kind
                    # F: persist the classification (need_kind) so the deck can render
                    # auto-resolving kinds differently from a true operator blocker,
                    # and the audit trail shows how each hand-raise was triaged.
                    if self.shared_graph is not None and need_text:
                        try:
                            self.shared_graph.add_hitl_request(
                                worker=worker or "worker", need=need_text,
                                need_kind=need_kind,
                                request_id=str(payload.get("request_id") or "") or None,
                                classification_confidence=float(
                                    payload.get("classification_confidence") or 1.0),
                                status=("awaiting_operator"
                                        if need_kind == "external_blocker"
                                        else "auto_resolved"))
                        except Exception:
                            pass
                    # emit the classification delta OUT-OF-BAND (scheduled, not awaited):
                    # this sink runs INSIDE bus.emit, so awaiting another emit here would
                    # re-enter the bus and reorder/deadlock the NEED_INPUT pause path.
                    try:
                        asyncio.create_task(self._emit_coord_bb(
                            "hitl_classified", worker=worker, need=need_text[:200],
                            need_kind=need_kind,
                            pauses_behavior=(need_kind == "external_blocker")))
                    except Exception:
                        pass
                    if need_kind == "lane_lock_request":
                        if self.shared_graph is not None:
                            lane_payload = self._lane_proposal_from_need(need_text, worker)
                            if lane_payload.get("lane_key"):
                                try:
                                    self.shared_graph.add_review_proposal(
                                        actor=worker or "worker",
                                        marker="LANE_LOCK",
                                        payload=lane_payload,
                                        tier="tier2",
                                    )
                                except Exception:
                                    pass
                            else:
                                self._pending_uncertainty_reviews.append(payload)
                        return
                    if need_kind == "route_dead_end":
                        if self.shared_graph is not None:
                            try:
                                route = SQLiteSharedGraph.normalize_route_hash(
                                    "", label=need_text)
                                self.shared_graph.add_review_proposal(
                                    actor=worker or "worker",
                                    marker="ROUTE_SUPPRESS",
                                    payload={
                                        "route_hash": route,
                                        "label": need_text[:120],
                                        "reason": need_text[:1000],
                                        "confidence": 0.85,
                                    },
                                    tier="tier2",
                                )
                            except Exception:
                                pass
                        return
                    if need_kind == "worker_uncertainty":
                        if self.shared_graph is not None:
                            try:
                                self.shared_graph.add_evidence(
                                    actor=worker or "worker",
                                    source="need_input",
                                    fact=f"Worker uncertainty: {need_text[:500]}",
                                    verified=False,
                                    confidence=0.30,
                                )
                            except Exception:
                                pass
                        self._pending_uncertainty_reviews.append(payload)
                        return
                    key = (str(payload.get("worker", "")),
                           str(payload.get("need", "")).strip())
                    for h in self._pending_help:
                        if (str(h.get("worker", "")),
                                str(h.get("need", "")).strip()) == key:
                            break  # already pending — don't duplicate
                    else:
                        self._pending_help.append(payload)
                        if len(self._pending_help) > _PENDING_HELP_MAX:
                            del self._pending_help[
                                : len(self._pending_help) - _PENDING_HELP_MAX]
                        # translate the (often English) hand-raise to zh in the
                        # background so the operator reads it more easily — same
                        # fire-and-forget pattern as node summaries; the deck swaps
                        # the card text to zh when HITL_TRANSLATED arrives. Only the
                        # FIRST occurrence of a (worker, need) is translated (we're in
                        # the dedup-miss branch), so a re-raise won't re-translate.
                        if self.llm is not None:
                            try:
                                from muteki.solver.summarizer import translate_need
                                asyncio.create_task(translate_need(
                                    str(payload.get("need", "")),
                                    worker=str(payload.get("worker", "")),
                                    model=self.titler_model, llm=self.llm,
                                    bus=self.bus, run_id=self.run_id,
                                    challenge_id=self.challenge.id))
                            except Exception:
                                pass
            try:
                self.bus.add_sink(_help_sink)
                self._coord_sinks.append(_help_sink)  # L3: detach on finalize
            except Exception:
                pass

        # ── submission gate (only when the target rate-limits its verifier) ──────
        # Serialize submissions across the swarm: when a worker declares
        # READY_TO_SUBMIT (board delta kind "ready_to_submit"), broadcast
        # SUBMIT_LOCKED so every OTHER worker holds its own submission, then
        # auto-release after a short lease (the submitting worker runs the verifier
        # within its current turn and can't explicitly hand the lock back). A worker
        # that detects a cooldown broadcasts VERIFIER_LOCKED independently
        # (cli_solver._maybe_broadcast_lockout) — the workers honor it via
        # _drain_control. The coordinator's job is the GRANT-time serialization +
        # not piling on redundant submitters. Default-off: ordinary CTFs never
        # register this sink, so their path is byte-identical.
        self._submit_lock_until = 0.0
        if self.bus is not None and getattr(self.challenge, "verifier_rate_limited", False):
            async def _submit_gate_sink(ev: Event) -> None:
                if ev.event_type is not EventType.BLACKBOARD_DELTA:
                    return
                p = ev.payload or {}
                if p.get("kind") != "ready_to_submit":
                    return
                now = time.time()
                # if a submission is already in flight (lease not elapsed), let the
                # worker's own SUBMIT_LOCKED broadcast handle the new declarer; don't
                # double-announce. Otherwise open a serialization window.
                if now < self._submit_lock_until:
                    return
                self._submit_lock_until = now + 90.0  # one submission's worth of lease
                actor = p.get("actor") or "worker"
                try:
                    # tell every OTHER worker to hold its submission while `actor`
                    # runs the verifier. (The worker also broadcasts this itself;
                    # the coordinator dedups + enforces the serialization window.)
                    await self.insight.submit_locked(actor)
                except Exception:
                    pass
            try:
                self.bus.add_sink(_submit_gate_sink)
                self._coord_sinks.append(_submit_gate_sink)  # L3: detach on finalize
            except Exception:
                pass

        hitl_task: Optional[asyncio.Task] = None
        if self.hitl_inbox is not None:
            hitl_task = asyncio.create_task(
                self._supervise_control_drain(), name="hitl-drain")

        tasks: dict[asyncio.Task, str] = {}        # task -> engine
        task_intents: dict[asyncio.Task, str] = {}  # task -> intent_id
        task_solvers: dict[asyncio.Task, Any] = {}  # task -> CliSolver (to cancel)
        task_lanes: dict[asyncio.Task, str] = {}   # task -> exclusive lane key
        # Mid-flight fruitless interrupt (MUTEKI_FRUITLESS_INTERRUPT): per-worker
        # start marks so a long tool-spin with no new fact/flag can be reaped and
        # replanned. Lazy-filled on first observation; never a global stall clock.
        task_started_at: dict[asyncio.Task, float] = {}
        task_prog_ckpt: dict[asyncio.Task, tuple[int, int]] = {}
        # Round-5: per-worker tool-progress marks (count + last increase time).
        task_tool_count: dict[asyncio.Task, int] = {}
        task_last_tool_t: dict[asyncio.Task, float] = {}
        fruitless_interrupt_tasks: set[asyncio.Task] = set()
        fruitless_interrupt_count = 0
        force_reason_after_fruitless_interrupt = False
        # Last interrupt meta for the Reason working packet (round 3).
        last_fruitless_interrupt_meta: dict[str, Any] = {}
        # Round-4: after interrupt-forced Reason, recover with re-bootstrap
        # instead of needs_new_information → collect_idle death.
        pending_interrupt_reason_recovery = False
        interrupt_rebootstrap_count = 0
        interrupt_empty_reason_retries = 0
        interrupt_chain_intent_injected = False
        last_interrupt_named_artifacts: list[str] = []
        last_interrupt_replan_domain: str = ""
        # Round-16 solo-depth verify: periodic live harvest without cancel.
        solo_verify_last_t: dict[asyncio.Task, float] = {}
        solo_verify_tool_ckpt: dict[asyncio.Task, int] = {}
        solo_verify_count = 0
        per_solver: dict[str, SolveOutcome] = {}
        winner: Optional[str] = None
        flag: Optional[str] = None
        # pentest product: goal_complete is set by _findings_complete (gated
        # findings), never by Reason verdict=complete alone. CTF leaves this
        # False and ends only on a gated flag (winner).
        goal_complete = False

        async def _abort_preloop_acquisitions() -> None:
            """Fail-closed cleanup for work acquired before the main loop's finally."""
            for task, solver in list(task_solvers.items()):
                if not task.done():
                    self._cancel_solver(solver)
                    task.cancel()
            if tasks:
                await asyncio.gather(*tasks.keys(), return_exceptions=True)
            released_ids: set[str] = set()
            for task, solver in task_solvers.items():
                sid = str(getattr(solver, "solver_id", "") or "")
                if sid and sid not in released_ids:
                    await self._retire_worker_account(
                        solver, intent_id=str(
                            task_intents.get(task)
                            or getattr(solver, "intent_id_assigned", "")
                            or getattr(solver, "_intent_id", "") or ""),
                        reason="coordinator acquisition aborted",
                        lane_key=str(task_lanes.get(task) or ""),
                    )
                    released_ids.add(sid)
            if hitl_task is not None:
                hitl_task.cancel()
                await asyncio.gather(hitl_task, return_exceptions=True)
            if self._shutdown_owners_incomplete():
                self._retain_control_shutdown_owner(
                    winner=winner, flag=flag, goal_complete=goal_complete,
                    per_solver=per_solver)
                raise ControlShutdownIncomplete(
                    "control shutdown incomplete; runtime owner retained")
            try:
                await self._finalize_coordinator_run(
                    winner=winner, flag=flag, goal_complete=goal_complete,
                    per_solver=per_solver)
            except Exception:
                pass

        try:
            healthy = await self._healthy_engines_async()
        except BaseException:
            await _abort_preloop_acquisitions()
            raise
        if not healthy:
            await self._emit_coord_bb(
                "health_unavailable",
                reason="NoEligibleEngine",
                configured_engines=list(self.engines),
            )
            if hitl_task is not None:
                hitl_task.cancel()
                await asyncio.gather(hitl_task, return_exceptions=True)
            await self._finalize_coordinator_run(
                winner=None, flag=None, goal_complete=False,
                per_solver=per_solver,
            )
            return SwarmOutcome(
                False, None, None, per_solver,
                "paused: NoEligibleEngine (all configured worker health probes failed)",
            )

        # ── race-scout layer (DESIGN_race_scout_layer.md) ────────────────────
        # ONE round of fresh single-shot bootstrap workers (one per race engine) in
        # parallel BEFORE the coordinator loop. FAST PATH: a worker captures the flag →
        # finish here, skip the coordinator loop (the simple-challenge speed of the
        # original muteki race). SLOW PATH: no flag → their facts are on the shared
        # graph and we fall through to the coordinator loop warm (Reason plans from
        # real facts, not an empty graph). Disabled (race_scout=False) →
        # byte-identical to the plain loop.
        #
        # run-75379 BUG④ — INVARIANT GUARD: race-scout only ever runs on a genuine
        # COLD start. On a reopen/resume of a populated graph (prior intents/flags) we
        # skip the race entirely and go straight to the Reason/Explore loop on the
        # existing evidence. _is_cold_start is the load-bearing guard (explicit
        # cold_start hint + a graph-state backstop), so a relaunch that forgot to pass
        # race_scout=False / cold_start=False is still protected — the web reopen path
        # (run_manager.resolve) keeps passing race_scout=False as harmless redundancy.
        try:
            cold_start = self._is_cold_start()
        except BaseException:
            await _abort_preloop_acquisitions()
            raise
        race_missed = False
        race_reasoned_state: tuple | None = None
        if self.race_scout and cold_start:
            try:
                # Adopt still-running race verifiers into the main loop's task
                # maps instead of cancelling them at the race boundary (they
                # were just dispatched to reproduce submitted reports).
                race_winner, race_flag, race_solvers = await self._run_race_scout(
                    healthy,
                    adopt_verifiers=(tasks, task_solvers, task_intents))
            except BaseException:
                await _abort_preloop_acquisitions()
                raise
            per_solver.update(race_solvers)
            if race_winner is not None and self._flags_complete():
                # fast path: reuse the winner-exit shape (persist + close + RUN_FINISHED)
                # via the shared M11 finalizer (idempotent).
                winner, flag = race_winner, race_flag
                if hitl_task is not None:
                    hitl_task.cancel()
                    await asyncio.gather(hitl_task, return_exceptions=True)
                if self._shutdown_owners_incomplete():
                    self._retain_control_shutdown_owner(
                        winner=winner, flag=flag, goal_complete=goal_complete,
                        per_solver=per_solver)
                    raise ControlShutdownIncomplete(
                        "control shutdown incomplete; runtime owner retained")
                await self._finalize_coordinator_run(
                    winner=winner, flag=flag, goal_complete=False, per_solver=per_solver)
                return SwarmOutcome(True, flag, winner, per_solver,
                                    "solved via race-scout",
                                    flags=list(self._found_flags))
            # slow path: facts already on the shared graph; fall through to the
            # main coordinator loop.
            race_missed = True
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
                # Exactly one planner pass over the race evidence.  The main loop
                # receives any focused intents it creates; a dry result becomes a
                # bounded NEEDS_NEW_INFORMATION pause instead of a second race.
                await self._run_reason()
                race_reasoned_state = (
                    self._verified_fact_count(),
                    tuple(self._found_flags),
                    tuple(sorted(
                        str(it.get("intent_id") or "")
                        for it in self._open_intents()
                    )),
                )
            except BaseException:
                await _abort_preloop_acquisitions()
                raise
            if self.review_policy.get("after_race", False):
                try:
                    await self._maybe_start_review(
                        trigger="after_race",
                        directive=(
                            "Race scout ended without completing the challenge. Audit "
                            "the seeded facts, repeated routes, challenged assumptions, "
                            "and propose suppress/reopen/branch/directive actions before "
                            "the coordinator expands the search."
                        ),
                        healthy=healthy, tasks=tasks, task_solvers=task_solvers,
                        emit_bb=self._emit_coord_bb,
                    )
                except BaseException:
                    await _abort_preloop_acquisitions()
                    raise
        elif self.race_scout and not cold_start:
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
                await _abort_preloop_acquisitions()
                raise

        # monotonic clock comes in via time.monotonic() — allowed (not Date.now)
        t0 = time.monotonic()
        # Warm starts begin at the current strong-information head.  Historical
        # facts must not masquerade as progress made by the first resumed worker.
        last_fact_count = self._verified_fact_count()
        # ── (A) reason checkpoint: graph-change trigger ───────────────────────
        # Snapshot of (facts, open_intents) at the last reason. Reason fires when the
        # graph GREW (new fact) or open intents were CONSUMED — not on a fixed stall.
        # This is what lets the swarm keep producing intents → keep filling slots,
        # instead of idling at 2 workers while one slowly emits facts (the
        # "permanently 2 workers" bug: progress was SUPPRESSING expansion).
        reason_fact_ckpt = last_fact_count
        reason_open_intent_ckpt = 0
        last_reason_state: tuple | None = race_reasoned_state
        # no-progress backpressure (run-10070: 48 barren Reason rounds; run-11190:
        # a 238-worker spike the old collect-only, idle-branch guardrail could not
        # reach because open intents kept the loop busy — same structural miss as
        # the run-11189 NEED_INPUT pause). Count CONSECUTIVE worker COMPLETIONS
        # that produced NO new fact (incl. candidates) AND NO new flag; at
        # barren_limit, soft-PAUSE for the operator at the TOP of the loop (fires
        # busy or idle). Keyed on zero-new-evidence per finished worker, NOT a
        # global-fact-stall timer, so it can't death-spiral a deep-exploit worker
        # that's mid-setup (run-7352 lesson: never time-based, never kill).
        fruitless_workers = 0
        prog_fact_ckpt = last_fact_count
        prog_flag_ckpt = len(self._found_flags)
        prog_report_ckpt = len(getattr(self, "_found_reports", []) or [])
        needs_new_information = False
        last_pause_fruitless = -1
        # H: long-run compaction trigger. Track when the board last grew; if too long
        # passes with no progress (or fruitless workers pile up past 2× barren_limit),
        # compact the graph (retire stale closed intents) and reset the barren count.
        last_progress_t = time.monotonic()
        last_compact_t = 0.0
        compact_no_progress_s = float(getattr(self, "compact_no_progress_s", 1800.0))
        self._last_candidate_review_count = self._candidate_fact_count()
        # NOTE: the old GLOBAL stall-kill is gone (run-7352 death spiral: every new
        # worker was born already stalled once easy facts froze the board clock).
        # Default reclaim remains: natural exit + explore per-turn timeout + intent
        # lease expiry.  An opt-in mid-flight fruitless interrupt
        # (MUTEKI_FRUITLESS_INTERRUPT=1) may cancel a *specific* worker whose own
        # start checkpoint shows no new verified fact/flag — never a global clock.

        def _sync_worker_start_marks() -> None:
            now_m = time.monotonic()
            fc = self._verified_fact_count()
            fl = len(self._found_flags)
            for t in tasks:
                if t not in task_started_at:
                    task_started_at[t] = now_m
                    task_prog_ckpt[t] = (fc, fl)

        async def _emit_bb(kind: str, **fields):
            if self.bus is not None:
                await self.bus.emit(Event(
                    event_type=EventType.BLACKBOARD_DELTA, run_id=self.run_id,
                    challenge_id=self.challenge.id,
                    payload=blackboard_delta_payload(kind, actor="coordinator", **fields)))

        def _running_engines() -> list[str]:
            return list(tasks.values())

        async def _stop_for_budget(kind: str) -> None:
            self._budget_exhausted_kind = kind
            await _emit_bb(kind, spawned_total=self._spawned_total,
                           max_total_workers=self.max_total_workers,
                           cost_usd=self._current_cost_usd(),
                           cost_budget_usd=self.cost_budget_usd)
            await _emit_bb("budget_exhausted", budget_kind=kind,
                           spawned_total=self._spawned_total,
                           cost_usd=self._current_cost_usd())
            for other in tasks:
                self._cancel_solver(task_solvers.get(other))
                other.cancel()

        async def _prepare_main_loop() -> None:
            """Acquire every pre-loop task under one cancellation-safe boundary."""
            # ── 刀4: revive resume-parked intents on a CONTINUED run ─────────
            # A prior non-solved finalize parked this run's in-flight intents in
            # dispatch_state='resume'. No-op on a fresh run.
            if self.shared_graph is not None:
                try:
                    revived = self.shared_graph.revive_resume_intents(
                        actor="coordinator")
                    if revived:
                        await _emit_bb(
                            "intent_state_changed", intent_id=",".join(revived),
                            dispatch_state="active")
                except Exception:
                    pass

            # ── Phase: Bootstrap — start_workers heterogeneous rush workers ──
            # 续解（resolve/reopen，非冷启动）没有 race-scout 阶段：若仍按
            # start_workers 只起一个 bootstrap，「拉起完整蜂群续解」就名不副实
            # （run-1987554：start_workers=1 导致单 worker 独跑 22 分钟，第二个
            # worker 直到结束前才被 Reason 补起）。续解起步直接覆盖全部健康普通
            # Seat；并发上限仍由 _ordinary_capacity_available(max_workers) 把关。
            # flag 已集齐的重开保持旧行为（少起 Worker，由主循环立刻收尾）。
            if race_missed:
                initial_workers = 0
            elif cold_start or self._flags_complete():
                initial_workers = min(self.start_workers, max(1, len(healthy)))
            else:
                initial_workers = max(1, len(healthy))
            for _i in range(initial_workers):
                if not self._ordinary_capacity_available(tasks):
                    break
                try:
                    engine = self._pick_engine(
                        _running_engines(), healthy, role="bootstrap")
                except RuntimeError as exc:
                    await _emit_bb(
                        "worker_spawn_rejected", reason=str(exc), phase="bootstrap")
                    break
                try:
                    worker = self._make_cli_worker(engine, mode="bootstrap")
                except WorkerSpawnRejected as exc:
                    await _emit_bb(
                        "worker_spawn_rejected", reason=str(exc),
                        engine=str(engine), phase="bootstrap")
                    break
                except WorkerBudgetExhausted as exc:
                    await _stop_for_budget(str(exc))
                    break
                task = await self._schedule_control_worker(
                    worker, name=f"bootstrap-{engine}")
                tasks[task] = engine
                task_solvers[task] = worker
                await _emit_bb(
                    "worker_spawned", worker=worker.solver_id,
                    phase="bootstrap", worker_role="worker",
                    **worker_identity_event_fields(worker))

        try:
            await _prepare_main_loop()
        except BaseException:
            await _abort_preloop_acquisitions()
            raise

        try:
            # A dispatcher pause/drain is itself live coordinator state. Workers
            # are allowed to finish while paused, so an empty ``tasks`` mapping is
            # not a terminal condition until RESUME/STOP (or drain completion)
            # resolves that latch.
            while (tasks or self._operator_draining
                   or self._operator_paused or needs_new_information):
                # ContextResource is the durable outbox for exact operator
                # continuations. A transient graph write in the control consumer
                # must heal during this same live run, not wait for a process
                # restart. The replay is metadata-only and graph-idempotent.
                await self._reconcile_control_continuations()
                if tasks:
                    done, _pending = await asyncio.wait(
                        set(tasks.keys()), timeout=self.config_poll_interval(),
                        return_when=asyncio.FIRST_COMPLETED)
                else:
                    done = set()

                # reap finished workers. reaped_n counts every worker that RAN to
                # an end (incl. errors — spent budget either way) for the barren
                # backpressure below; cancelled workers were killed, not fruitless.
                reaped_n = 0
                completed_for_review_n = 0
                for t in done:
                    is_review_task = t in self._active_review_tasks
                    is_verifier_task = t in self._active_verifier_tasks
                    engine = tasks.pop(t)
                    intent_id = task_intents.pop(t, None)
                    solver = task_solvers.pop(t, None)
                    task_started_at.pop(t, None)
                    task_prog_ckpt.pop(t, None)
                    task_tool_count.pop(t, None)
                    task_last_tool_t.pop(t, None)
                    was_fruitless_interrupt = t in fruitless_interrupt_tasks
                    fruitless_interrupt_tasks.discard(t)
                    was_stall_reclaim = bool(getattr(solver, "_stall_reclaim", False))
                    lane_key = task_lanes.get(t, "")
                    # Round-6: interrupt victims get a longer retire settle; a
                    # late exit proof must not abort the swarm before
                    # worker_finished / Reason / rebootstrap can run.
                    retire_timeout: float | None = None
                    if was_fruitless_interrupt:
                        try:
                            from muteki.swarm.fruitless_interrupt_v1 import (
                                settle_seconds as _fi_settle,
                            )
                            retire_timeout = _fi_settle()
                        except Exception:
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
                    if not retired_ok:
                        soft_continue = False
                        if was_fruitless_interrupt:
                            try:
                                from muteki.swarm.fruitless_interrupt_v1 import (
                                    should_soft_continue_after_retire_miss,
                                )
                                soft_continue = (
                                    should_soft_continue_after_retire_miss(
                                        was_fruitless_interrupt=True,
                                    )
                                )
                            except Exception:
                                soft_continue = True
                        if soft_continue:
                            await _emit_bb(
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
                            await _emit_bb(
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
                                    await _emit_bb(
                                        "intent_concluded",
                                        intent_id=str(intent_id),
                                        result="explored",
                                        reason="fruitless_interrupt",
                                    )
                                except Exception:
                                    pass
                            task_lanes.pop(t, None)
                            if t in self._active_verifier_tasks:
                                self._active_verifier_tasks.discard(t)
                            if not is_review_task and not is_verifier_task:
                                completed_for_review_n += 1
                            reaped_n += 1
                            force_reason_after_fruitless_interrupt = True
                            continue
                        for other, other_solver in list(task_solvers.items()):
                            if not other.done():
                                self._cancel_solver(other_solver)
                                other.cancel()
                        raise ControlShutdownIncomplete(
                            "worker wrapper exited before runtime exit proof")
                    # key per-solver outcomes by the worker's UNIQUE solver_id (e.g.
                    # cli-claude-2), not the bare engine — otherwise two same-engine
                    # workers (race + a later explore) clobber each other's record.
                    lane_key = task_lanes.pop(t, "")
                    if lane_key and self.shared_graph is not None:
                        try:
                            rel = getattr(
                                solver, "_muteki_lane_release_result", None)
                            if not isinstance(rel, dict):
                                rel = self.shared_graph.release_lane(  # type: ignore[attr-defined]
                                    actor="coordinator", lane_key=lane_key,
                                    by_worker=sid)
                            await self._consume_lane_release(rel, emit_bb=_emit_bb)
                        except Exception:
                            pass
                    try:
                        outcome = t.result()
                    except asyncio.CancelledError:
                        # Ordinary cancels (budget/stop/winner) stay silent. A
                        # mid-flight fruitless interrupt must surface as a finished
                        # worker so barren accounting + Reason replan can fire.
                        if was_fruitless_interrupt:
                            await _emit_bb(
                                "worker_finished",
                                worker=sid,
                                result="fruitless_interrupt",
                            )
                            if intent_id and self.shared_graph is not None:
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
                                    await _emit_bb(
                                        "intent_concluded",
                                        intent_id=str(intent_id),
                                        result="explored",
                                        reason="fruitless_interrupt",
                                    )
                                except Exception:
                                    pass
                            if not is_review_task and not is_verifier_task:
                                completed_for_review_n += 1
                            if t in self._active_verifier_tasks:
                                self._active_verifier_tasks.discard(t)
                            reaped_n += 1
                            force_reason_after_fruitless_interrupt = True
                        continue
                    except Exception as e:
                        per_solver[sid] = SolveOutcome(
                            False, None, 0, None, f"error: {e}")
                        if bool(getattr(solver, "_remote_start_uncertain", False)):
                            # StartWorker crossed the reverse link but no worker id /
                            # started ACK came back. The remote process may still own
                            # workspace state, so quarantine the whole run container;
                            # do not dispatch siblings until absence is proven.
                            self._mark_shutdown_incomplete(
                                "remote_start_uncertain")
                            for other in tasks:
                                if other is not t and not other.done():
                                    self._cancel_solver(task_solvers.get(other))
                                    other.cancel()
                            raise ControlShutdownIncomplete(
                                "remote worker start outcome is uncertain")
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
                        await _emit_bb("worker_finished", worker=sid,
                                       result="error")
                        if t in self._active_review_tasks:
                            self._active_review_tasks.discard(t)
                            await _emit_bb("review_finished", worker=sid,
                                           result="error")
                        if t in self._active_verifier_tasks:
                            self._active_verifier_tasks.discard(t)
                        if not is_review_task and not is_verifier_task:
                            completed_for_review_n += 1
                        reaped_n += 1
                        continue
                    reaped_n += 1
                    if not is_review_task and not is_verifier_task:
                        completed_for_review_n += 1
                    per_solver[outcome_id := sid] = outcome
                    await _emit_bb("worker_finished", worker=outcome_id,
                                   result="solved" if outcome.solved else "done")
                    if t in self._active_review_tasks:
                        self._active_review_tasks.discard(t)
                        await _emit_bb("review_finished", worker=outcome_id,
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
                    pentest_product = (
                        getattr(self.challenge, "mode", "ctf") == "pentest"
                        and not self._pentest_flag_required()
                    )
                    if pentest_product:
                        await self._drain_report_pipeline()
                        self._sync_findings_from_graph()
                        if self._findings_complete():
                            goal_complete = True
                            await _emit_bb(
                                "goal_complete",
                                why="gated_reports",
                                reports=len(self._found_reports),
                                findings=len(self._found_reports))
                            for other in tasks:
                                self._cancel_solver(task_solvers.get(other))
                                other.cancel()
                            break
                        if self._coverage_complete():
                            self._coverage_exhausted = True
                            await _emit_bb(
                                "coverage_complete",
                                findings=len(self._found_findings))
                            for other in tasks:
                                self._cancel_solver(task_solvers.get(other))
                                other.cancel()
                            break
                    if self._flags_complete() and winner is None and not pentest_product:
                        winner, flag = outcome_id, self._found_flags[0]
                        try:
                            await self.insight.all_flags_found(
                                "coordinator", count=len(self._found_flags))
                        except Exception:
                            pass
                        # kill the losing workers' subprocesses, not just their tasks
                        for other in tasks:
                            self._cancel_solver(task_solvers.get(other))
                            other.cancel()

                # ── split-brain reconcile (BUG②) ─────────────────────────────
                # The per-reap tally above only sees flags carried back in a clean
                # `outcome.flags`. A flag can reach the shared graph (and the UI /
                # planner) via a path that never delivered one — a worker cancelled
                # after it accepted a flag (reaped as CancelledError above), an
                # error-reaped worker, or the live DB→bus bridge. Sync the in-memory
                # set with the authoritative snapshot every iteration so completion
                # fires on the real flag count and a blacklisted flag is dropped
                # (run-75379: graph held 4 valid flags, _found_flags stuck at 2).
                if winner is None:
                    self._sync_flags_from_graph()
                    pentest_product = (
                        getattr(self.challenge, "mode", "ctf") == "pentest"
                        and not self._pentest_flag_required()
                    )
                    if pentest_product:
                        await self._drain_report_pipeline()
                        self._sync_findings_from_graph()
                        if self._findings_complete():
                            goal_complete = True
                            await _emit_bb(
                                "goal_complete",
                                why="gated_reports",
                                reports=len(self._found_reports),
                                findings=len(self._found_reports))
                            for other in tasks:
                                self._cancel_solver(task_solvers.get(other))
                                other.cancel()
                        elif self._coverage_complete():
                            self._coverage_exhausted = True
                            await _emit_bb(
                                "coverage_complete",
                                findings=len(self._found_findings))
                            for other in tasks:
                                self._cancel_solver(task_solvers.get(other))
                                other.cancel()
                    elif self._flags_complete():
                        winner = "coordinator"
                        flag = self._found_flags[0] if self._found_flags else None
                        try:
                            await self.insight.all_flags_found(
                                "coordinator", count=len(self._found_flags))
                        except Exception:
                            pass
                        await _emit_bb("all_flags_found",
                                       flags=len(self._found_flags))
                        for other in tasks:
                            self._cancel_solver(task_solvers.get(other))
                            other.cancel()

                if winner is not None or goal_complete or self._coverage_exhausted:
                    break

                # ── progress tracking (for the reason graph-change trigger) ──
                now = time.monotonic()
                fc = self._verified_fact_count()
                if fc > last_fact_count:
                    last_fact_count = fc

                # ── barren backpressure accounting (ALL modes) ───────────────
                # Per finished worker: did the board grow at all since the last
                # completion? Candidates count (engagement ≠ fruitless); flags
                # count; dead-ends deliberately do NOT — the run-11190 spike
                # workers wrote dead-ends while burning 238 slots on a solved wall.
                if reaped_n and self.barren_limit > 0:
                    # Only canonical/verified information clears stagnation.
                    # Candidate text and activity are useful telemetry, but letting
                    # them reset this cursor creates an infinite self-reward loop.
                    information_count = self._verified_fact_count()
                    pentest_product = (
                        getattr(self.challenge, "mode", "ctf") == "pentest"
                        and not self._pentest_flag_required()
                    )
                    if pentest_product:
                        grew = len(getattr(self, "_found_reports", []) or []) > prog_report_ckpt
                    else:
                        grew = (information_count > prog_fact_ckpt
                                or len(self._found_flags) > prog_flag_ckpt)
                    fruitless_workers = (0 if grew
                                         else fruitless_workers + reaped_n)
                    if grew:
                        needs_new_information = False
                        last_progress_t = time.monotonic()  # H: reset no-progress timer
                    prog_fact_ckpt = max(prog_fact_ckpt, information_count)
                    prog_flag_ckpt = max(prog_flag_ckpt, len(self._found_flags))
                    prog_report_ckpt = max(
                        prog_report_ckpt,
                        len(getattr(self, "_found_reports", []) or []))

                if completed_for_review_n:
                    self._completed_workers_since_review += completed_for_review_n

                if self._operator_stop:
                    # operator pressed stop / marked complete — end gracefully,
                    # keeping all recovered knowledge (the board persists). Distinct
                    # from budget_exhausted so the FE can show "stopped by operator".
                    await _emit_bb("operator_stopped",
                                   flags=len(self._found_flags))
                    for other in tasks:
                        self._cancel_solver(task_solvers.get(other))
                        other.cancel()
                    break

                elapsed = self._budget_elapsed(t0, now=now)
                if elapsed > self.wall_clock_budget:
                    self._budget_exhausted_kind = "wall_clock_budget_exhausted"
                    await _emit_bb("budget_exhausted", elapsed=int(elapsed))
                    for other in tasks:
                        self._cancel_solver(task_solvers.get(other))
                        other.cancel()
                    break

                budget_kind = self._budget_exhausted()
                if budget_kind:
                    await _stop_for_budget(budget_kind)
                    break

                if self._operator_draining:
                    if not tasks:
                        await _emit_bb("operator_drain_complete")
                        break
                    # Loop back through asyncio.wait/reap only. No Reason, review,
                    # dynamic command, or bootstrap path below may create work.
                    continue

                if await self._spawn_rebootstrap_from_directive(
                    healthy=healthy, tasks=tasks, task_solvers=task_solvers,
                    running_engines_fn=_running_engines, emit_bb=_emit_bb):
                    continue

                while self._pending_uncertainty_reviews:
                    p = self._pending_uncertainty_reviews.pop(0)
                    worker = str(p.get("worker") or "worker")
                    need = str(p.get("need") or "").strip()
                    self._queue_review_request(
                        trigger="worker_uncertainty",
                        directive=(
                            f"worker {worker} 不确定：{need[:500]}；"
                            "请审查当前事实/候选/意图，给出可执行的 NEXT_INTENT 或路线修正。"
                        ),
                    )

                if await self._maybe_run_queued_review(
                    healthy=healthy, tasks=tasks,
                    task_solvers=task_solvers, emit_bb=_emit_bb):
                    continue

                if await self._drain_review_proposals(
                    emit_bb=_emit_bb, fruitless_workers=fruitless_workers):
                    continue

                # E: surface any resource locks workers acquired since last tick.
                await self._drain_resource_locks(emit_bb=_emit_bb)
                await self._drain_graph_to_bus(emit_bb=_emit_bb)

                if self.review_policy.get("on_candidate_spike", True):
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
                                    "facts, suppress repeated routes, and propose verifier/code branches."
                                ),
                                healthy=healthy, tasks=tasks,
                                task_solvers=task_solvers, emit_bb=_emit_bb)):
                        continue

                every_completed = int(self.review_policy.get("every_completed_workers") or 0)
                if (every_completed > 0
                        and self._completed_workers_since_review >= every_completed
                        and await self._maybe_start_review(
                            trigger="every_completed_workers",
                            directive=(
                                f"{self._completed_workers_since_review} ordinary workers completed "
                                "since the last review. Audit whether the swarm is repeating a route, "
                                "needs a branch split, or should rebootstrap from a sharper directive."
                            ),
                            healthy=healthy, tasks=tasks,
                            task_solvers=task_solvers, emit_bb=_emit_bb)):
                    continue

                # ── operator SOFT-PAUSE (#5): the operator pressed pause. Stop
                # spawning NEW workers and wait for resume — but do NOT kill running
                # workers or end the run (that's stop). This is the meaningful "pause"
                # for a single-shot swarm. The wait is interruptible: resume sets
                # _operator_event; stop sets _operator_stop (handled at loop top next
                # iteration); a finite budget still expires (offline eval safety).
                if self._operator_paused:
                    self._operator_event.clear()
                    event_wait = asyncio.create_task(
                        self._operator_event.wait(), name="operator-pause-wait")
                    timeout = None
                    if (self.wall_clock_budget != float("inf")
                            and not self._control_frozen):
                        timeout = max(
                            0.0,
                            self.wall_clock_budget - self._budget_elapsed(t0),
                        )
                    done, _pending = await asyncio.wait(
                        {event_wait, *tasks},
                        timeout=timeout,
                        return_when=asyncio.FIRST_COMPLETED,
                    )
                    if event_wait not in done:
                        event_wait.cancel()
                        await asyncio.gather(event_wait, return_exceptions=True)
                    if not done:
                        # L6: balance the paused-state bracket so the FE clears its
                        # "awaiting operator / paused" banner on this exit too.
                        self._budget_exhausted_kind = "wall_clock_budget_exhausted"
                        self._operator_paused = False
                        await _emit_bb("operator_resumed")
                        await _emit_bb("budget_exhausted",
                                       elapsed=int(self._budget_elapsed(t0)))
                        for other in tasks:
                            self._cancel_solver(task_solvers.get(other))
                            other.cancel()
                        break
                    if self._operator_stop:
                        # L6: balance the paused-state bracket (see above).
                        self._operator_paused = False
                        await _emit_bb("operator_resumed")
                        await _emit_bb("operator_stopped",
                                       flags=len(self._found_flags))
                        for other in tasks:
                            self._cancel_solver(task_solvers.get(other))
                            other.cancel()
                        break
                    if self._operator_paused:
                        # A hint/answer may wake the loop for bookkeeping, but only
                        # RESUME/THAW owns this latch. Reap any completed tasks at the
                        # next loop top without emitting resumed or spawning work.
                        continue
                    needs_new_information = False
                    await _emit_bb("operator_resumed")
                    # same `while tasks:` guard as the other resume paths: if every
                    # worker finished while paused, seed one bootstrap so the loop
                    # lives on instead of falling out of `while tasks:`.
                    if not tasks:
                        try:
                            engine = self._pick_engine(_running_engines(), healthy, role="bootstrap")
                        except RuntimeError as exc:
                            await _emit_bb("worker_spawn_rejected", reason=str(exc),
                                           phase="resume_bootstrap")
                            continue
                        try:
                            w = self._make_cli_worker(
                                engine, mode="bootstrap",
                                intent_goal=self._retry_goal())
                        except WorkerSpawnRejected as exc:
                            await _emit_bb("worker_spawn_rejected", reason=str(exc),
                                           engine=str(engine), phase="resume_bootstrap")
                            break
                        except WorkerBudgetExhausted as exc:
                            await _stop_for_budget(str(exc))
                            break
                        t = await self._schedule_control_worker(
                            w, name=f"resume-bootstrap-{engine}")
                        tasks[t] = engine
                        task_solvers[t] = w
                        await _emit_bb("worker_spawned", worker=w.solver_id,
                                       phase="resume_bootstrap", worker_role="worker",
                                       **worker_identity_event_fields(w))
                    continue

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
                    await _emit_bb("awaiting_operator", reason=needs,
                                   count=len(self._pending_help))
                    if not self._pending_help or self._operator_stop:
                        # answered (or stopped) in the emit window — don't wait/freeze.
                        self._operator_paused = False
                        # L6: balance the awaiting_operator bracket so the FE clears its
                        # "awaiting operator" banner even on this no-wait exit.
                        await _emit_bb("operator_resumed")
                        if self._operator_stop:
                            await _emit_bb("operator_stopped",
                                           flags=len(self._found_flags))
                            for other in tasks:
                                self._cancel_solver(task_solvers.get(other))
                                other.cancel()
                            break
                        continue
                    # NEED_INPUT uses the same process/lease/budget transaction as an
                    # explicit FREEZE. A raw InsightBus SIGSTOP would let intent leases
                    # expire and could silently fail while the UI claimed a pause.
                    help_freeze_owned = False
                    try:
                        help_freeze_owned = self._begin_operator_help_freeze()
                    except Exception as exc:
                        await _emit_bb(
                            "operator_freeze_failed", reason=str(exc)[:300])
                        for other in tasks:
                            self._cancel_solver(task_solvers.get(other))
                            other.cancel()
                        break
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
                        remaining = self.wall_clock_budget - self._budget_elapsed(t0)
                        try:
                            await asyncio.wait_for(self._operator_event.wait(),
                                                   timeout=max(0.0, remaining))
                        except asyncio.TimeoutError:
                            if help_freeze_owned:
                                try:
                                    self._end_operator_help_freeze(
                                        reason="operator help wait timed out")
                                except Exception as exc:
                                    await _emit_bb(
                                        "operator_thaw_failed", reason=str(exc)[:300])
                            # L6: balance the paused-state bracket so the FE clears its
                            # "awaiting operator / paused" banner on this exit too.
                            self._budget_exhausted_kind = "wall_clock_budget_exhausted"
                            self._operator_paused = False
                            await _emit_bb("operator_resumed")
                            await _emit_bb("budget_exhausted",
                                           elapsed=int(self._budget_elapsed(t0)))
                            for other in tasks:
                                self._cancel_solver(task_solvers.get(other))
                                other.cancel()
                            break
                    if self._operator_stop:
                        if help_freeze_owned:
                            try:
                                self._end_operator_help_freeze(
                                    reason="operator stopped help wait")
                            except Exception as exc:
                                await _emit_bb(
                                    "operator_thaw_failed", reason=str(exc)[:300])
                        # L6: balance the paused-state bracket (see above).
                        self._operator_paused = False
                        await _emit_bb("operator_resumed")
                        await _emit_bb("operator_stopped",
                                       flags=len(self._found_flags))
                        for other in tasks:
                            self._cancel_solver(task_solvers.get(other))
                            other.cancel()
                        break
                    # operator responded → transactionally restore processes, leases,
                    # and active-time accounting before dispatch resumes.
                    if help_freeze_owned:
                        try:
                            self._end_operator_help_freeze(
                                reason="operator answered help wait")
                        except Exception as exc:
                            await _emit_bb(
                                "operator_thaw_failed", reason=str(exc)[:300])
                            for other in tasks:
                                self._cancel_solver(task_solvers.get(other))
                                other.cancel()
                            break
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
                        await _emit_bb(
                            "operator_still_frozen",
                            reason="help was answered; explicit freeze remains active")
                        while self._control_frozen and not self._operator_stop:
                            self._operator_event.clear()
                            if self._control_frozen:
                                await self._operator_event.wait()
                        if self._operator_stop:
                            await _emit_bb(
                                "operator_stopped", flags=len(self._found_flags))
                            for other in tasks:
                                self._cancel_solver(task_solvers.get(other))
                                other.cancel()
                            break
                    await _emit_bb("operator_resumed")
                    if not tasks:
                        try:
                            engine = self._pick_engine(_running_engines(), healthy, role="bootstrap")
                        except RuntimeError as exc:
                            await _emit_bb("worker_spawn_rejected", reason=str(exc),
                                           phase="resume_bootstrap")
                            continue
                        try:
                            w = self._make_cli_worker(
                                engine, mode="bootstrap",
                                intent_goal=self._retry_goal())
                        except WorkerSpawnRejected as exc:
                            await _emit_bb("worker_spawn_rejected", reason=str(exc),
                                           engine=str(engine), phase="resume_bootstrap")
                            break
                        except WorkerBudgetExhausted as exc:
                            await _stop_for_budget(str(exc))
                            break
                        t = await self._schedule_control_worker(
                            w, name=f"resume-bootstrap-{engine}")
                        tasks[t] = engine
                        task_solvers[t] = w
                        await _emit_bb("worker_spawned", worker=w.solver_id,
                                       phase="resume_bootstrap", worker_role="worker",
                                       **worker_identity_event_fields(w))
                    continue

                # ── H: long-run COMPACT — retire stale closed intents so the planner
                # board doesn't grow unbounded. Fires on a hard fruitless threshold
                # (2× barren_limit) OR a long no-progress window. Never touches facts
                # (design §12: compaction must not collapse a candidate into a fact);
                # only already-concluded, fact-less intents are retired. Resets the
                # barren counter so the run continues fresh after compaction.
                now_mono = time.monotonic()
                no_progress_elapsed = now_mono - last_progress_t
                compact_due = (
                    self.shared_graph is not None
                    and (now_mono - last_compact_t) > 60.0  # don't thrash
                    and (
                        (self.barren_limit > 0 and fruitless_workers >= 2 * self.barren_limit)
                        or (compact_no_progress_s > 0 and no_progress_elapsed >= compact_no_progress_s)
                    )
                )
                if compact_due:
                    trigger = ("fruitless_workers"
                               if fruitless_workers >= 2 * self.barren_limit
                               else "no_progress_time")
                    try:
                        fw_compact = getattr(self, "framework_before_compact", None)
                        if callable(fw_compact):
                            try:
                                fw_compact()
                            except Exception:
                                pass
                        info = self.shared_graph.compact_graph(
                            actor="coordinator", trigger=trigger,
                            summary=(f"compacted after {fruitless_workers} fruitless "
                                     f"workers / {int(no_progress_elapsed)}s no progress"))
                        last_compact_t = now_mono
                        if self.bus is not None:
                            try:
                                await self.bus.emit(Event(
                                    event_type=EventType.GRAPH_COMPACTED,
                                    run_id=self.run_id, challenge_id=self.challenge.id,
                                    payload={"compact_id": info.get("compact_id"),
                                             "trigger": trigger,
                                             "retired_intents": len(info.get("retired_intent_ids") or []),
                                             "summary": info.get("summary", "")}))
                            except Exception:
                                pass
                        await _emit_bb(
                            "graph_compacted", compact_id=info.get("compact_id"),
                            trigger=trigger,
                            retired=len(info.get("retired_intent_ids") or []))
                        # 刀6: mirror the per-intent retirement onto the bus so the
                        # deck folds dispatchState→retired (the DB event row alone
                        # never reaches the SSE/JSONL stream the UI reads).
                        retired_ids = [str(x) for x in (info.get("retired_intent_ids") or []) if x]
                        if retired_ids:
                            await _emit_bb(
                                "intent_state_changed",
                                intent_id=",".join(retired_ids),
                                dispatch_state="retired",
                                compact_id=info.get("compact_id"))
                    except Exception:
                        pass

                # ── barren backpressure pause: too many consecutive fruitless
                # workers → soft-pause for the operator BEFORE spawning more. Sits
                # here (top of loop, after the NEED_INPUT pause) so it fires even
                # while intents are open / workers are running — the old version
                # lived in the fully-idle branch and a busy churn spike never
                # reached it. Emits kind "collect_idle" (the historical name) so
                # the deck's existing paused-state handling applies unchanged.
                # Soft: no worker kill; any operator command resumes.
                barren_pause_due = (
                    self.barren_limit > 0
                    and fruitless_workers >= self.barren_limit
                    and fruitless_workers > last_pause_fruitless
                )
                if ((barren_pause_due or needs_new_information)
                        and not self._pending_help):
                    fruitless_review_after = int(self.review_policy.get("after_fruitless_workers") or 0)
                    if (fruitless_review_after > 0
                            and fruitless_workers >= fruitless_review_after
                            and await self._maybe_start_review(
                                trigger="fruitless_workers",
                                directive=(f"{fruitless_workers} consecutive workers produced no new fact or flag; "
                                           "audit repeated routes/dead-end amnesia and propose suppression or a corrected directive."),
                                healthy=healthy, tasks=tasks,
                                task_solvers=task_solvers, emit_bb=_emit_bb)):
                        continue
                    last_pause_fruitless = fruitless_workers
                    await _emit_bb(
                        "collect_idle",
                        reason=(f"{fruitless_workers} consecutive workers finished "
                                f"with no new fact or flag; "
                                f"{len(self._found_flags)} flags collected — "
                                "paused for the operator (STOP to finish, or send "
                                "a hint/input to continue)"),
                        flags=len(self._found_flags),
                        fruitless_workers=fruitless_workers)
                    self._operator_event.clear()
                    # Wait for the operator — but NOT unconditionally. Unlike the
                    # NEED_INPUT pause (a worker explicitly asked, so blocking is
                    # correct), this pause is autonomous, and a run with a finite
                    # wall_clock_budget (offline eval) must still be able to exhaust
                    # it rather than hang forever waiting for an operator who isn't
                    # there. With an infinite budget (the live default) we wait on the
                    # operator BUT also self-wake on real graph progress (see below).
                    if self.wall_clock_budget == float("inf"):
                        # ④ collect_idle must NOT block ONLY on the operator. The pause
                        # fires after N fruitless workers, but other in-flight workers
                        # (or the DB→bus bridge) can still land a NEW verified fact /
                        # flag, or a fresh dispatchable SOLVING intent can appear — in
                        # run-75377 the coordinator sat in this wait for 33 min while
                        # facts were still arriving and never re-dispatched. Poll on a
                        # short timeout and self-wake on real progress, treating it like
                        # an operator resume. The operator event still wins instantly.
                        _idle_fact_ckpt = self._verified_fact_count()
                        _idle_flag_ckpt = len(self._found_flags)
                        while not self._operator_event.is_set():
                            try:
                                await asyncio.wait_for(
                                    self._operator_event.wait(), timeout=15.0)
                                break
                            except asyncio.TimeoutError:
                                pass
                            if (self._verified_fact_count() > _idle_fact_ckpt
                                    or len(self._found_flags) > _idle_flag_ckpt):
                                break
                    elif self._control_frozen:
                        await self._operator_event.wait()
                    else:
                        remaining = self.wall_clock_budget - self._budget_elapsed(t0)
                        try:
                            await asyncio.wait_for(self._operator_event.wait(),
                                                   timeout=max(0.0, remaining))
                        except asyncio.TimeoutError:
                            # L6: balance the paused-state bracket so the FE clears its
                            # "awaiting operator / paused" banner on this exit too.
                            self._budget_exhausted_kind = "wall_clock_budget_exhausted"
                            self._operator_paused = False
                            await _emit_bb("operator_resumed")
                            await _emit_bb("budget_exhausted",
                                           elapsed=int(self._budget_elapsed(t0)))
                            for other in tasks:
                                self._cancel_solver(task_solvers.get(other))
                                other.cancel()
                            break
                    if self._operator_stop:
                        # L6: balance the paused-state bracket (see above).
                        self._operator_paused = False
                        await _emit_bb("operator_resumed")
                        await _emit_bb("operator_stopped",
                                       flags=len(self._found_flags))
                        for other in tasks:
                            self._cancel_solver(task_solvers.get(other))
                            other.cancel()
                        break
                    if self._operator_paused:
                        # A manual PAUSE command may be the event that woke the
                        # autonomous no-information wait.  Preserve its latch and
                        # let the dedicated manual-pause branch own the next turn.
                        continue
                    await _emit_bb("operator_resumed")
                    # Waking never erases the attempt streak.  Strong information
                    # clears it; an operator command merely grants one bounded next
                    # attempt before the same no-progress condition can pause again.
                    if self._verified_fact_count() > prog_fact_ckpt \
                            or len(self._found_flags) > prog_flag_ckpt:
                        prog_fact_ckpt = self._verified_fact_count()
                        prog_flag_ckpt = len(self._found_flags)
                        fruitless_workers = 0
                    needs_new_information = False
                    # same `while tasks:` guard as the NEED_INPUT resume above: if
                    # everything finished while paused, seed one bootstrap worker
                    # (with the operator's fresh standing hint) so the loop lives on.
                    if not tasks:
                        try:
                            engine = self._pick_engine(_running_engines(), healthy, role="bootstrap")
                        except RuntimeError as exc:
                            await _emit_bb("worker_spawn_rejected", reason=str(exc),
                                           phase="resume_bootstrap")
                            continue
                        try:
                            w = self._make_cli_worker(
                                engine, mode="bootstrap",
                                intent_goal=self._retry_goal())
                        except WorkerSpawnRejected as exc:
                            await _emit_bb("worker_spawn_rejected", reason=str(exc),
                                           engine=str(engine), phase="resume_bootstrap")
                            break
                        except WorkerBudgetExhausted as exc:
                            await _stop_for_budget(str(exc))
                            break
                        t = await self._schedule_control_worker(
                            w, name=f"resume-bootstrap-{engine}")
                        tasks[t] = engine
                        task_solvers[t] = w
                        await _emit_bb("worker_spawned", worker=w.solver_id,
                                       phase="resume_bootstrap", worker_role="worker",
                                       **worker_identity_event_fields(w))
                    continue

                # ── operator worker control: spawn/kill a specific engine on demand
                await self._apply_worker_cmds(
                    tasks=tasks, task_solvers=task_solvers, healthy=healthy,
                    running_engines_fn=_running_engines, emit_bb=_emit_bb)

                # ── mid-flight fruitless interrupt (MUTEKI_FRUITLESS_INTERRUPT=1)
                # Round-5: zero-fact board + tool-stall / sole-worker deferral +
                # hard cap for forever-tooling burns. Default OFF.
                _sync_worker_start_marks()
                try:
                    from muteki.swarm.fruitless_interrupt_v1 import (
                        artifact_extra_seconds as _fi_artifact_extra,
                        collect_named_artifacts as _fi_collect_arts,
                        enabled as _fi_enabled,
                        hard_cap_seconds as _fi_hard_cap,
                        max_interrupts as _fi_max,
                        should_interrupt_worker as _fi_should,
                        sole_extra_seconds as _fi_sole_extra,
                        threshold_seconds as _fi_threshold,
                        tool_stall_seconds as _fi_tool_stall,
                        worker_artifact_progress as _fi_art_prog,
                        worker_tool_count as _fi_tool_count,
                    )
                    interrupted_this_round: list[asyncio.Task] = []
                    # Round-16: solo-depth architecture disables cancel/replan
                    # interrupts — verify gate below owns progress folding.
                    _solo_depth_on = False
                    try:
                        from muteki.swarm.solo_depth_verify_v1 import (
                            enabled as _solo_enabled,
                        )
                        _solo_depth_on = bool(_solo_enabled())
                    except Exception:
                        _solo_depth_on = False
                    if (
                        (not _solo_depth_on)
                        and _fi_enabled()
                        and fruitless_interrupt_count < _fi_max()
                    ):
                        now_m = time.monotonic()
                        facts_now = self._verified_fact_count()
                        flags_now = len(self._found_flags)
                        thr = _fi_threshold()
                        stall_s = _fi_tool_stall()
                        sole_s = _fi_sole_extra()
                        cap_s = _fi_hard_cap()
                        art_extra_s = _fi_artifact_extra()
                        named_arts = _fi_collect_arts(
                            self.shared_graph,
                            attachments=list(
                                getattr(self.challenge, "attachments", None)
                                or []
                            ),
                            workspace_root=getattr(
                                self, "workspace_root", None
                            ),
                            challenge=self.challenge,
                        )
                        ordinary_n = self._ordinary_task_count(tasks)
                        for t, engine in list(tasks.items()):
                            if fruitless_interrupt_count >= _fi_max():
                                break
                            if t.done() or t in fruitless_interrupt_tasks:
                                continue
                            if t in self._active_review_tasks:
                                continue
                            solver = task_solvers.get(t)
                            sid_live = (
                                getattr(solver, "solver_id", None)
                                or f"cli-{engine}"
                            )
                            started = task_started_at.get(t, now_m)
                            f0, g0 = task_prog_ckpt.get(
                                t, (facts_now, flags_now))
                            w_mode = str(
                                getattr(solver, "mode", None) or "bootstrap"
                            )
                            tools_now = _fi_tool_count(solver)
                            prev_tools = task_tool_count.get(t, 0)
                            if tools_now > prev_tools:
                                task_tool_count[t] = tools_now
                                task_last_tool_t[t] = now_m
                            elif t not in task_last_tool_t:
                                # No tool mark yet — clock stall from start.
                                task_tool_count.setdefault(t, tools_now)
                                task_last_tool_t[t] = started
                            since_tool = now_m - task_last_tool_t.get(t, started)
                            art_prog = _fi_art_prog(solver, named_arts)
                            effective_cap = (
                                (cap_s + art_extra_s)
                                if (art_prog and cap_s > 0)
                                else cap_s
                            )
                            if not _fi_should(
                                running_for_s=now_m - started,
                                threshold_s=thr,
                                facts_at_start=f0,
                                flags_at_start=g0,
                                facts_now=facts_now,
                                flags_now=flags_now,
                                worker_mode=w_mode,
                                ordinary_worker_count=ordinary_n,
                                seconds_since_last_tool=since_tool,
                                tool_stall_s=stall_s,
                                sole_extra_s=sole_s,
                                hard_cap_s=cap_s,
                                artifact_progress=art_prog,
                                artifact_extra_s=art_extra_s,
                            ):
                                continue
                            # Round-10: flush tool observations into the graph
                            # BEFORE cancel — CLI rarely emits VERIFIED_FACT=.
                            harvested_n = 0
                            harvest_rows: list[dict[str, str]] = []
                            try:
                                from muteki.swarm.fruitless_interrupt_v1 import (
                                    commit_harvested_facts as _fi_commit,
                                    extract_crypto_clues as _fi_clues,
                                    harvest_artifact_tool_facts as _fi_harvest,
                                )
                                rows = _fi_harvest(solver, named_arts)
                                harvest_rows = list(rows or [])
                                if rows and self.shared_graph is not None:
                                    seqs = _fi_commit(
                                        self.shared_graph,
                                        actor=str(sid_live),
                                        rows=rows,
                                    )
                                    harvested_n = len(seqs)
                                    if harvested_n:
                                        clues = _fi_clues(
                                            rows, self.shared_graph, limit=6
                                        )
                                        await _emit_bb(
                                            "fruitless_interrupt_fact_harvest",
                                            worker=sid_live,
                                            harvested=harvested_n,
                                            checks=[
                                                r.get("check") for r in rows
                                            ][:8],
                                            artifacts=[
                                                r.get("artifact") for r in rows
                                            ][:8],
                                            fact_seqs=seqs[:8],
                                            crypto_clues=clues[:6],
                                        )
                            except Exception:
                                harvested_n = 0
                                harvest_rows = []
                            delivered = self._cancel_solver(solver)
                            t.cancel()
                            fruitless_interrupt_tasks.add(t)
                            interrupted_this_round.append(t)
                            fruitless_interrupt_count += 1
                            intent_goal = str(
                                getattr(solver, "intent_goal", None)
                                or getattr(solver, "_intent_goal", None)
                                or ""
                            )
                            last_fruitless_interrupt_meta = {
                                "worker": sid_live,
                                "goal": intent_goal,
                                "running_for_s": round(now_m - started, 1),
                                "worker_mode": w_mode,
                                "harvested_facts": harvested_n,
                                "harvest_rows": harvest_rows,
                            }
                            await _emit_bb(
                                "fruitless_interrupt",
                                worker=sid_live,
                                running_for_s=round(now_m - started, 1),
                                threshold_s=thr,
                                hard_cap_s=effective_cap,
                                base_hard_cap_s=cap_s,
                                artifact_progress=art_prog,
                                artifact_extra_s=art_extra_s,
                                tool_stall_s=stall_s,
                                seconds_since_last_tool=round(since_tool, 1),
                                tool_count=tools_now,
                                ordinary_workers=ordinary_n,
                                facts_at_start=f0,
                                flags_at_start=g0,
                                facts_now=facts_now,
                                flags_now=flags_now,
                                harvested_facts=harvested_n,
                                worker_mode=w_mode,
                                cancel_delivered=bool(delivered),
                                interrupt_index=fruitless_interrupt_count,
                            )
                    # Round-6: after cancel, wait briefly for wrapper done then
                    # loop so the reap path (worker_finished + Reason) runs
                    # before more explore spawns pile on.
                    if interrupted_this_round:
                        try:
                            from muteki.swarm.fruitless_interrupt_v1 import (
                                settle_seconds as _fi_settle_wait,
                            )
                            wait_s = min(5.0, _fi_settle_wait())
                        except Exception:
                            wait_s = 5.0
                        pending_victims = {
                            t for t in interrupted_this_round if not t.done()
                        }
                        if pending_victims:
                            await asyncio.wait(
                                pending_victims, timeout=wait_s)
                        continue
                except Exception:
                    pass

                # Per-worker stalled reclaim: not a global clock. Soft steer in
                # cli_solver runs first; if the worker is still stalled after
                # _STALL_RECLAIM_S, cancel it and reopen its intent.
                try:
                    now_stall = time.monotonic()
                    for t, solver in list(task_solvers.items()):
                        if t.done() or t in fruitless_interrupt_tasks:
                            continue
                        if t in self._active_review_tasks or t in self._active_verifier_tasks:
                            continue
                        if bool(getattr(solver, "_stall_reclaim", False)):
                            continue
                        stalled_at = getattr(solver, "_stalled_at", None)
                        if stalled_at is None:
                            continue
                        if (now_stall - float(stalled_at)) < _STALL_RECLAIM_S:
                            continue
                        solver._stall_reclaim = True
                        self._cancel_solver(solver)
                        t.cancel()
                        sid_live = (
                            getattr(solver, "solver_id", None) or "cli-?"
                        )
                        await _emit_bb(
                            "stall_reclaim",
                            worker=sid_live,
                            stalled_for_s=round(now_stall - float(stalled_at), 1),
                            intent_id=str(
                                task_intents.get(t)
                                or getattr(solver, "intent_id_assigned", "")
                                or getattr(solver, "_intent_id", "") or ""),
                        )
                except Exception:
                    pass

                # ── Round-16: solo-depth live verify/harvest (no cancel) ─────
                try:
                    from muteki.swarm.solo_depth_verify_v1 import (
                        enabled as _solo_on,
                        period_seconds as _solo_period,
                        run_live_verify_harvest as _solo_harvest,
                        should_run_verify_gate as _solo_should,
                    )
                    from muteki.swarm.fruitless_interrupt_v1 import (
                        collect_named_artifacts as _solo_arts,
                        worker_tool_count as _solo_tools,
                    )
                    if _solo_on():
                        now_sv = time.monotonic()
                        named_sv = _solo_arts(
                            self.shared_graph,
                            attachments=list(
                                getattr(self.challenge, "attachments", None)
                                or []
                            ),
                            workspace_root=getattr(
                                self, "workspace_root", None
                            ),
                            challenge=self.challenge,
                        )
                        for t, engine in list(tasks.items()):
                            if t.done() or t in self._active_review_tasks:
                                continue
                            solver = task_solvers.get(t)
                            if solver is None:
                                continue
                            tools_now = int(_solo_tools(solver))
                            last_t = float(
                                solo_verify_last_t.get(t, task_started_at.get(t, now_sv))
                            )
                            tools0 = int(solo_verify_tool_ckpt.get(t, 0))
                            if not _solo_should(
                                now_mono=now_sv,
                                last_verify_mono=last_t,
                                tools_now=tools_now,
                                tools_at_last_verify=tools0,
                                period_s=_solo_period(),
                            ):
                                continue
                            facts_before = self._verified_fact_count()
                            result = _solo_harvest(
                                solver,
                                self.shared_graph,
                                named_artifacts=named_sv,
                                actor=str(
                                    getattr(solver, "solver_id", None)
                                    or f"cli-{engine}"
                                ),
                            )
                            solo_verify_last_t[t] = now_sv
                            solo_verify_tool_ckpt[t] = tools_now
                            solo_verify_count += 1
                            harvested_n = int(result.get("harvested") or 0)
                            await _emit_bb(
                                "solo_depth_verify_harvest",
                                worker=str(
                                    getattr(solver, "solver_id", None)
                                    or f"cli-{engine}"
                                ),
                                harvested=harvested_n,
                                fact_seqs=list(result.get("fact_seqs") or [])[:8],
                                checks=list(result.get("checks") or [])[:8],
                                artifacts=list(result.get("artifacts") or [])[:8],
                                verify_index=solo_verify_count,
                                tools_now=tools_now,
                                facts_before=facts_before,
                                facts_after=self._verified_fact_count(),
                            )
                            # Graph growth wakes Reason on next loop without
                            # canceling the deep worker.
                            if harvested_n > 0:
                                last_fact_count = self._verified_fact_count()
                except Exception:
                    pass

                # ── (A) Reason — GRAPH-CHANGE trigger ────────────────────────
                # Reason fires when the graph changed since the last reason: a new
                # fact was confirmed, OR open intents were consumed to zero, OR the
                # swarm went fully idle (nothing running, nothing queued). Purely
                # graph/idle driven — there is no time-based "stalled" trigger
                # anymore. Reason is now the sole expansion engine, so it must keep
                # producing fresh directions; goal-hash intent dedup (reason.py)
                # stops it re-proposing the same batch.
                just_reaped = len(done) > 0
                # Framework end-of-tick hook (f01 receipt/surprise etc.). Default: no-op.
                if just_reaped:
                    fw_after = getattr(self, "framework_after_workers", None)
                    if callable(fw_after):
                        try:
                            await fw_after()
                        except Exception:
                            pass
                slots_free = self._ordinary_capacity_available(tasks)
                open_intents = self._open_intents()
                graph_grew = (last_fact_count > reason_fact_ckpt)
                intents_consumed = (reason_open_intent_ckpt > 0 and len(open_intents) == 0)

                need_reason = (
                    # graph-change driven (the real expansion engine):
                    (slots_free and (graph_grew or intents_consumed)) or
                    # a worker finished and there's nothing queued → plan next wave:
                    (just_reaped and slots_free and not open_intents) or
                    # genuinely empty (no work running, nothing queued) → re-plan:
                    (slots_free and not open_intents and len(tasks) == 0)
                )
                reason_state = (
                    self._verified_fact_count(),
                    tuple(self._found_flags),
                    tuple(sorted(str(it.get("intent_id") or "") for it in open_intents)),
                )
                if need_reason and reason_state == last_reason_state:
                    need_reason = False
                # Chain-completion (MUTEKI_CHAIN_COMPLETION=1): after a fruitless
                # worker, Reason was previously suppressed because reason_state
                # had not changed (still 0 facts / 0 flags). Force one replan and
                # inject a progress brief so the planner proposes a NEW follow-up.
                try:
                    from muteki.swarm.chain_completion_v1 import (
                        build_progress_brief,
                        recent_concluded_goals,
                        should_force_reason,
                    )
                    if should_force_reason(
                        just_reaped=just_reaped,
                        slots_free=slots_free,
                        graph_grew=graph_grew,
                        flag_count=len(self._found_flags),
                        need_reason_already=need_reason,
                        open_intents=len(open_intents),
                    ):
                        brief = build_progress_brief(
                            fact_count=self._verified_fact_count(),
                            flag_count=len(self._found_flags),
                            fruitless_workers=fruitless_workers,
                            open_intents=len(open_intents),
                            last_goals=recent_concluded_goals(self.shared_graph),
                        )
                        if brief not in self._standing_guidance:
                            self._standing_guidance.append(brief)
                            if len(self._standing_guidance) > 8:
                                self._standing_guidance = self._standing_guidance[-8:]
                        need_reason = True
                        await _emit_bb(
                            "chain_completion_force",
                            fruitless_workers=fruitless_workers,
                            fact_count=self._verified_fact_count(),
                        )
                except Exception:
                    pass
                # Fruitless-interrupt latch: after a mid-flight cancel is reaped,
                # force Reason and inject a short working packet (attempted goals /
                # dead-ends / do-not-repeat) into standing guidance so Reason
                # replans against a compressed board, not a bare force.
                if force_reason_after_fruitless_interrupt:
                    try:
                        from muteki.swarm.fruitless_interrupt_v1 import (
                            PACKET_PREFIX,
                            build_discriminating_constraint,
                            build_working_packet,
                            collect_named_artifacts,
                            extract_crypto_clues,
                            infer_replan_domain,
                            packet_meets_replan_quality,
                        )
                        meta = last_fruitless_interrupt_meta or {}
                        # Round-8/11/13: named attachments + domain-aware clues.
                        named_artifacts = collect_named_artifacts(
                            self.shared_graph,
                            attachments=list(
                                getattr(self.challenge, "attachments", None)
                                or []
                            ),
                            workspace_root=getattr(
                                self, "workspace_root", None
                            ),
                            challenge=self.challenge,
                        )
                        last_interrupt_named_artifacts = list(named_artifacts)
                        interrupt_empty_reason_retries = 0
                        harvest_rows = list(meta.get("harvest_rows") or [])
                        crypto_clues = extract_crypto_clues(
                            harvest_rows,
                            self.shared_graph,
                            limit=6,
                        )
                        challenge_category = str(
                            getattr(self.challenge, "category", "") or ""
                        )
                        replan_domain = infer_replan_domain(
                            category=challenge_category,
                            named_artifacts=named_artifacts,
                            harvest_rows=harvest_rows,
                            crypto_clues=crypto_clues,
                            fact_count=self._verified_fact_count(),
                        )
                        last_interrupt_replan_domain = replan_domain
                        packet = build_working_packet(
                            self.shared_graph,
                            fact_count=self._verified_fact_count(),
                            flag_count=len(self._found_flags),
                            fruitless_workers=fruitless_workers,
                            open_intents=len(open_intents),
                            interrupted_worker=str(meta.get("worker") or ""),
                            interrupted_goal=str(meta.get("goal") or ""),
                            running_for_s=float(
                                meta.get("running_for_s") or 0.0),
                            named_artifacts=named_artifacts,
                            crypto_clues=crypto_clues,
                            harvest_rows=harvest_rows,
                            category=challenge_category,
                        )
                        # Replace any prior interrupt packet so Reason sees one
                        # fresh working set instead of stacking rot.
                        self._standing_guidance = [
                            g for g in self._standing_guidance
                            if not str(g).startswith(PACKET_PREFIX)
                        ]
                        self._standing_guidance.append(packet)
                        if len(self._standing_guidance) > 8:
                            self._standing_guidance = self._standing_guidance[-8:]
                        await _emit_bb(
                            "fruitless_interrupt_working_packet",
                            chars=len(packet),
                            quality_ok=packet_meets_replan_quality(packet),
                            named_artifact_count=len(named_artifacts),
                            named_artifacts=named_artifacts[:8],
                            crypto_clue_count=len(crypto_clues),
                            crypto_clues=crypto_clues[:6],
                            replan_domain=replan_domain,
                            interrupt_count=fruitless_interrupt_count,
                            fact_count=self._verified_fact_count(),
                        )
                        # Round-7/11/13: MUST directive — domain-gated replan.
                        if self.shared_graph is not None:
                            constraint = build_discriminating_constraint(
                                interrupted_goal=str(meta.get("goal") or ""),
                                fact_count=self._verified_fact_count(),
                                named_artifacts=named_artifacts,
                                crypto_clues=crypto_clues,
                                harvest_rows=harvest_rows,
                                category=challenge_category,
                                domain=replan_domain,
                            )
                            try:
                                add_op = getattr(
                                    self.shared_graph,
                                    "add_operator_directive",
                                    None,
                                )
                                if callable(add_op):
                                    add_op(
                                        actor="coordinator",
                                        action="correction",
                                        text=constraint,
                                        scope="global",
                                        standing=False,
                                        preempt_policy="none",
                                        priority=100,
                                    )
                                else:
                                    self.shared_graph.add_coordinator_directive(
                                        actor="coordinator",
                                        action="correction",
                                        directive=constraint,
                                        priority="high",
                                    )
                                await _emit_bb(
                                    "fruitless_interrupt_reason_constraint",
                                    chars=len(constraint),
                                    fact_count=self._verified_fact_count(),
                                )
                            except Exception:
                                pass
                    except Exception:
                        pass
                    if not need_reason:
                        need_reason = True
                        await _emit_bb(
                            "fruitless_interrupt_force_reason",
                            fruitless_workers=fruitless_workers,
                            interrupt_count=fruitless_interrupt_count,
                            fact_count=self._verified_fact_count(),
                        )
                    # Regardless of whether Reason was already scheduled, the
                    # interrupt path owes a recovery actor if Reason yields nothing.
                    pending_interrupt_reason_recovery = True
                    force_reason_after_fruitless_interrupt = False
                    last_fruitless_interrupt_meta = {}
                if need_reason and self._reason_backpressure_active(open_intents):
                    await _emit_bb(
                        "reason_skipped",
                        trigger="queue_backpressure",
                        open_intents=len(open_intents),
                        ordinary_open_intents=self._ordinary_open_queue_depth(open_intents),
                        max_workers=self.max_workers,
                    )
                    need_reason = False
                reason_proposed_n = 0
                if need_reason:
                    trigger = ("graph" if (graph_grew or intents_consumed) else "idle")
                    await _emit_bb("reason_start", trigger=trigger)
                    last_reason_state = reason_state
                    n = await self._run_reason()
                    reason_proposed_n = int(n or 0)
                    # Round-9: interrupt-forced empty Reason must retry, not
                    # silently continue while a sibling burns to hard-cap.
                    if pending_interrupt_reason_recovery and reason_proposed_n == 0:
                        try:
                            from muteki.swarm.fruitless_interrupt_v1 import (
                                max_empty_reason_retries as _fi_empty_max,
                                should_retry_empty_reason as _fi_retry_empty,
                            )
                            while _fi_retry_empty(
                                pending_recovery=pending_interrupt_reason_recovery,
                                reason_proposed=reason_proposed_n,
                                retry_count=interrupt_empty_reason_retries,
                                max_retries=_fi_empty_max(),
                            ):
                                interrupt_empty_reason_retries += 1
                                await _emit_bb(
                                    "fruitless_interrupt_empty_reason_retry",
                                    retry_index=interrupt_empty_reason_retries,
                                    fact_count=self._verified_fact_count(),
                                    named_artifacts=list(
                                        last_interrupt_named_artifacts
                                    )[:8],
                                )
                                await _emit_bb(
                                    "reason_start",
                                    trigger="fruitless_interrupt_empty_retry",
                                )
                                n = await self._run_reason()
                                reason_proposed_n = int(n or 0)
                                if reason_proposed_n > 0:
                                    break
                        except Exception:
                            pass
                    if (
                        pending_interrupt_reason_recovery
                        and reason_proposed_n == 0
                    ):
                        try:
                            from muteki.swarm.fruitless_interrupt_v1 import (
                                build_artifact_chain_intent_goal as _fi_chain_goal,
                                should_inject_artifact_chain_intent as _fi_inject,
                            )
                            if _fi_inject(
                                pending_recovery=True,
                                reason_proposed=reason_proposed_n,
                                named_artifacts=last_interrupt_named_artifacts,
                                already_injected=interrupt_chain_intent_injected,
                            ) and self.shared_graph is not None:
                                chain_goal = _fi_chain_goal(
                                    last_interrupt_named_artifacts,
                                    domain=last_interrupt_replan_domain,
                                )
                                intent_id = (
                                    "intent:fruitless-interrupt-artifact-chain"
                                )
                                self.shared_graph.propose_intent(
                                    actor="coordinator",
                                    intent_id=intent_id,
                                    goal=chain_goal,
                                    payload={
                                        "worker_class": "code",
                                        "source": "fruitless_interrupt_chain",
                                        "priority": "high",
                                    },
                                )
                                interrupt_chain_intent_injected = True
                                reason_proposed_n = 1
                                await _emit_bb(
                                    "fruitless_interrupt_artifact_chain_intent",
                                    intent_id=intent_id,
                                    goal=chain_goal[:220],
                                    named_artifacts=list(
                                        last_interrupt_named_artifacts
                                    )[:8],
                                )
                                await _emit_bb(
                                    "intent_proposed",
                                    actor="coordinator",
                                    intent_id=intent_id,
                                    goal=chain_goal,
                                    worker_class="code",
                                )
                        except Exception:
                            pass
                    open_intents = self._open_intents()
                    # checkpoint the graph state we just reasoned over (A).
                    reason_fact_ckpt = last_fact_count
                    reason_open_intent_ckpt = len(open_intents)
                    # dropped_dup = intents Reason emitted that dispatch refused as
                    # duplicates (goal-hash exact + near-duplicate filter) — surfaced
                    # so a "planner is only re-proposing old directions" round is
                    # visible on the blackboard instead of silently shrinking.
                    _rr = getattr(self, "_last_reason", None)
                    rr_intents = len(getattr(_rr, "intents", []) or [])
                    dropped_dup = max(0, rr_intents - n)
                    await _emit_bb("reason_done", proposed=reason_proposed_n,
                                   dropped_dup=dropped_dup)
                    if (dropped_dup >= int(self.review_policy.get("after_duplicate_intents") or 0)
                            and await self._maybe_start_review(
                                trigger="duplicate_intents",
                                directive=f"Reason dropped {dropped_dup} duplicate intent(s); audit route loop and suppress repeated directions.",
                                healthy=healthy, tasks=tasks,
                                task_solvers=task_solvers, emit_bb=_emit_bb)):
                        continue

                    # ── pentest stop: P1 gated findings / P2 coverage.
                    # Reason verdict=complete is a planning signal only. Product
                    # success is finding_ok, not the planner's say-so. The eval
                    # bypass (pentest_flag_required) still salvages flags.
                    rr = getattr(self, "_last_reason", None)
                    if getattr(self.challenge, "mode", "ctf") == "pentest":
                        await self._drain_report_pipeline()
                        self._sync_findings_from_graph()
                        if self._findings_complete():
                            goal_complete = True
                            await _emit_bb(
                                "goal_complete",
                                why="gated_reports",
                                reports=len(self._found_reports),
                                findings=len(self._found_reports))
                            for other in tasks:
                                self._cancel_solver(task_solvers.get(other))
                                other.cancel()
                            break
                        if self._coverage_complete():
                            self._coverage_exhausted = True
                            await _emit_bb(
                                "coverage_complete",
                                findings=len(self._found_findings))
                            for other in tasks:
                                self._cancel_solver(task_solvers.get(other))
                                other.cancel()
                            break
                        if rr is not None and getattr(rr, "verdict", "") == "complete":
                            complete_why = getattr(rr, "complete_why", "")[:300]
                            if self._pentest_flag_required():
                                if not self._flags_complete():
                                    salvaged = await self._salvage_flags_from_evidence(
                                        complete_why=complete_why)
                                    if salvaged:
                                        await _emit_bb("flag_salvaged",
                                                       flags=[f[:80] for f in salvaged])
                                if self._flags_complete():
                                    goal_complete = True
                                    await _emit_bb("goal_complete",
                                                   why=complete_why,
                                                   flags=len(self._found_flags))
                                    for other in tasks:
                                        self._cancel_solver(task_solvers.get(other))
                                        other.cancel()
                                    break
                                await _emit_bb(
                                    "goal_complete_rejected",
                                    why=complete_why,
                                    reason="verdict=complete but no accepted flag in store; "
                                           "continuing until a provenance-admitted flag lands")
                            else:
                                await _emit_bb(
                                    "goal_complete_rejected",
                                    why=complete_why,
                                    reason="verdict=complete is a planning signal; "
                                           "success requires a gated finding")
                    if (getattr(self.challenge, "mode", "ctf") != "pentest"
                            and rr is not None
                            and getattr(rr, "verdict", "") == "complete"
                            and self._flags_complete()):
                        if winner is None:
                            winner = "coordinator"
                            flag = self._found_flags[0] if self._found_flags else None
                        await _emit_bb(
                            "goal_complete",
                            why=getattr(rr, "complete_why", "")[:300],
                            flags=len(self._found_flags))
                        for other in tasks:
                            self._cancel_solver(task_solvers.get(other))
                            other.cancel()
                        break

                    # ── Phase 7: adaptive re-bootstrap ──────────────────────
                    # If Reason says the run DRIFTED (course_correct), a fresh
                    # whole-challenge rush from the corrected direction often
                    # beats stepping through narrow Explores. Spawn ONE bootstrap
                    # worker seeded with the drift, if a slot is free. (Bootstrap is
                    # not a one-time phase: it can re-fire on a course correction.)
                    reason_res = getattr(self, "_last_reason", None)
                    drift = getattr(reason_res, "drift", "") if reason_res else ""
                    verdict = getattr(reason_res, "verdict", "") if reason_res else ""
                    if verdict == "course_correct" and drift:
                        if (self.review_policy.get("on_course_correct", True)
                                and await self._maybe_start_review(
                                    trigger="course_correct", directive=drift,
                                    healthy=healthy, tasks=tasks,
                                    task_solvers=task_solvers, emit_bb=_emit_bb)):
                            continue
                        if not self._ordinary_capacity_available(tasks):
                            continue
                        try:
                            engine = self._pick_engine(_running_engines(), healthy, role="bootstrap")
                        except RuntimeError as exc:
                            await _emit_bb("worker_spawn_rejected", reason=str(exc),
                                           phase="rebootstrap")
                            continue
                        try:
                            w = self._make_cli_worker(
                                engine, mode="bootstrap", intent_goal=drift)
                        except WorkerSpawnRejected as exc:
                            await _emit_bb("worker_spawn_rejected", reason=str(exc),
                                           engine=str(engine), phase="rebootstrap")
                            break
                        except WorkerBudgetExhausted as exc:
                            await _stop_for_budget(str(exc))
                            break
                        t = await self._schedule_control_worker(
                            w, name=f"rebootstrap-{engine}")
                        tasks[t] = engine
                        task_solvers[t] = w
                        await _emit_bb("worker_spawned", worker=w.solver_id,
                                       phase="rebootstrap", worker_role="worker",
                                       **worker_identity_event_fields(w))

                # ── Phase: Explore — fill free slots with intent workers ─────
                if getattr(self.challenge, "mode", "ctf") == "pentest":
                    await self._drain_report_pipeline()
                    open_intents = self._open_intents()
                # Spawn at most `explore_spawn_batch` (default 1) per loop iteration,
                # NOT a whole burst that fills every free slot at once. The poll
                # interval (~2s) means a slot refills within ~2s anyway, so the swarm
                # ramps smoothly instead of launching 10 workers in one tick that then
                # share a fate (run-7352: a 10-worker burst that all died together,
                # and 10 concurrent connections to one target tripped its rate limit).
                open_intents = self._dispatchable_open_intents(open_intents)
                open_intents = self._capacity_dispatchable_open_intents(open_intents, tasks)
                # Cognitive cluster planner: reorder open intents so the next
                # explore worker takes the highest-evidence-value direction, not
                # FIFO creation order. Default-off (see Swarm.cognitive_cluster_planner).
                if getattr(self, "cognitive_cluster_planner", False) and open_intents:
                    try:
                        from muteki.swarm.cognitive_cluster_planner import plan_dispatch

                        open_intents = plan_dispatch(
                            open_intents,
                            shared_graph=self.shared_graph,
                            running_engines=_running_engines(),
                        )
                    except Exception:
                        pass
                spawned_this_round = 0
                batch_engines: list[str] = []
                while open_intents and spawned_this_round < self.explore_spawn_batch:
                    intent = open_intents.pop(0)
                    iid = intent["intent_id"]
                    worker_class = str(intent.get("worker_class") or "code")
                    if worker_class == "review":
                        worker_mode = "review"
                        worker_role = "review"
                    elif worker_class == "verifier":
                        worker_mode = "verifier"
                        worker_role = "verifier"
                    else:
                        worker_mode = "explore"
                        worker_role = "explore"
                    intent_lane = str(intent.get("lane_key") or "")
                    try:
                        engine = self._pick_engine(
                            _running_engines(), healthy, role=worker_role,
                            intent_id=iid, lane=intent_lane,
                            intent=intent, avoid_engines=batch_engines)
                    except RuntimeError as exc:
                        await _emit_bb("worker_spawn_rejected", reason=str(exc),
                                       phase=worker_mode, intent_id=iid)
                        open_intents.insert(0, intent)
                        break
                    # build the worker FIRST so we can claim the intent under ITS
                    # unique solver_id — that makes the worker the intent's OWNER, so
                    # conclude_intent's owner-fence lets exactly this worker (not a
                    # later re-spawn that took over an expired lease) conclude it.
                    try:
                        worker_kwargs = {
                            "mode": worker_mode,
                            "intent_goal": intent["goal"],
                            "intent_id": iid,
                        }
                        if intent_lane:
                            worker_kwargs["lane"] = intent_lane
                        w = self._make_cli_worker(engine, **worker_kwargs)
                    except RequiredContextUnavailable as exc:
                        # Reconcile again at the live failure boundary.  TTL expiry,
                        # explicit EXPIRE_CONTEXT, or an already bound/unknown exact
                        # resource is terminal and must close its graph edge now;
                        # only an ACTIVE resource with a transient provider/resolver
                        # failure remains deferred.
                        await self._reconcile_control_continuations()
                        try:
                            still_open = any(
                                str(row.get("intent_id") or "") == iid
                                for row in self._open_intents())
                        except Exception:
                            still_open = True
                        await _emit_bb(
                            "worker_spawn_rejected", reason=str(exc),
                            engine=str(engine), phase=worker_mode,
                            intent_id=iid, deferred=still_open,
                            retired=not still_open)
                        continue
                    except WorkerSpawnRejected as exc:
                        # intent not claimed yet (claim happens after build) → just
                        # skip this spawn; the intent stays open for a later worker.
                        await _emit_bb("worker_spawn_rejected", reason=str(exc),
                                       engine=str(engine), phase=worker_mode)
                        break
                    except WorkerBudgetExhausted as exc:
                        await _stop_for_budget(str(exc))
                        break
                    # atomic claim (guards double-claim). Lease MUST outlast the
                    # explore worker's own per-turn timeout (+margin for conclude) —
                    # else a still-running worker's lease lapses, _open_intents
                    # re-dispatches the intent, and its later conclusion is fenced out.
                    won = False
                    try:
                        won = self.shared_graph.claim_intent(
                            worker=w.solver_id, intent_id=iid,
                            lease_s=float(self.explore_timeout) + 300.0)
                    except Exception:
                        won = False
                    if not won:
                        if not await self._retire_worker_account(
                                w, reason="intent claim not acquired"):
                            raise ControlShutdownIncomplete(
                                "claim-lost worker rollback incomplete")
                        continue  # someone else holds a live claim; drop this worker
                    lane_key = str(intent.get("lane_key") or "")
                    locked_lane = ""
                    if (lane_key and worker_mode != "review"
                            and self.shared_graph is not None):
                        try:
                            lock = self.shared_graph.lock_lane(  # type: ignore[attr-defined]
                                actor="coordinator",
                                lane_key=lane_key,
                                risk_class=str(intent.get("risk_class") or ""),
                                owner_worker=w.solver_id,
                                owner_intent=iid,
                                lease_s=float(self.explore_timeout) + 300.0,
                            )
                        except Exception:
                            lock = {"acquired": False, "held_seq": 0}
                        if not lock.get("acquired"):
                            try:
                                self.shared_graph.defer_intent_for_lane(  # type: ignore[attr-defined]
                                    actor="coordinator",
                                    intent_id=iid,
                                    lane_key=lane_key,
                                    against_locked_seq=int(lock.get("held_seq") or 0),
                                )
                                await _emit_bb(
                                    "intent_lane_deferred",
                                    intent_id=iid,
                                    lane_key=lane_key,
                                    held_by=str(lock.get("held_by") or ""),
                                    held_seq=int(lock.get("held_seq") or 0),
                                )
                            except Exception:
                                pass
                            if not await self._retire_worker_account(
                                    w, intent_id=iid,
                                    reason="intent deferred behind live lane"):
                                raise ControlShutdownIncomplete(
                                    "lane-deferred worker rollback incomplete")
                            continue
                        locked_lane = str(lock.get("lane_key") or lane_key)
                        try:
                            self.shared_graph.add_coordinator_directive(  # type: ignore[attr-defined]
                                actor="coordinator",
                                action="lane_lock",
                                directive=(
                                    f"lane {locked_lane} is exclusively held by {w.solver_id}; "
                                    "do not start destructive/exclusive work on that resource."
                                ),
                                priority="high",
                            )
                        except Exception:
                            pass
                        await _emit_bb("lane_locked", **lock, intent_id=iid)
                    t = await self._schedule_control_worker(
                        w, name=f"{worker_mode}-{engine}",
                        intent_id=iid, lane_key=locked_lane)
                    tasks[t] = engine
                    task_solvers[t] = w
                    task_intents[t] = iid
                    if locked_lane:
                        task_lanes[t] = locked_lane
                    if worker_mode == "review":
                        self._active_review_tasks.add(t)
                        self._review_workers_spawned += 1
                    elif worker_mode == "verifier":
                        self._active_verifier_tasks.add(t)
                        self._verifier_workers_spawned += 1
                    spawned_this_round += 1
                    batch_engines.append(str(engine))
                    await _emit_bb("worker_spawned", worker=w.solver_id,
                                   phase=worker_mode, intent_id=iid,
                                   worker_role=(
                                       "review" if worker_mode == "review"
                                       else "verifier" if worker_mode == "verifier"
                                       else "worker"),
                                   **worker_identity_event_fields(w))
                    open_intents = self._capacity_dispatchable_open_intents(open_intents, tasks)

                # ── bounded liveness: a dry planner on an unchanged evidence state
                # is NEEDS_NEW_INFORMATION, not permission to clone another whole-
                # challenge worker forever.  The pause above can be woken by strong
                # information or an operator command, then grants one bounded retry.
                #
                # Round-4 exception (MUTEKI_FRUITLESS_INTERRUPT): after we killed a
                # worker mid-flight and forced Reason, a planner ConnectTimeout /
                # empty plan must NOT park in collect_idle — spawn one bounded
                # re-bootstrap so the run keeps a live actor.
                open_intents = self._open_intents()
                # Round-9: only clear recovery once work exists. Do NOT drop the
                # latch just because a sibling task is still burning after an
                # empty Reason — that was the proposed=0 silent-stall.
                if pending_interrupt_reason_recovery and (
                    reason_proposed_n > 0 or open_intents
                ):
                    pending_interrupt_reason_recovery = False
                if not tasks and not open_intents:
                    try:
                        from muteki.swarm.fruitless_interrupt_v1 import (
                            max_reboots as _fi_max_reboots,
                            reason_failure_kind as _fi_fail_kind,
                            should_rebootstrap_after_reason as _fi_should_reboot,
                        )
                        failure = getattr(self, "_last_planner_failure", None)
                        if _fi_should_reboot(
                            pending_recovery=pending_interrupt_reason_recovery,
                            tasks_empty=True,
                            open_intents=0,
                            reason_proposed=reason_proposed_n,
                            planner_failure_kind=_fi_fail_kind(failure),
                            rebootstrap_count=interrupt_rebootstrap_count,
                            max_reboots_n=_fi_max_reboots(),
                        ):
                            if not self._ordinary_capacity_available(tasks):
                                pending_interrupt_reason_recovery = False
                            else:
                                try:
                                    engine = self._pick_engine(
                                        _running_engines(), healthy,
                                        role="bootstrap")
                                except RuntimeError as exc:
                                    await _emit_bb(
                                        "worker_spawn_rejected",
                                        reason=str(exc),
                                        phase="fruitless_interrupt_rebootstrap",
                                    )
                                    pending_interrupt_reason_recovery = False
                                    engine = None
                                if engine is not None:
                                    try:
                                        w = self._make_cli_worker(
                                            engine, mode="bootstrap",
                                            intent_goal=self._retry_goal())
                                        t = await self._schedule_control_worker(
                                            w,
                                            name=(
                                                "fruitless-interrupt-rebootstrap-"
                                                f"{engine}"
                                            ),
                                        )
                                        tasks[t] = engine
                                        task_solvers[t] = w
                                        interrupt_rebootstrap_count += 1
                                        pending_interrupt_reason_recovery = False
                                        needs_new_information = False
                                        await _emit_bb(
                                            "fruitless_interrupt_rebootstrap",
                                            worker=w.solver_id,
                                            engine=str(engine),
                                            rebootstrap_index=interrupt_rebootstrap_count,
                                            planner_failure=_fi_fail_kind(failure),
                                            reason_proposed=reason_proposed_n,
                                            detail=str(
                                                getattr(failure, "detail", "")
                                                or ""
                                            )[:300],
                                        )
                                        await _emit_bb(
                                            "worker_spawned",
                                            worker=w.solver_id,
                                            phase="fruitless_interrupt_rebootstrap",
                                            worker_role="worker",
                                            **worker_identity_event_fields(w),
                                        )
                                        continue
                                    except WorkerSpawnRejected as exc:
                                        await _emit_bb(
                                            "worker_spawn_rejected",
                                            reason=str(exc),
                                            engine=str(engine),
                                            phase="fruitless_interrupt_rebootstrap",
                                        )
                                        pending_interrupt_reason_recovery = False
                                    except WorkerBudgetExhausted as exc:
                                        await _stop_for_budget(str(exc))
                                        break
                    except Exception:
                        pass
                    needs_new_information = True
                    failure = getattr(self, "_last_planner_failure", None)
                    await _emit_bb(
                        "needs_new_information",
                        planner_failure=str(getattr(failure, "kind", "empty_plan")),
                        detail=str(getattr(failure, "detail", ""))[:300],
                    )
                    continue
        finally:
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
                    await _emit_bb(
                        "operator_resumed",
                        reason="coordinator epoch ended; dispatcher latch retired",
                    )
                except Exception:
                    pass
            if self._operator_draining:
                self._operator_draining = False
                try:
                    await _emit_bb(
                        "operator_drain_completed",
                        reason="coordinator epoch ended after draining in-flight work",
                    )
                except Exception:
                    pass
            leftover = [t for t in tasks if not t.done()]
            for t in leftover:
                self._cancel_solver(task_solvers.get(t))
                t.cancel()
            if leftover:
                await asyncio.gather(*leftover, return_exceptions=True)
            # The normal reap path releases profile/account/registry ownership, but
            # cancel/error exits can jump straight here with live WorkerRefs still
            # published. Release every constructed solver idempotently so a later
            # resolve never targets stale runtime objects.
            released_ids: set[str] = set()
            for task, solver in task_solvers.items():
                sid = str(getattr(solver, "solver_id", "") or "")
                if not sid or sid in released_ids:
                    continue
                await self._retire_worker_account(
                    solver, intent_id=str(
                        task_intents.get(task)
                        or getattr(solver, "intent_id_assigned", "")
                        or getattr(solver, "_intent_id", "") or ""),
                    reason="coordinator shutdown",
                    lane_key=str(task_lanes.get(task) or ""),
                )
                released_ids.add(sid)
            if task_lanes and self.shared_graph is not None:
                for t, lane_key in list(task_lanes.items()):
                    solver = task_solvers.get(t)
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
                        await self._consume_lane_release(rel, emit_bb=_emit_bb)
                    except Exception:
                        pass
                    task_lanes.pop(t, None)
            if hitl_task is not None:
                hitl_task.cancel()
                await asyncio.gather(hitl_task, return_exceptions=True)
            if self._shutdown_owners_incomplete():
                # Do not close the graph or emit a false terminal lifecycle while a
                # fenced handler still owns an in-flight mutation. The orphan set on
                # the Swarm is the retained ownership record for diagnostics/reap.
                self._retain_control_shutdown_owner(
                    winner=winner, flag=flag, goal_complete=goal_complete,
                    per_solver=per_solver)
                raise ControlShutdownIncomplete(
                    "control shutdown incomplete; runtime owner retained")
            # M11: if we are leaving via cancel/exception, finalize HERE so the shared
            # graph handle is closed and worker scratch is swept even on the error path
            # (the post-finally finalize below only runs on a clean return). Idempotent.
            try:
                await self._finalize_coordinator_run(
                    winner=winner, flag=flag, goal_complete=goal_complete,
                    per_solver=per_solver)
            except Exception:
                pass

        # M11: finalize (persist winner + close graph + RUN_FINISHED + scratch sweep).
        # Idempotent — the finally above already called this on a cancel/error exit, so
        # the normal path is a no-op here; on a clean return THIS is the call that runs.
        await self._finalize_coordinator_run(
            winner=winner, flag=flag, goal_complete=goal_complete, per_solver=per_solver)
        if winner is not None:
            return SwarmOutcome(True, flag, winner, per_solver, "solved",
                                flags=list(self._found_flags))
        if goal_complete:
            return SwarmOutcome(True,
                                self._found_flags[0] if self._found_flags else None,
                                None, per_solver, "goal_met",
                                flags=list(self._found_flags))
        if self._coverage_exhausted:
            return SwarmOutcome(False, None, None, per_solver, "coverage_complete")
        if self._budget_exhausted_kind:
            return SwarmOutcome(False, None, None, per_solver,
                                "budget_exhausted")
        return SwarmOutcome(False, None, None, per_solver,
                            "coordinator: no verified flag")

    def config_poll_interval(self) -> float:
        """How long asyncio.wait blocks before re-checking stall/intents. Short
        enough to be responsive, long enough not to busy-spin."""
        return 2.0

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
            trusted_payload = {
                "engine": getattr(outcome, "engine", "") or "",
                "worker_id": str(worker_id or ""),
                "session": session,
                "workdir": workdir,
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
