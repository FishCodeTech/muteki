"""Factory Droid chat adapter on the official ``droid-sdk`` stream-jsonrpc client.

The SDK supplies session streaming, permission and question callbacks, resume,
stdio MCP and partial messages. Three points are Muteki's own:

* The ``droid exec`` process is spawned through ``process_supervisor`` (own
  process group, ledger entry, complete stderr log) and handed to the SDK as a
  custom ``DroidClientTransport``. The SDK's built-in transport would spawn it
  directly, leave MCP and shell grandchildren behind on close and keep only a
  16 KiB prefix of stderr.
* Access modes map onto Droid autonomy and are applied on every open, resume
  included: ``Session.resume`` carries no autonomy argument, so a resumed
  session would otherwise keep whatever level it was last saved with. A session
  whose autonomy cannot be confirmed does not start.
* Approvals expose only ``proceed_once`` and ``cancel``. Droid's other options
  (``proceed_always``, ``proceed_auto_run*``, ``proceed_new_session*``) change
  the session autonomy, which an approval must never do; "allow for session"
  is kept in ``SessionApprovalGrants`` for the exact tool kind and target.

Steer stays unsupported until a concurrent send during an open turn is
observed. A failed resume is never replaced with a new session. Mission and
spec (plan) mode are not started.
"""

from __future__ import annotations

import asyncio
import contextlib
import json
import logging
import os
import time
from collections.abc import Mapping
from hashlib import sha256
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, AsyncIterator, Optional

from droid_sdk import (
    ApplyPatchAction,
    Autonomy,
    ContextAccuracy,
    CreateFile,
    DroidConnectionError,
    DroidError,
    DroidProcessError,
    DroidProtocolError,
    EditAction,
    ErrorEvent,
    ExecuteAction,
    ExitSpecModeAction,
    InteractionHandlers,
    InvalidWorkingDirectoryError,
    McpToolAction,
    Mode,
    PermissionRequest,
    QuestionRequest,
    ReasoningEffort,
    RunFailure,
    RunInterrupted,
    RunSuccess,
    RunTimeoutError,
    Runtime,
    Session,
    SessionBusyError,
    SessionConfig,
    SessionNotFoundError,
    SettingsUpdated,
    StdioMcpServerConfig,
    TextDelta,
    ThinkingDelta,
    TokenUsageUpdate,
    ToolCall,
    ToolConfirmationOutcome,
    ToolProgress,
    ToolResult,
    WorkingState,
    WorkingStateChanged,
)
from droid_sdk._attribution import sdk_process_environment
from muteki.platform.contracts.agent_events import (
    AgentFailure,
    ApprovalOption,
    ApprovalRequestedPayload,
    ApprovalResolvedPayload,
    CompactionPayload,
    FailureCategory,
    MessageCompletedPayload,
    MessageDeltaPayload,
    ReasoningPayload,
    RuntimeErrorPayload,
    RuntimeExitedPayload,
    RuntimeWarningPayload,
    SessionPayload,
    ToolPayload,
    TurnCompletedPayload,
    TurnFailedPayload,
    TurnStartedPayload,
    UsagePayload,
    UserInputRequestedPayload,
    UserInputResolvedPayload,
    WorkspaceFileChange,
    dump_payload,
)
from muteki.platform.contracts.base import new_id
from muteki.platform.contracts.capabilities import CapabilityInjectionPlan
from muteki.platform.contracts.errors import ErrorCategory, ErrorEnvelope
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
from muteki.platform.contracts.receipts import AggregateRef, CommandReceipt, ReceiptState

from .approvals import ApprovalDecision, ApprovalTarget, SessionApprovalGrants
from .base import BaseExternalAgentAdapter, TurnLimits, TurnRunner
from .capabilities import (
    SOURCE_PROBE,
    SOURCE_STATIC,
    CapabilityProbeReport,
    conservative_capabilities,
    probe_version,
    require_access_mode,
)
from .context_delivery import system_append, text_with_envelopes
from .droid_mcp import DroidMcp, DroidMcpError, stdio_mcp_servers
from .events import build_event
from .probe_environment import subprocess_environment
from .process_supervisor import SupervisedProcess, spawn_supervised
from .rpc import ProcessOutputLog, default_process_log_root
from .runtime_capabilities import (
    RuntimeCapabilityItem,
    RuntimeCapabilitySnapshot,
    dynamic_command_item,
)

_LOG = logging.getLogger(__name__)

_ACCESS_NOTES = {
    AccessMode.SUPERVISED.value: (
        "Autonomy off. Every permission request waits for the operator."
    ),
    AccessMode.AUTO_ACCEPT_EDITS.value: (
        "Autonomy low. File edits Droid still asks about are allowed once; "
        "everything else waits for the operator."
    ),
    AccessMode.AUTO.value: (
        "Autonomy medium. Requests Droid still raises wait for the operator."
    ),
    AccessMode.FULL_ACCESS.value: (
        "Autonomy high. Requests Droid still raises are allowed once. "
        "This is not --skip-permissions-unsafe."
    ),
}
_AUTONOMY = {
    AccessMode.SUPERVISED.value: Autonomy.OFF,
    AccessMode.AUTO_ACCEPT_EDITS.value: Autonomy.LOW,
    AccessMode.AUTO.value: Autonomy.MEDIUM,
    AccessMode.FULL_ACCESS.value: Autonomy.HIGH,
}
_ONCE = ToolConfirmationOutcome.PROCEED_ONCE.value
_CANCEL = ToolConfirmationOutcome.CANCEL.value
# Closed set of Droid's own tool names; unknown tools carry no kind.
_TOOL_KINDS = {
    "Execute": "command",
    "Create": "file_change",
    "Edit": "file_change",
    "MultiEdit": "file_change",
    "ApplyPatch": "file_change",
}
_EDIT_ACTIONS = (EditAction, CreateFile, ApplyPatchAction)
_STDOUT_LIMIT = 10 * 1024 * 1024
_STDERR_DRAIN_TIMEOUT_S = 5.0
_DEFAULT_TURN_TIMEOUT_S = 1800
# A spare process is dropped after this idle time so it does not keep stale
# Droid settings; at most _SPARE_LIMIT spares (different folders) are kept.
_SPARE_TTL_S = 600.0
_SPARE_LIMIT = 2
_CATALOG_REQUEST_TIMEOUT_S = 30.0


class DroidFailure(RuntimeError):
    """Typed start failure; callers branch on ``code`` and ``category``."""

    def __init__(
        self, code: str, message: str, *, category: str = "provider", detail: str = "",
    ) -> None:
        super().__init__(message)
        self.code = code
        self.category = category
        self.detail = detail


def _effort(value: Optional[str]) -> Optional[ReasoningEffort]:
    text = str(value or "").strip()
    if not text or text == "default":
        return None
    try:
        return ReasoningEffort(text)
    except ValueError as exc:
        raise DroidFailure(
            "droid.effort_unsupported",
            f"Droid reasoning effort {text!r} is not in the SDK enum",
            category="unsupported",
        ) from exc


def droid_model_ids(binary: Optional[str] = None) -> list[str]:
    """Compatibility export; discovery does not depend on an SDK or Web UI."""
    from muteki.core.droid_models import droid_model_ids as discover

    return discover(binary)


def _host_login_present() -> bool:
    return (Path.home() / ".factory" / "auth.v2.loginkeychain").is_file()


def _option_id(option: Any) -> str:
    value = getattr(option, "value", "")
    return str(getattr(value, "value", value) or "")


def _thaw(value: Any) -> Any:
    """SDK values are frozen (mappingproxy, tuple); events need plain JSON."""
    if isinstance(value, Mapping):
        return {str(key): _thaw(item) for key, item in value.items()}
    if isinstance(value, (list, tuple)):
        return [_thaw(item) for item in value]
    return value


def _json_text(value: Any) -> str:
    return value if isinstance(value, str) else json.dumps(_thaw(value), default=str)


