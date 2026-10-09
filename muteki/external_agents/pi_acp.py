"""Pi ACP Adapter backed by the upstream ``@automatalabs/pi-acp`` bridge.

The bridge embeds Pi's published SDK, emits standard ACP permission requests
from Pi's ``beforeToolCall`` hook, and accepts ACP MCP servers. Muteki keeps the
bridge and Pi configuration inside its own sessions root; no extension is
installed into the user's normal Pi directory.
"""

from __future__ import annotations

import os
import shutil
from pathlib import Path
from typing import Any, Optional

from muteki.platform.contracts.external_agents import SessionStart

from .acp import AcpError, AcpTransport, BaseAcpAdapter


PI_ACP_PACKAGE = "@automatalabs/pi-acp@0.6.1"
_PI_CONFIG_FILES = ("auth.json", "models.json")


class PiAcpAdapter(BaseAcpAdapter):
    """Structured Pi runtime using the maintained open-source ACP bridge."""

    adapter_id = "pi.acp"
    # The bridge asks through ACP before every tool call, so supervised,
    # auto-accept-edits (edit kinds allowed by the shared callback) and
    # full-access are enforced here; Pi has no native auto policy.
    unsupported_access_mode_reasons = {
        "auto": (
            "the pi-acp bridge asks before every tool call and Pi has no "
            "native auto policy"
        ),
    }

    def __init__(
        self,
        *,
        binary: Optional[str] = None,
        runtime_root: Optional[str | Path] = None,
        **kwargs: Any,
    ) -> None:
        super().__init__(**kwargs)
        self._binary = binary or os.environ.get("MUTEKI_PI_ACP_BIN", "")
        self._runtime_root = Path(
            runtime_root
            or (Path(os.environ.get("TMPDIR") or "/tmp") / "muteki-pi-acp")
        ).expanduser().resolve()

    def _agent_argv(self) -> list[str]:
        if self._binary:
            return [self._binary]
        installed = shutil.which("pi-acp")
        if installed:
            return [installed]
        npx = shutil.which("npx") or "npx"
        return [
            npx,
            "--yes",
            f"--package={PI_ACP_PACKAGE}",
            "pi-acp",
        ]

    def _prepare_session_environment(
        self, request: SessionStart, env: dict[str, str], cwd: str
    ) -> dict[str, str]:
        del cwd
        prepared = dict(env)
        source = Path(
            prepared.get("PI_CODING_AGENT_DIR")
            or (Path.home() / ".pi" / "agent")
        ).expanduser()
        target = self._runtime_root / request.agent_session_id / "agent"
        target.mkdir(parents=True, exist_ok=True)
        try:
            target.chmod(0o700)
        except OSError:
            pass
        if source.resolve() != target.resolve():
            for name in _PI_CONFIG_FILES:
                source_file = source / name
                if not source_file.is_file():
                    continue
                target_file = target / name
                shutil.copy2(source_file, target_file)
                try:
                    target_file.chmod(0o600)
                except OSError:
                    pass
        cache = self._runtime_root / "npm-cache"
        cache.mkdir(parents=True, exist_ok=True)
        prepared["PI_CODING_AGENT_DIR"] = str(target)
        prepared.setdefault("npm_config_cache", str(cache))
        return prepared

    async def _after_session_open(
        self, transport: AcpTransport, session_id: str, request: SessionStart
    ) -> None:
        session_env = dict(request.options.env)
        model_id = str(
            request.model or session_env.get("MUTEKI_PI_MODEL") or ""
        ).strip()
        provider = str(session_env.get("MUTEKI_PI_PROVIDER") or "").strip()
        model_value = (
            f"{provider}/{model_id}"
            if provider and model_id and "/" not in model_id else model_id
        )
        if model_value:
            if "/" not in model_value:
                # Resolve an unqualified ID only when the Agent's own catalog
                # identifies one provider. Never silently keep Pi's default.
                options = transport.session_setup(session_id).get("configOptions", [])
                selector = next((item for item in options if item.get("id") == "model"), {})
                choices = [choice for group in selector.get("options", [])
                           for choice in (group.get("options", []) if "group" in group else [group])]
                matches = {str(choice["value"]) for choice in choices
                           if str(choice.get("value", "")).split("/", 1)[-1] == model_value}
                if len(matches) != 1:
                    raise AcpError(
                        f"Pi ACP cannot uniquely resolve model {model_value!r}; "
                        "specify provider/model or MUTEKI_PI_PROVIDER")
                model_value = matches.pop()
            await transport.set_config_option(session_id, "model", model_value)
        if request.effort:
            await transport.set_config_option(session_id, "thinkingLevel", str(request.effort))


__all__ = ["PI_ACP_PACKAGE", "PiAcpAdapter"]
