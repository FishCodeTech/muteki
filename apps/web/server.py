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
    ServiceSessionMiddleware,
    allowed_web_origins,
    browser_request_error,
    request_token,
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
    async def normal_lifespan(app: FastAPI):
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

    @asynccontextmanager
    async def lifespan(app: FastAPI):
        if os.environ.get("MUTEKI_DESKTOP_MAINTENANCE") != "1":
            async with normal_lifespan(app):
                yield
            return
        # Schema construction has completed, but recovery, timers, agents and
        # external side effects remain stopped until the installer admits writes.
        app.state.desktop_activate = asyncio.Event()
        app.state.desktop_activated = asyncio.Event()
        stopped = asyncio.Event()

        async def activate():
            await app.state.desktop_activate.wait()
            try:
                async with normal_lifespan(app):
                    app.state.desktop_activated.set()
                    await stopped.wait()
            except Exception as exc:
                app.state.desktop_activation_error = str(exc)
                app.state.desktop_activated.set()
                raise

        task = asyncio.create_task(activate())
        try:
            yield
        finally:
            stopped.set()
            if not app.state.desktop_activate.is_set():
                task.cancel()
                await mgr.shutdown()
                await platform_stack.shutdown()
            results = await asyncio.gather(task, return_exceptions=True)
            for result in results:
                if isinstance(result, Exception):
                    raise result

    app = FastAPI(title="Project Muteki — Agent 安全工作台", lifespan=lifespan)
    app.state.desktop_draining = os.environ.get("MUTEKI_DESKTOP_MAINTENANCE") == "1"
    app.state.desktop_writes_in_flight = 0

    @app.middleware("http")
    async def desktop_write_gate(request: Request, call_next):
        mutation = request.method not in {"GET", "HEAD", "OPTIONS"} and request.url.path.startswith("/api/") and request.url.path != "/api/desktop/drain"
        if not mutation:
            return await call_next(request)
        if app.state.desktop_draining:
            return JSONResponse({"error": {"code": "desktop.maintenance", "message": "工作台正在切换版本，暂不接受新操作。"}}, status_code=503)
        app.state.desktop_writes_in_flight += 1
        try:
            return await call_next(request)
        finally:
            app.state.desktop_writes_in_flight -= 1

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

    auth = AuthConfig.from_env(mgr.state_root, service_id=platform_stack.store.installation_id)
    auth.fail_fast_check()
    app.state.auth = auth
    app.state.tickets = TicketStore()
    cors_origins = allowed_web_origins()
    app.add_middleware(CORSMiddleware, allow_origins=sorted(cors_origins),
                       allow_credentials=True, allow_methods=["*"], allow_headers=["*"])

    def _auth_error(request: Request, message: str, status: int) -> JSONResponse:
        resp = JSONResponse({"error": {"code": "auth.unauthorized" if status == 401 else "auth.origin_invalid",
                                       "message": message}}, status_code=status)
        origin = request.headers.get("origin")
        if origin in cors_origins:
            resp.headers["Access-Control-Allow-Origin"] = origin
            resp.headers["Access-Control-Allow-Credentials"] = "true"
            resp.headers["Vary"] = "Origin"
        resp.headers["Cache-Control"] = "no-store"
        return resp

    @app.middleware("http")
    async def _auth_gate(request: Request, call_next):
        cfg: AuthConfig = app.state.auth
        path = request.url.path
        if request.method == "OPTIONS" or not path.startswith("/api/"):
            return await call_next(request)
        # The capability bridge authenticates its separate session grants.
        if path == "/api/capability":
            return await call_next(request)
        origin_error = browser_request_error(cfg, request)
        if origin_error:
            return _auth_error(request, origin_error, 403)
        if path in PUBLIC_API_PATHS or not cfg.enabled:
            return await call_next(request)
        from apps.web.conversation_shares import is_public_share_read
        if is_public_share_read(request.method, path):
            return await call_next(request)
        if request.method == "GET" and re.fullmatch(r"/api/runs/[^/]+/events", path):
            return await call_next(request)  # handler consumes the one-time ticket
        token = request_token(cfg, request)
        if request.method == "GET" and (
            path == "/api/threads/inbox/events" or re.fullmatch(r"/api/threads/[^/]+/events", path)
        ) and app.state.tickets.redeem(request.query_params.get("ticket"), scope=request.scope):
            return await call_next(request)
        if not verify_token(cfg, token):
            return _auth_error(request, "登录已失效，请重新登录。", 401)
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

    app.add_middleware(ServiceSessionMiddleware, config=auth)
    return app


app = create_app()
