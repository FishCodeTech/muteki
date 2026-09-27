"""Kimi Adapter（RUNTIME-04，任务书 7.2/7.6）：ACP 与 Local Server 双模式。

核验结论（docs/research/third_party_verification.md §KIMI，核验基线
``@moonshot-ai/kimi-code@0.38.0``，本机实测 0.38.0）：

**KimiAcpAdapter**（首选结构化传输，``kimi acp``，复用 RUNTIME-03 的
``BaseAcpAdapter``）：

- 方法覆盖：``session/new``、``session/load``（回放历史）、
  ``session/resume``（不回放）、``session/list``、``session/prompt``、
  ``session/cancel``、审批走反向 ``session/request_permission``；replay
  与 live 事件区分由 ``AcpTransport`` 保证；
- ``session/new|load|resume`` 三处的 ``mcpServers`` 被转换为
  **ephemeral per-session** MCP server（不持久化，每次 resume 都重发，
  由 ``BaseAcpAdapter._launch`` 的恢复路径自动满足）；
- 认证是 terminal auth（设备码流程，``kimi acp --login``）；本 Adapter
  只在 initialize 声明了 authMethods 时才 authenticate，且默认不在
  自动化路径触发交互登录（``MUTEKI_KIMI_ACP_LOGIN=1`` 才允许）；
- 自定义 ``session/set_model``（v2 实现）：``SessionStart.model`` 在
  session 建立后下发；设置失败直接终止 session 创建并上抛原始错误。

**KimiLocalServerAdapter**（补充传输，``kimi web``，**experimental**）：

- 官方 WARNING：REST/WS API 为 experimental，接口稳定性不保证——启动后
  必须读实例 ``/openapi.json`` 与 ``/asyncapi.json``（本 Adapter 在
  ``_launch`` 与 probe 中实际读取，端点可用性以 spec paths 实测为准，
  不把字段写成跨版本常量）；
- Bearer token：所有 ``/api/*`` 要求 bearer；token 引用解析顺序：
  显式参数 > ``KIMI_CODE_SERVER_TOKEN`` > ``~/.kimi-code/server.token``
  （0600，由 ``kimi web`` 首启生成）；token 本体只进进程内存与
  请求鉴权上下文；
- 事件游标与 WS 协议（2026-08-21 本机实测 0.38.0 校准）：首帧
  ``server_hello``（``payload.{protocol_version, heartbeat_ms,
  max_event_buffer_size}``）；``subscribe``/``client_hello`` 的
  ``session_ids``/``cursors`` 在 ``payload`` 内，回执为 ``ack`` 帧；
  事件帧数据在 ``payload`` 内（工具事件字段为
  ``toolCallId``/``name``/``args``）；心跳 ``ping{payload.nonce}`` 须回
  ``pong``；``resync_required`` 的 ``payload.{current_seq, epoch}``
  直接给出新游标，原地重订阅即可（不必 snapshot 全量重建）；durable
  事件带严格递增 ``seq``，volatile 事件不重放；
- 中断：0.38.0 OpenAPI 无 ``/sessions/{id}:abort``（仅 :archive）——
  走 ``POST /api/v1/sessions/{id}/prompts/{prompt_id}:abort``
  （prompt_id 取自 prompt 响应）；审批应答 decision 枚举为
  ``approved|rejected|cancelled``；session 级 ``agent_config.model``
  经 REST 设置在 0.38.0 不生效，模型须按 prompt 下发
  （``POST /prompts`` 的 ``model`` 字段）；usage 在
  ``turn.step.completed`` 事件；
- 历史：``GET /api/v1/sessions/{id}/messages``（``before_id``/
  ``after_id`` 游标分页）只供恢复检阅，不作为新事件投影；
- 统一响应信封 ``{code, msg, data, request_id}``，HTTP 状态几乎恒
  200，业务结果看 ``code``。

兼容路径：``kimi -p --output-format stream-json`` 由
``muteki.solver.cli_driver.KimiCodeDriver`` + ``CliDriverAdapter``
（``cli.kimi``）承担；本模块不 import solver 层。
"""

from __future__ import annotations

import asyncio
import json
import os
import shutil
from pathlib import Path
from typing import Any, AsyncIterator, Optional

import httpx

from muteki.platform.contracts.base import new_id
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

from .acp import AcpHello, AcpTransport, BaseAcpAdapter, check_response
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
from .sessions import classify_exit


def default_kimi_binary() -> str:
    return os.environ.get("MUTEKI_KIMI_BIN", "kimi")


def _kimi_supervised_config(text: str) -> str:
    """Add Kimi-native ask rules without changing the user's config.

    Kimi's manual mode intentionally auto-approves Write/Edit inside a Git
    worktree.  Explicit user rules run before that built-in policy, so these
    rules make Muteki's supervised promise exact while leaving Kimi's mode
    engine and permission UI in control.
    """
    suffix = "" if not text or text.endswith(("\n", "\r")) else "\n"
    rules = "".join(
        "\n[[permission.rules]]\n"
        "decision = \"ask\"\n"
        f"pattern = \"{tool}\"\n"
        "reason = \"Muteki supervised mode\"\n"
        for tool in ("Write", "Edit", "Bash")
    )
    return f"{text}{suffix}{rules}"


