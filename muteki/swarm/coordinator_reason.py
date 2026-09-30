"""Reason trigger, Decide execution, and result handling stages."""

from __future__ import annotations

import asyncio
import time
from functools import partial

from muteki.swarm.coordinator_state import (
    emit_scheduler_bb,
)
from muteki.swarm.graph_defs import (
    EV_FLAG_FOUND,
    SEMANTIC_GRAPH_KINDS,
)

MAX_CONSECUTIVE_PLANNER_FAILURES = 3
MAX_DUPLICATE_PLAN_RETRIES = 2

# These graph writes describe scheduler ownership, not a changed solve
# frontier. They must not hold queued work behind another Reason pass. All
# other non-Reason semantic writes are planning inputs: facts, dead ends,
# completed attempts, flags, review decisions, route changes, and operator
# direction can invalidate queued work or unlock a shorter next step.
_SCHEDULER_ONLY_SEMANTIC_KINDS = frozenset({
    "intent_proposed",
    "fact_pinned",
    "lane_locked",
    "lane_released",
    "resource_locked",
    "resource_released",
    "intent_lane_deferred",
    "poc_claimed",
})

_CTF_PLANNING_INPUT_KINDS = frozenset({
    "fact_added",
    "intent_concluded",
    "dead_end",
    "finding_found",
    "flag_found",
    "flag_invalidated",
    "operator_directive",
    "operator_directive_status",
})


def _is_external_planning_input(
    event: dict, *, ctf_mode: bool = False, shared_graph=None,
) -> bool:
    if str(event.get("actor") or "") == "reason":
        return False
    kind = str(event.get("kind") or "")
    if kind in _SCHEDULER_ONLY_SEMANTIC_KINDS:
        return False
    if ctf_mode:
        # Worker/process lifecycle changes do not change the solve frontier.
        # A finished Worker either publishes a Fact/dead-end or, when the whole
        # queue becomes idle, is handled by the quiescence trigger below.
        return kind in _CTF_PLANNING_INPUT_KINDS
    if ctf_mode and kind == "branch_split":
        # A branch declared before its source Worker's checkpoint is a pending
        # handoff.  The commit path emits branch_facts_bound after attaching the
        # supporting Facts; that grounded event wakes Decide.  A proposal that
        # already cites valid Facts remains an immediate planning input.
        raw_payload = event.get("payload")
        payload = raw_payload if isinstance(raw_payload, dict) else {}
        raw_branches = payload.get("branches")
        branches = raw_branches if isinstance(raw_branches, list) else []
        return any(
            bool(row.get("from_facts"))
            for row in branches
            if isinstance(row, dict)
        )
    if kind == "coordinator_directive":
        raw_payload = event.get("payload")
        payload = raw_payload if isinstance(raw_payload, dict) else {}
        if str(payload.get("action") or "") == "lane_lock":
            return False
    return True


