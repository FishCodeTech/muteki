"""Agent Plugins 1.0.0 package integration for Muteki capabilities.

The portable package is stored at ``muteki/agent_plugins/muteki-control`` and
contains the only plugin manifest and Agent Skill used by capability injection.
Runtime adapters may copy that package or its skill component into a native
discovery location, but they do not generate vendor-specific plugin manifests.
"""

from __future__ import annotations

import json
import re
import shutil
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Iterable, Mapping, Optional, Sequence

from muteki.platform.contracts.capabilities import (
    CapabilityDescriptor,
    CapabilityInjectionPlan,
    InjectionKind,
)


PLUGIN_NAME = "muteki-control"
SKILL_NAME = "muteki-control"
ENV_ENDPOINT = "MUTEKI_CAPABILITY_ENDPOINT"
ENV_TOKEN = "MUTEKI_CAPABILITY_TOKEN"
PLUGIN_SCHEMA = "https://agent-plugins.org/schemas/1.0.0/plugin.schema.json"
MCP_SCHEMA = "https://agent-plugins.org/schemas/1.0.0/mcp.schema.json"

# Agent Plugins 1.0.0 selects component schema through the exact ``$schema``
# URI.  We keep this local and closed: a package load must not fetch a schema
# from the network before an Agent Runtime can decide whether to trust it.
SUPPORTED_SCHEMA_VERSIONS = frozenset({"1.0.0"})
_SCHEMA_URLS = {
    "plugin": {"1.0.0": PLUGIN_SCHEMA},
    "mcp": {"1.0.0": MCP_SCHEMA},
}
_PLUGIN_FIELDS = frozenset({
    "$schema", "schemaVersion", "name", "version", "description", "author",
    "homepage", "repository", "license", "keywords", "extensions",
})
_MCP_FIELDS = frozenset({"$schema", "schemaVersion", "mcpServers"})
_MCP_SERVER_FIELDS = frozenset({
    "type", "transport", "command", "args", "cwd", "env", "url",
    "headers", "description", "disabled", "timeout",
})
_PLUGIN_NAME_RE = re.compile(
    r"^(?!.*(?:--|\.\.))[a-z0-9](?:[a-z0-9.-]*[a-z0-9])?$"
)
_SEMVER_RE = re.compile(
    r"^\d+\.\d+\.\d+(?:-[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?"
    r"(?:\+[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?$"
)
_NAMESPACE_RE = re.compile(r"^[a-z0-9][a-z0-9_-]*(?:\.[a-z0-9][a-z0-9_-]*)+$")
_SKILL_NAME_RE = re.compile(r"^[a-z0-9][a-z0-9._-]*$")
_TEMPLATE_RE = re.compile(r"\$\{([^}]+)\}")


@dataclass(frozen=True)
class AgentPluginDiagnostic:
    """Non-secret per-component diagnostic exposed by capability management."""

    component: str
    code: str
    message: str
    path: str = ""
    field: str = ""
    severity: str = "error"

    def as_dict(self) -> dict[str, str]:
        return {
            "component": self.component,
            "code": self.code,
            "message": self.message,
            "path": self.path,
            "field": self.field,
            "severity": self.severity,
        }


class AgentPluginLoadError(ValueError):
    """A requested standard package component is unavailable or invalid."""

    def __init__(
        self,
        message: str,
        diagnostics: Iterable[AgentPluginDiagnostic] = (),
    ) -> None:
        super().__init__(message)
        self.diagnostics = tuple(diagnostics)


@dataclass(frozen=True)
class AgentPluginComponent:
    """One independently discovered portable component."""

    name: str
    status: str
    path: Optional[Path] = None
    schema_version: str = ""
    data: Mapping[str, Any] = field(default_factory=dict)
    diagnostics: tuple[AgentPluginDiagnostic, ...] = ()

    @property
    def ready(self) -> bool:
        return self.status == "ready"

    def as_dict(self) -> dict[str, Any]:
        return {
            "name": self.name,
            "status": self.status,
            "path": str(self.path) if self.path else "",
            "schema_version": self.schema_version,
            "diagnostics": [item.as_dict() for item in self.diagnostics],
        }


@dataclass(frozen=True)
class AgentPluginSkill:
    """Skill discovered only at ``skills/<name>/SKILL.md``."""

    name: str
    root: Path
    manifest: Path

    def as_dict(self) -> dict[str, str]:
        return {"name": self.name, "root": str(self.root), "manifest": str(self.manifest)}


