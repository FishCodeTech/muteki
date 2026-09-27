"""Conversation 领域事件词汇与构造辅助（CONV-01，任务书 9.1）。

事件命名空间使用 ``core.*``（契约 events.py 未定义独立的 conversation
命名空间；builtin.conversation 的 event_namespaces 即 ``["core."]``）。
Thread 级事件都落在 ``(thread, thread_id)`` 聚合流上，SSE 恢复以该流的
stream_seq + 全局事件水位为游标。
"""

from __future__ import annotations

from typing import Any, Optional

from muteki.platform.contracts.base import new_id
from muteki.platform.contracts.events import EventEnvelope

PRODUCER = "builtin.conversation"

# -- Project / Workspace ------------------------------------------------------
EV_PROJECT_CREATED = "core.project.created"
EV_PROJECT_UPDATED = "core.project.updated"
EV_WORKSPACE_BOUND = "core.workspace.bound"

# -- Thread -------------------------------------------------------------------
EV_THREAD_CREATED = "core.thread.created"
EV_THREAD_RENAME_REQUESTED = "core.thread.rename_requested"
EV_THREAD_RENAMED = "core.thread.renamed"
EV_THREAD_METADATA_UPDATED = "core.thread.metadata_updated"
EV_THREAD_ARCHIVED = "core.thread.archived"
EV_THREAD_UNARCHIVED = "core.thread.unarchived"
EV_THREAD_RESUMED = "core.thread.resumed"
EV_THREAD_FORKED = "core.thread.forked"
EV_RUNTIME_SWITCHED = "core.thread.runtime_switched"
EV_WORKSPACE_CHANGED = "core.workspace.changed"

# -- Turn ---------------------------------------------------------------------
EV_TURN_REQUESTED = "core.turn.requested"
EV_TURN_STARTED = "core.turn.started"
EV_TURN_COMPLETED = "core.turn.completed"
EV_TURN_FAILED = "core.turn.failed"
EV_TURN_INTERRUPTED = "core.turn.interrupted"
EV_TURN_STEERED = "core.turn.steered"
EV_TURN_RETRIED = "core.turn.retried"
EV_TURN_EDIT_RESENT = "core.turn.edit_resent"
EV_TURN_REWOUND = "core.turn.rewound"

# -- 后续消息队列 ------------------------------------------------------------
EV_QUEUE_ADDED = "core.queue.added"
EV_QUEUE_UPDATED = "core.queue.updated"
EV_QUEUE_DELETED = "core.queue.deleted"
EV_QUEUE_REORDERED = "core.queue.reordered"
EV_QUEUE_PAUSED = "core.queue.paused"
EV_QUEUE_RESUMED = "core.queue.resumed"
EV_QUEUE_PROMOTED = "core.queue.promoted"
EV_QUEUE_DISPATCH_FAILED = "core.queue.dispatch_failed"

# -- 消息 / 工具 ----------------------------------------------------------------
EV_MESSAGE_DELTA = "core.message.delta"
EV_MESSAGE_COMPLETED = "core.message.completed"
EV_REASONING_SUMMARY = "core.reasoning.summary"
EV_TOOL_STARTED = "core.tool.started"
EV_TOOL_PROGRESS = "core.tool.progress"
EV_TOOL_COMPLETED = "core.tool.completed"

# -- 交互（approval / user input） ---------------------------------------------
EV_APPROVAL_REQUESTED = "core.approval.requested"
EV_APPROVAL_RESOLVED = "core.approval.resolved"
EV_USER_INPUT_REQUESTED = "core.user_input.requested"
EV_USER_INPUT_RESOLVED = "core.user_input.resolved"

# -- 结构化执行计划（Adapter 上报；非 markdown 臆测） ---------------------------
EV_PLAN_UPDATED = "core.plan.updated"
EV_PLAN_CLEARED = "core.plan.cleared"

# -- 委派 Agent 树（只读归属；非普通工具） --------------------------------------
EV_AGENT_UPDATED = "core.agent.updated"
EV_AGENT_CLEARED = "core.agent.cleared"

# -- Session / Runtime ----------------------------------------------------------
EV_SESSION_STARTED = "core.session.started"
EV_SESSION_RESUMED = "core.session.resumed"
EV_SESSION_CLOSED = "core.session.closed"
EV_SESSION_ERROR = "core.session.error"
EV_RUNTIME_EXITED = "core.runtime.exited"
EV_RUNTIME_EVENT = "core.runtime.event"
EV_RUNTIME_CAPABILITIES_UPDATED = "core.runtime.capabilities_updated"
EV_RUNTIME_OPERATION_REQUESTED = "core.runtime.operation_requested"
EV_RUNTIME_OPERATION_COMPLETED = "core.runtime.operation_completed"
EV_RUNTIME_WARNING = "core.runtime.warning"
EV_RUNTIME_ERROR = "core.runtime.error"

