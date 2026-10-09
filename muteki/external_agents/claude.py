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
import uuid as uuid_module

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
    ApprovalResponseInput,
    MessageInput,
    ProbeRequest,
    SessionStart,
    SteerInput,
    UserInputResponseInput,
)
from muteki.platform.contracts.protocols import RuntimeOperationAdapter
from muteki.platform.contracts.receipts import AggregateRef, CommandReceipt, ReceiptState

from .base import BaseExternalAgentAdapter
from .claude_transport import owned_claude_transport
from .approvals import (
    ApprovalChoice,
    ApprovalDecision,
    ApprovalScope,
    ApprovalTarget,
    SessionApprovalGrants,
)
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
from muteki.platform.contracts.agent_events import (
    AgentNodePayload,
    AgentUpdatedPayload,
    ApprovalRequestedPayload,
    ApprovalResolvedPayload,
    FailureCategory,
    MessageCompletedPayload,
    MessageDeltaPayload,
    PlanPayload,
    RateLimitState,
    ReasoningPayload,
    RuntimeCapabilitiesPayload,
    RuntimeErrorPayload,
    RuntimeWarningPayload,
    SessionPayload,
    ToolPayload,
    TurnCompletedPayload,
    TurnFailedPayload,
    TurnStartedPayload,
    UsagePayload,
    UserInputRequestedPayload,
    UserInputResolvedPayload,
    dump_payload,
)
from .user_input_schema import normalize_question

#: 默认 CLI 路径与 MCP server 名。
DEFAULT_CLAUDE_CLI = "claude"
MCP_SERVER_NAME = acp_config.DEFAULT_SERVER_NAME

#: 已核验的 permission mode 全集（§CLAUDE-5，types.py / sdk.d.ts 一致）。
PERMISSION_MODES = (
    "default", "acceptEdits", "plan", "bypassPermissions", "dontAsk", "auto")


def _base_permission_mode(request: SessionStart, fallback: str) -> str:
    """Permission mode of the session outside plan turns."""
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


def _permission_mode_for(request: SessionStart, fallback: str) -> str:
    if request.interaction_mode == "plan":
        return "plan"
    return _base_permission_mode(request, fallback)


#: Tool names the SDK routes through ``can_use_tool`` for interaction that
#: only the operator can answer, regardless of the permission mode.
ASK_USER_QUESTION_TOOL = "AskUserQuestion"
EXIT_PLAN_MODE_TOOL = "ExitPlanMode"


def _ask_user_questions(
    tool_input: dict[str, Any],
) -> tuple[list[dict[str, Any]], dict[str, str]]:
    """AskUserQuestion input -> normalized questions plus id -> question text.

    The SDK keys the answers it hands back to the model by question text, so
    the text is kept to translate Muteki's ``question_id`` answers.
    """
    raw = tool_input.get("questions")
    questions: list[dict[str, Any]] = []
    texts: dict[str, str] = {}
    if not isinstance(raw, list):
        return questions, texts
    for index, item in enumerate(raw):
        if not isinstance(item, dict):
            continue
        text = str(item.get("question") or "").strip()
        if not text:
            continue
        options = [
            {"value": str(opt.get("label") or ""),
             "label": str(opt.get("label") or ""),
             "description": str(opt.get("description") or "")}
            if isinstance(opt, dict) else {"value": str(opt), "label": str(opt)}
            for opt in (item.get("options") or [])
        ]
        qid = f"q{index}"
        normalized = normalize_question({
            "question_id": qid,
            "header": str(item.get("header") or ""),
            "question": text,
            "options": [opt for opt in options if opt["value"]],
            "multi": bool(item.get("multiSelect")),
            # The CLI always offers an "Other" free-text answer.
            "allow_free_text": True,
        }, index=index)
        if normalized is not None:
            questions.append(normalized)
            texts[qid] = text
    return questions, texts


def _ask_user_answers(
    texts: dict[str, str], answers: dict[str, Any],
) -> dict[str, str]:
    """Muteki ``{question_id: {values, text}}`` -> SDK ``{question: "a, b"}``."""
    out: dict[str, str] = {}
    for qid, entry in answers.items():
        text = texts.get(str(qid))
        if text is None:
            continue
        if isinstance(entry, dict):
            parts = [str(v) for v in (entry.get("values") or []) if str(v)]
            free = entry.get("text")
            if free is not None and str(free) and str(free) not in parts:
                parts.append(str(free))
        elif isinstance(entry, list):
            parts = [str(v) for v in entry if str(v)]
        else:
            parts = [str(entry)] if str(entry) else []
        out[text] = ", ".join(parts)
    return out


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


def claude_extra_args(launch_args: "tuple[str, ...] | list[str]") -> dict[str, Optional[str]]:
    """Convert configured CLI arguments to Agent SDK ``extra_args``.

    Raises ValueError for arguments the SDK cannot forward (positionals or
    short flags), so the settings write fails instead of silently dropping them.
    """
    out: dict[str, Optional[str]] = {}
    items = list(launch_args)
    index = 0
    while index < len(items):
        token = items[index]
        if not token.startswith("--") or token == "--":
            raise ValueError(
                f"Claude 启动参数只支持 --flag [值] 形式：{token!r}")
        name, sep, value = token[2:].partition("=")
        if not name:
            raise ValueError(f"Claude 启动参数缺少名称：{token!r}")
        if sep:
            out[name] = value
        elif index + 1 < len(items) and not items[index + 1].startswith("-"):
            out[name] = items[index + 1]
            index += 1
        else:
            out[name] = None
        index += 1
    return out


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


def _sdk_plan_mode_supported(sdk: Any) -> bool:
    """True when the SDK ``PermissionMode`` literal includes ``plan``.

    This is the type's value set, not a reading of a later error string. A
    control-request failure after a supported mode is sent stays a provider
    error.
    """
    values = getattr(getattr(sdk, "PermissionMode", None), "__args__", None)
    return isinstance(values, tuple) and "plan" in values


def _cli_version(binary: str, *, timeout: float = 15.0) -> str:
    try:
        result = subprocess.run(
            [binary, "--version"], capture_output=True, text=True,
            encoding="utf-8", errors="replace", timeout=timeout, env=subprocess_environment(),
        )
    except (FileNotFoundError, subprocess.TimeoutExpired, OSError):
        return ""
    lines = (result.stdout or result.stderr or "").strip().splitlines()
    return lines[0].strip() if result.returncode == 0 and lines else ""


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
    return repr(value)


_TASK_MESSAGES = frozenset({
    "TaskStartedMessage", "TaskProgressMessage",
    "TaskNotificationMessage", "TaskUpdatedMessage",
})

# Claude Code renamed the delegation tool from ``Task`` to ``Agent``; both
# spawn a subagent whose messages carry ``parent_tool_use_id`` = this call.
_DELEGATION_TOOLS = frozenset({"task", "agent"})


def _is_delegation_tool(name: str) -> bool:
    return name.strip().lower() in _DELEGATION_TOOLS


#: Claude Code ``TaskType`` values that are subagents. task_started also
#: reports shells, workflows, monitors and teammates; those are not nodes.
_AGENT_TASK_TYPES = frozenset({"local_agent", "remote_agent"})


def _is_agent_task_type(task_type: str) -> bool:
    return task_type in _AGENT_TASK_TYPES


