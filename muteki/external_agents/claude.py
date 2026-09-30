"""Claude Agent SDK 结构化 Adapter（RUNTIME-02，任务书 7.2）。

以 2026-08-21 核验结论为准（docs/research/third_party_verification.md
§CLAUDE；SDK pin：Python ``claude-agent-sdk`` 0.2.x，CLI 2.1.x）：

- 交互式会话用 ``ClaudeSDKClient``（``connect/query/receive_messages/
  interrupt/set_model/set_permission_mode``）；一次性 ``query()`` 无
  interrupt / 追加消息能力，本 Adapter 不使用。
- session resume：``ClaudeAgentOptions.resume`` / ``fork_session``；
  external session id 取自 ``system/init`` 消息的 ``session_id``。
- 审批：``can_use_tool`` 回调 → APPROVAL_REQUESTED → ``respond_approval``
  → ``PermissionResultAllow/Deny``。**边界（核验 §CLAUDE-3）**：
  ``can_use_tool`` 只在权限规则评估为 ask 时触发；要观察/门控每一次工具
  调用需用 ``PreToolUse`` hook（``hooks`` 选项透传），hook 返回 allow 会
  跳过 can_use_tool。该边界写入 probe degradations，不静默。
- permission mode：``default/acceptEdits/plan/bypassPermissions/dontAsk/auto``；
  运行中可 ``set_permission_mode`` 切换。
- MCP 注入：``ClaudeAgentOptions.mcp_servers``（Python 字段名；TS 侧为
  ``mcpServers``）；能力不匹配（无网络 endpoint 场景）时按核验
  §CLAUDE-10 用进程内 SDK MCP server（``tool()`` +
  ``create_sdk_mcp_server()``，即 Native Tool——本质是 in-process MCP，
  复用 CAP-02 ``native_tools.build_sdk_mcp_server``）。
- usage：``ResultMessage.usage/modelUsage/total_cost_usd``；
  ``terminal_reason``（如 ``aborted_streaming``）标识被 interrupt 的 turn。
- SDK 消息按类名鸭子识别（对 SDK 版本 churn 更稳），未知消息 /
  content block 以 RUNTIME_WARNING + native 保留，不改核心状态机。

Headless CLI 兼容路径（``claude -p --output-format stream-json`` +
``--resume``）由 ``muteki.solver.cli_driver.CliDriverAdapter``
（``cli_adapter_for("claude")``）提供，本模块不 import solver 层；SDK
不可用 / 不满足版本门槛时 probe 在 degradations 里显式指向该降级。
"""

from __future__ import annotations

import asyncio
import json
import importlib
import importlib.util
import inspect
import subprocess

from .probe_environment import subprocess_environment
from typing import Any, AsyncIterator, Optional

from muteki.capability_bindings import acp_config, native_tools
from muteki.platform.contracts.base import new_id
from muteki.platform.contracts.capabilities import (
    BindingContext,
    CapabilityInjectionPlan,
    InjectionKind,
    ThreadMode,
)
from muteki.platform.contracts.external_agents import (
    ACCESS_MODE_VALUES,
    AccessMode,
    AgentCapabilities,
    AgentEvent,
    AgentEventType,
    AgentInput,
    AgentSessionRef,
    ProbeRequest,
    SessionStart,
)
from muteki.platform.contracts.receipts import AggregateRef, CommandReceipt, ReceiptState

from .base import BaseExternalAgentAdapter
from .approvals import ApprovalDecision, ApprovalRequest
from .attachment_input import claude_query_prompt
from .capabilities import (
    CapabilityProbeReport,
    SOURCE_PROBE,
    SOURCE_REPORTED,
    SOURCE_STATIC,
    conservative_capabilities,
)
from .events import build_event
from .runtime_capabilities import (
    RuntimeCapabilityItem,
    RuntimeCapabilitySnapshot,
    dynamic_command_item,
)
from .sessions import EXIT_CLOSED, EXIT_RESUMABLE

#: 默认 CLI 路径与 MCP server 名。
DEFAULT_CLAUDE_CLI = "claude"
MCP_SERVER_NAME = acp_config.DEFAULT_SERVER_NAME

#: 已核验的 permission mode 全集（§CLAUDE-5，types.py / sdk.d.ts 一致）。
PERMISSION_MODES = (
    "default", "acceptEdits", "plan", "bypassPermissions", "dontAsk", "auto")


def _permission_mode_for(request: SessionStart, fallback: str) -> str:
    if not request.access_mode:
        return str(request.permission_mode or fallback)
    try:
        mode = AccessMode(str(request.access_mode))
    except ValueError as exc:
        raise ValueError(
            f"claude unsupported access mode: {request.access_mode!r}") from exc
    return {
        AccessMode.SUPERVISED: "default",
        AccessMode.AUTO_ACCEPT_EDITS: "acceptEdits",
        AccessMode.AUTO: "auto",
        AccessMode.FULL_ACCESS: "bypassPermissions",
    }[mode]

#: Headless CLI 兼容降级路径说明（实现位于 solver 层，本包不反向依赖）。
HEADLESS_FALLBACK = (
    "muteki.solver.cli_driver.cli_adapter_for('claude')"
    "（claude -p --output-format stream-json --verbose + --resume；"
    "无带内 interrupt/steer，审批只能经 --permission-mode 固化）")


def _sdk_available() -> bool:
    return importlib.util.find_spec("claude_agent_sdk") is not None


def _import_sdk() -> Any:
    """惰性 import claude_agent_sdk；缺失时抛带修复提示的 RuntimeError。"""
    try:
        return importlib.import_module("claude_agent_sdk")
    except ImportError as exc:
        raise RuntimeError(
            "claude_agent_sdk 未安装：pip install claude-agent-sdk；"
            f"或走 Headless CLI 降级路径 {HEADLESS_FALLBACK}"
        ) from exc


