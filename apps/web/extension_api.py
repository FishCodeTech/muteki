"""EXT-02：Extension 设置页的后端薄适配 / 聚合层（任务书 13.4、EXT-02）。

EXT-01 的 ``muteki/extensions/api.py:create_extension_router`` 已覆盖扩展的
生命周期命令与基础查询（全部状态修改经 MutekiCommandAPI，返回
CommandReceipt）。本模块在它之上做 EXT-02 需要的聚合与适配，不复制权限与
生命周期判定：

- ``GET  /api/extensions/overview``：设置页卡片聚合——record + manifest 摘要
  （Agent Plugin schema、来源、权限、provides、是否声明 UI）+ 启用中扩展的实时健康探测；
- ``GET  /api/extensions/{id}/detail``：record + 活动版本 plugin.json 解析视图 +
  config schema + 状态迁移 receipt + 运行时 capabilities；
- ``GET  /api/extensions/{id}/ui``：Muteki 客户端命名空间声明的 UI Contribution
  （``plugin.json`` 中的 ``ui`` 指向 JSON，只读这个声明文件）；
- ``GET  /api/extensions/{id}/logs/page``：归档日志按尾部分页
  （offset 从最新一行往回数）；
- ``POST /api/extensions/{id}/health/refresh``：实时健康探测（只读，
  不改状态；监管与自动回滚在 ExtensionService.check_health / 巡检里）。

挂载形态：``create_extension_router(service, api)`` 返回完整 router（先注册
本模块的聚合路由，再 include EXT-01 router，保证 ``/overview`` 不被
``/{extension_id}`` 抢先匹配），由 INTEG-01 挂到 ``apps/web/server.py``；
本地验证可用 ``python -m apps.web.extension_api`` 启动一个挂在完整 Web
应用之上的开发实例。
"""

from __future__ import annotations

import logging
from pathlib import Path
from typing import Any, Optional

from fastapi import APIRouter, Query
from fastapi.responses import JSONResponse
from pydantic import BaseModel, Field

from muteki.extensions.api import create_extension_router as _create_core_router
from muteki.extensions.catalog import CatalogError, ExtensionCatalog
from muteki.extensions.handlers import register_extension_handlers
from muteki.extensions.manifest import ManifestError, load_schema
from muteki.extensions.ui import UIContributionError, load_ui_contributions
from muteki.extensions.permissions import EnvironmentSecretResolver
from muteki.extensions.registry import ExtensionError, ExtensionService
from muteki.platform.command_api import MutekiCommandApiImpl
from muteki.platform.contracts.errors import ErrorCategory, ErrorEnvelope
from muteki.platform.store import PlatformStore

LOG = logging.getLogger(__name__)

#: 日志分页的单页上限（归档日志保留完整内容，见 EXT-01 host.py）。
MAX_LOG_PAGE = 1000

#: 日志分页读取上限行数（service.logs 本身读全量再截尾，分页在路由层做）。
_LOG_READ_CAP = 100_000


class InvokeBody(BaseModel):
    """扩展业务命令调用请求（与 EXT-01 的 InvokeRequest 同构）。"""

    command_type: str
    params: dict[str, Any] = Field(default_factory=dict)


class ExtensionWebStack:
    """Web 侧扩展栈：PlatformStore + ExtensionService + MutekiCommandAPI。

    与 ``muteki/extensions/cli.py`` 的单进程栈同构，但供长驻 Web 进程使用：
    持有 service 句柄以便应用关闭时停掉全部扩展子进程（关闭顺序见 INTEG-01）。
    """

    def __init__(self, root: str | Path, *, workspace_root: str | Path | None = None,
                 monitor_interval: Optional[float] = 30.0) -> None:
        root = Path(root)
        root.mkdir(parents=True, exist_ok=True)
        self.store = PlatformStore(db_path=root / "platform.db")
        self.service = ExtensionService(
            self.store,
            install_root=root / "extensions" / "installed",
            state_root=root / "extensions" / "state",
            workspace_root=workspace_root or Path.cwd(),
            secret_resolver=EnvironmentSecretResolver(),
            monitor_interval=monitor_interval,
        )
        self.api = MutekiCommandApiImpl(self.store)
        register_extension_handlers(self.api, self.service)

    async def shutdown(self) -> None:
        """停止巡检与全部扩展子进程。"""
        await self.service.shutdown()


