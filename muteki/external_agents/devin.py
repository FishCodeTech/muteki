"""Local Devin CLI conversation adapter (ACP over stdio).

Uses the CLI's existing host login, or WINDSURF_API_KEY inherited by the
process. Conversation sessions and solving workers both use ACP; the latter
uses a JSONL bridge for the existing ``CliSolver`` event contract.
"""

from __future__ import annotations

import asyncio
from functools import lru_cache
import json
import os
from pathlib import Path
import shutil
import subprocess
import time
from typing import Any, Mapping, Optional

from muteki.platform.contracts.external_agents import (
    AgentCapabilities, ProbeRequest, SessionStart,
)

from .acp import AcpError, AcpTransport, BaseAcpAdapter, check_response


def default_devin_binary() -> str:
    return (os.environ.get("MUTEKI_DEVIN_BIN") or shutil.which("devin")
            or str(Path.home() / ".local/bin/devin"))


def devin_login_status(
    binary: Optional[str] = None, env: Optional[Mapping[str, str]] = None,
) -> str:
    source = dict(os.environ if env is None else env)
    if source.get("WINDSURF_API_KEY", "").strip():
        return "present"
    # ``devin auth status`` also refreshes remote account/team metadata.  A
    # transient network timeout must not make an existing host login disappear
    # while Worker settings are being saved.  This is the credential file path
    # reported and consumed by Devin CLI itself.
    try:
        data_home = source.get("XDG_DATA_HOME", "").strip()
        if data_home:
            credential_path = Path(data_home) / "devin" / "credentials.toml"
        else:
            home = source.get("HOME", "").strip()
            credential_path = (
                Path(home) if home else Path.home()
            ) / ".local" / "share" / "devin" / "credentials.toml"
        if credential_path.is_file() and credential_path.stat().st_size > 0:
            return "present"
    except OSError:
        pass
    try:
        result = subprocess.run(
            [binary or default_devin_binary(), "auth", "status"],
            stdin=subprocess.DEVNULL, capture_output=True, text=True,
            timeout=10, env=source,
        )
    except (OSError, subprocess.TimeoutExpired):
        return "unknown"
    status = (result.stdout + result.stderr).lower()
    if "not logged in" in status or "not authenticated" in status:
        return "absent"
    if result.returncode == 0 and "logged in" in status:
        return "present"
    return "unknown"


@lru_cache(maxsize=8)
def _model_ids(binary: str, epoch: int) -> tuple[str, ...]:
    del epoch  # Short-lived cache of public metadata, never credentials.
    try:
        result = subprocess.run(
            [binary, "models", "list", "--format", "json"],
            stdin=subprocess.DEVNULL, capture_output=True, text=True, timeout=20,
        )
        if result.returncode:
            return ()
        data = json.loads(result.stdout)
        return tuple(dict.fromkeys(
            str(variant["model_uid"])
            for family in data.get("families", [])
            for variant in family.get("variants", [])
            if variant.get("model_uid")
        ))
    except (OSError, subprocess.TimeoutExpired, ValueError, AttributeError, TypeError):
        return ()


