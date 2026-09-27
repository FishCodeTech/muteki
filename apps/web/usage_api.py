"""Read-only usage views shared by global, Run and Competition pages."""
from __future__ import annotations
import json
import time
from pathlib import Path
from threading import Lock
from typing import Any

from fastapi import APIRouter, Query, HTTPException, Request
from muteki.competition.models import RunBinding


def _quota_cache_path(state_root: str | Path) -> Path:
    return Path(state_root) / "_quota_cache.json"


def _read_quota_cache(state_root: str | Path) -> dict[str, Any]:
    try:
        raw = json.loads(_quota_cache_path(state_root).read_text(encoding="utf-8"))
        return raw if isinstance(raw, dict) else {}
    except (OSError, json.JSONDecodeError):
        return {}


def _write_quota_cache(state_root: str | Path, cache: dict[str, Any]) -> None:
    path = _quota_cache_path(state_root)
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_suffix(".json.tmp")
    tmp.write_text(json.dumps(cache, ensure_ascii=False, indent=2), encoding="utf-8")
    tmp.replace(path)


def create_usage_router(manager, competition_store):
    router = APIRouter(tags=['usage'])
    ownership_lock = Lock()
    ownership_signature: tuple[tuple[str, str, str], ...] | None = None

    def sync_competition_ownership():
        nonlocal ownership_signature
        signature = tuple(
            (binding.run_id, binding.competition_id,
             binding.competition_challenge_id)
            for binding in competition_store.list(RunBinding)
            if binding.run_id and binding.competition_id
        )
        if signature == ownership_signature:
            return
        with ownership_lock:
            if signature == ownership_signature:
                return
            manager.usage.bind_runs(signature)
            ownership_signature = signature

    @router.get('/api/usage')
    def usage(start: float = Query(0, ge=0), end: float | None = Query(None, ge=0),
              run_id: str | None = None, thread_id: str | None = None,
              competition_id: str | None = None, challenge_id: str | None = None,
              model: str | None = None,
              role: str | None = None, workspace_kind: str | None = None,
              generation: int | None = Query(None, ge=0),
              offset: int = Query(0, ge=0), limit: int = Query(100, ge=1, le=500)):
        if end is not None and end < start:
            raise HTTPException(422, "结束时间必须晚于开始时间")
        # Persist ownership so archived/deleted bindings do not erase attribution.
        sync_competition_ownership()
        return manager.usage.query(start=start, end=end, run_id=run_id, thread_id=thread_id,
                                   competition_id=competition_id, challenge_id=challenge_id,
                                   model=model, role=role,
                                   generation=generation, workspace_kind=workspace_kind,
                                   actor_kind='worker' if competition_id else None,
                                   offset=offset, limit=limit)
    @router.post('/api/usage/import-history')
    def import_history():
        return manager.usage.import_run_history()

    @router.get('/api/usage/quota')
    def get_quota() -> Any:
        """List subscription quota entries for all registered credentials.

        Each entry reports the cached quota state for one credential account.
        The default state is ``unknown`` — the quota source for most CLI engines
        cannot be read without a live authenticated request, so Muteki stores only
        what the engine has explicitly reported via response headers or a prior
        manual snapshot. Duplicate accounts (same credential_id) are collapsed:
        the most-recently-updated entry wins.

        Refresh semantics (acceptance criterion 3): this endpoint is read-only.
        Use POST /api/usage/quota/{credential_id}/refresh to explicitly re-read
        a credential's quota without starting an agent or consuming any tokens.
        """
        from muteki.solver.credential_accounts import (
            CredentialAccountStore,
            account_credential_id,
            account_store_root,
            system_credential_id,
        )
        from muteki.solver.engine_registry import SUPPORTED_ENGINE_IDS

        state_root = manager.state_root
        store = CredentialAccountStore(account_store_root(state_root))
        cache = _read_quota_cache(state_root)
        seen: set[str] = set()
        entries: list[dict[str, Any]] = []

        for account in store.list():
            account_id = str(account.get("account_id") or "").strip()
            if not account_id:
                continue
            engine = str(
                account.get("worker_engine") or account.get("engine") or ""
            ).strip().lower()
            credential_id = account_credential_id(account_id)
            if credential_id in seen:
                continue
            seen.add(credential_id)
            connection = str(account.get("connection") or "official").strip()
            mode = str(account.get("mode") or "subscription").strip()
            # API key / custom endpoint accounts do not have a subscription quota window.
            quota_type = "api_key" if (connection == "custom_endpoint" or mode == "api_key") else "subscription"
            cached = cache.get(credential_id) or {}
            entry: dict[str, Any] = {
                "credential_id": credential_id,
                "account_id": account_id,
                "engine": engine,
                "label": str(account.get("label") or account_id),
                "quota_type": quota_type,
                "status": cached.get("status") or ("not_supported" if quota_type == "api_key" else "unknown"),
                "remaining": cached.get("remaining"),
                "total": cached.get("total"),
                "window_seconds": cached.get("window_seconds"),
                "reset_at": cached.get("reset_at"),
                "updated_at": cached.get("updated_at"),
                "unknown_reason": cached.get("unknown_reason") or (
                    "API 密钥账号无订阅窗口" if quota_type == "api_key"
                    else "此引擎不主动上报额度；可在会话中使用后手动刷新"
                ),
            }
            entries.append(entry)

        # Append system-login credentials (subscription only, one per engine).
        for engine in SUPPORTED_ENGINE_IDS:
            cred_id = system_credential_id(engine)
            if cred_id in seen:
                continue
            seen.add(cred_id)
            cached = cache.get(cred_id) or {}
            entry = {
                "credential_id": cred_id,
                "account_id": "",
                "engine": engine,
                "label": f"{engine}（系统登录）",
                "quota_type": "subscription",
                "status": cached.get("status") or "unknown",
                "remaining": cached.get("remaining"),
                "total": cached.get("total"),
                "window_seconds": cached.get("window_seconds"),
                "reset_at": cached.get("reset_at"),
                "updated_at": cached.get("updated_at"),
                "unknown_reason": cached.get("unknown_reason") or "此引擎不主动上报额度；可在会话中使用后手动刷新",
            }
            entries.append(entry)

        return {"quota": entries, "as_of": time.time()}

    @router.post('/api/usage/quota/{credential_id}/refresh')
    async def refresh_quota(credential_id: str) -> Any:
        """Re-read quota for a specific credential without starting an agent.

        For CLI-based engines the live quota is not available without a billed
        request; this endpoint clears the unknown_reason and resets the timestamp
        so the UI knows a refresh was attempted. Explicit quota values must be
        reported by the engine runtime and stored via the internal quota update
        path — manual edits are not accepted here.
        """
        from muteki.solver.credential_accounts import canonical_credential_id

        try:
            stable_id = canonical_credential_id(credential_id)
        except ValueError as exc:
            raise HTTPException(status_code=400, detail=str(exc)) from exc

        state_root = manager.state_root
        cache = _read_quota_cache(state_root)
        existing = cache.get(stable_id) or {}
        # Preserve any previously reported values; only update the refresh timestamp.
        updated: dict[str, Any] = {
            **existing,
            "refreshed_at": time.time(),
            "unknown_reason": existing.get("unknown_reason") or "此引擎不主动上报额度",
        }
        if not existing.get("status"):
            updated["status"] = "unknown"
        cache[stable_id] = updated
        _write_quota_cache(state_root, cache)
        return {"ok": True, "credential_id": stable_id, "entry": updated}

    @router.put('/api/usage/quota/{credential_id}')
    async def update_quota(credential_id: str, request: Request) -> Any:
        """Internal endpoint to store a quota snapshot reported by the runtime.

        Only accepts fields: status, remaining, total, window_seconds, reset_at.
        Called by the runtime adapter when an engine returns usage/rate-limit
        headers; never called by the frontend directly.
        """
        from muteki.solver.credential_accounts import canonical_credential_id

        try:
            stable_id = canonical_credential_id(credential_id)
        except ValueError as exc:
            raise HTTPException(status_code=400, detail=str(exc)) from exc

        body = await request.json()
        if not isinstance(body, dict):
            raise HTTPException(status_code=400, detail="body must be a JSON object")
        allowed = {"status", "remaining", "total", "window_seconds", "reset_at", "unknown_reason"}
        payload: dict[str, Any] = {k: v for k, v in body.items() if k in allowed}
        if not payload:
            raise HTTPException(status_code=400, detail="no recognized fields in body")

        state_root = manager.state_root
        cache = _read_quota_cache(state_root)
        existing = cache.get(stable_id) or {}
        cache[stable_id] = {**existing, **payload, "updated_at": time.time()}
        _write_quota_cache(state_root, cache)
        return {"ok": True, "credential_id": stable_id}

    return router
