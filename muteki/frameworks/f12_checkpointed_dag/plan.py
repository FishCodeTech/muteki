"""F12 source-coordinator reply parsing and full plan-revision validation.

Design baseline: docs/frameworks_2026/12_checkpointed_dag_swarm.md §4.3 / §6.

The parser is whitelist-strict: unknown top-level / task fields stay in the
raw reply (kept for audit on the source-turn row) but never enter the
execution projection. The validator enforces the完整 revision invariants:
unique ids, existing deps, acyclic, materialised-task immutability, schedule
references, and exclusive-resource / write-directory conflicts.
"""

from __future__ import annotations

import hashlib
import json
import re
from typing import Any

PLAN_SCHEMA_ID = "muteki.f12.checkpointed-dag-plan.v1"

GOAL_STATUS_VALUES = ("continue", "goal_satisfied", "more_work_required", "inconclusive")
REVIEW_MODES = ("self", "independent")
WORKER_CLASSES = ("code", "shell_agent", "verifier", "review")
TIERS = ("C0", "C1", "C2", "C3")

MAX_TASKS = 12
MAX_DEPS_PER_TASK = 8
MAX_SUCCESS_CRITERIA = 6
MAX_GOAL_LEN = 1200
MAX_SUMMARY_LEN = 2000
MAX_CRITERION_LEN = 400
MAX_ID_LEN = 64
MAX_TEXT_FIELD_LEN = 200
MAX_PINNED_FACTS = 32
MAX_REPLY_BYTES = 64_000

_TOP_LEVEL_KEYS = {
    "schema",
    "summary",
    "goal_status",
    "review_mode",
    "pinned_facts",
    "tasks",
    "schedule",
}
_TASK_KEYS = {
    "id",
    "goal",
    "worker_class",
    "depends_on",
    "success_criteria",
    "required_tier",
    "profile_hint",
    "execution_directory",
    "route_hash",
    "lane_key",
    "risk_class",
    "resource_key",
    "priority",
}
_TASK_ID_RE = re.compile(r"^[A-Za-z][A-Za-z0-9_.\-]{0,63}$")


class PlanParseError(ValueError):
    """A structurally invalid source reply (never applied)."""


def canonical_plan_json(plan: dict[str, Any]) -> str:
    """Canonical JSON for hashing: sorted keys, tight separators."""
    return json.dumps(plan, ensure_ascii=False, sort_keys=True, separators=(",", ":"))


def plan_sha256(plan: dict[str, Any]) -> str:
    return hashlib.sha256(canonical_plan_json(plan).encode("utf-8")).hexdigest()


def _clean_str(value: Any, max_len: int) -> str:
    return str(value or "").strip()[:max_len]


