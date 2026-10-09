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
- 能力注入：Pi 没有原生 MCP 配置。``InjectionKind.MCP`` 时把 Muteki
  chat MCP 客户端写成 Adapter runtime 目录里的 ``--extension``（不写
  ``~/.pi``）。``InjectionKind.AGENT_PLUGIN`` 仍加载标准包里的
  ``io.github.fishcodetech.pi`` 扩展。
- 0.86.1 RPC 有 ``get_messages``（活动分支）、``get_fork_messages``、
  ``fork {entryId}``（默认落在该用户消息之前）、``clone``、
  ``set_session_name``、``new_session``。没有 ``rewind`` 命令；回退用
  ``fork``。未在本进程确认的命令不声明、不伪造。
- **trust / ``--approve``**：``--approve``/``-a`` 是 pi ≥0.84 的项目信任
  覆盖。0.73.1 不认识该选项，传入会退出。本 Adapter 仍不传
  ``--approve``；扩展经显式 ``--extension`` 注入。``trust_project`` 为
  兼容开关，当前为 no-op。能力以本进程 RPC 实测为准（当前核验版本 0.86.1）。

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
from muteki.platform.contracts.agent_events import (
    FailureCategory,
    MessageDeltaPayload,
    MessageCompletedPayload,
    RuntimeErrorPayload,
    RuntimeWarningPayload,
    SessionPayload,
    ToolPayload,
    TurnCompletedPayload,
    TurnFailedPayload,
    TurnStartedPayload,
    UsagePayload,
    dump_payload,
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
    AgentSessionSnapshot,
    AgentInput,
    MessageInput,
    SteerInput,
    AgentSessionRef,
    ProbeRequest,
    SessionStart,
)
from muteki.platform.contracts.protocols import (
    NativeRewindAdapter,
    RuntimeOperationAdapter,
)
from muteki.platform.contracts.receipts import (
    AggregateRef,
    CommandReceipt,
    ReceiptState,
)

from .base import BaseExternalAgentAdapter, TurnLimits, TurnRunner
from .attachment_input import pi_prompt_images
from .capabilities import (
    BOOL_CAPABILITY_FIELDS,
    CapabilityProbeReport,
    SOURCE_PROBE,
    SOURCE_STATIC,
    conservative_capabilities,
    _probe_version,
)
from .command_providers import operation_item
from .events import build_event
from .runtime_capabilities import (
    RuntimeCapabilitySnapshot,
    dynamic_command_item,
)
from .rpc import PeerClosedError, StdioJsonlPeer
from .sessions import classify_exit

PI_EXTENSION_NAMESPACE = "io.github.fishcodetech.pi"
PI_EXTENSION_ENTRY = "extension.ts"
PI_TOOLS_FILE_ENV = "MUTEKI_CAPABILITY_TOOLS_FILE"
PI_MCP_EXTENSION_FILENAME = "muteki-pi-mcp-extension.ts"
PI_MCP_PROTOCOL = "2025-11-25"
# Commands present in pi 0.86.1 ``rpc-types`` / ``rpc-mode.js``. ``rewind`` is
# not one of them; rollback is ``fork`` at the user entry (position "before").
PI_FORK_COMMAND = "fork"
PI_GET_MESSAGES_COMMAND = "get_messages"
PI_GET_FORK_MESSAGES_COMMAND = "get_fork_messages"


class PiRpcError(RuntimeError):
    """Typed Pi RPC failure. Callers branch on ``code``, never on ``str(exc)``."""

    def __init__(
        self,
        code: str,
        message: str,
        *,
        command: str = "",
        data: Any = None,
    ) -> None:
        self.code = code
        self.command = command
        self.data = data
        super().__init__(message)


def _pi_envelope(msg_id: str, method: str,
                 params: Optional[dict[str, Any]]) -> dict[str, Any]:
    """Pi RPC 请求形态：``{id, type, ...params}``。"""
    return {"id": msg_id, "type": method, **(params or {})}



def _pi_is_user_abort(stop_reason: str, *, abort_requested: bool) -> bool:
    """True when an abort, not a provider error, ended the assistant message.

    Pi has no dedicated interrupted event: an aborted message ends with
    ``stopReason == "aborted"``. Muteki also records its own RPC ``abort``
    so concurrent message_end / tool_execution_end classify as interrupted
    and Conversation emits ``core.turn.interrupted``.
    """
    return abort_requested or stop_reason == "aborted"


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


def _pi_usage_payload(usage: dict[str, Any], usage_id: str) -> dict[str, Any]:
    """Pi ``message_update``/``message_end`` usage → normalized contract.

    Pi 0.73/0.84 report disjoint buckets (``input`` excludes cached input),
    so the normalized ``input_tokens`` adds the cache buckets back.
    """
    def _num(value: Any) -> Optional[int]:
        return value if isinstance(value, int) and not isinstance(value, bool) and value >= 0 else None
    cost = usage.get("cost")
    return dump_payload(UsagePayload(
        scope="message",
        usage_id=usage_id,
        input_tokens=(
            (value if (value := _num(usage.get("input"))) is not None else 0)
            + (_num(usage.get("cacheRead")) or 0)
            + (_num(usage.get("cacheWrite")) or 0)
            if any(_num(usage.get(key)) is not None
                   for key in ("input", "cacheRead", "cacheWrite"))
            else None
        ),
        output_tokens=_num(usage.get("output")),
        cached_input_tokens=_num(usage.get("cacheRead")),
        cache_write_tokens=_num(usage.get("cacheWrite")),
        total_tokens=_num(usage.get("totalTokens")),
        cost_usd=(
            value if isinstance(cost, dict)
            and isinstance((value := cost.get("total")), (int, float))
            and not isinstance(value, bool) else None
        ),
        native=dict(usage),
    ))


def _plan_mcp_endpoint(plan: CapabilityInjectionPlan) -> str:
    servers = plan.runtime_config.get("mcpServers")
    if isinstance(servers, dict):
        for server in servers.values():
            if not isinstance(server, dict):
                continue
            url = server.get("url")
            if isinstance(url, str) and url.strip():
                return url.strip()
    return str(plan.gateway_endpoint or "").strip()


