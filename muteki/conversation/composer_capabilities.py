"""Conversation composer capability discovery and explicit reference resolution.

The composer exposes a small, engine-scoped view of capabilities:

* ``/`` lists commands and skills published by the active Runtime session;
* ``@`` lists conversations, files, plugins, and Muteki-provided components;
* ``$`` lists only the selected engine's skills.

Client payloads contain opaque IDs only.  Every selected reference is resolved
again against the current engine and workspace before it reaches an Agent.
"""

from __future__ import annotations

from hashlib import sha256
import os
import re
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Iterable

import yaml

from muteki.capability_management import enabled as capability_enabled
from muteki.capability_bindings.agent_plugin import package_root
from muteki.solver.worker_skills import project_skill_roots

from muteki.external_agents.descriptors import engine_ids, find_descriptor
from muteki.external_agents.runtime_capabilities import RuntimeCapabilitySnapshot


SUPPORTED_ENGINES = frozenset(engine_ids())

_MUTEKI_CONTROL_UNSUPPORTED_REASON = (
    "当前 Runtime 未接入 Muteki 能力 Gateway，不会注入 muteki-control"
)
_MUTEKI_CONTROL_UNSUPPORTED_ALTERNATIVE = (
    "请改用已接入 Gateway 的 Agent（如 Claude / Codex / Pi），"
    "或使用工作区内文件与该 Runtime 的原生本地工具"
)

_COMMANDS: tuple[dict[str, str], ...] = (
    {"name": "new", "description": "新建对话", "action": "new"},
    {"name": "clear", "description": "开始空白聊天，保留历史记录", "action": "new"},
    {"name": "clear-input", "description": "清空输入内容与引用", "action": "clear"},
)

_IGNORED_FILE_DIRS = frozenset({
    ".git", ".hg", ".svn", ".next", ".turbo", ".cache", ".pytest_cache",
    ".mypy_cache", ".ruff_cache", "node_modules", "dist", "build", "coverage",
    "__pycache__", ".venv", "venv", "sessions",
})

_MAX_SKILL_FILE_BYTES = 256_000
_MAX_CONTEXT_CHARS = 180_000
_MAX_SELECTED_REFS = 12
_MAX_THREAD_CONTEXT_CHARS = 24_000
_MAX_SNAPSHOT_CHARS = 4_000
_CONTEXT_SCHEMA_V2 = 2
# Strip chips (skill/MCP/plugin/thread/command) keep catalog identity even when
# the composer wires them as schema-v2 nodes. Resolve via the engine catalog,
# not the structured soft-stale path that treats them as unknown kinds (#118).
_STRIP_CATALOG_KINDS = frozenset({"skill", "mcp", "plugin", "command", "thread"})


class ComposerCapabilityError(ValueError):
    """A composer reference is stale, unavailable, or outside its scope."""


def _section_failure(errors: list[dict[str, Any]] | None, section: str, exc: Exception) -> None:
    if errors is not None:
        errors.append({"section": section, "code": "conversation.composer.section_failed",
                       "message": str(exc), "exception_type": type(exc).__name__})


def engine_receives_capability_gateway(engine: str) -> bool:
    """True when the selected engine's adapter may inject Muteki control tools."""
    descriptor = find_descriptor(engine)
    return descriptor is not None and descriptor.capability_gateway


def _muteki_control_mcp_row(*, engine: str, injected: bool) -> dict[str, Any]:
    """Composer @-menu row for muteki-control; ``injected`` matches real delivery."""
    if injected:
        return {
            "id": "mcp:muteki-control",
            "kind": "mcp",
            "name": "muteki-control",
            "description": "当前会话由 Muteki 注入的任务与运行控制工具",
            "source": "Muteki Agent Plugin",
            "scope": "thread",
            "engine": engine,
            "channel": "session_config",
            "delivery": "guaranteed",
            "verification": "verified",
            "status": "available",
            "support_level": "supported",
            "reason": "",
            "alternative": "",
            "invocable": True,
            "action": "select-capability",
        }
    return {
        "id": "mcp:muteki-control",
        "kind": "mcp",
        "name": "muteki-control",
        "description": "未向当前 Runtime 注入的任务与运行控制工具",
        "source": "Muteki Agent Plugin",
        "scope": "thread",
        "engine": engine,
        "channel": "session_config",
        "delivery": "local",
        "verification": "verified",
        "status": "unavailable",
        "support_level": "unsupported",
        "reason": _MUTEKI_CONTROL_UNSUPPORTED_REASON,
        "alternative": _MUTEKI_CONTROL_UNSUPPORTED_ALTERNATIVE,
        "invocable": False,
        "action": "inspect-runtime-capability",
    }


def _opaque_id(kind: str, identity: str) -> str:
    digest = sha256(identity.encode("utf-8")).hexdigest()[:24]
    return f"{kind}:{digest}"


def _frontmatter(path: Path, errors: list[dict[str, Any]] | None = None) -> tuple[dict[str, Any], str]:
    try:
        raw = path.read_text(encoding="utf-8")
    except (OSError, UnicodeError) as exc:
        _section_failure(errors, "skills", exc)
        return {}, ""
    if not raw.startswith("---"):
        return {}, raw
    lines = raw.splitlines()
    end = next((index for index in range(1, len(lines)) if lines[index].strip() == "---"), -1)
    if end < 0:
        return {}, raw
    try:
        parsed = yaml.safe_load("\n".join(lines[1:end])) or {}
    except yaml.YAMLError as exc:
        _section_failure(errors, "skills", exc)
        parsed = {}
    return (parsed if isinstance(parsed, dict) else {}), "\n".join(lines[end + 1:])


