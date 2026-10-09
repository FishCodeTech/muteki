"""capability probe 框架与注入方式选择（RUNTIME-01，任务书 6.5 / 7.2）。

probe 原则：

- 能力以**实测信号**为准（binary 可运行、``--version`` 可取、方法覆写
  检测、哨兵 argv 构造），不按 Runtime 名称推断；
- 每个字段标注来源：``probe``（本机实测）、``adapter_reported``
  （Adapter/Driver 声明）、``static``（保守默认，未实测一律 False）；
- 记录 ``transport_kind``、``protocol_version``、``runtime_version``
  与整体 ``schema_version``（契约基类字段）；
- structured transport 不可用时降级必须显式写入
  ``CapabilityProbeReport.degradations``，不允许静默关闭审批、恢复、
  来源追踪或 cwd 隔离。
"""

from __future__ import annotations

import asyncio
import errno
import os
import signal
import subprocess
import time
from dataclasses import dataclass, field, replace
from datetime import datetime
from enum import Enum
from typing import Any, Mapping, Sequence

from muteki.platform.contracts.base import utcnow
from muteki.platform.contracts.capabilities import InjectionKind
from muteki.platform.contracts.external_agents import AgentCapabilities

#: 能力来源标注。
SOURCE_PROBE = "probe"
SOURCE_REPORTED = "adapter_reported"
SOURCE_STATIC = "static"

#: AgentCapabilities 中的布尔能力字段（探测覆盖面）。
BOOL_CAPABILITY_FIELDS = (
    "streaming", "resume", "steer", "interrupt", "approval", "user_input",
    "fork", "structured_output", "subagents", "skills", "mcp",
    "native_tool_binding", "acp_mcp_config", "agent_plugin",
    "structured_http_rpc", "tool_events", "usage_events",
    "session_persistence", "plan", "plan_mode", "image_input", "compaction",
)


class AccessModeUnsupportedError(ValueError):
    """A session requested an access mode the adapter cannot honor natively.

    Raised at launch instead of silently substituting another mode, so the
    caller sees which modes this adapter actually enforces.
    """

    code = "external_agent.access_mode_unsupported"

    def __init__(
        self,
        adapter_id: str,
        access_mode: str,
        supported: "tuple[str, ...] | list[str]",
        reason: str = "",
    ) -> None:
        self.adapter_id = adapter_id
        self.access_mode = access_mode
        self.supported = tuple(supported)
        self.reason = reason
        message = (
            f"[{self.code}] {adapter_id} does not support access mode "
            f"{access_mode!r}; supported: {', '.join(self.supported) or 'none'}"
        )
        if reason:
            message = f"{message}. {reason}"
        super().__init__(message)


def require_access_mode(
    adapter_id: str,
    access_mode: "str | None",
    supported: "tuple[str, ...] | list[str]",
    *,
    reasons: "dict[str, str] | None" = None,
) -> None:
    """Raise ``AccessModeUnsupportedError`` unless ``access_mode`` is honored.

    ``None``/empty means the caller did not choose a Conversation access mode
    (Worker launches); adapters keep their unattended behavior for it.
    """
    if not access_mode or access_mode in supported:
        return
    raise AccessModeUnsupportedError(
        adapter_id, access_mode, supported,
        (reasons or {}).get(access_mode, ""),
    )


@dataclass
class CapabilityProbeReport:
    """一次 capability probe 的完整记录。

    ``capabilities`` 是契约对象；``field_sources`` 逐字段标注来源；
    ``degradations`` 显式列出结构化传输不可用导致的功能降级。
    """

    adapter_id: str
    instance_id: str
    capabilities: AgentCapabilities
    probed_at: datetime = field(default_factory=utcnow)
    binary_path: str = ""
    field_sources: dict[str, str] = field(default_factory=dict)
    degradations: list[str] = field(default_factory=list)
    detail: str = ""
    healthy_override: bool | None = None
    # Discovery evidence, not proof that the credential can complete inference.
    # Only the caller that selected the credential may persist this catalog.
    model_catalog: dict[str, Any] | None = None

    @property
    def healthy(self) -> bool:
        """binary 可用且取得了版本号即视为健康（不做真实模型调用）。"""
        if self.healthy_override is not None:
            return self.healthy_override
        return bool(self.binary_path) and bool(self.capabilities.runtime_version)


def conservative_capabilities(
    *,
    transport_kind: str = "",
    capability_source: str = SOURCE_STATIC,
) -> AgentCapabilities:
    """保守默能力快照：全部布尔能力 False（未实测不声明）。"""
    return AgentCapabilities(
        transport_kind=transport_kind,
        capability_source=capability_source,
    )