def parse_source_reply(raw: str) -> dict[str, Any]:
    """Parse + whitelist-bound one source-coordinator reply.

    Returns a normalised plan dict. Raises PlanParseError on any contract
    violation; the raw reply stays on the f12_source_turn row for audit.
    """
    text = str(raw or "").strip()
    if not text:
        raise PlanParseError("empty reply")
    if len(text.encode("utf-8", errors="replace")) > MAX_REPLY_BYTES:
        raise PlanParseError(f"reply exceeds {MAX_REPLY_BYTES} bytes")
    m = re.search(r"\{.*\}", text, flags=re.DOTALL)
    if not m:
        raise PlanParseError("no JSON object found")
    try:
        obj = json.loads(m.group(0))
    except json.JSONDecodeError as exc:
        raise PlanParseError(f"invalid JSON: {exc}") from exc
    if not isinstance(obj, dict):
        raise PlanParseError("reply is not a JSON object")
    unknown = sorted(set(obj) - _TOP_LEVEL_KEYS)
    if unknown:
        raise PlanParseError(f"unknown top-level fields: {unknown}")
    if str(obj.get("schema") or "") != PLAN_SCHEMA_ID:
        raise PlanParseError(f"schema must be {PLAN_SCHEMA_ID!r}")

    goal_status = _clean_str(obj.get("goal_status"), 40) or "continue"
    if goal_status not in GOAL_STATUS_VALUES:
        raise PlanParseError(f"goal_status must be one of {GOAL_STATUS_VALUES}")
    review_mode = _clean_str(obj.get("review_mode"), 40) or "self"
    if review_mode not in REVIEW_MODES:
        raise PlanParseError(f"review_mode must be one of {REVIEW_MODES}")

    pinned_facts: list[int] = []
    for item in list(obj.get("pinned_facts") or [])[:MAX_PINNED_FACTS]:
        if isinstance(item, bool):
            continue
        if isinstance(item, (int, float)) and int(item) > 0:
            pinned_facts.append(int(item))

    raw_tasks = obj.get("tasks")
    if not isinstance(raw_tasks, list):
        raise PlanParseError("tasks must be a list")
    if len(raw_tasks) > MAX_TASKS:
        raise PlanParseError(f"at most {MAX_TASKS} tasks per revision")
    tasks: list[dict[str, Any]] = []
    for i, raw in enumerate(raw_tasks):
        if not isinstance(raw, dict):
            raise PlanParseError(f"tasks[{i}] is not an object")
        unknown_t = sorted(set(raw) - _TASK_KEYS)
        if unknown_t:
            raise PlanParseError(f"tasks[{i}] unknown fields: {unknown_t}")
        task_id = _clean_str(raw.get("id"), MAX_ID_LEN)
        if not task_id or not _TASK_ID_RE.match(task_id):
            raise PlanParseError(f"tasks[{i}].id invalid: {task_id!r}")
        goal = _clean_str(raw.get("goal"), MAX_GOAL_LEN)
        if not goal:
            raise PlanParseError(f"task {task_id}: empty goal")
        worker_class = _clean_str(raw.get("worker_class"), 40) or "code"
        if worker_class not in WORKER_CLASSES:
            raise PlanParseError(
                f"task {task_id}: worker_class must be one of {WORKER_CLASSES}"
            )
        required_tier = _clean_str(raw.get("required_tier"), 8) or "C1"
        if required_tier not in TIERS:
            raise PlanParseError(
                f"task {task_id}: required_tier must be one of {TIERS}"
            )
        depends_on: list[str] = []
        for dep in list(raw.get("depends_on") or [])[:MAX_DEPS_PER_TASK]:
            dep_id = _clean_str(dep, MAX_ID_LEN)
            if dep_id and dep_id not in depends_on:
                depends_on.append(dep_id)
        success_criteria = [
            _clean_str(c, MAX_CRITERION_LEN)
            for c in list(raw.get("success_criteria") or [])[:MAX_SUCCESS_CRITERIA]
            if _clean_str(c, MAX_CRITERION_LEN)
        ]
        try:
            priority = int(raw.get("priority") or 0)
        except (TypeError, ValueError):
            priority = 0
        priority = max(-100, min(100, priority))
        tasks.append(
            {
                "id": task_id,
                "goal": goal,
                "worker_class": worker_class,
                "depends_on": depends_on,
                "success_criteria": success_criteria,
                "required_tier": required_tier,
                "profile_hint": _clean_str(raw.get("profile_hint"), MAX_TEXT_FIELD_LEN),
                "execution_directory": _clean_str(
                    raw.get("execution_directory"), MAX_TEXT_FIELD_LEN
                ),
                "route_hash": _clean_str(raw.get("route_hash"), MAX_TEXT_FIELD_LEN),
                "lane_key": _clean_str(raw.get("lane_key"), MAX_TEXT_FIELD_LEN),
                "risk_class": _clean_str(raw.get("risk_class"), 60),
                "resource_key": _clean_str(raw.get("resource_key"), MAX_TEXT_FIELD_LEN),
                "priority": priority,
            }
        )

    schedule: list[str] = []
    raw_schedule = obj.get("schedule")
    if not isinstance(raw_schedule, list):
        raise PlanParseError("schedule must be a list")
    for item in raw_schedule[:MAX_TASKS]:
        tid = _clean_str(item, MAX_ID_LEN)
        if tid and tid not in schedule:
            schedule.append(tid)

    return {
        "schema": PLAN_SCHEMA_ID,
        "summary": _clean_str(obj.get("summary"), MAX_SUMMARY_LEN),
        "goal_status": goal_status,
        "review_mode": review_mode,
        "pinned_facts": pinned_facts,
        "tasks": tasks,
        "schedule": schedule,
    }


# ---------------------------------------------------------------------------
# full-revision validation against the materialised task store (design §4.3)
# ---------------------------------------------------------------------------


def _find_cycle(tasks: dict[str, dict[str, Any]]) -> list[str] | None:
    """Return one dependency cycle (list of task ids) or None."""
    WHITE, GRAY, BLACK = 0, 1, 2
    color = {tid: WHITE for tid in tasks}
    stack: list[str] = []

    def dfs(node: str) -> list[str] | None:
        color[node] = GRAY
        stack.append(node)
        for dep in tasks[node]["depends_on"]:
            if dep not in tasks:
                continue
            if color[dep] == GRAY:
                return stack[stack.index(dep):] + [dep]
            if color[dep] == WHITE:
                found = dfs(dep)
                if found:
                    return found
        stack.pop()
        color[node] = BLACK
        return None

    for tid in tasks:
        if color[tid] == WHITE:
            found = dfs(tid)
            if found:
                return found
    return None


