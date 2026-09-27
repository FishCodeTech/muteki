"""Agent Plugins 1.0.0 根清单加载与 Muteki 客户端扩展校验。

插件包唯一清单是根目录 ``plugin.json``。可移植字段严格遵循 Agent Plugins
1.0.0；Muteki Extension Host 专用字段只从
``extensions.io.github.fishcodetech.muteki`` 读取。内部契约模型仍使用
``ExtensionManifest``，它是根清单与 Muteki 命名空间数据合并后的运行视图。

校验失败抛出携带准确字段位置的 ``ManifestError``（code 形如
``manifest.invalid_version``），不做静默兜底。

隔离原则（核验文档总览）：第三方扩展默认子进程运行；``host: in_process``
只允许 ``origin: builtin`` 的可信内置代码声明。
"""

from __future__ import annotations

import json
import logging
import re
from pathlib import Path
from typing import Any, Optional

from muteki.platform.contracts.errors import ErrorCategory, ErrorEnvelope
from muteki.platform.contracts.extensions import (
    ExtensionManifest,
)
from muteki.platform.contracts.events import NS_EXT_PREFIX

#: 当前宿主核心版本；requires_core 区间对它求值。
CORE_VERSION = "1.0.0"

#: 当前支持的 manifest_version 集合。
SUPPORTED_MANIFEST_VERSIONS = frozenset({1})

#: Agent Plugins 1.0.0 的根清单与 Muteki 客户端扩展命名空间。
AGENT_PLUGIN_SCHEMA = "https://agent-plugins.org/schemas/1.0.0/plugin.schema.json"
MUTEKI_EXTENSION_NAMESPACE = "io.github.fishcodetech.muteki"
MANIFEST_FILENAME = "plugin.json"

_PLUGIN_FIELDS = frozenset({
    "$schema", "name", "version", "description", "author", "homepage",
    "repository", "license", "keywords", "extensions",
})
_PLUGIN_STRING_FIELDS = frozenset({
    "version", "description", "homepage", "repository", "license",
})
_MUTEKI_FIELDS = frozenset({
    "manifest_version", "extension_version", "requires_core", "origin",
    "retain_previous_versions",
    "entrypoints", "provides", "requires", "permissions", "config_schema",
    "state_schema", "ui",
})

#: 扩展可声明的能力类型（设计文档 10.1 扩展类型表）。
KNOWN_PROVIDE_TYPES = frozenset({
    "domain-module",
    "run-executor",
    "external-agent-adapter",
    "platform-adapter",
    "workspace-provider",
    "graph-service",
    "policy-gate",
    "data-connection",
    "ui-contribution",
    "agent-capability-binding",
    "tool-integration",
    "protocol-binding",
})

#: 文件系统权限令牌。state-* 指向扩展私有状态目录，workspace-* 指向当前 workspace。
KNOWN_FILESYSTEM_TOKENS = frozenset({
    "workspace-read",
    "workspace-write",
    "state-read",
    "state-write",
})

#: 允许的 origin 取值。builtin 是随产品发布的可信代码；其余为第三方来源。
KNOWN_ORIGINS = frozenset({"builtin", "verified", "community", "installed"})

#: 兼容安装器内部的单根归档判断；只包含规范根清单。
MANIFEST_FILENAMES = (MANIFEST_FILENAME,)

_EXTENSION_ID_RE = re.compile(r"^[a-z0-9_]+(\.[a-z0-9_-]+)+$")
_PLUGIN_NAME_RE = re.compile(
    r"^(?!.*(?:--|\.\.))[a-z0-9](?:[a-z0-9.-]*[a-z0-9])?$"
)
_SEMVER_RE = re.compile(r"^(\d+)\.(\d+)\.(\d+)$")
_RANGE_RE = re.compile(r"^(>=|<=|==|!=|>|<)\s*(\d+(?:\.\d+){0,2})$")
_DOMAIN_RE = re.compile(
    r"^(\*\.)?[a-z0-9]([a-z0-9-]*[a-z0-9])?(\.[a-z0-9]([a-z0-9-]*[a-z0-9])?)*$"
)
_SECRET_REF_RE = re.compile(
    r"^(secret://)?[a-z0-9_-]+(?:/[a-z0-9_.-]+){1,2}$",
    re.IGNORECASE,
)