def _skill_item(path: Path, *, engine: str, source: str, scope: str, priority: int,
                errors: list[dict[str, Any]] | None = None) -> dict[str, Any] | None:
    try:
        if not path.is_file() or path.stat().st_size > _MAX_SKILL_FILE_BYTES:
            return None
        resolved = path.resolve()
    except OSError as exc:
        _section_failure(errors, "skills", exc)
        return None
    error_count = len(errors) if errors is not None else 0
    meta, body = _frontmatter(resolved, errors)
    if errors is not None and len(errors) > error_count:
        return None
    if meta.get("enabled") is False or meta.get("user-invocable") is False or meta.get("user_invocable") is False:
        return None
    name = str(meta.get("name") or resolved.parent.name).strip()
    if not name:
        return None
    description = str(meta.get("description") or "").strip()
    if not description:
        first_line = next((line.strip(" #\t") for line in body.splitlines() if line.strip()), "")
        description = first_line[:180] or "Agent Skill"
    return {
        "id": _opaque_id("skill", f"{engine}\0{resolved}"),
        "kind": "skill",
        "name": name,
        "description": description[:240],
        "source": source,
        "scope": scope,
        "engine": engine,
        "_path": str(resolved),
        "_priority": priority,
    }


def _skill_roots(engine: str, workspace_root: str = "") -> Iterable[tuple[Path, str, str, int]]:
    if workspace_root:
        workspace = Path(workspace_root).expanduser().resolve()
        for relative in project_skill_roots(engine):
            common = relative == ".agents/skills"
            yield workspace / relative, "当前项目", "project", 350 if common else 400
    if os.environ.get("MUTEKI_HOST_DISCOVERY", "1") != "0":
        home = Path.home()
        descriptor = find_descriptor(engine)
        roots = descriptor.environment.user_skill_roots if descriptor is not None else ()
        for relative in roots:
            common = relative == ".agents/skills"
            yield home / relative, "通用 Agent" if common else engine, "personal", 250 if common else 300


def discover_skills(engine: str, workspace_root: str = "", *,
                    errors: list[dict[str, Any]] | None = None) -> list[dict[str, Any]]:
    """Return user-invocable skills visible to one selected engine."""
    normalized = str(engine or "").strip().lower()
    if normalized not in SUPPORTED_ENGINES:
        return []
    deduped: dict[str, dict[str, Any]] = {}
    visited: set[Path] = set()
    for root, source, scope, priority in _skill_roots(normalized, workspace_root):
        try:
            resolved_root = root.resolve()
        except OSError as exc:
            _section_failure(errors, "skills", exc)
            continue
        if resolved_root in visited or not resolved_root.is_dir():
            continue
        visited.add(resolved_root)
        try:
            manifests = sorted(resolved_root.rglob("SKILL.md"))[:800]
        except OSError as exc:
            _section_failure(errors, "skills", exc)
            continue
        for manifest in manifests:
            item = _skill_item(
                manifest, engine=normalized, source=source, scope=scope, priority=priority,
                errors=errors,
            )
            if item is None:
                continue
            key = item["name"].casefold()
            previous = deduped.get(key)
            if previous is None or item["_priority"] > previous["_priority"]:
                deduped[key] = item

    # Native plugin manifests belong to their own engine provider.
    from muteki.conversation.chat_providers import PROVIDERS
    provider = PROVIDERS.get(normalized)
    if provider is not None and os.environ.get("MUTEKI_HOST_DISCOVERY", "1") != "0":
        for package_name, root in provider.plugin_skill_roots():
            for path in sorted((root / "skills").rglob("SKILL.md"))[:200]:
                item = _skill_item(path, engine=normalized, source=f"{normalized} 插件", scope="personal", priority=290, errors=errors)
                if item is not None:
                    item["name"] = f"{package_name}:{item['name']}"
                    deduped.setdefault(item["name"].casefold(), item)

    # Muteki Control 是随产品交付的 Agent Plugins 1.0.0 包。仅对会实际
    # 注入 Gateway / plugin 的 Runtime 作为可调用 Skill 展示；未接入引擎
    # 改由 resolve_composer_catalog 给出 unsupported 说明（#120）。
    if engine_receives_capability_gateway(normalized):
        plugin_skill = _skill_item(
            package_root() / "skills" / "muteki-control" / "SKILL.md",
            engine=normalized,
            source="Muteki Agent Plugin",
            scope="thread",
            priority=600,
            errors=errors,
        )
        if plugin_skill is not None:
            deduped[plugin_skill["name"].casefold()] = plugin_skill

    return sorted(deduped.values(), key=lambda item: (-int(item["_priority"]), item["name"].casefold()))


