"""Conversation 领域模型（CONV-01，任务书 9.1）。

Platform 通用对象（Project / Workspace / Thread / Task / AgentSession /
Artifact）复用 ``muteki.platform.contracts.objects``；这里只定义
Conversation 私有的读模型与记录：

- ``TurnRecord``：一次用户发起的回合（message / steer / resume），
  幂等键为 (thread_id, idempotency_key)，网络重试不产生第二个 Turn；
- ``TurnRunRef``：一个 Turn 对应的 Task / Run / 执行代（generation）记录。
  RunRef 不在 CORE-02 对象表内，Conversation 用自己的表保存；
- ``ThreadRuntimeSelection``：Thread 的 Runtime instance / 模型 / effort /
  权限模式选择（Thread 契约不含这些字段，存在 Conversation 自己的表）；
- ``ConversationMessage``：投影出的对话消息（user / assistant）；
- ``ThreadState``：Thread 读模型（运行中、待审批、unread、usage 等）。
"""

from __future__ import annotations

from datetime import datetime
from typing import Any, Literal, Optional

from pydantic import Field, model_validator

from muteki.platform.contracts.base import ContractModel, new_id, utcnow
from muteki.platform.contracts.external_agents import AccessMode

#: 后台 Runtime 能力刷新失败的稳定错误码（不属于任何 Turn）。
CAPABILITY_REFRESH_FAILED_CODE = "conversation.runtime.capability_refresh_failed"

#: Per-thread / per-turn interaction mode (mirrors ``SessionStart.interaction_mode``).
InteractionMode = Literal["default", "plan"]

#: Turn 状态机。
TURN_QUEUED = "queued"
TURN_RUNNING = "running"
TURN_COMPLETED = "completed"
TURN_FAILED = "failed"
TURN_INTERRUPTED = "interrupted"
TURN_CANCELLED = "cancelled"
TURN_SUPERSEDED = "superseded"

#: Turn 种类。
TURN_KIND_MESSAGE = "message"
TURN_KIND_RESUME = "resume"
TURN_KIND_RETRY = "retry"
TURN_KIND_EDIT_RESEND = "edit_resend"
TURN_KIND_NATIVE_REWIND = "native_rewind"

#: 对话执行器 id（任务书 9.1：builtin.conversation 的 default_executor）。
EXECUTOR_ID = "external-agent.single"

#: Agent node origins shared by native delegation trees and Muteki subagents.
SUBAGENT_ORIGIN_NATIVE = "provider_native"
SUBAGENT_ORIGIN_APP = "app_owned"


class TurnRecord(ContractModel):
    """一次回合：用户消息 → Runtime 执行 → 完成 / 失败 / 中断。"""

    turn_id: str = Field(default_factory=lambda: new_id("turn"))
    thread_id: str = ""
    task_id: Optional[str] = None
    run_id: Optional[str] = None
    agent_session_id: Optional[str] = None
    execution_generation: Optional[int] = None
    # 触发命令 id 与 Turn 级幂等键（重试不重复创建）
    command_id: str = ""
    idempotency_key: Optional[str] = None
    # 线程内单调序号（从 1 开始）
    seq: int = 0
    kind: str = TURN_KIND_MESSAGE
    # A retry creates a replacement Turn on the active branch.  The original
    # Turn stays in the immutable audit history with ``status=superseded``.
    retry_of_turn_id: Optional[str] = None
    text: str = ""
    # 附带 Artifact 的 sha256 清单
    attachments: list[str] = Field(default_factory=list)
    # 输入框显式选择的 MCP / Plugin / 项目文件 / 对话引用，以及 C09
    # schema-v2 结构化上下文节点（locator + snapshot + status）。
    # Skill 走 ``runtime_invocation``，不在这里保存或展开 SKILL.md。
    capability_refs: list[dict[str, Any]] = Field(default_factory=list)
    # Runtime 原生命令/Skill 的服务端解析结果。只保存能力 id、调用通道与
    # 参数；执行前会对当前会话的实时能力目录再次校验。
    runtime_invocation: dict[str, Any] = Field(default_factory=dict)
    runtime_snapshot: Optional[dict[str, str]] = None
    native_turn_id: str = ""
    # Planning mode this turn was sent with; retry/edit-resend replay it.
    interaction_mode: InteractionMode = "default"
    # Set by the terminal turn event: reusable | needs_restart | closed. Says
    # whether the next turn can reuse the native session; empty while running.
    thread_disposition: str = ""
    status: str = TURN_QUEUED
    usage: dict[str, Any] = Field(default_factory=dict)
    error: dict[str, Any] = Field(default_factory=dict)
    created_at: datetime = Field(default_factory=utcnow)
    completed_at: Optional[datetime] = None


