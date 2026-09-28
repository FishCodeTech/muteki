"""共享 ACP（Agent Client Protocol v1）transport 与 Adapter 基类（RUNTIME-03）。

协议依据 docs/research/third_party_verification.md §ACP（ACP 稳定协议
版本 = 整数 ``1``；schema 仓库 agentclientprotocol/agent-client-protocol）：

- stdio + JSON-RPC 2.0，换行分隔 JSON（分帧由 ``rpc.py`` 保证）；
- ``initialize`` 能力协商：Client 报 ``protocolVersion: 1``，响应含
  ``agentCapabilities``（``loadSession``、``sessionCapabilities.resume/
  close/list``、``mcpCapabilities.http/sse``）与 ``authMethods``；
  能力门控是硬要求——不支持的能力 Client MUST NOT 调用；
- Session 生命周期：``session/new{cwd, mcpServers}``、
  ``session/load{sessionId, cwd, mcpServers}``（以 ``session/update``
  重放全部历史后才响应）、``session/resume``（同参数，不重放，
  2026-04-22 已稳定）；三个注入点均可携带 ``mcpServers``；
- ``session/prompt{sessionId, prompt: ContentBlock[]}`` 只返回
  ``{stopReason}``，正文经 ``session/update`` 的 ``agent_message_chunk``
  流式到达；``session/cancel`` 为 notification，Agent 中止后须以
  ``stopReason: "cancelled"`` 响应原 prompt；
- 审批：Agent→Client 反向请求 ``session/request_permission``，Client
  必须应答（``{outcome:{outcome:"selected",optionId}}`` 或
  ``cancelled``），不答复会阻塞工具执行。

**replay 与 live event 区分**（任务书 RUNTIME-03 硬性要求）：一条
``session/update`` 只有在该 session 有进行中的 ``session/prompt`` 时才是
live；``session/load`` 的历史回放（协议保证全部回放先于 load 响应到达）
以及其他任何非 prompt 窗口内到达的 update 一律标记 ``replay=True``，
只入 ``AcpTransport.replay_log`` / Adapter 的回放清单，**不进入事件投影**，
恢复时不会重复写历史消息。generation fencing 由 RUNTIME-01 的
``EventProjector`` 兜底。

RUNTIME-04 的 Kimi ACP / OMP ACP 直接复用 ``AcpTransport`` 与
``BaseAcpAdapter``，只覆盖 argv、auth 选择与能力差异钩子。
"""

from __future__ import annotations

import asyncio
import inspect
import os
from dataclasses import dataclass, field
from typing import Any, AsyncIterator, Awaitable, Callable, Optional

from muteki.capability_bindings.acp_config import TOKEN_PLACEHOLDER
from muteki.platform.contracts.base import new_id
from muteki.platform.contracts.capabilities import CapabilityInjectionPlan
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
from muteki.platform.contracts.receipts import (
    AggregateRef,
    CommandReceipt,
    ReceiptState,
)

from .base import BaseExternalAgentAdapter
from .approvals import ApprovalDecision, ApprovalScope
from .capabilities import (
    BOOL_CAPABILITY_FIELDS,
    CapabilityProbeReport,
    SOURCE_PROBE,
    SOURCE_STATIC,
    _probe_version,
    conservative_capabilities,
)
from .events import build_event
from .rpc import PeerClosedError, StdioJsonlPeer
from .runtime_capabilities import (
    RuntimeCapabilitySnapshot,
    dynamic_command_item,
)
from .sessions import EXIT_INTERRUPTED, EXIT_RESUMABLE, classify_exit
from .user_input_schema import (
    content_for_elicitation,
    normalize_pending_user_input,
    questions_from_schema,
)

#: ACP 稳定协议主版本（核验结论，不接受 v2 draft）。
ACP_PROTOCOL_VERSION = 1


class AcpError(RuntimeError):
    """ACP 调用失败（对端返回 error 或违反协议时序）。"""


@dataclass
class AcpHello:
    """``initialize`` 协商结果的能力视图（只保留 Muteki 关心的字段）。"""

    protocol_version: int = 0
    load_session: bool = False
    resume: bool = False
    close: bool = False
    list_sessions: bool = False
    mcp_http: bool = False
    mcp_sse: bool = False
    agent_info: dict[str, Any] = field(default_factory=dict)
    auth_methods: list[dict[str, Any]] = field(default_factory=list)
    raw: dict[str, Any] = field(default_factory=dict)

    @classmethod
    def from_result(cls, result: dict[str, Any]) -> "AcpHello":
        caps = result.get("agentCapabilities") or {}
        session_caps = caps.get("sessionCapabilities") or {}
        mcp_caps = caps.get("mcpCapabilities") or {}
        return cls(
            protocol_version=int(result.get("protocolVersion") or 0),
            load_session=bool(caps.get("loadSession")),
            resume="resume" in session_caps,
            close="close" in session_caps,
            list_sessions="list" in session_caps,
            mcp_http=bool(mcp_caps.get("http")),
            mcp_sse=bool(mcp_caps.get("sse")),
            agent_info=dict(result.get("agentInfo") or {}),
            auth_methods=list(result.get("authMethods") or []),
            raw=dict(result),
        )


def check_response(method: str, resp: dict[str, Any]) -> dict[str, Any]:
    """解开 JSON-RPC 响应；对端 error 抛 ``AcpError``。"""
    if "error" in resp and resp["error"] is not None:
        err = resp["error"]
        message = err.get("message") if isinstance(err, dict) else str(err)
        raise AcpError(f"{method} failed: {message}")
    result = resp.get("result")
    return result if isinstance(result, dict) else {}


#: ``session/update`` 回调签名：``(session_id, update, replay)``。
UpdateHandler = Callable[[str, dict[str, Any], bool], None]
#: ``session/request_permission`` 回调签名：params → optionId（None = cancelled）。
PermissionHandler = Callable[
    [dict[str, Any]], Optional[str] | Awaitable[Optional[str]]
]
#: ACP form elicitation callback: params -> protocol result.
ElicitationHandler = Callable[
    [dict[str, Any]], dict[str, Any] | Awaitable[dict[str, Any]]
]