async def reason_trigger_stage(self, state) -> str:
    """Start one coalesced Decide pass for each changed planning frontier."""
    emit_bb = partial(emit_scheduler_bb, self, state)
    ctf_mode = getattr(self.challenge, "mode", "ctf") in {"ctf", "pentest"}
    planning_limit = self._ordinary_planning_slots(state.tasks)
    state.open_intents = self._open_intents()
    dispatchable = self._dispatchable_open_intents(
        state.open_intents, state.tasks)
    queue_depth = self._ordinary_open_queue_depth(dispatchable)
    state.reason_free_slots = planning_limit
    state.reason_queue_depth = queue_depth
    state.reason_requested_intents = max(0, planning_limit - queue_depth)
    state.graph_grew = state.last_fact_count > state.reason_fact_ckpt
    state.intents_consumed = (
        state.reason_open_intent_ckpt > 0 and not state.open_intents
    )
    state.wm_now = 0
    if self.shared_graph is not None:
        try:
            state.wm_now = int(self.shared_graph.semantic_graph_watermark() or 0)
        except Exception:
            state.wm_now = 0
    state.semantic_changed = state.wm_now > state.last_decided_wm
    state.planning_input_changed = bool(
        state.last_decided_wm < 0 and state.wm_now > 0
    )
    operator_replan = False
    if state.semantic_changed and state.last_decided_wm >= 0:
        try:
            semantic_events = self.shared_graph.events_since(
                state.last_decided_wm,
                kinds=list(SEMANTIC_GRAPH_KINDS),
            )
            external_events = [
                event for event in semantic_events
                if _is_external_planning_input(
                    event, ctf_mode=ctf_mode, shared_graph=self.shared_graph)
            ]
            state.planning_input_changed = bool(external_events)
            operator_replan = any(
                str(event.get("actor") or "") == "operator"
                or str(event.get("kind") or "").startswith("operator_")
                for event in external_events
            )
        except Exception:
            state.planning_input_changed = False if ctf_mode else just_reaped

        if ctf_mode and not state.planning_input_changed:
            # Consume scheduler-only/lifecycle writes so the same watermark is
            # not scanned on every coordinator tick.
            state.last_consumed_wm = state.wm_now
            state.last_decided_wm = state.wm_now

    idle = not state.tasks and queue_depth == 0
    retry_due = (
        state.reason_retry_pending
        and time.monotonic() >= float(state.reason_retry_not_before or 0.0)
        and (ctf_mode or idle)
    )
    if not ctf_mode:
        if (
            state.decide_followup_pending
            and state.wm_now > state.last_consumed_wm
        ):
            state.semantic_changed = True
        wm_pending = (
            state.last_decided_wm < 0
            or state.semantic_changed
            or retry_due
        )
        semantic_replan = bool(state.semantic_changed)
        state.need_reason = bool(
            wm_pending
            and (state.reason_requested_intents > 0 or semantic_replan)
        )
        if state.reason_task is not None:
            if state.planning_input_changed:
                state.reason_dispatch_barrier = True
            state.need_reason = False
            return "proceed"
        if (
            planning_limit > 0
            and wm_pending
            and queue_depth >= planning_limit
            and not semantic_replan
        ):
            await emit_bb(
                "reason_skipped",
                trigger="queue_covers_capacity",
                open_intents=len(state.open_intents),
                ordinary_open_intents=self._ordinary_open_queue_depth(
                    state.open_intents
                ),
                max_workers=self.max_workers,
                dispatch_mode=self.dispatch_mode,
                planning_limit=self._ordinary_capacity_limit(),
            )
        if state.need_reason and state.reason_requested_intents <= 0:
            state.reason_requested_intents = max(1, planning_limit)
        if (
            state.need_reason
            and self._reason_backpressure_active(state.open_intents)
            and not semantic_replan
        ):
            await emit_bb(
                "reason_skipped",
                trigger="queue_backpressure",
                open_intents=len(state.open_intents),
                ordinary_open_intents=self._ordinary_open_queue_depth(
                    state.open_intents
                ),
                max_workers=self.max_workers,
                dispatch_mode=self.dispatch_mode,
                planning_limit=self._ordinary_capacity_limit(),
            )
            state.need_reason = False
        return "proceed"

    # CTF Decide wakes on an actual graph result, including a Step concluded
    # without a Fact. Worker retirement/idle telemetry alone does not create
    # work. Decide stays serialized; graph changes set one follow-up bit.
    explicit_trigger = bool(state.reason_next_trigger)
    unconsumed_change = bool(
        state.planning_input_changed
        or (
            state.decide_followup_pending
            and state.wm_now > state.last_consumed_wm
        )
    )
    if state.reason_task is not None:
        if unconsumed_change or operator_replan or explicit_trigger:
            state.decide_followup_pending = True
        state.need_reason = False
        return "proceed"

    candidate_trigger = bool(
        retry_due
        or explicit_trigger
        or state.last_decided_wm < 0
        or unconsumed_change
    )
    state.need_reason = candidate_trigger
    if state.need_reason:
        # Plan only for real capacity. Zero is a valid upper bound: the model may
        # still drop/reprioritize Steps or commit a no-op after reading new Facts.
        capacity_deficit = max(0, planning_limit - queue_depth)
        state.reason_requested_intents = capacity_deficit
        if not state.reason_next_trigger:
            state.reason_next_trigger = (
                "operator" if operator_replan
                else "ctf_graph_change" if unconsumed_change
                else "ctf_initial"
            )
    return "proceed"