class TurnRunRef(ContractModel):
    """一个 Turn 的 Task / Run / 执行代记录（Conversation 私有表）。

    Run 是一次 Turn 的执行单元；切换 Runtime 时同一 Run 下 generation 递增，
    重新生成注入计划并创建新 AgentSession。
    """

    run_id: str = Field(default_factory=lambda: new_id("run"))
    turn_id: str = ""
    task_id: Optional[str] = None
    thread_id: str = ""
    executor_id: str = EXECUTOR_ID
    adapter_id: str = ""
    runtime_instance_id: str = "default"
    generation: int = 1
    agent_session_id: Optional[str] = None
    # running | completed | failed | interrupted | superseded
    status: str = "running"
    created_at: datetime = Field(default_factory=utcnow)
    ended_at: Optional[datetime] = None


class ThreadRuntimeSelection(ContractModel):
    """Thread 当前的 Runtime 与全局凭据选择。"""

    thread_id: str = ""
    adapter_id: str = ""
    instance_id: str = "default"
    credential_id: str = ""
    model: str = ""
    effort: str = ""
    service_tier: str = ""
    access_mode: str = AccessMode.SUPERVISED.value
    # Preference only: plan mode is applied per turn, so it is not part of
    # ``session_key`` unless the adapter cannot switch it per turn.
    interaction_mode: InteractionMode = "default"
    # Legacy fields are retained for reading existing Thread payloads.  New
    # selections persist only access_mode.
    permission_mode: str = ""
    sandbox_mode: str = ""
    updated_at: datetime = Field(default_factory=utcnow)

    @property
    def runtime_key(self) -> str:
        """Runtime instance 身份键；变化意味着需要切换执行代。"""
        return f"{self.adapter_id}:{self.instance_id}"

    @property
    def session_key(self) -> str:
        """Launch-time identity; model options must reach a new native session."""
        # The tier segment is appended only when set so sessions recorded
        # before service tiers existed keep matching their launch identity.
        return (f"{self.runtime_key}|{self.credential_id}"
                f"|model={self.model}|effort={self.effort}"
                f"|access_mode={self.access_mode}"
                f"|permission_mode={self.permission_mode}"
                f"|sandbox_mode={self.sandbox_mode}"
                + (f"|service_tier={self.service_tier}" if self.service_tier else ""))


class ConversationMessage(ContractModel):
    """投影出的一条对话消息。"""

    message_id: str = Field(default_factory=lambda: new_id("msg"))
    thread_id: str = ""
    turn_id: Optional[str] = None
    # user | assistant | system
    role: str = "user"
    # message | steer。steer 仍是用户输入，但附着于执行中的 Turn，前端
    # 单独标注，且不会为它再渲染一份助手回复。
    kind: str = "message"
    text: str = ""
    source_provider: Optional[str] = None
    source_role: Optional[str] = None
    source_content: Any = None
    # 对应线程事件流中的 stream_seq（排序与未读判定用）
    stream_seq: int = 0
    created_at: datetime = Field(default_factory=utcnow)