@dataclass(frozen=True)
class AgentPluginPackage:
    """Result of fixed-location Agent Plugins discovery.

    Components are intentionally isolated: a bad ``mcp.json`` does not hide a
    valid skill.  Adapters call :meth:`require` to gate only what they consume.
    """

    root: Path
    plugin: AgentPluginComponent
    mcp: AgentPluginComponent
    skills: AgentPluginComponent
    skill_items: tuple[AgentPluginSkill, ...] = ()
    diagnostics: tuple[AgentPluginDiagnostic, ...] = ()

    @property
    def status(self) -> str:
        if not self.plugin.ready:
            return "invalid"
        if self.mcp.status == "invalid" or self.skills.status == "invalid":
            return "degraded"
        if self.mcp.ready or self.skills.ready:
            return "verified"
        return "incomplete"

    @property
    def verified(self) -> bool:
        return self.status in {"verified", "degraded"}

    @property
    def manifest(self) -> Mapping[str, Any]:
        return self.plugin.data if self.plugin.ready else {}

    @property
    def schema_version(self) -> str:
        return self.plugin.schema_version

    def component(self, name: str) -> AgentPluginComponent:
        selected = str(name or "").strip().lower()
        if selected == "plugin":
            return self.plugin
        if selected == "mcp":
            return self.mcp
        if selected == "skills":
            return self.skills
        raise KeyError(f"unknown Agent Plugin component: {name}")

    def require(self, components: Sequence[str] = ()) -> "AgentPluginPackage":
        required = [str(name).strip().lower() for name in components if str(name).strip()]
        missing: list[str] = []
        if not self.plugin.ready:
            missing.append("plugin")
        for name in required:
            try:
                if not self.component(name).ready:
                    missing.append(name)
            except KeyError:
                missing.append(name)
        if missing:
            related = tuple(
                item for item in self.diagnostics
                if item.component in set(missing) or item.component == "package"
            )
            suffix = "; ".join(
                f"{item.component}:{item.code}" for item in related[:6]
            ) or "component missing"
            raise AgentPluginLoadError(
                "Agent Plugin package is not usable for " + ", ".join(missing)
                + f" ({suffix})",
                related or self.diagnostics,
            )
        return self

    def trusted_paths(self) -> dict[str, Any]:
        """Containment-checked absolute paths an adapter may consume."""
        return {
            "plugin_root": str(self.root),
            "plugin_manifest": str(self.plugin.path) if self.plugin.path else "",
            "mcp_config": str(self.mcp.path) if self.mcp.path else "",
            "skills_root": str(self.skills.path) if self.skills.path else "",
            "skills": [item.as_dict() for item in self.skill_items],
        }

    def as_dict(self) -> dict[str, Any]:
        return {
            "status": self.status,
            "verified": self.verified,
            "schema_version": self.schema_version,
            "root": str(self.root),
            "components": {
                "plugin": self.plugin.as_dict(),
                "mcp": self.mcp.as_dict(),
                "skills": self.skills.as_dict(),
            },
            "skills": [item.as_dict() for item in self.skill_items],
            "diagnostics": [item.as_dict() for item in self.diagnostics],
        }


@dataclass(frozen=True)
class AgentPluginRuntimeMaterialization:
    """Runtime-only package paths and rendered MCP config.

    The token is held only in ``connection_config``; ``as_runtime_config``
    deliberately returns no connection file contents.
    """

    package: AgentPluginPackage
    plugin_data: Path
    connection_config: Path
    mcp_servers: Mapping[str, Any]

    def as_runtime_config(self) -> dict[str, Any]:
        return {
            **self.package.trusted_paths(),
            "plugin_data": str(self.plugin_data),
            "connection_config": str(self.connection_config),
            "mcp_servers": _json_clone(self.mcp_servers),
            "agent_plugins_spec": self.package.schema_version,
            "plugin_status": self.package.status,
        }


def package_root() -> Path:
    """Return the bundled fixed package root, without claiming validity."""
    return Path(__file__).resolve().parents[1] / "agent_plugins" / PLUGIN_NAME


def discover_agent_plugin(root: str | Path) -> AgentPluginPackage:
    """Discover ``plugin.json``, ``mcp.json`` and ``skills/`` independently.

    Invalid components remain represented by diagnostics instead of throwing
    from discovery.  This lets management UIs show an exact failure and lets an
    adapter continue using another verified component when it is sufficient.
    """
    requested = Path(root).expanduser()
    try:
        real_root = requested.resolve(strict=True)
        if not real_root.is_dir():
            raise OSError("not a directory")
    except (OSError, RuntimeError) as exc:
        problem = _diagnostic(
            "package", "agent_plugin.root_unavailable",
            f"package root is not a readable directory: {exc}", requested,
        )
        return AgentPluginPackage(
            root=requested.absolute(),
            plugin=AgentPluginComponent("plugin", "invalid", diagnostics=(problem,)),
            mcp=AgentPluginComponent("mcp", "missing"),
            skills=AgentPluginComponent("skills", "missing"),
            diagnostics=(problem,),
        )
    plugin = _discover_plugin_manifest(real_root)
    mcp = _discover_mcp_manifest(real_root)
    skills, skill_items = _discover_skills(real_root)
    diagnostics = tuple([*plugin.diagnostics, *mcp.diagnostics, *skills.diagnostics])
    return AgentPluginPackage(
        root=real_root,
        plugin=plugin,
        mcp=mcp,
        skills=skills,
        skill_items=skill_items,
        diagnostics=diagnostics,
    )


def load_agent_plugin(
    root: str | Path,
    *,
    required_components: Sequence[str] = (),
) -> AgentPluginPackage:
    """Discover a package and require only the selected components."""
    return discover_agent_plugin(root).require(required_components)


