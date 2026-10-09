"""Muteki 平台契约（CORE-01）。

冻结对象、绑定、信封、事件与接口的字段和命名空间；
``CONTRACT_MODELS`` / ``CONTRACT_ENUMS`` 是 schema 生成脚本
（``scripts/gen_contracts_ts.py``）的固定输入，顺序即输出顺序。
"""

from .base import CONTRACT_SCHEMA_VERSION, ContractModel, new_id, utcnow
from .capabilities import (
    BindingContext,
    CapabilityBinding,
    CapabilityDescriptor,
    CapabilityGrant,
    CapabilityImage,
    CapabilityInjectionPlan,
    CapabilityInvocation,
    CapabilityResult,
    ExecutionBinding,
    InjectionKind,
    ThreadMode,
    ToolDescription,
)
from .commands import (
    ActorRef,
    CommandEnvelope,
    EventPage,
    EventReadRequest,
    QueryEnvelope,
    QueryResult,
    WaitRequest,
    WaitResult,
)
from .errors import ErrorCategory, ErrorEnvelope
from .events import (
    BUILTIN_NAMESPACES,
    NS_COMPETITION,
    NS_CORE,
    NS_CTF,
    NS_EXT_PREFIX,
    NS_PENTEST,
    NS_RUN,
    EventEnvelope,
    PublicEvent,
    ext_namespace,
    is_known_namespace,
)
from .extensions import (
    ExtensionEntrypoint,
    ExtensionManifest,
    ExtensionPermissions,
    ExtensionProvide,
    ExtensionRequire,
)
from .external_agents import (
    AgentCapabilities,
    AgentEvent,
    AgentEventType,
    AgentInput,
    AgentSessionRef,
    AgentSessionSnapshot,
    ApprovalResponseInput,
    ApprovalResponsePayload,
    AttachmentRef,
    MessageInput,
    MessagePayload,
    ProbeRequest,
    SessionOptions,
    SessionOptionsError,
    SessionStart,
    SteerInput,
    SteerPayload,
    UserInputResponseInput,
    UserInputResponsePayload,
)
from .graphs import (
    ClaimReceipt,
    ClaimRequest,
    GraphEvent,
    GraphReceipt,
    GraphScope,
    GraphSnapshot,
    LeaseReceipt,
    LeaseRequest,
)
from .modules import (
    ArtifactObject,
    DomainModuleDescriptor,
    InstanceLeaseRef,
    InstanceResult,
    PlatformCapabilities,
    PlatformChallengeRef,
    PlatformConnectionRef,
    RemoteArtifactRef,
    SubmissionRequest,
    SubmissionResult,
    SyncRequest,
    SyncResult,
)
from .objects import (
    AgentSession,
    Artifact,
    ExecutionGeneration,
    Project,
    ResourceLease,
    RunRef,
    Task,
    Thread,
    Workspace,
)
from .protocols import (
    AgentCapabilityGateway,
    BackgroundUpdateAdapter,
    DomainModule,
    ExternalAgentAdapter,
    GraphService,
    MutekiCommandAPI,
    NativeRewindAdapter,
    PlatformAdapter,
    ProbingAdapter,
    RunExecutor,
    RunGateway,
    RuntimeOperationAdapter,
)
from .receipts import (
    AggregateRef,
    CommandReceipt,
    EffectReceipt,
    EffectState,
    OutboxRecord,
    OutboxStatus,
    ReceiptState,
)
from .runs import (
    BoundRunRequest,
    RunCommand,
    RunEvent,
    RunExecutionContext,
    RunResult,
    RunSnapshot,
)

#: schema 生成脚本的固定模型清单（TS interface 按此顺序输出）
CONTRACT_MODELS = [
    # 通用对象
    Project,
    Workspace,
    Thread,
    Task,
    RunRef,
    ExecutionGeneration,
    AgentSession,
    Artifact,
    ResourceLease,
    # Run
    BoundRunRequest,
    RunCommand,
    RunEvent,
    RunSnapshot,
    RunExecutionContext,
    RunResult,
    # 绑定与能力
    ExecutionBinding,
    CapabilityBinding,
    CapabilityGrant,
    ToolDescription,
    CapabilityInjectionPlan,
    BindingContext,
    CapabilityDescriptor,
    CapabilityInvocation,
    CapabilityResult,
    CapabilityImage,
    # 外部 Agent
    AgentCapabilities,
    ProbeRequest,
    SessionOptions,
    SessionStart,
    AgentSessionRef,
    AttachmentRef,
    MessagePayload,
    SteerPayload,
    ApprovalResponsePayload,
    UserInputResponsePayload,
    MessageInput,
    SteerInput,
    ApprovalResponseInput,
    UserInputResponseInput,
    AgentEvent,
    AgentSessionSnapshot,
    # 命令 / 查询 / 事件
    ActorRef,
    CommandEnvelope,
    QueryEnvelope,
    EventReadRequest,
    WaitRequest,
    QueryResult,
    EventPage,
    WaitResult,
    EventEnvelope,
    PublicEvent,
    # 回执与 outbox
    AggregateRef,
    CommandReceipt,
    EffectReceipt,
    OutboxRecord,
    # 图服务
    GraphEvent,
    GraphReceipt,
    GraphScope,
    GraphSnapshot,
    ClaimRequest,
    ClaimReceipt,
    LeaseRequest,
    LeaseReceipt,
    # 模块 / 平台 / 扩展
    DomainModuleDescriptor,
    PlatformConnectionRef,
    PlatformCapabilities,
    SyncRequest,
    SyncResult,
    RemoteArtifactRef,
    ArtifactObject,
    PlatformChallengeRef,
    InstanceLeaseRef,
    InstanceResult,
    SubmissionRequest,
    SubmissionResult,
    ExtensionEntrypoint,
    ExtensionProvide,
    ExtensionRequire,
    ExtensionPermissions,
    ExtensionManifest,
    # 错误
    ErrorEnvelope,
]

#: schema 生成脚本的固定枚举清单（TS union type 按此顺序输出）
CONTRACT_ENUMS = [
    ThreadMode,
    InjectionKind,
    AgentEventType,
    ReceiptState,
    EffectState,
    OutboxStatus,
    ErrorCategory,
]

__all__ = [name for name in dir() if not name.startswith("_")]
