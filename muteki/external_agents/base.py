"""ExternalAgentAdapter 基础骨架（RUNTIME-01，任务书 6.5 / 7.6）。

``BaseExternalAgentAdapter`` 实现 ``ExternalAgentAdapter`` Protocol 的公共部分：

- instance identity：``adapter_id + instance_id`` 唯一标识一个 Runtime
  实例（任务书 7.3）；
- capability probe 缓存与 MCP 优先的注入计划选择（只读 capability 字段，
  不按 adapter id 推断功能）；
- SessionStart 的 Binding 七步交付（任务书 7.6）：建内部 AgentSession
  （preparing）→ 取 CapabilityBinding → probe 选 InjectionPlan → 签发短期
  grant 并注入启动参数 → external session id 回填 active（失败撤 grant）→
  无法热更新的 Session 用 grant lease 续期 → 关闭/撤销时同步撤 grant；
- 未支持能力返回 typed unsupported receipt（FAILED + 稳定机器码），
  不静默忽略。

子类只需实现：``probe``（实测能力）、``_launch``（按注入计划启动/接管
Runtime 会话）与可选的 ``_teardown``。具体 Runtime 的结构化 Adapter 属于
RUNTIME-02~04；CLI 兼容实现见 ``muteki.solver.cli_driver.CliDriverAdapter``。
"""

from __future__ import annotations

from dataclasses import dataclass
from typing import Any, AsyncIterator, Callable, Optional

from muteki.capability_bindings import (
    acp_config,
    agent_plugin,
    native_tools,
)
from muteki.capability_bindings.auth import issue_bearer_token
from muteki.platform.contracts.base import new_id
from muteki.platform.contracts.capabilities import (
    CapabilityBinding,
    CapabilityDescriptor,
    CapabilityGrant,
    CapabilityInjectionPlan,
    InjectionKind,
)
from muteki.platform.contracts.errors import ErrorCategory, ErrorEnvelope
from muteki.platform.contracts.external_agents import (
    AgentCapabilities,
    AgentEvent,
    AgentEventType,
    AgentInput,
    AgentSessionRef,
    AgentSessionSnapshot,
    ProbeRequest,
    SessionStart,
)
from muteki.platform.contracts.receipts import AggregateRef, CommandReceipt, ReceiptState

from .capabilities import (
    CapabilityProbeReport,
    conservative_capabilities,
    select_injection_kind,
)
from .events import EventSequencer, build_event
from .runtime_capabilities import (
    RuntimeCapabilityItem,
    RuntimeCapabilitySnapshot,
)
from .sessions import EventProjector, SessionTracker

#: 未支持能力的稳定机器码（typed unsupported receipt）。
UNSUPPORTED_CAPABILITY_CODE = "external_agent.capability.unsupported"


@dataclass(frozen=True)
class AdapterIdentity:
    """Runtime 实例身份：``adapter_id + instance_id``（任务书 7.3）。"""

    adapter_id: str
    instance_id: str = "default"

    @property
    def key(self) -> str:
        return f"{self.adapter_id}:{self.instance_id}"


