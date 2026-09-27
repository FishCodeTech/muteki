"""credentials routes moved from server.py."""
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
    account_credential_id,
    account_id_from_credential_id,
    account_store_root,
    canonical_credential_id,
    engine_from_system_credential_id,
    ensure_pi_ollama_account_from_env,
    invalidate_system_login_cache,
    system_credential_id,
)

def register(app: FastAPI) -> None:
    h = app.state.route_helpers
    _catalog_runtime_keys_by_engine = h._catalog_runtime_keys_by_engine
    _primary_catalog_runtime_key = h._primary_catalog_runtime_key
    _refresh_credential_catalog_record = h._refresh_credential_catalog_record
    _refresh_stale_credential_catalogs = h._refresh_stale_credential_catalogs
    _reject_temporarily_disabled_engine = h._reject_temporarily_disabled_engine
    _credential_usage_index = h._credential_usage_index

    @app.post("/api/settings/credential-models/refresh-stale")
    async def refresh_stale_credential_models() -> Any:
        return await _refresh_stale_credential_catalogs()

    @app.get("/api/settings/credentials")
    async def list_credentials(
        environment: str = "",
        fresh: bool = False,
    ) -> Any:
        from apps.web.worker_config import backend_for_profile
        from apps.web.worker_models import CredentialModelCatalogStore
        from muteki.core.runtime_env import is_web_container

        if fresh:
            invalidate_system_login_cache()

        requested_environment = str(environment or "").strip().lower()
        if requested_environment and requested_environment not in {
            "local", "container",
        }:
            raise HTTPException(
                status_code=422,
                detail="environment 必须是 local 或 container",
            )
        state_root = app.state.manager.state_root
        cfg = app.state.manager.worker_config.get()
        catalog_environment = requested_environment or backend_for_profile(
            worker_backend=str(cfg.get("worker_backend") or ""),
            in_web_container=is_web_container(),
        )

        usage = _credential_usage_index()

        def _payload() -> dict[str, Any]:
            store = CredentialAccountStore(account_store_root(state_root))
            catalog_store = CredentialModelCatalogStore(state_root)
            catalogs: dict[str, dict[str, Any]] = {}
            for account in store.list():
                account_id = str(account.get("account_id") or "").strip()
                engine = str(
                    account.get("worker_engine") or account.get("engine") or ""
                ).strip().lower()
                if not account_id or engine not in SUPPORTED_ENGINE_IDS:
                    continue
                credential_id = account_credential_id(account_id)
                catalog = catalog_store.get(
                    credential_id, engine, catalog_environment,
                    _primary_catalog_runtime_key(engine),
                )
                if catalog is not None:
                    catalogs[credential_id] = catalog
            for engine in SUPPORTED_ENGINE_IDS:
                credential_id = system_credential_id(engine)
                catalog = catalog_store.get(
                    credential_id, engine, catalog_environment,
                    _primary_catalog_runtime_key(engine),
                )
                if catalog is not None:
                    catalogs[credential_id] = catalog
            rows = store.credential_catalog(
                usage=usage,
                catalogs=catalogs,
                engines=SUPPORTED_ENGINE_IDS,
            )
            runtime_keys = _catalog_runtime_keys_by_engine(transport_kind="")
            for row in rows:
                row["model_catalogs"] = {
                    key: catalog
                    for key in runtime_keys.get(row["engine"], [])
                    if (catalog := catalog_store.get(
                        row["id"], row["engine"], catalog_environment, key,
                    )) is not None
                }
            return {"credentials": rows}

        return await asyncio.to_thread(_payload)

    @app.get("/api/settings/model-endpoints")
    async def list_model_endpoint_connections() -> Any:
        """List HTTP model endpoints without exposing Worker engine metadata."""
        from apps.web.llm_credentials import (
            account_id_from_model_endpoint_id,
            list_model_endpoints,
            model_endpoint_id,
        )

        endpoint_usage: dict[str, list[dict[str, Any]]] = {}
        for which, profile in (
            (app.state.manager.worker_config.get().get("llm_profiles") or {}).items()
        ):
            if which not in {"planner", "titler"} or not isinstance(profile, dict):
                continue
            raw_id = str(profile.get("endpoint_id") or "").strip()
            if not raw_id and profile.get("credential_id"):
                account_id = account_id_from_credential_id(
                    str(profile.get("credential_id") or ""))
                raw_id = model_endpoint_id(account_id) if account_id else ""
            account_id = account_id_from_model_endpoint_id(raw_id)
            if not account_id:
                continue
            endpoint_usage.setdefault(model_endpoint_id(account_id), []).append({
                "kind": "llm_profile",
                "id": str(which),
                "label": "任务规划" if which == "planner" else "对话标题",
                "model": str(profile.get("model") or ""),
            })
        return {"endpoints": list_model_endpoints(
            app.state.manager.state_root,
            usage=endpoint_usage,
        )}

    @app.post("/api/settings/model-endpoints")
    async def create_model_endpoint_connection(request: Request) -> Any:
        """Create or update an engine-independent HTTP endpoint from LLM settings.

        New endpoints require an API key. Editing an existing endpoint may omit
        ``api_key`` to keep the previously stored secret while changing Base URL,
        provider, or model.
        """
        body = await _require_dict_body(request)
        account_id = str(body.get("id") or body.get("label") or "").strip()
        base_url = str(body.get("base_url") or "").strip()
        api_key = str(body.get("api_key") or "").strip()
        model = str(body.get("model") or "").strip()
        provider = str(body.get("provider") or "").strip()
        if not account_id:
            raise HTTPException(status_code=400, detail="请填写端点名称")
        if not base_url:
            raise HTTPException(status_code=400, detail="请填写 HTTP Base URL")
        if not model:
            raise HTTPException(status_code=400, detail="请填写模型 ID")
        store = CredentialAccountStore(
            account_store_root(app.state.manager.state_root))
        existing = store.inspect(account_id)
        if existing is None and not api_key:
            raise HTTPException(status_code=400, detail="请填写 API Key")
        try:
            store.upsert_secret(
                account_id=account_id,
                engine="api",
                secret=api_key or None,
                base_url=base_url,
                provider=provider or None,
                target_model=model,
                models=[model],
            )
        except ValueError as exc:
            raise HTTPException(status_code=400, detail=str(exc)) from exc
        from apps.web.llm_credentials import list_model_endpoints, model_endpoint_id

        endpoint_id = model_endpoint_id(account_id)
        endpoint = next((
            row for row in list_model_endpoints(app.state.manager.state_root)
            if row.get("id") == endpoint_id
        ), None)
        if endpoint is None:
            raise HTTPException(status_code=500, detail="模型端点保存后未能读取")
        return {
            "ok": True,
            "endpoint": endpoint,
            "updated": existing is not None,
        }

    @app.post("/api/settings/credentials/{credential_id}/test")
    async def probe_credential(credential_id: str, request: Request) -> Any:
        body = await _require_dict_body(request, allow_empty=True)
        _reject_temporarily_disabled_engine(body)
        backend = str(body.get("backend") or "local").strip()
        backend = "container" if backend == "container" else "local"
        try:
            stable_id = canonical_credential_id(credential_id)
        except ValueError as exc:
            raise HTTPException(status_code=400, detail=str(exc))
        account_id = account_id_from_credential_id(stable_id)
        system_engine = engine_from_system_credential_id(stable_id)
        if system_engine:
            ensure_engine_supported(system_engine)
        if account_id:
            from apps.web.worker_models import probe_worker_model

            account = CredentialAccountStore(
                account_store_root(app.state.manager.state_root)).inspect(account_id)
            engine = str(
                body.get("engine")
                or ((account.details or {}).get("target_engine") if account else "")
                or (account.engine if account else "")
                or ""
            ).strip()
            ensure_engine_supported(engine)
            selected_model = str(body.get("model") or "").strip()
            if not selected_model:
                result = {"ok": False, "detail": "请选择一个候选模型后执行真实测试",
                          "layer": "model", "model": ""}
            elif account is None or not account.present:
                result = {"ok": False, "detail": "凭据账号不存在或未就绪",
                          "layer": "binding", "model": selected_model}
            else:
                details = dict(account.details or {})
                cfg = app.state.manager.worker_config.get()
                result = await asyncio.to_thread(
                    probe_worker_model,
                    profile={
                        "id": stable_id, "name": stable_id, "engine": engine,
                        "credential_id": stable_id,
                        "credential_account": account_id,
                        "credential_mode": (
                            "api_key" if details.get("api_key_file")
                            else "subscription"),
                        "base_url": str(details.get("base_url_value") or ""),
                        "model": selected_model, "enabled": True,
                    },
                    model=selected_model,
                    reasoning_effort=str(
                        body.get("reasoning_effort") or "default"),
                    sessions_root=app.state.manager.state_root,
                    backend=backend,
                    runtime={"network": str(
                        cfg.get("worker_network") or "bridge")},
                )
        elif system_engine:
            if backend == "container":
                result = {
                    "ok": False,
                    "detail": "系统登录仅适用于本地运行环境",
                    "layer": "binding",
                }
            else:
                from apps.web.worker_models import probe_worker_model

                selected_model = str(body.get("model") or "").strip()
                if not selected_model:
                    result = {
                        "ok": False,
                        "detail": "请选择一个候选模型后执行真实测试",
                        "layer": "model",
                    }
                else:
                    cfg = app.state.manager.worker_config.get()
                    result = await asyncio.to_thread(
                        probe_worker_model,
                        profile={
                            "id": stable_id,
                            "name": stable_id,
                            "engine": system_engine,
                            "credential_id": stable_id,
                            "credential_account": "",
                            "credential_kind": "system_inherit",
                            "credential_mode": "subscription",
                            "model": selected_model,
                            "enabled": True,
                        },
                        model=selected_model,
                        reasoning_effort=str(
                            body.get("reasoning_effort") or "default"),
                        sessions_root=app.state.manager.state_root,
                        backend="local",
                        runtime={
                            "network": str(
                                cfg.get("worker_network") or "bridge")
                        },
                    )
        else:
            raise HTTPException(status_code=400, detail="unknown credential id")
        store = CredentialAccountStore(
            account_store_root(app.state.manager.state_root))
        last_test = store.save_test_status(
            stable_id, result, backend=backend)
        from apps.web.worker_models import CredentialModelCatalogStore

        CredentialModelCatalogStore(
            app.state.manager.state_root
        ).mark_verified(
            credential_id=stable_id,
            engine=(system_engine or locals().get("engine") or ""),
            environment=backend,
            runtime_instance=str(body.get("runtime_instance") or "default"),
            model=str(result.get("model") or body.get("model") or ""),
            ok=bool(result.get("ok")),
        )
        return {**result, "credential_id": stable_id, "last_test": last_test}

    @app.post("/api/settings/credentials/{credential_id}/models/refresh")
    async def refresh_credential_models(credential_id: str, request: Request) -> Any:
        body = await _require_dict_body(request, allow_empty=True)
        _reject_temporarily_disabled_engine(body)
        try:
            stable_id = canonical_credential_id(credential_id)
        except ValueError as exc:
            raise HTTPException(status_code=400, detail=str(exc)) from exc
        account_id = account_id_from_credential_id(stable_id)
        system_engine = engine_from_system_credential_id(stable_id)
        connection = str(body.get("connection") or "").strip().lower()
        engine = str(body.get("engine") or "").strip().lower()
        base_url = str(body.get("base_url") or "").strip()
        secret = str(body.get("secret") or "").strip()
        backend = "container" if str(body.get("backend") or "") == "container" else "local"
        runtime_instance = str(body.get("runtime_instance") or "default").strip() or "default"
        store = CredentialAccountStore(account_store_root(app.state.manager.state_root))
        account = store.inspect(account_id) if account_id else None
        details = dict(account.details or {}) if account else {}
        if not connection:
            connection = (
                "custom_endpoint"
                if base_url or details.get("base_url_value")
                else "official"
            )
        if not engine:
            engine = str(
                (details.get("target_engine") if account else "")
                or (account.engine if account else "")
                or system_engine
                or ""
            ).strip().lower()
        ensure_engine_supported(engine or system_engine)
        if not base_url:
            base_url = str(details.get("base_url_value") or "").strip()
        if not secret:
            secret = str(details.get("secret_value") or "").strip()

        if not (system_engine or account_id):
            raise HTTPException(status_code=400, detail="unknown credential id")

        selected_engine = engine or system_engine or ""
        account_public = next((
            row for row in store.list()
            if str(row.get("account_id") or "") == account_id
        ), {}) if account_id else {}
        configured_models = [
            str(item).strip() for item in account_public.get("models") or []
            if str(item).strip()
        ]
        default_model = str(account_public.get("default_model") or "").strip()
        provider = str(account_public.get("provider") or "").strip()
        result, catalog = await _refresh_credential_catalog_record(
            credential_id=stable_id,
            engine=selected_engine,
            environment=backend,
            runtime_instance=runtime_instance,
            connection=connection,
            base_url=base_url,
            secret=secret,
            account_id=account_id or "",
            system_credential=bool(system_engine),
            provider=provider,
            configured_models=configured_models,
            default_model=default_model,
        )
        models = [
            str(item.get("id") if isinstance(item, dict) else item).strip()
            for item in catalog.get("discovered_models") or []
        ]
        models = [item for item in models if item]
        return {
            "ok": bool(result.get("ok")),
            "detail": str(result.get("detail") or ""),
            "source": str(result.get("source") or ""),
            "models": models,
            "credential_id": stable_id,
            "catalog": catalog,
            "error_code": str(result.get("error_code") or ""),
        }

    @app.get("/api/settings/credential-accounts")
    async def list_credential_accounts() -> Any:
        # Re-hydrate from Cursor secrets on read so Agents UI sees pi-ollama even
        # when the snapshot was built without PI_API_KEY.
        ensure_pi_ollama_account_from_env(app.state.manager.state_root)
        store = CredentialAccountStore(account_store_root(app.state.manager.state_root))
        return {"accounts": store.list()}

    @app.put("/api/settings/credential-accounts/{account_id}")
    async def put_credential_account(account_id: str, request: Request) -> Any:
        body = await _require_dict_body(request)
        _reject_temporarily_disabled_engine(body)
        store = CredentialAccountStore(account_store_root(app.state.manager.state_root))
        requested_engine = str(
            body.get("worker_engine") or body.get("target_engine") or body.get("engine") or ""
        ).strip().lower()
        ensure_engine_supported(requested_engine)
        connection = str(body.get("connection") or "").strip().lower()
        if not connection:
            legacy_engine = str(body.get("engine") or "").strip().lower()
            legacy_base_url = str(body.get("base_url") or "").strip()
            connection = (
                "custom_endpoint"
                if legacy_engine == "api" or legacy_base_url
                else "official"
            )
        if connection not in {"official", "custom_endpoint"}:
            raise HTTPException(status_code=400, detail="connection must be official or custom_endpoint")
        if requested_engine not in SUPPORTED_ENGINE_IDS:
            raise HTTPException(
                status_code=400,
                detail="worker_engine must be claude, codex, cursor, pi, omp, kimi, "
                       "grok, opencode, or devin",
            )
        base_url = str(body.get("base_url") or "").strip()
        if connection == "custom_endpoint" and not base_url:
            raise HTTPException(status_code=400, detail="自定义端点必须填写 Base URL")
        storage_engine = "api" if connection == "custom_endpoint" else requested_engine
        try:
            account = store.upsert_secret(
                account_id=account_id,
                engine=storage_engine,
                secret=(body.get("secret") if body.get("secret") is not None else None),
                codex_auth_json=(
                    body.get("codex_auth_json")
                    if body.get("codex_auth_json") is not None else None
                ),
                base_url=base_url,
                target_engine=requested_engine if connection == "custom_endpoint" else None,
                provider=(body.get("provider") if body.get("provider") is not None else None),
                target_model=(
                    body.get("target_model")
                    if body.get("target_model") is not None
                    else body.get("model")
                ),
                models=body.get("models"),
                clear_base_url=connection == "official",
            )
        except ValueError as exc:
            raise HTTPException(status_code=400, detail=str(exc))
        from apps.web.worker_models import CredentialModelCatalogStore

        catalog_store = CredentialModelCatalogStore(
            app.state.manager.state_root)
        configured_models = [
            str(item).strip() for item in account.get("models") or []
            if str(item).strip()
        ]
        runtime_keys = _catalog_runtime_keys_by_engine().get(
            requested_engine) or [f"cli.{requested_engine}:default"]
        for environment in ("local", "container"):
            for runtime_key in runtime_keys:
                catalog_store.sync_configuration(
                    credential_id=account_credential_id(account_id),
                    engine=requested_engine,
                    environment=environment,
                    runtime_instance=runtime_key,
                    configured_models=configured_models,
                    default_model=str(account.get("default_model") or ""),
                )
        return {"ok": True, "account": account}

    @app.delete("/api/settings/credential-accounts/{account_id}")
    async def delete_credential_account(
        account_id: str,
        detach_references: bool = False,
    ) -> Any:
        try:
            stable_id = account_credential_id(account_id)
        except ValueError as exc:
            raise HTTPException(status_code=400, detail=str(exc))
        all_usages = _credential_usage_index().get(stable_id) or []
        usages = [
            item for item in all_usages
            if item.get("enabled", True) is not False
        ]
        used_by = [
            str(item.get("label") or item.get("id") or item.get("kind") or "引用")
            for item in usages
        ]
        if used_by and not detach_references:
            raise HTTPException(
                status_code=409,
                detail=f"账号仍被 {len(used_by)} 处配置使用：{'、'.join(used_by[:4])}",
            )

        if detach_references and all_usages:
            usage_ids = {
                str(item.get("id") or "") for item in all_usages
            }

            # 一次写入完成 Worker、规划与标题引用清理。删除属于已有配置
            # 的收尾操作，不重新校验其他历史 Worker 的模型测试状态。
            app.state.manager.worker_config.detach_credential_references(
                stable_id)

            # 对话本体、消息和产物继续保留；运行选择回到对应 CLI 的系统登录。
            from muteki.external_agents.factory import engine_for_adapter

            conversation = app.state.platform_stack.conversation
            for selection in conversation.conv.list_runtime_selections():
                if selection.thread_id not in usage_ids:
                    continue
                engine = engine_for_adapter(selection.adapter_id)
                replacement = system_credential_id(engine) if engine else ""
                conversation.manager.save_runtime_selection(
                    selection.thread_id,
                    {"credential_id": replacement},
                    validate_credential=False,
                )

            # 旧 Runtime 引用已退出当前所有权模型，统一清除兼容字段。
            if any(item.get("kind") == "runtime_legacy" for item in all_usages):
                app.state.platform_stack.runtime_store.clear_legacy_identity_fields()

        store = CredentialAccountStore(account_store_root(app.state.manager.state_root))
        deleted = store.delete(account_id)
        if deleted:
            from apps.web.worker_models import CredentialModelCatalogStore

            CredentialModelCatalogStore(
                app.state.manager.state_root
            ).purge_credential(stable_id)
        return {
            "ok": deleted,
            "detached": len(all_usages) if detach_references else 0,
        }

    @app.post("/api/settings/credential-accounts/{account_id}/import-host-codex")
    async def import_host_codex(account_id: str) -> Any:
        # One-click refresh of a codex account from the HOST's ~/.codex/auth.json.
        # `codex login` writes the host file; container workers mount the account
        # COPY, so a fresh login must be re-imported. Only valid on a bare host —
        # inside the web container ~/.codex is the container's, not the operator's.
        from muteki.core.runtime_env import is_web_container

        if is_web_container():
            raise HTTPException(
                status_code=409,
                detail="import-from-host is unavailable when the web control plane "
                       "runs in a container (~/.codex is not the operator's). Paste "
                       "or upload the auth.json instead.",
            )
        store = CredentialAccountStore(account_store_root(app.state.manager.state_root))
        try:
            account = await asyncio.to_thread(store.import_host_codex_auth, account_id)
        except ValueError as exc:
            raise HTTPException(status_code=400, detail=str(exc))
        return {"ok": True, "account": account}

    @app.post("/api/settings/credential-accounts/{account_id}/import-host-login")
    async def import_host_login(account_id: str, request: Request) -> Any:
        """Copy a minimal Claude, Kimi Code, or Grok host login into an account.

        Container workers receive the account projection, so they can authenticate
        without mounting the operator's complete home directory.
        """
        from muteki.core.runtime_env import is_web_container

        if is_web_container():
            raise HTTPException(
                status_code=409,
                detail="import-from-host is unavailable when the web control plane runs in a container",
            )
        body = await _require_dict_body(request)
        engine = str(body.get("engine") or "").strip().lower()
        if engine not in {"claude", "kimi", "grok"}:
            raise HTTPException(status_code=400, detail="engine must be claude, kimi, or grok")
        store = CredentialAccountStore(account_store_root(app.state.manager.state_root))
        try:
            account = await asyncio.to_thread(store.import_host_login, account_id, engine)
        except ValueError as exc:
            raise HTTPException(status_code=400, detail=str(exc))
        return {"ok": True, "account": account}

    @app.post("/api/settings/credential-accounts/{account_id}/test")
    async def probe_credential_account(account_id: str, request: Request) -> Any:
        # Test the REGISTERED account (DESIGN §2.4 補強C-2). local → host probe with
        # the account's env; container → real `docker run --rm` plumbing test.
        # Never falls back to the host default login.
        body = await _require_dict_body(request, allow_empty=True)
        _reject_temporarily_disabled_engine(body)
        from apps.web.worker_models import probe_worker_model

        engine = str(body.get("engine") or "").strip()
        backend = str(body.get("backend") or "local").strip()
        if backend not in ("local", "container"):
            backend = "local"
        store = CredentialAccountStore(
            account_store_root(app.state.manager.state_root))
        account = store.inspect(account_id)
        account_engine = str(
            ((account.details or {}).get("target_engine") if account else "")
            or (account.engine if account else "")
            or engine
        ).strip().lower()
        ensure_engine_supported(account_engine)
        model = str(body.get("model") or "").strip()
        if not model:
            result = {"ok": False, "detail": "请选择模型后执行真实测试",
                      "layer": "model", "model": ""}
        elif account is None or not account.present:
            result = {"ok": False, "detail": "凭据账号不存在或未就绪",
                      "layer": "binding", "model": model}
        else:
            details = dict(account.details or {})
            cfg = app.state.manager.worker_config.get()
            result = await asyncio.to_thread(
                probe_worker_model,
                profile={
                    "id": account_id, "name": account_id, "engine": engine,
                    "credential_account": account_id,
                    "credential_mode": (
                        "api_key" if details.get("api_key_file")
                        else "subscription"),
                    "base_url": str(details.get("base_url_value") or ""),
                    "model": model, "enabled": True,
                },
                model=model,
                sessions_root=app.state.manager.state_root,
                backend=backend,
                runtime={"network": str(
                    cfg.get("worker_network") or "bridge")},
            )
        last_test = store.save_test_status(
            account_credential_id(account_id), result, backend=backend)
        from apps.web.worker_models import CredentialModelCatalogStore

        CredentialModelCatalogStore(
            app.state.manager.state_root
        ).mark_verified(
            credential_id=account_credential_id(account_id),
            engine=account_engine,
            environment=backend,
            runtime_instance=str(body.get("runtime_instance") or "default"),
            model=str(result.get("model") or model),
            ok=bool(result.get("ok")),
        )
        return {**result, "last_test": last_test}

    @app.get("/api/settings/system-login")
    async def get_system_login() -> Any:
        # Host-side login presence per engine (DESIGN §2.3 補強B). Drives the
        # local-mode credentials UI ("默认用系统登录"). Read-only, never raises.
        from muteki.solver.credential_accounts import detect_system_login

        logins = await asyncio.to_thread(
            lambda: {
                e: detect_system_login(e)
                for e in SUPPORTED_ENGINE_IDS
            }
        )
        return {"logins": logins}
