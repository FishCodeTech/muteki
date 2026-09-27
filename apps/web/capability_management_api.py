"""Operator 能力管理页的只读聚合与 Thread 级 Binding 写入口。

页面数据全部来自产品正在使用的 CapabilityCatalog、PlatformStore、Runtime
健康缓存和 Extension manifest。Secret 只以引用及使用状态出现。
"""

from __future__ import annotations

from collections import defaultdict
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

from fastapi import APIRouter, Body, HTTPException

from muteki.external_agents.factory import (
    DEFAULT_CLI_ADAPTER_BY_ENGINE,
    engine_for_adapter,
)
from muteki.platform.capability_catalog import (
    DEFAULT_CATALOG,
    MODE_TOOL_TEMPLATES,
    CapabilityCatalog,
    default_tool_set,
    resource_scopes_for_tool_set,
)
from muteki.platform.capability_bindings import (
    BindingServiceError,
    CapabilityBindingService,
)
from muteki.platform.contracts.capabilities import (
    CapabilityBinding,
    CapabilityGrant,
    ThreadMode,
)
from muteki.platform.contracts.objects import AgentSession, Thread
from muteki.platform.contracts.base import new_id
from muteki.platform.store import PlatformStore
from muteki.capability_management import (
    enabled as capability_resource_enabled,
    set_enabled as set_capability_resource_enabled,
)
from muteki.solver.worker_skills import legacy_user_skill_paths, project_skill_roots
from muteki.solver.worker_profiles import VALID_BASE_ENGINES


def _iso(value: Any) -> str | None:
    if value is None:
        return None
    return value.isoformat() if hasattr(value, "isoformat") else str(value)


def _credential_reference(reference: str | None) -> str:
    return str(reference or "").strip()


def _grant_state(grant: CapabilityGrant, *, now: datetime | None = None) -> str:
    current = now or datetime.now(timezone.utc)
    if grant.revoked_at is not None:
        return "revoked"
    if grant.expires_at is not None and grant.expires_at <= current:
        return "expired"
    return "active"


def _binding_view(binding: CapabilityBinding) -> dict[str, Any]:
    return {
        "binding_id": binding.binding_id,
        "binding_version": binding.binding_version,
        "thread_id": binding.thread_id,
        "principal_id": binding.principal_id,
        "mode": binding.mode.value,
        "tool_set": list(binding.tool_set),
        "allowed_commands": list(binding.allowed_commands),
        "allowed_queries": list(binding.allowed_queries),
        "resource_scopes": list(binding.resource_scopes),
        "policy_version": binding.policy_version,
        "created_at": _iso(binding.created_at),
        "revoked_at": _iso(binding.revoked_at),
    }


def _grant_view(grant: CapabilityGrant) -> dict[str, Any]:
    state = _grant_state(grant)
    return {
        "grant_id": grant.grant_id,
        "binding_id": grant.binding_id,
        "agent_session_id": grant.agent_session_id,
        "runtime_instance_id": grant.runtime_instance_id,
        "injection_kind": grant.injection_kind.value,
        "audience": grant.audience,
        "credential_reference": _credential_reference(grant.credential_ref),
        "credential_status": "referenced" if grant.credential_ref else "missing",
        "state": state,
        "issued_at": _iso(grant.issued_at),
        "expires_at": _iso(grant.expires_at),
        "revoked_at": _iso(grant.revoked_at),
        "last_touched_at": _iso(grant.last_touched_at),
    }


def _mode(value: str) -> ThreadMode:
    try:
        return ThreadMode(str(value or "conversation"))
    except ValueError:
        return ThreadMode.CONVERSATION


def _injection_support(
    capabilities: dict[str, Any],
    selected_kind: str = "",
) -> list[dict[str, Any]]:
    """按真实 probe 字段与实际选择结果列出注入通道。"""
    result: list[dict[str, Any]] = []
    fields = (
        ("mcp", "mcp"),
        ("native_tool_binding", "native_tool"),
        ("acp_mcp_config", "acp_mcp_config"),
        ("agent_plugin", "agent_plugin"),
        ("structured_http_rpc", "http_jsonrpc"),
    )
    for field, kind in fields:
        result.append({
            "kind": kind,
            "supported": bool(capabilities.get(field)),
            "source": "runtime_probe" if field in capabilities else "unprobed",
        })
    return result