def _defined_by(instance: Any, method: str) -> str:
    """返回 ``method`` 在 MRO 中定义类的名字（用于覆写检测）。"""
    for klass in type(instance).__mro__:
        if method in klass.__dict__:
            return klass.__name__
    return ""


#: Upper bound for one read-only probe command (``--version``, login status).
PROBE_COMMAND_TIMEOUT_S = 15.0


class ProbeCommandErrorCode(str, Enum):
    """Stable codes for a failed probe command; callers branch on these."""

    NOT_INSTALLED = "probe.command.not_installed"
    LAUNCH_FAILED = "probe.command.launch_failed"
    TIMEOUT = "probe.command.timeout"
    NONZERO_EXIT = "probe.command.nonzero_exit"
    EMPTY_OUTPUT = "probe.command.empty_output"


@dataclass(frozen=True)
class ProbeCommandResult:
    """Complete record of one probe command, including full stdout/stderr."""

    argv: tuple[str, ...]
    returncode: int | None
    stdout: str
    stderr: str
    elapsed_s: float
    error_code: ProbeCommandErrorCode | None = None
    error_detail: str = ""

    @property
    def ok(self) -> bool:
        return self.error_code is None

    @property
    def version(self) -> str:
        """First non-empty output line of a successful version command.

        This parses the version token; the complete output stays on
        ``stdout``/``stderr``.
        """
        if self.returncode != 0:
            return ""
        for stream in (self.stdout, self.stderr):
            for line in stream.splitlines():
                if line.strip():
                    return line.strip()
        return ""

    def describe(self) -> str:
        """Human-readable failure with the full captured output."""
        if self.ok:
            return ""
        parts = [f"[{self.error_code.value}] {' '.join(self.argv)}: {self.error_detail}"]
        if self.stdout.strip():
            parts.append(f"stdout:\n{self.stdout.strip()}")
        if self.stderr.strip():
            parts.append(f"stderr:\n{self.stderr.strip()}")
        return "\n".join(parts)


def _kill_probe_group(proc: "asyncio.subprocess.Process") -> None:
    # The child is unreaped (returncode is None), so its pid and process
    # group cannot have been reused; npm/node shims leave grandchildren in it.
    if proc.returncode is not None:
        return
    try:
        os.killpg(proc.pid, signal.SIGKILL)
    except ProcessLookupError:
        return


#: After killing a timed-out probe group, how long to wait for its pipes.
_PROBE_PIPE_DRAIN_S = 5.0


async def _drain_after_kill(
    communicate: "asyncio.Future[tuple[bytes, bytes]]",
) -> tuple[bytes, bytes, str]:
    done, _ = await asyncio.wait({communicate}, timeout=_PROBE_PIPE_DRAIN_S)
    if done:
        stdout_b, stderr_b = communicate.result()
        return stdout_b, stderr_b, ""
    # A descendant that left the process group still holds the pipes.
    communicate.cancel()
    return b"", b"", (
        f"; output pipes still open {_PROBE_PIPE_DRAIN_S:g}s after the kill "
        "(a descendant left the process group), captured output unavailable")


