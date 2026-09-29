"""Authenticated management of conversation-scoped plugins."""
from __future__ import annotations

import asyncio
from pathlib import Path
from typing import Any

from fastapi import APIRouter, HTTPException
from pydantic import BaseModel

from muteki.conversation.chat_plugins import ChatPluginError, ChatPluginService, ENGINES


class InstallBody(BaseModel):
    source: dict[str, Any]


class UpdateBody(BaseModel):
    enabled: bool | None = None
    rollback: bool = False
    native_hooks: bool | None = None
    digest: str = ""


class McpBody(BaseModel):
    name: str
    servers: dict[str, Any]


def create_chat_plugins_router(plugins: ChatPluginService, conversation: Any) -> APIRouter:
    router = APIRouter(prefix="/api/chat-plugins", tags=["chat-plugins"])

    async def changed():
        await plugins.invalidate()
        # Idle sessions reload at their next turn; active sessions finish first.
        for thread in conversation.manager.list_threads():
            snapshot = conversation.executor.cached_runtime_capabilities(thread.thread_id)
            if snapshot:
                snapshot.stale = True
                conversation.executor._emit_capabilities_updated(thread.thread_id, snapshot)

    @router.get("")
    async def listing(engine: str = "codex"):
        if engine not in ENGINES:
            raise HTTPException(400, "未知 Agent")
        from muteki.conversation.composer_capabilities import discover_skills
        from muteki.conversation.chat_providers import provider_for
        native = [{k: v for k, v in row.items() if not k.startswith("_")}
                  for row in await asyncio.to_thread(discover_skills, engine)
                  if row.get("source") != "Muteki Agent Plugin"]
        return {"engines": list(ENGINES), "engine": engine, "revision": plugins.revision(engine),
                "packages": [plugins.public(r) for r in plugins.records()], "native_skills": native,
                "native_mcp": provider_for(engine).native_mcp(), "transport": provider_for(engine).transport,
                "control_enabled": plugins.control_enabled(engine)}

    @router.get("/visualizations/{thread_id}")
    async def visualization(thread_id: str, path: str):
        thread = conversation.manager.get_thread(thread_id)
        if thread is None or thread.mode != "conversation":
            raise HTTPException(404, "对话不存在")
        root = plugins.visualization_root(thread_id)
        try:
            target = Path(path).resolve(strict=True)
            if not target.is_relative_to(root) or target.suffix.lower() != ".html" or target.stat().st_size > 1_000_000:
                raise ValueError("invalid visualization")
            # An arbitrary path is not a file-reading API. The assistant must
            # have attached this exact path in this conversation's output.
            import json
            import re
            attached = False
            for message in conversation.conv.list_current_messages(thread_id):
                if message.role != "assistant":
                    continue
                for marked, plain in re.findall(r"(?:visualize(\{[^\n]*?\})|^\s*(?:muteki-visualize|visualize)[ \t]+(\{[^\n]*\})[ \t]*$)", message.text or "", re.MULTILINE):
                    raw = marked or plain
                    try:
                        if Path(json.loads(raw).get("path", "")).resolve() == target:
                            attached = True
                    except (ValueError, TypeError):
                        continue
            if not attached:
                raise ValueError("not attached")
            from muteki.external_agents.factory import engine_for_adapter
            engine = engine_for_adapter(conversation.manager.runtime_selection(thread_id).adapter_id)
            return {"html": await asyncio.to_thread(target.read_text, encoding="utf-8"),
                    "assets": await asyncio.to_thread(plugins.visualization_assets, engine)}
        except (OSError, ValueError):
            raise HTTPException(404, "图形文件尚未生成或不属于当前对话") from None

    @router.post("/install")
    async def install(body: InstallBody):
        try:
            result = await asyncio.to_thread(plugins.install, body.source)
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
            result = await asyncio.to_thread(plugins.add_mcp, body.name, body.servers)
            await changed()
            return result
        except Exception as exc:
            raise HTTPException(400, "MCP 配置无效，请检查名称、command/URL 与参数") from exc

    @router.patch("/packages/{package_id}")
    async def update(package_id: str, body: UpdateBody):
        try:
            result = plugins.update(package_id, body.enabled, body.rollback, body.native_hooks, body.digest)
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
