"""system routes moved from server.py."""
from __future__ import annotations

from typing import Any

from fastapi import (
    FastAPI,
    HTTPException,
    Request,
)
from fastapi.responses import JSONResponse
from starlette.concurrency import run_in_threadpool

from apps.web.auth import (
    AuthConfig,
    check_password,
    issue_token,
    AccessSettingsError,
    request_token,
    token_payload,
)
from apps.web.routes.common import (
    _require_dict_body,
)
from muteki.version import get_version
from apps.web.ui_preferences import SETTINGS_FEATURES

def register(app: FastAPI) -> None:
    platform_stack = app.state.platform_stack

    def identity_scope() -> dict[str, str]:
        # The browser origin identifies an address, not the data installed
        # behind it. Bind durable client state to the actual platform database.
        return {"service_id": platform_stack.store.installation_id,
                "identity_id": "operator"}

    @app.get("/api/health")
    async def health() -> Any:
        return {"version": get_version(), "features": SETTINGS_FEATURES, **platform_stack.health(),
                **({"desktop_runtime": app.state.desktop_runtime} if hasattr(app.state, "desktop_runtime") else {})}

    @app.get("/api/readiness")
    async def readiness() -> Any:
        payload = {"version": get_version(), "features": SETTINGS_FEATURES, **platform_stack.health()}
        return JSONResponse(payload, status_code=200 if platform_stack.ready else 503)

    @app.get("/api/desktop/activity")
    async def desktop_activity() -> Any:
        if not hasattr(app.state, "desktop_runtime"):
            raise HTTPException(404, "Not a managed desktop service")
        turns = platform_stack.conversation.conv.list_active_turns()
        runs = [run.run_id for run in app.state.manager.runs.values() if run.task is not None and not run.task.done()]
        return {"active": len(turns) + len(runs), "turns": len(turns), "runs": len(runs)}

    @app.post("/api/desktop/drain")
    async def desktop_drain(request: Request) -> Any:
        if not hasattr(app.state, "desktop_runtime"):
            raise HTTPException(404, "Not a managed desktop service")
        body = await _require_dict_body(request)
        if body.get("cancel") is True:
            app.state.desktop_draining = False
            platform_stack.command_api.resume_admission()
            return {"draining": False}
        if app.state.desktop_writes_in_flight or not platform_stack.command_api.pause_admission():
            raise HTTPException(409, "工作台仍在处理操作，请稍后再更新。")
        app.state.desktop_draining = True
        try:
            activity = await desktop_activity()
            if activity["active"]:
                raise HTTPException(409, "请等待当前任务完成，再应用更新。")
        except BaseException:
            app.state.desktop_draining = False
            platform_stack.command_api.resume_admission()
            raise
        return {"draining": True, **activity}

    @app.get("/api/auth/status")
    async def auth_status() -> Any:
        cfg: AuthConfig = app.state.auth
        return JSONResponse({"auth_required": cfg.enabled, "session_protocol": 2,
                             **identity_scope()}, headers={"Cache-Control": "no-store"})

    @app.post("/api/auth/login")
    async def auth_login(request: Request) -> Any:
        cfg: AuthConfig = app.state.auth
        body = await _require_dict_body(request, allow_empty=True)
        client = body.get("client", "api")
        if not isinstance(client, str) or client not in {"web", "desktop", "api"} or not isinstance(body.get("remember", True), bool):
            raise HTTPException(422, "invalid login client or remember field")
        peer = request.client.host if request.client else "unknown"
        if not cfg.allow_login(peer):
            return JSONResponse({"detail": "尝试次数过多，请一分钟后重试。"}, status_code=429, headers={"Retry-After": "60"})

        def authenticate() -> str | None:
            # Checking the password and issuing a session share one revision.
            with cfg._lock:
                if not cfg.enabled:
                    return ""
                if not check_password(cfg, body.get("password")):
                    cfg.login_failed(peer)
                    return None
                return issue_token(cfg)

        token = await run_in_threadpool(authenticate)
        if token is None:
            raise HTTPException(401, "invalid password")
        payload = token_payload(cfg, token) if token else None
        data = {"ok": True, "auth_required": cfg.enabled, "session_protocol": 2,
                "session_owner": "cookie" if client == "web" else client,
                "expires_at": payload["exp"] if payload else None, **identity_scope()}
        if client != "web":
            data["token"] = token
        response = JSONResponse(data, headers={"Cache-Control": "no-store"})
        if client == "web" and token:
            response.set_cookie(cfg.cookie_name, token, httponly=True,
                                secure=request.url.scheme == "https", samesite="strict", path="/api",
                                max_age=cfg.ttl_s if body.get("remember", True) else None)
        return response

    @app.get("/api/auth/me")
    async def auth_me(request: Request) -> Any:
        from muteki.core.runtime_env import is_web_container
        cfg: AuthConfig = app.state.auth
        payload = token_payload(cfg, request_token(cfg, request))
        return JSONResponse({"authenticated": True, "auth_required": cfg.enabled,
                             "session_protocol": 2, "expires_at": payload["exp"] if payload else None,
                             "in_container": is_web_container(), **identity_scope()},
                            headers={"Cache-Control": "no-store"})

    @app.post("/api/auth/logout")
    async def auth_logout(request: Request) -> Any:
        cfg: AuthConfig = app.state.auth
        await run_in_threadpool(cfg.revoke, request_token(cfg, request))
        app.state.tickets.clear()
        response = JSONResponse({"ok": True}, headers={"Cache-Control": "no-store"})
        response.delete_cookie(cfg.cookie_name, path="/api", httponly=True, samesite="strict")
        return response

    @app.get("/api/settings/access")
    async def access_settings() -> Any:
        return JSONResponse(app.state.auth.settings(), headers={"Cache-Control": "no-store"})

    @app.put("/api/settings/access")
    async def update_access_settings(request: Request) -> Any:
        cfg: AuthConfig = app.state.auth
        body = await _require_dict_body(request)
        for key in ("action", "current_password", "new_password"):
            if key in body and not isinstance(body[key], str):
                raise HTTPException(422, f"{key} must be a string")
        try:
            await run_in_threadpool(cfg.update_password, action=body.get("action", ""),
                                    current_password=body.get("current_password", ""),
                                    new_password=body.get("new_password", ""))
        except AccessSettingsError as exc:
            return JSONResponse({"error": {"code": exc.code, "message": str(exc), "field": exc.field,
                                          "recovery_hint": "修正字段后重试。"}}, status_code=exc.status)
        app.state.tickets.clear()
        response = JSONResponse({"ok": True, "reauthenticate": cfg.enabled, **cfg.settings()},
                                headers={"Cache-Control": "no-store"})
        response.delete_cookie(cfg.cookie_name, path="/api", httponly=True, samesite="strict")
        return response

    @app.post("/api/auth/ticket")
    async def auth_ticket(request: Request) -> Any:
        # Mint a one-time, short-TTL ticket for opening an SSE/WS connection
        # (which can't carry an Authorization header). Requires a valid token —
        # it sits behind the gate, so reaching here already proves auth.
        ticket = app.state.tickets.mint(token=request_token(app.state.auth, request))
        return {"ticket": ticket}

    @app.get("/api/settings/system-update")
    async def get_system_update() -> Any:
        return {"update": app.state.platform_updates.status()}

    @app.post("/api/settings/system-update/check")
    async def check_system_update(request: Request) -> Any:
        body = await _require_dict_body(request, allow_empty=True)
        target = str(body.get("target") or "").strip() or None
        try:
            update = await app.state.platform_updates.check(target)
        except Exception:
            update = app.state.platform_updates.status()
        return {"update": update}

    @app.post("/api/settings/system-update/install")
    async def install_system_update(request: Request) -> Any:
        body = await _require_dict_body(request, allow_empty=True)
        target = str(body.get("target") or "").strip() or None
        try:
            update = await app.state.platform_updates.start(target, force=bool(body.get("force", False)))
        except Exception as exc:
            raise HTTPException(status_code=409, detail=str(exc))
        return {"update": update}

    @app.post("/api/settings/system-update/rollback")
    async def rollback_system_update() -> Any:
        try:
            update = await app.state.platform_updates.rollback()
        except Exception as exc:
            raise HTTPException(status_code=409, detail=str(exc))
        return {"update": update}
