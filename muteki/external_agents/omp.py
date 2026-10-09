"""OMP（oh-my-pi）Adapter（RUNTIME-04，任务书 7.2/7.6）。

正式接入：``omp --mode rpc`` / ``--mode rpc-ui``（NDJSON stdio RPC）；
``omp acp`` 作为通用客户端兼容传输（复用 RUNTIME-03 ``BaseAcpAdapter``）。
核验结论（docs/research/third_party_verification.md §OMP，仓库
``can1357/oh-my-pi`` @76a294cb，本机实测 17.2.12）：

- stdout 首帧为 ``ready``：``{"type":"ready","protocolVersion":1,
  "supportedProtocolVersions":[1,2],"maxFrameBytes":1048576,...}``；
  v1 每帧单 JSON + ``\\n``、物理帧上限 1 MiB；可发
  ``negotiate_protocol`` 升级 v2（``rpc_chunk`` 双向分块重组）；
- 命令全部支持可选 ``id`` 关联；**响应顺序不保证**（``bash`` 并发派发），
  按 ``id`` 匹配（``rpc.py`` 的 StdioJsonlPeer 天然满足）；解析失败回
  ``command:"parse", success:false`` 且不退出；
- prompt：立即 ack（``data.agentInvoked``，``false`` = 本地完成）；
  **turn 只在 ``agent_end`` 且 ``isTerminal !== false`` 时算完成**
  （字段可选，缺省视为终止）；另有 ``prompt_result {id, agentInvoked}``
  帧；不能拿 ack 当完成信号；
- abort：``{type:"abort"}``；另有 ``abort_and_prompt``（legacy，不发
  ``data.agentInvoked``/``prompt_result``）、``abort_bash``、
  ``abort_retry``；
- 交互请求（``--mode rpc-ui``）：出站 ``extension_ui_request``，method
  含 ``select/confirm/input/editor``（带 timeout，超时自动以默认值
  resolve）与 ``notify/setStatus/...``（fire-and-forget）；入站应答
  ``extension_ui_response {id, value|confirmed|cancelled}``——无人值守
  默认应答 ``cancelled``（先出 USER_INPUT_REQUESTED 事件，不静默吞掉）；
- workspace：无 ``--workspace``，等价物是 ``--cwd <path>`` 与
  ``--add-dir <path>``；
- model/role：``--provider``/``--model`` 与 RPC ``set_model {provider,
  modelId}``（双字段成对）；「role」不是会话参数，是 settings 的
  ``modelRoles`` 映射，CLI 仅 ``--model/--smol/--slow/--plan`` 对应——
  Adapter 接受 ``role`` 选项时映射为这些 flag 并记 degradation；
- 能力注入按 probe：``set_host_tools``/``host_tool_call``/
  ``host_tool_result``（宿主反向向 agent 暴露工具）已确认存在
  （§OMP-10）→ 声明 ``structured_http_rpc``，选 HTTP_JSONRPC 计划：
  工具清单经 ``set_host_tools`` 注入，``host_tool_call`` 由 Adapter
  代理到 Gateway 的 HTTP/JSON-RPC Bridge（``muteki.invoke``，bearer
  现场签发），``host_tool_result`` 回写；其余 Runtime（MCP 配置入口等）
  未核验不声明。
- 子智能体（原生 ``task`` 工具，本机 17.2.12 实测）：RPC 侧先
  ``set_subagent_subscription {level:"events"}`` 订阅，之后 stdout 出
  ``subagent_lifecycle``（started/completed/failed/aborted，带
  ``parentToolCallId`` 与 ``sessionFile``）、``subagent_progress``
  （``progress`` 为完整 AgentProgress：status/description/lastIntent/
  toolCount/tokens/durationMs/resolvedModel）、``subagent_event``
  （``payload.event`` 为子会话 AgentSessionEvent，含子工具调用与
  ``yield`` 结果提交）；``tool_execution_end`` 的
  ``result.details`` 为 TaskToolDetails（``results[]``/``progress[]``/
  ``async.state``）。异步派单时 task 调用立即返回且
  ``details.async.state=="running"``，子智能体终态只由 lifecycle 帧判定。
  ACP 侧无 subagent 帧，子智能体信号在 task ``tool_call`` 的 ``rawInput``
  （``task``/``tasks`` 参数，OMP 内置工具中唯一）与 ``tool_call_update``
  的 ``rawOutput.details``（TaskToolDetails）中。

兼容路径：本机 OMP 的 CLI 一次性路径若启用，由
``muteki.solver.cli_driver`` 的对应 Driver + ``CliDriverAdapter``
（``cli.omp``）承担；本模块不 import solver 层。

注意：OMP 是 Pi fork，``message_update``/``message_end``/
``tool_execution_*`` 事件形态按 Pi 系血缘宽容解析；逐字段差异属
「仍需真实环境确认」项（§OMP），mock 测试锁定本 Adapter 消费的子集。
"""

from __future__ import annotations

import asyncio
import json
import os
from typing import Any, AsyncIterator, Optional

import httpx