def builtin_agent_plugin(
    *, required_components: Sequence[str] = ("mcp", "skills"),
) -> AgentPluginPackage:
    """Load Muteki Control as a verified Agent Plugins package."""
    return load_agent_plugin(package_root(), required_components=required_components)


def require_verified_agent_plugin(
    root: str | Path,
    *,
    required_components: Sequence[str] = ("mcp", "skills"),
) -> AgentPluginPackage:
    """Hard gate for adapters before they hand paths to an external runtime."""
    return load_agent_plugin(root, required_components=required_components)


def load_manifest() -> dict[str, Any]:
    """Load Muteki Control's validated portable ``plugin.json`` manifest."""
    return dict(builtin_agent_plugin(required_components=()).manifest)


def _discover_plugin_manifest(root: Path) -> AgentPluginComponent:
    path = root / "plugin.json"
    try:
        path = _resolve_contained(
            root, path, component="plugin", field="plugin.json", must_exist=True,
        )
        if not path.is_file():
            _fail("plugin", "agent_plugin.invalid_manifest", "plugin.json is not a file", path)
        data = _read_json_object(path, "plugin")
        version = _select_schema_version(data, component="plugin", path=path)
        _validate_plugin_fields(data, path)
        return AgentPluginComponent(
            "plugin", "ready", path=path, schema_version=version,
            data=_json_clone(data),
        )
    except AgentPluginLoadError as exc:
        return AgentPluginComponent(
            "plugin", "invalid", path=_safe_resolve(path),
            diagnostics=exc.diagnostics or (
                _diagnostic("plugin", "agent_plugin.invalid_manifest", str(exc), path),
            ),
        )


def _discover_mcp_manifest(root: Path) -> AgentPluginComponent:
    path = root / "mcp.json"
    if not path.exists():
        return AgentPluginComponent(
            "mcp", "missing",
            diagnostics=(_diagnostic(
                "mcp", "agent_plugin.component_missing",
                "optional root mcp.json is not present", path, severity="info",
            ),),
        )
    try:
        path = _resolve_contained(
            root, path, component="mcp", field="mcp.json", must_exist=True,
        )
        if not path.is_file():
            _fail("mcp", "agent_plugin.invalid_mcp", "mcp.json is not a file", path)
        data = _read_json_object(path, "mcp")
        version = _select_schema_version(data, component="mcp", path=path)
        _validate_mcp_fields(data, root, path)
        return AgentPluginComponent(
            "mcp", "ready", path=path, schema_version=version,
            data=_json_clone(data),
        )
    except AgentPluginLoadError as exc:
        return AgentPluginComponent(
            "mcp", "invalid", path=_safe_resolve(path),
            diagnostics=exc.diagnostics or (
                _diagnostic("mcp", "agent_plugin.invalid_mcp", str(exc), path),
            ),
        )


def _discover_skills(
    root: Path,
) -> tuple[AgentPluginComponent, tuple[AgentPluginSkill, ...]]:
    path = root / "skills"
    if not path.exists():
        return AgentPluginComponent(
            "skills", "missing",
            diagnostics=(_diagnostic(
                "skills", "agent_plugin.component_missing",
                "optional skills/ directory is not present", path, severity="info",
            ),),
        ), ()
    try:
        skills_root = _resolve_contained(
            root, path, component="skills", field="skills", must_exist=True,
            require_dir=True,
        )
    except AgentPluginLoadError as exc:
        return AgentPluginComponent(
            "skills", "invalid", path=_safe_resolve(path), diagnostics=exc.diagnostics,
        ), ()

    diagnostics: list[AgentPluginDiagnostic] = []
    items: list[AgentPluginSkill] = []
    try:
        children = sorted(skills_root.iterdir(), key=lambda item: item.name.casefold())
    except OSError as exc:
        error = _diagnostic(
            "skills", "agent_plugin.skills_unreadable",
            f"cannot enumerate skills/: {exc}", skills_root,
        )
        return AgentPluginComponent(
            "skills", "invalid", path=skills_root, diagnostics=(error,),
        ), ()
    for child in children:
        if child.name.startswith("."):
            continue
        try:
            skill_root = _resolve_contained(
                root, child, component="skills", field=f"skills/{child.name}",
                must_exist=True, require_dir=True,
            )
            if not _SKILL_NAME_RE.fullmatch(skill_root.name):
                _fail(
                    "skills", "agent_plugin.invalid_skill_name",
                    "skill directory name must use lowercase letters, digits, dot, dash, or underscore",
                    skill_root, field=f"skills/{child.name}",
                )
            manifest = _resolve_contained(
                root, skill_root / "SKILL.md", component="skills",
                field=f"skills/{skill_root.name}/SKILL.md", must_exist=True,
            )
            if not manifest.is_file():
                _fail(
                    "skills", "agent_plugin.invalid_skill_manifest",
                    "standard skill manifest must be a regular SKILL.md file",
                    manifest, field=f"skills/{skill_root.name}/SKILL.md",
                )
            _validate_skill_manifest(manifest, skill_root.name)
            items.append(AgentPluginSkill(skill_root.name, skill_root, manifest))
        except AgentPluginLoadError as exc:
            diagnostics.extend(exc.diagnostics or (
                _diagnostic("skills", "agent_plugin.invalid_skill", str(exc), child),
            ))
    if not items:
        if not diagnostics:
            diagnostics.append(_diagnostic(
                "skills", "agent_plugin.no_skills",
                "skills/ contains no standard skills", skills_root, severity="info",
            ))
        status = "invalid" if any(item.severity == "error" for item in diagnostics) else "missing"
        return AgentPluginComponent(
            "skills", status, path=skills_root, diagnostics=tuple(diagnostics),
        ), ()
    # A bad sibling Skill remains diagnosed but cannot hide valid skill roots.
    return AgentPluginComponent(
        "skills", "ready", path=skills_root, diagnostics=tuple(diagnostics),
    ), tuple(items)


