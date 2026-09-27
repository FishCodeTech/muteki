"""Competition Web API router 工厂（任务书 10.9，设计 14.3/16.2，COMP-09）。

端点清单（10.9 逐字对齐）::

    GET  /api/platform-connections
    POST /api/platform-connections
    POST /api/platform-connections/{id}/probe
    GET  /api/competitions
    POST /api/competitions
    GET  /api/competitions/{id}
    GET  /api/competitions/{id}/events       （SSE：当前 snapshot + watermark）
    GET  /api/competitions/{id}/events/history （按需倒序分页的历史事件）
    POST /api/competitions/{id}/commands
    POST /api/competitions/{id}/messages
    GET  /api/competitions/{id}/challenges
    GET  /api/competitions/{id}/submissions
    GET  /api/competitions/{id}/leases

口径（任务书 10.9 / 设计 16.2）：

- 所有写操作进入 ``MutekiCommandAPI.dispatch``；``/commands`` 与专用端点
  只构造 typed command，不直接调用 CompetitionManager / Scheduler /
  SubmissionService，也不修改 projection；
- 同步、批量下发、实例、提交等命令立即返回异步 receipt（含 run_id 与
  event cursor，``next`` 声明 receipt/snapshot/events/wait 跟踪方式）；
- 查询走 ``command_api.query``（与能力工具同一授权边界）；SSE 只推送
  当前 snapshot，合并每秒内的变化，历史事件不进入实时看板；
- 事件经 ``CompetitionPublicEventAdapter`` 转换后完整下发（
  无 secret 引用本体 / 实例凭据 / 宿主绝对路径 / 候选 Flag 原文；
  子 Run 链接 run_id 提升为顶层字段）。

router 工厂由 INTEG-01 挂载到 ``apps/web/server.py``；本模块不修改
server.py。
"""

from __future__ import annotations

import asyncio
import json
import logging
from dataclasses import asdict
from typing import Any, Literal, Optional

from pydantic import BaseModel, Field, SecretStr

try:  # FastAPI 注解需模块级可解析（__future__ annotations 下按模块 globals 求值）
    from fastapi import Request
except ImportError:  # pragma: no cover - 非 Web 环境仅导入类型名
    Request = Any  # type: ignore[assignment]

from muteki.competition.commands import (
    COMPETITION_COMMAND_TYPES,
    CompetitionCommandApi,
)
from muteki.competition.models import Competition, PlatformConnection
from muteki.competition.secrets import PlatformSecretError
from muteki.competition.public_events import CompetitionPublicEventAdapter
from muteki.competition.store import CompetitionStore
from muteki.platform.command_handlers.base import CommandAPIError
from muteki.platform.contracts.commands import (
    ActorRef,
    CommandEnvelope,
    QueryEnvelope,
)
from muteki.platform.contracts.base import new_id
from muteki.platform.contracts.errors import ErrorCategory, ErrorEnvelope
from muteki.platform.contracts.receipts import CommandReceipt, ReceiptState

LOG = logging.getLogger(__name__)

#: 本地 Operator 主体（单操作员产品默认；与 agent_runtime_api 同口径）。
OPERATOR = ActorRef(kind="operator", id="local-user")

#: SSE 快照事件名 / 比赛领域事件名（前端 competition-events.ts 的消费约定）。
SSE_SNAPSHOT_EVENT = "snapshot"
SSE_COMPETITION_EVENT = "competition"


class CreatePlatformConnectionBody(BaseModel):
    """连接创建请求；credential 只在本次请求内使用。"""

    command_id: str = ""
    idempotency_key: str = ""
    platform_kind: str = ""
    platform: str = ""
    base_url: str
    account_key: str
    credential_ref: str = ""
    credential: Optional[SecretStr] = None
    credential_key: str = Field(default="token", pattern=r"^[A-Za-z0-9][A-Za-z0-9_-]{0,127}$")


class PlatformSecretWriteBody(BaseModel):
    command_id: str = ""
    idempotency_key: str = ""
    key: str = Field(default="token", pattern=r"^[A-Za-z0-9][A-Za-z0-9_-]{0,127}$")
    credential: SecretStr
    mode: Literal["update", "rotate"] = "rotate"
    expires_at: str = ""