LOG = logging.getLogger(__name__)


class ManifestError(ValueError):
    """manifest 加载 / 校验失败，携带统一错误 envelope 与字段位置。"""

    def __init__(self, code: str, message: str, *, field: str = "") -> None:
        super().__init__(f"{code}: {message}")
        self.error = ErrorEnvelope(
            code=code,
            message=message,
            category=ErrorCategory.VALIDATION,
            detail={"field": field} if field else {},
        )
        self.field = field


def parse_semver(text: str, *, field: str = "version") -> tuple[int, int, int]:
    """解析严格 semver X.Y.Z；失败抛 ManifestError。"""
    match = _SEMVER_RE.match(str(text).strip())
    if not match:
        raise ManifestError(
            "manifest.invalid_version",
            f"version must be strict semver X.Y.Z: {text!r}",
            field=field,
        )
    return int(match.group(1)), int(match.group(2)), int(match.group(3))


def version_satisfies(version: str, requirement: str) -> bool:
    """判断 ``version`` 是否满足 ``>=1.0,<2.0`` 形式的区间表达式。

    区间由逗号分隔的比较子句组成，运算符为 >=、<=、>、<、==、!=；
    右侧版本允许 X / X.Y / X.Y.Z（缺段位补 0）。
    """
    text = str(requirement or "").strip()
    if not text:
        return False
    target = _normalize_version(version)
    for clause in text.split(","):
        clause = clause.strip()
        match = _RANGE_RE.match(clause)
        if not match:
            return False
        op, raw = match.group(1), match.group(2)
        bound = _normalize_version(raw)
        if not _compare(op, target, bound):
            return False
    return True


def _normalize_version(text: str) -> tuple[int, int, int]:
    parts = [int(p) for p in str(text).strip().split(".")]
    while len(parts) < 3:
        parts.append(0)
    return parts[0], parts[1], parts[2]


def _compare(op: str, left: tuple[int, int, int], right: tuple[int, int, int]) -> bool:
    if op == ">=":
        return left >= right
    if op == "<=":
        return left <= right
    if op == ">":
        return left > right
    if op == "<":
        return left < right
    if op == "==":
        return left == right
    if op == "!=":
        return left != right
    return False


def find_manifest_file(package_dir: str | Path) -> Path:
    """定位并约束根 ``plugin.json``；其他旧清单不参与加载。"""
    root = Path(package_dir).resolve()
    candidate = root / MANIFEST_FILENAME
    if candidate.is_file():
        resolved = candidate.resolve()
        if resolved != root and root not in resolved.parents:
            raise ManifestError(
                "manifest.unsafe_path",
                f"plugin.json resolves outside plugin root: {candidate}",
                field="plugin.json",
            )
        return candidate
    legacy = next(
        (root / name for name in ("extension.yaml", "extension.yml", "extension.json")
         if (root / name).is_file()),
        None,
    )
    if legacy is not None:
        raise ManifestError(
            "manifest.legacy_format_unsupported",
            f"legacy manifest {legacy.name} is not an Agent Plugins package; "
            f"migrate it to root {MANIFEST_FILENAME}",
            field=legacy.name,
        )
    raise ManifestError(
        "manifest.not_found",
        f"root {MANIFEST_FILENAME} not found in plugin package {root}",
        field=MANIFEST_FILENAME,
    )