def _read_json_object(path: Path, component: str) -> dict[str, Any]:
    try:
        raw = path.read_text(encoding="utf-8")
    except (OSError, UnicodeError) as exc:
        _fail(
            component, "agent_plugin.component_unreadable",
            f"cannot read {path.name}: {exc}", path,
        )
    try:
        data = json.loads(raw)
    except json.JSONDecodeError as exc:
        _fail(
            component, "agent_plugin.invalid_json",
            f"cannot parse {path.name}: {exc.msg}", path,
        )
    if not isinstance(data, dict):
        _fail(
            component, "agent_plugin.invalid_shape",
            f"{path.name} root must be an object", path,
        )
    return data


def _select_schema_version(
    data: Mapping[str, Any], *, component: str, path: Path,
) -> str:
    schema = data.get("$schema")
    if not isinstance(schema, str):
        _fail(
            component, "agent_plugin.schema_missing",
            "$schema must select a supported Agent Plugins schema", path,
            field="$schema",
        )
    selected = next(
        (version for version, url in _SCHEMA_URLS[component].items() if schema == url),
        "",
    )
    if not selected or selected not in SUPPORTED_SCHEMA_VERSIONS:
        _fail(
            component, "agent_plugin.unsupported_schema",
            "$schema must be one of: " + ", ".join(_SCHEMA_URLS[component].values()),
            path, field="$schema",
        )
    # Some clients emit an explicit mirror.  It is accepted only when it agrees
    # with $schema; $schema remains the source of selection.
    explicit = data.get("schemaVersion")
    if explicit is not None and _normalize_schema_version(explicit) != selected:
        _fail(
            component, "agent_plugin.schema_version_mismatch",
            "schemaVersion must match the version selected by $schema", path,
            field="schemaVersion",
        )
    return selected


def _normalize_schema_version(value: Any) -> str:
    text = str(value or "").strip()
    return text + ".0" if re.fullmatch(r"\d+\.\d+", text) else text


def _validate_plugin_fields(data: Mapping[str, Any], path: Path) -> None:
    _reject_unknown_fields(data, _PLUGIN_FIELDS, "plugin", path)
    for name in ("name", "version", "description", "homepage", "repository", "license"):
        if name in data and not isinstance(data[name], str):
            _fail(
                "plugin", "agent_plugin.invalid_field_type",
                f"{name} must be a string", path, field=name,
            )
    name = data.get("name")
    if not isinstance(name, str) or not _PLUGIN_NAME_RE.fullmatch(name) or len(name) > 128:
        _fail(
            "plugin", "agent_plugin.invalid_name",
            "name must be a lowercase package name with safe punctuation", path,
            field="name",
        )
    version = data.get("version")
    if not isinstance(version, str) or not _SEMVER_RE.fullmatch(version):
        _fail(
            "plugin", "agent_plugin.invalid_version",
            "version must be semantic version X.Y.Z", path, field="version",
        )
    author = data.get("author")
    if author is not None:
        if not isinstance(author, Mapping):
            _fail("plugin", "agent_plugin.invalid_author", "author must be an object", path, field="author")
        _reject_unknown_fields(
            author, frozenset({"name", "email", "url"}), "plugin", path,
            prefix="author.",
        )
        if any(not isinstance(value, str) for value in author.values()):
            _fail(
                "plugin", "agent_plugin.invalid_author",
                "author values must be strings", path, field="author",
            )
    keywords = data.get("keywords")
    if keywords is not None and (
        not isinstance(keywords, list)
        or any(not isinstance(value, str) for value in keywords)
    ):
        _fail(
            "plugin", "agent_plugin.invalid_keywords",
            "keywords must be an array of strings", path, field="keywords",
        )
    extensions = data.get("extensions")
    if extensions is not None:
        if not isinstance(extensions, Mapping):
            _fail(
                "plugin", "agent_plugin.invalid_extensions",
                "extensions must be an object", path, field="extensions",
            )
        for namespace, declaration in extensions.items():
            if not isinstance(namespace, str) or not _NAMESPACE_RE.fullmatch(namespace):
                _fail(
                    "plugin", "agent_plugin.invalid_extension_namespace",
                    "extensions keys must use reverse-domain syntax", path,
                    field="extensions",
                )
            if not isinstance(declaration, Mapping):
                _fail(
                    "plugin", "agent_plugin.invalid_extension_declaration",
                    "each extensions value must be an object", path,
                    field=f"extensions.{namespace}",
                )


