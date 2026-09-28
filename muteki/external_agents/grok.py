"""Grok ACP Adapter（RUNTIME-03，任务书 7.2/7.6）。

正式接入：``grok agent stdio``（ACP v1，stdio JSON-RPC）。核验与本机实测
（docs/research/third_party_verification.md §ACP；2026-08-21，grok 1.0.5）：

- ``grok agent stdio`` 真实存在，以 ACP agent 身份在 stdin/stdout 跑
  JSON-RPC；本机实测 ``initialize`` 返回 ``loadSession: true``、
  ``sessionCapabilities: {list, resume, close}``、
  ``mcpCapabilities.http: true``——恢复优先 ``session/resume``（不重放），
  ``session/load`` 为降级；
- 认证 ``authMethods``：``cached_token``（需先 ``grok login``，读
  ``~/.grok/auth.json``）或 ``xai.api_key``（需 ``XAI_API_KEY``）；
  ``authenticate`` 必须带 Grok 扩展 ``_meta: {"headless": true}``；
- 注意：核验文档建议自动化加 ``--no-auto-update``，但本机 grok 1.0.5 的
  ``grok agent stdio`` **不接受该参数**（unexpected argument）；该开关只对
  顶层 ``grok -p`` headless 路径有效。本 Adapter 不传该参数，如未来版本
  支持可经 ``extra_argv`` 追加。

兼容路径（保留）：``grok -p --output-format streaming-messages-json`` 与
``--resume`` 由 ``muteki.solver.cli_driver.GrokDriver`` + ``CliDriverAdapter``
（``cli.grok``）承担；本模块不 import solver 层，两个 Adapter 可同时注册。
"""

from __future__ import annotations

import asyncio
import json
import math
import os
import re
import shutil
import tomllib
from pathlib import Path
from typing import Any, Optional

from muteki.platform.contracts.external_agents import AccessMode, SessionStart

from .acp import BaseAcpAdapter

#: 认证方式优先级：本机登录态优先，其次 API key（需环境变量在场）。
_AUTH_CACHED_TOKEN = "cached_token"
_AUTH_XAI_API_KEY = "xai.api_key"

# Grok ACP 会在异步 HTTP MCP 工具发现完成前返回 session/new。真实会话中，
# 首轮 prompt 可比 muteki-control 连接更早到达，而 HTTP MCP 连接与 tools/list
# 常需数秒。默认值覆盖该实测窗口并留出余量，避免模型只 search_tool 时
# muteki-control 工具尚未出现在 Runtime 目录中；较慢的本地 MCP 可在有界
# 范围内单独调大。
_DEFAULT_MCP_READY_WAIT_SECONDS = 8.0
_MAX_MCP_READY_WAIT_SECONDS = 30.0


def _mcp_ready_wait_seconds(value: Optional[float] = None) -> float:
    """返回 Grok HTTP MCP 启动稳定窗口的有界秒数。

    ``MUTEKI_GROK_MCP_READY_WAIT_SECONDS`` 是进程级覆盖项，可在不修改
    持久化 Runtime Profile 的前提下调节较慢的本地 MCP。非法或非有限值回落
    到已核验默认值；显式设为零可用于诊断时关闭等待。
    """
    raw: Any = (
        value if value is not None
        else os.environ.get("MUTEKI_GROK_MCP_READY_WAIT_SECONDS")
    )
    if raw in (None, ""):
        return _DEFAULT_MCP_READY_WAIT_SECONDS
    try:
        seconds = float(raw)
    except (TypeError, ValueError):
        return _DEFAULT_MCP_READY_WAIT_SECONDS
    if not math.isfinite(seconds):
        return _DEFAULT_MCP_READY_WAIT_SECONDS
    return min(max(seconds, 0.0), _MAX_MCP_READY_WAIT_SECONDS)


