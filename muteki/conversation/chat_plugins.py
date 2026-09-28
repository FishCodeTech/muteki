"""Conversation-only packages and per-engine capability providers.

Packages are immutable snapshots. Enabling a package never installs it into an
operator's agent directories, a project, an Extension Host, or a Worker.
"""
from __future__ import annotations

import asyncio
from contextlib import AsyncExitStack
import ctypes
from hashlib import sha256
import json
import os
from pathlib import Path
import re
import shlex
import shutil
import sqlite3
import tempfile
import threading
import sys
from typing import Any

import yaml

from muteki.extensions.installer import ExtensionInstaller, Source, sha256_tree
from muteki.conversation.chat_providers import PROVIDERS, provider_for
from muteki.conversation.chat_plugin_components import compatibility, inspect_components, install_native_components

ENGINES = ("claude", "codex", "cursor", "pi", "omp", "kimi", "grok", "opencode")


class ChatPluginError(ValueError):
    pass


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
    return shutil.copy2(source, target)


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


class ChatPluginService:
    def __init__(self, root: Path) -> None:
        self.root = root.resolve()
        self.root.mkdir(parents=True, exist_ok=True)
        self.root.chmod(0o700)
        self.db = self.root / "registry.sqlite3"
        self._lock = threading.RLock()
        self._workers: dict[str, McpWorker] = {}
        self._tools: dict[str, list[dict[str, Any]]] = {}
        self._verified_tools: dict[str, set[str]] = {}
        self._diagnostics: dict[str, str] = {}
        self._prepare_locks: dict[str, asyncio.Lock] = {}
        with self.connect() as conn:
            conn.execute("CREATE TABLE IF NOT EXISTS packages (id TEXT PRIMARY KEY, value TEXT NOT NULL)")
            conn.execute("CREATE TABLE IF NOT EXISTS policy (id TEXT PRIMARY KEY, value TEXT NOT NULL)")
        self.db.chmod(0o600)
        for record in self.records():
            if record.get("scope_version") != 2:
                record.update(enabled=bool(record.get("engines")), engines=list(ENGINES), scope_version=2)
                self.save(record)
            if record.get("component_schema_version") != 4:
                inspected = self._inspect(Path(record["root"]))
                record.update({key: inspected[key] for key in ("components", "requirements", "declared_engines", "skills", "environment", "component_schema_version")})
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

    def enabled(self, engine: str) -> list[dict[str, Any]]:
        return [r for r in self.records() if r.get("enabled", True)
                and compatibility(r, check_tools=False)[engine]["status"] != "blocked"]

    def revision(self, engine: str) -> str:
        values = [(r["id"], r["digest"], r.get("hooks_approved_digest", "")) for r in self.enabled(engine)]
        native = provider_for(engine).revision() if engine in PROVIDERS else ""
        return sha256(json.dumps([8, values, self.control_enabled(engine), native]).encode()).hexdigest()[:16]

    def install(self, source: dict[str, Any]) -> dict[str, Any]:
        kind = source.get("kind", "local-dir")
        if kind not in {"local-dir", "archive", "git"}:
            raise ChatPluginError("支持本地目录、压缩包和固定 Git 版本")
        with tempfile.TemporaryDirectory(dir=self.root, prefix="import-") as work:
            workdir = Path(work)
            if kind == "local-dir":
                origin = Path(str(source.get("path", ""))).expanduser().resolve(strict=True)
                if origin == Path.home() or self.root.is_relative_to(origin):
                    raise ChatPluginError("请选择具体插件目录")
                package = workdir / "package"
                _copy_package(origin, package)
            else:
                installer = ExtensionInstaller(self.root / "packages")
                package, _ = installer._fetch(Source(
                    kind=kind, path=str(source.get("path", "")),
                    url=str(source.get("url", "")), ref=str(source.get("ref", "")),
                ), workdir)
            record = self._inspect(package)
            record.update(engines=list(ENGINES), enabled=True, scope_version=2)
            record["source_kind"] = kind
            record["digest"] = sha256_tree(package)
            target = self.root / "packages" / record["id"] / record["digest"]
            target.parent.mkdir(parents=True, exist_ok=True)
            if not target.exists():
                _copy_package(package, target)
            record["root"] = str(target)
            previous = self.get(record["id"])
            record["previous"] = ({k: v for k, v in previous.items() if k != "previous"} if previous else None)
            self.save(record)
        return self.public(record)

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
            if any(key in package for key in ("pi", "omp", "opencode")):
                manifest = package
                manifest["name"] = str(package.get("name") or root.name).lstrip("@").replace("/", "-")
        if not manifest and not skill_files:
            raise ChatPluginError("未找到插件清单或 SKILL.md")
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
        self._validate_mcp(mcp)
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
                    item["engines"] = ["claude"]
        for skill in skills:
            if skill.get("hooks"):
                config = skill["hooks"]
                if not isinstance(config, dict):
                    raise ChatPluginError("Skill hooks 必须为事件对象")
                components["components"].append({"kind": "skill_hooks", "name": skill["name"],
                    "config": {"hooks": config}, "engines": ["claude"], "delivery": "native_hooks"})
        if not skills and not mcp and not components["components"]:
            raise ChatPluginError("未找到可导入的 Skill 或 MCP；此插件可能依赖原客户端")
        unsupported = [key for key in ("hooks", "agents", "commands", "extensions") if manifest.get(key) or (root / key).is_dir()]
        return {"id": name, "name": name, "version": str(manifest.get("version") or "local"),
                "description": str(manifest.get("description") or (skills[0]["description"] if skills else "MCP")),
                "skills": skills, "mcp": mcp, "native_components": unsupported, "component_schema_version": 4, **components}

    @staticmethod
    def _validate_mcp(servers: Any) -> None:
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

    def add_mcp(self, name: str, servers: dict[str, Any]) -> dict[str, Any]:
        self._validate_mcp(servers)
        if not servers:
            raise ChatPluginError("请至少配置一个 MCP 服务")
        # Materialize the same package shape without exposing configuration in UI responses.
        with tempfile.TemporaryDirectory(dir=self.root) as temp:
            root = Path(temp)
            (root / "plugin.json").write_text(json.dumps({"name": name, "version": "local", "mcpServers": servers}))
            return self.install({"kind": "local-dir", "path": temp})

    def update(self, package_id: str, enabled: bool | None = None, rollback: bool = False,
               native_hooks: bool | None = None, digest: str = "") -> dict[str, Any]:
        record = self.get(package_id)
        if not record:
            raise ChatPluginError("插件不存在")
        if rollback:
            if not record.get("previous"):
                raise ChatPluginError("没有可回滚的版本")
            record = {**record["previous"], "engines": list(ENGINES), "enabled": record["enabled"], "scope_version": 2, "previous": None}
        if enabled is not None:
            record["enabled"] = enabled
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
        return {k: record.get(k) for k in ("id", "name", "version", "description", "enabled", "engines", "digest", "skills", "native_components")} | {
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
                if skill.get("requires_native") and engine != "claude":
                    continue
                if skill["invocable"] or include_automatic:
                    rows.append({"id": f"managed:{engine}:{record['id']}:{record['digest'][:12]}:{skill['name']}",
                                 "kind": "skill", "name": f"{record['id']}:{skill['name']}",
                                 "description": skill["description"], "source": "Muteki 聊天插件", "scope": "chat",
                                 "engine": engine, "_path": str(_contained(Path(record["root"]), skill["path"])),
                                 "_package_root": record["root"], "component_kind": skill.get("component_kind", "skill"),
                                 "native_engine": "claude" if skill.get("requires_native") else "",
                                 "native_name": f"muteki-{record['id']}:{skill['name']}" if skill.get("requires_native") else "",
                                 "_priority": 500})
        return rows

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
        return list(self._tools.get(engine + ":" + self.revision(engine), []))

    def visualization_root(self, thread_id: str) -> Path:
        root = self.root / "workspaces" / sha256(thread_id.encode()).hexdigest()[:24] / "visualizations"
        root.mkdir(parents=True, exist_ok=True)
        return root

    def visualization_context(self, thread_id: str) -> str:
        root = self.visualization_root(thread_id)
        return ("[Muteki 聊天展示接口]\n若当前任务与已启用 Skill 需要在聊天展示 HTML 交互图，"
                f"请将 HTML 片段保存到本次对话的可写目录 {root}，并在回复中单独一行输出 "
                'visualize{"path":"绝对路径.html"}。使用普通 HTML/CSS/JS，'
                '也支持纯文本标记 muteki-visualize {"path":"绝对路径.html"}（单独一行，跨引擎建议使用此格式）。'
                "不要使用网络 API。支持主题变量、基础样式类、Lucide、图形本地交互和 widgetState。"
                "支持 Tweak 设计控件、sendFollowUpMessage（用户确认后发送）、openExternal（用户确认后打开）。"
                "使用 Visualize Skill 时，会载入该插件自带的样式、日历、选项卡、提示与设计轮播。"
                "未使用交互图时忽略此接口。\n")

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
        if engine not in {"claude", "codex"}:
            return {}
        private = Path(env["MUTEKI_CHAT_PRIVATE_ROOT"])
        packages = []
        approved_hooks = {}
        for record in self.enabled(engine):
            hooks_allowed = record.get("hooks_approved_digest") == record["digest"]
            items = [c for c in record.get("components", []) if engine in c["engines"] and c["kind"] in {"hooks", "agents"}
                     and (c["kind"] != "hooks" or hooks_allowed)]
            if engine == "claude" and any(s.get("requires_native") for s in record.get("skills", [])):
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
                if engine == "claude":
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
                if engine == "claude":
                    native["skills"] = [p for p in native["skills"] if (target / p / "SKILL.md").is_file()]
                    native["commands"] = [p for p in native["commands"] if (target / p).is_file()]
                if engine == "claude" and any(c["kind"] == "agents" for c in items):
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
        if engine == "claude":
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

    def prepare_environment(self, engine: str, identity: str, supplied: dict[str, str]) -> dict[str, str]:
        """Import only this engine's native config into a private chat home.

        The plugin cache is copied once per engine, shared only among Muteki chat
        sessions. Credentials override the native snapshot, never the reverse.
        """
        if engine not in PROVIDERS:
            return supplied
        provider = provider_for(engine)
        root = self.root / "sessions" / sha256(f"v3:{engine}:{identity}:{self.revision(engine)}".encode()).hexdigest()[:24]
        home = root / "home"
        target = home / provider.home_relative
        source = provider.native_root()
        root.mkdir(parents=True, exist_ok=True)
        root.chmod(0o700)
        with self._lock:
            if not (root / ".ready").exists():
                target.mkdir(parents=True, exist_ok=True)
                names = ("config.toml", "settings.json", "settings.yaml", "settings.yml", "models.json", "models.yml",
                         "auth.json", ".credentials.json", "credentials.json", "mcp.json", "cli-config.json",
                         "agent-cli-state.json", "acp-config.json", "opencode.json", "opencode.jsonc",
                         "skills", "skills-cursor", "commands", "agents", "extensions", "prompts",
                         "credentials", "oauth", "device_id")
                def copy(src: Path, dest: Path) -> None:
                    if not src.exists():
                        return
                    if src.is_dir():
                        shutil.copytree(src, dest, copy_function=_snapshot_copy, dirs_exist_ok=True, symlinks=False, ignore_dangling_symlinks=True,
                                        ignore=shutil.ignore_patterns(".git", "logs", "__pycache__"))
                    elif src.is_file():
                        dest.parent.mkdir(parents=True, exist_ok=True)
                        shutil.copy2(src, dest)
                        dest.chmod(0o600)
                for name in names:
                    copy(source / name, target / name)
                if (source / "plugins").is_dir():
                    cache = self.root / "native-cache" / engine / provider.revision() / "plugins"
                    if not cache.exists():
                        staging = cache.with_name("plugins-staging")
                        copy(source / "plugins", staging)
                        staging.rename(cache)
                    if not (target / "plugins").exists():
                        # Registry paths and mutable plugin state are session-local.
                        # APFS clones share bytes, never directory identity or writes.
                        copy(cache, target / "plugins")
                # Common skills are shared by the native engines themselves.
                copy(Path.home() / ".agents" / "skills", home / ".agents" / "skills")
                if engine == "claude":
                    copy(Path.home() / ".claude.json", home / ".claude.json")
                if engine == "opencode":
                    copy(Path.home() / ".local/share/opencode/auth.json", root / "data/opencode/auth.json")
                credential_root = Path(supplied.get(provider.home_variable) or str(source)).expanduser()
                if engine == "opencode":
                    credential_root = credential_root / "opencode" if provider.home_variable in supplied else source
                if credential_root.resolve() != source.resolve():
                    for name in names:
                        copy(credential_root / name, target / name)
                # Registry/config paths into the imported engine home must point
                # at its Muteki copy. External user paths stay read-only inputs.
                for path in target.rglob("*"):
                    if path.is_file() and path.suffix in {".json", ".toml", ".yaml", ".yml", ".jsonc"} and path.stat().st_size < 2_000_000:
                        try:
                            value = path.read_text()
                            replaced = value.replace(str(source), str(target))
                            if replaced != value:
                                path.write_text(replaced)
                        except (OSError, UnicodeError):
                            pass
                for record in self.enabled(engine):
                    install_native_components(record, engine, target)
                (root / ".ready").touch()
            # Account material may be refreshed without changing its identity.
            # Re-copy only generated account configuration into the private home.
            account_root = supplied.get(provider.home_variable)
            if account_root:
                account_source = Path(account_root).expanduser()
                if engine == "opencode":
                    account_source = account_source / "opencode"
                if account_source.resolve() != source.resolve():
                    for name in ("auth.json", ".credentials.json", "config.toml", "models.json", "models.yml", "settings.json"):
                        src = account_source / name
                        if src.is_file():
                            shutil.copy2(src, target / name)
                            (target / name).chmod(0o600)
        env = dict(supplied)
        env.update({"HOME": str(home), "USERPROFILE": str(home), provider.home_variable: str(target),
                    "XDG_DATA_HOME": str(root / "data"), "XDG_CACHE_HOME": str(root / "cache"),
                    "XDG_CONFIG_HOME": str(home / ".config"), "MUTEKI_CHAT_PRIVATE_ROOT": str(root)})
        if engine == "cursor":
            env["CURSOR_DATA_DIR"] = str(root / "data")
            # Cursor persists even explicitly supplied API keys/tokens to the
            # macOS keychain by default. A private chat home has no login
            # keychain, so this can open a system dialog on every startup.
            # Managed credentials are re-injected on each launch; keep them
            # only in memory. Preserve native login when no credential is given.
            if any(env.get(key, os.environ.get(key, "")).strip()
                   for key in ("CURSOR_API_KEY", "CURSOR_AUTH_TOKEN")):
                env["AGENT_CLI_CREDENTIAL_STORE"] = "memory"
        if engine == "opencode":
            env["OPENCODE_CONFIG_DIR"] = str(target)
        return env

    async def prepare_tools(self, engine: str) -> list[dict[str, Any]]:
        async with self._prepare_locks.setdefault(engine, asyncio.Lock()):
            return await self._prepare_tools(engine)

    async def _prepare_tools(self, engine: str) -> list[dict[str, Any]]:
        key = engine + ":" + self.revision(engine)
        if key in self._tools:
            return self._tools[key]
        tools = []
        for record in self.enabled(engine):
            for server, config in record["mcp"].items():
                if config.get("disabled"):
                    continue
                label = re.sub(r"[^a-zA-Z0-9_]", "_", record["id"])[:8]
                prefix = "chat_" + label + "_" + sha256(f"{engine}:{record['id']}:{record['digest']}:{server}".encode()).hexdigest()[:8] + "_"
                try:
                    worker = self.worker(engine, record, server, config)
                    await worker.wait_ready()
                    result = await worker.request("list_tools", {}) if "tools" in worker.capabilities else None
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
                    self._diagnostics.pop(record["id"], None)
                except Exception:
                    # Raw transport errors can include env/header credentials.
                    self._diagnostics[record["id"]] = f"MCP {server} 连接失败，请检查配置与运行环境"
        self._tools[key] = tools
        self._verified_tools[engine] = {name for tool in tools for name in
            (tool["name"], tool["_tool"], f"mcp__{tool['_server']}__{tool['_tool']}", f"{tool['_server']}.{tool['_tool']}")}
        return tools

    async def invalidate(self) -> None:
        self._tools.clear()
        self._verified_tools.clear()
        active = {f"{r['id']}:{r['digest']}:{server}"
                  for r in self.records() if r.get("enabled", True) for server in r["mcp"]}
        removed = [self._workers.pop(key) for key in list(self._workers) if key not in active]
        for worker in removed:
            worker.task.cancel()
        await asyncio.gather(*(w.task for w in removed), return_exceptions=True)
        self._diagnostics.clear()

    def worker(self, engine: str, record: dict[str, Any], server: str, config: dict[str, Any]) -> McpWorker:
        # Managed installations are global to Muteki chat. Reuse one connection
        # and private data directory; engine-scoped descriptors still gate calls.
        key = f"{record['id']}:{record['digest']}:{server}"
        worker = self._workers.get(key)
        if worker is None or worker.task.done():
            state = self.root / "mcp-state" / sha256(key.encode()).hexdigest()[:24]
            worker = McpWorker(config, Path(record["root"]), state)
            self._workers[key] = worker
        return worker

    async def invoke(self, engine: str, name: str, arguments: dict[str, Any]) -> Any:
        tool = next((t for t in await self.prepare_tools(engine) if t["name"] == name), None)
        if not tool:
            raise ChatPluginError("当前 Agent 未启用该工具，或插件已停用")
        record = self.get(tool["_package"])
        if not record or not record.get("enabled", True) or engine not in record["engines"] or record["digest"] != tool["_digest"]:
            raise ChatPluginError("插件版本已变化，请重新加载聊天能力")
        result = await self.worker(engine, record, tool["_server"], record["mcp"][tool["_server"]]).request(
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
            raise ChatPluginError("MCP 启动超时") from None

    async def request(self, method: str, params: dict[str, Any]) -> Any:
        await self.wait_ready()
        future = asyncio.get_running_loop().create_future()
        await self.queue.put((method, params, future))
        return await asyncio.wait_for(future, 90)

    async def run(self, config: dict[str, Any], package: Path, state: Path) -> None:
        from mcp import ClientSession, StdioServerParameters
        from mcp.client.stdio import stdio_client
        from mcp.client.sse import sse_client
        from mcp.client.streamable_http import streamablehttp_client
        current = None
        try:
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
                    command = isolated_mcp_command(argv, package, state, env, bool(config.get("network")))
                    errlog = stack.enter_context(open(os.devnull, "w"))
                    read, write = await stack.enter_async_context(stdio_client(StdioServerParameters(
                        command=command[0], args=command[1:], env=env, cwd=str(state)), errlog=errlog))
                else:
                    if str(config.get("type") or config.get("transport") or "").lower() == "sse":
                        read, write = await stack.enter_async_context(sse_client(config["url"], headers=config.get("headers")))
                    else:
                        read, write, _ = await stack.enter_async_context(streamablehttp_client(config["url"], headers=config.get("headers")))
                session = await stack.enter_async_context(ClientSession(read, write))
                initialized = await session.initialize()
                self.capabilities = initialized.capabilities.model_dump(exclude_none=True)
                self.ready.set_result(True)
                while True:
                    method, params, current = await self.queue.get()
                    if current.cancelled():
                        continue
                    try:
                        value = await getattr(session, method)(**params)
                        if not current.done():
                            current.set_result(value)
                    except Exception:
                        if not current.done():
                            current.set_exception(ChatPluginError("MCP 工具请求失败"))
        except BaseException:
            if not self.ready.done():
                self.ready.set_exception(ChatPluginError("MCP 服务启动失败"))
        finally:
            if current is not None and not current.done():
                current.set_exception(ChatPluginError("MCP 服务已停止"))
            while not self.queue.empty():
                _, _, future = self.queue.get_nowait()
                if not future.done():
                    future.set_exception(ChatPluginError("MCP 服务已停止"))
