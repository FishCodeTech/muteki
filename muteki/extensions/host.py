"""子进程 Extension Host（任务书 11.2，EXT-01）。

``ExtensionProcess`` 管理一个扩展版本的子进程生命周期：

- 启动：按 manifest entrypoints.command spawn，工作目录为该版本的安装目录，
  环境变量最小化（PATH/LANG + MUTEKI_* + 按权限解引用的 secret），
  不继承宿主进程环境（隔离等级：Local extension）；
- 握手：initialize（协议主版本必须一致）→ capabilities/list →
  config/validate（声明了 config_schema 时）→ activate → health/read；
- 运行：command/handle、projection/read；扩展经 event/propose 回宿主，
  由 ``proposal_handler`` 回调（registry.py 接 MutekiCommandAPI）；
- 停止：deactivate → shutdown → terminate → kill 的逐级降级；
- 日志：stderr 逐行归档到 ``<state_dir>/logs/``，写入前经
  原样写入日志。

Extension Host 不执行模型调用和 Agent Loop，也不加载第三方任意前端代码。
"""

from __future__ import annotations

import asyncio
import logging
import os
import time
from pathlib import Path
from typing import Any, Awaitable, Callable, Optional

from muteki.extensions.manifest import (
    SchemaValidationError,
    load_schema,
    validate_against_schema,
)
from muteki.extensions.isolation import (
    ExtensionIsolationPlan,
    IsolationUnavailable,
    apply_resource_limits,
    build_isolation_plan,
)
from muteki.extensions.permissions import PermissionChecker
from muteki.extensions.protocol import (
    PROTOCOL_VERSION,
    ExtensionRpcError,
    ExtensionUnavailable,
    JsonRpcPeer,
    ProtocolError,
    check_protocol_version,
)
from muteki.platform.contracts.extensions import ExtensionManifest

LOG = logging.getLogger(__name__)

#: 握手各阶段与子进程停止的默认超时（秒）。
DEFAULT_HANDSHAKE_TIMEOUT = 15.0
DEFAULT_STOP_TIMEOUT = 5.0
#: 健康检查连续失败多少次后视为 unhealthy（供监管层自动回滚判定）。
DEFAULT_HEALTH_FAIL_THRESHOLD = 2

#: 扩展 → Host 事件提案回调：``(event_type, payload) -> result dict``。
ProposalHandler = Callable[[str, dict[str, Any]], Awaitable[dict[str, Any]]]


class ExtensionProcessError(RuntimeError):
    """扩展进程启动 / 握手 / 调用失败的统一错误。"""