def devin_model_ids(binary: Optional[str] = None) -> list[str]:
    return list(_model_ids(binary or default_devin_binary(), int(time.time() // 60)))


class DevinAcpAdapter(BaseAcpAdapter):
    adapter_id = "devin.acp"

    def __init__(self, *, binary: Optional[str] = None, **kwargs: Any) -> None:
        super().__init__(**kwargs)
        self._binary = binary or default_devin_binary()

    def _agent_argv(self) -> list[str]:
        return [self._binary, "acp"]

    def _agent_argv_for_request(self, request: SessionStart) -> list[str]:
        argv = self._agent_argv()
        if request.model:
            argv.extend(["--model", request.model])
        return argv

    def _select_session_auth_method(
        self, auth_methods: list[dict[str, Any]], env: dict[str, str],
    ) -> Optional[str]:
        # session/new reports invalid/missing credentials. Never start a browser
        # login flow from a server-side conversation.
        return None

    def _resolve_binding(self, request: SessionStart) -> None:
        # The Conversation executor supplies a binding service to all adapters.
        # Devin currently uses only its native local tools, not gateway tools.
        # Composer menus must mirror this: see
        # muteki.conversation.composer_capabilities._ENGINES_WITHOUT_CAPABILITY_GATEWAY.
        return None

    async def probe(self, request: ProbeRequest) -> AgentCapabilities:
        caps = await super().probe(request)
        try:
            result = await asyncio.to_thread(
                subprocess.run, [self._binary, "--version"],
                stdin=subprocess.DEVNULL, capture_output=True, text=True, timeout=10,
            )
            if result.returncode == 0:
                caps.runtime_version = result.stdout.strip()
        except (OSError, subprocess.TimeoutExpired):
            pass
        caps.supported_models = (
            await asyncio.to_thread(devin_model_ids, self._binary)
            if request.include_models else []
        )
        caps.user_input = False  # Provider-specific question requests unverified.
        if self._probe_cache is not None:
            self._probe_cache.binary_path = self._binary
            self._probe_cache.field_sources["user_input"] = "static"
            login = await asyncio.to_thread(
                devin_login_status, self._binary, {**os.environ, **self._env_extra})
            self._probe_cache.healthy_override = bool(
                caps.protocol_version and caps.runtime_version and login == "present")
            if login != "present":
                self._probe_cache.detail = "本机 Devin 登录未确认；请执行 devin auth login"
        return caps

    async def _launch(self, request: SessionStart, plan: Any, bearer_token: Any) -> dict[str, Any]:
        if not request.thread_id or request.options.get("thread_mode", "conversation") != "conversation":
            raise ValueError("Devin ACP requires a Conversation thread")
        if plan is not None:
            raise ValueError("Devin ACP does not support capability gateway injection")
        return await super()._launch(request, plan, bearer_token)

    async def _after_session_open(
        self, transport: AcpTransport, session_id: str, request: SessionStart,
    ) -> None:
        setup = transport.session_setup(session_id)
        available = {
            str(mode.get("id")) for mode in
            (setup.get("modes") or {}).get("availableModes", [])
        }
        selected = request.permission_mode or {
            "supervised": "ask", "auto-accept-edits": "accept-edits",
            "auto": "smart", "full-access": "bypass",
        }.get(request.access_mode or "supervised", "ask")
        if selected not in available:
            raise AcpError(f"Devin does not advertise permission mode {selected!r}")
        if request.sandbox_mode:
            raise AcpError("Devin ACP sandbox selection is not supported")
        if request.effort not in (None, "", "default"):
            raise AcpError("Choose a Devin model variant instead of a separate effort")
        await transport.set_mode(session_id, selected)
        # Devin uses ACP configOptions, including on session/load. Applying the
        # choice here also covers resumed sessions whose old model differs.
        if request.model:
            model_option = next((item for item in setup.get("configOptions", [])
                                 if item.get("category") == "model"
                                 or item.get("id") == "model"), None)
            if model_option is None:
                raise AcpError("Devin did not advertise a model configuration option")
            response = await transport.peer.request("session/set_config_option", {
                "sessionId": session_id, "configId": model_option["id"],
                "value": request.model,
            }, timeout=30)
            check_response("session/set_config_option", response)

    def _turn_result_usage(self, result: dict[str, Any]) -> dict[str, Any]:
        usage = result.get("usage") or {}
        if not isinstance(usage, dict):
            return {}
        normalized: dict[str, Any] = {
            "source": "devin-acp",
            "input_includes_cache": True,
        }
        for sources, target in (
            (("inputTokens", "input_tokens"), "input_tokens"),
            (("outputTokens", "output_tokens"), "output_tokens"),
            (("cacheReadTokens", "cachedTokens", "cache_read_tokens"),
             "cache_read_tokens"),
            (("cacheWriteTokens", "cache_write_tokens"),
             "cache_write_tokens"),
            (("reasoningTokens", "reasoning_tokens"), "reasoning_tokens"),
        ):
            for source in sources:
                value = usage.get(source)
                if isinstance(value, int) and not isinstance(value, bool) and value >= 0:
                    normalized[target] = value
                    break
        cost = usage.get("costUsd", usage.get("cost_usd"))
        if isinstance(cost, (int, float)) and not isinstance(cost, bool) and cost >= 0:
            normalized["reported_cost"] = float(cost)
        return normalized if len(normalized) > 2 else {}