class PlanTaskEvidence(ContractModel):
    """Link from a plan step to tool / message / artifact evidence."""

    kind: str = "tool"
    id: str = ""
    turn_id: Optional[str] = None


class PlanTask(ContractModel):
    """One stable step inside a ThreadPlanSnapshot."""

    task_id: str = ""
    title: str = ""
    # pending | in_progress | completed | blocked | cancelled
    status: str = "pending"
    blocked_reason: Optional[str] = None
    evidence: list[PlanTaskEvidence] = Field(default_factory=list)
    updated_at: datetime = Field(default_factory=utcnow)


class ThreadPlanSnapshot(ContractModel):
    """Versioned execution-plan read model (Adapter-reported; not markdown)."""

    revision: int = 0
    # proposed | executing | awaiting_decision | completed | cleared | unsupported
    phase: str = "cleared"
    # adapter | fixture | none
    source: str = "none"
    adapter_id: Optional[str] = None
    agent_session_id: Optional[str] = None
    turn_id: Optional[str] = None
    title: Optional[str] = None
    tasks: list[PlanTask] = Field(default_factory=list)
    awaiting: Optional[dict[str, Any]] = None
    pending_amendment: Optional[dict[str, Any]] = None
    last_change_summary: Optional[str] = None
    unsupported_reason: Optional[str] = None
    # Proposed plan body a plan-mode turn produced (engine-reported text, not
    # parsed into tasks); shown by the plan follow-up card.
    markdown: Optional[str] = None
    updated_at: datetime = Field(default_factory=utcnow)


class ConversationAgentNode(ContractModel):
    """One delegated Agent in a read-only parent/child ownership tree (C21)."""

    agent_id: str = ""
    parent_id: Optional[str] = None
    title: str = ""
    nickname: Optional[str] = None
    role: Optional[str] = None
    model: Optional[str] = None
    turn_id: Optional[str] = None
    message_id: Optional[str] = None
    # Delegation tool call that spawned (or last resumed) this agent.
    call_id: Optional[str] = None
    # Native child session / thread id when the runtime exposes one.
    session_ref: Optional[str] = None
    # pending | running | completed | failed | cancelled
    status: str = "pending"
    request: Optional[str] = None
    result: Optional[str] = None
    error: Optional[str] = None
    # Latest runtime-reported activity line (e.g. last tool name).
    activity: Optional[str] = None
    tool_uses: Optional[int] = None
    total_tokens: Optional[int] = None
    duration_ms: Optional[int] = None
    started_at: Optional[datetime] = None
    completed_at: Optional[datetime] = None
    updated_at: datetime = Field(default_factory=utcnow)
    # provider_native: reported by the Runtime (``core.agent.updated``).
    # app_owned: spawned by Muteki as a separate conversation Thread; the
    # fields below are only populated for that origin.
    origin: str = SUBAGENT_ORIGIN_NATIVE
    # Child conversation Thread (also the stable subagent id).
    thread_id: Optional[str] = None
    # Latest Turn of the child Thread; ``message_id`` then references the
    # child's final assistant message for that Turn (full text lives there).
    child_turn_id: Optional[str] = None
    depth: Optional[int] = None
    adapter_id: Optional[str] = None
    access_mode: Optional[str] = None
    # shared | worktree
    isolation: Optional[str] = None
    workspace_id: Optional[str] = None
    worktree_path: Optional[str] = None
    # active | removed | retained
    worktree_state: Optional[str] = None
    # Durable progress of the spawn side effects, so startup recovery resumes
    # from the last confirmed step: planned | workspace_ready | activated |
    # dispatched | skipped (cancelled before the first Turn) | failed.
    # None on nodes recorded before progress tracking; those are never resumed.
    spawn_state: Optional[str] = None
    cancel_requested: bool = False
    error_code: Optional[str] = None
    # Principal the child Binding is issued for; recovery re-activates with it.
    principal_id: Optional[str] = None


