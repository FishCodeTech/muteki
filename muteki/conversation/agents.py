"""Conversation agent-tree helpers (C21).

Builds a read-only parent/child Agent ownership snapshot from explicit
``core.agent.updated`` payloads (and CU inject). Ordinary shell / file tools
are never promoted to Agent nodes here — that classification lives in the
frontend event view when deriving from tool streams.
"""

from __future__ import annotations

from datetime import datetime
from typing import Any, Optional

from muteki.platform.contracts.base import utcnow

from .models import ConversationAgentNode, ThreadAgentTreeSnapshot

AGENT_STATUSES = frozenset({
    "pending",
    "running",
    "completed",
    "failed",
    "cancelled",
})

_SHELL_NAME_MARKERS = (
    "shell",
    "bash",
    "zsh",
    "terminal",
    "exec",
    "command",
    "run_terminal",
    "powershell",
)


def _looks_like_shell_tool(raw: dict[str, Any]) -> bool:
    """Reject ordinary shell/tool rows that lack explicit agent identity."""
    if raw.get("is_agent") is True or raw.get("isAgent") is True:
        return False
    if raw.get("agent_id") or raw.get("parent_id") or raw.get("parent_agent_id"):
        return False
    name = str(
        raw.get("title")
        or raw.get("name")
        or raw.get("tool")
        or raw.get("label")
        or ""
    ).strip().lower().replace("-", "_").replace(" ", "_")
    if not name:
        return False
    return any(marker in name for marker in _SHELL_NAME_MARKERS)


def _norm_status(value: Any, *, default: str = "pending") -> str:
    raw = str(value or default).strip().lower().replace("-", "_")
    aliases = {
        "todo": "pending",
        "open": "pending",
        "in_progress": "running",
        "inprogress": "running",
        "active": "running",
        "done": "completed",
        "complete": "completed",
        "finished": "completed",
        "error": "failed",
        "blocked": "failed",
        "canceled": "cancelled",
    }
    mapped = aliases.get(raw, raw)
    return mapped if mapped in AGENT_STATUSES else default


def normalize_agent(raw: Any, *, index: int = 0) -> Optional[ConversationAgentNode]:
    """Build one ConversationAgentNode from Adapter / inject dict."""
    if not isinstance(raw, dict):
        return None
    if _looks_like_shell_tool(raw):
        return None
    agent_id = str(
        raw.get("agent_id")
        or raw.get("id")
        or raw.get("call_id")
        or raw.get("tool_call_id")
        or ""
    ).strip()
    title = str(
        raw.get("title")
        or raw.get("name")
        or raw.get("label")
        or raw.get("description")
        or ""
    ).strip()
    if not agent_id:
        if not title:
            return None
        agent_id = f"agent-{index + 1}"
    if not title:
        title = agent_id
    parent_id = raw.get("parent_id") or raw.get("parent_agent_id") or raw.get("parent")
    parent_id = str(parent_id).strip() if parent_id else None
    if parent_id == "":
        parent_id = None
    request = raw.get("request")
    if request is not None and not isinstance(request, str):
        request = str(request)
    result = raw.get("result") or raw.get("output") or raw.get("summary")
    if result is not None and not isinstance(result, str):
        result = str(result)
    return ConversationAgentNode(
        agent_id=agent_id,
        parent_id=parent_id,
        title=title,
        nickname=_text(raw.get("nickname")),
        role=_text(raw.get("role")),
        model=_text(raw.get("model")),
        turn_id=_text(raw.get("turn_id")),
        message_id=_text(raw.get("message_id")),
        call_id=_text(raw.get("call_id") or raw.get("tool_call_id")),
        session_ref=_text(raw.get("session_ref")),
        status=_norm_status(raw.get("status")),
        request=request,
        result=result,
        error=_text(raw.get("error")),
        activity=_text(raw.get("activity")),
        tool_uses=_count(raw.get("tool_uses")),
        total_tokens=_count(raw.get("total_tokens")),
        duration_ms=_count(raw.get("duration_ms")),
        started_at=raw.get("started_at") or None,
        completed_at=raw.get("completed_at") or None,
        updated_at=utcnow(),
    )


def _text(value: Any) -> Optional[str]:
    text = str(value or "").strip()
    return text or None


def _count(value: Any) -> Optional[int]:
    if isinstance(value, bool) or value is None:
        return None
    try:
        number = int(value)
    except (TypeError, ValueError):
        return None
    return number if number >= 0 else None


_TERMINAL = frozenset({"completed", "failed", "cancelled"})


