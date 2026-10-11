"""OpenCode Server Adapter (OpenCode 2 native API).

The adapter owns the managed ``opencode serve`` process, the server password,
session handles and the shared approval / event plumbing. The OpenCode 2
protocol (``/api/info``, ``/api/event``, session inbox/execution events,
forms, per-session rules and runtime MCP) lives in
``opencode2.OpenCode2Runtime``. OpenCode releases before major version 2 are
rejected with ``opencode.major_unsupported``; the version is never guessed.

The one-shot ``opencode run --format json`` CLI path is
``muteki.solver.cli_driver.OpenCodeDriver`` + ``CliDriverAdapter``
(``cli.opencode``); this module does not import the solver layer.
"""

from __future__ import annotations

import asyncio
import json
import os
import re
import secrets
import time
from pathlib import Path
from typing import Any, AsyncIterator, Callable, Optional

import httpx

from muteki.platform.contracts.capabilities import CapabilityInjectionPlan
from muteki.platform.contracts.external_agents import (
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
)
from muteki.platform.contracts.protocols import RuntimeOperationAdapter
from muteki.platform.contracts.receipts import CommandReceipt

from .base import BaseExternalAgentAdapter
from .approvals import (
    ApprovalDecision,
    ApprovalTarget,
    SessionApprovalGrants,
    native_decision,
)
from .capabilities import (
    BOOL_CAPABILITY_FIELDS,
    CapabilityProbeReport,
    SOURCE_STATIC,
    conservative_capabilities,
    _probe_version,
)
from .events import build_event
from muteki.platform.contracts.agent_events import (
    AgentNodePayload,
    AgentUpdatedPayload,
    ApprovalRequestedPayload,
    ApprovalResolvedPayload,
    FailureCategory,
    PlanPayload,
    PlanTaskPayload,
    RuntimeErrorPayload,
    RuntimeWarningPayload,
    dump_payload,
)
from .process_supervisor import SupervisedProcess
from .rpc import ProcessOutputLog
from .runtime_capabilities import RuntimeCapabilitySnapshot


class OpenCodeError(RuntimeError):
    """OpenCode HTTP 或版本错误。``code`` 是稳定机器码，不靠正文匹配。"""

    def __init__(
        self, message: str, *, code: str = "opencode.http_error",
        status: Optional[int] = None,
    ) -> None:
        super().__init__(message)
        self.code = code
        self.status = status


_SSE_STALL_TIMEOUT_S = 45.0