# -- Artifact / 用量 / 上下文窗口 -----------------------------------------------
EV_ARTIFACT_ATTACHED = "core.artifact.attached"
EV_ARTIFACT_CREATED = "core.artifact.created"
EV_USAGE_UPDATED = "core.usage.updated"
# C27: context-window fuel gauge (total, limit, zones, compacted, compact_status)
EV_CONTEXT_WINDOW = "core.context.window"

#: Thread 聚合类型常量。
AGGREGATE_THREAD = "thread"
AGGREGATE_PROJECT = "project"
AGGREGATE_WORKSPACE = "workspace"


def conversation_event(
    aggregate_type: str,
    aggregate_id: str,
    event_type: str,
    payload: dict[str, Any],
    *,
    actor_id: str = "system",
    command_id: Optional[str] = None,
    correlation_id: Optional[str] = None,
    idempotency_key: Optional[str] = None,
) -> EventEnvelope:
    """构造一条 Conversation 领域事件（统一 envelope）。"""
    return EventEnvelope(
        event_id=new_id("evt"),
        aggregate_type=aggregate_type,
        aggregate_id=aggregate_id,
        event_type=event_type,
        producer=PRODUCER,
        actor_id=actor_id or "system",
        command_id=command_id,
        causation_id=command_id,
        correlation_id=correlation_id or command_id,
        idempotency_key=idempotency_key,
        payload=payload,
    )


def thread_event(
    thread_id: str,
    event_type: str,
    payload: dict[str, Any],
    **kwargs: Any,
) -> EventEnvelope:
    """Thread 聚合流事件的便捷构造。"""
    return conversation_event(
        AGGREGATE_THREAD, thread_id, event_type, payload, **kwargs)


__all__ = [
    "AGGREGATE_PROJECT",
    "AGGREGATE_THREAD",
    "AGGREGATE_WORKSPACE",
    "EV_AGENT_CLEARED",
    "EV_AGENT_UPDATED",
    "EV_APPROVAL_REQUESTED",
    "EV_APPROVAL_RESOLVED",
    "EV_ARTIFACT_ATTACHED",
    "EV_ARTIFACT_CREATED",
    "EV_MESSAGE_COMPLETED",
    "EV_MESSAGE_DELTA",
    "EV_PLAN_CLEARED",
    "EV_PLAN_UPDATED",
    "EV_QUEUE_ADDED",
    "EV_QUEUE_DELETED",
    "EV_QUEUE_DISPATCH_FAILED",
    "EV_QUEUE_PAUSED",
    "EV_QUEUE_PROMOTED",
    "EV_QUEUE_REORDERED",
    "EV_QUEUE_RESUMED",
    "EV_QUEUE_UPDATED",
    "EV_REASONING_SUMMARY",
    "EV_PROJECT_CREATED",
    "EV_PROJECT_UPDATED",
    "EV_RUNTIME_ERROR",
    "EV_RUNTIME_CAPABILITIES_UPDATED",
    "EV_RUNTIME_OPERATION_COMPLETED",
    "EV_RUNTIME_OPERATION_REQUESTED",
    "EV_RUNTIME_EVENT",
    "EV_RUNTIME_EXITED",
    "EV_RUNTIME_WARNING",
    "EV_RUNTIME_SWITCHED",
    "EV_SESSION_CLOSED",
    "EV_SESSION_ERROR",
    "EV_SESSION_RESUMED",
    "EV_SESSION_STARTED",
    "EV_THREAD_ARCHIVED",
    "EV_THREAD_UNARCHIVED",
    "EV_THREAD_CREATED",
    "EV_THREAD_FORKED",
    "EV_THREAD_METADATA_UPDATED",
    "EV_THREAD_RENAME_REQUESTED",
    "EV_THREAD_RENAMED",
    "EV_THREAD_RESUMED",
    "EV_TOOL_COMPLETED",
    "EV_TOOL_PROGRESS",
    "EV_TOOL_STARTED",
    "EV_TURN_COMPLETED",
    "EV_TURN_FAILED",
    "EV_TURN_INTERRUPTED",
    "EV_TURN_RETRIED",
    "EV_TURN_EDIT_RESENT",
    "EV_TURN_REWOUND",
    "EV_TURN_REQUESTED",
    "EV_TURN_STARTED",
    "EV_TURN_STEERED",
    "EV_USAGE_UPDATED",
    "EV_USER_INPUT_REQUESTED",
    "EV_USER_INPUT_RESOLVED",
    "EV_WORKSPACE_BOUND",
    "EV_WORKSPACE_CHANGED",
    "PRODUCER",
    "conversation_event",
    "thread_event",
]