class _SupervisedDroidTransport:
    """``DroidClientTransport`` over a ``process_supervisor`` child.

    Mirrors the SDK's JSONL framing (one JSON object per line, non-JSON lines
    skipped, any exit of the process is a ``DroidProcessError``) while the
    process lives in its own group and stderr goes to a complete log file.
    """

    def __init__(
        self,
        argv: list[str],
        *,
        cwd: str,
        env: dict[str, str],
        adapter_id: str,
        session_id: str,
        log_root: Optional[Path],
    ) -> None:
        self._argv = list(argv)
        self._cwd = cwd
        self._env = dict(env)
        self._adapter_id = adapter_id
        self._session_id = session_id
        self._log_root = log_root
        self._supervised: Optional[SupervisedProcess] = None
        self._log: Optional[ProcessOutputLog] = None
        self._stderr_task: Optional[asyncio.Task] = None
        self._write_lock = asyncio.Lock()
        self._connected = False
        self._closing = False
        self._error: Optional[DroidProcessError] = None

    @property
    def is_connected(self) -> bool:
        return self._connected

    @property
    def exited(self) -> bool:
        return self._supervised is None or self._supervised.process.returncode is not None

    async def connect(self) -> None:
        if self._connected:
            raise DroidConnectionError(
                "Transport already connected", exec_path=self._argv[0], cwd=self._cwd)
        log = ProcessOutputLog.create(
            self._log_root, label=f"droid-rpc-{self._session_id}-stderr")
        try:
            self._supervised = await spawn_supervised(
                self._argv,
                adapter_id=self._adapter_id,
                session_id=self._session_id,
                label="droid exec stream-jsonrpc",
                stdin=asyncio.subprocess.PIPE,
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE,
                limit=_STDOUT_LIMIT,
                cwd=self._cwd,
                env=self._env,
            )
        except FileNotFoundError:
            await log.close()
            raise
        except OSError as exc:
            await log.close()
            raise DroidConnectionError(
                f"Failed to start droid process: {exc}",
                exec_path=self._argv[0], cwd=self._cwd,
            ) from exc
        except BaseException:
            await log.close()
            raise
        self._log = log
        stderr = self._supervised.process.stderr
        if stderr is not None:
            self._stderr_task = asyncio.ensure_future(self._drain_stderr(stderr, log))
        self._connected = True

    @staticmethod
    async def _drain_stderr(stream: asyncio.StreamReader, log: ProcessOutputLog) -> None:
        while True:
            chunk = await stream.read(65536)
            if not chunk:
                return
            log.append(chunk)

    def stderr_text(self) -> str:
        return self._log.read_all() if self._log is not None else ""

    async def wait_exit(self) -> Optional[int]:
        if self._supervised is None:
            return None
        return await self._supervised.process.wait()

    async def send(self, message: str) -> None:
        if self._error is not None:
            raise self._error
        proc = self._supervised.process if self._supervised is not None else None
        if not self._connected or proc is None or proc.stdin is None:
            raise DroidConnectionError(
                "Transport not connected", exec_path=self._argv[0], cwd=self._cwd)
        async with self._write_lock:
            if proc.returncode is not None:
                raise await self._exit_error()
            try:
                proc.stdin.write((message + "\n").encode("utf-8"))
                await proc.stdin.drain()
            except OSError as exc:
                if proc.returncode is not None:
                    raise await self._exit_error() from exc
                raise DroidConnectionError(
                    f"Failed to write to droid stdin: {exc}",
                    exec_path=self._argv[0], cwd=self._cwd,
                ) from exc

    async def read_messages(self) -> AsyncIterator[dict[str, Any]]:
        proc = self._supervised.process if self._supervised is not None else None
        if proc is None or proc.stdout is None:
            return
        while True:
            try:
                raw = await proc.stdout.readline()
            except ValueError as exc:
                await self._terminate()
                raise DroidConnectionError(
                    f"droid stdout line exceeded {_STDOUT_LIMIT} bytes; "
                    "the process group was stopped",
                    exec_path=self._argv[0], cwd=self._cwd,
                ) from exc
            if not raw:
                break
            text = raw.decode("utf-8", errors="replace").strip()
            if not text:
                continue
            try:
                message = json.loads(text)
            except ValueError:
                if self._log is not None:
                    self._log.append(b"[stdout] " + raw)
                continue
            if isinstance(message, dict):
                yield message
        raise await self._exit_error()

    async def _exit_error(self) -> DroidProcessError:
        if self._error is not None:
            return self._error
        proc = self._supervised.process
        code = await proc.wait()
        if self._stderr_task is not None:
            with contextlib.suppress(asyncio.TimeoutError, Exception):
                await asyncio.wait_for(
                    asyncio.shield(self._stderr_task), timeout=_STDERR_DRAIN_TIMEOUT_S)
        self._connected = False
        stderr = self.stderr_text()
        message = "Droid process exited"
        if stderr:
            message = f"{message}; stderr: {' '.join(stderr.split())}"
        self._error = DroidProcessError(
            message,
            exit_code=code if code >= 0 else None,
            signal=-code if code < 0 else None,
        )
        return self._error

    async def _terminate(self) -> None:
        if self._supervised is None:
            return
        result = await self._supervised.terminate()
        if not result.stopped:
            _LOG.warning(
                "droid process group not stopped: %s %s", result.code, result.detail)

    async def close(self) -> None:
        if self._closing:
            return
        self._closing = True
        self._connected = False
        try:
            await self._terminate()
        finally:
            task, self._stderr_task = self._stderr_task, None
            if task is not None:
                if not task.done():
                    task.cancel()
                with contextlib.suppress(asyncio.CancelledError, Exception):
                    await task
            if self._log is not None:
                await self._log.close()


@dataclass
class _Choice:
    option_id: str
    outcome: str
    scope: Optional[str] = None
    note: str = ""
    automatic: bool = False


@dataclass
class _PendingApproval:
    future: "asyncio.Future[_Choice]"
    offered: frozenset[str]
    payload: dict[str, Any]


@dataclass
class _PendingQuestion:
    future: "asyncio.Future[Any]"
    request: QuestionRequest
    declined: bool = False


@dataclass
class _Spare:
    transport: "_SupervisedDroidTransport"
    started_at: float
    expiry: Optional[asyncio.TimerHandle] = None


@dataclass
class _Live:
    """Everything one open Droid session owns."""

    agent_session_id: str
    cwd: str
    env: dict[str, str]
    access_mode: str
    thread_id: Optional[str]
    mcp: DroidMcp
    mcp_servers: tuple[StdioMcpServerConfig, ...]
    grants: SessionApprovalGrants
    requested_model: str = ""
    requested_effort: str = ""
    resumed: bool = False
    interactions: Optional[InteractionHandlers] = None
    session: Optional[Session] = None
    transport: Optional[_SupervisedDroidTransport] = None
    external_id: str = ""
    confirmed_model: str = ""
    confirmed_effort: str = ""
    queue: Optional["asyncio.Queue[tuple[str, Any]]"] = None
    turn_active: bool = False
    announced: bool = False
    approvals: dict[str, _PendingApproval] = field(default_factory=dict)
    questions: dict[str, _PendingQuestion] = field(default_factory=dict)
    text: list[str] = field(default_factory=list)
    turn_closed: bool = False
    catalog_digest: str = ""
    catalog_revision: int = 0