async def reason_execute_stage(self, state) -> str:
    emit_bb = partial(emit_scheduler_bb, self, state)
    if not state.need_reason or state.reason_task is not None:
        return "proceed"

    ctf_mode = getattr(self.challenge, "mode", "ctf") in {"ctf", "pentest"}
    if ctf_mode:
        trigger = (
            "campaign_boundary"
            if (state.reaped_n > 0 or state.decide_followup_pending)
            else "graph"
            if (state.graph_grew or state.intents_consumed or state.semantic_changed)
            else "idle"
        )
    else:
        evidence_epoch = bool(
            state.planning_input_changed or state.decide_followup_pending
        )
        trigger = (
            "evidence"
            if evidence_epoch
            else "graph"
            if (state.graph_grew or state.intents_consumed or state.semantic_changed)
            else "idle"
        )
    retry_index = (
        state.reason_retry_count + 1
        if state.reason_retry_pending else 0
    )
    next_trigger = str(state.reason_next_trigger or "")
    state.reason_started_trigger = (
        next_trigger or ("retry" if retry_index else trigger)
    )
    if next_trigger:
        retry_index = 0
        state.reason_next_trigger = ""
    state.reason_started_wm = state.wm_now
    requested_intents = max(0, int(state.reason_requested_intents or 0))
    state.reason_started_max_intents = requested_intents
    if ctf_mode:
        state.ctf_batch_id += 1
        state.ctf_batch_spawned = 0
        state.ctf_batch_reaped = 0
        state.ctf_batch_sealed = False
        reason_mode_fields = {"ctf_batch_id": state.ctf_batch_id}
    else:
        reason_mode_fields = {"dispatch_barrier": evidence_epoch}
    await emit_bb(
        "reason_start",
        trigger=state.reason_started_trigger,
        watermark=state.reason_started_wm,
        retry_index=retry_index,
        previous_failure=state.reason_retry_failure,
        requested_intents=requested_intents,
        ordinary_free_slots=state.reason_free_slots,
        queued_ordinary_intents=state.reason_queue_depth,
        dispatch_mode=self.dispatch_mode,
        planning_limit=self._ordinary_capacity_limit(),
        **reason_mode_fields,
    )
    retry_rejections = [
        {
            "reason": str(row.get("reason_code") or "unknown")[:80],
            "goal": str(row.get("goal") or "")[:300],
        }
        for row in list(getattr(self, "_last_dispatch_decisions", []) or [])
        if str(row.get("outcome") or "") == "dropped"
    ][:8]
    self._reason_retry_note = (
        {
            "retry_index": retry_index,
            "failure": state.reason_retry_failure,
            "rejections": retry_rejections,
        }
        if retry_index else None
    )
    state.last_consumed_wm = state.reason_started_wm
    state.last_decided_wm = state.reason_started_wm
    self._last_reason_context_wm = state.reason_started_wm
    if not ctf_mode:
        state.reason_dispatch_barrier = evidence_epoch
    state.decide_followup_pending = False
    state.reason_proposed_n = 0
    state.reason_result_ready = False
    state.reason_stale_refresh = False
    self._reason_max_intents_override = requested_intents
    state.reason_task = asyncio.create_task(
        self._run_reason(),
        name=f"reason-{self.run_id}-{state.reason_started_wm}",
    )
    return "proceed"