def _tool_result_text(content: Any) -> str:
    if isinstance(content, list):
        parts = []
        for part in content:
            if isinstance(part, dict):
                parts.append(str(part.get("text") or ""))
            else:
                parts.append(str(getattr(part, "text", "") or ""))
        return "".join(parts)
    return "" if content is None else str(content)


def _agent_patch(
    ev: Any, ctx: dict[str, Any], agent_id: str, update: dict[str, Any],
) -> list[AgentEvent]:
    nodes = ctx.setdefault("agent_nodes", {})
    previous = nodes.get(agent_id)
    if previous is None:
        # Narration or progress for an agent we never saw spawn (e.g. resumed
        # mid-flight) still deserves a node instead of leaking into the parent.
        previous = {"agent_id": agent_id, "title": agent_id,
                    "call_id": agent_id, "status": "running"}
    node = {**previous, **{k: v for k, v in update.items() if v is not None}}
    for key, value in update.items():
        if value is None and key in ("result", "error"):
            node[key] = None
    if node == previous and agent_id in nodes:
        return []
    nodes[agent_id] = node
    return [ev(AgentEventType.AGENT_UPDATED,
               dump_payload(AgentUpdatedPayload(
                   agents=[AgentNodePayload(**node)], patch=True)))]


def _agent_from_tool_use(
    ev: Any, ctx: dict[str, Any], block: Any, *,
    parent: Optional[str], model: str,
) -> list[AgentEvent]:
    call_id = str(getattr(block, "id", "") or "")
    if not call_id:
        return []
    raw = getattr(block, "input", None)
    args = raw if isinstance(raw, dict) else {}
    role = str(args.get("subagent_type") or "").strip()
    title = str(args.get("description") or "").strip() or role or "Agent"
    return _agent_patch(ev, ctx, call_id, {
        "agent_id": call_id,
        "parent_id": parent,
        "title": title,
        "role": role or None,
        "model": str(args.get("model") or "").strip() or None,
        "call_id": call_id,
        "status": "running",
        "request": str(args.get("prompt") or "").strip() or None,
        "result": None,
        "error": None,
    })


def _agent_from_tool_result(
    ev: Any, ctx: dict[str, Any], call_id: str, block: Any,
    meta: Optional[dict[str, Any]],
) -> list[AgentEvent]:
    meta = meta or {}
    native_status = str(meta.get("status") or "").lower()
    if "launch" in native_status or native_status in {"running", "pending"}:
        # Background agents return immediately; completion arrives later as a
        # task notification.
        update: dict[str, Any] = {"status": "running"}
    else:
        failed = bool(getattr(block, "is_error", False)) or native_status in {
            "failed", "error"}
        text = _tool_result_text(meta.get("content")) or _tool_result_text(
            getattr(block, "content", None))
        update = {"status": "failed" if failed else "completed"}
        if text:
            update["error" if failed else "result"] = text
    if meta.get("agentId"):
        update["session_ref"] = str(meta["agentId"])
    for key, native in (("duration_ms", "totalDurationMs"),
                        ("total_tokens", "totalTokens"),
                        ("tool_uses", "totalToolUseCount")):
        if meta.get(native) is not None:
            update[key] = meta.get(native)
    return _agent_patch(ev, ctx, call_id, update)


class ClaudeRewindError(RuntimeError):
    """Native rewind or fork cannot be served; ``code`` is stable."""

    def __init__(self, code: str, message: str) -> None:
        self.code = code
        super().__init__(f"{code}: {message}")