def _manifest_summary(service: ExtensionService, record: Any) -> dict[str, Any]:
    """活动（或最新已装）版本 manifest 的设置页摘要；读不出来时如实报错。"""
    try:
        manifest = service.manifest_of(record.extension_id)
    except (ExtensionError, ManifestError) as exc:
        return {"error": str(exc)}
    version = record.active_version or (record.installed_versions[-1]
                                        if record.installed_versions else "")
    install_dir = record.install_dirs.get(version, "")
    return {
        "id": manifest.id,
        "version": manifest.version,
        "plugin_version": manifest.plugin_version,
        "description": manifest.description,
        "author": manifest.author,
        "homepage": manifest.homepage,
        "plugin_schema": manifest.plugin_schema,
        "client_namespace": manifest.client_namespace,
        "origin": manifest.origin,
        "requires_core": manifest.requires_core,
        "entrypoints": manifest.entrypoints.model_dump(mode="json"),
        "provides": [p.model_dump(mode="json") for p in manifest.provides],
        "requires": [r.model_dump(mode="json") for r in manifest.requires],
        "permissions": manifest.permissions.model_dump(mode="json"),
        "has_ui": bool(manifest.ui),
        "has_config_schema": bool(manifest.config_schema),
        "install_dir": install_dir,
        "verification": (
            record.installations.get(version, {}).get("verification", {})),
        "portable_components": (
            record.installations.get(version, {})
            .get("verification", {})
            .get("agent_plugin", {})
            .get("components", {})
        ),
        "registry_state": (
            "enabled" if record.enabled and record.state.value == "ready"
            else "unavailable" if record.state.value == "unavailable"
            else "installed"),
    }


async def _live_health(service: ExtensionService, record: Any) -> Optional[dict[str, Any]]:
    """启用中的扩展做一次实时健康探测；未启用返回 None（不探测）。"""
    if not record.enabled:
        return None
    try:
        return await service.health(record.extension_id)
    except Exception as exc:  # 单扩展探测失败不阻断整页（与 RUNTIME-05 同语义）
        LOG.warning("health probe failed for %s: %s", record.extension_id, exc)
        return {"status": "error", "detail": str(exc)}


def _read_ui_contributions(service: ExtensionService,
                           record: Any) -> dict[str, Any]:
    """读取 plugin.json 的 Muteki 命名空间声明的 UI Contribution JSON。

    只读 Muteki ``ui`` 字段声明的相对路径（路径安全性已在安装/启用时由
    validate_manifest 校验），内容必须是 JSON object；解析失败如实返回错误，
    不静默兜底。
    """
    manifest = service.manifest_of(record.extension_id)
    if not manifest.ui:
        return {"declared": False, "contributions": {}}
    version = record.active_version or record.installed_versions[-1]
    try:
        data = load_ui_contributions(
            record.install_dirs[version], manifest.ui)
    except UIContributionError as exc:
        raise ExtensionError(
            exc.code, exc.message,
        ) from exc
    return {
        "declared": True,
        "file": manifest.ui,
        "schema_version": data.get("schema_version"),
        "source": {
            "extension_id": manifest.id,
            "version": manifest.version,
            "permissions": manifest.permissions.model_dump(mode="json"),
        },
        "contributions": data,
    }


def _read_config_schema(service: ExtensionService,
                        record: Any) -> Optional[dict[str, Any]]:
    """读取活动版本的 config schema（设置表单的数据源）；未声明返回 None。"""
    manifest = service.manifest_of(record.extension_id)
    if not manifest.config_schema:
        return None
    version = record.active_version or record.installed_versions[-1]
    return load_schema(record.install_dirs[version], manifest.config_schema)


