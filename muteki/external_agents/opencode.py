"""OpenCode Server Adapter（RUNTIME-04，任务书 7.2/7.6）。

正式接入：``opencode serve`` + v1 HTTP API + SSE（``@opencode-ai/sdk``
对应的同一份 OpenAPI 面）。核验结论
（docs/research/third_party_verification.md §OPENCODE，仓库已更名
``anomalyco/opencode``，稳定线 v1.18.x，本机实测 1.18.18）：

- ``opencode serve [--port 4096] [--hostname 127.0.0.1]`` 暴露 OpenAPI 3.1
  于 ``GET /doc``；``OPENCODE_SERVER_PASSWORD`` / ``OPENCODE_SERVER_USERNAME``
  启用 HTTP Basic Auth；
- Session：``POST /session`` 创建；**resume 语义 = 重新采用既有 session
  id**（先 ``GET /session/:sessionID`` 探测存在性，再继续 prompt；
  session 持久化于 opencode 全局存储，同 cwd 的新 server 可跨进程恢复，
  2026-08-21 本机实测；cwd 变化时官方路径是
  ``POST /session/:sessionID/fork``，本 Adapter 不自动 fork）；
- Prompt：``POST /session/:sessionID/message``（同步等待，返回
  ``{info, parts}``）——turn 完成以该响应为准；事件流经
  ``GET /event`` SSE（首事件 ``server.connected``，随后 bus 事件：
  ``session.status``、``message.part.updated``、``permission.asked``、
  ``question.asked`` 等）；
- Interrupt：``POST /session/:sessionID/abort``；
- Permission：``GET /permission`` + ``POST /permission/:requestID/reply``
  （body ``{reply, message?}``，``reply`` 为 ``PermissionV1.Reply``
  枚举 once/always/reject；旧的
  ``POST /session/:id/permissions/:permissionID`` 已 deprecated，仅在
  新端点 404 时兜底）；
- Question：``GET /question`` + ``POST /question/:requestID/reply``
  （body ``{answers: Answer[][]}``）/ ``POST /question/:requestID/reject``；
- MCP 注入：``POST /mcp``，body ``{name, config}``，remote 形态
  ``{type:"remote", url, headers, oauth:false}``（T3 生产用法；
  注意 T3 侧限制——仅对自己拉起的 server 注入；attach 外部 server 时
  注入是否生效属「仍需真实环境确认」项，失败只记 RUNTIME_WARNING 与
  probe degradations，不静默忽略）；
- SSE ``message.part.updated`` 中 tool part 的 ``state.output`` 是真实
  命令输出的结构化来源（gate witness 输入），``assistant`` message 的
  ``tokens`` 提供 usage。

能力注入按 probe 选择：server 可用即声明 ``mcp``（``POST /mcp`` 动态
注册）→ MCP 计划（``mcpServers`` dict 形态，由本 Adapter 物化为
``POST /mcp`` 调用）；probe 失败时 ``select_injection_kind`` 自然落到
CLI_SKILL 兜底。

兼容路径：``opencode run --format json`` 一次性 CLI 由
``muteki.solver.cli_driver.OpenCodeDriver`` + ``CliDriverAdapter``
（``cli.opencode``）承担；本模块不 import solver 层。
"""

from __future__ import annotations

import asyncio
import json
import os
from typing import Any, AsyncIterator, Callable, Optional

import httpx

from muteki.capability_bindings.acp_config import TOKEN_PLACEHOLDER
from muteki.platform.contracts.base import new_id
from muteki.platform.contracts.capabilities import (
    CapabilityInjectionPlan,
    InjectionKind,
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
    conservative_capabilities,
    _probe_version,
)
from .events import build_event
from .runtime_capabilities import (
    RuntimeCapabilityItem,
    RuntimeCapabilitySnapshot,
    dynamic_command_item,
)
from .sessions import classify_exit

class OpenCodeError(RuntimeError):
    """OpenCode HTTP 调用失败（非 2xx 或连接错误）。"""


class OpenCodeHttpClient:
    """``opencode serve`` v1 HTTP API 的最小异步客户端（httpx）。

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
            raise OpenCodeError(f"{method} {path} 连接失败：{exc}") from exc
        if resp.status_code == 204:
            return None
        if resp.status_code >= 400:
            detail = resp.text[:200]
            raise OpenCodeError(
                f"{method} {path} -> {resp.status_code}: {detail}")
        if not resp.content:
            return None
        try:
            return resp.json()
        except json.JSONDecodeError as exc:
            raise OpenCodeError(
                f"{method} {path} 返回非 JSON：{resp.text[:120]}") from exc

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
    ) -> None:
        """订阅 SSE（``GET /event``）：每帧 ``event: <type>`` +
        ``data: <json>``；``stop`` 置位或连接断开即返回。

        非 JSON / 缺字段的事件帧静默跳过（bus 事件格式由 Runtime 演进，
        传输层不做语义判定）。
        """
        try:
            async with self._client.stream("GET", path) as resp:
                if resp.status_code >= 400:
                    return
                event_name = ""
                async for line in resp.aiter_lines():
                    if stop.is_set():
                        return
                    if line.startswith("event:"):
                        event_name = line[6:].strip()
                    elif line.startswith("data:"):
                        try:
                            data = json.loads(line[5:].strip())
                        except json.JSONDecodeError:
                            continue
                        if isinstance(data, dict):
                            on_event(event_name, data)
                        event_name = ""
        except (httpx.HTTPError, asyncio.CancelledError):
            return


def _pick_free_port() -> int:
    """选取空闲 loopback 端口（0 由内核分配）。"""
    import socket
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return int(sock.getsockname()[1])


def _part_text(part: dict[str, Any]) -> str:
    return str(part.get("text") or "")


def _message_tokens(info: dict[str, Any]) -> dict[str, Any]:
    tokens = info.get("tokens")
    return dict(tokens) if isinstance(tokens, dict) else {}


def _response_error(value: Any) -> str:
    """Return a concise model/runtime error from an OpenCode response value."""
    if not value:
        return ""
    if isinstance(value, str):
        return value.strip()
    if not isinstance(value, dict):
        return str(value).strip()
    data = value.get("data") if isinstance(value.get("data"), dict) else {}
    message = str(
        value.get("message")
        or data.get("message")
        or data.get("error")
        or value.get("name")
        or ""
    ).strip()
    return message or json.dumps(value, ensure_ascii=False, default=str)[:300]


def _permission_rules(access_mode: str) -> list[dict[str, str]]:
    """Build OpenCode's native per-session PermissionRuleset."""
    if access_mode == AccessMode.FULL_ACCESS.value:
        return [{"permission": "*", "pattern": "*", "action": "allow"}]
    edit_action = (
        "allow" if access_mode == AccessMode.AUTO_ACCEPT_EDITS.value else "ask"
    )
    return [
        {"permission": "*", "pattern": "*", "action": "ask"},
        {"permission": "bash", "pattern": "*", "action": "ask"},
        {"permission": "edit", "pattern": "*", "action": edit_action},
        {"permission": "webfetch", "pattern": "*", "action": "ask"},
        {"permission": "websearch", "pattern": "*", "action": "ask"},
        {"permission": "codesearch", "pattern": "*", "action": "ask"},
        {"permission": "external_directory", "pattern": "*", "action": "ask"},
        {"permission": "doom_loop", "pattern": "*", "action": "ask"},
        {"permission": "question", "pattern": "*", "action": "allow"},
    ]


