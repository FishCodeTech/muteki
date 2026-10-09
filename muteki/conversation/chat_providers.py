"""Engine-owned native configuration discovery for Muteki conversations.

Providers describe only their engine. Runtime adapters remain authoritative for
commands: a TUI command is never promoted to a callable chat capability here.
"""
from __future__ import annotations

from dataclasses import dataclass
import json
import os
from pathlib import Path
import re
import tomllib
from typing import Any

from muteki.external_agents.descriptors import ProviderDescriptor, all_descriptors


@dataclass(frozen=True)
class ChatEngineProvider:
    descriptor: ProviderDescriptor

    @property
    def engine(self) -> str:
        return self.descriptor.engine

    @property
    def home_variable(self) -> str:
        return self.descriptor.environment.home_env_var

    @property
    def home_relative(self) -> str:
        return self.descriptor.environment.home_relative

    @property
    def transport(self) -> str:
        return self.descriptor.identity.transport_label

    @property
    def mcp_files(self) -> tuple[str, ...]:
        return self.descriptor.environment.mcp_files

    @property
    def managed_environment(self) -> bool:
        # Discovery and portable chat skills do not imply Gateway delivery or
        # a verified private-home/auth migration (Devin keeps its native home).
        return self.descriptor.environment.managed_home

    @property
    def gateway_tools(self) -> bool:
        return self.descriptor.capability_gateway

    def native_root(self) -> Path:
        environment = self.descriptor.environment
        value = os.environ.get(environment.home_env_var)
        if not value:
            return Path.home() / environment.home_relative
        root = Path(value).expanduser()
        return root / self.engine if environment.home_env_is_parent else root

    def native_mcp(self) -> list[dict[str, Any]]:
        if os.environ.get("MUTEKI_HOST_DISCOVERY", "1") == "0":
            return []
        rows: dict[str, dict[str, Any]] = {}
        paths = [self.native_root() / filename for filename in self.mcp_files]
        paths.extend(Path.home() / relative for relative in self.descriptor.environment.extra_mcp_home_files)
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

    @property
    def configuration_names(self) -> tuple[str, ...]:
        names = ("config.toml", "settings.json", "settings.yaml", "settings.yml", "models.json", "models.yml",
                 "mcp.json", "cli-config.json", "agent-cli-state.json", "acp-config.json", "opencode.json", "opencode.jsonc")
        return (*names, *self.descriptor.environment.extra_configuration_names)

    @property
    def credential_names(self) -> tuple[str, ...]:
        return ("auth.json", ".credentials.json", "credentials.json", "credentials", "oauth", "device_id")

    @property
    def asset_names(self) -> tuple[str, ...]:
        # Cursor's extensions directory contains desktop IDE extensions, not
        # Agent CLI capabilities. Only engines with native extensions import it.
        names = ("skills", "skills-cursor", "commands", "agents", "prompts", "plugins")
        return (*names, "extensions") if self.descriptor.environment.native_extension_assets else names

    def revision(self) -> str:
        if os.environ.get("MUTEKI_HOST_DISCOVERY", "1") == "0":
            return "host-discovery-disabled"
        from .native_environment import content_revision
        root = self.native_root()
        # Authentication rotation must not invalidate capability snapshots.
        # Cursor keeps its login/cache and recent model selection inside the
        # same file as permissions. Native CLI turns rewrite those values;
        # preserve permission/MCP changes without treating normal turns as a
        # capability change that discards the saved native conversation.
        cursor_runtime_fields = frozenset({
            "authInfo", "privacyCache", "autoReviewAvailabilityCache",
            "serverConfigCache", "model", "selectedModel", "modelParameters",
            "modelSelectionHistory", "modelSlashCommands", "hasChangedDefaultModel",
            "maxMode", "maxModeAutoEnabled", "exploreSubagentModel",
        })
        return content_revision([(name, root / name) for name in (*self.configuration_names, *self.asset_names)]
                                + [("common-skills", Path.home() / ".agents/skills")],
                                json_exclude={"cli-config.json": cursor_runtime_fields}
                                if self.engine == "cursor" else None)

    def plugin_skill_roots(self) -> list[tuple[str, Path]]:
        """Only enabled native plugins, never another engine's cache."""
        if os.environ.get("MUTEKI_HOST_DISCOVERY", "1") == "0":
            return []
        root = self.native_root()
        packages: list[tuple[str, Path]] = []
        try:
            manifest = self.descriptor.environment.plugin_manifest
            if manifest == "codex_config_toml":
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
            elif manifest == "claude_installed_plugins":
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


PROVIDERS: dict[str, ChatEngineProvider] = {
    descriptor.engine: ChatEngineProvider(descriptor) for descriptor in all_descriptors()
}


def provider_for(engine: str) -> ChatEngineProvider:
    try:
        return PROVIDERS[engine]
    except KeyError as exc:
        raise ValueError("未知 Agent") from exc