def _validate_mcp_fields(
    data: Mapping[str, Any], root: Path, path: Path,
) -> None:
    _reject_unknown_fields(data, _MCP_FIELDS, "mcp", path)
    servers = data.get("mcpServers")
    if not isinstance(servers, Mapping):
        _fail(
            "mcp", "agent_plugin.invalid_mcp_servers",
            "mcpServers must be an object", path, field="mcpServers",
        )
    for server_name, declaration in servers.items():
        prefix = f"mcpServers.{server_name}"
        if not isinstance(server_name, str) or not server_name.strip():
            _fail(
                "mcp", "agent_plugin.invalid_server_name",
                "MCP server name must be a non-empty string", path,
                field="mcpServers",
            )
        if not isinstance(declaration, Mapping):
            _fail(
                "mcp", "agent_plugin.invalid_server",
                "each mcpServers member must be an object", path, field=prefix,
            )
        _reject_unknown_fields(
            declaration, _MCP_SERVER_FIELDS, "mcp", path, prefix=prefix + ".",
        )
        transport = declaration.get("type", declaration.get("transport", "stdio"))
        if transport not in {"stdio", "http", "sse", "streamable-http"}:
            _fail(
                "mcp", "agent_plugin.invalid_transport",
                "server type must be stdio, http, sse, or streamable-http", path,
                field=prefix + ".type",
            )
        if transport == "stdio":
            command = declaration.get("command")
            if not isinstance(command, str) or not command.strip():
                _fail(
                    "mcp", "agent_plugin.invalid_stdio_command",
                    "stdio server requires a non-empty command", path,
                    field=prefix + ".command",
                )
            args = declaration.get("args", [])
            if not isinstance(args, list) or any(not isinstance(item, str) for item in args):
                _fail(
                    "mcp", "agent_plugin.invalid_stdio_args",
                    "stdio args must be an array of strings", path,
                    field=prefix + ".args",
                )
        else:
            url = declaration.get("url")
            if not isinstance(url, str) or not url.strip():
                _fail(
                    "mcp", "agent_plugin.invalid_remote_url",
                    "remote MCP server requires a non-empty url", path,
                    field=prefix + ".url",
                )
        if "cwd" in declaration:
            cwd = declaration["cwd"]
            if not isinstance(cwd, str) or not cwd.strip():
                _fail("mcp", "agent_plugin.invalid_cwd", "cwd must be a string", path, field=prefix + ".cwd")
            _validate_template_path(cwd, root, path, prefix + ".cwd")
        if "env" in declaration and not _is_string_map(declaration["env"]):
            _fail(
                "mcp", "agent_plugin.invalid_env",
                "env must be an object with string keys and values", path,
                field=prefix + ".env",
            )
        if "headers" in declaration and not _is_string_map(declaration["headers"]):
            _fail(
                "mcp", "agent_plugin.invalid_headers",
                "headers must be an object with string keys and values", path,
                field=prefix + ".headers",
            )
        for index, value in enumerate(declaration.get("args") or []):
            _validate_template_path(value, root, path, f"{prefix}.args[{index}]", optional=True)


def _validate_skill_manifest(path: Path, expected_name: str) -> None:
    """Check the fixed standard skill manifest without inventing a new schema."""
    try:
        text = path.read_text(encoding="utf-8")
    except (OSError, UnicodeError) as exc:
        _fail("skills", "agent_plugin.skill_unreadable", f"cannot read SKILL.md: {exc}", path)
    if not text.strip():
        _fail("skills", "agent_plugin.empty_skill", "SKILL.md must not be empty", path)
    # Front matter evolves with Agent Skills.  If a name is supplied, it has to
    # agree with the fixed skills/<name> location; otherwise this remains a
    # portable standard Skill rather than a Muteki-specific format.
    if not text.startswith("---"):
        return
    lines = text.splitlines()
    closing = next((index for index in range(1, len(lines)) if lines[index].strip() == "---"), -1)
    if closing < 0:
        _fail(
            "skills", "agent_plugin.skill_frontmatter_unclosed",
            "SKILL.md front matter must have a closing ---", path,
        )
    name = next(
        (
            line.split(":", 1)[1].strip().strip("'\"")
            for line in lines[1:closing]
            if line.strip().startswith("name:") and ":" in line
        ),
        "",
    )
    if name and name != expected_name:
        _fail(
            "skills", "agent_plugin.skill_name_mismatch",
            "front matter name must equal the skills/<name> directory", path,
            field="name",
        )


def _is_string_map(value: Any) -> bool:
    return isinstance(value, Mapping) and all(
        isinstance(key, str) and isinstance(item, str)
        for key, item in value.items()
    )