class KimiAcpAdapter(BaseAcpAdapter):
    """Kimi Code 的 ACP 结构化 Adapter。

    session new/load/list/prompt/cancel/approval、replay 区分与
    ``mcpServers`` ephemeral 注入全部由 ``BaseAcpAdapter`` 实现；
    本类只提供 argv、auth 选择与 Kimi 特有钩子（``session/set_model``、
    ``list_sessions``）。
    """

    adapter_id = "kimi.acp"

    def __init__(
        self,
        *,
        binary: Optional[str] = None,
        runtime_root: Optional[str | Path] = None,
        **kwargs: Any,
    ) -> None:
        super().__init__(**kwargs)
        self._binary = binary or default_kimi_binary()
        self._runtime_root = Path(
            runtime_root
            or os.environ.get("MUTEKI_KIMI_RUNTIME_ROOT")
            or (
                Path(os.environ.get("MUTEKI_STATE_ROOT") or "state")
                / "_kimi_acp_runtime"
            )
        ).expanduser().resolve()

    def _agent_argv(self) -> list[str]:
        return [self._binary, "acp"]

    def _agent_argv_for_request(self, request: SessionStart) -> list[str]:
        del request
        # Kimi ACP advertises and applies autonomy per session.  Keep argv
        # neutral and select the Agent's own mode after new/load/resume.
        return self._agent_argv()

    def _select_auth_method(
        self, auth_methods: list[dict[str, Any]]
    ) -> Optional[str]:
        # Kimi 认证是 terminal auth（设备码）。自动化路径默认不触发交互
        # 登录：仅当显式允许（MUTEKI_KIMI_ACP_LOGIN=1）时才 authenticate；
        # 已登录的 kimi 实例 initialize 一般不强制 authenticate。
        if not auth_methods:
            return None
        if os.environ.get("MUTEKI_KIMI_ACP_LOGIN") == "1":
            return super()._select_auth_method(auth_methods)
        return None

    def _prepare_session_environment(
        self, request: SessionStart, env: dict[str, str], cwd: str
    ) -> dict[str, str]:
        del cwd
        if request.effort and request.effort != "default":
            env = {**env, "KIMI_MODEL_THINKING_EFFORT": str(request.effort)}
        mode = request.access_mode or AccessMode.SUPERVISED.value
        if mode != AccessMode.SUPERVISED.value:
            return env

        prepared = dict(env)
        source = Path(
            prepared.get("KIMI_CODE_HOME")
            or os.environ.get("KIMI_CODE_HOME")
            or (Path.home() / ".kimi-code")
        ).expanduser().resolve()
        target = self._runtime_root / request.agent_session_id
        target.mkdir(parents=True, exist_ok=True)
        try:
            target.chmod(0o700)
        except OSError:
            pass

        # Reuse Kimi's auth, models, plugins and persistent data.  Only the
        # config is private, so Muteki never rewrites the user's Kimi setup.
        if source.is_dir() and source != target:
            for item in source.iterdir():
                if item.name == "config.toml":
                    continue
                if item.is_symlink() and not item.exists():
                    continue
                destination = target / item.name
                if destination.exists() or destination.is_symlink():
                    continue
                try:
                    destination.symlink_to(
                        item, target_is_directory=item.is_dir()
                    )
                except OSError:
                    if item.is_file():
                        shutil.copy2(item, destination)

        try:
            config_text = (source / "config.toml").read_text(
                encoding="utf-8"
            )
        except OSError:
            config_text = ""
        target_config = target / "config.toml"
        target_config.write_text(
            _kimi_supervised_config(config_text), encoding="utf-8"
        )
        try:
            target_config.chmod(0o600)
        except OSError:
            pass
        prepared["KIMI_CODE_HOME"] = str(target)
        # Kimi 0.38's default agent-core-v2 ACP path validates permission
        # rules but does not load them into the session permission service.
        # Its maintained SDK/ACP adapter path does load the same native rules.
        # Keep this scoped to Muteki supervised sessions until the v2 path
        # applies its documented permission config as well.
        prepared["KIMI_CODE_LEGACY_FLAG"] = "1"
        return prepared

    async def _after_session_open(
        self, transport: AcpTransport, session_id: str, request: SessionStart
    ) -> None:
        access_mode = request.access_mode or AccessMode.SUPERVISED.value
        native_mode = {
            AccessMode.SUPERVISED.value: "default",
            # Kimi has no edit-only mode; retain its manual-approval mode.
            AccessMode.AUTO_ACCEPT_EDITS.value: "default",
            AccessMode.AUTO.value: "auto",
            AccessMode.FULL_ACCESS.value: "yolo",
        }[access_mode]
        if request.model:
            # 指定模型属于会话契约。对端拒绝或返回 error 时直接阻止会话启动，
            # 避免继续使用旧模型并把结果归到错误的模型名下。
            response = await transport.peer.request("session/set_model", {
                "sessionId": session_id, "modelId": str(request.model),
            }, timeout=30.0)
            check_response("session/set_model", response)
        # Apply the native mode last so model/profile switching cannot restore
        # a persisted session mode after Muteki selected the requested one.
        await transport.set_mode(session_id, native_mode)

    async def list_sessions(self) -> list[dict[str, Any]]:
        """真实 ``session/list``（短连接：initialize → list → 关闭）。"""
        transport = AcpTransport(self._agent_argv())
        await transport.start()
        try:
            hello = await transport.initialize(timeout=self._startup_timeout)
            method_id = self._select_auth_method(hello.auth_methods)
            if method_id:
                await transport.authenticate(
                    method_id, meta=self._authenticate_meta())
            return await transport.list_sessions()
        finally:
            await transport.close()

    def _probe_extra_caps(self, caps: AgentCapabilities, hello: AcpHello) -> None:
        # Kimi v2 能力（核验 §KIMI-1）：loadSession、resume/close/list、
        # mcp http+sse；基础类已按 hello 映射，这里只补充 Kimi 实测差异。
        caps.supported_models = []  # ACP v1 无模型枚举方法，保持空（static）


# ---------------------------------------------------------------------------
# Kimi Local Server（experimental）
# ---------------------------------------------------------------------------

#: Local Server 默认端口（占用时 kimi 自动 +1 重试，实例注册在
#: ``~/.kimi-code/server/instances/``）。
DEFAULT_SERVER_PORT = 58627
#: bearer token 的默认持久化位置（kimi web 首启生成，0600）。
DEFAULT_TOKEN_FILE = Path.home() / ".kimi-code" / "server.token"