class ExtensionProcess:
    """一个扩展（一个版本）的子进程句柄与 RPC 通道。"""

    def __init__(
        self,
        manifest: ExtensionManifest,
        *,
        package_dir: str | Path,
        state_dir: str | Path,
        checker: PermissionChecker,
        config: Optional[dict[str, Any]] = None,
        proposal_handler: Optional[ProposalHandler] = None,
        handshake_timeout: float = DEFAULT_HANDSHAKE_TIMEOUT,
        request_timeout: float = 10.0,
        stop_timeout: float = DEFAULT_STOP_TIMEOUT,
        extra_env: Optional[dict[str, str]] = None,
    ) -> None:
        self.manifest = manifest
        self.package_dir = Path(package_dir)
        self.state_dir = Path(state_dir)
        self.checker = checker
        self.config = dict(config or {})
        self.proposal_handler = proposal_handler
        self._handshake_timeout = float(handshake_timeout)
        self._request_timeout = float(request_timeout)
        self._stop_timeout = float(stop_timeout)
        self._extra_env = dict(extra_env or {})
        self._process: Optional[asyncio.subprocess.Process] = None
        self._peer: Optional[JsonRpcPeer] = None
        self._stderr_task: Optional[asyncio.Task] = None
        self._capabilities: dict[str, Any] = {}
        self._isolation: Optional[ExtensionIsolationPlan] = None
        self._last_exit_code: Optional[int] = None
        self._secret_usage: list[dict[str, Any]] = []

    # -- 状态 -----------------------------------------------------------------

    @property
    def running(self) -> bool:
        return self._process is not None and self._process.returncode is None

    @property
    def capabilities(self) -> dict[str, Any]:
        return dict(self._capabilities)

    @property
    def pid(self) -> Optional[int]:
        return self._process.pid if self._process is not None else None

    @property
    def return_code(self) -> Optional[int]:
        if self._process is not None and self._process.returncode is not None:
            return self._process.returncode
        return self._last_exit_code

    @property
    def secret_usage(self) -> list[dict[str, Any]]:
        return [dict(item) for item in self._secret_usage]

    @property
    def isolation(self) -> dict[str, Any]:
        return self._isolation.public_view() if self._isolation else {
            "backend": "none", "enforcement": "unavailable"}

    # -- 启动与握手 -------------------------------------------------------------

    async def start(self) -> None:
        """spawn 子进程并完成 initialize → capabilities → config → activate → health。"""
        if self.running:
            raise ExtensionProcessError(
                f"extension {self.manifest.id} already running (pid {self.pid})"
            )
        if self.manifest.entrypoints.host != "subprocess":
            raise ExtensionProcessError(
                f"extension {self.manifest.id} is not a subprocess extension "
                f"(host={self.manifest.entrypoints.host}); in_process is only "
                "allowed for builtin code"
            )
        self.state_dir.mkdir(parents=True, exist_ok=True)
        # 启动前按权限解引用 secret：失败即启动失败，不静默降级。
        secrets = self.checker.resolve_secrets()
        from muteki.extensions.permissions import secret_env_name
        self._secret_usage = [{
            "reference": ref,
            "scope": f"extension:{self.manifest.id}",
            "injection": "minimal_process_environment",
            "environment_name": secret_env_name(ref),
            "expires_at": "process_exit",
        } for ref in self.manifest.permissions.secrets]
        env = self._build_env(secrets)
        command = list(self.manifest.entrypoints.command)
        try:
            self._isolation = build_isolation_plan(
                self.manifest,
                command,
                package_dir=self.package_dir,
                state_dir=self.state_dir,
                workspace_root=getattr(self.checker, "_workspace_root", None),
                env=env,
            )
        except IsolationUnavailable as exc:
            if os.environ.get("MUTEKI_ALLOW_ADVISORY_EXTENSION_PERMISSIONS") == "1":
                self._isolation = ExtensionIsolationPlan(
                    backend="none",
                    enforcement="advisory",
                    command=command,
                )
            else:
                raise ExtensionProcessError(
                    f"extension isolation unavailable: {exc}") from exc
        command = list(self._isolation.command)
        env["MUTEKI_PERMISSION_ENFORCEMENT"] = self._isolation.enforcement
        env["PYTHONDONTWRITEBYTECODE"] = "1"
        try:
            self._process = await asyncio.create_subprocess_exec(
                *command,
                cwd=str(self.package_dir),
                env=env,
                stdin=asyncio.subprocess.PIPE,
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE,
                preexec_fn=(
                    lambda: apply_resource_limits(self._isolation.resource_limits)
                ) if self._isolation.resource_limits else None,
            )
        except OSError as exc:
            raise ExtensionProcessError(
                f"cannot spawn extension {self.manifest.id} "
                f"({command!r}): {exc}"
            ) from exc
        assert self._process.stdout is not None and self._process.stdin is not None
        self._peer = JsonRpcPeer(
            self._process.stdout,
            self._process.stdin,
            host_handler=self._on_extension_request,
            request_timeout=self._request_timeout,
            peer_label=f"ext:{self.manifest.id}",
        )
        self._peer.start()
        if self._process.stderr is not None:
            self._stderr_task = asyncio.create_task(
                self._archive_stderr(self._process.stderr)
            )
        try:
            await self._handshake()
        except Exception:
            await self.stop()
            raise

    def _build_env(self, secrets: dict[str, str]) -> dict[str, str]:
        """最小环境：不继承宿主环境，只给 PATH/LANG 与 MUTEKI_* 注入。"""
        env = {
            "PATH": os.environ.get("PATH", "/usr/bin:/bin:/usr/local/bin"),
            "LANG": os.environ.get("LANG", "en_US.UTF-8"),
            "MUTEKI_EXTENSION_ID": self.manifest.id,
            "MUTEKI_EXTENSION_VERSION": self.manifest.version,
            "MUTEKI_EXTENSION_STATE_DIR": str(self.state_dir),
            "MUTEKI_PROTOCOL_VERSION": str(PROTOCOL_VERSION),
        }
        env.update(secrets)
        env.update(self._extra_env)
        return env

    async def _handshake(self) -> None:
        peer = self._require_peer()
        timeout = self._handshake_timeout
        result = await peer.request(
            "initialize",
            {
                "extension_id": self.manifest.id,
                "version": self.manifest.version,
                "core_version": None,  # 由 manifest 校验保证兼容，握手只核对协议版本
                "config": self.config,
                **self.checker.initialize_params(),
                "isolation": self.isolation,
            },
            timeout=timeout,
        )
        params = {"protocol_version": (result or {}).get("protocol_version")}
        try:
            check_protocol_version(params)
        except ProtocolError as exc:
            raise ExtensionProcessError(
                f"handshake failed for {self.manifest.id}: {exc}"
            ) from exc
        self._capabilities = dict(
            await peer.request("capabilities/list", {}, timeout=timeout) or {}
        )
        self._capabilities["muteki_isolation"] = self.isolation
        schema = load_schema(self.package_dir, self.manifest.config_schema)
        if schema is not None:
            # 宿主侧先校验一次（安装流程要求 config validation 先行），
            # 再让扩展自检；两处都过才 activate。
            try:
                validate_against_schema(self.config, schema)
            except SchemaValidationError as exc:
                raise ExtensionProcessError(
                    f"config does not satisfy config_schema: {exc}"
                ) from exc
            reply = await peer.request(
                "config/validate", {"config": self.config}, timeout=timeout
            )
            if not (reply or {}).get("valid", False):
                raise ExtensionProcessError(
                    f"extension rejected its config: "
                    f"{(reply or {}).get('errors') or reply}"
                )
        await peer.request(
            "activate",
            {"config": self.config, "capabilities": self._capabilities},
            timeout=timeout,
        )

    # -- 运行期调用 -------------------------------------------------------------

    async def health(self) -> dict[str, Any]:
        """health/read；返回至少含 status（healthy/degraded/unhealthy）。"""
        result = await self._require_peer().request("health/read", {})
        return dict(result or {})

    async def handle_command(
        self, command_type: str, payload: Optional[dict[str, Any]] = None
    ) -> dict[str, Any]:
        """command/handle：扩展业务命令。命令命名空间先过权限执行点。"""
        self.checker.check_command_type(command_type)
        result = await self._require_peer().request(
            "command/handle",
            {"command_type": command_type, "payload": dict(payload or {})},
        )
        return dict(result or {})

    async def read_projection(self, name: str = "") -> dict[str, Any]:
        """projection/read：扩展自己的公开 projection（不是核心投影）。"""
        result = await self._require_peer().request(
            "projection/read", {"name": name}
        )
        return dict(result or {})

    # -- 停止 / 重启 -------------------------------------------------------------

    async def stop(self) -> None:
        """deactivate → shutdown → terminate → kill 逐级降级；幂等。"""
        peer, process = self._peer, self._process
        self._peer, self._process = None, None
        if peer is not None and process is not None and process.returncode is None:
            for method in ("deactivate", "shutdown"):
                try:
                    await peer.request(method, {}, timeout=self._stop_timeout)
                except (ExtensionRpcError, ExtensionUnavailable, ProtocolError,
                        asyncio.TimeoutError) as exc:
                    LOG.info("ext %s %s failed during stop: %s",
                             self.manifest.id, method, exc)
            try:
                await asyncio.wait_for(process.wait(), timeout=self._stop_timeout)
            except asyncio.TimeoutError:
                process.terminate()
                try:
                    await asyncio.wait_for(
                        process.wait(), timeout=self._stop_timeout)
                except asyncio.TimeoutError:
                    process.kill()
                    await process.wait()
        if process is not None:
            self._last_exit_code = process.returncode
        if peer is not None:
            await peer.close()
        if self._stderr_task is not None:
            try:
                await asyncio.wait_for(self._stderr_task, timeout=self._stop_timeout)
            except (asyncio.TimeoutError, asyncio.CancelledError):
                self._stderr_task.cancel()
            self._stderr_task = None

    async def restart(self) -> None:
        """停止后按同一配置重新启动（监管层在 health 抖动时使用）。"""
        await self.stop()
        await self.start()

    # -- 扩展 → Host 入口 ---------------------------------------------------------

    async def _on_extension_request(
        self, method: str, params: dict[str, Any]
    ) -> dict[str, Any]:
        """扩展发来的请求。当前只有 event/propose；权限在这里强制执行。"""
        if method != "event/propose":
            raise ProtocolError(f"unsupported extension request: {method}")
        if self.proposal_handler is None:
            raise ProtocolError("event proposals are not accepted by this host")
        event_type = str(params.get("event_type") or "").strip()
        payload = params.get("payload")
        payload = payload if isinstance(payload, dict) else {}
        # 命名空间权限 + 扩展声明的 event schema 双重校验。
        self.checker.check_event_type(event_type)
        schemas = self._capabilities.get("event_schemas") or {}
        schema = schemas.get(event_type)
        if isinstance(schema, dict):
            validate_against_schema(payload, schema, field=f"event:{event_type}")
        return await self.proposal_handler(event_type, payload)

    # -- 日志归档 ---------------------------------------------------------------

    async def _archive_stderr(self, stream: asyncio.StreamReader) -> None:
        """扩展 stderr 逐行原样归档到 ``<state_dir>/logs/``。"""
        log_dir = self.state_dir / "logs"
        log_dir.mkdir(parents=True, exist_ok=True)
        path = log_dir / f"{self.manifest.version}-{int(time.time())}.log"
        try:
            with path.open("a", encoding="utf-8") as handle:
                while True:
                    line = await stream.readline()
                    if not line:
                        break
                    text = line.decode("utf-8", errors="replace")
                    handle.write(text)
                    handle.flush()
        except asyncio.CancelledError:
            raise
        except OSError:
            LOG.exception("cannot archive stderr for %s", self.manifest.id)

    def _require_peer(self) -> JsonRpcPeer:
        if self._peer is None or not self.running:
            raise ExtensionUnavailable(
                f"extension {self.manifest.id} is not running"
            )
        return self._peer


__all__ = [
    "DEFAULT_HANDSHAKE_TIMEOUT",
    "DEFAULT_HEALTH_FAIL_THRESHOLD",
    "DEFAULT_STOP_TIMEOUT",
    "ExtensionProcess",
    "ExtensionProcessError",
    "ProposalHandler",
]