def load_manifest(package_dir: str | Path) -> ExtensionManifest:
    """读取 Agent Plugins 根清单并解析 Muteki 客户端扩展数据。"""
    path = find_manifest_file(package_dir)
    try:
        raw = path.read_text(encoding="utf-8")
    except OSError as exc:
        raise ManifestError("manifest.unreadable", f"cannot read {path}: {exc}") from exc
    try:
        data = json.loads(raw)
    except json.JSONDecodeError as exc:
        raise ManifestError(
            "manifest.parse_error", f"cannot parse {path}: {exc}"
        ) from exc
    if not isinstance(data, dict):
        raise ManifestError(
            "manifest.invalid_shape", f"manifest root must be a mapping: {path}"
        )
    portable = _validate_plugin_manifest(data, path)
    extensions = portable.get("extensions")
    if not isinstance(extensions, dict):
        raise ManifestError(
            "manifest.missing_muteki_extension",
            f"plugin does not declare extensions.{MUTEKI_EXTENSION_NAMESPACE}",
            field=f"extensions.{MUTEKI_EXTENSION_NAMESPACE}",
        )
    client_data = extensions.get(MUTEKI_EXTENSION_NAMESPACE)
    if not isinstance(client_data, dict):
        raise ManifestError(
            "manifest.missing_muteki_extension",
            f"plugin does not declare extensions.{MUTEKI_EXTENSION_NAMESPACE}",
            field=f"extensions.{MUTEKI_EXTENSION_NAMESPACE}",
        )
    unknown_client = sorted(set(client_data) - _MUTEKI_FIELDS)
    if unknown_client:
        raise ManifestError(
            "manifest.invalid_muteki_fields",
            "unsupported Muteki extension fields: " + ", ".join(unknown_client),
            field=f"extensions.{MUTEKI_EXTENSION_NAMESPACE}",
        )
    merged = {
        **{key: value for key, value in client_data.items()
           if key != "extension_version"},
        "plugin_schema": portable["$schema"],
        "client_namespace": MUTEKI_EXTENSION_NAMESPACE,
        "id": portable["name"],
        "plugin_version": portable.get("version", ""),
        "version": client_data.get("extension_version", ""),
        "description": portable.get("description", ""),
        "author": portable.get("author", {}),
        "homepage": portable.get("homepage", ""),
        "repository": portable.get("repository", ""),
        "license": portable.get("license", ""),
        "keywords": portable.get("keywords", []),
    }
    try:
        return ExtensionManifest.model_validate(merged)
    except ValueError as exc:
        raise ManifestError(
            "manifest.invalid_fields",
            f"Muteki extension fields invalid in {path}: {exc}",
        ) from exc


def _validate_plugin_manifest(data: dict[str, Any], path: Path) -> dict[str, Any]:
    """实现 Agent Plugins 1.0.0 根清单的离线结构校验。

    规范要求加载时不得联网取 schema。未知顶层字段是唯一可恢复的顶层
    schema 违规：记录诊断并忽略；其余类型或必填字段错误直接拒绝。
    """
    schema = data.get("$schema")
    if schema != AGENT_PLUGIN_SCHEMA:
        raise ManifestError(
            "manifest.unsupported_plugin_schema",
            f"$schema must be {AGENT_PLUGIN_SCHEMA!r}: {schema!r}",
            field="$schema",
        )
    name = data.get("name")
    if not isinstance(name, str) or not _PLUGIN_NAME_RE.fullmatch(name) or not (1 <= len(name) <= 64):
        raise ManifestError(
            "manifest.invalid_plugin_name",
            "name must be 1-64 lowercase alphanumeric, hyphen, or period "
            "characters, with alphanumeric ends and no '--' or '..'",
            field="name",
        )
    for field in _PLUGIN_STRING_FIELDS:
        if field in data and not isinstance(data[field], str):
            raise ManifestError(
                "manifest.invalid_plugin_field",
                f"{field} must be a string",
                field=field,
            )
    if "keywords" in data and (
        not isinstance(data["keywords"], list)
        or any(not isinstance(item, str) for item in data["keywords"])
    ):
        raise ManifestError(
            "manifest.invalid_plugin_field",
            "keywords must be an array of strings",
            field="keywords",
        )
    author = data.get("author")
    if author is not None:
        if not isinstance(author, dict):
            raise ManifestError(
                "manifest.invalid_plugin_field", "author must be an object",
                field="author",
            )
        unknown_author = sorted(set(author) - {"name", "email", "url"})
        if unknown_author or any(not isinstance(value, str) for value in author.values()):
            raise ManifestError(
                "manifest.invalid_plugin_field",
                "author may contain only string name, email, and url fields",
                field="author",
            )
    extensions = data.get("extensions")
    if extensions is not None and not isinstance(extensions, dict):
        LOG.warning("ignoring non-object plugin extensions field in %s", path)
        data = {**data, "extensions": {}}
    elif isinstance(extensions, dict):
        invalid = [key for key, value in extensions.items()
                   if not isinstance(key, str) or not isinstance(value, dict)]
        if invalid:
            raise ManifestError(
                "manifest.invalid_plugin_field",
                "each extensions member must map a namespace to an object",
                field="extensions",
            )
    unknown = sorted(set(data) - _PLUGIN_FIELDS)
    if unknown:
        LOG.warning(
            "ignoring unknown Agent Plugins manifest fields in %s: %s",
            path, ", ".join(unknown),
        )
    return {key: value for key, value in data.items() if key in _PLUGIN_FIELDS}


