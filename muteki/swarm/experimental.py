"""Explicit opt-in experiment harness (stage-4b extraction).

The production ``Swarm`` never imports the five experiment modules
(``fruitless_interrupt_v1``, ``solo_depth_verify_v1``, ``chain_completion_v1``,
``context_firewall_v1``, ``cognitive_cluster_planner``): their per-tick bodies
were extracted from the coordinator pipeline into the hook functions below and
are reached only through the base ``_experiment_stage`` / ``_experiment_call``
dispatchers, which no-op when no hook is registered.

Research arms opt in via ``swarm_class="muteki.swarm.experimental:ExperimentalSwarm"``
(the ``drivers.py`` ``_resolve_swarm_class`` allowlist already covers
``muteki.swarm.``). Each hook still consults the experiment modules' own
``enabled()`` env gates, so env-arming semantics are unchanged for research.
"""

from __future__ import annotations

import asyncio
import time
from functools import partial
from typing import Any

from muteki.solver.worker_profiles import worker_identity_event_fields
from muteki.swarm.chain_completion_v1 import (
    build_progress_brief,
    recent_concluded_goals,
    should_force_reason,
)
from muteki.swarm.cognitive_cluster_planner import (
    ClusterEvidence,
    plan_dispatch,
    planner_enabled_from_env,
    select_engine,
)
from muteki.swarm.context_firewall_v1 import (
    enabled as _cf_on,
    fold_reason_context as _cf_fold,
)
from muteki.swarm.coordinator_state import (
    emit_scheduler_bb,
    running_engines as scheduler_running_engines,
    stop_for_budget as scheduler_stop_for_budget,
)
from muteki.swarm.fruitless_interrupt_v1 import (
    PACKET_PREFIX,
    artifact_extra_seconds as _fi_artifact_extra,
    build_artifact_chain_intent_goal as _fi_chain_goal,
    build_discriminating_constraint,
    build_working_packet,
    collect_named_artifacts as _fi_collect_arts,
    commit_harvested_facts as _fi_commit,
    enabled as _fi_enabled,
    extract_crypto_clues,
    hard_cap_seconds as _fi_hard_cap,
    harvest_artifact_tool_facts as _fi_harvest,
    infer_replan_domain,
    max_empty_reason_retries as _fi_empty_max,
    max_interrupts as _fi_max,
    max_reboots as _fi_max_reboots,
    packet_meets_replan_quality,
    reason_failure_kind as _fi_fail_kind,
    settle_seconds as _fi_settle_wait,
    should_inject_artifact_chain_intent as _fi_inject,
    should_interrupt_worker as _fi_should,
    should_rebootstrap_after_reason as _fi_should_reboot,
    should_retry_empty_reason as _fi_retry_empty,
    should_soft_continue_after_retire_miss as _fi_soft_continue,
    sole_extra_seconds as _fi_sole_extra,
    threshold_seconds as _fi_threshold,
    tool_stall_seconds as _fi_tool_stall,
    worker_artifact_progress as _fi_art_prog,
    worker_tool_count as _fi_tool_count,
)
from muteki.swarm.solo_depth_verify_v1 import (
    enabled as _solo_enabled,
    max_ordinary_workers as _solo_cap,
    period_seconds as _solo_period,
    run_live_verify_harvest as _solo_harvest,
    should_run_verify_gate as _solo_should,
)
from muteki.swarm.swarm import Swarm
from muteki.swarm.swarm_support import (
    WorkerBudgetExhausted,
    WorkerSpawnRejected,
)