def _extension_sources(extension_service: Any) -> list[dict[str, Any]]:
    rows: list[dict[str, Any]] = []
    if extension_service is None:
        return rows
    for record in extension_service.list_records():
        try:
            manifest = extension_service.manifest_of(record.extension_id)
        except Exception as exc:  # 单个损坏扩展不影响能力中心其余内容
            rows.append({
                "extension_id": record.extension_id,
                "origin": record.origin,
                "state": record.state.value,
                "enabled": record.enabled,
                "health": record.health,
                "manifest_error": str(exc),
                "provides": [],
                "secret_references": [],
            })
            continue
        usage_refs = {
            str(item.get("reference") or "")
            for item in record.secret_usage
            if isinstance(item, dict)
        }
        provides = []
        for provide in manifest.provides:
            section = {
                "protocol-binding": "mcp",
                "agent-capability-binding": "skills",
                "tool-integration": "tools",
                "external-agent-adapter": "adapters",
            }.get(provide.type, "other")
            registration = next((
                item for item in record.registry_entries
                if str(item.get("type") or "") == provide.type
                and str(item.get("id") or "") == provide.id
            ), None)
            provides.append({
                "type": provide.type,
                "id": provide.id,
                "api_version": provide.api_version,
                "section": section,
                "registry_state": str((registration or {}).get("state") or "declared"),
                "registry_detail": str((registration or {}).get("detail") or ""),
            })
        rows.append({
            "extension_id": record.extension_id,
            "version": manifest.version,
            "origin": manifest.origin or record.origin,
            "state": record.state.value,
            "enabled": record.enabled,
            "health": record.health,
            "provides": provides,
            "secret_references": [
                {
                    "reference": reference,
                    "status": "injected" if reference in usage_refs else "declared",
                }
                for reference in manifest.permissions.secrets
            ],
        })
    return rows


def _thread_bindings(store: PlatformStore) -> list[dict[str, Any]]:
    bindings = store.list(CapabilityBinding)
    grants = store.list(CapabilityGrant)
    by_thread_principal: dict[tuple[str, str], list[CapabilityBinding]] = defaultdict(list)
    grants_by_binding: dict[str, list[CapabilityGrant]] = defaultdict(list)
    for binding in bindings:
        by_thread_principal[(binding.thread_id, binding.principal_id)].append(binding)
    for grant in grants:
        grants_by_binding[grant.binding_id].append(grant)

    rows: list[dict[str, Any]] = []
    for thread in reversed(store.list(Thread)):
        keys = [key for key in by_thread_principal if key[0] == thread.thread_id]
        if not keys:
            keys = [(thread.thread_id, "system")]
        for _, principal_id in sorted(keys):
            versions = sorted(
                by_thread_principal.get((thread.thread_id, principal_id), []),
                key=lambda item: item.binding_version,
            )
            active = [item for item in versions if item.revoked_at is None]
            current = active[-1] if active else None
            binding_ids = {item.binding_id for item in versions}
            thread_grants = sorted(
                (
                    grant
                    for binding_id in binding_ids
                    for grant in grants_by_binding.get(binding_id, [])
                ),
                key=lambda item: item.issued_at,
                reverse=True,
            )
            mode = _mode(thread.mode)
            rows.append({
                "thread": {
                    "thread_id": thread.thread_id,
                    "title": thread.title or "未命名 Thread",
                    "mode": mode.value,
                    "updated_at": _iso(thread.updated_at),
                },
                "principal_id": principal_id,
                "active_binding": _binding_view(current) if current else None,
                "initial_tool_set": (
                    list(current.tool_set) if current else default_tool_set(mode)
                ),
                "selection_source": "binding" if current else "mode_template",
                "versions": [_binding_view(item) for item in reversed(versions)],
                "grants": [_grant_view(item) for item in thread_grants],
            })
    return rows


