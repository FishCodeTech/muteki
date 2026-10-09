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
import json
import inspect
import os
from dataclasses import dataclass, field
from typing import Any, AsyncIterator, Awaitable, Callable, Optional

from muteki.capability_bindings.acp_config import TOKEN_PLACEHOLDER
from muteki.platform.contracts.agent_events import (
    AgentEventContractError,
    AgentFailure,
    AgentNodePayload,
    ApprovalOption,
    ApprovalRequestedPayload,
    ApprovalResolvedPayload,
    AgentUpdatedPayload,
    FailureCategory,
    MessageCompletedPayload,
    MessageDeltaPayload,
    PlanPayload,
    PlanTaskPayload,
    ReasoningPayload,
    RuntimeCapabilitiesPayload,
    RuntimeErrorPayload,
    RuntimeExitedPayload,
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
from muteki.platform.contracts.capabilities import CapabilityInjectionPlan
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
    UserInputResponseInput,
)
from muteki.platform.contracts.protocols import (
    BackgroundUpdateAdapter,
    RuntimeOperationAdapter,
)
from muteki.platform.contracts.receipts import (
    AggregateRef,
    CommandReceipt,
    ReceiptState,
)

from .base import BaseExternalAgentAdapter
from .approvals import (
    ApprovalDecision,
    ApprovalScope,
    ApprovalScopeError,
    ApprovalTarget,
    SessionApprovalGrants,
    native_decision,
    reject_mode_upgrade,
)
from .capabilities import (
    BOOL_CAPABILITY_FIELDS,
    CapabilityProbeReport,
    SOURCE_PROBE,
    SOURCE_STATIC,
    probe_version,
    conservative_capabilities,
    require_access_mode,
)
from .events import build_event
from .rpc import PeerClosedError, StdioJsonlPeer
from .runtime_capabilities import (
    RuntimeCapabilitySnapshot,
    dynamic_command_item,
)
from .sessions import EXIT_RESUMABLE, classify_exit
from .user_input_schema import (
    UserInputValidationError,
    content_for_elicitation,
    questions_from_schema,
)

def _wire_answer(value: Any) -> str:
    if isinstance(value, bool):
        return "true" if value else "false"
    return str(value)


#: ACP 稳定协议主版本（核验结论，不接受 v2 draft）。
ACP_PROTOCOL_VERSION = 1


class AcpError(RuntimeError):
    """ACP 调用失败（对端返回 error 或违反协议时序）。"""


