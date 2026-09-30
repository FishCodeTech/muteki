"""system routes moved from server.py."""
from __future__ import annotations

from typing import Any

from fastapi import (
    FastAPI,
    HTTPException,
    Request,
)
from fastapi.responses import JSONResponse

from apps.web.auth import (
    AuthConfig,
    check_password,
    issue_token,
)
from apps.web.routes.common import (
    _require_dict_body,
)
from muteki.version import get_version

def register(app: FastAPI) -> None:
    platform_stack = app.state.platform_stack

    def identity_scope() -> dict[str, str]:
        # The browser origin identifies an address, not the data installed
        # behind it. Bind durable client state to the actual platform database.
        return {"service_id": platform_stack.store.installation_id,
                "identity_id": "operator"}

    @app.get("/api/health")
    async def health() -> Any:
        return {"version": get_version(), **platform_stack.health()}

    @app.get("/api/readiness")
    async def readiness() -> Any:
        payload = {"version": get_version(), **platform_stack.health()}
        return JSONResponse(payload, status_code=200 if platform_stack.ready else 503)

    @app.post("/api/auth/login")
    async def auth_login(request: Request) -> Any:
        # Exchange the operator password for a signed session token. This route
        # is intentionally reachable WITHOUT a token (you have none yet). When
        # auth is disabled it still returns a (useless) token so the frontend
        # flow is uniform.
        cfg: AuthConfig = app.state.auth
        body = await _require_dict_body(request, allow_empty=True)
        if not cfg.enabled:
            return {"ok": True, "token": "", "auth_required": False,
                    **identity_scope()}
        if not check_password(cfg, body.get("password")):
            # constant-time compare already done; uniform 401, no "wrong length"
            raise HTTPException(status_code=401, detail="invalid password")
        return {"ok": True, "token": issue_token(cfg), "auth_required": True,
                **identity_scope()}

    @app.get("/api/auth/me")
    async def auth_me(request: Request) -> Any:
        # Cheap "is my token still valid?" probe. Reachable past the gate only
        # with a valid token (when enabled), so a 200 means authenticated.
        # in_container (P2-v3): tells the UI the coordinator runs inside a
        # container, so the deck must force container mode and disable the
        # "local" worker-isolation toggle (local is rejected server-side anyway).
        from muteki.core.runtime_env import is_web_container
        cfg: AuthConfig = app.state.auth
        return {"authenticated": True, "auth_required": cfg.enabled,
                "in_container": is_web_container(), **identity_scope()}

    @app.post("/api/auth/ticket")
    async def auth_ticket(request: Request) -> Any:
        # Mint a one-time, short-TTL ticket for opening an SSE/WS connection
        # (which can't carry an Authorization header). Requires a valid token —
        # it sits behind the gate, so reaching here already proves auth.
        ticket = app.state.tickets.mint()
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
