"""FastAPI backend for the web command deck (§14.1 / Sprint 1.1).

Endpoints:
  GET  /api/runs                      list known runs
  POST /api/runs/{run_id}/start       launch a run (mock driver, or swarm if a
                                       challenge spec is posted) — see drivers.py
  GET  /api/runs/{run_id}/events      SSE: the typed event stream (Last-Event-ID
                                       resume via the standard header)
  WS   /api/runs/{run_id}/terminal    sandbox terminal: TERMINAL_OUTPUT bytes
  POST /api/runs/{run_id}/control     durable operator command admission
  POST /api/runs/{run_id}/hitl        legacy adapter onto /control
  GET  /                              the single-page UI (static)

The server holds NO solving logic — it only brokers the event bus + HITL. Event
schema is the only contract (§3).
"""

# Environment-backed route constants must be loaded before route imports.
# ruff: noqa: E402

from __future__ import annotations

import asyncio
import json
import os
import re
from contextlib import asynccontextmanager
from pathlib import Path
from typing import Any, Optional

from fastapi import (
    FastAPI,
    Request,
)
from fastapi.middleware.cors import CORSMiddleware
from fastapi.exceptions import RequestValidationError
from fastapi.responses import FileResponse, JSONResponse
from fastapi.staticfiles import StaticFiles

from apps.web.auth import (
    PUBLIC_API_PATHS,
    AuthConfig,
    TicketStore,
    bearer_from_header,
    verify_token,
)
from apps.web.run_manager import RunManager
from muteki.core.dotenv_boot import load_env

load_env()  # route modules read env-backed constants during import

from apps.web.routes import register_all
from apps.web.routes.common import (
    _env_float,
    _env_int,
    _listening_process,
)
from apps.web.routes.helpers import make_helpers
from apps.web.platform_update import PlatformUpdateController
from muteki.platform.contracts.errors import ErrorCategory, ErrorEnvelope
from muteki.solver.engine_registry import (
    ENGINE_TEMPORARILY_UNSUPPORTED_CODE,
    EngineTemporarilyUnsupportedError,
)

UI_DIR = Path(__file__).parent / "ui"


