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
import socket
from pathlib import Path
from typing import Any, AsyncIterator, Optional

import httpx

from muteki.platform.contracts.agent_events import (
    AgentNodePayload,
    AgentUpdatedPayload,
    ApprovalRequestedPayload,
    ApprovalResolvedPayload,
    FailureCategory,
    MessageCompletedPayload,
    MessageDeltaPayload,
    ReasoningPayload,
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
    SteerInput,
)
from muteki.platform.contracts.receipts import (
    AggregateRef,
    CommandReceipt,
    ReceiptState,
)

from .acp import AcpHello, AcpTransport, BaseAcpAdapter, check_response
from .approvals import ApprovalDecision, ApprovalScope
from .base import BaseExternalAgentAdapter
from .capabilities import (
    AccessModeUnsupportedError,
    BOOL_CAPABILITY_FIELDS,
    CapabilityProbeReport,
    SOURCE_PROBE,
    SOURCE_STATIC,
    conservative_capabilities,
    require_access_mode,
    _probe_version,
)
from .events import build_event
from .probe_environment import subprocess_environment
from .process_supervisor import SupervisedProcess, spawn_supervised
from .rpc import ProcessOutputLog
from .sessions import classify_exit

#: Stable codes. Callers branch on these, not on the message text.
KIMI_LOCAL_SERVER_UNAVAILABLE = "kimi.local_server.unavailable"
KIMI_LOCAL_SERVER_LAUNCH_FAILED = "kimi.local_server.launch_failed"


def default_kimi_binary() -> str:
    return os.environ.get("MUTEKI_KIMI_BIN", "kimi")


def _kimi_git_worktree_root(cwd: str) -> Optional[Path]:
    """Mirror Kimi's ``findGitWorkTree``: nearest ``.git`` dir or gitdir file."""
    try:
        current = Path(cwd).expanduser().resolve()
    except OSError:
        return None
    for directory in (current, *current.parents):
        marker = directory / ".git"
        try:
            if marker.is_dir():
                return directory
            if marker.is_file():
                first = marker.read_text(
                    encoding="utf-8", errors="replace"
                ).lstrip("\ufeff").lstrip().splitlines()
                if first and first[0].strip().startswith("gitdir:"):
                    return directory
        except OSError:
            continue
    return None


_KIMI_SUPERVISED_WORKTREE_REASON = (
    "Kimi's manual mode auto-approves Write/Edit inside a Git worktree "
    "(native git-cwd-write-approve policy), and Kimi (verified on 2.1.1) does "
    "not apply [[permission.rules]] ask rules that could override it, so "
    "supervised approval cannot be enforced here; choose auto-accept-edits, "
    "auto or full-access, or use a working directory outside Git"
)


def _require_kimi_supervised_enforceable(
    adapter_id: str, access_mode: Optional[str], cwd: str
) -> None:
    if access_mode != AccessMode.SUPERVISED.value:
        return
    root = _kimi_git_worktree_root(cwd)
    if root is None:
        return
    raise AccessModeUnsupportedError(
        adapter_id,
        access_mode,
        (
            AccessMode.AUTO_ACCEPT_EDITS.value,
            AccessMode.AUTO.value,
            AccessMode.FULL_ACCESS.value,
        ),
        f"{_KIMI_SUPERVISED_WORKTREE_REASON} (Git worktree: {root})",
    )