def _validate_template_path(
    value: str,
    root: Path,
    manifest_path: Path,
    field: str,
    *,
    optional: bool = False,
) -> None:
    """Check `${PLUGIN_ROOT}` paths before rendering; data paths are later."""
    text = str(value)
    matches = tuple(_TEMPLATE_RE.finditer(text))
    if not matches:
        if text.startswith("./"):
            _resolve_relative_path(
                root, text, component="mcp", field=field, must_exist=not optional,
            )
        return
    if any(match.group(1) not in {"PLUGIN_ROOT", "PLUGIN_DATA"} for match in matches):
        _fail(
            "mcp", "agent_plugin.unknown_template",
            "only ${PLUGIN_ROOT} and ${PLUGIN_DATA} are supported", manifest_path,
            field=field,
        )
    if "${PLUGIN_ROOT}" in text:
        suffix = text.replace("${PLUGIN_ROOT}", "", 1)
        if suffix and not suffix.startswith("/"):
            _fail(
                "mcp", "agent_plugin.invalid_template_path",
                "${PLUGIN_ROOT} must be followed by / or end of value", manifest_path,
                field=field,
            )
        if suffix:
            _resolve_contained(
                root, root / suffix.lstrip("/"), component="mcp", field=field,
                must_exist=not optional,
            )
    if "${PLUGIN_DATA}" in text:
        suffix = text.replace("${PLUGIN_DATA}", "", 1)
        if suffix and (not suffix.startswith("/") or ".." in Path(suffix).parts):
            _fail(
                "mcp", "agent_plugin.invalid_template_path",
                "${PLUGIN_DATA} path must stay below plugin data", manifest_path,
                field=field,
            )


def _resolve_relative_path(
    root: Path,
    value: str,
    *,
    component: str,
    field: str,
    must_exist: bool,
) -> Path:
    text = str(value or "").strip()
    if not text.startswith("./") or ".." in Path(text).parts:
        _fail(
            component, "agent_plugin.unsafe_relative_path",
            "component path must start with ./ and stay inside plugin root", root,
            field=field,
        )
    return _resolve_contained(
        root, root / text[2:], component=component, field=field,
        must_exist=must_exist,
    )


def _resolve_contained(
    root: Path,
    candidate: Path,
    *,
    component: str,
    field: str,
    must_exist: bool,
    require_dir: bool = False,
) -> Path:
    """Resolve both paths and enforce realpath containment."""
    try:
        real_root = root.resolve(strict=True)
        target = candidate.resolve(strict=must_exist)
    except (OSError, RuntimeError) as exc:
        _fail(
            component, "agent_plugin.path_unavailable",
            f"cannot resolve component path: {exc}", candidate, field=field,
        )
    if target != real_root and real_root not in target.parents:
        _fail(
            component, "agent_plugin.path_escape",
            "resolved component path must stay inside the plugin root", candidate,
            field=field,
        )
    if require_dir and not target.is_dir():
        _fail(
            component, "agent_plugin.path_not_directory",
            "component path must be a directory", candidate, field=field,
        )
    return target


def _reject_unknown_fields(
    data: Mapping[str, Any],
    allowed: frozenset[str],
    component: str,
    path: Path,
    *,
    prefix: str = "",
) -> None:
    unknown = sorted(str(key) for key in data if key not in allowed)
    if unknown:
        _fail(
            component, "agent_plugin.unknown_field",
            "unsupported closed-schema field(s): " + ", ".join(unknown), path,
            field=prefix.rstrip("."),
        )


def _diagnostic(
    component: str,
    code: str,
    message: str,
    path: str | Path,
    *,
    field: str = "",
    severity: str = "error",
) -> AgentPluginDiagnostic:
    return AgentPluginDiagnostic(component, code, message, str(path), field, severity)


def _fail(
    component: str,
    code: str,
    message: str,
    path: str | Path,
    *,
    field: str = "",
) -> None:
    raise AgentPluginLoadError(
        message, (_diagnostic(component, code, message, path, field=field),),
    )


def _safe_resolve(path: Path) -> Optional[Path]:
    try:
        return path.resolve(strict=False)
    except (OSError, RuntimeError):
        return None


def _json_clone(value: Any) -> Any:
    return json.loads(json.dumps(value, ensure_ascii=False))


def resolve_client_extension_entry(
    package: AgentPluginPackage,
    namespace: str,
    *,
    field: str = "entry",
) -> Path:
    """Resolve an adapter-specific extension entry under its own namespace.

    ``extensions`` remains vendor-owned data in the portable manifest.  This
    helper enforces the conventional ``entry`` only when an adapter explicitly
    requests that extension, including realpath containment below the matching
    reverse-domain directory.
    """
    package.require(())
    normalized = str(namespace or "").strip().lower()
    if not _NAMESPACE_RE.fullmatch(normalized):
        _fail(
            "extension", "agent_plugin.invalid_extension_namespace",
            "extension namespace must use reverse-domain syntax", package.root,
            field="extensions",
        )
    extensions = package.manifest.get("extensions")
    declaration = extensions.get(normalized) if isinstance(extensions, Mapping) else None
    if not isinstance(declaration, Mapping):
        _fail(
            "extension", "agent_plugin.extension_not_declared",
            f"plugin does not declare extensions.{normalized}", package.root,
            field=f"extensions.{normalized}",
        )
    raw = declaration.get(field)
    if not isinstance(raw, str) or not raw.strip():
        _fail(
            "extension", "agent_plugin.invalid_extension_entry",
            f"extensions.{normalized}.{field} must be a non-empty relative string",
            package.root, field=f"extensions.{normalized}.{field}",
        )
    namespace_root = _resolve_contained(
        package.root, package.root / normalized, component="extension",
        field=f"extensions.{normalized}", must_exist=True, require_dir=True,
    )
    entry = _resolve_relative_path(
        package.root, raw, component="extension",
        field=f"extensions.{normalized}.{field}", must_exist=True,
    )
    if entry == namespace_root or namespace_root not in entry.parents or not entry.is_file():
        _fail(
            "extension", "agent_plugin.extension_entry_outside_namespace",
            "extension entry must be a file below its namespace directory", entry,
            field=f"extensions.{normalized}.{field}",
        )
    return entry


