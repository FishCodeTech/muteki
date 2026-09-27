"""Private API-key storage for the coordinator planner and conversation titler."""

from __future__ import annotations

import os
import tempfile
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Mapping


LLM_PROFILE_NAMES = {"planner", "titler"}
MODEL_ENDPOINT_PREFIX = "endpoint:"


def model_endpoint_id(account_id: Any) -> str:
    """Return the stable public id for an HTTP model endpoint."""
    value = str(account_id or "").strip()
    if not value or value.startswith(("account:", MODEL_ENDPOINT_PREFIX)):
        raise ValueError("model endpoint account id is invalid")
    return f"{MODEL_ENDPOINT_PREFIX}{value}"


def account_id_from_model_endpoint_id(value: Any) -> str:
    text = str(value or "").strip()
    if not text.startswith(MODEL_ENDPOINT_PREFIX):
        return ""
    return text[len(MODEL_ENDPOINT_PREFIX):].strip()


def canonical_model_endpoint_id(value: Any) -> str:
    """Normalize endpoint ids and migrate old account:<id> references."""
    text = str(value or "").strip()
    if not text:
        return ""
    account_id = account_id_from_model_endpoint_id(text)
    if account_id:
        return model_endpoint_id(account_id)
    from muteki.solver.credential_accounts import account_id_from_credential_id

    account_id = account_id_from_credential_id(text)
    if account_id:
        return model_endpoint_id(account_id)
    raise ValueError("model endpoint id must use endpoint:<id>")


@dataclass(frozen=True)
class ResolvedLlmCredential:
    """Ephemeral planner/titler connection resolved at the call boundary."""

    endpoint_id: str
    api_key: str
    base_url: str
    models: tuple[str, ...]
    source: str


class LlmCredentialStore:
    """Store per-profile keys outside the normal worker configuration JSON."""

    def __init__(self, sessions_root: str | Path) -> None:
        self.root = Path(sessions_root) / "_secrets" / "llm_profiles"
        self.root.mkdir(parents=True, exist_ok=True)
        try:
            self.root.chmod(0o700)
        except OSError:
            pass

    @staticmethod
    def _profile(which: str) -> str:
        profile = (which or "").strip().lower()
        if profile not in LLM_PROFILE_NAMES:
            raise ValueError("which must be planner or titler")
        return profile

    def _path(self, which: str) -> Path:
        return self.root / self._profile(which) / "API_KEY"

    def saved_key(self, which: str) -> str:
        path = self._path(which)
        try:
            return path.read_text(encoding="utf-8").strip()
        except OSError:
            return ""

    def resolve(self, which: str) -> str:
        return self.saved_key(which) or os.environ.get("MUTEKI_DEEPSEEK_API_KEY", "").strip()

    def source(self, which: str) -> str:
        if self.saved_key(which):
            return "saved"
        if os.environ.get("MUTEKI_DEEPSEEK_API_KEY", "").strip():
            return "environment"
        return "missing"

    def save(self, which: str, api_key: str) -> None:
        value = str(api_key or "").strip()
        if not value:
            raise ValueError("API Key 不能为空")
        path = self._path(which)
        path.parent.mkdir(parents=True, exist_ok=True)
        try:
            path.parent.chmod(0o700)
        except OSError:
            pass
        fd, tmp_name = tempfile.mkstemp(prefix=".API_KEY.", dir=str(path.parent))
        try:
            with os.fdopen(fd, "w", encoding="utf-8") as handle:
                handle.write(value + "\n")
                handle.flush()
                os.fsync(handle.fileno())
            os.chmod(tmp_name, 0o600)
            os.replace(tmp_name, path)
        finally:
            try:
                os.unlink(tmp_name)
            except FileNotFoundError:
                pass

    def clear(self, which: str) -> None:
        path = self._path(which)
        try:
            path.unlink()
        except FileNotFoundError:
            pass