async def fruitless_interrupt_hook(swarm, state) -> str:
    """Extracted ``fruitless_interrupt_stage`` body (the production
    ``sync_worker_start_marks`` bookkeeping stayed in the base pipeline)."""
    emit_bb = partial(emit_scheduler_bb, swarm, state)
    try:
        interrupted_this_round: list[asyncio.Task] = []
        # Round-16: solo-depth architecture disables cancel/replan
        # interrupts — verify gate below owns progress folding.
        try:
            _solo_depth_on = bool(_solo_enabled())
        except Exception:
            _solo_depth_on = False
        if (
            (not _solo_depth_on)
            and _fi_enabled()
            and state.fruitless_interrupt_count < _fi_max()
        ):
            now_m = time.monotonic()
            facts_now = swarm._verified_fact_count()
            flags_now = len(swarm._found_flags)
            thr = _fi_threshold()
            stall_s = _fi_tool_stall()
            sole_s = _fi_sole_extra()
            cap_s = _fi_hard_cap()
            art_extra_s = _fi_artifact_extra()
            named_arts = _fi_collect_arts(
                swarm.shared_graph,
                attachments=list(
                    getattr(swarm.challenge, "attachments", None)
                    or []
                ),
                workspace_root=getattr(
                    swarm, "workspace_root", None
                ),
                challenge=swarm.challenge,
            )
            ordinary_n = swarm._ordinary_task_count(state.tasks)
            for t, engine in list(state.tasks.items()):
                if state.fruitless_interrupt_count >= _fi_max():
                    break
                if t.done() or t in state.fruitless_interrupt_tasks:
                    continue
                if t in swarm._active_review_tasks:
                    continue
                solver = state.task_solvers.get(t)
                sid_live = (
                    getattr(solver, "solver_id", None)
                    or f"cli-{engine}"
                )
                started = state.task_started_at.get(t, now_m)
                f0, g0 = state.task_prog_ckpt.get(
                    t, (facts_now, flags_now))
                w_mode = str(
                    getattr(solver, "mode", None) or "bootstrap"
                )
                tools_now = _fi_tool_count(solver)
                prev_tools = state.task_tool_count.get(t, 0)
                if tools_now > prev_tools:
                    state.task_tool_count[t] = tools_now
                    state.task_last_tool_t[t] = now_m
                elif t not in state.task_last_tool_t:
                    # No tool mark yet — clock stall from start.
                    state.task_tool_count.setdefault(t, tools_now)
                    state.task_last_tool_t[t] = started
                since_tool = now_m - state.task_last_tool_t.get(t, started)
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
                    rows = _fi_harvest(solver, named_arts)
                    harvest_rows = list(rows or [])
                    if rows and swarm.shared_graph is not None:
                        seqs = _fi_commit(
                            swarm.shared_graph,
                            actor=str(sid_live),
                            rows=rows,
                        )
                        harvested_n = len(seqs)
                        if harvested_n:
                            clues = extract_crypto_clues(
                                rows, swarm.shared_graph, limit=6
                            )
                            await emit_bb(
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
                delivered = swarm._cancel_solver(solver)
                t.cancel()
                state.fruitless_interrupt_tasks.add(t)
                interrupted_this_round.append(t)
                state.fruitless_interrupt_count += 1
                intent_goal = str(
                    getattr(solver, "intent_goal", None)
                    or getattr(solver, "_intent_goal", None)
                    or ""
                )
                state.last_fruitless_interrupt_meta = {
                    "worker": sid_live,
                    "goal": intent_goal,
                    "running_for_s": round(now_m - started, 1),
                    "worker_mode": w_mode,
                    "harvested_facts": harvested_n,
                    "harvest_rows": harvest_rows,
                }
                await emit_bb(
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
                    interrupt_index=state.fruitless_interrupt_count,
                )
        # Round-6: after cancel, wait briefly for wrapper done then
        # loop so the reap path (worker_finished + Reason) runs
        # before more explore spawns pile on.
        if interrupted_this_round:
            try:
                wait_s = min(5.0, _fi_settle_wait())
            except Exception:
                wait_s = 5.0
            pending_victims = {
                t for t in interrupted_this_round if not t.done()
            }
            if pending_victims:
                await asyncio.wait(
                    pending_victims, timeout=wait_s)
            return "continue"
    except Exception:
        pass
    return "proceed"


async def solo_depth_maintenance_hook(swarm, state) -> str:
    """Extracted solo-depth live verify/harvest block from
    ``worker_maintenance_stage`` (the stall_reclaim loop stayed in base)."""
    emit_bb = partial(emit_scheduler_bb, swarm, state)
    # ── Round-16: solo-depth live verify/harvest (no cancel) ─────
    try:
        if _solo_enabled():
            now_sv = time.monotonic()
            named_sv = _fi_collect_arts(
                swarm.shared_graph,
                attachments=list(
                    getattr(swarm.challenge, "attachments", None)
                    or []
                ),
                workspace_root=getattr(
                    swarm, "workspace_root", None
                ),
                challenge=swarm.challenge,
            )
            for t, engine in list(state.tasks.items()):
                if t.done() or t in swarm._active_review_tasks:
                    continue
                solver = state.task_solvers.get(t)
                if solver is None:
                    continue
                tools_now = int(_fi_tool_count(solver))
                last_t = float(
                    state.solo_verify_last_t.get(t, state.task_started_at.get(t, now_sv))
                )
                tools0 = int(state.solo_verify_tool_ckpt.get(t, 0))
                if not _solo_should(
                    now_mono=now_sv,
                    last_verify_mono=last_t,
                    tools_now=tools_now,
                    tools_at_last_verify=tools0,
                    period_s=_solo_period(),
                ):
                    continue
                facts_before = swarm._verified_fact_count()
                result = _solo_harvest(
                    solver,
                    swarm.shared_graph,
                    named_artifacts=named_sv,
                    actor=str(
                        getattr(solver, "solver_id", None)
                        or f"cli-{engine}"
                    ),
                )
                state.solo_verify_last_t[t] = now_sv
                state.solo_verify_tool_ckpt[t] = tools_now
                state.solo_verify_count += 1
                harvested_n = int(result.get("harvested") or 0)
                await emit_bb(
                    "solo_depth_verify_harvest",
                    worker=str(
                        getattr(solver, "solver_id", None)
                        or f"cli-{engine}"
                    ),
                    harvested=harvested_n,
                    fact_seqs=list(result.get("fact_seqs") or [])[:8],
                    checks=list(result.get("checks") or [])[:8],
                    artifacts=list(result.get("artifacts") or [])[:8],
                    verify_index=state.solo_verify_count,
                    tools_now=tools_now,
                    facts_before=facts_before,
                    facts_after=swarm._verified_fact_count(),
                )
                # Graph growth wakes Reason on next loop without
                # canceling the deep worker.
                if harvested_n > 0:
                    state.last_fact_count = swarm._verified_fact_count()
    except Exception:
        pass
    return "proceed"


async def reason_trigger_force_hook(swarm, state) -> str:
    """Extracted chain-completion force-reason block from
    ``reason_trigger_stage``."""
    emit_bb = partial(emit_scheduler_bb, swarm, state)
    # Chain-completion (MUTEKI_CHAIN_COMPLETION=1): after a fruitless
    # worker, force one replan and
    # inject a progress brief so the planner proposes a NEW follow-up.
    try:
        just_reaped = len(state.done) > 0
        slots_free = swarm._ordinary_capacity_available(state.tasks)
        if should_force_reason(
            just_reaped=just_reaped,
            slots_free=slots_free,
            graph_grew=state.graph_grew,
            flag_count=len(swarm._found_flags),
            need_reason_already=state.need_reason,
            open_intents=len(state.open_intents),
        ):
            brief = build_progress_brief(
                fact_count=swarm._verified_fact_count(),
                flag_count=len(swarm._found_flags),
                fruitless_workers=state.fruitless_workers,
                open_intents=len(state.open_intents),
                last_goals=recent_concluded_goals(swarm.shared_graph),
            )
            if brief not in swarm._standing_guidance:
                swarm._standing_guidance.append(brief)
                if len(swarm._standing_guidance) > 8:
                    swarm._standing_guidance = swarm._standing_guidance[-8:]
            state.need_reason = True
            await emit_bb(
                "chain_completion_force",
                fruitless_workers=state.fruitless_workers,
                fact_count=swarm._verified_fact_count(),
            )
    except Exception:
        pass
    return "proceed"


async def reason_trigger_interrupt_packet_hook(swarm, state) -> str:
    """Extracted fruitless-interrupt latch block from ``reason_trigger_stage``
    (working packet + MUST directive + force reason)."""
    emit_bb = partial(emit_scheduler_bb, swarm, state)
    # Fruitless-interrupt latch: after a mid-flight cancel is reaped,
    # force Reason and inject a short working packet (attempted goals /
    # dead-ends / do-not-repeat) into standing guidance so Reason
    # replans against a compressed board, not a bare force.
    try:
        meta = state.last_fruitless_interrupt_meta or {}
        # Round-8/11/13: named attachments + domain-aware clues.
        named_artifacts = _fi_collect_arts(
            swarm.shared_graph,
            attachments=list(
                getattr(swarm.challenge, "attachments", None)
                or []
            ),
            workspace_root=getattr(
                swarm, "workspace_root", None
            ),
            challenge=swarm.challenge,
        )
        state.last_interrupt_named_artifacts = list(named_artifacts)
        state.interrupt_empty_reason_retries = 0
        harvest_rows = list(meta.get("harvest_rows") or [])
        crypto_clues = extract_crypto_clues(
            harvest_rows,
            swarm.shared_graph,
            limit=6,
        )
        challenge_category = str(
            getattr(swarm.challenge, "category", "") or ""
        )
        replan_domain = infer_replan_domain(
            category=challenge_category,
            named_artifacts=named_artifacts,
            harvest_rows=harvest_rows,
            crypto_clues=crypto_clues,
            fact_count=swarm._verified_fact_count(),
        )
        state.last_interrupt_replan_domain = replan_domain
        packet = build_working_packet(
            swarm.shared_graph,
            fact_count=swarm._verified_fact_count(),
            flag_count=len(swarm._found_flags),
            fruitless_workers=state.fruitless_workers,
            open_intents=len(state.open_intents),
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
        swarm._standing_guidance = [
            g for g in swarm._standing_guidance
            if not str(g).startswith(PACKET_PREFIX)
        ]
        swarm._standing_guidance.append(packet)
        if len(swarm._standing_guidance) > 8:
            swarm._standing_guidance = swarm._standing_guidance[-8:]
        await emit_bb(
            "fruitless_interrupt_working_packet",
            chars=len(packet),
            quality_ok=packet_meets_replan_quality(packet),
            named_artifact_count=len(named_artifacts),
            named_artifacts=named_artifacts[:8],
            crypto_clue_count=len(crypto_clues),
            crypto_clues=crypto_clues[:6],
            replan_domain=replan_domain,
            interrupt_count=state.fruitless_interrupt_count,
            fact_count=swarm._verified_fact_count(),
        )
        # Round-7/11/13: MUST directive — domain-gated replan.
        if swarm.shared_graph is not None:
            constraint = build_discriminating_constraint(
                interrupted_goal=str(meta.get("goal") or ""),
                fact_count=swarm._verified_fact_count(),
                named_artifacts=named_artifacts,
                crypto_clues=crypto_clues,
                harvest_rows=harvest_rows,
                category=challenge_category,
                domain=replan_domain,
            )
            try:
                add_op = getattr(
                    swarm.shared_graph,
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
                    swarm.shared_graph.add_coordinator_directive(
                        actor="coordinator",
                        action="correction",
                        directive=constraint,
                        priority="high",
                    )
                await emit_bb(
                    "fruitless_interrupt_reason_constraint",
                    chars=len(constraint),
                    fact_count=swarm._verified_fact_count(),
                )
            except Exception:
                pass
    except Exception:
        pass
    if not state.need_reason:
        state.need_reason = True
        await emit_bb(
            "fruitless_interrupt_force_reason",
            fruitless_workers=state.fruitless_workers,
            interrupt_count=state.fruitless_interrupt_count,
            fact_count=swarm._verified_fact_count(),
        )
    # Regardless of whether Reason was already scheduled, the
    # interrupt path owes a recovery actor if Reason yields nothing.
    state.pending_interrupt_reason_recovery = True
    state.force_reason_after_fruitless_interrupt = False
    state.last_fruitless_interrupt_meta = {}
    return "proceed"


async def reason_empty_retry_hook(swarm, state) -> str:
    """Extracted empty-reason retry + artifact-chain intent inject from
    ``reason_execute_stage``."""
    emit_bb = partial(emit_scheduler_bb, swarm, state)
    # Round-9: interrupt-forced empty Reason must retry, not
    # silently continue while a sibling burns to hard-cap.
    if state.pending_interrupt_reason_recovery and state.reason_proposed_n == 0:
        try:
            while _fi_retry_empty(
                pending_recovery=state.pending_interrupt_reason_recovery,
                reason_proposed=state.reason_proposed_n,
                retry_count=state.interrupt_empty_reason_retries,
                max_retries=_fi_empty_max(),
            ):
                state.interrupt_empty_reason_retries += 1
                await emit_bb(
                    "fruitless_interrupt_empty_reason_retry",
                    retry_index=state.interrupt_empty_reason_retries,
                    fact_count=swarm._verified_fact_count(),
                    named_artifacts=list(
                        state.last_interrupt_named_artifacts
                    )[:8],
                )
                await emit_bb(
                    "reason_start",
                    trigger="fruitless_interrupt_empty_retry",
                )
                n = await swarm._run_reason()
                state.reason_proposed_n = int(n or 0)
                if state.reason_proposed_n > 0:
                    break
        except Exception:
            pass
    if (
        state.pending_interrupt_reason_recovery
        and state.reason_proposed_n == 0
    ):
        try:
            if _fi_inject(
                pending_recovery=True,
                reason_proposed=state.reason_proposed_n,
                named_artifacts=state.last_interrupt_named_artifacts,
                already_injected=state.interrupt_chain_intent_injected,
            ) and swarm.shared_graph is not None:
                chain_goal = _fi_chain_goal(
                    state.last_interrupt_named_artifacts,
                    domain=state.last_interrupt_replan_domain,
                )
                intent_id = (
                    "intent:fruitless-interrupt-artifact-chain"
                )
                swarm.shared_graph.propose_intent(
                    actor="coordinator",
                    intent_id=intent_id,
                    goal=chain_goal,
                    payload={
                        "worker_class": "code",
                        "source": "fruitless_interrupt_chain",
                        "priority": "high",
                    },
                )
                state.interrupt_chain_intent_injected = True
                state.reason_proposed_n = 1
                await emit_bb(
                    "fruitless_interrupt_artifact_chain_intent",
                    intent_id=intent_id,
                    goal=chain_goal[:220],
                    named_artifacts=list(
                        state.last_interrupt_named_artifacts
                    )[:8],
                )
                await emit_bb(
                    "intent_proposed",
                    actor="coordinator",
                    intent_id=intent_id,
                    goal=chain_goal,
                    worker_class="code",
                )
        except Exception:
            pass
    return "proceed"


async def planner_failure_rebootstrap_hook(swarm, state) -> str:
    """Extracted fruitless re-bootstrap block from ``idle_stage`` (only
    reached when ``not state.tasks and not state.open_intents``; returning
    "proceed" falls through to the base needs_new_information path)."""
    emit_bb = partial(emit_scheduler_bb, swarm, state)
    running_engines = partial(scheduler_running_engines, state)
    stop_for_budget = partial(scheduler_stop_for_budget, swarm, state)
    # Round-4 exception (MUTEKI_FRUITLESS_INTERRUPT): after we killed a
    # worker mid-flight and forced Reason, a planner ConnectTimeout /
    # empty plan must NOT park in collect_idle — spawn one bounded
    # re-bootstrap so the run keeps a live actor.
    try:
        failure = getattr(swarm, "_last_planner_failure", None)
        if _fi_should_reboot(
            pending_recovery=state.pending_interrupt_reason_recovery,
            tasks_empty=True,
            open_intents=0,
            reason_proposed=state.reason_proposed_n,
            planner_failure_kind=_fi_fail_kind(failure),
            rebootstrap_count=state.interrupt_rebootstrap_count,
            max_reboots_n=_fi_max_reboots(),
        ):
            if not swarm._ordinary_capacity_available(state.tasks):
                state.pending_interrupt_reason_recovery = False
            else:
                try:
                    engine = swarm._pick_engine(
                        running_engines(), state.healthy,
                        role="bootstrap")
                except RuntimeError as exc:
                    await emit_bb(
                        "worker_spawn_rejected",
                        reason=str(exc),
                        phase="fruitless_interrupt_rebootstrap",
                    )
                    state.pending_interrupt_reason_recovery = False
                    engine = None
                if engine is not None:
                    try:
                        w = swarm._make_cli_worker(
                            engine, mode="bootstrap",
                            intent_goal=swarm._retry_goal())
                        t = await swarm._schedule_control_worker(
                            w,
                            name=(
                                "fruitless-interrupt-rebootstrap-"
                                f"{engine}"
                            ),
                        )
                        state.tasks[t] = engine
                        state.task_solvers[t] = w
                        state.interrupt_rebootstrap_count += 1
                        state.pending_interrupt_reason_recovery = False
                        state.reason_retry_pending = False
                        state.reason_retry_count = 0
                        state.reason_retry_not_before = 0.0
                        state.reason_retry_failure = ""
                        await emit_bb(
                            "fruitless_interrupt_rebootstrap",
                            worker=w.solver_id,
                            engine=str(engine),
                            rebootstrap_index=state.interrupt_rebootstrap_count,
                            planner_failure=_fi_fail_kind(failure),
                            reason_proposed=state.reason_proposed_n,
                            detail=str(
                                getattr(failure, "detail", "")
                                or ""
                            )[:300],
                        )
                        await emit_bb(
                            "worker_spawned",
                            worker=w.solver_id,
                            phase="fruitless_interrupt_rebootstrap",
                            worker_role="worker",
                            **worker_identity_event_fields(w),
                        )
                        return "continue"
                    except WorkerSpawnRejected as exc:
                        await emit_bb(
                            "worker_spawn_rejected",
                            reason=str(exc),
                            engine=str(engine),
                            phase="fruitless_interrupt_rebootstrap",
                        )
                        state.pending_interrupt_reason_recovery = False
                    except WorkerBudgetExhausted as exc:
                        terminal = await stop_for_budget(str(exc))
                        return "break" if terminal else "continue"
    except Exception:
        pass
    return "proceed"


def reason_context_fold_hook(swarm, shared_graph):
    """Extracted context-firewall fold from ``_run_reason``; returns None
    (→ base ``to_reason_summary`` path) when the firewall is not armed."""
    if not _cf_on():
        return None
    return _cf_fold(shared_graph, list(swarm._standing_guidance))


def ordinary_capacity_cap_hook(swarm, cap: int) -> int:
    """Extracted solo-depth ordinary-worker cap from
    ``_ordinary_capacity_available``; uncapped when solo-depth is off."""
    try:
        if _solo_enabled():
            return min(cap, int(_solo_cap()))
    except Exception:
        pass
    return cap


def dispatch_reorder_hook(swarm, state, running_engines) -> None:
    """Extracted cognitive-cluster dispatch reorder from ``dispatch_stage``;
    mutates ``state.open_intents`` in place."""
    # Cognitive cluster planner: reorder open intents so the next
    # explore worker takes the highest-evidence-value direction, not
    # FIFO creation order.
    if getattr(swarm, "cognitive_cluster_planner", False) and state.open_intents:
        try:
            state.open_intents = plan_dispatch(
                state.open_intents,
                shared_graph=swarm.shared_graph,
                running_engines=running_engines,
            )
        except Exception:
            pass
    return None


def engine_pick_bias_hook(
    swarm, available, running_engines, *, role, intent, avoid_engines
):
    """Extracted cognitive-cluster engine bias from ``_pick_engine``;
    None falls through to the base heterogeneity pick."""
    if not getattr(swarm, "cognitive_cluster_planner", False):
        return None
    if role not in {"explore", "bootstrap", "review"}:
        return None
    try:
        evidence = ClusterEvidence.from_graph(swarm.shared_graph)
        return select_engine(
            available=available,
            running=running_engines,
            evidence=evidence,
            intent=intent or {},
            avoid_engines=list(avoid_engines or ()),
        )
    except Exception:
        return None


def reap_settle_seconds_hook(swarm) -> float:
    """Extracted fruitless-interrupt retire settle from
    ``_retire_finished_worker``."""
    return _fi_settle_wait()


def reap_soft_continue_hook(swarm) -> bool:
    """Extracted fruitless-interrupt retire-miss soft-continue consult from
    ``_retire_finished_worker``."""
    return bool(
        _fi_soft_continue(
            was_fruitless_interrupt=True,
        )
    )


class ExperimentalSwarm(Swarm):
    """Opt-in Swarm that re-arms the extracted v1 experiment hooks.

    The production ``Swarm`` never imports the five experiment modules
    (``fruitless_interrupt_v1``, ``solo_depth_verify_v1``,
    ``chain_completion_v1``, ``context_firewall_v1``,
    ``cognitive_cluster_planner``); with no hooks registered every
    ``_experiment_stage`` / ``_experiment_call`` site is a no-op and behavior
    is byte-identical to the pre-experiment pipeline. Research arms opt in via
    ``swarm_class="muteki.swarm.experimental:ExperimentalSwarm"`` (the
    ``drivers.py`` ``_resolve_swarm_class`` allowlist covers ``muteki.swarm.``).

    Hooks are registered unconditionally EXCEPT the cognitive-cluster pair,
    which is gated on the ``cognitive_cluster_planner`` flag — this class also
    owns the ``MUTEKI_COGNITIVE_CLUSTER_PLANNER=1`` env read the base ctor
    no longer performs. Every hook still consults its module's own
    ``enabled()`` env gate, so env-arming semantics are unchanged.
    """

    def __init__(self, *args: Any, **kwargs: Any) -> None:
        super().__init__(*args, **kwargs)
        # The base ctor stores the flag only; the experimental opt-in owns the
        # env gate so MUTEKI_COGNITIVE_CLUSTER_PLANNER=1 keeps working here.
        self.cognitive_cluster_planner = bool(
            self.cognitive_cluster_planner or planner_enabled_from_env()
        )
        hooks = self._experiment_hooks
        hooks["fruitless_interrupt"] = fruitless_interrupt_hook
        hooks["solo_depth_maintenance"] = solo_depth_maintenance_hook
        hooks["reason_trigger_force"] = reason_trigger_force_hook
        hooks["reason_trigger_interrupt_packet"] = (
            reason_trigger_interrupt_packet_hook
        )
        hooks["reason_empty_retry"] = reason_empty_retry_hook
        hooks["planner_failure_rebootstrap"] = planner_failure_rebootstrap_hook
        hooks["reason_context_fold"] = reason_context_fold_hook
        hooks["ordinary_capacity_cap"] = ordinary_capacity_cap_hook
        hooks["reap_settle_seconds"] = reap_settle_seconds_hook
        hooks["reap_soft_continue"] = reap_soft_continue_hook
        if self.cognitive_cluster_planner:
            hooks["dispatch_reorder"] = dispatch_reorder_hook
            hooks["engine_pick_bias"] = engine_pick_bias_hook


__all__ = ["ExperimentalSwarm"]