from muteki.capability_bindings.http_jsonrpc import METHOD_INVOKE
from muteki.platform.contracts.agent_events import (
    AgentNodePayload,
    AgentUpdatedPayload,
    FailureCategory,
    MessageCompletedPayload,
    MessageDeltaPayload,
    RuntimeCapabilitiesPayload,
    RuntimeErrorPayload,
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
from muteki.platform.contracts.base import new_id
from muteki.platform.contracts.capabilities import (
    CapabilityInjectionPlan,
    InjectionKind,
)
from muteki.platform.contracts.external_agents import (
    AccessMode,
    AgentCapabilities,
    AgentEvent,
    AgentEventType,
    AgentInput,
    AgentSessionRef,
    MessageInput,
    ProbeRequest,
    SessionStart,
    SteerInput,
)
from muteki.platform.contracts.protocols import RuntimeOperationAdapter
from muteki.platform.contracts.receipts import (
    AggregateRef,
    CommandReceipt,
    ReceiptState,
)

from .acp import BaseAcpAdapter
from .base import BaseExternalAgentAdapter
from .capabilities import (
    AccessModeUnsupportedError,
    BOOL_CAPABILITY_FIELDS,
    CapabilityProbeReport,
    SOURCE_PROBE,
    SOURCE_STATIC,
    conservative_capabilities,
    _probe_version,
)
from .events import build_event
from .runtime_capabilities import (
    RuntimeCapabilitySnapshot,
    dynamic_command_item,
)
from .rpc import PeerClosedError, StdioJsonlPeer
from .sessions import classify_exit

#: role 选项 → CLI flag（§OMP-6：role 是 modelRoles 配置，非会话参数）。
_ROLE_FLAGS = {"smol": "--smol", "slow": "--slow", "plan": "--plan"}


def _omp_envelope(msg_id: str, method: str,
                  params: Optional[dict[str, Any]]) -> dict[str, Any]:
    """OMP RPC 命令形态：``{id, type, ...params}``。"""
    return {"id": msg_id, "type": method, **(params or {})}


def _omp_message_text(message: dict[str, Any]) -> str:
    """拼接 message.content 中的文本块。"""
    content = message.get("content")
    if isinstance(content, str):
        return content
    out: list[str] = []
    for block in content or []:
        if not isinstance(block, dict):
            continue
        if block.get("type") == "text" or (
                block.get("text") and block.get("type") in (None, "", "text")):
            out.append(str(block.get("text") or ""))
    return "".join(out)


#: OMP 原生委派工具名。源码（src/tools/builtin-names.ts、task/executor.ts）
#: 与本机 17.2.12 实测确认：它是唯一使用 ``task``/``tasks`` 参数的内置工具。
_OMP_TASK_TOOL = "task"

#: 子智能体提交结构化结果的内置工具（task/yield-assembly.ts）。
_OMP_YIELD_TOOL = "yield"

#: SubagentLifecycle/AgentProgress 原生状态 → 统一 node 状态。
_OMP_AGENT_STATUS = {
    "pending": "pending",
    "started": "running",
    "running": "running",
    "completed": "completed",
    "failed": "failed",
    "aborted": "cancelled",
}

_OMP_TERMINAL_STATUS = {"completed", "failed", "cancelled"}


def _omp_task_items(args: Any) -> list[dict[str, Any]]:
    """task 工具参数 → 委派条目（批量 ``tasks[]`` 或单条扁平形态）。"""
    if not isinstance(args, dict):
        return []
    items = args.get("tasks")
    if isinstance(items, list):
        return [item for item in items if isinstance(item, dict)]
    if isinstance(args.get("task"), str):
        return [args]
    return []


def _omp_result_text(value: Any) -> str:
    if value is None:
        return ""
    if isinstance(value, str):
        return value
    return json.dumps(value, ensure_ascii=False, default=str)


def _omp_content_text(result: Any) -> str:
    """tool result.content 的 text 块拼接。"""
    if not isinstance(result, dict):
        return ""
    out: list[str] = []
    for block in result.get("content") or []:
        if isinstance(block, dict) and block.get("type") == "text":
            out.append(str(block.get("text") or ""))
    return "".join(out)


def _omp_task_details(raw_output: Any) -> Optional[dict[str, Any]]:
    """tool_execution_end/update 的 result/partialResult → TaskToolDetails。

    ``projectAgentsDir`` 与 ``totalDurationMs`` 是 TaskToolDetails 的固有
    键（src/task/types.ts），用来把 task 工具的 details 与其他工具的
    ``results`` 字段区分开。
    """
    if not isinstance(raw_output, dict):
        return None
    details = raw_output.get("details")
    if not isinstance(details, dict):
        return None
    if "projectAgentsDir" not in details and "totalDurationMs" not in details:
        return None
    if not ("results" in details or "progress" in details or "async" in details):
        return None
    return details


def _omp_progress_update(
    progress: dict[str, Any], existing: Optional[dict[str, Any]]
) -> dict[str, Any]:
    """AgentProgress（RPC subagent_progress / ACP details.progress[]）→ node patch。"""
    update: dict[str, Any] = {}
    mapped = _OMP_AGENT_STATUS.get(str(progress.get("status") or ""))
    if mapped:
        previous = str((existing or {}).get("status") or "")
        # 终态不被迟到的 running/pending 重开；已进入 running 不回退 pending。
        if previous in _OMP_TERMINAL_STATUS and mapped in ("pending", "running"):
            pass
        elif previous == "running" and mapped == "pending":
            pass
        else:
            update["status"] = mapped
    description = str(progress.get("description") or "").strip()
    if description:
        update["title"] = description
    model = str(progress.get("resolvedModel") or "").strip()
    if not model:
        override = progress.get("modelOverride")
        if isinstance(override, list) and override:
            model = str(override[0])
        elif isinstance(override, str):
            model = override.strip()
    if model:
        update["model"] = model
    activity = (str(progress.get("lastIntent") or "").strip()
                or str(progress.get("currentTool") or "").strip())
    if activity:
        update["activity"] = activity
    for key, native in (("tool_uses", "toolCount"),
                        ("total_tokens", "tokens"),
                        ("duration_ms", "durationMs")):
        if progress.get(native) is not None:
            update[key] = progress.get(native)
    retry_failure = progress.get("retryFailure")
    if isinstance(retry_failure, dict) and retry_failure.get("errorMessage"):
        # 子智能体放弃重试的终局错误；lifecycle failed 不带错误正文。
        update["error"] = str(retry_failure["errorMessage"])
    if mapped == "completed":
        yield_data = (progress.get("extractedToolData") or {}).get("yield")
        if isinstance(yield_data, list) and yield_data:
            last = yield_data[-1]
            payload = (last.get("data")
                       if isinstance(last, dict) and "data" in last else last)
            text = _omp_result_text(payload)
            if text:
                update["result"] = text
    return update


def _omp_result_update(
    agent_id: str,
    entry: dict[str, Any],
    progress: Optional[dict[str, Any]],
) -> dict[str, Any]:
    """SingleResult（TaskToolDetails.results[]，同步路径）→ node patch。"""
    update: dict[str, Any] = {"agent_id": agent_id}
    aborted = bool(entry.get("aborted"))
    failed = bool(entry.get("error")) or entry.get("exitCode") not in (None, 0)
    if aborted:
        update["status"] = "cancelled"
    elif failed:
        update["status"] = "failed"
    else:
        update["status"] = "completed"
    output = str(entry.get("output") or "")
    if update["status"] == "completed":
        if output:
            update["result"] = output
    elif update["status"] == "failed":
        error = str(entry.get("error") or "") or output
        if error:
            update["error"] = error
    assignment = str(entry.get("assignment") or entry.get("task") or "").strip()
    if assignment:
        update["request"] = assignment
    if entry.get("description"):
        update["title"] = str(entry["description"])
    if entry.get("agent"):
        update["role"] = str(entry["agent"])
    if entry.get("resolvedModel"):
        update["model"] = str(entry["resolvedModel"])
    if entry.get("tokens") is not None:
        update["total_tokens"] = entry.get("tokens")
    if entry.get("durationMs") is not None:
        update["duration_ms"] = entry.get("durationMs")
    if isinstance(progress, dict) and progress.get("toolCount") is not None:
        update["tool_uses"] = progress.get("toolCount")
    return update


def _omp_usage_payload(usage: dict[str, Any], usage_id: str) -> dict[str, Any]:
    """OMP (Pi 系血缘) usage → normalized contract; ``input`` 不含缓存，
    归一化 ``input_tokens`` 补回 cacheRead/cacheWrite。"""
    def _num(value: Any) -> Optional[int]:
        return value if isinstance(value, int) and not isinstance(value, bool) and value >= 0 else None
    cost = usage.get("cost")
    buckets = {key: _num(usage.get(key))
               for key in ("input", "output", "cacheRead", "cacheWrite", "totalTokens")}
    input_tokens = None
    if any(buckets[key] is not None for key in ("input", "cacheRead", "cacheWrite")):
        input_tokens = (buckets["input"] or 0) + (buckets["cacheRead"] or 0) \
            + (buckets["cacheWrite"] or 0)
    return dump_payload(UsagePayload(
        scope="message",
        usage_id=usage_id,
        input_tokens=input_tokens,
        output_tokens=buckets["output"],
        cached_input_tokens=buckets["cacheRead"],
        cache_write_tokens=buckets["cacheWrite"],
        total_tokens=buckets["totalTokens"],
        cost_usd=(
            value if isinstance(cost, dict)
            and isinstance((value := cost.get("total")), (int, float))
            and not isinstance(value, bool) else None
        ),
        native=dict(usage),
    ))


class OmpRpcAdapter(BaseExternalAgentAdapter, RuntimeOperationAdapter):
    """OMP 的 NDJSON RPC 结构化 Adapter（``--mode rpc`` / ``rpc-ui``）。"""

    adapter_id = "omp.rpc"
    context_compaction_events = True

    def __init__(
        self,
        *,
        binary: Optional[str] = None,
        rpc_ui: bool = False,
        session_dir: Optional[str] = None,
        default_env: Optional[dict[str, str]] = None,
        startup_timeout: float = 30.0,
        prompt_timeout: float = 600.0,
        **kwargs: Any,
    ) -> None:
        super().__init__(self.adapter_id, **kwargs)
        self._binary = binary or os.environ.get("MUTEKI_OMP_BIN", "omp")
        self._rpc_ui = rpc_ui
        self._session_dir = session_dir
        self._default_env = dict(default_env or {})
        self._startup_timeout = float(startup_timeout)
        self._prompt_timeout = float(prompt_timeout)
        self._rpc: dict[str, dict[str, Any]] = {}

    # -- argv ---------------------------------------------------------------

    def _rpc_argv(
        self,
        *,
        cwd: Optional[str] = None,
        provider: Optional[str] = None,
        model: Optional[str] = None,
        role: Optional[str] = None,
        add_dirs: Optional[list[str]] = None,
        no_session: bool = False,
        session_dir: Optional[str] = None,
    ) -> list[str]:
        argv = self._with_launch_args([self._binary, "--mode", "rpc-ui" if self._rpc_ui else "rpc"])
        if cwd:
            argv += ["--cwd", cwd]
        if provider:
            argv += ["--provider", provider]
        if model:
            argv += ["--model", model]
        if role and role in _ROLE_FLAGS and model:
            # role flag 需要模型值；无 model 时忽略（probe degradation 已说明
            # role 不是会话参数）。
            argv += [_ROLE_FLAGS[role], model]
        for extra in add_dirs or []:
            argv += ["--add-dir", extra]
        if no_session:
            argv.append("--no-session")
        elif session_dir or self._session_dir:
            argv += ["--session-dir", session_dir or str(self._session_dir)]
        return argv

    @staticmethod
    def _resolve_provider_model(
        request: SessionStart, env: dict[str, str]
    ) -> tuple[str, str]:
        """Resolve OMP identity without replacing an explicit Thread model."""
        provider = str(env.get("MUTEKI_OMP_PROVIDER") or "").strip()
        explicit_model = str(request.model or "").strip()
        model = explicit_model or str(env.get("MUTEKI_OMP_MODEL") or "").strip()
        if not provider and "/" in model:
            provider, model = model.split("/", 1)
        return provider, model

    @staticmethod
    async def _cmd(peer: StdioJsonlPeer, command: str,
                   params: Optional[dict[str, Any]] = None,
                   timeout: Optional[float] = 60.0) -> dict[str, Any]:
        """发命令并按 id 等响应；``success:false`` 抛运行时错误。"""
        resp = await peer.request(command, params, timeout=timeout)
        if not resp.get("success"):
            raise RuntimeError(
                f"omp rpc {command} failed: {resp.get('error')}")
        data = resp.get("data")
        return data if isinstance(data, dict) else {}

    async def _negotiate_v2(
        self, peer: StdioJsonlPeer, ready: dict[str, Any]
    ) -> bool:
        """优先启用 v2 分块；旧实现即使误报支持也可继续使用 v1。"""
        if 2 not in (ready.get("supportedProtocolVersions") or []):
            return False
        try:
            negotiated = await self._cmd(
                peer, "negotiate_protocol", {"protocolVersion": 2},
                timeout=self._startup_timeout)
        except RuntimeError:
            return False
        if negotiated.get("protocolVersion") != 2:
            return False
        peer.enable_rpc_chunks()
        return True

    # -- probe ----------------------------------------------------------------

    async def probe(self, request: ProbeRequest) -> AgentCapabilities:
        field_sources: dict[str, str] = {}
        degradations: list[str] = []
        caps = conservative_capabilities(
            transport_kind="rpc", capability_source=SOURCE_STATIC)
        version = _probe_version(self._binary)
        caps.runtime_version = version
        detail = ""
        ready: dict[str, Any] = {}
        state: dict[str, Any] = {}
        v2_negotiated = False
        if version:
            ready_fut: asyncio.Future = asyncio.get_running_loop() \
                .create_future()

            async def on_message(msg: dict[str, Any]) -> None:
                if msg.get("type") == "ready" and not ready_fut.done():
                    ready_fut.set_result(msg)

            peer = StdioJsonlPeer(
                self._rpc_argv(no_session=True), label="omp.probe",
                env=self._default_env,
                request_envelope=_omp_envelope, on_message=on_message)
            try:
                await peer.start()
                ready = await asyncio.wait_for(ready_fut,
                                               timeout=self._startup_timeout)
                v2_negotiated = await self._negotiate_v2(peer, ready)
                state = await self._cmd(peer, "get_state",
                                        timeout=self._startup_timeout)
                try:
                    await self._cmd(peer, "set_subagent_subscription",
                                    {"level": "events"},
                                    timeout=self._startup_timeout)
                    caps.subagents = True
                except (RuntimeError, asyncio.TimeoutError,
                        PeerClosedError) as exc:
                    degradations.append(
                        "subagents：set_subagent_subscription 实测失败："
                        f"{exc}；task 子智能体事件不可用")
                if request.include_models:
                    try:
                        models = await self._cmd(
                            peer, "get_available_models",
                            timeout=self._startup_timeout)
                        caps.supported_models = [
                            str(m.get("id")) for m in (
                                models.get("models") or [])
                            if isinstance(m, dict) and m.get("id")
                        ][:50]
                    except Exception:  # noqa: BLE001
                        pass
            except (OSError, asyncio.TimeoutError, PeerClosedError,
                    RuntimeError) as exc:
                detail = f"omp rpc 探测失败：{exc}"
                ready, state = {}, {}
            finally:
                await peer.close()

        probed = bool(ready and state is not None and version)
        if not probed:
            for field_name in BOOL_CAPABILITY_FIELDS:
                field_sources[field_name] = SOURCE_STATIC
            degradations.append(
                "structured transport 不可用：prompt/stream/abort/host "
                "tools 均未实测，按保守默认 False（不静默降级）")
            self._probe_cache = CapabilityProbeReport(
                adapter_id=self.identity.adapter_id,
                instance_id=self.identity.instance_id,
                capabilities=caps, binary_path=self._binary,
                field_sources=field_sources, degradations=degradations,
                detail=detail or "binary 不可用或 --version 失败",
            )
            return caps

        caps.capability_source = SOURCE_PROBE
        caps.protocol_version = (
            "2" if v2_negotiated else str(ready.get("protocolVersion") or ""))
        caps.streaming = True
        caps.tool_events = True
        caps.usage_events = True
        caps.interrupt = True       # abort 命令
        caps.steer = True           # steer 命令（fork 血缘，实测于 mock）
        caps.resume = True          # --resume / switch_session（fork 血缘）
        caps.resume_continues_turn = False
        caps.session_persistence = True
        caps.user_input = self._rpc_ui  # extension_ui_request 子协议
        caps.structured_http_rpc = True  # set_host_tools 反向工具（§OMP-10）
        # RPC command "compact" {customInstructions?}（rpc-mode.ts），
        # runtime 快照以 verified operation 暴露。
        caps.compaction = True
        for field_name in BOOL_CAPABILITY_FIELDS:
            field_sources[field_name] = SOURCE_PROBE
        if not self._rpc_ui:
            field_sources["user_input"] = SOURCE_STATIC
            degradations.append(
                "user_input：--mode rpc 也会出 extension_ui_request"
                "（setWidget，以及 always-ask 时的 select），但本实例把它们"
                "应答成 cancelled，不声明 user_input")
        versions = ready.get("supportedProtocolVersions") or []
        if 2 in versions and not v2_negotiated:
            degradations.append(
                "对端声明 RPC v2，但协商未成功；当前会话继续使用 v1")
        elif 2 not in versions:
            degradations.append(
                f"对端仅支持 RPC v1（物理帧上限 "
                f"{ready.get('maxFrameBytes', '?')} 字节）")
        # 17.2.12 实测（/tmp 隔离，协议 v2）：不传 --approval-mode 时
        # 工具直接执行，没有审批帧。--approval-mode always-ask 时进程会
        # 发出 extension_ui_request method=select（Approve/Deny），宿主
        # 回 extension_ui_response value=Approve 即可放行。本适配器不把
        # Conversation access mode 映射成该 flag，_auto_answer_ui 又把
        # extension_ui 应答成 cancelled，所以 access mode 不被遵守。
        # 不在运行时改走 omp.acp。
        degradations.append(
            "[omp.rpc.access_mode_not_honored] Conversation access mode "
            "不被遵守：启动参数不含 --approval-mode，未设置时工具按默认 "
            "策略直接执行；always-ask 的带内审批是 extension_ui select，"
            "本适配器不传该 flag，并把 extension_ui 应答成 cancelled。"
            "不会自动改走 omp.acp")
        self._probe_cache = CapabilityProbeReport(
            adapter_id=self.identity.adapter_id,
            instance_id=self.identity.instance_id,
            capabilities=caps, binary_path=self._binary,
            field_sources=field_sources, degradations=degradations,
            detail="omp rpc ready + get_state 协商成功",
        )
        return caps

    # -- host tools 注入（§OMP-10） ------------------------------------------------

    async def _inject_host_tools(
        self,
        peer: StdioJsonlPeer,
        handle: dict[str, Any],
        plan: Optional[CapabilityInjectionPlan],
        bearer_token: Optional[str],
    ) -> int:
        """HTTP_JSONRPC 计划 → ``set_host_tools`` 反向工具注入。

        工具执行由 ``host_tool_call`` 回调代理到 Gateway HTTP/JSON-RPC
        Bridge；凭据本体进入请求头。
        """
        if plan is None or plan.injection_kind is not InjectionKind.HTTP_JSONRPC:
            return 0
        tools = [{
            "name": tool.name,
            "description": tool.description,
            "parameters": tool.input_schema or {"type": "object"},
        } for tool in plan.tool_descriptions]
        if not tools:
            return 0
        handle["gateway_endpoint"] = plan.gateway_endpoint
        handle["gateway_token"] = bearer_token
        await self._cmd(peer, "set_host_tools", {"tools": tools},
                        timeout=self._startup_timeout)
        return len(tools)

    async def _serve_host_tool_call(
        self, handle: dict[str, Any], msg: dict[str, Any]
    ) -> None:
        """``host_tool_call`` → Gateway ``muteki.invoke`` → ``host_tool_result``。"""
        peer: StdioJsonlPeer = handle["peer"]
        call_id = str(msg.get("id") or "")
        tool_name = str(
            msg.get("toolName") or msg.get("name") or msg.get("tool") or ""
        )
        arguments = msg.get("arguments") or msg.get("args") or {}
        endpoint = handle.get("gateway_endpoint") or ""
        token = handle.get("gateway_token") or ""
        result_payload: dict[str, Any]
        try:
            if not endpoint:
                raise RuntimeError("gateway endpoint 未配置")
            async with httpx.AsyncClient(
                timeout=60.0,
                trust_env=not str(endpoint).startswith(
                    ("http://127.0.0.1", "http://localhost")),
            ) as client:
                resp = await client.post(endpoint, json={
                    "jsonrpc": "2.0", "id": new_id("invoke"),
                    "method": METHOD_INVOKE,
                    "params": {"tool_name": tool_name,
                               "arguments": arguments},
                }, headers={"Authorization": f"Bearer {token}"} if token
                            else {})
                resp.raise_for_status()
                result_payload = resp.json()
        except Exception as exc:  # noqa: BLE001
            result_payload = {"error": {"message": str(exc)}}

        rpc_error = result_payload.get("error")
        gateway_result = result_payload.get("result", result_payload)
        is_error = bool(rpc_error)
        if isinstance(gateway_result, dict) and gateway_result.get("ok") is False:
            is_error = True
        images = (gateway_result.pop("images", None) or []
                  if isinstance(gateway_result, dict) else [])
        visible_result = (
            {"error": rpc_error} if rpc_error is not None else gateway_result
        )
        agent_tool_result = {
            "content": [{
                "type": "text",
                "text": json.dumps(
                    visible_result, ensure_ascii=False, default=str),
            }, *(image for image in images if isinstance(image, dict))],
            "details": visible_result,
        }
        try:
            await peer.send({
                "type": "host_tool_result", "id": call_id,
                "result": agent_tool_result,
                "isError": is_error,
            })
        except PeerClosedError:
            pass
        sink = handle.get("event_sink")
        if sink is not None:
            sink.put_nowait((AgentEventType.TOOL_COMPLETED,
                             "omp.host_tool_call",
                             dump_payload(ToolPayload(
                                 tool_call_id=str(call_id or ""),
                                 name=tool_name,
                                 kind="mcp",
                                 status="failed" if is_error else "completed",
                                 native={"host_tool": True},
                             ))))

    # -- 启动 / 接管 -------------------------------------------------------------

    async def _launch(
        self,
        request: SessionStart,
        plan: Optional[CapabilityInjectionPlan],
        bearer_token: Optional[str],
    ) -> dict[str, Any]:
        options = request.options
        if options.is_conversation:
            raise AccessModeUnsupportedError(
                self.adapter_id, request.access_mode or "supervised", (),
                "OMP rpc_v2 不能执行 Muteki 对话的权限审批；请显式选择 ACP 接入方式。")
        cwd = options.cwd or os.getcwd()
        # access_mode 不映射到 --approval-mode，也不把本次启动改成 omp acp。
        # supervised 因此不会在这个进程里变成可应答的审批；见 probe
        # degradation omp.rpc.access_mode_not_honored。
        process_env = {**self._default_env, **options.env}
        provider, model = self._resolve_provider_model(request, process_env)
        handle: dict[str, Any] = {
            "conversation_thread_id": request.thread_id,
            "cwd": cwd,
            "options": options,
            "turns": 0,
            "external_session_id": None,
            "resume_handle": request.resume_handle,
            "resumed": bool(request.resume_handle),
            "event_sink": None,
            "current_turn_id": None,
            "ready": None,
            "host_tools": 0,
            "host_tool_tasks": set(),
            "available_commands": [],
            "capability_revision": 0,
            # 子智能体状态跨 turn 保持：异步 task 的 settle 帧可能晚于父 turn
            # 结束到达，agent_dirty 记录待下一 turn 开头补播的 node。
            "agent_nodes": {},
            "agent_dirty": set(),
            "agent_results": {},
            "agent_last_text": {},
            "agent_errors": {},
            "tool_call_args": {},
            "usage_message_index": 0,
        }
        ready_fut: asyncio.Future = asyncio.get_running_loop().create_future()

        async def on_message(msg: dict[str, Any]) -> None:
            mtype = msg.get("type")
            if mtype == "ready":
                handle["ready"] = msg
                if not ready_fut.done():
                    ready_fut.set_result(msg)
                return
            if mtype == "host_tool_call":
                task = asyncio.ensure_future(
                    self._serve_host_tool_call(handle, msg))
                handle["host_tool_tasks"].add(task)
                task.add_done_callback(handle["host_tool_tasks"].discard)
                return
            if mtype == "extension_ui_request":
                self._auto_answer_ui(handle, msg)
                return
            self._dispatch_event(request.agent_session_id, msg)

        peer = StdioJsonlPeer(
            self._rpc_argv(
                cwd=cwd,
                provider=provider or None,
                model=model or None,
                role=options.role or None,
                add_dirs=list(options.add_dirs),
                no_session=options.ephemeral,
                session_dir=options.session_dir or None,
            ),
            cwd=cwd, label="omp.rpc",
            env=process_env,
            request_envelope=_omp_envelope, on_message=on_message)
        handle["peer"] = peer
        self._rpc[request.agent_session_id] = handle
        try:
            await peer.start()
            # 必须先消费 ready 帧再发命令（§OMP-2）。
            ready = await asyncio.wait_for(
                ready_fut, timeout=self._startup_timeout)
            handle["protocol_v2"] = await self._negotiate_v2(peer, ready)
            state = await self._cmd(peer, "get_state",
                                    timeout=self._startup_timeout)
            if request.resume_handle:
                await self._cmd(peer, "switch_session",
                                {"sessionPath": request.resume_handle},
                                timeout=self._startup_timeout)
            if provider and model:
                # set_model 双字段成对（§OMP-6）；放在 resume 后确保当前 Thread
                # 的显式模型选择不被历史会话状态覆盖。
                await self._cmd(peer, "set_model",
                                {"provider": provider, "modelId": model},
                                timeout=self._startup_timeout)
            if request.effort and request.effort != "default":
                await self._cmd(peer, "set_thinking_level",
                                {"level": request.effort},
                                timeout=self._startup_timeout)
            # 订阅子智能体帧（probe 已实测该命令；失败则显式终止启动，
            # 不与 caps.subagents 产生口径差）。
            await self._cmd(peer, "set_subagent_subscription",
                            {"level": "events"},
                            timeout=self._startup_timeout)
            handle["host_tools"] = await self._inject_host_tools(
                peer, handle, plan, bearer_token)
            catalog = await self._cmd(
                peer, "get_available_commands", timeout=self._startup_timeout)
            handle["available_commands"] = list(catalog.get("commands") or [])
            handle["capability_revision"] = 1
            state = await self._cmd(peer, "get_state", timeout=self._startup_timeout)
        except BaseException:
            self._rpc.pop(request.agent_session_id, None)
            await peer.close()
            raise

        session_id = str(state.get("sessionId") or "")
        handle["external_session_id"] = session_id or None
        handle["resume_handle"] = str(state.get("sessionFile") or request.resume_handle or "") or None
        return {
            "external_session_id": session_id or None,
            "resume_handle": handle["resume_handle"],
        }

    # -- 交互请求自动应答（rpc-ui） ---------------------------------------------------

    def _auto_answer_ui(self, handle: dict[str, Any], msg: dict[str, Any]) -> None:
        """``extension_ui_request`` → 无人值守默认 ``cancelled`` 应答。

        对话类请求带 timeout（超时自动以默认值 resolve），但无人值守
        Worker 立即应答可避免空等；先出 USER_INPUT_REQUESTED 事件，
        不静默吞掉。``--approval-mode always-ask`` 的工具审批也走这条
        select；cancelled 不会把它交给用户，本适配器也不据此改传输。
        """
        request_id = msg.get("id")
        method = str(msg.get("method") or "")
        sink = handle.get("event_sink")
        if sink is not None:
            sink.put_nowait((AgentEventType.USER_INPUT_REQUESTED,
                             "omp.extension_ui_request",
                             dump_payload(UserInputRequestedPayload(
                                 request_id=str(request_id or ""),
                                 native={"ui_method": method},
                             ))))

        async def respond() -> None:
            try:
                await handle["peer"].send({
                    "type": "extension_ui_response", "id": request_id,
                    "cancelled": True,
                })
            except PeerClosedError:
                pass
            if sink is not None:
                sink.put_nowait((AgentEventType.USER_INPUT_RESOLVED,
                                 "omp.extension_ui_response",
                                 dump_payload(UserInputResolvedPayload(
                                     request_id=str(request_id or ""),
                                     outcome="cancelled",
                                 ))))
        asyncio.ensure_future(respond())

    # -- 事件归一化 -------------------------------------------------------------

    def _dispatch_event(self, agent_session_id: str,
                        msg: dict[str, Any]) -> None:
        """OMP 事件流 → turn 队列（reader task 上运行，只 put_nowait）。

        事件形态按 Pi 系血缘宽容解析；完成信号只认 ``agent_end`` /
        ``agent_settled`` 且 ``isTerminal !== false``。
        """
        handle = self._rpc.get(agent_session_id)
        if handle is None:
            return
        etype = msg.get("type")
        if etype in {"compaction_end", "auto_compaction_end"}:
            if isinstance(msg.get("result"), dict) and not msg.get("aborted") and not msg.get("errorMessage") and not msg.get("skipped"):
                self._context_compacted(agent_session_id)
            return
        if etype == "available_commands_update":
            handle["available_commands"] = list(msg.get("commands") or [])
            handle["capability_revision"] = int(
                handle.get("capability_revision") or 0
            ) + 1
            sink = handle.get("event_sink")
            if sink is not None:
                sink.put_nowait((
                    AgentEventType.RUNTIME_CAPABILITIES_UPDATED,
                    "omp.available_commands_update",
                    dump_payload(RuntimeCapabilitiesPayload(
                        revision=handle["capability_revision"],
                        reason="commands_changed",
                        native={
                            "adapter_id": self.id,
                            "commands": list(handle["available_commands"]),
                        },
                    )),
                ))
            return
        if etype == "subagent_lifecycle":
            self._on_subagent_lifecycle(handle, msg.get("payload"))
            return
        if etype == "subagent_progress":
            self._on_subagent_progress(handle, msg.get("payload"))
            return
        if etype == "subagent_event":
            self._on_subagent_event(handle, msg.get("payload"))
            return
        sink = handle.get("event_sink")
        if sink is None:
            return
        if etype == "command_output":
            text = str(msg.get("text") or "")
            if text:
                handle["assistant_text"] = str(handle.get("assistant_text") or "") + text + "\n"
                handle["saw_assistant_text"] = True
                sink.put_nowait((AgentEventType.MESSAGE_DELTA, "omp.command_output", dump_payload(
                    MessageDeltaPayload(text=text + "\n", native={"command_output": True}))))
        elif etype == "message_update":
            usage = msg.get("usage")
            if isinstance(usage, dict) and usage:
                sink.put_nowait((AgentEventType.USAGE_UPDATED,
                                 "omp.message_update",
                                 _omp_usage_payload(usage, str(handle.get("usage_message_index", 0)))))
            delta = msg.get("assistantMessageEvent") or {}
            if delta.get("type") == "text_delta" and delta.get("delta"):
                sink.put_nowait((AgentEventType.MESSAGE_DELTA,
                                 "omp.text_delta",
                                 dump_payload(MessageDeltaPayload(
                                     text=str(delta["delta"]),
                                     native={"content_index": delta.get("contentIndex")}))))
                handle["saw_assistant_text"] = True
        elif etype == "message_end":
            message = msg.get("message") or {}
            if not isinstance(message, dict) or message.get("role") != "assistant":
                return
            text = _omp_message_text(message)
            usage = message.get("usage")
            if isinstance(usage, dict) and usage:
                sink.put_nowait((AgentEventType.USAGE_UPDATED,
                                 "omp.message_end",
                                 _omp_usage_payload(usage, str(handle.get("usage_message_index", 0)))))
            # 消息水位单调递增，无 usage 的消息也占位，避免两条消息的
            # usage 落到同一个 ledger identity 互相覆盖。
            handle["usage_message_index"] = int(handle.get("usage_message_index", 0)) + 1
            error_message = str(message.get("errorMessage") or "").strip()
            stop_reason = str(message.get("stopReason") or "").strip()
            if error_message or stop_reason == "error":
                detail = error_message or f"stopReason={stop_reason or 'error'}"
                handle["turn_failed"] = detail
                sink.put_nowait((AgentEventType.RUNTIME_ERROR,
                                 "omp.message_end",
                                 dump_payload(RuntimeErrorPayload(
                                     error=self.failure(
                                         FailureCategory.PROVIDER,
                                         "assistant_error",
                                         message=detail, detail=detail,
                                         native_code=stop_reason or None)))))
                return
            if text:
                handle["assistant_text"] = text
                handle["saw_assistant_text"] = True
        elif etype == "tool_execution_start":
            tool = str(msg.get("toolName") or "")
            call_id = str(msg.get("toolCallId") or "")
            if tool == _OMP_TASK_TOOL and isinstance(msg.get("args"), dict):
                # lifecycle 帧只带 parentToolCallId；task 原文按调用 id 暂存，
                # turn 开始时清空。
                handle["tool_call_args"][call_id] = msg["args"]
            sink.put_nowait((AgentEventType.TOOL_STARTED,
                             "omp.tool_execution_start",
                             dump_payload(ToolPayload(
                                 tool_call_id=call_id,
                                 name=tool,
                                 input=msg.get("args"),
                                 status="running",
                                 kind="agent" if tool == _OMP_TASK_TOOL else None,
                             ))))
        elif etype == "tool_execution_end":
            call_id = str(msg.get("toolCallId") or "")
            tool = str(msg.get("toolName") or "")
            is_error = bool(msg.get("isError"))
            sink.put_nowait((AgentEventType.TOOL_COMPLETED,
                             "omp.tool_execution_end",
                             dump_payload(ToolPayload(
                                 tool_call_id=call_id,
                                 output=msg.get("result"),
                                 status="failed" if is_error else "completed",
                                 kind="agent" if tool == _OMP_TASK_TOOL else None,
                             ))))
            if tool == _OMP_TASK_TOOL:
                self._on_task_call_end(handle, call_id, msg.get("result"),
                                       is_error)
        elif etype in ("agent_end", "agent_settled"):
            # isTerminal 字段可选，缺省视为终止（§OMP-8）。
            if msg.get("isTerminal") is False:
                return
            sink.put_nowait(("__turn_done__", etype, {
                "will_retry": bool(msg.get("willRetry")),
                "is_terminal": msg.get("isTerminal", True),
            }))
        elif etype == "prompt_result":
            # 本地完成（agentInvoked=false）也算 turn 收尾。
            if msg.get("agentInvoked") is False or (
                    isinstance(msg.get("data"), dict)
                    and msg["data"].get("agentInvoked") is False):
                sink.put_nowait(("__turn_done__", etype,
                                 {"local": True, "prompt_result": msg}))
        # turn_start/compaction_*/queue_update 等其余事件不改变核心状态机。

    # -- 子智能体（原生 task 工具） --------------------------------------------

    def _agent_patch(self, handle: dict[str, Any], agent_id: str,
                     update: dict[str, Any], native_type: str) -> None:
        """按 key 合并子智能体 node 并广播 AGENT_UPDATED 补丁。

        turn 外到达的 settle 帧没有 sink：node 状态照更新，agent_id 记入
        agent_dirty，由下一 turn 开头补播最新快照（不丢终态）。
        """
        nodes = handle.setdefault("agent_nodes", {})
        previous = nodes.get(agent_id)
        if previous is None:
            # 恢复后先见到进展后见到 started，也要有 node。
            previous = {"agent_id": agent_id, "title": agent_id,
                        "status": "running"}
        node = {**previous, **{k: v for k, v in update.items() if v is not None}}
        for key, value in update.items():
            if value is None and key in ("result", "error"):
                node[key] = None
        if node == previous and agent_id in nodes:
            return
        nodes[agent_id] = node
        sink = handle.get("event_sink")
        if sink is not None:
            sink.put_nowait((AgentEventType.AGENT_UPDATED, native_type,
                             dump_payload(AgentUpdatedPayload(
                                 agents=[AgentNodePayload(**node)],
                                 patch=True))))
        else:
            handle.setdefault("agent_dirty", set()).add(agent_id)

    @staticmethod
    def _task_item_for(handle: dict[str, Any], call_id: str,
                       payload: dict[str, Any]) -> Optional[dict[str, Any]]:
        """lifecycle.started → task 调用参数里的对应条目（显式 name 与
        子智能体 id 一致；匿名条目按批次 index 对齐，实测于 17.2.12）。"""
        args = (handle.get("tool_call_args") or {}).get(call_id)
        items = _omp_task_items(args)
        if not items:
            return None
        agent_id = str(payload.get("id") or "")
        for item in items:
            if item.get("name") and str(item["name"]) == agent_id:
                return item
        index = payload.get("index")
        if isinstance(index, int) and not isinstance(index, bool) \
                and 0 <= index < len(items):
            return items[index]
        return items[0] if len(items) == 1 else None

    def _on_subagent_lifecycle(self, handle: dict[str, Any], payload: Any
                               ) -> None:
        if not isinstance(payload, dict):
            return
        agent_id = str(payload.get("id") or "")
        status = str(payload.get("status") or "")
        mapped = _OMP_AGENT_STATUS.get(status)
        if not agent_id or not mapped:
            return
        update: dict[str, Any] = {"agent_id": agent_id, "status": mapped}
        call_id = str(payload.get("parentToolCallId") or "")
        if call_id:
            update["call_id"] = call_id
        description = str(payload.get("description") or "").strip()
        if description:
            update["title"] = description
        if payload.get("agent"):
            update["role"] = str(payload["agent"])
        if payload.get("sessionFile"):
            update["session_ref"] = str(payload["sessionFile"])
        if status == "started":
            update["result"] = None
            update["error"] = None
            item = self._task_item_for(handle, call_id, payload)
            if item is not None:
                if not description and item.get("name"):
                    update["title"] = str(item["name"])
                if item.get("task"):
                    update["request"] = str(item["task"])
        elif mapped == "completed":
            result = handle["agent_results"].pop(agent_id, None) \
                or handle["agent_last_text"].pop(agent_id, None)
            if result:
                update["result"] = result
            handle["agent_errors"].pop(agent_id, None)
        elif mapped == "failed":
            error = handle["agent_errors"].pop(agent_id, None)
            if error:
                update["error"] = error
            handle["agent_results"].pop(agent_id, None)
            handle["agent_last_text"].pop(agent_id, None)
        else:
            for key in ("agent_results", "agent_last_text", "agent_errors"):
                handle[key].pop(agent_id, None)
        self._agent_patch(handle, agent_id, update,
                          f"omp.subagent_lifecycle.{status}")

    def _on_subagent_progress(self, handle: dict[str, Any], payload: Any
                              ) -> None:
        if not isinstance(payload, dict):
            return
        progress = payload.get("progress")
        if not isinstance(progress, dict):
            return
        agent_id = str(progress.get("id") or "")
        if not agent_id:
            return
        existing = (handle.get("agent_nodes") or {}).get(agent_id)
        update = _omp_progress_update(progress, existing)
        update["agent_id"] = agent_id
        call_id = str(payload.get("parentToolCallId") or "")
        if call_id:
            update["call_id"] = call_id
        # assignment 是委派原文；task 字段带 executor 的包装前缀。
        assignment = str(payload.get("assignment") or "").strip()
        if assignment:
            update["request"] = assignment
        elif payload.get("task"):
            update["request"] = str(payload["task"])
        if payload.get("agent"):
            update["role"] = str(payload["agent"])
        if payload.get("sessionFile"):
            update["session_ref"] = str(payload["sessionFile"])
        self._agent_patch(handle, agent_id, update, "omp.subagent_progress")

    def _on_subagent_event(self, handle: dict[str, Any], payload: Any) -> None:
        """子会话内部事件：工具事件带 agent_id 照常发；文本只进 node。"""
        if not isinstance(payload, dict):
            return
        agent_id = str(payload.get("id") or "")
        event = payload.get("event")
        if not agent_id or not isinstance(event, dict):
            return
        etype = event.get("type")
        sink = handle.get("event_sink")
        if etype == "tool_execution_start":
            tool = str(event.get("toolName") or "")
            call_id = str(event.get("toolCallId") or "")
            args = event.get("args")
            if tool == _OMP_TASK_TOOL:
                # 嵌套委派：孙智能体运行在子进程内，父进程收不到它们的
                # lifecycle 帧，node 以这次嵌套 task 调用本身为粒度。
                items = _omp_task_items(args)
                nested: dict[str, Any] = {
                    "agent_id": f"{agent_id}/{call_id}",
                    "parent_id": agent_id,
                    "call_id": call_id,
                    "status": "running",
                    "result": None,
                    "error": None,
                }
                if len(items) == 1:
                    item = items[0]
                    if item.get("name"):
                        nested["title"] = str(item["name"])
                    if item.get("agent"):
                        nested["role"] = str(item["agent"])
                    if item.get("task"):
                        nested["request"] = str(item["task"])
                self._agent_patch(handle, nested["agent_id"], nested,
                                  "omp.subagent_event.tool_execution_start")
            elif tool == _OMP_YIELD_TOOL and isinstance(args, dict) \
                    and args.get("result") is not None:
                handle["agent_results"][agent_id] = _omp_result_text(
                    args["result"])
            if sink is not None:
                sink.put_nowait((AgentEventType.TOOL_STARTED,
                                 "omp.subagent_event.tool_execution_start",
                                 dump_payload(ToolPayload(
                                     tool_call_id=call_id,
                                     name=tool,
                                     input=args,
                                     status="running",
                                     agent_id=agent_id,
                                     kind="agent" if tool == _OMP_TASK_TOOL else None,
                                 ))))
            return
        if etype == "tool_execution_update":
            if sink is not None:
                sink.put_nowait((AgentEventType.TOOL_PROGRESS,
                                 "omp.subagent_event.tool_execution_update",
                                 dump_payload(ToolPayload(
                                     tool_call_id=str(event.get("toolCallId") or ""),
                                     output=event.get("partialResult"),
                                     status="running",
                                     agent_id=agent_id,
                                 ))))
            return
        if etype == "tool_execution_end":
            tool = str(event.get("toolName") or "")
            call_id = str(event.get("toolCallId") or "")
            result = event.get("result")
            if sink is not None:
                sink.put_nowait((AgentEventType.TOOL_COMPLETED,
                                 "omp.subagent_event.tool_execution_end",
                                 dump_payload(ToolPayload(
                                     tool_call_id=call_id,
                                     output=result,
                                     status="failed" if event.get("isError") else "completed",
                                     agent_id=agent_id,
                                 ))))
            if tool == _OMP_YIELD_TOOL \
                    and agent_id not in handle["agent_results"]:
                details = result.get("details") if isinstance(result, dict) else None
                if details:
                    handle["agent_results"][agent_id] = _omp_result_text(details)
            elif tool == _OMP_TASK_TOOL:
                self._on_nested_task_end(handle, agent_id, call_id, result,
                                         bool(event.get("isError")))
            return
        if etype == "message_end":
            message = event.get("message")
            if not isinstance(message, dict) or message.get("role") != "assistant":
                return
            error_message = str(message.get("errorMessage") or "").strip()
            if error_message:
                handle["agent_errors"][agent_id] = error_message
            text = _omp_message_text(message)
            if text:
                handle["agent_last_text"][agent_id] = text
                self._agent_patch(handle, agent_id, {"activity": text},
                                  "omp.subagent_event.message_end")

    def _on_nested_task_end(self, handle: dict[str, Any], agent_id: str,
                            call_id: str, result: Any, is_error: bool) -> None:
        nested_id = f"{agent_id}/{call_id}"
        if is_error:
            self._agent_patch(handle, nested_id, {
                "status": "failed",
                "error": _omp_content_text(result) or None,
            }, "omp.subagent_event.tool_execution_end")
            return
        details = _omp_task_details(result)
        if details is None:
            self._agent_patch(handle, nested_id, {
                "status": "completed",
                "result": _omp_content_text(result) or None,
            }, "omp.subagent_event.tool_execution_end")
            return
        async_state = details.get("async")
        if isinstance(async_state, dict) and async_state.get("state") == "running" \
                and not details.get("results"):
            # 嵌套异步委派在子进程内交付，本进程没有后续帧；node 保持 running。
            return
        progress = {
            str(p.get("id")): p
            for p in details.get("progress") or []
            if isinstance(p, dict) and p.get("id")
        }
        results = [r for r in details.get("results") or []
                   if isinstance(r, dict)]
        if results:
            # 嵌套 node 以调用为粒度：任一条目失败即 failed，输出完整拼接。
            failed = [r for r in results
                      if r.get("error") or r.get("exitCode") not in (None, 0)
                      or r.get("aborted")]
            outputs = [str(r.get("output") or "") for r in results
                       if r.get("output")]
            update: dict[str, Any] = {
                "status": "failed" if failed else "completed",
            }
            if failed:
                update["error"] = str(failed[0].get("error") or "") \
                    or (outputs[0] if outputs else None)
            elif outputs:
                update["result"] = "\n\n".join(outputs)
            total_tokens = sum(int(r.get("tokens") or 0) for r in results)
            if total_tokens:
                update["total_tokens"] = total_tokens
            if details.get("totalDurationMs") is not None:
                update["duration_ms"] = details.get("totalDurationMs")
            tool_uses = sum(int((progress.get(str(r.get("id"))) or {}).get(
                "toolCount") or 0) for r in results)
            if tool_uses:
                update["tool_uses"] = tool_uses
        else:
            update = {"status": "completed"}
            text = _omp_content_text(result)
            if text:
                update["result"] = text
        self._agent_patch(handle, nested_id, update,
                          "omp.subagent_event.tool_execution_end")

    def _on_task_call_end(self, handle: dict[str, Any], call_id: str,
                          result: Any, is_error: bool) -> None:
        """父会话 task 调用收尾：同步批次从 details.results 落锤各 node。

        异步派单（details.async.state=="running"）时 results 为空，node
        终态由 subagent_lifecycle 帧驱动，这里不动。
        """
        details = _omp_task_details(result)
        if details is None:
            if is_error:
                self._fail_call_nodes(
                    handle, call_id,
                    _omp_content_text(result) or "task 调用失败")
            return
        progress = {
            str(p.get("id")): p
            for p in details.get("progress") or []
            if isinstance(p, dict) and p.get("id")
        }
        settled: set[str] = set()
        for entry in details.get("results") or []:
            if not isinstance(entry, dict):
                continue
            rid = str(entry.get("id") or "")
            if not rid:
                continue
            settled.add(rid)
            self._apply_task_result(handle, call_id, entry, progress.get(rid))
        async_state = details.get("async")
        if isinstance(async_state, dict) and async_state.get("state") == "failed":
            for rid in progress:
                if rid not in settled:
                    self._agent_patch(handle, rid, {"status": "failed"},
                                      "omp.tool_execution_end.task")
        if is_error and not settled:
            self._fail_call_nodes(
                handle, call_id,
                _omp_content_text(result) or "task 调用失败")

    def _apply_task_result(self, handle: dict[str, Any], call_id: str,
                           entry: dict[str, Any],
                           progress: Optional[dict[str, Any]]) -> None:
        rid = str(entry.get("id") or "")
        update = _omp_result_update(rid, entry, progress)
        update["call_id"] = call_id
        stored = handle["agent_results"].pop(rid, None)
        if stored and update.get("status") == "completed":
            # yield 提交的结构化结果优先于渲染后的 output 文本。
            update["result"] = stored
        handle["agent_last_text"].pop(rid, None)
        handle["agent_errors"].pop(rid, None)
        self._agent_patch(handle, rid, update, "omp.tool_execution_end.task")

    def _fail_call_nodes(self, handle: dict[str, Any], call_id: str,
                         error: str) -> None:
        nodes = handle.get("agent_nodes") or {}
        for agent_id, node in list(nodes.items()):
            if node.get("call_id") == call_id \
                    and node.get("status") in ("pending", "running"):
                self._agent_patch(handle, agent_id, {
                    "status": "failed", "error": error or None,
                }, "omp.tool_execution_end.task")

    # -- turn 流 --------------------------------------------------------------

    def send(
        self, session: AgentSessionRef, input: AgentInput
    ) -> AsyncIterator[AgentEvent]:
        if isinstance(input, MessageInput):
            return self._turn_stream(session, input)
        return self.unsupported_input_stream(session, input)

    async def _turn_stream(
        self, session: AgentSessionRef, input: MessageInput
    ) -> AsyncIterator[AgentEvent]:
        sid = session.agent_session_id
        seq = self.sequencer_for(sid)
        handle = self._rpc.get(sid)
        if handle is None or handle.get("peer") is None:
            async for event in self._unsupported_stream(session, "send",
                                                        "session"):
                yield event
            return
        peer: StdioJsonlPeer = handle["peer"]
        record = self._tracker.get(sid)
        external_id = handle.get("external_session_id") \
            or session.external_session_id
        common = dict(
            agent_session_id=sid,
            run_id=record.run_id if record else None,
            execution_generation=(
                record.execution_generation if record else None),
        )
        if handle["turns"] == 0:
            yield self.emit(build_event(
                AgentEventType.SESSION_RESUMED if handle.get("resumed")
                else AgentEventType.SESSION_STARTED, seq,
                external_session_id=external_id,
                native_type="omp.session.start",
                payload=SessionPayload(
                    transport="rpc-ui" if self._rpc_ui else "rpc",
                    adapter_id=self.id,
                    instance_id=self.identity.instance_id,
                    cwd=handle["cwd"],
                    native={
                        "host_tools": handle.get("host_tools", 0),
                        "protocol_version": (handle.get("ready") or {}).get(
                            "protocolVersion"),
                    }),
                **common))
        turn_id = new_id("turn")
        handle["current_turn_id"] = turn_id
        handle["turn_failed"] = None
        handle["saw_assistant_text"] = False
        handle["assistant_text"] = ""
        handle["tool_call_args"] = {}
        yield self.emit(build_event(
            AgentEventType.TURN_STARTED, seq,
            external_session_id=external_id, turn_id=turn_id,
            native_type="omp.prompt.start",
            payload=TurnStartedPayload(kind=input.kind),
            **common))

        queue: asyncio.Queue = asyncio.Queue()
        handle["event_sink"] = queue
        # 上一 turn 结束后才 settle 的子智能体：补播最新 node 快照。
        dirty = handle.get("agent_dirty") or set()
        if dirty:
            handle["agent_dirty"] = set()
            nodes = handle.get("agent_nodes") or {}
            agents = [dict(nodes[a]) for a in sorted(dirty) if a in nodes]
            if agents:
                queue.put_nowait((AgentEventType.AGENT_UPDATED,
                                  "omp.subagent.flush",
                                  dump_payload(AgentUpdatedPayload(
                                      agents=[AgentNodePayload(**node)
                                              for node in agents],
                                      patch=True))))

        async def run_prompt() -> Optional[Exception]:
            try:
                # prompt 响应只是 ack（data.agentInvoked）；完成由
                # agent_end/agent_settled（isTerminal!==false）判定。
                response = await self._cmd(peer, "prompt", {"message": input.text},
                                timeout=self.conversation_turn_timeout(
                                    handle.get("conversation_thread_id"),
                                    self._prompt_timeout))
                if response.get("agentInvoked") is False:
                    queue.put_nowait(("__turn_done__", "omp.command.completed", {
                        "local": True,
                        "prompt_response": response,
                    }))
                return None
            except Exception as exc:  # noqa: BLE001
                return exc

        task = asyncio.ensure_future(run_prompt())
        ack_error: Optional[Exception] = None
        ack_handled = False
        done_info: dict[str, Any] = {}
        get_task = asyncio.ensure_future(queue.get())
        try:
            while True:
                done, _pending = await asyncio.wait(
                    {get_task} if ack_handled else {task, get_task}, return_when=asyncio.FIRST_COMPLETED)
                if task in done:
                    ack_handled = True
                    exc = task.result()
                    if isinstance(exc, Exception):
                        ack_error = exc
                        break
                if get_task in done:
                    etype, native_type, payload = get_task.result()
                    get_task = asyncio.ensure_future(queue.get())
                    if etype == "__turn_done__":
                        done_info = dict(payload)
                        break
                    yield self.emit(build_event(
                        etype, seq,
                        external_session_id=external_id, turn_id=turn_id,
                        native_type=native_type, payload=payload,
                        **common))
        finally:
            get_task.cancel()
        if not task.done():
            task.cancel()
        host_tool_tasks = tuple(handle.get("host_tool_tasks") or ())
        if host_tool_tasks:
            await asyncio.gather(*host_tool_tasks, return_exceptions=True)
            while not queue.empty():
                etype, native_type, payload = queue.get_nowait()
                if etype == "__turn_done__":
                    continue
                yield self.emit(build_event(
                    etype, seq,
                    external_session_id=external_id, turn_id=turn_id,
                    native_type=native_type, payload=payload,
                    **common))
        handle["event_sink"] = None
        handle["turns"] += 1
        turn_failed = handle.pop("turn_failed", None)
        saw_assistant_text = bool(handle.pop("saw_assistant_text", False))
        handle["current_turn_id"] = None

        if ack_error is not None:
            yield self.emit(build_event(
                AgentEventType.TURN_FAILED, seq,
                external_session_id=external_id, turn_id=turn_id,
                native_type="omp.prompt.error",
                payload=TurnFailedPayload(error=self.exception_failure(
                    ack_error, FailureCategory.TRANSPORT, "prompt_error",
                    message=f"OMP prompt failed: {ack_error}")),
                **common))
            return
        if turn_failed:
            yield self.emit(build_event(
                AgentEventType.TURN_FAILED, seq,
                external_session_id=external_id, turn_id=turn_id,
                native_type="omp.assistant_error",
                payload=TurnFailedPayload(error=self.failure(
                    FailureCategory.PROVIDER, "assistant_error",
                    message=str(turn_failed), detail=str(turn_failed))),
                **common))
            return
        assistant_text = str(handle.pop("assistant_text", ""))
        for event in self.completed_turn_events(
            seq, text=assistant_text,
            common={**common, "external_session_id": external_id, "turn_id": turn_id},
            native_type="omp.agent_end",
            payload=TurnCompletedPayload(native=dict(done_info))):
            yield event

    def resume(self, session: AgentSessionRef) -> AsyncIterator[AgentEvent]:
        return self._resume_stream(session)

    async def _resume_stream(
        self, session: AgentSessionRef
    ) -> AsyncIterator[AgentEvent]:
        sid = session.agent_session_id
        seq = self.sequencer_for(sid)
        handle = self._rpc.get(sid)
        if handle is None:
            async for event in self._unsupported_stream(session, "resume",
                                                        "resume"):
                yield event
            return
        record = self._tracker.get(sid)
        yield self.emit(build_event(
            AgentEventType.SESSION_RESUMED, seq,
            external_session_id=handle.get("external_session_id"),
            native_type="omp.session.resumed",
            payload=SessionPayload(
                transport="rpc-ui" if self._rpc_ui else "rpc",
                native={"resume_handle": handle.get("resume_handle")}),
            agent_session_id=sid,
            run_id=record.run_id if record else None,
            execution_generation=(
                record.execution_generation if record else None),
        ))

    # -- 控制面 ---------------------------------------------------------------

    def _receipt(self, session: AgentSessionRef, ok: bool,
                 detail: str = "") -> CommandReceipt:
        if ok:
            return CommandReceipt(
                command_id=new_id("cmd"),
                state=ReceiptState.COMPLETED,
                aggregate=AggregateRef(type="agent_session",
                                       id=session.agent_session_id),
            )
        return self.unsupported_receipt(
            "command", "rpc_failed", session=session,
            detail={"detail": detail})

    async def steer(
        self, session: AgentSessionRef, input: AgentInput
    ) -> CommandReceipt:
        if not isinstance(input, SteerInput):
            return self.unsupported_receipt(
                "steer", "steer", session=session, detail={"input_kind": input.kind})
        handle = self._rpc.get(session.agent_session_id)
        if handle is None:
            return self.unsupported_receipt("steer", "no_active_session",
                                            session=session)
        try:
            await self._cmd(handle["peer"], "steer",
                            {"message": input.text})
            return self._receipt(session, True)
        except Exception as exc:  # noqa: BLE001
            return self._receipt(session, False, str(exc))

    async def interrupt(self, session: AgentSessionRef) -> CommandReceipt:
        self._mark_turn_interrupted(session.agent_session_id)
        handle = self._rpc.get(session.agent_session_id)
        if handle is None:
            return self.unsupported_receipt("interrupt", "no_active_session",
                                            session=session)
        try:
            await self._cmd(handle["peer"], "abort")
            return self._receipt(session, True)
        except Exception as exc:  # noqa: BLE001
            return self._receipt(session, False, str(exc))

    async def runtime_operation(self, session: AgentSessionRef, name: str, arguments: str = "") -> dict[str, Any]:
        from .rpc_commands import rpc_operation
        handle = self._rpc.get(session.agent_session_id)
        if not handle or handle.get("current_turn_id"):
            raise RuntimeError("请等待当前回复结束后执行命令")
        return await rpc_operation(
            self._cmd, handle["peer"], adapter_id=self.id, engine=self.engine,
            name=name, arguments=arguments)

    async def runtime_capability_snapshot(
        self, session: Optional[AgentSessionRef] = None
    ) -> RuntimeCapabilitySnapshot:
        base = await super().runtime_capability_snapshot(session)
        if session is None:
            base.stale = True
            base.diagnostics.append(
                "OMP 命令目录属于具体 RPC Session，当前没有活动会话")
            return base
        handle = self._rpc.get(session.agent_session_id)
        if handle is None or handle.get("peer") is None:
            base.stale = True
            base.diagnostics.append("OMP RPC Session 当前不在本进程")
            return base
        try:
            result = await self._cmd(
                handle["peer"], "get_available_commands",
                timeout=self._startup_timeout,
            )
            commands = list(result.get("commands") or [])
            handle["available_commands"] = commands
        except Exception as exc:  # noqa: BLE001
            commands = list(handle.get("available_commands") or [])
            base.stale = True
            base.diagnostics.append(
                "OMP get_available_commands 读取失败："
                f"{type(exc).__name__}: {exc}"
            )
        handle["capability_revision"] = int(
            handle.get("capability_revision") or 0
        ) + 1
        items = list(base.items)
        from .rpc_commands import rpc_operation_items
        native_names = {str(raw.get("name") or "").lstrip("/") for raw in commands if isinstance(raw, dict)}
        operations = [item for item in rpc_operation_items(self.id, "omp")
                      if item.name == "compact" or item.name not in native_names]
        items.extend(operations)
        operation_names = {item.name for item in operations}
        for raw in commands:
            if not isinstance(raw, dict):
                continue
            name = str(raw.get("name") or "").strip().lstrip("/")
            if not name or name in operation_names:
                continue
            source = str(raw.get("source") or "")
            items.append(dynamic_command_item(
                adapter_id=self.id,
                engine="omp",
                name=name,
                description=str(raw.get("description") or ""),
                argument_hint=str((raw.get("input") or {}).get("hint") or raw.get("argumentHint") or ""),
                channel="provider_native",
                kind="skill" if source.casefold() == "skill"
                or name.startswith("skill:") else "command",
                invocation={
                    "command": name,
                    "wire_text": f"/{name}",
                    "protocol": "omp.rpc.prompt",
                },
            ))
            for alias in raw.get("aliases") or []:
                if alias not in operation_names and alias != name:
                    items.append(items[-1].model_copy(update={
                        "id": f"runtime:{self.id}:command:{alias}", "name": str(alias),
                        "invocation": {**items[-1].invocation, "wire_text": f"/{alias}"},
                    }))
        base.items = items
        base.revision = int(handle["capability_revision"])
        base.external_session_id = handle.get("external_session_id")
        return base

    async def _teardown(self, session: AgentSessionRef) -> str:
        handle = self._rpc.pop(session.agent_session_id, None)
        if not handle:
            return "closed"
        peer: StdioJsonlPeer = handle["peer"]
        returncode = await peer.close()
        return classify_exit(
            returncode=returncode,
            cancelled=handle.get("current_turn_id") is not None,
            resume_handle=handle.get("resume_handle")
            or handle.get("external_session_id"),
        )


class OmpAcpAdapter(BaseAcpAdapter):
    """OMP 的 ACP 兼容传输（``omp acp``，通用客户端路径）。

    核验（§OMP-1/7）：``omp acp`` 子命令真实存在；``session/new`` /
    ``session/load`` / ``session/resume`` 均接受 ``mcpServers`` 并要求
    **绝对路径** ``cwd``（``#assertAbsoluteCwd``）；写操作经
    ``session/request_permission`` 门控。session/stream/approval/resume/
    interrupt 全部由 ``BaseAcpAdapter`` 实现。
    """

    adapter_id = "omp.acp"
    unsupported_access_mode_reasons = {
        AccessMode.AUTO.value: (
            "OMP --approval-mode offers only always-ask, write and yolo; "
            "there is no native auto policy"
        ),
    }

    def __init__(self, *, binary: Optional[str] = None,
                 **kwargs: Any) -> None:
        super().__init__(**kwargs)
        self._binary = binary or os.environ.get("MUTEKI_OMP_BIN", "omp")

    def _agent_argv(self) -> list[str]:
        return [self._binary, "acp"]

    def _agent_argv_for_request(self, request: SessionStart) -> list[str]:
        mode = request.access_mode or AccessMode.SUPERVISED.value
        argv = [self._binary]
        if mode == AccessMode.SUPERVISED.value:
            argv += ["--approval-mode", "always-ask"]
        elif mode == AccessMode.AUTO_ACCEPT_EDITS.value:
            argv += ["--approval-mode", "write"]
        elif mode == AccessMode.FULL_ACCESS.value:
            argv += ["--approval-mode", "yolo"]
        else:
            raise ValueError(f"omp.acp has no native approval mode for {mode!r}")
        env = {**os.environ, **self._env_extra, **dict(request.options.env)}
        provider, model = OmpRpcAdapter._resolve_provider_model(request, env)
        if provider:
            argv += ["--provider", provider]
        if model:
            argv += ["--model", model]
        argv.append("acp")
        return argv

    async def _request_permission(
        self, agent_session_id: str, params: dict[str, Any]
    ) -> Optional[str]:
        """Avoid OMP's duplicate gate while preserving its native approval.

        In ``always-ask`` and ``write`` OMP first asks through its ACP client
        gate for a subset of tools, then applies its own approval policy via a
        form elicitation.  The form is the complete native decision (including
        ``write``), so the preliminary gate is acknowledged once and only the
        native form is shown to the user.
        """
        handle = self._handle_for(agent_session_id)
        mode = str((handle or {}).get("access_mode") or "")
        if mode in {
            AccessMode.SUPERVISED.value,
            AccessMode.AUTO_ACCEPT_EDITS.value,
        }:
            options = [
                dict(option) for option in (params.get("options") or [])
                if isinstance(option, dict)
            ]
            return self._permission_option(
                options, ("allow_once", "allow_always"))
        return await super()._request_permission(agent_session_id, params)

    # -- 子智能体（task 工具，经 ACP tool_call 的 rawInput/rawOutput 识别） ------
    #
    # ACP 模式没有 RPC 的 subagent_* 帧；结构信号只有两处（本机 17.2.12
    # 实测）：task 调用的 rawInput 带 task/tasks 参数（OMP 内置工具中唯
    # 一），tool_call_update 的 rawOutput.details 是 TaskToolDetails。
    # 异步派单时工具调用以 completed 提前收尾（仅表示派单被接受），子智能体
    # 的真实进展由后续 in_progress 更新的 details.progress/async.state 携带。

    def _delegation_info(self, update: dict[str, Any]) -> Optional[dict[str, Any]]:
        items = _omp_task_items(update.get("rawInput"))
        if not items:
            return None
        first = items[0]
        desc: dict[str, Any] = {
            "title": str(first.get("name") or update.get("title") or "task"),
            "request": str(first.get("task") or "") or None,
            "role": str(first.get("agent") or "") or None,
        }
        if len(items) == 1 and first.get("name"):
            # 显式 name 即子智能体 id（实测 lifecycle/progress 的 id 一致）；
            # 匿名/批量条目的真实 id 由 progress 帧公布后再建 node。
            desc["agent_id"] = str(first["name"])
        return desc

    def _delegation_result(
        self, handle: dict[str, Any], update: dict[str, Any],
        desc: dict[str, Any],
    ) -> Optional[dict[str, Any]]:
        details = _omp_task_details(update.get("rawOutput"))
        if details is None:
            return None
        async_state = details.get("async")
        if isinstance(async_state, dict) and async_state.get("state") == "running" \
                and not details.get("results"):
            if str(update.get("status") or "") == "completed":
                # 异步派单回执：completed 只表示 spawn 被接受，不能把 node
                # 落锤；同时清掉基类会写入的派单回执文本（非子智能体结果）。
                return {"status": "running", "result": None}
            return None
        # 同步收尾：_tool_meta_nodes 已按 results[] 写好 node，这里防止基类
        # 用整段 content 文本覆盖 per-item 的 result/error。
        target = str(desc.get("agent_id") or "")
        node = (handle.get("agent_nodes") or {}).get(target) or {}
        link: dict[str, Any] = {}
        if node.get("result"):
            link["result"] = node["result"]
        if node.get("error"):
            link["error"] = node["error"]
        return link or None

    def _tool_meta_nodes(
        self, handle: dict[str, Any], update: dict[str, Any]
    ) -> list[dict[str, Any]]:
        details = _omp_task_details(update.get("rawOutput"))
        if details is None:
            return []
        status = str(update.get("status") or "")
        async_state = details.get("async")
        results = [r for r in details.get("results") or []
                   if isinstance(r, dict)]
        if status in ("completed", "failed") and not results and (
                isinstance(async_state, dict)
                and async_state.get("state") == "running"):
            # 异步派单回执不是子智能体进展；让 tool 行正常收尾。
            return []
        nodes = handle.get("agent_nodes") or {}
        patches: list[dict[str, Any]] = []
        progress: dict[str, dict[str, Any]] = {}
        for entry in details.get("progress") or []:
            if isinstance(entry, dict) and entry.get("id"):
                progress[str(entry["id"])] = entry
        for agent_id, entry in progress.items():
            patch = _omp_progress_update(entry, nodes.get(agent_id))
            patch["agent_id"] = agent_id
            assignment = str(
                entry.get("assignment") or entry.get("task") or "").strip()
            if assignment:
                patch["request"] = assignment
            if entry.get("agent"):
                patch["role"] = str(entry["agent"])
            patches.append(patch)
        for entry in results:
            rid = str(entry.get("id") or "")
            if rid:
                patches.append(
                    _omp_result_update(rid, entry, progress.get(rid)))
        return patches

    def _dispatch_update(
        self, agent_session_id: str, session_id: str,
        update: dict[str, Any], replay: bool,
    ) -> None:
        super()._dispatch_update(agent_session_id, session_id, update, replay)
        if replay:
            return
        handle = self._handle_for(agent_session_id)
        if handle is None:
            return
        if str(update.get("sessionUpdate") or "") != "tool_call_update":
            return
        if str(update.get("status") or "") not in ("completed", "failed"):
            return
        # hub wait/jobs 的 settle 快照是异步子智能体结果的正式投递通道
        # （§OMP task 异步契约）；只回填 node 缺失的字段，不覆盖 yield 结果。
        events: list[tuple[Any, str, dict[str, Any]]] = []
        for patch in self._hub_job_patches(handle, update):
            agent = str(patch.pop("agent_id", "") or "")
            if agent:
                event = self._patch_agent_node(handle, agent, patch)
                if event is not None:
                    events.append(event)
        self._emit_handle_events(handle, events)

    @staticmethod
    def _hub_job_patches(
        handle: dict[str, Any], update: dict[str, Any]
    ) -> list[dict[str, Any]]:
        raw_output = update.get("rawOutput")
        details = raw_output.get("details") if isinstance(raw_output, dict) else None
        jobs = details.get("jobs") if isinstance(details, dict) else None
        if not isinstance(jobs, list):
            return []
        nodes = handle.get("agent_nodes") or {}
        patches: list[dict[str, Any]] = []
        for job in jobs:
            if not isinstance(job, dict) or job.get("type") != "task":
                continue
            agent_id = str(job.get("id") or "")
            if not agent_id:
                continue
            mapped = _OMP_AGENT_STATUS.get(str(job.get("status") or ""))
            if mapped not in _OMP_TERMINAL_STATUS:
                continue
            node = nodes.get(agent_id) or {}
            patch: dict[str, Any] = {"agent_id": agent_id}
            if node.get("status") not in _OMP_TERMINAL_STATUS:
                patch["status"] = mapped
            text = str(job.get("resultText") or "")
            field = "result" if mapped == "completed" else "error"
            if text and not node.get(field):
                patch[field] = text
            if job.get("durationMs") is not None and node.get("duration_ms") is None:
                patch["duration_ms"] = job.get("durationMs")
            if job.get("resolvedModel") and not node.get("model"):
                patch["model"] = str(job["resolvedModel"])
            if len(patch) > 1:
                patches.append(patch)
        return patches

    def _probe_extra_caps(self, caps: AgentCapabilities, hello: Any) -> None:
        caps.supported_models = []  # ACP v1 无模型枚举方法，保持空（static）
        # 原生 task 子智能体：经 tool_call rawInput/rawOutput 的
        # TaskToolDetails 映射（本机 17.2.12 ACP 实测）。
        caps.subagents = True


__all__ = [
    "OmpAcpAdapter",
    "OmpRpcAdapter",
]
