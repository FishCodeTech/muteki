"""Cursor ACP Adapter（RUNTIME-03，任务书 7.2/7.6）。

正式接入：ACP v1（``cursor agent acp``，stdio JSON-RPC）。核验结论
（docs/research/third_party_verification.md §ACP）：

- 启动命令是 ``cursor agent acp``；显式 Cursor Agent 二进制也可直接
  执行 ``acp``。不按裸 ``agent`` 查 PATH，避免选到 Grok 的同名程序；
- 认证 ``methodId: "cursor_login"``；正式对话在已有
  ``CURSOR_API_KEY`` / ``CURSOR_AUTH_TOKEN`` 或宿主 ``cursor agent login``
  时必须跳过 ``authenticate``——再发 ``cursor_login`` 会打开交互登录；
- 本机实测（2026-08-21，cursor-agent 2026.08.11）：``loadSession: true``、
  ``mcpCapabilities.http: true``、``sessionCapabilities`` 仅 ``list``——
  **无 ``session/resume`` 能力**，恢复只能走 ``session/load``（重放历史，
  回放事件标记 replay 且不投影，见 ``acp.py``）；
- Cursor 扩展方法（``cursor/ask_question`` 等）未接入，收到时计入
  transport ``stats["unhandled"]``，不改变核心状态机。

兼容路径：当前 ``cursor-agent -p --output-format stream-json`` Driver 由
``muteki.solver.cli_driver.CursorDriver`` + ``CliDriverAdapter``
（``cli.cursor``）承担，本模块不重复实现、也不 import solver 层；两个
Adapter 实例可同时注册进 Registry，调度按 probe 能力选择。
"""

from __future__ import annotations

import json
import os
import subprocess
from pathlib import Path
from typing import Any, Optional

from muteki.platform.contracts.external_agents import (
    AccessMode,
    AgentCapabilities,
    SessionStart,
)

from .probe_environment import subprocess_environment
from .acp import AcpError, AcpHello, AcpTransport, BaseAcpAdapter
from .capabilities import AccessModeUnsupportedError

#: 认证方式优先级（核验：authMethods 当前只有 cursor_login）。
_AUTH_PREFERENCE = ("cursor_login",)
#: Cursor ACP advertises agent / plan / ask; only ``agent`` can act.
_IMPLEMENTATION_MODE_ID = "agent"
#: Protobuf oneof envelopes. They are not task roles; ``unspecified`` is no role.
_SUBAGENT_ROLE_WRAPPERS = frozenset({"custom", "unspecified"})


def _cursor_subagent_role(value: Any, _depth: int = 0) -> Optional[str]:
    """Concrete task role from Cursor's nested ``subagentType``.

    ACP sends a protobuf oneof such as ``{"custom": {"unspecified": {}}}``
    or ``{"explore": {}}``. The SDK sends ``{"kind": "shell"}`` or a plain
    string. ``custom`` and ``unspecified`` are wrappers, so unspecified
    yields no role. A string that is not one of those wrappers is the role.
    """
    if _depth > 8:
        return None
    if isinstance(value, str):
        role = value.strip()
        if not role or role.casefold() in _SUBAGENT_ROLE_WRAPPERS:
            return None
        return role
    if not isinstance(value, dict) or not value:
        return None
    if "kind" in value:
        resolved = _cursor_subagent_role(value.get("kind"), _depth=_depth + 1)
        if resolved or set(value) == {"kind"}:
            return resolved
    for key, nested in value.items():
        if str(key).casefold() not in _SUBAGENT_ROLE_WRAPPERS:
            continue
        resolved = _cursor_subagent_role(nested, _depth=_depth + 1)
        if resolved:
            return resolved
    for key, nested in value.items():
        name = str(key)
        if name == "kind" or name.casefold() in _SUBAGENT_ROLE_WRAPPERS:
            continue
        if isinstance(nested, str):
            return _cursor_subagent_role(nested, _depth=_depth + 1)
        return name or None
    return None


def default_cursor_binary() -> str:
    """Cursor launcher or an explicitly configured Cursor Agent executable.

    不用裸 ``agent`` 名查 PATH：``~/.grok/bin/agent`` 是 Grok 的同名
    二进制，裸名解析会起错 Runtime。
    """
    override = os.environ.get("MUTEKI_CURSOR_AGENT_BIN") or os.environ.get("MUTEKI_CURSOR_BIN")
    if override:
        return override
    local = Path.home() / ".local" / "bin" / "cursor"
    if local.is_file():
        return str(local)
    return "cursor"