def _mcp_extension_source() -> str:
    """TypeScript Pi loads via ``--extension``. It must not import Muteki.

    The process resolves ``typebox`` from the user's Pi install. The bearer
    token stays in the environment; this file never contains it.
    """
    url_env = json.dumps(ENV_ENDPOINT)
    token_env = json.dumps(ENV_TOKEN)
    protocol = json.dumps(PI_MCP_PROTOCOL)
    return """import { Type } from "typebox";

const URL_ENV = __URL_ENV__;
const TOKEN_ENV = __TOKEN_ENV__;
const PROTOCOL = __PROTOCOL__;

function requiredEnv(name) {
  const value = process.env[name];
  if (typeof value !== "string" || value.length === 0) {
    throw new Error("pi.mcp.env_missing:" + name);
  }
  return value;
}

function parseBody(body, contentType) {
  if (!body) return undefined;
  if (String(contentType || "").includes("text/event-stream")) {
    for (const line of body.split("\\n")) {
      const trimmed = line.startsWith("data:") ? line.slice(5).trim() : "";
      if (!trimmed) continue;
      const parsed = JSON.parse(trimmed);
      if (parsed.id !== undefined || parsed.result !== undefined || parsed.error !== undefined) {
        return parsed;
      }
    }
    throw new Error("pi.mcp.empty_sse");
  }
  return JSON.parse(body);
}

function createClient(endpoint, token) {
  let nextId = 1;
  const headers = () => ({
    accept: "application/json, text/event-stream",
    authorization: token.startsWith("Bearer ") ? token : "Bearer " + token,
    "content-type": "application/json",
    "mcp-protocol-version": PROTOCOL,
  });
  const request = async (method, params) => {
    const id = nextId++;
    const response = await fetch(endpoint, {
      method: "POST",
      headers: headers(),
      body: JSON.stringify({ jsonrpc: "2.0", id, method, params: params || {} }),
    });
    const body = await response.text();
    if (!response.ok) {
      throw new Error("pi.mcp.http_failed:" + method + ":" + String(response.status));
    }
    const parsed = parseBody(body, response.headers.get("content-type") || "");
    if (!parsed || parsed.error) {
      const message = parsed && parsed.error && parsed.error.message;
      throw new Error(typeof message === "string" && message ? message : "pi.mcp.rpc_failed:" + method);
    }
    return parsed.result;
  };
  const notify = async (method, params) => {
    const response = await fetch(endpoint, {
      method: "POST",
      headers: headers(),
      body: JSON.stringify({ jsonrpc: "2.0", method, params: params || {} }),
    });
    if (!response.ok && response.status !== 202) {
      throw new Error("pi.mcp.http_failed:" + method + ":" + String(response.status));
    }
  };
  return {
    async connect() {
      await request("initialize", {
        protocolVersion: PROTOCOL,
        capabilities: {},
        clientInfo: { name: "muteki-pi-mcp", version: "1.0.0" },
      });
      await notify("notifications/initialized", {});
    },
    async listTools() {
      const tools = [];
      let cursor = undefined;
      do {
        const result = await request("tools/list", cursor ? { cursor } : {});
        const page = result && result.tools;
        if (!Array.isArray(page)) throw new Error("pi.mcp.tools_list_invalid");
        tools.push(...page);
        cursor = result && result.nextCursor;
      } while (cursor);
      return tools;
    },
    async callTool(name, args, imageInput) {
      return request("tools/call", { name, arguments: args || {},
        ...(typeof imageInput === "boolean"
          ? { _meta: { "muteki/modelCapabilities": { imageInput } } } : {}),
      });
    },
  };
}

function formatResult(result) {
  if (result == null) return "";
  if (typeof result !== "object") return String(result);
  const texts = [];
  if (Array.isArray(result.content)) {
    for (const part of result.content) {
      if (part && part.type === "text" && typeof part.text === "string") texts.push(part.text);
    }
  }
  if (result.structuredContent !== undefined) {
    const structured = JSON.stringify(result.structuredContent);
    if (!texts.includes(structured)) texts.push(structured);
  }
  return texts.length > 0 ? texts.join("\\n") : JSON.stringify(result);
}

export default async function mutekiPiMcp(pi) {
  if (typeof Type.Unsafe !== "function") {
    throw new Error("pi.mcp.typebox_unsafe_missing");
  }
  const client = createClient(requiredEnv(URL_ENV), requiredEnv(TOKEN_ENV));
  await client.connect();
  const tools = await client.listTools();
  // Pi 0.86.1's execute-error path keeps only text. Restore the full MCP
  // evidence through its final-result hook while keeping the failed status.
  const failedResults = new Map();
  pi.on("tool_result", event => {
    const result = failedResults.get(event.toolCallId);
    if (!result) return;
    failedResults.delete(event.toolCallId);
    return { ...result, isError: true };
  });
  pi.on("agent_end", () => failedResults.clear());
  pi.on("session_shutdown", () => failedResults.clear());
  for (const tool of tools) {
    if (!tool || typeof tool.name !== "string" || tool.name.length === 0) {
      throw new Error("pi.mcp.tool_name_invalid");
    }
    const name = tool.name;
    const description = typeof tool.description === "string" && tool.description ? tool.description : name;
    const schema = tool.inputSchema && typeof tool.inputSchema === "object"
      ? tool.inputSchema
      : { type: "object", properties: {} };
    pi.registerTool({
      name,
      label: name,
      description,
      promptSnippet: description.split("\\n")[0] || name,
      parameters: Type.Unsafe(schema),
      async execute(_toolCallId, params, _signal, _onUpdate, ctx) {
        // Read the current model for each call, including after /model or restore.
        const imageInput = Array.isArray(ctx?.model?.input)
          ? ctx.model.input.includes("image") : undefined;
        const result = await client.callTool(name, params || {}, imageInput);
        const images = result && Array.isArray(result.content)
          ? result.content.filter((part) => part && part.type === "image"
            && typeof part.data === "string" && typeof part.mimeType === "string")
            .map((part) => ({ type: "image", data: part.data, mimeType: part.mimeType }))
          : [];
        const output = {
          content: [{ type: "text", text: formatResult(result) },
            ...(imageInput === false ? [] : images)],
          details: { server: "muteki-control", tool: name, image_input: imageInput },
        };
        // Throw for Pi's real failure path; tool_result restores its images.
        if (result && result.isError === true) {
          failedResults.set(_toolCallId, output);
          throw new Error(formatResult(result));
        }
        return output;
      },
    });
  }
}
""".replace("__URL_ENV__", url_env).replace("__TOKEN_ENV__", token_env).replace(
        "__PROTOCOL__", protocol)


def _write_mcp_extension(directory: Path) -> Path:
    """Write the MCP extension under the adapter runtime directory."""
    directory.mkdir(parents=True, exist_ok=True)
    dest = directory / PI_MCP_EXTENSION_FILENAME
    source = _mcp_extension_source()
    if dest.is_file() and dest.read_text(encoding="utf-8") == source:
        return dest
    temporary = dest.with_suffix(".ts.tmp")
    temporary.write_text(source, encoding="utf-8")
    temporary.chmod(0o644)
    temporary.replace(dest)
    return dest