def validate_revision(
    plan: dict[str, Any],
    *,
    existing_tasks: list[dict[str, Any]],
    open_task_states: tuple[str, ...] = ("pending", "scheduled", "running"),
) -> list[str]:
    """Validate a COMPLETE plan revision against already-materialised tasks.

    Returns a list of human-readable violations; empty means valid. The rules
    implement design §4.3 1–8:

    1. task ids unique;
    2. all depends_on exist in the revision;
    3. no self-dependency, no cycles;
    4. tasks that are scheduled/running/terminal must be present (terminal or
       in-flight work may not silently vanish from the plan);
    5. materialised tasks may not change goal / worker_class / depends_on in
       place — a changed meaning needs a NEW task id;
    6. schedule may only reference tasks of this revision;
    7. (profile/directory snapshot checks happen at the router/dispatch layer;
       here we check structural conflicts only)
    8. parallel-selected tasks must not declare the same exclusive resource or
       the same write directory.
    """
    errors: list[str] = []
    tasks = {str(t.get("id")): t for t in plan.get("tasks") or []}
    if len(tasks) != len(plan.get("tasks") or []):
        errors.append("duplicate task id in revision")

    # 2. deps exist (within the revision's own task set)
    for tid, task in tasks.items():
        for dep in task.get("depends_on") or []:
            if dep not in tasks:
                errors.append(f"task {tid}: unknown dependency {dep!r}")
        # 3a. self dependency
        if tid in (task.get("depends_on") or []):
            errors.append(f"task {tid}: self dependency")

    # 3b. cycles
    if not any("unknown dependency" in e for e in errors):
        cycle = _find_cycle(tasks)
        if cycle:
            errors.append(f"dependency cycle: {' -> '.join(cycle)}")

    # 4/5. carry-over + immutability of materialised tasks
    existing_by_id = {str(t.get("task_id")): t for t in existing_tasks}
    for task_id, prior in existing_by_id.items():
        prior_status = str(prior.get("status") or "pending")
        new = tasks.get(task_id)
        if new is None:
            if prior_status in ("scheduled", "running") or prior_status not in open_task_states:
                errors.append(
                    f"task {task_id}: {prior_status} task omitted from revision"
                )
            continue
        if str(prior.get("goal") or "") != str(new.get("goal") or ""):
            errors.append(f"task {task_id}: goal changed in place")
        if str(prior.get("worker_class") or "") != str(new.get("worker_class") or ""):
            errors.append(f"task {task_id}: worker_class changed in place")
        if sorted(str(d) for d in prior.get("depends_on") or []) != sorted(
            str(d) for d in new.get("depends_on") or []
        ):
            errors.append(f"task {task_id}: depends_on changed in place")

    # 6. schedule references
    for tid in plan.get("schedule") or []:
        if tid not in tasks:
            errors.append(f"schedule references unknown task {tid!r}")
    scheduled_terminal = [
        tid
        for tid in plan.get("schedule") or []
        if tid in existing_by_id
        and str(existing_by_id[tid].get("status") or "")
        not in ("pending", "scheduled")
    ]
    for tid in scheduled_terminal:
        errors.append(
            f"schedule selects task {tid} whose status is "
            f"{existing_by_id[tid].get('status')} (only pending tasks start)"
        )

    # 8. exclusive resource / write-directory conflicts among the selected set
    selected = [tid for tid in plan.get("schedule") or [] if tid in tasks]
    seen_resources: dict[str, str] = {}
    seen_dirs: dict[str, str] = {}
    for tid in selected:
        task = tasks[tid]
        deps = set(task.get("depends_on") or [])
        resource = str(task.get("resource_key") or "")
        if resource:
            other = seen_resources.get(resource)
            # tasks ordered by a dependency edge never run in parallel
            if other is not None and other not in deps and tid not in (
                tasks[other].get("depends_on") or []
            ):
                errors.append(
                    f"tasks {other} and {tid} share exclusive resource {resource!r}"
                )
            else:
                seen_resources.setdefault(resource, tid)
        write_dir = str(task.get("execution_directory") or "")
        if write_dir:
            other = seen_dirs.get(write_dir)
            if other is not None and other not in deps and tid not in (
                tasks[other].get("depends_on") or []
            ):
                errors.append(
                    f"tasks {other} and {tid} share write directory {write_dir!r}"
                )
            else:
                seen_dirs.setdefault(write_dir, tid)
    return errors


def dependency_status(
    task: dict[str, Any], tasks_by_id: dict[str, dict[str, Any]]
) -> str:
    """Read-model dependency state for one task (design §5.3: blocked is
    computed, not stored).

    Returns 'ready' (every dep succeeded), 'waiting' (some dep still open), or
    'blocked' (some dep terminal-but-not-succeeded).
    """
    saw_open = False
    for dep in task.get("depends_on") or []:
        dep_row = tasks_by_id.get(str(dep))
        if dep_row is None:
            return "blocked"
        status = str(dep_row.get("status") or "")
        if status == "succeeded":
            continue
        if status in ("failed", "cancelled", "inconclusive"):
            return "blocked"
        saw_open = True
    return "waiting" if saw_open else "ready"