def create_app(manager: Optional[RunManager] = None) -> FastAPI:
    mgr = manager or RunManager()
    from apps.web.platform_stack import WebPlatformStack

    platform_stack = WebPlatformStack(mgr)

    # Retention policy (BE-auto-archive): auto-archive idle runs, then delete the
    # ones that stay idle. Defaults: archive after 3 days, delete after 7 days,
    # sweep hourly. All env-tunable; set MUTEKI_RETENTION_ENABLED=0 to disable
    # (pinned runs are NEVER auto-touched).
    @asynccontextmanager
    async def lifespan(app: FastAPI):
        # Start the reverse-connect control receiver: the in-container supervisors
        # DIAL this (host.docker.internal:<port>) — so the host must be listening
        # before any container starts. Lazy-starts on first use too, but starting it
        # here makes "control port already in use" surface at boot, not mid-run.
        try:
            from muteki.solver.control_receiver import (
                ControlReceiver,
                DEFAULT_CONTROL_BIND,
                DEFAULT_CONTROL_PORT,
            )
            control_host = os.environ.get(
                "MUTEKI_CONTROL_BIND", DEFAULT_CONTROL_BIND)
            control_port = _env_int(
                "MUTEKI_CONTROL_PORT", DEFAULT_CONTROL_PORT)
            receiver = ControlReceiver.instance(
                host=control_host, port=control_port)
            platform_stack.set_control_receiver_status({
                "state": "ready",
                "detail": "control receiver is listening",
                "impact": "",
                "host": receiver.host,
                "port": receiver.port,
            })
        except (OSError, RuntimeError) as exc:
            holder = _listening_process(locals().get("control_port", 0))
            platform_stack.set_control_receiver_status({
                "state": "degraded",
                "detail": f"control receiver bind failed: {exc}",
                "impact": (
                    "container Worker start and control are unavailable; "
                    "Conversation and non-container Runtime remain available"
                ),
                "host": locals().get("control_host", ""),
                "port": locals().get("control_port", 0),
                "holder": holder,
            })
            print(f"[control-receiver] could not bind control port: {exc}", flush=True)
        # Cursor Cloud injects PI_* secrets at agent start (not snapshot install).
        # Hydrate pi-ollama before routes serve so Agents UI / workers see it.
        try:
            from muteki.solver.credential_accounts import (
                ensure_pi_ollama_account_from_env,
            )

            ensured = ensure_pi_ollama_account_from_env(mgr.state_root)
            if ensured is not None:
                print(
                    "[pi-ollama-bootstrap] account ready "
                    f"(id={ensured.get('account_id', 'pi-ollama')})",
                    flush=True,
                )
            else:
                print(
                    "[pi-ollama-bootstrap] PI_API_KEY unset — skipped",
                    flush=True,
                )
        except Exception as exc:  # noqa: BLE001 — boot must not die on hydrate
            print(
                f"[pi-ollama-bootstrap] {type(exc).__name__}: {exc}",
                flush=True,
            )
        await platform_stack.recover()
        print(
            "[muteki-startup] "
            + json.dumps(platform_stack.startup_summary(), ensure_ascii=False),
            flush=True,
        )
        retention_task: Optional[asyncio.Task] = None
        enabled = os.environ.get("MUTEKI_RETENTION_ENABLED", "1").lower() not in (
            "0", "false", "no", "off", "")
        if enabled:
            retention_task = asyncio.create_task(mgr.retention_loop(
                interval_s=_env_float("MUTEKI_RETENTION_INTERVAL", 3600.0),
                archive_after_s=_env_float("MUTEKI_ARCHIVE_DAYS", 3.0) * 86400.0,
                delete_after_s=_env_float("MUTEKI_DELETE_DAYS", 7.0) * 86400.0,
            ))
        runtime_refresh_task: Optional[asyncio.Task] = None
        runtime_refresh_interval = max(
            0.0, _env_float("MUTEKI_AGENT_REFRESH_INTERVAL", 300.0))
        if runtime_refresh_interval > 0:
            async def _runtime_refresh_loop() -> None:
                # 健康探测涉及多个外部 CLI。服务先使用持久化快照就绪，
                # 再在后台刷新，避免任一 CLI 阻塞整个生产 Web 启动。
                await asyncio.sleep(min(5.0, runtime_refresh_interval))
                while True:
                    try:
                        await platform_stack.runtime_service.probe_all()
                    except Exception as exc:  # noqa: BLE001 — 后台刷新不得终止服务
                        print(
                            f"[agent-runtime-refresh] {type(exc).__name__}: {exc}",
                            flush=True,
                        )
                    await asyncio.sleep(runtime_refresh_interval)

            runtime_refresh_task = asyncio.create_task(_runtime_refresh_loop())
        model_catalog_refresh_task: Optional[asyncio.Task] = None
        model_catalog_scan_interval = max(
            0.0, _env_float("MUTEKI_MODEL_CATALOG_SCAN_INTERVAL", 300.0))
        if model_catalog_scan_interval > 0:
            async def _model_catalog_refresh_loop() -> None:
                await asyncio.sleep(5)
                while True:
                    try:
                        await app.state.route_helpers._refresh_stale_credential_catalogs()
                    except Exception as exc:  # noqa: BLE001
                        print(
                            f"[credential-model-refresh] {type(exc).__name__}: {exc}",
                            flush=True,
                        )
                    await asyncio.sleep(model_catalog_scan_interval)

            model_catalog_refresh_task = asyncio.create_task(
                _model_catalog_refresh_loop())
        try:
            yield
        finally:
            background_tasks = [
                task for task in (
                    retention_task,
                    runtime_refresh_task,
                    model_catalog_refresh_task,
                )
                if task is not None
            ]
            for task in background_tasks:
                task.cancel()
            if background_tasks:
                await asyncio.gather(*background_tasks, return_exceptions=True)
            # Tear down every live swarm/standby task (and its shelled CLI subprocess
            # group) so a server restart doesn't leave budget-eating zombies. This was
            # never wired up before — shutdown() existed but nothing called it.
            await mgr.shutdown()
            await platform_stack.shutdown()

    app = FastAPI(title="Project Muteki — Agent 安全工作台", lifespan=lifespan)

    @app.exception_handler(RequestValidationError)
    async def _request_validation_error(
        _request: Request, exc: RequestValidationError
    ) -> JSONResponse:
        # Pydantic 的默认 422 响应使用另一套 ``detail`` 结构，而且可能带回
        # 原始 input（其中可能有凭据）。产品 API 返回完整 ErrorEnvelope。
        fields = [
            {
                "location": ".".join(str(part) for part in row.get("loc", ())),
                "message": str(row.get("msg") or "invalid value"),
                "type": str(row.get("type") or "validation_error"),
            }
            for row in exc.errors()
        ]
        error = ErrorEnvelope(
            code="request.validation",
            message="请求字段校验失败",
            category=ErrorCategory.VALIDATION,
            recovery_hint="根据 fields 修正请求后重试",
            detail={"fields": fields},
        )
        return JSONResponse(
            {"error": error.model_dump(mode="json")}, status_code=422
        )

    @app.exception_handler(EngineTemporarilyUnsupportedError)
    async def _engine_temporarily_unsupported(
        _request: Request, exc: EngineTemporarilyUnsupportedError,
    ) -> JSONResponse:
        return JSONResponse(
            {
                "detail": exc.reason,
                "error": {
                    "code": ENGINE_TEMPORARILY_UNSUPPORTED_CODE,
                    "message": exc.reason,
                    "category": "state",
                    "engine": exc.engine,
                },
            },
            status_code=409,
        )

    app.state.manager = mgr
    app.state.platform_updates = PlatformUpdateController()
    app.state.platform_stack = platform_stack
    app.state.credential_model_refresh_tasks = {}
    app.state.route_helpers = make_helpers(app)
    for router in platform_stack.routers():
        app.include_router(router)

    # Auth (P3): a single-password gate in front of /api. fail_fast_check refuses
    # to start if bound to a non-loopback host with no password — see auth.py and
    # docs/_local/plan_p3_auth.md. When no password is set AND the bind is
    # loopback, auth is disabled and the deck behaves exactly as before.
    auth = AuthConfig.from_env()
    auth.fail_fast_check()
    app.state.auth = auth
    app.state.tickets = TicketStore()

    # Dev convenience: the Next dev server (:3001) can talk to this backend
    # directly. Connecting the browser's EventSource straight here (instead of
    # through Next's dev rewrite proxy) avoids the proxy BUFFERING the SSE stream
    # — the proxy holds events until the connection closes, which makes a live
    # run look frozen until it finishes. In prod the static UI is served same-
    # origin by this app, so CORS is a no-op there. Allowlist localhost only.
    app.add_middleware(
        CORSMiddleware,
        allow_origin_regex=r"http://(localhost|127\.0\.0\.1)(:\d+)?",
        allow_methods=["*"],
        allow_headers=["*"],
    )

    # Auth gate. Added AFTER CORS, so CORS wraps it (outermost) — preflight
    # OPTIONS are answered by CORS and never reach here. We still bypass OPTIONS
    # defensively (a same-origin request via the Next proxy often omits Origin,
    # so CORS does not short-circuit it). Only /api is gated; the Next server
    # (:3001) owns the UI/login page and must be secured separately when exposed
    # (reverse proxy / loopback bind) — see docs/_local/plan_p3_auth.md.
    #
    # @app.middleware("http") does NOT see websocket scope; the /terminal WS and
    # the SSE /events stream do their own ticket/token check in-handler.
    #
    # IMPORTANT (CORS): a middleware that SHORT-CIRCUITS with its own Response
    # bypasses CORSMiddleware's response path, so a cross-origin 401 would arrive
    # at the browser WITHOUT Access-Control-Allow-Origin — the browser then
    # reports a network error instead of a 401, and the frontend can't tell "needs
    # login" from "backend down". The Next dev UI (:3001) talks to this backend
    # (:8000) cross-origin, so we must mirror the CORS allow-origin header onto the
    # 401 ourselves. (CORSMiddleware only auto-adds headers when the inner app
    # actually runs; our early return never reaches it.)
    _cors_origin_re = re.compile(r"http://(localhost|127\.0\.0\.1)(:\d+)?$")

    def _unauthorized(request: Request) -> JSONResponse:
        resp = JSONResponse({"error": "unauthorized"}, status_code=401)
        origin = request.headers.get("origin")
        if origin and _cors_origin_re.match(origin):
            resp.headers["Access-Control-Allow-Origin"] = origin
            resp.headers["Vary"] = "Origin"
        return resp

    @app.middleware("http")
    async def _auth_gate(request: Request, call_next):
        cfg: AuthConfig = app.state.auth
        if not cfg.enabled:
            return await call_next(request)
        path = request.url.path
        if request.method == "OPTIONS":
            return await call_next(request)
        if not path.startswith("/api/"):
            return await call_next(request)  # static/UI (only present if built)
        if path in PUBLIC_API_PATHS:
            return await call_next(request)
        # Only these exact read routes accept a separate snapshot-scoped token.
        # Their handlers check token, access mode, expiry and revocation.
        from apps.web.conversation_shares import is_public_share_read
        if is_public_share_read(request.method, path):
            return await call_next(request)
        # Capability Bridge 使用 Session Grant Bearer 自行认证；Web operator
        # token 不能替代 grant，也不应在此处抢先拒绝。
        if path == "/api/capability":
            return await call_next(request)
        # Legacy run streams enforce their own ticket check. Do not treat an
        # arbitrary endpoint named /events as a public route.
        if request.method == "GET" and re.fullmatch(r"/api/runs/[^/]+/events", path):
            return await call_next(request)
        if request.method == "GET" and (
            path == "/api/threads/inbox/events"
            or re.fullmatch(r"/api/threads/[^/]+/events", path)
        ):
            token = bearer_from_header(request.headers.get("Authorization"))
            if not (verify_token(cfg, token) or app.state.tickets.redeem(request.query_params.get("ticket"))):
                return _unauthorized(request)
            return await call_next(request)
        token = bearer_from_header(request.headers.get("Authorization"))
        if not verify_token(cfg, token):
            return _unauthorized(request)
        return await call_next(request)

    platform_prefixes = (
        "/api/projects",
        "/api/workspaces",
        "/api/threads",
        "/api/agent-runtimes",
        "/api/platform-connections",
        "/api/competitions",
        "/api/extensions",
        "/api/workspace-kinds",
        "/api/domain-modules",
        "/api/commands",
        "/api/queries",
        "/api/receipts",
        "/api/events/read",
        "/api/events/wait",
    )

    @app.middleware("http")
    async def _platform_ready_gate(request: Request, call_next):
        if (
            request.url.path.startswith(platform_prefixes)
            and not platform_stack.ready
        ):
            return JSONResponse(
                {"error": {
                    "code": "platform.recovery_incomplete",
                    "message": "平台状态恢复尚未完成",
                }},
                status_code=503,
            )
        return await call_next(request)

    register_all(app)

    # static UI: the deck is the Next.js app (run `./run.sh web` → :3001, which
    # talks to this backend's /api). If a Next.js static export ever drops an
    # index.html into ui/, serve it at / too; otherwise / is unused (the bare
    # backend is API-only).
    if (UI_DIR / "index.html").exists():
        @app.get("/")
        async def index() -> Any:
            return FileResponse(UI_DIR / "index.html")

    if UI_DIR.exists():
        app.mount("/ui", StaticFiles(directory=str(UI_DIR)), name="ui")

    return app


app = create_app()