class DroidRpcAdapter(BaseExternalAgentAdapter):
    """Default Droid conversation transport (``droid.rpc``)."""

    adapter_id = "droid.rpc"
    host_context_channel = "system_prompt"

    def __init__(
        self,
        *,
        binary: Optional[str] = None,
        env_extra: Optional[dict[str, str]] = None,
        log_root: Optional[Path] = None,
        turn_timeout_s: int = _DEFAULT_TURN_TIMEOUT_S,
        **kwargs: Any,
    ) -> None:
        super().__init__(self.adapter_id, **kwargs)
        self._binary = binary or os.environ.get("MUTEKI_DROID_BIN") or "droid"
        self._env_extra = dict(env_extra or {})
        self._log_root = Path(log_root) if log_root is not None else default_process_log_root()
        self._turn_timeout_s = int(turn_timeout_s)
        self._live: dict[str, _Live] = {}
        self._spares: dict[str, _Spare] = {}

    def probe_binary(self) -> str:
        return self._binary

    # -- probe ---------------------------------------------------------------

    def declared_chat_capabilities(self) -> AgentCapabilities:
        caps = AgentCapabilities(
            transport_kind="rpc",
            capability_source="adapter_reported",
            access_modes=list(ACCESS_MODE_VALUES),
            streaming=True,
            tool_events=True,
            approval=True,
            user_input=True,
            interrupt=True,
            resume=True,
            session_persistence=True,
            mcp=True,
            usage_events=True,
            compaction=True,
            context_usage=True,
            steer=False,
            plan_mode=False,
            subagents=False,
        )
        return caps

    async def probe(self, request: ProbeRequest) -> AgentCapabilities:
        del request
        result = await probe_version(
            self._binary, env=subprocess_environment(self._env_extra))
        if not result.ok:
            detail = f"{result.error_code.value}: {result.error_detail}"
            caps = conservative_capabilities(
                transport_kind="rpc", capability_source=SOURCE_STATIC)
            self._probe_cache = CapabilityProbeReport(
                adapter_id=self.identity.adapter_id,
                instance_id=self.identity.instance_id,
                capabilities=caps,
                binary_path=self._binary,
                degradations=[
                    "droid --version failed; streaming, approval, resume and "
                    "interrupt are not claimed",
                ],
                detail=detail,
            )
            return caps
        caps = self.declared_chat_capabilities()
        caps.capability_source = SOURCE_PROBE
        caps.runtime_version = result.version
        self._probe_cache = CapabilityProbeReport(
            adapter_id=self.identity.adapter_id,
            instance_id=self.identity.instance_id,
            capabilities=caps,
            binary_path=self._binary,
            field_sources={
                **{name: SOURCE_STATIC for name in (
                    "streaming", "tool_events", "approval", "user_input",
                    "interrupt", "resume", "session_persistence", "mcp",
                    "usage_events", "compaction", "context_usage", "steer",
                    "plan_mode", "subagents",
                )},
                "runtime_version": SOURCE_PROBE,
            },
            degradations=[
                "the stream-jsonrpc handshake is not run by the probe: it would "
                "create a persistent Droid session; protocol capabilities are "
                "the droid-sdk 0.5.0 baseline",
                "steer is not advertised: a second prompt during a live turn was not verified",
                "mission/subagents are not enabled",
                "plan_mode is not mapped onto Droid spec mode",
            ],
            detail="droid --version succeeded; droid-sdk 0.5.0 stream-jsonrpc",
        )
        return caps

    # -- launch / teardown ---------------------------------------------------

    def _argv(self) -> list[str]:
        return self._with_launch_args([
            self._binary, "exec",
            "--input-format", "stream-jsonrpc",
            "--output-format", "stream-jsonrpc",
        ])

    def _process_env(self, supplied: Mapping[str, str]) -> dict[str, str]:
        env = dict(subprocess_environment({**self._env_extra, **dict(supplied)}) or {})
        # The per-thread chat asset root is read only by engines that load
        # native plugin packages; Droid never reads it. Dropping it keeps the
        # process environment thread-independent so a pre-started process fits.
        env.pop("MUTEKI_CHAT_ASSETS_ROOT", None)
        return {**env, **sdk_process_environment()}

    def _spare_key(self, cwd: str, env: Mapping[str, str]) -> str:
        payload = json.dumps([self._argv(), cwd, sorted(env.items())], ensure_ascii=False)
        return sha256(payload.encode("utf-8")).hexdigest()

    async def prewarm(self, *, cwd: str, env: Mapping[str, str]) -> bool:
        """Start one idle ``droid exec`` process for a session that has not been created yet.

        Droid spends 6-15 s booting (settings, tools, connector catalog) before it
        reads the first JSON-RPC request; none of that depends on the session. The
        process is adopted only by a session with the same argv, cwd and env.
        """
        cwd = str(Path(cwd).expanduser().resolve())
        if not Path(cwd).is_dir():
            return False
        process_env = self._process_env(env)
        if not str(process_env.get("FACTORY_API_KEY") or "").strip() and not _host_login_present():
            return False
        key = self._spare_key(cwd, process_env)
        current = self._spares.get(key)
        if current is not None and (not current.transport.is_connected or not current.transport.exited):
            return False
        if current is not None:
            await self._drop_spare(key, current)
        while len(self._spares) >= _SPARE_LIMIT:
            ready = [item for item in self._spares.items() if item[1].transport.is_connected]
            if not ready:
                return False
            oldest_key, oldest = min(ready, key=lambda item: item[1].started_at)
            await self._drop_spare(oldest_key, oldest)
        transport = _SupervisedDroidTransport(
            self._argv(), cwd=cwd, env=process_env,
            adapter_id=self.identity.adapter_id,
            session_id=new_id("droid-prewarm"),
            log_root=self._log_root,
        )
        spare = _Spare(transport=transport, started_at=time.monotonic())
        self._spares[key] = spare
        try:
            await transport.connect()
        except BaseException as exc:
            if self._spares.get(key) is spare:
                self._spares.pop(key, None)
            await transport.close()
            if isinstance(exc, FileNotFoundError):
                raise DroidFailure(
                    "droid.binary_missing", f"Droid executable was not found: {self._binary}",
                    category="transport") from exc
            if isinstance(exc, DroidError):
                raise DroidFailure(
                    "droid.prewarm_failed", f"Droid process could not be started: {exc}",
                    category="transport") from exc
            raise
        if self._spares.get(key) is not spare:
            # Dropped (shutdown or eviction) while the process was starting.
            await transport.close()
            return False
        spare.expiry = asyncio.get_running_loop().call_later(
            _SPARE_TTL_S, lambda: asyncio.ensure_future(self._drop_spare(key, spare)))
        return True

    def _take_spare(self, cwd: str, env: Mapping[str, str]) -> Optional[_SupervisedDroidTransport]:
        key = self._spare_key(cwd, env)
        spare = self._spares.get(key)
        if spare is None or not spare.transport.is_connected:
            return None
        self._spares.pop(key, None)
        if spare.expiry is not None:
            spare.expiry.cancel()
        if spare.transport.exited:
            asyncio.ensure_future(spare.transport.close())
            return None
        return spare.transport

    async def _drop_spare(self, key: str, spare: _Spare) -> None:
        if self._spares.get(key) is spare:
            self._spares.pop(key, None)
        if spare.expiry is not None:
            spare.expiry.cancel()
        if spare.transport.is_connected:
            # A spare still starting is closed by prewarm once connect returns.
            await spare.transport.close()

    async def close_spares(self) -> None:
        for key, spare in list(self._spares.items()):
            await self._drop_spare(key, spare)

    async def _launch(
        self,
        request: SessionStart,
        plan: Optional[CapabilityInjectionPlan],
        bearer_token: Optional[str],
    ) -> dict[str, Any]:
        require_access_mode(
            self.identity.adapter_id, request.access_mode, ACCESS_MODE_VALUES,
            reasons=_ACCESS_NOTES,
        )
        if request.interaction_mode == "plan":
            raise DroidFailure(
                "droid.plan_mode_unsupported",
                "droid.rpc does not map Muteki plan mode onto Droid spec mode",
                category="unsupported",
            )
        env = self._process_env(request.options.env)
        if not str(env.get("FACTORY_API_KEY") or "").strip() and not _host_login_present():
            raise DroidFailure(
                "droid.auth_required",
                "FACTORY_API_KEY is unset and ~/.factory has no stored login",
                category="auth",
            )
        cwd = str(request.options.cwd or os.getcwd())
        if not Path(cwd).is_dir():
            raise DroidFailure(
                "droid.cwd_invalid", f"Working directory does not exist: {cwd}",
                category="validation",
            )
        effort = _effort(request.effort)
        try:
            mcp = stdio_mcp_servers(
                plan, bearer_token, gateway_endpoint=self._gateway_endpoint)
        except DroidMcpError as exc:
            raise DroidFailure(exc.code, str(exc), category="validation") from exc
        access_mode = str(request.access_mode or AccessMode.SUPERVISED.value)
        live = _Live(
            agent_session_id=request.agent_session_id,
            cwd=cwd,
            env=env,
            access_mode=access_mode,
            thread_id=request.thread_id,
            mcp=mcp,
            mcp_servers=tuple(
                StdioMcpServerConfig(
                    name=str(item["name"]),
                    command=str(item["command"]),
                    args=tuple(str(arg) for arg in item.get("args") or ()),
                    env={str(row["name"]): str(row["value"]) for row in item.get("env") or ()},
                )
                for item in mcp.servers if item.get("command") and item.get("name")
            ),
            grants=SessionApprovalGrants(access_mode),
            requested_model=str(request.model or "").strip(),
            requested_effort=effort.value if effort is not None else "",
            resumed=bool(request.resume_handle),
        )
        live.interactions = InteractionHandlers(
            on_permission=lambda req: self._on_permission(live, req),
            on_question=lambda req: self._on_question(live, req),
        )
        self._live[request.agent_session_id] = live
        try:
            await self._open_native(
                live, resume_id=str(request.resume_handle or "") or None, effort=effort)
            await self._apply_access_mode(live)
            if live.resumed:
                await self._apply_selection(live, effort)
        except BaseException as exc:
            await self._abandon(live)
            failure = self._start_failure(live, exc)
            if failure is exc:
                raise
            raise failure from exc
        settings = live.session.settings
        live.confirmed_model = str(settings.model or "")
        live.confirmed_effort = str(getattr(settings.reasoning_effort, "value", "") or "")
        return {"external_session_id": live.external_id, "resume_handle": live.external_id}

    def _start_failure(self, live: _Live, exc: BaseException) -> BaseException:
        """Typed start failure; anything not Droid's own propagates unchanged."""
        if isinstance(exc, (DroidFailure, asyncio.CancelledError)):
            return exc
        stderr = live.transport.stderr_text() if live.transport is not None else ""
        detail = f"{type(exc).__name__}: {exc}" + (f"\n{stderr}" if stderr else "")
        if isinstance(exc, SessionNotFoundError):
            return DroidFailure(
                "droid.resume_failed", "Droid refused to resume the saved session",
                category="provider", detail=detail)
        if isinstance(exc, InvalidWorkingDirectoryError):
            return DroidFailure(
                "droid.cwd_invalid", f"Working directory is not usable: {live.cwd}",
                category="validation", detail=detail)
        if isinstance(exc, FileNotFoundError):
            return DroidFailure(
                "droid.binary_missing", f"Droid executable was not found: {self._binary}",
                category="transport", detail=detail)
        if isinstance(exc, DroidProcessError):
            return DroidFailure(
                "droid.process_exited", "Droid exited while the session was opening",
                category="runtime_exited", detail=detail)
        if isinstance(exc, DroidError):
            return DroidFailure(
                "droid.session_open_failed", f"Droid could not open the session: {exc}",
                category="provider", detail=detail)
        return exc

    async def _open_native(
        self, live: _Live, *, resume_id: Optional[str], effort: Optional[ReasoningEffort],
    ) -> None:
        spare = self._take_spare(live.cwd, live.env)
        transport = spare or _SupervisedDroidTransport(
            self._argv(),
            cwd=live.cwd,
            env=live.env,
            adapter_id=self.identity.adapter_id,
            session_id=live.agent_session_id,
            log_root=self._log_root,
        )
        live.transport = transport
        if spare is None:
            await transport.connect()
        runtime = Runtime(transport=transport)
        if resume_id:
            session = Session.resume(
                resume_id,
                interactions=live.interactions,
                mcp_servers=live.mcp_servers,
                runtime=runtime,
            )
        else:
            host = self.host_context(live.agent_session_id)
            session = Session(
                cwd=live.cwd,
                model=live.requested_model or None,
                reasoning_effort=effort,
                config=SessionConfig(
                    autonomy=_AUTONOMY[live.access_mode],
                    mcp_servers=live.mcp_servers,
                    # Droid stores the creation-time prompt with the session;
                    # Session.resume keeps it.
                    system_prompt=({"type": "preset", "preset": "droid",
                                    "append": system_append(host)} if host else None),
                ),
                interactions=live.interactions,
                runtime=runtime,
            )
        try:
            await session.open()
        except BaseException:
            await transport.close()
            raise
        live.session = session
        live.external_id = str(session.id)

    async def _apply_access_mode(self, live: _Live) -> None:
        """Make the open session's autonomy equal the Muteki access mode."""
        session = live.session
        expected = _AUTONOMY[live.access_mode]
        if session.settings.autonomy is not expected:
            await session.update_settings(autonomy=expected)
        if session.settings.mode is Mode.SPEC:
            await session.update_settings(mode=Mode.AUTO)
        if session.settings.autonomy is not expected:
            raise DroidFailure(
                "droid.autonomy_unconfirmed",
                f"Droid autonomy is {session.settings.autonomy}, access mode "
                f"{live.access_mode} needs {expected.value}",
                category="provider",
            )

    async def _apply_selection(self, live: _Live, effort: Optional[ReasoningEffort]) -> None:
        """A resumed session keeps its saved model; apply an explicit choice."""
        session = live.session
        changes: dict[str, Any] = {}
        if live.requested_model and session.settings.model != live.requested_model:
            changes["model"] = live.requested_model
        if effort is not None and session.settings.reasoning_effort is not effort:
            changes["reasoning_effort"] = effort
        if changes:
            await session.update_settings(**changes)

    async def _abandon(self, live: _Live) -> None:
        self._live.pop(live.agent_session_id, None)
        self._cancel_pending(live)
        await self._close_native(live)
        live.mcp.remove()

    async def _close_native(self, live: _Live) -> None:
        session, live.session = live.session, None
        if session is not None:
            try:
                await session.close()
            except DroidProcessError:
                # The process is already gone; nothing is left to close.
                _LOG.debug("droid session close after process exit", exc_info=True)
            except Exception:
                _LOG.warning("droid session close failed", exc_info=True)
        if live.transport is not None:
            await live.transport.close()

    async def _teardown(self, session: AgentSessionRef) -> str:
        from .sessions import EXIT_CLOSED
        live = self._live.pop(session.agent_session_id, None)
        if live is not None:
            self._cancel_pending(live)
            live.grants.clear()
            await self._close_native(live)
            live.mcp.remove()
        return EXIT_CLOSED

    async def _reopen(self, live: _Live) -> None:
        """Attach the saved Droid session again after its process died.

        Only the id already stored is resumed. A missing or different id is
        ``droid.resume_failed``; no fresh session is opened.
        """
        expected = live.external_id
        await self._close_native(live)
        if not expected:
            raise DroidFailure(
                "droid.resume_failed", "Droid process died before a session id was saved")
        try:
            await self._open_native(live, resume_id=expected, effort=None)
            if live.external_id != expected:
                raise DroidFailure(
                    "droid.resume_failed", "Droid resumed a different session")
            await self._apply_access_mode(live)
        except BaseException as exc:
            await self._close_native(live)
            failure = self._start_failure(live, exc)
            if failure is exc:
                raise
            raise failure from exc

    # -- control ---------------------------------------------------------------

    def send(self, session: AgentSessionRef, input: AgentInput) -> AsyncIterator[AgentEvent]:
        if isinstance(input, ApprovalResponseInput):
            return self._approval_response(session, input)
        if isinstance(input, UserInputResponseInput):
            return self._question_response(session, input)
        if isinstance(input, MessageInput):
            return self._turn_stream(session, input, resumed=False)
        return self.unsupported_input_stream(session, input)

    def resume(self, session: AgentSessionRef) -> AsyncIterator[AgentEvent]:
        live = self._live.get(session.agent_session_id)
        if live is None or live.session is None:
            return self._unsupported_stream(session, "resume", "resume")
        return self._turn_stream(
            session, MessageInput(text="Continue from where you left off."), resumed=True)

    async def runtime_capability_snapshot(
        self, session: Optional[AgentSessionRef] = None,
    ) -> RuntimeCapabilitySnapshot:
        """Skills, custom slash commands and MCP status as Droid reports them.

        Every section is read from the open session over JSON-RPC, so project
        and personal skill directories, disabled skills and managed settings
        are Droid's own answer. A section that cannot be read is reported in
        ``diagnostics``; it is never replaced by a guessed catalog.
        """
        base = await super().runtime_capability_snapshot(session)
        if session is None:
            return base
        live = self._live.get(session.agent_session_id)
        if live is None or live.session is None:
            from .command_providers import detached_compaction_snapshot, operation_item
            return detached_compaction_snapshot(base.model_copy(update={
                "stale": True,
                "diagnostics": ["Droid Session 当前不在本进程，动态能力目录已失效"],
            }), operation_item(self.id, "droid", "compact", "droid.compact_session",
                               "压缩当前 Droid 会话上下文"))
        native = live.session
        items = list(base.items)
        diagnostics: list[str] = []
        engine = self.identity.adapter_id.split(".", 1)[0]

        async def read(label: str, request: Any) -> Any:
            try:
                return await asyncio.wait_for(request, _CATALOG_REQUEST_TIMEOUT_S)
            except (DroidError, asyncio.TimeoutError) as exc:
                diagnostics.append(f"{label} 读取失败：{type(exc).__name__}: {exc}")
                return None

        skills = await read("droid.list_skills", native.list_skills())
        for skill in (skills.skills if skills is not None else ()):
            if skill.enabled is False or skill.user_invocable is False:
                continue
            items.append(dynamic_command_item(
                adapter_id=self.id, engine=engine, name=skill.name,
                description=skill.description or "", channel="provider_native",
                kind="skill",
                invocation={
                    "command": skill.name,
                    "wire_text": f"/{skill.name}",
                    "protocol": "droid.add_user_message",
                    "location": skill.location,
                    "path": skill.file_path,
                },
            ))
        # The high-level Session has no list_commands; the pinned droid-sdk
        # client carries the `droid.list_commands` request.
        commands = await read(
            "droid.list_commands", native._require_client().list_commands())
        for command in (commands.commands if commands is not None else ()):
            items.append(dynamic_command_item(
                adapter_id=self.id, engine=engine, name=command.name,
                description=command.description,
                argument_hint=command.argument_hint or "",
                channel="provider_native",
                invocation={
                    "command": command.name,
                    "wire_text": f"/{command.name}",
                    "protocol": "droid.add_user_message",
                },
            ))
        from .command_providers import operation_item
        items = [item for item in items if item.name != "compact"]
        items.append(operation_item(
            self.id, engine, "compact", "droid.compact_session",
            "压缩当前 Droid 会话上下文", argument_hint="[摘要要求]"))
        servers = await read("droid.list_mcp_servers", native.list_mcp_servers())
        if servers is not None:
            reported = {server.name for server in servers.servers}
            items = [
                item for item in items
                if not (item.kind == "mcp_status" and item.name in reported)
            ]
            for server in servers.servers:
                detail = [f"Runtime 已报告 · {server.status.value}"]
                if server.tool_count is not None:
                    detail.append(f"{server.tool_count} 个工具")
                if server.error:
                    detail.append(server.error)
                items.append(RuntimeCapabilityItem(
                    id=f"runtime:{self.id}:mcp:{server.name}",
                    kind="mcp_status", name=server.name,
                    description=" · ".join(detail), source="Droid",
                    scope="session", engine=engine, channel="provider_native",
                    resolution="runtime", origin="dynamic", delivery="guaranteed",
                    verification="verified", status="runtime_reported",
                    invocation={
                        "status": server.status.value,
                        "tool_count": server.tool_count,
                        "source": server.source,
                    },
                ))

        digest = sha256(json.dumps(
            [item.model_dump(mode="json") for item in items],
            sort_keys=True, default=str,
        ).encode()).hexdigest()
        if digest != live.catalog_digest:
            live.catalog_digest = digest
            live.catalog_revision += 1
        return base.model_copy(update={
            "revision": live.catalog_revision,
            "items": items,
            "diagnostics": diagnostics,
        })

    async def steer(self, session: AgentSessionRef, input: AgentInput) -> CommandReceipt:
        del input
        return self.unsupported_receipt(
            "steer", "droid.steer_unsupported", session=session,
        )

    async def interrupt(self, session: AgentSessionRef) -> CommandReceipt:
        self._mark_turn_interrupted(session.agent_session_id)
        live = self._live.get(session.agent_session_id)
        aggregate = AggregateRef(type="agent_session", id=session.agent_session_id)
        if live is None or live.session is None:
            return self.unsupported_receipt("interrupt", "droid.session_missing", session=session)
        if not live.turn_active:
            return CommandReceipt(
                command_id=new_id("cmd"), state=ReceiptState.COMPLETED, aggregate=aggregate)
        return await self._interrupt_native(live, aggregate)

    async def _interrupt_native(
        self, live: _Live, aggregate: Optional[AggregateRef] = None,
    ) -> CommandReceipt:
        aggregate = aggregate or AggregateRef(type="agent_session", id=live.agent_session_id)
        # A pending approval or question blocks Droid's own turn; answer them
        # first so the interrupt can take effect.
        self._cancel_pending(live)
        try:
            await live.session.interrupt()
        except DroidError as exc:
            return CommandReceipt(
                command_id=new_id("cmd"), state=ReceiptState.FAILED, aggregate=aggregate,
                error=ErrorEnvelope(
                    code="droid.interrupt_failed",
                    message=f"Droid interrupt failed: {exc}",
                    category=ErrorCategory.RUNTIME,
                    detail={"exception": type(exc).__name__},
                    retryable=True,
                ),
            )
        return CommandReceipt(
            command_id=new_id("cmd"), state=ReceiptState.COMPLETED, aggregate=aggregate)

    # -- turn --------------------------------------------------------------------

    def _failure_for(self, exc: BaseException, *, delivery_unknown: bool = False) -> AgentFailure:
        if isinstance(exc, DroidProcessError):
            return self.exception_failure(
                exc, FailureCategory.RUNTIME_EXITED, "runtime_exited",
                message="Droid process exited before the turn finished",
                delivery_unknown=delivery_unknown)
        if isinstance(exc, DroidConnectionError):
            return self.exception_failure(
                exc, FailureCategory.TRANSPORT, "droid.connection",
                message="Droid connection failed", delivery_unknown=delivery_unknown)
        if isinstance(exc, RunTimeoutError):
            return self.exception_failure(
                exc, FailureCategory.TIMEOUT, "timeout", message="Droid request timed out",
                retryable=True, delivery_unknown=delivery_unknown)
        if isinstance(exc, SessionBusyError):
            return self.exception_failure(
                exc, FailureCategory.VALIDATION, "droid.turn_busy",
                message="Droid session already has an active turn")
        if isinstance(exc, DroidProtocolError):
            return self.failure(
                FailureCategory.PROVIDER, "droid.protocol",
                message=f"Droid rejected the request: {exc.message}",
                detail=f"{type(exc).__name__}: {exc}",
                native_code=str(exc.code) if exc.code is not None else None,
                delivery_unknown=delivery_unknown)
        if isinstance(exc, DroidError):
            return self.exception_failure(
                exc, FailureCategory.PROVIDER, "droid.error",
                message=f"Droid failed: {exc}", delivery_unknown=delivery_unknown)
        return self.exception_failure(
            exc, FailureCategory.UNKNOWN, "droid.internal",
            message=f"Droid adapter failed: {type(exc).__name__}",
            delivery_unknown=delivery_unknown)

    def _failed_turn(
        self, seq: Any, common: dict[str, Any], failure: AgentFailure, native_type: str,
    ) -> AgentEvent:
        return self.emit(build_event(
            AgentEventType.TURN_FAILED, seq, native_type=native_type,
            payload=TurnFailedPayload(error=failure), **common))

    async def _turn_stream(
        self, session: AgentSessionRef, input: MessageInput, *, resumed: bool,
    ) -> AsyncIterator[AgentEvent]:
        seq = self.sequencer_for(session.agent_session_id)
        record = self.session_record(session.agent_session_id)
        live = self._live.get(session.agent_session_id)
        common: dict[str, Any] = dict(
            agent_session_id=session.agent_session_id,
            external_session_id=live.external_id if live else session.external_session_id,
            run_id=record.run_id if record else None,
            execution_generation=record.execution_generation if record else None,
            turn_id=new_id("turn"),
        )
        if live is None or live.session is None:
            yield self._failed_turn(seq, common, self.failure(
                FailureCategory.RUNTIME_EXITED, "droid.session_missing",
                message="Droid session is not open"), "droid.session_missing")
            return
        if live.turn_active:
            yield self._failed_turn(seq, common, self.failure(
                FailureCategory.VALIDATION, "droid.turn_busy",
                message="Droid session already has an active turn"), "droid.turn_busy")
            return
        if input.payload.interaction_mode == "plan":
            yield self._failed_turn(seq, common, self.failure(
                FailureCategory.UNSUPPORTED, "plan_mode_unsupported",
                message="droid.rpc does not map Muteki plan mode onto Droid spec mode"),
                "droid.plan_mode_unsupported")
            return
        live.turn_active = True
        try:
            if live.transport.exited:
                try:
                    await self._reopen(live)
                except DroidFailure as exc:
                    yield self._failed_turn(seq, common, self.failure(
                        exc.category, exc.code, message=str(exc), detail=exc.detail,
                    ), exc.code)
                    return
                common["external_session_id"] = live.external_id
            for event in self._announce(live, seq, common, resumed):
                yield event
            yield self.emit(build_event(
                AgentEventType.TURN_STARTED, seq, native_type="droid.turn.start",
                payload=TurnStartedPayload(kind=input.kind), **common))
            limit = self.conversation_turn_timeout(live.thread_id, self._turn_timeout_s)

            async def abort(_failure: AgentFailure) -> None:
                await self._interrupt_native(live)

            runner = TurnRunner(
                self, session, turn_id=common["turn_id"],
                limits=TurnLimits(idle_s=limit, overall_s=limit),
                exit_watch=live.transport.wait_exit,
                on_abort=abort,
                diagnostics=live.transport.stderr_text,
                auto_ack=False,
                run_id=common["run_id"],
                execution_generation=common["execution_generation"],
            )
            async for event in runner.stream(
                    self._turn_events(live, input, seq, common, runner)):
                yield event
        finally:
            live.turn_active = False

    def _announce(
        self, live: _Live, seq: Any, common: dict[str, Any], resumed: bool,
    ) -> list[AgentEvent]:
        """Once per session: session start/resume, MCP gaps, model mismatch."""
        if live.announced:
            return []
        live.announced = True
        events = [self.emit(build_event(
            AgentEventType.SESSION_RESUMED if live.resumed or resumed
            else AgentEventType.SESSION_STARTED,
            seq,
            native_type="droid.session.resumed" if live.resumed or resumed
            else "droid.session.started",
            payload=SessionPayload(
                transport="stream-jsonrpc", adapter_id=self.id,
                instance_id=self.identity.instance_id, cwd=live.cwd,
                model=live.confirmed_model or None),
            **{key: value for key, value in common.items() if key != "turn_id"}))]
        for name in live.mcp.dropped:
            events.append(self.emit(build_event(
                AgentEventType.RUNTIME_WARNING, seq,
                native_type="droid.mcp.http_not_injected",
                payload=RuntimeWarningPayload(
                    kind="degraded", code="droid.mcp.http_not_injected",
                    message=f"HTTP MCP server {name} was not injected; Droid chat uses stdio"),
                **common)))
        mismatches = []
        if live.requested_model and live.confirmed_model != live.requested_model:
            mismatches.append(
                f"model requested {live.requested_model}, Droid uses {live.confirmed_model}")
        if live.requested_effort and live.confirmed_effort != live.requested_effort:
            mismatches.append(
                f"reasoning effort requested {live.requested_effort}, "
                f"Droid uses {live.confirmed_effort}")
        if mismatches:
            events.append(self.emit(build_event(
                AgentEventType.RUNTIME_WARNING, seq,
                native_type="droid.selection_mismatch",
                payload=RuntimeWarningPayload(
                    kind="config", code="droid.selection_mismatch",
                    message="; ".join(mismatches)),
                **common)))
        return events

    async def _consume(self, live: _Live, text: str, queue: "asyncio.Queue[tuple[str, Any]]") -> None:
        def usage_notification(notification: Mapping[str, object]) -> None:
            kind = notification.get("type")
            if kind == "session_token_usage_changed":
                usage = notification.get("inclusiveTokenUsage") or notification.get("tokenUsage")
            elif kind == "agent_turn_completed":
                usage = notification.get("cumulativeTokenUsage")
                if not isinstance(usage, Mapping):
                    queue.put_nowait(("usage_warning", RuntimeWarningPayload(
                        kind="degraded", code="droid.usage.cumulative_missing",
                        message="Droid did not report terminal cumulative usage; only observed session updates are counted.")))
                    return
            elif kind == "session_compacted":
                removed = notification.get("removedCount")
                queue.put_nowait(("native", (
                    AgentEventType.COMPACTION_UPDATED, "droid.session_compacted",
                    CompactionPayload(
                        status="completed", trigger="auto",
                        message=f"Droid 移除了 {removed} 条消息" if isinstance(removed, int) else None))))
                return
            else:
                return
            if isinstance(usage, Mapping):
                queue.put_nowait(("usage", self._usage(usage)))

        # SDK 0.5 exposes session-wide partials but per-turn terminal results;
        # its result fallback can also be a session total. Native notifications
        # retain the explicit cumulative field, including delegated work.
        unsubscribe = None
        try:
            unsubscribe = live.session.on_notification(usage_notification)
            stream = live.session.stream(text, include_partial_messages=True, timeout=None)
            async for event in stream:
                queue.put_nowait(("event", event))
        except asyncio.CancelledError:
            raise
        except Exception as exc:
            queue.put_nowait(("error", exc))
        finally:
            if unsubscribe is not None:
                unsubscribe()
            queue.put_nowait(("end", None))

    async def _turn_events(
        self, live: _Live, input: MessageInput, seq: Any, common: dict[str, Any],
        runner: TurnRunner,
    ) -> AsyncIterator[AgentEvent]:
        queue: asyncio.Queue[tuple[str, Any]] = asyncio.Queue()
        live.queue = queue
        live.text = []
        live.turn_closed = False
        runner.mark_sent()
        text = text_with_envelopes(
            input.text, self.turn_context(live.agent_session_id, input, host=False))
        task = asyncio.create_task(self._consume(live, text, queue))
        try:
            while True:
                kind, payload = await queue.get()
                if kind == "end":
                    return
                if kind == "error":
                    failure = self._failure_for(payload, delivery_unknown=runner.delivery_unknown)
                    yield self._failed_turn(seq, common, failure, "droid.turn.error")
                    if isinstance(payload, DroidProcessError):
                        yield self.emit(build_event(
                            AgentEventType.RUNTIME_EXITED, seq, native_type="droid.process.exit",
                            payload=RuntimeExitedPayload(
                                classification="failed", exit_code=payload.exit_code,
                                error=failure),
                            **common))
                    return
                if kind == "event":
                    runner.ack()
                    if isinstance(payload, RunSuccess) and not live.turn_closed:
                        # Must precede the terminal event that closes the turn.
                        yield await self._context_event(live, seq, common)
                    for event in self._map_sdk(live, seq, common, payload):
                        yield event
                    continue
                if kind in {"usage", "usage_warning"}:
                    yield self.emit(build_event(
                        AgentEventType.USAGE_UPDATED if kind == "usage" else AgentEventType.RUNTIME_WARNING,
                        seq, native_type="droid.session_usage", payload=payload, **common))
                    continue
                yield self.emit(build_event(
                    payload[0], seq, native_type=payload[1], payload=payload[2], **common))
        finally:
            live.queue = None
            self._cancel_pending(live)
            if not task.done():
                task.cancel()
            await asyncio.gather(task, return_exceptions=True)

    async def _context_event(self, live: _Live, seq: Any, common: dict[str, Any]) -> AgentEvent:
        """``droid.get_context_stats``: Droid's own context meter."""
        native = "droid.get_context_stats"
        try:
            stats = await asyncio.wait_for(live.session.context(), _CATALOG_REQUEST_TIMEOUT_S)
        except (DroidError, asyncio.TimeoutError) as exc:
            return self.emit(build_event(
                AgentEventType.RUNTIME_WARNING, seq, native_type=native,
                payload=RuntimeWarningPayload(
                    kind="degraded", code="droid.context_usage_unavailable",
                    message=f"Droid get_context_stats failed: {type(exc).__name__}: {exc}"),
                **common))
        return self.emit(build_event(
            AgentEventType.USAGE_UPDATED, seq, native_type=native,
            payload=UsagePayload(
                scope="context_only", context_used_tokens=max(0, stats.used),
                context_window=stats.limit if stats.limit > 0 else None,
                native={"accuracy": stats.accuracy.value}),
            **common))

    async def runtime_operation(
        self, session: AgentSessionRef, name: str, arguments: str = "",
    ) -> dict[str, Any]:
        """``/compact``: ``droid.compact_session`` replaces the native session
        with a summarized successor that keeps this connection."""
        live = self._live.get(session.agent_session_id)
        if live is None or live.session is None:
            raise DroidFailure("droid.session_missing", "Droid Session 当前不在本进程")
        if live.turn_active:
            raise DroidFailure("droid.session_busy", "请等待当前回复结束后执行命令")
        if name != "compact":
            raise DroidFailure("droid.operation_unsupported", f"Droid 不支持操作 /{name}")
        before = await live.session.context()
        outcome = await live.session.compact(instructions=arguments or None)
        live.session = outcome.session
        live.external_id = str(outcome.session.id)
        self._tracker.activate(
            session.agent_session_id, external_session_id=live.external_id,
            resume_handle=live.external_id)
        self._context_compacted(session.agent_session_id)
        after = await live.session.context()
        # Right after compact_session an "estimated" reading still carries the
        # pre-compaction figure (updated_at is just the call time); only an
        # exact measurement describes the successor session.
        if after.accuracy is ContextAccuracy.EXACT:
            return {
                "status": "completed",
                "message": f"Droid 已压缩上下文，移除 {outcome.removed_count} 条消息",
                "removed_count": outcome.removed_count,
                "compaction": {"tokens_before": before.used, "tokens_after": after.used},
                "context": {"used": after.used, "limit": after.limit},
            }
        return {
            "status": "completed",
            "message": (f"Droid 已压缩上下文，移除 {outcome.removed_count} 条消息；"
                        "压缩后的用量会在下一轮回复后更新"),
            "removed_count": outcome.removed_count,
            "compaction": {"tokens_before": before.used},
        }

    def _map_sdk(
        self, live: _Live, seq: Any, common: dict[str, Any], event: Any,
    ) -> list[AgentEvent]:
        def make(event_type: AgentEventType, native_type: str, payload: Any) -> AgentEvent:
            return self.emit(build_event(
                event_type, seq, native_type=native_type, payload=payload, **common))

        if isinstance(event, TextDelta):
            if not event.text:
                return []
            live.text.append(event.text)
            return [make(
                AgentEventType.MESSAGE_DELTA, "droid.text.delta",
                MessageDeltaPayload(text=event.text, message_id=event.message_id))]
        if isinstance(event, ThinkingDelta):
            if not event.text:
                return []
            return [make(
                AgentEventType.REASONING_SUMMARY, "droid.thinking.delta",
                ReasoningPayload(
                    text=event.text, channel="thinking", partial=True,
                    item_id=f"{event.message_id}:{event.block_index}"))]
        if isinstance(event, ToolCall):
            return [make(
                AgentEventType.TOOL_STARTED, "droid.tool_call",
                ToolPayload(
                    tool_call_id=event.tool_use_id, name=event.name,
                    kind=_TOOL_KINDS.get(event.name),
                    input=_thaw(event.input), status="running"))]
        if isinstance(event, ToolProgress):
            if not event.content:
                return []
            return [make(
                AgentEventType.TOOL_PROGRESS, "droid.tool_progress",
                ToolPayload(
                    tool_call_id=event.tool_use_id, name=event.tool_name,
                    kind=_TOOL_KINDS.get(event.tool_name),
                    chunk=event.content, status="running"))]
        if isinstance(event, ToolResult):
            output = _thaw(event.content)
            return [make(
                AgentEventType.TOOL_COMPLETED, "droid.tool_result",
                ToolPayload(
                    tool_call_id=event.tool_use_id, name=event.tool_name,
                    kind=_TOOL_KINDS.get(event.tool_name),
                    output=output,
                    status="failed" if event.is_error else "completed",
                    error=_json_text(output) if event.is_error else None))]
        if isinstance(event, TokenUsageUpdate):
            # The native notification already emitted this cumulative update.
            return []
        if isinstance(event, WorkingStateChanged):
            if event.state is WorkingState.COMPACTING_CONVERSATION:
                return [make(
                    AgentEventType.COMPACTION_UPDATED, "droid.working_state",
                    CompactionPayload(status="started", trigger="auto"))]
            return []
        if isinstance(event, ErrorEvent):
            return [make(
                AgentEventType.RUNTIME_WARNING, "droid.error_event",
                RuntimeWarningPayload(
                    kind="degraded",
                    code=f"droid.error.{getattr(event.error_type, 'value', event.error_type)}",
                    message=event.message or "Droid reported an error"))]
        if isinstance(event, SettingsUpdated):
            if event.settings.model:
                live.confirmed_model = str(event.settings.model)
            return []
        if isinstance(event, RunSuccess):
            if live.turn_closed:
                return []
            live.turn_closed = True
            events: list[AgentEvent] = []
            text = event.text or "".join(live.text)
            if text:
                events.append(make(
                    AgentEventType.MESSAGE_COMPLETED, "droid.message.completed",
                    MessageCompletedPayload(text=text)))
            events.append(make(
                AgentEventType.TURN_COMPLETED, "droid.turn.completed",
                TurnCompletedPayload(duration_ms=int(event.duration.total_seconds() * 1000))))
            return events
        if isinstance(event, (RunFailure, RunInterrupted)):
            if live.turn_closed:
                return []
            live.turn_closed = True
            if isinstance(event, RunInterrupted):
                failure = self.failure(
                    FailureCategory.CANCELLED, "interrupted", message="Droid turn was interrupted")
                return [make(
                    AgentEventType.TURN_FAILED, "droid.turn_interrupted",
                    TurnFailedPayload(error=failure))]
            error = event.error
            failure = self.failure(
                FailureCategory.PROVIDER, "turn_failed",
                message=(error.message if error is not None else "") or event.text
                or "Droid turn failed",
                native_code=(
                    str(getattr(error.error_type, "value", error.error_type))
                    if error is not None else str(event.subtype)))
            return [make(
                AgentEventType.TURN_FAILED, "droid.turn_failed",
                TurnFailedPayload(error=failure))]
        return []

    @staticmethod
    def _usage(usage: Mapping[str, Any]) -> UsagePayload:
        from muteki.core.usage import sum_token_buckets

        # Droid's provider bridges use disjoint input/cache buckets (the
        # OpenAI bridge subtracts cached_tokens before assigning inputTokens).
        # Output already includes provider reasoning; thinking is a detail.
        payload = UsagePayload(
            scope="session_cumulative",
            input_tokens=sum_token_buckets(
                usage.get("inputTokens"), usage.get("cacheReadTokens"), usage.get("cacheCreationTokens")),
            output_tokens=usage.get("outputTokens"),
            cached_input_tokens=usage.get("cacheReadTokens"),
            cache_write_tokens=usage.get("cacheCreationTokens"),
            reasoning_tokens=usage.get("thinkingTokens"),
        )
        if usage.get("factoryCredits") is not None:
            payload.native["factory_credits"] = usage["factoryCredits"]
        return payload

    # -- approvals and questions ---------------------------------------------------

    def _cancel_pending(self, live: _Live) -> None:
        """Answer every outstanding interaction so Droid's handlers return."""
        for pending in live.approvals.values():
            if pending.future.done():
                continue
            if _CANCEL in pending.offered:
                pending.future.set_result(_Choice(_CANCEL, "cancelled", automatic=True))
            else:
                pending.future.set_exception(DroidFailure(
                    "droid.approval.option_unavailable",
                    "Droid offered no cancel option for a pending permission request"))
        for pending_question in live.questions.values():
            if not pending_question.future.done():
                pending_question.future.set_result(pending_question.request.cancel())

    @staticmethod
    def _approval_fields(request: PermissionRequest) -> dict[str, Any]:
        """Kind and exact target of a permission request, from its actions."""
        commands: list[str] = []
        paths: list[str] = []
        files: list[WorkspaceFileChange] = []
        diffs: list[str] = []
        kind = "tool"
        title = "Droid permission"
        tool_use = None
        for action in request.actions:
            tool_use = tool_use or getattr(action, "tool_use", None)
            if isinstance(action, ExecuteAction):
                kind, title = "command_execution", "Run command"
                commands.append(action.full_command)
            elif isinstance(action, ApplyPatchAction):
                kind, title = "file_change", "Apply patch"
                diffs.append(action.patch_content)
                rows = action.files or ()
                for row in rows:
                    paths.append(row.file_path)
                    files.append(WorkspaceFileChange(
                        path=row.file_path,
                        change={"create": "add", "update": "modify",
                                "delete": "delete"}.get(row.operation)))
                if not rows:
                    paths.append(action.file_path)
            elif isinstance(action, (EditAction, CreateFile)):
                created = isinstance(action, CreateFile)
                kind, title = "file_change", "Create file" if created else "Edit file"
                paths.append(action.file_path)
                files.append(WorkspaceFileChange(
                    path=action.file_path, change="add" if created else "modify"))
            elif isinstance(action, ExitSpecModeAction):
                kind, title = "plan_exit", action.title or "Exit spec mode"
            elif isinstance(action, McpToolAction):
                kind, title = "mcp_tool_call", action.tool_name
            else:
                title = type(action).__name__
        fields: dict[str, Any] = {
            "approval_kind": kind,
            "title": title,
            "command": "\n".join(commands) or None,
            "paths": [path for path in dict.fromkeys(paths) if path],
            "files": files,
            "unified_diff": "\n".join(diffs) or None,
        }
        plan = request.plan
        if plan is not None:
            fields["plan_markdown"] = plan.text
        if tool_use is not None:
            fields["tool_call_id"] = tool_use.id
            fields["tool_name"] = tool_use.name
            if kind in {"mcp_tool_call", "tool"}:
                fields["input"] = _thaw(tool_use.input)
        return fields

    def _auto_allow(self, live: _Live, request: PermissionRequest, payload: dict[str, Any]) -> bool:
        """Whether this request is answered ``proceed_once`` without the operator."""
        if live.access_mode == AccessMode.FULL_ACCESS.value:
            return True
        if live.access_mode == AccessMode.AUTO_ACCEPT_EDITS.value:
            if request.actions and all(isinstance(a, _EDIT_ACTIONS) for a in request.actions):
                return True
        return live.grants.covers(payload, cwd=live.cwd)

    async def _on_permission(self, live: _Live, request: PermissionRequest) -> Any:
        offered = frozenset(item for item in map(_option_id, request.options) if item)
        approval_id = new_id("approval")
        fields = self._approval_fields(request)
        labels = {_option_id(option): str(option.label or "") for option in request.options}
        options = []
        if _ONCE in offered:
            options.append(ApprovalOption(
                option_id=_ONCE, label=labels[_ONCE], kind="allow_once"))
        if _CANCEL in offered:
            options.append(ApprovalOption(
                option_id=_CANCEL, label=labels[_CANCEL] or "Cancel", kind="reject_once"))
        can_remember = (
            _ONCE in offered and ApprovalTarget.from_payload(fields, cwd=live.cwd) is not None)
        payload = dump_payload(ApprovalRequestedPayload(
            approval_id=approval_id,
            options=options,
            scopes=["once", "session"] if can_remember else ["once"],
            cwd=live.cwd,
            **fields,
        ))
        from .src_policy import NativeToolError, authorize_native_tool, src_task_id
        if src_task_id(self._tracker._store, live.thread_id):
            record = self._tracker.get(live.agent_session_id)
            try:
                await authorize_native_tool(
                    self._tracker._store, thread_id=live.thread_id,
                    invocation_id=f"droid:{live.agent_session_id}:{approval_id}",
                    tool_name=str(fields.get("title") or "tool"),
                    arguments=dict(fields), action=str(fields.get("title") or "tool"),
                    approval_kind=str(fields.get("approval_kind") or "tool"),
                    adapter_id="droid.rpc", run_id=record.run_id if record else None, cwd=live.cwd,
                    environment_generation=(record.execution_generation or 0) if record else 0,
                )
            except NativeToolError:
                if _CANCEL not in offered:
                    raise DroidFailure(
                        "droid.approval.no_turn",
                        "SRC policy denied an action that cannot be cancelled")
                return request.respond(ToolConfirmationOutcome(_CANCEL))
            if _ONCE not in offered:
                raise DroidFailure(
                    "droid.approval.no_turn",
                    "SRC policy allowed an action but Droid has no once option")
            return request.respond(ToolConfirmationOutcome(_ONCE))
        if _ONCE in offered and self._auto_allow(live, request, payload):
            return request.respond(ToolConfirmationOutcome(_ONCE))
        queue = live.queue
        if queue is None:
            if _CANCEL not in offered:
                raise DroidFailure(
                    "droid.approval.no_turn",
                    "permission request arrived outside a turn and cannot be cancelled")
            return request.respond(ToolConfirmationOutcome(_CANCEL))
        future: asyncio.Future[_Choice] = asyncio.get_running_loop().create_future()
        live.approvals[approval_id] = _PendingApproval(
            future=future, offered=offered, payload=payload)
        queue.put_nowait(("raw", (AgentEventType.APPROVAL_REQUESTED, "droid.permission", payload)))
        try:
            choice = await future
        finally:
            live.approvals.pop(approval_id, None)
        queue.put_nowait(("raw", (
            AgentEventType.APPROVAL_RESOLVED, "droid.permission.resolved",
            ApprovalResolvedPayload(
                approval_id=approval_id, decision=choice.outcome, scope=choice.scope,
                option_id=choice.option_id, note=choice.note or None,
                automatic=choice.automatic))))
        return request.respond(
            ToolConfirmationOutcome(choice.option_id), comment=choice.note or None)

    async def _on_question(self, live: _Live, request: QuestionRequest) -> Any:
        request_id = new_id("ask")
        queue = live.queue
        if queue is None:
            return request.cancel()
        future: asyncio.Future[Any] = asyncio.get_running_loop().create_future()
        pending = _PendingQuestion(future=future, request=request)
        live.questions[request_id] = pending
        queue.put_nowait(("raw", (
            AgentEventType.USER_INPUT_REQUESTED, "droid.ask_user",
            dump_payload(UserInputRequestedPayload(
                request_id=request_id,
                user_input_kind="droid.ask_user",
                tool_call_id=request.tool_call_id,
                questions=[{
                    "index": question.index,
                    "question": question.question,
                    "header": question.topic,
                    "options": [{"label": option, "value": option} for option in question.options],
                    "multi_select": bool(question.multi_select),
                } for question in request.questions],
                response_actions=["submit", "cancel"],
            )))))
        try:
            response = await future
        finally:
            live.questions.pop(request_id, None)
        outcome = "cancelled" if response.cancelled else "answered"
        if pending.declined:
            outcome = "declined"
        queue.put_nowait(("raw", (
            AgentEventType.USER_INPUT_RESOLVED, "droid.ask_user.resolved",
            UserInputResolvedPayload(
                request_id=request_id, outcome=outcome,
                answers={str(a.index): a.answer for a in response.answers} or None))))
        return response

    def _control_error(
        self, session: AgentSessionRef, code: str, message: str,
    ) -> AgentEvent:
        return self.emit(build_event(
            AgentEventType.RUNTIME_ERROR,
            self.sequencer_for(session.agent_session_id),
            agent_session_id=session.agent_session_id,
            external_session_id=session.external_session_id,
            native_type=code,
            payload=RuntimeErrorPayload(error=self.failure(
                FailureCategory.VALIDATION, code, message=message)),
        ))

    async def _approval_response(
        self, session: AgentSessionRef, input: ApprovalResponseInput,
    ) -> AsyncIterator[AgentEvent]:
        """Settle a pending approval. The resolved event is emitted by the turn."""
        live = self._live.get(session.agent_session_id)
        decision = ApprovalDecision.from_payload(input.payload.model_dump())
        pending = live.approvals.get(decision.approval_id) if live else None
        if pending is None or pending.future.done():
            yield self._control_error(
                session, "droid.approval.stale", "Droid approval is not pending")
            return
        if decision.allowed:
            selected = decision.option_id or _ONCE
            allowed = selected == _ONCE and _ONCE in pending.offered
        else:
            selected = decision.option_id or _CANCEL
            allowed = selected == _CANCEL and _CANCEL in pending.offered
        if not allowed:
            yield self._control_error(
                session, "droid.approval.option_unavailable",
                "The approval reply is not an option Droid offered and Muteki allows "
                "(proceed_once to allow, cancel to deny)")
            return
        scope = "once"
        if decision.allowed and live.grants.remember(
                pending.payload, decision, cwd=live.cwd) is not None:
            scope = "session"
        pending.future.set_result(_Choice(
            option_id=selected,
            outcome="allow" if decision.allowed else "deny",
            scope=scope if decision.allowed else None,
            note=decision.note,
        ))

    async def _question_response(
        self, session: AgentSessionRef, input: UserInputResponseInput,
    ) -> AsyncIterator[AgentEvent]:
        live = self._live.get(session.agent_session_id)
        pending = live.questions.get(input.payload.request_id) if live else None
        if pending is None or pending.future.done():
            yield self._control_error(
                session, "droid.question.stale", "Droid question is not pending")
            return
        request = pending.request
        if input.payload.decision in {"cancel", "decline"}:
            pending.declined = input.payload.decision == "decline"
            pending.future.set_result(request.cancel())
            return
        answers = []
        provided = input.payload.answers or {}
        for question in request.questions:
            value = provided.get(str(question.index), provided.get(question.question, input.text))
            if value is None or value == "":
                continue
            if isinstance(value, list):
                answers.append(question.answer_multiple([str(item) for item in value]))
            else:
                answers.append(question.answer(str(value)))
        pending.future.set_result(request.submit(answers))