def _toml_with_value(
    text: str, section: str, key: str, rendered_value: str
) -> str:
    """Replace one TOML table value without rewriting unrelated settings."""
    lines = text.splitlines(keepends=True)
    section_header = re.compile(r"^\s*\[([^]]+)]\s*(?:#.*)?(?:\r?\n)?$")
    key_start = re.compile(rf"^\s*{re.escape(key)}\s*=")
    start: Optional[int] = None
    end = len(lines)
    for index, line in enumerate(lines):
        heading = section_header.match(line)
        if not heading:
            continue
        if start is not None:
            end = index
            break
        if heading.group(1).strip() == section:
            start = index + 1

    replacement = f"{key} = {rendered_value}\n"
    if start is None:
        suffix = "" if not text or text.endswith(("\n", "\r")) else "\n"
        spacer = "" if not text or not text.strip() else "\n"
        return f"{text}{suffix}{spacer}[{section}]\n{replacement}"

    for index in range(start, end):
        if not key_start.match(lines[index]):
            continue
        assignment_end = index + 1
        bracket_depth = lines[index].count("[") - lines[index].count("]")
        while bracket_depth > 0 and assignment_end < end:
            bracket_depth += (
                lines[assignment_end].count("[")
                - lines[assignment_end].count("]")
            )
            assignment_end += 1
        lines[index:assignment_end] = [replacement]
        return "".join(lines)

    lines.insert(end, replacement)
    return "".join(lines)


def _grok_supervised_config(text: str) -> str:
    """Return a private Grok config that enforces supervised mutations.

    Grok's native ``--permission-mode default`` still consults the persisted
    mode, permission rules and remembered grants. Muteki keeps every existing
    setting and rule, then adds native ``ask`` rules for commands and file
    mutations. The user's real config is never edited.
    """
    existing_ask: list[str] = []
    try:
        parsed = tomllib.loads(text)
        configured = (parsed.get("permission") or {}).get("ask") or []
        if isinstance(configured, list):
            existing_ask = [str(rule) for rule in configured]
    except (tomllib.TOMLDecodeError, AttributeError, TypeError):
        pass
    ask_rules = list(dict.fromkeys([
        *existing_ask,
        "Bash",
        "Edit",
        "Write",
    ]))
    prepared = _toml_with_value(text, "ui", "permission_mode", '"ask"')
    return _toml_with_value(
        prepared,
        "permission",
        "ask",
        json.dumps(ask_rules, ensure_ascii=False),
    )


def default_grok_binary() -> str:
    return os.environ.get("MUTEKI_GROK_BIN", "grok")