def discover_files(
    workspace_root: str,
    *,
    query: str = "",
    limit: int = 80,
    errors: list[dict[str, Any]] | None = None,
) -> list[dict[str, Any]]:
    if not workspace_root:
        return []
    try:
        root = Path(workspace_root).expanduser().resolve(strict=True)
    except (OSError, RuntimeError) as exc:
        _section_failure(errors, "files", exc)
        return []
    if not root.is_dir():
        return []
    rows: list[dict[str, Any]] = []
    needle = str(query or "").casefold().strip()
    for directory, dirnames, filenames in os.walk(root, followlinks=False,
            onerror=lambda exc: _section_failure(errors, "files", exc)):
        directory_path = Path(directory)
        dirnames[:] = sorted(
            name for name in dirnames
            if name not in _IGNORED_FILE_DIRS
            and not (directory_path / name).is_symlink()
        )
        for filename in sorted(filenames):
            path = directory_path / filename
            try:
                if path.is_symlink() or not path.is_file():
                    continue
                relative = path.relative_to(root).as_posix()
            except (OSError, ValueError):
                continue
            if needle and needle not in relative.casefold():
                continue
            rows.append({
                "id": _opaque_id("file", f"{root}\0{relative}"),
                "kind": "file",
                "name": path.name,
                "description": relative,
                "source": "当前项目",
                "scope": "project",
                "_relative_path": relative,
            })
            if len(rows) >= limit:
                return rows
    return rows


def _extension_items(extension_service: Any) -> list[dict[str, Any]]:
    if extension_service is None:
        return []
    rows: list[dict[str, Any]] = []
    for record in extension_service.list_records():
        if not record.enabled or str(record.state.value) != "ready":
            continue
        try:
            manifest = extension_service.manifest_of(record.extension_id)
        except Exception:
            continue
        provided = ", ".join(item.id for item in manifest.provides if item.id)
        rows.append({
            "id": f"plugin:{record.extension_id}",
            "kind": "plugin",
            "name": record.extension_id,
            "description": f"已启用扩展{f' · {provided}' if provided else ''}",
            "source": "Muteki",
            "scope": "platform",
        })
    return rows


def _thread_items(
    threads: Iterable[Any], *, current_thread_id: str = "",
) -> list[dict[str, Any]]:
    rows: list[dict[str, Any]] = []
    ordered = sorted(
        threads,
        key=lambda item: str(getattr(item, "updated_at", "")),
        reverse=True,
    )
    for thread in ordered:
        thread_id = str(getattr(thread, "thread_id", "") or "")
        if not thread_id or thread_id == current_thread_id:
            continue
        title = str(getattr(thread, "title", "") or "新对话").strip() or "新对话"
        summary = str(getattr(thread, "summary", "") or "").strip()
        rows.append({
            "id": _opaque_id("thread", thread_id),
            "kind": "thread",
            "name": title,
            "description": summary[:240] or "引用这段对话的上下文",
            "source": "对话",
            "scope": "conversation",
            "_thread_id": thread_id,
        })
        if len(rows) >= 12:
            break
    return rows


def _public_item(item: dict[str, Any]) -> dict[str, Any]:
    return {key: value for key, value in item.items() if not key.startswith("_")}


def _matches(item: dict[str, Any], query: str) -> bool:
    needle = query.casefold().strip()
    if not needle:
        return True
    return needle in " ".join((
        str(item.get("name") or ""), str(item.get("description") or ""),
        str(item.get("source") or ""), str(item.get("scope") or ""),
    )).casefold()


