"""control routes moved from server.py."""
from __future__ import annotations

from typing import Any

from fastapi import (
    FastAPI,
    HTTPException,
    Request,
)
from fastapi.responses import JSONResponse

from apps.web.routes.common import (
    _require_dict_body,
)

def register(app: FastAPI) -> None:
    h = app.state.route_helpers
    _reject_temporarily_disabled_engine = h._reject_temporarily_disabled_engine
    _dispatch_run_command = h._dispatch_run_command
    _run_receipt_status = h._run_receipt_status
    _control_effect_ok = h._control_effect_ok

    @app.post("/api/runs/{run_id}/resolve")
    async def resolve_run(run_id: str, request: Request) -> Any:
        """"继续做题": relaunch the full coordinator swarm on a finished run (reuses
        its workspace so verified facts carry over). Distinct from /hitl which, on a
        finished run, only cold-starts a single standby worker for a follow-up."""
        body = await _require_dict_body(request, allow_empty=True)
        command_id = str(body.pop("command_id", "") or "")
        receipt = await _dispatch_run_command(
            "run.resolve", run_id, body, command_id=command_id)
        return {"ok": receipt.error is None}

    @app.post("/api/runs/{run_id}/progress")
    async def request_progress(run_id: str) -> Any:
        """Publish the latest progress projection without steering the swarm."""
        run = app.state.manager.get(run_id)
        if run is None:
            raise HTTPException(status_code=404, detail="unknown run")
        if not run.started:
            raise HTTPException(status_code=409, detail="run has not started")
        if run.finished:
            raise HTTPException(status_code=409, detail="run has finished")
        publisher = run.progress_publisher
        if publisher is None:
            raise HTTPException(status_code=503, detail="progress publisher unavailable")
        event = await publisher.request_now()
        return {
            "ok": True,
            "status": "published" if event is not None else "unchanged",
            "brief_id": event.payload.get("brief_id") if event is not None else None,
        }

    @app.post("/api/runs/{run_id}/workers")
    async def spawn_worker(run_id: str, request: Request) -> Any:
        # operator runtime control: add a worker for a specific engine to a LIVE
        # coordinator run. Body {"engine": "cursor"|"claude"|"codex"} (optional —
        # omitted lets the coordinator pick a heterogeneity-aware engine).
        body = await _require_dict_body(request, allow_empty=True)
        _reject_temporarily_disabled_engine(body)
        command_id = str(body.pop("command_id", "") or "")
        receipt = await _dispatch_run_command(
            "run.spawn_worker", run_id,
            {"engine": body.get("engine"), "target": "global"},
            command_id=command_id)
        ok = receipt.error is None and await _control_effect_ok(
            run_id, receipt.command_id)
        return {"ok": ok}

    @app.delete("/api/runs/{run_id}/workers")
    async def kill_worker(run_id: str, request: Request) -> Any:
        # operator runtime control: stop a specific worker by its solver_id.
        body = await _require_dict_body(request, allow_empty=True)
        solver_id = str(body.get("solver_id") or "").strip()
        receipt = await _dispatch_run_command(
            "run.cancel_worker", run_id,
            {"worker_id": solver_id,
             "target": f"worker:{solver_id}" if solver_id else "global"},
            command_id=str(body.get("command_id") or ""))
        ok = receipt.error is None and await _control_effect_ok(
            run_id, receipt.command_id)
        return {"ok": ok}

    @app.post("/api/runs/{run_id}/control")
    async def control(run_id: str, request: Request) -> Any:
        """Persist an idempotent command; effects arrive later over SSE."""
        body = await _require_dict_body(request)
        raw_action = str(body.pop("action", "hint") or "hint").strip().lower()
        raw_payload = body.pop("payload", {}) or {}
        if not isinstance(raw_payload, dict):
            raise HTTPException(status_code=422, detail="payload must be a JSON object")
        payload = dict(raw_payload)
        payload.update(body)
        request_id = str(payload.get("request_id") or "")
        if raw_action in {"answer", "submit"} and request_id:
            raw_action = "answer_decision"
        elif raw_action == "reject":
            raw_action = "dismiss"
        command_id = str(payload.pop("command_id", "") or "")
        idempotency_key = str(payload.pop("idempotency_key", "") or "")
        receipt = await _dispatch_run_command(
            "run.control", run_id,
            {**payload, "control_action": raw_action},
            command_id=command_id, idempotency_key=idempotency_key)
        if receipt.error is not None:
            error = receipt.error
            return JSONResponse(
                {"ok": False, "status": "rejected", "code": error.code,
                 "detail": error.message, "command_id": receipt.command_id},
                status_code=_run_receipt_status(receipt),
            )
        result = app.state.manager.control_receipt(run_id, receipt.command_id)
        if result is not None and result.get("status") == "routed":
            result = {**result, "status": "persisted", "terminal": False}
        return result or {"ok": True, "status": "persisted",
                          "command_id": receipt.command_id}

    @app.get("/api/runs/{run_id}/control/{command_id}")
    async def control_receipt(run_id: str, command_id: str) -> Any:
        """Reconcile a command whose SSE terminal projection was interrupted."""
        if app.state.manager.get(run_id) is None:
            raise HTTPException(status_code=404, detail="unknown run")
        receipt = app.state.manager.control_receipt(run_id, command_id)
        if receipt is None:
            raise HTTPException(status_code=404, detail="unknown control command")
        return receipt

    @app.post("/api/runs/{run_id}/hitl")
    async def hitl(run_id: str, request: Request) -> Any:
        body = await _require_dict_body(request)
        action = str(body.pop("action", "hint") or "hint").strip().lower()
        target = str(body.pop("target", "global") or "global")
        command_id = str(body.pop("command_id", "") or "")
        run = app.state.manager.get(run_id)
        if run is not None and not body.get("request_id") and len(run.pending_help) == 1:
            body["request_id"] = next(iter(run.pending_help))
        receipt = await _dispatch_run_command(
            "run.control", run_id,
            {**body, "target": target, "control_action": action},
            command_id=command_id)
        ok = receipt.error is None and await _control_effect_ok(
            run_id, receipt.command_id)
        return {"ok": ok}