class ClaudeSDKAdapter(BaseExternalAgentAdapter, RuntimeOperationAdapter):
    """Claude Agent SDK（ClaudeSDKClient 交互模式）的结构化 Adapter。

    构造参数（除基类外）：

    - ``cli_path``：claude CLI 路径（SDK 底层仍拉起 CLI 进程）；
    - ``model`` / ``permission_mode``：默认值，可被 SessionStart 覆盖；
    - ``gateway``：Native Tool（in-process MCP）注入时需要的
      AgentCapabilityGateway；MCP/HTTP 注入不需要；
    - ``approval_timeout_s``：can_use_tool 等待审批答复的超时，超时按
      deny 应答（不悬挂 Runtime）。
    """

    context_compaction_events = True

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
        sdk_mod = _import_sdk() if sdk_ok else None
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
        caps.streaming = report("streaming", _sdk_option_is_supported(
            sdk_mod, "include_partial_messages") if sdk_mod is not None else False)
        caps.resume = report("resume", True)
        caps.session_persistence = report("session_persistence", True)
        caps.steer = report("steer", True)        # streaming input / client.query
        caps.interrupt = report("interrupt", True)  # ClaudeSDKClient.interrupt
        caps.approval = report("approval", True)    # can_use_tool
        caps.user_input = report("user_input", True)
        # fork_thread calls the module-level ``fork_session``.
        caps.fork = report("fork", callable(getattr(sdk_mod, "fork_session", None)))
        # permission_mode "plan", switched per turn via set_permission_mode.
        plan_supported = (
            _sdk_plan_mode_supported(sdk_mod) if sdk_mod is not None else False)
        caps.plan_mode = report("plan_mode", plan_supported)
        caps.plan = report("plan", plan_supported)
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
            "can_use_tool 始终安装：AskUserQuestion 与 ExitPlanMode 只经该回调"
            "到达宿主，安装不随普通工具是否需要审批而变化；T3 nightly 同样"
            "始终安装该回调。普通工具是否需要审批仍由权限模式和规则判定。")
        if sdk_mod is not None and not plan_supported:
            degradations.append(
                "SDK PermissionMode 不含 plan：interaction_mode 为 plan 的回合"
                "以 UNSUPPORTED / plan_mode_unsupported 失败，不向 CLI 发送该模式")
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
        base_mode = _base_permission_mode(request, self._permission_mode)
        # An SDK whose PermissionMode omits plan must not be launched in it.
        # The plan turn then fails with plan_mode_unsupported.
        if (
            permission_mode == "plan"
            and sdk is not None
            and not _sdk_plan_mode_supported(sdk)
        ):
            permission_mode = base_mode
        kwargs: dict[str, Any] = {
            "cwd": options.cwd or self._default_cwd or ".",
            "cli_path": self._cli_path,
            "permission_mode": permission_mode,
            # Always installed: AskUserQuestion and ExitPlanMode only reach the
            # operator through this callback, whatever the permission mode.
            # Ordinary tools are asked about only when the mode needs approval.
            "can_use_tool": self._make_can_use_tool(request.agent_session_id),
        }
        extra_args: dict[str, Optional[str]] = {}
        if base_mode == "bypassPermissions":
            # A session that starts in plan mode (or enters it per turn) can
            # only return to bypassPermissions when the CLI was launched with
            # the allow flag.
            if sdk is not None and _sdk_option_is_supported(
                    sdk, "allow_dangerously_skip_permissions"):
                kwargs["allow_dangerously_skip_permissions"] = True
            else:
                extra_args["allow-dangerously-skip-permissions"] = None
        if sdk is None or _sdk_option_is_supported(sdk, "include_partial_messages"):
            kwargs["include_partial_messages"] = True
        model = request.model or self._model
        if model:
            kwargs["model"] = model
        settings: dict[str, Any] = {}
        if request.effort in {"off", "on"}:
            settings["alwaysThinkingEnabled"] = request.effort == "on"
        if request.effort != "off":
            # Opus 4.7+ omits thinking text unless summaries are requested.
            if sdk is None or _sdk_option_is_supported(sdk, "thinking"):
                kwargs["thinking"] = {"type": "adaptive", "display": "summarized"}
            settings["showThinkingSummaries"] = True
        if settings:
            kwargs["settings"] = json.dumps(settings)
        if request.effort and request.effort not in {"off", "on", "default"}:
            # This is a model-scoped native option, validated by Conversation.
            # Older SDKs can forward the same CLI flag through extra_args.
            if sdk is not None and _sdk_option_is_supported(sdk, "effort"):
                kwargs["effort"] = request.effort
            else:
                extra_args["effort"] = request.effort
        if self.launch_args:
            # The SDK builds the CLI argv itself and only forwards `--flag [value]` pairs.
            extra_args = {**claude_extra_args(self.launch_args), **extra_args}
        if extra_args:
            kwargs["extra_args"] = extra_args
        if mcp_servers:
            kwargs["mcp_servers"] = mcp_servers
        if options.allowed_tools:
            kwargs["allowed_tools"] = list(options.allowed_tools)
        if options.plugins:
            kwargs["plugins"] = list(options.plugins)
        # Agent SDK 的 ``skills`` 是结构化启用入口：它会把 Skill 工具加入
        # allowed tools，并按 setting sources 让 Runtime 自行发现/按需加载。
        # 不再由 Muteki 读取 SKILL.md 全文塞进用户消息。
        kwargs["skills"] = options.skills
        if options.hooks:
            # PreToolUse 等 hook 透传（§CLAUDE-3：全程工具门控通道）。
            kwargs["hooks"] = dict(options.hooks)
        if options.max_turns:
            kwargs["max_turns"] = options.max_turns
        if options.env:
            kwargs["env"] = {**self._default_env, **options.env}
        elif self._default_env:
            kwargs["env"] = dict(self._default_env)
        if resume:
            kwargs["resume"] = resume
        if options.fork_session:
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
        sdk_options = sdk.ClaudeAgentOptions(**kwargs)
        client = sdk.ClaudeSDKClient(
            options=sdk_options, transport=owned_claude_transport(sdk_options, session_id=sid))
        try:
            await client.connect()
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
        except BaseException:
            await client.disconnect()
            raise
        # Fork and truncation targets apply to this connect only; a later
        # reconnect resumes the session id the CLI reports, plainly.
        reconnect_kwargs = {
            key: value for key, value in kwargs.items()
            if key not in {"fork_session", "resume_session_at", "resume"}}
        self._runs[sid] = {
            "client": client,
            "options_kwargs": reconnect_kwargs,
            "mcp_injected": bool(mcp_servers),
            "mcp_kind": (plan.injection_kind.value if plan else ""),
            "turns": 0,
            "approvals": {},       # request_id -> asyncio.Future
            "user_inputs": {},     # request_id -> asyncio.Future
            "approval_requests": {},   # request_id -> approval.requested payload
            "approval_grants": SessionApprovalGrants(
                AccessMode(request.access_mode)
                if request.access_mode else AccessMode.SUPERVISED),
            "event_queue": asyncio.Queue(),
            "session_id": request.resume_handle,
            "base_permission_mode": _base_permission_mode(
                request, self._permission_mode),
            "applied_permission_mode": kwargs["permission_mode"],
            "plan_mode_supported": _sdk_plan_mode_supported(sdk),
            # Per completed turn: (turn_id, uuid of the last root assistant
            # message). Native rewind truncates the transcript at these uuids,
            # so a resumed session (older turns unknown) cannot rewind natively.
            "turn_log": [],
            "turn_last_uuid": None,
            "rewind_known": not request.resume_handle,
            "streamed_kinds": {},
            "stream_message_id": "",
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
            tool_use_id = str(getattr(context, "tool_use_id", "") or "")

            def common() -> dict[str, Any]:
                return dict(
                    agent_session_id=agent_session_id,
                    external_session_id=ctx.get("session_id"),
                    run_id=record.run_id if record else None,
                    execution_generation=(
                        record.execution_generation if record else None),
                    turn_id=ctx.get("current_turn_id"),
                )

            if tool_name == ASK_USER_QUESTION_TOOL:
                return await self._ask_user_question(
                    sdk, ctx, seq, common(), input, tool_use_id)
            if tool_name == EXIT_PLAN_MODE_TOOL:
                return await self._exit_plan_mode(
                    sdk, ctx, seq, common(), input, tool_use_id)
            if ctx.get("applied_permission_mode") == "bypassPermissions":
                return sdk.PermissionResultAllow(behavior="allow")

            request_id = new_id("appr")
            approval_kind = (
                "file_change"
                if tool_name in {"Edit", "Write", "NotebookEdit"}
                else "command_execution"
                if tool_name in {"Bash", "Shell"} else "tool")
            cwd = str(ctx["options_kwargs"].get("cwd") or "")
            target_fields: dict[str, Any] = {}
            if approval_kind == "command_execution":
                command = str(input.get("command") or "").strip()
                if command:
                    target_fields["command"] = command
            elif approval_kind == "file_change":
                path = str(input.get("file_path")
                           or input.get("notebook_path") or "").strip()
                if path:
                    target_fields["paths"] = [path]
            payload = ApprovalRequestedPayload(
                approval_id=request_id,
                approval_kind=approval_kind,
                title=tool_name,
                tool_name=tool_name,
                tool_call_id=tool_use_id or None,
                cwd=cwd or None,
                input=input,
                **target_fields,
            )
            request_dump = dump_payload(payload)
            grants: SessionApprovalGrants = ctx["approval_grants"]
            # Native plan mode ignores allow rules for workspace writes. A
            # cached Muteki session grant must not bypass that fresh decision.
            if (ctx.get("applied_permission_mode") != "plan"
                    and grants.covers(request_dump, cwd=cwd)):
                # A session grant covers the same tool kind and exact target
                # only, and is answered one-shot so Claude keeps no broader rule.
                return sdk.PermissionResultAllow(behavior="allow")
            can_remember = ApprovalTarget.from_payload(
                request_dump, cwd=cwd) is not None
            payload = payload.model_copy(update={
                "scopes": ["once", "session"] if can_remember else ["once"]})
            requested = build_event(
                AgentEventType.APPROVAL_REQUESTED, seq,
                native_type="claude.control.can_use_tool",
                payload=payload,
                **common())
            ctx["approval_requests"][request_id] = dump_payload(payload)
            await ctx["event_queue"].put(("event", requested))
            future: asyncio.Future = asyncio.get_running_loop().create_future()
            ctx["approvals"][request_id] = future
            try:
                verdict = await asyncio.wait_for(
                    future, timeout=self._approval_timeout_s)
            except asyncio.TimeoutError:
                verdict = {"allow": False, "message": "approval timeout",
                           "_timeout": True}
            finally:
                ctx["approvals"].pop(request_id, None)
                ctx["approval_requests"].pop(request_id, None)
            resolved = build_event(
                AgentEventType.APPROVAL_RESOLVED, seq,
                native_type="claude.control.can_use_tool.resolved",
                payload=ApprovalResolvedPayload(
                    approval_id=request_id,
                    decision="allow" if verdict.get("allow") else "deny",
                    automatic=bool(verdict.get("_timeout")),
                    native={
                        "tool": tool_name,
                        "auto_denied": bool(verdict.get("_timeout")),
                    },
                ),
                **common())
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

    async def _exit_plan_mode(
        self, sdk: Any, ctx: dict[str, Any], seq: Any,
        common: dict[str, Any], tool_input: dict[str, Any], tool_use_id: str,
    ) -> Any:
        """ExitPlanMode -> plan approval. Allow leaves plan mode; deny stays.

        ``plan_markdown`` is the tool's plan text with nothing removed.
        An empty plan is a normal tool approval, not ``plan_exit``.
        """
        raw_plan = tool_input.get("plan")
        plan_text = raw_plan if isinstance(raw_plan, str) else ""
        has_plan = bool(plan_text.strip())
        title = ""
        if plan_text.strip():
            title = next(
                (line.lstrip("#").strip() for line in plan_text.splitlines()
                 if line.lstrip("#").strip()), "")
            await ctx["event_queue"].put(("event", build_event(
                AgentEventType.PLAN_UPDATED, seq,
                native_type="claude.control.exit_plan_mode",
                payload=PlanPayload(
                    title=title or None, explanation=plan_text,
                    phase="proposed",
                    native={"tool": EXIT_PLAN_MODE_TOOL,
                            "tool_call_id": tool_use_id,
                            "markdown": plan_text}),
                **common)))
        request_id = new_id("appr")
        payload = ApprovalRequestedPayload(
            approval_id=request_id,
            approval_kind="plan_exit" if has_plan else "tool",
            title=title or EXIT_PLAN_MODE_TOOL,
            tool_name=EXIT_PLAN_MODE_TOOL,
            tool_call_id=tool_use_id or None,
            scopes=["once"],
            plan_markdown=plan_text if has_plan else None,
            input=tool_input,
        )
        ctx["approval_requests"][request_id] = dump_payload(payload)
        await ctx["event_queue"].put(("event", build_event(
            AgentEventType.APPROVAL_REQUESTED, seq,
            native_type="claude.control.exit_plan_mode",
            payload=payload,
            **common)))
        future: asyncio.Future = asyncio.get_running_loop().create_future()
        ctx["approvals"][request_id] = future
        try:
            verdict = await asyncio.wait_for(
                future, timeout=self._approval_timeout_s)
        except asyncio.TimeoutError:
            verdict = {"allow": False, "message": "approval timeout",
                       "_timeout": True}
        finally:
            ctx["approvals"].pop(request_id, None)
            ctx["approval_requests"].pop(request_id, None)
        allowed = bool(verdict.get("allow"))
        await ctx["event_queue"].put(("event", build_event(
            AgentEventType.APPROVAL_RESOLVED, seq,
            native_type="claude.control.exit_plan_mode.resolved",
            payload=ApprovalResolvedPayload(
                approval_id=request_id,
                decision="allow" if allowed else "deny",
                automatic=bool(verdict.get("_timeout")),
                native={
                    "tool": EXIT_PLAN_MODE_TOOL,
                    "auto_denied": bool(verdict.get("_timeout")),
                },
            ),
            **common)))
        if allowed:
            return sdk.PermissionResultAllow(behavior="allow")
        message = str(verdict.get("message") or "").strip() or (
            "The user declined this plan. Stay in plan mode and revise it.")
        return sdk.PermissionResultDeny(behavior="deny", message=message)

    async def _ask_user_question(
        self, sdk: Any, ctx: dict[str, Any], seq: Any,
        common: dict[str, Any], tool_input: dict[str, Any], tool_use_id: str,
    ) -> Any:
        """AskUserQuestion -> USER_INPUT_REQUESTED; the chosen answers go back
        to the tool as ``updated_input.answers`` keyed by question text."""
        questions, texts = _ask_user_questions(tool_input)
        request_id = new_id("uinp")
        await ctx["event_queue"].put(("event", build_event(
            AgentEventType.USER_INPUT_REQUESTED, seq,
            native_type="claude.control.ask_user_question",
            payload=UserInputRequestedPayload(
                request_id=request_id,
                user_input_kind="claude.ask_user_question",
                questions=questions,
                response_actions=["submit", "cancel"],
                tool_call_id=tool_use_id or None,
                native={"input": _jsonable(tool_input)}),
            **common)))
        future: asyncio.Future = asyncio.get_running_loop().create_future()
        ctx["user_inputs"][request_id] = future
        try:
            reply = await asyncio.wait_for(
                future, timeout=self._approval_timeout_s)
        except asyncio.TimeoutError:
            reply = {"decision": "cancel", "_timeout": True}
        finally:
            ctx["user_inputs"].pop(request_id, None)
        raw_answers = dict(reply.get("answers") or {})
        if reply.get("text") and not raw_answers and texts:
            # Free text answers the first question.
            raw_answers = {next(iter(texts)): {
                "values": [], "text": str(reply["text"])}}
        answers = _ask_user_answers(texts, raw_answers)
        answered = reply.get("decision") == "submit" and bool(answers)
        outcome = ("answered" if answered
                   else "timeout" if reply.get("_timeout") else "cancelled")
        await ctx["event_queue"].put(("event", build_event(
            AgentEventType.USER_INPUT_RESOLVED, seq,
            native_type="claude.control.ask_user_question.resolved",
            payload=UserInputResolvedPayload(
                request_id=request_id, outcome=outcome,
                answers=raw_answers if answered else None),
            **common)))
        if not answered:
            return sdk.PermissionResultDeny(
                behavior="deny", message="User declined to answer questions.")
        return sdk.PermissionResultAllow(
            behavior="allow",
            updated_input={"questions": tool_input.get("questions"),
                           "answers": answers})

    async def respond_user_input(
        self, session: AgentSessionRef, request_id: str,
        answers: dict[str, Any], *, decision: str = "submit", text: str = "",
    ) -> CommandReceipt:
        """答复一次 AskUserQuestion（Operator / 调度层入口）。"""
        ctx = self._runs.get(session.agent_session_id) or {}
        future = (ctx.get("user_inputs") or {}).get(request_id)
        if future is None or future.done():
            return self.unsupported_receipt(
                "respond_user_input", "user_input_pending", session=session,
                detail={"request_id": request_id,
                        "detail": "no pending user input with this id"})
        future.set_result({"decision": decision, "answers": dict(answers),
                           "text": text})
        return CommandReceipt(
            command_id=new_id("cmd"), state=ReceiptState.COMPLETED,
            aggregate=AggregateRef(type="agent_session",
                                   id=session.agent_session_id))

    async def respond_approval(
        self,
        session: AgentSessionRef,
        request_id: str,
        allow: bool,
        *,
        message: str = "",
        updated_input: Optional[dict[str, Any]] = None,
        scope: str = "once",
    ) -> CommandReceipt:
        """答复一次 can_use_tool 审批（Operator / 调度层入口）。"""
        ctx = self._runs.get(session.agent_session_id) or {}
        future = (ctx.get("approvals") or {}).get(request_id)
        if future is None or future.done():
            return self.unsupported_receipt(
                "respond_approval", "approval_pending", session=session,
                detail={"request_id": request_id,
                        "detail": "no pending approval with this id"})
        request = ctx["approval_requests"].get(request_id)
        if request is not None:
            ctx["approval_grants"].remember(
                request,
                ApprovalDecision(
                    approval_id=request_id,
                    choice=ApprovalChoice.ALLOW if allow else ApprovalChoice.DENY,
                    scope=ApprovalScope(scope)),
                cwd=str(ctx["options_kwargs"].get("cwd") or ""))
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
        if isinstance(input, ApprovalResponseInput):
            return self._approval_response_stream(session, input)
        if isinstance(input, UserInputResponseInput):
            return self._user_input_response_stream(session, input)
        if isinstance(input, MessageInput):
            return self._turn_stream(session, input)
        return self.unsupported_input_stream(session, input)

    async def _user_input_response_stream(
        self, session: AgentSessionRef, input: UserInputResponseInput
    ) -> AsyncIterator[AgentEvent]:
        receipt = await self.respond_user_input(
            session, input.payload.request_id, dict(input.payload.answers),
            decision=input.payload.decision, text=input.text)
        if receipt.state is ReceiptState.FAILED:
            yield self.emit(build_event(
                AgentEventType.RUNTIME_ERROR,
                self.sequencer_for(session.agent_session_id),
                agent_session_id=session.agent_session_id,
                external_session_id=session.external_session_id,
                payload=RuntimeErrorPayload(
                    error=self.failure(
                        FailureCategory.UNKNOWN, "user_input.stale",
                        message="no pending user input with this id",
                        native_code=(
                            receipt.error.code if receipt.error else "")),
                    native={"operation": "user_input_response",
                            "request_id": input.payload.request_id}),
            ))

    async def _approval_response_stream(
        self, session: AgentSessionRef, input: ApprovalResponseInput
    ) -> AsyncIterator[AgentEvent]:
        approval = ApprovalDecision.from_payload(input.payload.model_dump())
        receipt = await self.respond_approval(
            session,
            approval.approval_id,
            approval.allowed,
            message=approval.note,
            updated_input=input.payload.updated_input,
            scope=approval.scope.value)
        if receipt.state is ReceiptState.FAILED:
            yield self.emit(build_event(
                AgentEventType.RUNTIME_ERROR,
                self.sequencer_for(session.agent_session_id),
                agent_session_id=session.agent_session_id,
                external_session_id=session.external_session_id,
                payload=RuntimeErrorPayload(
                    error=self.failure(
                        FailureCategory.UNKNOWN, "approval.stale",
                        message="no pending approval with this id",
                        native_code=(
                            receipt.error.code if receipt.error else "")),
                    native={"operation": "approval_response",
                            "request_id": approval.approval_id}),
            ))

    def resume(self, session: AgentSessionRef) -> AsyncIterator[AgentEvent]:
        """SESSION_RESUMED + continue turn；SDK resume 在 reconnect 时生效。"""
        ctx = self._runs.get(session.agent_session_id)
        if ctx is None:
            return self._unsupported_stream(session, "resume", "resume")
        return self._turn_stream(
            session,
            MessageInput(text="Continue from where you left off."),
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
        sdk_options = sdk.ClaudeAgentOptions(**kwargs)
        client = sdk.ClaudeSDKClient(
            options=sdk_options, transport=owned_claude_transport(sdk_options, session_id=sid))
        await client.connect()
        ctx["client"] = client
        ctx["applied_permission_mode"] = kwargs["permission_mode"]
        return client

    async def _turn_stream(
        self,
        session: AgentSessionRef,
        input: MessageInput,
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
                payload=RuntimeErrorPayload(error=self.failure(
                    FailureCategory.UNKNOWN, "session.unknown",
                    message="session was not started by this adapter")),
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

        if resumed or ctx.pop("needs_reconnect", False):
            # resume 语义由 reconnect 的 options.resume 承载。
            await self._reconnect(sid, resume=ctx.get("session_id"))
            yield self.emit(build_event(
                AgentEventType.SESSION_RESUMED, seq,
                native_type="claude.sdk.resume",
                payload=SessionPayload(transport="sdk"),
                **common))
        elif ctx["turns"] == 0:
            yield self.emit(build_event(
                AgentEventType.SESSION_STARTED, seq,
                native_type="claude.sdk.connect",
                payload=SessionPayload(
                    transport="sdk",
                    adapter_id=self.id,
                    instance_id=self.identity.instance_id,
                    native={
                        "mcp_injected": ctx["mcp_injected"],
                        "mcp_kind": ctx["mcp_kind"],
                    }),
                **common))

        turn_id = new_id("turn")
        ctx["current_turn_id"] = turn_id
        ctx["assistant_text_parts"] = []
        ctx["turn_last_uuid"] = None
        ctx["streamed_kinds"] = {}
        ctx["stream_message_id"] = ""
        yield self.emit(build_event(
            AgentEventType.TURN_STARTED, seq, turn_id=turn_id,
            native_type="claude.sdk.query",
            payload=TurnStartedPayload(kind=input.kind),
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

        # The turn's interaction mode is authoritative: Claude can also switch
        # itself into plan mode (EnterPlanMode), so the mode is re-asserted
        # whenever it differs from what this turn needs.
        desired_mode = (
            "plan" if input.payload.interaction_mode == "plan"
            else ctx["base_permission_mode"])
        if desired_mode == "plan" and not ctx.get("plan_mode_supported"):
            yield self.emit(build_event(
                AgentEventType.TURN_FAILED, seq, turn_id=turn_id,
                native_type="claude.sdk.plan_mode_unsupported",
                payload=TurnFailedPayload(error=self.failure(
                    FailureCategory.UNSUPPORTED, "plan_mode_unsupported",
                    message="Claude Agent SDK PermissionMode does not include plan")),
                **common))
            ctx["current_turn_id"] = None
            return
        if desired_mode != ctx["applied_permission_mode"]:
            try:
                await ctx["client"].set_permission_mode(desired_mode)
                ctx["applied_permission_mode"] = desired_mode
            except Exception as exc:  # noqa: BLE001
                yield self.emit(build_event(
                    AgentEventType.TURN_FAILED, seq, turn_id=turn_id,
                    native_type="claude.sdk.set_permission_mode.error",
                    payload=TurnFailedPayload(error=self.exception_failure(
                        exc, FailureCategory.PROVIDER,
                        "permission_mode_switch",
                        message=("Claude could not switch permission mode "
                                 f"to {desired_mode!r}"))),
                    **common))
                ctx["current_turn_id"] = None
                return

        try:
            prompt = await claude_query_prompt(input.text, input.payload.attachments)
            await ctx["client"].query(prompt)
        except Exception as exc:  # noqa: BLE001
            yield self.emit(build_event(
                AgentEventType.TURN_FAILED, seq, turn_id=turn_id,
                native_type="claude.sdk.query.error",
                payload=TurnFailedPayload(error=self.exception_failure(
                    exc, FailureCategory.TRANSPORT, "query_error",
                    message=f"Claude SDK query failed: {exc}")),
                **common))
            ctx["current_turn_id"] = None
            return
        # 先排空上一 turn 残留的泵哨兵（ResultMessage 之后的 ("done") 等），
        # 再消费本 turn 的消息；terminal 消息（ResultMessage）只标记，
        # 直到泵结束（done/error）才退出循环，保证队列不留残留。
        while not queue.empty():
            queue.get_nowait()
        pump_task = asyncio.ensure_future(pump())

        terminal_seen = False
        try:
            while True:
                kind, item = await queue.get()
                if kind == "event":
                    # can_use_tool 回调产生的审批事件（已带序号，直接投影）。
                    yield self.emit(item)
                    continue
                if kind == "done":
                    if not terminal_seen:
                        # The CLI closed its stream before a ResultMessage.
                        yield self.emit(build_event(
                            AgentEventType.TURN_FAILED, seq, turn_id=turn_id,
                            native_type="claude.sdk.stream.ended",
                            payload=TurnFailedPayload(error=self.failure(
                                FailureCategory.TRANSPORT,
                                "stream_ended_without_terminal",
                                message="Claude SDK stream ended without a "
                                        "result message")),
                            **common))
                    break
                if kind == "error":
                    yield self.emit(build_event(
                        AgentEventType.TURN_FAILED, seq, turn_id=turn_id,
                        native_type="claude.sdk.stream.error",
                        payload=TurnFailedPayload(error=self.exception_failure(
                            item, FailureCategory.PROVIDER, "stream_error",
                            message=f"Claude SDK stream failed: {item}")),
                        **common))
                    break
                events, terminal = self._map_message(
                    item, ctx, seq, common, turn_id)
                terminal_seen = terminal_seen or terminal
                for event in events:
                    yield self.emit(event)
            await pump_task
            ctx["turn_log"].append((turn_id, ctx.get("turn_last_uuid")))
            ctx["turns"] += 1
        finally:
            try:
                if not pump_task.done() or not terminal_seen:
                    pump_task.cancel()
                    await asyncio.gather(pump_task, return_exceptions=True)
                    # An abandoned response cannot be consumed by the next query.
                    ctx["needs_reconnect"] = True
                    await ctx["client"].disconnect()
            finally:
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
            reported_mode = data.get("permissionMode")
            if subtype in {"status", "init"} and isinstance(reported_mode, str) and reported_mode:
                # Claude can change its own mode (EnterPlanMode); trust what it reports.
                ctx["applied_permission_mode"] = reported_mode
            if subtype == "compact_boundary":
                self._context_compacted(common["agent_session_id"])
                return [], False
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
                    dump_payload(RuntimeCapabilitiesPayload(
                        revision=ctx["capability_revision"],
                        reason="claude.system.init",
                        native={
                            "adapter_id": self.id,
                            "commands": list(data.get("slash_commands") or []),
                        },
                    )),
                    native="claude.system.init",
                )]
                if failed:
                    events.append(ev(AgentEventType.RUNTIME_WARNING,
                                     dump_payload(RuntimeWarningPayload(
                                         kind="degraded",
                                         message="mcp server not connected",
                                         code="claude.mcp.unconnected",
                                         native={
                                             "servers": [str(f) for f in failed],
                                             "data": _jsonable(data),
                                         },
                                     )),
                                     native="claude.system.init"))
                return events, False
            return [], False
        if name in _TASK_MESSAGES:
            return self._map_task_message(ev, ctx, message, name), False
        if name == "AssistantMessage":
            events: list[AgentEvent] = []
            parent = str(getattr(message, "parent_tool_use_id", "") or "")
            if not parent and getattr(message, "uuid", None):
                ctx["turn_last_uuid"] = str(message.uuid)
            streamed = ctx.get("streamed_kinds", {}).get(
                str(getattr(message, "message_id", "") or ""), set())
            for block in (getattr(message, "content", None) or []):
                block_name = type(block).__name__
                if not parent and (
                    (block_name == "TextBlock" and "text" in streamed)
                    or (block_name == "ThinkingBlock" and "thinking" in streamed)
                ):
                    # Already delivered token by token through StreamEvents.
                    continue
                if parent:
                    # Subagent narration belongs to its agent card, never to
                    # the parent's assistant answer.
                    if block_name == "TextBlock":
                        text = str(getattr(block, "text", "") or "").strip()
                        if text:
                            events.extend(_agent_patch(ev, ctx, parent, {
                                "activity": text,
                            }))
                        continue
                    if block_name == "ThinkingBlock":
                        continue
                mapped = self._map_block(ev, block, agent_id=parent or None)
                events.extend(mapped)
                if block_name == "ToolUseBlock" and _is_delegation_tool(
                        str(getattr(block, "name", "") or "")):
                    events.extend(_agent_from_tool_use(
                        ev, ctx, block, parent=parent or None,
                        model=str(getattr(message, "model", "") or "")))
                if block_name == "TextBlock" and not parent:
                    text = str(getattr(block, "text", "") or "")
                    if text:
                        ctx["assistant_text_parts"].append(text)
            return events, False
        if name == "UserMessage":
            # UserMessage 是输入回显/工具结果载体，不属于助手正文；只取出
            # ToolResultBlock 作为工具完成事件。
            content = getattr(message, "content", None)
            if not isinstance(content, list):
                return [], False
            parent = str(getattr(message, "parent_tool_use_id", "") or "")
            result_meta = getattr(message, "tool_use_result", None)
            events = []
            for block in content:
                if type(block).__name__ != "ToolResultBlock":
                    continue
                events.extend(self._map_block(
                    ev, block, agent_id=parent or None))
                call_id = str(getattr(block, "tool_use_id", "") or "")
                if call_id in (ctx.get("agent_nodes") or {}):
                    events.extend(_agent_from_tool_result(
                        ev, ctx, call_id, block,
                        result_meta if isinstance(result_meta, dict) else None))
            return events, False
        if name == "StreamEvent":
            # include_partial_messages: raw Anthropic stream events.
            if getattr(message, "parent_tool_use_id", None):
                return [], False
            event = getattr(message, "event", None)
            if not isinstance(event, dict):
                return [], False
            kind = event.get("type")
            if kind == "message_start":
                ctx["stream_message_id"] = str(
                    (event.get("message") or {}).get("id") or "")
                return [], False
            if kind != "content_block_delta":
                return [], False
            delta = event.get("delta") or {}
            message_id = str(ctx.get("stream_message_id") or "")
            delta_type = delta.get("type")
            if delta_type == "text_delta":
                text = str(delta.get("text") or "")
                if not text:
                    return [], False
                ctx["streamed_kinds"].setdefault(message_id, set()).add("text")
                ctx["assistant_text_parts"].append(text)
                return [ev(AgentEventType.MESSAGE_DELTA,
                           dump_payload(MessageDeltaPayload(text=text)),
                           native="claude.stream_event")], False
            if delta_type == "thinking_delta":
                text = str(delta.get("thinking") or "")
                if not text:
                    return [], False
                ctx["streamed_kinds"].setdefault(
                    message_id, set()).add("thinking")
                return [ev(AgentEventType.REASONING_SUMMARY,
                           dump_payload(ReasoningPayload(
                               text=text, channel="thinking", partial=True,
                               item_id=f"{message_id}:{event.get('index')}"
                               if message_id else None)),
                           native="claude.stream_event")], False
            return [], False
        if name == "RateLimitEvent":
            info = getattr(message, "rate_limit_info", None)
            status = str(getattr(info, "status", "") or "")
            resets_at = getattr(info, "resets_at", None)
            return [ev(AgentEventType.RUNTIME_WARNING,
                       dump_payload(RuntimeWarningPayload(
                           kind="rate_limit",
                           message=f"Claude rate limit status: {status or 'unknown'}",
                           rate_limit=RateLimitState(
                               limited=status == "rejected",
                               warning=status == "allowed_warning",
                               resets_at=float(resets_at)
                               if isinstance(resets_at, (int, float)) else None,
                               kind=str(getattr(info, "rate_limit_type", "") or "")
                               or None,
                               utilization=getattr(info, "utilization", None),
                           ),
                           native=_jsonable(info or {}),
                       )))], False
        if name == "HookEventMessage":
            # hook 进度/回执属已知信息类消息：不进核心状态机，不产生事件。
            return [], False
        if name == "ResultMessage":
            from muteki.core.usage import sum_token_buckets
            usage = getattr(message, "usage", None) or {}
            events = [ev(AgentEventType.USAGE_UPDATED,
                         dump_payload(UsagePayload(
                             scope="turn",
                             input_tokens=sum_token_buckets(
                                 usage.get("input_tokens"), usage.get("cache_read_input_tokens"),
                                 usage.get("cache_creation_input_tokens")),
                             output_tokens=usage.get("output_tokens"),
                             cached_input_tokens=
                                 usage.get("cache_read_input_tokens"),
                             cache_write_tokens=
                                 usage.get("cache_creation_input_tokens"),
                             cost_usd=getattr(message, "total_cost_usd", None),
                             step_count=getattr(message, "num_turns", None),
                             llm_duration_ms=
                                 getattr(message, "duration_ms", None),
                         )))]
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
                                 dump_payload(MessageCompletedPayload(
                                     text=result_text))))
            if is_error or terminal_failure:
                interrupted = "abort" in terminal_reason
                events.append(ev(AgentEventType.TURN_FAILED,
                                 dump_payload(TurnFailedPayload(
                                     error=self.failure(
                                         FailureCategory.CANCELLED
                                         if interrupted
                                         else FailureCategory.PROVIDER,
                                         ("interrupted" if interrupted
                                          else (terminal_reason or "error")),
                                         message=(result_text
                                                  or terminal_reason
                                                  or "Claude turn failed")),
                                     native={
                                         "terminal_reason": terminal_reason,
                                         "subtype": str(getattr(
                                             message, "subtype", "") or ""),
                                         "result": result_text,
                                     }))))
            else:
                if not result_text.strip():
                    events.append(ev(AgentEventType.RUNTIME_WARNING,
                                     dump_payload(RuntimeWarningPayload(
                                         kind="degraded", code="external_agent.empty_assistant",
                                         message="Claude turn ended without assistant text"))))
                events.append(ev(AgentEventType.TURN_COMPLETED,
                                 dump_payload(TurnCompletedPayload(
                                     stop_reason=terminal_reason or None,
                                     duration_ms=getattr(
                                         message, "duration_ms", None),
                                     native={
                                         "subtype": str(getattr(
                                             message, "subtype", "") or ""),
                                     }))))
            return events, True
        # 未知 SDK 消息：RUNTIME_WARNING + native 保留，不改核心状态机。
        return [ev(AgentEventType.RUNTIME_WARNING,
                   dump_payload(RuntimeWarningPayload(
                       kind="protocol",
                       message="unmapped sdk message",
                       code=f"claude.unmapped.{name}",
                       native={"repr": repr(message)},
                   )))], False

    @staticmethod
    def _map_block(
        ev: Any, block: Any, *, agent_id: Optional[str] = None,
    ) -> list[AgentEvent]:
        """content block 映射（TextBlock/ThinkingBlock/ToolUseBlock/ToolResultBlock）。

        ``agent_id`` 是 SDK ``parent_tool_use_id``：子智能体内部的工具调用
        归属到该子智能体，而不是父会话的直接工具。
        """
        name = type(block).__name__
        if name == "TextBlock":
            text = str(getattr(block, "text", "") or "")
            return ([ev(AgentEventType.MESSAGE_DELTA,
                        dump_payload(MessageDeltaPayload(text=text)))]
                    if text else [])
        if name == "ThinkingBlock":
            # Claude ThinkingBlock is raw chain-of-thought: channel=thinking,
            # never merged into the assistant message text.
            thinking = str(getattr(block, "thinking", "") or "")
            return ([ev(AgentEventType.REASONING_SUMMARY,
                        dump_payload(ReasoningPayload(
                            text=thinking, channel="thinking")))]
                    if thinking else [])
        if name == "ToolUseBlock":
            tool = str(getattr(block, "name", "") or "")
            return [ev(AgentEventType.TOOL_STARTED,
                       dump_payload(ToolPayload(
                           tool_call_id=str(getattr(block, "id", "") or ""),
                           name=tool,
                           input=getattr(block, "input", None) or {},
                           status="running",
                           kind="agent" if _is_delegation_tool(tool) else None,
                           agent_id=agent_id,
                       )))]
        if name == "ToolResultBlock":
            is_error = bool(getattr(block, "is_error", False))
            output = _tool_result_text(getattr(block, "content", None))
            return [ev(AgentEventType.TOOL_COMPLETED,
                       dump_payload(ToolPayload(
                           tool_call_id=str(getattr(block, "tool_use_id", "") or ""),
                           output=output,
                           status="failed" if is_error else "completed",
                           error=(output or "tool failed") if is_error else None,
                           agent_id=agent_id,
                       )))]
        return []

    def _map_task_message(
        self, ev: Any, ctx: dict[str, Any], message: Any, name: str,
    ) -> list[AgentEvent]:
        """Task* 系统消息 → 子智能体生命周期（只处理 Agent 类任务）。"""
        task_id = str(getattr(message, "task_id", "") or "")
        tool_use_id = str(getattr(message, "tool_use_id", "") or "")
        tasks = ctx.setdefault("agent_tasks", {})
        nodes = ctx.setdefault("agent_nodes", {})
        agent_id = tasks.get(task_id) or (
            tool_use_id if tool_use_id in nodes else "")
        if name == "TaskStartedMessage":
            task_type = str(getattr(message, "task_type", "") or "")
            if not agent_id and not _is_agent_task_type(task_type):
                return []
            agent_id = agent_id or tool_use_id or task_id
            tasks[task_id] = agent_id
            update: dict[str, Any] = {"status": "running"}
            description = str(getattr(message, "description", "") or "").strip()
            if description and not (nodes.get(agent_id) or {}).get("title"):
                update["title"] = description
            if tool_use_id:
                update["call_id"] = tool_use_id
            update["session_ref"] = task_id
            return _agent_patch(ev, ctx, agent_id, update)
        if not agent_id:
            return []
        if name == "TaskProgressMessage":
            usage = getattr(message, "usage", None) or {}
            update = {
                "status": "running",
                "tool_uses": usage.get("tool_uses"),
                "total_tokens": usage.get("total_tokens"),
                "duration_ms": usage.get("duration_ms"),
            }
            last_tool = str(getattr(message, "last_tool_name", "") or "")
            if last_tool:
                update["activity"] = last_tool
            return _agent_patch(ev, ctx, agent_id, update)
        if name == "TaskNotificationMessage":
            status = {"completed": "completed", "failed": "failed",
                      "stopped": "cancelled"}.get(
                str(getattr(message, "status", "") or ""), "completed")
            usage = getattr(message, "usage", None) or {}
            summary = str(getattr(message, "summary", "") or "")
            update = {"status": status}
            field = "error" if status == "failed" else "result"
            # The tool result (when present) is the full answer; the summary
            # is only a fallback for background agents.
            if summary and not (nodes.get(agent_id) or {}).get(field):
                update[field] = summary
            for key in ("tool_uses", "total_tokens", "duration_ms"):
                if usage.get(key) is not None:
                    update[key] = usage.get(key)
            return _agent_patch(ev, ctx, agent_id, update)
        if name == "TaskUpdatedMessage":
            raw = str(getattr(message, "status", "") or "")
            status = {"pending": "pending", "running": "running",
                      "paused": "pending", "completed": "completed",
                      "failed": "failed", "killed": "cancelled"}.get(raw)
            if not status:
                return []
            patch = getattr(message, "patch", None) or {}
            update = {"status": status}
            if status == "failed" and patch.get("error"):
                update["error"] = str(patch.get("error"))
            return _agent_patch(ev, ctx, agent_id, update)
        return []

    # -- 控制面 ---------------------------------------------------------------------

    async def steer(
        self, session: AgentSessionRef, input: AgentInput
    ) -> CommandReceipt:
        """steer：交互客户端在 turn 进行中追加消息（SDK 排队）。"""
        if not isinstance(input, SteerInput):
            return self.unsupported_receipt(
                "steer", "steer", session=session, detail={"input_kind": input.kind})
        ctx = self._runs.get(session.agent_session_id)
        if ctx is None:
            return self.unsupported_receipt("steer", "steer", session=session)
        try:
            prompt = await claude_query_prompt(input.text, input.payload.attachments)
            await ctx["client"].query(prompt)
        except Exception as exc:  # noqa: BLE001
            return self.unsupported_receipt(
                "steer", "steer_rejected", session=session,
                detail={"error": str(exc)})
        return CommandReceipt(
            command_id=new_id("cmd"), state=ReceiptState.COMPLETED,
            aggregate=AggregateRef(type="agent_session",
                                   id=session.agent_session_id))

    async def interrupt(self, session: AgentSessionRef) -> CommandReceipt:
        self._mark_turn_interrupted(session.agent_session_id)
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
                detail={"error": str(exc)})
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

    @staticmethod
    def _log_index(ctx: dict[str, Any], native_turn_id: str) -> int:
        index = next((i for i, (turn_id, _uuid) in enumerate(ctx["turn_log"])
                      if turn_id == native_turn_id), None)
        if index is None:
            raise ClaudeRewindError(
                "claude.rewind.turn_unknown",
                f"turn {native_turn_id!r} is not in this session's transcript log")
        return index

    @staticmethod
    def _uuid_before(ctx: dict[str, Any], index: int) -> Optional[str]:
        """Last root assistant message uuid of the turns before ``index``."""
        for _turn_id, uuid in reversed(ctx["turn_log"][:index]):
            if uuid:
                return uuid
        return None

    def supports_native_rewind(self, session: AgentSessionRef) -> bool:
        ctx = self._runs.get(session.agent_session_id)
        return bool(
            ctx and ctx.get("rewind_known") and ctx.get("session_id")
            and ctx["turn_log"] and not ctx.get("current_turn_id")
            and _sdk_option_is_supported(_import_sdk(), "resume_session_at"))

    async def rewind_session(
        self, session: AgentSessionRef, native_turn_id: str
    ) -> dict[str, Any]:
        """Drop ``native_turn_id`` and everything after it.

        Resumes the same session truncated at the previous turn's last
        assistant message (``resume_session_at``); with no earlier message the
        session restarts under a fresh session id.
        """
        ctx = self._runs.get(session.agent_session_id)
        if not ctx or not self.supports_native_rewind(session):
            raise ClaudeRewindError(
                "claude.rewind.unavailable",
                "this Claude session cannot rewind natively")
        index = self._log_index(ctx, native_turn_id)
        resume_at = self._uuid_before(ctx, index)
        sdk = _import_sdk()
        old = ctx["client"]
        kwargs = dict(ctx["options_kwargs"])
        if resume_at:
            kwargs["resume"] = ctx["session_id"]
            kwargs["resume_session_at"] = resume_at
        else:
            kwargs["session_id"] = str(uuid_module.uuid4())
        try:
            await old.disconnect()
        except Exception:  # noqa: BLE001 — the replacement client is what matters
            pass
        sdk_options = sdk.ClaudeAgentOptions(**kwargs)
        client = sdk.ClaudeSDKClient(
            options=sdk_options, transport=owned_claude_transport(sdk_options, session_id=session.agent_session_id))
        await client.connect()
        ctx["client"] = client
        ctx["applied_permission_mode"] = kwargs["permission_mode"]
        del ctx["turn_log"][index:]
        if not resume_at:
            ctx["session_id"] = kwargs["session_id"]
            self._tracker.activate(
                session.agent_session_id,
                external_session_id=kwargs["session_id"],
                resume_handle=kwargs["session_id"])
        return {"strategy": "native",
                "method": "resume_session_at" if resume_at else "new_session",
                "removed_from_turn": native_turn_id}

    async def fork_thread(
        self, session: AgentSessionRef, native_turn_id: str = ""
    ) -> dict[str, Any]:
        """Copy the transcript (up to ``native_turn_id`` when given) into a new
        session; start it with ``SessionStart(resume_handle=session_id)``."""
        ctx = self._runs.get(session.agent_session_id)
        if ctx is None or not ctx.get("session_id"):
            raise ClaudeRewindError(
                "claude.fork.session_unknown",
                "the Claude session has no transcript yet")
        up_to: Optional[str] = None
        if native_turn_id:
            if not ctx.get("rewind_known"):
                raise ClaudeRewindError(
                    "claude.fork.turn_unmapped",
                    "turn uuids are unknown for a resumed Claude session")
            index = self._log_index(ctx, native_turn_id)
            up_to = ctx["turn_log"][index][1]
            if not up_to:
                raise ClaudeRewindError(
                    "claude.fork.turn_unmapped",
                    f"turn {native_turn_id!r} produced no assistant message")
        sdk = _import_sdk()
        result = await asyncio.to_thread(
            sdk.fork_session, ctx["session_id"],
            directory=ctx["options_kwargs"].get("cwd"),
            up_to_message_id=up_to)
        return {"thread_id": str(result.session_id),
                "root_session_id": str(ctx["session_id"])}

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
        for request_id, future in tuple(ctx.get("approvals", {}).items()):
            if not future.done():
                future.set_result({"allow": False, "message": "session closed"})
        for request_id, future in tuple(ctx.get("user_inputs", {}).items()):
            if not future.done():
                future.set_result({"decision": "cancel", "answers": {}})
        client = ctx.get("client")
        if client is not None:
            await client.disconnect()
        self._runs.pop(session.agent_session_id, None)
        # 留有 session id 的 Session 可经 resume 恢复。
        return EXIT_RESUMABLE if ctx.get("session_id") else EXIT_CLOSED


__all__ = [
    "ClaudeRewindError",
    "ClaudeSDKAdapter",
    "DEFAULT_CLAUDE_CLI",
    "HEADLESS_FALLBACK",
    "PERMISSION_MODES",
]
