"""Pi JSONL RPC Adapter（RUNTIME-03，任务书 7.2/7.6）。

正式接入：``pi --mode rpc``（JSONL RPC）。核验结论
（docs/research/third_party_verification.md §PI，pi-coding-agent 0.84.x）：

- stdin 收命令（每行一个 JSON，``{id, type, ...}``），stdout 出
  ``{id, type:"response", command, success, data|error}`` 响应与事件流；
  严格 LF 分帧（``rpc.py`` 的 ``JsonLineFramer``）；
- 命令面：``prompt {message, streamingBehavior?}``、``steer``、``abort``、
  ``new_session {parentSession?}``、``switch_session {sessionPath}``、
  ``get_state``、``get_messages``、``set_model {provider, modelId}``（两个
  字段必须成对）、``get_available_models``、``get_session_stats``、
  ``get_entries {since?}``（entry id 为跨重启 durable cursor）等；
- **完成判定用 ``agent_end`` / ``agent_settled`` 事件**——``prompt`` 的
  响应 ``success:true`` 只表示命令被接受/排队，不是 turn 完成信号；
- usage：``message_update`` 顶层带累计 ``usage``；``message_end.message``
  为权威消息（delta 需按 ``contentIndex`` 自行拼装）；
- 能力注入按 probe 选择：Pi 0.84 未确认原生 MCP 配置入口；Adapter 从
  Agent Plugins 1.0.0 包的 ``io.github.fishcodetech.pi`` 客户端扩展加载
  原生 ``muteki_*`` 工具。可移植 Skill 与 MCP 仍由同一个标准包提供；
  Pi 不再把 Skill 当作脚本入口执行。
- **trust / ``--approve``（§PI 影响⑤）**：``--approve``/``-a`` 是 pi ≥0.84
  的项目信任覆盖（非交互下让 Pi 加载项目级资源）。**pi 0.73.1 不认识该
  选项**，传入会直接 ``Unknown option: --approve`` 退出。本 Adapter 目标
  运行时是 0.73.1：不传 ``--approve``；Agent Plugin 能力经显式
  ``--extension`` 注入（配合 ``--no-extensions`` 等），不依赖项目信任门闩。
  ``trust_project`` 保留为兼容开关，当前为 no-op。

兼容路径：``pi -p --mode json`` 由 ``muteki.solver.cli_driver.PiDriver`` +
``CliDriverAdapter``（``cli.pi``）承担；本模块不 import solver 层。
"""

from __future__ import annotations

import asyncio
import json
import os
from pathlib import Path
from typing import Any, AsyncIterator, Optional