async def run_probe_command(
    argv: Sequence[str],
    *,
    timeout: float = PROBE_COMMAND_TIMEOUT_S,
    env: Mapping[str, str] | None = None,
    cwd: str | None = None,
) -> ProbeCommandResult:
    """Run one read-only probe command without blocking the event loop.

    ``env=None`` uses the probe environment of the current task
    (``probe_environment.subprocess_environment``); an explicit mapping is
    passed to the child unchanged.  The child runs in its own session so a
    timeout kills the whole process group.  Failures are returned as typed
    results, never raised.
    """
    command = tuple(str(item) for item in argv)
    started = time.monotonic()
    if not command or not command[0]:
        return ProbeCommandResult(
            argv=command, returncode=None, stdout="", stderr="", elapsed_s=0.0,
            error_code=ProbeCommandErrorCode.NOT_INSTALLED,
            error_detail="no executable configured")
    if env is None:
        from .probe_environment import subprocess_environment
        child_env: dict[str, str] | None = subprocess_environment()
    else:
        child_env = dict(env)
    try:
        proc = await asyncio.create_subprocess_exec(
            *command,
            stdin=asyncio.subprocess.DEVNULL,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
            env=child_env,
            cwd=cwd or None,
            start_new_session=True,
        )
    except OSError as exc:
        missing = isinstance(exc, FileNotFoundError) or exc.errno == errno.ENOENT
        return ProbeCommandResult(
            argv=command, returncode=None, stdout="", stderr="",
            elapsed_s=time.monotonic() - started,
            error_code=(ProbeCommandErrorCode.NOT_INSTALLED if missing
                        else ProbeCommandErrorCode.LAUNCH_FAILED),
            error_detail=f"{type(exc).__name__}: {exc}")
    communicate = asyncio.ensure_future(proc.communicate())
    try:
        stdout_b, stderr_b = await asyncio.wait_for(
            asyncio.shield(communicate), timeout=timeout)
    except asyncio.TimeoutError:
        _kill_probe_group(proc)
        stdout_b, stderr_b, drain_note = await _drain_after_kill(communicate)
        await proc.wait()
        return ProbeCommandResult(
            argv=command, returncode=proc.returncode,
            stdout=stdout_b.decode("utf-8", errors="replace"),
            stderr=stderr_b.decode("utf-8", errors="replace"),
            elapsed_s=time.monotonic() - started,
            error_code=ProbeCommandErrorCode.TIMEOUT,
            error_detail=f"exceeded {timeout:g}s; process group killed{drain_note}")
    except asyncio.CancelledError:
        _kill_probe_group(proc)
        await asyncio.shield(_drain_after_kill(communicate))
        raise
    stdout = stdout_b.decode("utf-8", errors="replace")
    stderr = stderr_b.decode("utf-8", errors="replace")
    elapsed = time.monotonic() - started
    if proc.returncode != 0:
        return ProbeCommandResult(
            argv=command, returncode=proc.returncode, stdout=stdout,
            stderr=stderr, elapsed_s=elapsed,
            error_code=ProbeCommandErrorCode.NONZERO_EXIT,
            error_detail=f"exit code {proc.returncode}")
    return ProbeCommandResult(
        argv=command, returncode=proc.returncode, stdout=stdout,
        stderr=stderr, elapsed_s=elapsed)


async def probe_version(
    argv: str | Sequence[str],
    *,
    timeout: float = PROBE_COMMAND_TIMEOUT_S,
    env: Mapping[str, str] | None = None,
) -> ProbeCommandResult:
    """Run ``<binary> --version`` (a bare string) or a declared version argv.

    A zero exit without any output is ``EMPTY_OUTPUT``: the binary ran but
    reported no version, which is not a healthy probe.
    """
    command = [argv, "--version"] if isinstance(argv, str) else list(argv)
    result = await run_probe_command(command, timeout=timeout, env=env)
    if result.ok and not result.version:
        return replace(
            result, error_code=ProbeCommandErrorCode.EMPTY_OUTPUT,
            error_detail="command exited 0 without printing a version")
    return result


def _probe_version_argv(argv: list[str], *, timeout: float = 15.0) -> str:
    """Synchronous version probe kept for adapters not yet on ``probe_version``.

    It blocks the calling thread; async code should await ``probe_version``.
    """
    try:
        from .probe_environment import subprocess_environment
        result = subprocess.run(
            argv, capture_output=True, text=True,
            encoding="utf-8", errors="replace", timeout=timeout,
            env=subprocess_environment(),
        )
    except (FileNotFoundError, subprocess.TimeoutExpired, OSError):
        return ""
    text = (result.stdout or result.stderr or "").strip().splitlines()
    return text[0].strip() if result.returncode == 0 and text else ""


def _probe_version(binary: str, *, timeout: float = 15.0) -> str:
    """运行常规 ``<binary> --version``；失败返回空串。"""
    return _probe_version_argv([binary, "--version"], timeout=timeout)


def probe_cli_driver(
    driver: Any,
    *,
    adapter_id: str = "",
    instance_id: str = "default",
    include_models: bool = False,
    version_timeout: float = PROBE_COMMAND_TIMEOUT_S,
) -> CapabilityProbeReport:
    """Synchronous boundary for thread callers; runs ``probe_cli_driver_async``.

    Raises ``RuntimeError`` when called on a running event loop; async code
    awaits ``probe_cli_driver_async`` directly.
    """
    return asyncio.run(probe_cli_driver_async(
        driver, adapter_id=adapter_id, instance_id=instance_id,
        include_models=include_models, version_timeout=version_timeout,
    ))