class AcpTransport:
    """一个 ACP agent 子进程的协议层（无语义状态之外的任何业务逻辑）。

    - ``on_update``：所有 ``session/update`` 通知（含 replay 标记）；
    - ``permission_handler``：``session/request_permission`` 审批策略，
      必须返回一个 optionId，返回 None 表示 cancelled；
    - 其他 Agent→Client 方法（Cursor 扩展 ``cursor/ask_question`` 等）
      与未知通知计入 ``stats["unhandled"]``，不改变核心状态机。
    """

    def __init__(
        self,
        argv: list[str],
        *,
        cwd: Optional[str] = None,
        env: Optional[dict[str, str]] = None,
        client_name: str = "muteki",
        client_version: str = "0.1.0",
        on_update: Optional[UpdateHandler] = None,
        permission_handler: Optional[PermissionHandler] = None,
        elicitation_handler: Optional[ElicitationHandler] = None,
        on_raw_line: Optional[Callable[[str], None]] = None,
    ) -> None:
        self._client_info = {"name": client_name, "version": client_version}
        self._on_update = on_update
        self._permission_handler = permission_handler
        self._elicitation_handler = elicitation_handler
        #: 有进行中 session/prompt 的 sessionId 集合——replay 判定的唯一依据。
        self._active_prompts: set[str] = set()
        self._replaying_sessions: set[str] = set()
        #: 回放事件流水（按到达顺序），供 Adapter 恢复时检阅而不投影。
        self.replay_log: list[dict[str, Any]] = []
        self.hello: Optional[AcpHello] = None
        self._session_setup: dict[str, dict[str, Any]] = {}
        self.stats = {"unhandled": 0, "replay_updates": 0, "live_updates": 0}
        self._peer = StdioJsonlPeer(
            argv, cwd=cwd, env=env, label="acp",
            on_message=self._on_message, on_raw_line=on_raw_line)

    @property
    def peer(self) -> StdioJsonlPeer:
        return self._peer

    @property
    def running(self) -> bool:
        return self._peer.running

    async def start(self) -> None:
        await self._peer.start()

    async def close(self) -> int:
        return await self._peer.close()

    # -- 协商与认证 -----------------------------------------------------------

    async def initialize(self, *, timeout: float = 30.0) -> AcpHello:
        resp = await self._peer.request("initialize", {
            "protocolVersion": ACP_PROTOCOL_VERSION,
            "clientCapabilities": {
                "fs": {"readTextFile": False, "writeTextFile": False},
                "terminal": False,
                "elicitation": {"form": {}},
            },
            "clientInfo": dict(self._client_info),
        }, timeout=timeout)
        hello = AcpHello.from_result(check_response("initialize", resp))
        if hello.protocol_version > ACP_PROTOCOL_VERSION:
            # 协商规则：Agent 返回的版本 Client 不支持时应断开。
            await self.close()
            raise AcpError(
                f"unsupported ACP protocol version {hello.protocol_version}")
        self.hello = hello
        return hello

    async def authenticate(
        self, method_id: str, *, meta: Optional[dict[str, Any]] = None,
        timeout: float = 60.0,
    ) -> None:
        params: dict[str, Any] = {"methodId": method_id}
        if meta:
            params["_meta"] = meta
        check_response("authenticate",
                       await self._peer.request("authenticate", params,
                                                timeout=timeout))

    # -- Session 生命周期 -------------------------------------------------------

    async def new_session(
        self, cwd: str, mcp_servers: list[dict[str, Any]], *,
        timeout: float = 60.0,
    ) -> str:
        resp = await self._peer.request("session/new", {
            "cwd": cwd, "mcpServers": list(mcp_servers),
        }, timeout=timeout)
        result = check_response("session/new", resp)
        session_id = str(result.get("sessionId") or "")
        if not session_id:
            raise AcpError("session/new returned no sessionId")
        self._session_setup[session_id] = result
        return session_id

    async def load_session(
        self, session_id: str, cwd: str, mcp_servers: list[dict[str, Any]], *,
        timeout: float = 120.0,
    ) -> None:
        """``session/load``：回放的历史 update 全部由 replay 判定标记。"""
        if self.hello is not None and not self.hello.load_session:
            raise AcpError("agent does not support session/load "
                           "(agentCapabilities.loadSession is false)")
        self._replaying_sessions.add(session_id)
        try:
            result = check_response("session/load", await self._peer.request(
                "session/load", {"sessionId": session_id, "cwd": cwd,
                                 "mcpServers": list(mcp_servers)}, timeout=timeout))
        finally:
            self._replaying_sessions.discard(session_id)
        self._session_setup[session_id] = result

    async def resume_session(
        self, session_id: str, cwd: str, mcp_servers: list[dict[str, Any]], *,
        timeout: float = 60.0,
    ) -> None:
        """``session/resume``：不重放历史（仅在 resume 能力存在时调用）。"""
        if self.hello is not None and not self.hello.resume:
            raise AcpError("agent does not support session/resume "
                           "(sessionCapabilities.resume absent)")
        result = check_response("session/resume", await self._peer.request(
            "session/resume",
            {"sessionId": session_id, "cwd": cwd,
             "mcpServers": list(mcp_servers)},
            timeout=timeout))
        self._session_setup[session_id] = result

    def session_setup(self, session_id: str) -> dict[str, Any]:
        """Return the Agent-reported setup state for one ACP session."""
        return dict(self._session_setup.get(session_id) or {})

    async def set_mode(
        self, session_id: str, mode_id: str, *, timeout: float = 30.0
    ) -> None:
        check_response("session/set_mode", await self._peer.request(
            "session/set_mode",
            {"sessionId": session_id, "modeId": mode_id},
            timeout=timeout,
        ))

    # -- Turn -----------------------------------------------------------------

    async def prompt(
        self, session_id: str, text: str, *,
        timeout: Optional[float] = 600.0,
    ) -> dict[str, Any]:
        """一个 prompt turn；返回 ``{stopReason, ...}``。

        ``timeout`` 为 ``None`` 或 ``<= 0`` 时不设上限。对话模式会显式
        传入 ``None``；做题 Worker 仍走默认 600 秒。
        """
        self._active_prompts.add(session_id)
        try:
            resp = await self._peer.request("session/prompt", {
                "sessionId": session_id,
                "prompt": [{"type": "text", "text": text}],
            }, timeout=timeout)
            return check_response("session/prompt", resp)
        finally:
            self._active_prompts.discard(session_id)

    async def cancel(self, session_id: str) -> None:
        """``session/cancel`` notification；Agent 须以 cancelled 结束 prompt。"""
        await self._peer.notify("session/cancel", {"sessionId": session_id})

    async def list_sessions(
        self, *, timeout: float = 30.0
    ) -> list[dict[str, Any]]:
        """``session/list``（2026 年稳定；仅在 list 能力存在时调用）。"""
        if self.hello is not None and not self.hello.list_sessions:
            raise AcpError("agent does not support session/list "
                           "(sessionCapabilities.list absent)")
        resp = await self._peer.request("session/list", {}, timeout=timeout)
        result = check_response("session/list", resp)
        sessions = result.get("sessions")
        return list(sessions) if isinstance(sessions, list) else []

    # -- 入站分发 ----------------------------------------------------------------

    async def _on_message(self, msg: dict[str, Any]) -> None:
        method = msg.get("method")
        if method == "session/update":
            params = msg.get("params") or {}
            session_id = str(params.get("sessionId") or "")
            update = params.get("update") or {}
            replay = session_id in self._replaying_sessions
            if replay:
                self.stats["replay_updates"] += 1
                self.replay_log.append(
                    {"session_id": session_id, "update": update})
            else:
                self.stats["live_updates"] += 1
            if self._on_update is not None:
                self._on_update(session_id, update, replay)
            return
        if method == "session/request_permission" and "id" in msg:
            option_id: Optional[str] = None
            if self._permission_handler is not None:
                decision = self._permission_handler(msg.get("params") or {})
                option_id = (
                    await decision if inspect.isawaitable(decision) else decision
                )
            if option_id is None:
                await self._peer.respond(
                    msg["id"], result={"outcome": {"outcome": "cancelled"}})
            else:
                await self._peer.respond(msg["id"], result={
                    "outcome": {"outcome": "selected", "optionId": option_id}})
            return
        if method in {"elicitation/create", "session/elicitation"} and "id" in msg:
            result: dict[str, Any] = {"action": "cancel"}
            if self._elicitation_handler is not None:
                response = self._elicitation_handler(msg.get("params") or {})
                result = (
                    await response if inspect.isawaitable(response) else response
                )
            await self._peer.respond(msg["id"], result=result)
            return
        self.stats["unhandled"] += 1


# ---------------------------------------------------------------------------
# session/update → 统一 AgentEvent 的归一化（纯函数，便于单测）
# ---------------------------------------------------------------------------