def _stamp(
    node: ConversationAgentNode,
    previous: Optional[ConversationAgentNode],
    at: Optional[datetime],
) -> ConversationAgentNode:
    """Lifecycle timestamps come from the event clock so replays stay stable."""
    if at is None:
        return node
    update: dict[str, Any] = {}
    reopened = (
        previous is not None
        and previous.status in _TERMINAL
        and node.status not in _TERMINAL
    )
    if node.started_at is None or reopened:
        update["started_at"] = (
            at if reopened or previous is None or previous.started_at is None
            else previous.started_at
        )
    if node.status in _TERMINAL:
        if node.completed_at is None:
            update["completed_at"] = (
                previous.completed_at
                if previous is not None and previous.status in _TERMINAL
                and previous.completed_at is not None
                else at
            )
    elif node.completed_at is not None:
        update["completed_at"] = None
    return node.model_copy(update=update) if update else node


def upsert_agent_tree(
    previous: Optional[ThreadAgentTreeSnapshot],
    payload: dict[str, Any],
    *,
    patch: bool = False,
    at: Optional[datetime] = None,
) -> ThreadAgentTreeSnapshot:
    """Build a versioned agent-tree snapshot; patch upserts by agent_id.

    Patch nodes merge field-by-field over the previous node: keys present in
    the incoming dict win (an explicit ``None`` clears), absent keys keep the
    previous value, so adapters can send partial lifecycle updates.
    """
    prev = previous or ThreadAgentTreeSnapshot()
    incoming_revision = payload.get("revision")
    if incoming_revision is None:
        revision = max(1, int(prev.revision or 0) + 1)
    else:
        revision = max(0, int(incoming_revision))
        if prev.revision and revision < prev.revision and not payload.get("force"):
            return prev

    raw_agents = payload.get("agents")
    if not isinstance(raw_agents, list):
        raw_agents = payload.get("nodes") if isinstance(payload.get("nodes"), list) else []

    prev_by_id = {agent.agent_id: agent for agent in prev.agents}
    normalized: list[ConversationAgentNode] = []
    for index, raw in enumerate(raw_agents):
        if patch and isinstance(raw, dict):
            raw_id = str(raw.get("agent_id") or raw.get("id") or "").strip()
            base = prev_by_id.get(raw_id)
            if base is not None:
                merged = base.model_dump(mode="python")
                merged.pop("updated_at", None)
                merged.update(raw)
                if base.turn_id:
                    # An agent belongs to the turn that spawned it, even when
                    # a background agent reports completion during a later turn.
                    merged["turn_id"] = base.turn_id
                raw = merged
        node = normalize_agent(raw, index=index)
        if node is not None:
            normalized.append(_stamp(node, prev_by_id.get(node.agent_id), at))

    if patch and prev.agents:
        by_id = dict(prev_by_id)
        for agent in normalized:
            by_id[agent.agent_id] = agent
        agents = list(by_id.values())
    elif normalized:
        agents = normalized
    else:
        agents = list(prev.agents)

    unsupported = bool(payload.get("unsupported")) or str(
        payload.get("phase") or ""
    ) == "unsupported"
    source = str(payload.get("source") or prev.source or "adapter")
    if unsupported:
        source = str(payload.get("source") or "none")
        agents = []

    return ThreadAgentTreeSnapshot(
        revision=revision,
        source=source,
        adapter_id=str(payload.get("adapter_id") or prev.adapter_id or "") or None,
        turn_id=str(payload.get("turn_id") or prev.turn_id or "") or None,
        agents=[] if unsupported else agents,
        unsupported=unsupported,
        unsupported_reason=(
            str(
                payload.get("unsupported_reason")
                or payload.get("reason")
                or prev.unsupported_reason
                or ""
            ).strip()
            or None
        ) if unsupported or prev.unsupported else (
            str(payload.get("unsupported_reason") or "").strip() or None
        ),
        tool_activity_summary=str(
            payload.get("tool_activity_summary")
            or payload.get("activity_summary")
            or prev.tool_activity_summary
            or ""
        ).strip() or None,
        updated_at=utcnow(),
    )


def unsupported_agent_tree(
    reason: str,
    *,
    previous: Optional[ThreadAgentTreeSnapshot] = None,
    activity: Optional[str] = None,
) -> ThreadAgentTreeSnapshot:
    prev = previous or ThreadAgentTreeSnapshot()
    return ThreadAgentTreeSnapshot(
        revision=max(1, int(prev.revision or 0) + 1),
        source="none",
        agents=[],
        unsupported=True,
        unsupported_reason=reason or "当前 Runtime 未上报委派 Agent 事件",
        tool_activity_summary=activity or prev.tool_activity_summary,
        updated_at=utcnow(),
    )
