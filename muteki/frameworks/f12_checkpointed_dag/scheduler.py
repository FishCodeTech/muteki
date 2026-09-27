"""F12 exact ready-set scheduling (design §4.5) + effect/speed policy (§7).

Only tasks the source coordination agent selected into the CURRENT schedule
enter the F12 dispatchable intent view. A task must additionally be:

- status ``scheduled`` (selected this round, not yet started);
- all dependency tasks ``succeeded`` (a failed dep leaves successors blocked
  until the source revises or cancels them — never auto-started);
- on the current accepted revision;
- on the current execution generation;
- within the speed-derived concurrency target (actual concurrency stays
  bounded by dependencies, coordinator capacity, budget and isolation).

System-level intents (review pipeline, report reproduction, operator
continuations, control intents) are identified STRUCTURALLY — they are simply
not F12-managed — and pass the filter unchanged. No text matching on goals.
"""

from __future__ import annotations

from typing import Any

from muteki.frameworks.f12_checkpointed_dag import schema as store
from muteki.frameworks.f12_checkpointed_dag.plan import dependency_status

# §7.1 effect → minimum capability tier
EFFECT_TIER_FLOOR = ((20, "C0"), (60, "C1"), (85, "C2"), (101, "C3"))
TIER_ORDER = {"C0": 0, "C1": 1, "C2": 2, "C3": 3}

# §7.2 speed → desired concurrency target
SPEED_CONCURRENCY = ((25, 1), (50, 2), (75, 3), (101, 4))


def effect_tier_floor(effect: int) -> str:
    for bound, tier in EFFECT_TIER_FLOOR:
        if int(effect) < bound:
            return tier
    return "C3"


def speed_concurrency_target(speed: int) -> int:
    for bound, n in SPEED_CONCURRENCY:
        if int(speed) < bound:
            return n
    return 4


def task_effective_tier(task: dict[str, Any], effect: int) -> str:
    """Task tier = max(task's own requirement, effect floor) (§7.1)."""
    own = str(task.get("required_tier") or "C1")
    floor = effect_tier_floor(effect)
    return own if TIER_ORDER.get(own, 1) >= TIER_ORDER.get(floor, 1) else floor


def f12_intent_ids(graph: Any, execution_id: str) -> set[str]:
    """Every intent id the F12 execution manages (any revision, any state)."""
    return {str(t.get("intent_id")) for t in store.list_tasks(graph, execution_id)}


def ready_scheduled_tasks(
    graph: Any,
    *,
    execution_id: str,
    active_revision_id: str,
    generation: int,
    concurrency_target: int,
) -> list[dict[str, Any]]:
    """The exact dispatchable F12 task set for this tick.

    Order: priority desc, then task id (deterministic). Bounded by the
    concurrency target's remaining seats (running F12 tasks count against it).
    """
    tasks = store.list_tasks(graph, execution_id)
    by_id = {t["task_id"]: t for t in tasks}
    running = sum(1 for t in tasks if str(t.get("status")) == "running")
    seats = max(0, int(concurrency_target) - running)
    if seats <= 0:
        return []
    ready: list[dict[str, Any]] = []
    for task in tasks:
        if str(task.get("status")) != "scheduled":
            continue
        if str(task.get("revision_id")) != str(active_revision_id):
            continue
        if int(task.get("generation") or 1) != int(generation):
            continue
        if dependency_status(task, by_id) != "ready":
            continue
        ready.append(task)
    ready.sort(key=lambda t: (-int(t.get("priority") or 0), str(t.get("task_id"))))
    return ready[:seats]


def blocked_tasks(
    graph: Any, *, execution_id: str
) -> list[dict[str, Any]]:
    """Tasks whose deps terminally failed — they wait for the source agent to
    revise or cancel them (never auto-dispatched)."""
    tasks = store.list_tasks(graph, execution_id)
    by_id = {t["task_id"]: t for t in tasks}
    out = []
    for task in tasks:
        if str(task.get("status")) not in ("pending", "scheduled"):
            continue
        if dependency_status(task, by_id) == "blocked":
            out.append(task)
    return out


def filter_open_intents(
    graph: Any,
    base_intents: list[dict[str, Any]],
    *,
    execution_id: str,
    active_revision_id: str,
    generation: int,
    concurrency_target: int,
) -> list[dict[str, Any]]:
    """Apply the F12 exact ready-set gate to the coordinator's open-intent
    view. Non-F12 intents pass through untouched; F12-managed intents are
    visible only while selected+ready."""
    managed = f12_intent_ids(graph, execution_id)
    if not managed:
        return list(base_intents)
    ready = ready_scheduled_tasks(
        graph,
        execution_id=execution_id,
        active_revision_id=active_revision_id,
        generation=generation,
        concurrency_target=concurrency_target,
    )
    visible = {str(t.get("intent_id")) for t in ready}
    out: list[dict[str, Any]] = []
    for intent in base_intents:
        iid = str(intent.get("intent_id") or "")
        if iid in managed and iid not in visible:
            continue
        out.append(intent)
    return out