class KimiLocalServerError(RuntimeError):
    """Kimi Local Server 调用失败（传输错误或业务 code 非成功）。"""


def resolve_server_token(explicit: str = "") -> str:
    """bearer token 引用解析：显式参数 > 环境变量 > 实例 token 文件。

    返回 token 本体。
    """
    if explicit:
        return explicit
    from_env = os.environ.get("KIMI_CODE_SERVER_TOKEN", "").strip()
    if from_env:
        return from_env
    try:
        return DEFAULT_TOKEN_FILE.read_text(encoding="utf-8").strip()
    except OSError:
        return ""


class KimiLocalServerAdapter(BaseExternalAgentAdapter):
    """Kimi Local Server（``kimi web``）REST + WebSocket Adapter。

    experimental 接口纪律（核验 §KIMI-3 的硬性要求）：

    - 启动后读取该实例 ``/openapi.json`` 与 ``/asyncapi.json``，端点
      可用性以 spec paths 实测为准（``probe`` 与 ``_launch`` 都做）；
      字段名仅在单版本内核验过，不作为跨版本常量承诺；
    - 事件走 WS ``/api/v1/ws``（durable seq/epoch 游标）；WS 不可用
      （缺 ``websockets`` 包或握手失败）时显式降级：probe 记录
      degradation，``send`` 返回 unsupported，不伪造完成事件。
    """

    adapter_id = "kimi.local_server"

    def __init__(
        self,
        *,
        binary: Optional[str] = None,
        base_url: Optional[str] = None,
        token: str = "",
        extra_env: Optional[dict[str, str]] = None,
        port: Optional[int] = None,
        manage_server: bool = True,
        startup_timeout: float = 30.0,
        prompt_timeout: float = 600.0,
        **kwargs: Any,
    ) -> None:
        super().__init__(self.adapter_id, **kwargs)
        self._binary = binary or default_kimi_binary()
        self._attach_url = base_url
        self._token = token
        self._extra_env = dict(extra_env or {})
        self._port = port
        self._manage_server = manage_server
        self._startup_timeout = float(startup_timeout)
        self._prompt_timeout = float(prompt_timeout)
        self._sessions: dict[str, dict[str, Any]] = {}

    # -- server 生命周期 -------------------------------------------------------

    def _serve_argv(self, port: int) -> list[str]:
        """自管 server 进程命令行（子类/测试可覆盖指向 mock server）。"""
        return [self._binary, "web", "--no-open"]

    def _server_env(
        self, session_env: Optional[dict[str, str]] = None
    ) -> dict[str, str]:
        return {**self._extra_env, **dict(session_env or {})}

    async def _start_server(
        self, cwd: str, *, env: Optional[dict[str, str]] = None
    ) -> "tuple[Optional[asyncio.subprocess.Process], str, str]":
        """返回 (进程, base_url, token)。attach 模式不拉起进程。"""
        token = resolve_server_token(self._token)
        if self._attach_url:
            return None, self._attach_url.rstrip("/"), token
        if not self._manage_server:
            raise KimiLocalServerError(
                "manage_server=False 时必须提供 base_url")
        port = self._port or DEFAULT_SERVER_PORT
        base_url = f"http://127.0.0.1:{port}"
        proc = await asyncio.create_subprocess_exec(
            *self._serve_argv(port),
            cwd=cwd or None,
            env={**os.environ, **self._server_env(env)},
            stdout=asyncio.subprocess.DEVNULL,
            stderr=asyncio.subprocess.DEVNULL,
        )
        # 首启会生成 token 文件，等服务可用后再解析一次。
        deadline = asyncio.get_running_loop().time() + self._startup_timeout
        while True:
            if proc.returncode is not None:
                raise KimiLocalServerError(
                    f"kimi web 提前退出（code={proc.returncode}）")
            try:
                async with httpx.AsyncClient(
                        base_url=base_url, timeout=5.0,
                        trust_env=False) as probe_client:
                    resp = await probe_client.get("/api/v1/healthz")
                    if resp.status_code < 400:
                        break
            except httpx.HTTPError:
                pass
            if asyncio.get_running_loop().time() > deadline:
                raise KimiLocalServerError("kimi web 启动超时（healthz 不可达）")
            await asyncio.sleep(0.3)
        if not token:
            token = resolve_server_token("")
        return proc, base_url, token

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

    def _client(self, base_url: str, token: str) -> httpx.AsyncClient:
        headers = {}
        if token:
            headers["Authorization"] = f"Bearer {token}"
        return httpx.AsyncClient(base_url=base_url, headers=headers,
                                 timeout=httpx.Timeout(30.0, read=None),
                                 trust_env=not base_url.startswith(
                                     ("http://127.0.0.1", "http://localhost")))

    @staticmethod
    async def _unwrap(payload: Any, what: str) -> Any:
        """解开统一信封 ``{code, msg, data}``；code 非 0 抛错。"""
        if not isinstance(payload, dict):
            return payload
        code = payload.get("code")
        if code not in (None, 0, "0"):
            raise KimiLocalServerError(
                f"{what} 失败：code={code} msg={str(payload.get('msg'))[:160]}")
        return payload.get("data")

    async def _read_specs(
        self, client: httpx.AsyncClient
    ) -> "tuple[dict[str, Any], dict[str, Any]]":
        """读取实例 OpenAPI / AsyncAPI（experimental 接口的能力依据）。

        注意：两个 spec 文档是**裸文档**，不走 ``{code,msg,data}`` 信封。
        """
        openapi: dict[str, Any] = {}
        asyncapi: dict[str, Any] = {}
        try:
            parsed = (await client.get("/openapi.json")).json()
            if isinstance(parsed, dict):
                openapi = parsed
        except Exception:  # noqa: BLE001
            openapi = {}
        try:
            parsed = (await client.get("/asyncapi.json")).json()
            if isinstance(parsed, dict):
                asyncapi = parsed
        except Exception:  # noqa: BLE001
            asyncapi = {}
        return openapi, asyncapi

    # -- probe ----------------------------------------------------------------

    async def probe(self, request: ProbeRequest) -> AgentCapabilities:
        field_sources: dict[str, str] = {}
        degradations: list[str] = []
        caps = conservative_capabilities(
            transport_kind="http+ws", capability_source=SOURCE_STATIC)
        version = _probe_version(self._binary)
        caps.runtime_version = version
        detail = ""
        openapi: dict[str, Any] = {}
        asyncapi: dict[str, Any] = {}
        meta: dict[str, Any] = {}
        proc: Optional[asyncio.subprocess.Process] = None
        if version or self._attach_url:
            try:
                proc, base_url, token = await self._start_server(os.getcwd())
                client = self._client(base_url, token)
                try:
                    openapi, asyncapi = await self._read_specs(client)
                    try:
                        meta = await self._unwrap(
                            (await client.get("/api/v1/meta")).json(),
                            "meta") or {}
                    except Exception:  # noqa: BLE001
                        meta = {}
                finally:
                    await client.aclose()
            except (OSError, KimiLocalServerError, asyncio.TimeoutError,
                    httpx.HTTPError) as exc:
                detail = f"kimi web 探测失败：{str(exc)[:160]}"
            finally:
                if proc is not None:
                    await self._stop_server(proc)

        paths = openapi.get("paths") if isinstance(
            openapi.get("paths"), dict) else {}
        probed = bool(paths)
        if not probed:
            for field_name in BOOL_CAPABILITY_FIELDS:
                field_sources[field_name] = SOURCE_STATIC
            degradations.append(
                "Local Server 不可用：REST/WS 会话、事件游标与历史均未实测，"
                "按保守默认 False（不静默降级）")
            self._probe_cache = CapabilityProbeReport(
                adapter_id=self.identity.adapter_id,
                instance_id=self.identity.instance_id,
                capabilities=caps, binary_path=self._binary,
                field_sources=field_sources, degradations=degradations,
                detail=detail or "binary 不可用或 /openapi.json 不可达",
            )
            return caps

        def has(path: str) -> bool:
            return any(p == path or p.startswith(path.rstrip("/") + "/")
                       for p in paths)

        try:
            import websockets  # noqa: F401
            ws_available = True
        except ImportError:
            ws_available = False

        caps.capability_source = SOURCE_PROBE
        caps.protocol_version = str(openapi.get("openapi") or "")[:40]
        if isinstance(meta, dict) and meta.get("version"):
            caps.runtime_version = str(meta["version"])[:120]
        caps.resume = has("/api/v1/sessions")
        caps.session_persistence = caps.resume
        caps.interrupt = ws_available
        caps.streaming = ws_available
        caps.tool_events = ws_available
        caps.usage_events = ws_available
        caps.approval = ws_available and any("approvals" in p for p in paths)
        caps.user_input = ws_available and any("questions" in p for p in paths)
        caps.steer = ws_available and any(
            p.endswith("/prompts:steer")
            or ("/prompts/{" in p and p.endswith(":steer"))
            for p in paths
        )
        for field_name in BOOL_CAPABILITY_FIELDS:
            field_sources[field_name] = SOURCE_PROBE
        if not ws_available:
            degradations.append(
                "websockets 包不可用：WS 事件流/中断/审批未启用，send 将"
                "返回 unsupported（experimental 接口不伪造完成事件）")
        degradations.append(
            "experimental 接口：端点与字段以该实例 /openapi.json + "
            "/asyncapi.json 实测为准，任何版本都可能变化")
        if not asyncapi:
            degradations.append("未读到 /asyncapi.json：WS 事件契约未知，"
                                "事件解析按宽容模式处理")
        self._probe_cache = CapabilityProbeReport(
            adapter_id=self.identity.adapter_id,
            instance_id=self.identity.instance_id,
            capabilities=caps, binary_path=self._binary,
            field_sources=field_sources, degradations=degradations,
            detail=f"kimi web OpenAPI 探测成功（{len(paths)} 条路径）",
        )
        return caps

    # -- 启动 / 接管 -------------------------------------------------------------

    async def _launch(
        self,
        request: SessionStart,
        plan: Optional[Any],
        bearer_token: Optional[str],
    ) -> dict[str, Any]:
        cwd = str(request.options.get("cwd") or os.getcwd())
        session_env = {
            str(key): str(value)
            for key, value in dict(request.options.get("env") or {}).items()
        }
        proc, base_url, token = await self._start_server(cwd, env=session_env)
        client = self._client(base_url, token)
        try:
            # experimental 纪律：启动后读实例 spec，端点以实测为准。
            openapi, asyncapi = await self._read_specs(client)
            paths = openapi.get("paths") if isinstance(
                openapi.get("paths"), dict) else {}
            if "/api/v1/sessions" not in paths and not any(
                    p.startswith("/api/v1/sessions") for p in paths):
                raise KimiLocalServerError(
                    "该实例 OpenAPI 无 /api/v1/sessions：版本不兼容，"
                    "拒绝按猜测路径启动会话")

            resume_handle = request.resume_handle
            if resume_handle:
                # 恢复：以 snapshot 探测存在性（带游标语义），随后 WS
                # subscribe 用 {seq, epoch} 续订。
                await self._unwrap(
                    (await client.get(
                        f"/api/v1/sessions/{resume_handle}/snapshot")).json(),
                    "snapshot")
                session_id = resume_handle
            else:
                created = await self._unwrap((await client.post(
                    "/api/v1/sessions",
                    json={"metadata": {"cwd": cwd}})).json(), "create session")
                session_id = str((created or {}).get("id")
                                 or (created or {}).get("session_id") or "")
                if not session_id:
                    raise KimiLocalServerError(
                        "POST /api/v1/sessions 未返回 session id")
        except Exception:
            await client.aclose()
            if proc is not None:
                await self._stop_server(proc)
            raise

        handle: dict[str, Any] = {
            "conversation_thread_id": request.thread_id,
            "proc": proc,
            "client": client,
            "base_url": base_url,
            "token": token,
            "cwd": cwd,
            "options": dict(request.options),
            "turns": 0,
            "external_session_id": session_id,
            "model": request.model,
            "resumed": bool(resume_handle),
            "event_sink": None,
            "current_turn_id": None,
            "ws_task": None,
            "ws_stop": None,
            # 事件游标：durable seq + epoch（重连续订/检洞依据）。
            "cursor": {"seq": 0, "epoch": ""},
            "asyncapi": bool(asyncapi),
            "openapi_paths": dict(paths),
        }
        self._sessions[request.agent_session_id] = handle
        return {"external_session_id": session_id,
                "resume_handle": session_id}

    # -- WS 事件流（durable 游标） -------------------------------------------------

    async def _ws_loop(self, agent_session_id: str) -> None:
        """WS 订阅循环：server_hello → subscribe（带游标）→ 事件分发。

        0.38.0 实测协议面（见核验 §KIMI 本机实测）：

        - 首帧 ``server_hello``，``protocol_version`` 在 ``payload`` 内；
        - ``subscribe`` 的 ``session_ids``/``cursors`` 在 ``payload`` 内，
          回执为 ``ack`` 帧；游标无效（epoch 空/过期）时服务器先回
          ``resync_required``（``payload.{current_seq, epoch}``），用其
          更新游标并重订阅即可，无需断连；
        - 心跳：服务器每 ``heartbeat_ms`` 发 ``ping{payload.nonce}``，
          须回 ``pong{payload.nonce}``；
        - 断线重连时用最近游标续订；durable 事件可重放。
        """
        import websockets

        handle = self._sessions.get(agent_session_id)
        if handle is None:
            return
        stop: asyncio.Event = handle["ws_stop"]
        token = handle["token"]
        base = handle["base_url"]
        ws_url = ("ws" + base[len("http"):]) + "/api/v1/ws"
        headers = {}
        if token:
            headers["Authorization"] = f"Bearer {token}"

        while not stop.is_set():
            try:
                async with websockets.connect(
                        ws_url, additional_headers=headers or None) as ws:
                    handle["ws"] = ws
                    hello_raw = await asyncio.wait_for(ws.recv(), timeout=10.0)
                    try:
                        hello = json.loads(hello_raw)
                    except (TypeError, json.JSONDecodeError):
                        hello = {}
                    hello_payload = hello.get("payload") if isinstance(
                        hello.get("payload"), dict) else hello
                    handle["ws_protocol_version"] = (
                        hello_payload.get("protocol_version")
                        if isinstance(hello_payload, dict) else None)
                    await self._ws_subscribe(ws, handle)
                    ready = handle.get("ws_ready")
                    if ready is not None and not ready.is_set():
                        ready.set()
                    async for raw in ws:
                        if stop.is_set():
                            return
                        try:
                            frame = json.loads(raw)
                        except (TypeError, json.JSONDecodeError):
                            continue
                        if not isinstance(frame, dict):
                            continue
                        ftype = str(frame.get("type") or "")
                        payload = frame.get("payload") if isinstance(
                            frame.get("payload"), dict) else {}
                        if ftype == "ping":
                            # 心跳应答（nonce 回显），否则服务器断开空闲连接。
                            await ws.send(json.dumps({
                                "type": "pong",
                                "payload": {"nonce": str(
                                    payload.get("nonce") or "")}}))
                            continue
                        if ftype == "ack":
                            # subscribe/abort 回执：不投影为事件。
                            continue
                        if ftype == "resync_required":
                            # 游标失效：用 payload 里的 current_seq/epoch
                            # 更新水位并原地重订阅（历史不作为新事件投影）。
                            handle["cursor"] = {
                                "seq": int(payload.get("current_seq") or 0),
                                "epoch": str(payload.get("epoch") or "")}
                            sink = handle.get("event_sink")
                            if sink is not None:
                                sink.put_nowait((
                                    AgentEventType.RUNTIME_WARNING,
                                    "kimi.ws.resync_required",
                                    {"reason": payload.get("reason")}))
                            await self._ws_subscribe(ws, handle)
                            continue
                        await self._handle_ws_frame(agent_session_id, frame)
            except asyncio.CancelledError:
                return
            except Exception:  # noqa: BLE001
                # 断线：短暂退避后带游标重连（durable 事件可重放）。
                try:
                    await asyncio.wait_for(stop.wait(), timeout=1.0)
                    return
                except asyncio.TimeoutError:
                    continue

    @staticmethod
    async def _ws_subscribe(ws: Any, handle: dict[str, Any]) -> None:
        """发送 ``subscribe`` 控制帧（session_ids/cursors 在 payload 内）。"""
        session_id = handle["external_session_id"]
        cursor = handle["cursor"]
        await ws.send(json.dumps({
            "type": "subscribe",
            "id": new_id("sub"),
            "payload": {
                "session_ids": [session_id],
                "cursors": {session_id: {
                    "seq": int(cursor.get("seq") or 0),
                    "epoch": str(cursor.get("epoch") or ""),
                }},
            },
        }))

    async def _handle_ws_frame(
        self, agent_session_id: str, frame: dict[str, Any]
    ) -> None:
        handle = self._sessions.get(agent_session_id)
        if handle is None:
            return
        ftype = str(frame.get("type") or "")

        # durable 事件游标推进（volatile 不重放，只用于检洞）。
        seq_no = frame.get("seq")
        if isinstance(seq_no, int):
            cursor = handle["cursor"]
            cursor["seq"] = max(int(cursor.get("seq") or 0), seq_no)
            if frame.get("epoch"):
                cursor["epoch"] = str(frame["epoch"])

        sink = handle.get("event_sink")
        if sink is None:
            handle["ws_outside_turn"] = handle.get("ws_outside_turn", 0) + 1
            return

        # 事件数据在 payload 内（0.38.0 实测；payload 自带重复 type 字段，
        # 无害）。旧 mock/文档形态用 data，兜底兼容。
        data = frame.get("payload") if isinstance(frame.get("payload"), dict) \
            else (frame.get("data") if isinstance(frame.get("data"), dict)
                  else frame)
        if ftype == "turn.started":
            return
        if ftype == "turn.ended":
            sink.put_nowait(("__turn_done__", "kimi.turn.ended", {
                "stop_reason": data.get("reason")
                or data.get("stop_reason") or "end_turn",
                "error": data.get("error"),
            }))
        elif ftype in ("assistant.delta", "assistant.message"):
            text = str(data.get("delta") or data.get("text") or "")
            if text:
                sink.put_nowait((AgentEventType.MESSAGE_DELTA,
                                 f"kimi.{ftype}", {"text": text}))
        elif ftype == "thinking.delta":
            text = str(data.get("delta") or data.get("text") or "")
            if text:
                sink.put_nowait((AgentEventType.MESSAGE_DELTA,
                                 "kimi.thinking.delta",
                                 {"text": text, "thinking": True}))
        elif ftype == "tool.call.started":
            sink.put_nowait((AgentEventType.TOOL_STARTED,
                             "kimi.tool.call.started", {
                                 "call_id": str(data.get("toolCallId")
                                                or data.get("call_id")
                                                or data.get("id") or ""),
                                 "tool": str(data.get("name")
                                             or data.get("tool") or ""),
                                 "input": data.get("args")
                                 or data.get("arguments"),
                             }))
        elif ftype == "tool.result":
            sink.put_nowait((AgentEventType.TOOL_COMPLETED,
                             "kimi.tool.result", {
                                 "call_id": str(data.get("toolCallId")
                                                or data.get("call_id")
                                                or data.get("id") or ""),
                                 "output": data.get("output")
                                 or data.get("content"),
                                 "is_error": bool(data.get("is_error")),
                             }))
        elif ftype == "event.approval.requested":
            approval_id = str(data.get("approval_id") or data.get("id") or "")
            sink.put_nowait((AgentEventType.APPROVAL_REQUESTED,
                             "kimi.approval.requested", {
                                 "approval_id": approval_id,
                                 "tool": data.get("tool_name"),
                                 "action": data.get("action"),
                                 "native": data,
                             }))
            if approval_id:
                asyncio.ensure_future(
                    self._reply_approval(handle, approval_id))
        elif ftype in ("usage", "turn.step.completed"):
            # 0.38.0 实测 usage 在 turn.step.completed
            # （{inputOther, output, inputCacheRead, inputCacheCreation}）。
            usage = data.get("usage") if ftype == "turn.step.completed" \
                else data
            if isinstance(usage, dict) and usage:
                sink.put_nowait((AgentEventType.USAGE_UPDATED,
                                 f"kimi.{ftype}", {"usage": usage}))
        # 其余事件（subagent.*/task.*/compaction.*/tool.call.delta/
        # agent.status.updated 等）不改变核心状态机。

    async def _reply_approval(
        self, handle: dict[str, Any], approval_id: str
    ) -> None:
        """无人值守默认批准（decision 枚举为 approved|rejected|cancelled，
        0.38.0 实测）；结果事件在 WS 侧已发 REQUESTED。"""
        client: httpx.AsyncClient = handle["client"]
        session_id = handle["external_session_id"]
        sink = handle.get("event_sink")
        try:
            await client.post(
                f"/api/v1/sessions/{session_id}/approvals/{approval_id}",
                json={"decision": "approved"})
            resolved = {"approval_id": approval_id, "decision": "approved"}
        except Exception as exc:  # noqa: BLE001
            resolved = {"approval_id": approval_id, "decision": "failed",
                        "detail": str(exc)[:200]}
        if sink is not None:
            sink.put_nowait((AgentEventType.APPROVAL_RESOLVED,
                             "kimi.approval.resolved", resolved))

    # -- turn 流 --------------------------------------------------------------

    def send(
        self, session: AgentSessionRef, input: AgentInput
    ) -> AsyncIterator[AgentEvent]:
        return self._turn_stream(session, input)

    async def _ensure_ws(self, handle: dict[str, Any]) -> bool:
        if handle.get("ws_task") is not None:
            return True
        try:
            import websockets  # noqa: F401
        except ImportError:
            return False
        stop = asyncio.Event()
        ready = asyncio.Event()
        handle["ws_stop"] = stop
        handle["ws_ready"] = ready
        handle["ws_task"] = asyncio.ensure_future(
            self._ws_loop(handle["agent_session_id"]))
        # 等首轮 server_hello + subscribe 完成再放行 prompt，否则 turn 早期
        # 事件（delta/tool/approval）可能在订阅建立前被漏收。
        try:
            await asyncio.wait_for(ready.wait(), timeout=10.0)
        except asyncio.TimeoutError:
            return False
        return True

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
        handle["agent_session_id"] = sid
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
                native_type="kimi.local.session.start",
                payload={"transport": "http+ws",
                         "adapter_id": self.id,
                         "instance_id": self.identity.instance_id,
                         "cwd": handle["cwd"],
                         "experimental": True},
                **common))

        if not await self._ensure_ws(handle):
            yield self.emit(build_event(
                AgentEventType.RUNTIME_ERROR, seq,
                external_session_id=external_id,
                native_type="kimi.ws.unavailable",
                payload={"code": "external_agent.kimi.ws_unavailable",
                         "detail": "websockets 不可用，experimental Local "
                                   "Server 事件流未启用（不伪造完成事件）"},
                **common))
            return

        turn_id = new_id("turn")
        handle["current_turn_id"] = turn_id
        yield self.emit(build_event(
            AgentEventType.TURN_STARTED, seq,
            external_session_id=external_id, turn_id=turn_id,
            native_type="kimi.prompt.start",
            payload={"kind": input.kind},
            **common))

        queue: asyncio.Queue = asyncio.Queue()
        handle["event_sink"] = queue
        handle["saw_assistant_text"] = False
        handle["assistant_text_parts"] = []
        client: httpx.AsyncClient = handle["client"]

        async def run_prompt() -> Any:
            try:
                body: dict[str, Any] = {
                    "content": [{"type": "text", "text": input.text}],
                }
                # 0.38.0 实测：session agent_config.model 经 REST 设置不生效，
                # 模型须按 prompt 下发（POST /prompts 的 model 字段）。
                model = handle.get("model")
                if model:
                    body["model"] = str(model)
                resp = await client.post(
                    f"/api/v1/sessions/{external_id}/prompts",
                    json=body,
                    timeout=self.conversation_turn_timeout(
                        handle.get("conversation_thread_id"),
                        self._prompt_timeout))
                # prompt_id 供 interrupt（prompts/{pid}:abort）使用。
                try:
                    pdata = resp.json().get("data") or {}
                    if pdata.get("prompt_id"):
                        handle["current_prompt_id"] = str(pdata["prompt_id"])
                except Exception:  # noqa: BLE001
                    pass
                return resp
            except Exception as exc:  # noqa: BLE001
                return {"__error__": exc}

        task = asyncio.ensure_future(run_prompt())
        get_task = asyncio.ensure_future(queue.get())
        done_info: dict[str, Any] = {}
        submit_error: Optional[Exception] = None
        prompt_response: Any = None
        try:
            while True:
                done, _pending = await asyncio.wait(
                    {task, get_task}, return_when=asyncio.FIRST_COMPLETED)
                if task in done:
                    prompt_response = task.result()
                    if isinstance(prompt_response, dict) and isinstance(
                            prompt_response.get("__error__"), Exception):
                        submit_error = prompt_response["__error__"]
                        break
                    # prompt 提交成功只表示受理；完成由 turn.ended 事件判定
                    # （信封 code 非 0 在 _unwrap 之外，这里显式检查）。
                    if isinstance(prompt_response, dict):
                        body = None
                        try:
                            body = prompt_response.json()
                        except Exception:  # noqa: BLE001
                            body = None
                        if isinstance(body, dict) and body.get("code") not in (
                                None, 0, "0"):
                            submit_error = KimiLocalServerError(
                                f"prompt 提交被拒：code={body.get('code')} "
                                f"msg={str(body.get('msg'))[:160]}")
                            break
                if get_task in done:
                    etype, native_type, payload = get_task.result()
                    get_task = asyncio.ensure_future(queue.get())
                    if etype == "__turn_done__":
                        done_info = dict(payload)
                        break
                    if (etype is AgentEventType.MESSAGE_DELTA
                            and not payload.get("thinking")
                            and str(payload.get("text") or "").strip()):
                        handle["saw_assistant_text"] = True
                        handle["assistant_text_parts"].append(
                            str(payload.get("text") or ""))
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
        handle["current_turn_id"] = None
        handle["current_prompt_id"] = None

        if submit_error is not None:
            yield self.emit(build_event(
                AgentEventType.TURN_FAILED, seq,
                external_session_id=external_id, turn_id=turn_id,
                native_type="kimi.prompt.error",
                payload={"error": str(submit_error)[:300]},
                **common))
            return
        stop_reason = str(done_info.get("stop_reason") or "end_turn")
        done_error = done_info.get("error")
        if stop_reason in ("cancelled", "aborted", "interrupted"):
            yield self.emit(build_event(
                AgentEventType.TURN_FAILED, seq,
                external_session_id=external_id, turn_id=turn_id,
                native_type="kimi.prompt.aborted",
                payload={"reason": "interrupted", "stop_reason": stop_reason},
                **common))
            return
        if stop_reason in ("failed", "error"):
            # turn.ended reason=failed（如 model.not_configured）如实上报。
            yield self.emit(build_event(
                AgentEventType.TURN_FAILED, seq,
                external_session_id=external_id, turn_id=turn_id,
                native_type="kimi.turn.failed",
                payload={"reason": "failed", "stop_reason": stop_reason,
                         "error": done_error},
                **common))
            return
        if not handle.pop("saw_assistant_text", False):
            yield self.emit(build_event(
                AgentEventType.TURN_FAILED, seq,
                external_session_id=external_id, turn_id=turn_id,
                native_type="kimi.empty_assistant",
                payload={
                    "reason": "empty_assistant",
                    "error": {
                        "code": "kimi.empty_assistant",
                        "message": "Kimi turn ended without assistant text",
                    },
                    "stop_reason": stop_reason,
                },
                **common))
            return
        final_text = "".join(handle.pop("assistant_text_parts", []))
        yield self.emit(build_event(
            AgentEventType.MESSAGE_COMPLETED, seq,
            external_session_id=external_id, turn_id=turn_id,
            native_type="kimi.message.completed",
            payload={"text": final_text, "role": "assistant"},
            **common))
        yield self.emit(build_event(
            AgentEventType.TURN_COMPLETED, seq,
            external_session_id=external_id, turn_id=turn_id,
            native_type="kimi.turn.ended",
            payload={"stop_reason": stop_reason},
            **common))

    def resume(self, session: AgentSessionRef) -> AsyncIterator[AgentEvent]:
        return self._resume_stream(session)

    async def _resume_stream(
        self, session: AgentSessionRef
    ) -> AsyncIterator[AgentEvent]:
        """恢复：snapshot 已在 ``_launch`` 探测；历史经
        ``GET .../messages`` 由调用方另行检阅，不作为新事件投影。"""
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
            native_type="kimi.local.session.resumed",
            payload={"transport": "http+ws",
                     "cursor": dict(handle.get("cursor") or {}),
                     "experimental": True},
            agent_session_id=sid,
            run_id=record.run_id if record else None,
            execution_generation=(
                record.execution_generation if record else None),
        ))

    async def history(
        self, session: AgentSessionRef, *, after_id: str = "",
        page_size: int = 50,
    ) -> list[dict[str, Any]]:
        """``GET /api/v1/sessions/{id}/messages`` 游标分页历史（只读）。"""
        handle = self._sessions.get(session.agent_session_id)
        if handle is None:
            return []
        client: httpx.AsyncClient = handle["client"]
        params: dict[str, Any] = {"page_size": page_size}
        if after_id:
            params["after_id"] = after_id
        payload = await self._unwrap((await client.get(
            f"/api/v1/sessions/{handle['external_session_id']}/messages",
            params=params)).json(), "messages")
        if isinstance(payload, dict):
            messages = payload.get("messages") or payload.get("items") or []
            return list(messages) if isinstance(messages, list) else []
        return list(payload) if isinstance(payload, list) else []

    # -- 控制面 ---------------------------------------------------------------

    async def steer(
        self, session: AgentSessionRef, input: AgentInput
    ) -> CommandReceipt:
        """用 Kimi 原生 prompt queue + ``:steer`` 注入当前回合。"""
        handle = self._sessions.get(session.agent_session_id)
        client = (handle or {}).get("client")
        external_id = (handle or {}).get("external_session_id") \
            or session.external_session_id
        paths = (handle or {}).get("openapi_paths") or {}
        steer_path = f"/api/v1/sessions/{external_id}/prompts/{{prompt_id}}:steer"
        steer_available = any(
            str(path).endswith("/prompts:steer")
            or ("/prompts/{" in str(path) and str(path).endswith(":steer"))
            for path in paths
        )
        if client is None or not external_id or not steer_available:
            return self.unsupported_receipt(
                "steer", "steer", session=session,
                detail={"detail": "该 Kimi Local Server 实例未声明 prompt steer"},
            )
        # Kimi 的 prompt id 是服务端资源标识，独立于前端消息幂等 id。
        # 统一使用本地生成的 prompt id，避免把不同客户端的 id 格式带进协议。
        prompt_id = new_id("prompt")
        try:
            submitted = await self._unwrap((await client.post(
                f"/api/v1/sessions/{external_id}/prompts",
                json={
                    "prompt_id": prompt_id,
                    "content": [{"type": "text", "text": input.text}],
                },
            )).json(), "steer prompt submit")
            actual_prompt_id = str(
                (submitted or {}).get("prompt_id") or prompt_id
            )
            await self._unwrap((await client.post(
                steer_path.format(prompt_id=actual_prompt_id), json={}
            )).json(), "steer prompt")
        except Exception as exc:  # noqa: BLE001
            return self.unsupported_receipt(
                "steer", "steer_rejected", session=session,
                detail={"detail": str(exc)[:200]},
            )
        return CommandReceipt(
            command_id=new_id("cmd"),
            state=ReceiptState.COMPLETED,
            aggregate=AggregateRef(
                type="agent_session", id=session.agent_session_id
            ),
        )

    async def interrupt(self, session: AgentSessionRef) -> CommandReceipt:
        """中断当前 prompt：``POST .../prompts/{prompt_id}:abort``。

        0.38.0 实测 OpenAPI 无 ``/sessions/{id}:abort``（仅 :archive）；
        prompt 级 abort 端点描述为 "Abort a running prompt"。无 prompt_id
        （turn 间隙）时兜底旧 ``:abort`` 路径（向后兼容文档形态）。
        """
        handle = self._sessions.get(session.agent_session_id)
        client = (handle or {}).get("client")
        external_id = (handle or {}).get("external_session_id") \
            or session.external_session_id
        if client is None or not external_id:
            return self.unsupported_receipt(
                "interrupt", "no_active_session", session=session)
        prompt_id = (handle or {}).get("current_prompt_id")
        try:
            if prompt_id:
                resp = await client.post(
                    f"/api/v1/sessions/{external_id}/prompts/"
                    f"{prompt_id}:abort")
                await self._unwrap(resp.json(), "abort")
            else:
                await client.post(f"/api/v1/sessions/{external_id}:abort")
        except Exception as exc:  # noqa: BLE001
            return self.unsupported_receipt(
                "interrupt", "abort_failed", session=session,
                detail={"detail": str(exc)[:200]})
        return CommandReceipt(
            command_id=new_id("cmd"),
            state=ReceiptState.COMPLETED,
            aggregate=AggregateRef(type="agent_session",
                                   id=session.agent_session_id),
        )

    async def _teardown(self, session: AgentSessionRef) -> str:
        handle = self._sessions.pop(session.agent_session_id, None)
        if not handle:
            return "closed"
        stop = handle.get("ws_stop")
        if stop is not None:
            stop.set()
        ws_task = handle.get("ws_task")
        if ws_task is not None:
            ws_task.cancel()
        client = handle.get("client")
        if client is not None:
            await client.aclose()
        returncode = await self._stop_server(handle.get("proc"))
        return classify_exit(
            returncode=returncode if handle.get("proc") is not None else None,
            cancelled=handle.get("current_turn_id") is not None,
            resume_handle=handle.get("external_session_id"),
        )


__all__ = [
    "DEFAULT_SERVER_PORT",
    "DEFAULT_TOKEN_FILE",
    "KimiAcpAdapter",
    "KimiLocalServerAdapter",
    "KimiLocalServerError",
    "default_kimi_binary",
    "resolve_server_token",
]