class KimiAcpAdapter(BaseAcpAdapter):
    """Kimi Code 的 ACP 结构化 Adapter。

    session new/load/list/prompt/cancel/approval、replay 区分与
    ``mcpServers`` ephemeral 注入全部由 ``BaseAcpAdapter`` 实现；
    本类只提供 argv、auth 选择与 Kimi 特有钩子（``session/set_model``、
    ``list_sessions``）。
    """

    adapter_id = "kimi.acp"
    # supervised/auto/full-access select Kimi's default/auto/yolo modes;
    # auto-accept-edits keeps ``default`` and the shared ACP callback allows
    # the ``edit`` kind Kimi reports for Write/Edit.
    supported_access_modes = (
        AccessMode.SUPERVISED.value,
        AccessMode.AUTO_ACCEPT_EDITS.value,
        AccessMode.AUTO.value,
        AccessMode.FULL_ACCESS.value,
    )

    def __init__(
        self,
        *,
        binary: Optional[str] = None,
        **kwargs: Any,
    ) -> None:
        super().__init__(**kwargs)
        self._binary = binary or default_kimi_binary()

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

    def _validate_access_mode(self, request: SessionStart, cwd: str) -> None:
        super()._validate_access_mode(request, cwd)
        _require_kimi_supervised_enforceable(self.id, request.access_mode, cwd)

    def _prepare_session_environment(
        self, request: SessionStart, env: dict[str, str], cwd: str
    ) -> dict[str, str]:
        del cwd
        if request.effort and request.effort != "default":
            env = {**env, "KIMI_MODEL_THINKING_EFFORT": str(request.effort)}
        return env

    async def _after_session_open(
        self, transport: AcpTransport, session_id: str, request: SessionStart
    ) -> None:
        access_mode = request.access_mode or AccessMode.SUPERVISED.value
        native_mode = {
            AccessMode.SUPERVISED.value: "default",
            # Kimi has no edit-only mode: its manual mode asks, and the shared
            # ACP permission callback allows the ``edit`` kind.
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
        transport = AcpTransport(self._with_launch_args(self._agent_argv()))
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

#: ``kimi web`` 自身的默认端口。端口被占用时 kimi 会静默换到下一个端口
#: （2.1.1 实测），所以自管 server 不用它，见 ``_free_local_port``。
DEFAULT_SERVER_PORT = 58627


def _free_local_port() -> int:
    """A currently unused loopback port for a Muteki-owned ``kimi web``."""
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as probe:
        probe.bind(("127.0.0.1", 0))
        return int(probe.getsockname()[1])


#: bearer token 的默认持久化位置（kimi web 首启生成，0600）。
DEFAULT_TOKEN_FILE = Path.home() / ".kimi-code" / "server.token"


class KimiLocalServerError(RuntimeError):
    """Kimi Local Server 调用失败（传输错误或业务 code 非成功）。

    ``code`` 是稳定机器码（例如 ``kimi.local_server.unavailable``）。
    启动失败必须带 code；本适配器不因此改走 ``kimi.acp``。
    """

    def __init__(self, message: str, *, code: str = "") -> None:
        self.code = code
        super().__init__(f"[{code}] {message}" if code else message)


def resolve_server_token(explicit: str = "", *, home: str = "") -> str:
    """bearer token 引用解析：显式参数 > 环境变量 > 实例 token 文件。

    ``home`` 是该 kimi 进程的 ``KIMI_CODE_HOME``：``kimi web`` 把 token 写在
    它自己的 home 下，不是固定的 ``~/.kimi-code``。返回 token 本体。
    """
    if explicit:
        return explicit
    from_env = os.environ.get("KIMI_CODE_SERVER_TOKEN", "").strip()
    if from_env:
        return from_env
    token_file = (
        Path(home).expanduser() / "server.token" if home else DEFAULT_TOKEN_FILE)
    try:
        return token_file.read_text(encoding="utf-8").strip()
    except OSError:
        return ""


#: Conversation access mode -> Kimi ``permission_mode`` sent with each prompt.
#: auto-accept-edits keeps ``manual`` and Muteki answers Write/Edit approvals.
_KIMI_SERVER_PERMISSION_MODES = {
    AccessMode.SUPERVISED.value: "manual",
    AccessMode.AUTO_ACCEPT_EDITS.value: "manual",
    AccessMode.AUTO.value: "auto",
    AccessMode.FULL_ACCESS.value: "yolo",
}
#: Kimi tool names whose approvals auto-accept-edits answers (the same tools
#: Kimi's ACP server reports with the ``edit`` kind).
_KIMI_EDIT_TOOLS = frozenset({"Write", "Edit"})
#: Kimi Local Server envelope codes for approvals that are no longer pending.
_KIMI_APPROVAL_GONE_CODES = {40404, 40902}


def _openapi_request_properties(
    openapi: dict[str, Any], path_suffix: str, method: str = "post"
) -> Optional[set[str]]:
    """Property names of a JSON request body in this instance's OpenAPI.

    Returns ``None`` when the operation or its schema cannot be located, so
    callers can refuse instead of guessing that a field is accepted.
    """
    paths = openapi.get("paths") if isinstance(openapi.get("paths"), dict) else {}
    operation = next((
        item.get(method) for path, item in paths.items()
        if str(path).endswith(path_suffix) and isinstance(item, dict)
        and isinstance(item.get(method), dict)
    ), None)
    if operation is None:
        return None
    content = ((operation.get("requestBody") or {}).get("content") or {})
    schema = (content.get("application/json") or {}).get("schema")
    components = (openapi.get("components") or {}).get("schemas") or {}

    def collect(node: Any, depth: int = 0) -> Optional[set[str]]:
        if not isinstance(node, dict) or depth > 8:
            return None
        ref = node.get("$ref")
        if isinstance(ref, str) and ref.startswith("#/components/schemas/"):
            return collect(components.get(ref.rsplit("/", 1)[-1]), depth + 1)
        names: set[str] = set()
        found = False
        if isinstance(node.get("properties"), dict):
            names.update(str(key) for key in node["properties"])
            found = True
        for key in ("allOf", "anyOf", "oneOf"):
            for child in node.get(key) or []:
                nested = collect(child, depth + 1)
                if nested is not None:
                    names.update(nested)
                    found = True
        return names if found else None

    return collect(schema)


def _kimi_usage_payload(usage: dict[str, Any]) -> dict[str, Any]:
    """Kimi Local Server usage → normalized contract.

    实测字段（0.38.0）：``inputOther`` / ``output`` / ``inputCacheRead`` /
    ``inputCacheCreation``，input 与其他桶互斥，归一化 ``input_tokens``
    补回缓存桶。
    """
    def _num(value: Any) -> Optional[int]:
        return value if isinstance(value, int) and not isinstance(value, bool) and value >= 0 else None
    other = _num(usage.get("inputOther"))
    read = _num(usage.get("inputCacheRead"))
    write = _num(usage.get("inputCacheCreation"))
    input_tokens = None
    if any(value is not None for value in (other, read, write)):
        input_tokens = (other or 0) + (read or 0) + (write or 0)
    return dump_payload(UsagePayload(
        scope="turn",
        input_tokens=input_tokens,
        output_tokens=_num(usage.get("output")),
        cached_input_tokens=read,
        cache_write_tokens=write,
        native=dict(usage),
    ))


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
        log_root: Optional[str | Path] = None,
        **kwargs: Any,
    ) -> None:
        super().__init__(self.adapter_id, **kwargs)
        self._binary = binary or default_kimi_binary()
        self._log_root = Path(log_root) if log_root is not None else None
        self._server_outputs: dict[int, ProcessOutputLog] = {}
        self._supervised: dict[int, SupervisedProcess] = {}
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
        return [self._binary, "web", "--no-open", "--port", str(port)]

    def _server_env(
        self, session_env: Optional[dict[str, str]] = None
    ) -> dict[str, str]:
        return {**self._extra_env, **dict(session_env or {})}

    async def _start_server(
        self, cwd: str, *, env: Optional[dict[str, str]] = None
    ) -> "tuple[Optional[asyncio.subprocess.Process], str, str]":
        """返回 (进程, base_url, token)。attach 模式不拉起进程。"""
        server_env = self._server_env(env)
        token = resolve_server_token(
            self._token, home=server_env.get("KIMI_CODE_HOME", ""))
        if self._attach_url:
            return None, self._attach_url.rstrip("/"), token
        if not self._manage_server:
            raise KimiLocalServerError(
                "manage_server=False 时必须提供 base_url",
                code=KIMI_LOCAL_SERVER_LAUNCH_FAILED,
            )
        port = self._port or _free_local_port()
        base_url = f"http://127.0.0.1:{port}"
        output = ProcessOutputLog.create(
            self._log_root, label=f"kimi-web-{port}")
        try:
            supervised = await spawn_supervised(
                self._with_launch_args(self._serve_argv(port)),
                adapter_id=self.adapter_id,
                label=f"kimi-web-{port}",
                cwd=cwd or None,
                env=subprocess_environment(server_env),
                stdin=asyncio.subprocess.DEVNULL,
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE,
            )
        except Exception as exc:
            await output.close()
            raise KimiLocalServerError(
                f"kimi web 无法启动：{exc}",
                code=KIMI_LOCAL_SERVER_LAUNCH_FAILED,
            ) from exc
        except BaseException:
            await output.close()
            raise
        proc = supervised.process
        self._supervised[proc.pid] = supervised
        output.attach(proc)
        self._server_outputs[proc.pid] = output
        try:
            # 首启会生成 token 文件，等服务可用后再解析一次。
            deadline = asyncio.get_running_loop().time() + self._startup_timeout
            while True:
                if proc.returncode is not None:
                    raise KimiLocalServerError(
                        f"kimi web 提前退出（code={proc.returncode}）；"
                        f"{output.detail()}",
                        code=KIMI_LOCAL_SERVER_LAUNCH_FAILED,
                    )
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
                    raise KimiLocalServerError(
                        "kimi web 的 HTTP API 没有在超时内就绪（healthz 不可达）；"
                        f"{output.detail()}",
                        code=KIMI_LOCAL_SERVER_UNAVAILABLE,
                    )
                await asyncio.sleep(0.3)
        except BaseException:
            await self._stop_server(proc)
            raise
        if not token:
            token = resolve_server_token(
                "", home=server_env.get("KIMI_CODE_HOME", ""))
        return proc, base_url, token

    def server_output_detail(
        self, proc: Optional[asyncio.subprocess.Process]
    ) -> str:
        output = self._server_outputs.get(proc.pid) if proc is not None else None
        return output.detail() if output is not None else ""

    async def _stop_server(
        self, proc: Optional[asyncio.subprocess.Process]
    ) -> int:
        if proc is None:
            return -1
        supervised = self._supervised.pop(proc.pid, None)
        try:
            if supervised is not None:
                await supervised.terminate()
            elif proc.returncode is None:
                try:
                    proc.terminate()
                    await asyncio.wait_for(proc.wait(), timeout=5.0)
                except (asyncio.TimeoutError, ProcessLookupError):
                    try:
                        proc.kill()
                    except ProcessLookupError:
                        pass
                    await proc.wait()
        finally:
            output = self._server_outputs.pop(proc.pid, None)
            if output is not None:
                await output.close()
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
                f"{what} 失败：code={code} msg={payload.get('msg')}")
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
                detail = f"kimi web 探测失败：{exc}"
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
        caps.protocol_version = str(openapi.get("openapi") or "")
        if isinstance(meta, dict) and meta.get("version"):
            caps.runtime_version = str(meta["version"])
        caps.resume = has("/api/v1/sessions")
        caps.resume_continues_turn = False
        caps.session_persistence = caps.resume
        caps.interrupt = ws_available
        caps.streaming = ws_available
        caps.tool_events = ws_available
        caps.usage_events = ws_available
        caps.approval = ws_available and any("approvals" in p for p in paths)
        prompt_fields = _openapi_request_properties(
            openapi, "/sessions/{session_id}/prompts") or set()
        # Every mode needs the per-prompt native permission_mode; supervised
        # is additionally refused at launch inside a Git worktree.
        caps.access_modes = (
            list(_KIMI_SERVER_PERMISSION_MODES)
            if caps.approval and "permission_mode" in prompt_fields else []
        )
        caps.user_input = ws_available and any("questions" in p for p in paths)
        caps.steer = ws_available and any(
            p.endswith("/prompts:steer")
            or ("/prompts/{" in p and p.endswith(":steer"))
            for p in paths
        )
        # Agent 工具派生的子智能体与主 agent 共用 WS 流（帧上 agentId 区分，
        # subagent.spawned/completed 给出生命周期；0.38 实测）。
        caps.subagents = ws_available
        for field_name in BOOL_CAPABILITY_FIELDS:
            field_sources[field_name] = SOURCE_PROBE
        if not ws_available:
            degradations.append(
                "websockets 包不可用：WS 事件流/中断/审批未启用，send 将"
                "返回 unsupported（experimental 接口不伪造完成事件）")
        if not caps.access_modes:
            degradations.append(
                "该实例 OpenAPI 未声明 prompt permission_mode 或审批端点："
                "Conversation access mode 无法映射，会话启动时将拒绝")
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
        cwd = request.options.cwd or os.getcwd()
        access_mode = str(request.access_mode or "").strip() or None
        require_access_mode(
            self.id, access_mode, tuple(_KIMI_SERVER_PERMISSION_MODES))
        _require_kimi_supervised_enforceable(self.id, access_mode, cwd)
        session_env = dict(request.options.env)
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
                    "拒绝按猜测路径启动会话",
                    code=KIMI_LOCAL_SERVER_UNAVAILABLE,
                )
            if access_mode is not None:
                prompt_fields = _openapi_request_properties(
                    openapi, "/sessions/{session_id}/prompts")
                if prompt_fields is None or "permission_mode" not in prompt_fields:
                    raise AccessModeUnsupportedError(
                        self.id, access_mode, (),
                        "this Kimi Local Server's OpenAPI does not declare "
                        "permission_mode on POST /sessions/{session_id}/prompts, "
                        "so the native permission mode cannot be selected",
                    )

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
        except BaseException:
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
            "options": request.options,
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
            # None: Worker launch without a Conversation access mode; Kimi's
            # approvals keep being answered unattended as before.
            "access_mode": access_mode,
            "permission_mode": (
                _KIMI_SERVER_PERMISSION_MODES[access_mode]
                if access_mode is not None else None
            ),
            # approval_id -> {"native": request, "agent_id": ..., "responding": bool}
            "pending_approvals": {},
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
                                    dump_payload(RuntimeWarningPayload(
                                        kind="protocol",
                                        message=(
                                            "Kimi WS cursor invalidated; "
                                            "resubscribed at the reported "
                                            "watermark"),
                                        code="kimi.ws.resync_required",
                                        native={"reason": payload.get("reason")},
                                    ))))
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
        # 同一 session 的 WS 流同时承载主 agent（agentId=main）与子智能体
        # （agent-N）的事件；子智能体的 turn/usage 不能推进主回合。
        owner = str(data.get("agentId") or data.get("agent_id") or "")
        if owner and owner != "main":
            self._handle_subagent_frame(handle, sink, ftype, owner, data)
            return
        if ftype.startswith("subagent."):
            self._handle_subagent_lifecycle(handle, sink, ftype, data)
            return
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
                                 f"kimi.{ftype}",
                                 dump_payload(MessageDeltaPayload(text=text))))
        elif ftype == "thinking.delta":
            text = str(data.get("delta") or data.get("text") or "")
            if text:
                sink.put_nowait((AgentEventType.REASONING_SUMMARY,
                                 "kimi.thinking.delta",
                                 dump_payload(ReasoningPayload(
                                     text=text, channel="thinking",
                                     partial=True))))
        elif ftype == "tool.call.started":
            payload = self._tool_started_payload(data)
            display = data.get("display") if isinstance(
                data.get("display"), dict) else {}
            is_agent_call = display.get("kind") == "agent_call"
            if is_agent_call:
                args = payload.get("input") if isinstance(
                    payload.get("input"), dict) else {}
                handle.setdefault("agent_call_requests", {})[
                    payload["call_id"]] = str(
                        args.get("prompt") or display.get("prompt") or "")
            sink.put_nowait((AgentEventType.TOOL_STARTED,
                             "kimi.tool.call.started",
                             dump_payload(ToolPayload(
                                 tool_call_id=payload["call_id"],
                                 name=payload.get("tool") or None,
                                 input=payload.get("input"),
                                 status="running",
                                 kind="agent" if is_agent_call else None,
                             ))))
        elif ftype == "tool.result":
            raw = self._tool_result_payload(data)
            sink.put_nowait((AgentEventType.TOOL_COMPLETED,
                             "kimi.tool.result",
                             dump_payload(ToolPayload(
                                 tool_call_id=raw["call_id"],
                                 output=raw.get("output"),
                                 status="failed" if raw.get("is_error")
                                 else "completed",
                             ))))
        elif ftype == "event.approval.requested":
            self._emit_approval(handle, sink, data, None)
        elif ftype == "event.approval.resolved":
            self._handle_approval_resolved(handle, sink, data, None)
        elif ftype in ("usage", "turn.step.completed"):
            # 0.38.0 实测 usage 在 turn.step.completed
            # （{inputOther, output, inputCacheRead, inputCacheCreation}）。
            usage = data.get("usage") if ftype == "turn.step.completed" \
                else data
            if isinstance(usage, dict) and usage:
                sink.put_nowait((AgentEventType.USAGE_UPDATED,
                                 f"kimi.{ftype}", _kimi_usage_payload(usage)))
        # 其余事件（task.*/compaction.*/tool.call.delta/
        # agent.status.updated 等）不改变核心状态机。

    # -- 子智能体 -------------------------------------------------------------

    @staticmethod
    def _tool_started_payload(data: dict[str, Any]) -> dict[str, Any]:
        return {
            "call_id": str(data.get("toolCallId") or data.get("call_id")
                           or data.get("id") or ""),
            "tool": str(data.get("name") or data.get("tool") or ""),
            "input": data.get("args") or data.get("arguments"),
        }

    @staticmethod
    def _tool_result_payload(data: dict[str, Any]) -> dict[str, Any]:
        return {
            "call_id": str(data.get("toolCallId") or data.get("call_id")
                           or data.get("id") or ""),
            "output": data.get("output") or data.get("content"),
            "is_error": bool(data.get("is_error")),
        }

    def _emit_approval(
        self, handle: dict[str, Any], sink: Any, data: dict[str, Any],
        agent_id: Optional[str],
    ) -> None:
        """Route a Kimi approval by the session's access mode.

        Kimi's own ``permission_mode`` already decided that this call needs
        approval.  full-access and Worker sessions (no access mode) answer it
        unattended, auto-accept-edits answers Write/Edit, and everything else
        becomes a Conversation approval card answered via ``send``.
        """
        approval_id = str(data.get("approval_id") or data.get("id") or "")
        tool_name = str(data.get("tool_name") or "")
        access_mode = handle.get("access_mode")
        if not approval_id:
            sink.put_nowait((AgentEventType.RUNTIME_ERROR,
                             "kimi.approval.invalid",
                             dump_payload(RuntimeErrorPayload(
                                 error=self.failure(
                                     FailureCategory.VALIDATION,
                                     "approval.missing_id",
                                     message="Kimi approval request has no approval_id"),
                                 native={"data": data}))))
            return
        policy = ""
        if access_mode is None:
            policy = "unattended"
        elif access_mode == AccessMode.FULL_ACCESS.value:
            policy = AccessMode.FULL_ACCESS.value
        elif (access_mode == AccessMode.AUTO_ACCEPT_EDITS.value
              and tool_name in _KIMI_EDIT_TOOLS):
            policy = AccessMode.AUTO_ACCEPT_EDITS.value
        if policy:
            asyncio.ensure_future(self._auto_approve(
                handle, sink, approval_id, tool_name, agent_id, policy))
            return
        handle["pending_approvals"][approval_id] = {
            "native": dict(data),
            "agent_id": agent_id,
            "responding": False,
        }
        approval_kind = (
            "file_change" if tool_name in _KIMI_EDIT_TOOLS
            else "command_execution" if tool_name in {"Bash", "Shell"}
            else "tool"
        )
        sink.put_nowait((AgentEventType.APPROVAL_REQUESTED,
                         "kimi.approval.requested",
                         dump_payload(ApprovalRequestedPayload(
                             approval_id=approval_id,
                             agent_id=agent_id,
                             approval_kind=approval_kind,
                             title=str(
                                 data.get("action") or tool_name
                                 or "Kimi operation"),
                             tool_name=tool_name or None,
                             native={"access_mode": access_mode, "data": data},
                         ))))

    def _handle_approval_resolved(
        self, handle: dict[str, Any], sink: Any, data: dict[str, Any],
        agent_id: Optional[str],
    ) -> None:
        """Kimi resolved a pending approval itself (expiry, another client)."""
        approval_id = str(data.get("approval_id") or "")
        pending = handle["pending_approvals"].get(approval_id)
        # Resolutions Muteki sent are reported by the sending path.
        if pending is None or pending.get("responding"):
            return
        handle["pending_approvals"].pop(approval_id, None)
        native_decision = str(data.get("decision") or "")
        sink.put_nowait((AgentEventType.APPROVAL_RESOLVED,
                         "kimi.approval.resolved",
                         dump_payload(ApprovalResolvedPayload(
                             approval_id=approval_id,
                             agent_id=agent_id,
                             decision=(
                                 "allow" if native_decision == "approved"
                                 else "deny"),
                             scope=(
                                 str(data["scope"])
                                 if data.get("scope") is not None else None),
                             automatic=True,
                             native={"native_decision": native_decision,
                                     "data": data},
                         ))))

    def _agent_patch(
        self, handle: dict[str, Any], sink: Any, agent_id: str,
        update: dict[str, Any],
    ) -> None:
        nodes = handle.setdefault("agent_nodes", {})
        previous = nodes.get(agent_id) or {
            "agent_id": agent_id, "title": agent_id, "status": "running"}
        node = {**previous, **{k: v for k, v in update.items()
                               if v is not None}}
        if node == previous and agent_id in nodes:
            return
        nodes[agent_id] = node
        sink.put_nowait((AgentEventType.AGENT_UPDATED,
                         "kimi.subagent.updated",
                         dump_payload(AgentUpdatedPayload(
                             agents=[AgentNodePayload(**node)], patch=True))))

    def _flush_agent_text(
        self, handle: dict[str, Any], sink: Any, agent_id: str
    ) -> None:
        buffers = handle.setdefault("agent_text", {})
        text = str(buffers.get(agent_id) or "").strip()
        if text:
            self._agent_patch(handle, sink, agent_id, {"activity": text})

    def _handle_subagent_lifecycle(
        self, handle: dict[str, Any], sink: Any, ftype: str,
        data: dict[str, Any],
    ) -> None:
        """主 agent 侧的 ``subagent.*`` 生命周期帧（0.38 实测字段）。"""
        agent_id = str(data.get("subagentId") or "")
        if not agent_id:
            return
        if ftype == "subagent.spawned":
            call_id = str(data.get("parentToolCallId") or "")
            parent = str(data.get("parentAgentId") or "")
            self._agent_patch(handle, sink, agent_id, {
                "status": "running",
                "title": str(data.get("description") or "").strip() or None,
                "role": str(data.get("subagentName") or "").strip() or None,
                "model": str(data.get("model") or "").strip() or None,
                "call_id": call_id or None,
                "parent_id": parent if parent and parent != "main" else None,
                "request": (handle.get("agent_call_requests") or {}).get(
                    call_id) or None,
                "session_ref": agent_id,
            })
            handle.setdefault("agent_spawned_at", {})[agent_id] = data.get(
                "time")
        elif ftype == "subagent.started":
            self._agent_patch(handle, sink, agent_id, {"status": "running"})
        elif ftype in ("subagent.completed", "subagent.failed",
                       "subagent.cancelled"):
            self._flush_agent_text(handle, sink, agent_id)
            usage = data.get("usage") if isinstance(
                data.get("usage"), dict) else {}
            total = sum(int(usage.get(key) or 0) for key in (
                "inputOther", "output", "inputCacheRead",
                "inputCacheCreation"))
            started = (handle.get("agent_spawned_at") or {}).get(agent_id)
            ended = data.get("time")
            duration = (int(ended) - int(started)
                        if isinstance(started, int) and isinstance(ended, int)
                        else None)
            node = (handle.get("agent_nodes") or {}).get(agent_id) or {}
            error = data.get("error")
            if isinstance(error, dict):
                error = error.get("message") or error.get("code")
            status = {"subagent.completed": "completed",
                      "subagent.failed": "failed",
                      "subagent.cancelled": "cancelled"}[ftype]
            self._agent_patch(handle, sink, agent_id, {
                "status": status,
                "result": str(data.get("resultSummary") or "").strip()
                or None,
                "error": str(error) if error else None,
                "total_tokens": total or None,
                "tool_uses": node.get("tool_uses"),
                "duration_ms": duration,
            })

    def _handle_subagent_frame(
        self, handle: dict[str, Any], sink: Any, ftype: str, agent_id: str,
        data: dict[str, Any],
    ) -> None:
        """子智能体自身的事件：工具/审批带 agent_id，文本汇成节点活动。"""
        if ftype == "tool.call.started":
            self._flush_agent_text(handle, sink, agent_id)
            handle.setdefault("agent_text", {})[agent_id] = ""
            payload = self._tool_started_payload(data)
            sink.put_nowait((AgentEventType.TOOL_STARTED,
                             "kimi.tool.call.started",
                             dump_payload(ToolPayload(
                                 tool_call_id=payload["call_id"],
                                 name=payload.get("tool") or None,
                                 input=payload.get("input"),
                                 status="running",
                                 agent_id=agent_id,
                             ))))
            node = (handle.get("agent_nodes") or {}).get(agent_id) or {}
            self._agent_patch(handle, sink, agent_id, {
                "tool_uses": int(node.get("tool_uses") or 0) + 1})
        elif ftype == "tool.result":
            payload = self._tool_result_payload(data)
            sink.put_nowait((AgentEventType.TOOL_COMPLETED,
                             "kimi.tool.result",
                             dump_payload(ToolPayload(
                                 tool_call_id=payload["call_id"],
                                 output=payload.get("output"),
                                 status="failed" if payload.get("is_error")
                                 else "completed",
                                 agent_id=agent_id,
                             ))))
        elif ftype == "event.approval.requested":
            self._emit_approval(handle, sink, data, agent_id)
        elif ftype == "event.approval.resolved":
            self._handle_approval_resolved(handle, sink, data, agent_id)
        elif ftype in ("assistant.delta", "assistant.message"):
            text = str(data.get("delta") or data.get("text") or "")
            if text:
                buffers = handle.setdefault("agent_text", {})
                buffers[agent_id] = (
                    text if ftype == "assistant.message"
                    else str(buffers.get(agent_id) or "") + text)
        elif ftype in ("turn.step.completed", "turn.ended"):
            self._flush_agent_text(handle, sink, agent_id)
        # 子智能体的 thinking/usage/status 帧不进入主回合；用量在
        # subagent.completed 汇总。

    async def _post_approval_decision(
        self, handle: dict[str, Any], approval_id: str, body: dict[str, Any]
    ) -> "tuple[Optional[Any], Optional[dict[str, Any]]]":
        """POST one decision; returns ``(native_code, None)`` or ``(None, failure)``.

        ``native_code`` is the envelope ``code`` (0 on success).  Transport or
        malformed-response failures return a typed :class:`AgentFailure`.
        """
        client: httpx.AsyncClient = handle["client"]
        session_id = handle["external_session_id"]
        try:
            resp = await client.post(
                f"/api/v1/sessions/{session_id}/approvals/{approval_id}",
                json=body)
            envelope = resp.json()
        except (httpx.HTTPError, ValueError) as exc:
            return None, self.exception_failure(
                exc, FailureCategory.TRANSPORT, "approval.reply_failed",
                message=f"Kimi approval reply failed: {exc}")
        if not isinstance(envelope, dict) or "code" not in envelope:
            return None, self.failure(
                FailureCategory.TRANSPORT, "approval.reply_failed",
                message=f"unexpected response (HTTP {resp.status_code})",
                detail=json.dumps(envelope, ensure_ascii=False, default=str))
        return envelope.get("code"), (
            None if envelope.get("code") in (0, "0") else self.failure(
                FailureCategory.UNKNOWN, "approval.reply_rejected",
                message=str(envelope.get("msg") or "Kimi rejected the decision"),
                native_code=str(envelope.get("code")),
            ))

    async def _auto_approve(
        self, handle: dict[str, Any], sink: Any, approval_id: str,
        tool_name: str, agent_id: Optional[str], policy: str,
    ) -> None:
        """Answer an approval the session's access mode allows without a user."""
        _code, error = await self._post_approval_decision(
            handle, approval_id, {"decision": "approved"})
        if error is not None:
            sink.put_nowait((AgentEventType.RUNTIME_ERROR,
                             "kimi.approval.auto_reply_failed",
                             dump_payload(RuntimeErrorPayload(
                                 error=error,
                                 native={
                                     "approval_id": approval_id,
                                     "tool": tool_name,
                                     "policy": policy,
                                 }))))
            return
        sink.put_nowait((AgentEventType.APPROVAL_RESOLVED,
                         "kimi.approval.auto_approved",
                         dump_payload(ApprovalResolvedPayload(
                             approval_id=approval_id,
                             agent_id=agent_id,
                             decision="allow",
                             scope="once",
                             automatic=True,
                             native={"resolved_by": policy,
                                     "tool": tool_name,
                                     "native_decision": "approved"},
                         ))))

    async def _approval_response_stream(
        self, session: AgentSessionRef, input: ApprovalResponseInput
    ) -> AsyncIterator[AgentEvent]:
        sid = session.agent_session_id
        seq = self.sequencer_for(sid)
        handle = self._sessions.get(sid)
        common = dict(
            agent_session_id=sid,
            external_session_id=(handle or {}).get("external_session_id")
            or session.external_session_id,
            turn_id=(handle or {}).get("current_turn_id"),
        )
        try:
            decision = ApprovalDecision.from_payload(input.payload.model_dump())
        except ValueError as exc:
            yield self.emit(build_event(
                AgentEventType.RUNTIME_ERROR, seq,
                native_type="kimi.approval.invalid",
                payload=RuntimeErrorPayload(error=self.exception_failure(
                    exc, FailureCategory.VALIDATION, "approval.invalid",
                    message=f"Invalid approval response: {exc}")),
                **common))
            return
        pending = (
            (handle.get("pending_approvals") or {}).get(decision.approval_id)
            if handle is not None and handle.get("client") is not None
            else None
        )
        if pending is None:
            yield self.emit(build_event(
                AgentEventType.RUNTIME_ERROR, seq,
                native_type="kimi.approval.stale",
                payload=RuntimeErrorPayload(
                    error=self.failure(
                        FailureCategory.UNKNOWN, "approval.stale",
                        message="Kimi approval is no longer pending"),
                    native={"approval_id": decision.approval_id}),
                **common))
            return
        if pending.get("responding"):
            yield self.emit(build_event(
                AgentEventType.RUNTIME_ERROR, seq,
                native_type="kimi.approval.in_flight",
                payload=RuntimeErrorPayload(
                    error=self.failure(
                        FailureCategory.UNKNOWN, "approval.in_flight",
                        message="a decision for this approval is already being sent"),
                    native={"approval_id": decision.approval_id}),
                **common))
            return
        body: dict[str, Any] = {
            "decision": "approved" if decision.allowed else "rejected",
        }
        if decision.allowed and decision.scope is ApprovalScope.SESSION:
            body["scope"] = "session"
        if decision.note:
            body["feedback"] = decision.note
        pending["responding"] = True
        native_code, error = await self._post_approval_decision(
            handle, decision.approval_id, body)
        if error is not None:
            if native_code in _KIMI_APPROVAL_GONE_CODES:
                handle["pending_approvals"].pop(decision.approval_id, None)
                error = self.failure(
                    FailureCategory.UNKNOWN, "approval.stale",
                    message="Kimi approval is no longer pending",
                    detail=error.detail, native_code=error.native_code)
            else:
                pending["responding"] = False
            yield self.emit(build_event(
                AgentEventType.RUNTIME_ERROR, seq,
                native_type="kimi.approval.reply_failed",
                payload=RuntimeErrorPayload(
                    error=error,
                    native={"approval_id": decision.approval_id}),
                **common))
            return
        handle["pending_approvals"].pop(decision.approval_id, None)
        yield self.emit(build_event(
            AgentEventType.APPROVAL_RESOLVED, seq,
            native_type="kimi.approval.replied",
            payload=ApprovalResolvedPayload(
                approval_id=decision.approval_id,
                agent_id=(
                    str(pending["agent_id"]) if pending.get("agent_id") else None),
                decision="allow" if decision.allowed else "deny",
                scope=decision.scope.value,
                native={"native_decision": body["decision"]},
            ),
            **common))

    # -- turn 流 --------------------------------------------------------------

    def send(
        self, session: AgentSessionRef, input: AgentInput
    ) -> AsyncIterator[AgentEvent]:
        if isinstance(input, ApprovalResponseInput):
            return self._approval_response_stream(session, input)
        if isinstance(input, MessageInput):
            return self._turn_stream(session, input)
        return self.unsupported_input_stream(session, input)

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
        self, session: AgentSessionRef, input: MessageInput
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
                payload=SessionPayload(
                    transport="http+ws",
                    adapter_id=self.id,
                    instance_id=self.identity.instance_id,
                    cwd=handle["cwd"],
                    native={"experimental": True}),
                **common))

        if not await self._ensure_ws(handle):
            yield self.emit(build_event(
                AgentEventType.RUNTIME_ERROR, seq,
                external_session_id=external_id,
                native_type="kimi.ws.unavailable",
                payload=RuntimeErrorPayload(error=self.failure(
                    FailureCategory.UNSUPPORTED, "ws_unavailable",
                    message="websockets 不可用，experimental Local "
                            "Server 事件流未启用（不伪造完成事件）")),
                **common))
            return

        turn_id = new_id("turn")
        handle["current_turn_id"] = turn_id
        yield self.emit(build_event(
            AgentEventType.TURN_STARTED, seq,
            external_session_id=external_id, turn_id=turn_id,
            native_type="kimi.prompt.start",
            payload=TurnStartedPayload(kind=input.kind),
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
                if handle.get("permission_mode"):
                    body["permission_mode"] = handle["permission_mode"]
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
                                f"msg={body.get('msg')}")
                            break
                if get_task in done:
                    etype, native_type, payload = get_task.result()
                    get_task = asyncio.ensure_future(queue.get())
                    if etype == "__turn_done__":
                        done_info = dict(payload)
                        break
                    if (etype is AgentEventType.MESSAGE_DELTA
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
            server_exit: dict[str, Any] = {}
            server_proc = handle.get("proc")
            if server_proc is not None and server_proc.returncode is not None:
                server_exit = {
                    "returncode": server_proc.returncode,
                    "output": self.server_output_detail(server_proc),
                }
            yield self.emit(build_event(
                AgentEventType.TURN_FAILED, seq,
                external_session_id=external_id, turn_id=turn_id,
                native_type="kimi.prompt.error",
                payload=TurnFailedPayload(
                    error=self.exception_failure(
                        submit_error, FailureCategory.TRANSPORT,
                        "prompt_error",
                        message=f"Kimi prompt failed: {submit_error}"),
                    native={"server_exit": server_exit} if server_exit else {}),
                **common))
            return
        stop_reason = str(done_info.get("stop_reason") or "end_turn")
        done_error = done_info.get("error")
        if stop_reason in ("cancelled", "aborted", "interrupted"):
            yield self.emit(build_event(
                AgentEventType.TURN_FAILED, seq,
                external_session_id=external_id, turn_id=turn_id,
                native_type="kimi.prompt.aborted",
                payload=TurnFailedPayload(
                    error=self.failure(
                        FailureCategory.CANCELLED, "interrupted",
                        message=f"Kimi turn {stop_reason}"),
                    native={"stop_reason": stop_reason}),
                **common))
            return
        if stop_reason in ("failed", "error"):
            # turn.ended reason=failed（如 model.not_configured）如实上报。
            yield self.emit(build_event(
                AgentEventType.TURN_FAILED, seq,
                external_session_id=external_id, turn_id=turn_id,
                native_type="kimi.turn.failed",
                payload=TurnFailedPayload(
                    error=self.failure(
                        FailureCategory.PROVIDER, "turn_failed",
                        message=str(done_error or f"stopReason={stop_reason}"),
                        detail=str(done_error or "")),
                    native={"stop_reason": stop_reason}),
                **common))
            return
        handle.pop("saw_assistant_text", False)
        final_text = "".join(handle.pop("assistant_text_parts", []))
        for event in self.completed_turn_events(
            seq, text=final_text,
            common={**common, "external_session_id": external_id, "turn_id": turn_id},
            native_type="kimi.turn.ended",
            payload=TurnCompletedPayload(stop_reason=stop_reason)):
            yield event

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
            payload=SessionPayload(
                transport="http+ws",
                native={"cursor": dict(handle.get("cursor") or {}),
                        "experimental": True}),
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
        if not isinstance(input, SteerInput):
            return self.unsupported_receipt(
                "steer", "steer", session=session, detail={"input_kind": input.kind})
        handle = self._sessions.get(session.agent_session_id)
        client = (handle or {}).get("client")
        external_id = (handle or {}).get("external_session_id") \
            or session.external_session_id
        paths = (handle or {}).get("openapi_paths") or {}
        declared = [str(path) for path in paths]
        # 2.1.1 OpenAPI is POST /prompts:steer with {prompt_ids}. The older
        # /prompts/{prompt_id}:steer shape is used only when that is what
        # this instance actually declares.
        session_steer = any(path.endswith("/prompts:steer") for path in declared)
        id_steer = any(
            "/prompts/{" in path and path.endswith(":steer") for path in declared
        )
        if client is None or not external_id or not (session_steer or id_steer):
            return self.unsupported_receipt(
                "steer", "steer", session=session,
                detail={"detail": "该 Kimi Local Server 实例未声明 prompt steer",
                        "code": KIMI_LOCAL_SERVER_UNAVAILABLE},
            )
        # Kimi 的 prompt id 是服务端资源标识，独立于前端消息幂等 id。
        # 统一使用本地生成的 prompt id，避免把不同客户端的 id 格式带进协议。
        prompt_id = new_id("prompt")
        try:
            steer_body: dict[str, Any] = {
                "prompt_id": prompt_id,
                "content": [{"type": "text", "text": input.text}],
            }
            if (handle or {}).get("permission_mode"):
                steer_body["permission_mode"] = handle["permission_mode"]
            submitted = await self._unwrap((await client.post(
                f"/api/v1/sessions/{external_id}/prompts",
                json=steer_body,
            )).json(), "steer prompt submit")
            actual_prompt_id = str(
                (submitted or {}).get("prompt_id") or prompt_id
            )
            if session_steer:
                steer_body_ids = {"prompt_ids": [actual_prompt_id]}
                await self._unwrap((await client.post(
                    f"/api/v1/sessions/{external_id}/prompts:steer",
                    json=steer_body_ids,
                )).json(), "steer prompt")
            else:
                await self._unwrap((await client.post(
                    f"/api/v1/sessions/{external_id}/prompts/"
                    f"{actual_prompt_id}:steer",
                    json={},
                )).json(), "steer prompt")
        except Exception as exc:  # noqa: BLE001
            return self.unsupported_receipt(
                "steer", "steer_rejected", session=session,
                detail={"detail": str(exc)},
            )
        return CommandReceipt(
            command_id=new_id("cmd"),
            state=ReceiptState.COMPLETED,
            aggregate=AggregateRef(
                type="agent_session", id=session.agent_session_id
            ),
        )

    async def interrupt(self, session: AgentSessionRef) -> CommandReceipt:
        self._mark_turn_interrupted(session.agent_session_id)
        """中断当前 prompt。

        2.1.1 把 abort 声明成 ``POST /prompts/{tail}``（tail 取
        ``{prompt_id}:abort``）。规格里没有这条、也没有独立 ``:abort``
        路径时返回 ``kimi.local_server.unavailable``，不向猜测地址发
        请求，也不改走 ``kimi.acp``。
        """
        handle = self._sessions.get(session.agent_session_id)
        client = (handle or {}).get("client")
        external_id = (handle or {}).get("external_session_id") \
            or session.external_session_id
        if client is None or not external_id:
            return self.unsupported_receipt(
                "interrupt", "no_active_session", session=session)
        prompt_id = (handle or {}).get("current_prompt_id")
        declared = [
            str(path) for path in ((handle or {}).get("openapi_paths") or {})
        ]
        # 2.1.1 declares abort as POST /prompts/{tail} ("Abort a running
        # prompt"), not a session-level :abort route. Call the tail form
        # only when the spec lists it; do not post an undeclared fallback.
        prompt_abort = any(
            path.endswith("/prompts/{tail}")
            or ("/prompts/" in path and path.endswith(":abort"))
            for path in declared
        )
        session_abort = any(
            path.endswith(":abort") and "/prompts/" not in path
            for path in declared
        )
        if prompt_id and prompt_abort:
            abort_path = (
                f"/api/v1/sessions/{external_id}/prompts/{prompt_id}:abort"
            )
        elif session_abort:
            abort_path = f"/api/v1/sessions/{external_id}:abort"
        else:
            return self.unsupported_receipt(
                "interrupt", "abort_unavailable", session=session,
                detail={"code": KIMI_LOCAL_SERVER_UNAVAILABLE,
                        "detail": "OpenAPI has no prompt abort route"},
            )
        try:
            resp = await client.post(abort_path)
            await self._unwrap(resp.json(), "abort")
        except Exception as exc:  # noqa: BLE001
            return self.unsupported_receipt(
                "interrupt", "abort_failed", session=session,
                detail={"detail": str(exc)})
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