class OpenCodeHttpClient:
    """``opencode serve`` HTTP API 的最小异步客户端（httpx）。

    只做传输：端点路径、鉴权头与错误解包；不含任何会话语义。
    """

    def __init__(
        self,
        base_url: str,
        *,
        username: str = "",
        password: str = "",
        timeout: float = 30.0,
    ) -> None:
        auth = None
        if password or username:
            auth = httpx.BasicAuth(username or "opencode", password)
        self._client = httpx.AsyncClient(
            base_url=base_url.rstrip("/"), auth=auth,
            timeout=httpx.Timeout(timeout, read=None),
            trust_env=not base_url.startswith(
                ("http://127.0.0.1", "http://localhost")),
        )

    async def close(self) -> None:
        await self._client.aclose()

    async def request(
        self, method: str, path: str, *,
        body: Optional[dict[str, Any]] = None,
        timeout: Optional[float] = None,
    ) -> Any:
        try:
            resp = await self._client.request(
                method, path, json=body, timeout=timeout)
        except httpx.HTTPError as exc:
            raise OpenCodeError(
                f"{method} {path} 连接失败：{type(exc).__name__}: {exc}",
                code="opencode.http_timeout" if isinstance(exc, httpx.TimeoutException)
                else "opencode.http_error",
            ) from exc
        if resp.status_code == 204:
            return None
        if resp.status_code >= 400:
            raise OpenCodeError(
                f"{method} {path} -> {resp.status_code}: {resp.text}",
                status=resp.status_code)
        if not resp.content:
            return None
        try:
            return resp.json()
        except json.JSONDecodeError as exc:
            raise OpenCodeError(
                f"{method} {path} 返回非 JSON：{resp.text}") from exc

    async def get(self, path: str, **kw: Any) -> Any:
        return await self.request("GET", path, **kw)

    async def post(self, path: str, body: Optional[dict[str, Any]] = None,
                   **kw: Any) -> Any:
        return await self.request("POST", path, body=body, **kw)

    async def patch(self, path: str, body: Optional[dict[str, Any]] = None,
                    **kw: Any) -> Any:
        return await self.request("PATCH", path, body=body, **kw)

    async def stream_sse(
        self,
        path: str,
        on_event: Callable[[str, dict[str, Any]], None],
        stop: asyncio.Event,
        connected: Optional[asyncio.Event] = None,
        *,
        stall_timeout: float = _SSE_STALL_TIMEOUT_S,
    ) -> None:
        """订阅 SSE（``GET /event``）。

        ``stop`` 置位后正常返回；其他任何结束（HTTP 拒绝、连接断开、服务端
        关流、超过 ``stall_timeout`` 没有任何帧）都抛带稳定 ``code`` 的
        ``OpenCodeError``，由持有该任务的一方决定回合如何收尾。
        ``connected`` 在收到 ``server.connected`` 帧后置位：服务端此时已把
        订阅挂到 bus 上，之后发出的 prompt 不会丢事件。服务端每约 10 秒发
        ``server.heartbeat``，所以长时间无帧即连接已死。

        非 JSON 事件帧跳过（bus 事件格式由 Runtime 演进，传输层不做语义判定）。
        """
        try:
            async with self._client.stream("GET", path) as resp:
                if resp.status_code >= 400:
                    await resp.aread()
                    raise OpenCodeError(
                        f"GET {path} -> {resp.status_code}: {resp.text}",
                        code="opencode.sse_rejected")
                lines = resp.aiter_lines().__aiter__()
                event_name = ""
                while not stop.is_set():
                    try:
                        line = await asyncio.wait_for(
                            lines.__anext__(), timeout=stall_timeout)
                    except StopAsyncIteration:
                        break
                    except asyncio.TimeoutError as exc:
                        raise OpenCodeError(
                            f"GET {path} 超过 {stall_timeout}s 没有任何帧"
                            "（含 heartbeat）",
                            code="opencode.sse_stalled") from exc
                    if line.startswith("event:"):
                        event_name = line[6:].strip()
                    elif line.startswith("data:"):
                        try:
                            data = json.loads(line[5:].strip())
                        except json.JSONDecodeError:
                            continue
                        if isinstance(data, dict):
                            if connected is not None and str(
                                    data.get("type") or event_name
                            ) == "server.connected":
                                connected.set()
                            on_event(event_name, data)
                        event_name = ""
        except httpx.HTTPError as exc:
            if stop.is_set():
                return
            raise OpenCodeError(
                f"GET {path} 连接中断：{exc}",
                code="opencode.sse_closed") from exc
        if not stop.is_set():
            raise OpenCodeError(
                f"GET {path} 被服务端关闭", code="opencode.sse_closed")


def _pick_free_port() -> int:
    """选取空闲 loopback 端口（0 由内核分配）。"""
    import socket
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return int(sock.getsockname()[1])


_BASE62 = "0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz"
_last_message_stamp = 0


def _opencode_message_id() -> str:
    """Client-chosen user message id in OpenCode's own ascending format.

    OpenCode sorts a session's messages by id and decides from that order
    whether the last user message already has an assistant reply. An id that
    sorts before earlier assistant ids (a random uuid does) makes a later
    prompt look already answered. ``msg_`` + 12 hex of ``ms * 0x1000 +
    counter`` + 14 random base62 characters, strictly increasing per process.
    """
    global _last_message_stamp
    stamp = max(int(time.time() * 1000) * 0x1000, _last_message_stamp + 1)
    _last_message_stamp = stamp
    suffix = "".join(secrets.choice(_BASE62) for _ in range(14))
    return f"msg_{stamp & 0xFFFFFFFFFFFF:012x}{suffix}"


