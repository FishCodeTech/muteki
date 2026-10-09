"""Package components, explicit requirements and private native installation.

Portable instructions are shared; executable extensions keep their native ABI.
All generated files belong to the chat environment, never the operator home.
"""
from __future__ import annotations

import json
from hashlib import sha256
from pathlib import Path
import re
import shutil
from typing import Any

import yaml

from muteki.external_agents.descriptors import engines_where, get_descriptor
from muteki.solver.engine_registry import SUPPORTED_ENGINE_IDS
from .chat_providers import PROVIDERS

ENGINES = SUPPORTED_ENGINE_IDS
# Native component ABIs declared by the provider descriptors.
_NATIVE_SKILL_ENGINES = engines_where(lambda d: d.components.native_skill_semantics)
_PLUGIN_AGENT_ENGINES = engines_where(lambda d: d.components.native_agents == "plugin_package")
_NATIVE_AGENT_ENGINES = engines_where(lambda d: d.components.native_agents != "none")
_HOOK_ENGINES = engines_where(lambda d: d.components.native_hooks == "all")
_COMMAND_HOOK_ENGINES = engines_where(lambda d: d.components.native_hooks != "none")
_EXTENSION_ENGINES = engines_where(lambda d: d.components.extension_abi)


def resource(root: Path, relative: str) -> Path:
    path = (root / relative).resolve()
    if not path.is_relative_to(root.resolve()):
        raise ValueError("插件组件路径必须位于插件包内")
    return path


def frontmatter(path: Path) -> tuple[dict, str]:
    text = path.read_text(encoding="utf-8")
    if len(text.encode()) > 256000:
        raise ValueError("插件指令超过 256 KB")
    if text.startswith("---") and text.count("---") >= 2:
        _, raw, body = text.split("---", 2)
        meta = yaml.safe_load(raw)
        return meta if isinstance(meta, dict) else {}, body
    return {}, text