def resolve_composer_catalog(
    *,
    engine: str,
    workspace_root: str = "",
    trigger: str,
    query: str = "",
    extension_service: Any = None,
    threads: Iterable[Any] = (),
    current_thread_id: str = "",
    runtime_snapshot: RuntimeCapabilitySnapshot | None = None,
    plugin_service: Any = None,
    section_errors: list[dict[str, Any]] | None = None,
) -> list[dict[str, Any]]:
    def read_section(section: str, loader: Any, default: Any):
        try:
            return loader()
        except Exception as exc:
            if section_errors is None:
                raise
            section_errors.append({"section": section,
                "code": "conversation.composer.section_failed", "message": str(exc),
                "exception_type": type(exc).__name__})
            return default

    normalized = str(engine or "").strip().lower()
    if normalized not in SUPPORTED_ENGINES:
        return []
    raw_query = str(query or "").strip()
    kind_filter = ""
    if ":" in raw_query:
        prefix, suffix = raw_query.split(":", 1)
        if prefix in {"skill", "mcp", "plugin", "file", "thread"}:
            kind_filter, raw_query = prefix, suffix

    snapshot_items = [item for item in runtime_snapshot.items if not item.engine or item.engine == normalized] if runtime_snapshot else []
    local_skills = {
        str(item.get("name") or "").casefold(): item
        for item in read_section("skills", lambda: discover_skills(normalized, workspace_root, errors=section_errors), [])
    }

    managed_skills = read_section("managed_skills", lambda: plugin_service.skill_rows(normalized), []) if plugin_service is not None else []
    control_enabled = read_section("plugins", lambda: plugin_service.control_enabled(normalized), False) if plugin_service is not None else True
    if plugin_service is not None:
        if not control_enabled:
            local_skills = {k: v for k, v in local_skills.items() if v.get("source") != "Muteki Agent Plugin"}

    runtime_rows: list[dict[str, Any]] = []
    snapshot_stale = bool(runtime_snapshot and runtime_snapshot.stale)
    for capability in snapshot_items:
        if capability.engine and capability.engine != normalized:
            continue
        if capability.kind not in {"command", "operation", "skill"}:
            continue
        if capability.status not in {
            "", "available", "runtime_reported", "requested", "unavailable",
        }:
            continue
        runtime_name = capability.name
        skill_key = runtime_name.removeprefix("skill:").casefold()
        local_skill = local_skills.get(skill_key)
        public_kind = (
            "skill" if capability.kind == "skill" or local_skill is not None
            else "command"
        )
        display_name = (
            str(local_skill.get("name"))
            if public_kind == "skill" and local_skill is not None
            else runtime_name
        )
        wire_text = str(
            capability.invocation.get("wire_text") or f"/{runtime_name}"
        )
        if wire_text.startswith("/") and runtime_name.casefold() in {
            item["name"] for item in _COMMANDS
        }:
            # Muteki 本地命令拥有固定优先级；同名 Runtime 命令不进入菜单。
            continue
        verified = capability.verification == "verified"
        if snapshot_stale:
            support_level = "expired"
            invocable = False
            reason = capability.reason or "能力目录已过期，正在刷新"
            alternative = (
                capability.alternative
                or "请等待刷新完成后再调用该命令"
            )
        elif not verified:
            support_level = getattr(capability, "support_level", None) or "unknown"
            if support_level == "supported":
                support_level = "unknown"
            invocable = False
            reason = (
                capability.reason
                or "该项尚未通过 Runtime 验证，不能当作可调用能力"
            )
            alternative = (
                capability.alternative
                or "请改用已验证的命令，或等待 Runtime 公布确认结果"
            )
        else:
            support_level = getattr(capability, "support_level", None) or "supported"
            invocable = bool(getattr(capability, "invocable", True))
            reason = capability.reason or ""
            alternative = capability.alternative or ""
        runtime_rows.append({
            "id": capability.id,
            "kind": public_kind,
            "name": display_name,
            "description": (
                capability.description
                or (str(local_skill.get("description")) if local_skill else "")
                or "Runtime 当前公布的能力"
            ),
            "source": capability.source or runtime_snapshot.adapter_id,
            "scope": capability.scope,
            "engine": capability.engine or normalized,
            "action": (
                "insert-runtime-invocation" if invocable
                else "inspect-runtime-capability"
            ),
            "channel": capability.channel,
            "origin": capability.origin,
            "delivery": capability.delivery,
            "verification": capability.verification,
            "status": capability.status,
            "argument_hint": capability.argument_hint,
            "invocation": {**capability.invocation, "wire_text": wire_text},
            "support_level": support_level,
            "reason": reason,
            "alternative": alternative,
            "invocable": invocable,
            "revision": (
                runtime_snapshot.revision if runtime_snapshot is not None else 0
            ),
        })

    runtime_skill_names = {
        str(item.get("name") or "").casefold()
        for item in runtime_rows if item.get("kind") == "skill"
    }
    local_skill_rows = [
        {
            **item,
            "action": "select-capability",
            "channel": "session_config",
            "origin": "verified_static",
            "delivery": "guaranteed",
            "verification": "verified",
            "status": "available",
            "support_level": "supported",
            "reason": "",
            "alternative": "",
            "invocable": True,
        }
        for item in [*local_skills.values(), *managed_skills]
        if item.get("source") == "Muteki 聊天插件" or str(item.get("name") or "").casefold() not in runtime_skill_names
    ]

    from muteki.external_agents.command_providers import client_commands, unavailable_commands
    native_names = {row["name"] for row in runtime_rows}
    client_rows = client_commands(normalized, native_names)
    matrix = runtime_snapshot.public_matrix() if runtime_snapshot else None
    rewind = next((row for row in (matrix or {}).get("rows", []) if row.get("key") == "rewind"), {})
    for row in client_rows:
        if row["action"] == "ui:rewind" and not rewind.get("invocable"):
            row.update(invocable=False, support_level="unsupported", action="inspect-runtime-capability",
                       reason="当前接入尚不能同步回退引擎历史与聊天记录", alternative="可使用 /fork 保留原记录并从选定轮次继续")
    client_names = {row["name"] for row in client_rows}
    runtime_rows = [row for row in runtime_rows if row["name"] not in client_names]
    if trigger == "/":
        rows = [
            {
                "id": f"command:{item['name']}", "kind": "command",
                "name": item["name"], "description": item["description"],
                "source": "Muteki", "scope": "conversation", "action": item["action"],
            }
            for item in _COMMANDS
        ]
        rows.extend(client_rows)
        rows.extend(runtime_rows)
        rows.extend(local_skill_rows)
        rows.extend(unavailable_commands(normalized, native_names | client_names))
    elif trigger == "@":
        rows = _thread_items(threads, current_thread_id=current_thread_id)
        runtime_mcp = [
            {
                "id": capability.id,
                "kind": "mcp",
                "name": capability.name,
                "description": capability.description,
                "source": capability.source or runtime_snapshot.adapter_id,
                "scope": capability.scope,
                "channel": capability.channel,
                "delivery": capability.delivery,
                "verification": capability.verification,
                "status": capability.status,
                "action": "inspect-runtime-status",
            }
            for capability in snapshot_items
            if capability.kind == "mcp_status"
        ]
        rows.extend(runtime_mcp)
        if read_section("mcp", lambda: capability_enabled("mcp", "muteki-control"), False) and control_enabled:
            rows.append(_muteki_control_mcp_row(
                engine=normalized,
                injected=engine_receives_capability_gateway(normalized),
            ))
        if plugin_service is not None:
            rows.extend({"id": f"managed-plugin:{normalized}:{r['id']}", "kind": "plugin", "name": r["name"],
                         "description": r["description"], "source": "Muteki 聊天插件", "scope": "chat",
                         "action": "inspect-runtime-status"} for r in read_section("plugins", lambda: plugin_service.enabled(normalized), []))
        rows.extend(read_section("files", lambda: discover_files(workspace_root, query=raw_query, limit=48, errors=section_errors), []))
        if raw_query:
            rows = [item for item in rows if _matches(item, raw_query)]
        raw_query = ""
    elif trigger == "$":
        rows = [
            item for item in [*runtime_rows, *local_skill_rows]
            if item.get("kind") == "skill"
        ]
        if (
            read_section("mcp", lambda: capability_enabled("mcp", "muteki-control"), False)
            and control_enabled
            and not engine_receives_capability_gateway(normalized)
            and not any(
                str(item.get("name") or "").casefold() == "muteki-control"
                for item in rows
            )
        ):
            # Keep the Skill visible with an explicit disable reason so the
            # menu matches Devin's real "no gateway injection" boundary (#120).
            plugin_skill = _skill_item(
                package_root() / "skills" / "muteki-control" / "SKILL.md",
                engine=normalized,
                source="Muteki Agent Plugin",
                scope="thread",
                priority=600,
            )
            if plugin_skill is not None:
                rows.append({
                    **plugin_skill,
                    "action": "inspect-runtime-capability",
                    "channel": "session_config",
                    "origin": "verified_static",
                    "delivery": "local",
                    "verification": "verified",
                    "status": "unavailable",
                    "support_level": "unsupported",
                    "reason": _MUTEKI_CONTROL_UNSUPPORTED_REASON,
                    "alternative": _MUTEKI_CONTROL_UNSUPPORTED_ALTERNATIVE,
                    "invocable": False,
                })
    else:
        return []

    if kind_filter:
        rows = [item for item in rows if item.get("kind") == kind_filter]
    filtered = [item for item in rows if _matches(item, raw_query)]
    return [_public_item(item) for item in filtered[:1000]]


