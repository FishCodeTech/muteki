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

import subprocess
from dataclasses import dataclass, field
from datetime import datetime
from typing import Any

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
    "session_persistence", "plan", "image_input",
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


def _probe_version_argv(argv: list[str], *, timeout: float = 15.0) -> str:
    """运行 Driver 声明的只读版本命令；失败返回空串。"""
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
    return text[0].strip()[:120] if result.returncode == 0 and text else ""


def _probe_version(binary: str, *, timeout: float = 15.0) -> str:
    """运行常规 ``<binary> --version``；失败返回空串。"""
    return _probe_version_argv([binary, "--version"], timeout=timeout)


def probe_cli_driver(
    driver: Any,
    *,
    adapter_id: str = "",
    instance_id: str = "default",
    include_models: bool = False,
    version_timeout: float = 15.0,
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
        degradations.append(f"binary 解析失败：{str(exc)[:120]}")

    version_argv = (
        list(driver.version_argv())
        if binary and callable(getattr(driver, "version_argv", None))
        else [binary, "--version"] if binary else []
    )
    version = (
        _probe_version_argv(version_argv, timeout=version_timeout)
        if version_argv else ""
    )
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
        except Exception:  # noqa: BLE001
            resume_ok = False
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

    return CapabilityProbeReport(
        adapter_id=adapter_id,
        instance_id=instance_id,
        capabilities=caps,
        binary_path=binary,
        field_sources=field_sources,
        degradations=degradations,
        detail="" if probed else "binary 不可用或 --version 失败，使用保守默认",
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
    "BOOL_CAPABILITY_FIELDS",
    "CapabilityProbeReport",
    "SOURCE_PROBE",
    "SOURCE_REPORTED",
    "SOURCE_STATIC",
    "conservative_capabilities",
    "describe_injection_path",
    "probe_cli_driver",
    "select_injection_kind",
]
