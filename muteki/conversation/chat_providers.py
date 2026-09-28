"""Engine-owned native configuration discovery for Muteki conversations.

Providers describe only their engine. Runtime adapters remain authoritative for
commands: a TUI command is never promoted to a callable chat capability here.
"""
from __future__ import annotations

from dataclasses import dataclass
from hashlib import sha256
import json
import os
from pathlib import Path
import re
import tomllib
from typing import Any


@dataclass(frozen=True)
class ChatEngineProvider:
    engine: str
    home_variable: str
    home_relative: str
    transport: str
    mcp_files: tuple[str, ...] = ("mcp.json",)

    def native_root(self) -> Path:
        value = os.environ.get(self.home_variable)
        if self.engine == "opencode":
            return Path(value).expanduser() / "opencode" if value else Path.home() / self.home_relative
        return Path(value).expanduser() if value else Path.home() / self.home_relative

    def native_mcp(self) -> list[dict[str, Any]]:
        rows: dict[str, dict[str, Any]] = {}
        paths = [self.native_root() / filename for filename in self.mcp_files]
        if self.engine == "claude":
            paths.append(Path.home() / ".claude.json")
        for path in paths:
            try:
                if not path.is_file() or path.stat().st_size > 2_000_000:
                    continue
                value = tomllib.loads(path.read_text()) if path.suffix == ".toml" else json.loads(path.read_text())
                servers = value.get("mcpServers", value.get("mcp_servers", value.get("mcp", {})))
                if not isinstance(servers, dict):
                    continue
                for name, config in servers.items():
                    if isinstance(config, dict):
                        rows[name] = {"name": name, "source": self.engine, "kind": "mcp",
                                      "enabled": config.get("enabled", True) and not config.get("disabled", False),
                                      "status": "configured", "transport": "stdio" if config.get("command") else "http"}
            except (OSError, ValueError, TypeError):
                continue
        # Only identities/status are public; command args, env, URLs and headers
        # often contain secrets. Runtime connection status is a separate fact.
        return list(rows.values())

    def revision(self) -> str:
        root = self.native_root()
        paths = [root / name for name in ("config.toml", "settings.json", "mcp.json", "auth.json", ".credentials.json",
                                         "models.json", "settings.yml", "models.yml", "cli-config.json", "plugins", "skills")]
        for name in ("credentials", "oauth"):
            if (root / name).is_dir():
                paths.extend((root / name).glob("*"))
        stamps = []
        for path in paths:
            try:
                stat = path.stat()
                stamps.append((str(path), stat.st_mtime_ns, stat.st_size))
            except OSError:
                pass
        return sha256(json.dumps(stamps).encode()).hexdigest()[:16]

    def plugin_skill_roots(self) -> list[tuple[str, Path]]:
        """Only enabled native plugins, never another engine's cache."""
        root = self.native_root()
        packages: list[tuple[str, Path]] = []
        try:
            if self.engine == "codex":
                config = tomllib.loads((root / "config.toml").read_text())
                for identity, value in config.get("plugins", {}).items():
                    if not isinstance(value, dict) or value.get("enabled") is not True:
                        continue
                    if not re.fullmatch(r"[\w.-]+@[\w.-]+", identity):
                        continue
                    name, market = identity.split("@", 1)
                    cache = root / "plugins/cache" / market / name
                    versions = [p for p in cache.iterdir() if p.is_dir()] if cache.is_dir() else []
                    if versions:
                        packages.append((name, max(versions, key=lambda p: p.stat().st_mtime_ns)))
            elif self.engine == "claude":
                enabled = json.loads((root / "settings.json").read_text()).get("enabledPlugins", {})
                installed = json.loads((root / "plugins/installed_plugins.json").read_text()).get("plugins", {})
                for identity, versions in installed.items():
                    if enabled.get(identity) is not True or not isinstance(versions, list):
                        continue
                    for version in versions:
                        path = Path(version.get("installPath", ""))
                        if path.is_absolute() and path.is_dir():
                            packages.append((identity.split("@")[0], path))
                            break
        except (OSError, ValueError, TypeError):
            pass
        return packages


PROVIDERS = {p.engine: p for p in (
    ChatEngineProvider("claude", "CLAUDE_CONFIG_DIR", ".claude", "Agent SDK", ("settings.json", "mcp.json")),
    ChatEngineProvider("codex", "CODEX_HOME", ".codex", "App Server", ("config.toml",)),
    ChatEngineProvider("cursor", "CURSOR_CONFIG_DIR", ".cursor", "ACP", ("mcp.json", "cli-config.json")),
    ChatEngineProvider("pi", "PI_CODING_AGENT_DIR", ".pi/agent", "RPC"),
    ChatEngineProvider("omp", "PI_CODING_AGENT_DIR", ".omp/agent", "RPC", ("mcp.json", "settings.json")),
    ChatEngineProvider("kimi", "KIMI_CODE_HOME", ".kimi-code", "ACP / Wire", ("mcp.json", "config.toml")),
    ChatEngineProvider("grok", "GROK_HOME", ".grok", "ACP", ("mcp.json", "config.toml")),
    ChatEngineProvider("opencode", "XDG_CONFIG_HOME", ".config/opencode", "HTTP API", ("opencode.json",)),
)}


def provider_for(engine: str) -> ChatEngineProvider:
    try:
        return PROVIDERS[engine]
    except KeyError as exc:
        raise ValueError("未知 Agent") from exc