def _cap_snapshot(text: str) -> str:
    value = str(text or "")
    if len(value) <= _MAX_SNAPSHOT_CHARS:
        return value
    return f"{value[:_MAX_SNAPSHOT_CHARS]}\n…"


def _content_hash(text: str) -> str:
    return sha256(text.encode("utf-8")).hexdigest()[:24]


def _is_structured_ref(raw: dict[str, Any]) -> bool:
    kind = str(raw.get("kind") or "").strip()
    if kind in _STRIP_CATALOG_KINDS:
        return False
    if int(raw.get("context_schema") or 0) == _CONTEXT_SCHEMA_V2:
        return True
    if str(raw.get("node_id") or "").strip() and isinstance(raw.get("locator"), dict):
        return True
    return kind in {"message_span", "tool_excerpt"}


def _snapshot_dict(raw: dict[str, Any], *, fallback_label: str, fallback_text: str) -> dict[str, Any]:
    snapshot = raw.get("snapshot") if isinstance(raw.get("snapshot"), dict) else {}
    label = str(snapshot.get("label") or fallback_label or "").strip() or fallback_label
    text = _cap_snapshot(str(snapshot.get("text") or fallback_text or label))
    captured = str(snapshot.get("captured_at") or "").strip()
    out = {
        "label": label,
        "text": text,
        "captured_at": captured or datetime.now(timezone.utc).isoformat(),
    }
    content_hash = str(snapshot.get("content_hash") or "").strip()
    if content_hash:
        out["content_hash"] = content_hash
    return out


def _structured_public(
    *,
    raw: dict[str, Any],
    kind: str,
    name: str,
    description: str,
    source: str,
    scope: str,
    locator: dict[str, Any],
    snapshot: dict[str, Any],
    status: str,
    status_reason: str = "",
    legacy_id: str = "",
) -> dict[str, Any]:
    node_id = str(raw.get("node_id") or "").strip() or f"ctx-{_opaque_id(kind, legacy_id or name)}"
    item_id = str(raw.get("id") or legacy_id or node_id).strip()
    out: dict[str, Any] = {
        "context_schema": _CONTEXT_SCHEMA_V2,
        "node_id": node_id,
        "id": item_id,
        "kind": kind,
        "name": name,
        "description": description,
        "source": source,
        "scope": scope,
        "locator": locator,
        "snapshot": snapshot,
        "status": status,
    }
    if status_reason:
        out["status_reason"] = status_reason
    if legacy_id:
        out["legacy_capability_id"] = legacy_id
    return out


def _resolve_file_lines(path: Path, start_line: int | None, end_line: int | None) -> str:
    try:
        text = path.read_text(encoding="utf-8")
    except (OSError, UnicodeError):
        return ""
    if not start_line and not end_line:
        return _cap_snapshot(text)
    lines = text.splitlines()
    start = max(1, int(start_line or 1))
    end = max(start, int(end_line or start))
    sliced = "\n".join(lines[start - 1:end])
    return _cap_snapshot(sliced)