def _sdk_option_is_supported(sdk: Any, field_name: str) -> bool:
    """只在 SDK 明确定义字段时传入版本兼容选项。

    Claude Agent SDK 的 ``ClaudeAgentOptions`` 会随版本删除或新增字段。
    ``permission_mode='bypassPermissions'`` 是完全访问的稳定语义；旧版的
    补充字段只能在运行时类型明确列出时才传入，避免当前 SDK 因未知参数
    整个会话启动失败，也不筛掉 ``mcp_servers`` 等必需注入字段。
    """
    options_type = getattr(sdk, "ClaudeAgentOptions", None)
    if options_type is None:
        return False
    dataclass_fields = getattr(options_type, "__dataclass_fields__", None)
    if isinstance(dataclass_fields, dict):
        return field_name in dataclass_fields
    annotations = getattr(options_type, "__annotations__", None)
    if isinstance(annotations, dict):
        return field_name in annotations
    try:
        return field_name in inspect.signature(options_type).parameters
    except (TypeError, ValueError):
        return False


def _cli_version(binary: str, *, timeout: float = 15.0) -> str:
    try:
        result = subprocess.run(
            [binary, "--version"], capture_output=True, text=True,
            encoding="utf-8", errors="replace", timeout=timeout, env=subprocess_environment(),
        )
    except (FileNotFoundError, subprocess.TimeoutExpired, OSError):
        return ""
    lines = (result.stdout or result.stderr or "").strip().splitlines()
    return lines[0].strip()[:120] if result.returncode == 0 and lines else ""


def _jsonable(value: Any) -> Any:
    """把 SDK 消息里的原生对象（dataclass 等）转成 JSON 安全结构。

    事件 payload 必须可 JSON 序列化（事件总线 / 前端消费）；转不动时
    退化为 repr 字符串。
    """
    if value is None or isinstance(value, (str, int, float, bool)):
        return value
    if isinstance(value, dict):
        return {str(k): _jsonable(v) for k, v in value.items()}
    if isinstance(value, (list, tuple)):
        return [_jsonable(v) for v in value]
    if hasattr(value, "__dict__"):
        return _jsonable(vars(value))
    return repr(value)[:500]