def resolve_llm_profile_credential(
    which: str,
    profile: Mapping[str, Any],
    *,
    sessions_root: str | Path,
) -> ResolvedLlmCredential:
    """Resolve a planner/titler stable credential reference for one real call.

    New profiles point at ``endpoint:<id>`` and never carry endpoint/key material.
    Profiles without ``endpoint_id`` retain the old per-profile key/base-url
    read path so existing installations keep working until the operator saves a
    global credential selection.
    """
    selected = LlmCredentialStore._profile(which)
    row = dict(profile or {})
    endpoint_id = canonical_model_endpoint_id(
        row.get("endpoint_id") or row.get("credential_id") or "")
    model = str(row.get("model") or "").strip()
    if not endpoint_id:
        legacy = LlmCredentialStore(sessions_root)
        return ResolvedLlmCredential(
            endpoint_id="",
            api_key=legacy.resolve(selected),
            base_url=str(row.get("base_url") or "").strip(),
            models=(model,) if model else (),
            source=f"legacy_{legacy.source(selected)}",
        )

    from apps.web.worker_models import CredentialModelCatalogStore
    from muteki.solver.credential_accounts import (
        CredentialAccountStore,
        account_store_root,
        canonical_credential_id,
    )

    account_id = account_id_from_model_endpoint_id(endpoint_id)
    stable_id = canonical_credential_id(f"account:{account_id}")
    if not account_id:
        raise ValueError(
            f"{selected} 只能使用已保存的 endpoint:<id> 模型端点")
    store = CredentialAccountStore(account_store_root(sessions_root))
    account = store.inspect(account_id)
    if account is None or not account.present:
        raise ValueError(f"{selected} 模型端点不可用：{endpoint_id}")
    details = dict(account.details or {})
    api_key = str(details.get("secret_value") or "").strip()
    if not api_key or not details.get("api_key_file"):
        raise ValueError(
            f"{selected} 模型端点 {endpoint_id} 缺少可用的 API Key")
    public = next((
        item for item in store.list()
        if str(item.get("account_id") or "") == account_id
    ), {})
    base_url = str(details.get("base_url_value") or "").strip()
    if (
        str(public.get("connection") or "") != "custom_endpoint"
        or not base_url
    ):
        raise ValueError(
            f"{selected} 模型端点 {endpoint_id} 缺少可直连的 HTTP Base URL")
    # Planner/Titler must validate against the selected credential's catalog.
    # An engine-wide fallback can contain unrelated models, while omitting the
    # credential catalog rejects models that the same settings UI just loaded
    # from this endpoint.
    catalog_store = CredentialModelCatalogStore(sessions_root)
    credential_catalogs: list[dict[str, Any]] = []
    for environment in ("local", "container"):
        catalog_row = catalog_store.by_credential(environment).get(stable_id)
        if catalog_row:
            credential_catalogs.append(catalog_row)
    models: list[str] = []
    model_sources: list[Any] = [*(public.get("models") or [])]
    default_model = str(public.get("default_model") or "").strip()
    if default_model:
        model_sources.append(default_model)
    for credential_catalog in credential_catalogs:
        model_sources.extend(credential_catalog.get("configured_models") or [])
        model_sources.extend(credential_catalog.get("discovered_models") or [])
        model_sources.extend(credential_catalog.get("verified_models") or [])
        catalog_default = str(
            credential_catalog.get("default_model") or "").strip()
        if catalog_default:
            model_sources.append(catalog_default)
    for item in model_sources:
        value = str(
            item.get("id") if isinstance(item, Mapping) else item
        ).strip()
        if value and value not in models:
            models.append(value)
    migration = row.get("credential_migration")
    pending_legacy_base = ""
    if (
        isinstance(migration, Mapping)
        and migration.get("status") == "pending_explicit_save"
        and str(migration.get("credential_id") or "") == stable_id
        and str(migration.get("legacy_model") or "") == model
    ):
        # Unique-match compatibility projection. It is surfaced as an auditable
        # pending migration and becomes normal account metadata on explicit save.
        if model and model not in models:
            models.append(model)
        pending_legacy_base = str(
            migration.get("legacy_base_url") or "").strip()
    if not model:
        raise ValueError(f"{selected} model 不能为空")
    if model not in models:
        raise ValueError(
            f"{selected} 模型 {model!r} 不属于端点 {endpoint_id} 的模型目录")
    return ResolvedLlmCredential(
        endpoint_id=endpoint_id,
        api_key=api_key,
        base_url=(
            pending_legacy_base
            or base_url
        ),
        models=tuple(models),
        source="global",
    )


def list_model_endpoints(
    sessions_root: str | Path,
    *,
    usage: Mapping[str, list[dict[str, Any]]] | None = None,
) -> list[dict[str, Any]]:
    """Project custom HTTP accounts as engine-independent model endpoints.

    Secret storage remains shared with the credential account store. The public
    endpoint object intentionally omits Worker engine and runtime fields because
    Planner/Titler invoke the HTTP API directly.
    """
    from apps.web.worker_models import CredentialModelCatalogStore
    from muteki.solver.credential_accounts import (
        CredentialAccountStore,
        account_credential_id,
        account_store_root,
    )

    store = CredentialAccountStore(account_store_root(sessions_root))
    catalog_store = CredentialModelCatalogStore(sessions_root)
    usage = usage or {}
    rows: list[dict[str, Any]] = []
    for account in store.list():
        if (
            str(account.get("connection") or "") != "custom_endpoint"
            or str(account.get("credential_format") or "") != "api_key"
        ):
            continue
        account_id = str(account.get("account_id") or "").strip()
        base_url = str(account.get("base_url") or "").strip()
        if not account_id or not base_url:
            continue
        credential_id = account_credential_id(account_id)
        endpoint_id = model_endpoint_id(account_id)
        models: list[str] = []
        model_sources: list[Any] = [*(account.get("models") or [])]
        default_model = str(account.get("default_model") or "").strip()
        if default_model:
            model_sources.append(default_model)
        catalogs: list[dict[str, Any]] = []
        for environment in ("local", "container"):
            catalog = catalog_store.by_credential(environment).get(credential_id)
            if not catalog:
                continue
            catalogs.append(catalog)
            model_sources.extend(catalog.get("configured_models") or [])
            model_sources.extend(catalog.get("discovered_models") or [])
            model_sources.extend(catalog.get("verified_models") or [])
            if catalog.get("default_model"):
                model_sources.append(catalog["default_model"])
        for item in model_sources:
            value = str(
                item.get("id") if isinstance(item, Mapping) else item
            ).strip()
            if value and value not in models:
                models.append(value)
        latest_catalog = max(
            catalogs,
            key=lambda item: float(item.get("refreshed_at") or 0),
            default={},
        )
        rows.append({
            "id": endpoint_id,
            "label": account_id,
            "provider": str(account.get("provider") or "").strip(),
            "base_url": base_url,
            "present": bool(account.get("present")),
            "status": (
                "ready" if account.get("present") else "missing"
            ),
            "models": models,
            "default_model": default_model,
            "catalog": latest_catalog,
            "usage": list(usage.get(endpoint_id) or []),
        })
    return rows