def _resolve_structured_ref(
    raw: dict[str, Any],
    *,
    workspace_root: str,
    message_lookup: Any = None,
) -> dict[str, Any]:
    """Resolve a schema-v2 context node with soft-stale semantics."""
    kind = str(raw.get("kind") or "").strip()
    locator = raw.get("locator") if isinstance(raw.get("locator"), dict) else {}
    name = str(raw.get("name") or "").strip() or kind
    description = str(raw.get("description") or "").strip()
    source = str(raw.get("source") or "").strip() or "composer"
    scope = str(raw.get("scope") or "").strip() or "thread"
    legacy_id = str(raw.get("legacy_capability_id") or raw.get("id") or "").strip()
    snapshot = _snapshot_dict(raw, fallback_label=name, fallback_text=description)

    if kind == "message_span":
        message_id = str(locator.get("message_id") or "").strip()
        thread_id = str(locator.get("thread_id") or "").strip()
        start_offset = locator.get("start_offset")
        end_offset = locator.get("end_offset")
        loc = {
            key: value for key, value in {
                "thread_id": thread_id or None,
                "message_id": message_id or None,
                "turn_id": str(locator.get("turn_id") or "").strip() or None,
                "start_offset": int(start_offset) if isinstance(start_offset, int)
                or (isinstance(start_offset, str) and str(start_offset).isdigit())
                else None,
                "end_offset": int(end_offset) if isinstance(end_offset, int)
                or (isinstance(end_offset, str) and str(end_offset).isdigit())
                else None,
            }.items() if value is not None
        }
        message = None
        if message_id and callable(message_lookup):
            try:
                message = message_lookup(message_id)
            except Exception:  # noqa: BLE001 — soft stale on lookup failure
                message = None
        if message is None:
            return _structured_public(
                raw=raw, kind=kind, name=name or "回答摘录",
                description=description or snapshot["text"][:180],
                source=source or "对话", scope=scope,
                locator=loc, snapshot=snapshot,
                status="missing" if not snapshot.get("text") else "stale",
                status_reason="来源消息不可访问，已保留引用快照",
                legacy_id=legacy_id,
            )
        live_text = str(getattr(message, "text", "") or "")
        start = int(loc.get("start_offset") or 0)
        end = int(loc.get("end_offset") or len(live_text))
        start = max(0, min(start, len(live_text)))
        end = max(start, min(end, len(live_text)))
        live_slice = live_text[start:end]
        expected_hash = str(snapshot.get("content_hash") or "").strip()
        live_hash = _content_hash(live_slice) if live_slice else ""
        status = "ok"
        reason = ""
        if not live_slice:
            status = "stale"
            reason = "来源消息范围已变更，已保留引用快照"
        elif expected_hash and live_hash and expected_hash != live_hash:
            status = "stale"
            reason = "来源消息内容已更新，已保留引用快照"
        elif snapshot.get("text") and live_slice and snapshot["text"].rstrip("…\n") not in live_text:
            # Snapshot-only compare when no hash was captured at cite time.
            if live_slice != snapshot["text"] and not snapshot["text"].startswith(live_slice[:80]):
                status = "stale"
                reason = "来源消息内容已更新，已保留引用快照"
        if not snapshot.get("content_hash") and live_hash:
            snapshot = {**snapshot, "content_hash": live_hash}
        return _structured_public(
            raw=raw, kind=kind, name=name or "回答摘录",
            description=description or snapshot["text"][:180],
            source=source or "对话", scope=scope,
            locator=loc, snapshot=snapshot,
            status=status, status_reason=reason, legacy_id=legacy_id,
        )

    if kind == "file":
        relative = str(
            locator.get("relative_path") or description or ""
        ).strip()
        start_line = locator.get("start_line")
        end_line = locator.get("end_line")
        try:
            start_line_i = int(start_line) if start_line is not None else None
        except (TypeError, ValueError):
            start_line_i = None
        try:
            end_line_i = int(end_line) if end_line is not None else None
        except (TypeError, ValueError):
            end_line_i = None
        loc = {
            key: value for key, value in {
                "relative_path": relative or None,
                "workspace_id": str(locator.get("workspace_id") or "").strip() or None,
                "start_line": start_line_i,
                "end_line": end_line_i,
            }.items() if value is not None
        }
        if not workspace_root or not relative:
            return _structured_public(
                raw=raw, kind=kind, name=name or relative or "file",
                description=relative or description,
                source=source or "当前项目", scope=scope or "project",
                locator=loc, snapshot=snapshot,
                status="missing",
                status_reason="文件路径不可解析，已保留引用快照",
                legacy_id=legacy_id,
            )
        try:
            root = Path(workspace_root).expanduser().resolve(strict=True)
            candidate = (root / relative).resolve(strict=True)
            candidate.relative_to(root)
        except (OSError, RuntimeError, ValueError):
            return _structured_public(
                raw=raw, kind=kind, name=name or Path(relative).name or "file",
                description=relative or description,
                source=source or "当前项目", scope=scope or "project",
                locator=loc, snapshot=snapshot,
                status="missing",
                status_reason="文件不存在或已移出工作区，已保留引用快照",
                legacy_id=legacy_id,
            )
        if not candidate.is_file() or candidate.is_symlink():
            return _structured_public(
                raw=raw, kind=kind, name=name or candidate.name,
                description=relative or description,
                source=source or "当前项目", scope=scope or "project",
                locator=loc, snapshot=snapshot,
                status="missing",
                status_reason="文件不可访问，已保留引用快照",
                legacy_id=legacy_id,
            )
        live_text = _resolve_file_lines(candidate, start_line_i, end_line_i)
        live_hash = _content_hash(live_text) if live_text else ""
        expected_hash = str(snapshot.get("content_hash") or "").strip()
        status = "ok"
        reason = ""
        if expected_hash and live_hash and expected_hash != live_hash:
            status = "stale"
            reason = "文件内容已变更，已保留引用快照"
        if not snapshot.get("text"):
            snapshot = {**snapshot, "text": live_text or relative}
        if not snapshot.get("content_hash") and live_hash:
            snapshot = {**snapshot, "content_hash": live_hash}
        # Keep cite-time snapshot readable; do not overwrite with live text when stale.
        return _structured_public(
            raw=raw, kind=kind, name=name or candidate.name,
            description=relative or description,
            source=source or "当前项目", scope=scope or "project",
            locator=loc, snapshot=snapshot,
            status=status, status_reason=reason, legacy_id=legacy_id,
        )

    if kind == "tool_excerpt":
        # Reserved for #34; soft-stale with snapshot only.
        return _structured_public(
            raw=raw, kind=kind, name=name or "工具摘录",
            description=description or snapshot["text"][:180],
            source=source, scope=scope,
            locator={
                key: value for key, value in {
                    "tool_call_id": str(locator.get("tool_call_id") or "").strip() or None,
                    "event_id": str(locator.get("event_id") or "").strip() or None,
                }.items() if value is not None
            },
            snapshot=snapshot,
            status="stale" if snapshot.get("text") else "missing",
            status_reason="工具摘录入口尚未启用，已保留快照",
            legacy_id=legacy_id,
        )

    # Unknown future kinds: soft-stale with snapshot, never hard-fail.
    return _structured_public(
        raw=raw, kind=kind or "unknown",
        name=name or kind or "context",
        description=description or snapshot["text"][:180],
        source=source, scope=scope,
        locator=dict(locator),
        snapshot=snapshot,
        status="stale",
        status_reason=f"未知引用类型 {kind or 'unknown'}，已保留快照",
        legacy_id=legacy_id,
    )