async def probe_cli_driver_async(
    driver: Any,
    *,
    adapter_id: str = "",
    instance_id: str = "default",
    include_models: bool = False,
    version_timeout: float = PROBE_COMMAND_TIMEOUT_S,
) -> CapabilityProbeReport:
    """对现有 ``CliDriver`` 做行为级 capability probe（CLI 兼容路径）。

    实测信号（capability_source=probe）：

    - binary 可解析、``--version`` 可运行 → ``runtime_version``；
    - ``parse_stream_steps`` / ``parse_stream_line`` 被覆写 → ``streaming``
      与 ``tool_events``（流式行能解析出工具步骤才声明 tool_events）；
    - 用哨兵 session id 构造 ``build_resume`` argv 并确认该 id 进入 argv
      → ``resume`` / ``session_persistence``；
    - ``_usage_tokens`` 存在 → ``usage_events``（adapter_reported）。

    保守默认（static，一律 False 并在 degradations 说明）：approval、
    user_input、steer、fork、subagents、structured_output、skills、mcp、
    native_tool_binding、acp_mcp_config、agent_plugin、
    structured_http_rpc——这些需要结构化传输，CLI 一次性进程不支持，
    由 RUNTIME-02~04 的结构化 Adapter 实测后再声明。

    ``interrupt`` 为 True：由兼容层的可取消进程树（cancel_event →
    SIGKILL 进程组 + 后代表）保证，与 Runtime 无关。
    """
    name = str(getattr(driver, "name", "") or "")
    adapter_id = adapter_id or (f"cli.{name}" if name else "cli.unknown")
    field_sources: dict[str, str] = {}
    degradations: list[str] = []

    binary = ""
    try:
        binary = str(driver.bin)
    except Exception as exc:  # noqa: BLE001
        degradations.append(f"binary 解析失败：{exc}")

    version_argv = (
        list(driver.version_argv())
        if binary and callable(getattr(driver, "version_argv", None))
        else [binary, "--version"] if binary else []
    )
    version_result = (
        await probe_version(version_argv, timeout=version_timeout)
        if version_argv else None
    )
    version = version_result.version if version_result is not None and version_result.ok else ""
    probed = bool(binary and version)

    def mark(field_name: str, value: bool, source: str) -> bool:
        field_sources[field_name] = source
        return bool(value)

    caps = conservative_capabilities(
        transport_kind="cli",
        capability_source=SOURCE_PROBE if probed else SOURCE_STATIC,
    )
    caps.runtime_version = version
    permission_modes = tuple(
        driver.native_permission_modes()
        if callable(getattr(driver, "native_permission_modes", None)) else ()
    )
    caps.permission_modes = list(permission_modes) if probed else []
    field_sources["permission_modes"] = (
        SOURCE_REPORTED if probed else SOURCE_STATIC)
    sandbox_modes = tuple(
        driver.native_sandbox_modes()
        if callable(getattr(driver, "native_sandbox_modes", None)) else ()
    )
    caps.sandbox_modes = list(sandbox_modes) if probed else []
    field_sources["sandbox_modes"] = (
        SOURCE_REPORTED if probed else SOURCE_STATIC)

    # ── 实测能力（binary 不可运行时全部回落保守默认：未实测不声明） ──────────
    stream_override = (
        _defined_by(driver, "parse_stream_steps") != "CliDriver"
        or _defined_by(driver, "parse_stream_line") != "CliDriver"
    )
    caps.streaming = mark("streaming", probed and stream_override,
                          SOURCE_PROBE if probed else SOURCE_STATIC)
    caps.tool_events = mark("tool_events", probed and stream_override,
                            SOURCE_PROBE if probed else SOURCE_STATIC)

    resume_ok = False
    if probed:
        sentinel = "muteki-probe-session-sentinel"
        try:
            argv = driver.build_resume("muteki-probe", sentinel)
            resume_ok = any(sentinel in str(arg) for arg in argv)
        except Exception as exc:  # noqa: BLE001 — recorded as a degradation
            resume_ok = False
            degradations.append(
                f"resume argv 构造失败（{type(exc).__name__}: {exc}），resume 未声明")
    caps.resume = mark("resume", resume_ok,
                       SOURCE_PROBE if probed else SOURCE_STATIC)
    caps.session_persistence = mark(
        "session_persistence", resume_ok,
        SOURCE_PROBE if probed else SOURCE_STATIC)

    caps.usage_events = mark(
        "usage_events", probed and (
            "_usage_tokens" in type(driver).__dict__
            or any("_usage_tokens" in k.__dict__ for k in type(driver).__mro__)),
        SOURCE_REPORTED if probed else SOURCE_STATIC,
    )
    caps.interrupt = mark("interrupt", probed,
                          SOURCE_PROBE if probed else SOURCE_STATIC)
    degradations.append(
        "interrupt 语义为终止进程树（cancel_event → SIGKILL），"
        "被中断的 turn 状态由 Runtime 自行持久化，兼容层不伪造完成事件")

    # ── 结构化传输能力：保守默认 False ────────────────────────────────────
    for field_name in BOOL_CAPABILITY_FIELDS:
        if field_name not in field_sources:
            setattr(caps, field_name, False)
            field_sources[field_name] = SOURCE_STATIC
    if not stream_override:
        degradations.append("无流式结构化输出：仅最终文本，来源追踪退化为整段 stdout")
    degradations.append(
        "approval/user_input/steer：CLI 一次性进程无带内审批与转向通道，"
        "审批策略只能经启动参数（权限模式）固化，未静默关闭")
    degradations.append(
        "mcp/native_tool_binding/acp_mcp_config/agent_plugin/"
        "structured_http_rpc：structured transport 不可用，能力注入降级为 "
        "Agent Plugin（Gateway endpoint + 环境变量引用，不含凭据本体）")
    if not include_models:
        field_sources["supported_models"] = SOURCE_STATIC

    if probed:
        detail = ""
    elif version_result is not None:
        detail = f"--version 失败，使用保守默认：{version_result.describe()}"
    else:
        detail = "binary 不可用，使用保守默认"
    return CapabilityProbeReport(
        adapter_id=adapter_id,
        instance_id=instance_id,
        capabilities=caps,
        binary_path=binary,
        field_sources=field_sources,
        degradations=degradations,
        detail=detail,
    )