async def reason_collect_stage(self, state) -> str:
    """Fold one completed Decide task without blocking Worker scheduling."""
    task = state.reason_task
    if task is None or not task.done():
        return "proceed"

    emit_bb = partial(emit_scheduler_bb, self, state)
    from muteki.solver.reason import PlannerFailure, PlannerFailureKind
    ctf_mode = getattr(self.challenge, "mode", "ctf") in {"ctf", "pentest"}

    state.reason_task = None
    self._reason_max_intents_override = None
    decision_wm = max(
        int(state.reason_started_wm),
        int(getattr(self, "_last_reason_context_wm", -1) or -1),
    )
    reason_error = ""
    try:
        n = task.result()
    except asyncio.CancelledError:
        n = 0
        reason_error = f"{PlannerFailureKind.EXCEPTION.value}: planner task cancelled"
        self._last_planner_failure = PlannerFailure(
            PlannerFailureKind.EXCEPTION,
            "planner task cancelled before producing a result",
        )
    except Exception as exc:
        n = 0
        reason_error = (
            f"{PlannerFailureKind.EXCEPTION.value}: "
            f"{type(exc).__name__}: {exc}"
        )
        self._last_planner_failure = PlannerFailure(
            PlannerFailureKind.EXCEPTION,
            f"{type(exc).__name__}: {exc}",
        )
    else:
        failure = getattr(self, "_last_planner_failure", None)
        failure_kind = getattr(failure, "kind", "")
        if hasattr(failure_kind, "value"):
            failure_kind = failure_kind.value
        if str(failure_kind or "") in (
            PlannerFailureKind.UNAVAILABLE.value,
            PlannerFailureKind.EXCEPTION.value,
            PlannerFailureKind.TIMEOUT.value,
        ):
            reason_error = (
                f"{failure_kind}: "
                f"{str(getattr(failure, 'detail', '') or '')}"
            )

    if (
        not reason_error
        and int(n or 0) > 0
        and getattr(self.challenge, "mode", "ctf") == "ctf"
        and self._auto_dispatch_enabled()
        and self.shared_graph is not None
    ):
        try:
            newer_flags = self.shared_graph.events_since(
                decision_wm,
                kinds=[EV_FLAG_FOUND],
            )
        except Exception:
            newer_flags = []
        if newer_flags and self._flags_complete():
            accepted_ids = [
                str(decision.get("resolved_intent_id") or "")
                for decision in list(
                    getattr(self, "_last_dispatch_decisions", []) or []
                )
                if str(decision.get("outcome") or "") == "accepted"
                and str(decision.get("resolved_intent_id") or "")
            ]
            try:
                stale_ids = self.shared_graph.supersede_open_intent_ids(
                    actor="coordinator",
                    intent_ids=accepted_ids,
                    reason="flag frontier advanced while this plan was running",
                )
            except Exception:
                stale_ids = []
            if stale_ids:
                n = 0
                state.reason_stale_refresh = True
                state.reason_next_trigger = "ctf_frontier_advanced_during_plan"
                state.decide_followup_pending = True
                previous = list(
                    getattr(self, "_last_reason_superseded", []) or []
                )
                self._last_reason_superseded = list(dict.fromkeys([
                    *previous,
                    *stale_ids,
                ]))
                await emit_bb(
                    "reason_plan_superseded",
                    trigger="flag_frontier_advanced",
                    watermark=decision_wm,
                    intent_ids=stale_ids,
                )
        elif newer_flags:
            # In a multi-Flag run, a partial Flag advances progress without
            # invalidating capability work planned from the same graph cut.
            # Keep those admitted Steps (RCE, pivoting and shared access remain
            # useful for the outstanding Flags) and request one coalesced fresh
            # Decide so the new terminal progress is also visible.
            state.reason_stale_refresh = True
            state.reason_next_trigger = "ctf_partial_flag_during_plan"
            state.decide_followup_pending = True

    state.reason_result_ready = True
    if reason_error:
        state.reason_proposed_n = 0
        state.last_reason_error = reason_error
        state.last_decided_wm = decision_wm
        state.last_consumed_wm = decision_wm
        await emit_bb(
            "reason_failed",
            error=reason_error,
            watermark=decision_wm,
        )
    else:
        state.reason_proposed_n = int(n or 0)
        state.last_reason_error = ""
        state.last_decided_wm = decision_wm
        state.last_consumed_wm = decision_wm

    reason_result = getattr(self, "_last_reason", None)
    clean_noop = bool(
        not reason_error
        and reason_result is not None
        and getattr(reason_result, "planner_failure", None) is None
        and state.reason_proposed_n == 0
    )
    if state.reason_proposed_n > 0 or clean_noop:
        state.reason_duplicate_fact_ckpt = -1
        state.reason_retry_pending = False
        state.reason_retry_count = 0
        state.reason_retry_not_before = 0.0
        state.reason_retry_failure = ""
        self._reason_retry_note = None
    elif state.reason_stale_refresh:
        state.reason_retry_pending = False
        state.reason_retry_count = 0
        state.reason_retry_not_before = 0.0
        state.reason_retry_failure = ""
        self._reason_retry_note = None
    else:
        failure = getattr(self, "_last_planner_failure", None)
        failure_kind = getattr(failure, "kind", "empty_plan")
        if hasattr(failure_kind, "value"):
            failure_kind = failure_kind.value
        state.open_intents = self._open_intents()
        all_proposals_dropped = bool(
            getattr(reason_result, "intents", None)
        ) and str(failure_kind) == PlannerFailureKind.NEEDS_NEW_INFORMATION.value
        dropped_codes = {
            str(row.get("reason_code") or "")
            for row in list(getattr(self, "_last_dispatch_decisions", []) or [])
            if str(row.get("outcome") or "") == "dropped"
        }
        covered_by_inflight_work = bool(dropped_codes) and dropped_codes <= {
            "declared_duplicate",
            "equivalent_step",
            "active_step_stage",
            "active_branch_stage",
            "active_coverage_method",
            "producer_successor_queued",
            "producer_successor_exists",
            "repeated_lane_evidence",
            "active_lane",
            "active_route",
            "storage_duplicate",
            "dependency_not_ready",
        }
        if all_proposals_dropped and self._auto_dispatch_enabled():
            state.reason_duplicate_fact_ckpt = self._total_fact_count()
        if (all_proposals_dropped
                and covered_by_inflight_work
                and (state.tasks or state.open_intents)):
            # The proposed work already has an active or queued owner. Waiting
            # for that owner to publish new evidence is the useful next action;
            # immediate correction passes only ask the planner to paraphrase the
            # same Step and compete with Workers for model capacity.
            state.reason_retry_pending = False
            state.reason_retry_count = 0
            state.reason_retry_not_before = 0.0
            state.reason_retry_failure = ""
            self._reason_retry_note = None
            await emit_bb(
                "reason_retry_deferred",
                trigger="covered_work_in_flight",
                active_workers=len(state.tasks),
                open_intents=len(state.open_intents),
                planner_failure=str(failure_kind or "empty_plan"),
                detail="all proposed intents already have active or queued owners",
            )
        elif (all_proposals_dropped
                and state.reason_started_max_intents > 0
                and state.reason_retry_count < MAX_DUPLICATE_PLAN_RETRIES):
            # The model can identify a novel action in progress.next yet return
            # only rejected intents. A bounded correction pass fills otherwise-
            # idle capacity even while unrelated Workers remain active.
            state.reason_retry_count += 1
            state.reason_retry_pending = True
            state.reason_retry_not_before = time.monotonic() + 2.0
            state.reason_retry_failure = "all_proposals_dropped"
            await emit_bb(
                "reason_retry_scheduled",
                retry_index=state.reason_retry_count + 1,
                delay_s=2.0,
                planner_failure=state.reason_retry_failure,
                detail="all proposed intents were duplicate or already active",
            )
        elif state.tasks or state.open_intents:
            state.reason_retry_pending = False
            state.reason_retry_count = 0
            state.reason_retry_not_before = 0.0
            state.reason_retry_failure = ""
            self._reason_retry_note = None
            await emit_bb(
                "reason_retry_deferred",
                trigger="work_in_flight",
                active_workers=len(state.tasks),
                open_intents=len(state.open_intents),
                planner_failure=str(failure_kind or "empty_plan"),
                detail=str(getattr(failure, "detail", "")),
            )
        else:
            state.reason_retry_count += 1
            terminal_planner_failure = (
                reason_error
                and state.reason_retry_count >= MAX_CONSECUTIVE_PLANNER_FAILURES
                and (not ctf_mode or str(failure_kind) == PlannerFailureKind.UNAVAILABLE.value)
            )
            if terminal_planner_failure:
                detail = str(
                    getattr(failure, "detail", "") or reason_error
                )
                state.reason_retry_pending = False
                state.reason_retry_not_before = 0.0
                state.reason_retry_failure = str(failure_kind or "planner_exception")
                state.planner_terminal_failure = True
                self._runtime_failure_detail = (
                    f"Planner unavailable after {state.reason_retry_count} attempts: "
                    f"{detail}"
                )
                self._runtime_failure_code = "planner_unavailable"
                self._runtime_failure_phase = "planner"
                await emit_bb(
                    "planner_unavailable",
                    attempts=state.reason_retry_count,
                    planner_failure=state.reason_retry_failure,
                    detail=detail,
                )
            else:
                retry_delay = min(
                    30.0,
                    float(2 ** min(state.reason_retry_count, 5)),
                )
                state.reason_retry_pending = True
                state.reason_retry_not_before = time.monotonic() + retry_delay
                state.reason_retry_failure = str(failure_kind or "empty_plan")
                await emit_bb(
                    "reason_retry_scheduled",
                    retry_index=state.reason_retry_count + 1,
                    delay_s=retry_delay,
                    planner_failure=state.reason_retry_failure,
                    detail=str(getattr(failure, "detail", "")),
                )

    wm_after = decision_wm
    if self.shared_graph is not None:
        try:
            wm_after = int(self.shared_graph.semantic_graph_watermark() or 0)
        except Exception:
            wm_after = decision_wm
    if wm_after > decision_wm:
        external_planning_change = True
        try:
            semantic_events = self.shared_graph.events_since(
                decision_wm,
                kinds=list(SEMANTIC_GRAPH_KINDS),
            )
            external_planning_change = any(
                _is_external_planning_input(event)
                if not ctf_mode
                else _is_external_planning_input(
                    event, ctf_mode=True, shared_graph=self.shared_graph)
                for event in semantic_events
            )
        except Exception:
            pass
        if external_planning_change:
            state.decide_followup_pending = True
        else:
            state.last_consumed_wm = wm_after
            state.last_decided_wm = wm_after

    return "proceed"


