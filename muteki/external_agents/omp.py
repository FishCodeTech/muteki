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
    ProbeRequest,
    SessionStart,
)
from muteki.platform.contracts.receipts import (
    AggregateRef,
    CommandReceipt,
    ReceiptState,
)

from .acp import BaseAcpAdapter
from .base import BaseExternalAgentAdapter
from .capabilities import (
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


class OmpRpcAdapter(BaseExternalAgentAdapter):
    """OMP 的 NDJSON RPC 结构化 Adapter（``--mode rpc`` / ``rpc-ui``）。"""

    adapter_id = "omp.rpc"

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
        argv = [self._binary, "--mode", "rpc-ui" if self._rpc_ui else "rpc"]
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
                f"omp rpc {command} failed: {str(resp.get('error'))[:200]}")
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
                detail = f"omp rpc 探测失败：{str(exc)[:160]}"
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
        caps.session_persistence = True
        caps.user_input = self._rpc_ui  # extension_ui_request 子协议
        caps.structured_http_rpc = True  # set_host_tools 反向工具（§OMP-10）
        for field_name in BOOL_CAPABILITY_FIELDS:
            field_sources[field_name] = SOURCE_PROBE
        if not self._rpc_ui:
            field_sources["user_input"] = SOURCE_STATIC
            degradations.append(
                "user_input：extension_ui_request 子协议仅 --mode rpc-ui "
                "下出站；本实例为 rpc 模式，未声明")
        versions = ready.get("supportedProtocolVersions") or []
        if 2 in versions and not v2_negotiated:
            degradations.append(
                "对端声明 RPC v2，但协商未成功；当前会话继续使用 v1")
        elif 2 not in versions:
            degradations.append(
                f"对端仅支持 RPC v1（物理帧上限 "
                f"{ready.get('maxFrameBytes', '?')} 字节）")
        degradations.append(
            "approval：RPC 无非阻塞带内审批，工具审批策略由启动参数固化，"
            "未静默关闭")
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
            result_payload = {"error": {"message": str(exc)[:200]}}

        rpc_error = result_payload.get("error")
        gateway_result = result_payload.get("result", result_payload)
        is_error = bool(rpc_error)
        if isinstance(gateway_result, dict) and gateway_result.get("ok") is False:
            is_error = True
        visible_result = (
            {"error": rpc_error} if rpc_error is not None else gateway_result
        )
        agent_tool_result = {
            "content": [{
                "type": "text",
                "text": json.dumps(
                    visible_result, ensure_ascii=False, default=str),
            }],
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
                             "omp.host_tool_call", {
                                 "call_id": str(call_id or ""),
                                 "tool": tool_name,
                                 "host_tool": True,
                                 "is_error": is_error,
                             }))

    # -- 启动 / 接管 -------------------------------------------------------------

    async def _launch(
        self,
        request: SessionStart,
        plan: Optional[CapabilityInjectionPlan],
        bearer_token: Optional[str],
    ) -> dict[str, Any]:
        cwd = str(request.options.get("cwd") or os.getcwd())
        options = request.options
        process_env = {
            **self._default_env,
            **{k: str(v) for k, v in (options.get("env") or {}).items()},
        }
        provider, model = self._resolve_provider_model(request, process_env)
        handle: dict[str, Any] = {
            "conversation_thread_id": request.thread_id,
            "cwd": cwd,
            "options": dict(options),
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
                role=str(options.get("role") or "") or None,
                add_dirs=[str(p) for p in (options.get("add_dirs") or [])],
                no_session=bool(options.get("ephemeral")),
                session_dir=str(options.get("session_dir") or "") or None,
            ),
            cwd=cwd, label="omp.rpc",
            env=process_env,
            request_envelope=_omp_envelope, on_message=on_message)
        handle["peer"] = peer
        self._rpc[request.agent_session_id] = handle
        await peer.start()
        try:
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
            handle["host_tools"] = await self._inject_host_tools(
                peer, handle, plan, bearer_token)
            catalog = await self._cmd(
                peer, "get_available_commands", timeout=self._startup_timeout)
            handle["available_commands"] = list(catalog.get("commands") or [])
            handle["capability_revision"] = 1
        except Exception:
            self._rpc.pop(request.agent_session_id, None)
            await peer.close()
            raise

        session_id = str(state.get("sessionId") or "")
        handle["external_session_id"] = session_id or None
        return {
            "external_session_id": session_id or None,
            "resume_handle": request.resume_handle or session_id or None,
        }

    # -- 交互请求自动应答（rpc-ui） ---------------------------------------------------

    def _auto_answer_ui(self, handle: dict[str, Any], msg: dict[str, Any]) -> None:
        """``extension_ui_request`` → 无人值守默认 ``cancelled`` 应答。

        对话类请求带 timeout（超时自动以默认值 resolve），但无人值守
        Worker 立即应答可避免空等；先出 USER_INPUT_REQUESTED 事件，
        不静默吞掉。
        """
        request_id = msg.get("id")
        method = str(msg.get("method") or "")
        sink = handle.get("event_sink")
        if sink is not None:
            sink.put_nowait((AgentEventType.USER_INPUT_REQUESTED,
                             "omp.extension_ui_request", {
                                 "request_id": str(request_id or ""),
                                 "ui_method": method,
                             }))

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
                                 "omp.extension_ui_response", {
                                     "request_id": str(request_id or ""),
                                     "resolution": "cancelled",
                                 }))
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
                    {
                        "revision": handle["capability_revision"],
                        "adapter_id": self.id,
                        "commands": list(handle["available_commands"]),
                    },
                ))
            return
        sink = handle.get("event_sink")
        if sink is None:
            return
        if etype == "message_update":
            usage = msg.get("usage")
            if isinstance(usage, dict) and usage:
                sink.put_nowait((AgentEventType.USAGE_UPDATED,
                                 "omp.message_update", {"usage": usage}))
            delta = msg.get("assistantMessageEvent") or {}
            if delta.get("type") == "text_delta" and delta.get("delta"):
                sink.put_nowait((AgentEventType.MESSAGE_DELTA,
                                 "omp.text_delta",
                                 {"text": str(delta["delta"]),
                                  "content_index": delta.get("contentIndex")}))
                handle["saw_assistant_text"] = True
        elif etype == "message_end":
            message = msg.get("message") or {}
            if not isinstance(message, dict) or message.get("role") != "assistant":
                return
            text = _omp_message_text(message)
            usage = message.get("usage")
            if isinstance(usage, dict) and usage:
                sink.put_nowait((AgentEventType.USAGE_UPDATED,
                                 "omp.message_end", {"usage": usage}))
            error_message = str(message.get("errorMessage") or "").strip()
            stop_reason = str(message.get("stopReason") or "").strip()
            if error_message or stop_reason == "error":
                detail = error_message or f"stopReason={stop_reason or 'error'}"
                handle["turn_failed"] = detail
                sink.put_nowait((AgentEventType.RUNTIME_ERROR,
                                 "omp.message_end", {
                                     "code": "omp.assistant_error",
                                     "detail": detail,
                                     "error": detail,
                                     "stop_reason": stop_reason,
                                 }))
                return
            if text:
                handle["assistant_text"] = text
                handle["saw_assistant_text"] = True
        elif etype == "tool_execution_start":
            sink.put_nowait((AgentEventType.TOOL_STARTED,
                             "omp.tool_execution_start", {
                                 "call_id": str(msg.get("toolCallId") or ""),
                                 "tool": str(msg.get("toolName") or ""),
                                 "input": msg.get("args"),
                             }))
        elif etype == "tool_execution_end":
            sink.put_nowait((AgentEventType.TOOL_COMPLETED,
                             "omp.tool_execution_end", {
                                 "call_id": str(msg.get("toolCallId") or ""),
                                 "output": msg.get("result"),
                                 "is_error": bool(msg.get("isError")),
                             }))
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
                                 {"local": True}))
        # turn_start/compaction_*/queue_update 等其余事件不改变核心状态机。

    # -- turn 流 --------------------------------------------------------------

    def send(
        self, session: AgentSessionRef, input: AgentInput
    ) -> AsyncIterator[AgentEvent]:
        return self._turn_stream(session, input)

    async def _turn_stream(
        self, session: AgentSessionRef, input: AgentInput
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
                payload={"transport": "rpc-ui" if self._rpc_ui else "rpc",
                         "adapter_id": self.id,
                         "instance_id": self.identity.instance_id,
                         "cwd": handle["cwd"],
                         "host_tools": handle.get("host_tools", 0),
                         "protocol_version": (handle.get("ready") or {}).get(
                             "protocolVersion")},
                **common))
        turn_id = new_id("turn")
        handle["current_turn_id"] = turn_id
        handle["turn_failed"] = None
        handle["saw_assistant_text"] = False
        handle["assistant_text"] = ""
        yield self.emit(build_event(
            AgentEventType.TURN_STARTED, seq,
            external_session_id=external_id, turn_id=turn_id,
            native_type="omp.prompt.start",
            payload={"kind": input.kind},
            **common))

        queue: asyncio.Queue = asyncio.Queue()
        handle["event_sink"] = queue

        async def run_prompt() -> Optional[Exception]:
            try:
                # prompt 响应只是 ack（data.agentInvoked）；完成由
                # agent_end/agent_settled（isTerminal!==false）判定。
                await self._cmd(peer, "prompt", {"message": input.text},
                                timeout=self.conversation_turn_timeout(
                                    handle.get("conversation_thread_id"),
                                    self._prompt_timeout))
                return None
            except Exception as exc:  # noqa: BLE001
                return exc

        task = asyncio.ensure_future(run_prompt())
        ack_error: Optional[Exception] = None
        done_info: dict[str, Any] = {}
        get_task = asyncio.ensure_future(queue.get())
        try:
            while True:
                done, _pending = await asyncio.wait(
                    {task, get_task}, return_when=asyncio.FIRST_COMPLETED)
                if task in done:
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
                payload={"error": str(ack_error)[:300]},
                **common))
            return
        if turn_failed:
            yield self.emit(build_event(
                AgentEventType.TURN_FAILED, seq,
                external_session_id=external_id, turn_id=turn_id,
                native_type="omp.assistant_error",
                payload={
                    "error": {
                        "code": "omp.assistant_error",
                        "message": str(turn_failed)[:500],
                    },
                },
                **common))
            return
        assistant_text = str(handle.pop("assistant_text", ""))
        if not saw_assistant_text or not assistant_text.strip():
            yield self.emit(build_event(
                AgentEventType.TURN_FAILED, seq,
                external_session_id=external_id, turn_id=turn_id,
                native_type="omp.empty_assistant",
                payload={
                    "error": {
                        "code": "omp.empty_assistant",
                        "message": "OMP turn ended without assistant text",
                    },
                },
                **common))
            return
        yield self.emit(build_event(
            AgentEventType.MESSAGE_COMPLETED, seq,
            external_session_id=external_id, turn_id=turn_id,
            native_type="omp.message.completed",
            payload={"text": assistant_text, "role": "assistant"},
            **common))
        yield self.emit(build_event(
            AgentEventType.TURN_COMPLETED, seq,
            external_session_id=external_id, turn_id=turn_id,
            native_type="omp.agent_end",
            payload=done_info,
            **common))

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
            payload={"transport": "rpc-ui" if self._rpc_ui else "rpc",
                     "resume_handle": handle.get("resume_handle")},
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
            detail={"detail": detail[:200]})

    async def steer(
        self, session: AgentSessionRef, input: AgentInput
    ) -> CommandReceipt:
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
        handle = self._rpc.get(session.agent_session_id)
        if handle is None:
            return self.unsupported_receipt("interrupt", "no_active_session",
                                            session=session)
        try:
            await self._cmd(handle["peer"], "abort")
            return self._receipt(session, True)
        except Exception as exc:  # noqa: BLE001
            return self._receipt(session, False, str(exc))

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
                f"{type(exc).__name__}: {str(exc)[:160]}"
            )
        handle["capability_revision"] = int(
            handle.get("capability_revision") or 0
        ) + 1
        items = list(base.items)
        for raw in commands:
            if not isinstance(raw, dict):
                continue
            name = str(raw.get("name") or "").strip().lstrip("/")
            if not name:
                continue
            source = str(raw.get("source") or "")
            items.append(dynamic_command_item(
                adapter_id=self.id,
                engine="omp",
                name=name,
                description=str(raw.get("description") or ""),
                channel="provider_native",
                kind="skill" if source.casefold() == "skill"
                or name.startswith("skill:") else "command",
                invocation={
                    "command": name,
                    "wire_text": f"/{name}",
                    "protocol": "omp.rpc.prompt",
                },
            ))
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

    def __init__(self, *, binary: Optional[str] = None,
                 **kwargs: Any) -> None:
        super().__init__(**kwargs)
        self._binary = binary or os.environ.get("MUTEKI_OMP_BIN", "omp")

    def _agent_argv(self) -> list[str]:
        return [self._binary, "acp"]

    def _agent_argv_for_request(self, request: SessionStart) -> list[str]:
        mode = request.access_mode or AccessMode.SUPERVISED.value
        argv = [self._binary]
        # OMP owns these modes.  ``auto`` intentionally passes no flag so the
        # user's own OMP default/config remains authoritative.
        if mode == AccessMode.SUPERVISED.value:
            argv += ["--approval-mode", "always-ask"]
        elif mode == AccessMode.AUTO_ACCEPT_EDITS.value:
            argv += ["--approval-mode", "write"]
        elif mode == AccessMode.FULL_ACCESS.value:
            argv += ["--approval-mode", "yolo"]
        if request.model:
            argv += ["--model", str(request.model)]
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

    def _probe_extra_caps(self, caps: AgentCapabilities, hello: Any) -> None:
        caps.supported_models = []  # ACP v1 无模型枚举方法，保持空（static）


__all__ = [
    "OmpAcpAdapter",
    "OmpRpcAdapter",
]
