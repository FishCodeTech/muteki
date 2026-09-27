"""外部 Agent Runtime 契约（任务书 6.5、6.7）。

Adapter 负责发现、启动、恢复、控制、事件归一化、能力注入和退出管理；
模型调用、Agent Loop、上下文压缩、工具调用继续由外部 Runtime 负责。
"""

from __future__ import annotations

from datetime import datetime
from enum import Enum
from typing import Any, Optional

from pydantic import Field

from .base import ContractModel, new_id, utcnow


class AgentEventType(str, Enum):
    """统一 AgentEvent 事件类型（任务书 6.7，含 plan.updated）。"""

    SESSION_STARTED = "session.started"
    SESSION_RESUMED = "session.resumed"
    SESSION_CLOSED = "session.closed"
    TURN_STARTED = "turn.started"
    TURN_COMPLETED = "turn.completed"
    TURN_FAILED = "turn.failed"
    MESSAGE_DELTA = "message.delta"
    MESSAGE_COMPLETED = "message.completed"
    REASONING_SUMMARY = "reasoning.summary"
    TOOL_STARTED = "tool.started"
    TOOL_PROGRESS = "tool.progress"
    TOOL_COMPLETED = "tool.completed"
    APPROVAL_REQUESTED = "approval.requested"
    APPROVAL_RESOLVED = "approval.resolved"
    USER_INPUT_REQUESTED = "user_input.requested"
    USER_INPUT_RESOLVED = "user_input.resolved"
    ARTIFACT_CREATED = "artifact.created"
    WORKSPACE_CHANGED = "workspace.changed"
    USAGE_UPDATED = "usage.updated"
    PLAN_UPDATED = "plan.updated"
    AGENT_UPDATED = "agent.updated"
    RUNTIME_CAPABILITIES_UPDATED = "runtime.capabilities_updated"
    RUNTIME_WARNING = "runtime.warning"
    RUNTIME_ERROR = "runtime.error"
    RUNTIME_EXITED = "runtime.exited"


class AccessMode(str, Enum):
    """Conversation-facing autonomy contract.

    Runtime adapters own the translation from these stable product choices to
    their Agent's native permission and sandbox controls.  Worker launches do
    not set this field and therefore keep their existing unattended behavior.
    """

    SUPERVISED = "supervised"
    AUTO_ACCEPT_EDITS = "auto-accept-edits"
    AUTO = "auto"
    FULL_ACCESS = "full-access"


ACCESS_MODE_VALUES = tuple(mode.value for mode in AccessMode)


class AgentCapabilities(ContractModel):
    """Adapter probe 得到的 Runtime 能力（任务书 6.5 全量字段）。

    RunExecutor 和 Adapter 读取 capability 后决定功能；
    禁止根据 ``adapter.id`` 写隐式功能判断。
    """

    streaming: bool = False
    resume: bool = False
    steer: bool = False
    interrupt: bool = False
    approval: bool = False
    user_input: bool = False
    fork: bool = False
    structured_output: bool = False
    subagents: bool = False
    skills: bool = False
    mcp: bool = False
    native_tool_binding: bool = False
    acp_mcp_config: bool = False
    agent_plugin: bool = False
    # 历史 probe 字段；新 Adapter 只声明 agent_plugin。
    runtime_plugin_binding: bool = False
    structured_http_rpc: bool = False
    tool_events: bool = False
    usage_events: bool = False
    session_persistence: bool = False
    # Structured execution-plan / task-progress notifications (Codex
    # turn/plan/updated, ACP sessionUpdate.plan). Distinct from permission
    # mode alias "plan" and from CapabilityInjectionPlan.
    plan: bool = False
    # Native multimodal image input (Codex localImage/image, Claude image
    # content blocks, Pi prompt.images). When False, Conversation still
    # stages attachments into the workspace and injects explicit paths.
    image_input: bool = False
    supported_models: list[str] = Field(default_factory=list)
    supported_efforts: list[str] = Field(default_factory=list)
    access_modes: list[str] = Field(default_factory=list)
    # Legacy native controls remain readable for old Runtime records.  New
    # Conversation UI uses access_modes exclusively.
    permission_modes: list[str] = Field(default_factory=list)
    sandbox_modes: list[str] = Field(default_factory=list)
    # C27: Native context compaction (Claude /compact, Codex compact turn).
    # When True the adapter can handle a compact command; False/absent = unknown.
    compaction: bool = False
    # 传输形态，例如 cli / sdk / rpc / acp / http
    transport_kind: str = ""
    protocol_version: str = ""
    runtime_version: str = ""
    # 能力来源：probe（实测）| adapter_reported（声明）| static（保守默认）
    capability_source: str = "probe"


class ProbeRequest(ContractModel):
    """``ExternalAgentAdapter.probe`` 的探测请求。"""

    runtime_instance_id: Optional[str] = None
    # 是否枚举 supported_models 等较重信息
    include_models: bool = True
    options: dict[str, Any] = Field(default_factory=dict)


class SessionStart(ContractModel):
    """启动（或恢复）一个外部 Agent Session 的请求。"""

    agent_session_id: str = Field(default_factory=lambda: new_id("asess"))
    thread_id: Optional[str] = None
    run_id: Optional[str] = None
    execution_generation: Optional[int] = None
    workspace_id: Optional[str] = None
    # 恢复已有 Session 时携带的可恢复句柄
    resume_handle: Optional[str] = None
    model: Optional[str] = None
    effort: Optional[str] = None
    access_mode: Optional[str] = None
    permission_mode: Optional[str] = None
    sandbox_mode: Optional[str] = None
    options: dict[str, Any] = Field(default_factory=dict)


class AgentSessionRef(ContractModel):
    """Adapter 返回的会话引用。"""

    agent_session_id: str
    adapter_id: str = ""
    external_session_id: Optional[str] = None
    runtime_instance_id: Optional[str] = None
    resume_handle: Optional[str] = None


class AgentInput(ContractModel):
    """发送给 Agent Session 的一次输入（消息、steer、approval 答复等）。"""

    # message | steer | approval_response | user_input_response
    kind: str = "message"
    text: str = ""
    payload: dict[str, Any] = Field(default_factory=dict)


class AgentEvent(ContractModel):
    """统一 AgentEvent（任务书 6.7）。

    未知原生事件可以保存为 Adapter 私有事件放入 payload，
    但不能修改核心状态机。
    """

    event_type: AgentEventType
    agent_session_id: str = ""
    external_session_id: Optional[str] = None
    run_id: Optional[str] = None
    execution_generation: Optional[int] = None
    turn_id: Optional[str] = None
    # 会话内单调序号
    seq: int = 0
    occurred_at: datetime = Field(default_factory=utcnow)
    # 原生事件类型名，便于排障与私有事件回放
    native_type: Optional[str] = None
    # 完整事件负载
    payload: dict[str, Any] = Field(default_factory=dict)


class AgentSessionSnapshot(ContractModel):
    """Agent Session 当前状态快照。"""

    agent_session_id: str
    adapter_id: str = ""
    external_session_id: Optional[str] = None
    # active | idle | closed | error
    state: str = "active"
    current_turn_id: Optional[str] = None
    pending_approval: Optional[dict[str, Any]] = None
    pending_user_input: Optional[dict[str, Any]] = None
    usage: dict[str, Any] = Field(default_factory=dict)
    last_event_seq: int = 0