class PlatformSecretRevokeBody(BaseModel):
    command_id: str = ""
    idempotency_key: str = ""
    confirm: bool = False


class BrowserStorageStateBody(BaseModel):
    command_id: str = ""
    idempotency_key: str = ""
    storage_state: dict[str, Any] = Field(default_factory=dict)
    expires_at: str = ""
    renew: bool = False


class ConnectionProbeBody(BaseModel):
    command_id: str = ""
    idempotency_key: str = ""


class ConnectionUnregisterBody(BaseModel):
    command_id: str = ""
    idempotency_key: str = ""
    confirm: bool = False


class CreateCompetitionBody(BaseModel):
    command_id: str = ""
    idempotency_key: str = ""
    connection_id: str = Field(min_length=1)
    external_competition_id: str = Field(min_length=1)
    title: str = ""
    description: str = ""
    starts_at: str = ""
    ends_at: str = ""


class CompetitionMessageBody(BaseModel):
    text: str = ""
    author: str = ""
    command: Optional[dict[str, Any]] = None
    command_id: str = ""
    idempotency_key: str = ""


class CompetitionUnregisterBody(BaseModel):
    command_id: str = ""
    idempotency_key: str = ""
    confirm: bool = False


def _receipt_status(receipt: CommandReceipt) -> int:
    """回执终态 → HTTP 状态码（与 agent_runtime_api 同语义）。"""
    if receipt.state is ReceiptState.COMPLETED:
        return 200
    if receipt.state in {
        ReceiptState.ACCEPTED, ReceiptState.RUNNING, ReceiptState.WAITING,
    }:
        return 202
    if receipt.state is ReceiptState.CONFLICT:
        return 409
    category = getattr(getattr(receipt, "error", None), "category", None)
    if category is ErrorCategory.NOT_FOUND:
        return 404
    if category is ErrorCategory.VALIDATION:
        return 400
    if category is ErrorCategory.PERMISSION:
        return 403
    if category is ErrorCategory.STATE:
        return 409
    return 502


def _receipt_body(receipt: CommandReceipt) -> dict[str, Any]:
    return {"receipt": receipt.model_dump(mode="json")}


def _error_status(error: ErrorEnvelope) -> int:
    """统一错误 envelope → HTTP 状态码。"""
    return {
        ErrorCategory.VALIDATION: 400,
        ErrorCategory.NOT_FOUND: 404,
        ErrorCategory.PERMISSION: 403,
        ErrorCategory.CONFLICT: 409,
        ErrorCategory.STATE: 409,
        ErrorCategory.RATE_LIMIT: 429,
    }.get(error.category, 500)