class ThreadLineage(ContractModel):
    """Ancestry of a Muteki-spawned (app_owned) subagent Thread."""

    origin: str = SUBAGENT_ORIGIN_APP
    subagent_id: str = ""
    parent_thread_id: str = ""
    # Parent Turn that issued the spawn; None when spawned outside a Turn.
    parent_turn_id: Optional[str] = None
    root_thread_id: str = ""
    # root Thread = 0, its children = 1, grandchildren = 2.
    depth: int = 1
    isolation: str = "shared"
    workspace_id: Optional[str] = None
    worktree_path: Optional[str] = None
    spawned_at: datetime = Field(default_factory=utcnow)


class ThreadAgentTreeSnapshot(ContractModel):
    """Versioned Agents-panel read model (delegation only; not ordinary tools)."""

    revision: int = 0
    # adapter | fixture | derived | none
    source: str = "none"
    adapter_id: Optional[str] = None
    turn_id: Optional[str] = None
    agents: list[ConversationAgentNode] = Field(default_factory=list)
    unsupported: bool = False
    unsupported_reason: Optional[str] = None
    tool_activity_summary: Optional[str] = None
    updated_at: datetime = Field(default_factory=utcnow)


class ThreadState(ContractModel):
    """Thread 读模型：运行状态、待处理交互、未读与用量。"""

    thread_id: str = ""
    # active | archived
    status: str = "active"
    running_turn_id: Optional[str] = None
    current_generation: int = 1
    history_rebuild_pending: bool = False
    # Durable journal for a crash between provider rewind and branch commit.
    history_recovery_required: bool = False
    # Deferred native fork, created on the target connection with its own
    # credentials/MCP. Contains provider references, never credentials.
    native_fork: Optional[dict[str, str]] = None
    # 当前活跃 AgentSession 及其 Runtime key（切换检测用）
    agent_session_id: Optional[str] = None
    session_runtime_key: str = ""
    # C23: approval queue keyed by approval_id. ``pending_approval`` remains the
    # oldest actionable entry for list badges / inbox deep-links (#29).
    pending_approvals: dict[str, dict[str, Any]] = Field(default_factory=dict)
    pending_approval: Optional[dict[str, Any]] = None
    pending_user_input: Optional[dict[str, Any]] = None
    # Adapter / fixture reported execution plan (distinct from injection_plan).
    plan: Optional[ThreadPlanSnapshot] = None
    # C21: delegated Agent ownership tree (not ordinary shell/tool rows).
    agents: Optional[ThreadAgentTreeSnapshot] = None
    # Set only on Threads spawned by ``conversation.subagent.spawn``.
    lineage: Optional[ThreadLineage] = None
    # Direct app_owned children of this Thread (grandchildren live on the
    # child Thread's own state).
    subagents: list[ConversationAgentNode] = Field(default_factory=list)
    last_turn_seq: int = 0
    message_count: int = 0
    usage: dict[str, Any] = Field(default_factory=dict)
    # C27: latest known context-window fuel gauge.
    # shape: {total, limit, zones, compacted, compact_status, updated_at, source}
    # limit=None means unknown; compact_status: idle|running|done|failed
    context_window: Optional[dict[str, Any]] = None
    last_error: dict[str, Any] = Field(default_factory=dict)
    last_message_preview: str = ""
    # head：已投影的线程流水位；attention：最后一条需要用户查看的事件
    # （正文、回合结果、审批等；会话生命周期、能力探测、用量这类后台
    # 记账不算）；read：用户已读水位。attention > read 即未读。
    head_stream_seq: int = 0
    attention_stream_seq: int = 0
    read_stream_seq: int = 0
    last_active_at: datetime = Field(default_factory=utcnow)
    archived_at: Optional[datetime] = None
    # Conversation 自管的后续消息队列摘要。队列实体独立于 Turn：只有被
    # 提升的队首项才创建 Turn、进入主时间线并占用 active turn claim。
    queue_count: int = 0
    queue_paused: bool = False
    queue_pause_reason: str = ""
    queue_revision: int = 0
    queue_failed_item_id: Optional[str] = None
    # Pending automatic resume after a provider quota reset:
    # {turn_id, resets_at, resume_at, kind, scheduled_at}. Cleared by any new Turn.
    quota_resume: Optional[dict[str, Any]] = None
    # Latest structured rate-limit hold the engine reported in a Turn:
    # {turn_id, resets_at, kind, handled}. ``handled`` flips once any schedule
    # decision (auto, user, or cancel) was made, so a cancelled schedule is not
    # recreated by the server-side auto pass.
    quota_hold: Optional[dict[str, Any]] = None

    @model_validator(mode="before")
    @classmethod
    def _legacy_attention_seq(cls, data: Any) -> Any:
        # Rows persisted before attention_stream_seq existed derived unread
        # from head_stream_seq; seeding from it keeps their unread state.
        if not isinstance(data, dict) or "attention_stream_seq" in data:
            return data
        legacy = {**data, "attention_stream_seq": data.get("head_stream_seq", 0)}
        # Those rows also recorded background capability-probe failures as
        # the Thread's last_error; the probe result lives in the capability
        # snapshot, so it is not a failure of the Thread.
        last_error = legacy.get("last_error")
        if (isinstance(last_error, dict)
                and last_error.get("code") == CAPABILITY_REFRESH_FAILED_CODE
                and not last_error.get("turn_id")):
            legacy["last_error"] = {}
        return legacy

    @property
    def unread(self) -> bool:
        return self.attention_stream_seq > self.read_stream_seq


