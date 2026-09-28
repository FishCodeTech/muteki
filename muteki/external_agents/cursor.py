"""Cursor ACP Adapter（RUNTIME-03，任务书 7.2/7.6）。

正式接入：ACP v1（``agent acp``，stdio JSON-RPC）。核验结论
（docs/research/third_party_verification.md §ACP）：

- 启动命令是 ``agent acp``（CLI 二进制现名 ``agent``，安装于
  ``~/.local/bin/agent``；PATH 上 ``~/.grok/bin/agent`` 是 Grok 的同名
  二进制，因此本 Adapter 优先用 ``cursor-agent`` / 显式路径，不用裸
  ``agent`` 名解析）；
- 认证 ``methodId: "cursor_login"``；正式对话在已有
  ``CURSOR_API_KEY`` / ``CURSOR_AUTH_TOKEN`` 或宿主 ``agent login``
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

#: 认证方式优先级（核验：authMethods 当前只有 cursor_login）。
_AUTH_PREFERENCE = ("cursor_login",)
_PLAN_MODE_ALIASES = ("plan", "architect")
_IMPLEMENT_MODE_ALIASES = ("code", "agent", "default", "chat", "implement")
_APPROVAL_MODE_ALIASES = ("ask",)


def default_cursor_binary() -> str:
    """解析 Cursor ACP 二进制：环境变量 > ``~/.local/bin/agent`` > PATH。

    不用裸 ``agent`` 名查 PATH：``~/.grok/bin/agent`` 是 Grok 的同名
    二进制，裸名解析会起错 Runtime。
    """
    override = os.environ.get("MUTEKI_CURSOR_AGENT_BIN")
    if override:
        return override
    local = Path.home() / ".local" / "bin" / "agent"
    if local.is_file():
        return str(local)
    return "cursor-agent"


def cursor_is_authenticated(binary: str, env: dict[str, str]) -> bool:
    """读取 Cursor CLI 登录状态，避免 ACP authenticate 挂在交互登录。"""
    if env.get("CURSOR_API_KEY") or env.get("CURSOR_AUTH_TOKEN"):
        return True
    try:
        result = subprocess.run(
            [binary, "status", "--format", "json"],
            capture_output=True,
            text=True,
            timeout=10,
            env=subprocess_environment(env),
        )
        payload = json.loads(result.stdout or "{}")
    except (OSError, subprocess.TimeoutExpired, json.JSONDecodeError):
        return False
    return bool(payload.get("isAuthenticated"))


class CursorAcpAdapter(BaseAcpAdapter):
    """Cursor 的 ACP 结构化 Adapter（session/stream/tool event/approval/
    resume/interrupt/cwd 全部由 ``BaseAcpAdapter`` 经 ACP v1 实现）。"""

    adapter_id = "cursor.acp"

    def __init__(self, *, binary: Optional[str] = None, **kwargs: Any) -> None:
        super().__init__(**kwargs)
        self._binary = binary or default_cursor_binary()

    def _agent_argv(self) -> list[str]:
        return [self._binary, "acp"]

    def _agent_argv_for_request(self, request: SessionStart) -> list[str]:
        argv = [self._binary]
        if request.model:
            argv.extend(["--model", request.model])
        argv.append("acp")
        return argv

    @staticmethod
    def _normalize_mode_search_text(mode: dict[str, Any]) -> str:
        text = " ".join(
            str(mode.get(key) or "")
            for key in ("id", "name", "description")
        ).lower()
        return " ".join("".join(
            char if char.isalnum() else " " for char in text
        ).split())

    @classmethod
    def _find_mode_by_aliases(
        cls,
        modes: list[dict[str, Any]],
        aliases: tuple[str, ...],
    ) -> Optional[dict[str, Any]]:
        normalized_aliases = tuple(alias.lower() for alias in aliases)
        for alias in normalized_aliases:
            exact = next((
                mode for mode in modes
                if str(mode.get("id") or "").lower() == alias
                or str(mode.get("name") or "").lower() == alias
            ), None)
            if exact is not None:
                return exact
        for alias in normalized_aliases:
            partial = next((
                mode for mode in modes
                if alias in cls._normalize_mode_search_text(mode)
            ), None)
            if partial is not None:
                return partial
        return None

    @classmethod
    def _is_plan_mode(cls, mode: dict[str, Any]) -> bool:
        return cls._find_mode_by_aliases(
            [mode], _PLAN_MODE_ALIASES
        ) is not None

    async def _after_session_open(
        self, transport: AcpTransport, session_id: str, request: SessionStart
    ) -> None:
        """Select Cursor's ACP mode using T3 Code's provider-native policy.

        ``supervised`` resolves to Cursor's ``ask`` mode.  The other runtime
        modes resolve to Cursor's implementation mode (normally ``agent``).
        ACP permission requests are still handled by ``BaseAcpAdapter``.
        """
        modes = transport.session_setup(session_id).get("modes") or {}
        available = [
            dict(mode) for mode in (modes.get("availableModes") or [])
            if isinstance(mode, dict)
        ]
        current = str(modes.get("currentModeId") or "")
        supervised = (
            request.access_mode or AccessMode.SUPERVISED.value
        ) == AccessMode.SUPERVISED.value
        primary = self._find_mode_by_aliases(
            available,
            _APPROVAL_MODE_ALIASES if supervised else _IMPLEMENT_MODE_ALIASES,
        )
        fallback = self._find_mode_by_aliases(
            available,
            _IMPLEMENT_MODE_ALIASES if supervised else _APPROVAL_MODE_ALIASES,
        )
        selected = str((primary or fallback or {}).get("id") or "") or next(
            (str(mode.get("id") or "") for mode in available
             if not self._is_plan_mode(mode)),
            "",
        ) or current
        if selected:
            await transport.set_mode(session_id, selected)

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
                "Cursor Agent 尚未登录；请先执行 cursor-agent login，"
                "或为该 Runtime 配置 CURSOR_API_KEY"
            )
        return None

    def _probe_extra_caps(self, caps: AgentCapabilities, hello: AcpHello) -> None:
        # Cursor 文档明示 ACP 支持 .cursor/mcp.json，且本机实测
        # mcpCapabilities.http=true → mcpServers 注入能力已在
        # acp_mcp_config 位声明；Cursor 的 blocking 扩展（ask_question /
        # create_plan）未核验消费方式，不声明 user_input。
        caps.supported_models = []  # ACP v1 无模型枚举方法，保持空（static）


__all__ = [
    "CursorAcpAdapter", "cursor_is_authenticated", "default_cursor_binary",
]
