"""扩展 API 后端 router factory（EXT-01；前端与挂载由 EXT-02 / INTEG-01 负责）。

``create_extension_router(service, api)`` 返回一个 FastAPI ``APIRouter``：
所有状态修改都经 MutekiCommandAPI dispatch typed command，查询走
Query Handler；路由层只做身份（默认本地 operator）与格式转换，不复制
权限判定。``/api/extensions/...`` 路径与任务书 13.1 的
``/settings/extensions`` 页面对应。
"""

from __future__ import annotations

from typing import Any, Optional

from fastapi import APIRouter
from fastapi.responses import JSONResponse
from pydantic import BaseModel, Field

from muteki.extensions.installer import Source
from muteki.extensions.registry import ExtensionService
from muteki.platform.command_handlers.base import CommandAPIError
from muteki.platform.contracts.commands import ActorRef, CommandEnvelope, QueryEnvelope
from muteki.platform.contracts.errors import ErrorCategory, ErrorEnvelope
from muteki.platform.contracts.receipts import ReceiptState

#: 本地单操作员默认身份（与 apps/web 现有默认一致）。
DEFAULT_ACTOR = ActorRef(kind="operator", id="local-user")


class ExtensionSourceRequest(BaseModel):
    kind: str
    path: str = ""
    url: str = ""
    ref: str = ""
    sha256: str = ""
    catalog_root: str = ""
    extension_id: str = ""
    version: str = ""


class PreviewRequest(BaseModel):
    source: ExtensionSourceRequest
    extension_id: str = ""


class InstallRequest(BaseModel):
    source: ExtensionSourceRequest
    preview_id: str = ""
    confirmations: list[str] = Field(default_factory=list)


class EnableRequest(BaseModel):
    version: Optional[str] = None
    config: Optional[dict[str, Any]] = None


class UpgradeRequest(BaseModel):
    source: ExtensionSourceRequest
    config: Optional[dict[str, Any]] = None
    preview_id: str = ""
    confirmations: list[str] = Field(default_factory=list)


class RollbackRequest(BaseModel):
    version: Optional[str] = None


class InvokeRequest(BaseModel):
    command_type: str
    params: dict[str, Any] = {}


class UninstallRequest(BaseModel):
    confirm: bool = False
    preserve_state: bool = True
    preserve_logs: bool = True
    preserve_artifacts: bool = True


