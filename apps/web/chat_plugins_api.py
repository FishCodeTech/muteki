"""Authenticated management of chat and Worker Agent extensions."""
from __future__ import annotations

import asyncio
from typing import Any

from fastapi import APIRouter, HTTPException
from pydantic import BaseModel

from muteki.conversation.chat_plugins import ChatPluginError, ChatPluginService, ENGINES, MODES


class InstallBody(BaseModel):
    source: dict[str, Any]
    modes: list[str] | None = None


class UpdateBody(BaseModel):
    enabled: bool | None = None
    rollback: bool = False
    native_hooks: bool | None = None
    digest: str = ""
    modes: list[str] | None = None


class McpBody(BaseModel):
    name: str
    servers: dict[str, Any]
    modes: list[str] | None = None


def create_chat_plugins_router(plugins: ChatPluginService, conversation: Any,
                               *, prefix: str = "/api/chat-plugins") -> APIRouter:
    router = APIRouter(prefix=prefix, tags=["agent-extensions"])

    async def changed():
        await plugins.invalidate()
        # Idle sessions reload at their next turn; active sessions finish first.
        for thread in conversation.manager.list_threads():
            snapshot = conversation.executor.cached_runtime_capabilities(thread.thread_id)
            if snapshot:
                snapshot.stale = True
                conversation.executor._emit_capabilities_updated(thread.thread_id, snapshot)

    @router.get("")
    async def listing(engine: str = "codex", mode: str = "chat", run_id: str = ""):
        if engine not in ENGINES:
            raise HTTPException(400, "未知 Agent")
        if mode not in MODES:
            raise HTTPException(400, "未知使用场景")
        if run_id and (len(run_id) > 100 or not run_id.startswith("run-")
                       or not all(char.isalnum() or char in "-_" for char in run_id)):
            raise HTTPException(400, "Run ID 无效")
        from muteki.conversation.composer_capabilities import discover_skills
        from muteki.conversation.chat_providers import provider_for
        from muteki.capability_management import enabled as capability_enabled
        from muteki.solver.credential_accounts import host_discovery_enabled
        native = [{k: v for k, v in row.items() if not k.startswith("_")}
                  for row in await asyncio.to_thread(discover_skills, engine)
                  if row.get("source") != "Muteki Agent Plugin"]
        return {"engines": list(ENGINES), "modes": list(MODES), "engine": engine, "revision": plugins.revision(engine),
                "host_discovery_enabled": host_discovery_enabled(),
                "packages": [plugins.public(r) for r in plugins.records()], "native_skills": native,
                "bundled_plugins": [{"id": "muteki-visualize", "name": "Muteki Visualize", "modes": ["chat"]}],
                "builtin_skills": [{"id": "agent-browser", "modes": ["pentest"],
                                    "enabled": capability_enabled("skills", "agent-browser"),
                                    "scope": "new_workers"}],
                "native_mcp": provider_for(engine).native_mcp(), "transport": provider_for(engine).transport,
                "control_enabled": plugins.control_enabled(engine),
                "runtime_scope": run_id if mode != "chat" else "",
                "runtime_mcp": plugins.runtime_mcp_health(engine, mode, run_id if mode != "chat" else "")}

    @router.get("/visualizations/{thread_id}")
    async def visualization(thread_id: str, path: str, message_id: str = ""):
        from muteki.conversation.visualizations import VisualizationError
        from muteki.external_agents.factory import engine_for_adapter
        thread = conversation.manager.get_thread(thread_id)
        if thread is None or thread.mode != "conversation":
            raise HTTPException(404, "对话不存在")
        try:
            engine = engine_for_adapter(conversation.manager.runtime_selection(thread_id).adapter_id)
            return await asyncio.to_thread(
                plugins.visualization_document, thread_id,
                conversation.conv.list_current_messages(thread_id), engine, path, message_id,
            )
        except VisualizationError as exc:
            raise HTTPException(404 if exc.code == "visualization.not_attached" else 422,
                                {"code": exc.code, "message": str(exc)}) from exc
        except (OSError, ValueError) as exc:
            raise HTTPException(500, {"code": "visualization.store_failed", "message": str(exc)}) from exc

    @router.post("/bundled/muteki-visualize")
    async def install_visualize():
        try:
            result = await asyncio.to_thread(plugins.install_visualize)
            await changed()
            return result
        except ChatPluginError as exc:
            raise HTTPException(400, {"code": exc.code, "message": str(exc)}) from exc

    @router.post("/install")
    async def install(body: InstallBody):
        try:
            result = await asyncio.to_thread(plugins.install, body.source, body.modes)
            await changed()
            return result
        except ChatPluginError as exc:
            raise HTTPException(
                400,
                {"code": exc.code, "message": str(exc)},
            ) from exc
        except ValueError as exc:
            raise HTTPException(
                400,
                {"code": "chat_plugin.invalid", "message": str(exc)},
            ) from exc
        except OSError as exc:
            raise HTTPException(
                400,
                {"code": "chat_plugin.source_inaccessible", "message": "无法访问插件来源"},
            ) from exc
        except Exception as exc:
            raise HTTPException(
                400,
                {"code": "chat_plugin.install_failed", "message": "插件导入失败，请稍后重试"},
            ) from exc

    @router.post("/mcp")
    async def add_mcp(body: McpBody):
        try:
            result = await asyncio.to_thread(plugins.add_mcp, body.name, body.servers, body.modes)
            await changed()
            return result
        except ChatPluginError as exc:
            raise HTTPException(400, {"code": exc.code, "message": str(exc)}) from exc
        except Exception as exc:
            raise HTTPException(400, "MCP 配置无效，请检查名称、command/URL 与参数") from exc

    @router.patch("/packages/{package_id}")
    async def update(package_id: str, body: UpdateBody):
        try:
            result = plugins.update(package_id, body.enabled, body.rollback, body.native_hooks, body.digest, body.modes)
            await changed()
            return result
        except ValueError as exc:
            raise HTTPException(400, str(exc)) from exc

    @router.delete("/packages/{package_id}")
    async def uninstall(package_id: str):
        plugins.uninstall(package_id)
        await changed()
        return {"removed": True}

    @router.post("/check/{package_id}")
    async def check(package_id: str):
        record = plugins.get(package_id)
        if not record:
            raise HTTPException(404, "扩展不存在")
        count = 0
        diagnostics = []
        for name, config in record.get("mcp", {}).items():
            if config.get("disabled"):
                continue
            try:
                worker = plugins.worker("codex", record, name, config)
                await worker.wait_ready()
                if "tools" in worker.capabilities:
                    result = await worker.request("list_tools", {})
                    count += len(result.tools)
            except Exception:
                diagnostics.append(f"{name} 连接失败，请检查配置与运行环境")
        await plugins.invalidate()
        if diagnostics:
            plugins._diagnostics[package_id] = "；".join(diagnostics)
        return {"tool_count": count, "diagnostics": diagnostics}

    @router.put("/control")
    async def control(body: dict[str, bool]):
        if type(body.get("enabled")) is not bool:
            raise HTTPException(400, "启用状态无效")
        plugins.set_control(body["enabled"])
        await changed()
        return {"enabled": body["enabled"]}

    return router