class ClaudeSDKAdapter(BaseExternalAgentAdapter):
    """Claude Agent SDK（ClaudeSDKClient 交互模式）的结构化 Adapter。

    构造参数（除基类外）：

    - ``cli_path``：claude CLI 路径（SDK 底层仍拉起 CLI 进程）；
    - ``model`` / ``permission_mode``：默认值，可被 SessionStart 覆盖；
    - ``gateway``：Native Tool（in-process MCP）注入时需要的
      AgentCapabilityGateway；MCP/HTTP 注入不需要；
    - ``approval_timeout_s``：can_use_tool 等待审批答复的超时，超时按
      deny 应答（不悬挂 Runtime）。
    """

    def __init__(
        self,
        *,
        cli_path: str = DEFAULT_CLAUDE_CLI,
        instance_id: str = "default",
        store: Any = None,
        binding_service: Any = None,
        gateway_endpoint: str = "",
        descriptor_provider: Any = None,
        gateway: Any = None,
        default_cwd: Optional[str] = None,
        default_env: Optional[dict[str, str]] = None,
        model: Optional[str] = None,
        permission_mode: str = "default",
        approval_timeout_s: int = 300,
    ) -> None:
        super().__init__(
            "claude.agent_sdk",
            instance_id=instance_id,
            store=store,
            binding_service=binding_service,
            gateway_endpoint=gateway_endpoint,
            descriptor_provider=descriptor_provider,
        )
        if permission_mode not in PERMISSION_MODES:
            raise ValueError(
                f"unknown permission_mode {permission_mode!r}; "
                f"allowed: {', '.join(PERMISSION_MODES)}")
        self._cli_path = cli_path
        self._gateway = gateway
        self._default_cwd = default_cwd
        self._default_env = dict(default_env or {})
        self._model = model
        self._permission_mode = permission_mode
        self._approval_timeout_s = int(approval_timeout_s)
        self._runs: dict[str, dict[str, Any]] = {}

    # -- capability probe ------------------------------------------------------

    async def probe(self, request: ProbeRequest) -> AgentCapabilities:
        """probe：CLI --version 实测 + SDK 可用性；SDK 方法面如实标注来源。

        SDK 行为面（can_use_tool 回调语义、resume、interrupt）无法用
        无模型调用的方式实测，按 SDK 类型契约标注 ``adapter_reported``；
        binary 与 SDK 安装状态为 ``probe`` 实测。
        """
        field_sources: dict[str, str] = {}
        degradations: list[str] = []
        version = _cli_version(self._cli_path)
        sdk_ok = _sdk_available()
        probed = bool(version) and sdk_ok

        caps = conservative_capabilities(
            transport_kind="sdk",
            capability_source=SOURCE_PROBE if probed else SOURCE_STATIC,
        )
        caps.runtime_version = version

        if not version:
            degradations.append(
                f"claude CLI 不可用或 --version 失败（{self._cli_path}）")
        if not sdk_ok:
            degradations.append(
                "claude_agent_sdk 未安装：pip install claude-agent-sdk；"
                f"结构化能力整体降级，Headless 兼容路径：{HEADLESS_FALLBACK}")

        def report(field_name: str, value: bool) -> bool:
            field_sources[field_name] = (
                SOURCE_REPORTED if probed else SOURCE_STATIC)
            return bool(value and probed)

        # SDK 交互客户端的方法面（§CLAUDE-1/2/3/4/5/6/7/8 已核验）。
        caps.streaming = report("streaming", True)
        caps.resume = report("resume", True)
        caps.session_persistence = report("session_persistence", True)
        caps.steer = report("steer", True)        # streaming input / client.query
        caps.interrupt = report("interrupt", True)  # ClaudeSDKClient.interrupt
        caps.approval = report("approval", True)    # can_use_tool
        caps.user_input = report("user_input", True)
        caps.fork = report("fork", True)            # fork_session
        caps.mcp = report("mcp", True)              # mcp_servers
        caps.native_tool_binding = report("native_tool_binding", True)
        caps.structured_output = report("structured_output", True)
        caps.subagents = report("subagents", True)
        caps.tool_events = report("tool_events", True)
        caps.usage_events = report("usage_events", True)
        # ClaudeSDKClient.query accepts multimodal user content blocks.
        caps.image_input = report("image_input", True)
        caps.permission_modes = list(PERMISSION_MODES)
        field_sources["permission_modes"] = (
            SOURCE_REPORTED if probed else SOURCE_STATIC)
        caps.access_modes = list(ACCESS_MODE_VALUES) if probed else []
        field_sources["access_modes"] = (
            SOURCE_REPORTED if probed else SOURCE_STATIC)
        caps.protocol_version = version

        # 核验确认的能力边界，必须显式记录（不静默关闭）。
        degradations.append(
            "can_use_tool 只在权限规则评估为 ask 时触发（§CLAUDE-3）："
            "allowed_tools/permission_mode/settings 放行的调用不触发回调；"
            "全程工具门控需经 options.hooks 配置 PreToolUse hook，"
            "且 hook 返回 allow 会跳过 can_use_tool")
        degradations.append(
            "Native Tool 即 in-process MCP server（§CLAUDE-10）："
            "server→client 的 sampling/elicitation/roots/logging/progress "
            "不转发；mcp 1.x 上被 Claude Code 放弃的工具调用不取消")
        if not request.include_models:
            field_sources["supported_models"] = SOURCE_STATIC

        self._probe_cache = CapabilityProbeReport(
            adapter_id=self.id, instance_id=self.identity.instance_id,
            capabilities=caps, binary_path=self._cli_path,
            field_sources=field_sources, degradations=degradations,
            detail="" if probed else "CLI/SDK 不可用，使用保守默认",
            healthy_override=probed)
        return caps

    # -- 启动（任务书 7.6 步骤 4 的 Runtime 侧动作） ------------------------------

    def _build_mcp_servers(
        self,
        request: SessionStart,
        plan: Optional[CapabilityInjectionPlan],
        bearer_token: Optional[str],
    ) -> dict[str, Any]:
        """按注入计划生成 ``ClaudeAgentOptions.mcp_servers``。

        MCP/HTTP 形态带 bearer 头；NATIVE_TOOL 形态现场
        构造进程内 SDK MCP server（handler 闭包持有 BindingContext，
        进程内调用不需要 bearer token）。
        """
        if plan is None:
            return {}
        if plan.injection_kind is InjectionKind.MCP:
            if not plan.gateway_endpoint or not bearer_token:
                return {}
            return {
                MCP_SERVER_NAME: {
                    "type": "http",
                    "url": plan.gateway_endpoint,
                    "headers": {
                        "Authorization": f"Bearer {bearer_token}",
                    },
                },
            }
        if plan.injection_kind is InjectionKind.NATIVE_TOOL:
            if self._gateway is None:
                raise RuntimeError(
                    "NATIVE_TOOL 注入需要 AgentCapabilityGateway"
                    "（构造 Adapter 时传 gateway=）；或改用 MCP/HTTP 注入")
            binding = self._resolve_binding(request)
            grant = (self._binding_service.get_grant(plan.grant_id)
                     if self._binding_service is not None and plan.grant_id
                     else None)
            if binding is None or grant is None:
                raise RuntimeError(
                    "NATIVE_TOOL 注入缺少 binding/grant 上下文")
            context = BindingContext(
                binding_id=binding.binding_id,
                binding_version=binding.binding_version,
                grant_id=grant.grant_id,
                thread_id=binding.thread_id,
                agent_session_id=request.agent_session_id,
                principal_id=binding.principal_id,
                audience=grant.audience,
                mode=binding.mode if isinstance(binding.mode, ThreadMode)
                else ThreadMode(str(binding.mode)),
            )
            descriptor = self._describe(binding)
            descriptor = descriptor.model_copy(update={"tools": list(plan.tool_descriptions)})
            server = native_tools.build_sdk_mcp_server(
                descriptor, context, self._gateway,
                name=str(plan.runtime_config.get(
                    "server_name", MCP_SERVER_NAME)))
            return {MCP_SERVER_NAME: server}
        # 其他注入种类（CLI_SKILL 等）不由 SDK mcp_servers 承载。
        return {}

    def _options_kwargs(
        self,
        request: SessionStart,
        mcp_servers: dict[str, Any],
        *,
        resume: Optional[str] = None,
        sdk: Optional[Any] = None,
    ) -> dict[str, Any]:
        options = request.options
        permission_mode = _permission_mode_for(request, self._permission_mode)
        kwargs: dict[str, Any] = {
            "cwd": str(options.get("cwd") or self._default_cwd or "."),
            "cli_path": self._cli_path,
            "permission_mode": permission_mode,
            "can_use_tool": self._make_can_use_tool(request.agent_session_id),
        }
        if (
            permission_mode == "bypassPermissions"
            and sdk is not None
            and _sdk_option_is_supported(
                sdk, "allow_dangerously_skip_permissions")
        ):
            # 当前 SDK 通过 permission_mode 本身实现完全访问。仅在旧 SDK
            # 明确声明该补充字段时传入，避免版本更新后因未知参数启动失败。
            kwargs["allow_dangerously_skip_permissions"] = True
        model = request.model or self._model
        if model:
            kwargs["model"] = model
        if request.effort in {"off", "on"}:
            kwargs["settings"] = json.dumps({"alwaysThinkingEnabled": request.effort == "on"})
        elif request.effort and request.effort != "default":
            # This is a model-scoped native option, validated by Conversation.
            # Older SDKs can forward the same CLI flag through extra_args.
            if sdk is not None and _sdk_option_is_supported(sdk, "effort"):
                kwargs["effort"] = request.effort
            else:
                kwargs["extra_args"] = {"effort": request.effort}
        if mcp_servers:
            kwargs["mcp_servers"] = mcp_servers
        if options.get("allowed_tools"):
            kwargs["allowed_tools"] = list(options["allowed_tools"])
        if options.get("plugins"):
            kwargs["plugins"] = list(options["plugins"])
        # Agent SDK 的 ``skills`` 是结构化启用入口：它会把 Skill 工具加入
        # allowed tools，并按 setting sources 让 Runtime 自行发现/按需加载。
        # 不再由 Muteki 读取 SKILL.md 全文塞进用户消息。
        kwargs["skills"] = options.get("skills", "all")
        if options.get("hooks"):
            # PreToolUse 等 hook 透传（§CLAUDE-3：全程工具门控通道）。
            kwargs["hooks"] = dict(options["hooks"])
        if options.get("max_turns"):
            kwargs["max_turns"] = int(options["max_turns"])
        if options.get("env"):
            kwargs["env"] = {**self._default_env,
                             **{k: str(v) for k, v in options["env"].items()}}
        elif self._default_env:
            kwargs["env"] = dict(self._default_env)
        if resume:
            kwargs["resume"] = resume
        if options.get("fork_session"):
            kwargs["fork_session"] = True
        return kwargs

    async def _launch(
        self,
        request: SessionStart,
        plan: Optional[CapabilityInjectionPlan],
        bearer_token: Optional[str],
    ) -> dict[str, Any]:
        sdk = _import_sdk()
        sid = request.agent_session_id
        mcp_servers = self._build_mcp_servers(request, plan, bearer_token)
        kwargs = self._options_kwargs(
            request, mcp_servers, resume=request.resume_handle, sdk=sdk)
        client = sdk.ClaudeSDKClient(options=sdk.ClaudeAgentOptions(**kwargs))
        try:
            await client.connect()
        except Exception:
            raise
        server_info = await client.get_server_info() or {}
        commands = (
            server_info.get("commands")
            or server_info.get("slash_commands")
            or []
        )
        mcp_servers: list[dict[str, Any]] = []
        try:
            mcp_status = await client.get_mcp_status()
            mcp_servers = list(mcp_status.get("mcpServers") or [])
        except Exception:  # noqa: BLE001 — 状态查询失败进入诊断，不伪造成功
            mcp_servers = []
        self._runs[sid] = {
            "client": client,
            "options_kwargs": kwargs,
            "mcp_injected": bool(mcp_servers),
            "mcp_kind": (plan.injection_kind.value if plan else ""),
            "turns": 0,
            "approvals": {},       # request_id -> asyncio.Future
            "event_queue": asyncio.Queue(),
            "session_id": request.resume_handle,
            "init_native": {
                **dict(server_info),
                "slash_commands": list(commands),
                "mcp_servers": mcp_servers,
            },
            "capability_revision": 1,
        }
        # session id 要等首个 system/init 消息；resume_handle 沿用已知 id。
        return {
            "external_session_id": request.resume_handle,
            "resume_handle": request.resume_handle,
        }

    # -- 审批（can_use_tool → APPROVAL_REQUESTED/RESOLVED） ----------------------

    def _make_can_use_tool(self, agent_session_id: str):
        """生成 SDK can_use_tool 回调。

        事件经 per-session 队列汇入 send 流（保证序号单调）；回调在等待
        审批答复时阻塞 SDK 控制通道，超时按 deny 应答。
        """
        async def can_use_tool(tool_name: str, input: dict[str, Any],
                               context: Any) -> Any:
            sdk = _import_sdk()
            ctx = self._runs.get(agent_session_id)
            if ctx is None:
                return sdk.PermissionResultDeny(
                    behavior="deny", message="session context lost")
            seq = self.sequencer_for(agent_session_id)
            record = self._tracker.get(agent_session_id)
            request_id = new_id("appr")
            tool_use_id = str(getattr(context, "tool_use_id", "") or "")
            common = dict(
                agent_session_id=agent_session_id,
                external_session_id=ctx.get("session_id"),
                run_id=record.run_id if record else None,
                execution_generation=(
                    record.execution_generation if record else None),
                turn_id=ctx.get("current_turn_id"),
            )
            requested = build_event(
                AgentEventType.APPROVAL_REQUESTED, seq,
                native_type="claude.control.can_use_tool",
                payload=ApprovalRequest(
                    approval_id=request_id,
                    details={
                        "tool": tool_name,
                        "tool_use_id": tool_use_id,
                        "input": input,
                    },
                ).to_payload(),
                **common)
            await ctx["event_queue"].put(("event", requested))
            future: asyncio.Future = asyncio.get_running_loop().create_future()
            ctx["approvals"][request_id] = future
            try:
                verdict = await asyncio.wait_for(
                    future, timeout=self._approval_timeout_s)
            except asyncio.TimeoutError:
                verdict = {"allow": False, "message": "approval timeout",
                           "_timeout": True}
            ctx["approvals"].pop(request_id, None)
            resolved = build_event(
                AgentEventType.APPROVAL_RESOLVED, seq,
                native_type="claude.control.can_use_tool.resolved",
                payload={
                    "approval_id": request_id,
                    "tool": tool_name,
                    "decision": "allow" if verdict.get("allow") else "deny",
                    "auto_denied": bool(verdict.get("_timeout")),
                },
                **common)
            await ctx["event_queue"].put(("event", resolved))
            if verdict.get("allow"):
                kwargs: dict[str, Any] = {"behavior": "allow"}
                if verdict.get("updated_input") is not None:
                    kwargs["updated_input"] = verdict["updated_input"]
                return sdk.PermissionResultAllow(**kwargs)
            return sdk.PermissionResultDeny(
                behavior="deny",
                message=str(verdict.get("message") or "denied by operator"))

        return can_use_tool

    async def respond_approval(
        self,
        session: AgentSessionRef,
        request_id: str,
        allow: bool,
        *,
        message: str = "",
        updated_input: Optional[dict[str, Any]] = None,
    ) -> CommandReceipt:
        """答复一次 can_use_tool 审批（Operator / 调度层入口）。"""
        ctx = self._runs.get(session.agent_session_id) or {}
        future = (ctx.get("approvals") or {}).get(request_id)
        if future is None or future.done():
            return self.unsupported_receipt(
                "respond_approval", "approval_pending", session=session,
                detail={"request_id": request_id,
                        "detail": "no pending approval with this id"})
        future.set_result({
            "allow": bool(allow), "message": message,
            "updated_input": updated_input})
        return CommandReceipt(
            command_id=new_id("cmd"), state=ReceiptState.COMPLETED,
            aggregate=AggregateRef(type="agent_session",
                                   id=session.agent_session_id))

    # -- turn 流 -----------------------------------------------------------------

    def send(
        self, session: AgentSessionRef, input: AgentInput
    ) -> AsyncIterator[AgentEvent]:
        if input.kind == "approval_response":
            return self._approval_response_stream(session, input)
        return self._turn_stream(session, input)

    async def _approval_response_stream(
        self, session: AgentSessionRef, input: AgentInput
    ) -> AsyncIterator[AgentEvent]:
        approval = ApprovalDecision.from_payload(input.payload)
        receipt = await self.respond_approval(
            session,
            approval.approval_id,
            approval.allowed,
            message=approval.note,
            updated_input=input.payload.get("updated_input"))
        if receipt.state is ReceiptState.FAILED:
            yield self.emit(build_event(
                AgentEventType.RUNTIME_ERROR,
                self.sequencer_for(session.agent_session_id),
                agent_session_id=session.agent_session_id,
                external_session_id=session.external_session_id,
                payload={"code": receipt.error.code if receipt.error else "",
                         "operation": "approval_response"},
            ))

    def resume(self, session: AgentSessionRef) -> AsyncIterator[AgentEvent]:
        """SESSION_RESUMED + continue turn；SDK resume 在 reconnect 时生效。"""
        ctx = self._runs.get(session.agent_session_id)
        if ctx is None:
            return self._unsupported_stream(session, "resume", "resume")
        return self._turn_stream(
            session,
            AgentInput(kind="message",
                       text="Continue from where you left off."),
            resumed=True)

    async def _reconnect(self, sid: str, *, resume: Optional[str]) -> Any:
        """按保存的 options 重建客户端（resume 需要新建 query 上下文）。"""
        sdk = _import_sdk()
        ctx = self._runs[sid]
        old = ctx.get("client")
        if old is not None:
            try:
                await old.disconnect()
            except Exception:  # noqa: BLE001
                pass
        kwargs = dict(ctx["options_kwargs"])
        if resume:
            kwargs["resume"] = resume
        client = sdk.ClaudeSDKClient(options=sdk.ClaudeAgentOptions(**kwargs))
        await client.connect()
        ctx["client"] = client
        return client

    async def _turn_stream(
        self,
        session: AgentSessionRef,
        input: AgentInput,
        *,
        resumed: bool = False,
    ) -> AsyncIterator[AgentEvent]:
        _import_sdk()  # 提前失败：SDK 缺失时给带修复提示的 RuntimeError
        sid = session.agent_session_id
        seq = self.sequencer_for(sid)
        ctx = self._runs.get(sid)
        if ctx is None:
            yield self.emit(build_event(
                AgentEventType.RUNTIME_ERROR, seq,
                agent_session_id=sid,
                external_session_id=session.external_session_id,
                payload={"code": "external_agent.session.unknown",
                         "detail": "session was not started by this adapter"},
            ))
            return
        record = self._tracker.get(sid)
        common = dict(
            agent_session_id=sid,
            external_session_id=ctx.get("session_id"),
            run_id=record.run_id if record else None,
            execution_generation=(
                record.execution_generation if record else None),
        )

        if resumed:
            # resume 语义由 reconnect 的 options.resume 承载。
            await self._reconnect(sid, resume=ctx.get("session_id"))
            yield self.emit(build_event(
                AgentEventType.SESSION_RESUMED, seq,
                native_type="claude.sdk.resume",
                payload={"transport": "sdk"},
                **common))
        elif ctx["turns"] == 0:
            yield self.emit(build_event(
                AgentEventType.SESSION_STARTED, seq,
                native_type="claude.sdk.connect",
                payload={
                    "transport": "sdk",
                    "adapter_id": self.id,
                    "instance_id": self.identity.instance_id,
                    "mcp_injected": ctx["mcp_injected"],
                    "mcp_kind": ctx["mcp_kind"],
                },
                **common))

        turn_id = new_id("turn")
        ctx["current_turn_id"] = turn_id
        ctx["assistant_text_parts"] = []
        yield self.emit(build_event(
            AgentEventType.TURN_STARTED, seq, turn_id=turn_id,
            native_type="claude.sdk.query",
            payload={"kind": input.kind},
            **common))

        # 审批事件（can_use_tool 回调）与 SDK 消息共用 per-session 队列，
        # 保证单一序号序列、不丢事件。
        queue: "asyncio.Queue[tuple[str, Any]]" = ctx["event_queue"]

        async def pump() -> None:
            try:
                async for message in ctx["client"].receive_messages():
                    await queue.put(("msg", message))
                    # receive_messages() 是持续流，不因 ResultMessage 结束；
                    # 一个 turn 以 ResultMessage 为界，泵到此即停。
                    if type(message).__name__ == "ResultMessage":
                        break
                await queue.put(("done", None))
            except Exception as exc:  # noqa: BLE001
                await queue.put(("error", exc))

        try:
            prompt = await claude_query_prompt(input.text, input.payload)
            await ctx["client"].query(prompt)
        except Exception as exc:  # noqa: BLE001
            yield self.emit(build_event(
                AgentEventType.TURN_FAILED, seq, turn_id=turn_id,
                native_type="claude.sdk.query.error",
                payload={"error": str(exc)[:300]},
                **common))
            ctx["current_turn_id"] = None
            return
        # 先排空上一 turn 残留的泵哨兵（ResultMessage 之后的 ("done") 等），
        # 再消费本 turn 的消息；terminal 消息（ResultMessage）只标记，
        # 直到泵结束（done/error）才退出循环，保证队列不留残留。
        while not queue.empty():
            queue.get_nowait()
        pump_task = asyncio.ensure_future(pump())

        done = False
        while not done:
            kind, item = await queue.get()
            if kind == "event":
                # can_use_tool 回调产生的审批事件（已带序号，直接投影）。
                yield self.emit(item)
                continue
            if kind == "done":
                break
            if kind == "error":
                yield self.emit(build_event(
                    AgentEventType.TURN_FAILED, seq, turn_id=turn_id,
                    native_type="claude.sdk.stream.error",
                    payload={"error": str(item)[:300]},
                    **common))
                break
            events, _terminal = self._map_message(
                item, ctx, seq, common, turn_id)
            for event in events:
                yield self.emit(event)
        await asyncio.wait([pump_task], timeout=5)
        ctx["turns"] += 1
        ctx["current_turn_id"] = None

    # -- SDK 消息 → 统一 AgentEvent 映射 -------------------------------------------

    def _map_message(
        self,
        message: Any,
        ctx: dict[str, Any],
        seq: Any,
        common: dict[str, Any],
        turn_id: str,
    ) -> tuple[list[AgentEvent], bool]:
        """按类名鸭子识别 SDK Message；返回（事件列表, 是否 turn 终止）。"""
        name = type(message).__name__

        def ev(event_type: AgentEventType, payload: dict[str, Any],
               *, native: Optional[str] = None) -> AgentEvent:
            merged = dict(common)
            if ctx.get("session_id"):
                merged["external_session_id"] = ctx["session_id"]
            return build_event(
                event_type, seq, turn_id=turn_id,
                native_type=native or f"claude.{name}", payload=payload,
                **merged)

        if name == "SystemMessage":
            subtype = str(getattr(message, "subtype", "") or "")
            data = getattr(message, "data", None) or {}
            if subtype == "init":
                session_id = str(data.get("session_id") or "")
                ctx["init_native"] = data
                ctx["capability_revision"] = int(
                    ctx.get("capability_revision") or 0
                ) + 1
                if session_id and ctx.get("session_id") != session_id:
                    ctx["session_id"] = session_id
                    self._tracker.activate(
                        common["agent_session_id"],
                        external_session_id=session_id,
                        resume_handle=session_id)
                # init 携带注入 MCP server 的真实连接状态（§CLAUDE-8/9）。
                failed = [
                    s.get("name") for s in (data.get("mcp_servers") or [])
                    if isinstance(s, dict)
                    and str(s.get("status") or "") not in ("connected", "ready")
                ]
                for err in (data.get("mcp_server_errors") or []):
                    failed.append(str(err))
                events: list[AgentEvent] = [ev(
                    AgentEventType.RUNTIME_CAPABILITIES_UPDATED,
                    {
                        "revision": ctx["capability_revision"],
                        "commands": list(data.get("slash_commands") or []),
                        "adapter_id": self.id,
                    },
                    native="claude.system.init",
                )]
                if failed:
                    events.append(ev(AgentEventType.RUNTIME_WARNING, {
                        "warning": "mcp server not connected",
                        "servers": [str(f) for f in failed],
                        "native": data,
                    }, native="claude.system.init"))
                return events, False
            return [], False
        if name == "AssistantMessage":
            events: list[AgentEvent] = []
            for block in (getattr(message, "content", None) or []):
                mapped = self._map_block(ev, block)
                events.extend(mapped)
                if type(block).__name__ == "TextBlock":
                    text = str(getattr(block, "text", "") or "")
                    if text:
                        ctx["assistant_text_parts"].append(text)
            return events, False
        if name == "UserMessage":
            # SDK 的 UserMessage 是输入回显/工具结果载体，不属于助手正文。
            return [], False
        if name == "StreamEvent":
            # --include-partial-messages 的部分增量。
            event = getattr(message, "event", None) or {}
            delta = (event.get("delta") or {}) if isinstance(event, dict) else {}
            text = str(delta.get("text") or "")
            if text:
                ctx["assistant_text_parts"].append(text)
            return ([ev(AgentEventType.MESSAGE_DELTA, {"text": text},
                        native="claude.stream_event")] if text else []), False
        if name == "RateLimitEvent":
            return [ev(AgentEventType.RUNTIME_WARNING, {
                "warning": "rate limit",
                "native": _jsonable(
                    getattr(message, "rate_limit_info", None) or {}),
            })], False
        if name == "HookEventMessage":
            # hook 进度/回执属已知信息类消息：不进核心状态机，不产生事件。
            return [], False
        if name == "ResultMessage":
            usage = getattr(message, "usage", None) or {}
            events = [ev(AgentEventType.USAGE_UPDATED, {
                "usage": {
                    "input_tokens": usage.get("input_tokens"),
                    "output_tokens": usage.get("output_tokens"),
                    "cache_read_input_tokens":
                        usage.get("cache_read_input_tokens"),
                    "reported_cost": getattr(message, "total_cost_usd", None),
                    "cache_creation_input_tokens": usage.get("cache_creation_input_tokens"),
                    "num_turns": getattr(message, "num_turns", None),
                    "duration_ms": getattr(message, "duration_ms", None),
                },
            })]
            is_error = bool(getattr(message, "is_error", False))
            terminal_reason = str(
                getattr(message, "terminal_reason", "") or "")
            # CLI 2.1.x 成功时 terminal_reason="completed"；只有空/completed
            # 之外的取值（aborted_streaming 等）才是异常终止。
            terminal_failure = bool(
                terminal_reason and terminal_reason != "completed")
            result_text = str(getattr(message, "result", "") or "")
            if not result_text:
                result_text = "".join(ctx.get("assistant_text_parts") or [])
            if result_text and not is_error and not terminal_failure:
                events.append(ev(AgentEventType.MESSAGE_COMPLETED,
                                 {"text": result_text}))
            if is_error or terminal_failure:
                events.append(ev(AgentEventType.TURN_FAILED, {
                    "reason": ("interrupted"
                               if "abort" in terminal_reason
                               else (terminal_reason or "error")),
                    "terminal_reason": terminal_reason,
                    "subtype": str(getattr(message, "subtype", "") or ""),
                    "result": result_text[:1000],
                }))
            elif not result_text.strip():
                events.append(ev(AgentEventType.TURN_FAILED, {
                    "reason": "empty_assistant",
                    "error": {
                        "code": "claude.empty_assistant",
                        "message": "Claude turn ended without assistant text",
                    },
                }))
            else:
                events.append(ev(AgentEventType.TURN_COMPLETED, {
                    "subtype": str(getattr(message, "subtype", "") or ""),
                    "duration_ms": getattr(message, "duration_ms", None),
                }))
            return events, True
        # 未知 SDK 消息：RUNTIME_WARNING + native 保留，不改核心状态机。
        return [ev(AgentEventType.RUNTIME_WARNING, {
            "warning": "unmapped sdk message",
            "native": {"repr": repr(message)[:500]},
        })], False

    @staticmethod
    def _map_block(ev: Any, block: Any) -> list[AgentEvent]:
        """content block 映射（TextBlock/ThinkingBlock/ToolUseBlock/ToolResultBlock）。"""
        name = type(block).__name__
        if name == "TextBlock":
            text = str(getattr(block, "text", "") or "")
            return [ev(AgentEventType.MESSAGE_DELTA, {"text": text})] if text else []
        if name == "ThinkingBlock":
            # Claude ThinkingBlock is raw reasoning. The SDK does not label it
            # as a public reasoning summary, so it stays inside the adapter.
            return []
        if name == "ToolUseBlock":
            return [ev(AgentEventType.TOOL_STARTED, {
                "tool": str(getattr(block, "name", "") or ""),
                "call_id": str(getattr(block, "id", "") or ""),
                "input": getattr(block, "input", None) or {},
            })]
        if name == "ToolResultBlock":
            content = getattr(block, "content", None)
            if isinstance(content, list):
                output = "".join(
                    str(getattr(part, "text", "") or "") for part in content)
            else:
                output = str(content or "")
            return [ev(AgentEventType.TOOL_COMPLETED, {
                "call_id": str(getattr(block, "tool_use_id", "") or ""),
                "output": output[:2000],
                "is_error": bool(getattr(block, "is_error", False)),
            })]
        return []

    # -- 控制面 ---------------------------------------------------------------------

    async def steer(
        self, session: AgentSessionRef, input: AgentInput
    ) -> CommandReceipt:
        """steer：交互客户端在 turn 进行中追加消息（SDK 排队）。"""
        ctx = self._runs.get(session.agent_session_id)
        if ctx is None:
            return self.unsupported_receipt("steer", "steer", session=session)
        try:
            prompt = await claude_query_prompt(input.text, input.payload)
            await ctx["client"].query(prompt)
        except Exception as exc:  # noqa: BLE001
            return self.unsupported_receipt(
                "steer", "steer_rejected", session=session,
                detail={"error": str(exc)[:200]})
        return CommandReceipt(
            command_id=new_id("cmd"), state=ReceiptState.COMPLETED,
            aggregate=AggregateRef(type="agent_session",
                                   id=session.agent_session_id))

    async def interrupt(self, session: AgentSessionRef) -> CommandReceipt:
        """interrupt：SDK 控制请求；turn 以 terminal_reason=aborted_* 结束。"""
        ctx = self._runs.get(session.agent_session_id)
        if ctx is None:
            return self.unsupported_receipt(
                "interrupt", "interrupt", session=session)
        try:
            await ctx["client"].interrupt()
        except Exception as exc:  # noqa: BLE001
            return self.unsupported_receipt(
                "interrupt", "interrupt_rejected", session=session,
                detail={"error": str(exc)[:200]})
        return CommandReceipt(
            command_id=new_id("cmd"), state=ReceiptState.COMPLETED,
            aggregate=AggregateRef(type="agent_session",
                                   id=session.agent_session_id))

    async def set_model(self, session: AgentSessionRef, model: str) -> CommandReceipt:
        """运行中切换模型（ClaudeSDKClient.set_model）。"""
        ctx = self._runs.get(session.agent_session_id)
        if ctx is None:
            return self.unsupported_receipt("set_model", "model", session=session)
        await ctx["client"].set_model(model)
        return CommandReceipt(
            command_id=new_id("cmd"), state=ReceiptState.COMPLETED,
            aggregate=AggregateRef(type="agent_session",
                                   id=session.agent_session_id))

    async def set_permission_mode(
        self, session: AgentSessionRef, mode: str
    ) -> CommandReceipt:
        """运行中切换 permission mode。"""
        if mode not in PERMISSION_MODES:
            return self.unsupported_receipt(
                "set_permission_mode", "permission_mode", session=session,
                detail={"mode": mode, "allowed": list(PERMISSION_MODES)})
        ctx = self._runs.get(session.agent_session_id)
        if ctx is None:
            return self.unsupported_receipt(
                "set_permission_mode", "permission_mode", session=session)
        await ctx["client"].set_permission_mode(mode)
        return CommandReceipt(
            command_id=new_id("cmd"), state=ReceiptState.COMPLETED,
            aggregate=AggregateRef(type="agent_session",
                                   id=session.agent_session_id))

    async def runtime_operation(self, session: AgentSessionRef, name: str, arguments: str = "") -> dict[str, Any]:
        ctx = self._runs.get(session.agent_session_id)
        if not ctx or ctx.get("current_turn_id"):
            raise RuntimeError("请等待当前回复结束后执行命令")
        if arguments:
            raise ValueError(f"/{name} 不接受参数")
        if name == "context":
            result = await ctx["client"].get_context_usage()
        elif name == "mcp":
            result = await ctx["client"].get_mcp_status()
        else:
            raise RuntimeError("未知 Claude SDK 操作")
        return {"status": "completed", "result": _jsonable(result)}

    async def runtime_capability_snapshot(
        self, session: Optional[AgentSessionRef] = None
    ) -> RuntimeCapabilitySnapshot:
        base = await super().runtime_capability_snapshot(session)
        if session is None:
            return base
        ctx = self._runs.get(session.agent_session_id)
        if ctx is None:
            return base.model_copy(update={
                "stale": True,
                "diagnostics": ["Claude SDK Session 当前不在本进程"],
            })
        init = ctx.get("init_native") or {}
        items = list(base.items)
        for raw in init.get("slash_commands") or []:
            if isinstance(raw, dict):
                name = str(raw.get("name") or "").strip().lstrip("/")
                description = str(raw.get("description") or "")
                argument_hint = str(
                    raw.get("argument_hint")
                    or raw.get("argumentHint")
                    or ""
                )
            else:
                name = str(raw or "").strip().lstrip("/")
                description = ""
                argument_hint = ""
            if not name:
                continue
            items.append(dynamic_command_item(
                adapter_id=self.id,
                engine="claude",
                name=name,
                description=description,
                argument_hint=argument_hint,
                channel="provider_native",
                invocation={
                    "command": name,
                    "wire_text": f"/{name}",
                    "protocol": "claude.sdk.query",
                },
            ))

        from .command_providers import operation_item
        for name, method, description in (("context", "get_context_usage", "查看 Claude 原生上下文用量"),
                                           ("mcp", "get_mcp_status", "查看 Claude MCP 连接状态")):
            if callable(getattr(ctx["client"], method, None)):
                items = [item for item in items if item.name != name]
                items.append(operation_item(self.id, "claude", name, method, description))

        reported_mcp = {
            str(item.get("name") or "")
            for item in init.get("mcp_servers") or []
            if isinstance(item, dict) and item.get("name")
        }
        items = [
            item for item in items
            if not (
                item.kind == "mcp_status" and item.name in reported_mcp
            )
        ]
        for status in init.get("mcp_servers") or []:
            if not isinstance(status, dict):
                continue
            name = str(status.get("name") or "").strip()
            if not name:
                continue
            state = str(status.get("status") or "unknown")
            items.append(RuntimeCapabilityItem(
                id=f"runtime:{self.id}:mcp:{name}",
                kind="mcp_status",
                name=name,
                description=f"Claude SDK 报告状态：{state}",
                source="Claude Agent SDK",
                scope="session",
                engine="claude",
                channel="provider_native",
                resolution="runtime",
                origin="dynamic",
                delivery="guaranteed",
                verification="verified",
                status=(
                    "runtime_reported"
                    if state in {"connected", "ready"} else "failed"
                ),
                invocation={"native_status": state},
            ))
        return base.model_copy(update={
            "revision": int(ctx.get("capability_revision") or 0),
            "items": items,
            "diagnostics": (
                ["等待 Claude SDK system/init 公布命令目录"]
                if not init else []
            ),
        })

    async def _teardown(self, session: AgentSessionRef) -> str:
        ctx = self._runs.get(session.agent_session_id)
        if not ctx:
            return EXIT_CLOSED
        client = ctx.get("client")
        if client is not None:
            await client.disconnect()
        self._runs.pop(session.agent_session_id, None)
        # 留有 session id 的 Session 可经 resume 恢复。
        return EXIT_RESUMABLE if ctx.get("session_id") else EXIT_CLOSED


__all__ = [
    "ClaudeSDKAdapter",
    "DEFAULT_CLAUDE_CLI",
    "HEADLESS_FALLBACK",
    "PERMISSION_MODES",
]