def inspect_components(root: Path, manifest: dict) -> dict[str, Any]:
    root = root.resolve()
    namespaces = manifest.get("extensions")
    native_meta = namespaces.get("io.github.fishcodetech.muteki") if isinstance(namespaces, dict) else None
    meta = native_meta if native_meta is not None else manifest.get("muteki") or {}
    if not isinstance(meta, dict):
        raise ValueError("muteki 插件元数据必须为对象")
    declared_engines = meta.get("engines", list(ENGINES))
    if not isinstance(declared_engines, list) or any(e not in ENGINES for e in declared_engines):
        raise ValueError("插件引擎声明无效")
    allowed_modes = meta.get("modes", ["chat", "ctf", "pentest"])
    if (not isinstance(allowed_modes, list) or not allowed_modes
            or any(not isinstance(mode, str) or mode not in {"chat", "ctf", "pentest"} for mode in allowed_modes)):
        raise ValueError("插件使用场景声明无效")
    components = []
    # Markdown commands are prompt templates, not arbitrary executable code.
    for kind in ("commands", "agents"):
        paths = manifest.get(kind, kind)
        if kind == "commands" and isinstance(paths, dict):
            expanded = []
            for name, entry in paths.items():
                if not re.fullmatch(r"[\w.-]{1,100}", name) or not isinstance(entry, dict):
                    raise ValueError("命令模板声明无效")
                if ("content" in entry) == ("source" in entry):
                    raise ValueError("命令模板必须且只能声明 content 或 source")
                if "source" in entry:
                    config, body = frontmatter(resource(root, entry["source"]))
                else:
                    config, body = {}, str(entry["content"])
                config.update(name=name, description=entry.get("description", config.get("description", name)))
                file = resource(root, ".muteki-commands/" + name + ".md")
                file.parent.mkdir(exist_ok=True)
                file.write_text("---\n" + yaml.safe_dump(config, allow_unicode=True) + "---\n" + body)
                expanded.append(str(file.relative_to(root)))
            paths = expanded
        paths = paths if isinstance(paths, list) else [paths]
        for relative in paths:
            if not isinstance(relative, str):
                continue
            location = resource(root, relative)
            files = sorted(location.rglob("*.md")) if location.is_dir() else [location] if location.is_file() else []
            for file in files:
                config, body = frontmatter(file)
                name = str(config.get("name") or file.stem)
                if not re.fullmatch(r"[\w.-]{1,100}", name):
                    raise ValueError("插件组件名称无效")
                components.append({"kind": kind, "name": name, "path": str(file.relative_to(root)),
                    "description": str(config.get("description") or name), "metadata": config,
                    "engines": list(ENGINES) if kind == "commands" else list(_PLUGIN_AGENT_ENGINES),
                    "delivery": "prompt_template" if kind == "commands" else "native_agent"})
    # Each native agent path belongs only to its explicitly declared engine.
    for engine in _NATIVE_AGENT_ENGINES:
        spec = manifest.get(engine)
        if not isinstance(spec, dict) or "agents" not in spec:
            continue
        paths = spec["agents"]
        if not isinstance(paths, list) or not all(isinstance(p, str) for p in paths):
            raise ValueError(f"{engine}.agents 必须为包内路径数组")
        for relative in paths:
            location = resource(root, relative)
            files = sorted(location.rglob("*.md")) if location.is_dir() else [location] if location.is_file() else []
            if not files:
                raise ValueError(f"{engine}.agents 入口不存在")
            for file in files:
                config, _ = frontmatter(file)
                name = str(config.get("name") or file.stem)
                if not re.fullmatch(r"[\w.-]{1,100}", name):
                    raise ValueError("插件组件名称无效")
                components.append({"kind": "agents", "name": name, "path": str(file.relative_to(root)),
                    "description": str(config.get("description") or name), "metadata": config,
                    "engines": [engine], "delivery": "native_agent"})
    hooks = manifest.get("hooks") or ("hooks/hooks.json" if (root / "hooks/hooks.json").is_file() else None)
    if hooks:
        merged = {}
        for value in hooks if isinstance(hooks, list) else [hooks]:
            if isinstance(value, str):
                value = json.loads(resource(root, value).read_text())
            if not isinstance(value, dict):
                raise ValueError("hooks 必须为 JSON 对象或包内路径")
            for event, entries in value.get("hooks", value).items():
                if not isinstance(entries, list): raise ValueError("Hook 事件必须为数组")
                merged.setdefault(event, []).extend(entries)
        hooks = {"hooks": merged}
        commands_only = all(h.get("type") == "command" for entries in merged.values() for row in entries for h in row.get("hooks", []))
        components.append({"kind": "hooks", "name": "hooks", "config": hooks,
                           "engines": list(_COMMAND_HOOK_ENGINES if commands_only else _HOOK_ENGINES),
                           "delivery": "native_hooks"})
    extensions = meta.get("extensions") or {}
    if not isinstance(extensions, dict):
        raise ValueError("muteki.extensions 必须按引擎声明入口")
    extensions = dict(extensions)
    for engine in _EXTENSION_ENGINES:
        package_spec = manifest.get(engine)
        if isinstance(package_spec, dict) and isinstance(package_spec.get("extensions"), list):
            dependencies = {**(manifest.get("dependencies") if isinstance(manifest.get("dependencies"), dict) else {}),
                            **(manifest.get("devDependencies") if isinstance(manifest.get("devDependencies"), dict) else {})}
            native_engine = "omp" if engine == "pi" and "@oh-my-pi/pi-coding-agent" in dependencies else engine
            extensions.setdefault(native_engine, package_spec["extensions"])
    if not extensions and (root / "extensions").is_dir():
        # No inference between Pi, OMP and OpenCode executable plugin ABIs.
        candidates = [e for e in declared_engines if e in _EXTENSION_ENGINES]
        if len(candidates) == 1:
            extensions = {candidates[0]: [str(p.relative_to(root)) for p in (root / "extensions").glob("*")
                                          if p.suffix in {".js", ".ts", ".mjs"}]}
        else:
            components.append({"kind": "extensions", "name": "extensions", "engines": [],
                               "reason": "请在插件 muteki.extensions 中声明对应引擎和入口；不会猜测执行 ABI"})
    if not isinstance(extensions, dict):
        raise ValueError("muteki.extensions 必须按引擎声明入口")
    for engine, paths in extensions.items():
        if engine not in _EXTENSION_ENGINES or not isinstance(paths, list) or not all(isinstance(p, str) for p in paths):
            raise ValueError("可执行扩展支持 Pi、OMP、OpenCode 的原生入口")
        for path in paths:
            file = resource(root, path)
            if file.is_dir():
                file = next((file / name for name in ("index.ts", "index.js", "index.mjs") if (file / name).is_file()), file)
            if not file.is_file() or file.suffix not in {".js", ".ts", ".mjs"}:
                raise ValueError("可执行扩展入口不存在")
            components.append({"kind": "extensions", "name": file.stem if file.stem != "index" else file.parent.name, "path": str(file.relative_to(root)),
                               "engines": [engine], "delivery": "native_extension"})
    requirements = meta.get("requires", {})
    if not isinstance(requirements, dict):
        raise ValueError("requires 必须为对象")
    for key in ("executables", "env", "files", "tools"):
        if key in requirements and (not isinstance(requirements[key], list) or
                                   not all(isinstance(v, str) for v in requirements[key])):
            raise ValueError(f"requires.{key} 必须为字符串数组")
    environment = meta.get("env", {})
    if not isinstance(environment, dict) or not all(isinstance(k, str) and isinstance(v, str) for k, v in environment.items()):
        raise ValueError("插件环境变量必须为字符串映射")
    return {"components": components, "requirements": requirements, "declared_engines": declared_engines,
            "environment": environment, "allowed_modes": list(dict.fromkeys(allowed_modes))}


