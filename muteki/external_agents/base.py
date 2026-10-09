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

import asyncio
from dataclasses import dataclass
from typing import Any, AsyncIterator, Awaitable, Callable, Optional

from muteki.capability_bindings import (
    acp_config,
    agent_plugin,
    native_tools,
)
from muteki.capability_bindings.auth import issue_bearer_token
from muteki.platform.contracts.agent_events import (
    AgentFailure,
    FailureCategory,
    RuntimeErrorPayload,
    RuntimeExitedPayload,
    RuntimeWarningPayload,
    TurnCompletedPayload,
    TurnFailedPayload,
    agent_failure,
    exception_failure,
)
from muteki.platform.contracts.base import new_id
from muteki.platform.contracts.objects import AgentSession
from muteki.platform.contracts.capabilities import (
    CapabilityBinding,
    CapabilityDescriptor,
    CapabilityGrant,
    CapabilityInjectionPlan,
    InjectionKind,
    ThreadMode,
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
from .sessions import EXIT_FAILED, EventProjector, SessionTracker

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


#: Events after which a turn stream has reached its end state.
TURN_TERMINAL_EVENTS = frozenset({
    AgentEventType.TURN_COMPLETED,
    AgentEventType.TURN_FAILED,
    AgentEventType.RUNTIME_EXITED,
})
_WAIT_START = frozenset({
    AgentEventType.APPROVAL_REQUESTED,
    AgentEventType.USER_INPUT_REQUESTED,
})
_WAIT_END = frozenset({
    AgentEventType.APPROVAL_RESOLVED,
    AgentEventType.USER_INPUT_RESOLVED,
})


@dataclass(frozen=True)
class TurnLimits:
    """Time limits for one turn; ``None`` disables a limit.

    ``idle_s``: longest gap between two events from the engine.  It is paused
    while an approval or user-input request is outstanding, because the user
    is the one being waited for then.
    ``overall_s``: wall-clock cap for the whole turn (includes user waits).
    ``exit_drain_s``: once the engine process has exited, how long to keep
    reading events that were already buffered before declaring the exit.
    """

    idle_s: Optional[float] = None
    overall_s: Optional[float] = None
    exit_drain_s: float = 0.5


class TurnRunner:
    """Opt-in turn supervisor shared by adapters (chat mode).

    Wraps an adapter's raw turn event stream and guarantees it ends with a
    terminal event instead of hanging or stopping silently:

    - the engine process exits mid-turn: ``TURN_FAILED`` + ``RUNTIME_EXITED``
      carrying a typed ``external_agent.runtime_exited`` failure, as soon as
      the exit is observed (after ``limits.exit_drain_s`` of buffered events),
      not after a fixed timeout;
    - idle or overall limit exceeded: ``TURN_FAILED`` with
      ``external_agent.timeout`` (reason ``timeout.idle`` / ``timeout.overall``)
      after calling ``on_abort`` so the adapter can interrupt or stop the
      engine;
    - the stream ends without a terminal event while the process is alive:
      ``TURN_FAILED`` with ``external_agent.transport``
      (reason ``stream_ended_without_terminal``).

    Delivery signalling: the adapter calls ``mark_sent()`` right before
    writing the input to the runtime and ``ack()`` when the runtime accepted
    it (a protocol response, ``turn/started``, the first stream event ...).
    With ``auto_ack=True`` (default) the first event from the source is an
    ack.  Failures built by the runner set ``delivery_unknown=True`` exactly
    when the input was sent but never acknowledged, so callers know resending
    could duplicate it.  ``await runner.wait_delivered()`` resolves once acked.

    Usage inside an adapter's ``send``::

        runner = TurnRunner(
            self, session, turn_id=turn_id,
            limits=TurnLimits(
                idle_s=...,
                overall_s=self.conversation_turn_timeout(thread_id, default)),
            exit_watch=process.wait,          # () -> Awaitable[int | None]
            on_abort=self._abort_turn,        # (AgentFailure) -> Awaitable[None]
            diagnostics=stderr_log.text,      # () -> str, full stderr, not truncated
        )
        runner.mark_sent()
        await write_input(...)
        async for event in runner.stream(self._raw_turn_events(...)):
            yield event

    ``source`` events must already be built (``build_event``) and emitted
    (``self.emit``); the runner passes them through untouched and only builds
    and emits its own failure events with the session's sequencer.  After a
    terminal event the runner stops all timers and just drains the source.
    """

    def __init__(
        self,
        adapter: "BaseExternalAgentAdapter",
        session: AgentSessionRef,
        *,
        turn_id: Optional[str] = None,
        limits: TurnLimits = TurnLimits(),
        exit_watch: Optional[Callable[[], Awaitable[Optional[int]]]] = None,
        on_abort: Optional[Callable[[AgentFailure], Awaitable[None]]] = None,
        diagnostics: Optional[Callable[[], str]] = None,
        auto_ack: bool = True,
        run_id: Optional[str] = None,
        execution_generation: Optional[int] = None,
    ) -> None:
        self._adapter = adapter
        self._session = session
        self._turn_id = turn_id
        self._limits = limits
        self._exit_watch = exit_watch
        self._on_abort = on_abort
        self._diagnostics = diagnostics
        self._auto_ack = auto_ack
        self._run_id = run_id
        self._execution_generation = execution_generation
        self._sent = False
        self._delivered = asyncio.Event()

    def set_turn_id(self, turn_id: str) -> None:
        """Bind the runtime-assigned turn id once the turn has started."""
        self._turn_id = turn_id

    # -- delivery ------------------------------------------------------------

    def mark_sent(self) -> None:
        """The input is about to reach (or has reached) the runtime."""
        self._sent = True

    def ack(self) -> None:
        """The runtime accepted the input."""
        self._sent = True
        self._delivered.set()

    @property
    def delivered(self) -> bool:
        return self._delivered.is_set()

    @property
    def delivery_unknown(self) -> bool:
        return self._sent and not self._delivered.is_set()

    async def wait_delivered(self) -> None:
        await self._delivered.wait()

    # -- events --------------------------------------------------------------

    def _event(self, event_type: AgentEventType, payload: Any, native_type: str) -> AgentEvent:
        adapter = self._adapter
        return adapter.emit(build_event(
            event_type,
            adapter.sequencer_for(self._session.agent_session_id),
            agent_session_id=self._session.agent_session_id,
            external_session_id=self._session.external_session_id,
            run_id=self._run_id,
            execution_generation=self._execution_generation,
            turn_id=self._turn_id,
            native_type=native_type,
            payload=payload,
        ))

    def _failure_events(
        self, failure: AgentFailure, native_type: str, *,
        exit_code: Optional[int] = None, runtime_exited: bool = False,
    ) -> list[AgentEvent]:
        events = [self._event(
            AgentEventType.TURN_FAILED, TurnFailedPayload(error=failure),
            native_type)]
        if runtime_exited:
            events.append(self._event(
                AgentEventType.RUNTIME_EXITED,
                RuntimeExitedPayload(
                    classification=EXIT_FAILED, exit_code=exit_code,
                    error=failure),
                native_type))
        return events

    def _failure(
        self, category: FailureCategory, reason: str, message: str, *,
        detail: str = "", retryable: bool = False,
    ) -> AgentFailure:
        diagnostics = (self._diagnostics() if self._diagnostics else "") or ""
        if diagnostics:
            detail = f"{detail}\n{diagnostics}" if detail else diagnostics
        return self._adapter.failure(
            category, reason, message=message, detail=detail,
            retryable=retryable, delivery_unknown=self.delivery_unknown)

    async def _abort(self, failure: AgentFailure) -> AgentFailure:
        if self._on_abort is None:
            return failure
        try:
            await self._on_abort(failure)
        except Exception as exc:
            prefix = f"{failure.detail}\n" if failure.detail else ""
            return failure.model_copy(update={
                "detail": f"{prefix}abort failed: {type(exc).__name__}: {exc}"})
        return failure

    def _exited(
        self, exit_code: Optional[int], exit_error: Optional[BaseException]
    ) -> list[AgentEvent]:
        detail = (f"exit code {exit_code}" if exit_error is None
                  else "exit status unreadable: "
                       f"{type(exit_error).__name__}: {exit_error}")
        failure = self._failure(
            FailureCategory.RUNTIME_EXITED, "runtime_exited",
            "Runtime process exited before the turn finished", detail=detail)
        return self._failure_events(
            failure, "turn_runner.process_exit",
            exit_code=exit_code, runtime_exited=True)

    async def _timed_out(self, kind: str, seconds: Optional[float]) -> list[AgentEvent]:
        failure = self._failure(
            FailureCategory.TIMEOUT, f"timeout.{kind}",
            f"Turn exceeded its {kind} time limit of {seconds}s",
            detail=f"{kind} limit {seconds}s", retryable=True)
        failure = await self._abort(failure)
        return self._failure_events(failure, f"turn_runner.timeout.{kind}")

    # -- stream --------------------------------------------------------------

    async def stream(
        self, source: AsyncIterator[AgentEvent]
    ) -> AsyncIterator[AgentEvent]:
        loop = asyncio.get_running_loop()
        limits = self._limits
        iterator = source.__aiter__()
        started = loop.time()
        last_activity = started
        exit_task: Optional[asyncio.Future] = (
            asyncio.ensure_future(self._exit_watch())
            if self._exit_watch is not None else None)
        pending: Optional[asyncio.Future] = None
        waiting_on_user = 0
        terminal = False
        exit_seen_at: Optional[float] = None
        exit_code: Optional[int] = None
        exit_error: Optional[BaseException] = None

        def note_exit() -> None:
            nonlocal exit_seen_at, exit_code, exit_error
            exit_seen_at = loop.time()
            try:
                exit_code = exit_task.result()
            except Exception as exc:
                exit_error = exc

        try:
            while True:
                if pending is None:
                    pending = asyncio.ensure_future(iterator.__anext__())
                watched: set = {pending}
                if exit_task is not None and exit_seen_at is None and not terminal:
                    watched.add(exit_task)
                deadlines: list[tuple[float, str]] = []
                if not terminal:
                    if limits.overall_s is not None:
                        deadlines.append((started + limits.overall_s, "overall"))
                    if limits.idle_s is not None and waiting_on_user == 0:
                        deadlines.append((last_activity + limits.idle_s, "idle"))
                    if exit_seen_at is not None:
                        deadlines.append((
                            max(exit_seen_at, last_activity) + limits.exit_drain_s,
                            "exit"))
                timeout = (max(0.0, min(deadlines)[0] - loop.time())
                           if deadlines else None)
                done, _ = await asyncio.wait(
                    watched, timeout=timeout,
                    return_when=asyncio.FIRST_COMPLETED)

                if exit_task is not None and exit_task in done and exit_seen_at is None:
                    note_exit()
                if pending in done:
                    try:
                        event = pending.result()
                    except StopAsyncIteration:
                        pending = None
                        break
                    pending = None
                    last_activity = loop.time()
                    if self._auto_ack:
                        self.ack()
                    if event.event_type in _WAIT_START:
                        waiting_on_user += 1
                    elif event.event_type in _WAIT_END:
                        waiting_on_user = max(0, waiting_on_user - 1)
                    if event.event_type in TURN_TERMINAL_EVENTS:
                        terminal = True
                    yield event
                    continue
                if terminal:
                    continue
                now = loop.time()
                expired = {name for when, name in deadlines if when <= now}
                if "overall" in expired:
                    for event in await self._timed_out("overall", limits.overall_s):
                        yield event
                    return
                if "idle" in expired:
                    for event in await self._timed_out("idle", limits.idle_s):
                        yield event
                    return
                if "exit" in expired:
                    for event in self._exited(exit_code, exit_error):
                        yield event
                    return

            # The source ended on its own.
            if terminal:
                return
            if exit_task is not None and exit_seen_at is None:
                try:
                    await asyncio.wait_for(
                        asyncio.shield(exit_task), timeout=limits.exit_drain_s)
                except asyncio.TimeoutError:
                    pass
                except Exception:
                    pass
                if exit_task.done():
                    note_exit()
            if exit_seen_at is not None:
                for event in self._exited(exit_code, exit_error):
                    yield event
                return
            failure = self._failure(
                FailureCategory.TRANSPORT, "stream_ended_without_terminal",
                "Runtime event stream ended without a terminal turn event")
            for event in self._failure_events(failure, "turn_runner.stream_end"):
                yield event
        finally:
            tasks = [t for t in (pending, exit_task) if t is not None]
            for task in tasks:
                if not task.done():
                    task.cancel()
            if tasks:
                await asyncio.gather(*tasks, return_exceptions=True)
            aclose = getattr(iterator, "aclose", None)
            if aclose is not None:
                await aclose()


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

    # Enable cross-turn hint caching only with observed native compaction events.
    context_compaction_events = False

    # Executable a subclass probes; set in ``__init__`` (``driver`` is the
    # CliDriver of the CLI compatibility adapter, ``_cli_path`` the Claude CLI).
    _binary: str = ""
    _cli_path: str = ""
    driver: Any = None

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
        self._capability_gateway = None
        self._context_revisions: dict[str, int] = {}
        self._sequencers: dict[str, EventSequencer] = {}
        self._grants: dict[str, list[str]] = {}
        self._plans: dict[str, CapabilityInjectionPlan] = {}
        self._probe_cache: Optional[CapabilityProbeReport] = None
        self._turn_outcomes: dict[tuple[str, Optional[str], Optional[int]], dict[str, Any]] = {}
        self.probe_environment_factory: Optional[Callable[[], dict[str, str]]] = None
        # User-configured CLI arguments from the Runtime instance settings.
        self.launch_args: tuple[str, ...] = ()

    def _with_launch_args(self, argv: list[str]) -> list[str]:
        """Insert configured arguments right after the executable.

        That is the global-option position for every supported CLI, including
        ones whose adapter argv ends in a subcommand (``app-server``, ``acp``).
        """
        if not self.launch_args or not argv:
            return list(argv)
        return [argv[0], *self.launch_args, *argv[1:]]

    # -- platform wiring ------------------------------------------------------

    def attach_platform(self, *, store: Any, binding_service: Any) -> None:
        """Late-bind PlatformStore and binding service left unset at construction.

        Adapters may be built before registration; without a store the
        SessionTracker keeps AgentSession records in memory only.
        """
        self._tracker.attach_store(store)
        if self._binding_service is None:
            self._binding_service = binding_service

    @property
    def has_binding_service(self) -> bool:
        return self._binding_service is not None

    def session_record(self, agent_session_id: str) -> Optional[AgentSession]:
        """AgentSession record tracked by this adapter (store or memory)."""
        return self._tracker.get(agent_session_id)

    # --  probe -------------------------------------------------------------

    async def probe(self, request: ProbeRequest) -> AgentCapabilities:
        """基础层默认：保守静态能力（子类应实测后覆写）。"""
        return conservative_capabilities()

    def probe_binary(self) -> str:
        """Executable this adapter probes, for the probe cache key."""
        from .probe_cache import driver_binary
        if self.driver is not None:
            return driver_binary(self.driver)
        return str(self._binary or self._cli_path or "")

    def probe_version_argv(self) -> list[str]:
        """Version command for the same executable entry point as the session."""
        return [self.probe_binary(), "--version"]

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

    def bind_capability_gateway(self, gateway: Any) -> None:
        """Use the same live descriptor as HTTP MCP for every injection kind."""
        self._capability_gateway = gateway

    def context_revision(self, session: AgentSessionRef) -> int:
        """Native context changes invalidate once-per-session application hints."""
        return self._context_revisions.get(session.agent_session_id, 0)

    def _context_compacted(self, agent_session_id: str) -> None:
        self._context_revisions[agent_session_id] = self._context_revisions.get(agent_session_id, 0) + 1

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
        if self._capability_gateway is not None:
            descriptor = await self._capability_gateway.describe(binding.binding_id)
        else:
            descriptor = self._describe(binding)
        options = request.options
        if (self._capability_gateway is None
                and options.is_conversation
                and options.chat_tools is not None):
            tools = descriptor.tools if options.chat_control_enabled and capability_enabled("mcp", "muteki-control") else []
            descriptor = descriptor.model_copy(update={"tools": [*tools, *options.chat_tools]})
        if (not capability_enabled("mcp", "muteki-control")
                and not any(not tool.name.startswith("muteki_") for tool in descriptor.tools)):
            caps = caps.model_copy(update={"mcp": False, "acp_mcp_config": False})
        kind = select_injection_kind(caps)
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
        options = request.options
        return self._binding_service.issue_binding(
            thread_id,
            options.principal_id or "system",
            options.thread_mode or ThreadMode.CONVERSATION,
            resource_scopes=list(options.resource_scopes) if options.resource_scopes else None,
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
                audience = request.options.audience or f"adapter:{self.identity.key}"
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
        except BaseException:
            # Cancellation also aborts an open (T3 ProviderSessionManager's
            # onError finalizer). Never leave its grant or preparing record.
            try:
                self.revoke_session_grants(record.agent_session_id)
            finally:
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

    @property
    def engine(self) -> str:
        """Failure-reporting engine key (``AgentFailure.engine``)."""
        from .factory import engine_for_adapter
        return engine_for_adapter(self.identity.adapter_id) or self.identity.adapter_id

    def failure(
        self,
        category: FailureCategory | str,
        reason: str,
        *,
        message: str,
        detail: Any = "",
        retryable: bool = False,
        native_code: Optional[str] = None,
        delivery_unknown: bool = False,
    ) -> AgentFailure:
        """Typed failure for TURN_FAILED / RUNTIME_ERROR payloads."""
        return agent_failure(
            category, reason, engine=self.engine, message=message,
            detail=detail, retryable=retryable, native_code=native_code,
            delivery_unknown=delivery_unknown,
        )

    def exception_failure(
        self,
        exc: BaseException,
        category: FailureCategory | str,
        reason: str,
        *,
        message: str,
        retryable: bool = False,
        delivery_unknown: bool = False,
    ) -> AgentFailure:
        """Typed failure from an exception: full text kept in ``detail``."""
        return exception_failure(
            exc, category, reason, engine=self.engine, message=message,
            retryable=retryable, delivery_unknown=delivery_unknown,
        )

    def sequencer_for(self, agent_session_id: str) -> EventSequencer:
        """该 Session 的单调序号分配器。"""
        seq = self._sequencers.get(agent_session_id)
        if seq is None:
            seq = EventSequencer()
            self._sequencers[agent_session_id] = seq
        return seq

    def emit(self, event: AgentEvent) -> AgentEvent:
        """事件进入投影（generation fencing 拒绝时返回原事件但不落影）。"""
        key = (event.agent_session_id, event.turn_id, event.execution_generation)
        outcome = self._turn_outcomes.get(key, {})
        if (outcome.get("interrupted")
                and event.event_type is AgentEventType.TURN_FAILED
                and event.payload.get("error", {}).get("category") == "cancelled"):
            # Native abort/cancel codes do not identify who stopped the turn.
            # The interrupt call is the authority for a user-requested stop.
            failure = dict(event.payload["error"])
            failure["reason"] = "interrupted"
            event = event.model_copy(update={
                "payload": {**event.payload, "error": failure},
            })
        elif (outcome.get("denied") and not outcome.get("interrupted")
                and (event.event_type is AgentEventType.TURN_COMPLETED
                     or (event.event_type is AgentEventType.TURN_FAILED
                         and event.payload.get("error", {}).get("category") == "cancelled"))):
            event = event.model_copy(update={
                "event_type": AgentEventType.TURN_FAILED,
                "payload": TurnFailedPayload(error=self.failure(
                    FailureCategory.CANCELLED, "approval_denied",
                    message="Turn ended after a rejected approval or question"),
                    native={"terminal": event.payload}).model_dump(mode="json"),
            })
        accepted, _snapshot, _reason = self._projector.apply(event)
        if accepted:
            if event.event_type is AgentEventType.TURN_STARTED:
                self._turn_outcomes[key] = {"approvals": {}}
            elif event.event_type is AgentEventType.APPROVAL_REQUESTED:
                outcome.setdefault("approvals", {})[event.payload.get("approval_id")] = event.payload.get("approval_kind")
            elif event.event_type is AgentEventType.APPROVAL_RESOLVED:
                kind = outcome.get("approvals", {}).get(event.payload.get("approval_id"))
                if event.payload.get("decision") in {"deny", "reject"} and kind != "plan_exit":
                    outcome["denied"] = True
            elif event.event_type is AgentEventType.USER_INPUT_RESOLVED:
                if event.payload.get("outcome") in {"cancelled", "declined"}:
                    outcome["denied"] = True
            elif event.event_type is AgentEventType.MESSAGE_COMPLETED:
                if event.payload.get("role", "assistant") == "assistant" and str(event.payload.get("text") or "").strip():
                    outcome["assistant_text"] = True
            if event.event_type in TURN_TERMINAL_EVENTS:
                self._turn_outcomes.pop(key, None)
            self._on_event(event)
        return event

    def _mark_turn_interrupted(self, agent_session_id: str) -> None:
        for (sid, _tid, _generation), outcome in self._turn_outcomes.items():
            if sid == agent_session_id:
                outcome["interrupted"] = True

    def completed_turn_events(
        self, seq: EventSequencer, *, text: str, common: dict[str, Any],
        native_type: str, payload: Optional[TurnCompletedPayload] = None,
    ) -> list[AgentEvent]:
        """A native successful terminal is valid even without assistant text."""
        from muteki.platform.contracts.agent_events import MessageCompletedPayload
        events = []
        if text.strip():
            events.append(self.emit(build_event(
                AgentEventType.MESSAGE_COMPLETED, seq,
                payload=MessageCompletedPayload(text=text), **common)))
        else:
            events.append(self.emit(build_event(
                AgentEventType.RUNTIME_WARNING, seq,
                payload=RuntimeWarningPayload(
                    kind="degraded", code="external_agent.empty_assistant",
                    message="Runtime ended the turn without assistant text"),
                **common)))
        events.append(self.emit(build_event(
            AgentEventType.TURN_COMPLETED, seq, native_type=native_type,
            payload=payload or TurnCompletedPayload(), **common)))
        return events

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

    async def unsupported_input_stream(
        self, session: AgentSessionRef, input: AgentInput
    ) -> AsyncIterator[AgentEvent]:
        """Typed failure for an AgentInput variant this adapter cannot deliver."""
        yield build_event(
            AgentEventType.RUNTIME_ERROR,
            self.sequencer_for(session.agent_session_id),
            agent_session_id=session.agent_session_id,
            external_session_id=session.external_session_id,
            payload=RuntimeErrorPayload(error=self.failure(
                FailureCategory.UNSUPPORTED, "input.unsupported_kind",
                message=(
                    f"adapter {self.identity.key} cannot deliver "
                    f"{input.kind!r} input through send"),
                native_code=UNSUPPORTED_CAPABILITY_CODE)),
        )

    async def _unsupported_stream(
        self, session: AgentSessionRef, operation: str, capability: str
    ) -> AsyncIterator[AgentEvent]:
        yield build_event(
            AgentEventType.RUNTIME_ERROR,
            self.sequencer_for(session.agent_session_id),
            agent_session_id=session.agent_session_id,
            external_session_id=session.external_session_id,
            payload=RuntimeErrorPayload(error=self.failure(
                FailureCategory.UNSUPPORTED, "capability.unsupported",
                message=(
                    f"adapter {self.identity.key} does not support "
                    f"{operation}: capability {capability!r} is unavailable"),
                native_code=UNSUPPORTED_CAPABILITY_CODE)),
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
        try:
            classification = await self._teardown(session)
            self._tracker.close(session.agent_session_id, classification)
        finally:
            # Scoped credentials must not survive a failed/interrupted close.
            # The teardown error remains visible to the caller.
            self.revoke_session_grants(session.agent_session_id)
            self._sequencers.pop(session.agent_session_id, None)
            self._context_revisions.pop(session.agent_session_id, None)

    async def _teardown(self, session: AgentSessionRef) -> str:
        """子类钩子：停止 Runtime 进程 / 连接，返回退出分类。"""
        from .sessions import EXIT_CLOSED
        return EXIT_CLOSED


__all__ = [
    "AdapterIdentity",
    "BaseExternalAgentAdapter",
    "TURN_TERMINAL_EVENTS",
    "TurnLimits",
    "TurnRunner",
    "UNSUPPORTED_CAPABILITY_CODE",
]