def _opencode_commands(value: Any) -> list[dict[str, Any]]:
    """Normalize the stable ``GET /command`` response."""
    if isinstance(value, dict):
        value = value.get("data") or value.get("commands") or []
    if not isinstance(value, list):
        return []
    commands: list[dict[str, Any]] = []
    for raw in value:
        if isinstance(raw, str):
            name = raw.strip().lstrip("/")
            item: dict[str, Any] = {"name": name}
        elif isinstance(raw, dict):
            name = str(
                raw.get("name") or raw.get("command") or raw.get("id") or ""
            ).strip().lstrip("/")
            item = dict(raw)
            item["name"] = name
        else:
            continue
        if name:
            commands.append(item)
    return commands


def _opencode_mcp_statuses(value: Any) -> list[dict[str, Any]]:
    """Normalize the stable ``GET /mcp`` response into named states."""
    if isinstance(value, dict) and isinstance(value.get("data"), dict):
        value = value["data"]
    if not isinstance(value, dict):
        return []
    return [
        {"name": str(name), **(dict(state) if isinstance(state, dict)
                                else {"status": str(state)})}
        for name, state in value.items()
    ]


class OpenCodeServerAdapter(BaseExternalAgentAdapter):
    """OpenCode 的 Server（HTTP + SSE）结构化 Adapter。

    覆盖任务书 RUNTIME-04 要求的 session、prompt、permission、question、
    event stream、resume、cwd 与 MCP 注入：

    - ``send``：``POST /session/:id/message`` 同步 prompt 为完成信号，
      SSE 事件流并行归一化为统一事件（delta/tool/permission/question/
      usage）；
    - ``interrupt``：``POST /session/:id/abort``；被中断的同步 prompt 以
      error/中断分类收尾，不伪造 TURN_COMPLETED；
    - ``resume``：重新采用既有 session id（先 GET 探测）；session 持久化
      于 opencode 全局存储，同 cwd 的新 server 可跨进程恢复（2026-08-21
      本机实测）；跨 cwd 恢复需 fork，本 Adapter 不自动 fork；
    - cwd：Adapter 自管 server 以 ``cwd`` 启动（session 继承 server
      工作目录）；attach 模式 cwd 由外部 server 决定并记入 degradations；
    - 审批/提问：沿用 OpenCode 会话的原生 permission rules；需要用户决定
      的请求接到 Conversation 审批卡。
    """

    adapter_id = "opencode.server"

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
        **kwargs: Any,
    ) -> None:
        super().__init__(self.adapter_id, **kwargs)
        self._binary = binary or os.environ.get("MUTEKI_OPENCODE_BIN",
                                                "opencode")
        self._attach_url = base_url
        self._server_username = server_username or os.environ.get(
            "OPENCODE_SERVER_USERNAME", "")
        self._server_password = server_password or os.environ.get(
            "OPENCODE_SERVER_PASSWORD", "")
        self._manage_server = manage_server
        self._startup_timeout = float(startup_timeout)
        self._prompt_timeout = float(prompt_timeout)
        self._extra_env = dict(extra_env or {})
        # agent_session_id -> 会话句柄
        self._sessions: dict[str, dict[str, Any]] = {}

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

    def _serve_argv(self, port: int) -> list[str]:
        """自管 server 进程命令行（子类/测试可覆盖指向 mock server）。"""
        return [self._binary, "serve", "--hostname", "127.0.0.1",
                "--port", str(port), "--pure"]

    async def _start_server(
        self,
        cwd: str,
        *,
        port: Optional[int] = None,
        env: Optional[dict[str, str]] = None,
    ) -> "tuple[Optional[asyncio.subprocess.Process], str]":
        """拉起自管 server 并等待 ``GET /doc`` 可用；返回 (进程, base_url)。

        attach 模式（``base_url`` 构造参数非空）不拉起进程，直接返回
        既有地址。
        """
        if self._attach_url:
            return None, self._attach_url
        if not self._manage_server:
            raise OpenCodeError("manage_server=False 时必须提供 base_url")
        port = port or _pick_free_port()
        base_url = f"http://127.0.0.1:{port}"
        process_cwd = os.path.abspath(cwd or os.getcwd())
        from .probe_environment import subprocess_environment
        argv = self._serve_argv(port)
        if (env or {}).get("MUTEKI_CHAT_PRIVATE_ROOT"):
            argv = [value for value in argv if value != "--pure"]
        proc = await asyncio.create_subprocess_exec(
            *argv,
            cwd=process_cwd,
            env=subprocess_environment({**self._server_env(env), "PWD": process_cwd}),
            stdout=asyncio.subprocess.DEVNULL,
            stderr=asyncio.subprocess.DEVNULL,
        )
        client = OpenCodeHttpClient(
            base_url, username=self._server_username,
            password=self._server_password)
        try:
            deadline = asyncio.get_running_loop().time() + self._startup_timeout
            while True:
                if proc.returncode is not None:
                    raise OpenCodeError(
                        f"opencode serve 提前退出（code={proc.returncode}）")
                try:
                    await client.get("/doc", timeout=5.0)
                    break
                except OpenCodeError:
                    if asyncio.get_running_loop().time() > deadline:
                        raise OpenCodeError("opencode serve 启动超时（/doc 不可达）")
                    await asyncio.sleep(0.2)
        finally:
            await client.close()
        return proc, base_url

    @staticmethod
    async def _stop_server(proc: Optional[asyncio.subprocess.Process]) -> int:
        if proc is None:
            return -1
        if proc.returncode is None:
            try:
                proc.terminate()
                await asyncio.wait_for(proc.wait(), timeout=5.0)
            except (asyncio.TimeoutError, ProcessLookupError):
                try:
                    proc.kill()
                except ProcessLookupError:
                    pass
                await proc.wait()
        return int(proc.returncode if proc.returncode is not None else -1)

    # -- probe ----------------------------------------------------------------

    async def probe(self, request: ProbeRequest) -> AgentCapabilities:
        field_sources: dict[str, str] = {}
        degradations: list[str] = []
        caps = conservative_capabilities(
            transport_kind="http", capability_source=SOURCE_STATIC)
        version = _probe_version(self._binary)
        caps.runtime_version = version
        detail = ""
        doc: dict[str, Any] = {}
        if version or self._attach_url:
            proc: Optional[asyncio.subprocess.Process] = None
            try:
                proc, base_url = await self._start_server(os.getcwd())
                client = OpenCodeHttpClient(
                    base_url, username=self._server_username,
                    password=self._server_password)
                try:
                    doc = await client.get("/doc",
                                           timeout=self._startup_timeout)
                finally:
                    await client.close()
            except (OSError, OpenCodeError, asyncio.TimeoutError) as exc:
                detail = f"opencode serve 探测失败：{str(exc)[:160]}"
                doc = {}
            finally:
                if proc is not None:
                    await self._stop_server(proc)
        if not isinstance(doc, dict):
            doc = {}
        paths = doc.get("paths") if isinstance(doc.get("paths"), dict) else {}

        probed = bool(paths)
        if not probed:
            for field_name in BOOL_CAPABILITY_FIELDS:
                field_sources[field_name] = SOURCE_STATIC
            degradations.append(
                "structured transport 不可用：HTTP/SSE session/prompt/"
                "permission/question/mcp 注入均未实测，按保守默认 False"
                "（不静默降级）")
            self._probe_cache = CapabilityProbeReport(
                adapter_id=self.identity.adapter_id,
                instance_id=self.identity.instance_id,
                capabilities=caps, binary_path=self._binary,
                field_sources=field_sources, degradations=degradations,
                detail=detail or "binary 不可用或 /doc 不可达",
            )
            return caps

        # 能力以 OpenAPI paths 实测为准（不按版本号推断）。
        def has(path: str) -> bool:
            return any(p == path or p.startswith(path + "/")
                       or p.startswith(path + "{") for p in paths)

        caps.capability_source = SOURCE_PROBE
        caps.protocol_version = str(
            (doc.get("openapi") or doc.get("swagger") or ""))[:40]
        caps.streaming = has("/event")
        caps.tool_events = has("/event")
        caps.usage_events = True  # assistant message.tokens（SSE/message 响应）
        caps.interrupt = has("/session")
        caps.approval = has("/permission")
        caps.access_modes = list(ACCESS_MODE_VALUES)
        caps.user_input = has("/question")
        caps.resume = True  # session id 重新采用（需同一 server 存活）
        caps.session_persistence = True
        # POST /mcp 动态注册（§OPENCODE-3）→ MCP 注入档。
        caps.mcp = has("/mcp")
        # prompt_async 只承诺异步接收一条普通消息，官方契约没有说明它会
        # 修改当前模型回合。Muteki 的后续消息队列负责这类语义，因此这里
        # 不把 prompt_async 冒充成同回合 steer。
        caps.steer = False
        for field_name in BOOL_CAPABILITY_FIELDS:
            field_sources[field_name] = SOURCE_PROBE
        if not caps.mcp:
            degradations.append(
                "无 POST /mcp 端点：MCP 注入不可用，能力注入降级（由 "
                "select_injection_kind 选择后续档位）")
        if self._attach_url:
            degradations.append(
                "attach 外部 server 模式：cwd 由外部 server 决定；"
                "POST /mcp 动态注入对外部 server 是否生效属待确认项"
                "（T3 侧限制），失败只记 warning 不静默忽略")
        degradations.append(
            "resume 语义为重新采用 session id：session 持久化于 opencode "
            "全局存储（同 cwd 新 server 可跨进程恢复，2026-08-21 本机实测）；"
            "跨 cwd/跨项目恢复需 fork，本 Adapter 不自动 fork")
        self._probe_cache = CapabilityProbeReport(
            adapter_id=self.identity.adapter_id,
            instance_id=self.identity.instance_id,
            capabilities=caps, binary_path=self._binary,
            field_sources=field_sources, degradations=degradations,
            detail=f"opencode serve OpenAPI 探测成功（{len(paths)} 条路径）",
        )
        return caps

    # -- 注入物化 --------------------------------------------------------------

    async def _inject_mcp(
        self,
        client: OpenCodeHttpClient,
        plan: Optional[CapabilityInjectionPlan],
        bearer_token: Optional[str],
    ) -> "tuple[int, list[str]]":
        """把注入计划的 mcpServers 物化为 ``POST /mcp`` 动态注册。

        返回 ``(注入数量, 警告清单)``；token 缺失时占位符 server 跳过
        （不注入无效凭据），单条注册失败只记 warning（§OPENCODE 影响：
        对外部 server 的适用性待确认）。
        """
        if plan is None or plan.injection_kind is not InjectionKind.MCP:
            return 0, []
        cfg = (plan.runtime_config or {}).get("mcpServers")
        if not isinstance(cfg, dict) or not cfg:
            return 0, []
        injected = 0
        warnings: list[str] = []
        for name, entry in cfg.items():
            entry = dict(entry or {})
            headers: dict[str, str] = {}
            usable = True
            raw_headers = entry.get("headers")
            items = (raw_headers.items() if isinstance(raw_headers, dict)
                     else ((h.get("name"), h.get("value"))
                           for h in (raw_headers or [])))
            for key, value in items:
                value = str(value or "")
                if TOKEN_PLACEHOLDER in value:
                    if not bearer_token:
                        usable = False
                        break
                    value = value.replace(TOKEN_PLACEHOLDER, bearer_token)
                headers[str(key)] = value
            if not usable:
                warnings.append(f"mcp server {name!r} 缺 bearer token，跳过注入")
                continue
            try:
                await client.post("/mcp", {
                    "name": str(name),
                    "config": {
                        "type": "remote",
                        "url": str(entry.get("url")
                                   or plan.gateway_endpoint),
                        "headers": headers,
                        "oauth": False,
                    },
                })
                injected += 1
            except OpenCodeError as exc:
                warnings.append(f"POST /mcp {name!r} 失败：{str(exc)[:120]}")
        return injected, warnings

    # -- 启动 / 接管 -------------------------------------------------------------

    async def _launch(
        self,
        request: SessionStart,
        plan: Optional[CapabilityInjectionPlan],
        bearer_token: Optional[str],
    ) -> dict[str, Any]:
        cwd = str(request.options.get("cwd") or os.getcwd())
        session_env = {
            str(key): str(value)
            for key, value in dict(request.options.get("env") or {}).items()
        }
        proc: Optional[asyncio.subprocess.Process] = None
        client: Optional[OpenCodeHttpClient] = None
        try:
            proc, base_url = await self._start_server(cwd, env=session_env)
            client = OpenCodeHttpClient(
                base_url, username=self._server_username,
                password=self._server_password,
                timeout=self._prompt_timeout + 30.0)

            resume_handle = request.resume_handle
            warnings: list[str] = []
            access_mode = str(
                request.access_mode or AccessMode.SUPERVISED.value
            ).strip()
            if access_mode not in ACCESS_MODE_VALUES:
                raise ValueError(
                    f"opencode unsupported access mode: {access_mode}")
            permissions = _permission_rules(access_mode)
            if resume_handle:
                # resume = 重新采用既有 session id（先探测存在性）。
                try:
                    await client.get(f"/session/{resume_handle}")
                except OpenCodeError as exc:
                    raise OpenCodeError(
                        f"resume 失败：session {resume_handle} 在此 server "
                        f"不可达（{str(exc)[:120]}）") from exc
                session_id = resume_handle
                await client.patch(
                    f"/session/{session_id}", {"permission": permissions})
            else:
                body: dict[str, Any] = {"permission": permissions}
                title = request.options.get("title")
                if title:
                    body["title"] = str(title)
                created = await client.post("/session", body)
                session_id = str((created or {}).get("id") or "")
                if not session_id:
                    raise OpenCodeError("POST /session 未返回 session id")

            injected, inject_warnings = await self._inject_mcp(
                client, plan, bearer_token)
            warnings.extend(inject_warnings)
            try:
                available_commands = _opencode_commands(
                    await client.get("/command"))
            except OpenCodeError as exc:
                available_commands = []
                warnings.append(
                    f"GET /command 失败，Runtime 命令目录不可用：{str(exc)[:120]}"
                )
            try:
                mcp_statuses = _opencode_mcp_statuses(
                    await client.get("/mcp"))
            except OpenCodeError as exc:
                mcp_statuses = []
                warnings.append(
                    f"GET /mcp 失败，Runtime MCP 状态不可用：{str(exc)[:120]}"
                )
        except Exception:
            if client is not None:
                await client.close()
            if proc is not None:
                await self._stop_server(proc)
            raise

        handle: dict[str, Any] = {
            "conversation_thread_id": request.thread_id,
            "proc": proc,
            "client": client,
            "base_url": base_url,
            "cwd": cwd,
            "options": dict(request.options),
            "turns": 0,
            "external_session_id": session_id,
            "model": request.model,
            "variant": request.effort if request.effort != "default" else None,
            "resumed": bool(resume_handle),
            "event_sink": None,
            "current_turn_id": None,
            "sse_stop": None,
            "sse_task": None,
            "mcp_injected": injected,
            "warnings": warnings,
            "access_mode": access_mode,
            "pending_approvals": {},
            "pending_questions": {},
            "available_commands": available_commands,
            "mcp_statuses": mcp_statuses,
            "capability_revision": 1,
        }
        self._sessions[request.agent_session_id] = handle
        # SSE 事件流随会话启动订阅（permission/question 可能在 prompt 外
        # 到达，但本实现只在 turn 窗口内消费；窗口外事件仅计数）。
        stop = asyncio.Event()
        handle["sse_stop"] = stop
        handle["sse_task"] = asyncio.ensure_future(client.stream_sse(
            "/event",
            lambda name, data: self._dispatch_sse(
                request.agent_session_id, name, data),
            stop))
        return {"external_session_id": session_id,
                "resume_handle": session_id}

    # -- SSE 事件归一化 ------------------------------------------------------------

    def _dispatch_sse(
        self, agent_session_id: str, event_name: str, data: dict[str, Any]
    ) -> None:
        """bus 事件 → turn 队列（在 SSE task 上运行，只 put_nowait）。"""
        handle = self._sessions.get(agent_session_id)
        if handle is None:
            return
        props = data.get("properties") if isinstance(
            data.get("properties"), dict) else data
        etype = str(data.get("type") or event_name or "")

        event_session_id = str(props.get("sessionID") or "")
        if (event_session_id and event_session_id
                != str(handle.get("external_session_id") or "")):
            return
        if etype in {"permission.asked", "permission.v2.asked"}:
            self._handle_permission(handle, props)
            return
        if etype == "question.asked":
            self._handle_question(handle, props)
            return

        sink = handle.get("event_sink")
        if sink is None:
            handle["sse_outside_turn"] = handle.get("sse_outside_turn", 0) + 1
            return

        if etype == "message.part.updated":
            part = props.get("part") if isinstance(
                props.get("part"), dict) else {}
            kind = str(part.get("type") or "")
            message = props.get("message") if isinstance(
                props.get("message"), dict) else {}
            role = str(part.get("role") or message.get("role")
                       or props.get("role") or "").lower()
            if kind == "text" and role == "assistant" and _part_text(part):
                sink.put_nowait((AgentEventType.MESSAGE_DELTA,
                                 "opencode.message.part.updated",
                                 {"text": _part_text(part),
                                  "part_id": part.get("id"),
                                  "message_id": part.get("messageID")}))
            elif kind == "tool":
                state = part.get("state") if isinstance(
                    part.get("state"), dict) else {}
                status = str(state.get("status") or "")
                payload = {
                    "call_id": str(part.get("callID") or part.get("id") or ""),
                    "tool": str(part.get("tool") or ""),
                    "status": status or None,
                }
                if status == "running":
                    payload["input"] = state.get("input")
                    sink.put_nowait((AgentEventType.TOOL_STARTED,
                                     "opencode.tool.running", payload))
                elif status in ("completed", "error"):
                    # tool part 的 state.output 是真实命令输出的结构化
                    # 来源（gate witness 输入，§OPENCODE 影响）。
                    payload["output"] = state.get("output")
                    if status == "error":
                        payload["is_error"] = True
                        payload["error"] = state.get("error")
                    sink.put_nowait((AgentEventType.TOOL_COMPLETED,
                                     "opencode.tool.completed", payload))
        elif etype == "message.updated":
            info = props.get("info") if isinstance(
                props.get("info"), dict) else {}
            tokens = _message_tokens(info)
            if tokens and str(info.get("role") or "") == "assistant":
                sink.put_nowait((AgentEventType.USAGE_UPDATED,
                                 "opencode.message.updated",
                                 {"usage": tokens}))
        # session.status / session.updated / message.part.delta 等其余
        # bus 事件不改变核心状态机，忽略。

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
        if access_mode == AccessMode.FULL_ACCESS.value:
            asyncio.ensure_future(self._reply_permission(
                handle, request_id, "always"))
            return
        if (access_mode == AccessMode.AUTO_ACCEPT_EDITS.value
                and permission == "edit"):
            asyncio.ensure_future(self._reply_permission(
                handle, request_id, "once"))
            return
        if sink is None:
            return
        handle["pending_approvals"][request_id] = dict(props)
        sink.put_nowait((AgentEventType.APPROVAL_REQUESTED,
                         "opencode.permission.asked", {
                             "approval_id": request_id,
                             "tool": permission,
                             "action": permission or "OpenCode operation",
                             "permission": permission,
                             "patterns": props.get("patterns")
                             or props.get("resources"),
                             "metadata": props.get("metadata"),
                             "access_mode": access_mode,
                         }))

    @staticmethod
    async def _reply_permission(
        handle: dict[str, Any], request_id: str, reply: str
    ) -> None:
        client: OpenCodeHttpClient = handle["client"]
        try:
            await client.post(
                f"/permission/{request_id}/reply", {"reply": reply})
            return
        except OpenCodeError:
            session_id = handle.get("external_session_id")
        await client.post(
            f"/session/{session_id}/permissions/{request_id}",
            {"response": reply},
        )

    def _handle_question(
        self, handle: dict[str, Any], props: dict[str, Any]
    ) -> None:
        """Expose OpenCode's native question request to Conversation."""
        request_id = str(props.get("id") or props.get("requestID") or "")
        if not request_id:
            return
        sink = handle.get("event_sink")
        questions = props.get("questions") or []
        if sink is not None:
            handle["pending_questions"][request_id] = dict(props)
            sink.put_nowait((AgentEventType.USER_INPUT_REQUESTED,
                             "opencode.question.asked", {
                                 "request_id": request_id,
                                 "questions": questions,
                             }))


    # -- turn 流 --------------------------------------------------------------

    def send(
        self, session: AgentSessionRef, input: AgentInput
    ) -> AsyncIterator[AgentEvent]:
        if input.kind == "approval_response":
            return self._approval_response_stream(session, input)
        if input.kind == "user_input_response":
            return self._user_input_response_stream(session, input)
        return self._turn_stream(session, input)

    async def _approval_response_stream(
        self, session: AgentSessionRef, input: AgentInput
    ) -> AsyncIterator[AgentEvent]:
        sid = session.agent_session_id
        seq = self.sequencer_for(sid)
        handle = self._sessions.get(sid)
        try:
            decision = ApprovalDecision.from_payload(input.payload)
        except ValueError as exc:
            yield self.emit(build_event(
                AgentEventType.RUNTIME_ERROR, seq,
                agent_session_id=sid,
                native_type="opencode.permission.invalid",
                payload={"code": "opencode.approval.invalid",
                         "detail": str(exc)},
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
                payload={"code": "opencode.approval.stale",
                         "detail": "OpenCode approval is no longer pending"},
            ))
            return
        reply = (
            "reject" if not decision.allowed
            else "always" if decision.scope is ApprovalScope.SESSION
            else "once"
        )
        try:
            await self._reply_permission(handle, decision.approval_id, reply)
        except OpenCodeError as exc:
            yield self.emit(build_event(
                AgentEventType.RUNTIME_ERROR, seq,
                agent_session_id=sid,
                native_type="opencode.permission.reply_failed",
                payload={"code": "opencode.approval.reply_failed",
                         "detail": str(exc)[:200]},
            ))
            return
        handle["pending_approvals"].pop(decision.approval_id, None)
        yield self.emit(build_event(
            AgentEventType.APPROVAL_RESOLVED, seq,
            agent_session_id=sid,
            external_session_id=handle.get("external_session_id"),
            turn_id=handle.get("current_turn_id"),
            native_type="opencode.permission.replied",
            payload={"approval_id": decision.approval_id,
                     "decision": decision.choice.value,
                     "scope": decision.scope.value,
                     "reply": reply},
        ))

    async def _user_input_response_stream(
        self, session: AgentSessionRef, input: AgentInput
    ) -> AsyncIterator[AgentEvent]:
        sid = session.agent_session_id
        seq = self.sequencer_for(sid)
        handle = self._sessions.get(sid)
        request_id = str(input.payload.get("request_id") or "")
        pending = (
            handle.get("pending_questions", {}).get(request_id)
            if handle else None
        )
        if pending is None:
            yield self.emit(build_event(
                AgentEventType.RUNTIME_ERROR, seq,
                agent_session_id=sid,
                native_type="opencode.question.stale",
                payload={"code": "opencode.question.stale",
                         "detail": "OpenCode question is no longer pending"},
            ))
            return
        client: OpenCodeHttpClient = handle["client"]
        await client.post(
            f"/question/{request_id}/reply",
            {"answers": [[input.text]]},
        )
        handle["pending_questions"].pop(request_id, None)
        yield self.emit(build_event(
            AgentEventType.USER_INPUT_RESOLVED, seq,
            agent_session_id=sid,
            external_session_id=handle.get("external_session_id"),
            turn_id=handle.get("current_turn_id"),
            native_type="opencode.question.replied",
            payload={"request_id": request_id, "resolution": "answered"},
        ))

    async def _turn_stream(
        self, session: AgentSessionRef, input: AgentInput
    ) -> AsyncIterator[AgentEvent]:
        sid = session.agent_session_id
        seq = self.sequencer_for(sid)
        handle = self._sessions.get(sid)
        if handle is None or handle.get("client") is None:
            async for event in self._unsupported_stream(session, "send",
                                                        "session"):
                yield event
            return
        client: OpenCodeHttpClient = handle["client"]
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
                native_type="opencode.session.start",
                payload={"transport": "http+sse",
                         "adapter_id": self.id,
                         "instance_id": self.identity.instance_id,
                         "cwd": handle["cwd"],
                         "mcp_injected": handle.get("mcp_injected", 0),
                         "warnings": list(handle.get("warnings") or [])},
                **common))
            yield self.emit(build_event(
                AgentEventType.RUNTIME_CAPABILITIES_UPDATED, seq,
                external_session_id=external_id,
                native_type="opencode.runtime_capabilities",
                payload={
                    "adapter_id": self.id,
                    "revision": handle.get("capability_revision", 1),
                    "commands": list(handle.get("available_commands") or []),
                    "mcp_servers": list(handle.get("mcp_statuses") or []),
                },
                **common))
        turn_id = new_id("turn")
        handle["current_turn_id"] = turn_id
        yield self.emit(build_event(
            AgentEventType.TURN_STARTED, seq,
            external_session_id=external_id, turn_id=turn_id,
            native_type="opencode.prompt.start",
            payload={"kind": input.kind},
            **common))

        queue: asyncio.Queue = asyncio.Queue()
        handle["event_sink"] = queue

        async def run_prompt() -> Any:
            try:
                runtime_capability = input.payload.get("runtime_capability")
                runtime_capability = (
                    runtime_capability
                    if isinstance(runtime_capability, dict)
                    else {}
                )
                invocation = runtime_capability.get("invocation")
                invocation = invocation if isinstance(invocation, dict) else {}
                if invocation.get("transport") == "opencode.command":
                    command = str(invocation.get("command") or "").strip()
                    arguments = str(
                        input.payload.get("runtime_command_arguments") or ""
                    )
                    if not command:
                        raise OpenCodeError(
                            "Runtime 命令缺少经过服务端解析的 command")
                    return await client.post(
                        f"/session/{external_id}/command",
                        {"command": command, "arguments": arguments},
                        timeout=self.conversation_turn_timeout(
                            handle.get("conversation_thread_id"),
                            self._prompt_timeout),
                    )

                # 同步 message 端点：响应即 turn 完成信号（{info, parts}）。
                body: dict[str, Any] = {
                    "parts": [{"type": "text", "text": input.text}],
                }
                if handle.get("variant"):
                    body["variant"] = str(handle["variant"])
                model = handle.get("model")
                custom_provider = str(
                    (handle.get("options") or {}).get("env", {}).get(
                        "MUTEKI_OPENCODE_PROVIDER"
                    ) or ""
                ).strip()
                if model and custom_provider:
                    body["model"] = {
                        "providerID": custom_provider,
                        "modelID": str(model),
                    }
                elif model and "/" in str(model):
                    provider, model_id = str(model).split("/", 1)
                    body["model"] = {"providerID": provider,
                                     "modelID": model_id}
                return await client.post(
                    f"/session/{external_id}/message", body,
                    timeout=self.conversation_turn_timeout(
                        handle.get("conversation_thread_id"),
                        self._prompt_timeout))
            except Exception as exc:  # noqa: BLE001
                return {"__error__": exc}

        task = asyncio.ensure_future(run_prompt())
        get_task = asyncio.ensure_future(queue.get())
        result: Any = None
        usage_seen = False  # SSE 已给出 usage 时不再用同步响应重复上报
        try:
            while True:
                done, _pending = await asyncio.wait(
                    {task, get_task}, return_when=asyncio.FIRST_COMPLETED)
                if task in done:
                    result = task.result()
                    break
                if get_task in done:
                    etype, native_type, payload = get_task.result()
                    get_task = asyncio.ensure_future(queue.get())
                    if etype is AgentEventType.USAGE_UPDATED:
                        usage_seen = True
                    yield self.emit(build_event(
                        etype, seq,
                        external_session_id=external_id, turn_id=turn_id,
                        native_type=native_type, payload=payload,
                        **common))
        finally:
            get_task.cancel()
        handle["event_sink"] = None
        handle["turns"] += 1
        handle["current_turn_id"] = None

        if isinstance(result, dict) and isinstance(
                result.get("__error__"), Exception):
            error = result["__error__"]
            interrupted = handle.pop("abort_requested", False)
            yield self.emit(build_event(
                AgentEventType.TURN_FAILED, seq,
                external_session_id=external_id, turn_id=turn_id,
                native_type="opencode.prompt.error",
                payload={"error": str(error)[:300],
                         "reason": "interrupted" if interrupted else "failed"},
                **common))
            yield self.emit(build_event(
                AgentEventType.RUNTIME_EXITED, seq,
                external_session_id=external_id, turn_id=turn_id,
                native_type="opencode.exit",
                payload={"classification": classify_exit(
                    cancelled=interrupted, error=str(error)[:120])},
                **common))
            return

        # turn 完成：响应 {info, parts}；正文 parts 里 text 合并为
        # MESSAGE_COMPLETED（若 SSE 已流式给出 delta，这里给最终权威文本）。
        # usage 优先取 SSE 已上报值；仅当 SSE 未提供时才用同步响应补报，
        # 且补报排在 MESSAGE_COMPLETED 之前，保持 TURN_COMPLETED 收尾。
        info = result.get("info") if isinstance(result, dict) else {}
        parts = (result.get("parts") if isinstance(result, dict) else None) \
            or []
        response_error = _response_error(
            (result or {}).get("error") if isinstance(result, dict) else None
        ) or _response_error(
            (info or {}).get("error") if isinstance(info, dict) else None
        )
        finish = str((info or {}).get("finish") or "").strip().lower()
        if response_error or finish in {"error", "failed"}:
            detail = response_error or f"OpenCode finish={finish}"
            yield self.emit(build_event(
                AgentEventType.TURN_FAILED, seq,
                external_session_id=external_id, turn_id=turn_id,
                native_type="opencode.prompt.error_response",
                payload={"error": detail[:300], "reason": "failed"},
                **common))
            return
        tokens = _message_tokens(info or {})
        if tokens and not usage_seen:
            yield self.emit(build_event(
                AgentEventType.USAGE_UPDATED, seq,
                external_session_id=external_id, turn_id=turn_id,
                native_type="opencode.usage",
                payload={"usage": tokens},
                **common))
        text = "".join(
            _part_text(p) for p in parts
            if isinstance(p, dict) and p.get("type") == "text"
            and str(p.get("role") or "assistant").lower() == "assistant")
        if not text.strip():
            yield self.emit(build_event(
                AgentEventType.TURN_FAILED, seq,
                external_session_id=external_id, turn_id=turn_id,
                native_type="opencode.empty_assistant",
                payload={
                    "reason": "empty_assistant",
                    "error": {
                        "code": "opencode.empty_assistant",
                        "message": "OpenCode turn ended without assistant text",
                    },
                },
                **common))
            return
        yield self.emit(build_event(
            AgentEventType.MESSAGE_COMPLETED, seq,
            external_session_id=external_id, turn_id=turn_id,
            native_type="opencode.message.completed",
            payload={"text": text, "role": "assistant"},
            **common))
        yield self.emit(build_event(
            AgentEventType.TURN_COMPLETED, seq,
            external_session_id=external_id, turn_id=turn_id,
            native_type="opencode.prompt.completed",
            payload={"finish": (info or {}).get("finish") or "stop"},
            **common))

    async def runtime_operation(self, session: AgentSessionRef, name: str, arguments: str = "") -> dict[str, Any]:
        handle = self._sessions.get(session.agent_session_id)
        if not handle or handle.get("current_turn_id"):
            raise RuntimeError("请等待当前回复结束后再压缩")
        name = name.strip().lstrip("/")
        if arguments:
            raise ValueError(f"/{name} 不接受参数")
        paths = {"mcp": "/mcp", "agents": "/agent", "status": f"/session/{handle['external_session_id']}"}
        if name in paths:
            return {"status": "completed", "result": await handle["client"].get(paths[name])}
        if name not in {"compact", "summarize"}:
            raise RuntimeError("未知原生操作")
        model = str(handle.get("model") or "")
        provider = str((handle.get("options") or {}).get("env", {}).get("MUTEKI_OPENCODE_PROVIDER") or "")
        if not provider and "/" in model:
            provider, model = model.split("/", 1)
        if not provider or not model:
            raise RuntimeError("当前模型未提供 OpenCode 压缩所需的 provider/model")
        result = await handle["client"].post(
            f"/session/{handle['external_session_id']}/summarize",
            {"providerID": provider, "modelID": model}, timeout=180,
        )
        if result is not True:
            raise RuntimeError("OpenCode 未确认压缩完成")
        return {"status": "completed", "message": "上下文已由 OpenCode 原生压缩"}

    async def runtime_capability_snapshot(
        self, session: Optional[AgentSessionRef] = None
    ) -> RuntimeCapabilitySnapshot:
        base = await super().runtime_capability_snapshot(session)
        if session is None:
            base.stale = True
            base.diagnostics.append(
                "OpenCode 命令目录属于具体 server/session，当前没有活动会话"
            )
            return base
        handle = self._sessions.get(session.agent_session_id)
        if handle is None or handle.get("client") is None:
            base.stale = True
            base.diagnostics.append("OpenCode 活动会话已结束")
            return base

        client: OpenCodeHttpClient = handle["client"]
        try:
            commands = _opencode_commands(await client.get("/command"))
            handle["available_commands"] = commands
        except OpenCodeError as exc:
            commands = list(handle.get("available_commands") or [])
            base.stale = True
            base.diagnostics.append(
                f"刷新 OpenCode 命令目录失败：{str(exc)[:160]}"
            )
        try:
            mcp_statuses = _opencode_mcp_statuses(await client.get("/mcp"))
            handle["mcp_statuses"] = mcp_statuses
        except OpenCodeError as exc:
            mcp_statuses = list(handle.get("mcp_statuses") or [])
            base.diagnostics.append(
                f"刷新 OpenCode MCP 状态失败：{str(exc)[:160]}"
            )

        handle["capability_revision"] = int(
            handle.get("capability_revision") or 0
        ) + 1
        items: list[RuntimeCapabilityItem] = []
        model = str(handle.get("model") or "")
        provider = str((handle.get("options") or {}).get("env", {}).get("MUTEKI_OPENCODE_PROVIDER") or "")
        if model and (provider or "/" in model):
            items.append(RuntimeCapabilityItem(
                id=f"runtime:{self.id}:operation:compact", kind="operation", name="compact",
                description="由 OpenCode 原生压缩当前上下文", source=self.id, scope="session",
                engine="opencode", channel="app_server_rpc", resolution="client",
                origin="verified_static", delivery="guaranteed", verification="verified",
                action="invoke-runtime-operation", invocation={"method": "session.summarize"},
            ))
        from .command_providers import operation_item
        for name, method, description in (("mcp", "mcp.status", "查看 OpenCode MCP 连接状态"),
                                          ("agents", "agent.list", "查看 OpenCode 可用 Agent"),
                                          ("status", "session.get", "查看 OpenCode 会话状态")):
            items.append(operation_item(self.id, "opencode", name, method, description))
        compact = next((item for item in items if item.name == "compact"), None)
        if compact:
            items.append(compact.model_copy(update={"id": f"runtime:{self.id}:operation:summarize", "name": "summarize"}))
        operation_names = {item.name for item in items}
        for command in commands:
            name = str(command.get("name") or "").strip().lstrip("/")
            if not name or name in operation_names:
                continue
            description = str(
                command.get("description") or command.get("template") or ""
            ).strip()
            items.append(dynamic_command_item(
                adapter_id=self.id,
                engine="opencode",
                name=name,
                description=description,
                argument_hint=str(
                    command.get("arguments") or command.get("argumentHint") or ""
                ),
                channel="app_server_rpc",
                kind="skill" if str(command.get("source") or "").lower()
                == "skill" else "command",
                invocation={
                    "transport": "opencode.command",
                    "command": name,
                    "wire_text": f"/{name}",
                },
            ))
        runtime_mcp = {str(item.get("name")): item for item in mcp_statuses}
        for item in base.items:
            if item.kind == "mcp_status" and item.name in runtime_mcp:
                state = runtime_mcp[item.name]
                item.status = str(state.get("status") or "runtime_reported")
                item.origin = "dynamic"
                item.verification = "verified"
                item.delivery = "guaranteed"
        known_mcp = {item.name for item in base.items if item.kind == "mcp_status"}
        for name, state in runtime_mcp.items():
            if name in known_mcp:
                continue
            items.append(RuntimeCapabilityItem(
                id=f"runtime:{self.id}:mcp:{name}",
                kind="mcp_status",
                name=name,
                description=str(state.get("error") or ""),
                source=self.id,
                scope="session",
                engine="opencode",
                channel="session_config",
                origin="dynamic",
                delivery="guaranteed",
                verification="verified",
                action="inspect-runtime-status",
                status=str(state.get("status") or "runtime_reported"),
                invocation={"runtime": state},
            ))
        base.items = items + base.items
        base.revision = int(handle["capability_revision"])
        base.external_session_id = handle.get("external_session_id")
        return base

    def resume(self, session: AgentSessionRef) -> AsyncIterator[AgentEvent]:
        return self._resume_stream(session)

    async def _resume_stream(
        self, session: AgentSessionRef
    ) -> AsyncIterator[AgentEvent]:
        """恢复：session id 重新采用在 ``_launch`` 完成；这里透出状态。

        历史消息可经 ``GET /session/:id/message`` 由调用方另行检阅；
        本流不把历史当作新事件投影（与 ACP replay 语义一致）。
        """
        sid = session.agent_session_id
        seq = self.sequencer_for(sid)
        handle = self._sessions.get(sid)
        if handle is None:
            async for event in self._unsupported_stream(session, "resume",
                                                        "resume"):
                yield event
            return
        record = self._tracker.get(sid)
        yield self.emit(build_event(
            AgentEventType.SESSION_RESUMED, seq,
            external_session_id=handle.get("external_session_id"),
            native_type="opencode.session.resumed",
            payload={"transport": "http+sse",
                     "cwd": handle["cwd"]},
            agent_session_id=sid,
            run_id=record.run_id if record else None,
            execution_generation=(
                record.execution_generation if record else None),
        ))

    # -- 控制面 ---------------------------------------------------------------

    async def interrupt(self, session: AgentSessionRef) -> CommandReceipt:
        handle = self._sessions.get(session.agent_session_id)
        client = (handle or {}).get("client")
        external_id = (handle or {}).get("external_session_id") \
            or session.external_session_id
        if client is None or not external_id:
            return self.unsupported_receipt(
                "interrupt", "no_active_session", session=session)
        # Record the operator's abort intent before awaiting the HTTP response.
        # The abort endpoint may close the in-flight message request before its
        # own response resumes this coroutine; send() must still classify that
        # prompt failure as interrupted.
        handle["abort_requested"] = True
        try:
            await client.post(f"/session/{external_id}/abort")
        except OpenCodeError as exc:
            handle.pop("abort_requested", None)
            return self.unsupported_receipt(
                "interrupt", "abort_failed", session=session,
                detail={"detail": str(exc)[:200]})
        return CommandReceipt(
            command_id=new_id("cmd"),
            state=ReceiptState.COMPLETED,
            aggregate=AggregateRef(type="agent_session",
                                   id=session.agent_session_id),
        )

    async def steer(
        self, session: AgentSessionRef, input: AgentInput
    ) -> CommandReceipt:
        """OpenCode 当前没有可验证的同回合引导协议。"""
        del input
        return self.unsupported_receipt(
            "steer",
            "steer",
            session=session,
            detail={
                "detail": "prompt_async 是异步普通消息端点，未声明同回合引导语义"
            },
        )

    async def _teardown(self, session: AgentSessionRef) -> str:
        handle = self._sessions.pop(session.agent_session_id, None)
        if not handle:
            return "closed"
        stop = handle.get("sse_stop")
        if stop is not None:
            stop.set()
        sse_task = handle.get("sse_task")
        if sse_task is not None:
            sse_task.cancel()
        client = handle.get("client")
        if client is not None:
            await client.close()
        returncode = await self._stop_server(handle.get("proc"))
        return classify_exit(
            returncode=returncode if handle.get("proc") is not None else None,
            cancelled=handle.get("current_turn_id") is not None,
            resume_handle=handle.get("external_session_id")
            if handle.get("proc") is None else None,
        )


__all__ = [
    "OpenCodeError",
    "OpenCodeHttpClient",
    "OpenCodeServerAdapter",
]