def compatibility(record: dict, verified_tools: dict[str, set[str]] | None = None, *, check_tools: bool = True) -> dict[str, Any]:
    requirements = record.get("requirements", {})
    missing = [f"缺少程序 {name}" for name in requirements.get("executables", []) if not shutil.which(name)]
    configured_env = dict(record.get("environment", {}))
    for config in record.get("mcp", {}).values(): configured_env.update(config.get("env", {}))
    missing += [f"插件未配置环境变量 {name}" for name in requirements.get("env", []) if not configured_env.get(name)]
    root = Path(record.get("root", "."))
    missing += [f"缺少包内文件 {name}" for name in requirements.get("files", []) if not resource(root, name).is_file()]
    # Tool dependencies must be checked against the session, never guessed from prose.
    tools = requirements.get("tools", [])
    result = {}
    for engine in ENGINES:
        reasons = list(missing)
        if engine not in record.get("declared_engines", ENGINES):
            reasons.append("插件未声明支持此引擎")
        if check_tools:
            absent = sorted(set(tools) - (verified_tools or {}).get(engine, set()))
            if absent:
                reasons.append("当前会话尚未确认所需工具：" + "、".join(absent))
        available = []
        unavailable = []
        if any((not skill.get("requires_native") or engine in _NATIVE_SKILL_ENGINES) and
               (not skill.get("hooks") or record.get("hooks_approved_digest") == record.get("digest"))
               for skill in record.get("skills", [])):
            available.append("skills")
        if engine not in _NATIVE_SKILL_ENGINES and any(skill.get("requires_native") for skill in record.get("skills", [])):
            unavailable.append("Claude 专属 Skill 语义")
        if record.get("mcp"):
            if engine in PROVIDERS and PROVIDERS[engine].gateway_tools:
                available.append("mcp")
            else:
                unavailable.append("聊天 Gateway 工具尚未接入")
        for component in record.get("components", []):
            needs_approval = component["kind"] in {"hooks", "skill_hooks"} or component.get("metadata", {}).get("hooks")
            approved = not needs_approval or record.get("hooks_approved_digest") == record.get("digest")
            (available if engine in component["engines"] and approved else unavailable).append(component["kind"])
        for server, config in record.get("mcp", {}).items():
            command = str(config.get("command") or "")
            if command and "${" not in command and not shutil.which(command):
                reasons.append(f"MCP {server} 缺少启动程序 {command}")
        result[engine] = {"status": "blocked" if reasons or not available else "partial" if unavailable or tools else "supported",
                          "components": sorted(set(available)) if not reasons else [],
                          "unavailable": sorted(set(unavailable)), "reasons": reasons,
                          "required_tools": tools}
    return result


def install_native_components(record: dict, engine: str, home: Path) -> None:
    """Materialize supported native components into the supplied staging home."""
    root = Path(record["root"])
    package = str(record["id"])
    components = get_descriptor(engine).components
    for item in record.get("components", []):
        if engine not in item["engines"]:
            continue
        if item["kind"] == "agents" and components.native_agents == "home_agents_dir":
            target = home / "agents" / f"{package}-{item['name']}.md"
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(resource(root, item["path"]), target)
        elif item["kind"] == "extensions":
            # Preserve relative imports inside an immutable package directory.
            directory = home / components.extension_dir
            directory.mkdir(parents=True, exist_ok=True)
            entry = resource(root, item["path"])
            suffix = sha256(item["path"].encode()).hexdigest()[:8]
            target = directory / f"muteki-{package}-{item['name']}-{suffix}.js"
            target.write_text(f"export * from {json.dumps(entry.as_uri())};\n" if components.extension_export == "star"
                              else f"export {{default}} from {json.dumps(entry.as_uri())};\n")
