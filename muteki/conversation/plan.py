"""Conversation execution-plan helpers (C20).

Normalizes Adapter / inject payloads into a versioned ThreadPlanSnapshot and
upserts tasks by stable task_id. Never derives completion from markdown todos.
"""

from __future__ import annotations

from typing import Any, Optional

from muteki.platform.contracts.base import utcnow

from .models import PlanTask, PlanTaskEvidence, ThreadPlanSnapshot

PLAN_PHASES = frozenset({
    "proposed",
    "executing",
    "awaiting_decision",
    "completed",
    "cleared",
    "unsupported",
})

TASK_STATUSES = frozenset({
    "pending",
    "in_progress",
    "completed",
    "blocked",
    "cancelled",
})


def _norm_status(value: Any, *, default: str = "pending") -> str:
    raw = str(value or default).strip().lower().replace("-", "_")
    aliases = {
        "todo": "pending",
        "open": "pending",
        "running": "in_progress",
        "inprogress": "in_progress",
        "active": "in_progress",
        "done": "completed",
        "complete": "completed",
        "finished": "completed",
        "failed": "blocked",
        "error": "blocked",
        "canceled": "cancelled",
    }
    mapped = aliases.get(raw, raw)
    return mapped if mapped in TASK_STATUSES else default


def _norm_phase(value: Any, *, default: str = "proposed") -> str:
    raw = str(value or default).strip().lower().replace("-", "_")
    aliases = {
        "proposal": "proposed",
        "suggest": "proposed",
        "running": "executing",
        "in_progress": "executing",
        "waiting": "awaiting_decision",
        "awaiting": "awaiting_decision",
        "decision": "awaiting_decision",
        "done": "completed",
        "complete": "completed",
        "clear": "cleared",
        "none": "unsupported",
        "unknown": "unsupported",
    }
    mapped = aliases.get(raw, raw)
    return mapped if mapped in PLAN_PHASES else default


def _evidence_list(raw: Any) -> list[PlanTaskEvidence]:
    if not isinstance(raw, list):
        return []
    out: list[PlanTaskEvidence] = []
    for item in raw:
        if not isinstance(item, dict):
            continue
        kind = str(item.get("kind") or "tool").strip() or "tool"
        evid_id = str(item.get("id") or item.get("call_id") or "").strip()
        if not evid_id:
            continue
        out.append(PlanTaskEvidence(
            kind=kind,
            id=evid_id,
            turn_id=str(item.get("turn_id") or "") or None,
        ))
    return out


def normalize_task(raw: Any, *, index: int = 0) -> Optional[PlanTask]:
    """Build a PlanTask from Adapter / inject dict; generate stable id if absent."""
    if isinstance(raw, str):
        title = raw.strip()
        if not title:
            return None
        return PlanTask(
            task_id=f"step-{index + 1}",
            title=title,
            status="pending",
            updated_at=utcnow(),
        )
    if not isinstance(raw, dict):
        return None
    title = str(
        raw.get("title")
        or raw.get("content")
        or raw.get("description")
        or raw.get("text")
        or raw.get("step")
        or ""
    ).strip()
    task_id = str(
        raw.get("task_id")
        or raw.get("id")
        or raw.get("step_id")
        or raw.get("item_id")
        or ""
    ).strip()
    if not task_id:
        if not title:
            return None
        task_id = f"step-{index + 1}"
    if not title:
        title = task_id
    return PlanTask(
        task_id=task_id,
        title=title,
        status=_norm_status(raw.get("status")),
        blocked_reason=str(raw.get("blocked_reason") or "") or None,
        evidence=_evidence_list(raw.get("evidence") or raw.get("evidence_refs")),
        updated_at=utcnow(),
    )


def extract_task_dicts(payload: dict[str, Any]) -> list[Any]:
    """Pull task-like rows from Codex / ACP / inject payloads."""
    for key in ("tasks", "entries", "steps", "items"):
        value = payload.get(key)
        if isinstance(value, list):
            return value
    plan = payload.get("plan")
    if isinstance(plan, dict):
        for key in ("tasks", "entries", "steps", "items"):
            value = plan.get(key)
            if isinstance(value, list):
                return value
    if isinstance(plan, list):
        return plan
    item = payload.get("item")
    if isinstance(item, dict):
        # item/plan/delta often wraps a single plan item.
        if any(k in item for k in ("title", "content", "status", "id", "task_id")):
            return [item]
        for key in ("tasks", "entries", "steps"):
            value = item.get(key)
            if isinstance(value, list):
                return value
    return []


