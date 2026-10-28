"""Read-only, secret-free model choices for built-in chat orchestration."""

from __future__ import annotations

from pathlib import Path
from typing import Any

from apps.web.worker_models import CredentialModelCatalogStore
from muteki.solver.credential_accounts import CredentialAccountStore, account_store_root


def subagent_runtime_catalog(
    runtime_service: Any, sessions_root: str | Path, *, engine: str = "",
) -> list[dict[str, Any]]:
    """Reuse the chat picker's credential/runtime-scoped catalogs, never rankings."""
    accounts = CredentialAccountStore(account_store_root(sessions_root))
    catalogs = CredentialModelCatalogStore(sessions_root)
    rows: list[dict[str, Any]] = []
    for instance in runtime_service.list_instances(transport_kind="structured"):
        provider = str(instance["engine"])
        if engine and provider != engine:
            continue
        runtime_key = str(instance["key"])
        scoped_catalogs = catalogs.by_credential("local", runtime_instance=runtime_key)
        credentials = accounts.credential_catalog(
            engines=(provider,), catalogs=scoped_catalogs, environment="local")
        for credential in credentials:
            if credential["engine"] != provider:
                continue
            catalog = scoped_catalogs.get(credential["id"]) or {}
            discovered = {
                item["id"]: item for item in catalog.get("discovered_models", [])
                if isinstance(item, dict) and item.get("id")
            }
            model_ids = list(dict.fromkeys([
                *discovered, *catalog.get("configured_models", []),
                *credential.get("candidate_models", []),
                *catalog.get("verified_models", []),
            ]))
            models = []
            for model_id in model_ids:
                metadata = discovered.get(model_id, {})
                model: dict[str, Any] = {
                    "id": model_id,
                    "verified": model_id in catalog.get("verified_models", []),
                }
                if isinstance(metadata.get("reasoning"), dict):
                    model["reasoning"] = metadata["reasoning"]
                models.append(model)
            reasons = []
            if not instance.get("enabled"):
                reasons.append("Runtime 未启用或尚未通过探测")
            if not credential.get("present") or credential.get("status") in {"failed", "unavailable", "missing"}:
                reasons.append("凭据不可用，请在设置中检查登录或测试结果")
            if not models:
                reasons.append("没有该凭据和 Runtime 的模型目录，请在设置中刷新")
            rows.append({
                "engine": provider,
                "adapter_id": instance["adapter_id"],
                "instance_id": instance["instance_id"],
                "credential_id": credential["id"],
                "available": not reasons,
                "reason": "；".join(reasons),
                "models": models,
                "default_model": catalog.get("default_model") or credential.get("default_model") or "",
                "access_modes": list(instance.get("access_modes") or []),
                "catalog_status": catalog.get("refresh_status", "missing"),
                "catalog_error_code": catalog.get("error_code") or "",
            })
    return rows