def create_extension_router(
    service: ExtensionService, api: Any, *, prefix: str = "/api/extensions"
) -> APIRouter:
    """EXT-02 完整扩展路由：聚合/适配路由 + EXT-01 核心 router。

    注意顺序：聚合路由先注册，``/overview`` 才不会被 EXT-01 的
    ``GET /{extension_id}`` 抢先匹配。
    """
    router = APIRouter()

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

    def _record_or_error(extension_id: str) -> tuple[Any, Optional[JSONResponse]]:
        record = service.get_record(extension_id)
        if record is None:
            return None, _error_response(
                "extension.not_found",
                f"extension not installed: {extension_id}",
                ErrorCategory.NOT_FOUND,
                status_code=404,
            )
        return record, None

    @router.get(f"{prefix}/overview")
    async def overview() -> Any:
        """设置页卡片聚合：record + manifest 摘要 + 实时健康。"""
        cards: list[dict[str, Any]] = []
        for record in service.list_records():
            cards.append({
                "record": record.model_dump(mode="json"),
                "manifest": _manifest_summary(service, record),
                "live_health": await _live_health(service, record),
                "migrations": service.migration_receipts(record.extension_id),
            })
        return {"extensions": cards, "install_policy": service.install_policy}

    @router.get(f"{prefix}/catalog")
    async def catalog(
        root: str = Query(default=""),
        q: str = Query(default="", max_length=200),
    ) -> Any:
        """浏览本地管理员配置的 Catalog；只返回声明元数据。"""
        catalog_root = root.strip()
        if not catalog_root:
            import os
            catalog_root = os.environ.get("MUTEKI_EXTENSION_CATALOG", "").strip()
        if not catalog_root:
            return {"root": "", "entries": [], "detail": "未配置 Catalog"}
        try:
            entries = ExtensionCatalog(catalog_root).list()
        except CatalogError as exc:
            return _error_response(
                "extension.catalog_unavailable",
                str(exc),
                ErrorCategory.VALIDATION,
                status_code=422,
            )
        needle = q.strip().lower()
        if needle:
            entries = [item for item in entries if needle in (
                f"{item.id} {item.version} {item.description}".lower())]
        return {
            "root": catalog_root,
            "entries": [item.model_dump(mode="json") for item in entries],
        }

    @router.get(prefix + "/{extension_id}/detail")
    async def detail(extension_id: str) -> Any:
        record, error = _record_or_error(extension_id)
        if error is not None:
            return error
        try:
            manifest = service.manifest_of(extension_id).model_dump(mode="json")
            manifest_error = ""
        except (ExtensionError, ManifestError) as exc:
            manifest, manifest_error = None, str(exc)
        try:
            config_schema = _read_config_schema(service, record)
            schema_error = ""
        except (ExtensionError, ManifestError) as exc:
            config_schema, schema_error = None, str(exc)
        return {
            "record": record.model_dump(mode="json"),
            "manifest": manifest,
            "manifest_error": manifest_error,
            "config_schema": config_schema,
            "config_schema_error": schema_error,
            "migrations": service.migration_receipts(extension_id),
            "live_health": await _live_health(service, record),
        }

    @router.get(prefix + "/{extension_id}/ui")
    async def ui_contributions(extension_id: str) -> Any:
        record, error = _record_or_error(extension_id)
        if error is not None:
            return error
        try:
            return _read_ui_contributions(service, record)
        except ExtensionError as exc:
            return _error_response(
                exc.code,
                exc.message,
                ErrorCategory.VALIDATION,
                status_code=422,
            )

    @router.get(prefix + "/{extension_id}/logs/page")
    async def logs_page(
        extension_id: str,
        offset: int = Query(default=0, ge=0),
        limit: int = Query(default=200, ge=1, le=MAX_LOG_PAGE),
    ) -> Any:
        """归档日志尾部分页：offset=0 是最新一页，has_more 表示还有更早的行。"""
        _record, error = _record_or_error(extension_id)
        if error is not None:
            return error
        lines = service.logs(extension_id, limit=_LOG_READ_CAP)
        total = len(lines)
        end = max(0, total - offset)
        start = max(0, end - limit)
        return {
            "lines": lines[start:end],
            "total": total,
            "offset": offset,
            "limit": limit,
            "has_more": start > 0,
        }

    @router.post(prefix + "/{extension_id}/health/refresh")
    async def health_refresh(extension_id: str) -> Any:
        """实时健康探测（只读）。自动回滚由启用流程与后台巡检负责。"""
        record, error = _record_or_error(extension_id)
        if error is not None:
            return error
        health = await _live_health(service, record)
        return {
            "extension_id": extension_id,
            "state": record.state.value,
            "enabled": record.enabled,
            "health": health if health is not None else {
                "status": "unavailable",
                "detail": "extension is not enabled",
            },
        }

    @router.post(prefix + "/{extension_id}/invoke")
    async def invoke_with_result(extension_id: str, body: InvokeBody) -> Any:
        """调用扩展业务命令并回读业务结果。

        命令本身仍经 MutekiCommandAPI dispatch（与 EXT-01 的 /invoke 同一
        命令类型、同一 Handler）；CommandReceipt 不携带业务结果，结果写在
        ``core.extension.command.completed`` 事件里，这里按 command_id 回读，
        让命令表单能直接展示扩展返回值。注册顺序在 EXT-01 路由之前，本路由
        覆盖同路径的核心版本。
        """
        from muteki.extensions.api import DEFAULT_ACTOR
        from muteki.platform.contracts.commands import (
            CommandEnvelope,
            EventReadRequest,
        )
        from muteki.platform.contracts.receipts import ReceiptState

        receipt = await api.dispatch(CommandEnvelope(
            command_type="extension.command",
            aggregate_type="extension",
            aggregate_id=extension_id,
            actor=DEFAULT_ACTOR,
            payload={
                "extension_id": extension_id,
                "command_type": body.command_type,
                "params": body.params,
            },
        ))
        body_out: dict[str, Any] = receipt.model_dump(mode="json")
        if receipt.state not in (ReceiptState.COMPLETED, ReceiptState.ACCEPTED):
            return JSONResponse({"receipt": body_out}, status_code=409)
        # 回读本命令产生的结果事件（completed 在前，accepted 无结果）。
        result: Any = None
        if receipt.state is ReceiptState.COMPLETED:
            page = await api.read_events(
                EventReadRequest(
                    aggregate_type="extension",
                    aggregate_id=extension_id,
                    limit=200,
                ),
                principal=DEFAULT_ACTOR,
            )
            for event in reversed(page.events):
                if (event.command_id == receipt.command_id
                        and event.event_type == "core.extension.command.completed"):
                    result = (event.payload or {}).get("result")
                    break
        return {"receipt": body_out, "result": result}

    # EXT-01 核心路由（生命周期命令 + 基础查询）放最后注册。
    router.include_router(_create_core_router(
        service, api, prefix=prefix, require_preview=True))
    return router