def materialize_runtime(
    root: str | Path | AgentPluginPackage,
    *,
    plugin_data: str | Path,
    endpoint: str,
    bearer_token: str,
    required_components: Sequence[str] = ("mcp",),
) -> AgentPluginRuntimeMaterialization:
    """Materialize a verified package for an external Runtime.

    This is the common adapter hand-off: fixed package root, client-owned
    ``PLUGIN_DATA``, a private ``connection.json``, and rendered stdio MCP
    server declarations.  It is suitable for an ACP/HTTP adapter that needs to
    select stdio as well as for CLI/native adapters.  It carries no token in
    the returned public config.
    """
    package = root if isinstance(root, AgentPluginPackage) else discover_agent_plugin(root)
    package.require(required_components)
    data_root = Path(plugin_data).expanduser()
    data_root.mkdir(parents=True, exist_ok=True)
    try:
        data_root = data_root.resolve(strict=True)
    except (OSError, RuntimeError) as exc:
        _fail(
            "package", "agent_plugin.data_root_unavailable",
            f"cannot materialize plugin data root: {exc}", data_root,
        )
    if data_root == package.root or package.root in data_root.parents:
        _fail(
            "package", "agent_plugin.data_inside_package",
            "PLUGIN_DATA must be outside the immutable plugin root", data_root,
        )
    connection = write_connection_config(
        data_root, endpoint=endpoint, bearer_token=bearer_token,
    )
    servers: Mapping[str, Any] = {}
    if package.mcp.ready:
        raw_servers = package.mcp.data.get("mcpServers")
        if not isinstance(raw_servers, Mapping):  # already checked by discovery
            _fail(
                "mcp", "agent_plugin.invalid_mcp_servers",
                "verified mcp component has no mcpServers object", package.mcp.path or package.root,
            )
        servers = _render_mcp_servers(raw_servers, package.root, data_root)
    return AgentPluginRuntimeMaterialization(
        package=package,
        plugin_data=data_root,
        connection_config=connection,
        mcp_servers=servers,
    )


def _render_mcp_servers(
    servers: Mapping[str, Any],
    plugin_root: Path,
    plugin_data: Path,
) -> dict[str, Any]:
    rendered: dict[str, Any] = {}
    for name, declaration in servers.items():
        if not isinstance(name, str) or not isinstance(declaration, Mapping):
            _fail(
                "mcp", "agent_plugin.invalid_server",
                "verified MCP data contains an invalid server declaration", plugin_root,
            )
        value = _render_templates(_json_clone(declaration), plugin_root, plugin_data)
        if not isinstance(value, dict):
            _fail("mcp", "agent_plugin.invalid_server", "rendered MCP server is not an object", plugin_root)
        _assert_rendered_paths(value, name, plugin_root, plugin_data)
        rendered[name] = value
    return rendered


def _render_templates(value: Any, plugin_root: Path, plugin_data: Path) -> Any:
    if isinstance(value, str):
        def replace(match: re.Match[str]) -> str:
            name = match.group(1)
            if name == "PLUGIN_ROOT":
                return str(plugin_root)
            if name == "PLUGIN_DATA":
                return str(plugin_data)
            _fail(
                "mcp", "agent_plugin.unknown_template",
                "only ${PLUGIN_ROOT} and ${PLUGIN_DATA} are supported", plugin_root,
                field=name,
            )
        return _TEMPLATE_RE.sub(replace, value)
    if isinstance(value, list):
        return [_render_templates(item, plugin_root, plugin_data) for item in value]
    if isinstance(value, dict):
        return {str(key): _render_templates(item, plugin_root, plugin_data)
                for key, item in value.items()}
    return value


def _assert_rendered_paths(
    declaration: Mapping[str, Any],
    server_name: str,
    plugin_root: Path,
    plugin_data: Path,
) -> None:
    candidates: list[tuple[str, Any]] = [("cwd", declaration.get("cwd"))]
    candidates.extend(
        (f"args[{index}]", item)
        for index, item in enumerate(declaration.get("args") or [])
    )
    command = declaration.get("command")
    if isinstance(command, str) and Path(command).is_absolute():
        candidates.append(("command", command))
    for field, raw in candidates:
        if not isinstance(raw, str) or not raw or not Path(raw).is_absolute():
            continue
        candidate = Path(raw)
        if candidate == plugin_root or plugin_root in candidate.parents:
            _resolve_contained(
                plugin_root, candidate, component="mcp",
                field=f"mcpServers.{server_name}.{field}", must_exist=True,
            )
        elif candidate == plugin_data or plugin_data in candidate.parents:
            _resolve_contained(
                plugin_data, candidate, component="mcp",
                field=f"mcpServers.{server_name}.{field}", must_exist=True,
            )