class GrokAcpAdapter(BaseAcpAdapter):
    """Grok Build 的 ACP 结构化 Adapter。

    ``model`` / ``reasoning_effort`` 固化进 agent 进程 argv
    （``grok agent stdio -m <model> --reasoning-effort <effort>``）；
    Session 级能力（stream/approval/resume/interrupt/cwd）由
    ``BaseAcpAdapter`` 实现。
    """

    adapter_id = "grok.acp"

    def __init__(
        self,
        *,
        binary: Optional[str] = None,
        model: Optional[str] = None,
        reasoning_effort: Optional[str] = None,
        extra_argv: tuple[str, ...] = (),
        runtime_root: Optional[str | Path] = None,
        mcp_ready_wait_seconds: Optional[float] = None,
        **kwargs: Any,
    ) -> None:
        super().__init__(**kwargs)
        self._binary = binary or default_grok_binary()
        self._model = model
        self._reasoning_effort = reasoning_effort
        self._extra_argv = list(extra_argv)
        self._mcp_ready_wait_seconds = _mcp_ready_wait_seconds(
            mcp_ready_wait_seconds)
        self._runtime_root = Path(
            runtime_root
            or os.environ.get("MUTEKI_GROK_RUNTIME_ROOT")
            or (
                Path(os.environ.get("MUTEKI_STATE_ROOT") or "state")
                / "_grok_acp_runtime"
            )
        ).expanduser().resolve()

    def _agent_argv(self) -> list[str]:
        return self._argv()

    def _argv(
        self,
        *,
        model: Optional[str] = None,
        reasoning_effort: Optional[str] = None,
        permission_mode: Optional[str] = None,
    ) -> list[str]:
        argv = [self._binary]
        if permission_mode:
            argv += ["--permission-mode", permission_mode]
        selected_model = model or self._model
        selected_effort = reasoning_effort or self._reasoning_effort
        if selected_model:
            argv += ["--model", selected_model]
        if selected_effort:
            argv += ["--reasoning-effort", selected_effort]
        argv += ["agent", "stdio"]
        argv += self._extra_argv
        return argv

    def _agent_argv_for_request(self, request: SessionStart) -> list[str]:
        mode = request.access_mode or AccessMode.SUPERVISED.value
        if mode == AccessMode.FULL_ACCESS.value:
            argv = [self._binary]
            if request.model:
                argv += ["--model", str(request.model)]
            if request.effort:
                argv += ["--reasoning-effort", str(request.effort)]
            return [*argv, "agent", "--always-approve", "stdio",
                    *self._extra_argv]
        native_mode = {
            AccessMode.SUPERVISED.value: "default",
            AccessMode.AUTO_ACCEPT_EDITS.value: "acceptEdits",
            AccessMode.AUTO.value: "auto",
        }[mode]
        return self._argv(
            model=request.model,
            reasoning_effort=request.effort,
            permission_mode=native_mode,
        )

    def _prepare_session_environment(
        self, request: SessionStart, env: dict[str, str], cwd: str
    ) -> dict[str, str]:
        del cwd
        mode = request.access_mode or AccessMode.SUPERVISED.value
        if mode != AccessMode.SUPERVISED.value:
            return env

        prepared = dict(env)
        source = Path(
            prepared.get("GROK_HOME")
            or os.environ.get("GROK_HOME")
            or (Path.home() / ".grok")
        ).expanduser().resolve()
        target = self._runtime_root / request.agent_session_id
        target.mkdir(parents=True, exist_ok=True)
        try:
            target.chmod(0o700)
        except OSError:
            pass

        # Keep Grok's normal auth, models, plugins and persisted sessions by
        # referencing the user's files.  config.toml is the sole private copy.
        if source.is_dir() and source != target:
            for item in source.iterdir():
                if item.name == "config.toml":
                    continue
                if item.is_symlink() and not item.exists():
                    continue
                destination = target / item.name
                if env.get("MUTEKI_CHAT_PRIVATE_ROOT") and destination.is_symlink():
                    private = Path(env["MUTEKI_CHAT_PRIVATE_ROOT"]).resolve()
                    if not destination.resolve().is_relative_to(private):
                        destination.unlink()
                if destination.exists() or destination.is_symlink():
                    continue
                try:
                    destination.symlink_to(
                        item, target_is_directory=item.is_dir()
                    )
                except OSError:
                    if item.is_file():
                        shutil.copy2(item, destination)

        source_config = source / "config.toml"
        try:
            config_text = source_config.read_text(encoding="utf-8")
        except OSError:
            config_text = ""
        target_config = target / "config.toml"
        target_config.write_text(
            _grok_supervised_config(config_text),
            encoding="utf-8",
        )
        try:
            target_config.chmod(0o600)
        except OSError:
            pass
        prepared["GROK_HOME"] = str(target)
        return prepared

    def _select_auth_method(
        self, auth_methods: list[dict[str, Any]]
    ) -> Optional[str]:
        available = {str(m.get("id")) for m in auth_methods}
        if _AUTH_CACHED_TOKEN in available:
            return _AUTH_CACHED_TOKEN
        if _AUTH_XAI_API_KEY in available and os.environ.get("XAI_API_KEY"):
            return _AUTH_XAI_API_KEY
        return super()._select_auth_method(auth_methods)

    def _select_session_auth_method(
        self,
        auth_methods: list[dict[str, Any]],
        env: dict[str, str],
    ) -> Optional[str]:
        available = {str(method.get("id")) for method in auth_methods}
        # A Thread-selected account is present only in the session environment.
        # Prefer it over a cached host login so credential_id remains authoritative.
        if env.get("XAI_API_KEY") and _AUTH_XAI_API_KEY in available:
            return _AUTH_XAI_API_KEY
        return self._select_auth_method(auth_methods)

    def _authenticate_meta(self) -> Optional[dict[str, Any]]:
        # Grok 扩展：headless 模式标记（核验 §ACP，官方示例流程）。
        return {"headless": True}

    async def _await_injected_mcp_ready(
        self,
        transport: Any,
        session_id: str,
        request: SessionStart,
        mcp_servers: list[dict[str, Any]],
    ) -> None:
        """避免 Grok 在 HTTP MCP 工具发现完成前接收首轮 prompt。

        Grok ACP 目前没有暴露逐服务的 readiness RPC；其 session/new 响应
        和 ``mcp_servers[].status`` 都只代表配置已接受。因而这里限定为
        已真实注入 HTTP MCP 时的有界稳定窗口，而非将未确认的配置状态
        误报为已连接。仅 Grok 覆盖此钩子，其他 ACP Runtime 不受影响。
        """
        del transport, session_id, request
        has_http_mcp = any(
            str(server.get("type") or "").strip().lower() == "http"
            and bool(str(server.get("url") or "").strip())
            for server in mcp_servers
            if isinstance(server, dict)
        )
        if has_http_mcp and self._mcp_ready_wait_seconds > 0:
            await asyncio.sleep(self._mcp_ready_wait_seconds)


__all__ = ["GrokAcpAdapter", "default_grok_binary"]
