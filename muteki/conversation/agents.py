"""Conversation agent-tree helpers (C21).

Builds a read-only parent/child Agent ownership snapshot from explicit
``core.agent.updated`` payloads (and CU inject). Ordinary shell / file tools
are never promoted to Agent nodes here — that classification lives in the
frontend event view when deriving from tool streams.
"""

from __future__ import annotations

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
        model=str(raw.get("model") or "").strip() or None,
        turn_id=str(raw.get("turn_id") or "").strip() or None,
        message_id=str(raw.get("message_id") or "").strip() or None,
        call_id=str(raw.get("call_id") or raw.get("tool_call_id") or "").strip() or None,
        status=_norm_status(raw.get("status")),
        request=request,
        result=result,
        error=str(raw.get("error") or "").strip() or None,
        updated_at=utcnow(),
    )


def upsert_agent_tree(
    previous: Optional[ThreadAgentTreeSnapshot],
    payload: dict[str, Any],
    *,
    patch: bool = False,
) -> ThreadAgentTreeSnapshot:
    """Build a versioned agent-tree snapshot; patch upserts by agent_id."""
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

    normalized: list[ConversationAgentNode] = []
    for index, raw in enumerate(raw_agents):
        node = normalize_agent(raw, index=index)
        if node is not None:
            normalized.append(node)

    if patch and prev.agents:
        by_id = {agent.agent_id: agent for agent in prev.agents}
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
