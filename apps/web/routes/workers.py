"""workers routes moved from server.py."""
from __future__ import annotations

import asyncio
import copy
import hashlib
import os
import time
from typing import Any

from fastapi import (
    FastAPI,
    HTTPException,
    Request,
)

from apps.web.routes.common import (
    _require_dict_body,
)
from muteki.solver.engine_registry import (
    descriptor_payload,
)
from muteki.solver.credential_accounts import (
    account_store_root,
)

def register(app: FastAPI) -> None:
    h = app.state.route_helpers
    _reject_temporarily_disabled_engine = h._reject_temporarily_disabled_engine
    _dispatch_platform_command = h._dispatch_platform_command
    _run_receipt_status = h._run_receipt_status
    llm_settings_payload = h.llm_settings_payload
    _engine_cache = h._engine_cache
    _engine_cache_ttl_s = h._engine_cache_ttl_s
    _engine_refresh_lock = h._engine_refresh_lock
    _invalidate_engine_cache = h._invalidate_engine_cache

    @app.get("/api/settings/agent-engines")
    async def list_agent_engines() -> Any:
        return descriptor_payload()

    @app.get("/api/engines")
    async def engines() -> Any:
        from muteki.solver.cli_driver import engine_status

        now = time.time()
        if _engine_cache["data"] is not None and now - _engine_cache["ts"] <= _engine_cache_ttl_s:
            return {"engines": _engine_cache["data"]}
        if _engine_refresh_lock.locked() and _engine_cache["data"] is not None:
            return {"engines": _engine_cache["data"]}
        async with _engine_refresh_lock:
            now = time.time()
            if _engine_cache["data"] is not None and now - _engine_cache["ts"] <= _engine_cache_ttl_s:
                return {"engines": _engine_cache["data"]}
            # run the (blocking) probes off the event loop. Pass the account store
            # so health probes use the SAME creds the worker uses.
            acct_root = str(account_store_root(app.state.manager.state_root))
            try:
                cfg = app.state.manager.worker_config.get()
                backend = str(cfg.get("worker_backend") or "local")
                enabled = set(cfg.get("engines") or [])
                profiles = [
                    p for p in (cfg.get("worker_profiles") or [])
                    if (p.get("name") or p.get("id")) in enabled
                ]
            except Exception:
                backend = "local"
                profiles = []
            data = await asyncio.to_thread(engine_status, acct_root, backend, profiles)
            _engine_cache["data"] = data
            _engine_cache["ts"] = time.time()
        return {"engines": _engine_cache["data"]}

    @app.get("/api/engines/health")
    async def engines_health(request: Request) -> Any:
        # DEEP self-check. `backend` query selects local (host CLI + auth) vs
        # container (docker run --rm: image + CLI launchable inside the worker
        # image). On-demand only — the self-check page triggers it.
        from muteki.solver.cli_driver import engine_health

        backend = str(request.query_params.get("backend") or "local")
        if backend not in ("local", "container"):
            backend = "local"
        acct_root = str(account_store_root(app.state.manager.state_root))
        profiles = []
        if backend == "local":
            try:
                cfg = app.state.manager.worker_config.get()
                enabled = set(cfg.get("engines") or [])
                profiles = [
                    p for p in (cfg.get("worker_profiles") or [])
                    if (p.get("name") or p.get("id")) in enabled
                ]
            except Exception:
                profiles = []
        data = await asyncio.to_thread(engine_health, backend, acct_root, profiles)
        return {"engines": data}

    @app.get("/api/settings/workers")
    async def get_worker_settings() -> Any:
        # the default worker roster (engines + bootstrap count + per-category
        # overrides) the dispatch path falls back to when a request is silent.
        return {"config": llm_settings_payload(app.state.manager.worker_config.get())}

    def _openvpn_path() -> Any:
        return (app.state.manager.state_root / "_secrets" / "runtime"
                / "openvpn" / "client.ovpn")

    def _openvpn_status() -> dict[str, Any]:
        path = _openvpn_path()
        if not path.is_file():
            return {"present": False}
        data = path.read_bytes()
        return {
            "present": True,
            "filename": "client.ovpn",
            "size": len(data),
            "sha256": hashlib.sha256(data).hexdigest(),
        }

    @app.get("/api/settings/runtime/openvpn")
    async def get_openvpn_config() -> Any:
        return _openvpn_status()

    @app.post("/api/settings/runtime/openvpn")
    async def upload_openvpn_config(request: Request) -> Any:
        data = await request.body()
        if not data or len(data) > 256 * 1024:
            raise HTTPException(status_code=400, detail="OpenVPN 配置大小必须为 1–262144 字节")
        text = data.decode("utf-8", errors="ignore")
        required = ("client", "dev tun", "<ca>", "<cert>", "<key>")
        if any(marker not in text for marker in required):
            raise HTTPException(status_code=400, detail="上传内容不是完整的 OpenVPN 客户端配置")
        path = _openvpn_path()
        path.parent.mkdir(parents=True, exist_ok=True)
        temporary = path.with_suffix(".ovpn.tmp")
        fd = os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
        try:
            with os.fdopen(fd, "wb") as stream:
                stream.write(data)
            fd = -1
        finally:
            if fd >= 0:
                os.close(fd)
        os.replace(temporary, path)
        os.chmod(path, 0o600)
        return {"ok": True, **_openvpn_status()}

    @app.put("/api/settings/workers")
    async def put_worker_settings(request: Request) -> Any:
        body = await _require_dict_body(request)
        _reject_temporarily_disabled_engine(body)
        raw_llm_profiles = body.get("llm_profiles")
        llm_profiles = copy.deepcopy(raw_llm_profiles)
        if isinstance(llm_profiles, dict):
            for which in ("planner", "titler"):
                row = llm_profiles.get(which)
                if isinstance(row, dict):
                    if str(row.get("api_key") or "").strip():
                        raise HTTPException(
                            status_code=400,
                            detail=(
                                f"llm_profiles.{which} 不再接受内联 API Key；"
                                "请在 Agents 设置中保存模型端点并提交 endpoint_id"
                            ),
                        )
                    row.pop("api_key", None)
                    row.pop("clear_api_key", None)
                    row.pop("credential_source", None)
        settings_payload = {
            **body,
            "llm_profiles": llm_profiles,
        }
        receipt = await _dispatch_platform_command(
            "worker_settings.update", "worker_settings", "global",
            settings_payload)
        if receipt.error is not None:
            raise HTTPException(
                status_code=_run_receipt_status(receipt),
                detail=receipt.error.message)
        cfg = receipt.output.get("config") or {}
        _invalidate_engine_cache()
        return {"ok": True, "config": llm_settings_payload(cfg)}

    @app.put("/api/settings/identity")
    async def put_identity_model(request: Request) -> Any:
        # Save the Credential/Seat model. Additive to the legacy
        # PUT /workers above (which still accepts worker_profiles/engines). The
        # store validates the container×system_inherit legality gate and rejects an
        # illegal combo with 400. GET the model back via GET /workers.
        body = await _require_dict_body(request)
        _reject_temporarily_disabled_engine(body)
        receipt = await _dispatch_platform_command(
            "worker_identity.update", "worker_settings", "global", {
                "seats": body.get("seats"),
                "credentials": body.get("credentials"),
                "worker_backend": body.get("worker_backend"),
                "worker_network": body.get("worker_network"),
            })
        if receipt.error is not None:
            raise HTTPException(
                status_code=_run_receipt_status(receipt),
                detail=receipt.error.message)
        cfg = receipt.output.get("config") or {}
        _invalidate_engine_cache()
        return {"ok": True, "config": cfg}

    @app.get("/api/settings/profiles/health")
    async def get_profiles_health() -> Any:
        # Per-profile readiness for the settings badge + account rows. Uses the
        # CHEAP binding layer (zero network / zero docker) so opening the modal
        # never fires a wall of CLI hellos — the deep auth probe is the explicit
        # "测连通" button (POST below). Backend is resolved from SERVER context
        # via backend_for_profile (the same global backend mapping dispatch uses),
        # NEVER trusted from the client, so the verdict predicts
        # what a real run would use.
        from dataclasses import asdict

        from muteki.core.runtime_env import is_web_container
        from muteki.solver.profile_health import evaluate_profile_health
        from apps.web.worker_config import backend_for_profile

        cfg = app.state.manager.worker_config.get()
        profiles = [p for p in (cfg.get("worker_profiles") or []) if isinstance(p, dict)]
        worker_backend = str(cfg.get("worker_backend") or "")
        in_web = is_web_container()
        sessions_root = app.state.manager.state_root

        def _eval_all() -> list[dict]:
            out: list[dict] = []
            for p in profiles:
                backend = backend_for_profile(
                    worker_backend=worker_backend, in_web_container=in_web,
                )
                h = evaluate_profile_health(
                    p, backend=backend, sessions_root=sessions_root, depth="binding"
                )
                out.append(asdict(h))
            return out

        return {"profiles": await asyncio.to_thread(_eval_all)}

    @app.post("/api/settings/profiles/{profile_id}/health")
    async def probe_profile_health(profile_id: str) -> Any:
        # "测连通" — the DEEP probe for one profile: binding + (container) plumbing
        # + a real auth hello using the profile's PINNED model. This is the verdict
        # that matches the dispatch precheck, so a green here means the run won't
        # die on profile_unhealthy.
        from dataclasses import asdict

        from muteki.core.runtime_env import is_web_container
        from muteki.solver.profile_health import evaluate_profile_health
        from apps.web.worker_config import backend_for_profile

        cfg = app.state.manager.worker_config.get()
        profiles = [p for p in (cfg.get("worker_profiles") or []) if isinstance(p, dict)]
        match = next(
            (p for p in profiles
             if str(p.get("name") or p.get("id")) == profile_id
             or str(p.get("id")) == profile_id),
            None,
        )
        # After the identity migration a profile's id is the new seat id; the old
        # name (e.g. "claude-local") survives only in the alias table. Resolve it so
        # the "测连通" button keeps working with either an old name or a new seat id.
        if match is None:
            from muteki.solver.worker_profiles import resolve_seat_ref
            sid = resolve_seat_ref(
                profile_id, seats=cfg.get("seats") or [],
                alias_table=cfg.get("seat_alias") or {},
            )
            if sid is not None:
                match = next((p for p in profiles if str(p.get("id")) == sid), None)
        if match is None:
            raise HTTPException(status_code=404, detail=f"unknown profile: {profile_id}")
        backend = backend_for_profile(
            worker_backend=str(cfg.get("worker_backend") or ""),
            in_web_container=is_web_container(),
        )
        h = await asyncio.to_thread(
            evaluate_profile_health,
            match, backend=backend,
            sessions_root=app.state.manager.state_root, depth="auth",
        )
        return asdict(h)

    @app.get("/api/settings/worker-image")
    async def get_worker_image() -> Any:
        # P2-v3: worker-image health (daemon reachable / image pulled / version
        # match). Docker probes are blocking subprocess.run → off the event loop.
        from apps.web.worker_image import image_status
        return await asyncio.to_thread(image_status)

    @app.post("/api/settings/worker-image/pull")
    async def pull_worker_image() -> Any:
        # P2-v3: one-click `docker pull` of the worker image. Can take minutes;
        # to_thread it so the single uvicorn loop keeps serving.
        from apps.web.worker_image import pull_image
        return await asyncio.to_thread(pull_image)