class QueuedTurnRequest(ContractModel):
    """等待提升为 Turn 的持久化用户消息。"""

    queue_id: str = Field(default_factory=lambda: new_id("queue"))
    thread_id: str = ""
    command_id: str = ""
    idempotency_key: str = ""
    client_message_id: str = ""
    actor_id: str = "local-user"
    correlation_id: str = ""
    text: str = ""
    attachments: list[str] = Field(default_factory=list)
    capability_refs: list[dict[str, Any]] = Field(default_factory=list)
    runtime_invocation: dict[str, Any] = Field(default_factory=dict)
    # 入队时固定本条消息的 Runtime 选择，避免等待期间的界面选择变化让
    # 实际执行环境与用户提交时看到的不一致。
    runtime: dict[str, Any] = Field(default_factory=dict)
    position: int = 0
    # queued | dispatching | failed | consumed | cancelled
    status: str = "queued"
    promoted_turn_id: Optional[str] = None
    error: dict[str, Any] = Field(default_factory=dict)
    created_at: datetime = Field(default_factory=utcnow)
    updated_at: datetime = Field(default_factory=utcnow)


__all__ = [
    "EXECUTOR_ID",
    "SUBAGENT_ORIGIN_APP",
    "SUBAGENT_ORIGIN_NATIVE",
    "ThreadLineage",
    "TURN_COMPLETED",
    "TURN_FAILED",
    "TURN_INTERRUPTED",
    "TURN_CANCELLED",
    "TURN_KIND_MESSAGE",
    "TURN_KIND_RETRY",
    "TURN_KIND_EDIT_RESEND",
    "TURN_KIND_NATIVE_REWIND",
    "TURN_KIND_RESUME",
    "TURN_QUEUED",
    "TURN_RUNNING",
    "TURN_SUPERSEDED",
    "ConversationAgentNode",
    "ConversationMessage",
    "PlanTask",
    "PlanTaskEvidence",
    "QueuedTurnRequest",
    "ThreadAgentTreeSnapshot",
    "ThreadPlanSnapshot",
    "ThreadRuntimeSelection",
    "ThreadState",
    "TurnRecord",
    "TurnRunRef",
]