def validate_manifest(
    manifest: ExtensionManifest,
    *,
    package_dir: str | Path | None = None,
    core_version: str = CORE_VERSION,
    capabilities: Optional[dict[str, int]] = None,
) -> ExtensionManifest:
    """逐字段语义校验；任何一项失败都抛带字段位置的 ManifestError。

    - ``package_dir`` 给出时校验 config_schema / state_schema / ui 相对路径
      真实存在且为文件；
    - ``capabilities``（capability 名 -> 主版本号）给出时校验 requires 依赖
      可用且主版本匹配；
    - requires_core 版本区间必须覆盖 ``core_version``。
    """
    if manifest.plugin_schema != AGENT_PLUGIN_SCHEMA:
        raise ManifestError(
            "manifest.unsupported_plugin_schema",
            f"plugin_schema must be {AGENT_PLUGIN_SCHEMA!r}",
            field="$schema",
        )
    if manifest.client_namespace != MUTEKI_EXTENSION_NAMESPACE:
        raise ManifestError(
            "manifest.invalid_client_namespace",
            f"Muteki metadata must use extensions.{MUTEKI_EXTENSION_NAMESPACE}",
            field="extensions",
        )
    if manifest.manifest_version not in SUPPORTED_MANIFEST_VERSIONS:
        raise ManifestError(
            "manifest.unsupported_manifest_version",
            f"manifest_version {manifest.manifest_version} is not supported "
            f"(supported: {sorted(SUPPORTED_MANIFEST_VERSIONS)})",
            field="manifest_version",
        )
    _validate_id(manifest.id)
    parse_semver(
        manifest.version,
        field=f"extensions.{MUTEKI_EXTENSION_NAMESPACE}.extension_version",
    )
    _validate_requires_core(manifest.requires_core, core_version)
    _validate_origin(manifest)
    _validate_entrypoints(manifest)
    _validate_provides(manifest)
    _validate_requires(manifest, capabilities)
    _validate_permissions(manifest)
    if package_dir is not None:
        root = Path(package_dir)
        _validate_client_extension_directory(manifest, root)
        _validate_relative_files(manifest, root)
    else:
        for attr in ("config_schema", "state_schema", "ui"):
            value = getattr(manifest, attr)
            if value is not None:
                _check_relative_path(value, field=attr)
    return manifest


# ---------------------------------------------------------------------------
# 逐字段校验
# ---------------------------------------------------------------------------


def _validate_id(extension_id: str) -> None:
    text = extension_id.strip()
    if not text or not _EXTENSION_ID_RE.match(text):
        raise ManifestError(
            "manifest.invalid_id",
            f"extension id must be dotted reverse-dns style "
            f"(e.g. org.example.extension): {extension_id!r}",
            field="id",
        )


