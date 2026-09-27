"""Serial coordinator stage orchestration."""

from __future__ import annotations

import asyncio
import time

from muteki.swarm.coordinator_bootstrap import (
    abort_preloop_acquisitions,
    initialize_loop_state,
    prepare_main_loop,
    start_coordinator,
)
from muteki.swarm.coordinator_completion import (
    reconcile_completion_stage,
    wait_for_workers_stage,
)
from muteki.swarm.coordinator_events import install_coordinator_sinks
from muteki.swarm.coordinator_finalize import (
    coordinator_outcome,
    teardown_coordinator_stage,
)
from muteki.swarm.coordinator_progress import (
    pending_help_stage,
    progress_guard_stage,
    soft_pause_stage,
)
from muteki.swarm.coordinator_recovery import (
    compact_rebootstrap_stage,
    idle_stage,
)
from muteki.swarm.coordinator_reason import (
    reason_collect_stage,
    reason_execute_stage,
    reason_result_stage,
    reason_trigger_stage,
)
from muteki.swarm.coordinator_schedule_dispatch import dispatch_stage
from muteki.swarm.coordinator_state import (
    CoordinatorRunState,
    sync_worker_start_marks as scheduler_sync_worker_start_marks,
)
from muteki.swarm.coordinator_worker_reap import reap_workers_stage
from muteki.swarm.swarm_support import SwarmOutcome


async def worker_maintenance_stage(self, state) -> str:
    now = time.monotonic()
    if (
        getattr(self.challenge, "mode", "ctf") != "ctf"
        and now - float(
            getattr(self, "_last_access_path_health_at", 0.0) or 0.0
        ) >= 5.0
    ):
        self._last_access_path_health_at = now
        graph = getattr(self, "shared_graph", None)
        if graph is not None and hasattr(graph, "refresh_access_path_health"):
            await asyncio.to_thread(
                graph.refresh_access_path_health,
                actor="coordinator",
                target_epoch=str(getattr(self, "_target_epoch", "") or ""),
            )
    # ── Round-16: solo-depth live verify/harvest (no cancel) ─────
    # Experiment hook (stage-4b): default-registered nowhere, so production
    # is a no-op; ExperimentalSwarm wires the solo-depth verify/harvest here.
    await self._experiment_stage("solo_depth_maintenance", state)
    return "proceed"


async def _run_coordinator(self) -> SwarmOutcome:
    """Run the existing coordinator phases in their original serial order."""
    install_coordinator_sinks(self)

    hitl_task = None
    if self.hitl_inbox is not None:
        hitl_task = asyncio.create_task(
            self._supervise_control_drain(), name="hitl-drain")
    state = CoordinatorRunState(hitl_task=hitl_task)

    early_outcome = await start_coordinator(self, state)
    if early_outcome is not None:
        return early_outcome

    initialize_loop_state(self, state)
    try:
        await prepare_main_loop(self, state)
    except BaseException:
        await abort_preloop_acquisitions(self, state)
        raise

    if isinstance(
        getattr(self, "worker_control_ready", None), asyncio.Event
    ):
        self.worker_control_ready.set()

    try:
        while (state.tasks or self._has_dispatchable_open_intents(state.tasks)
               or state.reason_task is not None
               or state.reason_result_ready
               or state.reason_next_trigger
               or state.decide_followup_pending
               or (self.worker_cmds is not None
                   and not self.worker_cmds.empty())
               or state.termination_conclude_reason
               or self._operator_draining
               or self._operator_paused or state.reason_retry_pending):
            await wait_for_workers_stage(self, state)
            await reap_workers_stage(self, state)
            if state.runtime_terminal_failure:
                break

            action = await reason_collect_stage(self, state)
            if action == "break":
                break
            if action == "continue":
                continue

            action = await reason_result_stage(self, state)
            if action == "break":
                break
            if action == "continue":
                continue

            action = await reconcile_completion_stage(self, state)
            if action == "break":
                break
            if action == "continue":
                continue

            action = await progress_guard_stage(self, state)
            if action == "break":
                break
            if action == "continue":
                continue

            action = await soft_pause_stage(self, state)
            if action == "break":
                break
            if action == "continue":
                continue

            action = await pending_help_stage(self, state)
            if action == "break":
                break
            if action == "continue":
                continue

            action = await compact_rebootstrap_stage(self, state)
            if action == "break":
                break
            if action == "continue":
                continue

            # Production bookkeeping (stage-4b: relocated out of the deleted
            # fruitless_interrupt_stage) — per-worker start marks must be
            # synced every tick regardless of any experiment.
            scheduler_sync_worker_start_marks(self, state)
            # Experiment hook: the fruitless-interrupt mid-flight cancel stage.
            action = await self._experiment_stage("fruitless_interrupt", state)
            if action == "break":
                break
            if action == "continue":
                continue

            action = await worker_maintenance_stage(self, state)
            if action == "break":
                break
            if action == "continue":
                continue

            # Decide and Worker execution are independent. Planning reads the
            # durable graph while already accepted work keeps its ownership.
            action = await reason_trigger_stage(self, state)
            if action == "break":
                break
            if action == "continue":
                continue

            action = await reason_execute_stage(self, state)
            if action == "break":
                break
            if action == "continue":
                continue

            if (
                getattr(self.challenge, "mode", "ctf") == "ctf"
                or not state.reason_dispatch_barrier
            ):
                state.open_intents = self._open_intents()
                action = await dispatch_stage(self, state)
                if action == "break":
                    break
                if action == "continue":
                    continue

            action = await idle_stage(self, state)
            if action == "break":
                break
            if action == "continue":
                continue
    finally:
        await teardown_coordinator_stage(self, state)

    return await coordinator_outcome(self, state)
