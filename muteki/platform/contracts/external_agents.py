"""外部 Agent Runtime 契约（任务书 6.5、6.7）。

Adapter 负责发现、启动、恢复、控制、事件归一化、能力注入和退出管理；
模型调用、Agent Loop、上下文压缩、工具调用继续由外部 Runtime 负责。
"""

from __future__ import annotations

from datetime import datetime
from enum import Enum
from typing import Annotated, Any, Literal, Optional, Union

from pydantic import Field, TypeAdapter, model_validator

from .base import ContractModel, new_id, utcnow
from .capabilities import ThreadMode, ToolDescription


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
    # False: reopening adopts native history; the caller must send a new
    # continuation prompt. True: adapter.resume() executes that prompt itself.
    resume_continues_turn: bool = True
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
    # Native read-only planning turn mode selected by
    # ``SessionStart.interaction_mode == "plan"`` (Codex collaborationMode,
    # Claude permissionMode "plan", Cursor/OpenCode plan agent).
    plan_mode: bool = False
    # False: plan mode can only be applied by (re)starting the session, so
    # Conversation restarts the session when the thread's interaction mode
    # changes. True: the adapter honors ``MessagePayload.interaction_mode`` on
    # every turn of a live session.
    plan_mode_per_turn: bool = True
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


#: Stable machine code raised when a SessionStart carries an option no adapter reads.
SESSION_OPTIONS_UNKNOWN_KEY_CODE = "external_agent.session_options.unknown_key"


class SessionOptionsError(Exception):
    """SessionStart.options contains keys outside the SessionOptions contract.

    Deliberately not a ``ValueError``: pydantic would fold it into a generic
    ValidationError and the stable ``code`` would be lost to callers.
    """

    def __init__(self, unknown_keys: list[str]) -> None:
        self.code = SESSION_OPTIONS_UNKNOWN_KEY_CODE
        self.unknown_keys = sorted(unknown_keys)
        super().__init__(
            f"{self.code}: unsupported session option(s): {', '.join(self.unknown_keys)}")


class SessionOptions(ContractModel):
    """Every launch option an adapter may read from ``SessionStart.options``.

    Fields are grouped by owner. Adapters read only the fields they support;
    unknown keys raise :class:`SessionOptionsError` instead of being ignored.
    """

    # Capability binding (BaseExternalAgentAdapter.start).
    principal_id: Optional[str] = None
    thread_mode: Optional[ThreadMode] = None
    resource_scopes: Optional[list[str]] = None
    audience: Optional[str] = None
    # Process environment shared by every adapter.
    cwd: str = ""
    env: dict[str, str] = Field(default_factory=dict)
    # Conversation chat plugin tools; None means the caller did not prepare any.
    chat_tools: Optional[list[ToolDescription]] = None
    chat_control_enabled: bool = True
    # Prompt delivered by adapters that must restart a closed native session.
    resume_prompt: str = ""
    # Claude Agent SDK.
    plugins: list[dict[str, Any]] = Field(default_factory=list)
    skills: Union[Literal["all"], list[str], None] = "all"
    hooks: dict[str, Any] = Field(default_factory=dict)
    allowed_tools: list[str] = Field(default_factory=list)
    max_turns: Optional[int] = None
    fork_session: bool = False
    # Codex App Server.
    chat_native_plugins: list[dict[str, str]] = Field(default_factory=list)
    chat_hook_approvals: dict[str, Any] = Field(default_factory=dict)
    fork_from: str = ""
    fork_last_turn_id: str = ""
    # Pi / OMP RPC.
    role: str = ""
    add_dirs: list[str] = Field(default_factory=list)
    ephemeral: bool = False
    session_dir: str = ""
    # OpenCode.
    title: Optional[str] = None
    # Solver CLI driver.
    timeout_s: Optional[float] = None
    web_access: bool = True
    kb_access: bool = True
    prompt_via_stdin: bool = False
    # Swarm worker session record.
    solver_id: str = ""
    worker_mode: str = ""
    engine: str = ""

    @model_validator(mode="before")
    @classmethod
    def _reject_unknown_keys(cls, data: Any) -> Any:
        if isinstance(data, dict):
            unknown = [key for key in data if key not in cls.model_fields]
            if unknown:
                raise SessionOptionsError(unknown)
        return data

    @property
    def is_conversation(self) -> bool:
        return self.thread_mode is ThreadMode.CONVERSATION


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
    # Native service tier id from the model catalog (Codex ``serviceTier``).
    service_tier: Optional[str] = None
    access_mode: Optional[str] = None
    # "plan" requires AgentCapabilities.plan_mode; worker launches leave it "default".
    interaction_mode: Literal["default", "plan"] = "default"
    permission_mode: Optional[str] = None
    sandbox_mode: Optional[str] = None
    options: SessionOptions = Field(default_factory=SessionOptions)


