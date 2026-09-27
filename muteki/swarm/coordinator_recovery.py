"""Compaction, rebootstrap, and idle recovery stages."""

from __future__ import annotations

async def compact_rebootstrap_stage(self, state) -> str:
    import time
    from functools import partial
    from muteki.swarm.coordinator_state import (
        emit_scheduler_bb,
        running_engines as scheduler_running_engines,
    )
    from muteki.core.events import Event, EventType

    emit_bb = partial(emit_scheduler_bb, self, state)
    running_engines = partial(scheduler_running_engines, state)
    now_mono = time.monotonic()
    no_progress_elapsed = now_mono - state.last_progress_t
    compact_due = (
        getattr(self.challenge, "mode", "ctf") != "ctf"
        and self.shared_graph is not None
        and (now_mono - state.last_compact_t) > 60.0  # don't thrash
        and (
            (self.barren_limit > 0 and state.fruitless_workers >= 2 * self.barren_limit)
            or (state.compact_no_progress_s > 0 and no_progress_elapsed >= state.compact_no_progress_s)
        )
    )
    if compact_due:
        trigger = ("fruitless_workers"
                   if state.fruitless_workers >= 2 * self.barren_limit
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
                summary=(f"compacted after {state.fruitless_workers} fruitless "
                         f"workers / {int(no_progress_elapsed)}s no progress"))
            state.last_compact_t = now_mono
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
            await emit_bb(
                "graph_compacted", compact_id=info.get("compact_id"),
                trigger=trigger,
                retired=len(info.get("retired_intent_ids") or []))
            # 刀6: mirror the per-intent retirement onto the bus so the
            # deck folds dispatchState→retired (the DB event row alone
            # never reaches the SSE/JSONL stream the UI reads).
            retired_ids = [str(x) for x in (info.get("retired_intent_ids") or []) if x]
            if retired_ids:
                await emit_bb(
                    "intent_state_changed",
                    intent_id=",".join(retired_ids),
                    dispatch_state="retired",
                    compact_id=info.get("compact_id"))
        except Exception:
            pass

    # Too many consecutive fruitless workers is a replanning signal. It must
    # never become an autonomous pause: an unsolved run remains live until the
    # operator stops it or an explicit budget expires.
    barren_replan_due = (
        getattr(self.challenge, "mode", "ctf") != "ctf"
        and self.barren_limit > 0
        and state.fruitless_workers >= self.barren_limit
        and state.fruitless_workers > state.last_pause_fruitless
    )
    if barren_replan_due and not self._pending_help:
        fruitless_review_after = int(self.review_policy.get("after_fruitless_workers") or 0)
        if (getattr(self.challenge, "mode", "ctf") != "ctf"
                and fruitless_review_after > 0
                and state.fruitless_workers >= fruitless_review_after
                and await self._maybe_start_review(
                    trigger="fruitless_workers",
                    directive=(f"{state.fruitless_workers} consecutive workers produced no new fact or flag; "
                               "audit repeated work and dead-end amnesia, then record a REVIEW_FINDING "
                               "with concrete recommended_actions for Reason."),
                    healthy=state.healthy, tasks=state.tasks,
                    task_solvers=state.task_solvers, emit_bb=emit_bb)):
            return "continue"
        state.last_pause_fruitless = state.fruitless_workers
        state.reason_retry_pending = True
        state.reason_retry_not_before = min(
            float(state.reason_retry_not_before or now_mono), now_mono
        )
        await emit_bb(
            "stagnation_replan_scheduled",
            reason=(f"{state.fruitless_workers} consecutive workers finished "
                    f"with no new fact or flag; goal remains incomplete"),
            flags=len(self._found_flags),
            fruitless_workers=state.fruitless_workers)

    # ── operator worker control: spawn/kill a specific engine on demand
    await self._apply_worker_cmds(
        tasks=state.tasks, task_solvers=state.task_solvers, healthy=state.healthy,
        running_engines_fn=running_engines, emit_bb=emit_bb)
    return "proceed"


async def idle_stage(self, state) -> str:
    import time
    from functools import partial
    from muteki.swarm.coordinator_state import emit_scheduler_bb

    emit_bb = partial(emit_scheduler_bb, self, state)
    # An empty queue in a goal-incomplete run schedules another Decide pass.
    # The retry is paced by reason_retry_not_before in reason_trigger_stage;
    # this branch never waits for operator input on its own.
    state.open_intents = self._open_intents()
    # Round-9: only clear recovery once work exists. Do NOT drop the
    # latch just because a sibling task is still burning after an
    # empty Reason — that was the proposed=0 silent-stall.
    if state.pending_interrupt_reason_recovery and (
        state.reason_proposed_n > 0 or state.open_intents
    ):
        state.pending_interrupt_reason_recovery = False
    if (not state.tasks and not state.open_intents
            and state.reason_task is None
            and not state.reason_result_ready):
        # A CTF round with no committed Fact has no new planning input. 12662
        # ends the round here instead of inventing a replacement Worker or an
        # idle Decide pass.
        if getattr(self.challenge, "mode", "ctf") == "ctf" \
                and not state.reason_retry_pending:
            return "break"
        # Experimental recovery may still provide concrete work first.
        action = await self._experiment_stage("planner_failure_rebootstrap", state)
        if action == "break":
            return "break"
        if action == "continue":
            return "continue"
        failure = getattr(self, "_last_planner_failure", None)
        if not state.reason_retry_pending:
            state.reason_retry_pending = True
            state.reason_retry_not_before = time.monotonic()
            state.reason_retry_failure = str(
                getattr(failure, "kind", "empty_plan")
            )
            await emit_bb(
                "reason_retry_scheduled",
                retry_index=state.reason_retry_count + 1,
                delay_s=0,
                planner_failure=state.reason_retry_failure,
                detail=str(getattr(failure, "detail", ""))[:300],
            )
        return "continue"
    return "proceed"