def _content_text(content: Any) -> str:
    """ContentBlock / content 数组 → 纯文本（只取 text 块）。"""
    if isinstance(content, str):
        return content
    if isinstance(content, dict):
        if content.get("text") is not None:
            return str(content.get("text") or "")
        # Devin wraps terminal output as
        # {type: "content", content: {type: "text", text: "..."}}.
        # Resource-backed previews use the same shape with one more `resource`
        # level. Recurse through those protocol containers so tool output is not
        # discarded before it reaches Conversation or a solving Worker.
        for key in ("content", "resource"):
            nested = content.get(key)
            if nested is not None:
                text = _content_text(nested)
                if text:
                    return text
        return ""
    if isinstance(content, list):
        return "".join(_content_text(block) for block in content)
    return ""


def normalize_session_update(
    update: dict[str, Any]
) -> "list[tuple[AgentEventType, str, dict[str, Any]]]":
    """把一条 ``session/update`` 的 ``update`` 字段归一化为统一事件。

    返回 ``[(AgentEventType, native_kind, payload), ...]``；未识别的
    ``sessionUpdate`` 取值返回空列表（由调用方计数，不改变核心状态机）。
    """
    kind = str(update.get("sessionUpdate") or "")
    if kind == "user_message_chunk":
        # ACP 会把本轮用户输入作为 session/update 回显。它属于输入确认，
        # 绝不能进入助手消息流或最终正文。
        return []
    if kind == "agent_thought_chunk":
        # ACP exposes raw thought chunks, not an explicit reasoning summary.
        # Keep them inside the Runtime boundary.
        return []
    if kind == "agent_message_chunk":
        text = _content_text(update.get("content"))
        if not text:
            return []
        return [(AgentEventType.MESSAGE_DELTA, f"acp.{kind}", {
            "text": text,
            "role": "agent",
            "message_id": update.get("messageId"),
        })]
    if kind == "tool_call":
        return [(AgentEventType.TOOL_STARTED, "acp.tool_call", {
            "call_id": str(update.get("toolCallId") or ""),
            "tool": str(update.get("title") or update.get("kind") or ""),
            "status": update.get("status"),
            "input": update.get("rawInput"),
        })]
    if kind == "tool_call_update":
        status = str(update.get("status") or "")
        payload = {
            "call_id": str(update.get("toolCallId") or ""),
            "status": status or None,
            "output": _content_text(update.get("content")) or None,
        }
        if status in ("completed", "failed"):
            return [(AgentEventType.TOOL_COMPLETED, "acp.tool_call_update",
                     payload)]
        return [(AgentEventType.TOOL_PROGRESS, "acp.tool_call_update", payload)]
    if kind == "plan":
        entries: list[dict[str, Any]] = []
        for index, entry in enumerate(update.get("entries") or []):
            if isinstance(entry, str):
                text = entry.strip()
                if text:
                    entries.append({
                        "task_id": f"acp-step-{index + 1}",
                        "title": text,
                        "status": "pending",
                    })
                continue
            if not isinstance(entry, dict):
                continue
            text = str(
                entry.get("content")
                or entry.get("title")
                or entry.get("description")
                or ""
            ).strip()
            task_id = str(
                entry.get("id")
                or entry.get("task_id")
                or entry.get("entryId")
                or f"acp-step-{index + 1}"
            ).strip()
            if not text and not task_id:
                continue
            entries.append({
                "task_id": task_id,
                "title": text or task_id,
                "status": entry.get("status") or "pending",
            })
        if not entries:
            return []
        phase = "proposed"
        if any(
            str(row.get("status") or "").lower() in (
                "in_progress", "running", "active",
            )
            for row in entries
        ):
            phase = "executing"
        return [(AgentEventType.PLAN_UPDATED, "acp.plan", {
            "tasks": entries,
            "phase": phase,
            "source": "adapter",
            "patch": False,
        })]
    if kind == "usage_update":
        usage = {
            "used": update.get("used"),
            "size": update.get("size"),
        }
        if isinstance(update.get("cost"), dict):
            usage["cost"] = update["cost"]
        return [(AgentEventType.USAGE_UPDATED, "acp.usage_update",
                 {"usage": usage})]
    if kind == "available_commands_update":
        commands: list[dict[str, Any]] = []
        raw_commands = (
            update.get("availableCommands")
            or update.get("available_commands")
            or update.get("commands")
            or []
        )
        for raw in raw_commands:
            if not isinstance(raw, dict):
                continue
            name = str(raw.get("name") or "").strip().lstrip("/")
            if not name:
                continue
            commands.append({
                "name": name,
                "description": str(raw.get("description") or "").strip(),
                "argument_hint": str(
                    (raw.get("input") or {}).get("hint")
                    or raw.get("inputHint")
                    or raw.get("input_hint")
                    or raw.get("argumentHint")
                    or ""
                ).strip(),
                "source": str(raw.get("source") or "").strip(),
            })
        return [(
            AgentEventType.RUNTIME_CAPABILITIES_UPDATED,
            "acp.available_commands_update",
            {"commands": commands},
        )]
    return []


def materialize_mcp_servers(
    plan: Optional[CapabilityInjectionPlan],
    bearer_token: Optional[str],
) -> list[dict[str, Any]]:
    """从注入计划展开 ACP ``mcpServers`` 数组并替换 bearer 占位符。

    接受两种形态：ACP_MCP_CONFIG 计划的 list 形态（CAP-02 builder 产出），
    以及 MCP 计划的 ``{name: entry}`` dict 形态（兜底转换为 ACP HTTP 元素）。
    token 缺失时占位符被替换为空串并跳过该 server（不注入无效凭据）。
    """
    if plan is None:
        return []
    cfg = (plan.runtime_config or {}).get("mcpServers")
    if not cfg:
        return []
    if isinstance(cfg, dict):
        entries = []
        for name, entry in cfg.items():
            item = dict(entry or {})
            item.setdefault("name", name)
            headers = item.get("headers")
            if isinstance(headers, dict):
                item["headers"] = [
                    {"name": str(k), "value": str(v)}
                    for k, v in headers.items()]
            entries.append(item)
    else:
        entries = [dict(entry) for entry in cfg]
    out: list[dict[str, Any]] = []
    for entry in entries:
        headers = []
        usable = True
        for header in entry.get("headers") or []:
            header = dict(header)
            value = str(header.get("value") or "")
            if TOKEN_PLACEHOLDER in value:
                if not bearer_token:
                    usable = False
                    break
                value = value.replace(TOKEN_PLACEHOLDER, bearer_token)
            header["value"] = value
            headers.append(header)
        if not usable:
            continue
        entry["headers"] = headers
        out.append(entry)
    return out