def _adapter_rows(
    store: PlatformStore,
    runtime_service: Any,
    extension_sources: list[dict[str, Any]],
) -> list[dict[str, Any]]:
    sessions = {item.agent_session_id: item for item in store.list(AgentSession)}
    last_by_engine: dict[str, tuple[CapabilityGrant, AgentSession]] = {}
    for grant in store.list(CapabilityGrant):
        session = sessions.get(grant.agent_session_id)
        if session is None:
            continue
        engine = engine_for_adapter(session.adapter_id)
        if not engine:
            continue
        previous = last_by_engine.get(engine)
        if previous is None or grant.issued_at > previous[0].issued_at:
            last_by_engine[engine] = (grant, session)

    extension_adapters: dict[str, list[str]] = defaultdict(list)
    for source in extension_sources:
        for provide in source.get("provides", []):
            if provide.get("section") != "adapters":
                continue
            provide_id = str(provide.get("id") or "")
            engine = engine_for_adapter(provide_id) or provide_id
            extension_adapters[engine].append(str(source.get("extension_id") or ""))

    instances = (
        runtime_service.list_instances(include_discovered=True)
        if runtime_service is not None else []
    )
    rows: list[dict[str, Any]] = []
    for engine in VALID_BASE_ENGINES:
        engine_instances = [
            item for item in instances if str(item.get("engine") or "") == engine
        ]
        preferred_id = DEFAULT_CLI_ADAPTER_BY_ENGINE.get(engine, "")
        preferred = next((
            item for item in engine_instances
            if item.get("adapter_id") == preferred_id
            and item.get("instance_id") == "default"
        ), None)
        preferred_health = (
            preferred.get("health") if isinstance(preferred, dict) else None
        )
        preferred_healthy = (
            isinstance(preferred_health, dict)
            and preferred_health.get("healthy") is True
        )
        selected = (preferred if preferred_healthy else None) or next((
            item for item in engine_instances
            if isinstance(item.get("health"), dict)
            and item["health"].get("healthy") is True
        ), None) or next((
            item for item in engine_instances if item.get("configured")
        ), None) or preferred or (engine_instances[0] if engine_instances else {})
        health = selected.get("health") if isinstance(selected, dict) else None
        health = health if isinstance(health, dict) else {}
        capabilities = health.get("capabilities")
        capabilities = capabilities if isinstance(capabilities, dict) else {}
        injection = health.get("capability_injection")
        injection = injection if isinstance(injection, dict) else {}
        latest = last_by_engine.get(engine)
        latest_view = None
        if latest is not None:
            grant, session = latest
            latest_view = {
                **_grant_view(grant),
                "adapter_id": session.adapter_id,
                "runtime_instance_id": session.runtime_instance_id or "default",
                "thread_id": session.thread_id,
                "session_closed": session.closed_at is not None,
            }
        rows.append({
            "engine": engine,
            "adapter_id": str(selected.get("adapter_id") or preferred_id),
            "instance_id": str(selected.get("instance_id") or "default"),
            "configured": bool(selected.get("configured")),
            "enabled": bool(selected.get("enabled", True)),
            "transport_kind": str(selected.get("transport_kind") or "structured"),
            "health_state": (
                "healthy" if health.get("healthy") is True
                else "unhealthy" if health.get("healthy") is False
                else "unprobed"
            ),
            "health_detail": str(health.get("detail") or ""),
            "probed_at": health.get("probed_at"),
            "runtime_version": str(health.get("runtime_version") or ""),
            "capabilities": capabilities,
            "injection_support": _injection_support(
                capabilities, str(injection.get("kind") or "")),
            "last_session_injection": latest_view,
            "source": (
                ["builtin", *extension_adapters.get(engine, [])]
                if extension_adapters.get(engine) else ["builtin"]
            ),
            "instances": [
                {
                    "adapter_id": item.get("adapter_id"),
                    "instance_id": item.get("instance_id"),
                    "configured": bool(item.get("configured")),
                    "enabled": bool(item.get("enabled", True)),
                    "transport_kind": item.get("transport_kind"),
                    "health_state": (
                        "healthy" if (item.get("health") or {}).get("healthy") is True
                        else "unhealthy" if (item.get("health") or {}).get("healthy") is False
                        else "unprobed"
                    ),
                }
                for item in engine_instances
            ],
        })
    return rows