def _validate_requires_core(requires_core: str, core_version: str) -> None:
    text = requires_core.strip()
    if not text:
        raise ManifestError(
            "manifest.missing_requires_core",
            "requires_core cannot be empty (e.g. \">=1.0,<2.0\")",
            field="requires_core",
        )
    for clause in text.split(","):
        if not _RANGE_RE.match(clause.strip()):
            raise ManifestError(
                "manifest.invalid_requires_core",
                f"invalid requires_core clause: {clause.strip()!r} "
                f"(operators: >=, <=, >, <, ==, !=; versions X[.Y[.Z]])",
                field="requires_core",
            )
    if not version_satisfies(core_version, text):
        raise ManifestError(
            "manifest.core_version_incompatible",
            f"requires_core {text!r} does not cover core version {core_version}",
            field="requires_core",
        )


def _validate_origin(manifest: ExtensionManifest) -> None:
    if manifest.origin not in KNOWN_ORIGINS:
        raise ManifestError(
            "manifest.invalid_origin",
            f"origin must be one of {sorted(KNOWN_ORIGINS)}: {manifest.origin!r}",
            field="origin",
        )


def _validate_entrypoints(manifest: ExtensionManifest) -> None:
    entry = manifest.entrypoints
    if entry.host not in ("subprocess", "in_process"):
        raise ManifestError(
            "manifest.invalid_host",
            f"entrypoints.host must be subprocess or in_process: {entry.host!r}",
            field="entrypoints.host",
        )
    if entry.host == "in_process" and manifest.origin != "builtin":
        raise ManifestError(
            "manifest.in_process_not_allowed",
            "in_process entrypoint is restricted to origin=builtin "
            "(third-party extensions run in a subprocess)",
            field="entrypoints.host",
        )
    if entry.host == "subprocess":
        if not entry.command:
            raise ManifestError(
                "manifest.missing_command",
                "subprocess entrypoint requires a non-empty command",
                field="entrypoints.command",
            )
        for item in entry.command:
            if not str(item).strip():
                raise ManifestError(
                    "manifest.invalid_command",
                    "entrypoints.command entries must be non-empty strings",
                    field="entrypoints.command",
                )


def _validate_provides(manifest: ExtensionManifest) -> None:
    seen: set[tuple[str, str]] = set()
    for index, provide in enumerate(manifest.provides):
        field = f"provides[{index}]"
        if provide.type not in KNOWN_PROVIDE_TYPES:
            raise ManifestError(
                "manifest.invalid_provide_type",
                f"unknown provide type: {provide.type!r} "
                f"(known: {sorted(KNOWN_PROVIDE_TYPES)})",
                field=f"{field}.type",
            )
        if not provide.id.strip():
            raise ManifestError(
                "manifest.invalid_provide_id",
                "provide id cannot be empty",
                field=f"{field}.id",
            )
        if int(provide.api_version) < 1:
            raise ManifestError(
                "manifest.invalid_api_version",
                f"api_version must be >= 1: {provide.api_version}",
                field=f"{field}.api_version",
            )
        key = (provide.type, provide.id)
        if key in seen:
            raise ManifestError(
                "manifest.duplicate_provide",
                f"duplicate provide: {provide.type}/{provide.id}",
                field=field,
            )
        seen.add(key)


def _validate_requires(
    manifest: ExtensionManifest, capabilities: Optional[dict[str, int]]
) -> None:
    for index, require in enumerate(manifest.requires):
        field = f"requires[{index}]"
        capability = require.capability.strip()
        if not capability:
            raise ManifestError(
                "manifest.invalid_require_capability",
                "requires capability cannot be empty",
                field=f"{field}.capability",
            )
        if int(require.version) < 1:
            raise ManifestError(
                "manifest.invalid_require_version",
                f"requires version must be >= 1: {require.version}",
                field=f"{field}.version",
            )
        if capabilities is not None:
            available = capabilities.get(capability)
            if available is None:
                raise ManifestError(
                    "manifest.missing_capability",
                    f"required capability is not available: {capability}",
                    field=f"{field}.capability",
                )
            if int(available) != int(require.version):
                raise ManifestError(
                    "manifest.capability_version_mismatch",
                    f"capability {capability} requires major {require.version} "
                    f"but host provides major {available}",
                    field=f"{field}.version",
                )


