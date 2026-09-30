"""Route helpers moved from create_app. Bodies unchanged; they close over app/mgr."""
from __future__ import annotations

import asyncio
import copy
import hashlib
import json
from types import SimpleNamespace
from typing import Any, Optional

from fastapi import (
    FastAPI,
)

from muteki.platform.contracts.errors import ErrorCategory
from muteki.solver.engine_registry import (
    SUPPORTED_ENGINE_IDS,
    EngineTemporarilyUnsupportedError,
    ensure_engine_supported,
    temporarily_disabled_engine_in_payload,
)
from muteki.solver.credential_accounts import (
    CredentialAccountStore,
    account_credential_id,
    account_store_root,
    canonical_credential_id,
    system_credential_id,
)


def make_helpers(app: FastAPI) -> SimpleNamespace:
    mgr = app.state.manager
    platform_stack = app.state.platform_stack
    def _catalog_runtime_keys_by_engine(transport_kind: str = "cli") -> dict[str, list[str]]:
        """Return CLI Runtime keys in the same preference order as the UI."""
        grouped: dict[str, list[tuple[bool, str]]] = {
            engine: [] for engine in SUPPORTED_ENGINE_IDS
        }
        for row in platform_stack.runtime_service.list_instances(
            include_discovered=True, transport_kind=transport_kind,
        ):
            engine = str(row.get("engine") or "").strip().lower()
            key = str(row.get("key") or "").strip()
            if engine not in grouped or not key:
                continue
            grouped[engine].append((bool(row.get("configured")), key))
        out: dict[str, list[str]] = {}
        for engine in SUPPORTED_ENGINE_IDS:
            ordered = [
                key for _configured, key in sorted(
                    grouped.get(engine) or [],
                    key=lambda item: (not item[0], item[1]),
                )
            ]
            out[engine] = list(dict.fromkeys(
                ordered or [f"cli.{engine}:default"]
            ))
        return out

    def _primary_catalog_runtime_key(engine: str) -> str:
        keys = _catalog_runtime_keys_by_engine().get(
            str(engine or "").strip().lower()) or []
        return keys[0] if keys else f"cli.{engine}:default"

    async def _refresh_credential_catalog_record(
        *,
        credential_id: str,
        engine: str,
        environment: str,
        runtime_instance: str = "default",
        connection: str,
        base_url: str = "",
        secret: str = "",
        account_id: str = "",
        system_credential: bool = False,
        provider: str = "",
        configured_models: list[str] | None = None,
        default_model: str = "",
        persist: bool = True,
        expected_revision: str | None = None,
    ) -> tuple[dict[str, Any], dict[str, Any]]:
        """Run one read-only catalog refresh through the shared single-flight."""
        from apps.web.worker_models import (
            CredentialModelCatalogStore,
            credential_catalog_runtime_key,
            discover_anthropic_compatible_models,
            discover_openai_compatible_models,
            discover_worker_models,
        )

        selected_engine = ensure_engine_supported(engine)
        if not selected_engine:
            raise ValueError("unknown credential engine")
        backend = "container" if environment == "container" else "local"
        selected_runtime = credential_catalog_runtime_key(
            selected_engine, runtime_instance)
        request_identity = hashlib.sha256(json.dumps({
            "connection": connection, "base_url": base_url.rstrip("/"),
            "secret": secret, "provider": provider,
            "runtime": platform_stack.runtime_service.get_instance(*selected_runtime.rsplit(":", 1)) or {},
        }, sort_keys=True, default=str).encode()).hexdigest()
        refresh_key = f"{credential_id}\0{selected_engine}\0{backend}\0{selected_runtime}\0{request_identity}"

        async def _perform() -> dict[str, Any]:
            if connection == "custom_endpoint" or base_url:
                if not base_url:
                    return {
                        "ok": False,
                        "models": [],
                        "source": "endpoint",
                        "error_code": "endpoint_protocol_mismatch",
                        "detail": "自定义端点需要填写 Base URL",
                    }
                endpoint_discovery = (
                    discover_anthropic_compatible_models
                    if selected_engine == "claude"
                    else discover_openai_compatible_models
                )
                return await asyncio.to_thread(
                    endpoint_discovery,
                    base_url=base_url,
                    secret=secret,
                )
            adapter_id, instance_id = selected_runtime.rsplit(":", 1)
            runtime = platform_stack.runtime_service.get_instance(
                adapter_id, instance_id) or {}
            return await asyncio.to_thread(
                discover_worker_models,
                profile={
                    "id": credential_id,
                    "name": credential_id,
                    "engine": selected_engine,
                    "provider": provider,
                    "credential_id": credential_id,
                    "credential_account": account_id,
                    "credential_kind": (
                        "system_inherit" if system_credential else "engine_key"
                    ),
                    "credential_mode": "subscription",
                    "binary_path": str(runtime.get("binary_path") or ""),
                    "enabled": True,
                },
                sessions_root=app.state.manager.state_root,
                backend=backend,
            )

        tasks: dict[str, asyncio.Task] = app.state.credential_model_refresh_tasks
        task = tasks.get(refresh_key)
        if task is None or task.done():
            task = asyncio.create_task(_perform())
            tasks[refresh_key] = task
        try:
            result = await asyncio.shield(task)
        finally:
            if task.done() and tasks.get(refresh_key) is task:
                tasks.pop(refresh_key, None)
        if account_id and expected_revision is not None:
            current_revision = CredentialAccountStore(account_store_root(mgr.state_root)).revision(account_id)
            if current_revision != expected_revision:
                return {"ok": False, "models": [], "error_code": "credential.configuration_changed",
                        "detail": "凭据配置在刷新期间已改变，请重新刷新。"}, {"discovered_models": []}
        if not persist:
            return result, {"discovered_models": result.get("models") or [], "draft": True}
        catalog = CredentialModelCatalogStore(
            app.state.manager.state_root
        ).save_discovery(
            credential_id=credential_id,
            engine=selected_engine,
            environment=backend,
            runtime_instance=selected_runtime,
            result=result,
            configured_models=configured_models or [],
            default_model=default_model,
            preserve_previous_on_failure=not (
                connection == "custom_endpoint" or bool(base_url)
            ),
        )
        return result, catalog

    async def _refresh_stale_credential_catalogs() -> dict[str, Any]:
        """Refresh only missing/expired credential catalogs; never run a model turn."""
        from apps.web.worker_config import backend_for_profile
        from apps.web.worker_models import CredentialModelCatalogStore
        from muteki.core.runtime_env import is_web_container
        from muteki.solver.credential_accounts import (
            detect_system_login,
            invalidate_system_login_cache,
        )

        invalidate_system_login_cache()
        cfg = mgr.worker_config.get()
        environment = backend_for_profile(
            worker_backend=str(cfg.get("worker_backend") or ""),
            in_web_container=is_web_container(),
        )
        account_store = CredentialAccountStore(account_store_root(mgr.state_root))
        catalog_store = CredentialModelCatalogStore(mgr.state_root)
        runtime_keys = _catalog_runtime_keys_by_engine(transport_kind="")
        requests: list[dict[str, Any]] = []
        for public in account_store.list():
            engine = str(
                public.get("worker_engine") or public.get("engine") or ""
            ).strip().lower()
            if engine not in SUPPORTED_ENGINE_IDS or not public.get("present"):
                continue
            account_id = str(public.get("account_id") or "")
            credential_id = account_credential_id(account_id)
            account = account_store.inspect(account_id)
            details = dict(account.details or {}) if account else {}
            for runtime_key in runtime_keys.get(engine) or [
                f"cli.{engine}:default"
            ]:
                if not catalog_store.stale_or_missing(
                    credential_id, engine, environment, runtime_key,
                ):
                    continue
                requests.append({
                    "credential_id": credential_id,
                    "engine": engine,
                    "environment": environment,
                    "runtime_instance": runtime_key,
                    "connection": str(public.get("connection") or "official"),
                    "base_url": str(details.get("base_url_value") or ""),
                    "secret": str(details.get("secret_value") or ""),
                    "account_id": account_id,
                    "provider": str(public.get("provider") or ""),
                    "configured_models": list(public.get("models") or []),
                    "default_model": str(public.get("default_model") or ""),
                })
        if environment == "local":
            for engine in SUPPORTED_ENGINE_IDS:
                credential_id = system_credential_id(engine)
                if detect_system_login(engine) != "present":
                    continue
                for runtime_key in runtime_keys.get(engine) or [
                    f"cli.{engine}:default"
                ]:
                    if not catalog_store.stale_or_missing(
                        credential_id, engine, environment, runtime_key,
                    ):
                        continue
                    requests.append({
                        "credential_id": credential_id,
                        "engine": engine,
                        "environment": environment,
                        "runtime_instance": runtime_key,
                        "connection": "official",
                        "system_credential": True,
                    })

        semaphore = asyncio.Semaphore(3)

        async def _refresh_one(values: dict[str, Any]) -> dict[str, Any]:
            async with semaphore:
                result, _catalog = await _refresh_credential_catalog_record(**values)
                return result

        results = list(await asyncio.gather(
            *(_refresh_one(values) for values in requests),
            return_exceptions=True,
        )) if requests else []
        failed = sum(
            1 for result in results
            if isinstance(result, Exception)
            or (isinstance(result, dict) and not result.get("ok"))
        )
        return {
            "environment": environment,
            "requested": len(requests),
            "failed": failed,
        }
    def _reject_temporarily_disabled_engine(payload: Any) -> None:
        engine = temporarily_disabled_engine_in_payload(payload)
        if engine:
            raise EngineTemporarilyUnsupportedError(engine)
    async def _dispatch_run_command(
        command_type: str,
        run_id: str,
        payload: Optional[dict[str, Any]] = None,
        *,
        command_id: str = "",
        idempotency_key: str = "",
    ) -> Any:
        """旧 Run HTTP 外形到共享 MutekiCommandAPI 的唯一适配入口。"""
        from muteki.platform.contracts.commands import ActorRef, CommandEnvelope

        fields: dict[str, Any] = {
            "command_type": command_type,
            "aggregate_type": "run",
            "aggregate_id": run_id,
            "actor": ActorRef(kind="operator", id="local-user"),
            "payload": dict(payload or {}),
        }
        if command_id:
            fields["command_id"] = command_id
        if idempotency_key:
            fields["idempotency_key"] = idempotency_key
        return await platform_stack.command_api.dispatch(CommandEnvelope(**fields))

    async def _dispatch_platform_command(
        command_type: str,
        aggregate_type: str,
        aggregate_id: str,
        payload: Optional[dict[str, Any]] = None,
    ) -> Any:
        """共享平台设置的兼容 HTTP 入口。"""
        from muteki.platform.contracts.commands import ActorRef, CommandEnvelope

        return await platform_stack.command_api.dispatch(CommandEnvelope(
            command_type=command_type,
            aggregate_type=aggregate_type,
            aggregate_id=aggregate_id,
            actor=ActorRef(kind="operator", id="local-user"),
            payload=dict(payload or {}),
        ))

    def _run_receipt_status(receipt: Any) -> int:
        from muteki.platform.contracts.receipts import ReceiptState

        if receipt.state not in (ReceiptState.FAILED, ReceiptState.CONFLICT):
            return 200
        category = getattr(receipt.error, "category", None)
        return {
            ErrorCategory.VALIDATION: 422,
            ErrorCategory.NOT_FOUND: 404,
            ErrorCategory.PERMISSION: 403,
            ErrorCategory.CONFLICT: 409,
            ErrorCategory.STATE: 409,
        }.get(category, 500)

    async def _control_effect_ok(run_id: str, command_id: str) -> bool:
        """等待已接收控制命令的当前终态；该函数只读取 control journal。"""
        from muteki.control import EffectState

        run = app.state.manager.get(run_id)
        if run is None or not command_id:
            return False
        if run.control_actor is not None:
            await run.control_actor.join()
        effect = run.control_journal.latest_effect(command_id) if run.control_journal else None
        return bool(effect is not None and effect.state in {
            EffectState.EFFECT_OBSERVED,
            EffectState.PARTIAL,
        })
    def llm_settings_payload(config: dict[str, Any]) -> dict[str, Any]:
        """Expose credential presence/source without returning any secret value."""
        from apps.web.llm_credentials import LlmCredentialStore
        from muteki.solver.worker_profiles import resolve_seat_ref

        payload = copy.deepcopy(config)
        store = LlmCredentialStore(app.state.manager.state_root)
        profiles = payload.get("llm_profiles") or {}
        for which in ("planner", "titler"):
            row = profiles.get(which)
            if isinstance(row, dict):
                row["credential_source"] = (
                    "model_endpoint" if row.get("endpoint_id") or row.get("credential_id")
                    else f"legacy_{store.source(which)}"
                )
        # The settings UI edits canonical Seat IDs. Scheduler policy may still be
        # stored with a legacy profile name, so translate only the API payload and
        # leave the scheduler's legacy projection unchanged.
        seats = [row for row in (payload.get("seats") or []) if isinstance(row, dict)]
        aliases = payload.get("seat_alias") if isinstance(payload.get("seat_alias"), dict) else {}
        coordinator = (payload.get("stage_policy") or {}).get("coordinator") or {}
        for key, role in (("review", "review"), ("verifier", "verifier")):
            policy = coordinator.get(key)
            if not isinstance(policy, dict):
                continue
            canonical = resolve_seat_ref(
                policy.get("engine"), seats=seats, alias_table=aliases)
            if canonical is None:
                canonical = next((
                    str(seat.get("id")) for seat in seats
                    if role in (seat.get("roles") or []) and seat.get("enabled", True)
                ), None)
            policy["engine"] = canonical or ""
        # #171 / MNT-09.04: surface requested vs effective Docker network.
        from muteki.solver.container_exec import (
            WorkerNetworkConfigError,
            project_worker_network,
        )
        try:
            proj = project_worker_network(payload.get("worker_network"))
            payload["network_requested"] = proj["requested"]
            payload["effective_network"] = proj["effective"]
            if proj.get("reason"):
                payload["network_reason"] = proj["reason"]
        except WorkerNetworkConfigError as exc:
            payload["network_requested"] = str(
                payload.get("worker_network") or "bridge"
            )
            payload["effective_network"] = payload["network_requested"]
            payload["network_error"] = str(exc)
        return payload

    # cheap TTL cache so a polling deck doesn't re-probe every engine's --version
    # on each request (the probes are subprocess spawns). Codex' real-turn probe can
    # legitimately take minutes during websocket→HTTPS fallback, so keep a long UI
    # TTL and singleflight refreshes: stale data is better than stacking probes.
    _engine_cache: dict[str, Any] = {"ts": 0.0, "data": None}
    _engine_cache_ttl_s = 300.0
    _engine_refresh_lock = asyncio.Lock()

    def _invalidate_engine_cache() -> None:
        # The header polls this cache slowly.  A user who changes the enabled
        # Worker roster must see the saved engine on the very next run instead of
        # the previous roster for up to five minutes.
        _engine_cache["ts"] = 0.0
        _engine_cache["data"] = None
    def _credential_usage_index() -> dict[str, list[dict[str, Any]]]:
        """Collect stable, non-secret reverse references for the credential API."""
        usage: dict[str, list[dict[str, Any]]] = {}

        def add(credential_id: str, row: dict[str, Any]) -> None:
            try:
                stable_id = canonical_credential_id(credential_id)
            except ValueError:
                return
            if not stable_id:
                return
            bucket = usage.setdefault(stable_id, [])
            identity = (str(row.get("kind") or ""), str(row.get("id") or ""))
            if not any(
                (str(item.get("kind") or ""), str(item.get("id") or ""))
                == identity
                for item in bucket
            ):
                bucket.append(row)

        cfg = app.state.manager.worker_config.get()
        identity_credentials = {
            str(item.get("id") or ""): item
            for item in (cfg.get("credentials") or [])
            if isinstance(item, dict)
        }

        def stable_of_identity(row: dict[str, Any], engine: str = "") -> str:
            raw_id = str(row.get("id") or "")
            try:
                if raw_id.startswith(("account:", "system:")):
                    return canonical_credential_id(raw_id, engine=engine)
                if str(row.get("kind") or "") == "system_inherit":
                    return system_credential_id(engine or str(row.get("engine") or ""))
                if row.get("secret_ref"):
                    return account_credential_id(str(row.get("secret_ref")))
            except ValueError:
                return ""
            return ""

        seat_ids: set[str] = set()
        for seat in cfg.get("seats") or []:
            if not isinstance(seat, dict):
                continue
            sid = str(seat.get("id") or "")
            seat_ids.add(sid)
            engine = str(seat.get("engine") or "")
            ref = str(seat.get("credential_id") or "")
            credential = identity_credentials.get(ref) or {}
            stable_id = stable_of_identity(credential, engine)
            if not stable_id:
                try:
                    stable_id = canonical_credential_id(ref, engine=engine)
                except ValueError:
                    stable_id = ""
            roles = [str(item) for item in seat.get("roles") or []]
            add(stable_id, {
                "kind": "worker",
                "id": sid,
                "label": str(seat.get("label") or sid),
                "engine": engine,
                "roles": roles,
                "model": str(seat.get("model") or ""),
                "enabled": bool(seat.get("enabled", True)),
            })

        # Legacy-only profiles remain visible during rolling migration.
        for profile in cfg.get("worker_profiles") or []:
            if not isinstance(profile, dict):
                continue
            pid = str(profile.get("id") or profile.get("name") or "")
            if pid in seat_ids:
                continue
            engine = str(profile.get("engine") or "")
            raw_ref = str(profile.get("credential_id") or "")
            if not raw_ref and profile.get("credential_account"):
                raw_ref = account_credential_id(
                    str(profile.get("credential_account")))
            if not raw_ref and profile.get("credential_kind") == "system_inherit":
                raw_ref = system_credential_id(engine)
            add(raw_ref, {
                "kind": "worker",
                "id": pid,
                "label": str(profile.get("label") or pid),
                "engine": engine,
                "roles": [str(item) for item in profile.get("roles") or []],
                "model": str(profile.get("model") or ""),
                "enabled": bool(profile.get("enabled", True)),
            })

        # Old Runtime credential_ref is a read-only compatibility reference.
        for runtime in app.state.platform_stack.runtime_store.list():
            if not runtime.credential_ref:
                continue
            add(runtime.credential_ref, {
                "kind": "runtime_legacy",
                "id": runtime.key,
                "label": runtime.label or runtime.key,
                "engine": runtime.engine,
                "model": runtime.default_model,
            })

        for selection in app.state.platform_stack.conversation.conv.list_runtime_selections():
            if not selection.credential_id:
                continue
            thread_state = app.state.platform_stack.conversation.conv.get_state(
                selection.thread_id)
            add(selection.credential_id, {
                "kind": "conversation",
                "id": selection.thread_id,
                "label": selection.thread_id,
                "model": selection.model,
                "runtime": selection.runtime_key,
                "enabled": thread_state.status != "archived",
            })
        for which, profile in (
            (app.state.manager.worker_config.get().get("llm_profiles") or {}).items()
        ):
            if which not in {"planner", "titler"} or not isinstance(profile, dict):
                continue
            from apps.web.llm_credentials import (
                account_id_from_model_endpoint_id,
                canonical_model_endpoint_id,
            )
            try:
                endpoint_id = canonical_model_endpoint_id(
                    profile.get("endpoint_id") or profile.get("credential_id") or "")
            except ValueError:
                endpoint_id = ""
            endpoint_account_id = account_id_from_model_endpoint_id(endpoint_id)
            add(account_credential_id(endpoint_account_id) if endpoint_account_id else "", {
                "kind": "llm_profile",
                "id": str(which),
                "label": "任务规划" if which == "planner" else "对话标题",
                "model": str(profile.get("model") or ""),
            })
        return usage
    return SimpleNamespace(
        _catalog_runtime_keys_by_engine=_catalog_runtime_keys_by_engine,
        _primary_catalog_runtime_key=_primary_catalog_runtime_key,
        _refresh_credential_catalog_record=_refresh_credential_catalog_record,
        _refresh_stale_credential_catalogs=_refresh_stale_credential_catalogs,
        _reject_temporarily_disabled_engine=_reject_temporarily_disabled_engine,
        _dispatch_run_command=_dispatch_run_command,
        _dispatch_platform_command=_dispatch_platform_command,
        _run_receipt_status=_run_receipt_status,
        _control_effect_ok=_control_effect_ok,
        llm_settings_payload=llm_settings_payload,
        _engine_cache=_engine_cache,
        _engine_cache_ttl_s=_engine_cache_ttl_s,
        _engine_refresh_lock=_engine_refresh_lock,
        _invalidate_engine_cache=_invalidate_engine_cache,
        _credential_usage_index=_credential_usage_index,
    )