from muteki.capability_bindings.agent_plugin import (
    ENV_ENDPOINT,
    ENV_TOKEN,
    materialize_runtime,
    package_root,
    resolve_client_extension_entry,
)
from muteki.platform.contracts.base import new_id
from muteki.platform.contracts.capabilities import (
    CapabilityInjectionPlan,
    InjectionKind,
)
from muteki.platform.contracts.external_agents import (
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

from .base import BaseExternalAgentAdapter
from .attachment_input import pi_prompt_images
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
from .sessions import EXIT_INTERRUPTED, classify_exit

PI_EXTENSION_NAMESPACE = "io.github.fishcodetech.pi"
PI_EXTENSION_ENTRY = "extension.ts"
PI_TOOLS_FILE_ENV = "MUTEKI_CAPABILITY_TOOLS_FILE"


def _pi_envelope(msg_id: str, method: str,
                 params: Optional[dict[str, Any]]) -> dict[str, Any]:
    """Pi RPC 请求形态：``{id, type, ...params}``。"""
    return {"id": msg_id, "type": method, **(params or {})}



def _pi_is_user_abort(detail: str, *, abort_requested: bool = False) -> bool:
    """True when operator abort (RPC ``abort``) ended the turn.

    Pi surfaces user interrupt as assistant ``errorMessage`` like
    ``Request was aborted.`` / tool ``Command aborted``, not a dedicated
    interrupted event. Classify those as EXIT_INTERRUPTED so Conversation
    emits ``core.turn.interrupted`` instead of ``core.turn.failed``.
    """
    if abort_requested:
        return True
    lowered = str(detail or "").casefold()
    if not lowered:
        return False
    # Prefer specific phrases; bare "abort" only when it is the whole token-ish msg.
    if "request was aborted" in lowered or "command aborted" in lowered:
        return True
    if "aborted by user" in lowered:
        return True
    if lowered.strip() in {"aborted", "abort", "cancelled", "canceled", "interrupted"}:
        return True
    return False


def _pi_message_text(message: dict[str, Any]) -> str:
    """拼接 message.content 中的文本块（与 CLI PiDriver 一致）。"""
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


class PiAdapter(BaseExternalAgentAdapter):
    """Pi 的 JSONL RPC 结构化 Adapter。

    覆盖任务书 RUNTIME-03 要求的 session、send、steer、interrupt、
    model、extension、usage：

    - ``send``：``prompt`` 命令 + 事件流，turn 完成以 ``agent_end`` /
      ``agent_settled`` 判定；
    - ``steer``：``steer`` 命令（带内转向，CLI 兼容路径没有的能力）；
    - ``interrupt``：``abort`` 命令；
    - ``resume``：``switch_session`` 接管持久化 session 文件，恢复后用
      ``get_entries`` durable cursor 记录水位，重连不重复消费历史；
    - model：``SessionStart.model`` 为 ``provider/modelId`` 时经
      ``set_model`` 双字段切换；
    - Agent Plugin：从标准包的反向域名客户端扩展加载原生工具；模型无需
      读取 Skill 文件或执行 ``muteki_client.py``。
    """

    adapter_id = "pi.rpc"

    def __init__(
        self,
        *,
        binary: Optional[str] = None,
        session_dir: Optional[str] = None,
        runtime_root: Optional[str | Path] = None,
        default_env: Optional[dict[str, str]] = None,
        trust_project: bool = True,
        startup_timeout: float = 30.0,
        prompt_timeout: float = 600.0,
        **kwargs: Any,
    ) -> None:
        super().__init__(self.adapter_id, **kwargs)
        self._binary = binary or os.environ.get("MUTEKI_PI_BIN", "pi")
        self._session_dir = session_dir
        self._runtime_root = Path(
            runtime_root or "state/_pi_runtime").expanduser().resolve()
        self._default_env = dict(default_env or {})
        self._trust_project = trust_project
        self._startup_timeout = float(startup_timeout)
        self._prompt_timeout = float(prompt_timeout)
        # agent_session_id -> RPC 会话句柄
        self._rpc: dict[str, dict[str, Any]] = {}

    # -- argv ---------------------------------------------------------------

    def _rpc_argv(self, *, approve: bool = False,
                  no_session: bool = False,
                  session_dir: Optional[str] = None,
                  extension_path: Optional[str] = None) -> list[str]:
        argv = [self._binary, "--mode", "rpc"]
        if no_session:
            argv.append("--no-session")
        elif session_dir or self._session_dir:
            argv += ["--session-dir", session_dir or str(self._session_dir)]
        # ``approve`` / ``trust_project``：pi ≥0.84 才有 ``--approve``（项目
        # 信任覆盖，非工具 HITL）。目标运行时 0.73.1 传入会
        # ``Unknown option: --approve`` 退出，故忽略该参数。Agent Plugin
        # 经显式 ``--extension`` 注入，不依赖项目信任门闩。
        _ = approve
        if extension_path:
            # 禁止项目/用户目录自动发现，显式加载标准包内的客户端扩展。
            # 这样旧的 .pi Skill 或 Extension 不会覆盖本次授权工具集。
            argv += [
                "--no-extensions",
                "--no-skills",
                "--no-builtin-tools",
                # Keep local shell/fs tools so long-tool / Stop (#133) and HITL
                # paths stay exercisable alongside Agent Plugin MCP tools.
                "--tools",
                "bash,read,edit,write,grep,find,ls",
                "--no-context-files",
                "--extension",
                extension_path,
            ]
        return argv

    @staticmethod
    async def _cmd(peer: StdioJsonlPeer, command: str,
                   params: Optional[dict[str, Any]] = None,
                   timeout: Optional[float] = 60.0) -> dict[str, Any]:
        """发 RPC 命令并解开响应；``success:false`` 抛 ``PeerClosedError``
        之外的运行时错误。"""
        resp = await peer.request(command, params, timeout=timeout)
        if not resp.get("success"):
            raise RuntimeError(
                f"pi rpc {command} failed: {str(resp.get('error'))[:200]}")
        data = resp.get("data")
        return data if isinstance(data, dict) else {}

    @staticmethod
    def _turn_diagnostics(handle: dict[str, Any]) -> dict[str, Any]:
        """构造 Pi turn 失败时可持久化的最小诊断。

        ``prompt_rpc`` 明确记录 Adapter 发给 peer 的实际命令与 timeout；
        peer 自己的摘要补充 stdout/stderr/进程状态。两者都不含 RPC 参数、
        argv、环境变量或凭据。
        """
        result: dict[str, Any] = {}
        prompt_rpc = handle.get("prompt_rpc")
        if isinstance(prompt_rpc, dict):
            result["prompt_rpc"] = dict(prompt_rpc)
        peer = handle.get("peer")
        if isinstance(peer, StdioJsonlPeer):
            result["peer"] = peer.diagnostics()
        return result

    # -- probe ----------------------------------------------------------------

    async def probe(self, request: ProbeRequest) -> AgentCapabilities:
        field_sources: dict[str, str] = {}
        degradations: list[str] = []
        caps = conservative_capabilities(
            transport_kind="rpc", capability_source=SOURCE_STATIC)
        version = _probe_version(self._binary)
        detail = ""
        state: dict[str, Any] = {}
        if version:
            # 真实拉起一个 ephemeral RPC 进程验证协议面（get_state）。
            peer = StdioJsonlPeer(
                self._rpc_argv(no_session=True), label="pi.probe",
                env=self._default_env,
                request_envelope=_pi_envelope)
            try:
                await peer.start()
                state = await self._cmd(peer, "get_state",
                                        timeout=self._startup_timeout)
                if request.include_models:
                    models = await self._cmd(
                        peer, "get_available_models",
                        timeout=self._startup_timeout)
                    caps.supported_models = [
                        str(m.get("id")) for m in (
                            models.get("models") or [])
                        if isinstance(m, dict) and m.get("id")
                    ][:50]
            except (OSError, asyncio.TimeoutError, PeerClosedError,
                    RuntimeError) as exc:
                detail = f"pi rpc get_state 失败：{str(exc)[:160]}"
                state = {}
            finally:
                await peer.close()

        caps.runtime_version = version
        probed = bool(version and state)
        if not probed:
            for field_name in BOOL_CAPABILITY_FIELDS:
                field_sources[field_name] = SOURCE_STATIC
            degradations.append(
                "structured transport 不可用：send/steer/abort/extension "
                "均未实测，按保守默认 False（不静默降级）")
            self._probe_cache = CapabilityProbeReport(
                adapter_id=self.identity.adapter_id,
                instance_id=self.identity.instance_id,
                capabilities=caps, binary_path=self._binary,
                field_sources=field_sources, degradations=degradations,
                detail=detail or "binary 不可用或 --version 失败",
            )
            return caps

        # 实测能力（命令面逐条与 rpc-types.ts 核验一致）。
        caps.capability_source = SOURCE_PROBE
        caps.protocol_version = "jsonl-rpc"
        caps.streaming = True
        caps.tool_events = True
        caps.usage_events = True
        caps.steer = True
        caps.interrupt = True
        caps.resume = True
        caps.session_persistence = True
        caps.skills = True
        caps.agent_plugin = True
        # prompt {message, images?} — ImageContent {type, data, mimeType}.
        caps.image_input = True
        field_sources["image_input"] = SOURCE_PROBE
        # 未确认的能力保持保守 False：mcp（0.84 未确认原生 MCP 配置入口）、
        # approval（RPC 无非阻塞带内审批，工具审批策略由启动参数固化）、
        # user_input（extension_ui 子协议未接入）、
        # plan（无原生 plan/task 事件流；勿从 markdown 待办臆测进度）。
        for field_name in BOOL_CAPABILITY_FIELDS:
            field_sources[field_name] = SOURCE_PROBE
        degradations.append(
            "approval：RPC 模式无带内审批请求；pi 0.73.1 无 --approve，"
            "工具范围由启动旗标（--tools / --extension）固化，未静默关闭")
        degradations.append(
            "plan：Pi RPC 无结构化计划事件；Conversation 计划面板按 unsupported 降级")
        degradations.append(
            "mcp：pi 0.84 未确认原生 MCP 配置入口，能力注入默认走 "
            "Agent Plugins 标准包的 io.github.fishcodetech.pi 客户端扩展，"
            "由扩展注册原生 muteki_* 工具")
        model = state.get("model") or {}
        self._probe_cache = CapabilityProbeReport(
            adapter_id=self.identity.adapter_id,
            instance_id=self.identity.instance_id,
            capabilities=caps, binary_path=self._binary,
            field_sources=field_sources, degradations=degradations,
            detail=f"pi rpc 协商成功（当前模型 "
                   f"{model.get('provider')}/{model.get('id')}）",
        )
        return caps

    # -- 启动 / 恢复 -----------------------------------------------------------

    async def _launch(
        self,
        request: SessionStart,
        plan: Optional[CapabilityInjectionPlan],
        bearer_token: Optional[str],
    ) -> dict[str, Any]:
        cwd = str(request.options.get("cwd") or os.getcwd())
        env = {
            **self._default_env,
            **{
                k: str(v)
                for k, v in (request.options.get("env") or {}).items()
            },
        }
        # 与 Pi CLI compatibility 路径保持一致：Worker 的本地工具、项目扩展
        # 和 Provider 配置都在非交互 Runtime 内使用。实际命令范围仍由
        # Muteki 的任务、授权与工作目录约束。
        env.setdefault("PI_OFFLINE", "1")
        env.setdefault("PI_SKIP_VERSION_CHECK", "1")
        # 受管的 Pi Profile 通过私有 models.json 提供 provider 凭据。Pi 的
        # bash 工具会继承本进程环境，因此在已配置该目录时不再传递通用 API
        # Key 环境变量，避免工具输出意外携带凭据。
        if (env.get("MUTEKI_PI_PROVIDER") and env.get("PI_CODING_AGENT_DIR")):
            env.pop("OPENAI_API_KEY", None)
            env.pop("OPENAI_API_KEY_FILE", None)
        approve = self._trust_project
        plugin_extension_path: Optional[Path] = None
        # SessionStart.model 通常只有模型 ID；Profile 的 Provider 由
        # runtime_env_for_engine() 放到 MUTEKI_PI_PROVIDER 中。旧实现仅处理
        # "provider/model" 形式，导致结构化 Pi 忽略 Profile 的 Provider 与
        # 模型，回落到 Pi 自己的 openai/gpt-5.5 默认值。
        requested_model = str(request.model or "").strip()
        model_provider = ""
        model_id = ""
        if "/" in requested_model:
            model_provider, model_id = requested_model.split("/", 1)
            model_provider = model_provider.strip()
            model_id = model_id.strip()
        else:
            model_provider = str(env.get("MUTEKI_PI_PROVIDER") or "").strip()
            model_id = (
                requested_model
                or str(env.get("MUTEKI_PI_MODEL") or "").strip()
            )
        if plan is not None:
            env[ENV_ENDPOINT] = plan.gateway_endpoint
            if bearer_token:
                env[ENV_TOKEN] = bearer_token
            if plan.injection_kind is InjectionKind.AGENT_PLUGIN:
                plugin_data = self._runtime_root / request.agent_session_id
                plugin_runtime = materialize_runtime(
                    str((plan.runtime_config or {}).get("plugin_root") or package_root()),
                    plugin_data=plugin_data,
                    endpoint=plan.gateway_endpoint,
                    bearer_token=bearer_token or "",
                    required_components=("mcp", "skills"),
                )
                plugin_extension_path = resolve_client_extension_entry(
                    plugin_runtime.package,
                    PI_EXTENSION_NAMESPACE,
                )
                tools_file = plugin_data / "tools.json"
                tools_file.write_text(json.dumps([
                    tool.model_dump(mode="json")
                    for tool in plan.tool_descriptions
                ], ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
                tools_file.chmod(0o600)
                env["PLUGIN_ROOT"] = str(plugin_runtime.package.root)
                env["PLUGIN_DATA"] = str(plugin_runtime.plugin_data)
                env[PI_TOOLS_FILE_ENV] = str(tools_file)

        async def on_message(msg: dict[str, Any]) -> None:
            self._dispatch_event(request.agent_session_id, msg)

        peer = StdioJsonlPeer(
            self._rpc_argv(
                approve=approve,
                no_session=bool(request.options.get("ephemeral")),
                session_dir=str(request.options.get("session_dir") or ""),
                extension_path=(
                    str(plugin_extension_path)
                    if plugin_extension_path else None)),
            cwd=cwd, env=env, label="pi.rpc",
            request_envelope=_pi_envelope,
            on_message=on_message,
        )
        await peer.start()
        try:
            state = await self._cmd(peer, "get_state",
                                    timeout=self._startup_timeout)
            resume_handle = request.resume_handle
            if resume_handle:
                await self._cmd(peer, "switch_session",
                                {"sessionPath": resume_handle},
                                timeout=self._startup_timeout)
            if model_id and not model_provider:
                available = await self._cmd(peer, "get_available_models", timeout=self._startup_timeout)
                providers = {str(item.get("provider") or "")
                             for item in available.get("models", [])
                             if item.get("id") == model_id and item.get("provider")}
                if len(providers) != 1:
                    raise RuntimeError(f"Pi 模型 {model_id!r} 无法唯一匹配 Provider，请选择 provider/model")
                model_provider = providers.pop()
            if model_provider and model_id:
                await self._cmd(peer, "set_model",
                                {"provider": model_provider, "modelId": model_id},
                                timeout=self._startup_timeout)
            if request.effort and request.effort != "default":
                await self._cmd(peer, "set_thinking_level",
                                {"level": request.effort},
                                timeout=self._startup_timeout)
            stats = await self._cmd(peer, "get_session_stats",
                                    timeout=self._startup_timeout)
        except Exception:
            await peer.close()
            raise

        session_id = str(stats.get("sessionId") or state.get("sessionId")
                         or "")
        # 启动环境只应留在当前进程的 env；handle 后续可能被诊断代码读取，
        # 因而不保留 request.options 中的凭据环境副本。
        handle_options = dict(request.options)
        handle_options.pop("env", None)
        handle = {
            "peer": peer,
            "conversation_thread_id": request.thread_id,
            "cwd": cwd,
            "env": env,
            "options": handle_options,
            "turns": 0,
            "external_session_id": session_id,
            "resume_handle": resume_handle,
            "agent_plugin_extension": (
                str(plugin_extension_path) if plugin_extension_path else None),
            "entries_cursor": None,
            "event_sink": None,
            "current_turn_id": None,
            "resumed": bool(resume_handle),
            "model_provider": model_provider,
            "model_id": model_id,
            "capability_revision": 0,
        }
        self._rpc[request.agent_session_id] = handle
        # durable cursor：恢复后立即取一次 entries 水位，之后的事件流
        # 只消费新增 entry，重启/重连不重复写历史。
        try:
            entries = await self._cmd(peer, "get_entries",
                                      timeout=self._startup_timeout)
            # 0.84.1 实测字段为 leafId（核验文档按 lastEntryId 描述，
            # 两者都兜底）。
            handle["entries_cursor"] = (
                entries.get("leafId") or entries.get("lastEntryId"))
        except Exception:  # noqa: BLE001
            pass
        return {
            "external_session_id": session_id or None,
            "resume_handle": resume_handle or session_id or None,
        }

    # -- 事件归一化 -------------------------------------------------------------

    def _dispatch_event(self, agent_session_id: str,
                        msg: dict[str, Any]) -> None:
        """Pi 事件流 → turn 队列（在 reader task 上运行，只 put_nowait）。"""
        handle = self._rpc.get(agent_session_id)
        if handle is None:
            return
        sink = handle.get("event_sink")
        if sink is None:
            return
        etype = msg.get("type")
        if etype == "message_update":
            # 累计 usage 在事件顶层（§PI 核验）。
            usage = msg.get("usage")
            if isinstance(usage, dict) and usage:
                sink.put_nowait((AgentEventType.USAGE_UPDATED,
                                 "pi.message_update", {"usage": usage, "usage_message_id": str(handle.get("usage_message_index", 0))}))
            delta = msg.get("assistantMessageEvent") or {}
            if delta.get("type") == "text_delta" and delta.get("delta"):
                sink.put_nowait((AgentEventType.MESSAGE_DELTA,
                                 "pi.text_delta",
                                 {"text": str(delta["delta"]),
                                  "content_index": delta.get("contentIndex")}))
                handle["saw_assistant_text"] = True
        elif etype == "message_end":
            # 与 CLI PiDriver 一致：只投影 assistant；user/tool 的 message_end
            # 不能写成 MESSAGE_COMPLETED，否则界面会把用户原文当成 Agent 回复。
            message = msg.get("message") or {}
            if not isinstance(message, dict) or message.get("role") != "assistant":
                return
            text = _pi_message_text(message)
            usage = message.get("usage")
            if isinstance(usage, dict) and usage:
                sink.put_nowait((AgentEventType.USAGE_UPDATED,
                                 "pi.message_end", {"usage": usage, "usage_message_id": str(handle.get("usage_message_index", 0))}))
            handle["usage_message_index"] = int(handle.get("usage_message_index", 0)) + 1
            error_message = str(message.get("errorMessage") or "").strip()
            stop_reason = str(message.get("stopReason") or "").strip()
            if error_message or stop_reason == "error":
                detail = error_message or f"stopReason={stop_reason or 'error'}"
                if _pi_is_user_abort(
                    detail, abort_requested=bool(handle.get("abort_requested")),
                ):
                    # User interrupt: do not publish RUNTIME_ERROR / failed.
                    handle["turn_interrupted"] = detail or "aborted"
                    return
                handle["turn_failed"] = detail
                diagnostics = self._turn_diagnostics(handle)
                handle["turn_failure_diagnostics"] = diagnostics
                sink.put_nowait((AgentEventType.RUNTIME_ERROR,
                                 "pi.message_end", {
                                     "code": "pi.assistant_error",
                                     "detail": detail,
                                     "error": detail,
                                     "stop_reason": stop_reason,
                                     "diagnostics": diagnostics,
                                 }))
                return
            if text:
                handle["assistant_text"] = text
                handle["saw_assistant_text"] = True
        elif etype == "tool_execution_start":
            sink.put_nowait((AgentEventType.TOOL_STARTED,
                             "pi.tool_execution_start", {
                                 "call_id": str(msg.get("toolCallId") or ""),
                                 "tool": str(msg.get("toolName") or ""),
                                 "input": msg.get("args"),
                             }))
        elif etype == "tool_execution_update":
            sink.put_nowait((AgentEventType.TOOL_PROGRESS,
                             "pi.tool_execution_update", {
                                 "call_id": str(msg.get("toolCallId") or ""),
                                 "output": msg.get("partialResult"),
                             }))
        elif etype == "tool_execution_end":
            # On operator abort Pi often ends the tool with isError +
            # "Command aborted". Emitting TOOL_COMPLETED(is_error=true)
            # makes Pane label the tool 「执行失败」 permanently (settle
            # only rewrites open tools). Drop the end event so the tool
            # stays running until core.turn.interrupted settles it to
            # cancelled / 「已取消」.
            if handle.get("abort_requested"):
                return
            sink.put_nowait((AgentEventType.TOOL_COMPLETED,
                             "pi.tool_execution_end", {
                                 "call_id": str(msg.get("toolCallId") or ""),
                                 "output": msg.get("result"),
                                 "is_error": bool(msg.get("isError")),
                             }))
        elif etype in ("agent_end", "agent_settled"):
            # turn 完成信号（prompt 响应只是 ack，见模块 docstring）。
            sink.put_nowait(("__turn_done__", etype, {
                "will_retry": bool(msg.get("willRetry")),
            }))
        # 其余事件（turn_start/compaction_*/extension_ui_request 等）
        # 不改变核心状态机，忽略。

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
        if handle is None:
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
                native_type="pi.session.start",
                payload={"transport": "rpc",
                         "adapter_id": self.id,
                         "instance_id": self.identity.instance_id,
                         "cwd": handle["cwd"],
                         "agent_plugin_extension": handle.get(
                             "agent_plugin_extension"),
                         "model_provider": handle.get("model_provider"),
                         "model_id": handle.get("model_id")},
                **common))
        turn_id = new_id("turn")
        handle["current_turn_id"] = turn_id
        handle["turn_failed"] = None
        handle["turn_interrupted"] = None
        handle.pop("turn_failure_diagnostics", None)
        handle.pop("abort_requested", None)
        handle["saw_assistant_text"] = False
        handle["assistant_text"] = ""
        prompt_timeout = self.conversation_turn_timeout(
            handle.get("conversation_thread_id"), self._prompt_timeout)
        handle["prompt_rpc"] = {
            "method": "prompt",
            "timeout_seconds": prompt_timeout,
            "acknowledged": False,
        }
        yield self.emit(build_event(
            AgentEventType.TURN_STARTED, seq,
            external_session_id=external_id, turn_id=turn_id,
            native_type="pi.prompt.start",
            payload={"kind": input.kind},
            **common))

        queue: asyncio.Queue = asyncio.Queue()
        handle["event_sink"] = queue

        async def run_prompt() -> Optional[Exception]:
            try:
                # prompt 响应只是 ack；完成由 agent_end/agent_settled 事件
                # 判定（_dispatch_event 放 __turn_done__ 标记）。
                prompt_params: dict[str, Any] = {"message": input.text}
                images = pi_prompt_images(input.payload)
                if images:
                    prompt_params["images"] = images
                    handle["prompt_rpc"]["image_count"] = len(images)
                await self._cmd(peer, "prompt", prompt_params,
                                timeout=prompt_timeout)
                handle["prompt_rpc"]["acknowledged"] = True
                return None
            except Exception as exc:  # noqa: BLE001
                handle["prompt_rpc"]["error_type"] = type(exc).__name__
                return exc

        task = asyncio.ensure_future(run_prompt())
        ack_error: Optional[Exception] = None
        done_info: dict[str, Any] = {}
        get_task = asyncio.ensure_future(queue.get())
        try:
            while True:
                # 同时等事件与 prompt ack：ack 失败时不会到达任何事件，
                # 必须能被 task 完成唤醒，不能死等队列。
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
        handle["event_sink"] = None
        handle["turns"] += 1
        turn_failed = handle.pop("turn_failed", None)
        turn_interrupted = handle.pop("turn_interrupted", None)
        abort_requested = bool(handle.pop("abort_requested", False))
        failure_diagnostics = handle.pop("turn_failure_diagnostics", None)
        saw_assistant_text = bool(handle.pop("saw_assistant_text", False))
        handle["current_turn_id"] = None

        if ack_error is not None:
            if abort_requested or _pi_is_user_abort(str(ack_error)):
                yield self.emit(build_event(
                    AgentEventType.TURN_FAILED, seq,
                    external_session_id=external_id, turn_id=turn_id,
                    native_type="pi.prompt.aborted",
                    payload={"reason": EXIT_INTERRUPTED,
                             "stop_reason": "aborted",
                             "detail": str(ack_error)[:300]},
                    **common))
                return
            yield self.emit(build_event(
                AgentEventType.TURN_FAILED, seq,
                external_session_id=external_id, turn_id=turn_id,
                native_type="pi.prompt.error",
                payload={
                    "error": str(ack_error)[:300],
                    "diagnostics": self._turn_diagnostics(handle),
                },
                **common))
            return
        if (
            turn_interrupted
            or abort_requested
            or _pi_is_user_abort(str(turn_failed or ""))
        ):
            yield self.emit(build_event(
                AgentEventType.TURN_FAILED, seq,
                external_session_id=external_id, turn_id=turn_id,
                native_type="pi.turn.interrupted",
                payload={
                    "reason": EXIT_INTERRUPTED,
                    "stop_reason": "aborted",
                    "detail": str(turn_interrupted or turn_failed or "")[:300],
                },
                **common))
            return
        if turn_failed:
            yield self.emit(build_event(
                AgentEventType.TURN_FAILED, seq,
                external_session_id=external_id, turn_id=turn_id,
                native_type="pi.assistant_error",
                payload={
                    "error": {
                        "code": "pi.assistant_error",
                        "message": str(turn_failed)[:500],
                    },
                    "diagnostics": (
                        failure_diagnostics
                        if isinstance(failure_diagnostics, dict)
                        else self._turn_diagnostics(handle)
                    ),
                },
                **common))
            return
        assistant_text = str(handle.pop("assistant_text", ""))
        if not saw_assistant_text or not assistant_text.strip():
            yield self.emit(build_event(
                AgentEventType.TURN_FAILED, seq,
                external_session_id=external_id, turn_id=turn_id,
                native_type="pi.empty_assistant",
                payload={
                    "error": {
                        "code": "pi.empty_assistant",
                        "message": "Pi turn ended without assistant text",
                    },
                },
                **common))
            return
        yield self.emit(build_event(
            AgentEventType.MESSAGE_COMPLETED, seq,
            external_session_id=external_id, turn_id=turn_id,
            native_type="pi.message.completed",
            payload={"text": assistant_text, "role": "assistant"},
            **common))
        yield self.emit(build_event(
            AgentEventType.TURN_COMPLETED, seq,
            external_session_id=external_id, turn_id=turn_id,
            native_type="pi.agent_end",
            payload={"will_retry": done_info.get("will_retry", False)},
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
            native_type="pi.session.resumed",
            payload={"transport": "rpc",
                     "entries_cursor": handle.get("entries_cursor")},
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
        # Record operator abort before awaiting RPC so concurrent
        # message_end / tool_execution_end classify as interrupted.
        handle["abort_requested"] = True
        try:
            await self._cmd(handle["peer"], "abort")
            return self._receipt(session, True)
        except Exception as exc:  # noqa: BLE001
            handle.pop("abort_requested", None)
            return self._receipt(session, False, str(exc))

    async def runtime_capability_snapshot(
        self, session: Optional[AgentSessionRef] = None
    ) -> RuntimeCapabilitySnapshot:
        base = await super().runtime_capability_snapshot(session)
        if session is None:
            base.stale = True
            base.diagnostics.append(
                "Pi 命令目录属于具体 RPC Session，当前没有活动会话")
            return base
        handle = self._rpc.get(session.agent_session_id)
        if handle is None or handle.get("peer") is None:
            base.stale = True
            base.diagnostics.append("Pi RPC Session 当前不在本进程")
            return base
        try:
            result = await self._cmd(
                handle["peer"], "get_commands", timeout=self._startup_timeout)
        except Exception as exc:  # noqa: BLE001
            base.stale = True
            base.diagnostics.append(
                f"Pi get_commands 读取失败：{type(exc).__name__}: {str(exc)[:160]}"
            )
            return base
        handle["capability_revision"] = int(
            handle.get("capability_revision") or 0
        ) + 1
        items = list(base.items)
        for raw in result.get("commands") or []:
            if not isinstance(raw, dict):
                continue
            name = str(raw.get("name") or "").strip().lstrip("/")
            if not name:
                continue
            source = str(raw.get("source") or "")
            items.append(dynamic_command_item(
                adapter_id=self.id,
                engine="pi",
                name=name,
                description=str(raw.get("description") or ""),
                channel="provider_native",
                kind="skill" if source.casefold() == "skill"
                or name.startswith("skill:") else "command",
                invocation={
                    "command": name,
                    "wire_text": f"/{name}",
                    "protocol": "pi.rpc.prompt",
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


__all__ = [
    "_pi_is_user_abort",
    "PI_EXTENSION_ENTRY",
    "PI_EXTENSION_NAMESPACE",
    "PI_TOOLS_FILE_ENV",
    "PiAdapter",
]