def _validate_permissions(manifest: ExtensionManifest) -> None:
    permissions = manifest.permissions
    for token in permissions.filesystem:
        if token not in KNOWN_FILESYSTEM_TOKENS:
            raise ManifestError(
                "manifest.invalid_filesystem_permission",
                f"unknown filesystem permission: {token!r} "
                f"(known: {sorted(KNOWN_FILESYSTEM_TOKENS)})",
                field="permissions.filesystem",
            )
    for host in permissions.network:
        text = str(host).strip().lower()
        if not text or text == "*" or not _DOMAIN_RE.match(text):
            raise ManifestError(
                "manifest.invalid_network_permission",
                f"network entries must be explicit hostnames or *.suffix "
                f"wildcards (bare \"*\" is not allowed): {host!r}",
                field="permissions.network",
            )
    for ref in permissions.secrets:
        if not _SECRET_REF_RE.match(str(ref).strip()):
            raise ManifestError(
                "manifest.invalid_secret_ref",
                f"secret refs must look like secret://scope/id or "
                f"secret://platform/connection/key: {ref!r}",
                field="permissions.secrets",
            )
    expected_prefix = f"{NS_EXT_PREFIX}{manifest.id}."
    for pattern in permissions.events_write:
        text = str(pattern).strip()
        if not text.startswith(expected_prefix):
            raise ManifestError(
                "manifest.invalid_events_write",
                f"events_write entries must stay inside the extension namespace "
                f"{expected_prefix}*: {pattern!r}",
                field="permissions.events_write",
            )


def _check_relative_path(value: str, *, field: str) -> None:
    text = str(value).strip()
    if not text:
        raise ManifestError(
            "manifest.invalid_path", "path cannot be empty", field=field
        )
    if not text.startswith("./") or ".." in Path(text).parts:
        raise ManifestError(
            "manifest.unsafe_path",
            f"plugin-relative path must begin with './' and stay inside the "
            f"plugin root: {value!r}",
            field=field,
        )


def _validate_client_extension_directory(
    manifest: ExtensionManifest, package_dir: Path
) -> None:
    """校验 Muteki 专用文件只位于规范命名空间目录。"""
    root = package_dir.resolve()
    namespace_root = (root / MUTEKI_EXTENSION_NAMESPACE).resolve()
    declared_files = [
        (field, getattr(manifest, field))
        for field in ("config_schema", "state_schema", "ui")
        if getattr(manifest, field) is not None
    ]
    package_command_paths = [
        (index, str(item).strip())
        for index, item in enumerate(manifest.entrypoints.command)
        if str(item).strip().startswith("./")
    ]
    if declared_files or package_command_paths:
        if not namespace_root.is_dir() or root not in namespace_root.parents:
            raise ManifestError(
                "manifest.missing_client_extension_directory",
                f"Muteki client files must be under top-level "
                f"{MUTEKI_EXTENSION_NAMESPACE}/",
                field=MUTEKI_EXTENSION_NAMESPACE,
            )
    for field, value in declared_files:
        _check_relative_path(str(value), field=field)
        target = (root / str(value)).resolve()
        if namespace_root not in target.parents:
            raise ManifestError(
                "manifest.invalid_client_extension_path",
                f"{field} must resolve inside {MUTEKI_EXTENSION_NAMESPACE}/: {value!r}",
                field=field,
            )
    for index, value in package_command_paths:
        _check_relative_path(value, field=f"entrypoints.command[{index}]")
        target = (root / value).resolve()
        if namespace_root not in target.parents or not target.is_file():
            raise ManifestError(
                "manifest.invalid_client_extension_path",
                f"entrypoint package path must be an existing file inside "
                f"{MUTEKI_EXTENSION_NAMESPACE}/: {value!r}",
                field=f"entrypoints.command[{index}]",
            )


