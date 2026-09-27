"""DomainModule / workspace kind 只读公开 API（任务书 11.1，CORE-03）。

导出 ``create_registry_router`` router factory；挂载到 FastAPI app 属于
INTEG-01（``apps/web/server.py`` 接线），本模块不做挂载。

- ``GET /api/workspace-kinds``：已注册 workspace kind 及描述/状态
  （禁用与校验失败的模块不返回入口）。
- ``GET /api/domain-modules``：全部模块注册记录（含 unavailable/disabled，
  供设置页展示注册结果与失败原因）。
- ``GET /api/domain-modules/{module_id}``：单个模块记录；禁用后仍可读，
  保证历史数据查询入口不被注册状态影响。

全部为只读端点，不改变注册表状态。
"""

from __future__ import annotations

from fastapi import APIRouter, Body, HTTPException

from .registry import DomainModuleRegistry, ModuleRegistration, WorkspaceKindInfo
from .contracts.commands import ActorRef, CommandEnvelope


def create_registry_router(
    registry: DomainModuleRegistry, command_api: object | None = None
) -> APIRouter:
    """基于给定注册表构造只读 router。"""
    router = APIRouter(tags=["platform-registry"])

    @router.get("/api/workspace-kinds", response_model=list[WorkspaceKindInfo])
    def list_workspace_kinds() -> list[WorkspaceKindInfo]:
        return registry.list_workspace_kinds()

    @router.get("/api/domain-modules", response_model=list[ModuleRegistration])
    def list_domain_modules() -> list[ModuleRegistration]:
        return registry.list_modules()

    @router.get(
        "/api/domain-modules/{module_id}",
        response_model=ModuleRegistration,
    )
    def get_domain_module(module_id: str) -> ModuleRegistration:
        record = registry.get(module_id)
        if record is None:
            raise HTTPException(status_code=404, detail=f"unknown module: {module_id}")
        return record

    if command_api is not None:
        @router.post("/api/domain-modules/{module_id}/{action}")
        async def change_domain_module(
            module_id: str, action: str, body: dict = Body(default={})
        ):
            if action not in {"enable", "disable"}:
                raise HTTPException(status_code=404, detail="unknown module action")
            payload = dict(body or {})
            command_id = str(payload.pop("command_id", "") or "")
            fields = {
                "command_type": f"module.{action}",
                "aggregate_type": "module",
                "aggregate_id": module_id,
                "actor": ActorRef(kind="operator", id="local-user"),
                "payload": {"module_id": module_id},
            }
            if command_id:
                fields["command_id"] = command_id
            receipt = await command_api.dispatch(CommandEnvelope(**fields))
            status = 200 if receipt.error is None else (
                404 if receipt.error.category.value == "not_found" else 409)
            from fastapi.responses import JSONResponse
            return JSONResponse(
                {"receipt": receipt.model_dump(mode="json")},
                status_code=status,
            )

    return router
