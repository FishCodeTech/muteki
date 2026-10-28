from __future__ import annotations

from typing import Any

from fastapi import APIRouter, Body
from fastapi.responses import JSONResponse

from muteki.platform.command_handlers.base import CommandAPIError
from muteki.platform.contracts.commands import ActorRef, CommandEnvelope, QueryEnvelope
from muteki.platform.contracts.base import new_id
from muteki.src.commands import COMMAND_TYPES, QUERY_TYPES
from muteki.src.control_commands import SRC_CONTROL_QUERY_TYPES
from muteki.src.service import SrcService


def create_src_router(*, command_api: Any, service: SrcService) -> APIRouter:
    router = APIRouter(prefix="/api/src", tags=["src"])
    actor = ActorRef(kind="operator", id="local-user")

    def failure(exc: Exception) -> JSONResponse:
        if isinstance(exc, CommandAPIError):
            error = exc.error
            category = error.category.value
            status = {
                "permission": 403, "not_found": 404, "conflict": 409,
                "state": 409, "timeout": 504, "runtime": 502,
                "internal": 500, "validation": 400,
            }.get(category, 400)
            return JSONResponse({"error": error.model_dump(mode="json")}, status_code=status)
        return JSONResponse({"error": {"code": "src.internal", "message": f"{type(exc).__name__}: {exc}"}}, status_code=500)

    async def query(query_type: str, task_id: str = "", params: dict[str, Any] | None = None) -> Any:
        return await command_api.query(QueryEnvelope(
            query_type=query_type, aggregate_type="task" if task_id else None,
            aggregate_id=task_id or None, actor=actor, params=params or {},
        ))

    @router.get("/tasks")
    async def list_tasks() -> Any:
        try:
            result = await query("src.task.list")
            return result.result
        except Exception as exc:
            return failure(exc)

    @router.post("/tasks")
    async def create_task(payload: dict[str, Any] = Body(default_factory=dict)) -> Any:
        command = CommandEnvelope(
            command_type="src.task.create", aggregate_type="task",
            aggregate_id="", actor=actor, payload=payload,
            idempotency_key=str(payload.get("idempotency_key") or new_id("src-create")),
        )
        receipt = await command_api.dispatch(command)
        return JSONResponse(receipt.model_dump(mode="json"), status_code=200 if receipt.error is None else 409)

    @router.get("/tasks/{task_id}")
    async def get_task(task_id: str) -> Any:
        try:
            result = await query("src.task.get", task_id, {"task_id": task_id})
            return result.result
        except Exception as exc:
            return failure(exc)

    @router.get("/tasks/{task_id}/policy")
    async def current_policy(task_id: str) -> Any:
        try:
            result = await query("src.policy.current", task_id, {"task_id": task_id})
            return result.result
        except Exception as exc:
            return failure(exc)

    @router.get("/tasks/{task_id}/capabilities")
    async def execution_capabilities(task_id: str) -> Any:
        try:
            result = await query("src.execution.capabilities", task_id, {"task_id": task_id})
            return result.result
        except Exception as exc:
            return failure(exc)

    @router.post("/tasks/{task_id}/policy/preview")
    async def preview_policy(task_id: str, payload: dict[str, Any] = Body(default_factory=dict)) -> Any:
        try:
            result = await query("src.policy.preview", task_id, {"task_id": task_id, "request": payload})
            return result.result
        except Exception as exc:
            return failure(exc)

    @router.get("/tasks/{task_id}/executions/{invocation_id}")
    async def execution_receipt(task_id: str, invocation_id: str) -> Any:
        try:
            result = await query("src.execution.receipt", task_id, {"task_id": task_id, "invocation_id": invocation_id})
            return result.result
        except Exception as exc:
            return failure(exc)

    @router.get("/tasks/{task_id}/runs")
    async def list_runs(task_id: str) -> Any:
        try:
            result = await query("src.run.list", task_id, {"task_id": task_id})
            return result.result
        except Exception as exc:
            return failure(exc)

    @router.post("/tasks/{task_id}/runs")
    async def ensure_run(task_id: str, payload: dict[str, Any] = Body(default_factory=dict)) -> Any:
        command = CommandEnvelope(
            command_type="src.run.ensure", aggregate_type="task", aggregate_id=task_id,
            actor=actor, payload=payload,
            idempotency_key=str(payload.get("idempotency_key") or new_id("src-run")),
        )
        receipt = await command_api.dispatch(command)
        return JSONResponse(receipt.model_dump(mode="json"), status_code=200 if receipt.error is None else 409)

    @router.post("/tasks/{task_id}/runs/{run_id}/start")
    async def start_run(task_id: str, run_id: str, payload: dict[str, Any] = Body(default_factory=dict)) -> Any:
        command = CommandEnvelope(
            command_type="src.run.start", aggregate_type="task", aggregate_id=task_id,
            actor=actor, payload={**payload, "run_id": run_id},
            idempotency_key=str(payload.get("idempotency_key") or new_id("src-start")),
        )
        receipt = await command_api.dispatch(command)
        return JSONResponse(receipt.model_dump(mode="json"), status_code=200 if receipt.error is None else 409)

    @router.get("/tasks/{task_id}/records")
    async def list_records(task_id: str, kind: str = "", parent_id: str | None = None, limit: int = 100) -> Any:
        try:
            params: dict[str, Any] = {"task_id": task_id, "kind": kind, "limit": limit}
            if parent_id is not None:
                params["parent_id"] = parent_id
            result = await query("src.record.list", task_id, params)
            return result.result
        except Exception as exc:
            return failure(exc)

    @router.get("/tasks/{task_id}/records/{kind}/{record_id}")
    async def get_record(task_id: str, kind: str, record_id: str) -> Any:
        try:
            result = await query("src.record.get", task_id, {
                "task_id": task_id, "kind": kind, "record_id": record_id,
            })
            return result.result
        except Exception as exc:
            return failure(exc)

    @router.post("/tasks/{task_id}/queries/{query_type}")
    async def task_query(task_id: str, query_type: str, payload: dict[str, Any] = Body(default_factory=dict)) -> Any:
        full_type = query_type if query_type.startswith("src.") else f"src.{query_type}"
        if full_type not in {*QUERY_TYPES, *SRC_CONTROL_QUERY_TYPES}:
            return JSONResponse({"error": {"code": "query.unsupported", "message": "unsupported SRC query"}}, status_code=400)
        try:
            result = await query(full_type, task_id, {**payload, "task_id": task_id})
            return result.result
        except Exception as exc:
            return failure(exc)

    @router.post("/tasks/{task_id}/commands/{command_type}")
    async def task_command(task_id: str, command_type: str, payload: dict[str, Any] = Body(default_factory=dict)) -> Any:
        full_type = command_type if command_type.startswith("src.") else f"src.{command_type}"
        if full_type not in COMMAND_TYPES or full_type in {"src.task.create"}:
            return JSONResponse({"error": {"code": "command.unsupported", "message": "unsupported SRC command"}}, status_code=400)
        command = CommandEnvelope(
            command_type=full_type, aggregate_type="task", aggregate_id=task_id,
            actor=actor, payload=payload,
            expected_version=payload.get("expected_version"),
            idempotency_key=str(payload.get("idempotency_key") or new_id("src-command")),
        )
        receipt = await command_api.dispatch(command)
        return JSONResponse(receipt.model_dump(mode="json"), status_code=200 if receipt.error is None else 409)

    return router


__all__ = ["create_src_router"]