class BaseExternalAgentAdapter:
    """``ExternalAgentAdapter`` Protocol 的基础实现。

    构造参数：

    - ``store``：可选 PlatformStore，用于持久化内部 AgentSession 记录；
    - ``binding_service``：可选 CAP-01 CapabilityBindingService，提供
      Binding 签发、短期 grant 签发 / 续期 / 撤销；缺省时 start 只做
      无注入的会话启动（计划为 None）；
    - ``gateway_endpoint``：AgentCapabilityGateway 的协议入口地址；
    - ``descriptor_provider``：可选 ``(binding) -> CapabilityDescriptor``，
      缺省用产品默认目录按 ``binding.tool_set`` 过滤。
    """

    def __init__(
        self,
        adapter_id: str,
        *,
        instance_id: str = "default",
        store: Any = None,
        binding_service: Any = None,
        gateway_endpoint: str = "",
        descriptor_provider: Optional[
            Callable[[CapabilityBinding], CapabilityDescriptor]] = None,
    ) -> None:
        self.id = adapter_id
        self.identity = AdapterIdentity(adapter_id, instance_id)
        self._tracker = SessionTracker(store)
        self._projector = EventProjector(self._tracker)
        self._binding_service = binding_service
        self._gateway_endpoint = gateway_endpoint
        self._descriptor_provider = descriptor_provider
        self._sequencers: dict[str, EventSequencer] = {}
        self._grants: dict[str, list[str]] = {}
        self._plans: dict[str, CapabilityInjectionPlan] = {}
        self._probe_cache: Optional[CapabilityProbeReport] = None
        self.probe_environment_factory: Optional[Callable[[], dict[str, str]]] = None

    # --  probe -------------------------------------------------------------

    async def probe(self, request: ProbeRequest) -> AgentCapabilities:
        """基础层默认：保守静态能力（子类应实测后覆写）。"""
        return conservative_capabilities()

    def probe_report(self) -> Optional[CapabilityProbeReport]:
        """最近一次 probe 的完整记录（含字段来源与降级说明）。"""
        return self._probe_cache

    async def probe_with_environment(self, request: ProbeRequest) -> AgentCapabilities:
        from .probe_environment import PROBE_ENVIRONMENT
        # Keep an explicitly selected credential's prepared, task-local home.
        if PROBE_ENVIRONMENT.get() is not None:
            return await self.probe(request)
        if self.probe_environment_factory is None:
            return await self.probe(request)
        import asyncio
        environment = await asyncio.to_thread(self.probe_environment_factory)
        token = PROBE_ENVIRONMENT.set(environment)
        try:
            return await self.probe(request)
        finally:
            PROBE_ENVIRONMENT.reset(token)

    async def _capabilities(self, *, refresh: bool = False) -> AgentCapabilities:
        if self._probe_cache is None or refresh:
            caps = await self.probe_with_environment(ProbeRequest(
                runtime_instance_id=self.identity.instance_id))
            if self._probe_cache is None:
                # 子类未维护 report 时包一层，保证调用方总能读到来源标注。
                self._probe_cache = CapabilityProbeReport(
                    adapter_id=self.identity.adapter_id,
                    instance_id=self.identity.instance_id,
                    capabilities=caps,
                )
            else:
                self._probe_cache.capabilities = caps
        return self._probe_cache.capabilities

    async def capabilities(self, *, refresh: bool = False) -> AgentCapabilities:
        """返回缓存后的实测能力，供调度层选择明确的降级路径。"""
        return await self._capabilities(refresh=refresh)

    # -- 注入计划（任务书 6.5：MCP 默认优先） --------------------------------

    def _describe(self, binding: CapabilityBinding) -> CapabilityDescriptor:
        if self._descriptor_provider is not None:
            return self._descriptor_provider(binding)
        from muteki.platform.capability_catalog import (
            DEFAULT_CATALOG,
            effective_tool_set,
        )
        tool_set = effective_tool_set(binding.mode, binding.tool_set)
        return CapabilityDescriptor(
            binding_id=binding.binding_id,
            binding_version=binding.binding_version,
            mode=binding.mode,
            tools=[spec.describe() for spec in DEFAULT_CATALOG.filter(tool_set)],
            allowed_commands=DEFAULT_CATALOG.command_types_of(tool_set),
            allowed_queries=DEFAULT_CATALOG.query_types_of(tool_set),
            resource_scopes=list(binding.resource_scopes),
        )

    async def prepare_capability_injection(
        self,
        request: SessionStart,
        binding: CapabilityBinding,
        grant: CapabilityGrant,
    ) -> CapabilityInjectionPlan:
        """按实测能力选择注入方式并委托 CAP-02 builder 生成计划。

        计划只含 Gateway endpoint、工具描述、短期 credential reference、
        audience 与 Runtime 配置；bearer token 在计划中以占位符出现，
        本体只在 ``start`` 的启动参数 / 环境注入时经
        ``auth.issue_bearer_token`` 现场签发。
        """
        caps = await self._capabilities()
        from muteki.capability_management import enabled as capability_enabled
        if not capability_enabled("mcp", "muteki-control") and not request.options.get("chat_tools"):
            caps = caps.model_copy(update={
                "mcp": False,
                "acp_mcp_config": False,
            })
        kind = select_injection_kind(caps)
        descriptor = self._describe(binding)
        if request.options.get("thread_mode") == "conversation" and "chat_tools" in request.options:
            from muteki.platform.contracts.capabilities import ToolDescription
            tools = descriptor.tools if request.options.get("chat_control_enabled", True) and capability_enabled("mcp", "muteki-control") else []
            descriptor = descriptor.model_copy(update={"tools": [*tools, *[
                ToolDescription(name=t["name"], description=t["description"], input_schema=t["input_schema"])
                for t in request.options["chat_tools"]
            ]]})
        common = dict(
            binding_id=binding.binding_id,
            grant_id=grant.grant_id,
            credential_ref=grant.credential_ref,
            audience=grant.audience,
        )
        endpoint = self._gateway_endpoint
        if kind is InjectionKind.MCP:
            plan = CapabilityInjectionPlan(
                injection_kind=InjectionKind.MCP,
                gateway_endpoint=endpoint,
                tool_descriptions=list(descriptor.tools),
                runtime_config={
                    "mcpServers": {
                        acp_config.DEFAULT_SERVER_NAME: {
                            "type": "http",
                            "url": endpoint,
                            "headers": {
                                "Authorization": (
                                    f"Bearer {acp_config.TOKEN_PLACEHOLDER}"),
                            },
                        },
                    },
                },
                **common,
            )
        elif kind is InjectionKind.NATIVE_TOOL:
            plan = native_tools.build_injection_plan(descriptor, **common)
        elif kind is InjectionKind.ACP_MCP_CONFIG:
            plan = acp_config.build_injection_plan(
                descriptor, gateway_endpoint=endpoint, **common)
        elif kind is InjectionKind.AGENT_PLUGIN:
            plan = agent_plugin.build_injection_plan(
                descriptor, gateway_endpoint=endpoint, **common)
        elif kind is InjectionKind.HTTP_JSONRPC:
            plan = CapabilityInjectionPlan(
                injection_kind=InjectionKind.HTTP_JSONRPC,
                gateway_endpoint=endpoint,
                tool_descriptions=list(descriptor.tools),
                runtime_config={
                    "endpoint": endpoint,
                    "auth_scheme": "bearer",
                    "token_env": agent_plugin.ENV_TOKEN,
                },
                **common,
            )
        else:
            plan = agent_plugin.build_injection_plan(
                descriptor, gateway_endpoint=endpoint, **common)
        self._plans[request.agent_session_id] = plan
        return plan

    def injection_plan(self, agent_session_id: str) -> Optional[CapabilityInjectionPlan]:
        """该 Session 最近一次生成的注入计划（记录注入种类与 schema）。"""
        return self._plans.get(agent_session_id)

    def _mcp_capability_items(
        self, session: Optional[AgentSessionRef]
    ) -> list[RuntimeCapabilityItem]:
        """把已生成的注入计划投影为只读 MCP 请求状态。

        基类只能证明配置已经请求交付。只有 Adapter 从 Runtime 读回状态后，
        才能把它升级为 runtime_reported/failed。
        """
        if session is None:
            return []
        plan = self._plans.get(session.agent_session_id)
        if plan is None:
            return []
        cfg = (plan.runtime_config or {}).get("mcpServers")
        names: list[str] = []
        if isinstance(cfg, dict):
            names = [str(name) for name in cfg]
        elif isinstance(cfg, list):
            names = [
                str(item.get("name") or "")
                for item in cfg if isinstance(item, dict)
            ]
        return [
            RuntimeCapabilityItem(
                id=f"runtime:{self.id}:mcp:{name}",
                kind="mcp_status",
                name=name,
                description="会话创建时已经请求 Runtime 注册该 MCP",
                source=self.id,
                scope="session",
                channel="session_config",
                resolution="runtime",
                origin="dynamic",
                delivery="best_effort",
                verification="unverified",
                status="requested",
                invocation={"injection_kind": plan.injection_kind.value},
            )
            for name in names if name
        ]

    async def runtime_capability_snapshot(
        self, session: Optional[AgentSessionRef] = None
    ) -> RuntimeCapabilitySnapshot:
        """返回当前会话的实时能力快照。

        默认 Adapter 不声明第三方命令。静态命令表不得在这里自动升级为
        verified；子类只能加入协议公布或真实接口读回的能力。
        """
        return RuntimeCapabilitySnapshot(
            adapter_id=self.id,
            instance_id=self.identity.instance_id,
            agent_session_id=session.agent_session_id if session else "",
            external_session_id=session.external_session_id if session else None,
            items=self._mcp_capability_items(session),
        )

    # -- SessionStart 的 Binding 交付（任务书 7.6 七步） ---------------------

    def _resolve_binding(self, request: SessionStart) -> Optional[CapabilityBinding]:
        """步骤 2：按 Thread 模式、Principal 与资源范围取 CapabilityBinding。"""
        if self._binding_service is None:
            return None
        thread_id = request.thread_id
        if not thread_id:
            return None
        principal = str(request.options.get("principal_id") or "system")
        mode = str(request.options.get("thread_mode") or "conversation")
        scopes = request.options.get("resource_scopes")
        return self._binding_service.issue_binding(
            thread_id, principal, mode,
            resource_scopes=list(scopes) if scopes else None,
        )

    async def start(self, request: SessionStart) -> AgentSessionRef:
        """创建（或接管）一个外部 Agent Session，完成 Binding 交付。"""
        # 步骤 1：先建内部 AgentSession（preparing），取得稳定 id。
        record = self._tracker.create(
            request,
            adapter_id=self.identity.adapter_id,
            runtime_instance_id=self.identity.instance_id,
        )
        if request.execution_generation is not None:
            self._tracker.set_generation(
                record.agent_session_id, int(request.execution_generation))

        binding: Optional[CapabilityBinding] = None
        grant: Optional[CapabilityGrant] = None
        plan: Optional[CapabilityInjectionPlan] = None
        token: Optional[str] = None
        try:
            # 步骤 2：取 CapabilityBinding。
            binding = self._resolve_binding(request)
            if binding is not None and self._binding_service is not None:
                # 步骤 3：probe 选 InjectionPlan；步骤 4：签发短期 grant。
                caps = await self._capabilities()
                kind = select_injection_kind(caps)
                audience = str(request.options.get("audience") or (
                    f"adapter:{self.identity.key}"))
                grant = self._binding_service.issue_grant(
                    binding,
                    record.agent_session_id,
                    runtime_instance_id=self.identity.instance_id,
                    injection_kind=kind,
                    audience=audience,
                )
                self._grants.setdefault(record.agent_session_id, []).append(
                    grant.grant_id)
                plan = await self.prepare_capability_injection(
                    request, binding, grant)
                token = issue_bearer_token(binding, grant, session=record)

            # 步骤 4（后半）：在 Runtime 启动参数中完成注入并启动。
            launched = await self._launch(request, plan, token)
        except Exception:
            # 步骤 5（失败分支）：撤销 grant，Session 标记 error。
            if grant is not None and self._binding_service is not None:
                self._binding_service.revoke_grant(grant.grant_id)
            self._tracker.mark_error(record.agent_session_id, "start failed")
            raise

        # 步骤 5：external session id 回填，状态转 active。
        record = self._tracker.activate(
            record.agent_session_id,
            external_session_id=launched.get("external_session_id"),
            resume_handle=launched.get("resume_handle"),
        )
        return AgentSessionRef(
            agent_session_id=record.agent_session_id,
            adapter_id=self.identity.adapter_id,
            external_session_id=record.external_session_id,
            runtime_instance_id=self.identity.instance_id,
            resume_handle=record.resume_handle,
        )

    async def _launch(
        self,
        request: SessionStart,
        plan: Optional[CapabilityInjectionPlan],
        bearer_token: Optional[str],
    ) -> dict[str, Any]:
        """按注入计划启动 / 接管 Runtime 会话（子类实现）。

        返回 ``{"external_session_id": ..., "resume_handle": ...}``（均可选）。
        """
        raise NotImplementedError

    # -- grant lease 与撤销（任务书 7.6 步骤 6/7） ----------------------------

    def renew_grants(self, agent_session_id: str) -> int:
        """在一次 Runtime 交互前延长已注入 Grant，保持 bearer token 不变。

        注入计划中的 grant 才是 Runtime 当前持有 token 所引用的 grant。
        因此这里不再轮换，也不续期历史 grant；撤销/关闭状态由服务层拒绝。
        """
        if self._binding_service is None:
            return 0
        plan = self._plans.get(agent_session_id)
        if plan is None:
            return 0
        self._binding_service.renew_grant(plan.grant_id)
        return 1

    def revoke_session_grants(self, agent_session_id: str) -> int:
        """同步撤销该 Session 的全部短期 grant（关闭/切换/撤销时调用）。"""
        if self._binding_service is None:
            return 0
        revoked = 0
        for grant_id in self._grants.get(agent_session_id, []):
            grant = self._binding_service.get_grant(grant_id)
            if grant is not None and grant.revoked_at is None:
                self._binding_service.revoke_grant(grant_id)
                revoked += 1
        return revoked

    # -- 事件与状态 -----------------------------------------------------------

    def sequencer_for(self, agent_session_id: str) -> EventSequencer:
        """该 Session 的单调序号分配器。"""
        seq = self._sequencers.get(agent_session_id)
        if seq is None:
            seq = EventSequencer()
            self._sequencers[agent_session_id] = seq
        return seq

    def emit(self, event: AgentEvent) -> AgentEvent:
        """事件进入投影（generation fencing 拒绝时返回原事件但不落影）。"""
        accepted, _snapshot, _reason = self._projector.apply(event)
        if accepted:
            self._on_event(event)
        return event

    def _on_event(self, event: AgentEvent) -> None:
        """子类钩子：事件已被投影（可转发到 Run 事件总线）。"""

    def unsupported_receipt(
        self,
        operation: str,
        capability: str,
        *,
        session: Optional[AgentSessionRef] = None,
        detail: Optional[dict[str, Any]] = None,
    ) -> CommandReceipt:
        """未支持能力的 typed receipt：FAILED + 稳定机器码，不静默忽略。"""
        return CommandReceipt(
            command_id=new_id("cmd"),
            state=ReceiptState.FAILED,
            aggregate=AggregateRef(
                type="agent_session",
                id=session.agent_session_id if session else "",
            ),
            error=ErrorEnvelope(
                code=UNSUPPORTED_CAPABILITY_CODE,
                message=(
                    f"adapter {self.identity.key} does not support "
                    f"{operation}: capability {capability!r} is unavailable"),
                category=ErrorCategory.STATE,
                recovery_hint="check probe capabilities; use a structured "
                              "transport adapter or degrade explicitly",
                retryable=False,
                detail={
                    "operation": operation,
                    "capability": capability,
                    "adapter_id": self.identity.adapter_id,
                    "instance_id": self.identity.instance_id,
                    **(detail or {}),
                },
            ),
        )

    # -- 控制面（默认按能力返回 unsupported receipt） -------------------------

    def send(
        self, session: AgentSessionRef, input: AgentInput
    ) -> AsyncIterator[AgentEvent]:
        return self._unsupported_stream(session, "send", "streaming")

    def resume(
        self, session: AgentSessionRef
    ) -> AsyncIterator[AgentEvent]:
        return self._unsupported_stream(session, "resume", "resume")

    async def _unsupported_stream(
        self, session: AgentSessionRef, operation: str, capability: str
    ) -> AsyncIterator[AgentEvent]:
        yield build_event(
            AgentEventType.RUNTIME_ERROR,
            self.sequencer_for(session.agent_session_id),
            agent_session_id=session.agent_session_id,
            external_session_id=session.external_session_id,
            payload={
                "code": UNSUPPORTED_CAPABILITY_CODE,
                "operation": operation,
                "capability": capability,
            },
        )

    async def steer(
        self, session: AgentSessionRef, input: AgentInput
    ) -> CommandReceipt:
        return self.unsupported_receipt("steer", "steer", session=session)

    async def interrupt(self, session: AgentSessionRef) -> CommandReceipt:
        return self.unsupported_receipt("interrupt", "interrupt", session=session)

    async def snapshot(self, session: AgentSessionRef) -> AgentSessionSnapshot:
        snap = self._projector.snapshot(session.agent_session_id)
        state = self._tracker.state(session.agent_session_id) or snap.state
        record = self._tracker.get(session.agent_session_id)
        return snap.model_copy(update={
            "adapter_id": self.identity.adapter_id,
            "state": state,
            "external_session_id": (
                session.external_session_id
                or (record.external_session_id if record else None)
                or snap.external_session_id),
        })

    @staticmethod
    def conversation_turn_timeout(
        conversation_thread_id: Any,
        default: float,
    ) -> Optional[float]:
        """对话整轮不设等待上限；做题 Worker 仍用 ``default``。

        对话一轮可以含多轮工具调用。固定秒数会让 Muteki 先收线，
        引擎进程却继续调 Capability。做题 Worker 的超时是调度设定。
        """
        if conversation_thread_id:
            return None
        return float(default)

    async def close(self, session: AgentSessionRef) -> None:
        """关闭：先 teardown Runtime 侧资源，再落内部状态并同步撤 grant。"""
        classification = await self._teardown(session)
        self._tracker.close(session.agent_session_id, classification)
        self.revoke_session_grants(session.agent_session_id)
        self._sequencers.pop(session.agent_session_id, None)

    async def _teardown(self, session: AgentSessionRef) -> str:
        """子类钩子：停止 Runtime 进程 / 连接，返回退出分类。"""
        from .sessions import EXIT_CLOSED
        return EXIT_CLOSED


__all__ = [
    "AdapterIdentity",
    "BaseExternalAgentAdapter",
    "UNSUPPORTED_CAPABILITY_CODE",
]