def _instruction_for_structured(item: dict[str, Any]) -> str | None:
    kind = str(item.get("kind") or "")
    snapshot = item.get("snapshot") if isinstance(item.get("snapshot"), dict) else {}
    locator = item.get("locator") if isinstance(item.get("locator"), dict) else {}
    status = str(item.get("status") or "ok")
    label = str(snapshot.get("label") or item.get("name") or kind)
    body = str(snapshot.get("text") or "").strip()
    stale_note = ""
    if status in {"stale", "missing", "forbidden"}:
        stale_note = (
            f"\n（注意：来源状态为 {status}"
            f"{('：' + str(item.get('status_reason') or '')) if item.get('status_reason') else ''}；"
            "以下为引用时快照，来源可能已变更。）"
        )
    if kind == "file":
        relative = str(locator.get("relative_path") or item.get("description") or "")
        line_hint = ""
        if locator.get("start_line"):
            end = locator.get("end_line") or locator.get("start_line")
            line_hint = f" L{locator['start_line']}-{end}"
        return (
            f"[显式引用的项目文件{line_hint}]\n{relative}{stale_note}\n"
            f"{body or '（无快照）'}\n"
            "处理当前请求时参考上述内容；若状态非 ok，优先信任快照并谨慎对待磁盘现状。"
        )
    if kind == "message_span":
        message_id = str(locator.get("message_id") or "")
        return (
            f"[显式引用的回答摘录: {label}]\n"
            f"message_id={message_id}{stale_note}\n"
            f"{body or '（无快照）'}"
        )
    if kind == "tool_excerpt":
        return f"[显式引用的工具摘录: {label}]{stale_note}\n{body or '（无快照）'}"
    if kind == "thread":
        return None  # handled by legacy path when present
    return f"[显式引用的上下文: {label}]{stale_note}\n{body or '（无快照）'}"