_SUPPORTED_ACCESS_MODES = (
    AccessMode.SUPERVISED.value,
    AccessMode.AUTO_ACCEPT_EDITS.value,
    AccessMode.FULL_ACCESS.value,
)
_OPENCODE_EDIT_PERMISSIONS = frozenset({
    "edit", "write", "patch", "apply_patch",
})

_VERSION_MAJOR = re.compile(r"(?:^|[^0-9])(\d+)\.\d+")


def _opencode_major(version: str) -> Optional[int]:
    """Return the numeric major from ``opencode --version``, or None."""
    match = _VERSION_MAJOR.search(str(version or "").strip())
    if match is None:
        return None
    return int(match.group(1))


def _plan_from_todos(todos: Any) -> PlanPayload:
    tasks: list[PlanTaskPayload] = []
    if isinstance(todos, list):
        for index, todo in enumerate(todos, start=1):
            if not isinstance(todo, dict):
                continue
            status = str(todo.get("status") or "pending").strip()
            if status == "canceled":
                status = "cancelled"
            if status not in {"pending", "in_progress", "completed", "blocked", "cancelled"}:
                status = "pending"
            title = str(todo.get("content") or "").strip() or f"Todo {index}"
            tasks.append(PlanTaskPayload(
                task_id=f"todo-{index}", title=title, status=status))
    return PlanPayload(tasks=tasks, patch=False)


def _as_process(proc: Any) -> Optional[asyncio.subprocess.Process]:
    if isinstance(proc, SupervisedProcess):
        return proc.process
    if isinstance(proc, asyncio.subprocess.Process):
        return proc
    return None