class AcpRequestError(AcpError):
    """对端以 JSON-RPC error 应答；携带稳定的 ``code``/``data``。

    调用方按 ``code`` 分类（例如 Grok 的 -32003 用量限制），绝不匹配
    ``message`` 文字。作为 ``AcpError`` 的子类，既有 ``except AcpError``
    路径行为不变。
    """

    def __init__(self, message: str, *, code: Optional[int] = None,
                 data: Any = None) -> None:
        super().__init__(message)
        self.code = code
        self.data = data


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
        code = err.get("code") if isinstance(err, dict) else None
        raise AcpRequestError(
            f"{method} failed: {message}",
            code=code if isinstance(code, int) and not isinstance(code, bool) else None,
            data=err.get("data") if isinstance(err, dict) else None)
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
#: Agent->Client request served off the read loop (it may wait for a user or a
#: child process); raise ``AcpRequestError`` to answer with a JSON-RPC error.
ClientRequestHandler = Callable[[dict[str, Any]], Awaitable[dict[str, Any]]]


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
        control_delivery_handler: Optional[Callable[[dict[str, Any], dict[str, Any]], None]] = None,
        extension_handler: Optional[Callable[[str, dict[str, Any], bool], Optional[dict[str, Any]]]] = None,
        on_raw_line: Optional[Callable[[str], None]] = None,
        initialize_meta: Optional[dict[str, Any]] = None,
        client_capabilities: Optional[dict[str, Any]] = None,
        request_handlers: Optional[dict[str, ClientRequestHandler]] = None,
        cancel_meta: Optional[dict[str, Any]] = None,
    ) -> None:
        self._client_info = {"name": client_name, "version": client_version}
        #: Engine dialect: ``_meta`` of ``initialize`` and the complete
        #: ``clientCapabilities`` (``None`` keeps the protocol baseline).
        self._initialize_meta = dict(initialize_meta) if initialize_meta else None
        self._client_capabilities = (
            dict(client_capabilities) if client_capabilities is not None else None)
        self._request_handlers = dict(request_handlers or {})
        self._cancel_meta = dict(cancel_meta) if cancel_meta else None
        self._request_tasks: set[asyncio.Task] = set()
        #: (sessionId, promptId) -> future settled by an engine completion signal.
        self._prompt_waiters: dict[tuple[str, str], asyncio.Future] = {}
        self._on_update = on_update
        self._permission_handler = permission_handler
        self._elicitation_handler = elicitation_handler
        # provider 扩展方法（cursor/task、_x.ai/session_notification 等）：
        # 返回 dict 作为 result 应答；返回 None 表示不认识该方法。
        self._extension_handler = extension_handler
        self._control_delivery_handler = control_delivery_handler
        #: 有进行中 session/prompt 的 sessionId 集合——replay 判定的唯一依据。
        self._active_prompts: set[str] = set()
        self._replaying_sessions: set[str] = set()
        #: 回放事件流水（按到达顺序），供 Adapter 恢复时检阅而不投影。
        self.replay_log: list[dict[str, Any]] = []
        self.hello: Optional[AcpHello] = None
        self._session_setup: dict[str, dict[str, Any]] = {}
        self._config_changed: dict[str, asyncio.Event] = {}
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
        for task in list(self._request_tasks):
            task.cancel()
        if self._request_tasks:
            await asyncio.gather(*self._request_tasks, return_exceptions=True)
        for waiter in self._prompt_waiters.values():
            if not waiter.done():
                waiter.set_exception(PeerClosedError("acp closed"))
        return await self._peer.close()

    # -- 协商与认证 -----------------------------------------------------------

    async def initialize(self, *, timeout: float = 30.0) -> AcpHello:
        params: dict[str, Any] = {
            "protocolVersion": ACP_PROTOCOL_VERSION,
            "clientCapabilities": (
                dict(self._client_capabilities)
                if self._client_capabilities is not None else {
                    "fs": {"readTextFile": False, "writeTextFile": False},
                    "terminal": False,
                    "elicitation": {"form": {}},
                }),
            "clientInfo": dict(self._client_info),
        }
        if self._initialize_meta:
            params["_meta"] = dict(self._initialize_meta)
        resp = await self._peer.request("initialize", params, timeout=timeout)
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
        self._session_setup[session_id] = {**result, **self._session_setup.get(session_id, {})}
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
        self._session_setup.pop(session_id, None)
        try:
            result = check_response("session/load", await self._peer.request(
                "session/load", {"sessionId": session_id, "cwd": cwd,
                                 "mcpServers": list(mcp_servers)}, timeout=timeout))
        finally:
            self._replaying_sessions.discard(session_id)
        self._session_setup[session_id] = {**result, **self._session_setup.get(session_id, {})}

    async def resume_session(
        self, session_id: str, cwd: str, mcp_servers: list[dict[str, Any]], *,
        timeout: float = 60.0,
    ) -> None:
        """``session/resume``：不重放历史（仅在 resume 能力存在时调用）。"""
        if self.hello is not None and not self.hello.resume:
            raise AcpError("agent does not support session/resume "
                           "(sessionCapabilities.resume absent)")
        self._session_setup.pop(session_id, None)
        result = check_response("session/resume", await self._peer.request(
            "session/resume",
            {"sessionId": session_id, "cwd": cwd,
             "mcpServers": list(mcp_servers)},
            timeout=timeout))
        self._session_setup[session_id] = {**result, **self._session_setup.get(session_id, {})}

    def session_setup(self, session_id: str) -> dict[str, Any]:
        """Return the Agent-reported setup state for one ACP session."""
        return dict(self._session_setup.get(session_id) or {})

    async def wait_config_option_values(
        self, session_id: str, config_id: str, values: set[str], *, timeout: float,
    ) -> dict[str, Any]:
        """Wait for the Agent's live selector to advertise a requested value."""
        changed = self._config_changed.setdefault(session_id, asyncio.Event())
        closed = asyncio.create_task(self._peer.wait_closed())
        closed.add_done_callback(lambda _task: changed.set())
        try:
            async with asyncio.timeout(timeout):
                while True:
                    changed.clear()
                    if closed.done() or not self.running:
                        raise AcpError("ACP connection closed while waiting for session configuration")
                    option = next((item for item in self.session_setup(session_id).get("configOptions", [])
                                   if item.get("id") == config_id), {})
                    choices = [choice for group in option.get("options", [])
                               for choice in (group.get("options", []) if "group" in group else [group])]
                    if option.get("type") == "select" and values.intersection(item.get("value") for item in choices):
                        return option
                    await changed.wait()
        finally:
            closed.cancel()
            await asyncio.gather(closed, return_exceptions=True)

    async def set_config_option(
        self, session_id: str, config_id: str, value: str, *,
        timeout: float = 30.0,
    ) -> None:
        result = check_response("session/set_config_option", await self._peer.request(
            "session/set_config_option",
            {"sessionId": session_id, "configId": config_id, "value": value},
            timeout=timeout,
        ))
        options = result.get("configOptions")
        if not isinstance(options, list):
            raise AcpError("session/set_config_option returned no configOptions")
        setup = self._session_setup.setdefault(session_id, {})
        setup["configOptions"] = options
        self._config_changed.setdefault(session_id, asyncio.Event()).set()
        option = next((item for item in options if item.get("id") == config_id), None)
        if option is None or option.get("currentValue") != value:
            raise AcpError(f"session/set_config_option did not apply {config_id!r}={value!r}")
        if option.get("category") == "mode" or option.get("id") == "mode":
            setup.setdefault("modes", {})["currentModeId"] = value

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
        meta: Optional[dict[str, Any]] = None,
        prompt_id: Optional[str] = None,
    ) -> dict[str, Any]:
        """一个 prompt turn；返回 ``{stopReason, ...}``。

        ``timeout`` 为 ``None`` 或 ``<= 0`` 时不设上限。对话模式会显式
        传入 ``None``；做题 Worker 仍走默认 600 秒。

        ``prompt_id`` 给出时，prompt 响应与引擎的完成信号竞速
        （:meth:`settle_prompt`）：先到者决定本轮结果。
        """
        self._active_prompts.add(session_id)
        params: dict[str, Any] = {
            "sessionId": session_id,
            "prompt": [{"type": "text", "text": text}],
        }
        request_meta = dict(meta or {})
        if prompt_id:
            request_meta.update({"promptId": prompt_id, "requestId": prompt_id})
        if request_meta:
            params["_meta"] = request_meta
        try:
            if not prompt_id:
                resp = await self._peer.request(
                    "session/prompt", params, timeout=timeout)
                return check_response("session/prompt", resp)
            key = (session_id, prompt_id)
            waiter: asyncio.Future = asyncio.get_running_loop().create_future()
            self._prompt_waiters[key] = waiter
            request = asyncio.ensure_future(self._peer.request(
                "session/prompt", params, timeout=timeout))
            try:
                await asyncio.wait({request, waiter}, return_when=asyncio.FIRST_COMPLETED)
                if request.done():
                    return check_response("session/prompt", request.result())
                return waiter.result()
            finally:
                self._prompt_waiters.pop(key, None)
                if not request.done():
                    request.cancel()
                await asyncio.gather(request, return_exceptions=True)
        finally:
            self._active_prompts.discard(session_id)

    def settle_prompt(
        self, session_id: str, prompt_id: str, *,
        result: Optional[dict[str, Any]] = None,
        error: Optional[AcpError] = None,
    ) -> bool:
        """Settle a pending prompt from an engine completion notification.

        Returns False when no prompt with that id is waiting (a duplicate or
        foreign signal), so the notification never ends an unrelated turn.
        """
        waiter = self._prompt_waiters.get((session_id, prompt_id))
        if waiter is None or waiter.done():
            return False
        if error is not None:
            waiter.set_exception(error)
        else:
            waiter.set_result(dict(result or {}))
        return True

    def pending_prompt_ids(self, session_id: str) -> list[str]:
        return [pid for sid, pid in self._prompt_waiters if sid == session_id]

    async def cancel(self, session_id: str) -> None:
        """``session/cancel`` notification；Agent 须以 cancelled 结束 prompt。"""
        params: dict[str, Any] = {"sessionId": session_id}
        if self._cancel_meta:
            params["_meta"] = dict(self._cancel_meta)
        await self._peer.notify("session/cancel", params)
        # Prompts raced against an engine completion signal must not outlive
        # a cancel when the engine never answers the request.
        for waiter_session, prompt_id in list(self._prompt_waiters):
            if waiter_session == session_id:
                self.settle_prompt(
                    session_id, prompt_id, result={"stopReason": "cancelled"})

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
            if not replay and update.get("sessionUpdate") == "config_option_update" and isinstance(update.get("configOptions"), list):
                self._session_setup.setdefault(session_id, {})["configOptions"] = update["configOptions"]
                self._config_changed.setdefault(session_id, asyncio.Event()).set()
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
            params = {**(msg.get("params") or {}), "__muteki_rpc_request_id": str(msg["id"])}
            option_id: Optional[str] = None
            if self._permission_handler is not None:
                decision = self._permission_handler(params)
                option_id = (
                    await decision if inspect.isawaitable(decision) else decision
                )
            if option_id is None:
                await self._respond_control(msg["id"], params, {"outcome": {"outcome": "cancelled"}})
            else:
                await self._respond_control(msg["id"], params, {
                    "outcome": {"outcome": "selected", "optionId": option_id}})
            return
        if method in {"elicitation/create", "session/elicitation"} and "id" in msg:
            params = {**(msg.get("params") or {}), "__muteki_rpc_request_id": str(msg["id"])}
            result: dict[str, Any] = {"action": "cancel"}
            if self._elicitation_handler is not None:
                response = self._elicitation_handler(params)
                result = (
                    await response if inspect.isawaitable(response) else response
                )
            await self._respond_control(msg["id"], params, result)
            return
        handler = self._request_handlers.get(str(method or ""))
        if handler is not None and "id" in msg:
            task = asyncio.ensure_future(self._serve_request(msg, handler))
            self._request_tasks.add(task)
            task.add_done_callback(self._request_tasks.discard)
            return
        if self._extension_handler is not None:
            extension_result = self._extension_handler(
                str(method or ""), msg.get("params") or {}, "id" in msg)
            if extension_result is not None:
                if "id" in msg:
                    await self._peer.respond(msg["id"], result=extension_result)
                return
        if "id" in msg:
            # JSON-RPC 请求必须应答，否则对端会一直挂起等待。
            await self._peer.respond(msg["id"], error={
                "code": -32601, "message": f"unsupported method: {method}"})
        self.stats["unhandled"] += 1

    async def _serve_request(self, msg: dict[str, Any], handler: ClientRequestHandler) -> None:
        params = {**(msg.get("params") or {}), "__muteki_rpc_request_id": str(msg["id"])}
        try:
            result = await handler(params)
        except asyncio.CancelledError:
            raise
        except AcpRequestError as exc:
            await self._respond_error(msg["id"], exc.code if exc.code is not None else -32603,
                                      str(exc), exc.data)
            return
        except Exception as exc:  # noqa: BLE001 - reported to the peer, not swallowed
            await self._respond_error(msg["id"], -32603, f"{type(exc).__name__}: {exc}", None)
            return
        try:
            await self._respond_control(msg["id"], params, result)
        except PeerClosedError:
            return

    async def _respond_error(self, request_id: Any, code: int, message: str, data: Any) -> None:
        error: dict[str, Any] = {"code": code, "message": message}
        if data is not None:
            error["data"] = data
        try:
            await self._peer.respond(request_id, error=error)
        except PeerClosedError:
            return

    async def _respond_control(self, request_id: Any, params: dict[str, Any], result: dict[str, Any]) -> None:
        try:
            await self._peer.respond(request_id, result=result)
        except BaseException as exc:
            if self._control_delivery_handler is not None:
                self._control_delivery_handler(params, {"ok": False, "detail": f"{type(exc).__name__}: {exc}"})
            raise
        else:
            if self._control_delivery_handler is not None:
                self._control_delivery_handler(params, {"ok": True})


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
        # Raw thinking channel: carried as REASONING_SUMMARY(channel=thinking)
        # deltas, never mixed into the assistant message stream.
        text = _content_text(update.get("content"))
        if not text:
            return []
        return [(AgentEventType.REASONING_SUMMARY, f"acp.{kind}", {
            "text": text,
            "message_id": update.get("messageId"),
        })]
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
        output = _content_text(update.get("content")) or None
        if output is None and update.get("rawOutput") is not None:
            # Some ACP runtimes (including Grok) publish only rawOutput.
            # Preserve its native structure instead of dropping the evidence.
            output = json.dumps(update["rawOutput"], ensure_ascii=False)
        payload = {
            "call_id": str(update.get("toolCallId") or ""),
            "status": status or None,
            "output": output,
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


_PLAN_TASK_STATUS = {
    "pending": "pending", "todo": "pending", "open": "pending",
    "in_progress": "in_progress", "running": "in_progress",
    "active": "in_progress",
    "completed": "completed", "done": "completed", "complete": "completed",
    "blocked": "blocked",
    "cancelled": "cancelled", "canceled": "cancelled",
}

_TOOL_STATUS = {
    "pending": "pending", "in_progress": "running", "running": "running",
    "completed": "completed", "failed": "failed",
    "cancelled": "cancelled", "canceled": "cancelled",
}


def _legacy_to_contract(
    etype: AgentEventType, payload: dict[str, Any]
) -> Optional[dict[str, Any]]:
    """``normalize_session_update`` 的 legacy dict → 契约 payload dump。

    ``normalize_session_update`` 的返回形态被 ``solver.devin_cli_bridge``
    读取，不能直接改；进入统一事件流前在这里落成契约模型。字段不可用时
    返回 ``None``（调用方计数后跳过，与 unmapped update 同策略）。
    """
    if etype is AgentEventType.MESSAGE_DELTA:
        text = str(payload.get("text") or "")
        if not text:
            return None
        return dump_payload(MessageDeltaPayload(
            text=text,
            role="user" if str(payload.get("role") or "") == "user" else "assistant",
            phase=str(payload.get("phase") or "") or None,
            message_id=(
                str(payload["message_id"])
                if payload.get("message_id") is not None else None),
        ))
    if etype is AgentEventType.REASONING_SUMMARY:
        text = str(payload.get("text") or "")
        if not text:
            return None
        return dump_payload(ReasoningPayload(
            text=text, channel="thinking", partial=True,
            item_id=(
                str(payload["message_id"])
                if payload.get("message_id") is not None else None),
        ))
    if etype in (AgentEventType.TOOL_STARTED, AgentEventType.TOOL_PROGRESS,
                 AgentEventType.TOOL_COMPLETED):
        call_id = str(payload.get("call_id") or "")
        if not call_id:
            return None
        return dump_payload(ToolPayload(
            tool_call_id=call_id,
            name=str(payload.get("tool") or "") or None,
            input=payload.get("input"),
            output=payload.get("output"),
            status=_TOOL_STATUS.get(str(payload.get("status") or "").lower()),
            kind="agent" if payload.get("is_agent") else None,
            agent_id=str(payload.get("agent_id") or "") or None,
        ))
    if etype is AgentEventType.PLAN_UPDATED:
        tasks: list[PlanTaskPayload] = []
        for raw in payload.get("tasks") or []:
            if not isinstance(raw, dict):
                continue
            status = _PLAN_TASK_STATUS.get(
                str(raw.get("status") or "pending").strip().lower(), "pending")
            tasks.append(PlanTaskPayload(
                task_id=str(raw.get("task_id") or ""),
                title=str(raw.get("title") or raw.get("task_id") or ""),
                status=status,
            ))
        if not tasks:
            return None
        return dump_payload(PlanPayload(
            tasks=tasks, patch=False,
            native={"phase": payload.get("phase")},
        ))
    if etype is AgentEventType.USAGE_UPDATED:
        # ACP capacity/occupancy is context-only, not token consumption.
        usage = dict(payload.get("usage") or {})
        def _num(value: Any) -> Optional[int]:
            if isinstance(value, bool) or not isinstance(value, (int, float)):
                return None
            return int(value) if value >= 0 else None
        cost = usage.get("cost")
        return dump_payload(UsagePayload(
            scope="context_only",
            context_used_tokens=_num(usage.get("used")),
            context_window=_num(usage.get("size")),
            cost_usd=(
                value if isinstance(cost, dict)
                and isinstance((value := cost.get("total")), (int, float))
                and not isinstance(value, bool) else None
            ),
            native=usage,
        ))
    if etype is AgentEventType.RUNTIME_CAPABILITIES_UPDATED:
        revision = payload.get("revision")
        return dump_payload(RuntimeCapabilitiesPayload(
            revision=int(revision) if isinstance(revision, int) else None,
            reason="commands_changed",
            native={
                "adapter_id": payload.get("adapter_id"),
                "commands": list(payload.get("commands") or []),
            },
        ))
    return None


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


#: Memory guard for one client terminal.  Reaching it terminates the process
#: group and every later ``terminal/output`` answers ``terminal_output_limit``.
TERMINAL_HARD_OUTPUT_LIMIT = 64 * 1024 * 1024
TERMINAL_OUTPUT_LIMIT_CODE = -32010


@dataclass
class _ClientTerminal:
    terminal_id: str
    session_id: str
    process: Any
    byte_limit: Optional[int]
    buffer: bytearray = field(default_factory=bytearray)
    truncated: bool = False
    limit_exceeded: bool = False
    reader: Optional[asyncio.Task] = None
    released: bool = False


class AcpClientTerminals:
    """``terminal/*`` client methods of ACP, bound to one adapter session.

    Commands run in the session workspace (a requested ``cwd`` must resolve
    inside it), under the process supervisor so the whole group is owned and
    reaped.  The engine's own permission request precedes execution; this
    class never grants more than the request names.
    """

    def __init__(
        self, *, adapter_id: str, agent_session_id: str, cwd: str,
        env: dict[str, str], shell_commands: bool = False,
    ) -> None:
        self._adapter_id = adapter_id
        self._agent_session_id = agent_session_id
        self._cwd = os.path.realpath(cwd)
        self._env = dict(env)
        self._shell_commands = shell_commands
        self._terminals: dict[str, _ClientTerminal] = {}

    def request_handlers(self) -> dict[str, ClientRequestHandler]:
        return {
            "terminal/create": self.create,
            "terminal/output": self.output,
            "terminal/wait_for_exit": self.wait_for_exit,
            "terminal/kill": self.kill,
            "terminal/release": self.release,
        }

    def _resolve_cwd(self, requested: Any) -> str:
        if requested in (None, ""):
            return self._cwd
        path = os.path.realpath(
            requested if os.path.isabs(str(requested))
            else os.path.join(self._cwd, str(requested)))
        if path != self._cwd and not path.startswith(self._cwd + os.sep):
            raise AcpRequestError(
                f"terminal cwd {requested!r} is outside the session workspace",
                code=-32602)
        return path

    def _get(self, params: dict[str, Any]) -> _ClientTerminal:
        terminal = self._terminals.get(str(params.get("terminalId") or ""))
        if terminal is None or terminal.session_id != str(params.get("sessionId") or ""):
            raise AcpRequestError("unknown terminal", code=-32602)
        return terminal

    async def create(self, params: dict[str, Any]) -> dict[str, Any]:
        from .process_supervisor import spawn_supervised

        command = str(params.get("command") or "").strip()
        if not command:
            raise AcpRequestError("terminal/create requires a command", code=-32602)
        args = [str(item) for item in params.get("args") or []]
        argv = (
            ["/bin/sh", "-c", command]
            if self._shell_commands and not args else [command, *args])
        env = dict(os.environ)
        env.update(self._env)
        for row in params.get("env") or []:
            if isinstance(row, dict) and row.get("name"):
                env[str(row["name"])] = str(row.get("value") or "")
        limit = params.get("outputByteLimit")
        try:
            supervised = await spawn_supervised(
                argv, adapter_id=self._adapter_id,
                session_id=self._agent_session_id, label="acp-terminal",
                cwd=self._resolve_cwd(params.get("cwd")), env=env,
                stdin=asyncio.subprocess.DEVNULL,
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.STDOUT)
        except OSError as exc:
            raise AcpRequestError(f"cannot start terminal command: {exc}", code=-32000) from exc
        terminal = _ClientTerminal(
            terminal_id=new_id("term"), session_id=str(params.get("sessionId") or ""),
            process=supervised,
            byte_limit=int(limit) if isinstance(limit, int) and limit > 0 else None)
        terminal.reader = asyncio.ensure_future(self._pump(terminal))
        self._terminals[terminal.terminal_id] = terminal
        return {"terminalId": terminal.terminal_id}

    async def _pump(self, terminal: _ClientTerminal) -> None:
        stream = terminal.process.process.stdout
        total = 0
        while True:
            chunk = await stream.read(65536)
            if not chunk:
                return
            total += len(chunk)
            terminal.buffer.extend(chunk)
            if terminal.byte_limit is not None and len(terminal.buffer) > terminal.byte_limit:
                del terminal.buffer[:len(terminal.buffer) - terminal.byte_limit]
                terminal.truncated = True
            if total > TERMINAL_HARD_OUTPUT_LIMIT:
                terminal.limit_exceeded = True
                await terminal.process.terminate()
                return

    @staticmethod
    def _exit_status(terminal: _ClientTerminal) -> Optional[dict[str, Any]]:
        code = terminal.process.process.returncode
        if code is None:
            return None
        if code < 0:
            return {"exitCode": None, "signal": signal_name(-code)}
        return {"exitCode": code, "signal": None}

    async def output(self, params: dict[str, Any]) -> dict[str, Any]:
        terminal = self._get(params)
        if terminal.limit_exceeded:
            raise AcpRequestError(
                "terminal output exceeded the client limit; process group terminated",
                code=TERMINAL_OUTPUT_LIMIT_CODE,
                data={"reason": "terminal_output_limit"})
        result: dict[str, Any] = {
            "output": bytes(terminal.buffer).decode("utf-8", errors="replace"),
            "truncated": terminal.truncated,
        }
        status = self._exit_status(terminal)
        if status is not None:
            result["exitStatus"] = status
        return result

    async def wait_for_exit(self, params: dict[str, Any]) -> dict[str, Any]:
        terminal = self._get(params)
        await terminal.process.process.wait()
        if terminal.reader is not None:
            await asyncio.gather(terminal.reader, return_exceptions=True)
        return self._exit_status(terminal) or {}

    async def kill(self, params: dict[str, Any]) -> dict[str, Any]:
        terminal = self._get(params)
        if terminal.process.process.returncode is None:
            await terminal.process.terminate()
        return {}

    async def release(self, params: dict[str, Any]) -> dict[str, Any]:
        terminal = self._get(params)
        await self._dispose(terminal)
        self._terminals.pop(terminal.terminal_id, None)
        return {}

    async def _dispose(self, terminal: _ClientTerminal) -> None:
        terminal.released = True
        if terminal.process.process.returncode is None:
            await terminal.process.terminate()
        if terminal.reader is not None:
            terminal.reader.cancel()
            await asyncio.gather(terminal.reader, return_exceptions=True)

    async def close(self) -> None:
        for terminal in list(self._terminals.values()):
            await self._dispose(terminal)
        self._terminals.clear()


def signal_name(number: int) -> str:
    import signal as _signal

    try:
        return _signal.Signals(number).name
    except ValueError:
        return f"SIG{number}"


def _flag_true(node: Any, key: str) -> bool:
    return isinstance(node, dict) and node.get(key) is True


def _update_is_background(update: dict[str, Any]) -> bool:
    """True when the update flags a background task.

    Recognizes ``rawOutput.isBackground`` and the same boolean under the
    usual equivalent locations.  Other payloads are not background work.
    """
    if update.get("isBackground") is True or update.get("is_background") is True:
        return True
    raw = update.get("rawOutput")
    if _flag_true(raw, "isBackground") or _flag_true(raw, "is_background"):
        return True
    if isinstance(raw, dict):
        value = raw.get("value")
        if _flag_true(value, "isBackground") or _flag_true(value, "is_background"):
            return True
    meta = update.get("_meta")
    if _flag_true(meta, "isBackground") or _flag_true(meta, "is_background"):
        return True
    if isinstance(meta, dict):
        started = meta.get("cognition.ai/subagent_started")
        if _flag_true(started, "isBackground") or _flag_true(started, "is_background"):
            return True
    return False


def _option_requested_mode(option: dict[str, Any]) -> Optional[str]:
    """Structured access mode on a permission option, if it names one.

    Only ``accessMode`` / ``access_mode`` values that are already an
    ``AccessMode`` count.  Option labels are not interpreted.
    """
    sources: list[dict[str, Any]] = [option]
    meta = option.get("_meta")
    if isinstance(meta, dict):
        sources.append(meta)
    for source in sources:
        for key in ("accessMode", "access_mode"):
            raw = source.get(key)
            if not isinstance(raw, str) or not raw:
                continue
            try:
                return AccessMode(raw).value
            except ValueError:
                continue
    return None


class BaseAcpAdapter(
    BaseExternalAgentAdapter, RuntimeOperationAdapter, BackgroundUpdateAdapter
):
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

    #: Conversation access modes this provider enforces, either through a
    #: native control selected at launch or through the shared permission
    #: callback below.  ``auto`` has no client-side meaning in ACP, so a
    #: provider lists it only when it selects a native auto policy.
    supported_access_modes: tuple[str, ...] = (
        AccessMode.SUPERVISED.value,
        AccessMode.AUTO_ACCEPT_EDITS.value,
        AccessMode.FULL_ACCESS.value,
    )
    #: Why a listed-out mode is unavailable, included in the typed error.
    unsupported_access_mode_reasons: dict[str, str] = {
        AccessMode.AUTO.value: (
            "ACP has no client-side auto policy and this provider exposes no "
            "native auto mode"
        ),
    }
    #: JSON-RPC error codes the engine uses for a usage/rate limit.  A prompt
    #: failing with one of them is a typed ``usage_limit`` turn failure and the
    #: still-running process is not reported as exited.
    rate_limit_error_codes: tuple[int, ...] = ()
    #: ``session/set_mode`` id of the engine's read-only planning mode.
    plan_mode_id = "plan"

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

    def _validate_access_mode(self, request: SessionStart, cwd: str) -> None:
        """Reject an explicitly requested mode this provider cannot honor.

        Providers extend this when honoring a mode depends on the session
        (for example on the working directory).
        """
        del cwd
        require_access_mode(
            self.id, request.access_mode, self.supported_access_modes,
            reasons=self.unsupported_access_mode_reasons,
        )

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

    # -- engine dialect hooks (defaults keep the protocol baseline) ----------

    def _initialize_options(self) -> dict[str, Any]:
        """``AcpTransport`` kwargs of the ``initialize`` dialect.

        ``initialize_meta`` is the request's ``_meta``; ``client_capabilities``
        replaces the whole ``clientCapabilities`` object.
        """
        return {}

    def _session_transport_options(
        self, request: SessionStart, handle: dict[str, Any]
    ) -> dict[str, Any]:
        """Per-session ``AcpTransport`` kwargs (``request_handlers``, ``cancel_meta``)."""
        return {}

    def _normalize_update(self, update: dict[str, Any]) -> dict[str, Any]:
        """Rewrite one engine ``session/update`` into the standard shape."""
        return update

    def _normalize_tool_call(self, update: dict[str, Any]) -> dict[str, Any]:
        """Rewrite one ``tool_call`` / ``tool_call_update``.

        Default returns ``update`` unchanged, so the tool name stays
        ``title`` or ``kind``.
        """
        return update

    def _background_tasks_hold_turn(self) -> bool:
        """Whether a background task keeps the turn open.

        Default True: the turn still ends only when ``session/prompt``
        returns, or when an engine completion signal wins the prompt
        race.  ``rawOutput.isBackground`` does not shorten that wait.
        Override to False to finish the turn while that work continues.
        """
        return True

    def _background_task_key(self, update: dict[str, Any]) -> Optional[str]:
        """Id of a background task in ``update``, or None.

        Default recognizes ``rawOutput.isBackground`` and the equivalent
        booleans checked by ``_update_is_background``.  An id does not end
        the turn while ``_background_tasks_hold_turn`` stays True.
        """
        if str(update.get("sessionUpdate") or "") not in (
            "tool_call", "tool_call_update",
        ):
            return None
        if not _update_is_background(update):
            return None
        call_id = str(update.get("toolCallId") or "").strip()
        return call_id or None

    def _note_turn_tools(self, handle: dict[str, Any], update: dict[str, Any]) -> None:
        if self._background_tasks_hold_turn():
            return
        kind = str(update.get("sessionUpdate") or "")
        if kind not in ("tool_call", "tool_call_update"):
            return
        call_id = str(update.get("toolCallId") or "").strip()
        if not call_id:
            return
        background: set[str] = handle.setdefault("_background_tasks", set())
        open_tools: dict[str, str] = handle.setdefault("_open_tools", {})
        if self._background_task_key(update):
            background.add(call_id)
            open_tools.pop(call_id, None)
            return
        status = str(update.get("status") or "")
        if status in ("completed", "failed", "cancelled", "canceled"):
            open_tools.pop(call_id, None)
        elif call_id not in background:
            open_tools[call_id] = status or "running"

    def _release_background_turn(self, handle: dict[str, Any]) -> bool:
        """True when the turn may finish without waiting for background tasks."""
        if self._background_tasks_hold_turn():
            return False
        if not handle.get("_background_tasks"):
            return False
        return not handle.get("_open_tools")

    def _prompt_id(self, handle: dict[str, Any]) -> Optional[str]:
        """Id to race ``session/prompt`` against an engine completion signal."""
        return None

    def _prompt_meta(self, handle: dict[str, Any]) -> Optional[dict[str, Any]]:
        """``_meta`` of ``session/prompt``."""
        return None

    def _native_session_scope_exact(
        self, handle: dict[str, Any], pending: dict[str, Any]
    ) -> bool:
        """True when the engine's own ``allow_always`` equals the shown target.

        Engines with project-wide or tool-wide grants keep the default False,
        so a session-scoped approval is answered one-shot and remembered by
        Muteki for exactly the same kind and target.
        """
        return False

    def _mcp_http_degradation(self, hello: AcpHello) -> Optional[str]:
        if hello.mcp_http:
            return None
        return (
            "mcpCapabilities.http=false：无法注入 HTTP 形态 mcpServers，"
            "能力注入将降级（由 select_injection_kind 选择后续档位）"
        )

    def _new_transport(self, argv: list[str], **kwargs: Any) -> AcpTransport:
        """构造 ACP 传输。子类可换自己的传输，不在这里按引擎名分支。"""
        return AcpTransport(argv, **kwargs)

    def _materialize_session_mcp(
        self,
        request: SessionStart,
        plan: Optional[CapabilityInjectionPlan],
        bearer_token: Optional[str],
    ) -> list[dict[str, Any]]:
        """session/new、load、resume 共用的 mcpServers。"""
        del request
        return materialize_mcp_servers(plan, bearer_token)

    async def _after_session_open(
        self, transport: "AcpTransport", session_id: str, request: SessionStart
    ) -> None:
        """子类钩子：session/new|load|resume 成功后的 Runtime 特有动作。"""

    # -- 子智能体钩子（依据真实协议字段，不靠工具名猜测） ----------------------

    def _delegation_info(
        self, update: dict[str, Any]
    ) -> Optional[dict[str, Any]]:
        """父会话 ``tool_call`` 是子智能体委派时返回描述。

        字段：``title``/``request``/``role``/``model``；``agent_id`` 存在时
        立即以该 id 建 node（如 Cursor 用 toolCallId）；缺省时只记录委派
        描述并给 TOOL_STARTED 打 ``is_agent``，node 等子会话/子 agent id
        公布后再建（Grok/Devin）。
        """
        return None

    def _delegation_result(
        self, handle: dict[str, Any], update: dict[str, Any],
        desc: dict[str, Any],
    ) -> Optional[dict[str, Any]]:
        """委派 ``tool_call_update`` 完成时返回子 agent 关联与统计。

        返回 ``{"agent_id": ..., ...patch}`` 时把 node 关联到真实子 agent
        id 并合并 patch（Grok 的 ``rawOutput.subagent_id``）。
        """
        return None

    def _tool_owner(self, update: dict[str, Any]) -> Optional[str]:
        """父会话工具事件实际属于某个子智能体时返回其 agent_id（Devin
        ``_meta.subagent_context.parentAgentId``）。"""
        return None

    def _tool_meta_nodes(
        self, handle: dict[str, Any], update: dict[str, Any]
    ) -> "list[dict[str, Any]]":
        """从 tool_call/_update 的 ``_meta`` 提取子智能体生命周期（Devin
        ``subagent_started``/``subagent_completed``）。返回 node patch 列表。"""
        return []

    def _map_agent_extension(
        self, handle: dict[str, Any], method: str,
        params: dict[str, Any], is_request: bool,
    ) -> "tuple[Optional[dict[str, Any]], list[tuple[Any, str, dict[str, Any]]]]":
        """provider 扩展 RPC：返回 (result, events)。

        result 非 None 表示已处理（请求以该 result 应答）；Grok 消费
        ``_x.ai/session_notification``，Cursor 消费 ``cursor/task``。
        """
        return None, []

    # -- probe（真实 initialize 协商） -----------------------------------------

    def _turn_result_usage(self, result: dict[str, Any]) -> Optional[UsagePayload]:
        """Provider-specific per-turn usage returned by session/prompt."""
        return None

    async def probe(self, request: ProbeRequest) -> AgentCapabilities:
        argv = self._with_launch_args(self._agent_argv())
        field_sources: dict[str, str] = {}
        degradations: list[str] = []
        caps = conservative_capabilities(
            transport_kind="acp", capability_source=SOURCE_STATIC)
        binary = argv[0] if argv else ""
        hello: Optional[AcpHello] = None
        detail = ""
        try:
            transport = self._new_transport(
                argv, client_name=self._client_name,
                **self._initialize_options())
            await transport.start()
            try:
                hello = await transport.initialize(
                    timeout=self._startup_timeout)
            finally:
                await transport.close()
        except (OSError, AcpError, asyncio.TimeoutError, PeerClosedError) as exc:
            detail = f"ACP initialize 失败：{exc}"
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
            hello.agent_info.get("version") or "")
        if not caps.runtime_version and binary:
            # 部分 ACP Agent 的 initialize 响应省略版本号。协议协商已经
            # 成功时，再用真实 ``--version`` 输出补齐展示与健康判定。
            version = await probe_version(self.probe_version_argv())
            caps.runtime_version = version.version if version.ok else ""
            if not version.ok:
                degradations.append(version.describe())
        caps.streaming = True
        caps.tool_events = True
        caps.approval = True
        caps.access_modes = list(self.supported_access_modes)
        caps.interrupt = True
        caps.usage_events = True
        # ACP sessionUpdate.kind == "plan" is first-class in the wire protocol.
        caps.plan = True
        caps.resume = hello.resume or hello.load_session
        caps.resume_continues_turn = False
        caps.session_persistence = caps.resume
        # ACP 注入走 mcpServers（HTTP 形态需 mcpCapabilities.http 门控）：
        # 不声明通用 mcp 位，避免 select_injection_kind 选错计划形态。
        caps.acp_mcp_config = hello.mcp_http
        self._probe_extra_caps(caps, hello)
        for field_name in BOOL_CAPABILITY_FIELDS:
            field_sources[field_name] = SOURCE_PROBE
        http_note = self._mcp_http_degradation(hello)
        if http_note:
            degradations.append(http_note)
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
        cwd = str(request.options.cwd or self._default_cwd
                  or os.getcwd())
        mcp_servers = self._materialize_session_mcp(
            request, plan, bearer_token)
        access_mode = str(
            request.access_mode or AccessMode.SUPERVISED.value
        ).strip()
        if access_mode not in ACCESS_MODE_VALUES:
            raise ValueError(
                f"{self.adapter_id} unsupported access mode: {access_mode}")
        self._validate_access_mode(request, cwd)
        env = dict(self._env_extra)
        env.update(request.options.env)
        env = self._prepare_session_environment(request, env, cwd)

        handle: dict[str, Any] = {
            "cwd": cwd,
            "env": env,
            "options": request.options,
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
            "control_delivery_enabled": True,
            "control_deliveries": {},
            "available_commands": [],
            "capability_revision": 0,
            # 子智能体：agent_id -> node；子 ACP session id -> agent_id；
            # 父会话委派 toolCallId -> 委派描述；agent_id -> 子消息缓冲。
            "agent_nodes": {},
            "subagent_sessions": {},
            "delegation_calls": {},
            "child_messages": {},
            # 子智能体的 toolCallId -> agent_id：子会话审批请求的 sessionId
            # 实测仍是父会话（Grok），只能靠 toolCallId 归属。
            "subagent_tool_calls": {},
            # toolCallId -> ACP ToolKind announced by ``tool_call``. The
            # permission request's ``toolCall`` is a partial update that some
            # engines send without ``kind`` (Kimi 2.1.1).
            "tool_kinds": {},
            "tool_raw_inputs": {},
            "approval_grants": SessionApprovalGrants(access_mode),
            "default_mode_id": None,
            "interaction_mode": "default",
            "mcp_servers": mcp_servers,
        }
        transport = self._new_transport(
            self._with_launch_args(self._agent_argv_for_request(request)), cwd=cwd, env=env,
            client_name=self._client_name,
            on_update=lambda sid, upd, replay: self._dispatch_update(
                request.agent_session_id, sid, upd, replay),
            permission_handler=lambda params: self._request_permission(
                request.agent_session_id, params),
            elicitation_handler=lambda params: self._request_elicitation(
                request.agent_session_id, params),
            control_delivery_handler=lambda params, outcome: self._control_delivery_result(
                request.agent_session_id, params, outcome),
            extension_handler=lambda method, params, is_request: self._dispatch_extension(
                request.agent_session_id, method, params, is_request),
            **{**self._initialize_options(),
               **self._session_transport_options(request, handle)},
        )
        # 先注册句柄再启动：session/load 的历史回放在 load 响应之前到达，
        # 必须能被 _dispatch_update 找到句柄记入 replay_events。
        self._acp[request.agent_session_id] = handle
        try:
            await transport.start()
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
            handle["default_mode_id"] = (
                (transport.session_setup(session_id).get("modes") or {})
                .get("currentModeId"))
        except BaseException:
            try:
                await transport.close()
            finally:
                self._acp.pop(request.agent_session_id, None)
            raise

        handle["transport"] = transport
        handle["external_session_id"] = session_id
        self._resume_ctx[request.agent_session_id] = {
            "plan": plan, "token": bearer_token, "cwd": cwd,
            "options": request.options,
            "access_mode": access_mode,
            "model": request.model,
            "effort": request.effort,
            "thread_id": request.thread_id,
        }
        return {"external_session_id": session_id, "resume_handle": session_id}

    # -- update / 审批分发 ------------------------------------------------------

    def _handle_for(self, agent_session_id: str) -> Optional[dict[str, Any]]:
        return self._acp.get(agent_session_id)

    # -- 子智能体 node 维护 ---------------------------------------------------

    def _patch_agent_node(
        self, handle: dict[str, Any], agent_id: str, update: dict[str, Any]
    ) -> "Optional[tuple[Any, str, dict[str, Any]]]":
        """按 key 合并 node patch；有变化时返回 AGENT_UPDATED 事件元组。"""
        nodes = handle["agent_nodes"]
        previous = nodes.get(agent_id)
        if previous is None:
            # 子会话中途恢复时先见到进展后见到公布，也要有 node。
            previous = {"agent_id": agent_id, "title": agent_id,
                        "status": "running"}
        node = {**previous, **{k: v for k, v in update.items() if v is not None}}
        for key, value in update.items():
            if value is None and key in ("result", "error"):
                node[key] = None
        if node == previous and agent_id in nodes:
            return None
        nodes[agent_id] = node
        return (AgentEventType.AGENT_UPDATED, "acp.subagent",
                dump_payload(AgentUpdatedPayload(
                    agents=[AgentNodePayload(**node)], patch=True)))

    def _emit_handle_events(
        self, handle: dict[str, Any], events: "list[tuple[Any, str, dict[str, Any]]]"
    ) -> None:
        if not events:
            return
        sink = handle.get("event_sink")
        if sink is not None:
            for etype, native_kind, payload in events:
                sink.put_nowait(("update", etype, native_kind, payload))
        elif callable(handle.get("background_handler")):
            handle["background_handler"](events)

    def _flush_child_activity(
        self, handle: dict[str, Any], agent_id: str
    ) -> "Optional[tuple[Any, str, dict[str, Any]]]":
        """把子会话缓冲的助手文本合并成 node 的一行 activity。"""
        buf = handle["child_messages"].pop(agent_id, None)
        text = str((buf or {}).get("text") or "").strip()
        if not text:
            return None
        return self._patch_agent_node(handle, agent_id, {"activity": text})

    def _dispatch_extension(
        self, agent_session_id: str, method: str,
        params: dict[str, Any], is_request: bool,
    ) -> Optional[dict[str, Any]]:
        handle = self._handle_for(agent_session_id)
        if handle is None:
            return None
        result, events = self._map_agent_extension(
            handle, method, params, is_request)
        self._emit_handle_events(handle, events)
        return result

    def _dispatch_child_update(
        self, handle: dict[str, Any], session_id: str,
        update: dict[str, Any], replay: bool,
    ) -> None:
        """同一连接上子会话的 session/update：工具事件归属子智能体，
        文本只进 node 的 activity，plan/命令目录绝不覆盖父会话。"""
        if replay:
            handle["replay_events"].append({
                "session_id": session_id,
                "updates": [str(update.get("sessionUpdate") or "")],
                "replay": True,
            })
            return
        kind = str(update.get("sessionUpdate") or "")
        agent_id = handle["subagent_sessions"].get(session_id)
        events: list[tuple[Any, str, dict[str, Any]]] = []
        content_bearing = kind in ("tool_call", "tool_call_update", "agent_message_chunk")
        if agent_id is None and not content_bearing:
            # 元数据类更新（命令目录、模式等）不足以证明存在子智能体。
            return
        if agent_id is None:
            agent_id = session_id
            handle["subagent_sessions"][session_id] = agent_id
            event = self._patch_agent_node(handle, agent_id, {
                "agent_id": agent_id,
                "session_ref": session_id,
                "status": "running",
            })
            if event is not None:
                events.append(event)
        if kind in ("tool_call", "tool_call_update"):
            call_id = str(update.get("toolCallId") or "")
            if call_id:
                handle["subagent_tool_calls"][call_id] = agent_id
            for etype, native_kind, payload in normalize_session_update(update):
                payload["agent_id"] = agent_id
                contract = _legacy_to_contract(etype, payload)
                if contract is None:
                    handle["unmapped_updates"] = handle.get("unmapped_updates", 0) + 1
                    continue
                events.append((etype, native_kind, contract))
        elif kind == "agent_message_chunk":
            text = _content_text(update.get("content"))
            if text:
                buf = handle["child_messages"].setdefault(
                    agent_id, {"message_id": None, "text": ""})
                message_id = update.get("messageId")
                boundary = (
                    message_id is not None
                    and buf["message_id"] is not None
                    and message_id != buf["message_id"]
                )
                if message_id is not None:
                    buf["message_id"] = message_id
                if boundary:
                    flushed = self._flush_child_activity(handle, agent_id)
                    if flushed is not None:
                        events.append(flushed)
                    buf = handle["child_messages"].setdefault(
                        agent_id, {"message_id": message_id, "text": ""})
                buf["text"] += text
                # 无 messageId 的实现对长文本做有界节流，避免整段攒到结束。
                if message_id is None and len(buf["text"]) >= 2000:
                    flushed = self._flush_child_activity(handle, agent_id)
                    if flushed is not None:
                        events.append(flushed)
        elif kind in ("user_message_chunk", "agent_thought_chunk", "plan",
                      "available_commands_update", "usage_update",
                      "session_info_update", "current_mode_update",
                      "config_option_update"):
            pass
        else:
            handle["unmapped_updates"] = handle.get("unmapped_updates", 0) + 1
        self._emit_handle_events(handle, events)

    def _dispatch_update(
        self, agent_session_id: str, session_id: str,
        update: dict[str, Any], replay: bool,
    ) -> None:
        """transport 回调：replay 入回放清单（不投影），live 入 turn 队列。"""
        handle = self._handle_for(agent_session_id)
        if handle is None:
            return
        update = self._normalize_update(update)
        if str(update.get("sessionUpdate") or "") in ("tool_call", "tool_call_update"):
            update = self._normalize_tool_call(update)
            call_id = str(update.get("toolCallId") or "")
            kind_hint = update.get("kind")
            if call_id and isinstance(kind_hint, str) and kind_hint and not replay:
                handle.setdefault("tool_kinds", {})[call_id] = kind_hint
            raw_hint = update.get("rawInput")
            if call_id and raw_hint is not None and not replay:
                handle.setdefault("tool_raw_inputs", {})[call_id] = raw_hint
        if handle.get("external_session_id") and session_id != handle["external_session_id"]:
            # Native workflows 会在同一连接上公布子会话（实测 Grok）；子会话
            # 事件归属子智能体 node，命令目录/计划不覆盖父会话。
            self._dispatch_child_update(handle, session_id, update, replay)
            return
        if not replay:
            self._note_turn_tools(handle, update)
        kind = str(update.get("sessionUpdate") or "")
        extra: list[tuple[Any, str, dict[str, Any]]] = []
        owner_agent: Optional[str] = None
        meta_lifecycle = False
        if not replay and kind == "tool_call":
            info = self._delegation_info(update)
            if info is not None:
                call_id = str(update.get("toolCallId") or "")
                desc = dict(info)
                desc["call_id"] = call_id
                handle["delegation_calls"][call_id] = desc
                node_id = str(info.get("agent_id") or "")
                if node_id:
                    event = self._patch_agent_node(handle, node_id, {
                        "agent_id": node_id,
                        "parent_id": info.get("parent_id"),
                        "title": info.get("title") or node_id,
                        "nickname": info.get("nickname"),
                        "role": info.get("role"),
                        "model": info.get("model"),
                        "call_id": call_id,
                        "session_ref": info.get("session_ref"),
                        "status": "running",
                        "request": info.get("request"),
                        "result": None,
                        "error": None,
                    })
                    if event is not None:
                        extra.append(event)
        if not replay and kind in ("tool_call", "tool_call_update"):
            owner_agent = self._tool_owner(update)
            if owner_agent:
                owner_call = str(update.get("toolCallId") or "")
                if owner_call:
                    handle["subagent_tool_calls"][owner_call] = owner_agent
            if owner_agent and owner_agent not in handle["agent_nodes"]:
                event = self._patch_agent_node(handle, owner_agent, {
                    "agent_id": owner_agent,
                    "session_ref": owner_agent,
                    "status": "running",
                })
                if event is not None:
                    extra.append(event)
            meta_patches = self._tool_meta_nodes(handle, update)
            for patch in meta_patches:
                agent = str(patch.get("agent_id") or "")
                if agent:
                    patch = {k: v for k, v in patch.items() if k != "agent_id"}
                    event = self._patch_agent_node(handle, agent, patch)
                    if event is not None:
                        extra.append(event)
            if meta_patches:
                # subagent_started/completed 已由 node 表达，不再是工具行。
                meta_lifecycle = True
            else:
                meta_lifecycle = False
        if not replay and kind == "tool_call_update":
            status = str(update.get("status") or "")
            call_id = str(update.get("toolCallId") or "")
            desc = handle["delegation_calls"].get(call_id)
            if desc is not None and status in ("completed", "failed"):
                patch: dict[str, Any] = {
                    "status": "completed" if status == "completed" else "failed",
                }
                output = _content_text(update.get("content"))
                if status == "failed":
                    patch["error"] = output or None
                elif output:
                    patch["result"] = output
                link = self._delegation_result(handle, update, desc) or {}
                target = str(link.pop("agent_id", "") or desc.get("agent_id") or "")
                patch.update(link)
                if target:
                    if target != desc.get("agent_id"):
                        # node 由子会话/子 agent 公布建立：回填委派关联。
                        patch.setdefault("title", desc.get("title"))
                        patch.setdefault("request", desc.get("request"))
                        patch.setdefault("call_id", call_id)
                    event = self._patch_agent_node(handle, target, patch)
                    if event is not None:
                        extra.append(event)
        mapped = [
            (etype, native_kind, contract)
            for etype, native_kind, legacy in (
                (e, n, p) for e, n, p in normalize_session_update(update)
            )
            if (contract := _legacy_to_contract(etype, legacy)) is not None
        ]
        if kind in ("tool_call", "tool_call_update") and meta_lifecycle:
            mapped = [
                entry for entry in mapped
                if entry[0] not in (AgentEventType.TOOL_STARTED,
                                    AgentEventType.TOOL_PROGRESS,
                                    AgentEventType.TOOL_COMPLETED)
            ]
        if extra:
            mapped = list(mapped) + extra
        if kind == "available_commands_update":
            handle["capability_revision"] = int(
                handle.get("capability_revision") or 0
            ) + 1
            if mapped:
                # 契约 dump 里命令清单在 native；handle 保留扁平结构供
                # runtime_capability_snapshot 使用。
                native = mapped[0][2].get("native") or {}
                handle["available_commands"] = list(native.get("commands") or [])
            mapped = [
                (etype, native_kind, {
                    **payload,
                    "revision": handle["capability_revision"],
                    "native": {**dict(payload.get("native") or {}),
                               "adapter_id": self.id},
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
        if kind == "tool_call":
            call_id = str(update.get("toolCallId") or "")
            if call_id in handle["delegation_calls"]:
                mapped = [
                    (etype, native_kind, {**payload, "kind": "agent"})
                    if etype is AgentEventType.TOOL_STARTED
                    and str(payload.get("tool_call_id") or "") == call_id
                    else (etype, native_kind, payload)
                    for etype, native_kind, payload in mapped
                ]
        if owner_agent:
            mapped = [
                (etype, native_kind, {**payload, "agent_id": owner_agent})
                if etype in (AgentEventType.TOOL_STARTED,
                             AgentEventType.TOOL_PROGRESS,
                             AgentEventType.TOOL_COMPLETED)
                else (etype, native_kind, payload)
                for etype, native_kind, payload in mapped
            ]
        self._emit_handle_events(handle, mapped)

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

    def _permission_option_for_mode(
        self, options: list[dict[str, Any]], kinds: tuple[str, ...],
        access_mode: str,
    ) -> Optional[str]:
        """First option of ``kinds`` that does not raise the access mode.

        ``allow_always`` stays a permission answer.  A structured
        ``accessMode`` broader than ``access_mode`` is refused via
        ``reject_mode_upgrade`` and that option is skipped.
        """
        current = access_mode or AccessMode.SUPERVISED.value
        for kind in kinds:
            for selected in options:
                if selected.get("kind") != kind or selected.get("optionId") is None:
                    continue
                requested = _option_requested_mode(selected)
                if requested is not None:
                    try:
                        reject_mode_upgrade(current, requested)
                    except ApprovalScopeError:
                        continue
                return str(selected["optionId"])
        return None

    def _approval_grants(self, handle: dict[str, Any]) -> SessionApprovalGrants:
        """Session ledger, created from the handle's access mode on first use."""
        grants = handle.get("approval_grants")
        if isinstance(grants, SessionApprovalGrants):
            return grants
        mode = str(handle.get("access_mode") or AccessMode.SUPERVISED.value)
        grants = SessionApprovalGrants(mode)
        handle["approval_grants"] = grants
        return grants

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
        tool_kind = str(
            tool_call.get("kind")
            or handle.get("tool_kinds", {}).get(str(tool_call.get("toolCallId") or ""))
            or "other")

        # These decisions are made by the ACP client exactly as the protocol
        # intends. Provider-native modes run first and only requests that reach
        # this callback are considered here.
        if access_mode == AccessMode.FULL_ACCESS.value:
            # One-shot first: an engine's allow_always can persist a grant
            # (project-wide or tool-wide) beyond this session.
            return self._permission_option_for_mode(
                options, ("allow_once", "allow_always"), access_mode)
        if (
            access_mode == AccessMode.AUTO_ACCEPT_EDITS.value
            and tool_kind in {"edit", "delete", "move"}
        ):
            return self._permission_option_for_mode(
                options, ("allow_once", "allow_always"), access_mode)

        sink = self._interaction_sink(handle)
        if sink is None:
            return None
        approval_id = str(
            params.get("requestId")
            or tool_call.get("toolCallId")
            or new_id("approval")
        )
        # 子会话发起的审批要带上所属子智能体，UI 才能挂到对应 node。
        # 实测 Grok 的子审批 sessionId 仍是父会话，toolCallId 才是可靠归属。
        agent_id = (
            handle["subagent_tool_calls"].get(
                str(tool_call.get("toolCallId") or ""))
            or handle["subagent_sessions"].get(str(params.get("sessionId") or ""))
        )
        loop = asyncio.get_running_loop()
        future: asyncio.Future[Optional[str]] = loop.create_future()
        approval_kind = {
            "execute": "command_execution",
            "edit": "file_change",
            "delete": "file_change",
            "move": "file_change",
        }.get(tool_kind, "tool")
        raw_input = tool_call.get("rawInput")
        if raw_input is None:
            raw_input = handle.get("tool_raw_inputs", {}).get(
                str(tool_call.get("toolCallId") or ""))
        target_fields = self._approval_target_fields(
            approval_kind, tool_call, raw_input)
        requested = dump_payload(ApprovalRequestedPayload(
            approval_id=approval_id,
            agent_id=str(agent_id) if agent_id else None,
            approval_kind=approval_kind,
            title=str(
                tool_call.get("title")
                or tool_call.get("kind")
                or "Agent operation"),
            tool_name=str(tool_call.get("title") or "") or None,
            tool_call_id=(
                str(tool_call["toolCallId"])
                if tool_call.get("toolCallId") is not None else None),
            cwd=handle.get("cwd"),
            options=[ApprovalOption(
                option_id=str(option.get("optionId") or ""),
                label=str(option.get("name") or ""),
                kind=str(option.get("kind") or "") or None,
            ) for option in options],
            **target_fields,
            native={
                "tool_kind": tool_kind,
                "access_mode": access_mode,
                "tool_call": {
                    "call_id": tool_call.get("toolCallId"),
                    "title": tool_call.get("title"),
                    "kind": tool_kind,
                },
            },
        ))
        target = ApprovalTarget.from_payload(requested, cwd=str(handle.get("cwd") or ""))
        if approval_kind == "tool" and raw_input is None:
            # Without the call's input a "tool" target is just its name, and a
            # session grant would cover every future call of that tool.
            target = None
        grants = self._approval_grants(handle)
        if target is not None and grants.covers(requested, cwd=str(handle.get("cwd") or "")):
            # A grant covers the same tool kind and normalized target only,
            # and is answered one-shot so the engine keeps no broader memory.
            once = self._permission_option_for_mode(
                options, ("allow_once",), access_mode)
            if once is not None:
                return once
        if target is not None and any(
            option.get("kind") in ("allow_once", "allow_always") for option in options
        ):
            requested["scopes"] = ["once", "session"]
        else:
            requested["scopes"] = ["once"]
        handle["pending_approvals"][approval_id] = {
            "future": future,
            "options": options,
            "params": dict(params),
            "delivery": self._register_control_delivery(handle, params),
            "request_payload": requested,
        }
        sink.put_nowait(("approval", AgentEventType.APPROVAL_REQUESTED,
                         "acp.request_permission", requested))
        try:
            return await future
        finally:
            handle["pending_approvals"].pop(approval_id, None)

    @staticmethod
    def _approval_target_fields(
        approval_kind: str, tool_call: dict[str, Any], raw_input: Any
    ) -> dict[str, Any]:
        """Structured target of an ACP permission request (exact kind + target)."""
        raw = raw_input if isinstance(raw_input, dict) else {}
        if approval_kind == "command_execution":
            command = raw.get("command") or raw.get("cmd")
            if isinstance(command, list):
                command = " ".join(str(part) for part in command)
            return {"command": str(command).strip()} if isinstance(command, str) and command.strip() else {}
        if approval_kind == "file_change":
            paths = [
                str(location["path"]) for location in tool_call.get("locations") or []
                if isinstance(location, dict) and location.get("path")
            ]
            for key in ("path", "file_path", "filePath", "filepath", "file"):
                if isinstance(raw.get(key), str) and raw[key]:
                    paths.append(raw[key])
            return {"paths": list(dict.fromkeys(paths))}
        return {"input": raw_input} if raw_input is not None else {}

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
    def _elicitation_answer(value: Any, prop: dict[str, Any]) -> dict[str, Any]:
        if isinstance(value, list):
            return {"values": [_wire_answer(item) for item in value], "text": ""}
        text = _wire_answer(value)
        if prop.get("type") == "array":
            return {"values": [part.strip() for part in text.split(",")
                               if part.strip()], "text": ""}
        return {"values": [], "text": text}

    @classmethod
    def _elicitation_content_from_text(
        cls, params: dict[str, Any], text: str
    ) -> dict[str, Any]:
        """Convert a free-text answer with the requested schema's own types.

        Values must already be in schema wire form (``true``/``false``,
        numbers, enum values); anything else raises
        ``UserInputValidationError`` instead of being guessed.
        """
        properties = cls._elicitation_properties(params)
        schema = {"properties": properties}
        if len(properties) == 1:
            key, prop = next(iter(properties.items()))
            return content_for_elicitation(
                {key: cls._elicitation_answer(text, prop)}, schema=schema)
        try:
            import json
            parsed = json.loads(text)
        except (TypeError, ValueError):
            parsed = None
        if not isinstance(parsed, dict):
            return {}
        return content_for_elicitation(
            {key: cls._elicitation_answer(parsed[key], prop)
             for key, prop in properties.items() if key in parsed},
            schema=schema)

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
        agent_id = (
            handle["subagent_tool_calls"].get(
                str((params.get("toolCall") or {}).get("toolCallId") or ""))
            or handle["subagent_sessions"].get(str(params.get("sessionId") or ""))
        )
        loop = asyncio.get_running_loop()
        future: asyncio.Future[dict[str, Any]] = loop.create_future()
        message = str(params.get("message") or "Agent 请求输入")
        positive_content = self._approval_elicitation_content(params)
        delivery = self._register_control_delivery(handle, params)
        if positive_content is not None:
            handle["pending_approvals"][request_id] = {
                "future": future,
                "options": [],
                "params": dict(params),
                "kind": "elicitation",
                "positive_content": positive_content,
                "delivery": delivery,
            }
            sink.put_nowait(("approval", AgentEventType.APPROVAL_REQUESTED,
                             "acp.elicitation",
                             dump_payload(ApprovalRequestedPayload(
                                 approval_id=request_id,
                                 agent_id=str(agent_id) if agent_id else None,
                                 approval_kind="tool",
                                 title=message,
                                 reason=message,
                                 native={
                                     "access_mode": handle.get("access_mode"),
                                     "elicitation": True,
                                 },
                             ))))
            try:
                return await future
            finally:
                handle["pending_approvals"].pop(request_id, None)

        handle["pending_user_inputs"][request_id] = {
            "future": future,
            "params": dict(params),
            "delivery": delivery,
        }
        questions = questions_from_schema(
            params.get("requestedSchema") or {},
            title=message,
        )
        pending = dump_payload(UserInputRequestedPayload(
            request_id=request_id,
            user_input_kind="acp.elicitation",
            agent_id=str(agent_id) if agent_id else None,
            title=message,
            message=message,
            questions=questions,
            requested_schema=(
                params.get("requestedSchema")
                if isinstance(params.get("requestedSchema"), dict) else None),
            response_actions=["submit", "cancel", "decline"],
            native={key: value for key, value in params.items()
                    if key != "__muteki_rpc_request_id"},
        ))
        sink.put_nowait(("user_input", AgentEventType.USER_INPUT_REQUESTED,
                         "acp.elicitation", pending))
        try:
            return await future
        finally:
            handle["pending_user_inputs"].pop(request_id, None)

    async def _await_extension_approval(
        self, handle: dict[str, Any], *, approval_id: str,
        params: dict[str, Any], payload: dict[str, Any],
        resolve: Callable[[ApprovalDecision], Any], native_type: str,
        cancel_result: Any = None,
    ) -> Any:
        """Approval card for an engine extension request.

        ``resolve`` turns the operator's decision into the result object the
        engine expects; ``cancel_result`` answers when the session ends first.
        Returns ``cancel_result`` when no turn is listening for interactions.
        """
        sink = self._interaction_sink(handle)
        if sink is None:
            return cancel_result
        future: asyncio.Future = asyncio.get_running_loop().create_future()
        handle["pending_approvals"][approval_id] = {
            "future": future,
            "options": [],
            "params": dict(params),
            "delivery": self._register_control_delivery(handle, params),
            "resolve": resolve,
            "cancel_result": cancel_result,
            "native_type": native_type,
            "request_payload": payload,
        }
        sink.put_nowait(("approval", AgentEventType.APPROVAL_REQUESTED,
                         native_type, payload))
        try:
            return await future
        finally:
            handle["pending_approvals"].pop(approval_id, None)

    async def _await_extension_user_input(
        self, handle: dict[str, Any], *, request_id: str,
        params: dict[str, Any], payload: dict[str, Any],
        resolve: Callable[[str, dict[str, Any], str], "tuple[Any, str]"],
        native_type: str, cancel_result: Any = None,
    ) -> Any:
        """User-input card for an engine extension request.

        ``resolve(decision, answers, text)`` returns ``(engine_result,
        outcome)`` with outcome in ``answered|cancelled|declined``.
        """
        sink = self._interaction_sink(handle)
        if sink is None:
            return cancel_result
        future: asyncio.Future = asyncio.get_running_loop().create_future()
        handle["pending_user_inputs"][request_id] = {
            "future": future,
            "params": dict(params),
            "delivery": self._register_control_delivery(handle, params),
            "resolve": resolve,
            "cancel_result": cancel_result,
            "native_type": native_type,
        }
        sink.put_nowait(("user_input", AgentEventType.USER_INPUT_REQUESTED,
                         native_type, payload))
        try:
            return await future
        finally:
            handle["pending_user_inputs"].pop(request_id, None)

    # -- turn 流 --------------------------------------------------------------

    def send(
        self, session: AgentSessionRef, input: AgentInput
    ) -> AsyncIterator[AgentEvent]:
        if isinstance(input, ApprovalResponseInput):
            return self._approval_response_stream(session, input)
        if isinstance(input, UserInputResponseInput):
            return self._user_input_response_stream(session, input)
        if isinstance(input, MessageInput):
            return self._prompt_stream(session, input)
        return self.unsupported_input_stream(session, input)

    async def _approval_response_stream(
        self, session: AgentSessionRef, input: ApprovalResponseInput
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
            decision = ApprovalDecision.from_payload(input.payload.model_dump())
        except ValueError as exc:
            yield self.emit(build_event(
                AgentEventType.RUNTIME_ERROR, seq,
                native_type="acp.permission.invalid",
                payload=RuntimeErrorPayload(error=self.exception_failure(
                    exc, FailureCategory.VALIDATION, "approval.invalid",
                    message=f"Invalid approval response: {exc}")),
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
                payload=RuntimeErrorPayload(error=self.failure(
                    FailureCategory.UNKNOWN, "approval.stale",
                    message="ACP approval request is no longer pending")),
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
            if handle.get("control_delivery_enabled"):
                outcome = await self._await_control_delivery(pending.get("delivery"))
                if not outcome["ok"]:
                    yield self.emit(build_event(AgentEventType.RUNTIME_ERROR, seq,
                        payload=RuntimeErrorPayload(error=self.failure(
                            FailureCategory.TRANSPORT, "control.delivery_unknown",
                            message="ACP response delivery was not confirmed",
                            detail=str(outcome.get("detail") or ""),
                            delivery_unknown=True)), **common))
                    return
            yield self.emit(build_event(
                AgentEventType.APPROVAL_RESOLVED, seq,
                native_type="acp.elicitation",
                payload=ApprovalResolvedPayload(
                    approval_id=decision.approval_id,
                    decision="allow" if decision.allowed else "deny",
                    scope=decision.scope.value,
                    native={"outcome": result["action"]},
                ),
                **common,
            ))
            return
        resolver = pending.get("resolve")
        if resolver is not None:
            # Extension requests (plan exit, ...) answer with an engine-specific
            # result object instead of a permission option.
            result = resolver(decision)
            future = pending["future"]
            if not future.done():
                future.set_result(result)
            if handle.get("control_delivery_enabled"):
                outcome = await self._await_control_delivery(pending.get("delivery"))
                if not outcome["ok"]:
                    yield self.emit(build_event(AgentEventType.RUNTIME_ERROR, seq,
                        payload=RuntimeErrorPayload(error=self.failure(
                            FailureCategory.TRANSPORT, "control.delivery_unknown",
                            message="ACP response delivery was not confirmed",
                            detail=str(outcome.get("detail") or ""),
                            delivery_unknown=True)), **common))
                    return
            yield self.emit(build_event(
                AgentEventType.APPROVAL_RESOLVED, seq,
                native_type=str(pending.get("native_type") or "acp.extension_approval"),
                payload=ApprovalResolvedPayload(
                    approval_id=decision.approval_id,
                    decision="allow" if decision.allowed else "deny",
                    scope=ApprovalScope.ONCE.value,
                    native={"result": result},
                ),
                **common,
            ))
            return
        request_payload = pending.get("request_payload") or {}
        cwd = str(handle.get("cwd") or "")
        current_mode = str(handle.get("access_mode") or AccessMode.SUPERVISED.value)
        offered_scopes = request_payload.get("scopes")
        if (
            decision.scope is ApprovalScope.SESSION
            and offered_scopes is not None
            and "session" not in offered_scopes
        ):
            decision = ApprovalDecision(
                approval_id=decision.approval_id, choice=decision.choice,
                scope=ApprovalScope.ONCE, note=decision.note,
                option_id=decision.option_id)
        requested_decision = decision
        decision = native_decision(
            request_payload, decision,
            native_scope_exact=self._native_session_scope_exact(handle, pending),
            cwd=cwd)
        if decision.allowed:
            wanted = (
                ("allow_always",)
                if decision.scope is ApprovalScope.SESSION
                else ("allow_once",)
            )
        else:
            wanted = (
                ("reject_always",)
                if decision.scope is ApprovalScope.SESSION
                else ("reject_once",)
            )
        option_id = self._permission_option_for_mode(options, wanted, current_mode)
        if decision.option_id:
            offered = next((row for row in options if str(row.get("optionId") or "") == decision.option_id), None)
            option_id = decision.option_id if offered is not None and offered.get("kind") in wanted else None
            if offered is not None and option_id is None and requested_decision.scope is ApprovalScope.SESSION:
                option_id = self._permission_option_for_mode(options, wanted, current_mode)
        if option_id is None:
            yield self.emit(build_event(
                AgentEventType.RUNTIME_ERROR, seq,
                native_type="acp.permission.unsupported_scope",
                payload=RuntimeErrorPayload(error=self.failure(
                    FailureCategory.UNSUPPORTED, "approval.unsupported_scope",
                    message="The requested permission scope is not offered by this request")),
                **common,
            ))
            return
        chosen = next(
            (row for row in options if str(row.get("optionId") or "") == option_id),
            None,
        )
        requested_mode = _option_requested_mode(chosen or {})
        if requested_mode is not None:
            try:
                # allow_always never switches the session access mode.
                reject_mode_upgrade(current_mode, requested_mode)
            except ApprovalScopeError as exc:
                yield self.emit(build_event(
                    AgentEventType.RUNTIME_ERROR, seq,
                    native_type="acp.permission.mode_upgrade",
                    payload=RuntimeErrorPayload(error=self.failure(
                        FailureCategory.UNSUPPORTED, "approval.mode_upgrade",
                        message=str(exc), native_code=exc.code)),
                    **common,
                ))
                return
        # Engine-side scope may be narrower than the operator's session
        # grant; the ledger answers repeats of the exact target one-shot.
        self._approval_grants(handle).remember(
            request_payload, requested_decision, cwd=cwd)
        future = pending["future"]
        if not future.done():
            future.set_result(option_id)
        if handle.get("control_delivery_enabled"):
            outcome = await self._await_control_delivery(pending.get("delivery"))
            if not outcome["ok"]:
                yield self.emit(build_event(AgentEventType.RUNTIME_ERROR, seq,
                    payload=RuntimeErrorPayload(error=self.failure(
                        FailureCategory.TRANSPORT, "control.delivery_unknown",
                        message="ACP response delivery was not confirmed",
                        detail=str(outcome.get("detail") or ""),
                        delivery_unknown=True)), **common))
                return
        yield self.emit(build_event(
            AgentEventType.APPROVAL_RESOLVED, seq,
            native_type="acp.request_permission",
            payload=ApprovalResolvedPayload(
                approval_id=decision.approval_id,
                decision="allow" if decision.allowed else "deny",
                scope=decision.scope.value,
                option_id=option_id,
                native={
                    "native_option": next(
                        (dict(row) for row in options
                         if str(row.get("optionId") or "") == option_id), None),
                    "outcome": "selected" if option_id else "cancelled",
                },
            ),
            **common,
        ))

    async def _user_input_response_stream(
        self, session: AgentSessionRef, input: UserInputResponseInput
    ) -> AsyncIterator[AgentEvent]:
        sid = session.agent_session_id
        seq = self.sequencer_for(sid)
        handle = self._handle_for(sid)
        request_id = input.payload.request_id
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
                payload=RuntimeErrorPayload(error=self.failure(
                    FailureCategory.UNKNOWN, "elicitation.stale",
                    message="ACP elicitation is no longer pending")),
                **common,
            ))
            return
        decision = input.payload.decision
        structured = dict(input.payload.answers)
        resolver = pending.get("resolve")
        resolved_outcome: Optional[str] = None
        if resolver is not None:
            result, resolved_outcome = resolver(decision, structured, input.text)
        elif decision in {"cancel", "decline"}:
            result = {"action": decision}
        elif "answers" in input.payload.model_fields_set:
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
            result = {"action": "accept", "content": content}
        else:
            try:
                content = self._elicitation_content_from_text(
                    pending["params"], input.text)
            except UserInputValidationError as exc:
                # The request stays pending so the user can answer again.
                yield self.emit(build_event(
                    AgentEventType.RUNTIME_ERROR, seq,
                    native_type="acp.elicitation.invalid",
                    payload=RuntimeErrorPayload(error=self.failure(
                        FailureCategory.VALIDATION, "user_input.invalid",
                        message=f"Invalid user input response: {exc.message}",
                        native_code=exc.code)),
                    **common,
                ))
                return
            result = (
                {"action": "accept", "content": content}
                if content else {"action": "decline"}
            )
        future = pending["future"]
        if not future.done():
            future.set_result(result)
        if handle.get("control_delivery_enabled"):
            outcome = await self._await_control_delivery(pending.get("delivery"))
            if not outcome["ok"]:
                yield self.emit(build_event(AgentEventType.RUNTIME_ERROR, seq,
                    payload=RuntimeErrorPayload(error=self.failure(
                        FailureCategory.TRANSPORT, "control.delivery_unknown",
                        message="ACP response delivery was not confirmed",
                        detail=str(outcome.get("detail") or ""),
                        delivery_unknown=True)), **common))
                return
        action = str(result.get("action") or "") if isinstance(result, dict) else ""
        yield self.emit(build_event(
            AgentEventType.USER_INPUT_RESOLVED, seq,
            native_type=str(pending.get("native_type") or "acp.elicitation"),
            payload=UserInputResolvedPayload(
                request_id=request_id,
                outcome=resolved_outcome or {
                    "accept": "answered",
                    "cancel": "cancelled",
                    "decline": "declined",
                }.get(action, "answered"),
                answers=structured or None,
                native={"decision": decision},
            ),
            **common,
        ))

    @staticmethod
    def _register_control_delivery(handle: dict[str, Any], params: dict[str, Any]) -> Any:
        if not handle.get("control_delivery_enabled"):
            return None
        delivery = asyncio.get_running_loop().create_future()
        handle.setdefault("control_deliveries", {})[str(params["__muteki_rpc_request_id"])] = delivery
        return delivery

    def _control_delivery_result(self, sid: str, params: dict[str, Any], outcome: dict[str, Any]) -> None:
        handle = self._handle_for(sid) or {}
        delivery = handle.get("control_deliveries", {}).pop(str(params.get("__muteki_rpc_request_id") or ""), None)
        if delivery is not None and not delivery.done():
            delivery.set_result(outcome)

    @staticmethod
    async def _await_control_delivery(delivery: Any) -> dict[str, Any]:
        if delivery is None:
            return {"ok": False, "detail": "Native response delivery tracker is missing"}
        try:
            return await asyncio.wait_for(asyncio.shield(delivery), timeout=30)
        except asyncio.TimeoutError:
            return {"ok": False, "detail": "Native response write confirmation timed out"}

    def _native_mode_target(
        self, transport: "AcpTransport", session_id: str, mode_id: str
    ) -> Optional[str]:
        """How the engine switches to ``mode_id``: ``"config"``, ``"mode"`` or None."""
        setup = transport.session_setup(session_id)
        for option in setup.get("configOptions") or []:
            if not isinstance(option, dict):
                continue
            if option.get("id") == "mode" or option.get("category") == "mode":
                choices = [
                    choice for group in option.get("options") or []
                    for choice in (group.get("options", []) if "group" in group else [group])
                ]
                if any(choice.get("value") == mode_id for choice in choices):
                    return "config"
        modes = (setup.get("modes") or {}).get("availableModes") or []
        if any(isinstance(row, dict) and row.get("id") == mode_id for row in modes):
            return "mode"
        return None

    async def _set_native_mode(
        self, transport: "AcpTransport", session_id: str, mode_id: str, how: str
    ) -> None:
        if how == "config":
            setup = transport.session_setup(session_id)
            option = next(
                item for item in setup.get("configOptions") or []
                if isinstance(item, dict)
                and (item.get("id") == "mode" or item.get("category") == "mode"))
            await transport.set_config_option(session_id, str(option["id"]), mode_id)
        else:
            await transport.set_mode(session_id, mode_id)

    async def _apply_interaction_mode(
        self, handle: dict[str, Any], transport: "AcpTransport",
        session_id: str, wanted: str,
    ) -> Optional[AgentFailure]:
        """Switch the session's native plan mode; a typed failure when it cannot."""
        if wanted == "plan":
            target_mode = self.plan_mode_id
        else:
            target_mode = handle.get("default_mode_id")
            if not target_mode:
                handle["interaction_mode"] = wanted
                return None
        how = self._native_mode_target(transport, session_id, str(target_mode))
        if how is None:
            if wanted != "plan":
                handle["interaction_mode"] = wanted
                return None
            return self.failure(
                FailureCategory.UNSUPPORTED, "plan_mode_unsupported",
                message=f"{self.id} does not advertise a native plan mode "
                        f"({self.plan_mode_id!r}) for this session")
        try:
            await self._set_native_mode(transport, session_id, str(target_mode), how)
        except AcpError as exc:
            return self.exception_failure(
                exc, FailureCategory.PROVIDER, "interaction_mode_failed",
                message=f"Switching {self.id} to mode {target_mode!r} failed: {exc}")
        handle["interaction_mode"] = wanted
        return None

    async def _prompt_stream(
        self, session: AgentSessionRef, input: MessageInput
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
                payload=SessionPayload(
                    transport="acp",
                    adapter_id=self.id,
                    instance_id=self.identity.instance_id,
                    cwd=handle["cwd"],
                    native={"replay_updates": len(handle["replay_events"]),
                            **({"process_ownership": handle["process_ownership"]} if handle.get("process_ownership") else {})}),
                **common))
        turn_id = new_id("turn")
        handle["current_turn_id"] = turn_id
        yield self.emit(build_event(
            AgentEventType.TURN_STARTED, seq,
            external_session_id=external_id, turn_id=turn_id,
            native_type="acp.prompt.start",
            payload=TurnStartedPayload(kind=input.kind),
            **common))

        wanted_mode = str(getattr(input.payload, "interaction_mode", "default") or "default")
        if wanted_mode != handle.get("interaction_mode", "default"):
            mode_error = await self._apply_interaction_mode(
                handle, transport, external_id, wanted_mode)
            if mode_error is not None:
                handle["current_turn_id"] = None
                # The session itself is live; only this turn failed, so the
                # next turn must not re-emit SESSION_STARTED.
                handle["turns"] = max(handle["turns"], 1)
                yield self.emit(build_event(
                    AgentEventType.TURN_FAILED, seq,
                    external_session_id=external_id, turn_id=turn_id,
                    native_type="acp.interaction_mode.failed",
                    payload=TurnFailedPayload(error=mode_error),
                    **common))
                return

        queue: asyncio.Queue = asyncio.Queue()
        handle["event_sink"] = queue

        async def run_prompt() -> dict[str, Any]:
            try:
                return await transport.prompt(
                    external_id, input.text,
                    timeout=self.conversation_turn_timeout(
                        handle.get("thread_id"), self._prompt_timeout),
                    meta=self._prompt_meta(handle),
                    prompt_id=self._prompt_id(handle))
            except Exception as exc:  # noqa: BLE001
                return {"__error__": exc}
            finally:
                await queue.put(("done", None, None, None))

        task = asyncio.ensure_future(run_prompt())
        stop_reason = ""
        error: Optional[Exception] = None
        text_parts: list[str] = []
        released = False
        while True:
            if (self._release_background_turn(handle)
                    and "".join(text_parts).strip()
                    and queue.empty()):
                await asyncio.sleep(0)
                if (queue.empty() and self._release_background_turn(handle)
                        and "".join(text_parts).strip()):
                    released = True
                    break
            kind, etype, native_kind, payload = await queue.get()
            if kind == "done":
                break
            if (etype is AgentEventType.MESSAGE_DELTA
                    and payload.get("role") == "assistant"
                    and payload.get("phase") != "commentary"):
                text_parts.append(str(payload.get("text") or ""))
            yield self.emit(build_event(
                etype, seq,
                external_session_id=external_id, turn_id=turn_id,
                native_type=native_kind, payload=payload,
                **common))
        if released and not task.done():
            task.cancel()
            await asyncio.gather(task, return_exceptions=True)
            result = {"stopReason": "end_turn"}
        else:
            result = await task
        handle["event_sink"] = None
        handle["turns"] += 1
        handle["current_turn_id"] = None

        if isinstance(result.get("__error__"), Exception):
            error = result["__error__"]
            if (isinstance(error, AcpRequestError) and error.code is not None
                    and error.code in self.rate_limit_error_codes):
                # The engine answered the prompt with its usage-limit code and
                # is still running; only this turn fails.
                yield self.emit(build_event(
                    AgentEventType.TURN_FAILED, seq,
                    external_session_id=external_id, turn_id=turn_id,
                    native_type="acp.prompt.usage_limit",
                    payload=TurnFailedPayload(error=self.exception_failure(
                        error, FailureCategory.USAGE_LIMIT, "usage_limit",
                        message=str(error).strip() or "Usage limit reached",
                        retryable=True)),
                    **common))
                return
            timed_out = isinstance(error, asyncio.TimeoutError)
            err_text = str(error).strip() or type(error).__name__
            failure = self.exception_failure(
                error,
                FailureCategory.TIMEOUT if timed_out else FailureCategory.TRANSPORT,
                "prompt_timeout" if timed_out else "prompt_error",
                message=err_text,
                retryable=timed_out,
            )
            yield self.emit(build_event(
                AgentEventType.TURN_FAILED, seq,
                external_session_id=external_id, turn_id=turn_id,
                native_type="acp.prompt.error",
                payload=TurnFailedPayload(error=failure),
                **common))
            yield self.emit(build_event(
                AgentEventType.RUNTIME_EXITED, seq,
                external_session_id=external_id, turn_id=turn_id,
                native_type="acp.exit",
                payload=RuntimeExitedPayload(
                    classification=classify_exit(
                        error=err_text, timed_out=timed_out),
                    error=failure),
                **common))
            return

        stop_reason = str(result.get("stopReason") or "")
        result_usage = self._turn_result_usage(result)
        if result_usage is not None:
            yield self.emit(build_event(
                AgentEventType.USAGE_UPDATED, seq,
                external_session_id=external_id, turn_id=turn_id,
                native_type="acp.prompt.usage",
                payload=result_usage, **common))
        if stop_reason == "cancelled":
            # 中断：只分类收尾，不伪造正常完成。
            yield self.emit(build_event(
                AgentEventType.TURN_FAILED, seq,
                external_session_id=external_id, turn_id=turn_id,
                native_type="acp.prompt.cancelled",
                payload=TurnFailedPayload(
                    error=self.failure(
                        FailureCategory.CANCELLED,
                        "interrupted" if handle.pop("interrupt_requested", False) else "cancelled",
                        message="ACP turn cancelled",
                        detail=f"stopReason={stop_reason}"),
                    native={"stop_reason": stop_reason}),
                **common))
        else:
            for event in self.completed_turn_events(
                seq, text="".join(text_parts),
                common={**common, "external_session_id": external_id, "turn_id": turn_id},
                native_type="acp.prompt.completed",
                payload=TurnCompletedPayload(stop_reason=stop_reason or "end_turn")):
                yield event

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
                options=ctx["options"],
            )
            try:
                await self._launch(request, ctx["plan"], ctx["token"])
            except Exception as exc:  # noqa: BLE001
                yield self.emit(build_event(
                    AgentEventType.RUNTIME_ERROR, seq,
                    agent_session_id=sid, external_session_id=external,
                    payload=RuntimeErrorPayload(error=self.exception_failure(
                        exc, FailureCategory.TRANSPORT, "resume_failed",
                        message=f"ACP resume failed: {exc}", retryable=True)),
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
        # 回放事件只进 SESSION_RESUMED 的 native 供排障检阅，不投影
        # （不重复写历史消息，也不伪造 MESSAGE_DELTA）。
        replayed = list(handle["replay_events"])
        yield self.emit(build_event(
            AgentEventType.SESSION_RESUMED, seq,
            external_session_id=external_id,
            native_type="acp.session.resumed",
            payload=SessionPayload(
                transport="acp",
                native={
                    "replay_updates": len(replayed),
                    "replays": replayed,
                }),
            **common))

    # -- 控制面 ---------------------------------------------------------------

    async def interrupt(self, session: AgentSessionRef) -> CommandReceipt:
        self._mark_turn_interrupted(session.agent_session_id)
        handle = self._handle_for(session.agent_session_id)
        transport = (handle or {}).get("transport")
        external_id = (handle or {}).get("external_session_id") \
            or session.external_session_id
        if transport is None or not external_id:
            return self.unsupported_receipt(
                "interrupt", "no_active_session", session=session)
        # ACP: the client answers every pending permission request as
        # cancelled once it sends session/cancel.
        if handle is not None:
            handle["interrupt_requested"] = True
            self._cancel_pending_interactions(handle)
        await transport.cancel(external_id)
        return CommandReceipt(
            command_id=new_id("cmd"),
            state=ReceiptState.COMPLETED,
            aggregate=AggregateRef(type="agent_session",
                                   id=session.agent_session_id),
        )

    @staticmethod
    def _cancel_pending_interactions(handle: dict[str, Any]) -> None:
        """Answer every open approval / user-input request as cancelled."""
        for pending in handle.get("pending_approvals", {}).values():
            future = pending.get("future")
            if future is not None and not future.done():
                if pending.get("resolve") is not None:
                    future.set_result(pending.get("cancel_result"))
                elif pending.get("kind") == "elicitation":
                    future.set_result({"action": "cancel"})
                else:
                    future.set_result(None)
        for pending in handle.get("pending_user_inputs", {}).values():
            future = pending.get("future")
            if future is not None and not future.done():
                future.set_result(
                    pending.get("cancel_result")
                    if pending.get("resolve") is not None
                    else {"action": "cancel"})

    async def _teardown(self, session: AgentSessionRef) -> str:
        handle = self._acp.get(session.agent_session_id)
        if not handle:
            return EXIT_RESUMABLE if session.resume_handle else "closed"
        self._cancel_pending_interactions(handle)
        transport: Optional[AcpTransport] = handle.get("transport")
        returncode: Optional[int] = None
        if transport is not None:
            returncode = await transport.close()
        terminals = handle.get("terminals")
        if terminals is not None:
            await terminals.close()
        self._acp.pop(session.agent_session_id, None)
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