def create_extension_stack_router(
    root: str | Path, *, prefix: str = "/api/extensions", **stack_kwargs: Any
) -> tuple[APIRouter, ExtensionWebStack]:
    """一次性构造扩展栈 + 完整 router（INTEG-01 挂载入口）。"""
    stack = ExtensionWebStack(root, **stack_kwargs)
    return create_extension_router(stack.service, stack.api, prefix=prefix), stack


__all__ = [
    "ExtensionWebStack",
    "create_extension_router",
    "create_extension_stack_router",
]


# ---------------------------------------------------------------------------
# 本地开发 / 验证入口：完整 Web 应用 + 扩展 router（不改动 server.py 本身；
# 生产挂载由 INTEG-01 在 create_app 里做同一件事）。
# ---------------------------------------------------------------------------


def _dev_app(root: str | Path, **stack_kwargs: Any):  # pragma: no cover
    from apps.web.server import create_app

    app = create_app()
    router, stack = create_extension_stack_router(root, **stack_kwargs)
    app.include_router(router)
    app.state.extension_stack = stack

    @app.on_event("shutdown")
    async def _shutdown_extensions() -> None:
        await stack.shutdown()

    return app


if __name__ == "__main__":  # pragma: no cover
    import os

    import uvicorn

    port = int(os.environ.get("MUTEKI_EXT_DEV_PORT", "8000"))
    uvicorn.run(
        _dev_app(Path(os.environ.get(
            "MUTEKI_EXT_ROOT", "state/_ext_dev/extensions"))),
        host=os.environ.get("MUTEKI_EXT_DEV_HOST", "127.0.0.1"),
        port=port,
    )
