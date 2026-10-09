"""Managed Agent packages and per-engine capability providers.

Packages are immutable snapshots. Worker-selected components are projected into
run-scoped workspaces without modifying the operator's Agent directories.
"""
from __future__ import annotations

import asyncio
from contextlib import AsyncExitStack, closing
import ctypes
import errno
from datetime import datetime, timezone
from hashlib import sha256
import json
import logging
import os
from pathlib import Path
import re
import shlex
import shutil
import sqlite3
import stat
import tempfile
import threading
import time
import sys
from typing import Any

import yaml

from muteki.extensions.installer import ExtensionInstaller, InstallError, Source, sha256_file, sha256_tree
from muteki.extensions.isolation import IsolationUnavailable
from muteki.conversation.chat_providers import PROVIDERS, provider_for
from muteki.conversation.chat_plugin_components import ENGINES, compatibility, inspect_components, install_native_components
from muteki.external_agents.descriptors import engines_where, find_descriptor, get_descriptor

MODES = ("chat", "ctf", "pentest")
# Skills marked ``requires_native`` run only on engines with their own skill
# semantics; Worker-home native components need a declared native home.
NATIVE_SKILL_ENGINES = frozenset(engines_where(lambda d: d.components.native_skill_semantics))
WORKER_COMPONENT_ENGINES = frozenset(engines_where(lambda d: bool(d.environment.worker_component_home_env)))
EXTENSION_ENGINES = frozenset(engines_where(lambda d: d.components.extension_abi))
VISUALIZE_PACKAGE = Path(__file__).resolve().parents[1] / "agent_plugins" / "muteki-visualize"
_log = logging.getLogger(__name__)


class ChatPluginError(ValueError):
    """User-correctable or attributed plugin failure with a stable API code."""

    def __init__(self, message: str, *, code: str = "chat_plugin.invalid") -> None:
        super().__init__(message)
        self.code = code


def _contained(root: Path, value: str) -> Path:
    path = (root / value).resolve()
    if not path.is_relative_to(root.resolve()):
        raise ChatPluginError("插件资源路径必须位于插件包内")
    return path


def _snapshot_copy(source: str | Path, target: str | Path) -> str:
    # Native skill packs can contain large assets. APFS clones keep independent
    # writable files without duplicating their bytes or sharing host symlinks.
    if sys.platform == "darwin" and not Path(target).exists():
        clone = ctypes.CDLL("/usr/lib/libSystem.B.dylib", use_errno=True).clonefile
        clone.argtypes = (ctypes.c_char_p, ctypes.c_char_p, ctypes.c_int)
        clone.restype = ctypes.c_int
        if clone(os.fsencode(source), os.fsencode(target), 0) == 0:
            return str(target)
        error = ctypes.get_errno()
        if error not in {errno.ENOTSUP, errno.EXDEV, errno.ENOSYS}:
            raise OSError(error, os.strerror(error), str(source))
    return shutil.copy2(source, target)


def _sync_omp_databases(source: Path, target: Path, root: Path) -> None:
    """Import native credentials/catalogs, never host history, leases or settings.

    Read through SQLite so committed WAL contents belong to the snapshot.
    Refresh only when the host rows change; native private refreshes survive
    ordinary launches. Transactions update only the selected tables.
    """
    from .native_environment import NativeEnvironmentError
    manifest = root / ".omp-state-imports.json"
    previous = json.loads(manifest.read_text()) if manifest.exists() else {}
    current = dict(previous)
    for filename, names in {
        "agent.db": ("auth_credentials", "auth_schema_version"),
        "models.db": ("model_cache",),
    }.items():
        origin = source / filename
        if not origin.is_file():
            continue
        snapshot = []
        with closing(sqlite3.connect(origin.as_uri() + "?mode=ro", uri=True)) as conn:
            conn.execute("BEGIN")
            for name in names:
                schema = conn.execute("SELECT sql FROM sqlite_master WHERE type='table' AND name=?", (name,)).fetchone()
                if schema is None:
                    continue
                columns = [row[1] for row in conn.execute(f'PRAGMA table_info("{name}")')]
                rows = conn.execute(f'SELECT * FROM "{name}" ORDER BY rowid').fetchall()
                snapshot.append((name, schema[0], columns, rows))
        digest = sha256(json.dumps(snapshot, sort_keys=True).encode()).hexdigest()
        destination = target / filename
        if destination.is_symlink():
            raise NativeEnvironmentError(f"OMP native database is a symlink: {destination}")
        if previous.get(filename) == digest and destination.is_file():
            continue
        with closing(sqlite3.connect(destination, timeout=30)) as conn, conn:
            for name, schema, columns, rows in snapshot:
                conn.execute(schema.replace("CREATE TABLE ", "CREATE TABLE IF NOT EXISTS ", 1))
                existing = [row[1] for row in conn.execute(f'PRAGMA table_info("{name}")')]
                if existing != columns:
                    raise NativeEnvironmentError(f"OMP native database schema differs: {filename}:{name}")
                conn.execute(f'DELETE FROM "{name}"')
                conn.executemany(f'INSERT INTO "{name}" VALUES ({",".join("?" for _ in columns)})', rows)
        destination.chmod(0o600)
        current[filename] = digest
    fd, temporary = tempfile.mkstemp(prefix=".omp-state-", dir=root)
    try:
        with os.fdopen(fd, "w") as stream:
            json.dump(current, stream)
        os.replace(temporary, manifest)
    finally:
        Path(temporary).unlink(missing_ok=True)


def _copy_package(source: Path, target: Path) -> None:
    # Do not follow links into credentials, other packages or the operator home.
    size = 0
    paths = list(source.rglob("*"))
    if len(paths) > 12000:
        raise ChatPluginError("插件文件数量超过 12000")
    for path in paths:
        if path.is_symlink():
            raise ChatPluginError("插件包不能包含符号链接，请导入完整文件副本")
        if path.is_file():
            size += path.stat().st_size
    if size > 64 * 1024 * 1024:
        raise ChatPluginError("插件包超过 64 MiB")
    shutil.copytree(source, target, copy_function=_snapshot_copy, ignore=shutil.ignore_patterns(".git", "__pycache__"))


def _checked_tree_digest(root: Path) -> str:
    """Hash a snapshot only after rejecting links that could leave its tree."""
    if not root.is_dir() or root.is_symlink():
        raise ChatPluginError("扩展快照目录缺失或已被替换", code="chat_plugin.snapshot_invalid")
    if any(path.is_symlink() for path in root.rglob("*")):
        raise ChatPluginError("扩展快照含有符号链接", code="chat_plugin.snapshot_invalid")
    return sha256_tree(root)


