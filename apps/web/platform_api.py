"""MutekiCommandAPI 的 Web 传输入口。

该 Router 只负责 HTTP 请求与平台契约之间的转换。权限、幂等、回执、
事件游标和业务处理全部由共享 Command API 完成。
"""

from __future__ import annotations

from typing import Any

from fastapi import APIRouter, Body
from fastapi.responses import JSONResponse
from pydantic import BaseModel, ValidationError

from muteki.platform.command_handlers.base import CommandAPIError
from muteki.platform.contracts.commands import (
    ActorRef,
    CommandEnvelope,
    EventReadRequest,
    QueryEnvelope,
    WaitRequest,
)
from muteki.platform.contracts.errors import ErrorCategory, ErrorEnvelope
from muteki.platform.contracts.receipts import (
    CommandReceipt,
    EffectState,
    ReceiptState,
)


OPERATOR = ActorRef(kind="operator", id="local-user")


class EffectActionBody(BaseModel):
    command_id: str = ""
    idempotency_key: str = ""
    confirm: bool = False


def _status_for_error(error: ErrorEnvelope | None) -> int:
    if error is None:
        return 500
    return {
        ErrorCategory.VALIDATION: 400,
        ErrorCategory.PERMISSION: 403,
        ErrorCategory.NOT_FOUND: 404,
        ErrorCategory.CONFLICT: 409,
        ErrorCategory.STATE: 409,
        ErrorCategory.RATE_LIMIT: 429,
    }.get(error.category, 500)


def _receipt_response(receipt: CommandReceipt) -> JSONResponse:
    status = (
        202
        if receipt.state in {
            ReceiptState.ACCEPTED, ReceiptState.RUNNING, ReceiptState.WAITING,
        }
        else 200
    )
    if receipt.state in (ReceiptState.FAILED, ReceiptState.CONFLICT):
        status = _status_for_error(receipt.error)
    return JSONResponse(
        {"receipt": receipt.model_dump(mode="json")}, status_code=status)


def _error_response(error: ErrorEnvelope) -> JSONResponse:
    return JSONResponse(
        {"error": error.model_dump(mode="json")},
        status_code=_status_for_error(error),
    )


def _validation_error(exc: Exception) -> JSONResponse:
    error = ErrorEnvelope(
        code="platform.request.invalid",
        message=str(exc),
        category=ErrorCategory.VALIDATION,
        recovery_hint="检查请求是否符合公开平台契约",
    )
    return _error_response(error)


def create_platform_router(command_api: Any) -> APIRouter:
    """构造 Command API 的统一 HTTP 入口。"""
    router = APIRouter(tags=["platform-command-api"])

    @router.post("/api/commands")
    async def dispatch(body: dict[str, Any] = Body(...)) -> Any:
        try:
            payload = dict(body)
            payload.setdefault("actor", OPERATOR.model_dump(mode="json"))
            command = CommandEnvelope.model_validate(payload)
        except (ValidationError, TypeError, ValueError) as exc:
            return _validation_error(exc)
        return _receipt_response(await command_api.dispatch(command))

    @router.get("/api/receipts/{command_id}")
    async def get_receipt(command_id: str) -> Any:
        try:
            receipt = await command_api.get_receipt(command_id)
        except CommandAPIError as exc:
            return _error_response(exc.error)
        return {"receipt": receipt.model_dump(mode="json")}

    @router.get("/api/receipts/{command_id}/effects")
    async def receipt_effects(command_id: str) -> Any:
        store = getattr(command_api, "global_store", None)
        if store is None:
            return _error_response(ErrorEnvelope(
                code="effect.store_unavailable",
                message="effect receipt store is unavailable",
                category=ErrorCategory.INTERNAL,
            ))
        return {
            "effects": [
                item.model_dump(mode="json")
                for item in store.effects_for_command(command_id)
            ],
        }

    @router.get("/api/effects/{effect_id}")
    async def get_effect(effect_id: str) -> Any:
        store = getattr(command_api, "global_store", None)
        effect = store.get_effect_receipt(effect_id) if store is not None else None
        if effect is None:
            return _error_response(ErrorEnvelope(
                code="effect.receipt_not_found",
                message=f"no effect receipt {effect_id!r}",
                category=ErrorCategory.NOT_FOUND,
                correlation_id=effect_id,
            ))
        return {"effect": effect.model_dump(mode="json")}

    @router.get("/api/effects")
    async def list_effects(state: str = "", limit: int = 200) -> Any:
        store = getattr(command_api, "global_store", None)
        if store is None:
            return {"effects": []}
        states = None
        if state:
            try:
                states = {EffectState(item.strip()) for item in state.split(",")}
            except ValueError as exc:
                return _validation_error(exc)
        return {
            "effects": [
                item.model_dump(mode="json")
                for item in store.list_effect_receipts(
                    states=states, limit=limit)
            ],
        }

    async def _effect_action(
        effect_id: str, action: str, body: EffectActionBody
    ) -> JSONResponse:
        fields: dict[str, Any] = {
            "command_type": f"effect.{action}",
            "aggregate_type": "effect",
            "aggregate_id": effect_id,
            "actor": OPERATOR,
            "payload": {
                "effect_id": effect_id,
                "confirm": body.confirm,
            },
        }
        if body.command_id:
            fields["command_id"] = body.command_id
        if body.idempotency_key:
            fields["idempotency_key"] = body.idempotency_key
        return _receipt_response(await command_api.dispatch(
            CommandEnvelope(**fields)))

    @router.post("/api/effects/{effect_id}/retry")
    async def retry_effect(
        effect_id: str, body: EffectActionBody = Body(default=EffectActionBody())
    ) -> Any:
        return await _effect_action(effect_id, "retry", body)

    @router.post("/api/effects/{effect_id}/cancel")
    async def cancel_effect(
        effect_id: str, body: EffectActionBody = Body(...)
    ) -> Any:
        return await _effect_action(effect_id, "cancel", body)

    @router.post("/api/queries")
    async def query(body: dict[str, Any] = Body(...)) -> Any:
        try:
            payload = dict(body)
            payload.setdefault("actor", OPERATOR.model_dump(mode="json"))
            envelope = QueryEnvelope.model_validate(payload)
            result = await command_api.query(envelope)
        except (ValidationError, TypeError, ValueError) as exc:
            return _validation_error(exc)
        except CommandAPIError as exc:
            return _error_response(exc.error)
        return {"result": result.model_dump(mode="json")}

    @router.post("/api/events/read")
    async def read_events(body: dict[str, Any] = Body(...)) -> Any:
        try:
            request = EventReadRequest.model_validate(body)
            page = await command_api.read_events(request, principal=OPERATOR)
        except (ValidationError, TypeError, ValueError) as exc:
            return _validation_error(exc)
        except CommandAPIError as exc:
            return _error_response(exc.error)
        return page.model_dump(mode="json")

    @router.post("/api/events/wait")
    async def wait(body: dict[str, Any] = Body(...)) -> Any:
        try:
            request = WaitRequest.model_validate(body)
            result = await command_api.wait(request, principal=OPERATOR)
        except (ValidationError, TypeError, ValueError) as exc:
            return _validation_error(exc)
        except CommandAPIError as exc:
            return _error_response(exc.error)
        return result.model_dump(mode="json")

    return router


__all__ = ["create_platform_router"]