def resolve_capability_refs(
    refs: Any,
    *,
    engine: str,
    workspace_root: str = "",
    extension_service: Any = None,
    threads: Iterable[Any] = (),
    message_loader: Any = None,
    message_lookup: Any = None,
    plugin_service: Any = None,
) -> tuple[list[dict[str, Any]], str]:
    """Validate client refs and build the explicit Agent instruction block.

    Schema-v2 context nodes (``context_schema: 2`` / ``node_id``+locator) use
    soft-stale semantics: missing or drifted sources keep the display snapshot
    and enqueue with ``status`` marked, instead of hard-failing the send.
    Legacy opaque mcp/skill/plugin/file/thread chips retain prior behavior.
    """
    if not refs:
        return [], ""
    if not isinstance(refs, list) or len(refs) > _MAX_SELECTED_REFS:
        raise ComposerCapabilityError(f"能力引用最多 {_MAX_SELECTED_REFS} 个")
    normalized = str(engine or "").strip().lower()
    if normalized not in SUPPORTED_ENGINES:
        raise ComposerCapabilityError("当前 Agent 不支持能力引用")

    catalog: dict[str, dict[str, Any]] = {}
    requested_kinds = {str(raw.get("kind") or str(raw.get("id") or raw.get("legacy_capability_id") or "").partition(":")[0])
                       for raw in refs if isinstance(raw, dict) and not _is_structured_ref(raw)}
    if (
        "mcp" in requested_kinds and capability_enabled("mcp", "muteki-control")
        and (plugin_service is None or plugin_service.control_enabled(normalized))
        and engine_receives_capability_gateway(normalized)
    ):
        catalog["mcp:muteki-control"] = {
            "id": "mcp:muteki-control", "kind": "mcp", "name": "muteki-control",
            "description": "当前会话由 Muteki 注入的 MCP 能力入口",
            "source": "Muteki", "scope": "thread",
        }
    for item in (_extension_items(extension_service) if "plugin" in requested_kinds else []):
        catalog[str(item["id"])] = item
    for item in (discover_skills(normalized, workspace_root) if "skill" in requested_kinds else []):
        if plugin_service is not None and item.get("source") == "Muteki Agent Plugin" and not plugin_service.control_enabled(normalized):
            continue
        catalog[str(item["id"])] = item
    if plugin_service is not None and "skill" in requested_kinds:
        for item in plugin_service.skill_rows(normalized):
            catalog[str(item["id"])] = item
    for item in (_thread_items(threads) if "thread" in requested_kinds else []):
        catalog[str(item["id"])] = item

    selected: list[dict[str, Any]] = []
    instructions: list[str] = []
    seen: set[str] = set()
    for raw in refs:
        if not isinstance(raw, dict):
            raise ComposerCapabilityError("能力引用格式无效")

        if _is_structured_ref(raw):
            item = _resolve_structured_ref(
                raw,
                workspace_root=workspace_root,
                message_lookup=message_lookup,
            )
            dedupe_key = str(item.get("node_id") or item.get("id") or "")
            if not dedupe_key or dedupe_key in seen:
                continue
            # Soft-stale still requires a readable snapshot for enqueue.
            snapshot = item.get("snapshot") if isinstance(item.get("snapshot"), dict) else {}
            if not str(snapshot.get("text") or "").strip() and item.get("status") != "ok":
                raise ComposerCapabilityError("引用缺少可读快照，请重新选择")
            seen.add(dedupe_key)
            selected.append(item)
            instruction = _instruction_for_structured(item)
            if instruction:
                instructions.append(instruction)
            continue

        item_id = str(raw.get("id") or raw.get("legacy_capability_id") or "").strip()
        if not item_id or item_id in seen:
            continue
        item = catalog.get(item_id)
        if item is None and str(raw.get("legacy_capability_id") or "").strip():
            item = catalog.get(str(raw.get("legacy_capability_id")).strip())
        if item is None and str(raw.get("kind") or "") == "file" and workspace_root:
            relative = str(raw.get("description") or "").strip()
            try:
                root = Path(workspace_root).expanduser().resolve(strict=True)
                candidate = (root / relative).resolve(strict=True)
                candidate.relative_to(root)
            except (OSError, RuntimeError, ValueError):
                candidate = None
            if (
                candidate is not None
                and candidate.is_file()
                and not candidate.is_symlink()
                and item_id == _opaque_id("file", f"{root}\0{relative}")
            ):
                item = {
                    "id": item_id,
                    "kind": "file",
                    "name": candidate.name,
                    "description": relative,
                    "source": "当前项目",
                    "scope": "project",
                    "_relative_path": relative,
                }
        if item is None:
            raise ComposerCapabilityError("所选能力已失效，请按当前 Agent 重新选择")
        seen.add(item_id)
        public = _public_item(item)
        if item.get("source") == "Muteki 聊天插件":
            public["arguments"] = str(raw.get("arguments") or "")
        selected.append(public)
        kind = str(item["kind"])
        name = str(item["name"])
        if kind == "skill":
            if item.get("native_engine"):
                raise ComposerCapabilityError(f"此 Skill 依赖原生执行语义，请通过 /{item['native_name']} 调用")
            skill_path = Path(str(item.get("_path") or ""))
            try:
                skill_text = skill_path.read_text(encoding="utf-8")
            except (OSError, UnicodeError) as exc:
                raise ComposerCapabilityError(
                    f"Skill {name} 当前不可读取，请重新选择"
                ) from exc
            if item.get("source") == "Muteki 聊天插件":
                import shlex
                arguments = str(raw.get("arguments") or "")
                try:
                    positional = shlex.split(arguments)
                except ValueError:
                    positional = arguments.split()
                def substitute(match):
                    if match.group(0) == "$ARGUMENTS":
                        return arguments
                    index = int(match.group(1) or match.group(2))
                    return positional[index] if index < len(positional) else match.group(0)
                skill_text = re.sub(r"\$ARGUMENTS\[(\d+)\]|\$(\d+)\b|\$ARGUMENTS\b", substitute, skill_text)
            for variable in ("${PLUGIN_ROOT}", "${CLAUDE_PLUGIN_ROOT}", "${CODEX_PLUGIN_ROOT}"):
                skill_text = skill_text.replace(variable, str(item.get("_package_root") or skill_path.parent))
            instructions.append(
                f"[用户显式选择的 Agent Skill: {name}]\n资源根目录：{skill_path.parent}\n{skill_text}"
            )
        elif kind == "file":
            instructions.append(
                f"[显式引用的项目文件]\n{item['_relative_path']}\n"
                "处理当前请求时读取并使用该文件。"
            )
        elif kind in {"mcp", "plugin"}:
            # MCP/插件能力已经在 SessionStart 结构化交付；引用只用于 UI
            # 选择与审计，不把“请使用某工具”再次伪装成用户文本。
            continue
        elif kind == "thread":
            thread_id = str(item.get("_thread_id") or "")
            messages = list(message_loader(thread_id) if callable(message_loader) else [])
            transcript = "\n".join(
                f"{'用户' if str(getattr(message, 'role', '')) == 'user' else '助手'}："
                f"{str(getattr(message, 'text', '') or '').strip()}"
                for message in messages[-24:]
                if str(getattr(message, "text", "") or "").strip()
            )[-_MAX_THREAD_CONTEXT_CHARS:]
            instructions.append(
                f"[显式引用的对话: {name}]\n{transcript or '该对话暂无可引用消息。'}"
            )

    context = "\n\n".join(instructions)
    if len(context) > _MAX_CONTEXT_CHARS:
        raise ComposerCapabilityError("所选参考上下文过长，请减少选择后重试")
    return selected, context


__all__ = [
    "ComposerCapabilityError", "SUPPORTED_ENGINES", "discover_files",
    "discover_skills", "engine_receives_capability_gateway",
    "resolve_capability_refs", "resolve_composer_catalog",
]