def create_capability_management_router(
    *,
    store: PlatformStore,
    binding_service: CapabilityBindingService,
    runtime_service: Any = None,
    conversation_service: Any = None,
    extension_service: Any = None,
    catalog: CapabilityCatalog = DEFAULT_CATALOG,
) -> APIRouter:
    router = APIRouter(
        prefix="/api/capability-management", tags=["capability-management"]
    )

    @router.get("")
    async def overview() -> Any:
        extensions = _extension_sources(extension_service)
        tools = [
            {
                "name": spec.name,
                "description": spec.description,
                "target_kind": spec.target_kind.value,
                "command_type": spec.command_type,
                "query_type": spec.query_type,
                "aggregate_type": spec.aggregate_type,
                "source": "builtin",
                "default_modes": [
                    mode.value
                    for mode, names in MODE_TOOL_TEMPLATES.items()
                    if spec.name in names
                ],
            }
            for spec in catalog.filter(catalog.names())
        ]
        grants = store.list(CapabilityGrant)
        active_mcp = sum(
            1 for item in grants
            if item.injection_kind.value == "mcp" and _grant_state(item) == "active"
        )
        active_agent_plugins = sum(
            1 for item in grants
            if item.injection_kind.value == "agent_plugin"
            and _grant_state(item) == "active"
        )
        mcp_enabled = capability_resource_enabled("mcp", "muteki-control")
        blackboard_enabled = capability_resource_enabled(
            "skills", "muteki-blackboard")
        repo_root = Path(__file__).parents[2]
        agent_plugin_root = (
            repo_root / "muteki" / "agent_plugins" / "muteki-control")
        blackboard_source = repo_root / "skills" / "muteki-blackboard"
        legacy_copies = [
            {
                "path": str(path),
                "state": (
                    "linked" if path.is_symlink()
                    else "copied" if path.is_dir()
                    else "absent"
                ),
            }
            for path in legacy_user_skill_paths()
        ]
        return {
            "mcp": {
                "status": "ready" if mcp_enabled else "disabled",
                "endpoint": "/api/capability",
                "implementation": "muteki.capability_bindings.mcp_server",
                "active_grants": active_mcp,
                "servers": [{
                    "id": "muteki-control",
                    "name": "Muteki Control Gateway",
                    "enabled": mcp_enabled,
                    "health": "ready" if mcp_enabled else "disabled",
                    "source": "builtin",
                    "scope": "global",
                    "endpoint": "/api/capability",
                    "protocol_version": "2026-07-28",
                    "tools": len(tools),
                    "active_grants": active_mcp,
                    "lifecycle": "new_sessions",
                }],
                "extension_sources": [
                    item for item in extensions
                    if any(p.get("section") == "mcp" for p in item.get("provides", []))
                ],
            },
            "skills": {
                "status": (
                    "ready"
                    if (agent_plugin_root / "plugin.json").is_file()
                    and (agent_plugin_root / "mcp.json").is_file()
                    and (agent_plugin_root / "skills" / "muteki-control"
                         / "SKILL.md").is_file()
                    else "unavailable"
                ),
                "id": "muteki-control",
                "implementation": "agent-plugins.org/1.0.0",
                "active_grants": active_agent_plugins,
                "items": [
                    {
                        "id": "muteki-blackboard",
                        "name": "Muteki Blackboard",
                        "enabled": blackboard_enabled,
                        "mutable": True,
                        "health": (
                            "ready" if blackboard_enabled
                            and (blackboard_source / "SKILL.md").is_file()
                            else "disabled" if not blackboard_enabled
                            else "missing"
                        ),
                        "source": str(blackboard_source),
                        "scope": "worker_workspace",
                        "engines": {
                            engine: list(project_skill_roots(engine))
                            for engine in VALID_BASE_ENGINES
                        },
                        "legacy_copies": legacy_copies,
                        "lifecycle": "new_workers",
                    },
                    {
                        "id": "muteki-control",
                        "name": "Muteki Control Agent Plugin",
                        "enabled": True,
                        "mutable": False,
                        "health": (
                            "ready" if (agent_plugin_root / "plugin.json").is_file()
                            else "missing"),
                        "source": str(agent_plugin_root),
                        "scope": "thread_session",
                        "engines": {"format": "Agent Plugins 1.0.0"},
                        "legacy_copies": [],
                        "lifecycle": "binding_managed",
                    },
                ],
                "extension_sources": [
                    item for item in extensions
                    if any(p.get("section") == "skills" for p in item.get("provides", []))
                ],
            },
            "tools": tools,
            "adapters": _adapter_rows(store, runtime_service, extensions),
            "extensions": extensions,
            "threads": _thread_bindings(store),
            "mode_templates": {
                mode.value: list(names)
                for mode, names in MODE_TOOL_TEMPLATES.items()
            },
        }

    @router.put("/resources/{kind}/{resource_id}")
    async def update_resource(
        kind: str,
        resource_id: str,
        body: dict[str, Any] = Body(default_factory=dict),
    ) -> Any:
        if not isinstance(body.get("enabled"), bool):
            raise HTTPException(status_code=422, detail="enabled 必须是布尔值")
        try:
            result = set_capability_resource_enabled(
                kind, resource_id, bool(body["enabled"]))
        except KeyError as exc:
            raise HTTPException(status_code=404, detail=str(exc)) from exc
        except RuntimeError as exc:
            raise HTTPException(status_code=503, detail=str(exc)) from exc
        return {
            **result,
            "command_id": new_id("cmd"),
            "state": "completed",
            "applies_to": (
                "new_sessions" if kind == "mcp" else "new_workers"
            ),
        }

    @router.put("/threads/{thread_id}/binding")
    async def replace_binding(
        thread_id: str,
        body: dict[str, Any] = Body(default_factory=dict),
    ) -> Any:
        thread = store.get(Thread, thread_id)
        if thread is None:
            raise HTTPException(status_code=404, detail="Thread 不存在")
        principal_id = str(body.get("principal_id") or "system").strip()
        raw_tools = body.get("tool_set")
        if not isinstance(raw_tools, list):
            raise HTTPException(status_code=422, detail="tool_set 必须是数组")
        try:
            binding, changed, revoked_grants = binding_service.replace_tool_set(
                thread_id,
                principal_id,
                [str(item) for item in raw_tools],
                mode=_mode(thread.mode),
                resource_scopes=resource_scopes_for_tool_set(
                    _mode(thread.mode),
                    thread_id,
                    [str(item) for item in raw_tools],
                ),
            )
        except (BindingServiceError, ValueError) as exc:
            raise HTTPException(status_code=422, detail=str(exc)) from exc
        refresh = {"interrupted": False, "session_closed": False}
        if changed and conversation_service is not None:
            refresh = await conversation_service.executor.reload_thread_capabilities(
                thread_id)
        return {
            "binding": _binding_view(binding),
            "changed": changed,
            "revoked_grants": revoked_grants,
            "runtime_refresh": refresh,
            "applies_to": "next_turn" if changed else "unchanged",
        }

    @router.post("/threads/{thread_id}/revoke")
    async def revoke_binding_group(
        thread_id: str,
        body: dict[str, Any] = Body(default_factory=dict),
    ) -> Any:
        if store.get(Thread, thread_id) is None:
            raise HTTPException(status_code=404, detail="Thread 不存在")
        principal_id = str(body.get("principal_id") or "system").strip()
        binding = binding_service.active_binding_for_thread(thread_id, principal_id)
        if binding is None:
            raise HTTPException(status_code=404, detail="该 Thread 没有活动 Binding")
        active_grants = [
            item for item in store.list(CapabilityGrant, binding_id=binding.binding_id)
            if _grant_state(item) == "active"
        ]
        revoked = binding_service.revoke_binding(
            binding.binding_id, binding.binding_version
        )
        refresh = {"interrupted": False, "session_closed": False}
        if conversation_service is not None:
            refresh = await conversation_service.executor.reload_thread_capabilities(
                thread_id)
        return {
            "binding": _binding_view(revoked),
            "revoked_grants": len(active_grants),
            "runtime_refresh": refresh,
        }

    return router


__all__ = ["create_capability_management_router"]