class BaseAcpAdapter(BaseExternalAgentAdapter):
    """ACP Runtime 的 ExternalAgentAdapter 基类（Cursor/Grok 共用）。

    子类只需提供：

    - ``_agent_argv()``：ACP agent 进程命令行（cwd 隔离由 transport 保证）；
    - ``_select_auth_method(auth_methods)``：从 ``authMethods`` 选择认证
      方式（返回 None 表示无需认证）；
    - ``_authenticate_meta()``：认证附加 ``_meta``（如 Grok 的
      ``{"headless": true}`` 扩展）；
    - 可选 ``_probe_extra_caps(caps, hello)``：补充 Runtime 特有能力。

    Agent 自己决定哪些操作需要确认；Muteki 只把 ACP 原生权限请求接到
    Conversation 的审批卡，并根据当前会话的 access mode 应答。
    """

    adapter_id = "acp.base"

    def __init__(
        self,
        adapter_id: Optional[str] = None,
        *,
        instance_id: str = "default",
        store: Any = None,
        binding_service: Any = None,
        gateway_endpoint: str = "",
        descriptor_provider: Any = None,
        default_cwd: Optional[str] = None,
        env_extra: Optional[dict[str, str]] = None,
        client_name: str = "muteki",
        startup_timeout: float = 30.0,
        prompt_timeout: float = 600.0,
    ) -> None:
        super().__init__(
            adapter_id or self.adapter_id,
            instance_id=instance_id,
            store=store,
            binding_service=binding_service,
            gateway_endpoint=gateway_endpoint,
            descriptor_provider=descriptor_provider,
        )
        self._default_cwd = default_cwd
        self._env_extra = dict(env_extra or {})
        self._client_name = client_name
        self._startup_timeout = float(startup_timeout)
        self._prompt_timeout = float(prompt_timeout)
        # agent_session_id -> ACP 会话句柄
        self._acp: dict[str, dict[str, Any]] = {}
        # agent_session_id -> 恢复上下文（plan/token/cwd/options，供 resume 重连）
        self._resume_ctx: dict[str, dict[str, Any]] = {}

    # -- 子类钩子 -----------------------------------------------------------

    def _agent_argv(self) -> list[str]:
        raise NotImplementedError

    def _agent_argv_for_request(self, request: SessionStart) -> list[str]:
        """Return this Agent's native launch command for one Conversation session.

        Providers override this hook when their own ACP command exposes native
        autonomy controls.  The shared ACP layer never keeps a provider flag map.
        """
        return self._agent_argv()

    def _prepare_session_environment(
        self, request: SessionStart, env: dict[str, str], cwd: str
    ) -> dict[str, str]:
        """Provider hook for per-session environment isolation."""
        return env

    def _select_auth_method(
        self, auth_methods: list[dict[str, Any]]
    ) -> Optional[str]:
        return str(auth_methods[0].get("id")) if auth_methods else None

    def _select_session_auth_method(
        self,
        auth_methods: list[dict[str, Any]],
        env: dict[str, str],
    ) -> Optional[str]:
        """Select auth with access to the final per-session environment."""
        return self._select_auth_method(auth_methods)

    def _authenticate_meta(self) -> Optional[dict[str, Any]]:
        return None

    def _probe_extra_caps(self, caps: AgentCapabilities, hello: AcpHello) -> None:
        """子类补充 Runtime 特有能力（默认无）。"""

    async def _after_session_open(
        self, transport: "AcpTransport", session_id: str, request: SessionStart
    ) -> None:
        """子类钩子：session/new|load|resume 成功后的 Runtime 特有动作。"""

    # -- probe（真实 initialize 协商） -----------------------------------------

    def _turn_result_usage(self, result: dict[str, Any]) -> dict[str, Any]:
        """Provider-specific per-turn usage returned by session/prompt."""
        return {}

    async def probe(self, request: ProbeRequest) -> AgentCapabilities:
        argv = self._agent_argv()
        field_sources: dict[str, str] = {}
        degradations: list[str] = []
        caps = conservative_capabilities(
            transport_kind="acp", capability_source=SOURCE_STATIC)
        binary = argv[0] if argv else ""
        hello: Optional[AcpHello] = None
        detail = ""
        try:
            transport = AcpTransport(argv, client_name=self._client_name)
            await transport.start()
            try:
                hello = await transport.initialize(
                    timeout=self._startup_timeout)
            finally:
                await transport.close()
        except (OSError, AcpError, asyncio.TimeoutError, PeerClosedError) as exc:
            detail = f"ACP initialize 失败：{str(exc)[:160]}"
            degradations.append(detail)

        if hello is None:
            for field_name in BOOL_CAPABILITY_FIELDS:
                field_sources[field_name] = SOURCE_STATIC
            degradations.append(
                "structured transport 不可用：streaming/approval/resume/"
                "interrupt 均未实测，按保守默认 False（不静默降级）")
            self._probe_cache = CapabilityProbeReport(
                adapter_id=self.identity.adapter_id,
                instance_id=self.identity.instance_id,
                capabilities=caps, binary_path=binary,
                field_sources=field_sources, degradations=degradations,
                detail=detail or "binary 不可用或 initialize 失败",
            )
            return caps

        # 实测能力（capability_source=probe，逐字段标注）。
        caps.capability_source = SOURCE_PROBE
        caps.protocol_version = str(hello.protocol_version)
        caps.runtime_version = str(
            hello.agent_info.get("version") or "")[:120]
        if not caps.runtime_version and binary:
            # 部分 ACP Agent 的 initialize 响应省略版本号。协议协商已经
            # 成功时，再用真实 ``--version`` 输出补齐展示与健康判定。
            caps.runtime_version = _probe_version(binary)
        caps.streaming = True
        caps.tool_events = True
        caps.approval = True
        caps.access_modes = list(ACCESS_MODE_VALUES)
        caps.interrupt = True
        caps.usage_events = True
        # ACP sessionUpdate.kind == "plan" is first-class in the wire protocol.
        caps.plan = True
        caps.resume = hello.resume or hello.load_session
        caps.session_persistence = caps.resume
        # ACP 注入走 mcpServers（HTTP 形态需 mcpCapabilities.http 门控）：
        # 不声明通用 mcp 位，避免 select_injection_kind 选错计划形态。
        caps.acp_mcp_config = hello.mcp_http
        self._probe_extra_caps(caps, hello)
        for field_name in BOOL_CAPABILITY_FIELDS:
            field_sources[field_name] = SOURCE_PROBE
        if not hello.mcp_http:
            degradations.append(
                "mcpCapabilities.http=false：无法注入 HTTP 形态 mcpServers，"
                "能力注入将降级（由 select_injection_kind 选择后续档位）")
        if not hello.resume and hello.load_session:
            degradations.append(
                "无 session/resume 能力：恢复走 session/load（重放历史，"
                "回放事件标记 replay 且不进入投影）")
        if not caps.resume:
            degradations.append("无 load/resume 能力：Session 不可恢复")
        self._probe_cache = CapabilityProbeReport(
            adapter_id=self.identity.adapter_id,
            instance_id=self.identity.instance_id,
            capabilities=caps, binary_path=binary,
            field_sources=field_sources, degradations=degradations,
            detail=f"ACP initialize 协商成功（protocolVersion "
                   f"{hello.protocol_version}）",
        )
        return caps

    # -- 启动 / 接管 -----------------------------------------------------------

    async def _launch(
        self,
        request: SessionStart,
        plan: Optional[CapabilityInjectionPlan],
        bearer_token: Optional[str],
    ) -> dict[str, Any]:
        cwd = str(request.options.get("cwd") or self._default_cwd
                  or os.getcwd())
        mcp_servers = materialize_mcp_servers(plan, bearer_token)
        env = dict(self._env_extra)
        env.update({k: str(v)
                    for k, v in (request.options.get("env") or {}).items()})
        env = self._prepare_session_environment(request, env, cwd)
        access_mode = str(
            request.access_mode or AccessMode.SUPERVISED.value
        ).strip()
        if access_mode not in ACCESS_MODE_VALUES:
            raise ValueError(
                f"{self.adapter_id} unsupported access mode: {access_mode}")

        handle: dict[str, Any] = {
            "cwd": cwd,
            "env": env,
            "options": dict(request.options),
            "thread_id": request.thread_id,
            "turns": 0,
            "transport": None,
            "external_session_id": None,
            "replay_events": [],
            "event_sink": None,
            "current_turn_id": None,
            "access_mode": access_mode,
            "pending_approvals": {},
            "pending_user_inputs": {},
            "available_commands": [],
            "capability_revision": 0,
        }
        transport = AcpTransport(
            self._agent_argv_for_request(request), cwd=cwd, env=env,
            client_name=self._client_name,
            on_update=lambda sid, upd, replay: self._dispatch_update(
                request.agent_session_id, sid, upd, replay),
            permission_handler=lambda params: self._request_permission(
                request.agent_session_id, params),
            elicitation_handler=lambda params: self._request_elicitation(
                request.agent_session_id, params),
        )
        # 先注册句柄再启动：session/load 的历史回放在 load 响应之前到达，
        # 必须能被 _dispatch_update 找到句柄记入 replay_events。
        self._acp[request.agent_session_id] = handle
        await transport.start()
        try:
            hello = await transport.initialize(timeout=self._startup_timeout)
            method_id = self._select_session_auth_method(
                hello.auth_methods, env)
            if method_id:
                await transport.authenticate(
                    method_id, meta=self._authenticate_meta())

            resume_handle = request.resume_handle
            if resume_handle:
                # 恢复：优先 session/resume（不重放），load 为降级（重放
                # 历史，回放事件只入 replay_events，不进投影）。
                if hello.resume:
                    await transport.resume_session(
                        resume_handle, cwd, mcp_servers)
                elif hello.load_session:
                    await transport.load_session(
                        resume_handle, cwd, mcp_servers)
                else:
                    raise AcpError(
                        "agent supports neither session/resume nor "
                        "session/load; cannot resume session")
                session_id = resume_handle
                handle["resumed"] = True
            else:
                session_id = await transport.new_session(
                    cwd, mcp_servers, timeout=120 if self.id == "cursor.acp" else 60)
            # 子类钩子：session 建立后的 Runtime 特有动作（如 Kimi
            # session/set_model）；默认无操作。
            await self._after_session_open(transport, session_id, request)
        except Exception:
            await transport.close()
            self._acp.pop(request.agent_session_id, None)
            raise

        handle["transport"] = transport
        handle["external_session_id"] = session_id
        self._resume_ctx[request.agent_session_id] = {
            "plan": plan, "token": bearer_token, "cwd": cwd,
            "options": dict(request.options),
            "access_mode": access_mode,
            "model": request.model,
            "effort": request.effort,
            "thread_id": request.thread_id,
        }
        return {"external_session_id": session_id, "resume_handle": session_id}

    # -- update / 审批分发 ------------------------------------------------------

    def _handle_for(self, agent_session_id: str) -> Optional[dict[str, Any]]:
        return self._acp.get(agent_session_id)

    def _dispatch_update(
        self, agent_session_id: str, session_id: str,
        update: dict[str, Any], replay: bool,
    ) -> None:
        """transport 回调：replay 入回放清单（不投影），live 入 turn 队列。"""
        handle = self._handle_for(agent_session_id)
        if handle is None:
            return
        if handle.get("external_session_id") and session_id != handle["external_session_id"]:
            # Native workflows can announce child sessions on the same ACP
            # connection. Their command catalogs never replace the parent's.
            return
        mapped = normalize_session_update(update)
        if str(update.get("sessionUpdate") or "") == "available_commands_update":
            commands = list(mapped[0][2].get("commands") or []) if mapped else []
            handle["available_commands"] = commands
            handle["capability_revision"] = int(
                handle.get("capability_revision") or 0
            ) + 1
            mapped = [
                (etype, native_kind, {
                    **payload,
                    "revision": handle["capability_revision"],
                    "adapter_id": self.id,
                })
                for etype, native_kind, payload in mapped
            ]
        if replay:
            # 历史回放：记录但绝不进入投影（不重复写历史消息）。
            handle["replay_events"].append({
                "session_id": session_id,
                "updates": [kind for _t, kind, _p in mapped],
                "replay": True,
            })
            return
        if not mapped:
            handle["unmapped_updates"] = handle.get("unmapped_updates", 0) + 1
            return
        sink = handle.get("event_sink")
        if sink is not None:
            for etype, native_kind, payload in mapped:
                sink.put_nowait(("update", etype, native_kind, payload))
        elif callable(handle.get("background_handler")):
            handle["background_handler"](mapped)

    def bind_background_handler(self, session: AgentSessionRef, handler) -> None:
        handle = self._handle_for(session.agent_session_id)
        if handle is not None:
            handle["background_handler"] = handler

    async def runtime_operation(self, session: AgentSessionRef, name: str, arguments: str = "") -> dict[str, Any]:
        handle = self._handle_for(session.agent_session_id)
        if not handle or handle.get("current_turn_id"):
            raise RuntimeError("请等待当前回复结束后切换模式")
        transport = handle["transport"]
        sid = handle["external_session_id"]
        setup = transport.session_setup(sid)
        available = (setup.get("modes") or {}).get("availableModes") or []
        mode = next((row for row in available if str(row.get("id")) == name), None)
        if mode is None:
            raise RuntimeError("当前 ACP 会话未公布此模式")
        if arguments:
            raise ValueError(f"请先执行 /{name} 切换模式，再发送任务")
        await transport.set_mode(sid, name)
        transport._session_setup[sid].setdefault("modes", {})["currentModeId"] = name
        return {"status": "completed", "message": f"当前引擎已切换到 {mode.get('name') or name} 模式"}

    async def runtime_capability_snapshot(
        self, session: Optional[AgentSessionRef] = None
    ) -> RuntimeCapabilitySnapshot:
        base = await super().runtime_capability_snapshot(session)
        if session is None:
            return base
        handle = self._handle_for(session.agent_session_id)
        if handle is None:
            return base.model_copy(update={
                "stale": True,
                "diagnostics": ["ACP Session 当前不在本进程，动态命令目录已失效"],
            })
        engine = self.id.split(".", 1)[0]
        items = list(base.items)
        for command in handle.get("available_commands") or []:
            name = str(command.get("name") or "").strip().lstrip("/")
            if not name:
                continue
            source = str(command.get("source") or "").casefold()
            kind = "skill" if (
                name.startswith("skill:") or "skill" in source
            ) else "command"
            items.append(dynamic_command_item(
                adapter_id=self.id,
                engine=engine,
                name=name,
                description=str(command.get("description") or ""),
                argument_hint=str(command.get("argument_hint") or ""),
                channel="acp_prompt",
                kind=kind,
                invocation={
                    "command": name,
                    "wire_text": f"/{name}",
                    "protocol": "acp.session/prompt",
                },
            ))
        if engine == "grok":
            for item in items:
                if item.name == "context":
                    item.invocation["native_wire_text"] = "/session-info"
                    item.description = "查看 Grok 原生会话与上下文用量"
        from .command_providers import operation_item
        setup = handle["transport"].session_setup(handle["external_session_id"])
        names = {item.name for item in items}
        for mode in (setup.get("modes") or {}).get("availableModes") or []:
            name = str(mode.get("id") or "")
            if name and name not in names:
                items.append(operation_item(self.id, engine, name, "session/set_mode",
                    f"切换到 {mode.get('name') or name} 模式"))
        return base.model_copy(update={
            "revision": int(handle.get("capability_revision") or 0),
            "items": items,
        })

    @staticmethod
    def _permission_option(
        options: list[dict[str, Any]], kinds: tuple[str, ...]
    ) -> Optional[str]:
        for kind in kinds:
            selected = next(
                (option for option in options if option.get("kind") == kind),
                None,
            )
            if selected is not None and selected.get("optionId") is not None:
                return str(selected["optionId"])
        return None

    async def _request_permission(
        self, agent_session_id: str, params: dict[str, Any]
    ) -> Optional[str]:
        """Route a native ACP permission request to the selected access mode."""
        options = [
            dict(option) for option in (params.get("options") or [])
            if isinstance(option, dict)
        ]
        tool_call = params.get("toolCall") or {}
        handle = self._handle_for(agent_session_id)
        if handle is None:
            return None
        access_mode = str(handle.get("access_mode") or "")
        tool_kind = str(tool_call.get("kind") or "other")

        # These decisions are made by the ACP client exactly as the protocol
        # intends. Provider-native modes run first and only requests that reach
        # this callback are considered here.
        if access_mode == AccessMode.FULL_ACCESS.value:
            return self._permission_option(
                options, ("allow_always", "allow_once"))
        if (
            access_mode == AccessMode.AUTO_ACCEPT_EDITS.value
            and tool_kind in {"edit", "delete", "move"}
        ):
            return self._permission_option(
                options, ("allow_once", "allow_always"))

        sink = self._interaction_sink(handle)
        if sink is None:
            return None
        approval_id = str(
            params.get("requestId")
            or tool_call.get("toolCallId")
            or new_id("approval")
        )
        loop = asyncio.get_running_loop()
        future: asyncio.Future[Optional[str]] = loop.create_future()
        handle["pending_approvals"][approval_id] = {
            "future": future,
            "options": options,
            "params": dict(params),
        }
        sink.put_nowait(("approval", AgentEventType.APPROVAL_REQUESTED,
                         "acp.request_permission", {
                             "approval_id": approval_id,
                             "tool": str(tool_call.get("title") or ""),
                             "action": str(
                                 tool_call.get("title")
                                 or tool_call.get("kind")
                                 or "Agent operation"),
                             "tool_kind": tool_kind,
                             "tool_call": {
                                 "call_id": tool_call.get("toolCallId"),
                                 "title": tool_call.get("title"),
                                 "kind": tool_kind,
                             },
                             "options": [
                                 {"option_id": option.get("optionId"),
                                  "kind": option.get("kind")}
                                 for option in options],
                             "access_mode": access_mode,
                         }))
        try:
            return await future
        finally:
            handle["pending_approvals"].pop(approval_id, None)

    @staticmethod
    def _interaction_sink(handle: dict[str, Any]):
        if handle.get("event_sink") is not None:
            return handle["event_sink"]
        callback = handle.get("background_handler")
        if not callable(callback):
            return None
        class BackgroundSink:
            def put_nowait(self, item):
                callback([item[1:]])
        return BackgroundSink()

    @staticmethod
    def _elicitation_properties(
        params: dict[str, Any]
    ) -> dict[str, dict[str, Any]]:
        schema = params.get("requestedSchema") or {}
        properties = schema.get("properties") if isinstance(schema, dict) else {}
        if not isinstance(properties, dict):
            return {}
        return {
            str(key): dict(value)
            for key, value in properties.items()
            if isinstance(value, dict)
        }

    @classmethod
    def _approval_elicitation_content(
        cls, params: dict[str, Any]
    ) -> Optional[dict[str, Any]]:
        """Recognize the standard single-field approve/deny form.

        OMP's own approval UI is a ``select`` bridged to an ACP form with one
        string enum (currently ``Approve`` / ``Deny``).  Returning the positive
        value lets Conversation render its normal approval card while all
        other forms continue through the user-input UI.
        """
        properties = cls._elicitation_properties(params)
        if len(properties) != 1:
            return None
        key, prop = next(iter(properties.items()))
        values = prop.get("enum")
        if not isinstance(values, list):
            return None
        positive = next(
            (value for value in values
             if str(value).strip().lower() in {"approve", "allow", "yes"}),
            None,
        )
        negative = next(
            (value for value in values
             if str(value).strip().lower() in {"deny", "reject", "no"}),
            None,
        )
        if positive is None or negative is None:
            return None
        return {key: positive}

    @staticmethod
    def _coerce_elicitation_value(value: str, prop: dict[str, Any]) -> Any:
        kind = str(prop.get("type") or "string")
        if kind == "boolean":
            return value.strip().lower() in {
                "1", "true", "yes", "y", "是", "允许", "同意",
            }
        if kind == "integer":
            return int(value)
        if kind == "number":
            return float(value)
        if kind == "array":
            return [part.strip() for part in value.split(",") if part.strip()]
        return value

    @classmethod
    def _elicitation_content_from_text(
        cls, params: dict[str, Any], text: str
    ) -> dict[str, Any]:
        properties = cls._elicitation_properties(params)
        if len(properties) == 1:
            key, prop = next(iter(properties.items()))
            return {key: cls._coerce_elicitation_value(text, prop)}
        try:
            import json
            parsed = json.loads(text)
        except (TypeError, ValueError):
            parsed = None
        if not isinstance(parsed, dict):
            return {}
        return {
            key: cls._coerce_elicitation_value(str(parsed[key]), prop)
            for key, prop in properties.items()
            if key in parsed
        }

    async def _request_elicitation(
        self, agent_session_id: str, params: dict[str, Any]
    ) -> dict[str, Any]:
        """Route an ACP form to Conversation approval or user-input UI."""
        handle = self._handle_for(agent_session_id)
        if handle is None or str(params.get("mode") or "form") != "form":
            return {"action": "cancel"}
        sink = self._interaction_sink(handle)
        if sink is None:
            return {"action": "cancel"}
        request_id = str(params.get("elicitationId") or new_id("elicitation"))
        loop = asyncio.get_running_loop()
        future: asyncio.Future[dict[str, Any]] = loop.create_future()
        message = str(params.get("message") or "Agent 请求输入")
        positive_content = self._approval_elicitation_content(params)
        if positive_content is not None:
            handle["pending_approvals"][request_id] = {
                "future": future,
                "options": [],
                "params": dict(params),
                "kind": "elicitation",
                "positive_content": positive_content,
            }
            sink.put_nowait(("approval", AgentEventType.APPROVAL_REQUESTED,
                             "acp.elicitation", {
                                 "approval_id": request_id,
                                 "action": message,
                                 "reason": message,
                                 "access_mode": handle.get("access_mode"),
                             }))
            try:
                return await future
            finally:
                handle["pending_approvals"].pop(request_id, None)

        handle["pending_user_inputs"][request_id] = {
            "future": future,
            "params": dict(params),
        }
        questions = questions_from_schema(
            params.get("requestedSchema") or {},
            title=message,
        )
        pending = normalize_pending_user_input({
            "request_id": request_id,
            "user_input_kind": "acp.elicitation",
            "title": message,
            "message": message,
            "question": message,
            "questions": questions,
            "schema": params.get("requestedSchema") or {},
            "native": params,
        })
        sink.put_nowait(("user_input", AgentEventType.USER_INPUT_REQUESTED,
                         "acp.elicitation", pending))
        try:
            return await future
        finally:
            handle["pending_user_inputs"].pop(request_id, None)

    # -- turn 流 --------------------------------------------------------------

    def send(
        self, session: AgentSessionRef, input: AgentInput
    ) -> AsyncIterator[AgentEvent]:
        if input.kind == "approval_response":
            return self._approval_response_stream(session, input)
        if input.kind == "user_input_response":
            return self._user_input_response_stream(session, input)
        return self._prompt_stream(session, input)

    async def _approval_response_stream(
        self, session: AgentSessionRef, input: AgentInput
    ) -> AsyncIterator[AgentEvent]:
        sid = session.agent_session_id
        seq = self.sequencer_for(sid)
        handle = self._handle_for(sid)
        external_id = (
            handle.get("external_session_id") if handle
            else session.external_session_id
        )
        record = self._tracker.get(sid)
        common = dict(
            agent_session_id=sid,
            external_session_id=external_id,
            run_id=record.run_id if record else None,
            execution_generation=(
                record.execution_generation if record else None),
            turn_id=handle.get("current_turn_id") if handle else None,
        )
        try:
            decision = ApprovalDecision.from_payload(input.payload)
        except ValueError as exc:
            yield self.emit(build_event(
                AgentEventType.RUNTIME_ERROR, seq,
                native_type="acp.permission.invalid",
                payload={"code": "acp.approval.invalid",
                         "detail": str(exc)},
                **common,
            ))
            return
        pending = (
            handle.get("pending_approvals", {}).get(decision.approval_id)
            if handle else None
        )
        if pending is None:
            yield self.emit(build_event(
                AgentEventType.RUNTIME_ERROR, seq,
                native_type="acp.permission.stale",
                payload={"code": "acp.approval.stale",
                         "detail": "ACP approval request is no longer pending"},
                **common,
            ))
            return
        options = pending["options"]
        if pending.get("kind") == "elicitation":
            future = pending["future"]
            result = (
                {"action": "accept", "content": pending["positive_content"]}
                if decision.allowed else {"action": "decline"}
            )
            if not future.done():
                future.set_result(result)
            yield self.emit(build_event(
                AgentEventType.APPROVAL_RESOLVED, seq,
                native_type="acp.elicitation",
                payload={
                    "approval_id": decision.approval_id,
                    "decision": decision.choice.value,
                    "scope": decision.scope.value,
                    "outcome": result["action"],
                },
                **common,
            ))
            return
        if decision.allowed:
            wanted = (
                ("allow_always", "allow_once")
                if decision.scope is ApprovalScope.SESSION
                else ("allow_once", "allow_always")
            )
        else:
            wanted = (
                ("reject_always", "reject_once")
                if decision.scope is ApprovalScope.SESSION
                else ("reject_once", "reject_always")
            )
        option_id = self._permission_option(options, wanted)
        future = pending["future"]
        if not future.done():
            future.set_result(option_id)
        yield self.emit(build_event(
            AgentEventType.APPROVAL_RESOLVED, seq,
            native_type="acp.request_permission",
            payload={
                "approval_id": decision.approval_id,
                "decision": decision.choice.value,
                "scope": decision.scope.value,
                "option_id": option_id,
                "outcome": "selected" if option_id else "cancelled",
            },
            **common,
        ))

    async def _user_input_response_stream(
        self, session: AgentSessionRef, input: AgentInput
    ) -> AsyncIterator[AgentEvent]:
        sid = session.agent_session_id
        seq = self.sequencer_for(sid)
        handle = self._handle_for(sid)
        request_id = str(input.payload.get("request_id") or "")
        pending = (
            handle.get("pending_user_inputs", {}).get(request_id)
            if handle else None
        )
        record = self._tracker.get(sid)
        common = dict(
            agent_session_id=sid,
            external_session_id=(handle or {}).get("external_session_id"),
            run_id=record.run_id if record else None,
            execution_generation=(
                record.execution_generation if record else None),
            turn_id=(handle or {}).get("current_turn_id"),
        )
        if pending is None:
            yield self.emit(build_event(
                AgentEventType.RUNTIME_ERROR, seq,
                native_type="acp.elicitation.stale",
                payload={"code": "acp.elicitation.stale",
                         "detail": "ACP elicitation is no longer pending"},
                **common,
            ))
            return
        decision = str(input.payload.get("decision") or "submit").strip().lower()
        structured = dict(input.payload.get("answers") or {})
        if decision == "cancel":
            result = {"action": "decline"}
        elif structured:
            schema = (
                (pending.get("params") or {}).get("requestedSchema")
                if isinstance(pending.get("params"), dict)
                else {}
            )
            content = content_for_elicitation(
                {
                    str(qid): (
                        value if isinstance(value, dict)
                        else {"values": [str(value)], "text": str(value)}
                    )
                    for qid, value in structured.items()
                },
                schema=schema if isinstance(schema, dict) else {},
            )
            result = (
                {"action": "accept", "content": content}
                if content else {"action": "decline"}
            )
        else:
            content = self._elicitation_content_from_text(
                pending["params"], input.text)
            result = (
                {"action": "accept", "content": content}
                if content else {"action": "decline"}
            )
        future = pending["future"]
        if not future.done():
            future.set_result(result)
        yield self.emit(build_event(
            AgentEventType.USER_INPUT_RESOLVED, seq,
            native_type="acp.elicitation",
            payload={
                "request_id": request_id,
                "decision": decision,
                "answered": result.get("action") == "accept",
            },
            **common,
        ))

    async def _prompt_stream(
        self, session: AgentSessionRef, input: AgentInput
    ) -> AsyncIterator[AgentEvent]:
        sid = session.agent_session_id
        seq = self.sequencer_for(sid)
        handle = self._handle_for(sid)
        if handle is None or handle.get("transport") is None:
            async for event in self._unsupported_stream(session, "send",
                                                        "session"):
                yield event
            return
        transport: AcpTransport = handle["transport"]
        record = self._tracker.get(sid)
        external_id = (handle.get("external_session_id")
                       or session.external_session_id)
        common = dict(
            agent_session_id=sid,
            run_id=record.run_id if record else None,
            execution_generation=(
                record.execution_generation if record else None),
        )

        if handle["turns"] == 0:
            started_type = (AgentEventType.SESSION_RESUMED
                            if handle.get("resumed")
                            else AgentEventType.SESSION_STARTED)
            yield self.emit(build_event(
                started_type, seq,
                external_session_id=external_id,
                native_type="acp.session.load" if handle.get("resumed")
                            else "acp.session.new",
                payload={
                    "transport": "acp",
                    "adapter_id": self.id,
                    "instance_id": self.identity.instance_id,
                    "cwd": handle["cwd"],
                    "replay_updates": len(handle["replay_events"]),
                },
                **common))
        turn_id = new_id("turn")
        handle["current_turn_id"] = turn_id
        yield self.emit(build_event(
            AgentEventType.TURN_STARTED, seq,
            external_session_id=external_id, turn_id=turn_id,
            native_type="acp.prompt.start",
            payload={"kind": input.kind},
            **common))

        queue: asyncio.Queue = asyncio.Queue()
        handle["event_sink"] = queue

        async def run_prompt() -> dict[str, Any]:
            try:
                return await transport.prompt(
                    external_id, input.text,
                    timeout=self.conversation_turn_timeout(
                        handle.get("thread_id"), self._prompt_timeout))
            except Exception as exc:  # noqa: BLE001
                return {"__error__": exc}
            finally:
                await queue.put(("done", None, None, None))

        task = asyncio.ensure_future(run_prompt())
        stop_reason = ""
        error: Optional[Exception] = None
        text_parts: list[str] = []
        while True:
            kind, etype, native_kind, payload = await queue.get()
            if kind == "done":
                break
            if (etype is AgentEventType.MESSAGE_DELTA
                    and payload.get("role", "agent") == "agent"
                    and not payload.get("thinking")
                    and payload.get("phase") != "commentary"):
                text_parts.append(str(payload.get("text") or ""))
            yield self.emit(build_event(
                etype, seq,
                external_session_id=external_id, turn_id=turn_id,
                native_type=native_kind, payload=payload,
                **common))
        result = await task
        handle["event_sink"] = None
        handle["turns"] += 1
        handle["current_turn_id"] = None

        if isinstance(result.get("__error__"), Exception):
            error = result["__error__"]
            timed_out = isinstance(error, asyncio.TimeoutError)
            err_text = str(error).strip()[:300] or type(error).__name__
            yield self.emit(build_event(
                AgentEventType.TURN_FAILED, seq,
                external_session_id=external_id, turn_id=turn_id,
                native_type="acp.prompt.error",
                payload={"error": err_text, "timed_out": timed_out},
                **common))
            yield self.emit(build_event(
                AgentEventType.RUNTIME_EXITED, seq,
                external_session_id=external_id, turn_id=turn_id,
                native_type="acp.exit",
                payload={"classification": classify_exit(
                    error=err_text, timed_out=timed_out)},
                **common))
            return

        stop_reason = str(result.get("stopReason") or "")
        result_usage = self._turn_result_usage(result)
        if result_usage:
            yield self.emit(build_event(
                AgentEventType.USAGE_UPDATED, seq,
                external_session_id=external_id, turn_id=turn_id,
                native_type="acp.prompt.usage",
                payload={"usage": result_usage}, **common))
        if stop_reason == "cancelled":
            # 中断：只分类收尾，不伪造正常完成。
            yield self.emit(build_event(
                AgentEventType.TURN_FAILED, seq,
                external_session_id=external_id, turn_id=turn_id,
                native_type="acp.prompt.cancelled",
                payload={"reason": EXIT_INTERRUPTED,
                         "stop_reason": stop_reason},
                **common))
        elif stop_reason == "refusal":
            yield self.emit(build_event(
                AgentEventType.TURN_FAILED, seq,
                external_session_id=external_id, turn_id=turn_id,
                native_type="acp.prompt.refusal",
                payload={"stop_reason": stop_reason},
                **common))
        elif not "".join(text_parts).strip() and (input.payload.get("runtime_capability") or {}).get("verification") == "verified":
            capability = input.payload["runtime_capability"]
            yield self.emit(build_event(
                AgentEventType.MESSAGE_COMPLETED, seq,
                external_session_id=external_id, turn_id=turn_id,
                native_type="acp.command.completed",
                payload={"text": f"引擎已处理 /{capability.get('name', '')}（未返回文本）", "role": "assistant"},
                **common))
            yield self.emit(build_event(
                AgentEventType.TURN_COMPLETED, seq,
                external_session_id=external_id, turn_id=turn_id,
                native_type="acp.prompt.completed", payload={"stop_reason": stop_reason or "end_turn"}, **common))
        elif not "".join(text_parts).strip():
            yield self.emit(build_event(
                AgentEventType.TURN_FAILED, seq,
                external_session_id=external_id, turn_id=turn_id,
                native_type="acp.empty_assistant",
                payload={
                    "reason": "empty_assistant",
                    "error": {
                        "code": "acp.empty_assistant",
                        "message": "ACP turn ended without assistant text",
                    },
                    "stop_reason": stop_reason,
                },
                **common))
        else:
            yield self.emit(build_event(
                AgentEventType.MESSAGE_COMPLETED, seq,
                external_session_id=external_id, turn_id=turn_id,
                native_type="acp.message.completed",
                payload={"text": "".join(text_parts), "role": "assistant"},
                **common))
            yield self.emit(build_event(
                AgentEventType.TURN_COMPLETED, seq,
                external_session_id=external_id, turn_id=turn_id,
                native_type="acp.prompt.completed",
                payload={"stop_reason": stop_reason or "end_turn"},
                **common))

    def resume(self, session: AgentSessionRef) -> AsyncIterator[AgentEvent]:
        """恢复：回放事件带 ``replay`` 标记透出但不投影，随后可继续 prompt。"""
        return self._resume_stream(session)

    async def _resume_stream(
        self, session: AgentSessionRef
    ) -> AsyncIterator[AgentEvent]:
        sid = session.agent_session_id
        seq = self.sequencer_for(sid)
        handle = self._handle_for(sid)
        if handle is None:
            # Adapter 进程内无句柄：尝试用 resume 上下文重连（重走注入）。
            ctx = self._resume_ctx.get(sid)
            external = session.external_session_id or session.resume_handle
            if ctx is None or not external:
                async for event in self._unsupported_stream(
                        session, "resume", "resume"):
                    yield event
                return
            request = SessionStart(
                agent_session_id=sid,
                thread_id=ctx.get("thread_id"),
                execution_generation=(
                    self._tracker.current_generation(sid)),
                resume_handle=external,
                model=ctx.get("model"),
                effort=ctx.get("effort"),
                access_mode=ctx.get("access_mode"),
                options=dict(ctx["options"]),
            )
            try:
                await self._launch(request, ctx["plan"], ctx["token"])
            except Exception as exc:  # noqa: BLE001
                yield self.emit(build_event(
                    AgentEventType.RUNTIME_ERROR, seq,
                    agent_session_id=sid, external_session_id=external,
                    payload={"code": "external_agent.acp.resume_failed",
                             "detail": str(exc)[:200]},
                ))
                return
            handle = self._handle_for(sid)
        assert handle is not None
        external_id = handle.get("external_session_id")
        record = self._tracker.get(sid)
        common = dict(
            agent_session_id=sid,
            run_id=record.run_id if record else None,
            execution_generation=(
                record.execution_generation if record else None),
        )
        # 回放事件：透出给调用方检阅历史，但不投影（不重复写历史消息）。
        replayed = list(handle["replay_events"])
        for item in replayed:
            yield build_event(
                AgentEventType.MESSAGE_DELTA, seq,
                external_session_id=external_id,
                native_type="acp.replay",
                payload={"replay": True, "updates": item["updates"]},
                **common)
        yield self.emit(build_event(
            AgentEventType.SESSION_RESUMED, seq,
            external_session_id=external_id,
            native_type="acp.session.resumed",
            payload={"transport": "acp",
                     "replay_updates": len(replayed)},
            **common))

    # -- 控制面 ---------------------------------------------------------------

    async def interrupt(self, session: AgentSessionRef) -> CommandReceipt:
        handle = self._handle_for(session.agent_session_id)
        transport = (handle or {}).get("transport")
        external_id = (handle or {}).get("external_session_id") \
            or session.external_session_id
        if transport is None or not external_id:
            return self.unsupported_receipt(
                "interrupt", "no_active_session", session=session)
        await transport.cancel(external_id)
        return CommandReceipt(
            command_id=new_id("cmd"),
            state=ReceiptState.COMPLETED,
            aggregate=AggregateRef(type="agent_session",
                                   id=session.agent_session_id),
        )

    async def _teardown(self, session: AgentSessionRef) -> str:
        handle = self._acp.pop(session.agent_session_id, None)
        if not handle:
            return EXIT_RESUMABLE if session.resume_handle else "closed"
        for pending in handle.get("pending_approvals", {}).values():
            future = pending.get("future")
            if future is not None and not future.done():
                if pending.get("kind") == "elicitation":
                    future.set_result({"action": "cancel"})
                else:
                    future.set_result(None)
        for pending in handle.get("pending_user_inputs", {}).values():
            future = pending.get("future")
            if future is not None and not future.done():
                future.set_result({"action": "cancel"})
        transport: Optional[AcpTransport] = handle.get("transport")
        returncode: Optional[int] = None
        if transport is not None:
            returncode = await transport.close()
        return classify_exit(
            returncode=returncode,
            cancelled=handle.get("current_turn_id") is not None,
            resume_handle=handle.get("external_session_id"),
        )

    def replay_events(self, agent_session_id: str) -> list[dict[str, Any]]:
        """该 Session 收到的历史回放记录（未投影）。"""
        handle = self._handle_for(agent_session_id)
        return list((handle or {}).get("replay_events") or [])


__all__ = [
    "ACP_PROTOCOL_VERSION",
    "AcpError",
    "AcpHello",
    "AcpTransport",
    "BaseAcpAdapter",
    "check_response",
    "materialize_mcp_servers",
    "normalize_session_update",
]