def _normalize_active_messages(data: dict[str, Any]) -> list[dict[str, Any]]:
    """``get_messages`` is the active branch. Anything else is a protocol error."""
    raw = data.get("messages")
    if not isinstance(raw, list):
        raise PiRpcError(
            "pi.rpc.protocol",
            "get_messages did not return a message list",
            command=PI_GET_MESSAGES_COMMAND,
            data=data,
        )
    messages: list[dict[str, Any]] = []
    for message in raw:
        if not isinstance(message, dict):
            continue
        role = message.get("role")
        if role not in {"user", "assistant"}:
            continue
        text = _pi_message_text(message)
        if not text:
            continue
        messages.append({
            "role": role,
            "text": text,
            "timestamp": message.get("timestamp"),
        })
    return messages


def _active_user_entry_ids(data: dict[str, Any]) -> list[str]:
    """User-message entry ids on the active branch, root to leaf."""
    entries = data.get("entries")
    if not isinstance(entries, list):
        raise PiRpcError(
            "pi.rpc.protocol",
            "get_entries did not return an entry list",
            command="get_entries",
            data=data,
        )
    by_id: dict[str, dict[str, Any]] = {}
    for entry in entries:
        if isinstance(entry, dict) and isinstance(entry.get("id"), str):
            by_id[entry["id"]] = entry
    current = data.get("leafId")
    chain: list[dict[str, Any]] = []
    seen: set[str] = set()
    while isinstance(current, str) and current and current not in seen:
        seen.add(current)
        entry = by_id.get(current)
        if entry is None:
            break
        chain.append(entry)
        parent = entry.get("parentId")
        current = parent if isinstance(parent, str) else None
    ids: list[str] = []
    for entry in reversed(chain):
        message = entry.get("message")
        if (
            entry.get("type") == "message"
            and isinstance(message, dict)
            and message.get("role") == "user"
            and isinstance(entry.get("id"), str)
        ):
            ids.append(entry["id"])
    return ids