def create_extension_router(
    service: ExtensionService,
    api: Any,
    *,
    prefix: str = "/api/extensions",
    require_preview: bool = False,
) -> APIRouter:
    """构造扩展管理路由；状态修改一律 dispatch typed command。"""
    router = APIRouter(prefix=prefix)

    def _error_response(
        code: str,
        message: str,
        category: ErrorCategory,
        *,
        status_code: int,
    ) -> JSONResponse:
        error = ErrorEnvelope(code=code, message=message, category=category)
        return JSONResponse(
            {"error": error.model_dump(mode="json")}, status_code=status_code
        )

    async def _dispatch(command_type: str, aggregate_id: str,
                        payload: dict[str, Any]) -> JSONResponse:
        receipt = await api.dispatch(CommandEnvelope(
            command_type=command_type,
            aggregate_type="extension",
            aggregate_id=aggregate_id,
            actor=DEFAULT_ACTOR,
            payload=payload,
        ))
        body: dict[str, Any] = receipt.model_dump(mode="json")
        status = 200 if receipt.state is ReceiptState.COMPLETED else 202
        if receipt.state not in (ReceiptState.COMPLETED, ReceiptState.ACCEPTED):
            status = 409
        return JSONResponse({"receipt": body}, status_code=status)

    async def _query(query_type: str, **params: Any) -> Any:
        try:
            result = await api.query(QueryEnvelope(
                query_type=query_type,
                aggregate_type="extension",
                aggregate_id=str(params.get("extension_id") or "") or None,
                actor=DEFAULT_ACTOR,
                params={k: v for k, v in params.items() if v is not None},
            ))
        except CommandAPIError as exc:
            return JSONResponse(
                {"error": exc.error.model_dump(mode="json")}, status_code=400
            )
        return result.result

    # -- 查询 -------------------------------------------------------------------

    @router.get("")
    async def list_extensions() -> Any:
        return await _query("extension.list")

    @router.get("/{extension_id}")
    async def get_extension(extension_id: str) -> Any:
        return await _query("extension.get", extension_id=extension_id)

    @router.get("/{extension_id}/health")
    async def extension_health(extension_id: str) -> Any:
        return await _query("extension.health", extension_id=extension_id)

    @router.get("/{extension_id}/projection")
    async def extension_projection(extension_id: str, name: str = "") -> Any:
        return await _query(
            "extension.projection", extension_id=extension_id, name=name)

    @router.get("/{extension_id}/logs")
    async def extension_logs(extension_id: str, limit: int = 200) -> Any:
        return await _query(
            "extension.logs", extension_id=extension_id, limit=limit)

    # -- 生命周期（经 Command API） -------------------------------------------------

    @router.post("/preview")
    async def preview(body: PreviewRequest) -> Any:
        try:
            return service.preview_install(
                Source(**body.source.model_dump()),
                extension_id=body.extension_id,
            )
        except Exception as exc:
            code = str(getattr(exc, "code", "extension.preview_failed"))
            message = str(getattr(exc, "message", str(exc)))
            return _error_response(
                code, message, ErrorCategory.VALIDATION, status_code=422
            )

    @router.post("/install")
    async def install(body: InstallRequest) -> Any:
        if require_preview and not body.preview_id:
            return _error_response(
                "extension.preview_required",
                "create and confirm an install preview first",
                ErrorCategory.VALIDATION,
                status_code=400,
            )
        return await _dispatch("extension.install", "registry",
                               {"source": body.source.model_dump(),
                                "preview_id": body.preview_id,
                                "confirmations": body.confirmations})

    @router.post("/{extension_id}/enable")
    async def enable(extension_id: str, body: EnableRequest) -> Any:
        payload: dict[str, Any] = {"extension_id": extension_id}
        if body.version:
            payload["version"] = body.version
        if body.config is not None:
            payload["config"] = body.config
        return await _dispatch("extension.enable", extension_id, payload)

    @router.post("/{extension_id}/disable")
    async def disable(extension_id: str) -> Any:
        return await _dispatch(
            "extension.disable", extension_id, {"extension_id": extension_id})

    @router.post("/{extension_id}/upgrade")
    async def upgrade(extension_id: str, body: UpgradeRequest) -> Any:
        if require_preview and not body.preview_id:
            return _error_response(
                "extension.preview_required",
                "create and confirm an upgrade preview first",
                ErrorCategory.VALIDATION,
                status_code=400,
            )
        payload = {"extension_id": extension_id,
                   "source": body.source.model_dump(),
                   "preview_id": body.preview_id,
                   "confirmations": body.confirmations}
        if body.config is not None:
            payload["config"] = body.config
        return await _dispatch("extension.upgrade", extension_id, payload)

    @router.post("/{extension_id}/rollback")
    async def rollback(extension_id: str, body: RollbackRequest) -> Any:
        payload: dict[str, Any] = {"extension_id": extension_id}
        if body.version:
            payload["version"] = body.version
        return await _dispatch("extension.rollback", extension_id, payload)

    @router.delete("/{extension_id}")
    async def uninstall(
        extension_id: str, body: Optional[UninstallRequest] = None
    ) -> Any:
        request = body or UninstallRequest(confirm=not require_preview)
        if require_preview and not request.confirm:
            return _error_response(
                "extension.uninstall_confirmation_required",
                "uninstall requires explicit retention choices",
                ErrorCategory.VALIDATION,
                status_code=400,
            )
        return await _dispatch(
            "extension.uninstall", extension_id, {
                "extension_id": extension_id,
                "preserve_state": request.preserve_state,
                "preserve_logs": request.preserve_logs,
                "preserve_artifacts": request.preserve_artifacts,
            })

    @router.post("/{extension_id}/invoke")
    async def invoke(extension_id: str, body: InvokeRequest) -> Any:
        return await _dispatch("extension.command", extension_id, {
            "extension_id": extension_id,
            "command_type": body.command_type,
            "params": body.params,
        })

    return router


__all__ = ["create_extension_router"]