def install_plugin(destination: str | Path) -> Path:
    """Copy the complete verified standard package to a client location."""
    source = builtin_agent_plugin(required_components=("mcp", "skills"))
    target = Path(destination).expanduser()
    if target.exists() and target.is_symlink():
        _fail(
            "package", "agent_plugin.install_symlink_target",
            "refusing to install package into a symlink target", target,
        )
    target.parent.mkdir(parents=True, exist_ok=True)
    shutil.copytree(source.root, target, dirs_exist_ok=True, symlinks=True)
    return require_verified_agent_plugin(
        target, required_components=("mcp", "skills"),
    ).root


def install_skill(destination: str | Path) -> Path:
    """Copy the verified standard Agent Skill to a skills-only client."""
    package = builtin_agent_plugin(required_components=("skills",))
    source = next((item for item in package.skill_items if item.name == SKILL_NAME), None)
    if source is None:
        _fail(
            "skills", "agent_plugin.skill_missing",
            f"verified package is missing skills/{SKILL_NAME}/SKILL.md", package.root,
        )
    target = Path(destination).expanduser()
    if target.exists() and target.is_symlink():
        _fail(
            "skills", "agent_plugin.install_symlink_target",
            "refusing to install Skill into a symlink target", target,
        )
    target.parent.mkdir(parents=True, exist_ok=True)
    shutil.copytree(source.root, target, dirs_exist_ok=True, symlinks=True)
    target_root = target.resolve(strict=True)
    manifest = _resolve_contained(
        target_root, target_root / "SKILL.md", component="skills",
        field="SKILL.md", must_exist=True,
    )
    if not manifest.is_file():
        _fail("skills", "agent_plugin.skill_missing", "installed SKILL.md is missing", target_root)
    return target_root


def write_connection_config(
    plugin_data: str | Path,
    *,
    endpoint: str,
    bearer_token: str,
) -> Path:
    """Materialize client-managed MCP connection state under ``PLUGIN_DATA``.

    The token is written only at session launch, never placed in ``plugin.json``,
    ``mcp.json``, or ``CapabilityInjectionPlan``.
    """
    data_root = Path(plugin_data).expanduser()
    data_root.mkdir(parents=True, exist_ok=True)
    try:
        data_root = data_root.resolve(strict=True)
    except (OSError, RuntimeError) as exc:
        _fail(
            "package", "agent_plugin.data_root_unavailable",
            f"cannot materialize plugin data root: {exc}", data_root,
        )
    endpoint_text = str(endpoint or "").strip()
    token_text = str(bearer_token or "").strip()
    if not endpoint_text or not token_text:
        _fail(
            "package", "agent_plugin.missing_connection",
            "endpoint and bearer token are required for runtime materialization",
            data_root,
        )
    target = data_root / "connection.json"
    target.write_text(json.dumps({
        "endpoint": endpoint_text,
        "bearer_token": token_text,
    }, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    target.chmod(0o600)
    return _resolve_contained(
        data_root, target, component="package", field="connection.json",
        must_exist=True,
    )


def build_injection_plan(
    descriptor: CapabilityDescriptor,
    *,
    gateway_endpoint: str,
    binding_id: Optional[str] = None,
    grant_id: Optional[str] = None,
    credential_ref: Optional[str] = None,
    audience: str = "",
) -> CapabilityInjectionPlan:
    """Build a plan referencing a verified package without secret material."""
    package = builtin_agent_plugin(required_components=("mcp", "skills"))
    return CapabilityInjectionPlan(
        injection_kind=InjectionKind.AGENT_PLUGIN,
        gateway_endpoint=gateway_endpoint,
        tool_descriptions=list(descriptor.tools),
        credential_ref=credential_ref,
        audience=audience,
        binding_id=binding_id,
        grant_id=grant_id,
        runtime_config={
            **package.trusted_paths(),
            "skill_name": SKILL_NAME,
            "endpoint_env": ENV_ENDPOINT,
            "token_env": ENV_TOKEN,
            "agent_plugins_spec": package.schema_version,
            "plugin_status": package.status,
            "required_components": ["mcp", "skills"],
            "mcp_server_names": sorted(
                str(name) for name in (package.mcp.data.get("mcpServers") or {})
            ),
        },
    )


__all__ = [
    "AgentPluginComponent",
    "AgentPluginDiagnostic",
    "AgentPluginLoadError",
    "AgentPluginPackage",
    "AgentPluginRuntimeMaterialization",
    "AgentPluginSkill",
    "ENV_ENDPOINT",
    "ENV_TOKEN",
    "MCP_SCHEMA",
    "PLUGIN_NAME",
    "PLUGIN_SCHEMA",
    "SKILL_NAME",
    "SUPPORTED_SCHEMA_VERSIONS",
    "build_injection_plan",
    "builtin_agent_plugin",
    "discover_agent_plugin",
    "install_plugin",
    "install_skill",
    "load_agent_plugin",
    "load_manifest",
    "materialize_runtime",
    "package_root",
    "require_verified_agent_plugin",
    "resolve_client_extension_entry",
    "write_connection_config",
]