async def reason_result_stage(self, state) -> str:
    emit_bb = partial(emit_scheduler_bb, self, state)
    if (self.cost_budget_usd is not None and self.cost is not None
            and self.cost.snapshot()["unpriced_calls"]):
        state.runtime_terminal_failure = True
        self._runtime_failure_code = "budget_coverage_incomplete"
        self._runtime_failure_phase = "usage_settlement"
        self._runtime_failure_detail = (
            "本 Run 出现未定价或不完整用量，无法继续执行美元预算；请取消美元预算或新建 Run"
        )
        await emit_bb(
            "budget_coverage_incomplete", code="budget_coverage_incomplete",
            detail=self._runtime_failure_detail,
        )
        if state.reason_task is not None and not state.reason_task.done():
            state.reason_task.cancel()
        if state.pentest_review_task is not None and not state.pentest_review_task.done():
            state.pentest_review_task.cancel()
        for task, solver in list(state.task_solvers.items()):
            if not task.done():
                self._cancel_solver(solver)
                task.cancel()
        return "break"
    ctf_mode = getattr(self.challenge, "mode", "ctf") in {"ctf", "pentest"}
    if state.reason_result_ready:
        state.reason_result_ready = False
        decision_wm = max(
            int(state.reason_started_wm),
            int(getattr(self, "_last_reason_context_wm", -1) or -1),
        )
        # Release only timeout handoffs that this exact Reason snapshot could
        # see. A Worker may have timed out while Reason was already running;
        # that newer checkpoint stays deferred for the required follow-up pass.
        for intent_id, required_wm in list(
            state.checkpoint_replan_wm.items()
        ):
            if int(required_wm) <= decision_wm:
                state.checkpoint_replan_wm.pop(intent_id, None)
        state.open_intents = self._open_intents()
        if (
            getattr(self.challenge, "mode", "ctf") in {"ctf", "pentest"}
            and state.reason_proposed_n > 0
        ):
            for decision in list(
                getattr(self, "_last_dispatch_decisions", []) or []
            ):
                if str(decision.get("outcome") or "") != "accepted":
                    continue
                intent_id = str(decision.get("resolved_intent_id") or "")
                if intent_id:
                    state.ctf_intent_batches[intent_id] = state.ctf_batch_id
        # checkpoint the graph state we just reasoned over (A).
        state.reason_fact_ckpt = self._verified_fact_count()
        state.reason_open_intent_ckpt = len(state.open_intents)
        reason_fields = self._reason_event_fields(state.reason_proposed_n)
        await emit_bb(
            "reason_done",
            trigger=state.reason_started_trigger,
            watermark=decision_wm,
            requested_intents=state.reason_started_max_intents,
            ctf_batch_id=state.ctf_batch_id,
            **reason_fields,
        )
        dropped_intents = (
            list(getattr(self, "_last_reason_superseded", []) or [])
            if ctf_mode else []
        )
        for intent_id in dropped_intents:
            for task, task_intent in list(state.task_intents.items()):
                if task.done() or str(task_intent or "") != str(intent_id):
                    continue
                solver = state.task_solvers.get(task)
                worker_id = str(getattr(solver, "solver_id", "") or "")
                if not self._cancel_solver(solver):
                    continue
                task.cancel()
                await emit_bb(
                    "intent_drop_requested",
                    intent_id=str(intent_id),
                    worker=worker_id,
                    reason="Decide explicitly dropped this Step",
                )
                break
        preemptions = (
            []
            if ctf_mode
            else list(getattr(self, "_last_reason_preemptions", []) or [])
        )
        self._last_reason_preemptions = []
        for preemption in preemptions:
            intent_id = str(preemption.get("intent_id") or "")
            expected_worker = str(preemption.get("worker") or "")
            expected_lane = str(preemption.get("lane_key") or "")
            for task, task_intent in list(state.task_intents.items()):
                if task.done() or str(task_intent or "") != intent_id:
                    continue
                solver = state.task_solvers.get(task)
                worker_id = str(getattr(solver, "solver_id", "") or "")
                lane_key = str(state.task_lanes.get(task) or "")
                try:
                    lane_key = self.shared_graph.normalize_lane_key(lane_key)
                except Exception:
                    lane_key = ""
                if (
                    solver is None
                    or (expected_worker and worker_id != expected_worker)
                    or (expected_lane and lane_key != expected_lane)
                ):
                    continue
                if not self._cancel_solver(solver):
                    continue
                task.cancel()
                await emit_bb(
                    "intent_preempt_requested",
                    intent_id=intent_id,
                    worker=worker_id,
                    lane_key=lane_key,
                    replacement_intent_id=str(
                        preemption.get("replacement_intent_id") or ""
                    ),
                    reason=str(preemption.get("reason") or "")[:1000],
                    checkpoint_first=False,
                )
                break
        if state.planner_terminal_failure:
            return "break"
        dropped_dup = int(reason_fields["dropped_dup"])
        # 0 = disabled: without the threshold guard, `dropped_dup >= 0` fired a
        # review on EVERY Reason pass.
        dup_review_threshold = int(
            self.review_policy.get("after_duplicate_intents") or 0)
        if (dup_review_threshold > 0 and dropped_dup >= dup_review_threshold
                and await self._maybe_start_review(
                    trigger="duplicate_intents",
                    directive=(
                        f"Reason dropped {dropped_dup} duplicate intent(s); audit "
                        "the supporting facts and record any search gap as a "
                        "REVIEW_FINDING for the next Reason pass."
                    ),
                    healthy=state.healthy, tasks=state.tasks,
                    task_solvers=state.task_solvers, emit_bb=emit_bb)):
            return "continue"

        # Decide interprets the goal; the host checks only cited evidence and
        # authorization before making that decision durable.
        rr = getattr(self, "_last_reason", None)
        if getattr(self.challenge, "mode", "ctf") == "pentest":
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
                return "break"
            if rr is not None and getattr(rr, "verdict", "") == "complete":
                citations = list(getattr(rr, "goal_evidence_facts", None) or [])
                committed = self.shared_graph.record_pentest_goal_completion(
                    fact_seqs=citations,
                    reason=str(getattr(rr, "complete_why", "") or ""),
                )
                if self._findings_complete():
                    state.goal_complete = True
                    await emit_bb(
                        "goal_complete",
                        why="model_goal_with_evidence",
                        fact_seqs=citations,
                    )
                    for other in state.tasks:
                        self._cancel_solver(state.task_solvers.get(other))
                        other.cancel()
                    return "break"
                contract = getattr(self.challenge, "pentest_contract", None)
                if (contract is not None and contract.version >= 2
                        and contract.report_goal_mode == "automatic"):
                    from muteki.pentest.judgement import submitted_reports
                    if any(item.get("review_status") == "pending" for item in
                           submitted_reports(self.shared_graph.events(), contract)):
                        await emit_bb(
                            "goal_completion_deferred",
                            reason="pending_report_reviews",
                        )
                        return "continue"
                await emit_bb(
                    "goal_complete_rejected",
                    reason="cited Fact provenance or scope is invalid",
                    fact_seqs=citations,
                )
        if (getattr(self.challenge, "mode", "ctf") != "pentest"
                and rr is not None
                and getattr(rr, "verdict", "") == "complete"
                and self._flags_complete()):
            if state.winner is None:
                state.winner = "coordinator"
                state.flag = self._found_flags[0] if self._found_flags else None
            await emit_bb(
                "goal_complete",
                why=getattr(rr, "complete_why", "")[:300],
                flags=len(self._found_flags))
            for other in state.tasks:
                self._cancel_solver(state.task_solvers.get(other))
                other.cancel()
            return "break"

    return "proceed"