def create_competition_router(
    *,
    command_api: CompetitionCommandApi,
    secret_store: Any = None,
    platform_adapter_factory: Any = None,
    on_secret_changed: Any = None,
    actor: ActorRef = OPERATOR,
    sse_poll_seconds: float = 0.5,
    sse_page_limit: int = 200,
    sse_metrics: Any = None,
) -> Any:
    """构造比赛 API router（10.9 全部端点）。

    ``command_api`` 必须是绑定 ``CompetitionStore`` 的
    ``CompetitionCommandApi``（receipt / 事件 / outbox 落在
    competition.db）。
    """
    from fastapi import APIRouter, Body, Query
    from fastapi.responses import JSONResponse
    from sse_starlette.sse import EventSourceResponse

    store: CompetitionStore = command_api.competition_store
    public_events = CompetitionPublicEventAdapter()

    router = APIRouter(tags=["competitions"])

    @router.get("/api/platform-kinds")
    async def list_platform_kinds() -> Any:
        """builtin ∪ 已启用扩展 platform-adapter，供大厅平台下拉。"""
        if platform_adapter_factory is not None and hasattr(
            platform_adapter_factory, "list_kinds"
        ):
            return {"kinds": platform_adapter_factory.list_kinds()}
        from muteki.competition.models import PlatformKind
        return {
            "kinds": [
                {
                    "id": kind.value,
                    "label": kind.value,
                    "origin": "builtin",
                    "source": "builtin",
                    "state": "ready",
                }
                for kind in PlatformKind
            ]
        }

    # -- 内部工具 -------------------------------------------------------------

    def _command(
        command_type: str,
        aggregate_type: str,
        aggregate_id: str,
        payload: dict[str, Any],
        *,
        command_id: str = "",
        idempotency_key: str = "",
        expected_version: Optional[int] = None,
    ) -> CommandEnvelope:
        fields: dict[str, Any] = {
            "command_type": command_type,
            "aggregate_type": aggregate_type,
            "aggregate_id": aggregate_id,
            "actor": actor,
            "payload": payload,
        }
        if command_id:
            fields["command_id"] = command_id
        if idempotency_key:
            fields["idempotency_key"] = idempotency_key
        if expected_version is not None:
            fields["expected_version"] = expected_version
        return CommandEnvelope(**fields)

    async def _dispatch(envelope: CommandEnvelope) -> JSONResponse:
        receipt = await command_api.dispatch(envelope)
        return JSONResponse(
            _receipt_body(receipt), status_code=_receipt_status(receipt))

    def _error_response(
        code: str,
        message: str,
        category: ErrorCategory,
        *,
        status_code: Optional[int] = None,
        recovery_hint: str = "",
        detail: Optional[dict[str, Any]] = None,
    ) -> JSONResponse:
        error = ErrorEnvelope(
            code=code,
            message=message,
            category=category,
            recovery_hint=recovery_hint,
            detail=detail or {},
        )
        return JSONResponse(
            {"error": error.model_dump(mode="json")},
            status_code=status_code or _error_status(error),
        )

    async def _query(query_type: str, **params: Any) -> Any:
        """查询统一走 Command API（授权边界一致）；错误映射为 HTTP。"""
        try:
            result = await command_api.query(QueryEnvelope(
                query_type=query_type,
                aggregate_type=(
                    "connection" if query_type.startswith("connection.")
                    else "competition"),
                aggregate_id=str(params.get("competition_id") or "") or None,
                actor=actor,
                params={k: v for k, v in params.items() if v not in (None, "")},
            ))
        except CommandAPIError as exc:
            return JSONResponse(
                {"error": exc.error.model_dump(mode="json")},
                status_code=_error_status(exc.error))
        return result.result

    def _require_competition(competition_id: str) -> Optional[JSONResponse]:
        if store.get(Competition, competition_id) is None:
            return _error_response(
                "competition.competition.not_found",
                f"unknown competition {competition_id!r}",
                ErrorCategory.NOT_FOUND,
            )
        return None

    def _typed_command(
        competition_id: str, body: dict[str, Any]
    ) -> CommandEnvelope | JSONResponse:
        """/commands 与 /messages 共用的 typed command 构造。

        只接受比赛命令命名空间（COMPETITION_COMMAND_TYPES）；路由本身不
        产生任何状态变化，全部经 dispatch。
        """
        command_type = str(
            body.get("command_type") or body.get("kind") or "").strip()
        if command_type not in COMPETITION_COMMAND_TYPES:
            return _error_response(
                "competition.command.unsupported",
                f"unsupported competition command_type {command_type!r}",
                ErrorCategory.VALIDATION,
            )
        payload = dict(body.get("payload") or {})
        # 路径限定的比赛 id 作为 payload 缺省（显式给出的优先）。
        payload.setdefault("competition_id", competition_id)
        return _command(
            command_type,
            str(body.get("aggregate_type") or "competition").strip(),
            str(body.get("aggregate_id") or competition_id).strip(),
            payload,
            command_id=str(body.get("command_id") or "").strip(),
            idempotency_key=str(body.get("idempotency_key") or "").strip(),
            expected_version=(
                int(body["expected_version"])
                if body.get("expected_version") is not None else None),
        )

    # -- 平台连接 -------------------------------------------------------------

    @router.get("/api/platform-connections")
    async def list_connections() -> Any:
        return await _query("connection.list")

    @router.post("/api/platform-connections")
    async def create_connection(body: CreatePlatformConnectionBody) -> Any:
        values = body.model_dump()
        command_id = str(values.pop("command_id", "") or "")
        idem = str(values.pop("idempotency_key", "") or "")
        platform_kind = str(
            values.get("platform_kind") or values.get("platform") or "").strip()
        payload = {
            "platform_kind": platform_kind,
            "base_url": str(values.get("base_url") or "").strip(),
            "account_key": str(values.get("account_key") or "").strip(),
        }
        if values.get("credential_ref"):
            payload["credential_ref"] = str(values["credential_ref"]).strip()
        receipt = await command_api.dispatch(_command(
            "connection.create", "connection", "", payload,
            command_id=command_id, idempotency_key=idem))
        credential = body.credential
        if credential is None or receipt.state is not ReceiptState.COMPLETED:
            return JSONResponse(
                _receipt_body(receipt), status_code=_receipt_status(receipt))
        if body.credential_ref:
            return _error_response(
                "competition.secret.input_conflict",
                "provide credential or credential_ref, not both",
                ErrorCategory.VALIDATION,
            )
        if secret_store is None:
            return _error_response(
                "competition.secret.store_unavailable",
                "platform SecretStore is unavailable",
                ErrorCategory.RUNTIME,
                status_code=503,
            )
        reference = secret_store.put(
            receipt.aggregate.id,
            body.credential_key,
            credential.get_secret_value(),
        )
        credential_receipt = await command_api.dispatch(_command(
            "connection.credential.set", "connection", receipt.aggregate.id,
            {"connection_id": receipt.aggregate.id,
             "credential_ref": reference},
            command_id=f"{receipt.command_id}:credential",
            idempotency_key=(f"{idem}:credential" if idem else ""),
        ))
        return JSONResponse({
            "receipt": credential_receipt.model_dump(mode="json"),
            "connection_receipt": receipt.model_dump(mode="json"),
            "secret": asdict(secret_store.get(reference)),
        }, status_code=_receipt_status(credential_receipt))

    @router.get("/api/platform-secrets")
    async def list_platform_secrets(connection_id: str = "") -> Any:
        if secret_store is None:
            return _error_response(
                "competition.secret.store_unavailable",
                "platform SecretStore is unavailable",
                ErrorCategory.RUNTIME,
                status_code=503,
            )
        return [asdict(item) for item in secret_store.list(
            connection_id or None)]

    @router.get("/api/platform-secrets/inspect")
    async def inspect_platform_secret(reference: str) -> Any:
        if secret_store is None:
            return _error_response(
                "competition.secret.store_unavailable",
                "platform SecretStore is unavailable",
                ErrorCategory.RUNTIME,
                status_code=503,
            )
        try:
            return asdict(secret_store.get(reference))
        except PlatformSecretError as exc:
            return _error_response(
                "competition.secret.not_found",
                str(exc),
                ErrorCategory.NOT_FOUND,
            )

    @router.put("/api/platform-connections/{connection_id}/secret")
    async def rotate_platform_secret(
        connection_id: str, body: PlatformSecretWriteBody
    ) -> Any:
        connection = store.get(PlatformConnection, connection_id)
        if connection is None:
            return _error_response(
                "competition.connection.not_found",
                f"unknown connection {connection_id!r}",
                ErrorCategory.NOT_FOUND,
            )
        if secret_store is None:
            return _error_response(
                "competition.secret.store_unavailable",
                "platform SecretStore is unavailable",
                ErrorCategory.RUNTIME,
                status_code=503,
            )
        old_reference = str(connection.credential_ref or "")
        if body.mode == "update" and old_reference:
            metadata = secret_store.get(old_reference)
            reference = secret_store.put(
                connection_id,
                metadata.key,
                body.credential.get_secret_value(),
                expires_at=body.expires_at,
            )
        elif old_reference:
            metadata = secret_store.rotate(
                old_reference,
                body.credential.get_secret_value(),
                key=body.key,
                expires_at=body.expires_at,
                revoke_old=False,
            )
            reference = metadata.reference
        else:
            reference = secret_store.put(
                connection_id,
                body.key,
                body.credential.get_secret_value(),
                expires_at=body.expires_at,
            )
        receipt = await command_api.dispatch(_command(
            "connection.credential.set", "connection", connection_id,
            {"connection_id": connection_id, "credential_ref": reference},
            command_id=body.command_id,
            idempotency_key=body.idempotency_key,
        ))
        change_effect: dict[str, Any] = {}
        if (receipt.state is ReceiptState.COMPLETED
                and old_reference and reference != old_reference):
            secret_store.delete(old_reference)
        elif receipt.state is not ReceiptState.COMPLETED and reference != old_reference:
            secret_store.delete(reference)
        if receipt.state is ReceiptState.COMPLETED and on_secret_changed is not None:
            change_effect = await on_secret_changed(
                old_reference, reference, body.mode)
        secret_metadata = (
            asdict(secret_store.get(reference))
            if receipt.state is ReceiptState.COMPLETED else None
        )
        return JSONResponse({
            "receipt": receipt.model_dump(mode="json"),
            "secret": secret_metadata,
            "previous_reference_invalidated": (
                old_reference if (receipt.state is ReceiptState.COMPLETED
                                  and reference != old_reference) else ""
            ),
            "change_effect": change_effect,
        }, status_code=_receipt_status(receipt))

    @router.delete("/api/platform-connections/{connection_id}/secret")
    async def revoke_platform_secret(
        connection_id: str, body: PlatformSecretRevokeBody
    ) -> Any:
        connection = store.get(PlatformConnection, connection_id)
        if connection is None:
            return _error_response(
                "competition.connection.not_found",
                f"unknown connection {connection_id!r}",
                ErrorCategory.NOT_FOUND,
            )
        if not body.confirm:
            return _error_response(
                "competition.secret.confirmation_required",
                "credential revocation requires confirm=true",
                ErrorCategory.VALIDATION,
            )
        if secret_store is None or not connection.credential_ref:
            return _error_response(
                "competition.secret.not_found",
                "this connection has no active credential",
                ErrorCategory.NOT_FOUND,
            )
        reference = connection.credential_ref
        receipt = await command_api.dispatch(_command(
            "connection.credential.revoke", "connection", connection_id,
            {"connection_id": connection_id, "credential_ref": reference},
            command_id=body.command_id,
            idempotency_key=body.idempotency_key,
        ))
        if receipt.state is ReceiptState.COMPLETED:
            secret_store.delete(reference)
            change_effect = (
                await on_secret_changed(reference, "", "revoked")
                if on_secret_changed is not None else {})
        else:
            change_effect = {}
        return JSONResponse({
            "receipt": receipt.model_dump(mode="json"),
            "revoked_reference": reference,
            "change_effect": change_effect,
        }, status_code=_receipt_status(receipt))

    @router.get("/api/platform-connections/{connection_id}/browser-session")
    async def browser_session_status(connection_id: str) -> Any:
        connection = store.get(PlatformConnection, connection_id)
        if connection is None:
            return _error_response(
                "competition.connection.not_found",
                f"unknown connection {connection_id!r}",
                ErrorCategory.NOT_FOUND,
            )
        if platform_adapter_factory is None:
            return _error_response(
                "competition.connection.browser_service_unavailable",
                "browser Adapter factory is unavailable",
                ErrorCategory.RUNTIME,
                status_code=503,
            )
        try:
            adapter = platform_adapter_factory.for_connection(connection)
            status = adapter.storage_state_status(
                platform_adapter_factory.connection_ref(connection))
        except Exception as exc:
            return _error_response(
                "competition.connection.browser_session_unavailable",
                f"{type(exc).__name__}: {exc}",
                ErrorCategory.RUNTIME,
                status_code=409,
            )
        return {"connection_id": connection_id, "browser_session": status}

    @router.put("/api/platform-connections/{connection_id}/browser-session")
    async def import_browser_session(
        connection_id: str, body: BrowserStorageStateBody
    ) -> Any:
        connection = store.get(PlatformConnection, connection_id)
        if connection is None:
            return _error_response(
                "competition.connection.not_found",
                f"unknown connection {connection_id!r}",
                ErrorCategory.NOT_FOUND,
            )
        if secret_store is None:
            return _error_response(
                "competition.secret.store_unavailable",
                "platform SecretStore is unavailable",
                ErrorCategory.RUNTIME,
                status_code=503,
            )
        command_id = body.command_id or new_id("cmd")
        state_ref = ""
        if not body.renew:
            staging_key = new_id("browser_state").replace("-", "_")
            state_ref = secret_store.put(
                connection_id,
                staging_key,
                json.dumps(body.storage_state, ensure_ascii=False),
            )
        receipt = await command_api.dispatch(_command(
            ("connection.browser_session.renew" if body.renew
             else "connection.browser_session.import"),
            "connection", connection_id,
            {
                "connection_id": connection_id,
                "state_ref": state_ref,
                "expires_at": body.expires_at,
            },
            command_id=command_id,
            idempotency_key=body.idempotency_key,
        ))
        return JSONResponse(
            _receipt_body(receipt), status_code=_receipt_status(receipt))

    @router.delete("/api/platform-connections/{connection_id}/browser-session")
    async def revoke_browser_session(
        connection_id: str, body: PlatformSecretRevokeBody
    ) -> Any:
        if not body.confirm:
            return _error_response(
                "competition.connection.browser_confirmation_required",
                "browser session revocation requires confirm=true",
                ErrorCategory.VALIDATION,
            )
        receipt = await command_api.dispatch(_command(
            "connection.browser_session.revoke",
            "connection", connection_id,
            {"connection_id": connection_id},
            command_id=body.command_id,
            idempotency_key=body.idempotency_key,
        ))
        return JSONResponse(
            _receipt_body(receipt), status_code=_receipt_status(receipt))

    @router.post("/api/platform-connections/{connection_id}/probe")
    async def probe_connection(
        connection_id: str,
        body: ConnectionProbeBody = Body(default=ConnectionProbeBody()),
    ) -> Any:
        return await _dispatch(_command(
            "connection.test", "connection", connection_id,
            {"connection_id": connection_id},
            command_id=body.command_id,
            idempotency_key=body.idempotency_key))

    @router.delete("/api/platform-connections/{connection_id}")
    async def unregister_connection(
        connection_id: str,
        body: ConnectionUnregisterBody = Body(default_factory=ConnectionUnregisterBody),
    ) -> Any:
        """从本地清单移除平台连接（connection.unregister；历史事件保留）。"""
        if not body.confirm:
            return JSONResponse(
                status_code=400,
                content={
                    "error": {
                        "code": "connection.unregister.confirm_required",
                        "message": "删除连接需要 confirm=true",
                        "category": ErrorCategory.VALIDATION.value,
                    },
                },
            )
        return await _dispatch(_command(
            "connection.unregister",
            "connection",
            connection_id,
            {"connection_id": connection_id},
            command_id=body.command_id,
            idempotency_key=body.idempotency_key,
        ))

    # -- 比赛 ------------------------------------------------------------------

    @router.get("/api/competitions")
    async def list_competitions() -> Any:
        return await _query("competition.list")

    @router.post("/api/competitions")
    async def create_competition(body: CreateCompetitionBody) -> Any:
        """登记 / 同步一场比赛：即 competition.sync 命令（异步 receipt）。"""
        payload = {
            "connection_id": body.connection_id.strip(),
            "external_competition_id": body.external_competition_id.strip(),
        }
        for key in ("title", "description", "starts_at", "ends_at"):
            value = getattr(body, key)
            if value:
                payload[key] = value
        return await _dispatch(_command(
            "competition.sync", "competition", "", payload,
            command_id=body.command_id,
            idempotency_key=body.idempotency_key))

    @router.get("/api/competitions/{competition_id}")
    async def get_competition(competition_id: str) -> Any:
        """比赛快照（含 event watermark；SSE 恢复游标基于此值）。"""
        return await _query("competition.snapshot", competition_id=competition_id)

    @router.delete("/api/competitions/{competition_id}")
    async def unregister_competition(
        competition_id: str,
        body: CompetitionUnregisterBody = Body(default_factory=CompetitionUnregisterBody),
    ) -> Any:
        """从本地清单移除比赛（competition.unregister；历史事件与数据保留）。"""
        if not body.confirm:
            return JSONResponse(
                status_code=400,
                content={
                    "error": {
                        "code": "competition.unregister.confirm_required",
                        "message": "删除比赛需要 confirm=true",
                        "category": ErrorCategory.VALIDATION.value,
                    },
                },
            )
        return await _dispatch(_command(
            "competition.unregister",
            "competition",
            competition_id,
            {"competition_id": competition_id},
            command_id=body.command_id,
            idempotency_key=body.idempotency_key,
        ))

    @router.post("/api/competitions/{competition_id}/commands")
    async def post_command(
        competition_id: str, body: dict[str, Any] = Body(...)
    ) -> Any:
        not_found = _require_competition(competition_id)
        if not_found is not None:
            return not_found
        envelope = _typed_command(competition_id, dict(body))
        if isinstance(envelope, JSONResponse):
            return envelope
        return await _dispatch(envelope)

    @router.post("/api/competitions/{competition_id}/messages")
    async def post_message(
        competition_id: str, body: CompetitionMessageBody
    ) -> Any:
        """比赛聊天：自然语言消息只记录为聊天事件，不直接改状态。

        可选附带结构化命令（``body.command``）：先 dispatch 得到 receipt，
        再把 receipt 引用随消息事件落库，响应同时携带两份 receipt。
        """
        not_found = _require_competition(competition_id)
        if not_found is not None:
            return not_found
        text = body.text.strip()

        command_receipt: Optional[CommandReceipt] = None
        attached = body.command
        if attached is not None:
            if not isinstance(attached, dict):
                return _error_response(
                    "competition.message.command_invalid",
                    "command must be an object",
                    ErrorCategory.VALIDATION,
                )
            envelope = _typed_command(competition_id, attached)
            if isinstance(envelope, JSONResponse):
                return envelope
            command_receipt = await command_api.dispatch(envelope)

        if not text and command_receipt is None:
            return _error_response(
                "competition.message.text_required",
                "message requires text or an attached command",
                ErrorCategory.VALIDATION,
            )

        payload: dict[str, Any] = {
            "competition_id": competition_id,
            "text": text,
            "author": str(body.author or actor.id or "operator"),
        }
        if command_receipt is not None:
            # 结构化 command receipt 是实际操作记录（设计 14.4）。
            payload["command_id"] = command_receipt.command_id
            payload["receipt_state"] = command_receipt.state.value
        message_receipt = await command_api.dispatch(_command(
            "competition.message", "competition", competition_id, payload,
            command_id=body.command_id.strip(),
            idempotency_key=body.idempotency_key.strip()))
        response: dict[str, Any] = _receipt_body(message_receipt)
        response["message_receipt"] = response.pop("receipt")
        if command_receipt is not None:
            response["command_receipt"] = command_receipt.model_dump(mode="json")
        return JSONResponse(
            response, status_code=_receipt_status(message_receipt))

    # -- 读模型 ----------------------------------------------------------------

    @router.get("/api/competitions/{competition_id}/challenges")
    async def list_challenges(competition_id: str) -> Any:
        return await _query(
            "competition.challenges", competition_id=competition_id)

    @router.get("/api/competitions/{competition_id}/submissions")
    async def list_submissions(competition_id: str) -> Any:
        return await _query(
            "competition.submissions", competition_id=competition_id)

    @router.get("/api/competitions/{competition_id}/leases")
    async def list_leases(competition_id: str) -> Any:
        return await _query("competition.leases", competition_id=competition_id)

    @router.get("/api/competitions/{competition_id}/events/history")
    async def competition_history(
        competition_id: str,
        before: Optional[int] = Query(default=None, ge=1),
        limit: int = Query(default=100, ge=1, le=200),
    ) -> Any:
        # 与快照使用同一授权边界；历史只按需读取有限的一页。
        authorized = await _query("competition.snapshot", competition_id=competition_id)
        if isinstance(authorized, JSONResponse):
            return authorized
        cursor = before if before is not None else store.event_watermark() + 1
        rows = store.read_competition_history(
            competition_id, before_seq=cursor, limit=limit + 1)
        page = rows[:limit]
        return {
            "events": public_events.to_public_many(page),
            "next_before": page[-1][0] if len(rows) > limit else None,
        }

    # -- SSE：仅推送当前快照，历史独立分页 -----------------------------------

    @router.get("/api/competitions/{competition_id}/events")
    async def competition_events(
        request: Request, competition_id: str, after: int = 0
    ) -> Any:
        """进入和重连立即发送当前快照；后续合并变化，最多每秒一帧。

        Last-Event-ID / after 仅记录重连信息，不触发历史日志回放。
        中间状态可以合并，历史记录通过 events/history 分页查看。
        """
        not_found = _require_competition(competition_id)
        if not_found is not None:
            return not_found

        last_event_id = request.headers.get("last-event-id", "").strip()
        try:
            resume_after = int(last_event_id) if last_event_id else int(after)
        except ValueError:
            return _error_response(
                "competition.events.cursor_invalid",
                "after / Last-Event-ID must be an integer seq",
                ErrorCategory.VALIDATION,
                recovery_hint="使用上一条已确认事件的整数 seq 重新连接",
            )

        async def stream_body():
            # 授权边界与 query 一致：snapshot 查询经 Command API 判定后才
            # 开始流式读取（operator 主体全权；非 operator 由 policy 拒绝）。
            try:
                snapshot_result = await command_api.query(QueryEnvelope(
                    query_type="competition.snapshot",
                    aggregate_type="competition",
                    aggregate_id=competition_id,
                    actor=actor,
                    params={"competition_id": competition_id},
                ))
            except CommandAPIError as exc:
                yield {
                    "event": "error",
                    "data": json.dumps(
                        {"error": exc.error.model_dump(mode="json")},
                        ensure_ascii=False),
                }
                return
            snapshot = snapshot_result.result
            previous_content = None
            while True:
                if await request.is_disconnected():
                    return
                # 比较实际读模型而非仅看事件序号：某些异步投影在事件写入后
                # 才落库，同一个水位下也可能出现更新。
                content = json.dumps(
                    {key: value for key, value in snapshot.items() if key != "generated_at"},
                    ensure_ascii=False, sort_keys=True, separators=(",", ":"),
                )
                if content != previous_content:
                    watermark = int(snapshot.get("event_watermark") or 0)
                    yield {
                        "id": str(watermark),
                        "event": SSE_SNAPSHOT_EVENT,
                        "data": json.dumps(
                            {"snapshot": snapshot, "watermark_seq": watermark},
                            ensure_ascii=False),
                    }
                    previous_content = content
                await asyncio.sleep(max(1.0, sse_poll_seconds))
                try:
                    result = await command_api.query(QueryEnvelope(
                        query_type="competition.snapshot",
                        aggregate_type="competition",
                        aggregate_id=competition_id,
                        actor=actor,
                        params={"competition_id": competition_id},
                    ))
                except CommandAPIError as exc:
                    yield {
                        "event": "error",
                        "data": json.dumps({"error": exc.error.model_dump(mode="json")}),
                    }
                    return
                snapshot = result.result

        async def stream():
            if sse_metrics is not None:
                sse_metrics.open_sse("competition", resumed=resume_after > 0)
            try:
                async for item in stream_body():
                    yield item
            finally:
                if sse_metrics is not None:
                    sse_metrics.close_sse("competition")

        return EventSourceResponse(stream(), ping=10, send_timeout=15)

    return router


__all__ = [
    "OPERATOR",
    "SSE_COMPETITION_EVENT",
    "SSE_SNAPSHOT_EVENT",
    "create_competition_router",
]