def _cursor_agent_command(binary: str) -> list[str]:
    # ``cursor`` is the desktop launcher; its Agent entry point is a subcommand.
    # Explicit cursor-agent / absolute Agent binaries already are that entry.
    return [binary, "agent"] if Path(binary).name.lower() in {"cursor", "cursor.cmd", "cursor.exe"} else [binary]


def cursor_is_authenticated(binary: str, env: dict[str, str]) -> bool:
    """读取 Cursor CLI 登录状态，避免 ACP authenticate 挂在交互登录。"""
    if env.get("CURSOR_API_KEY") or env.get("CURSOR_AUTH_TOKEN"):
        return True
    try:
        result = subprocess.run(
            [*_cursor_agent_command(binary), "status", "--format", "json"],
            capture_output=True,
            text=True,
            timeout=10,
            env=subprocess_environment(env),
        )
        payload = json.loads(result.stdout or "{}")
    except (OSError, subprocess.TimeoutExpired, json.JSONDecodeError):
        return False
    return bool(payload.get("isAuthenticated"))


class CursorAcpEffortUnsupportedError(ValueError):
    code = "external_agent.effort_unsupported"


class CursorAcpAdapter(BaseAcpAdapter):
    """Cursor 的 ACP 结构化 Adapter（session/stream/tool event/approval/
    resume/interrupt/cwd 全部由 ``BaseAcpAdapter`` 经 ACP v1 实现）。"""

    adapter_id = "cursor.acp"
    unsupported_access_mode_reasons = {
        AccessMode.AUTO.value: (
            "Cursor ACP does not apply --auto-review. The flag exists on "
            "`cursor agent` and is honored by the interactive chat command; "
            "the ACP server only reads --force"
        ),
    }

    def __init__(self, *, binary: Optional[str] = None, **kwargs: Any) -> None:
        super().__init__(**kwargs)
        self._binary = binary or default_cursor_binary()

    def _agent_argv(self) -> list[str]:
        return [*_cursor_agent_command(self._binary), "acp"]

    def probe_version_argv(self) -> list[str]:
        return [*_cursor_agent_command(self._binary), "--version"]

    def _agent_argv_for_request(self, request: SessionStart) -> list[str]:
        if request.effort not in (None, "", "default"):
            raise CursorAcpEffortUnsupportedError(
                "Cursor ACP does not accept a separate effort setting; "
                "select a native model variant or use Cursor SDK.")
        argv = _cursor_agent_command(self._binary)
        if request.model:
            argv.extend(["--model", request.model])
        argv.append("acp")
        return argv

    async def _after_session_open(
        self, transport: AcpTransport, session_id: str, request: SessionStart
    ) -> None:
        """Select Cursor's implementation mode for every access mode.

        Cursor's ``ask`` mode is read-only Q&A and ``plan`` is read-only
        planning, so neither represents an access mode.  In the ``agent``
        mode Cursor sends ``session/request_permission`` for writes, deletes,
        shell and MCP calls outside the user's own allowlist; the shared ACP
        callback then asks (supervised), allows edit kinds
        (auto-accept-edits) or allows everything (full-access).
        """
        modes = transport.session_setup(session_id).get("modes") or {}
        available = [
            dict(mode) for mode in (modes.get("availableModes") or [])
            if isinstance(mode, dict)
        ]
        # Exact id only: a description match could pick a read-only mode.
        selected = next((
            mode for mode in available
            if str(mode.get("id") or "").lower() == _IMPLEMENTATION_MODE_ID
        ), None)
        if selected is None:
            raise AccessModeUnsupportedError(
                self.id,
                str(request.access_mode or AccessMode.SUPERVISED.value),
                (),
                "Cursor did not advertise its implementation (agent) mode; "
                f"advertised modes: {[mode.get('id') for mode in available]!r}",
            )
        await transport.set_mode(session_id, str(selected.get("id") or ""))

    def _select_auth_method(
        self, auth_methods: list[dict[str, Any]]
    ) -> Optional[str]:
        available = {str(m.get("id")) for m in auth_methods}
        for method in _AUTH_PREFERENCE:
            if method in available:
                return method
        return super()._select_auth_method(auth_methods)

    def _select_session_auth_method(
        self,
        auth_methods: list[dict[str, Any]],
        env: dict[str, str],
    ) -> Optional[str]:
        """Pick ACP auth, or skip it when the process is already pre-authenticated.

        Cursor's only advertised method is ``cursor_login``. Calling it after the
        subprocess already has ``CURSOR_API_KEY`` / host login still opens the
        interactive login UI. Credential probes succeed because they use the
        print/offline bridge with the key in env and never hit this path.
        """
        if not auth_methods:
            return None
        if not cursor_is_authenticated(self._binary, env):
            raise AcpError(
                "Cursor Agent 尚未登录；请先执行 cursor agent login，"
                "或为该 Runtime 配置 CURSOR_API_KEY"
            )
        return None

    def _probe_extra_caps(self, caps: AgentCapabilities, hello: AcpHello) -> None:
        # Cursor 文档明示 ACP 支持 .cursor/mcp.json，且本机实测
        # mcpCapabilities.http=true → mcpServers 注入能力已在
        # acp_mcp_config 位声明；Cursor 的 blocking 扩展（ask_question /
        # create_plan）未核验消费方式，不声明 user_input。
        caps.supported_models = []  # ACP v1 无模型枚举方法，保持空（static）
        # 本机实测（2026.09.26）：Task 委派以 rawInput._toolName == "task"
        # 的 tool_call 公布，完成时伴随 cursor/task 扩展请求携带 agentId。
        caps.subagents = True

    def _background_tasks_hold_turn(self) -> bool:
        """A backgrounded Cursor task does not keep this turn open.

        ``BaseAcpAdapter`` defaults to waiting out ``session/prompt``.
        Cursor reports ``rawOutput.isBackground`` when the parent has handed
        the task off; that work continues after the turn ends.
        """
        return False

    # -- 子智能体（实测帧见 /tmp 录制，2026-11） -------------------------------

    def _delegation_info(self, update: dict[str, Any]) -> Optional[dict[str, Any]]:
        raw = update.get("rawInput") or {}
        if str(raw.get("_toolName") or "") != "task":
            return None
        call_id = str(update.get("toolCallId") or "")
        role = _cursor_subagent_role(raw.get("subagentType"))
        # Cursor 不在连接上流式公布子会话，node 只能以 toolCallId 为锚。
        return {
            "agent_id": call_id or None,
            "title": str(raw.get("description") or "").strip() or None,
            "request": str(raw.get("prompt") or "").strip() or None,
            "role": role,
        }

    def _delegation_result(
        self, handle: dict[str, Any], update: dict[str, Any],
        desc: dict[str, Any],
    ) -> Optional[dict[str, Any]]:
        raw = update.get("rawOutput")
        if not isinstance(raw, dict):
            return None
        patch: dict[str, Any] = {}
        if isinstance(raw.get("durationMs"), (int, float)):
            patch["duration_ms"] = raw["durationMs"]
        if raw.get("isBackground") is True:
            # Handoff, not a foreground task the turn must wait on.
            # ``background`` is not an AgentNodePayload field; ``_patch_agent_node``
            # drops it before the node is validated. The delegation record keeps it.
            desc["background"] = True
            patch["background"] = True
        return patch or None

    def _patch_agent_node(
        self, handle: dict[str, Any], agent_id: str, update: dict[str, Any]
    ):
        if "background" in update:
            update = {
                key: value for key, value in update.items() if key != "background"
            }
        return super()._patch_agent_node(handle, agent_id, update)

    def _map_agent_extension(
        self, handle: dict[str, Any], method: str,
        params: dict[str, Any], is_request: bool,
    ):
        if method != "cursor/task":
            return None, []
        # cursor/task 在委派完成时携带子 agentId 与耗时；必须应答，
        # 否则 agent 侧一直等待。
        call_id = str(params.get("toolCallId") or "")
        desc = handle["delegation_calls"].get(call_id) or {}
        agent_id = str(desc.get("agent_id") or "") or call_id
        events = []
        if agent_id:
            node_patch: dict[str, Any] = {
                "session_ref": str(params.get("agentId") or "") or None,
                "model": str(params.get("model") or "") or None,
                "duration_ms": params.get("durationMs"),
            }
            role = _cursor_subagent_role(params.get("subagentType"))
            if role:
                node_patch["role"] = role
            event = self._patch_agent_node(handle, agent_id, node_patch)
            if event is not None:
                events.append(event)
        return {}, events


__all__ = [
    "CursorAcpAdapter", "cursor_is_authenticated", "default_cursor_binary",
]
