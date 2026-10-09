"""平台接口 Protocol（任务书 6.1–6.10、8.3）。

Web、AgentCapabilityGateway、领域模块和兼容 CLI 都不能直接调用 Manager、
Store、SharedGraph 数据库或 Result Gate，只能经以下接口进入平台。
"""

from __future__ import annotations

from typing import Any, AsyncIterator, Callable, Optional, Protocol, runtime_checkable

from .capabilities import (
    BindingContext,
    CapabilityBinding,
    CapabilityDescriptor,
    CapabilityGrant,
    CapabilityInjectionPlan,
    CapabilityInvocation,
    CapabilityResult,
)
from .commands import (
    CommandEnvelope,
    EventPage,
    EventReadRequest,
    QueryEnvelope,
    QueryResult,
    WaitRequest,
    WaitResult,
)
from .external_agents import (
    AgentCapabilities,
    AgentEvent,
    AgentEventType,
    AgentInput,
    AgentSessionRef,
    AgentSessionSnapshot,
    ProbeRequest,
    SessionStart,
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
from .receipts import CommandReceipt
from .runs import (
    BoundRunRequest,
    RunCommand,
    RunEvent,
    RunExecutionContext,
    RunResult,
    RunSnapshot,
)
from .objects import RunRef


@runtime_checkable
class MutekiCommandAPI(Protocol):
    """统一应用入口（任务书 6.1）。

    传输入口只做身份认证和格式转换；业务权限、资源范围、幂等、状态机、
    回执、事件、外部副作用投递和恢复都在 Command API 内完成。
    """

    async def dispatch(self, command: CommandEnvelope) -> CommandReceipt: ...
    async def get_receipt(self, command_id: str) -> CommandReceipt: ...
    async def query(self, query: QueryEnvelope) -> QueryResult: ...
    async def read_events(self, request: EventReadRequest) -> EventPage: ...
    async def wait(self, request: WaitRequest) -> WaitResult: ...


@runtime_checkable
class AgentCapabilityGateway(Protocol):
    """外部主对话 Agent 调用 Muteki 获准能力的唯一入口（任务书 6.6）。

    Gateway 无业务状态：不保存 Run、Competition、SharedGraph、提交队列、
    事件缓存或 AgentSession 状态；所有调用映射到 MutekiCommandAPI。
    """

    async def describe(self, binding_id: str) -> CapabilityDescriptor: ...
    async def invoke(
        self, context: BindingContext, invocation: CapabilityInvocation
    ) -> CapabilityResult: ...


@runtime_checkable
class RunGateway(Protocol):
    """领域模块访问 RunManager 的唯一接口（任务书 6.3）。"""

    async def ensure_bound_run(self, request: BoundRunRequest) -> RunRef: ...
    async def command(self, run_id: str, command: RunCommand) -> CommandReceipt: ...
    async def list_runs(
        self, *, include_archived: bool = False
    ) -> list[dict[str, Any]]: ...
    async def snapshot(self, run_id: str) -> RunSnapshot: ...
    def events(self, run_id: str, after_seq: int = 0) -> AsyncIterator[RunEvent]: ...


@runtime_checkable
class RunExecutor(Protocol):
    """Run 的实际执行器（任务书 6.4）。

    产品内置 swarm.coordinator 与 external-agent.single；实验 Executor 只能
    显式指定完整 id，不能成为产品默认。
    """

    id: str
    supported_task_kinds: set[str]

    async def execute(self, context: RunExecutionContext) -> RunResult: ...


@runtime_checkable
class ExternalAgentAdapter(Protocol):
    """外部 Agent Runtime 适配器（任务书 6.5）。"""

    id: str

    async def probe(self, request: ProbeRequest) -> AgentCapabilities: ...
    async def prepare_capability_injection(
        self,
        request: SessionStart,
        binding: CapabilityBinding,
        grant: CapabilityGrant,
    ) -> CapabilityInjectionPlan: ...
    async def start(self, request: SessionStart) -> AgentSessionRef: ...
    def send(
        self, session: AgentSessionRef, input: AgentInput
    ) -> AsyncIterator[AgentEvent]: ...
    def resume(self, session: AgentSessionRef) -> AsyncIterator[AgentEvent]: ...
    async def steer(self, session: AgentSessionRef, input: AgentInput) -> CommandReceipt: ...
    async def interrupt(self, session: AgentSessionRef) -> CommandReceipt: ...
    async def snapshot(self, session: AgentSessionRef) -> AgentSessionSnapshot: ...
    async def close(self, session: AgentSessionRef) -> None: ...


# -- Optional adapter capabilities ----------------------------------------------
#
# Adapters declare these by inheriting the Protocol; consumers dispatch with
# ``isinstance`` instead of probing attribute names. Whether a declared
# operation is usable for a given session is still decided by the adapter's
# runtime capability snapshot.

#: Callback for runtime updates that arrive outside an active turn stream.
BackgroundUpdate = tuple[AgentEventType, str, dict[str, Any]]


@runtime_checkable
class RuntimeOperationAdapter(Protocol):
    """Runs a verified client-resolved runtime operation (``/compact`` etc.)."""

    async def runtime_operation(
        self, session: AgentSessionRef, name: str, arguments: str = ""
    ) -> dict[str, Any]: ...


@runtime_checkable
class NativeRewindAdapter(Protocol):
    """Rolls the native thread back to before a given native turn."""

    def supports_native_rewind(self, session: AgentSessionRef) -> bool: ...
    async def rewind_session(
        self, session: AgentSessionRef, native_turn_id: str
    ) -> dict[str, Any]: ...


@runtime_checkable
class BackgroundUpdateAdapter(Protocol):
    """Delivers runtime updates that arrive after the turn stream ended."""

    def bind_background_handler(
        self,
        session: AgentSessionRef,
        handler: Callable[[list[BackgroundUpdate]], None],
    ) -> None: ...


@runtime_checkable
class NativeContinuationAdapter(Protocol):
    """A native background execution offers a turn without another prompt.

    Mirrors T3's continuationRequests / dispatchIfCurrent / runWake port.
    Updates retain the conversation turn that started their background work.
    """

    def bind_continuations(
        self, session: AgentSessionRef, turn_id: str,
        updates: Callable[[str, list[BackgroundUpdate]], None],
        offer: Callable[[], None],
    ) -> None: ...

    def pending_continuation(self, session: AgentSessionRef) -> tuple[str, str] | None: ...

    def background_turn_id(self, session: AgentSessionRef) -> str | None: ...

    def run_continuation(
        self, session: AgentSessionRef, wake_id: str,
    ) -> AsyncIterator[AgentEvent]: ...


@runtime_checkable
class ProbingAdapter(Protocol):
    """What the Adapter Registry needs from an adapter to key and run probes."""

    id: str
    identity: Any
    launch_args: tuple[str, ...]

    def probe_binary(self) -> str: ...
    def probe_report(self) -> Any: ...
    async def probe_with_environment(self, request: ProbeRequest) -> AgentCapabilities: ...


@runtime_checkable
class DomainModule(Protocol):
    """领域模块（任务书 6.8）。

    内置：builtin.conversation、builtin.single-security-task、
    builtin.competition。属性语义同 DomainModuleDescriptor。
    """

    id: str
    version: str
    task_kinds: set[str]
    required_capabilities: set[str]
    default_executor: str
    command_handlers: dict[str, str]
    event_namespaces: list[str]
    api_routes: list[str]
    workspace_kind: str
    ui_contributions: dict[str, Any]
    artifact_types: list[str]
    graph_binding: Optional[str]
    gate_binding: Optional[str]


@runtime_checkable
class PlatformAdapter(Protocol):
    """比赛平台适配器（任务书 6.10）。领域接口与传输分离。"""

    id: str

    async def probe(self, connection: PlatformConnectionRef) -> PlatformCapabilities: ...
    async def sync_competition(self, request: SyncRequest) -> SyncResult: ...
    async def fetch_artifact(self, artifact: RemoteArtifactRef) -> ArtifactObject: ...
    async def acquire_instance(self, challenge: PlatformChallengeRef) -> InstanceResult: ...
    async def renew_instance(self, lease: InstanceLeaseRef) -> InstanceResult: ...
    async def release_instance(self, lease: InstanceLeaseRef) -> None: ...
    async def submit(self, request: SubmissionRequest) -> SubmissionResult: ...


@runtime_checkable
class GraphService(Protocol):
    """共享图服务（任务书 8.3）。"""

    id: str

    async def append(self, event: GraphEvent) -> GraphReceipt: ...
    async def snapshot(self, scope: GraphScope) -> GraphSnapshot: ...
    async def claim(self, request: ClaimRequest) -> ClaimReceipt: ...
    async def lease(self, request: LeaseRequest) -> LeaseReceipt: ...