class AgentSessionRef(ContractModel):
    """Adapter 返回的会话引用。"""

    agent_session_id: str
    adapter_id: str = ""
    external_session_id: Optional[str] = None
    runtime_instance_id: Optional[str] = None
    resume_handle: Optional[str] = None


class AttachmentRef(ContractModel):
    """A resolved, authorized attachment ready for adapter delivery."""

    sha256: str
    name: str = ""
    media_type: str = ""
    size: int = 0
    cas_path: str = ""
    path: str = ""
    workspace_path: str = ""
    delivery: Literal["native_image", "workspace_file"] = "workspace_file"
    content_base64: str = ""


class MessagePayload(ContractModel):
    """Structured side data of an ordinary user message."""

    attachments: list[AttachmentRef] = Field(default_factory=list)
    # Extra reference context a CLI driver prepends to the prompt.
    capability_context: str = ""
    # Selected runtime capability item (RuntimeCapabilityItem dump plus
    # ``arguments`` / ``revision``); empty when the user typed plain text.
    runtime_capability: dict[str, Any] = Field(default_factory=dict)
    runtime_command_arguments: str = ""
    client_user_message_id: str = ""
    # Per-turn planning mode; "plan" requires AgentCapabilities.plan_mode.
    interaction_mode: Literal["default", "plan"] = "default"


class SteerPayload(ContractModel):
    expected_turn_id: str = ""
    client_user_message_id: str = ""
    capability_revision: Optional[int] = None
    attachments: list[AttachmentRef] = Field(default_factory=list)


class ApprovalResponsePayload(ContractModel):
    approval_id: str = Field(min_length=1)
    decision: Literal["allow", "deny"]
    scope: Literal["once", "session"] = "once"
    note: str = ""
    option_id: str = ""
    # Claude can_use_tool lets the operator rewrite tool input on allow.
    updated_input: Optional[dict[str, Any]] = None


class UserInputResponsePayload(ContractModel):
    request_id: str = Field(min_length=1)
    decision: Literal["submit", "decline", "cancel"] = "submit"
    answers: dict[str, Any] = Field(default_factory=dict)


class AgentInput(ContractModel):
    """发送给 Agent Session 的一次输入；按 ``kind`` 区分的联合类型。

    ``AgentInput(kind=..., ...)`` and ``AgentInput.model_validate(...)``
    return the concrete variant, so adapters dispatch with ``isinstance``
    and read typed payload attributes. ``kind`` defaults to ``message``.
    """

    def __new__(cls, *args: Any, **data: Any) -> "AgentInput":
        if cls is AgentInput:
            return _AGENT_INPUT_ADAPTER.validate_python({"kind": "message", **data})
        return super().__new__(cls)

    @classmethod
    def model_validate(cls, obj: Any, **kwargs: Any) -> "AgentInput":  # type: ignore[override]
        if cls is AgentInput:
            if isinstance(obj, dict):
                obj = {"kind": "message", **obj}
            return _AGENT_INPUT_ADAPTER.validate_python(obj, **kwargs)
        return super().model_validate(obj, **kwargs)


class MessageInput(AgentInput):
    """An ordinary user message that starts a new turn."""

    kind: Literal["message"] = "message"
    text: str = ""
    payload: MessagePayload = Field(default_factory=MessagePayload)


class SteerInput(AgentInput):
    """Extra guidance appended to the running turn."""

    kind: Literal["steer"] = "steer"
    text: str = ""
    payload: SteerPayload = Field(default_factory=SteerPayload)


class ApprovalResponseInput(AgentInput):
    """Operator verdict for a pending approval request."""

    kind: Literal["approval_response"] = "approval_response"
    text: str = ""
    payload: ApprovalResponsePayload


class UserInputResponseInput(AgentInput):
    """Operator answers for a pending user-input request."""

    kind: Literal["user_input_response"] = "user_input_response"
    # Free-text answer when the request has no structured questions.
    text: str = ""
    payload: UserInputResponsePayload


AgentInputVariant = Annotated[
    Union[MessageInput, SteerInput, ApprovalResponseInput, UserInputResponseInput],
    Field(discriminator="kind"),
]
_AGENT_INPUT_ADAPTER: TypeAdapter[AgentInput] = TypeAdapter(AgentInputVariant)


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