class OpenCodeServerAdapter(BaseExternalAgentAdapter, RuntimeOperationAdapter):
    """OpenCode's structured server (HTTP + SSE) Adapter for OpenCode 2."""

    adapter_id = "opencode.server"
    context_compaction_events = True
    host_context_channel = "opencode_instructions"

    def __init__(
        self,
        *,
        binary: Optional[str] = None,
        base_url: Optional[str] = None,
        server_username: str = "",
        server_password: str = "",
        manage_server: bool = True,
        startup_timeout: float = 30.0,
        prompt_timeout: float = 600.0,
        extra_env: Optional[dict[str, str]] = None,
        log_root: Optional[str | Path] = None,
        **kwargs: Any,
    ) -> None:
        super().__init__(self.adapter_id, **kwargs)
        self._binary = binary or os.environ.get("MUTEKI_OPENCODE_BIN",
                                                "opencode")
        self._log_root = Path(log_root) if log_root is not None else None
        self._server_outputs: dict[int, ProcessOutputLog] = {}
        self._attach_url = base_url
        self._server_username = server_username or os.environ.get(
            "OPENCODE_SERVER_USERNAME", "")
        self._server_password = server_password or os.environ.get(
            "OPENCODE_SERVER_PASSWORD", "")
        if not base_url and manage_server and not self._server_password:
            # A loopback server without auth lets any local process drive the
            # session (shell included). The secret lives only in this process
            # and the child's environment, never in argv or logs.
            self._server_password = secrets.token_urlsafe(24)
            self._server_username = self._server_username or "opencode"
        self._manage_server = manage_server
        self._startup_timeout = float(startup_timeout)
        self._prompt_timeout = float(prompt_timeout)
        self._extra_env = dict(extra_env or {})
        # agent_session_id -> 会话句柄
        self._sessions: dict[str, dict[str, Any]] = {}
        self._runtime_version = ""
        from .opencode2 import OpenCode2Runtime
        self._v2 = OpenCode2Runtime(self)

    async def _require_v2(self) -> str:
        """Read the runtime's version identity; only OpenCode 2 is spoken."""
        if self._attach_url:
            client = OpenCodeHttpClient(self._attach_url, username=self._server_username,
                                        password=self._server_password)
            try:
                response = await client._client.get("/api/info", timeout=5)
            finally:
                await client.close()
            if response.status_code == 401:
                raise OpenCodeError("OpenCode server rejected its configured password",
                                    code="opencode.server_unauthorized", status=401)
            body: Any = None
            if (response.status_code == 200 and response.headers.get(
                    "content-type", "").split(";")[0] == "application/json"):
                body = response.json()
            if not isinstance(body, dict) or not isinstance(body.get("pid"), int):
                raise OpenCodeError("Configured server did not identify itself as OpenCode 2",
                                    code="opencode.version_unreadable")
            version = str(body.get("version") or "")
        else:
            version = await asyncio.to_thread(_probe_version, self._binary)
        major = _opencode_major(version)
        if major is None:
            raise OpenCodeError(f"OpenCode reported no readable version: {version!r}",
                                code="opencode.version_unreadable")
        if major < 2:
            raise OpenCodeError(
                f"opencode {version} is major version {major}; Muteki supports OpenCode 2 or newer",
                code="opencode.major_unsupported")
        self._runtime_version = version
        return version

    # -- server 生命周期 -------------------------------------------------------

    def _server_env(
        self, session_env: Optional[dict[str, str]] = None
    ) -> dict[str, str]:
        env = {
            "OPENCODE_DISABLE_AUTOUPDATE": "1",
            "OPENCODE_DISABLE_LSP_DOWNLOAD": "1",
        }
        env.update(self._extra_env)
        env.update(dict(session_env or {}))
        if self._server_password or self._server_username:
            env["OPENCODE_SERVER_USERNAME"] = self._server_username \
                or "opencode"
            env["OPENCODE_SERVER_PASSWORD"] = self._server_password
        return env

    def server_output_detail(self, proc: Any) -> str:
        process = _as_process(proc)
        output = self._server_outputs.get(process.pid) if process is not None else None
        return output.detail() if output is not None else ""

    async def _stop_server(self, proc: Any) -> int:
        """Stop a supervised server process group. Not used as interrupt."""
        process = _as_process(proc)
        if process is None:
            return -1
        try:
            if isinstance(proc, SupervisedProcess):
                await proc.terminate()
            elif process.returncode is None:
                try:
                    process.terminate()
                    await asyncio.wait_for(process.wait(), timeout=5.0)
                except (asyncio.TimeoutError, ProcessLookupError):
                    try:
                        process.kill()
                    except ProcessLookupError:
                        pass
                    await process.wait()
        finally:
            output = self._server_outputs.pop(process.pid, None)
            if output is not None:
                await output.close()
        return int(process.returncode if process.returncode is not None else -1)

    # -- probe ----------------------------------------------------------------

    async def probe(self, request: ProbeRequest) -> AgentCapabilities:
        try:
            await self._require_v2()
        except (OpenCodeError, OSError, httpx.HTTPError, ValueError) as exc:
            caps = conservative_capabilities(transport_kind="http", capability_source=SOURCE_STATIC)
            self._probe_cache = CapabilityProbeReport(
                adapter_id=self.identity.adapter_id, instance_id=self.identity.instance_id,
                capabilities=caps, binary_path=self._binary,
                field_sources={name: SOURCE_STATIC for name in BOOL_CAPABILITY_FIELDS},
                degradations=[f"{getattr(exc, 'code', 'opencode.probe_failed')}: {exc}"], detail=str(exc))
            return caps
        return await self._v2.probe(request)

    # -- 本机组件 --------------------------------------------------------------

    def _native_launch_env(self, request: SessionStart) -> dict[str, str]:
        """Load Muteki components through per-process config, keeping native user data."""
        env = dict(request.options.env)
        extensions = request.options.chat_native_extensions
        agents = request.options.chat_native_agents
        if not extensions and not agents:
            return env
        if self._attach_url:
            raise OpenCodeError("已连接的 OpenCode 服务不支持按会话加载本机组件",
                                code="opencode.native_components_attach_unsupported")
        raw = env.get("OPENCODE_CONFIG_CONTENT", self._extra_env.get("OPENCODE_CONFIG_CONTENT", ""))
        try:
            config = json.loads(raw) if raw else {}
        except ValueError as exc:
            raise OpenCodeError(f"OPENCODE_CONFIG_CONTENT 不是有效 JSON：{exc}",
                                code="opencode.config_invalid") from exc
        if not isinstance(config, dict):
            raise OpenCodeError("OPENCODE_CONFIG_CONTENT 必须为 JSON 对象", code="opencode.config_invalid")
        configured_plugins = config.get("plugin", [])
        if not isinstance(configured_plugins, list):
            raise OpenCodeError("OpenCode plugin 配置必须为数组", code="opencode.config_invalid")
        plugins = list(configured_plugins)
        for entry in extensions:
            uri = Path(entry).resolve().as_uri()
            if uri not in plugins:
                plugins.append(uri)
        if plugins:
            config["plugin"] = plugins
        configured_agents = config.get("agent", {})
        if not isinstance(configured_agents, dict):
            raise OpenCodeError("OpenCode agent 配置必须为对象", code="opencode.config_invalid")
        from muteki.conversation.chat_plugin_components import frontmatter
        native_agents = dict(configured_agents)
        for entry in agents:
            file = Path(entry)
            try:
                metadata, prompt = frontmatter(file)
            except (OSError, ValueError) as exc:
                raise OpenCodeError(f"OpenCode Agent 组件读取失败：{file}: {exc}",
                                    code="opencode.native_component_invalid") from exc
            name = file.stem
            if name in native_agents:
                raise OpenCodeError(f"OpenCode Agent 组件名称冲突：{name}",
                                    code="opencode.native_component_conflict")
            native_agents[name] = {key: value for key, value in metadata.items() if key != "name"}
            native_agents[name].update(prompt=prompt, mode=metadata.get("mode", "subagent"))
        if native_agents:
            config["agent"] = native_agents
        env["OPENCODE_CONFIG_CONTENT"] = json.dumps(config, ensure_ascii=False)
        return env

    # -- 启动 / 接管 -------------------------------------------------------------

    async def _launch(
        self,
        request: SessionStart,
        plan: Optional[CapabilityInjectionPlan],
        bearer_token: Optional[str],
    ) -> dict[str, Any]:
        native_env = self._native_launch_env(request)
        request = request.model_copy(update={
            "options": request.options.model_copy(update={"env": native_env}),
        })
        await self._require_v2()
        return await self._v2.launch(request, plan, bearer_token)

    # -- 事件流与审批 ----------------------------------------------------------

    async def _close_event_stream(self, handle: dict[str, Any]) -> None:
        stop = handle.get("sse_stop")
        if stop is not None:
            stop.set()
        task = handle.get("sse_task")
        handle["sse_stop"] = None
        handle["sse_task"] = None
        if task is not None:
            if not task.done():
                task.cancel()
            await asyncio.gather(task, return_exceptions=True)

    def _spawn(self, handle: dict[str, Any], coro: Any) -> "asyncio.Future[Any]":
        """Run a fire-and-forget coroutine that ``_teardown`` can cancel."""
        task = asyncio.ensure_future(coro)
        tasks: set = handle["background_tasks"]
        tasks.add(task)
        task.add_done_callback(tasks.discard)
        return task

    def _queue_event(
        self, handle: dict[str, Any], etype: Any, native_type: str, payload: Any,
    ) -> None:
        item = (etype, native_type, payload)
        sink = handle.get("event_sink")
        if isinstance(etype, AgentEventType):
            origin = handle.get("routing_turn_id")
            if origin and (origin != handle.get("conversation_turn_id")
                           or sink is None or handle.get("execution_ended")):
                callback = handle.get("background_handler")
                if callback is not None:
                    callback(origin, [item])
                    return
        if sink is not None:
            sink.put_nowait(item)
            return
        if isinstance(etype, str) and etype.startswith("__"):
            return
        handle.setdefault("deferred_events", []).append(item)

    def _emit_plan(self, handle: dict[str, Any], todos: Any) -> None:
        self._queue_event(
            handle, AgentEventType.PLAN_UPDATED, "opencode.todo.updated",
            dump_payload(_plan_from_todos(todos)))

    def _emit_warning(
        self, handle: dict[str, Any], code: str, message: str,
    ) -> None:
        self._queue_event(
            handle, AgentEventType.RUNTIME_WARNING, code,
            dump_payload(RuntimeWarningPayload(
                kind="notice", code=code, message=message)))

    def _agent_patch(
        self, handle: dict[str, Any], agent_id: str, update: dict[str, Any]
    ) -> None:
        nodes = handle.setdefault("agent_nodes", {})
        previous = nodes.get(agent_id)
        if previous is None:
            previous = {"agent_id": agent_id, "title": agent_id,
                        "status": "running"}
        node = {**previous, **{k: v for k, v in update.items() if v is not None}}
        if node == previous and agent_id in nodes:
            return
        nodes[agent_id] = node
        self._queue_event(handle, AgentEventType.AGENT_UPDATED,
                          "opencode.subagent.updated",
                          dump_payload(AgentUpdatedPayload(
                              agents=[AgentNodePayload(**node)],
                              patch=True)))

    def _permission_request(
        self, handle: dict[str, Any], props: dict[str, Any], *,
        request_id: str, agent_id: str,
    ) -> ApprovalRequestedPayload:
        """Structured approval request for one native ``permission.asked``."""
        permission = str(props.get("permission") or props.get("action") or "")
        raw_patterns = props.get("patterns") or props.get("resources") or []
        patterns = [str(p) for p in raw_patterns if str(p)] if isinstance(
            raw_patterns, list) else []
        metadata = props.get("metadata") if isinstance(
            props.get("metadata"), dict) else {}
        tool = props.get("tool") if isinstance(props.get("tool"), dict) else {}
        fields: dict[str, Any] = {}
        if permission == "bash":
            kind = "command_execution"
            command = str(metadata.get("command") or "").strip() or (
                " && ".join(patterns))
            if command:
                fields["command"] = command
        elif permission in _OPENCODE_EDIT_PERMISSIONS:
            kind = "file_change"
            paths = [str(metadata.get("filepath") or "")] if metadata.get(
                "filepath") else patterns
            fields["paths"] = [p for p in paths if p]
            if isinstance(metadata.get("diff"), str) and metadata["diff"]:
                fields["unified_diff"] = metadata["diff"]
        else:
            kind = "tool"
            fields["input"] = {"patterns": patterns}
        return ApprovalRequestedPayload(
            approval_id=request_id,
            agent_id=agent_id or None,
            approval_kind=kind,
            title=permission or "OpenCode operation",
            tool_name=permission or None,
            tool_call_id=str(tool.get("callID") or "") or None,
            cwd=str(handle.get("cwd") or "") or None,
            native={
                "permission": permission,
                "patterns": patterns,
                "always": props.get("always"),
                "metadata": metadata,
                "access_mode": str(handle.get("access_mode") or ""),
                **({"background": True} if props.get("background") else {}),
            },
            **fields,
        )

    def _handle_permission(
        self, handle: dict[str, Any], props: dict[str, Any]
    ) -> None:
        """Route an OpenCode native permission request to Conversation."""
        request_id = str(props.get("id") or props.get("requestID") or "")
        if not request_id:
            return
        sink = handle.get("event_sink")
        access_mode = str(handle.get("access_mode") or "")
        permission = str(
            props.get("permission") or props.get("action") or ""
        )
        session_id = str(props.get("sessionID") or "")
        agent_id = (handle.get("child_sessions") or {}).get(session_id) or ""
        if access_mode == AccessMode.FULL_ACCESS.value:
            self._spawn(handle, self._auto_reply_permission(
                handle, request_id, "once", session_id=session_id))
            return
        if (access_mode == AccessMode.AUTO_ACCEPT_EDITS.value
                and permission in _OPENCODE_EDIT_PERMISSIONS):
            self._spawn(handle, self._auto_reply_permission(
                handle, request_id, "once", session_id=session_id))
            return
        payload = self._permission_request(
            handle, props, request_id=request_id, agent_id=agent_id)
        request = dump_payload(payload)
        cwd = str(handle.get("cwd") or "")
        grants: SessionApprovalGrants = handle["approval_grants"]
        if grants.covers(request, cwd=cwd):
            # OpenCode's own "always" covers a wider pattern than the exact
            # target the operator approved, so repeats are answered once here.
            self._spawn(handle, self._auto_reply_permission(
                handle, request_id, "once", session_id=session_id))
            return
        if sink is None and not (props.get("background") and handle.get("background_handler")):
            self._emit_warning(
                handle, "opencode.permission_outside_turn",
                f"OpenCode asked for {permission or 'a permission'} while no "
                "turn was running; the request stays pending in OpenCode")
            return
        remembered = ApprovalTarget.from_payload(request, cwd=cwd) is not None
        payload = payload.model_copy(update={
            "scopes": ["once", "session"] if remembered else ["once"]})
        handle["pending_approvals"][request_id] = dict(props)
        handle.setdefault("approval_requests", {})[request_id] = dump_payload(
            payload)
        self._queue_event(handle, AgentEventType.APPROVAL_REQUESTED,
                          "opencode.permission.asked", dump_payload(payload))

    async def _auto_reply_permission(
        self, handle: dict[str, Any], request_id: str, reply: str,
        *, session_id: str,
    ) -> None:
        try:
            await self._reply_permission(
                handle, request_id, reply, session_id=session_id)
        except OpenCodeError as exc:
            self._emit_warning(
                handle, "opencode.permission_reply_failed",
                f"OpenCode permission auto-reply failed: {exc}")

    async def _reply_permission(
        self, handle: dict[str, Any], request_id: str, reply: str,
        *, session_id: str = "", message: str = "",
    ) -> None:
        await self._v2.reply_permission(handle, request_id, reply, session_id, message)

    # -- turn 流 --------------------------------------------------------------

    def bind_continuations(self, session, turn_id, updates, offer) -> None:
        handle = self._sessions.get(session.agent_session_id)
        if handle is not None:
            handle.update(conversation_turn_id=turn_id, background_handler=updates,
                          continuation_handler=offer)

    def pending_continuation(self, session) -> tuple[str, str] | None:
        handle = self._sessions.get(session.agent_session_id)
        if handle is not None:
            for wake in handle["wakes"]:
                if not wake["dropped"]:
                    return wake["id"], wake["detail"]
        return None

    def run_continuation(self, session, wake_id):
        return self._v2._turn_stream(session, MessageInput(text=""), wake_id=wake_id)

    def background_turn_id(self, session) -> str | None:
        handle = self._sessions.get(session.agent_session_id)
        if handle is None:
            return None
        # T3 hasBackground / owesWork, including nested subagent follow-ups.
        if (handle["wakes"] or any(handle["reports"].values())
                or handle["busy"].intersection(handle["child_sessions"])
                or any(c["background"] and c["status"] == "running" for c in handle["calls"].values())):
            return handle.get("conversation_turn_id")
        return None

    def send(
        self, session: AgentSessionRef, input: AgentInput
    ) -> AsyncIterator[AgentEvent]:
        return self._v2.send(session, input)

    async def _approval_response_stream(
        self, session: AgentSessionRef, input: ApprovalResponseInput
    ) -> AsyncIterator[AgentEvent]:
        sid = session.agent_session_id
        seq = self.sequencer_for(sid)
        handle = self._sessions.get(sid)
        try:
            decision = ApprovalDecision.from_payload(input.payload.model_dump())
        except ValueError as exc:
            yield self.emit(build_event(
                AgentEventType.RUNTIME_ERROR, seq,
                agent_session_id=sid,
                native_type="opencode.permission.invalid",
                payload=RuntimeErrorPayload(error=self.exception_failure(
                    exc, FailureCategory.VALIDATION, "approval.invalid",
                    message=f"Invalid approval response: {exc}")),
            ))
            return
        pending = (
            handle.get("pending_approvals", {}).get(decision.approval_id)
            if handle else None
        )
        if pending is None:
            yield self.emit(build_event(
                AgentEventType.RUNTIME_ERROR, seq,
                agent_session_id=sid,
                native_type="opencode.permission.stale",
                payload=RuntimeErrorPayload(error=self.failure(
                    FailureCategory.UNKNOWN, "approval.stale",
                    message="OpenCode approval is no longer pending")),
            ))
            return
        request = (handle.get("approval_requests") or {}).get(
            decision.approval_id) or {}
        cwd = str(handle.get("cwd") or "")
        # OpenCode's "always" approves a pattern wider than the shown target,
        # so a session grant is answered once and kept in the local ledger.
        sent = native_decision(
            request, decision, native_scope_exact=False, cwd=cwd)
        reply = "reject" if not sent.allowed else "once"
        handle.setdefault("answering_requests", set()).add(decision.approval_id)
        try:
            await self._reply_permission(
                handle, decision.approval_id, reply,
                session_id=str(pending.get("sessionID") or ""),
                message=decision.note if not decision.allowed else "")
        except OpenCodeError as exc:
            handle["answering_requests"].discard(decision.approval_id)
            yield self.emit(build_event(
                AgentEventType.RUNTIME_ERROR, seq,
                agent_session_id=sid,
                native_type="opencode.permission.reply_failed",
                payload=RuntimeErrorPayload(error=self.exception_failure(
                    exc, FailureCategory.TRANSPORT, "approval.reply_failed",
                    message=f"OpenCode approval reply failed: {exc}")),
            ))
            return
        handle["pending_approvals"].pop(decision.approval_id, None)
        handle["answering_requests"].discard(decision.approval_id)
        (handle.get("approval_requests") or {}).pop(decision.approval_id, None)
        handle["approval_grants"].remember(request, decision, cwd=cwd)
        if not decision.allowed:
            if pending.get("background"):
                handle.setdefault("rejected_sessions", set()).add(pending.get("sessionID"))
            else:
                handle["turn_rejected"] = True
        yield self.emit(build_event(
            AgentEventType.APPROVAL_RESOLVED, seq,
            agent_session_id=sid,
            external_session_id=handle.get("external_session_id"),
            turn_id=handle.get("current_turn_id"),
            native_type="opencode.permission.replied",
            payload=ApprovalResolvedPayload(
                approval_id=decision.approval_id,
                decision="allow" if decision.allowed else "deny",
                scope=sent.scope.value,
                native={"reply": reply}),
        ))

    async def runtime_operation(self, session: AgentSessionRef, name: str, arguments: str = "") -> dict[str, Any]:
        return await self._v2.operation(session, name, arguments)

    async def runtime_capability_snapshot(
        self, session: Optional[AgentSessionRef] = None
    ) -> RuntimeCapabilitySnapshot:
        return await self._v2.snapshot(session)

    def resume(self, session: AgentSessionRef) -> AsyncIterator[AgentEvent]:
        if session.agent_session_id not in self._sessions:
            return self._unsupported_stream(session, "resume", "resume")
        return self._v2.resume(session)

    # -- 控制面 ---------------------------------------------------------------

    async def interrupt(self, session: AgentSessionRef) -> CommandReceipt:
        if session.agent_session_id not in self._sessions:
            return self.unsupported_receipt("interrupt", "no_active_session", session=session)
        return await self._v2.interrupt(session)

    async def steer(
        self, session: AgentSessionRef, input: AgentInput
    ) -> CommandReceipt:
        if session.agent_session_id not in self._sessions:
            return self.unsupported_receipt("steer", "active_turn", session=session)
        return await self._v2.steer(session, input)

    async def _teardown(self, session: AgentSessionRef) -> str:
        if session.agent_session_id not in self._sessions:
            return "closed"
        return await self._v2.teardown(session)


__all__ = [
    "OpenCodeError",
    "OpenCodeHttpClient",
    "OpenCodeServerAdapter",
]