class ChatPluginService:
    def __init__(self, root: Path) -> None:
        self.root = root.resolve()
        self.root.mkdir(parents=True, exist_ok=True)
        self.root.chmod(0o700)
        self.db = self.root / "registry.sqlite3"
        self._lock = threading.RLock()
        self._workers: dict[str, McpWorker] = {}
        self._tools: dict[str, list[dict[str, Any]]] = {}
        self._mcp_connection_errors: dict[str, list[dict[str, str]]] = {}
        self._mcp_server_tools: dict[tuple[str, str, str], list[dict[str, Any]]] = {}
        self._mcp_health: dict[tuple[str, str, str], dict[str, Any]] = {}
        self._verified_tools: dict[str, set[str]] = {}
        self._diagnostics: dict[str, str] = {}
        self._prepare_locks: dict[str, asyncio.Lock] = {}
        with self.connect() as conn:
            conn.execute("CREATE TABLE IF NOT EXISTS packages (id TEXT PRIMARY KEY, value TEXT NOT NULL)")
            conn.execute("CREATE TABLE IF NOT EXISTS policy (id TEXT PRIMARY KEY, value TEXT NOT NULL)")
        self.db.chmod(0o600)
        for record in self.records():
            if record.get("mode_scope_version") != 1:
                record.update(modes=["chat"], mode_scope_version=1)
                self.save(record)
            if record.get("scope_version") != 2:
                record.update(enabled=bool(record.get("engines")), engines=list(ENGINES), scope_version=2)
                self.save(record)
            if record.get("component_schema_version") != 5:
                inspected = self._inspect(Path(record["root"]))
                record.update({key: inspected[key] for key in ("components", "requirements", "declared_engines", "skills", "environment", "allowed_modes", "component_schema_version")})
                self.save(record)

    def connect(self) -> sqlite3.Connection:
        return sqlite3.connect(self.db, timeout=10)

    def records(self) -> list[dict[str, Any]]:
        with self.connect() as conn:
            return [json.loads(row[0]) for row in conn.execute("SELECT value FROM packages ORDER BY id")]

    def get(self, package_id: str) -> dict[str, Any]:
        return next((r for r in self.records() if r["id"] == package_id), {})

    def save(self, record: dict[str, Any]) -> None:
        with self._lock, self.connect() as conn:
            conn.execute("INSERT OR REPLACE INTO packages VALUES (?, ?)",
                         (record["id"], json.dumps(record, ensure_ascii=False)))

    def control_enabled(self, engine: str) -> bool:
        with self.connect() as conn:
            row = conn.execute("SELECT value FROM policy WHERE id='control'").fetchone()
        return row is None or json.loads(row[0]) is True

    def set_control(self, enabled: bool) -> None:
        with self.connect() as conn:
            conn.execute("INSERT OR REPLACE INTO policy VALUES (?, ?)", ("control", json.dumps(enabled)))

    def session_revision(self, session_id: str) -> str | None:
        with self.connect() as conn:
            row = conn.execute("SELECT value FROM policy WHERE id=?", ("session:" + session_id,)).fetchone()
        return row[0] if row else None

    def remember_session(self, session_id: str, revision: str) -> None:
        with self.connect() as conn:
            conn.execute("INSERT OR REPLACE INTO policy VALUES (?, ?)", ("session:" + session_id, revision))

    def enabled(self, engine: str, mode: str = "chat") -> list[dict[str, Any]]:
        if mode not in MODES:
            raise ChatPluginError("未知使用场景")
        if mode != "chat" and (engine not in PROVIDERS or not PROVIDERS[engine].managed_environment):
            return []  # Chat-skill support does not enable new Worker transports.
        return [r for r in self.records() if r.get("enabled", True)
                and mode in r.get("modes", ["chat"])
                and mode in r.get("allowed_modes", MODES)
                and (compatibility(r, check_tools=False)[engine]["status"] != "blocked"
                     if mode == "chat"
                     else engine in r.get("declared_engines", ENGINES)
                     and bool(any(not skill.get("requires_native") or engine in NATIVE_SKILL_ENGINES
                                  for skill in r.get("skills", [])) or r.get("mcp") or any(
                         component.get("kind") in {"extensions", "agents"}
                         and engine in WORKER_COMPONENT_ENGINES
                         and engine in component.get("engines", [])
                         for component in r.get("components", []))))]

    def revision(self, engine: str, mode: str = "chat") -> str:
        values = [(r["id"], r["digest"], r.get("hooks_approved_digest", "")) for r in self.enabled(engine, mode)]
        native = (provider_for(engine).revision()
                  if mode == "chat" and engine in PROVIDERS
                  and os.environ.get("MUTEKI_HOST_DISCOVERY", "1") != "0" else "")
        return sha256(json.dumps([9, mode, values, self.control_enabled(engine) if mode == "chat" else False, native]).encode()).hexdigest()[:16]

    @staticmethod
    def validate_modes(modes: Any) -> list[str]:
        if not isinstance(modes, list) or not modes or any(mode not in MODES for mode in modes):
            raise ChatPluginError("请至少选择一个有效使用场景：聊天、CTF 或渗透测试")
        return [mode for mode in MODES if mode in modes]

    @staticmethod
    def _strip_worker_mcp_config(root: Path, record: dict[str, Any]) -> None:
        """Keep host-owned MCP declarations out of a Worker-visible package."""
        if not record.get("mcp"):
            return
        manifests = (
            "plugin.json", ".codex-plugin/plugin.json",
            ".claude-plugin/plugin.json", ".cursor-plugin/plugin.json", "package.json",
        )
        component_paths = {str(item.get("path") or "") for item in (
            list(record.get("skills", [])) + list(record.get("components", [])))
            if isinstance(item, dict)}
        remove: set[Path] = set()
        manifest_paths = {root / name for name in manifests}
        for name in manifests:
            path = root / name
            if not path.exists():
                continue
            if path.is_symlink() or not path.is_file():
                raise ChatPluginError("Worker 扩展清单无效", code="chat_plugin.snapshot_invalid")
            try:
                manifest = json.loads(path.read_text(encoding="utf-8"))
            except (OSError, ValueError) as exc:
                raise ChatPluginError("Worker 扩展清单无法解析", code="chat_plugin.snapshot_invalid") from exc
            if not isinstance(manifest, dict):
                raise ChatPluginError("Worker 扩展清单无效", code="chat_plugin.snapshot_invalid")
            declaration = manifest.pop("mcpServers", None)
            if declaration is None:
                continue
            for value in declaration if isinstance(declaration, list) else [declaration]:
                if isinstance(value, str):
                    referenced = _contained(root, value)
                    if not referenced.is_file() or referenced.is_symlink():
                        raise ChatPluginError("Worker MCP 声明文件无效", code="chat_plugin.snapshot_invalid")
                    remove.add(referenced)
            path.write_text(json.dumps(manifest, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
        # MCP clients also discover conventional declaration files without an
        # explicit manifest reference. Remove them at every package depth while
        # retaining all other relative assets and native import dependencies.
        remove.update(path for path in root.rglob("*")
                      if path.name in {"mcp.json", ".mcp.json"})
        for path in remove:
            if (path in manifest_paths or path.is_symlink() or not path.is_file()
                    or path.relative_to(root).as_posix() in component_paths):
                raise ChatPluginError(
                    "MCP 配置文件与 Worker 组件或清单重叠，请将配置拆到独立文件",
                    code="chat_plugin.snapshot_invalid",
                )
            path.unlink()
        # A package may duplicate an MCP header or environment value in a
        # README or executable asset. Such bytes cannot be projected safely;
        # fail instead of treating a stripped manifest as proof of secrecy.
        private_values = {value.encode("utf-8") for config in record["mcp"].values()
                          if isinstance(config, dict)
                          for field in ("env", "headers")
                          for value in (config.get(field) or {}).values()
                          if isinstance(value, str) and value}
        if private_values:
            for path in root.rglob("*"):
                if not path.is_file():
                    continue
                content = path.read_bytes()
                if any(value in content for value in private_values):
                    raise ChatPluginError(
                        "MCP 配置值仍出现在 Worker 资源中；请拆分包或移除重复凭据",
                        code="chat_plugin.snapshot_contains_mcp_secret",
                    )

    @staticmethod
    def _require_run_snapshots_safe(shared: Path) -> None:
        """Reject older mixed snapshots still mounted after a package is disabled."""
        if not shared.exists():
            return
        if shared.is_symlink() or not shared.is_dir():
            raise ChatPluginError("Run 扩展快照目录无效", code="chat_plugin.snapshot_invalid")
        manifests = (
            "plugin.json", ".codex-plugin/plugin.json",
            ".claude-plugin/plugin.json", ".cursor-plugin/plugin.json", "package.json",
        )
        for snapshot in shared.glob("*/*"):
            if snapshot.is_symlink() or not snapshot.is_dir():
                raise ChatPluginError("Run 扩展快照目录无效", code="chat_plugin.snapshot_invalid")
            candidates = [(snapshot / name, False) for name in manifests]
            candidates.extend((path, True) for path in snapshot.rglob("*")
                              if path.name in {"mcp.json", ".mcp.json"})
            for path, standalone in candidates:
                if not path.exists() and not path.is_symlink():
                    continue
                if path.is_symlink() or not path.is_file():
                    raise ChatPluginError("Run 扩展快照配置无效", code="chat_plugin.snapshot_invalid")
                try:
                    data = json.loads(path.read_text(encoding="utf-8"))
                except (OSError, ValueError) as exc:
                    raise ChatPluginError("Run 扩展快照配置无效", code="chat_plugin.snapshot_invalid") from exc
                declaration = data.get("mcpServers", data if standalone else None) if isinstance(data, dict) else None
                if declaration:
                    raise ChatPluginError(
                        "Run 中留有包含 MCP 配置的旧扩展快照；请新建 Run，避免凭据进入 Worker 工作区",
                        code="chat_plugin.worker_mcp_mixed_package",
                    )

    @classmethod
    def validate_record_modes(cls, record: dict[str, Any], modes: Any) -> list[str]:
        selected = cls.validate_modes(modes)
        if any(mode not in record.get("allowed_modes", MODES) for mode in selected):
            raise ChatPluginError("此插件不支持所选场景", code="chat_plugin.mode_unsupported")
        if any(mode != "chat" for mode in selected):
            engines = set(record.get("declared_engines", ENGINES))
            portable = bool(engines) and (bool(record.get("mcp")) or any(
                not skill.get("requires_native") or bool(engines & NATIVE_SKILL_ENGINES)
                for skill in record.get("skills", [])))
            native = any(item.get("kind") in {"extensions", "agents"}
                         and engines.intersection(WORKER_COMPONENT_ENGINES)
                             .intersection(item.get("engines", []))
                         for item in record.get("components", []))
            if not portable and not native:
                raise ChatPluginError("此包没有可投放给 Worker 的 Skill、MCP 或兼容原生插件")
        return selected

    @staticmethod
    def _resolve_local_dir(raw: str) -> Path:
        path = str(raw or "").strip()
        if not path:
            raise ChatPluginError("请填写来源目录路径", code="chat_plugin.source_missing")
        try:
            origin = Path(path).expanduser().resolve(strict=True)
        except FileNotFoundError as exc:
            raise ChatPluginError("来源目录不存在", code="chat_plugin.source_not_found") from exc
        except NotADirectoryError as exc:
            raise ChatPluginError("来源路径不是目录", code="chat_plugin.source_not_directory") from exc
        except OSError as exc:
            raise ChatPluginError("无法访问来源目录", code="chat_plugin.source_inaccessible") from exc
        if not origin.is_dir():
            raise ChatPluginError("来源路径不是目录", code="chat_plugin.source_not_directory")
        return origin

    @staticmethod
    def _from_install_error(exc: InstallError) -> ChatPluginError:
        mapping = {
            "extension.unpinned_git_ref": (
                "请提供固定的 Git commit 或 tag，不支持裸分支",
                "chat_plugin.unpinned_git_ref",
            ),
            "extension.git_fetch_failed": (
                "Git 拉取失败，请检查地址与固定版本",
                "chat_plugin.git_fetch_failed",
            ),
            "extension.source_not_found": (
                "来源文件或目录不存在",
                "chat_plugin.source_not_found",
            ),
            "extension.invalid_source": (
                "插件来源无效",
                "chat_plugin.invalid_source",
            ),
        }
        message, code = mapping.get(
            getattr(exc, "code", ""),
            ("插件导入失败，请检查来源", "chat_plugin.install_failed"),
        )
        return ChatPluginError(message, code=code)

    def install(self, source: dict[str, Any], modes: list[str] | None = None) -> dict[str, Any]:
        kind = source.get("kind", "local-dir")
        if kind not in {"local-dir", "archive", "git"}:
            raise ChatPluginError(
                "支持本地目录、压缩包和固定 Git 版本",
                code="chat_plugin.unsupported_source",
            )
        with tempfile.TemporaryDirectory(dir=self.root, prefix="import-") as work:
            workdir = Path(work)
            if kind == "local-dir":
                origin = self._resolve_local_dir(str(source.get("path", "")))
                if origin == Path.home() or self.root.is_relative_to(origin):
                    raise ChatPluginError("请选择具体插件目录", code="chat_plugin.source_too_broad")
                package = workdir / "package"
                try:
                    _copy_package(origin, package)
                except ChatPluginError:
                    raise
                except OSError as exc:
                    raise ChatPluginError(
                        "无法读取来源目录",
                        code="chat_plugin.source_inaccessible",
                    ) from exc
            else:
                installer = ExtensionInstaller(self.root / "packages")
                try:
                    package, _ = installer._fetch(Source(
                        kind=kind, path=str(source.get("path", "")),
                        url=str(source.get("url", "")), ref=str(source.get("ref", "")),
                    ), workdir)
                except InstallError as exc:
                    raise self._from_install_error(exc) from exc
            # The same normalized bytes are inspected, named by digest, and
            # installed. Archive sources can contain ignored cache files.
            normalized = workdir / "normalized"
            _copy_package(package, normalized)
            record = self._inspect(normalized)
            previous = self.get(record["id"])
            record.update(engines=list(ENGINES), enabled=True, scope_version=2,
                          modes=self.validate_record_modes(record, modes if modes is not None else previous.get("modes", ["chat"])),
                          mode_scope_version=1)
            record["source_kind"] = kind
            record["digest"] = sha256_tree(normalized)
            target = self.root / "packages" / record["id"] / record["digest"]
            target.parent.mkdir(parents=True, exist_ok=True)
            if not target.exists() and not target.is_symlink():
                _copy_package(normalized, target)
            if _checked_tree_digest(target) != record["digest"]:
                raise ChatPluginError("已安装扩展包内容发生变化", code="chat_plugin.package_modified")
            record["root"] = str(target)
            record["previous"] = ({k: v for k, v in previous.items() if k != "previous"} if previous else None)
            self.save(record)
        return self.public(record)

    def install_visualize(self) -> dict[str, Any]:
        """Install the independent, chat-only package through normal lifecycle controls."""
        return self.install({"path": str(VISUALIZE_PACKAGE)}, modes=["chat"])

    def _inspect(self, root: Path) -> dict[str, Any]:
        manifest: dict[str, Any] = {}
        for relative in ("plugin.json", ".codex-plugin/plugin.json", ".claude-plugin/plugin.json", ".cursor-plugin/plugin.json"):
            path = _contained(root, relative)
            if path.is_file():
                manifest = json.loads(path.read_text())
                break
        skill_files = sorted(root.rglob("SKILL.md"))
        if not manifest and (root / "package.json").is_file():
            package = json.loads((root / "package.json").read_text())
            if any(key in package for key in EXTENSION_ENGINES):
                manifest = package
                manifest["name"] = str(package.get("name") or root.name).lstrip("@").replace("/", "-")
        if not manifest and not skill_files:
            raise ChatPluginError("未找到插件清单或 SKILL.md", code="chat_plugin.not_a_plugin")
        name = str(manifest.get("name") or "").strip()
        skills = []
        for path in skill_files[:200]:
            if path.stat().st_size > 256000:
                raise ChatPluginError("SKILL.md 超过 256 KB")
            text = path.read_text()
            front = yaml.safe_load(text.split("---", 2)[1]) if text.startswith("---") and text.count("---") >= 2 else {}
            front = front if isinstance(front, dict) else {}
            skill_name = str(front.get("name") or path.parent.name)
            if not re.fullmatch(r"[\w.-]{1,100}", skill_name):
                raise ChatPluginError("Skill 名称无效")
            if not name:
                name = skill_name
            skills.append({"name": skill_name, "description": str(front.get("description") or "Agent Skill"),
                           "path": str(path.relative_to(root)),
                           "hooks": front.get("hooks") or {},
                           "requires_native": bool(any(front.get(key) for key in ("context", "agent", "hooks", "allowed-tools", "disallowed-tools", "model", "disable-model-invocation")) or "!`" in text),
                           "invocable": front.get("user-invocable", front.get("user_invocable", True)) is not False})
        if not re.fullmatch(r"[a-zA-Z0-9][a-zA-Z0-9_.-]{0,79}", name):
            raise ChatPluginError("插件名称仅支持字母、数字、点、横线和下划线")
        mcp: dict[str, Any] = {}
        declaration = manifest.get("mcpServers")
        declarations = ["mcp.json", ".mcp.json", *(declaration if isinstance(declaration, list) else [declaration] if declaration else [])]
        for value in declarations:
            if isinstance(value, str):
                path = _contained(root, value)
                if not path.is_file():
                    if value not in {"mcp.json", ".mcp.json"}: raise ChatPluginError("MCP 声明文件不存在")
                    continue
                value = json.loads(path.read_text())
            if not isinstance(value, dict): raise ChatPluginError("MCP 声明必须为对象或包内 JSON 路径")
            mcp.update(value.get("mcpServers", value))
        self._validate_mcp(mcp, root=root)
        components = inspect_components(root, manifest)
        for item in components["components"]:
            if item["kind"] == "commands":
                skills.append({"name": item["name"], "description": item["description"],
                               "path": item["path"], "invocable": True, "component_kind": "command",
                               "hooks": item.get("metadata", {}).get("hooks") or {},
                               "requires_native": bool(any(item.get("metadata", {}).get(key) for key in
                                    ("context", "agent", "hooks", "allowed-tools", "disallowed-tools", "model", "disable-model-invocation"))
                                    or "!`" in _contained(root, item["path"]).read_text())})
                if skills[-1]["requires_native"]:
                    item["engines"] = sorted(NATIVE_SKILL_ENGINES)
        for skill in skills:
            if skill.get("hooks"):
                config = skill["hooks"]
                if not isinstance(config, dict):
                    raise ChatPluginError("Skill hooks 必须为事件对象")
                components["components"].append({"kind": "skill_hooks", "name": skill["name"],
                    "config": {"hooks": config}, "engines": sorted(NATIVE_SKILL_ENGINES), "delivery": "native_hooks"})
        if not skills and not mcp and not components["components"]:
            raise ChatPluginError("未找到可导入的 Skill 或 MCP；此插件可能依赖原客户端")
        unsupported = [key for key in ("hooks", "agents", "commands", "extensions") if manifest.get(key) or (root / key).is_dir()]
        return {"id": name, "name": name, "version": str(manifest.get("version") or "local"),
                "description": str(manifest.get("description") or (skills[0]["description"] if skills else "MCP")),
                "skills": skills, "mcp": mcp, "native_components": unsupported, "component_schema_version": 5, **components}

    @staticmethod
    def _validate_mcp(servers: Any, *, root: Path | None = None) -> None:
        if not isinstance(servers, dict) or len(servers) > 20:
            raise ChatPluginError("MCP 配置必须是最多 20 个服务的 mcpServers 对象")
        for name, cfg in servers.items():
            if not re.fullmatch(r"[\w.-]{1,80}", name) or not isinstance(cfg, dict):
                raise ChatPluginError("MCP 服务名称或配置无效")
            if not cfg.get("command") and not str(cfg.get("url", "")).startswith(("https://", "http://")):
                raise ChatPluginError("MCP 需要 command 或 HTTP URL")
            if cfg.get("args") and (not isinstance(cfg["args"], list) or not all(isinstance(v, str) for v in cfg["args"])):
                raise ChatPluginError("MCP args 必须是字符串数组")
            if cfg.get("command") and not isinstance(cfg["command"], str):
                raise ChatPluginError("MCP command 必须是字符串")
            for key in ("env", "headers"):
                if key in cfg and (not isinstance(cfg[key], dict) or not all(isinstance(k, str) and isinstance(v, str) for k, v in cfg[key].items())):
                    raise ChatPluginError(f"MCP {key} 必须是字符串映射")
            if "network" in cfg and (not isinstance(cfg["network"], list) or not all(isinstance(v, str) for v in cfg["network"])):
                raise ChatPluginError("MCP network 必须是数组")
            for index, argument in enumerate([cfg.get("command") or "", *(cfg.get("args") or [])]):
                # An executable may live in a declared runtime (for example a
                # Python virtualenv); path arguments must stay in the immutable
                # package or the MCP's private writable state. The macOS
                # sandbox cannot read an arbitrary absolute script path.
                value = argument.split("=", 1)[1] if "=" in argument else argument
                token = next((token for token in ("${PLUGIN_ROOT}", "${CLAUDE_PLUGIN_ROOT}",
                                                  "${CODEX_PLUGIN_ROOT}", "${PLUGIN_DATA}")
                              if value.startswith(token)), "")
                if token:
                    suffix = value[len(token):]
                    if suffix and not suffix.startswith("/"):
                        raise ChatPluginError("MCP 路径模板后必须是 / 或结束", code="chat_plugin.mcp_path_invalid")
                    base = root if token != "${PLUGIN_DATA}" else Path("/muteki-plugin-data")
                    if base is not None:
                        target = (base / suffix.lstrip("/")).resolve()
                        if not target.is_relative_to(base.resolve()):
                            raise ChatPluginError("MCP 路径不得越过扩展包或私有数据目录", code="chat_plugin.mcp_path_invalid")
                        if root is not None and token != "${PLUGIN_DATA}" and not target.exists():
                            raise ChatPluginError(
                                "MCP 包内资源不存在；请导入包含脚本和资源的扩展包，并以 ${PLUGIN_ROOT}/... 引用",
                                code="chat_plugin.mcp_resource_missing",
                            )
                elif index > 0 and value.startswith("/"):
                    raise ChatPluginError(
                        "MCP 参数引用包外绝对路径；请将脚本和资源导入同一扩展包，并以 ${PLUGIN_ROOT}/... 引用",
                        code="chat_plugin.mcp_path_outside_package",
                    )
                elif "${" in value:
                    raise ChatPluginError("MCP 路径模板无效，仅支持 ${PLUGIN_ROOT} 或 ${PLUGIN_DATA}",
                                          code="chat_plugin.mcp_path_invalid")

    def add_mcp(self, name: str, servers: dict[str, Any], modes: list[str] | None = None) -> dict[str, Any]:
        self._validate_mcp(servers)
        if not servers:
            raise ChatPluginError("请至少配置一个 MCP 服务")
        # Materialize the same package shape without exposing configuration in UI responses.
        with tempfile.TemporaryDirectory(dir=self.root) as temp:
            root = Path(temp)
            (root / "plugin.json").write_text(json.dumps({"name": name, "version": "local", "mcpServers": servers}))
            return self.install({"kind": "local-dir", "path": temp}, modes=modes)

    def update(self, package_id: str, enabled: bool | None = None, rollback: bool = False,
               native_hooks: bool | None = None, digest: str = "",
               modes: list[str] | None = None) -> dict[str, Any]:
        record = self.get(package_id)
        if not record:
            raise ChatPluginError("插件不存在")
        if rollback:
            if not record.get("previous"):
                raise ChatPluginError("没有可回滚的版本")
            record = {**record["previous"], "engines": list(ENGINES), "enabled": record["enabled"], "scope_version": 2,
                      "modes": list(record.get("modes", ["chat"])), "mode_scope_version": 1, "previous": None}
        if enabled is not None:
            record["enabled"] = enabled
        if modes is not None:
            record["modes"] = self.validate_record_modes(record, modes)
        if native_hooks is not None:
            if native_hooks and digest != record["digest"]:
                raise ChatPluginError("插件版本已变化，请重新查看此版本的 hooks 后启用")
            record["hooks_approved_digest"] = record["digest"] if native_hooks else ""
        self.save(record)
        return self.public(record)

    def uninstall(self, package_id: str) -> None:
        # Retain immutable files for in-flight skill references. No host paths are removed.
        with self.connect() as conn:
            conn.execute("DELETE FROM packages WHERE id=?", (package_id,))

    def public(self, record: dict[str, Any]) -> dict[str, Any]:
        worker_compatibility = {}
        for engine in ENGINES:
            components = []
            if any((not skill.get("requires_native") or engine in NATIVE_SKILL_ENGINES)
                   and (not skill.get("hooks") or record.get("hooks_approved_digest") == record.get("digest"))
                   for skill in record.get("skills", [])):
                components.append("Skill")
            if record.get("mcp"):
                components.append("MCP")
            if engine in WORKER_COMPONENT_ENGINES and any(
                item.get("kind") in {"extensions", "agents"}
                and engine in item.get("engines", []) for item in record.get("components", [])
            ):
                components.append("原生插件")
            if not any(mode != "chat" for mode in record.get("allowed_modes", MODES)) or not PROVIDERS[engine].managed_environment:
                components = []
            worker_compatibility[engine] = {
                "status": "available" if components and engine in record.get("declared_engines", ENGINES)
                          else "unavailable",
                "components": components if engine in record.get("declared_engines", ENGINES) else [],
            }
        return {k: record.get(k) for k in ("id", "name", "version", "description", "enabled", "engines", "digest", "skills", "native_components")} | {
            "modes": list(record.get("modes", ["chat"])),
            "allowed_modes": list(record.get("allowed_modes", MODES)),
            "worker_compatibility": worker_compatibility,
            "mcp_servers": list(record.get("mcp", {})), "origin": "managed",
            "can_rollback": bool(record.get("previous")),
            "diagnostics": [self._diagnostics.get(record["id"], "")] if self._diagnostics.get(record["id"]) else [],
            "compatibility": compatibility(record, self._verified_tools),
            "native_hooks_enabled": record.get("hooks_approved_digest") == record.get("digest"),
            "hook_commands": self.hook_commands(record),
            "hook_definitions": [item["config"] for item in record.get("components", []) if item["kind"] in {"hooks", "skill_hooks"}],
        }

    def skill_rows(self, engine: str, *, include_automatic: bool = False) -> list[dict[str, Any]]:
        rows = []
        for record in self.enabled(engine):
            if compatibility(record, self._verified_tools)[engine]["status"] == "blocked":
                continue
            for skill in record["skills"]:
                if skill.get("hooks") and record.get("hooks_approved_digest") != record["digest"]:
                    continue
                if skill.get("requires_native") and engine not in NATIVE_SKILL_ENGINES:
                    continue
                if skill["invocable"] or include_automatic:
                    rows.append({"id": f"managed:{engine}:{record['id']}:{record['digest'][:12]}:{skill['name']}",
                                 "kind": "skill", "name": f"{record['id']}:{skill['name']}",
                                 "description": skill["description"], "source": "Muteki 聊天插件", "scope": "chat",
                                 "engine": engine, "_path": str(_contained(Path(record["root"]), skill["path"])),
                                 "_package_root": record["root"], "component_kind": skill.get("component_kind", "skill"),
                                 "native_engine": engine if skill.get("requires_native") else "",
                                 "native_name": f"muteki-{record['id']}:{skill['name']}" if skill.get("requires_native") else "",
                                 "_priority": 500})
        return rows

    def stage_worker(self, workdir: str | Path, *, engine: str, mode: str,
                     container: Any = None) -> list[str]:
        """Project selected portable Skills into one run-scoped Worker workspace.

        Package bytes are copied once per Run. Relative skill links work in the
        container mount as well as locally, without exposing host Agent homes.
        Native executable components remain governed by their engine ABI; this
        projection exposes only Skills and the separate host-owned MCP bridge.
        """
        if mode not in {"ctf", "pentest"}:
            return []
        from muteki.solver.worker_skills import project_skill_roots
        from muteki.solver.workspace import workspace_root_for_worker

        cwd = Path(workdir).resolve()
        run_root = workspace_root_for_worker(cwd)
        shared = run_root / ".muteki-extensions" / "packages"
        staged: list[str] = []
        with self._lock:
            self._require_run_snapshots_safe(shared)
            records = self.enabled(engine, mode)
            for record in records:
                skills = [skill for skill in record.get("skills", [])
                          if (not skill.get("requires_native") or engine in NATIVE_SKILL_ENGINES)
                          and (not skill.get("hooks") or
                               record.get("hooks_approved_digest") == record.get("digest"))]
                native = any(component.get("kind") in {"extensions", "agents"}
                             and engine in component.get("engines", [])
                             for component in record.get("components", []))
                if not skills and not native:
                    continue
                target = shared / record["id"] / record["digest"]
                self._stage_worker_snapshot(record, target)
                for skill in skills:
                    name = f"muteki-{record['id']}-{skill['name']}"
                    source = (_contained(target, str(skill["path"]))
                              if Path(str(skill["path"])).name == "SKILL.md"
                              else target / ".muteki-worker-skills" / name / "SKILL.md")
                    if not source.is_file():
                        raise ChatPluginError(f"Worker Skill 缺失：{name}")
                    for relative in project_skill_roots(engine):
                        destination = cwd / relative / name
                        destination.parent.mkdir(parents=True, exist_ok=True)
                        relative_target = os.path.relpath(source.parent, destination.parent)
                        if destination.is_symlink() and os.readlink(destination) == relative_target:
                            continue
                        if destination.is_symlink():
                            old_target = (destination.parent / os.readlink(destination)).resolve()
                            if old_target.is_relative_to(shared.resolve()):
                                destination.unlink()
                        if destination.exists() or destination.is_symlink():
                            raise ChatPluginError(f"Worker Skill 目录已存在：{destination}")
                        destination.symlink_to(relative_target, target_is_directory=True)
                    staged.append(name)
        return staged

    def _stage_worker_snapshot(self, record: dict[str, Any], target: Path) -> bool:
        """Recheck Run-visible bytes against a manifest outside the Worker mount."""
        if (not re.fullmatch(r"[a-zA-Z0-9][a-zA-Z0-9_.-]{0,79}", str(record.get("id") or ""))
                or not re.fullmatch(r"[0-9a-f]{64}", str(record.get("digest") or ""))):
            raise ChatPluginError("扩展包身份或版本摘要无效", code="chat_plugin.snapshot_invalid")
        manifest = self.root / "stage-digests" / record["id"] / record["digest"]
        expected = manifest.read_text(encoding="ascii").strip() if manifest.is_file() else ""
        if expected and not re.fullmatch(r"[0-9a-f]{64}", expected):
            raise ChatPluginError("扩展快照校验记录损坏", code="chat_plugin.snapshot_invalid")
        if target.exists() or target.is_symlink():
            current = _checked_tree_digest(target)
            if expected:
                if current == expected:
                    self._freeze_worker_snapshot(target)
                    return False
        source = Path(str(record.get("root") or ""))
        if not source.resolve().is_relative_to((self.root / "packages").resolve()):
            raise ChatPluginError("已安装扩展包路径不在私有目录", code="chat_plugin.snapshot_invalid")
        if _checked_tree_digest(source) != record["digest"]:
            raise ChatPluginError("已安装扩展包内容发生变化", code="chat_plugin.package_modified")
        target.parent.mkdir(parents=True, exist_ok=True)
        temporary = target.with_name(target.name + ".staging-" + os.urandom(4).hex())
        try:
            _copy_package(source, temporary)
            self._strip_worker_mcp_config(temporary, record)
            for skill in record.get("skills", []):
                file = _contained(temporary, str(skill["path"]))
                from .chat_plugin_components import frontmatter
                front, body = frontmatter(file)
                front["name"] = f"muteki-{record['id']}-{skill['name']}"
                content = "---\n" + yaml.safe_dump(front, allow_unicode=True) + "---\n" + body
                if file.name == "SKILL.md":
                    file.write_text(content, encoding="utf-8")
                else:
                    generated = (temporary / ".muteki-worker-skills" /
                                 f"muteki-{record['id']}-{skill['name']}" / "SKILL.md")
                    generated.parent.mkdir(parents=True, exist_ok=True)
                    generated.write_text(content, encoding="utf-8")
            projected = _checked_tree_digest(temporary)
            if expected and projected != expected:
                raise ChatPluginError("扩展快照与已登记版本不一致", code="chat_plugin.snapshot_invalid")
            if target.exists():
                if current != projected:
                    difference = self._snapshot_difference(temporary, target)
                    raise ChatPluginError(
                        f"Run 扩展快照内容发生变化：{record['id']}/{difference}",
                        code="chat_plugin.snapshot_modified",
                    )
                materialized = False
            else:
                temporary.rename(target)
                materialized = True
            self._freeze_worker_snapshot(target)
            if not expected:
                manifest.parent.mkdir(parents=True, exist_ok=True)
                pending = manifest.with_name(manifest.name + "." + os.urandom(4).hex())
                try:
                    pending.write_text(projected, encoding="ascii")
                    pending.chmod(0o600)
                    pending.replace(manifest)
                finally:
                    pending.unlink(missing_ok=True)
            return materialized
        finally:
            if temporary.exists():
                shutil.rmtree(temporary)

    @staticmethod
    def _snapshot_difference(expected: Path, actual: Path) -> str:
        expected_files = {path.relative_to(expected).as_posix(): path
                          for path in expected.rglob("*") if path.is_file()}
        actual_files = {path.relative_to(actual).as_posix(): path
                        for path in actual.rglob("*") if path.is_file()}
        for relative in sorted(expected_files.keys() | actual_files.keys()):
            if relative not in expected_files:
                return f"{relative}（新增文件）"
            if relative not in actual_files:
                return f"{relative}（文件缺失）"
            if sha256_file(expected_files[relative]) != sha256_file(actual_files[relative]):
                return f"{relative}（内容变化）"
        return "文件清单与内容摘要不一致"

    @staticmethod
    def _freeze_worker_snapshot(root: Path) -> None:
        # Python and other runtimes commonly create caches beside source files.
        # Keep the projected source tree read-only; writable state belongs in
        # each Worker's workspace or the MCP's private PLUGIN_DATA directory.
        for path in sorted(root.rglob("*"), key=lambda item: len(item.parts), reverse=True):
            if path.is_symlink():
                raise ChatPluginError("扩展快照含有符号链接", code="chat_plugin.snapshot_invalid")
            mode = 0o555 if path.is_dir() else 0o444 | (stat.S_IMODE(path.stat().st_mode) & 0o111)
            path.chmod(mode)
        root.chmod(0o555)

    def stage_native_worker(self, workdir: str | Path, *, engine: str, mode: str,
                            env: dict[str, str], container: Any = None) -> list[str]:
        """Place ABI-specific Pi/OMP/OpenCode components in the private Worker home."""
        if mode not in {"ctf", "pentest"} or engine not in WORKER_COMPONENT_ENGINES:
            return []
        descriptor = get_descriptor(engine)
        from muteki.solver.workspace import workspace_root_for_worker

        records = [record for record in self.enabled(engine, mode)
                   if any(engine in item.get("engines", []) and item.get("kind") in {"extensions", "agents"}
                          for item in record.get("components", []))]
        if not records:
            return []

        cwd = Path(workdir).resolve()
        run_root = workspace_root_for_worker(cwd)
        shared = run_root / ".muteki-extensions" / "packages"

        def host_path(runtime: str) -> Path:
            path = Path(runtime)
            if container is None:
                result = path.resolve()
            else:
                from muteki.solver.container_exec import CONTAINER_WORKSPACE
                from muteki.solver.credential_accounts import CONTAINER_ACCOUNTS_ROOT
                if path.is_relative_to(CONTAINER_WORKSPACE):
                    result = (Path(container.host_workspace) /
                              path.relative_to(CONTAINER_WORKSPACE)).resolve()
                elif path.is_relative_to(CONTAINER_ACCOUNTS_ROOT) and container.account_root:
                    # Managed Pi/OMP identities use a private, writable Run
                    # projection outside the Worker workspace. Never write to
                    # the operator's source account store.
                    result = (Path(container.account_root) /
                              path.relative_to(CONTAINER_ACCOUNTS_ROOT)).resolve()
                    if not result.is_relative_to(Path(container.account_root).resolve()):
                        raise ChatPluginError("Worker 原生扩展目录越过当前 Run 的账户投影")
                else:
                    raise ChatPluginError("Worker 扩展目录未挂载进容器")
            if not result.is_relative_to(run_root) and not (
                container is not None and container.account_root
                and result.is_relative_to(Path(container.account_root).resolve())
            ):
                raise ChatPluginError("Worker 原生扩展只能写入当前 Run 的私有目录")
            return result

        runtime_home = env.get(descriptor.environment.worker_component_home_env, "")
        if not runtime_home:
            raise ChatPluginError(f"{engine} Worker 缺少原生扩展目录")
        home = host_path(runtime_home)
        if descriptor.environment.worker_component_home_subdir:
            home /= descriptor.environment.worker_component_home_subdir
        delivered: list[str] = []
        with self._lock:
            self._require_run_snapshots_safe(shared)
            for record in records:
                package = shared / record["id"] / record["digest"]
                self._stage_worker_snapshot(record, package)
                for item in record.get("components", []):
                    if engine not in item.get("engines", []) or item.get("kind") not in {"extensions", "agents"}:
                        continue
                    entry = _contained(package, str(item["path"]))
                    if not entry.is_file():
                        raise ChatPluginError(f"Worker 原生组件缺失：{record['id']}/{item['name']}")
                    if item["kind"] == "agents":
                        destination = home / "agents" / f"muteki-{record['id']}-{item['name']}.md"
                        destination.parent.mkdir(parents=True, exist_ok=True)
                        _snapshot_copy(entry, destination) if not destination.exists() else None
                    else:
                        directory = home / descriptor.components.extension_dir
                        directory.mkdir(parents=True, exist_ok=True)
                        suffix = sha256(str(item["path"]).encode()).hexdigest()[:8]
                        destination = directory / f"muteki-{record['id']}-{item['name']}-{suffix}.js"
                        runtime_entry = (str(container.to_container_path(str(entry)))
                                         if container is not None else str(entry))
                        export = ("export *" if descriptor.components.extension_export == "star"
                                  else "export { default }")
                        content = f"{export} from {json.dumps(Path(runtime_entry).as_uri())};\n"
                        if not destination.exists() or destination.read_text() != content:
                            destination.write_text(content, encoding="utf-8")
                    delivered.append(f"{record['id']}:{item['name']}")
        if container is not None and delivered:
            from muteki.solver.container_exec import _chown_tree_to_worker
            _chown_tree_to_worker(str(home), image=container.image)
        return delivered

    def skill_catalog_context(self, engine: str) -> str:
        rows = [row for row in self.skill_rows(engine, include_automatic=True) if not row.get("native_engine")]
        if not rows:
            return ""
        catalog = [{"name": r["name"], "description": r["description"][:600], "path": r["_path"]} for r in rows]
        return ("[Muteki 当前引擎的聊天 Skills]\n以下目录来自用户在 Muteki 中为当前引擎启用的插件。"
                "仅在任务需要时读取相应 SKILL.md，并按其所在目录解析相对资源路径。"
                "这些插件仅作用于本次聊天，不要安装或写入用户的 Agent 配置目录。\n"
                + json.dumps(catalog, ensure_ascii=False))

    def descriptor_tools(self, engine: str) -> list[dict[str, Any]]:
        return list(self._tools.get(f"chat:{engine}::{self.revision(engine)}", []))

    def visualization_root(self, thread_id: str) -> Path:
        root = self.root / "workspaces" / sha256(thread_id.encode()).hexdigest()[:24] / "visualizations"
        root.mkdir(parents=True, exist_ok=True)
        return root

    def _visualization_snapshot(self, thread_id: str, message_id: str, reference: Any, engine: str) -> dict[str, Any]:
        from .visualizations import snapshot
        root = self.root / "visualizations" / sha256(thread_id.encode()).hexdigest()
        return snapshot(root, self.visualization_root(thread_id), message_id, reference,
                        lambda: self.visualization_assets(engine))

    def publish_visualizations(self, thread_id: str, messages: list[Any], engine: str) -> None:
        from .visualizations import references
        for message in messages:
            if message.role == "assistant":
                for reference in references(message.text or ""):
                    self._visualization_snapshot(thread_id, message.message_id, reference, engine)

    def visualization_document(self, thread_id: str, messages: list[Any], engine: str,
                               path: str, message_id: str = "") -> dict[str, Any]:
        from .visualizations import VisualizationError, references
        matches = [(message, reference) for message in messages
                   if message.role == "assistant" and (not message_id or message.message_id == message_id)
                   for reference in references(message.text or "") if reference.path == path]
        if len({message.message_id for message, _ in matches}) != 1:
            raise VisualizationError("visualization.not_attached", "图形未附着到当前消息，或来源消息不唯一")
        message, reference = matches[0]
        result = self._visualization_snapshot(thread_id, message.message_id, reference, engine)
        if "error" in result:
            raise VisualizationError(result["error"]["code"], result["error"]["message"])
        return result

    def visualization_context(self, thread_id: str) -> str:
        root = self.visualization_root(thread_id)
        return ("[Muteki 聊天展示接口]\n需要结构图或交互图时，直接输出 muteki-visualize 围栏，"
                "块内放完整 HTML 片段；宿主保存并内联渲染，无需文件写权限或控制工具。"
                "不要使用网络 API。主题与交互细节按已启用的 visualize Skill 读取。"
                f"文件交付与本地图片目录为 {root}；文件交付时单独一行输出 "
                'muteki-visualize {"path":"绝对路径.html"}。\n')

    def visualization_assets(self, engine: str) -> dict[str, str]:
        from .composer_capabilities import discover_skills
        candidates = [*self.skill_rows(engine), *discover_skills(engine)]
        for row in candidates:
            if str(row.get("name", "")).split(":")[-1] != "visualize":
                continue
            root = Path(row["_path"]).parent
            result = {}
            for name in ("visualize.css", "visualize.html", "calendar.js"):
                path = _contained(root, "assets/" + name)
                if path.is_file() and path.stat().st_size <= 600000:
                    result[name] = path.read_text()
            if result:
                return result
        return {}

    @staticmethod
    def hook_commands(record: dict[str, Any]) -> list[str]:
        commands = []
        def collect(value):
            if isinstance(value, dict):
                if value.get("type") == "command":
                    commands.append(str(value.get("command") or ""))
                elif value.get("type") in {"prompt", "agent"}:
                    commands.append(str(value["type"]) + ": " + str(value.get("prompt") or ""))
                for item in value.values(): collect(item)
            elif isinstance(value, list):
                for item in value: collect(item)
        for item in record.get("components", []):
            if item["kind"] in {"hooks", "skill_hooks"}: collect(item.get("config"))
        return commands

    def native_launch_options(self, engine: str, env: dict[str, str]) -> dict[str, Any]:
        descriptor = find_descriptor(engine)
        packaging = descriptor.components.native_plugin_packages if descriptor is not None else "none"
        if packaging == "none":
            return {}
        claude_packages = packaging == "claude_local_plugins"
        private = Path(env["MUTEKI_CHAT_PRIVATE_ROOT"])
        packages = []
        approved_hooks = {}
        for record in self.enabled(engine):
            hooks_allowed = record.get("hooks_approved_digest") == record["digest"]
            items = [c for c in record.get("components", []) if engine in c["engines"] and c["kind"] in {"hooks", "agents"}
                     and (c["kind"] != "hooks" or hooks_allowed)]
            if engine in NATIVE_SKILL_ENGINES and any(s.get("requires_native") for s in record.get("skills", [])):
                items.append({"kind": "skills"})
            if compatibility(record, self._verified_tools)[engine]["status"] == "blocked":
                continue
            if not items:
                continue
            target = private / "native-packages" / "plugins" / record["id"]
            if not target.exists():
                _copy_package(Path(record["root"]), target)
                native = {"name": record["id"], "version": str(record.get("version") or "1.0.0"),
                          "description": record.get("description", ""), "hooks": {"hooks": {}}}
                if claude_packages:
                    native["name"] = "muteki-" + record["id"]
                    native["skills"] = sorted({"./" + str(Path(s["path"]).parent)
                                               for s in record.get("skills", []) if s.get("component_kind") != "command"})
                    native["commands"] = ["./" + s["path"] for s in record.get("skills", []) if s.get("component_kind") == "command"]
                hooks = next((c["config"] for c in items if c["kind"] == "hooks"), None)
                def wrap(node):
                    if isinstance(node, dict):
                        if node.get("type") == "command" and isinstance(node.get("command"), str):
                            key = sha256((record["id"] + node["command"]).encode()).hexdigest()[:16]
                            cfg = private / "hook-config" / (key + ".json")
                            cfg.parent.mkdir(parents=True, exist_ok=True)
                            cfg.write_text(json.dumps({"root": str(target), "state": str(private / "hook-state" / record["id"]),
                                                       "command": node["command"], "env": record.get("environment", {})}))
                            node["command"] = shlex.join([sys.executable, str(Path(__file__).with_name("chat_hook_runner.py")), str(cfg)])
                        for value in node.values():
                            wrap(value)
                    elif isinstance(node, list):
                        for value in node:
                            wrap(value)
                if hooks:
                    hooks = json.loads(json.dumps(hooks))
                    wrap(hooks)
                    native["hooks"] = hooks
                for skill in record.get("skills", []):
                    if not skill.get("hooks"):
                        continue
                    file = target / skill["path"]
                    if not hooks_allowed:
                        file.unlink(missing_ok=True)
                        continue
                    from .chat_plugin_components import frontmatter
                    front, body = frontmatter(file)
                    wrap(front["hooks"])
                    file.write_text("---\n" + yaml.safe_dump(front, allow_unicode=True) + "---\n" + body)
                if claude_packages:
                    native["skills"] = [p for p in native["skills"] if (target / p / "SKILL.md").is_file()]
                    native["commands"] = [p for p in native["commands"] if (target / p).is_file()]
                if claude_packages and any(c["kind"] == "agents" for c in items):
                    native["agents"] = ["./" + c["path"] for c in items if c["kind"] == "agents"]
                # Native-only envelope: MCP stays behind the authorized Muteki gateway.
                (target / "plugin.json").unlink(missing_ok=True)
                for folder in (".claude-plugin", ".codex-plugin"):
                    (target / folder).mkdir(exist_ok=True)
                    payload = dict(native)
                    if folder == ".claude-plugin":
                        payload["hooks"] = native["hooks"].get("hooks", native["hooks"])
                    (target / folder / "plugin.json").write_text(json.dumps(payload))
                for mcp in (".mcp.json", "mcp.json"):
                    (target / mcp).unlink(missing_ok=True)
                (target / "hooks/hooks.json").unlink(missing_ok=True)
            packages.append({"name": record["id"], "path": str(target)})
            if hooks_allowed:
                native_manifest = json.loads((target / ".codex-plugin/plugin.json").read_text())
                approved_hooks[record["id"] + "@muteki-chat"] = self.hook_commands({"components": [{"kind": "hooks", "config": native_manifest.get("hooks")} ]})
        if claude_packages:
            return {"plugins": [{"type": "local", "path": p["path"]} for p in packages]}
        marketplace = private / "native-packages" / ".agents" / "plugins" / "marketplace.json"
        if packages:
            marketplace.parent.mkdir(parents=True, exist_ok=True)
            marketplace.write_text(json.dumps({"name": "muteki-chat", "interface": {"displayName": "Muteki Chat"},
                "plugins": [{"name": p["name"], "source": {"source": "local", "path": "./plugins/" + p["name"]},
                             "policy": {"installation": "AVAILABLE", "authentication": "ON_INSTALL"},
                             "category": "Productivity"} for p in packages]}))
        return {"chat_native_plugins": [{"pluginName": p["name"], "marketplacePath": str(marketplace)} for p in packages],
                "chat_hook_approvals": approved_hooks}

    def prepare_environment(
        self, engine: str, identity: str, supplied: dict[str, str], *,
        previous_revision: str | None = None, include_assets: bool = True,
    ) -> dict[str, str]:
        """Reuse one private home per identity and incrementally refresh imports."""
        if engine not in PROVIDERS or not PROVIDERS[engine].managed_environment:
            return supplied
        from .native_environment import NativeEnvironmentError, environment_home, synchronize
        provider = provider_for(engine)
        environment = provider.descriptor.environment
        host_discovery = os.environ.get("MUTEKI_HOST_DISCOVERY", "1") != "0"
        source = provider.native_root() if host_discovery else None
        names = (*provider.configuration_names, *provider.credential_names,
                 *(provider.asset_names if include_assets else ()))
        try:
            with self._lock, environment_home(self.root, engine, identity, previous_revision) as root:
                home = root / "home"
                target = home / provider.home_relative
                target.mkdir(parents=True, exist_ok=True)
                imports = [(source / name, Path("home") / provider.home_relative / name) for name in names] if source is not None else []
                if include_assets and host_discovery:
                    imports.append((Path.home() / ".agents/skills", Path("home/.agents/skills")))
                if host_discovery:
                    imports.extend((Path.home() / item.host_relative, Path(item.target))
                                   for item in environment.extra_imports)
                account = supplied.get(provider.home_variable)
                if account:
                    account_source = Path(account).expanduser()
                    if environment.home_env_is_parent:
                        account_source /= engine
                    if source is None or account_source.resolve() != source.resolve():
                        # Generated accounts override configuration/auth only;
                        # never import a second copy of all host capability assets.
                        overrides = [(account_source / name, Path("home") / provider.home_relative / name)
                                     for name in (*provider.configuration_names, *provider.credential_names)
                                     if (account_source / name).exists()]
                        replaced = {destination for _, destination in overrides}
                        imports = [(src, dst) for src, dst in imports if dst not in replaced] + overrides
                revision = self.revision(engine) if include_assets else "probe"
                ready = root / ".ready"
                # Stage generated components as tracked imports too, so disabling
                # a package removes only its generated files, not native history.
                with tempfile.TemporaryDirectory(prefix=".components-", dir=root) as stage:
                    if include_assets:
                        for record in self.enabled(engine):
                            install_native_components(record, engine, Path(stage))
                    imports.append((Path(stage), Path("home") / provider.home_relative))
                    remappings = ((str(source), str(target)),) if source is not None else ()
                    synchronize(root, imports, _snapshot_copy, remappings)
                if (engine == "omp" and source is not None
                        and (not account or Path(account).expanduser().resolve() == source.resolve())):
                    _sync_omp_databases(source, target, root)
                if ready.exists() and ready.read_text() != revision:
                    native_packages = root / "native-packages"
                    if native_packages.is_symlink():
                        raise NativeEnvironmentError("Native package directory is a symlink")
                    if native_packages.exists():
                        shutil.rmtree(native_packages)
                ready.write_text(revision)
        except (NativeEnvironmentError, OSError, sqlite3.Error, ValueError) as exc:
            raise ChatPluginError(f"Native environment preparation failed: {exc}",
                                  code="chat_plugin.environment_prepare_failed") from exc
        env = dict(supplied)
        env.update({"HOME": str(home), "USERPROFILE": str(home), provider.home_variable: str(target),
                    "XDG_DATA_HOME": str(root / "data"), "XDG_CACHE_HOME": str(root / "cache"),
                    "XDG_CONFIG_HOME": str(home / ".config"), "MUTEKI_CHAT_PRIVATE_ROOT": str(root)})
        for name, location in environment.private_env.items():
            env[name] = str(target if location == "home_target" else root / "data")
        # Cursor persists even explicitly supplied API keys/tokens to the
        # macOS keychain by default. A private chat home has no login
        # keychain, so this can open a system dialog on every startup.
        # Managed credentials are re-injected on each launch; keep them
        # only in memory. Preserve native login when no credential is given.
        if environment.memory_credential_store_env and any(
                env.get(key, os.environ.get(key, "")).strip()
                for key in environment.memory_credential_store_when):
            env[environment.memory_credential_store_env] = "memory"
        return env

    def prepare_codex_fork_history(
        self, source_identity: str, supplied: dict[str, str], *, previous_revision: str | None = None,
    ) -> None:
        """T3's shared history/private auth overlay, within managed chat homes.

        The target keeps its own auth, provider config, models and assets.
        Only native history/SQLite state is shared; user homes are unchanged.
        Conflicting existing target state is an explicit error.
        """
        from .native_environment import environment_home
        target = Path(supplied["CODEX_HOME"]).resolve()
        managed = (self.root / "sessions").resolve()
        if not target.is_relative_to(managed):
            raise ChatPluginError("Codex fork requires a managed target home", code="conversation.fork.home_invalid")
        with self._lock, environment_home(self.root, "codex", source_identity, previous_revision) as root:
            source = root / "home" / provider_for("codex").home_relative
            if not source.is_dir() or not (source / "sessions").exists():
                raise ChatPluginError("Native Codex fork history is missing", code="conversation.fork.history_missing")
            entries = {"sessions", "archived_sessions", "sqlite", "shell_snapshots", "worktrees"}
            entries.update(path.name for path in source.iterdir()
                           if path.name.endswith((".sqlite", ".sqlite-wal", ".sqlite-shm")))
            # Sidecars must address the same store even when currently absent.
            for name in list(entries):
                if name.endswith(".sqlite"):
                    entries.update({name + "-wal", name + "-shm"})
            for name in sorted(entries):
                original, link = source / name, target / name
                resolved = original.resolve()
                if not resolved.is_relative_to(managed):
                    raise ChatPluginError("Native history escapes managed homes", code="conversation.fork.home_invalid")
                if link.is_symlink() and link.resolve() == resolved:
                    continue
                if link.exists() or link.is_symlink():
                    raise ChatPluginError(f"Target native history already exists: {name}", code="conversation.fork.home_conflict")
                link.symlink_to(resolved, target_is_directory=name in {"sessions", "archived_sessions", "sqlite", "shell_snapshots", "worktrees"})

    def release_thread_assets(self, thread_id: str) -> int:
        """Called after successful close when archiving a chat. Keep its history."""
        from .native_environment import environment_home, evict_imports
        index = self.root / "environments.sqlite3"
        if not index.exists():
            return 0
        with closing(sqlite3.connect(index, timeout=60)) as conn:
            prefix = thread_id + ":"
            rows = conn.execute("SELECT engine, owner FROM homes WHERE substr(owner,1,?)=?",
                                (len(prefix), prefix)).fetchall()
        removed = 0
        for engine, identity in rows:
            provider = provider_for(engine)
            prefixes = tuple((Path("home") / provider.home_relative / name).as_posix()
                             for name in provider.asset_names) + ("home/.agents/skills",)
            with self._lock, environment_home(self.root, engine, identity) as root:
                removed += evict_imports(root, prefixes)
        return removed


    async def prepare_tools(self, engine: str, mode: str = "chat", scope: str = "") -> list[dict[str, Any]]:
        if mode == "chat" and engine in PROVIDERS and not PROVIDERS[engine].gateway_tools:
            return []  # Portable skills work without claiming MCP injection.
        cache_key = f"{mode}:{engine}:{scope}:{self.revision(engine, mode)}"
        async with self._prepare_locks.setdefault(cache_key, asyncio.Lock()):
            return await self._prepare_tools(engine, mode, scope)

    async def _discover_mcp_server(self, engine: str, mode: str, scope: str,
                                   record: dict[str, Any], server: str,
                                   config: dict[str, Any]) -> list[dict[str, Any]]:
        worker = self.worker(engine, record, server, config, scope=scope if mode != "chat" else "")
        await worker.wait_ready()
        result = await worker.request("list_tools", {}) if "tools" in worker.capabilities else None
        label = re.sub(r"[^a-zA-Z0-9_]", "_", record["id"])[:8]
        prefix = (("chat_" if mode == "chat" else "work_") + label + "_"
                  + sha256(f"{mode}:{engine}:{record['id']}:{record['digest']}:{server}".encode()).hexdigest()[:8] + "_")
        tools = []
        for tool in result.tools if result else []:
            suffix = re.sub(r"[^a-zA-Z0-9_-]", "_", tool.name)[:12] + "_" + sha256(tool.name.encode()).hexdigest()[:6]
            tools.append({"name": prefix + suffix, "description": f"[Muteki {record['name']} / {server}] {tool.description or tool.name}",
                          "input_schema": tool.inputSchema, "_package": record["id"], "_server": server,
                          "_tool": tool.name, "_digest": record["digest"]})
        for capability, methods in {"resources": ("list_resources", "list_resource_templates", "read_resource"),
                                    "prompts": ("list_prompts", "get_prompt")}.items():
            if capability not in worker.capabilities:
                continue
            for method in methods:
                schema = {"type": "object", "properties": {}, "additionalProperties": False}
                if method == "read_resource":
                    schema.update(properties={"uri": {"type": "string"}}, required=["uri"])
                elif method == "get_prompt":
                    schema.update(properties={"name": {"type": "string"}, "arguments": {"type": "object", "additionalProperties": {"type": "string"}}}, required=["name"])
                else:
                    schema["properties"] = {"cursor": {"type": "string"}}
                tools.append({"name": prefix + method, "description": f"[Muteki {record['name']} / {server}] MCP {method}",
                              "input_schema": schema, "_package": record["id"], "_server": server,
                              "_tool": method, "_method": method, "_digest": record["digest"]})
        return tools

    async def _prepare_tools(self, engine: str, mode: str = "chat", scope: str = "") -> list[dict[str, Any]]:
        key = f"{mode}:{engine}:{scope}:{self.revision(engine, mode)}"
        selected = [(record, server, config) for record in self.enabled(engine, mode)
                    for server, config in record["mcp"].items() if not config.get("disabled")]
        server_tools: dict[tuple[str, str, str], list[dict[str, Any]]] = {}
        failures: list[dict[str, str]] = []
        pending: dict[asyncio.Task, tuple[dict[str, Any], str, tuple[str, str, str]]] = {}
        async def discover(record: dict[str, Any], server: str, config: dict[str, Any]):
            return await self._discover_mcp_server(engine, mode, scope, record, server, config)

        def failed(record: dict[str, Any], server: str, cause: str, cause_code: str) -> None:
            # Never expose transport text: headers and environment may contain credentials.
            if mode == "chat":
                self._diagnostics[record["id"]] = f"MCP {server} 连接失败，请检查配置与运行环境"
            failures.append({"package": record["id"], "server": server,
                             "code": "worker_mcp.connection_failed", "cause": cause,
                             "cause_code": cause_code})

        now = time.monotonic()
        for record, server, config in selected:
            identity = (key, record["id"], server)
            health = self._mcp_health.get(identity, {})
            worker_key = f"{record['id']}:{record['digest']}:{server}" + (f"|{scope}" if mode != "chat" and scope else "")
            worker = self._workers.get(worker_key)
            if health.get("status") == "connected" and worker is not None and not worker.task.done():
                server_tools[identity] = self._mcp_server_tools.get(identity, [])
            elif health.get("status") == "failed" and now < float(health.get("retry_after", 0)):
                failed(record, server, str(health.get("cause") or "ConnectionError"),
                       str(health.get("cause_code") or "worker_mcp.connection_failed"))
            else:
                task = asyncio.create_task(discover(record, server, config))
                pending[task] = (record, server, identity)
        if pending:
            # All servers are probed concurrently, with one deadline for the
            # catalog. A broken endpoint cannot serialize startup or hold it
            # for 25 seconds per server. It is retried on a later catalog read.
            done, waiting = await asyncio.wait(pending, timeout=12)
            for task in waiting:
                task.cancel()
            if waiting:
                await asyncio.gather(*waiting, return_exceptions=True)
            for task in done:
                record, server, identity = pending[task]
                try:
                    result = task.result()
                except Exception as exc:
                    cause = type(exc).__name__
                    cause_code = (exc.code if isinstance(exc, ChatPluginError)
                                  else "worker_mcp.connection_failed")
                    self._mcp_health[identity] = {"status": "failed", "cause": cause,
                                                  "cause_code": cause_code,
                                                  "retry_after": time.monotonic() + 15,
                                                  "observed_at": datetime.now(timezone.utc).isoformat()}
                    failed(record, server, cause, cause_code)
                else:
                    self._mcp_server_tools[identity] = result
                    self._mcp_health[identity] = {"status": "connected",
                                                  "observed_at": datetime.now(timezone.utc).isoformat()}
                    server_tools[identity] = result
            for task in waiting:
                record, server, identity = pending[task]
                self._mcp_health[identity] = {"status": "failed", "cause": "TimeoutError",
                                              "cause_code": "worker_mcp.startup_timeout",
                                              "retry_after": time.monotonic() + 15,
                                              "observed_at": datetime.now(timezone.utc).isoformat()}
                failed(record, server, "TimeoutError", "worker_mcp.startup_timeout")
        tools = [tool for record, server, _ in selected
                 for tool in server_tools.get((key, record["id"], server), [])]
        self._tools[key] = tools
        self._mcp_connection_errors[key] = failures
        if mode == "chat":
            failed_packages = {item["package"] for item in failures}
            for record, _, _ in selected:
                if record["id"] not in failed_packages:
                    self._diagnostics.pop(record["id"], None)
            self._verified_tools[engine] = {name for tool in tools for name in
                (tool["name"], tool["_tool"], f"mcp__{tool['_server']}__{tool['_tool']}", f"{tool['_server']}.{tool['_tool']}")}
        return tools

    def runtime_mcp_health(self, engine: str, mode: str, scope: str) -> list[dict[str, Any]]:
        """Observed Run connections; a configured server is not a live connection."""
        key = f"{mode}:{engine}:{scope}:{self.revision(engine, mode)}"
        rows = []
        for record in self.enabled(engine, mode):
            for server, config in record["mcp"].items():
                if config.get("disabled"):
                    continue
                identity = (key, record["id"], server)
                health = self._mcp_health.get(identity, {})
                worker_key = f"{record['id']}:{record['digest']}:{server}" + (f"|{scope}" if mode != "chat" and scope else "")
                worker = self._workers.get(worker_key)
                status = str(health.get("status") or "not_checked")
                if status == "connected" and (worker is None or worker.task.done()):
                    status = "disconnected"
                rows.append({"package_id": record["id"], "server": server, "status": status,
                             "observed_at": health.get("observed_at")})
        return rows

    async def invalidate(self) -> None:
        self._tools.clear()
        self._mcp_connection_errors.clear()
        self._mcp_server_tools.clear()
        self._mcp_health.clear()
        self._prepare_locks.clear()
        self._verified_tools.clear()
        active = {f"{r['id']}:{r['digest']}:{server}"
                  for r in self.records() if r.get("enabled", True) for server in r["mcp"]}
        removed = [self._workers.pop(key) for key in list(self._workers)
                   if key.split("|", 1)[0] not in active]
        for worker in removed:
            worker.task.cancel()
        await asyncio.gather(*(w.task for w in removed), return_exceptions=True)
        self._diagnostics.clear()

    async def release_scope(self, scope: str) -> None:
        """Stop MCP processes owned by a finished Run."""
        if not scope:
            return
        for cache in (self._tools, self._prepare_locks, self._mcp_connection_errors):
            for key in list(cache):
                if len(parts := key.split(":", 3)) == 4 and parts[2] == scope:
                    cache.pop(key, None)
        for cache in (self._mcp_server_tools, self._mcp_health):
            for identity in list(cache):
                if len(parts := identity[0].split(":", 3)) == 4 and parts[2] == scope:
                    cache.pop(identity, None)
        removed = [self._workers.pop(key) for key in list(self._workers)
                   if key.endswith("|" + scope)]
        for worker in removed:
            worker.task.cancel()
        await asyncio.gather(*(worker.task for worker in removed), return_exceptions=True)

    def mcp_connection_errors(self, engine: str, mode: str, scope: str) -> list[dict[str, str]]:
        key = f"{mode}:{engine}:{scope}:{self.revision(engine, mode)}"
        return list(self._mcp_connection_errors.get(key, []))

    def worker(self, engine: str, record: dict[str, Any], server: str, config: dict[str, Any],
               *, scope: str = "") -> McpWorker:
        # Chat keeps its existing connection. Worker MCP processes are scoped to
        # one Run so mutable server state cannot cross engagement boundaries.
        key = f"{record['id']}:{record['digest']}:{server}" + (f"|{scope}" if scope else "")
        worker = self._workers.get(key)
        if worker is None or worker.task.done():
            state = self.root / "mcp-state" / sha256(key.encode()).hexdigest()[:24]
            worker = McpWorker(config, Path(record["root"]), state)
            self._workers[key] = worker
        return worker

    async def invoke(self, engine: str, name: str, arguments: dict[str, Any],
                     *, mode: str = "chat", scope: str = "") -> Any:
        tool = next((t for t in await self.prepare_tools(engine, mode, scope) if t["name"] == name), None)
        if not tool:
            raise ChatPluginError("当前 Agent 未启用该工具，或插件已停用")
        record = self.get(tool["_package"])
        if (not record or not record.get("enabled", True) or mode not in record.get("modes", ["chat"])
                or engine not in record["engines"] or record["digest"] != tool["_digest"]):
            raise ChatPluginError("插件版本或使用范围已变化，请重新读取工具目录")
        result = await self.worker(engine, record, tool["_server"], record["mcp"][tool["_server"]],
                                   scope=scope if mode != "chat" else "").request(
            tool.get("_method", "call_tool"), arguments if tool.get("_method") else {"name": tool["_tool"], "arguments": arguments})
        return result.model_dump(mode="json", by_alias=True)

    async def close(self) -> None:
        workers = list(self._workers.values())
        for worker in workers:
            worker.task.cancel()
        await asyncio.gather(*(w.task for w in workers), return_exceptions=True)
        self._workers.clear()


class McpWorker:
    """One task owns the MCP connection and its AnyIO cancel scopes."""
    def __init__(self, config: dict[str, Any], package: Path, state: Path) -> None:
        self.queue: asyncio.Queue = asyncio.Queue()
        self.ready = asyncio.get_running_loop().create_future()
        self.capabilities: dict[str, Any] = {}
        self.task = asyncio.create_task(self.run(config, package, state))

    async def wait_ready(self) -> None:
        try:
            await asyncio.wait_for(asyncio.shield(self.ready), 25)
        except asyncio.TimeoutError:
            self.task.cancel()
            await asyncio.gather(self.task, return_exceptions=True)
            raise ChatPluginError("MCP 启动超时", code="worker_mcp.startup_timeout") from None

    async def request(self, method: str, params: dict[str, Any]) -> Any:
        await self.wait_ready()
        future = asyncio.get_running_loop().create_future()
        await self.queue.put((method, params, future))
        return await asyncio.wait_for(future, 90)

    async def run(self, config: dict[str, Any], package: Path, state: Path) -> None:
        current = None
        stage = "import"
        try:
            from mcp import ClientSession, StdioServerParameters
            from mcp.client.stdio import stdio_client
            from mcp.client.sse import sse_client
            from mcp.client.streamable_http import streamablehttp_client
            stage = "state"
            state.mkdir(parents=True, exist_ok=True)
            state.chmod(0o700)
            def render(value: str) -> str:
                return (value.replace("${PLUGIN_ROOT}", str(package)).replace("${PLUGIN_DATA}", str(state))
                        .replace("${CLAUDE_PLUGIN_ROOT}", str(package)).replace("${CODEX_PLUGIN_ROOT}", str(package)))
            async with AsyncExitStack() as stack:
                if config.get("command"):
                    from muteki.conversation.chat_mcp_isolation import isolated_mcp_command
                    env = {k: v for k, v in os.environ.items() if k in {"PATH", "LANG", "LC_ALL", "SYSTEMROOT"}}
                    env.update({k: render(str(v)) for k, v in config.get("env", {}).items()})
                    env.update({"HOME": str(state), "TMPDIR": str(state), "XDG_CONFIG_HOME": str(state / "config"), "XDG_CACHE_HOME": str(state / "cache")})
                    argv = [render(config["command"]), *[render(v) for v in config.get("args", [])]]
                    stage = "isolation"
                    command = isolated_mcp_command(argv, package, state, env, bool(config.get("network")))
                    # Preserve the complete server/sandbox stderr in a private
                    # artifact. It may contain credentials, so only a typed
                    # reason is returned to Workers and the management UI.
                    fd = os.open(state / "mcp-stderr.log", os.O_WRONLY | os.O_CREAT | os.O_APPEND, 0o600)
                    errlog = stack.enter_context(os.fdopen(fd, "w", encoding="utf-8"))
                    stage = "connect"
                    read, write = await stack.enter_async_context(stdio_client(StdioServerParameters(
                        command=command[0], args=command[1:], env=env, cwd=str(state)), errlog=errlog))
                else:
                    stage = "connect"
                    if str(config.get("type") or config.get("transport") or "").lower() == "sse":
                        read, write = await stack.enter_async_context(sse_client(config["url"], headers=config.get("headers")))
                    else:
                        read, write, _ = await stack.enter_async_context(streamablehttp_client(config["url"], headers=config.get("headers")))
                session = await stack.enter_async_context(ClientSession(read, write))
                stage = "initialize"
                initialized = await session.initialize()
                self.capabilities = initialized.capabilities.model_dump(exclude_none=True)
                self.ready.set_result(True)
                stage = "running"
                while True:
                    method, params, current = await self.queue.get()
                    if current.cancelled():
                        continue
                    try:
                        value = await getattr(session, method)(**params)
                        if not current.done():
                            current.set_result(value)
                    except Exception as exc:
                        _log.warning("MCP request failed state=%s error_type=%s", state.name, type(exc).__name__)
                        if not current.done():
                            current.set_exception(ChatPluginError("MCP 工具请求失败", code="worker_mcp.request_failed"))
        except BaseException as exc:
            if not isinstance(exc, asyncio.CancelledError):
                _log.warning("MCP process failed state=%s stage=%s error_type=%s stderr=%s",
                             state.name, stage, type(exc).__name__, state / "mcp-stderr.log")
            if not self.ready.done():
                if isinstance(exc, asyncio.CancelledError):
                    code = "worker_mcp.cancelled"
                elif stage == "isolation" and isinstance(exc, IsolationUnavailable):
                    code = "worker_mcp.isolation_unavailable"
                else:
                    code = {"import": "worker_mcp.sdk_unavailable",
                            "state": "worker_mcp.state_unavailable",
                            "isolation": "worker_mcp.isolation_failed",
                            "connect": "worker_mcp.launch_failed",
                            "initialize": "worker_mcp.protocol_failed"}.get(stage, "worker_mcp.disconnected")
                self.ready.set_exception(ChatPluginError("MCP 服务启动失败", code=code))
        finally:
            if current is not None and not current.done():
                current.set_exception(ChatPluginError("MCP 服务已停止", code="worker_mcp.disconnected"))
            while not self.queue.empty():
                _, _, future = self.queue.get_nowait()
                if not future.done():
                    future.set_exception(ChatPluginError("MCP 服务已停止", code="worker_mcp.disconnected"))