def _validate_relative_files(manifest: ExtensionManifest, package_dir: Path) -> None:
    for attr in ("config_schema", "state_schema", "ui"):
        value = getattr(manifest, attr)
        if value is None:
            continue
        _check_relative_path(value, field=attr)
        target = (package_dir / value).resolve()
        root = package_dir.resolve()
        if root not in target.parents and target != root:
            raise ManifestError(
                "manifest.unsafe_path",
                f"path escapes the package directory: {value!r}",
                field=attr,
            )
        if not target.is_file():
            raise ManifestError(
                "manifest.missing_file",
                f"declared file does not exist in package: {value}",
                field=attr,
            )


# ---------------------------------------------------------------------------
# 声明式 schema 子集（config / state / 扩展事件提案共用）
# ---------------------------------------------------------------------------


class SchemaValidationError(ValueError):
    """配置 / 状态 / 事件提案不满足扩展声明的 JSON schema 子集。"""


def load_schema(package_dir: str | Path, relative: Optional[str]) -> Optional[dict]:
    """读取扩展声明的 schema 文件；未声明返回 None。"""
    if relative is None:
        return None
    path = Path(package_dir) / relative
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as exc:
        raise ManifestError(
            "manifest.invalid_schema", f"cannot load schema {path}: {exc}"
        ) from exc
    if not isinstance(data, dict):
        raise ManifestError(
            "manifest.invalid_schema", f"schema root must be an object: {path}"
        )
    return data


def validate_against_schema(
    value: Any, schema: dict, *, field: str = "config"
) -> None:
    """按 JSON Schema 子集校验：type/properties/required/enum/additionalProperties。

    不引入 jsonschema 依赖；扩展 manifest 的 config_schema / state_schema 与
    capabilities/list 声明的 event_schemas 统一走这个子集，覆盖不到的
    关键字按未知处理（忽略），保持校验器简单可审计。
    """
    expected_type = schema.get("type")
    if expected_type is not None and not _type_matches(value, expected_type):
        raise SchemaValidationError(
            f"{field}: expected type {expected_type}, got {type(value).__name__}"
        )
    if "enum" in schema and value not in schema["enum"]:
        raise SchemaValidationError(
            f"{field}: value {value!r} not in enum {schema['enum']!r}"
        )
    if isinstance(value, dict):
        properties = schema.get("properties") or {}
        for key in schema.get("required") or []:
            if key not in value:
                raise SchemaValidationError(f"{field}: missing required key {key!r}")
        if schema.get("additionalProperties") is False:
            unknown = sorted(set(value) - set(properties))
            if unknown:
                raise SchemaValidationError(
                    f"{field}: unknown keys not allowed: {', '.join(unknown)}"
                )
        for key, subschema in properties.items():
            if key in value and isinstance(subschema, dict):
                validate_against_schema(
                    value[key], subschema, field=f"{field}.{key}"
                )
    items = schema.get("items")
    if isinstance(value, list) and isinstance(items, dict):
        for index, item in enumerate(value):
            validate_against_schema(item, items, field=f"{field}[{index}]")


def _type_matches(value: Any, expected: str) -> bool:
    return {
        "object": isinstance(value, dict),
        "array": isinstance(value, list),
        "string": isinstance(value, str),
        "integer": isinstance(value, int) and not isinstance(value, bool),
        "number": isinstance(value, (int, float)) and not isinstance(value, bool),
        "boolean": isinstance(value, bool),
        "null": value is None,
    }.get(str(expected), True)


__all__ = [
    "AGENT_PLUGIN_SCHEMA",
    "CORE_VERSION",
    "KNOWN_FILESYSTEM_TOKENS",
    "KNOWN_ORIGINS",
    "KNOWN_PROVIDE_TYPES",
    "MANIFEST_FILENAME",
    "MANIFEST_FILENAMES",
    "MUTEKI_EXTENSION_NAMESPACE",
    "SUPPORTED_MANIFEST_VERSIONS",
    "ManifestError",
    "SchemaValidationError",
    "find_manifest_file",
    "load_manifest",
    "load_schema",
    "parse_semver",
    "validate_against_schema",
    "validate_manifest",
    "version_satisfies",
]
