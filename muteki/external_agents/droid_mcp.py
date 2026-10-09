"""Stdio Muteki Control injection for Factory Droid.

Droid's ACP initialize does not advertise HTTP MCP, and the SDK path takes
stdio servers only. Both chat transports therefore launch the existing
muteki-control stdio bridge. The bearer token is written only to a mode-0600
connection file inside a private temporary directory, never to argv, and the
directory is removed when the session closes.
"""

from __future__ import annotations

import json
import logging
import os
import shutil
import sys
import tempfile
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Optional

from muteki.capability_bindings.acp_config import DEFAULT_SERVER_NAME
from muteki.platform.contracts.capabilities import CapabilityInjectionPlan

from .acp import materialize_mcp_servers

_LOG = logging.getLogger(__name__)

_STDIO_SERVER = (
    Path(__file__).resolve().parents[1]
    / "agent_plugins" / "muteki-control" / "mcp" / "stdio_server.py"
)


class DroidMcpError(RuntimeError):
    def __init__(self, code: str, message: str) -> None:
        super().__init__(message)
        self.code = code


@dataclass
class DroidMcp:
    """ACP-shaped stdio servers plus what the caller must report or clean up."""

    servers: list[dict[str, Any]] = field(default_factory=list)
    #: HTTP servers that were not injected; the caller reports them.
    dropped: list[str] = field(default_factory=list)
    #: Private directory holding the connection file, or None when unused.
    data_dir: Optional[Path] = None

    def remove(self) -> None:
        if self.data_dir is None:
            return
        directory, self.data_dir = self.data_dir, None
        try:
            shutil.rmtree(directory)
        except FileNotFoundError:
            return
        except OSError:
            _LOG.warning("droid MCP directory %s was not removed", directory, exc_info=True)


def stdio_mcp_servers(
    plan: Optional[CapabilityInjectionPlan],
    bearer_token: Optional[str],
    *,
    gateway_endpoint: str = "",
) -> DroidMcp:
    """Shape the injection plan for Droid.

    Native stdio entries from the plan are kept. The Muteki gateway entry is
    replaced by the stdio bridge. Other HTTP servers are listed in ``dropped``
    so the caller can report them instead of pretending they were connected.
    """
    raw = materialize_mcp_servers(plan, bearer_token)
    endpoint = str(gateway_endpoint or getattr(plan, "gateway_endpoint", "") or "").strip()
    result = DroidMcp()
    needs_control = False
    for entry in raw:
        name = str(entry.get("name") or "").strip()
        if entry.get("command"):
            result.servers.append(_stdio_server(entry))
            continue
        url = str(entry.get("url") or entry.get("httpUrl") or "")
        if name == DEFAULT_SERVER_NAME or (endpoint and url == endpoint):
            needs_control = True
            endpoint = endpoint or url
        elif url:
            result.dropped.append(name or url)
    if not needs_control:
        return result
    if not endpoint or not bearer_token:
        raise DroidMcpError(
            "droid.mcp.token_missing",
            "Droid MCP injection has no gateway endpoint or bearer token",
        )
    if not _STDIO_SERVER.is_file():
        raise DroidMcpError(
            "droid.mcp.bridge_missing",
            f"muteki-control stdio bridge is missing: {_STDIO_SERVER}",
        )
    result.data_dir = Path(tempfile.mkdtemp(prefix="muteki-droid-mcp-"))
    try:
        config = result.data_dir / "connection.json"
        descriptor = os.open(config, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        with os.fdopen(descriptor, "w", encoding="utf-8") as handle:
            json.dump({"endpoint": endpoint, "bearer_token": bearer_token}, handle)
    except BaseException:
        result.remove()
        raise
    result.servers.append(_stdio_server({
        "name": DEFAULT_SERVER_NAME,
        "command": sys.executable,
        "args": [str(_STDIO_SERVER), "--config", str(config)],
    }))
    return result


def _stdio_server(entry: dict[str, Any]) -> dict[str, Any]:
    """Shape a server the way Droid ACP validates stdio MCP.

    Omitting ``type`` makes Droid validate the entry as HTTP/SSE and reject
    the whole ``session/new`` with ``Invalid params``. ``env`` must be an
    array of ``{name, value}`` objects; a mapping is also rejected.
    """
    env = entry.get("env")
    if isinstance(env, dict):
        env_rows = [{"name": str(key), "value": str(value)} for key, value in env.items()]
    elif isinstance(env, list):
        env_rows = [dict(item) for item in env if isinstance(item, dict)]
    else:
        env_rows = []
    return {
        "type": "stdio",
        "name": str(entry.get("name") or ""),
        "command": str(entry.get("command") or ""),
        "args": [str(arg) for arg in entry.get("args") or []],
        "env": env_rows,
    }