def select_injection_kind(caps: AgentCapabilities) -> InjectionKind:
    """按 Runtime 实测能力选择注入方式（MCP 为默认优先）。

    顺序（任务书 6.5、设计 9.3）：

    1. ``mcp`` → MCP（默认与优先路径）；
    2. ``native_tool_binding`` → Native Tool（MCP 不可用时）；
    3. ``acp_mcp_config`` → ACP ``session/new|load|resume`` 的 mcpServers；
    4. ``agent_plugin`` 或 ``skills`` → Agent Plugins 1.0.0 标准包；
    5. ``structured_http_rpc`` → 结构化 HTTP/JSON-RPC；
    6. 其余 → 同一个 Agent Plugin 的 Skill 组件作为文本兜底。

    只读 capability 字段，不看 adapter id。
    """
    if caps.mcp:
        return InjectionKind.MCP
    if caps.native_tool_binding:
        return InjectionKind.NATIVE_TOOL
    if caps.acp_mcp_config:
        return InjectionKind.ACP_MCP_CONFIG
    if caps.agent_plugin or caps.skills:
        return InjectionKind.AGENT_PLUGIN
    if caps.structured_http_rpc:
        return InjectionKind.HTTP_JSONRPC
    return InjectionKind.AGENT_PLUGIN


def describe_injection_path(caps: AgentCapabilities) -> dict[str, str]:
    """返回 Runtime 当前能力快照对应的真实工具接入路径。

    这个视图与 ``select_injection_kind`` 使用同一选择结果，供
    Runtime API / UI 直接展示，避免前端再按 Adapter 名称推断协议。
    """
    kind = select_injection_kind(caps)
    fields = {
        InjectionKind.MCP: "mcp",
        InjectionKind.NATIVE_TOOL: "native_tool_binding",
        InjectionKind.ACP_MCP_CONFIG: "acp_mcp_config",
        InjectionKind.AGENT_PLUGIN: "agent_plugin",
        InjectionKind.HTTP_JSONRPC: "structured_http_rpc",
    }
    transports = {
        InjectionKind.MCP: "mcp_streamable_http",
        InjectionKind.NATIVE_TOOL: "in_process_sdk_mcp",
        InjectionKind.ACP_MCP_CONFIG: "acp_mcp_servers",
        InjectionKind.AGENT_PLUGIN: "agent_plugins_1_0",
        InjectionKind.HTTP_JSONRPC: "host_tool_http_jsonrpc",
    }
    return {
        "kind": kind.value,
        "capability_field": fields[kind],
        "transport": transports[kind],
    }


__all__ = [
    "AccessModeUnsupportedError",
    "BOOL_CAPABILITY_FIELDS",
    "CapabilityProbeReport",
    "PROBE_COMMAND_TIMEOUT_S",
    "ProbeCommandErrorCode",
    "ProbeCommandResult",
    "SOURCE_PROBE",
    "SOURCE_REPORTED",
    "SOURCE_STATIC",
    "conservative_capabilities",
    "describe_injection_path",
    "probe_cli_driver",
    "probe_cli_driver_async",
    "probe_version",
    "run_probe_command",
    "require_access_mode",
    "select_injection_kind",
]
