"""models routes moved from server.py."""
from __future__ import annotations

import asyncio
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
    SUPPORTED_ENGINE_IDS,
    ensure_engine_supported,
)
from muteki.solver.credential_accounts import (
    CredentialAccountStore,
    account_id_from_credential_id,
    account_store_root,
    canonical_credential_id,
    system_credential_id,
)

def register(app: FastAPI) -> None:
    h = app.state.route_helpers
    _primary_catalog_runtime_key = h._primary_catalog_runtime_key
    _refresh_credential_catalog_record = h._refresh_credential_catalog_record
    _reject_temporarily_disabled_engine = h._reject_temporarily_disabled_engine
    _credential_usage_index = h._credential_usage_index

    @app.get("/api/settings/worker-models")
    async def get_worker_models() -> Any:
        from apps.web.worker_models import worker_model_options_payload

        return worker_model_options_payload(app.state.manager.state_root)

    @app.post("/api/settings/worker-models/discover")
    async def discover_worker_models_now(request: Request) -> Any:
        from muteki.core.runtime_env import is_web_container
        from apps.web.worker_config import backend_for_profile
        from apps.web.worker_models import (
            worker_model_options_payload,
        )

        cfg = app.state.manager.worker_config.get()
        profiles = [
            profile
            for profile in (cfg.get("worker_profiles") or [])
            if isinstance(profile, dict)
        ]
        body = await _require_dict_body(request, allow_empty=True)
        _reject_temporarily_disabled_engine(body)
        profile_id = str(body.get("profile_id") or "").strip()
        if profile_id:
            profiles = [
                profile for profile in profiles
                if profile_id in {
                    str(profile.get("id") or "").strip(),
                    str(profile.get("name") or "").strip(),
                }
            ]
        account_store = CredentialAccountStore(
            account_store_root(app.state.manager.state_root))
        public_accounts = {
            str(item.get("account_id") or ""): item
            for item in account_store.list()
        }
        results: list[dict[str, Any]] = []
        for profile in profiles:
            engine = str(profile.get("engine") or "").strip().lower()
            ensure_engine_supported(engine)
            if engine not in SUPPORTED_ENGINE_IDS:
                continue
            backend = backend_for_profile(
                worker_backend=str(cfg.get("worker_backend") or ""),
                in_web_container=is_web_container(),
            )
            raw_credential = str(
                profile.get("credential_id")
                or profile.get("credential_account")
                or ""
            ).strip()
            if raw_credential in {"", "__system__"}:
                stable_id = system_credential_id(engine)
                account_id = ""
            else:
                stable_id = canonical_credential_id(
                    raw_credential, engine=engine)
                account_id = account_id_from_credential_id(stable_id)
            public = public_accounts.get(account_id) or {}
            account = account_store.inspect(account_id) if account_id else None
            details = dict(account.details or {}) if account else {}
            result, catalog = await _refresh_credential_catalog_record(
                credential_id=stable_id,
                engine=engine,
                environment=backend,
                runtime_instance=_primary_catalog_runtime_key(engine),
                connection=str(public.get("connection") or "official"),
                base_url=str(details.get("base_url_value") or ""),
                secret=str(details.get("secret_value") or ""),
                account_id=account_id,
                system_credential=not bool(account_id),
                provider=str(public.get("provider") or ""),
                configured_models=[
                    str(item).strip() for item in public.get("models") or []
                    if str(item).strip()
                ],
                default_model=str(public.get("default_model") or ""),
            )
            results.append({
                **result,
                "profile_id": str(
                    profile.get("id") or profile.get("name") or engine),
                "credential_id": stable_id,
                "catalog": catalog,
            })
        payload = worker_model_options_payload(app.state.manager.state_root)
        payload["discovery_results"] = results
        payload["discovery_ok"] = any(bool(result.get("ok")) for result in results)
        return payload

    @app.post("/api/settings/worker-model/test")
    async def probe_worker_model(request: Request) -> Any:
        body = await _require_dict_body(request)
        _reject_temporarily_disabled_engine(body)
        from apps.web.worker_config import backend_for_profile
        from apps.web.worker_models import WorkerModelTestStore, probe_worker_model
        from muteki.core.runtime_env import is_web_container

        profile = body.get("profile")
        if not isinstance(profile, dict):
            raise HTTPException(status_code=400, detail="profile must be an object")
        cfg = app.state.manager.worker_config.get()
        backend = backend_for_profile(
            worker_backend=str(cfg.get("worker_backend") or ""),
            in_web_container=is_web_container(),
        )
        runtime = {"network": str(cfg.get("worker_network") or "bridge")}
        result = await asyncio.to_thread(
            probe_worker_model,
            profile=profile,
            model=str(body.get("model") or ""),
            reasoning_effort=str(body.get("reasoning_effort") or "default"),
            sessions_root=app.state.manager.state_root,
            backend=backend,
            runtime=runtime,
        )
        credential_id = str(profile.get("credential_id") or "").strip()
        if credential_id:
            CredentialAccountStore(
                account_store_root(app.state.manager.state_root)
            ).save_test_status(credential_id, result, backend=backend)
        saved = WorkerModelTestStore(
            app.state.manager.state_root
        ).save_result(
            profile_id=str(profile.get("id") or profile.get("name") or ""),
            profile=profile,
            model=str(body.get("model") or ""),
            reasoning_effort=str(body.get("reasoning_effort") or "default"),
            backend=backend,
            runtime=runtime,
            result=result,
        )
        if saved.get("tested_at") is not None:
            result["tested_at"] = saved["tested_at"]
        return result

    @app.get("/api/settings/worker-model/test-results")
    async def get_worker_model_test_results() -> Any:
        from apps.web.worker_config import backend_for_profile
        from apps.web.worker_models import WorkerModelTestStore
        from muteki.core.runtime_env import is_web_container

        cfg = app.state.manager.worker_config.get()
        profiles = [
            profile
            for profile in (cfg.get("worker_profiles") or [])
            if isinstance(profile, dict)
        ]
        backend = backend_for_profile(
            worker_backend=str(cfg.get("worker_backend") or ""),
            in_web_container=is_web_container(),
        )
        runtime = {"network": str(cfg.get("worker_network") or "bridge")}
        results = WorkerModelTestStore(
            app.state.manager.state_root
        ).matching_results(
            profiles=profiles,
            backend=backend,
            runtime=runtime,
        )
        return {"results": results}

    @app.post("/api/settings/worker-model/test-batch")
    async def probe_worker_models_batch(request: Request) -> Any:
        body = await _require_dict_body(request)
        _reject_temporarily_disabled_engine(body)
        from apps.web.worker_config import backend_for_profile
        from apps.web.worker_models import (
            WorkerModelTestStore,
            probe_worker_models_batch,
        )
        from muteki.core.runtime_env import is_web_container

        items = body.get("items")
        if not isinstance(items, list):
            raise HTTPException(status_code=400, detail="items must be an array")
        cfg = app.state.manager.worker_config.get()
        backend = backend_for_profile(
            worker_backend=str(cfg.get("worker_backend") or ""),
            in_web_container=is_web_container(),
        )
        runtime = {"network": str(cfg.get("worker_network") or "bridge")}
        payload = await asyncio.to_thread(
            probe_worker_models_batch,
            items=[item for item in items if isinstance(item, dict)],
            sessions_root=app.state.manager.state_root,
            backend=backend,
            runtime=runtime,
        )
        result_rows = [
            row for row in (payload.get("results") or [])
            if isinstance(row, dict)
        ]
        results_by_profile = {
            str(row.get("profile_id") or ""): row
            for row in result_rows
            if str(row.get("profile_id") or "")
        }
        store = WorkerModelTestStore(app.state.manager.state_root)
        for index, item in enumerate(items):
            if not isinstance(item, dict):
                continue
            profile = item.get("profile")
            if not isinstance(profile, dict):
                continue
            profile_id = str(
                item.get("profile_id")
                or profile.get("id")
                or profile.get("name")
                or ""
            ).strip()
            result = results_by_profile.get(profile_id)
            if result is None and index < len(result_rows):
                result = result_rows[index]
            if result is None:
                continue
            credential_id = str(profile.get("credential_id") or "").strip()
            if credential_id:
                CredentialAccountStore(
                    account_store_root(app.state.manager.state_root)
                ).save_test_status(credential_id, result, backend=backend)
            saved = store.save_result(
                profile_id=profile_id,
                profile=profile,
                model=str(item.get("model") or ""),
                reasoning_effort=str(
                    item.get("reasoning_effort") or "default"
                ),
                backend=backend,
                runtime=runtime,
                result=result,
            )
            if saved.get("tested_at") is not None:
                result["tested_at"] = saved["tested_at"]
        return payload

    @app.post("/api/settings/llm/test")
    async def probe_llm_endpoint_route(request: Request) -> Any:
        # Test a persisted global credential selection with the selected model.
        body = await _require_dict_body(request)
        from apps.web.llm_credentials import resolve_llm_profile_credential
        from apps.web.llm_probe import probe_llm_endpoint

        which = str(body.get("which") or "planner")
        saved_profiles = (
            app.state.manager.worker_config.get().get("llm_profiles") or {})
        profile = {
            **dict(saved_profiles.get(which) or {}),
            **{
                key: body[key]
                for key in (
                    "endpoint_id", "model", "temperature_mode", "temperature"
                )
                if body.get(key) is not None
            },
        }
        try:
            resolved = resolve_llm_profile_credential(
                which,
                profile,
                sessions_root=app.state.manager.state_root,
            )
        except ValueError as exc:
            raise HTTPException(status_code=400, detail=str(exc))
        return await probe_llm_endpoint(
            which=which,
            base_url=resolved.base_url or None,
            model=str(profile.get("model") or ""),
            api_key=resolved.api_key,
            temperature_mode=profile.get("temperature_mode"),
            temperature=profile.get("temperature"),
        )