class PiAdapter(BaseExternalAgentAdapter, NativeRewindAdapter, RuntimeOperationAdapter):
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
    - MCP：``InjectionKind.MCP`` 时在 runtime 目录写入扩展，向 Muteki
      chat MCP 注册工具。
    - 回退 / 分叉：本进程确认 ``get_fork_messages`` 后声明。回退是
      RPC ``fork {entryId}``；分叉是独立进程的 ``--fork``。没有
      ``rewind`` 命令。
    - 快照：``get_messages`` 的活动分支，不使用 ``get_entries``。
    """

    adapter_id = "pi.rpc"
    context_compaction_events = True

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
        self._rpc_features: dict[str, bool] = {
            "get_messages": False, "fork": False,
        }

    # -- argv ---------------------------------------------------------------

    def _rpc_argv(self, *, approve: bool = False,
                  no_session: bool = False,
                  session_dir: Optional[str] = None,
                  extension_path: Optional[str] = None,
                  chat_mode: bool = False,
                  mcp_extension: bool = False) -> list[str]:
        argv = self._with_launch_args([self._binary, "--mode", "rpc"])
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
            if chat_mode or mcp_extension:
                # Chat keeps the imported Pi home. MCP tools are extension
                # tools, so a ``--tools`` allowlist must not hide them.
                # ``--no-extensions`` still blocks discovery; ``-e`` is explicit.
                if mcp_extension and not chat_mode:
                    argv += ["--no-extensions", "--extension", extension_path]
                    return argv
                return [*argv, "--extension", extension_path]
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
        if not isinstance(resp, dict) or resp.get("type") != "response":
            raise PiRpcError(
                "pi.rpc.protocol",
                f"pi rpc {command} returned a non-response frame",
                command=command,
                data=resp,
            )
        if not resp.get("success"):
            raise PiRpcError(
                "pi.rpc.command_failed",
                f"pi rpc {command} failed: {resp.get('error')}",
                command=command,
                data=resp.get("error"),
            )
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

    async def _confirm_rpc_features(
        self, peer: StdioJsonlPeer,
    ) -> dict[str, str]:
        """Confirm get_messages and the fork family on this process.

        ``fork`` itself needs a real user entry to succeed. ``get_fork_messages``
        is the same 0.86.1 command family and returns ``{messages: []}`` on an
        empty session. A failed call leaves the flag false; the error code is
        recorded and the text is not used to pick another command.
        """
        errors: dict[str, str] = {}
        checks = (
            (PI_GET_MESSAGES_COMMAND, "get_messages"),
            (PI_GET_FORK_MESSAGES_COMMAND, "fork"),
        )
        for command, key in checks:
            try:
                data = await self._cmd(
                    peer, command, timeout=self._startup_timeout)
            except PiRpcError as exc:
                self._rpc_features[key] = False
                errors[key] = exc.code
                continue
            if isinstance(data.get("messages"), list):
                self._rpc_features[key] = True
            else:
                self._rpc_features[key] = False
                errors[key] = "pi.rpc.protocol"
        return errors

    # -- probe ----------------------------------------------------------------

    async def probe(self, request: ProbeRequest) -> AgentCapabilities:
        field_sources: dict[str, str] = {}
        degradations: list[str] = []
        caps = conservative_capabilities(
            transport_kind="rpc", capability_source=SOURCE_STATIC)
        version = _probe_version(self._binary)
        detail = ""
        state: dict[str, Any] = {}
        feature_errors: dict[str, str] = {}
        self._rpc_features = {"get_messages": False, "fork": False}
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
                feature_errors = await self._confirm_rpc_features(peer)
                if request.include_models:
                    models = await self._cmd(
                        peer, "get_available_models",
                        timeout=self._startup_timeout)
                    caps.supported_models = [
                        str(m.get("id")) for m in (
                            models.get("models") or [])
                        if isinstance(m, dict) and m.get("id")
                    ]
            except (OSError, asyncio.TimeoutError, PeerClosedError,
                    RuntimeError) as exc:
                detail = f"pi rpc get_state 失败：{exc}"
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
        caps.resume_continues_turn = False
        caps.session_persistence = True
        caps.skills = True
        caps.agent_plugin = True
        # prompt {message, images?} — ImageContent {type, data, mimeType}.
        caps.image_input = True
        field_sources["image_input"] = SOURCE_PROBE
        # RPC command "compact" {customInstructions?} 由 runtime 快照以
        # verified operation 暴露（rpc_commands.RPC_OPERATIONS）。
        caps.compaction = True
        # Pi has no native MCP config. This adapter injects the chat MCP
        # server with an extension in its own runtime directory.
        caps.mcp = True
        caps.fork = bool(self._rpc_features.get("fork"))
        if not self._rpc_features.get("get_messages"):
            degradations.append(
                "get_messages：活动分支快照命令未在本进程确认"
                + (f"（{feature_errors.get('get_messages')}）"
                   if feature_errors.get("get_messages") else "")
                + "，不伪造历史")
        if not caps.fork:
            degradations.append(
                "fork：get_fork_messages 未在本进程确认"
                + (f"（{feature_errors.get('fork')}）"
                   if feature_errors.get("fork") else "")
                + "；0.86.1 没有 rewind 命令，不声明 fork / native rewind")
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
        # caps.subagents 保持保守 False：Pi 无原生子智能体（README 明示
        # "No sub-agents"，核心工具无 task；0.86.1 实测 RPC 无 subagent
        # 命令面，set_subagent_subscription 返回 Unknown command）。
        # 用户扩展可自行注册类似工具，但 Pi RPC 没有标准子智能体事件契约，
        # 不做按工具名的臆测映射。
        degradations.append(
            "subagents：Pi 无原生子智能体（0.86.1 实测 RPC 无 "
            "set_subagent_subscription/subagent_* 命令与帧，核心工具无 "
            "task），保持 False")
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
        options = request.options
        cwd = options.cwd or os.getcwd()
        env = {**self._default_env, **options.env}
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
        mcp_extension = False
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
        if plan is not None and plan.injection_kind is InjectionKind.MCP:
            endpoint = _plan_mcp_endpoint(plan)
            if not endpoint:
                raise PiRpcError(
                    "pi.mcp.endpoint_missing",
                    "MCP injection plan has no gateway endpoint",
                    command="mcp",
                )
            if not bearer_token:
                raise PiRpcError(
                    "pi.mcp.token_missing",
                    "MCP injection plan has no bearer token",
                    command="mcp",
                )
            env[ENV_ENDPOINT] = endpoint
            env[ENV_TOKEN] = bearer_token
            plugin_extension_path = _write_mcp_extension(
                self._runtime_root / request.agent_session_id)
            mcp_extension = True
        elif plan is not None:
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

        startup_errors: list[dict[str, Any]] = []

        async def on_message(msg: dict[str, Any]) -> None:
            if msg.get("type") == "extension_error":
                startup_errors.append({
                    "extensionPath": msg.get("extensionPath"),
                    "event": msg.get("event"),
                    "error": msg.get("error"),
                })
            if msg.get("type") == "extension_ui_request" and msg.get("method") in {"select", "confirm", "input", "editor"}:
                await peer.send({"type": "extension_ui_response", "id": msg.get("id"), "cancelled": True})
                self._dispatch_event(request.agent_session_id, {
                    "type": "extension_error", "error": "此扩展请求终端交互，当前 Pi 聊天协议未接入该界面",
                })
                return
            self._dispatch_event(request.agent_session_id, msg)

        peer = StdioJsonlPeer(
            self._rpc_argv(
                approve=approve,
                no_session=options.ephemeral,
                session_dir=options.session_dir,
                chat_mode=options.is_conversation,
                mcp_extension=mcp_extension,
                extension_path=(
                    str(plugin_extension_path)
                    if plugin_extension_path else None)),
            cwd=cwd, env=env, label="pi.rpc",
            request_envelope=_pi_envelope,
            on_message=on_message,
        )
        try:
            await peer.start()
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
            if mcp_extension and startup_errors:
                first = startup_errors[0]
                raise PiRpcError(
                    "pi.mcp.extension_failed",
                    str(first.get("error") or "Pi MCP extension failed"),
                    command="extension",
                    data=first,
                )
        except BaseException:
            await peer.close()
            if mcp_extension and plugin_extension_path is not None:
                plugin_extension_path.unlink(missing_ok=True)
            raise

        session_id = str(stats.get("sessionId") or state.get("sessionId")
                         or "")
        resume_handle = str(stats.get("sessionFile") or state.get("sessionFile") or resume_handle or "")
        # 启动环境只应留在当前进程的 env；handle 后续可能被诊断代码读取，
        # 因而不保留 request.options 中的凭据环境副本。
        handle_options = options.model_copy(update={"env": {}})
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
                None if mcp_extension or plugin_extension_path is None
                else str(plugin_extension_path)),
            "mcp_extension": (
                str(plugin_extension_path) if mcp_extension else None),
            "entries_cursor": None,
            "entries_cursor_error": None,
            "get_messages_command": False,
            "fork_command": False,
            "user_entry_ids": [],
            "user_entry_baseline": False,
            "turn_user_baseline": [],
            "turn_entries": {},
            "active_branch_messages": None,
            "event_sink": None,
            "current_turn_id": None,
            "resumed": bool(request.resume_handle),
            "model_provider": model_provider,
            "model_id": model_id,
            "capability_revision": 0,
        }
        self._rpc[request.agent_session_id] = handle
        try:
            handle["turn_entries"] = self._load_turn_entries(
                request.agent_session_id)
            try:
                entries = await self._cmd(peer, "get_entries",
                                          timeout=self._startup_timeout)
            except PiRpcError as exc:
                if exc.code != "pi.rpc.command_failed":
                    raise
                handle["entries_cursor_error"] = exc.code
            else:
                # 0.84.1 实测字段为 leafId（核验文档按 lastEntryId 描述，
                # 两者都兜底）。
                handle["entries_cursor"] = (
                    entries.get("leafId") or entries.get("lastEntryId"))
                handle["user_entry_ids"] = _active_user_entry_ids(entries)
                handle["user_entry_baseline"] = True
            feature_errors = await self._confirm_rpc_features(peer)
            handle["get_messages_command"] = bool(
                self._rpc_features.get("get_messages"))
            handle["fork_command"] = bool(self._rpc_features.get("fork"))
            handle["rpc_feature_errors"] = feature_errors
        except BaseException:
            self._rpc.pop(request.agent_session_id, None)
            await peer.close()
            if mcp_extension and plugin_extension_path is not None:
                plugin_extension_path.unlink(missing_ok=True)
            raise
        return {
            "external_session_id": session_id or None,
            "resume_handle": resume_handle or None,
        }

    # -- 事件归一化 -------------------------------------------------------------

    def _dispatch_event(self, agent_session_id: str,
                        msg: dict[str, Any]) -> None:
        """Pi 事件流 → turn 队列（在 reader task 上运行，只 put_nowait）。"""
        handle = self._rpc.get(agent_session_id)
        if handle is None:
            return
        etype = msg.get("type")
        if etype in {"compaction_end", "auto_compaction_end"}:
            if isinstance(msg.get("result"), dict) and not msg.get("aborted") and not msg.get("errorMessage"):
                self._context_compacted(agent_session_id)
            return
        sink = handle.get("event_sink")
        if sink is None:
            return
        if etype == "extension_error":
            handle["turn_failed"] = str(msg.get("error") or "Pi 扩展执行失败")
        elif etype == "extension_ui_request" and msg.get("method") == "notify":
            text = str(msg.get("message") or "")
            if text:
                handle["assistant_text"] = str(handle.get("assistant_text") or "") + text + "\n"
                handle["saw_assistant_text"] = True
                sink.put_nowait((AgentEventType.MESSAGE_DELTA, "pi.command.notify", dump_payload(
                    MessageDeltaPayload(text=text + "\n",
                                        native={"message": msg.get("message")}))))
        elif etype == "message_update":
            # 累计 usage 在事件顶层（§PI 核验）。
            usage = msg.get("usage")
            if isinstance(usage, dict) and usage:
                sink.put_nowait((AgentEventType.USAGE_UPDATED,
                                 "pi.message_update",
                                 _pi_usage_payload(usage, str(handle.get("usage_message_index", 0)))))
            delta = msg.get("assistantMessageEvent") or {}
            if delta.get("type") == "text_delta" and delta.get("delta"):
                sink.put_nowait((AgentEventType.MESSAGE_DELTA,
                                 "pi.text_delta",
                                 dump_payload(MessageDeltaPayload(
                                     text=str(delta["delta"]),
                                     native={"content_index": delta.get("contentIndex")}))))
                handle["saw_assistant_text"] = True
        elif etype == "message_end":
            # 与 CLI PiDriver 一致：只投影 assistant；user/tool 的 message_end
            # 不能写成 MESSAGE_COMPLETED，否则界面会把用户原文当成 Agent 回复。
            message = msg.get("message") or {}
            if isinstance(message, dict) and message.get("role") == "custom" and message.get("display"):
                text = _pi_message_text(message)
                if text:
                    handle["assistant_text"] = str(handle.get("assistant_text") or "") + text
                    handle["saw_assistant_text"] = True
                    sink.put_nowait((AgentEventType.MESSAGE_DELTA, "pi.command.message", dump_payload(
                        MessageDeltaPayload(text=text, native={"message": message}))))
                return
            if not isinstance(message, dict) or message.get("role") != "assistant":
                return
            text = _pi_message_text(message)
            usage = message.get("usage")
            if isinstance(usage, dict) and usage:
                sink.put_nowait((AgentEventType.USAGE_UPDATED,
                                 "pi.message_end",
                                 _pi_usage_payload(usage, str(handle.get("usage_message_index", 0)))))
            handle["usage_message_index"] = int(handle.get("usage_message_index", 0)) + 1
            error_message = str(message.get("errorMessage") or "").strip()
            stop_reason = str(message.get("stopReason") or "").strip()
            if error_message or stop_reason in {"error", "aborted"}:
                detail = error_message or f"stopReason={stop_reason}"
                if _pi_is_user_abort(
                    stop_reason, abort_requested=bool(handle.get("abort_requested")),
                ):
                    # User interrupt: do not publish RUNTIME_ERROR / failed.
                    handle["turn_interrupted"] = detail or "aborted"
                    return
                handle["turn_failed"] = detail
                diagnostics = self._turn_diagnostics(handle)
                handle["turn_failure_diagnostics"] = diagnostics
                sink.put_nowait((AgentEventType.RUNTIME_ERROR,
                                 "pi.message_end",
                                 dump_payload(RuntimeErrorPayload(
                                     error=self.failure(
                                         FailureCategory.PROVIDER,
                                         "assistant_error",
                                         message=detail, detail=detail,
                                         native_code=stop_reason or None),
                                     native={"diagnostics": diagnostics}))))
                return
            if text:
                handle["assistant_text"] = text
                handle["saw_assistant_text"] = True
        elif etype == "tool_execution_start":
            sink.put_nowait((AgentEventType.TOOL_STARTED,
                             "pi.tool_execution_start",
                             dump_payload(ToolPayload(
                                 tool_call_id=str(msg.get("toolCallId") or ""),
                                 name=str(msg.get("toolName") or ""),
                                 input=msg.get("args"),
                                 status="running",
                             ))))
        elif etype == "tool_execution_update":
            sink.put_nowait((AgentEventType.TOOL_PROGRESS,
                             "pi.tool_execution_update",
                             dump_payload(ToolPayload(
                                 tool_call_id=str(msg.get("toolCallId") or ""),
                                 output=msg.get("partialResult"),
                                 status="running",
                             ))))
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
                             "pi.tool_execution_end",
                             dump_payload(ToolPayload(
                                 tool_call_id=str(msg.get("toolCallId") or ""),
                                 output=msg.get("result"),
                                 status="failed" if msg.get("isError") else "completed",
                             ))))
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
        if isinstance(input, MessageInput):
            return self._turn_stream(session, input)
        return self.unsupported_input_stream(session, input)

    async def _turn_stream(
        self, session: AgentSessionRef, input: MessageInput
    ) -> AsyncIterator[AgentEvent]:
        sid = session.agent_session_id
        handle = self._rpc.get(sid)
        if handle is None:
            async for event in self._unsupported_stream(session, "send",
                                                        "session"):
                yield event
            return
        peer: StdioJsonlPeer = handle["peer"]
        record = self._tracker.get(sid)
        turn_timeout = self.conversation_turn_timeout(
            handle.get("conversation_thread_id"), self._prompt_timeout)

        async def _abort_turn(_failure: Any) -> None:
            await self.interrupt(session)

        # The prompt response is only an ack: once it has arrived, a Pi
        # process that exits would otherwise leave the turn waiting forever.
        turn_id = new_id("turn")
        runner = TurnRunner(
            self, session, turn_id=turn_id,
            limits=TurnLimits(idle_s=turn_timeout, overall_s=turn_timeout),
            exit_watch=peer.wait_exit, on_abort=_abort_turn,
            diagnostics=peer.stderr_text, auto_ack=False,
            run_id=record.run_id if record else None,
            execution_generation=(
                record.execution_generation if record else None))
        runner.mark_sent()
        async for event in runner.stream(
                self._raw_turn_events(session, input, turn_id, runner)):
            yield event

    async def _raw_turn_events(
        self, session: AgentSessionRef, input: MessageInput,
        turn_id: str, runner: TurnRunner,
    ) -> AsyncIterator[AgentEvent]:
        sid = session.agent_session_id
        seq = self.sequencer_for(sid)
        handle = self._rpc[sid]
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
                payload=SessionPayload(
                    transport="rpc",
                    adapter_id=self.id,
                    instance_id=self.identity.instance_id,
                    cwd=handle["cwd"],
                    model=(
                        f"{handle['model_provider']}/{handle['model_id']}"
                        if handle.get("model_provider") and handle.get("model_id")
                        else handle.get("model_id")
                    ),
                    native={
                        "agent_plugin_extension": handle.get("agent_plugin_extension"),
                        "mcp_extension": handle.get("mcp_extension"),
                        "model_provider": handle.get("model_provider"),
                        "model_id": handle.get("model_id"),
                    }),
                **common))
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
        handle["turn_user_baseline"] = list(handle.get("user_entry_ids") or [])
        yield self.emit(build_event(
            AgentEventType.TURN_STARTED, seq,
            external_session_id=external_id, turn_id=turn_id,
            native_type="pi.prompt.start",
            payload=TurnStartedPayload(kind=input.kind),
            **common))

        queue: asyncio.Queue = asyncio.Queue()
        handle["event_sink"] = queue

        async def run_prompt() -> Optional[Exception]:
            try:
                # prompt 响应只是 ack；完成由 agent_end/agent_settled 事件
                # 判定（_dispatch_event 放 __turn_done__ 标记）。
                prompt_params: dict[str, Any] = {"message": input.text}
                images = pi_prompt_images(input.payload.attachments)
                if images:
                    prompt_params["images"] = images
                    handle["prompt_rpc"]["image_count"] = len(images)
                prompt_response = await self._cmd(peer, "prompt", prompt_params,
                                                  timeout=prompt_timeout)
                handle["prompt_rpc"]["acknowledged"] = True
                runner.ack()
                if input.payload.runtime_capability.get("verification") == "verified":
                    state = await self._cmd(peer, "get_state", timeout=self._startup_timeout)
                    if not state.get("isStreaming") and not state.get("isCompacting") and not state.get("pendingMessageCount"):
                        queue.put_nowait(("__turn_done__", "pi.command.completed", {
                            "local": True,
                            "prompt_response": prompt_response,
                            "state": state,
                        }))
                return None
            except Exception as exc:  # noqa: BLE001
                handle["prompt_rpc"]["error_type"] = type(exc).__name__
                return exc

        task = asyncio.ensure_future(run_prompt())
        ack_error: Optional[Exception] = None
        ack_handled = False
        done_info: dict[str, Any] = {}
        get_task = asyncio.ensure_future(queue.get())
        loop_finished = False
        try:
            while True:
                # 同时等事件与 prompt ack：ack 失败时不会到达任何事件，
                # 必须能被 task 完成唤醒，不能死等队列。
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
            loop_finished = True
        finally:
            get_task.cancel()
            if not loop_finished:
                # TurnRunner closed this source (process exit, limit, or the
                # consumer stopped): release the turn state it would leave.
                if not task.done():
                    task.cancel()
                handle["event_sink"] = None
                handle["current_turn_id"] = None
                handle["turns"] += 1
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
        verified_local = (
            input.payload.runtime_capability.get("verification") == "verified")
        if (
            not verified_local
            and handle.get("fork_command")
            and isinstance(handle.get("prompt_rpc"), dict)
            and handle["prompt_rpc"].get("acknowledged")
        ):
            async for event in self._record_turn_entry(
                session, seq, external_id, turn_id, common, handle, peer,
            ):
                yield event

        if ack_error is not None:
            if abort_requested:
                yield self.emit(build_event(
                    AgentEventType.TURN_FAILED, seq,
                    external_session_id=external_id, turn_id=turn_id,
                    native_type="pi.prompt.aborted",
                    payload=TurnFailedPayload(error=self.exception_failure(
                        ack_error, FailureCategory.CANCELLED, "interrupted",
                        message="Pi prompt aborted")),
                    **common))
                return
            yield self.emit(build_event(
                AgentEventType.TURN_FAILED, seq,
                external_session_id=external_id, turn_id=turn_id,
                native_type="pi.prompt.error",
                payload=TurnFailedPayload(
                    error=self.exception_failure(
                        ack_error, FailureCategory.TRANSPORT, "prompt_error",
                        message=f"Pi prompt failed: {ack_error}"),
                    native={"diagnostics": self._turn_diagnostics(handle)}),
                **common))
            return
        if turn_interrupted or abort_requested:
            yield self.emit(build_event(
                AgentEventType.TURN_FAILED, seq,
                external_session_id=external_id, turn_id=turn_id,
                native_type="pi.turn.interrupted",
                payload=TurnFailedPayload(error=self.failure(
                    FailureCategory.CANCELLED, "interrupted",
                    message="Pi turn aborted",
                    detail=str(turn_interrupted or turn_failed or ""))),
                **common))
            return
        if turn_failed:
            yield self.emit(build_event(
                AgentEventType.TURN_FAILED, seq,
                external_session_id=external_id, turn_id=turn_id,
                native_type="pi.assistant_error",
                payload=TurnFailedPayload(
                    error=self.failure(
                        FailureCategory.PROVIDER, "assistant_error",
                        message=str(turn_failed), detail=str(turn_failed)),
                    native={
                        "diagnostics": (
                            failure_diagnostics
                            if isinstance(failure_diagnostics, dict)
                            else self._turn_diagnostics(handle)
                        ),
                    }),
                **common))
            return
        assistant_text = str(handle.pop("assistant_text", ""))
        for event in self.completed_turn_events(
            seq, text=assistant_text,
            common={**common, "external_session_id": external_id, "turn_id": turn_id},
            native_type="pi.agent_end",
            payload=TurnCompletedPayload(native={"will_retry": bool(done_info.get("will_retry", False)), **dict(done_info)})):
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
            native_type="pi.session.resumed",
            payload=SessionPayload(
                transport="rpc",
                native={"entries_cursor": handle.get("entries_cursor")}),
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
        # Record operator abort before awaiting RPC so concurrent
        # message_end / tool_execution_end classify as interrupted.
        handle["abort_requested"] = True
        try:
            await self._cmd(handle["peer"], "abort")
            return self._receipt(session, True)
        except Exception as exc:  # noqa: BLE001
            handle.pop("abort_requested", None)
            return self._receipt(session, False, str(exc))

    def _turn_entry_path(self, agent_session_id: str) -> Path:
        return self._runtime_root / agent_session_id / "turn-entries.json"

    def _load_turn_entries(self, agent_session_id: str) -> dict[str, str]:
        path = self._turn_entry_path(agent_session_id)
        if not path.is_file():
            return {}
        try:
            raw = json.loads(path.read_text(encoding="utf-8"))
        except (OSError, UnicodeError, json.JSONDecodeError) as exc:
            raise PiRpcError(
                "pi.rewind.entry_index_invalid",
                "Pi turn entry index cannot be read",
                command=PI_FORK_COMMAND,
                data=type(exc).__name__,
            ) from exc
        if not isinstance(raw, dict):
            raise PiRpcError(
                "pi.rewind.entry_index_invalid",
                "Pi turn entry index is not an object",
                command=PI_FORK_COMMAND,
            )
        mapping: dict[str, str] = {}
        for key, value in raw.items():
            if not isinstance(key, str) or not isinstance(value, str) or not key or not value:
                raise PiRpcError(
                    "pi.rewind.entry_index_invalid",
                    "Pi turn entry index has a non-string mapping",
                    command=PI_FORK_COMMAND,
                )
            mapping[key] = value
        return mapping

    def _remember_turn_entry(
        self, handle: dict[str, Any], agent_session_id: str,
        turn_id: str, entry_id: str,
    ) -> None:
        mapping = dict(handle.get("turn_entries") or {})
        mapping[turn_id] = entry_id
        path = self._turn_entry_path(agent_session_id)
        path.parent.mkdir(parents=True, exist_ok=True)
        temporary = path.with_suffix(".json.tmp")
        temporary.write_text(
            json.dumps(mapping, ensure_ascii=False, indent=2) + "\n",
            encoding="utf-8",
        )
        temporary.chmod(0o600)
        temporary.replace(path)
        handle["turn_entries"] = mapping

    def _resolve_entry_id(self, handle: dict[str, Any], native_turn_id: str) -> str:
        mapping = handle.get("turn_entries") or {}
        if not isinstance(mapping, dict):
            return ""
        found = mapping.get(native_turn_id)
        if isinstance(found, str) and found:
            return found
        if native_turn_id in mapping.values():
            return native_turn_id
        return ""

    async def _capture_new_user_entry(
        self, peer: StdioJsonlPeer, handle: dict[str, Any],
    ) -> Optional[str]:
        data = await self._cmd(peer, "get_entries", timeout=self._startup_timeout)
        ids = _active_user_entry_ids(data)
        known = list(handle.get("turn_user_baseline") or [])
        handle["user_entry_ids"] = ids
        handle["entries_cursor"] = data.get("leafId") or data.get("lastEntryId")
        fresh = [entry_id for entry_id in ids if entry_id not in known]
        # The first new user entry is the turn boundary. Later steer messages
        # on the same turn stay after it.
        return fresh[0] if fresh else None

    async def _record_turn_entry(
        self,
        session: AgentSessionRef,
        seq: Any,
        external_id: Any,
        turn_id: str,
        common: dict[str, Any],
        handle: dict[str, Any],
        peer: StdioJsonlPeer,
    ) -> AsyncIterator[AgentEvent]:
        if not handle.get("user_entry_baseline"):
            yield self.emit(build_event(
                AgentEventType.RUNTIME_WARNING, seq,
                external_session_id=external_id, turn_id=turn_id,
                native_type="pi.rewind.baseline_missing",
                payload=RuntimeWarningPayload(
                    kind="protocol",
                    code="pi.rewind.baseline_missing",
                    message="Pi active-branch baseline is missing; this turn has no rewind entry",
                ),
                **common))
            return
        try:
            entry_id = await self._capture_new_user_entry(peer, handle)
            if entry_id:
                self._remember_turn_entry(
                    handle, session.agent_session_id, turn_id, entry_id)
        except (PiRpcError, OSError) as exc:
            code = exc.code if isinstance(exc, PiRpcError) else "pi.rewind.entry_index_write_failed"
            yield self.emit(build_event(
                AgentEventType.RUNTIME_WARNING, seq,
                external_session_id=external_id, turn_id=turn_id,
                native_type=code,
                payload=RuntimeWarningPayload(
                    kind="protocol",
                    code=code,
                    message=str(exc),
                ),
                **common))

    def supports_native_rewind(self, session: AgentSessionRef) -> bool:
        handle = self._rpc.get(session.agent_session_id)
        return bool(
            handle and handle.get("fork_command")
            and not handle.get("current_turn_id"))

    async def rewind_session(
        self, session: AgentSessionRef, native_turn_id: str,
    ) -> dict[str, Any]:
        """Drop ``native_turn_id`` and everything after it via RPC ``fork``.

        Pi 0.86.1 has no ``rewind`` command. ``fork {entryId}`` re-roots the
        live session on the parent of that user message.
        """
        handle = self._rpc.get(session.agent_session_id)
        if not handle or not self.supports_native_rewind(session):
            raise PiRpcError(
                "pi.rewind.unavailable",
                "this Pi session cannot rewind natively",
                command=PI_FORK_COMMAND,
            )
        entry_id = self._resolve_entry_id(handle, native_turn_id)
        if not entry_id:
            raise PiRpcError(
                "pi.rewind.entry_unknown",
                "the turn has no captured Pi session-tree entry",
                command=PI_FORK_COMMAND,
                data=native_turn_id,
            )
        data = await self._cmd(
            handle["peer"], PI_FORK_COMMAND, {"entryId": entry_id},
            timeout=self._startup_timeout)
        if data.get("cancelled") is True:
            raise PiRpcError(
                "pi.rewind.cancelled",
                "a Pi extension cancelled the session fork",
                command=PI_FORK_COMMAND,
                data=data,
            )
        state = await self._cmd(
            handle["peer"], "get_state", timeout=self._startup_timeout)
        session_file = state.get("sessionFile")
        if not isinstance(session_file, str) or not session_file:
            raise PiRpcError(
                "pi.rewind.no_session_file",
                "Pi fork did not return a persisted session file",
                command=PI_FORK_COMMAND,
                data=state,
            )
        handle["resume_handle"] = session_file
        if isinstance(state.get("sessionId"), str) and state.get("sessionId"):
            handle["external_session_id"] = state["sessionId"]
        if self._tracker.get(session.agent_session_id) is not None:
            self._tracker.activate(
                session.agent_session_id,
                external_session_id=handle.get("external_session_id"),
                resume_handle=session_file,
            )
        try:
            entries = await self._cmd(
                handle["peer"], "get_entries", timeout=self._startup_timeout)
            handle["entries_cursor"] = (
                entries.get("leafId") or entries.get("lastEntryId"))
            handle["user_entry_ids"] = _active_user_entry_ids(entries)
            handle["user_entry_baseline"] = True
            handle["entries_cursor_error"] = None
        except PiRpcError as exc:
            handle["entries_cursor_error"] = exc.code
        return {
            "strategy": "native",
            "method": PI_FORK_COMMAND,
            "entry_id": entry_id,
            "session_file": session_file,
            "cancelled": False,
        }

    async def fork_thread(
        self, session: AgentSessionRef, native_turn_id: str = "",
    ) -> dict[str, Any]:
        """Copy the session with ``pi --fork`` into the adapter runtime dir.

        The live RPC process stays on the source session. An optional turn
        id then applies RPC ``fork`` on the copy only.
        """
        handle = self._rpc.get(session.agent_session_id)
        if not handle or not handle.get("fork_command"):
            raise PiRpcError(
                "pi.fork.unavailable",
                "this Pi session has no confirmed fork command",
                command=PI_FORK_COMMAND,
            )
        if handle.get("current_turn_id"):
            raise PiRpcError(
                "pi.fork.busy",
                "cannot fork while a Pi turn is active",
                command=PI_FORK_COMMAND,
            )
        source = str(handle.get("resume_handle") or "")
        if not source or not Path(source).is_file():
            raise PiRpcError(
                "pi.fork.no_session_file",
                "Pi fork source has no session file",
                command=PI_FORK_COMMAND,
            )
        entry_id = ""
        if native_turn_id:
            entry_id = self._resolve_entry_id(handle, native_turn_id)
            if not entry_id:
                raise PiRpcError(
                    "pi.fork.entry_unknown",
                    "the fork boundary has no captured Pi session-tree entry",
                    command=PI_FORK_COMMAND,
                    data=native_turn_id,
                )
        fork_root = self._runtime_root / session.agent_session_id / "forks"
        session_dir = fork_root / "sessions"
        agent_dir = fork_root / "agent"
        session_dir.mkdir(parents=True, exist_ok=True)
        agent_dir.mkdir(parents=True, exist_ok=True)
        env = {
            key: value for key, value in dict(handle.get("env") or {}).items()
            if key not in {ENV_ENDPOINT, ENV_TOKEN}
        }
        env["PI_CODING_AGENT_DIR"] = str(agent_dir)
        env["PI_OFFLINE"] = "1"
        env["PI_SKIP_VERSION_CHECK"] = "1"
        argv = [
            self._binary, "--mode", "rpc", "--no-extensions", "--no-tools",
            "--offline", "--session-dir", str(session_dir), "--fork", source,
        ]

        async def on_helper_message(msg: dict[str, Any]) -> None:
            if msg.get("type") == "extension_ui_request" and msg.get("method") in {
                "select", "confirm", "input", "editor",
            }:
                await helper.send({
                    "type": "extension_ui_response",
                    "id": msg.get("id"),
                    "cancelled": True,
                })

        helper = StdioJsonlPeer(
            argv, cwd=str(handle.get("cwd") or os.getcwd()), env=env,
            label="pi.fork", request_envelope=_pi_envelope,
            on_message=on_helper_message,
        )
        state: dict[str, Any] = {}
        try:
            await helper.start()
            if entry_id:
                forked = await self._cmd(
                    helper, PI_FORK_COMMAND, {"entryId": entry_id},
                    timeout=self._startup_timeout)
                if forked.get("cancelled") is True:
                    raise PiRpcError(
                        "pi.fork.cancelled",
                        "a Pi extension cancelled the session fork",
                        command=PI_FORK_COMMAND,
                        data=forked,
                    )
            state = await self._cmd(
                helper, "get_state", timeout=self._startup_timeout)
        finally:
            await helper.close()
        session_file = state.get("sessionFile")
        if (
            not isinstance(session_file, str) or not session_file
            or os.path.realpath(session_file) == os.path.realpath(source)
        ):
            raise PiRpcError(
                "pi.fork.no_distinct_session",
                "Pi fork did not create a distinct session file",
                command=PI_FORK_COMMAND,
                data=state,
            )
        return {
            "thread_id": session_file,
            "session_file": session_file,
            "root_session_id": source,
            "entry_id": entry_id or None,
        }

    async def read_active_branch_messages(
        self, session: AgentSessionRef,
    ) -> list[dict[str, Any]]:
        """Active-branch messages from RPC ``get_messages``.

        ``get_entries`` includes abandoned branches and is not used here.
        """
        handle = self._rpc.get(session.agent_session_id)
        if handle is None or not handle.get("get_messages_command"):
            raise PiRpcError(
                "pi.snapshot.unsupported",
                "Pi get_messages is not available for this session",
                command=PI_GET_MESSAGES_COMMAND,
            )
        data = await self._cmd(
            handle["peer"], PI_GET_MESSAGES_COMMAND,
            timeout=self._startup_timeout)
        messages = _normalize_active_messages(data)
        handle["active_branch_messages"] = messages
        return messages

    async def snapshot(self, session: AgentSessionRef) -> AgentSessionSnapshot:
        snap = await super().snapshot(session)
        handle = self._rpc.get(session.agent_session_id)
        if handle is None or not handle.get("get_messages_command"):
            return snap
        messages = await self.read_active_branch_messages(session)
        usage = dict(snap.usage)
        usage["active_branch_messages"] = messages
        return snap.model_copy(update={"usage": usage})

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
                f"Pi get_commands 读取失败：{type(exc).__name__}: {exc}"
            )
            return base
        handle["capability_revision"] = int(
            handle.get("capability_revision") or 0
        ) + 1
        items = list(base.items)
        from .rpc_commands import rpc_operation_items
        operations = rpc_operation_items(self.id, "pi")
        items.extend(operations)
        operation_names = {item.name for item in operations}
        if handle.get("fork_command"):
            items.append(operation_item(
                self.id, "pi", "rewind", PI_FORK_COMMAND,
                "用 Pi fork 把活动分支回退到该用户消息之前；工作区文件保持原状",
            ))
            operation_names.add("rewind")
        for raw in result.get("commands") or []:
            if not isinstance(raw, dict):
                continue
            name = str(raw.get("name") or "").strip().lstrip("/")
            if not name or name in operation_names:
                continue
            source = str(raw.get("source") or "")
            items.append(dynamic_command_item(
                adapter_id=self.id,
                engine="pi",
                name=name,
                description=str(raw.get("description") or ""),
                argument_hint=str((raw.get("input") or {}).get("hint") or raw.get("argumentHint") or ""),
                channel="provider_native",
                kind="skill" if source.casefold() == "skill"
                or name.startswith("skill:") else "command",
                invocation={
                    "command": name,
                    "wire_text": f"/{name}",
                    "protocol": "pi.rpc.prompt",
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
        mcp_extension = handle.get("mcp_extension")
        if mcp_extension:
            # Rewritten by the next launch; leaving it would accumulate one
            # copy per session under the runtime root.
            Path(mcp_extension).unlink(missing_ok=True)
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
    "PiRpcError",
]