def snapshot_from_payload(
    payload: dict[str, Any],
    *,
    previous: Optional[ThreadPlanSnapshot] = None,
    patch: bool = False,
) -> ThreadPlanSnapshot:
    """Build a versioned snapshot; patch mode upserts into previous tasks."""
    prev = previous or ThreadPlanSnapshot()
    incoming_revision = payload.get("revision")
    if incoming_revision is None:
        revision = max(1, int(prev.revision or 0) + 1)
    else:
        revision = max(0, int(incoming_revision))

    raw_tasks = extract_task_dicts(payload)
    normalized: list[PlanTask] = []
    for index, raw in enumerate(raw_tasks):
        task = normalize_task(raw, index=index)
        if task is not None:
            normalized.append(task)

    if patch and prev.tasks:
        by_id = {task.task_id: task for task in prev.tasks}
        for task in normalized:
            by_id[task.task_id] = task
        tasks = list(by_id.values())
    elif normalized:
        tasks = normalized
    else:
        tasks = list(prev.tasks)

    phase = _norm_phase(
        payload.get("phase"),
        default=prev.phase or ("proposed" if tasks else "cleared"),
    )
    if phase == "proposed" and any(
        task.status == "in_progress" for task in tasks
    ):
        phase = "executing"
    if phase in {"proposed", "executing"} and all(
        task.status in {"completed", "cancelled"} for task in tasks
    ) and tasks:
        phase = "completed"

    awaiting = payload.get("awaiting")
    if "awaiting" not in payload:
        awaiting = dict(prev.awaiting) if prev.awaiting else None
    elif awaiting is None:
        awaiting = None
    elif not isinstance(awaiting, dict):
        awaiting = {"kind": "user_input", "summary": str(awaiting)}

    pending_amendment = payload.get("pending_amendment")
    if "pending_amendment" not in payload:
        pending_amendment = (
            dict(prev.pending_amendment) if prev.pending_amendment else None
        )
    elif pending_amendment is None:
        pending_amendment = None
    elif not isinstance(pending_amendment, dict):
        pending_amendment = {"text": str(pending_amendment), "status": "unconfirmed"}

    title = str(payload.get("title") or prev.title or "").strip()
    source = str(payload.get("source") or prev.source or "adapter").strip() or "adapter"
    unsupported_reason = str(
        payload.get("unsupported_reason") or prev.unsupported_reason or ""
    ).strip()
    last_change = str(
        payload.get("last_change_summary")
        or payload.get("summary")
        or ""
    ).strip()
    if not last_change and normalized:
        last_change = f"更新 {len(normalized)} 个计划步骤"

    return ThreadPlanSnapshot(
        revision=revision,
        phase=phase,
        source=source,
        adapter_id=str(payload.get("adapter_id") or prev.adapter_id or "") or None,
        agent_session_id=str(
            payload.get("agent_session_id") or prev.agent_session_id or ""
        ) or None,
        turn_id=str(payload.get("turn_id") or prev.turn_id or "") or None,
        title=title or None,
        tasks=tasks,
        awaiting=awaiting,
        pending_amendment=pending_amendment,
        last_change_summary=last_change or None,
        unsupported_reason=unsupported_reason or None,
        updated_at=utcnow(),
    )


def upsert_plan(
    previous: Optional[ThreadPlanSnapshot],
    payload: dict[str, Any],
    *,
    patch: bool = False,
) -> Optional[ThreadPlanSnapshot]:
    """Apply a plan payload; drop stale revisions (equal+same → keep previous)."""
    prev = previous
    incoming_revision = payload.get("revision")
    if (
        prev is not None
        and incoming_revision is not None
        and int(incoming_revision) < int(prev.revision or 0)
    ):
        return prev
    next_snap = snapshot_from_payload(payload, previous=prev, patch=patch)
    if (
        prev is not None
        and next_snap.revision == prev.revision
        and next_snap.model_dump(mode="json") == prev.model_dump(mode="json")
    ):
        return prev
    return next_snap


def unsupported_snapshot(reason: str, *, previous: Optional[ThreadPlanSnapshot] = None) -> ThreadPlanSnapshot:
    """Freeze / replace with an explicit unsupported phase."""
    return ThreadPlanSnapshot(
        revision=max(1, int(previous.revision if previous else 0) + 1),
        phase="unsupported",
        source="none",
        tasks=[],
        unsupported_reason=reason,
        last_change_summary="Runtime 不支持结构化计划事件",
        updated_at=utcnow(),
    )


__all__ = [
    "PLAN_PHASES",
    "TASK_STATUSES",
    "extract_task_dicts",
    "normalize_task",
    "snapshot_from_payload",
    "unsupported_snapshot",
    "upsert_plan",
]
