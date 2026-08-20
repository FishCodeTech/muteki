"""Pure Protocol 2 reducers: no IO, clock, randomness, model, or filesystem."""

from __future__ import annotations

from dataclasses import dataclass, replace
from enum import Enum
from typing import Iterable

from muteki.epistemic.contracts import EventEnvelopeV2, canonical_digest


class KernelHealth(str, Enum):
    BOOTSTRAPPING = "bootstrapping"
    VERIFYING = "verifying"
    READY = "ready"
    DEGRADED_NO_AUTHORITY = "degraded_no_authority"
    DEGRADED_INTEGRITY = "degraded_integrity"
    RECOVERY_REQUIRED = "recovery_required"
    CORRUPT_QUARANTINED = "corrupt_quarantined"


class RunExecution(str, Enum):
    NEW = "new"
    RUNNING = "running"
    QUIESCING = "quiescing"
    STOPPED = "stopped"
    REOPEN_REQUIRED = "reopen_required"
    ARCHIVED = "archived"


class SearchControlMode(str, Enum):
    ACTIVE = "active"
    PAUSING = "pausing"
    PAUSED = "paused"
    DRAINING = "draining"


@dataclass(frozen=True, slots=True)
class CanonicalState:
    run_id: str
    head_seq: int = 0
    head_event_digest: str = ""
    command_count: int = 0
    event_count: int = 0
    kernel_health: KernelHealth = KernelHealth.BOOTSTRAPPING
    run_execution: RunExecution = RunExecution.NEW
    search_mode: SearchControlMode = SearchControlMode.PAUSED
    run_fence_epoch: int = 0
    execution_generation: int = 0
    completion_generation: int = 0

    def as_dict(self) -> dict:
        return {
            "command_count": self.command_count,
            "completion_generation": self.completion_generation,
            "event_count": self.event_count,
            "execution_generation": self.execution_generation,
            "head_event_digest": self.head_event_digest,
            "head_seq": self.head_seq,
            "kernel_health": self.kernel_health.value,
            "run_execution": self.run_execution.value,
            "run_fence_epoch": self.run_fence_epoch,
            "run_id": self.run_id,
            "search_mode": self.search_mode.value,
        }

    @property
    def checksum(self) -> str:
        return canonical_digest(self.as_dict())


def initial_state(run_id: str) -> CanonicalState:
    if not run_id:
        raise ValueError("run_id is required")
    return CanonicalState(run_id=run_id)


def apply_event(state: CanonicalState, event: EventEnvelopeV2, *, seq: int) -> CanonicalState:
    if event.run_id != state.run_id:
        raise ValueError("cross-run event splice")
    if seq != state.head_seq + 1:
        raise ValueError("event sequence is not contiguous")
    if event.parent_event_digest != state.head_event_digest:
        raise ValueError("event parent digest mismatch")

    next_state = replace(
        state,
        head_seq=seq,
        head_event_digest=event.digest,
        event_count=state.event_count + 1,
    )
    payload = event.payload
    kind = event.kind
    if kind == "BOOT_VERIFYING":
        next_state = replace(next_state, kernel_health=KernelHealth.VERIFYING)
    elif kind == "BOOT_READY":
        next_state = replace(next_state, kernel_health=KernelHealth.READY)
    elif kind == "AUTHORITY_DEGRADED":
        next_state = replace(next_state, kernel_health=KernelHealth.DEGRADED_NO_AUTHORITY,
                             search_mode=SearchControlMode.PAUSED)
    elif kind == "INTEGRITY_DEGRADED":
        next_state = replace(next_state, kernel_health=KernelHealth.DEGRADED_INTEGRITY,
                             search_mode=SearchControlMode.PAUSED)
    elif kind == "START_EXECUTION":
        if next_state.kernel_health is not KernelHealth.READY:
            raise ValueError("execution cannot start before kernel READY")
        if state.run_execution not in {RunExecution.NEW, RunExecution.STOPPED}:
            raise ValueError("execution start requires a new or stopped run")
        generation = payload.get("execution_generation")
        fence = payload.get("run_fence_epoch")
        if type(generation) is not int or type(fence) is not int:
            raise ValueError("execution generation/fence must be exact integers")
        if generation != state.execution_generation + 1 or fence < state.run_fence_epoch + 1:
            raise ValueError("execution generation/fence is not fresh")
        next_state = replace(next_state, run_execution=RunExecution.RUNNING,
                             search_mode=SearchControlMode.ACTIVE,
                             execution_generation=generation, run_fence_epoch=fence)
    elif kind == "SEARCH_PAUSED":
        next_state = replace(next_state, search_mode=SearchControlMode.PAUSED)
    elif kind == "SEARCH_RESUMED":
        if next_state.run_execution is not RunExecution.RUNNING:
            raise ValueError("only a running execution can resume search")
        next_state = replace(next_state, search_mode=SearchControlMode.ACTIVE)
    elif kind == "GOAL_COMPLETED":
        next_state = replace(
            next_state,
            run_execution=RunExecution.QUIESCING,
            search_mode=SearchControlMode.DRAINING,
            completion_generation=state.completion_generation + 1,
        )
    elif kind == "EXECUTION_STOP_REQUESTED":
        if next_state.run_execution is not RunExecution.RUNNING:
            raise ValueError("execution stop requires a running scope")
        next_state = replace(
            next_state,
            run_execution=RunExecution.QUIESCING,
            search_mode=SearchControlMode.DRAINING,
        )
    elif kind == "EXECUTION_SCOPE_DRAINED":
        if next_state.run_execution not in {RunExecution.QUIESCING,
                                             RunExecution.REOPEN_REQUIRED}:
            raise ValueError("scope drain requires quiescing/reopen state")
        target = (RunExecution.REOPEN_REQUIRED
                  if next_state.run_execution is RunExecution.REOPEN_REQUIRED
                  else RunExecution.STOPPED)
        next_state = replace(next_state, run_execution=target,
                             search_mode=SearchControlMode.PAUSED)
    elif kind == "GOAL_INVALIDATED":
        if next_state.run_execution is not RunExecution.ARCHIVED:
            next_state = replace(
                next_state,
                run_execution=RunExecution.REOPEN_REQUIRED,
                search_mode=(SearchControlMode.DRAINING
                             if state.search_mode is SearchControlMode.DRAINING
                             else SearchControlMode.PAUSED),
                run_fence_epoch=state.run_fence_epoch + 1,
                completion_generation=state.completion_generation + 1,
            )
    elif kind == "RUN_ARCHIVE_REQUESTED":
        if next_state.run_execution is RunExecution.ARCHIVED:
            raise ValueError("archived run cannot be archived again")
        next_state = replace(
            next_state,
            run_execution=RunExecution.QUIESCING,
            search_mode=SearchControlMode.DRAINING,
            run_fence_epoch=state.run_fence_epoch + 1,
        )
    elif kind == "RUN_ARCHIVED":
        if state.run_execution is not RunExecution.QUIESCING:
            raise ValueError("archive completion requires quiescing state")
        next_state = replace(next_state, run_execution=RunExecution.ARCHIVED,
                             search_mode=SearchControlMode.PAUSED)
    return next_state


def fold_events(run_id: str, events: Iterable[tuple[int, EventEnvelopeV2]],
                *, command_count: int = 0) -> CanonicalState:
    state = initial_state(run_id)
    for seq, event in events:
        state = apply_event(state, event, seq=seq)
    return replace(state, command_count=command_count)
