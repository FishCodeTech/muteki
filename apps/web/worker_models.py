"""Credential-scoped model discovery, reference metadata, and model probes.

Read-only catalog discovery may run in the background or on demand.  Every
result is stored by credential, engine, execution environment, and Runtime
instance; a failed refresh never falls back to another credential's models.
Real model calls remain an explicit operator action.
"""

from __future__ import annotations

import hashlib
import asyncio
import json
import logging
import os
import re
import signal
import shlex
import shutil
import subprocess
import tempfile
import threading
import time
import urllib.error
import urllib.request
import uuid
from concurrent.futures import ThreadPoolExecutor, as_completed
from contextlib import ExitStack, contextmanager
from dataclasses import asdict, dataclass
from pathlib import Path
from typing import Any

from muteki.solver.cli_driver import (
    apply_runtime_argv,
    driver_for,
)
from muteki.solver.credential_accounts import (
    CONTAINER_ACCOUNTS_ROOT,
    CredentialAccountStore,
    account_store_root,
    engine_account_id,
    host_discovery_enabled,
    project_account_root,
    runtime_env_for_engine,
)
from muteki.solver.worker_profiles import base_engine_for_profile, profile_uses_endpoint
from muteki.external_agents.descriptors import ModelParser, find_descriptor


_LOG = logging.getLogger(__name__)

ModelOption = dict[str, Any]

CREDENTIAL_MODEL_CATALOG_TTL_SECONDS = 24 * 60 * 60
REASONING_CATALOG_VERSION = 2

_MANUAL_CATALOG_PATH = Path(__file__).with_name("worker_models.manual.json")


def credential_catalog_runtime_key(
    engine: str, runtime_instance: str = "default",
) -> str:
    """Return one stable Runtime key for catalog persistence.

    Older callers sent only ``default`` (or a bare instance id), while the
    Runtime API publishes ``<adapter_id>:<instance_id>``.  Normalizing at the
    store boundary keeps those forms from creating parallel caches.
    """
    selected_engine = str(engine or "").strip().lower()
    selected_runtime = str(runtime_instance or "default").strip() or "default"
    if ":" in selected_runtime:
        return selected_runtime
    return f"cli.{selected_engine}:{selected_runtime}"


def _read_manual_catalog() -> dict[str, Any]:
    raw = json.loads(_MANUAL_CATALOG_PATH.read_text(encoding="utf-8"))
    models = raw.get("models")
    if not isinstance(models, dict):
        raise ValueError("worker_models.manual.json must contain a models object")
    return raw


_MANUAL_CATALOG = _read_manual_catalog()
WORKER_MODEL_OPTIONS: dict[str, list[ModelOption]] = _MANUAL_CATALOG["models"]


def _manual_options(engine: str, options: list[ModelOption]) -> list[ModelOption]:
    # A reference model may declare its own options; an engine-wide list is
    # never evidence that every model supports the same settings.
    return [dict(option) for option in options]


def _reasoning(
    levels: Any = (), *, default: Any = "", kind: str = "effort",
    source: str = "model_catalog", supported: bool | None = None,
) -> dict[str, Any]:
    values = list(dict.fromkeys(
        str(value).strip() for value in (levels if isinstance(levels, (list, tuple)) else [])
        if re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9_.-]{0,63}", str(value).strip())
        and str(value).strip() != "default"
    ))
    if supported is False:
        values = []
    return {
        "supported": bool(values), "levels": values,
        "default": str(default) if str(default) in values else "",
        "kind": kind, "source": source,
    }

def _service_tiers(value: Any) -> list[dict[str, str]]:
    """Keep provider-declared service tiers (for example Codex ``priority``)."""
    out: list[dict[str, str]] = []
    for tier in value if isinstance(value, list) else []:
        if not isinstance(tier, dict):
            continue
        tier_id = str(tier.get("id") or "").strip()
        if (not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9_.-]{0,63}", tier_id)
                or tier_id == "default" or any(row["id"] == tier_id for row in out)):
            continue
        out.append({
            "id": tier_id,
            "name": str(tier.get("name") or tier_id).strip() or tier_id,
            "description": str(tier.get("description") or "").strip(),
        })
    return out


_CONTAINER_BIN = {
    "claude": "claude",
    "codex": "codex",
    "cursor": "/home/kali/.local/bin/cursor-agent",
    "pi": "pi",
    "omp": "/home/kali/.local/bin/omp",
    "opencode": "opencode",
    "kimi": "kimi",
    "grok": "/home/kali/.grok/bin/grok",
}

_CONTAINER_OFFLINE_BRIDGE = "/opt/muteki/offline_acp_bridge.py"
_CONTAINER_OMP_OFFLINE_CONFIG = "/opt/muteki/omp_offline_config.yml"
# Offline agent definitions baked into the worker image, by the CLI flag that
# selects them in each driver's argv.
_CONTAINER_OFFLINE_AGENT_ARGS: dict[str, tuple[str, str]] = {
    "kimi": ("--agent-file", "/opt/muteki/kimi_offline_agent.md"),
    "grok": ("--agent", "/opt/muteki/grok_offline_agent.md"),
}

_CONTAINER_BASE_ENV = {
    "PATH": "/home/kali/.local/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
    "HOME": "/home/kali",
    "USER": "kali",
    "LOGNAME": "kali",
    "LANG": "C.UTF-8",
    "PYTHONUNBUFFERED": "1",
    "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": "1",
}


@dataclass(frozen=True)
class CredentialModelCatalog:
    """Models discovered for one credential in one execution environment."""

    credential_id: str
    engine: str
    environment: str
    runtime_instance: str
    discovered_models: list[ModelOption]
    configured_models: list[str]
    verified_models: list[str]
    default_model: str
    source: str
    refresh_status: str
    refreshed_at: float | None
    expires_at: float | None
    error_code: str
    last_error: str

    def to_dict(self) -> dict[str, Any]:
        return asdict(self)


class CredentialModelCatalogStore:
    """Credential-scoped model metadata with a 24-hour freshness window.

    There is intentionally no engine-level projection here.  A result can only
    be read back through the same ``credential_id + engine + environment +
    Runtime instance`` key.
    Failed CLI refreshes keep that credential's own previous rows and mark
    them stale. Custom endpoint failures clear discovered rows so only models
    explicitly configured for that credential remain. Neither path borrows
    another account's or the public engine catalog.
    """

    _lock = threading.RLock()

    def __init__(self, sessions_root: str | Path) -> None:
        self.path = Path(sessions_root) / "_credential_model_catalog.json"

    @staticmethod
    def _key(
        credential_id: str, engine: str, environment: str,
        runtime_instance: str = "default",
    ) -> str:
        runtime_key = credential_catalog_runtime_key(engine, runtime_instance)
        return "\u001f".join((
            str(credential_id or "").strip(),
            str(engine or "").strip().lower(),
            str(environment or "local").strip().lower() or "local",
            runtime_key,
        ))

    def _read_unlocked(self) -> dict[str, dict[str, Any]]:
        try:
            raw = json.loads(self.path.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError):
            return {}
        catalogs = raw.get("catalogs") if isinstance(raw, dict) else None
        if not isinstance(catalogs, dict):
            return {}
        normalized: dict[str, dict[str, Any]] = {}
        for value in catalogs.values():
            if not isinstance(value, dict):
                continue
            row = dict(value)
            if row.get("reasoning_catalog_version") != REASONING_CATALOG_VERSION:
                # Older catalogs guessed levels from an engine or a boolean
                # reasoning flag. Keep model identities, discard those guesses.
                row["discovered_models"] = [
                    {k: v for k, v in item.items() if k != "reasoning"}
                    if isinstance(item, dict) else item
                    for item in row.get("discovered_models") or []
                ]
                row["refresh_status"] = "stale"
            credential_id = str(row.get("credential_id") or "").strip()
            engine = str(row.get("engine") or "").strip().lower()
            environment = str(
                row.get("environment") or "local").strip().lower()
            if not credential_id or not engine:
                continue
            runtime_instance = credential_catalog_runtime_key(
                engine, str(row.get("runtime_instance") or "default"))
            row["runtime_instance"] = runtime_instance
            row["error_code"] = str(row.get("error_code") or "").strip()
            key = self._key(
                credential_id, engine, environment, runtime_instance)
            previous = normalized.get(key)
            if previous is not None and float(
                previous.get("refreshed_at") or 0
            ) > float(row.get("refreshed_at") or 0):
                continue
            normalized[key] = row
        return normalized

    def _write_unlocked(self, catalogs: dict[str, dict[str, Any]]) -> None:
        self.path.parent.mkdir(parents=True, exist_ok=True)
        tmp = self.path.with_name(f".{self.path.name}.{os.getpid()}.tmp")
        tmp.write_text(
            json.dumps(
                {"version": 1, "catalogs": catalogs},
                ensure_ascii=False,
                indent=2,
            ) + "\n",
            encoding="utf-8",
        )
        try:
            tmp.chmod(0o600)
        except OSError:
            pass
        tmp.replace(self.path)

    @staticmethod
    def _models(value: Any) -> list[ModelOption]:
        return _dedupe_models([
            item if isinstance(item, dict) else {
                "id": str(item or "").strip(),
                "label": str(item or "").strip(),
            }
            for item in (value if isinstance(value, list) else [])
            if str(item.get("id") if isinstance(item, dict) else item or "").strip()
        ])

    def save_discovery(
        self,
        *,
        credential_id: str,
        engine: str,
        environment: str,
        runtime_instance: str = "default",
        result: dict[str, Any],
        configured_models: list[str] | None = None,
        default_model: str = "",
        preserve_previous_on_failure: bool = True,
    ) -> dict[str, Any]:
        now = time.time()
        key = self._key(credential_id, engine, environment, runtime_instance)
        with self._lock:
            catalogs = self._read_unlocked()
            previous = dict(catalogs.get(key) or {})
            ok = bool(result.get("ok"))
            if ok:
                discovered = self._models(result.get("models"))
            elif preserve_previous_on_failure:
                discovered = self._models(previous.get("discovered_models"))
            else:
                discovered = []
            previous_refresh = previous.get("refreshed_at")
            row = {
                "credential_id": str(credential_id),
                "engine": str(engine).strip().lower(),
                "environment": str(environment or "local").strip().lower(),
                "runtime_instance": credential_catalog_runtime_key(
                    engine, runtime_instance),
                "reasoning_catalog_version": (
                    REASONING_CATALOG_VERSION if ok
                    else previous.get("reasoning_catalog_version", 0)
                ),
                "discovered_models": discovered,
                "configured_models": list(dict.fromkeys(
                    str(item).strip() for item in (configured_models or [])
                    if str(item).strip()
                )),
                "verified_models": list(previous.get("verified_models") or []),
                "default_model": str(default_model or "").strip(),
                "source": str(result.get("source") or previous.get("source") or ""),
                "refresh_status": (
                    "fresh" if ok else "stale" if discovered else "failed"
                ),
                "refreshed_at": now if ok else previous_refresh,
                "expires_at": (
                    now + CREDENTIAL_MODEL_CATALOG_TTL_SECONDS
                    if ok else previous.get("expires_at")
                ),
                "error_code": (
                    "" if ok else str(
                        result.get("error_code") or "catalog_request_failed"
                    ).strip()
                ),
                "last_error": "" if ok else str(result.get("detail") or "目录请求失败"),
                **({"discovery_evidence": result.get("evidence") if ok else previous.get("discovery_evidence"),
                    **({"last_attempt_evidence": result["evidence"]} if not ok and "evidence" in result else {})}
                   if "evidence" in result or "discovery_evidence" in previous else {}),
            }
            catalogs[key] = row
            self._write_unlocked(catalogs)
        return dict(row)

    def mark_verified(
        self,
        *,
        credential_id: str,
        engine: str,
        environment: str,
        runtime_instance: str = "default",
        model: str,
        ok: bool,
        source: str = "manual_test",
    ) -> None:
        selected = str(model or "").strip()
        if not selected:
            return
        key = self._key(credential_id, engine, environment, runtime_instance)
        with self._lock:
            catalogs = self._read_unlocked()
            row = dict(catalogs.get(key) or {
                "credential_id": credential_id,
                "engine": engine,
                "environment": environment,
                "runtime_instance": credential_catalog_runtime_key(
                    engine, runtime_instance),
                "discovered_models": [],
                "configured_models": [],
                "verified_models": [],
                "default_model": "",
                "source": source,
                "refresh_status": "missing",
                "refreshed_at": None,
                "expires_at": None,
                "error_code": "",
                "last_error": "",
            })
            verified = [str(item) for item in row.get("verified_models") or []]
            if ok and selected in verified:
                return
            if ok and selected not in verified:
                verified.append(selected)
            if not ok and selected in verified:
                verified.remove(selected)
            row["verified_models"] = verified
            catalogs[key] = row
            self._write_unlocked(catalogs)

    def record_conversation_success(self, selection: dict[str, str]) -> None:
        """A completed chat proves the selected credential/model can be used."""
        from muteki.external_agents.factory import engine_for_adapter
        from muteki.solver.credential_accounts import canonical_credential_id

        adapter_id = str(selection.get("adapter_id") or "").strip()
        engine = engine_for_adapter(adapter_id)
        credential_id = str(selection.get("credential_id") or "").strip()
        model = str(selection.get("model") or "").strip()
        if not engine or not credential_id or not model or model == "default":
            return
        self.mark_verified(
            credential_id=canonical_credential_id(credential_id),
            engine=engine,
            # Conversation adapters run on the Web host, independent of the
            # separate Worker local/container setting.
            environment="local",
            runtime_instance=f"{adapter_id}:{selection.get('instance_id') or 'default'}",
            model=model,
            ok=True,
            source="conversation",
        )

    def sync_configuration(
        self,
        *,
        credential_id: str,
        engine: str,
        environment: str,
        runtime_instance: str = "default",
        configured_models: list[str],
        default_model: str,
    ) -> None:
        key = self._key(credential_id, engine, environment, runtime_instance)
        with self._lock:
            catalogs = self._read_unlocked()
            row = dict(catalogs.get(key) or {
                "credential_id": credential_id,
                "engine": engine,
                "environment": environment,
                "runtime_instance": credential_catalog_runtime_key(
                    engine, runtime_instance),
                "discovered_models": [],
                "verified_models": [],
                "source": "",
                "refresh_status": "missing",
                "refreshed_at": None,
                "expires_at": None,
                "error_code": "",
                "last_error": "",
            })
            row["configured_models"] = list(dict.fromkeys(
                str(item).strip() for item in configured_models
                if str(item).strip()
            ))
            row["default_model"] = str(default_model or "").strip()
            catalogs[key] = row
            self._write_unlocked(catalogs)

    def get(
        self, credential_id: str, engine: str, environment: str,
        runtime_instance: str = "default",
    ) -> dict[str, Any] | None:
        key = self._key(credential_id, engine, environment, runtime_instance)
        with self._lock:
            row = self._read_unlocked().get(key)
        if not isinstance(row, dict):
            return None
        public = dict(row)
        expires_at = float(public.get("expires_at") or 0)
        if expires_at and expires_at <= time.time() and public.get("refresh_status") == "fresh":
            public["refresh_status"] = "stale"
        return public

    def by_credential(
        self, environment: str, runtime_instance: str | None = None,
    ) -> dict[str, dict[str, Any]]:
        normalized = str(environment or "local").strip().lower()
        with self._lock:
            rows = list(self._read_unlocked().values())
        out: dict[str, dict[str, Any]] = {}
        for row in rows:
            if str(row.get("environment") or "") != normalized:
                continue
            if runtime_instance is not None:
                requested_runtime = str(runtime_instance or "default")
                row_runtime = str(row.get("runtime_instance") or "default")
                if ":" in requested_runtime:
                    runtime_matches = row_runtime == requested_runtime
                else:
                    runtime_matches = (
                        row_runtime.rsplit(":", 1)[-1] == requested_runtime
                    )
                if not runtime_matches:
                    continue
            credential_id = str(row.get("credential_id") or "")
            if not credential_id:
                continue
            current = out.get(credential_id)
            if current is not None and float(current.get("refreshed_at") or 0) >= float(row.get("refreshed_at") or 0):
                continue
            out[credential_id] = dict(row)
            expires_at = float(out[credential_id].get("expires_at") or 0)
            if expires_at and expires_at <= time.time() and out[credential_id].get("refresh_status") == "fresh":
                out[credential_id]["refresh_status"] = "stale"
        return out

    def stale_or_missing(
        self, credential_id: str, engine: str, environment: str,
        runtime_instance: str = "default",
    ) -> bool:
        row = self.get(credential_id, engine, environment, runtime_instance)
        return row is None or row.get("refresh_status") in {"missing", "stale", "failed"}

    def purge_engine(self, engine: str) -> int:
        selected = str(engine or "").strip().lower()
        with self._lock:
            catalogs = self._read_unlocked()
            keys = [
                key for key, row in catalogs.items()
                if str(row.get("engine") or "").strip().lower() == selected
            ]
            for key in keys:
                catalogs.pop(key, None)
            if keys:
                self._write_unlocked(catalogs)
        return len(keys)

    def purge_credential(self, credential_id: str) -> int:
        selected = str(credential_id or "").strip()
        with self._lock:
            catalogs = self._read_unlocked()
            keys = [
                key for key, row in catalogs.items()
                if str(row.get("credential_id") or "").strip() == selected
            ]
            for key in keys:
                catalogs.pop(key, None)
            if keys:
                self._write_unlocked(catalogs)
        return len(keys)

    def clear(self) -> int:
        with self._lock:
            count = len(self._read_unlocked())
            self._write_unlocked({})
        return count


class WorkerModelTestStore:
    """Durable results from explicit Worker model checks.

    The terminal output returned to the browser for the current click is useful
    for diagnosis, but it is deliberately not written to disk.  Persist only a
    full verdict and bind it to the effective Worker configuration so
    a changed model, credential, backend, or network cannot inherit an old green
    badge after the settings page is reloaded.
    """

    _lock = threading.RLock()

    def __init__(self, sessions_root: str | Path) -> None:
        self.sessions_root = Path(sessions_root)
        self.path = self.sessions_root / "_worker_model_test_status.json"

    def _read_unlocked(self) -> dict[str, dict[str, Any]]:
        try:
            raw = json.loads(self.path.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError):
            return {}
        profiles = raw.get("profiles") if isinstance(raw, dict) else None
        if not isinstance(profiles, dict):
            return {}
        return {
            str(profile_id): dict(row)
            for profile_id, row in profiles.items()
            if isinstance(row, dict)
        }

    def _write_unlocked(self, profiles: dict[str, dict[str, Any]]) -> None:
        self.path.parent.mkdir(parents=True, exist_ok=True)
        tmp = self.path.with_name(f".{self.path.name}.{os.getpid()}.tmp")
        tmp.write_text(
            json.dumps(
                {"version": 1, "profiles": profiles},
                ensure_ascii=False,
                indent=2,
            ) + "\n",
            encoding="utf-8",
        )
        try:
            tmp.chmod(0o600)
        except OSError:
            pass
        tmp.replace(self.path)

    def _credential_revision(self, profile: dict[str, Any]) -> float | None:
        account_id = str(profile.get("credential_account") or "").strip()
        if not account_id:
            return None
        account = CredentialAccountStore(
            account_store_root(self.sessions_root)
        ).inspect(account_id)
        return account.updated_at if account is not None else None

    def signature(
        self,
        *,
        profile: dict[str, Any],
        model: str,
        reasoning_effort: str,
        backend: str,
        runtime: dict[str, Any] | None = None,
    ) -> str:
        """Hash only non-secret fields that determine the model check."""
        selected_model = str(model or profile.get("model") or "").strip()
        selected_effort = str(
            reasoning_effort or profile.get("reasoning_effort") or "default"
        ).strip().lower()
        engine = base_engine_for_profile(profile)
        wire_api = str(profile.get("wire_api") or "").strip().lower() or {
            "codex": "responses",
            "opencode": "chat_completions",
        }.get(engine, "")
        identity = {
            "engine": engine,
            "auth": str(profile.get("auth") or "").strip().lower(),
            "credential_mode": str(
                profile.get("credential_mode") or ""
            ).strip().lower(),
            "credential_account": str(
                profile.get("credential_account") or ""
            ).strip(),
            "credential_revision": self._credential_revision(profile),
            "base_url": str(profile.get("base_url") or "").strip(),
            "wire_api": wire_api,
            "model": selected_model,
            "reasoning_effort": selected_effort,
            "backend": "container" if backend == "container" else "local",
            "network": str((runtime or {}).get("network") or "bridge").strip(),
        }
        encoded = json.dumps(
            identity, ensure_ascii=False, sort_keys=True, separators=(",", ":")
        ).encode("utf-8")
        return hashlib.sha256(encoded).hexdigest()

    def save_result(
        self,
        *,
        profile_id: str,
        profile: dict[str, Any],
        model: str,
        reasoning_effort: str,
        backend: str,
        runtime: dict[str, Any] | None,
        result: dict[str, Any],
    ) -> dict[str, Any]:
        stable_id = str(profile_id or profile.get("id") or profile.get("name") or "").strip()
        if not stable_id:
            return {}
        credential_store = CredentialAccountStore(
            account_store_root(self.sessions_root)
        )
        detail = str(result.get("detail") or "")[:320]
        elapsed_ms = max(0, int(result.get("elapsed_ms") or 0))
        row: dict[str, Any] = {
            "profile_id": stable_id,
            "signature": self.signature(
                profile=profile,
                model=model,
                reasoning_effort=reasoning_effort,
                backend=backend,
                runtime=runtime,
            ),
            "ok": bool(result.get("ok")),
            "detail": detail,
            "engine": str(result.get("engine") or base_engine_for_profile(profile)),
            "model": str(result.get("model") or model or ""),
            "backend": "container" if backend == "container" else "local",
            "exit_code": (
                int(result["exit_code"])
                if isinstance(result.get("exit_code"), int)
                else None
            ),
            "elapsed_ms": elapsed_ms,
            "layer": str(result.get("layer") or ""),
            "tested_at": time.time(),
            "logs": [{
                "stream": "success" if result.get("ok") else "error",
                "message": detail,
                "elapsed_ms": elapsed_ms,
            }],
        }
        with self._lock:
            profiles = self._read_unlocked()
            profiles[stable_id] = row
            self._write_unlocked(profiles)
        return dict(row)

    def matching_results(
        self,
        *,
        profiles: list[dict[str, Any]],
        backend: str,
        runtime: dict[str, Any] | None,
    ) -> dict[str, dict[str, Any]]:
        with self._lock:
            stored = self._read_unlocked()
        matched: dict[str, dict[str, Any]] = {}
        for profile in profiles:
            profile_id = str(
                profile.get("id") or profile.get("name") or ""
            ).strip()
            row = stored.get(profile_id)
            if not profile_id or not isinstance(row, dict):
                continue
            expected = self.signature(
                profile=profile,
                model=str(profile.get("model") or ""),
                reasoning_effort=str(
                    profile.get("reasoning_effort") or "default"
                ),
                backend=backend,
                runtime=runtime,
            )
            if str(row.get("signature") or "") != expected:
                continue
            public = dict(row)
            public.pop("signature", None)
            matched[profile_id] = public
        return matched


def _dedupe_models(*groups: list[ModelOption]) -> list[ModelOption]:
    out: list[ModelOption] = []
    seen: set[str] = set()
    for group in groups:
        for item in group:
            mid = str(item.get("id") or "").strip()
            if not mid:
                continue
            if mid in seen:
                existing = next(row for row in out if row["id"] == mid)
                if "input" not in existing and isinstance(item.get("input"), list):
                    normalized_input = _dedupe_models([item])[0].get("input")
                    if normalized_input is not None:
                        existing["input"] = normalized_input
                if (not isinstance(existing.get("reasoning"), dict)
                        and isinstance(item.get("reasoning"), dict)):
                    existing["reasoning"] = _dedupe_models([item])[0]["reasoning"]
                continue
            seen.add(mid)
            normalized: ModelOption = {
                "id": mid,
                "label": str(item.get("label") or mid),
            }
            provider = str(item.get("provider") or "").strip()
            if provider:
                normalized["provider"] = provider
            inputs = item.get("input")
            if (isinstance(inputs, list) and inputs
                    and all(isinstance(value, str) for value in inputs)):
                normalized["input"] = list(dict.fromkeys(inputs))
            reasoning = item.get("reasoning")
            if isinstance(reasoning, dict):
                normalized["reasoning"] = _reasoning(
                    reasoning.get("levels"), default=reasoning.get("default", ""),
                    supported=reasoning.get("supported"),
                    kind=str(reasoning.get("kind") or "effort"),
                    source=str(reasoning.get("source") or "model_catalog"),
                )
            tiers = _service_tiers(item.get("service_tiers"))
            if tiers:
                normalized["service_tiers"] = tiers
                default_tier = str(item.get("default_service_tier") or "")
                if any(tier["id"] == default_tier for tier in tiers):
                    normalized["default_service_tier"] = default_tier
            out.append(normalized)
    return out


def conversation_model_input(sessions_root: str | Path, selection: Any) -> list[str] | None:
    """Only explicit, credential/runtime-scoped model metadata is authoritative."""
    from muteki.external_agents.factory import engine_for_adapter
    engine = engine_for_adapter(selection.adapter_id)
    catalog = CredentialModelCatalogStore(sessions_root).get(
        selection.credential_id, engine, "local",
        f"{selection.adapter_id}:{selection.instance_id}",
    ) or {}
    if catalog.get("refresh_status") != "fresh":
        return None
    model = next((item for item in catalog.get("discovered_models", [])
                  if isinstance(item, dict) and item.get("id") == selection.model), {})
    inputs = model.get("input")
    return (list(inputs) if isinstance(inputs, list) and inputs
            and all(isinstance(value, str) for value in inputs) else None)


def worker_model_options_payload(
    sessions_root: str | Path | None = None,
) -> dict[str, Any]:
    """Return reference metadata only; discovered rows are credential-owned.

    ``sessions_root`` remains accepted for API compatibility.  It is
    deliberately ignored so an old profile discovery file can never be merged
    into every credential of the same engine.
    """
    manual = {
        engine: _dedupe_models(_manual_options(engine, options))
        for engine, options in WORKER_MODEL_OPTIONS.items()
    }
    return {
        "allow_custom": False,
        "manual_updated_at": _MANUAL_CATALOG.get("updated_at"),
        "manual_sources": _MANUAL_CATALOG.get("sources") or {},
        "manual_models": manual,
        "discovered_models": {},
        "models_by_profile": {},
        "discovery": {},
        "models": manual,
    }


def validate_conversation_effort(sessions_root: str | Path, selection: Any) -> None:
    """Use the same credential / runtime catalog as the chat model picker."""
    service_tier = str(getattr(selection, "service_tier", "") or "")
    if not selection.effort and not service_tier:
        return
    from muteki.external_agents.factory import engine_for_adapter

    engine = engine_for_adapter(selection.adapter_id)
    catalog = CredentialModelCatalogStore(sessions_root).get(
        selection.credential_id, engine, "local",
        f"{selection.adapter_id}:{selection.instance_id}",
    ) or {}
    model = next((item for item in catalog.get("discovered_models", [])
                  if isinstance(item, dict) and item.get("id") == selection.model), {})
    reasoning = model.get("reasoning") or {}
    if selection.effort and (
            not reasoning.get("supported") or selection.effort not in reasoning.get("levels", [])):
        raise ValueError(
            f"当前 {engine} 接入的模型 {selection.model} 不支持思考配置 {selection.effort!r}；"
            "请重新选择思考程度或切回默认"
        )
    if service_tier and not any(
            isinstance(tier, dict) and tier.get("id") == service_tier
            for tier in model.get("service_tiers") or []):
        raise ValueError(
            f"当前 {engine} 接入的模型 {selection.model} 不支持速度档位 {service_tier!r}；"
            "请切回标准速度或刷新模型目录"
        )


def _insert_model(argv: list[str], model: str) -> list[str]:
    model = (model or "").strip()
    if not model or "--model" in argv or "-m" in argv:
        return argv
    if "--" in argv:
        idx = argv.index("--")
        return [*argv[:idx], "--model", model, *argv[idx:]]
    if len(argv) <= 1:
        return [*argv, "--model", model]
    return [*argv[:-1], "--model", model, argv[-1]]


def _detail(returncode: int, stdout: str, stderr: str) -> str:
    for raw in (stderr, stdout):
        for line in reversed((raw or "").strip().splitlines()):
            try:
                doc = json.loads(line)
            except (json.JSONDecodeError, TypeError):
                continue
            if not isinstance(doc, dict):
                continue
            error = doc.get("error")
            error_message = (
                str(error.get("message") or "") if isinstance(error, dict) else str(error or "")
            ).strip()
            message = str(doc.get("result") or error_message or "").strip()
            status = doc.get("api_error_status")
            if message:
                prefix = f"HTTP {status}，" if status else ""
                return f"模型测试退出 {returncode}：{prefix}{message[:300]}"
    tail = (stderr or stdout or "").strip().splitlines()
    if tail:
        return f"模型测试退出 {returncode}：{tail[-1][:300]}"
    return f"模型测试退出 {returncode}"


def _safe_output(value: Any, *, limit: int = 12000) -> str:
    """Keep model-test output useful without returning unbounded terminal data."""
    text = str(value or "").replace("\x00", "")
    if len(text) <= limit:
        return text
    return f"{text[:limit]}\n… 输出已截断（原始长度 {len(text)} 字符）"


def _test_log(stream: str, message: str, elapsed_ms: int) -> dict[str, Any]:
    return {
        "stream": stream,
        "message": _safe_output(message, limit=4000),
        "elapsed_ms": max(0, int(elapsed_ms)),
    }


def _claude_actual_models(stdout: Any) -> list[str]:
    """Read the model IDs Claude Code reports in its result envelope."""
    text = str(stdout or "").strip()
    if not text:
        return []
    docs: list[dict[str, Any]] = []
    try:
        parsed = json.loads(text)
        if isinstance(parsed, dict):
            docs.append(parsed)
    except json.JSONDecodeError:
        for line in text.splitlines():
            try:
                parsed = json.loads(line)
            except json.JSONDecodeError:
                continue
            if isinstance(parsed, dict):
                docs.append(parsed)
    found: list[str] = []
    for doc in docs:
        usage = doc.get("modelUsage") or doc.get("model_usage") or {}
        if isinstance(usage, dict):
            for value in usage:
                model_id = str(value or "").strip()
                if model_id and model_id not in found:
                    found.append(model_id)
    return found


def _model_matches(expected: str, actual: list[str]) -> bool:
    wanted = str(expected or "").strip().casefold()
    return bool(wanted) and any(item.casefold() == wanted for item in actual)


def _process_result(
    *,
    ok: bool,
    detail: str,
    engine: str,
    model: str,
    backend: str,
    command: str,
    started: float,
    returncode: int | None = None,
    stdout: Any = "",
    stderr: Any = "",
    layer: str | None = None,
    error_code: str | None = None,
    actual_models: list[str] | None = None,
) -> dict[str, Any]:
    elapsed_ms = int((time.perf_counter() - started) * 1000)
    clean_stdout = _safe_output(stdout)
    clean_stderr = _safe_output(stderr)
    logs = [
        _test_log("system", f"启动 {engine} Worker 模型测试", 0),
        _test_log("command", command, 0),
    ]
    if clean_stdout.strip():
        logs.append(_test_log("stdout", clean_stdout, elapsed_ms))
    if clean_stderr.strip():
        logs.append(_test_log("stderr", clean_stderr, elapsed_ms))
    if actual_models is not None:
        actual_text = "、".join(actual_models) if actual_models else "未返回模型 ID"
        logs.append(_test_log(
            "success" if ok else "error",
            f"配置模型：{model or 'Worker 默认模型'}；实际模型：{actual_text}",
            elapsed_ms,
        ))
    exit_message = (
        f"执行完成，退出码 {returncode}，耗时 {elapsed_ms} ms"
        if returncode is not None
        else f"执行结束，耗时 {elapsed_ms} ms"
    )
    logs.append(_test_log("success" if ok else "error", exit_message, elapsed_ms))
    result: dict[str, Any] = {
        "ok": bool(ok),
        "detail": detail,
        "engine": engine,
        "model": model,
        "backend": backend,
        "command": command,
        "stdout": clean_stdout,
        "stderr": clean_stderr,
        "exit_code": returncode,
        "elapsed_ms": elapsed_ms,
        "logs": logs,
        "actual_models": list(actual_models or []),
    }
    if layer:
        result["layer"] = layer
    if error_code:
        result["error_code"] = error_code
    return result


def _model_turn_failure(
    *, engine: str, returncode: int, stdout: Any, stderr: Any,
) -> tuple[str, str, str]:
    """Classify a completed minimal turn without conflating no reply with auth."""
    output = f"{stdout or ''}\n{stderr or ''}"
    lowered = output.casefold()
    if "request timed out" in lowered or "request timeout" in lowered:
        attempts = output.count('"type":"auto_retry_start"') + 1
        return (
            f"模型请求超时：Pi CLI 的 {attempts} 次请求均未收到模型响应",
            "model",
            "provider_request_timeout",
        )
    descriptor = find_descriptor(engine)
    settled_event = descriptor.cli.turn_settled_event if descriptor is not None else ""
    if settled_event and settled_event in lowered:
        return (
            "模型无回复：Pi CLI 已结束任务，但没有返回助手消息",
            "model",
            "model_no_reply",
        )
    rejected_markers = (
        "unauthorized", "forbidden", "authentication", "invalid api key",
        "invalid_api_key", "not logged in", "login required", "http 401",
        "http 403", "api_error_status\":401", "api_error_status\":403",
    )
    if any(marker in lowered for marker in rejected_markers):
        return (
            _detail(returncode, str(stdout or ""), str(stderr or "")),
            "model",
            "model_rejected",
        )
    if returncode == 0:
        return (
            "模型无回复：CLI 已正常结束，但没有返回助手消息",
            "model",
            "model_no_reply",
        )
    return (
        _detail(returncode, str(stdout or ""), str(stderr or "")),
        "model",
        "model_rejected",
    )


class ProbeCancelled(RuntimeError):
    """A task-level Worker probe was cancelled by its owning run."""


class ProbeProcessOwner:
    """Track every subprocess/container started by one task preflight.

    ``asyncio.to_thread`` cancellation does not stop the underlying thread.  The
    run therefore owns an explicit cancellation token and a set of concrete
    process handles.  Cancellation terminates the process group, removes any
    named one-shot container, and ``wait`` confirms that all registered handles
    have been reaped before the execution generation can finish.
    """

    def __init__(self) -> None:
        self._cancelled = threading.Event()
        self._condition = threading.Condition()
        self._processes: dict[subprocess.Popen, str] = {}

    @property
    def cancelled(self) -> bool:
        return self._cancelled.is_set()

    def register(self, process: subprocess.Popen, container_name: str = "") -> bool:
        with self._condition:
            self._processes[process] = container_name
            cancelled = self._cancelled.is_set()
        if cancelled:
            self._terminate(process)
        return not cancelled

    def unregister(self, process: subprocess.Popen) -> None:
        with self._condition:
            self._processes.pop(process, None)
            self._condition.notify_all()

    @staticmethod
    def _terminate(process: subprocess.Popen) -> None:
        if process.poll() is not None:
            return
        try:
            if os.name == "posix":
                os.killpg(process.pid, signal.SIGTERM)
            else:
                process.terminate()
        except (OSError, ProcessLookupError):
            pass
        try:
            process.wait(timeout=1.0)
            return
        except (subprocess.TimeoutExpired, OSError):
            pass
        try:
            if os.name == "posix":
                os.killpg(process.pid, signal.SIGKILL)
            else:
                process.kill()
        except (OSError, ProcessLookupError):
            pass

    def cancel(self) -> None:
        self._cancelled.set()
        with self._condition:
            owned = list(self._processes.items())
        for process, _container_name in owned:
            self._terminate(process)
        for _process, container_name in owned:
            if not container_name:
                continue
            try:
                subprocess.run(
                    ["docker", "rm", "-f", container_name],
                    capture_output=True,
                    text=True,
                    encoding="utf-8",
                    errors="replace",
                    timeout=10,
                )
            except (FileNotFoundError, subprocess.TimeoutExpired, OSError):
                pass

    def wait(self, timeout: float = 15.0) -> bool:
        deadline = time.monotonic() + max(0.0, float(timeout))
        with self._condition:
            while self._processes:
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    return False
                self._condition.wait(timeout=min(0.2, remaining))
            return True


def _run_owned_process(
    argv: list[str], *, timeout: float, owner: ProbeProcessOwner,
    env: dict[str, str] | None = None, container_name: str = "",
) -> subprocess.CompletedProcess:
    if owner.cancelled:
        raise ProbeCancelled("task preflight cancelled")
    process = subprocess.Popen(
        argv,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        encoding="utf-8",
        errors="replace",
        env=env,
        start_new_session=(os.name == "posix"),
    )
    owner.register(process, container_name)
    deadline = time.monotonic() + max(0.01, float(timeout))
    try:
        while True:
            if owner.cancelled:
                owner._terminate(process)
                stdout, stderr = process.communicate()
                raise ProbeCancelled("task preflight cancelled")
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                owner._terminate(process)
                stdout, stderr = process.communicate()
                raise subprocess.TimeoutExpired(
                    argv, timeout, output=stdout, stderr=stderr)
            try:
                stdout, stderr = process.communicate(timeout=min(0.2, remaining))
                return subprocess.CompletedProcess(
                    argv, process.returncode, stdout, stderr)
            except subprocess.TimeoutExpired:
                continue
    finally:
        owner.unregister(process)


def _docker(
    *args: str, timeout: float = 30.0,
    owner: ProbeProcessOwner | None = None, container_name: str = "",
) -> subprocess.CompletedProcess:
    argv = ["docker", *args]
    if owner is not None:
        return _run_owned_process(
            argv, timeout=timeout, owner=owner,
            container_name=container_name)
    return subprocess.run(
        argv, capture_output=True, text=True, encoding="utf-8",
        errors="replace", timeout=timeout)


def _containerize_argv(engine: str, argv: list[str]) -> list[str]:
    if not argv:
        return argv
    bin_in = _CONTAINER_BIN.get(engine)
    out = list(argv)
    if len(out) >= 2 and os.path.basename(out[1]) == "offline_acp_bridge.py":
        out[0] = "python3"
        out[1] = _CONTAINER_OFFLINE_BRIDGE
        for index, arg in enumerate(out):
            if arg == "--agent-bin" and index + 1 < len(out):
                out[index + 1] = bin_in or os.path.basename(out[index + 1])
            elif arg.startswith("--agent-arg=") and os.path.basename(
                    arg.removeprefix("--agent-arg=")) == "omp_offline_config.yml":
                out[index] = f"--agent-arg={_CONTAINER_OMP_OFFLINE_CONFIG}"
        return out
    binary_index = 0
    if out[0] == "env":
        # ``env NAME=VALUE ... <binary>``: the binary is the first operand.
        binary_index = next(
            (index for index, arg in enumerate(out[1:], start=1) if "=" not in arg),
            len(out),
        )
    if binary_index < len(out):
        out[binary_index] = bin_in or os.path.basename(out[binary_index])
    offline_agent = _CONTAINER_OFFLINE_AGENT_ARGS.get(engine)
    if offline_agent is not None:
        flag, container_path = offline_agent
        for index, arg in enumerate(out[:-1]):
            if arg == flag:
                out[index + 1] = container_path
    return out


def _probe_ok(profile: dict[str, Any], r: subprocess.CompletedProcess) -> bool:
    drv = driver_for(profile)
    # EndpointDriver's build_execute output is still the base engine's envelope.
    # Use the base checker when present so codex keeps its tolerant JSONL success
    # predicate instead of the generic "rc 0 + non-empty stdout" fallback.
    checker = getattr(drv, "base", drv)
    return bool(checker._hello_ok(r))  # noqa: SLF001


def _probe_argv_for_profile(
    profile: dict[str, Any], engine: str, model: str, *,
    runtime_env: dict[str, Any] | None = None,
    container: bool = False,
) -> list[str]:
    drv = driver_for(profile)
    argv = drv._hello_argv()  # noqa: SLF001 - same minimal model turn as health checks.
    if not argv:
        prompt = getattr(drv, "HELLO_PROMPT", "Reply with exactly: OK")
        argv = drv.build_execute(
            prompt, None, web_access=False, kb_access=False, stream=False
        )
        # EndpointDriver injects Codex provider/model flags itself. Claude Code
        # custom endpoints receive model selection through their shared
        # ANTHROPIC_* environment mapping.
        descriptor = find_descriptor(engine)
        cli = descriptor.cli if descriptor is not None else None
        model_from_env = cli is not None and bool(cli.model_env_var) and bool(
            (getattr(drv, "env_extra", lambda: {})() or {}).get(cli.model_env_var)
        )
        driver_sets_model = (
            cli is not None and cli.endpoint_driver_sets_model and profile_uses_endpoint(profile)
        )
        if not driver_sets_model and not model_from_env:
            argv = _insert_model(argv, model)
    argv = apply_runtime_argv(
        argv,
        driver=drv,
        env={
            **(runtime_env or {}),
            "MUTEKI_WORKER_MODEL": model,
            "MUTEKI_WORKER_REASONING_EFFORT": str(
                profile.get("reasoning_effort") or "default"),
        },
    )
    return _containerize_argv(engine, argv) if container else argv


def _worker_container_model_probe(
    *,
    profile: dict[str, Any],
    model: str,
    sessions_root: str | Path,
    engine: str,
    runtime: dict[str, Any] | None = None,
    owner: ProbeProcessOwner | None = None,
) -> dict[str, Any]:
    """Run the selected profile/model inside the actual worker image.

    This is intentionally a one-shot `docker run --rm`, not the long-lived
    per-run supervisor container: the settings button needs a fresh, bounded
    validation that the worker image, projected credentials, network, CLI, and
    selected model can complete one minimal turn.
    """

    from muteki.solver.container_exec import (
        CONTAINER_WORKSPACE,
        WORKER_IMAGE,
        WorkerNetworkConfigError,
        _HOST_DATA_ROOT,
        _mount_source,
        resolve_worker_run_network,
    )

    started = time.perf_counter()
    root = account_store_root(sessions_root)
    account_id = str(profile.get("credential_account") or "").strip() or None
    effective_account_id = account_id or engine_account_id(engine)
    acct = CredentialAccountStore(root).inspect(effective_account_id)
    if acct is None or not acct.present:
        return _process_result(
            ok=False, detail=f"容器模型测试需要已登记账号: {effective_account_id}",
            engine=engine, model=model, backend="container",
            command="准备 Worker 容器", started=started, layer="auth",
        )

    try:
        img = _docker(
            "image", "inspect", WORKER_IMAGE, timeout=20,
            **({"owner": owner} if owner is not None else {}),
        )
    except FileNotFoundError:
        return _process_result(
            ok=False, detail="docker 不可用", engine=engine, model=model,
            backend="container", command="docker image inspect", started=started,
            layer="image",
        )
    except subprocess.TimeoutExpired:
        return _process_result(
            ok=False, detail="docker image inspect 超时", engine=engine, model=model,
            backend="container", command="docker image inspect", started=started,
            layer="image",
        )
    if img.returncode != 0:
        return _process_result(
            ok=False, detail=f"worker 镜像缺失或不可用: {WORKER_IMAGE}",
            engine=engine, model=model, backend="container",
            command=f"docker image inspect {WORKER_IMAGE}", started=started,
            returncode=img.returncode, stdout=img.stdout, stderr=img.stderr,
            layer="image",
        )

    tmp_base = None
    if _HOST_DATA_ROOT:
        tmp_base = os.path.join(
            os.environ.get("MUTEKI_CONTAINER_DATA_ROOT") or _HOST_DATA_ROOT,
            "_tmp",
            "model-tests",
        )
        try:
            os.makedirs(tmp_base, exist_ok=True)
        except OSError:
            tmp_base = None

    with tempfile.TemporaryDirectory(prefix="muteki-model-test-", dir=tmp_base) as td:
        workspace = os.path.join(td, "ws")
        projection = os.path.join(td, "accounts")
        os.makedirs(workspace, exist_ok=True)
        try:
            os.chmod(workspace, 0o777)
        except OSError:
            pass
        try:
            project_account_root(root, projection, account_ids=[effective_account_id])
        except OSError as exc:
            return _process_result(
                ok=False, detail=f"凭据投影失败: {str(exc)[:120]}",
                engine=engine, model=model, backend="container",
                command="投影模型服务连接", started=started, layer="mount",
            )

        agent_state_dir = os.path.join(workspace, f".{engine}-agent-state")
        agent_state_container = f"{CONTAINER_WORKSPACE}/.{engine}-agent-state"
        resolved = runtime_env_for_engine(
            engine,
            account_root=root,
            account_id=effective_account_id,
            container=True,
            agent_state_dir=agent_state_dir,
            agent_state_container_path=agent_state_container,
            model=model,
        )
        if _writes_agent_state(engine):
            from muteki.solver.container_exec import _chown_tree_to_worker
            _chown_tree_to_worker(agent_state_dir)

        drv = driver_for(profile)
        env_extra = getattr(drv, "env_extra", None)
        profile_env = env_extra() if callable(env_extra) else {}
        env = {
            **_CONTAINER_BASE_ENV,
            **profile_env,
            **resolved.env,
            "MUTEKI_WORKER_MODEL": model,
            "MUTEKI_WORKER_REASONING_EFFORT": str(
                profile.get("reasoning_effort") or "default"),
        }
        argv = _probe_argv_for_profile(
            profile, engine, model, runtime_env=env, container=True)
        if not argv:
            return _process_result(
                ok=False, detail="该引擎没有可用的容器内模型探针",
                engine=engine, model=model, backend="container",
                command=f"{engine} <minimal-model-turn>", started=started,
            )
        isolated_config = _endpoint_test_config_var(engine, profile)
        verify_claude_model = bool(isolated_config)
        if isolated_config:
            env[isolated_config] = f"{CONTAINER_WORKSPACE}/.muteki-claude-config"
        prelude = [
            'if [ -n "$CLAUDE_CONFIG_DIR" ]; then mkdir -p "$CLAUDE_CONFIG_DIR"; fi',
            'if [ -n "$MUTEKI_CODEX_HOME_SEED" ] && [ -d "$MUTEKI_CODEX_HOME_SEED" ]; then '
            'export CODEX_HOME="${CODEX_HOME:-$HOME/.codex-muteki-model-test}"; '
            'rm -rf "$CODEX_HOME"; mkdir -p "$CODEX_HOME"; '
            'cp -R "$MUTEKI_CODEX_HOME_SEED"/. "$CODEX_HOME"/; '
            'chmod -R u+rwX "$CODEX_HOME"; fi',
            'if [ -r "$CLAUDE_CODE_OAUTH_TOKEN_FILE" ]; then '
            'export CLAUDE_CODE_OAUTH_TOKEN="$(cat "$CLAUDE_CODE_OAUTH_TOKEN_FILE")"; fi',
            'if [ -r "$ANTHROPIC_AUTH_TOKEN_FILE" ]; then '
            'export ANTHROPIC_AUTH_TOKEN="$(cat "$ANTHROPIC_AUTH_TOKEN_FILE")"; fi',
            'if [ -r "$CURSOR_API_KEY_FILE" ]; then '
            'export CURSOR_API_KEY="$(cat "$CURSOR_API_KEY_FILE")"; fi',
            'if [ -r "$ANTHROPIC_API_KEY_FILE" ]; then '
            'export ANTHROPIC_API_KEY="$(cat "$ANTHROPIC_API_KEY_FILE")"; fi',
            'if [ -r "$OPENAI_API_KEY_FILE" ]; then '
            'export OPENAI_API_KEY="$(cat "$OPENAI_API_KEY_FILE")"; fi',
            'if [ -r "$OPENCODE_API_KEY_FILE" ]; then '
            'export OPENCODE_API_KEY="$(cat "$OPENCODE_API_KEY_FILE")"; fi',
            'if [ -r "$DEEPSEEK_API_KEY_FILE" ]; then '
            'export DEEPSEEK_API_KEY="$(cat "$DEEPSEEK_API_KEY_FILE")"; fi',
            'if [ -r "$KIMI_MODEL_API_KEY_FILE" ]; then '
            'export KIMI_MODEL_API_KEY="$(cat "$KIMI_MODEL_API_KEY_FILE")"; fi',
            'if [ -r "$XAI_API_KEY_FILE" ]; then '
            'export XAI_API_KEY="$(cat "$XAI_API_KEY_FILE")"; fi',
        ]
        timeout_s = max(1, int(getattr(driver_for(profile), "_HELLO_TIMEOUT", 90)))
        script = (
            "; ".join(prelude)
            + f"; exec timeout -s KILL {timeout_s}s {shlex.join(argv)} < /dev/null"
        )

        runtime = runtime or {}
        try:
            network = resolve_worker_run_network(
                str(runtime.get("network") or ""),
                needs_egress=True,
            )
        except Exception as exc:
            from muteki.solver.container_exec import WorkerNetworkConfigError
            if not isinstance(exc, WorkerNetworkConfigError):
                raise
            return _process_result(
                ok=False,
                detail=str(exc),
                engine=engine,
                model=model,
                backend="container",
                command="resolve worker network",
                started=started,
                layer="network",
                error_code="network_config_rejected",
            )
        container_name = (
            f"muteki-preflight-{os.getpid()}-{uuid.uuid4().hex[:12]}"
            if owner is not None else ""
        )
        run_cmd = [
            "run", "--rm", "--init",
            *(["--name", container_name] if container_name else []),
            "--network", network,
            "--user", "kali",
            "--workdir", CONTAINER_WORKSPACE,
            "--entrypoint", "bash",
            "--mount", f"type=bind,source={_mount_source(workspace)},target={CONTAINER_WORKSPACE}",
            "--mount", f"type=bind,source={_mount_source(projection)},target={CONTAINER_ACCOUNTS_ROOT}",
        ]
        if network != "host":
            run_cmd += ["--add-host", "host.docker.internal:host-gateway"]
        memory = str(runtime.get("memory") or "").strip()
        cpus = str(runtime.get("cpus") or "").strip()
        pids_limit = int(runtime.get("pids_limit") or 0)
        if memory:
            run_cmd += ["--memory", memory]
        if cpus:
            run_cmd += ["--cpus", cpus]
        if pids_limit > 0:
            run_cmd += ["--pids-limit", str(pids_limit)]
        for k, v in env.items():
            run_cmd += ["-e", f"{k}={v}"]
        run_cmd += [WORKER_IMAGE, "-lc", script]

        try:
            run = _docker(
                *run_cmd, timeout=timeout_s + 30,
                **({"owner": owner, "container_name": container_name}
                   if owner is not None else {}),
            )
        except FileNotFoundError:
            return _process_result(
                ok=False, detail="docker 不可用", engine=engine, model=model,
                backend="container", command="docker run … " + shlex.join(argv),
                started=started, layer="image", error_code="cli_missing",
            )
        except subprocess.TimeoutExpired:
            return _process_result(
                ok=False, detail=f"worker 容器模型测试超时（>{timeout_s}s）",
                engine=engine, model=model, backend="container",
                command="docker run … " + shlex.join(argv), started=started,
                layer="model", error_code="timeout",
            )

    reply_ok = _probe_ok(profile, run)
    actual_models = _claude_actual_models(run.stdout) if verify_claude_model else None
    model_ok = (
        (not actual_models or _model_matches(model, actual_models))
        if verify_claude_model else True
    )
    ok = reply_ok and model_ok
    failure_layer = ""
    failure_code = ""
    if reply_ok and not model_ok:
        actual_text = "、".join(actual_models or []) or "未返回模型 ID"
        detail = f"模型不匹配：配置为 {model}，实际调用 {actual_text}"
        failure_layer = "model"
        failure_code = "model_rejected"
    elif not reply_ok:
        detail, failure_layer, failure_code = _model_turn_failure(
            engine=engine,
            returncode=run.returncode,
            stdout=run.stdout,
            stderr=run.stderr,
        )
    else:
        detail = (
            "worker 容器内模型可用，实际模型与配置一致"
            if ok and verify_claude_model
            else "worker 容器内模型可用（已完成真实对话）"
            if ok
            else "worker 容器模型测试失败"
        )
    return _process_result(
        ok=ok,
        detail=detail,
        engine=engine, model=model, backend="container",
        command="docker run … " + shlex.join(argv), started=started,
        returncode=run.returncode, stdout=run.stdout, stderr=run.stderr,
        layer=None if ok else failure_layer or "model",
        error_code=None if ok else failure_code or "model_rejected",
        actual_models=actual_models,
    )


def probe_worker_model(
    *,
    profile: dict[str, Any],
    model: str,
    reasoning_effort: str = "default",
    sessions_root: str | Path,
    backend: str = "local",
    runtime: dict[str, Any] | None = None,
    owner: ProbeProcessOwner | None = None,
) -> dict[str, Any]:
    """Run one minimal turn with the selected model for this worker profile."""

    started = time.perf_counter()
    profile = dict(profile or {})
    model = str(model or profile.get("model") or "").strip()
    if model:
        profile["model"] = model
    profile["reasoning_effort"] = str(reasoning_effort or "default").strip().lower()
    engine = base_engine_for_profile(profile)
    if profile_uses_endpoint(profile) and not model:
        return _process_result(
            ok=False,
            detail="自定义 API 需要明确的模型 ID，已停止默认模型回退",
            engine=engine,
            model="",
            backend=backend if backend in ("local", "container") else "local",
            command=f"{engine} <model-required>",
            started=started,
            layer="model",
        )

    # In compose deploys the web container does not ship engine CLIs; run the
    # selected profile/model in the worker image instead of shelling the host/web
    # filesystem. This spends one minimal model turn by design: the operator
    # explicitly clicked "test model".
    if backend == "container":
        return _worker_container_model_probe(
            profile=profile,
            model=model,
            sessions_root=sessions_root,
            engine=engine,
            runtime=runtime,
            owner=owner,
        )

    account_id = str(profile.get("credential_account") or "").strip()
    # In local mode an empty credential_account means "use the host CLI login"
    # (e.g. ~/.codex), matching the live swarm worker path. Passing None here
    # would silently fall back to the default <engine>-main account and can pick
    # up a stale registered Codex home.
    resolved_account_id = account_id if account_id else ("" if backend == "local" else None)
    root = account_store_root(sessions_root)
    drv = driver_for(profile)
    env_extra = getattr(drv, "env_extra", None)
    profile_env = env_extra() if callable(env_extra) else {}

    with ExitStack() as stack:
        agent_state_dir = None
        if _writes_agent_state(engine):
            agent_state_dir = stack.enter_context(
                tempfile.TemporaryDirectory(
                    prefix=f"muteki-{engine}-model-test-"))
        credential_env = runtime_env_for_engine(
            engine,
            account_root=root,
            account_id=resolved_account_id,
            container=False,
            agent_state_dir=agent_state_dir,
            model=model,
        ).env
        env = {
            **profile_env,
            **credential_env,
            "MUTEKI_WORKER_MODEL": model,
            "MUTEKI_WORKER_REASONING_EFFORT": str(reasoning_effort or "default"),
        }
        env = stack.enter_context(_temporary_model_probe_env(engine, sessions_root, resolved_account_id, env))
        isolated_config = _endpoint_test_config_var(engine, profile)
        verify_claude_model = bool(isolated_config)
        if isolated_config:
            env[isolated_config] = stack.enter_context(
                tempfile.TemporaryDirectory(prefix="muteki-claude-model-test-")
            )
        # A model test must exercise the same CLI envelope as a real worker.  The old
        # custom-endpoint branch called EndpointDriver.health_detail(), which only
        # issued a curl for Claude endpoints: the endpoint/key could be green while
        # Claude Code itself rejected the selected model or failed to launch.  Build
        # the real profile argv for every local probe (including endpoint profiles),
        # matching the already-correct container model-test path.
        argv = _probe_argv_for_profile(
            profile, engine, model, runtime_env=env)
        if not argv:
            return _process_result(
                ok=False, detail="该引擎没有可用的最小模型探针",
                engine=engine, model=model,
                backend=backend if backend in ("local", "container") else "local",
                command=f"{engine} <minimal-model-turn>", started=started,
            )
        command = shlex.join(argv)
        try:
            process_env = {**os.environ, **env}
            if owner is not None:
                r = _run_owned_process(
                    argv,
                    timeout=getattr(drv, "_HELLO_TIMEOUT", 90),
                    owner=owner,
                    env=process_env,
                )
            else:
                r = subprocess.run(
                    argv,
                    stdin=subprocess.DEVNULL,
                    capture_output=True,
                    text=True,
                    encoding="utf-8", errors="replace",
                    timeout=getattr(drv, "_HELLO_TIMEOUT", 90),
                    env=process_env,
                )
        except FileNotFoundError:
            return _process_result(
                ok=False, detail="CLI 不存在", engine=engine, model=model,
                backend=backend if backend in ("local", "container") else "local",
                command=command, started=started, layer="cli", error_code="cli_missing",
            )
        except subprocess.TimeoutExpired as exc:
            return _process_result(
                ok=False, detail="模型测试超时", engine=engine, model=model,
                backend=backend if backend in ("local", "container") else "local",
                command=command, started=started,
                stdout=getattr(exc, "stdout", ""), stderr=getattr(exc, "stderr", ""),
                layer="model", error_code="timeout",
            )
        except ProbeCancelled:
            raise
        except Exception as exc:  # noqa: BLE001
            return _process_result(
                ok=False, detail=str(exc)[:600],
                engine=engine, model=model,
                backend=backend if backend in ("local", "container") else "local",
                command=command, started=started,
            )

        reply_ok = drv._hello_ok(r)  # noqa: SLF001 - same model round-trip predicate.
        actual_models = _claude_actual_models(r.stdout) if verify_claude_model else None
        model_ok = (
            (not actual_models or _model_matches(model, actual_models))
            if verify_claude_model else True
        )
        ok = reply_ok and model_ok
        failure_layer = ""
        failure_code = ""
        if reply_ok and not model_ok:
            actual_text = "、".join(actual_models or []) or "未返回模型 ID"
            detail = f"模型不匹配：配置为 {model}，实际调用 {actual_text}"
            failure_layer = "model"
            failure_code = "model_rejected"
        elif not reply_ok:
            detail, failure_layer, failure_code = _model_turn_failure(
                engine=engine,
                returncode=r.returncode,
                stdout=r.stdout,
                stderr=r.stderr,
            )
        else:
            detail = (
                "模型可用，实际模型与配置一致"
                if ok and verify_claude_model
                else "模型可用，已完成真实对话"
                if ok
                else "模型测试失败"
            )
        return _process_result(
            ok=bool(ok),
            detail=detail,
            engine=engine, model=model,
            backend=backend if backend in ("local", "container") else "local",
            command=command, started=started, returncode=r.returncode,
            stdout=r.stdout, stderr=r.stderr,
            layer=None if ok else failure_layer or "model",
            error_code=None if ok else failure_code or "model_rejected",
            actual_models=actual_models,
        )


def _worker_container_model_batch_probe(
    *,
    items: list[dict[str, Any]],
    sessions_root: str | Path,
    runtime: dict[str, Any] | None = None,
    owner: ProbeProcessOwner | None = None,
) -> dict[str, Any]:
    """Run all requested model turns in one disposable worker container."""

    from muteki.solver.container_exec import (
        CONTAINER_WORKSPACE,
        WORKER_IMAGE,
        WorkerNetworkConfigError,
        _HOST_DATA_ROOT,
        _mount_source,
        resolve_worker_run_network,
    )

    root = account_store_root(sessions_root)
    store = CredentialAccountStore(root)
    results: list[dict[str, Any] | None] = [None] * len(items)
    runnable: list[dict[str, Any]] = []

    for index, item in enumerate(items):
        profile = dict(item.get("profile") or {})
        model = str(item.get("model") or profile.get("model") or "").strip()
        if model:
            profile["model"] = model
        profile["reasoning_effort"] = str(
            item.get("reasoning_effort")
            or profile.get("reasoning_effort")
            or "default"
        ).strip().lower()
        engine = base_engine_for_profile(profile)
        profile_id = str(
            item.get("profile_id")
            or profile.get("id")
            or profile.get("name")
            or f"profile-{index}"
        )
        started = time.perf_counter()
        if profile_uses_endpoint(profile) and not model:
            result = _process_result(
                ok=False,
                detail="自定义 API 需要明确的模型 ID，已停止默认模型回退",
                engine=engine,
                model="",
                backend="container",
                command=f"{engine} <model-required>",
                started=started,
                layer="model",
            )
            result["profile_id"] = profile_id
            results[index] = result
            continue

        account_id = str(profile.get("credential_account") or "").strip() or None
        effective_account_id = account_id or engine_account_id(engine)
        account = store.inspect(effective_account_id)
        if account is None or not account.present:
            result = _process_result(
                ok=False,
                detail=f"容器模型测试需要已登记账号: {effective_account_id}",
                engine=engine,
                model=model,
                backend="container",
                command="准备 Worker 批量检查容器",
                started=started,
                layer="auth",
            )
            result["profile_id"] = profile_id
            results[index] = result
            continue
        runnable.append({
            "index": index,
            "profile_id": profile_id,
            "profile": profile,
            "model": model,
            "engine": engine,
            "account_id": effective_account_id,
        })

    if not runnable:
        return {
            "backend": "container",
            "container_count": 0,
            "results": [result for result in results if result is not None],
        }

    image_started = time.perf_counter()
    try:
        image = _docker(
            "image", "inspect", WORKER_IMAGE, timeout=20,
            **({"owner": owner} if owner is not None else {}),
        )
    except FileNotFoundError:
        image = None
        image_detail = "docker 不可用"
    except subprocess.TimeoutExpired:
        image = None
        image_detail = "docker image inspect 超时"
    else:
        image_detail = (
            "" if image.returncode == 0
            else f"worker 镜像缺失或不可用: {WORKER_IMAGE}"
        )
    if image is None or image.returncode != 0:
        for task in runnable:
            result = _process_result(
                ok=False,
                detail=image_detail,
                engine=task["engine"],
                model=task["model"],
                backend="container",
                command=f"docker image inspect {WORKER_IMAGE}",
                started=image_started,
                returncode=image.returncode if image is not None else None,
                stdout=image.stdout if image is not None else "",
                stderr=image.stderr if image is not None else "",
                layer="image",
            )
            result["profile_id"] = task["profile_id"]
            results[task["index"]] = result
        return {
            "backend": "container",
            "container_count": 0,
            "results": [result for result in results if result is not None],
        }

    tmp_base = None
    if _HOST_DATA_ROOT:
        tmp_base = os.path.join(
            os.environ.get("MUTEKI_CONTAINER_DATA_ROOT") or _HOST_DATA_ROOT,
            "_tmp",
            "model-batch-tests",
        )
        try:
            os.makedirs(tmp_base, exist_ok=True)
        except OSError:
            tmp_base = None

    runtime = runtime or {}
    try:
        network = resolve_worker_run_network(
            str(runtime.get("network") or ""),
            needs_egress=True,
        )
    except Exception as exc:
        from muteki.solver.container_exec import WorkerNetworkConfigError
        if not isinstance(exc, WorkerNetworkConfigError):
            raise
        for task in runnable:
            result = _process_result(
                ok=False,
                detail=str(exc),
                engine=task["engine"],
                model=task["model"],
                backend="container",
                command="resolve worker network",
                started=time.perf_counter(),
                layer="network",
                error_code="network_config_rejected",
            )
            result["profile_id"] = task["profile_id"]
            results[task["index"]] = result
        return {
            "backend": "container",
            "container_count": 0,
            "results": [result for result in results if result is not None],
        }
    container_name = f"muteki-model-batch-{os.getpid()}-{uuid.uuid4().hex[:12]}"
    container_started = False

    with tempfile.TemporaryDirectory(
        prefix="muteki-model-batch-test-", dir=tmp_base
    ) as td:
        workspace = os.path.join(td, "ws")
        projection = os.path.join(td, "accounts")
        os.makedirs(workspace, exist_ok=True)
        try:
            os.chmod(workspace, 0o777)
            project_account_root(root, projection, account_ids=[task["account_id"] for task in runnable])
        except OSError as exc:
            for task in runnable:
                result = _process_result(
                    ok=False,
                    detail=f"凭据投影失败: {str(exc)[:120]}",
                    engine=task["engine"],
                    model=task["model"],
                    backend="container",
                    command="投影模型服务连接",
                    started=time.perf_counter(),
                    layer="mount",
                )
                result["profile_id"] = task["profile_id"]
                results[task["index"]] = result
            return {
                "backend": "container",
                "container_count": 0,
                "results": [result for result in results if result is not None],
            }

        prepared: list[dict[str, Any]] = []
        for task in runnable:
            engine = task["engine"]
            profile = task["profile"]
            model = task["model"]
            task_name = f"{task['index']:03d}-{re.sub(r'[^a-zA-Z0-9_.-]+', '-', task['profile_id'])[:80]}"
            task_host = os.path.join(workspace, "batch", task_name)
            task_container = f"{CONTAINER_WORKSPACE}/batch/{task_name}"
            home_host = os.path.join(task_host, "home")
            os.makedirs(home_host, exist_ok=True)
            for directory in (os.path.dirname(task_host), task_host, home_host):
                try:
                    os.chmod(directory, 0o777)
                except OSError:
                    pass

            state_host = None
            state_container = None
            if _writes_agent_state(engine):
                state_host = os.path.join(task_host, f".{engine}-agent-state")
                state_container = f"{task_container}/.{engine}-agent-state"
            resolved = runtime_env_for_engine(
                engine,
                account_root=root,
                account_id=task["account_id"],
                container=True,
                agent_state_dir=state_host,
                agent_state_container_path=state_container,
                model=model,
            )

            drv = driver_for(profile)
            env_extra = getattr(drv, "env_extra", None)
            profile_env = env_extra() if callable(env_extra) else {}
            env = {
                **_CONTAINER_BASE_ENV,
                **profile_env,
                **resolved.env,
                "HOME": f"{task_container}/home",
                "MUTEKI_WORKER_MODEL": model,
                "MUTEKI_WORKER_REASONING_EFFORT": str(
                    profile.get("reasoning_effort") or "default"
                ),
            }
            state_seeds = (
                ("CODEX_HOME", "MUTEKI_CODEX_HOME_SEED", ".codex-muteki-model-test"),
                ("KIMI_CODE_HOME", "MUTEKI_KIMI_CODE_HOME_SEED", ".kimi-muteki-model-test"),
                ("GROK_HOME", "MUTEKI_GROK_HOME_SEED", ".grok-muteki-model-test"),
            )
            for state_var, seed_var, dirname in state_seeds:
                seed = str(env.get(state_var) or "").strip()
                if not seed:
                    continue
                env[seed_var] = seed
                env[state_var] = f"{task_container}/{dirname}"
            argv = _probe_argv_for_profile(
                profile, engine, model, runtime_env=env, container=True
            )
            if not argv:
                result = _process_result(
                    ok=False,
                    detail="该引擎没有可用的容器内模型探针",
                    engine=engine,
                    model=model,
                    backend="container",
                    command=f"{engine} <minimal-model-turn>",
                    started=time.perf_counter(),
                )
                result["profile_id"] = task["profile_id"]
                results[task["index"]] = result
                continue

            isolated_config = _endpoint_test_config_var(engine, profile)
            verify_claude_model = bool(isolated_config)
            if isolated_config:
                env[isolated_config] = f"{task_container}/.muteki-claude-config"
            prelude = [
                'if [ -n "$CLAUDE_CONFIG_DIR" ]; then mkdir -p "$CLAUDE_CONFIG_DIR"; fi',
                'if [ -n "$MUTEKI_CODEX_HOME_SEED" ] && [ -d "$MUTEKI_CODEX_HOME_SEED" ]; then '
                'export CODEX_HOME="${CODEX_HOME:-$HOME/.codex-muteki-model-test}"; '
                'rm -rf "$CODEX_HOME"; mkdir -p "$CODEX_HOME"; '
                'cp -R "$MUTEKI_CODEX_HOME_SEED"/. "$CODEX_HOME"/; '
                'chmod -R u+rwX "$CODEX_HOME"; fi',
                'if [ -n "$MUTEKI_KIMI_CODE_HOME_SEED" ] && [ -d "$MUTEKI_KIMI_CODE_HOME_SEED" ]; then '
                'rm -rf "$KIMI_CODE_HOME"; mkdir -p "$KIMI_CODE_HOME"; '
                'cp -R "$MUTEKI_KIMI_CODE_HOME_SEED"/. "$KIMI_CODE_HOME"/; '
                'chmod -R u+rwX "$KIMI_CODE_HOME"; fi',
                'if [ -n "$MUTEKI_GROK_HOME_SEED" ] && [ -d "$MUTEKI_GROK_HOME_SEED" ]; then '
                'rm -rf "$GROK_HOME"; mkdir -p "$GROK_HOME"; '
                'cp -R "$MUTEKI_GROK_HOME_SEED"/. "$GROK_HOME"/; '
                'chmod -R u+rwX "$GROK_HOME"; fi',
                'if [ -r "$CLAUDE_CODE_OAUTH_TOKEN_FILE" ]; then '
                'export CLAUDE_CODE_OAUTH_TOKEN="$(cat "$CLAUDE_CODE_OAUTH_TOKEN_FILE")"; fi',
                'if [ -r "$ANTHROPIC_AUTH_TOKEN_FILE" ]; then '
                'export ANTHROPIC_AUTH_TOKEN="$(cat "$ANTHROPIC_AUTH_TOKEN_FILE")"; fi',
                'if [ -r "$CURSOR_API_KEY_FILE" ]; then '
                'export CURSOR_API_KEY="$(cat "$CURSOR_API_KEY_FILE")"; fi',
                'if [ -r "$ANTHROPIC_API_KEY_FILE" ]; then '
                'export ANTHROPIC_API_KEY="$(cat "$ANTHROPIC_API_KEY_FILE")"; fi',
                'if [ -r "$OPENAI_API_KEY_FILE" ]; then '
                'export OPENAI_API_KEY="$(cat "$OPENAI_API_KEY_FILE")"; fi',
                'if [ -r "$OPENCODE_API_KEY_FILE" ]; then '
                'export OPENCODE_API_KEY="$(cat "$OPENCODE_API_KEY_FILE")"; fi',
                'if [ -r "$DEEPSEEK_API_KEY_FILE" ]; then '
                'export DEEPSEEK_API_KEY="$(cat "$DEEPSEEK_API_KEY_FILE")"; fi',
                'if [ -r "$KIMI_MODEL_API_KEY_FILE" ]; then '
                'export KIMI_MODEL_API_KEY="$(cat "$KIMI_MODEL_API_KEY_FILE")"; fi',
                'if [ -r "$XAI_API_KEY_FILE" ]; then '
                'export XAI_API_KEY="$(cat "$XAI_API_KEY_FILE")"; fi',
            ]
            timeout_s = max(1, int(getattr(drv, "_HELLO_TIMEOUT", 90)))
            prepared.append({
                **task,
                "argv": argv,
                "env": {str(key): str(value) for key, value in env.items()},
                "script": "; ".join(prelude)
                + f"; exec timeout -s KILL {timeout_s}s {shlex.join(argv)} < /dev/null",
                "task_container": task_container,
                "timeout": timeout_s,
                "verify_claude_model": verify_claude_model,
            })

        if not prepared:
            return {
                "backend": "container",
                "container_count": 0,
                "results": [result for result in results if result is not None],
            }

        run_cmd = [
            "run", "-d", "--rm", "--init",
            "--name", container_name,
            "--network", network,
            "--user", "kali",
            "--workdir", CONTAINER_WORKSPACE,
            "--entrypoint", "sleep",
            "--mount",
            f"type=bind,source={_mount_source(workspace)},target={CONTAINER_WORKSPACE}",
            "--mount",
            f"type=bind,source={_mount_source(projection)},target={CONTAINER_ACCOUNTS_ROOT}",
        ]
        if network != "host":
            run_cmd += ["--add-host", "host.docker.internal:host-gateway"]
        memory = str(runtime.get("memory") or "").strip()
        cpus = str(runtime.get("cpus") or "").strip()
        pids_limit = int(runtime.get("pids_limit") or 0)
        if memory:
            run_cmd += ["--memory", memory]
        if cpus:
            run_cmd += ["--cpus", cpus]
        if pids_limit > 0:
            run_cmd += ["--pids-limit", str(pids_limit)]
        run_cmd += [WORKER_IMAGE, "infinity"]

        try:
            started = time.perf_counter()
            container = _docker(
                *run_cmd,
                timeout=30,
                **({"owner": owner, "container_name": container_name}
                   if owner is not None else {}),
            )
            if container.returncode != 0:
                for task in prepared:
                    result = _process_result(
                        ok=False,
                        detail="Worker 批量检查容器启动失败: "
                        + _detail(container.returncode, container.stdout, container.stderr),
                        engine=task["engine"],
                        model=task["model"],
                        backend="container",
                        command="docker run --rm <worker-model-batch>",
                        started=started,
                        returncode=container.returncode,
                        stdout=container.stdout,
                        stderr=container.stderr,
                        layer="cli",
                    )
                    result["profile_id"] = task["profile_id"]
                    results[task["index"]] = result
            else:
                container_started = True

                ownership = _docker(
                    "exec",
                    "--user", "root",
                    container_name,
                    "chown", "-R", "kali:kali", f"{CONTAINER_WORKSPACE}/batch",
                    timeout=20,
                    **({"owner": owner, "container_name": container_name}
                       if owner is not None else {}),
                )
                if ownership.returncode != 0:
                    for task in prepared:
                        result = _process_result(
                            ok=False,
                            detail="批量检查容器无法准备独立 Worker 目录: "
                            + _detail(
                                ownership.returncode,
                                ownership.stdout,
                                ownership.stderr,
                            ),
                            engine=task["engine"],
                            model=task["model"],
                            backend="container",
                            command="docker exec <worker-model-batch> chown",
                            started=started,
                            returncode=ownership.returncode,
                            stdout=ownership.stdout,
                            stderr=ownership.stderr,
                            layer="mount",
                        )
                        result["profile_id"] = task["profile_id"]
                        results[task["index"]] = result

                def run_task(task: dict[str, Any]) -> tuple[dict[str, Any], float, Any]:
                    task_started = time.perf_counter()
                    exec_cmd = [
                        "exec",
                        "--user", "kali",
                        "--workdir", task["task_container"],
                    ]
                    for key, value in task["env"].items():
                        exec_cmd += ["-e", f"{key}={value}"]
                    exec_cmd += [container_name, "bash", "-lc", task["script"]]
                    try:
                        run = _docker(
                            *exec_cmd,
                            timeout=task["timeout"] + 30,
                            **({"owner": owner, "container_name": container_name}
                               if owner is not None else {}),
                        )
                        return task, task_started, run
                    except Exception as exc:  # noqa: BLE001
                        return task, task_started, exc

                max_concurrent = max(1, min(4, len(prepared)))
                if ownership.returncode == 0:
                    with ThreadPoolExecutor(max_workers=max_concurrent) as pool:
                        futures = [pool.submit(run_task, task) for task in prepared]
                        for future in as_completed(futures):
                            task, task_started, outcome = future.result()
                            if isinstance(outcome, subprocess.TimeoutExpired):
                                result = _process_result(
                                    ok=False,
                                    detail=(
                                        "worker 容器模型测试超时"
                                        f"（>{task['timeout']}s）"
                                    ),
                                    engine=task["engine"],
                                    model=task["model"],
                                    backend="container",
                                    command="docker exec <worker-model-batch> … "
                                    + shlex.join(task["argv"]),
                                    started=task_started,
                                    stdout=getattr(outcome, "stdout", ""),
                                    stderr=getattr(outcome, "stderr", ""),
                                    layer="model",
                                    error_code="timeout",
                                )
                            elif isinstance(outcome, Exception):
                                result = _process_result(
                                    ok=False,
                                    detail=str(outcome)[:600],
                                    engine=task["engine"],
                                    model=task["model"],
                                    backend="container",
                                    command="docker exec <worker-model-batch> … "
                                    + shlex.join(task["argv"]),
                                    started=task_started,
                                    layer="cli",
                                )
                            else:
                                reply_ok = _probe_ok(task["profile"], outcome)
                                actual_models = (
                                    _claude_actual_models(outcome.stdout)
                                    if task["verify_claude_model"] else None
                                )
                                model_ok = (
                                    (not actual_models or _model_matches(
                                        task["model"], actual_models
                                    ))
                                    if task["verify_claude_model"] else True
                                )
                                ok = reply_ok and model_ok
                                failure_layer = ""
                                failure_code = ""
                                if reply_ok and not model_ok:
                                    actual_text = (
                                        "、".join(actual_models or [])
                                        or "未返回模型 ID"
                                    )
                                    detail = (
                                        f"模型不匹配：配置为 {task['model']}，"
                                        f"实际调用 {actual_text}"
                                    )
                                    failure_layer = "model"
                                    failure_code = "model_rejected"
                                elif not reply_ok:
                                    detail, failure_layer, failure_code = _model_turn_failure(
                                        engine=task["engine"],
                                        returncode=outcome.returncode,
                                        stdout=outcome.stdout,
                                        stderr=outcome.stderr,
                                    )
                                else:
                                    detail = (
                                        "worker 批量检查容器内模型可用，"
                                        "实际模型与配置一致"
                                        if ok and task["verify_claude_model"]
                                        else "worker 批量检查容器内模型可用"
                                        "（已完成真实对话）"
                                        if ok
                                        else "worker 容器模型测试失败"
                                    )
                                result = _process_result(
                                    ok=ok,
                                    detail=detail,
                                    engine=task["engine"],
                                    model=task["model"],
                                    backend="container",
                                    command="docker exec <worker-model-batch> … "
                                    + shlex.join(task["argv"]),
                                    started=task_started,
                                    returncode=outcome.returncode,
                                    stdout=outcome.stdout,
                                    stderr=outcome.stderr,
                                    layer=None if ok else failure_layer or "model",
                                    error_code=None if ok else failure_code or "model_rejected",
                                    actual_models=actual_models,
                                )
                            result["profile_id"] = task["profile_id"]
                            results[task["index"]] = result
        except FileNotFoundError:
            for task in prepared:
                result = _process_result(
                    ok=False,
                    detail="docker 不可用",
                    engine=task["engine"],
                    model=task["model"],
                    backend="container",
                    command="docker run --rm <worker-model-batch>",
                    started=time.perf_counter(),
                    layer="image",
                )
                result["profile_id"] = task["profile_id"]
                results[task["index"]] = result
        except subprocess.TimeoutExpired:
            for task in prepared:
                result = _process_result(
                    ok=False,
                    detail="Worker 批量检查容器启动超时",
                    engine=task["engine"],
                    model=task["model"],
                    backend="container",
                    command="docker run --rm <worker-model-batch>",
                    started=time.perf_counter(),
                    layer="cli",
                )
                result["profile_id"] = task["profile_id"]
                results[task["index"]] = result
        finally:
            if container_started:
                try:
                    _docker("rm", "-f", container_name, timeout=15)
                except (FileNotFoundError, subprocess.TimeoutExpired, OSError):
                    pass

    return {
        "backend": "container",
        "container_count": 1 if container_started else 0,
        "max_concurrent": max(1, min(4, len(prepared))),
        "results": [result for result in results if result is not None],
    }


def probe_worker_models_batch(
    *,
    items: list[dict[str, Any]],
    sessions_root: str | Path,
    backend: str = "local",
    runtime: dict[str, Any] | None = None,
    owner: ProbeProcessOwner | None = None,
) -> dict[str, Any]:
    """Run one real model turn for every requested profile."""

    normalized = [item for item in items if isinstance(item, dict)]
    if backend == "container":
        return _worker_container_model_batch_probe(
            items=normalized,
            sessions_root=sessions_root,
            runtime=runtime,
            owner=owner,
        )

    results: list[dict[str, Any] | None] = [None] * len(normalized)

    def run_local(index: int, item: dict[str, Any]) -> tuple[int, dict[str, Any]]:
        profile = dict(item.get("profile") or {})
        profile_id = str(
            item.get("profile_id")
            or profile.get("id")
            or profile.get("name")
            or f"profile-{index}"
        )
        result = probe_worker_model(
            profile=profile,
            model=str(item.get("model") or ""),
            reasoning_effort=str(item.get("reasoning_effort") or "default"),
            sessions_root=sessions_root,
            backend="local",
            runtime=runtime,
            owner=owner,
        )
        result["profile_id"] = profile_id
        return index, result

    if normalized:
        with ThreadPoolExecutor(max_workers=len(normalized)) as pool:
            futures = [
                pool.submit(run_local, index, item)
                for index, item in enumerate(normalized)
            ]
            for future in as_completed(futures):
                index, result = future.result()
                results[index] = result
    return {
        "backend": "local",
        "container_count": 0,
        "results": [result for result in results if result is not None],
    }


def parse_cursor_models(text: str) -> list[ModelOption]:
    """Small parser kept for future refresh tooling and tests."""

    rows: list[tuple[str, str]] = []
    for line in text.splitlines():
        if " - " not in line or line.lower().startswith("available models"):
            continue
        mid, label = line.split(" - ", 1)
        mid = mid.strip()
        label = label.strip()
        if mid:
            rows.append((mid, label or mid))

    # Cursor's CLI catalog contains concrete variants. Selecting one already
    # chooses its effort; do not synthesize another variant from a suffix.
    return [
        {"id": mid, "label": label, "reasoning": _reasoning(source="cursor_model_variant")}
        for mid, label in rows
    ]


def _openai_models_url(base_url: str) -> str:
    root = str(base_url or "").strip().rstrip("/")
    if root.endswith("/models"):
        return root
    return f"{root}/models"


class _CredentialCatalogRedirectHandler(urllib.request.HTTPRedirectHandler):
    """Credentials stay within the explicitly selected endpoint origin."""
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        from urllib.parse import urlsplit
        old, new = urlsplit(req.full_url), urlsplit(newurl)
        def origin(url):
            return (url.scheme.lower(), url.hostname, url.port or (443 if url.scheme == "https" else 80))
        if origin(old) != origin(new) or new.scheme not in {"http", "https"}:
            raise urllib.error.HTTPError(req.full_url, code, "catalog.redirect_scope_denied", headers, fp)
        return super().redirect_request(req, fp, code, msg, headers, newurl)


def _credential_catalog_open(request):
    return urllib.request.build_opener(_CredentialCatalogRedirectHandler()).open(request, timeout=20)


def discover_openai_compatible_models(
    *,
    base_url: str,
    secret: str = "",
) -> dict[str, Any]:
    """Read an OpenAI-compatible /models list from a custom endpoint."""
    url = _openai_models_url(base_url)
    if not url.startswith(("http://", "https://")):
        return {
            "ok": False,
            "models": [],
            "detail": "自定义端点需要填写 http(s) Base URL",
            "source": "endpoint",
            "error_code": "endpoint_protocol_mismatch",
        }
    headers = {
        "Accept": "application/json",
        "User-Agent": "muteki-credential-models/1.0",
    }
    token = str(secret or "").strip()
    if token:
        headers["Authorization"] = f"Bearer {token}"
    request = urllib.request.Request(url, headers=headers, method="GET")
    try:
        with _credential_catalog_open(request) as response:
            text = response.read().decode("utf-8", errors="replace")
    except urllib.error.HTTPError as exc:
        body = exc.read().decode("utf-8", errors="replace")[:240]
        return {
            "ok": False,
            "models": [],
            "detail": f"HTTP {exc.code}：{body or exc.reason}",
            "source": "endpoint",
            "error_code": "catalog_request_failed",
        }
    except urllib.error.URLError as exc:
        return {
            "ok": False,
            "models": [],
            "detail": f"无法连接端点：{exc.reason}",
            "source": "endpoint",
            "error_code": "catalog_request_failed",
        }
    except TimeoutError:
        return {
            "ok": False,
            "models": [],
            "detail": "读取模型列表超时（>20s）",
            "source": "endpoint",
            "error_code": "timeout",
        }
    try:
        models = parse_openai_models(text)
    except (json.JSONDecodeError, TypeError, ValueError):
        return {
            "ok": False,
            "models": [],
            "detail": "端点返回的不是可解析的模型列表",
            "source": "endpoint",
            "error_code": "endpoint_protocol_mismatch",
        }
    if not models:
        return {
            "ok": False,
            "models": [],
            "detail": "端点没有返回可用模型",
            "source": "endpoint",
            "error_code": "model_catalog_unsupported",
        }
    return {
        "ok": True,
        "models": models,
        "detail": f"从端点读到 {len(models)} 个模型",
        "source": "endpoint",
    }


def _anthropic_models_url(base_url: str) -> str:
    root = str(base_url or "").strip().rstrip("/")
    if root.endswith("/models"):
        return root
    if root.endswith("/v1"):
        return f"{root}/models"
    return f"{root}/v1/models"


def discover_anthropic_compatible_models(
    *,
    base_url: str,
    secret: str = "",
) -> dict[str, Any]:
    """Read Anthropic's credential-scoped ``GET /v1/models`` catalog."""
    url = _anthropic_models_url(base_url)
    if not url.startswith(("http://", "https://")):
        return {
            "ok": False,
            "models": [],
            "detail": "自定义端点需要填写 http(s) Base URL",
            "source": "anthropic_endpoint",
            "error_code": "endpoint_protocol_mismatch",
        }
    token = str(secret or "").strip()
    headers = {
        "Accept": "application/json",
        "User-Agent": "muteki-credential-models/1.0",
        "anthropic-version": "2023-06-01",
    }
    if token:
        headers["x-api-key"] = token
    request = urllib.request.Request(url, headers=headers, method="GET")
    try:
        with _credential_catalog_open(request) as response:
            text = response.read().decode("utf-8", errors="replace")
    except urllib.error.HTTPError as exc:
        body = exc.read().decode("utf-8", errors="replace")[:240]
        return {
            "ok": False,
            "models": [],
            "detail": f"HTTP {exc.code}：{body or exc.reason}",
            "source": "anthropic_endpoint",
            "error_code": "catalog_request_failed",
        }
    except urllib.error.URLError as exc:
        return {
            "ok": False,
            "models": [],
            "detail": f"无法连接端点：{exc.reason}",
            "source": "anthropic_endpoint",
            "error_code": "catalog_request_failed",
        }
    except TimeoutError:
        return {
            "ok": False,
            "models": [],
            "detail": "读取模型列表超时（>20s）",
            "source": "anthropic_endpoint",
            "error_code": "timeout",
        }
    try:
        data = json.loads(text)
    except json.JSONDecodeError:
        data = None
    items = data.get("data") if isinstance(data, dict) else None
    models = _dedupe_models([
        {
            "id": str(item.get("id") or "").strip(),
            "label": str(item.get("display_name") or item.get("id") or "").strip(),
        }
        for item in (items if isinstance(items, list) else [])
        if isinstance(item, dict) and str(item.get("id") or "").strip()
    ])
    if not models:
        return {
            "ok": False,
            "models": [],
            "detail": "端点不支持 Anthropic 模型目录，保留该凭据手工填写的模型",
            "source": "anthropic_endpoint",
            "error_code": "model_catalog_unsupported",
        }
    return {
        "ok": True,
        "models": models,
        "detail": f"从 Anthropic 模型接口读到 {len(models)} 个模型",
        "source": "anthropic_endpoint",
    }


def parse_openai_models(text: str) -> list[ModelOption]:
    data = json.loads(text)
    models = None
    if isinstance(data, dict):
        models = data.get("models") or data.get("data")
    if not isinstance(models, list):
        return []
    out: list[ModelOption] = []
    for item in models:
        if not isinstance(item, dict):
            continue
        mid = str(item.get("slug") or item.get("id") or "").strip()
        if mid:
            raw_levels = item.get("supported_reasoning_levels", item.get("supportedReasoningEfforts", []))
            levels = [
                level.get("effort", level.get("reasoningEffort")) if isinstance(level, dict) else level
                for level in raw_levels if level is not None
            ] if isinstance(raw_levels, list) else []
            out.append({
                "id": mid,
                "label": str(item.get("display_name") or item.get("displayName") or mid),
                **({"input": item["input"]} if isinstance(item.get("input"), list) else {}),
                "reasoning": _reasoning(
                    levels,
                    default=item.get("default_reasoning_level", item.get("defaultReasoningEffort", "")),
                    source="model_metadata",
                ),
            })
    return out


def parse_kimi_models(text: str) -> list[ModelOption]:
    data = json.loads(text)
    models = data.get("models") if isinstance(data, dict) else None
    if not isinstance(models, dict):
        return []
    out: list[ModelOption] = []
    for alias, metadata in models.items():
        model_id = str(alias or "").strip()
        if not model_id:
            continue
        provider = ""
        metadata = metadata if isinstance(metadata, dict) else {}
        if isinstance(metadata, dict):
            provider = str(
                metadata.get("provider") or metadata.get("provider_id") or ""
            ).strip()
        out.append({
            "id": model_id, "label": str(metadata.get("displayName") or model_id),
            "provider": provider,
            "reasoning": _reasoning(
                metadata.get("supportEfforts"), default=metadata.get("defaultEffort", ""),
                source="kimi_model_metadata",
            ),
        })
    return out


def parse_droid_models(text: str) -> list[ModelOption]:
    """Parse ``droid exec --help`` model tables from Droid 0.234.0."""
    lines = text.splitlines()
    models: list[tuple[str, str]] = []
    in_models = False
    for line in lines:
        if line.strip() == "Available Models:":
            in_models = True
            continue
        if in_models:
            if not line.strip() or line.startswith("Model details"):
                break
            parts = line.split()
            if len(parts) < 2:
                continue
            model_id = parts[0]
            label = " ".join(parts[1:]).replace("[Deprecated]", "").strip()
            models.append((model_id, label))
    efforts: dict[str, tuple[list[str], str]] = {}
    for line in lines:
        stripped = line.strip()
        if not stripped.startswith("- ") or "supported:" not in stripped:
            continue
        label = stripped[2:].split(":", 1)[0].strip()
        supported = ""
        default = ""
        if "supported:" in stripped:
            supported = stripped.split("supported:", 1)[1].split("]", 1)[0].strip(" [")
        if "default:" in stripped:
            default = stripped.split("default:", 1)[1].strip().rstrip(".")
        levels = [item.strip() for item in supported.split(",") if item.strip() and item.strip() != "none"]
        efforts[label.casefold()] = (levels, default if default != "none" else "")
    out: list[ModelOption] = []
    for model_id, label in models:
        levels, default = efforts.get(label.casefold(), ([], ""))
        out.append({
            "id": model_id,
            "label": label,
            "reasoning": _reasoning(levels, default=default, source="droid_exec_help"),
        })
    return out


def parse_devin_models(text: str) -> list[ModelOption]:
    data = json.loads(text)
    families = data.get("families") if isinstance(data, dict) else None
    if not isinstance(families, list):
        return []
    out: list[ModelOption] = []
    for family in families:
        if not isinstance(family, dict):
            continue
        for variant in family.get("variants") or []:
            if not isinstance(variant, dict):
                continue
            model_id = str(variant.get("model_uid") or "").strip()
            if model_id:
                out.append({
                    "id": model_id,
                    "label": str(variant.get("label") or model_id),
                    "reasoning": {"supported": False, "levels": [], "default": ""},
                })
    return out


def parse_grok_models(text: str) -> list[ModelOption]:
    if text.lstrip().startswith("{"):
        data = json.loads(text)
        out = []
        for item in (data.get("models") or {}).get("availableModels", []):
            meta = item.get("_meta") or {}
            choices = [row for row in meta.get("reasoningEfforts", []) if isinstance(row, dict)]
            levels = [row.get("value") or row.get("id") for row in choices]
            default = next((row.get("value") or row.get("id") for row in choices
                            if row.get("default") or row.get("isDefault")), "")
            out.append({
                "id": item.get("modelId"), "label": item.get("name") or item.get("modelId"),
                "reasoning": _reasoning(
                    levels, default=default, source="grok_acp_model_metadata",
                    supported=meta.get("supportsReasoningEffort"),
                ),
            })
        return out
    clean = re.sub(r"\x1b\[[0-9;]*m", "", text)
    out: list[ModelOption] = []
    for line in clean.splitlines():
        match = re.match(r"\s*[-*]\s+([^\s(]+)", line)
        if not match:
            continue
        mid = match.group(1).strip()
        out.append({
            "id": mid,
            "label": mid,
            "reasoning": _reasoning(source="grok_names_only"),
        })
    return out


def parse_pi_models(text: str) -> list[ModelOption]:
    if text.lstrip().startswith("{"):
        return [{
            "id": item["id"], "label": item.get("label") or item["id"],
            "provider": item.get("provider", ""),
            **({"input": item["input"]} if isinstance(item.get("input"), list) else {}),
            "reasoning": _reasoning(item.get("levels"), source="pi_model_runtime"),
        } for item in json.loads(text).get("models", []) if item.get("id")]
    out: list[ModelOption] = []
    for line in text.splitlines()[1:]:
        cols = line.split()
        if len(cols) < 2:
            continue
        out.append({
            "id": cols[1],
            "label": f"{cols[1]} ({cols[0]})",
            "provider": cols[0],
            "reasoning": _reasoning(source="pi_names_only"),
        })
    return out


def parse_omp_models(text: str) -> list[ModelOption]:
    data = json.loads(text)
    models = data.get("models") if isinstance(data, dict) else None
    if not isinstance(models, list):
        return []
    out: list[ModelOption] = []
    for item in models:
        if not isinstance(item, dict):
            continue
        mid = str(item.get("selector") or item.get("id") or "").strip()
        if not mid:
            continue
        out.append({
            "id": mid,
            "label": str(item.get("name") or mid),
            **({"input": item["input"]} if isinstance(item.get("input"), list) else {}),
            "provider": str(
                item.get("provider") or item.get("provider_id")
                or (mid.split("/", 1)[0] if "/" in mid else "")
            ).strip(),
            "reasoning": _reasoning(item.get("thinking"), source="omp_model_metadata"),
        })
    return out


class ModelDiscoveryError(RuntimeError):
    def __init__(self, message: str, code: str = "catalog_request_failed") -> None:
        self.code = code
        super().__init__(message)


def parse_opencode_models(text: str) -> list[ModelOption]:
    """`models --verbose` emits an id followed by one JSON object per model."""
    out: list[ModelOption] = []
    decoder = json.JSONDecoder()
    offset = 0
    while offset < len(text):
        start = text.find("{", offset)
        if start < 0:
            break
        try:
            item, length = decoder.raw_decode(text[start:])
        except ValueError:
            break
        offset = start + length
        if not isinstance(item, dict) or not item.get("id") or not item.get("providerID"):
            continue
        provider = str(item["providerID"])
        out.append({
            "id": f"{provider}/{item['id']}", "label": item.get("name") or item["id"],
            "provider": provider,
            "reasoning": _reasoning(
                list((item.get("variants") or {}).keys()),
                kind="variant", source="opencode_model_variants",
            ),
        })
    if out:
        return out
    return [{"id": line.strip(), "label": line.strip(),
             "reasoning": _reasoning(source="opencode_names_only")}
            for line in text.splitlines() if re.fullmatch(r"[\w.-]+/[\w./:-]+", line.strip())]


def _discovery_argv(engine: str, binary: str, *, bundled: bool = False) -> list[str]:
    descriptor = find_descriptor(engine)
    spec = descriptor.models if descriptor is not None else None
    if spec is None or spec.method != "cli" or not spec.argv:
        raise ModelDiscoveryError(
            f"{engine} 当前没有非交互模型发现命令",
            "model_catalog_unsupported",
        )
    if bundled and not spec.fallback_argv:
        raise ModelDiscoveryError(
            f"{engine} 没有内置模型目录命令",
            "model_catalog_unsupported",
        )
    return [binary, *(spec.fallback_argv if bundled else spec.argv)]


def _writes_agent_state(engine: str) -> bool:
    descriptor = find_descriptor(engine)
    return descriptor is not None and descriptor.credentials.agent_state_dir


def _endpoint_test_config_var(engine: str, profile: dict[str, Any]) -> str:
    """Native config variable to point at a throwaway dir for endpoint model tests."""
    descriptor = find_descriptor(engine)
    if (descriptor is not None and descriptor.models.endpoint_test_isolated_config
            and profile_uses_endpoint(profile)):
        return descriptor.environment.home_env_var
    return ""


def _private_model_probe_env(
    engine: str, sessions_root: str | Path, account_id: str, env: dict[str, str],
) -> dict[str, str]:
    # Model catalogs and minimal model tests can refresh auth or write CLI
    # preferences too. Keep that discovery traffic out of the operator home.
    from muteki.conversation.chat_plugins import ChatPluginService, ENGINES
    if engine not in ENGINES:
        return env
    service = ChatPluginService(Path(sessions_root) / "_model_probe_environments")
    return service.prepare_environment(engine, "model-probe:" + account_id, env, include_assets=False)


@contextmanager
def _temporary_model_probe_env(engine: str, sessions_root: str | Path, account_id: str, env: dict[str, str]):
    base = Path(sessions_root) / "_model_probe_environments"
    base.mkdir(parents=True, exist_ok=True, mode=0o700)
    # Each concurrent request gets its own small home. Success, failure and
    # cancellation all release it; model catalogs do not need plugin assets.
    with tempfile.TemporaryDirectory(prefix="probe-", dir=base) as directory:
        yield _private_model_probe_env(engine, directory, account_id, env)


def _run_local_discovery(
    profile: dict[str, Any], sessions_root: str | Path, *, bundled: bool = False,
) -> subprocess.CompletedProcess:
    engine = base_engine_for_profile(profile)
    account_id = str(profile.get("credential_account") or "").strip()
    resolved_account_id = account_id if account_id else ""
    if not resolved_account_id and not host_discovery_enabled():
        raise ModelDiscoveryError("此服务未提供宿主模型发现；请选择已登记凭据", "host_discovery_disabled")
    resolved = runtime_env_for_engine(
        engine,
        account_root=account_store_root(sessions_root),
        account_id=resolved_account_id,
        container=False,
    )
    binary = str(profile.get("binary_path") or "").strip() or driver_for(profile).bin
    argv = _discovery_argv(engine, binary, bundled=bundled)
    env = {**os.environ, **driver_for(profile).env_extra(), **resolved.env}
    descriptor = find_descriptor(engine)
    metadata_probe = descriptor.models.metadata_probe if descriptor is not None else ""
    metadata_error = ""
    with _temporary_model_probe_env(engine, sessions_root, resolved_account_id, env) as env:
        try:
            if metadata_probe == "grok_acp_session":
                # Called from a worker thread (routes use asyncio.to_thread), so
                # a private event loop here does not nest inside the server loop.
                try:
                    return subprocess.CompletedProcess(
                        argv, 0, json.dumps(asyncio.run(_grok_model_metadata(binary, env))), "",
                    )
                except Exception as exc:  # noqa: BLE001 — recorded below
                    # Older Grok versions can still list names. No effort options
                    # are invented when the metadata handshake is unavailable.
                    metadata_error = f"grok ACP model metadata failed: {type(exc).__name__}: {exc}"
            if metadata_probe == "pi_node_catalog" and shutil.which("node"):
                probe = Path(__file__).resolve().parents[2] / "muteki/solver/pi_model_catalog.mjs"
                metadata = subprocess.run(
                    [shutil.which("node"), str(probe), shutil.which(binary) or binary],
                    capture_output=True, text=True, timeout=30, env=env,
                )
                if metadata.returncode == 0 and metadata.stdout.lstrip().startswith("{"):
                    return metadata
                metadata_error = (
                    f"pi node model catalog exited {metadata.returncode}: "
                    f"{(metadata.stderr or metadata.stdout or '').strip()}")
            if metadata_error:
                _LOG.warning("%s model metadata unavailable, listing names only: %s",
                             engine, metadata_error)
            result = subprocess.run(
                argv,
                capture_output=True,
                text=True,
                encoding="utf-8",
                errors="replace",
                timeout=45,
                env=env,
            )
            if metadata_error:
                result.stderr = "\n".join(
                    part for part in ((result.stderr or "").rstrip(), metadata_error) if part)
            return result
        except FileNotFoundError as exc:
            raise ModelDiscoveryError("CLI 不存在", "cli_missing") from exc
        except subprocess.TimeoutExpired as exc:
            raise ModelDiscoveryError("模型发现超时（>45s）", "timeout") from exc


async def _grok_model_metadata(binary: str, env: dict[str, str]) -> dict[str, Any]:
    from muteki.external_agents.acp import AcpTransport

    with tempfile.TemporaryDirectory(prefix="muteki-model-catalog-") as cwd:
        transport = AcpTransport(
            [binary, "agent", "--no-leader", "stdio"], cwd=cwd, env=env,
            client_name="muteki-model-catalog",
        )
        try:
            await transport.start()
            await transport.initialize(timeout=15)
            session = await transport.new_session(cwd, [], timeout=20)
            return {"models": transport.session_setup(session).get("models", {})}
        finally:
            await transport.close()


def _run_container_discovery(
    profile: dict[str, Any], sessions_root: str | Path, *, bundled: bool = False,
) -> subprocess.CompletedProcess:
    from muteki.solver.container_exec import (
        CONTAINER_WORKSPACE,
        WORKER_IMAGE,
        WorkerNetworkConfigError,
        _HOST_DATA_ROOT,
        _mount_source,
        resolve_worker_run_network,
    )

    engine = base_engine_for_profile(profile)
    try:
        image = _docker("image", "inspect", WORKER_IMAGE, timeout=20)
    except FileNotFoundError as exc:
        raise ModelDiscoveryError("docker 不可用", "cli_missing") from exc
    except subprocess.TimeoutExpired as exc:
        raise ModelDiscoveryError("worker 镜像检查超时", "timeout") from exc
    if image.returncode != 0:
        raise ModelDiscoveryError(f"worker 镜像缺失或不可用: {WORKER_IMAGE}")

    root = account_store_root(sessions_root)
    account_id = str(profile.get("credential_account") or "").strip() or None
    resolved = runtime_env_for_engine(
        engine, account_root=root, account_id=account_id, container=True
    )
    binary = (
        str(profile.get("binary_path") or "").strip()
        or _CONTAINER_BIN.get(engine)
        or engine
    )
    argv = _discovery_argv(engine, binary, bundled=bundled)
    tmp_base = None
    if _HOST_DATA_ROOT:
        tmp_base = os.path.join(
            os.environ.get("MUTEKI_CONTAINER_DATA_ROOT") or _HOST_DATA_ROOT,
            "_tmp",
            "model-discovery",
        )
        try:
            os.makedirs(tmp_base, exist_ok=True)
        except OSError:
            tmp_base = None

    with tempfile.TemporaryDirectory(prefix="muteki-model-discovery-", dir=tmp_base) as td:
        workspace = os.path.join(td, "ws")
        projection = os.path.join(td, "accounts")
        os.makedirs(workspace, exist_ok=True)
        try:
            os.chmod(workspace, 0o777)
            project_account_root(root, projection, account_ids=[account_id or engine_account_id(engine)])
        except OSError as exc:
            raise ModelDiscoveryError(f"凭据投影失败: {str(exc)[:120]}") from exc

        prelude = [
            'if [ -r "$CLAUDE_CODE_OAUTH_TOKEN_FILE" ]; then '
            'export CLAUDE_CODE_OAUTH_TOKEN="$(cat "$CLAUDE_CODE_OAUTH_TOKEN_FILE")"; fi',
            'if [ -r "$ANTHROPIC_AUTH_TOKEN_FILE" ]; then '
            'export ANTHROPIC_AUTH_TOKEN="$(cat "$ANTHROPIC_AUTH_TOKEN_FILE")"; fi',
            'if [ -r "$CURSOR_API_KEY_FILE" ]; then '
            'export CURSOR_API_KEY="$(cat "$CURSOR_API_KEY_FILE")"; fi',
            'if [ -r "$ANTHROPIC_API_KEY_FILE" ]; then '
            'export ANTHROPIC_API_KEY="$(cat "$ANTHROPIC_API_KEY_FILE")"; fi',
            'if [ -r "$OPENAI_API_KEY_FILE" ]; then '
            'export OPENAI_API_KEY="$(cat "$OPENAI_API_KEY_FILE")"; fi',
            'if [ -r "$OPENCODE_API_KEY_FILE" ]; then '
            'export OPENCODE_API_KEY="$(cat "$OPENCODE_API_KEY_FILE")"; fi',
            'if [ -r "$DEEPSEEK_API_KEY_FILE" ]; then '
            'export DEEPSEEK_API_KEY="$(cat "$DEEPSEEK_API_KEY_FILE")"; fi',
            'if [ -r "$KIMI_MODEL_API_KEY_FILE" ]; then '
            'export KIMI_MODEL_API_KEY="$(cat "$KIMI_MODEL_API_KEY_FILE")"; fi',
            'if [ -r "$XAI_API_KEY_FILE" ]; then '
            'export XAI_API_KEY="$(cat "$XAI_API_KEY_FILE")"; fi',
        ]
        script = "; ".join(prelude) + f"; exec timeout -s KILL 45s {shlex.join(argv)} < /dev/null"
        network = resolve_worker_run_network(None)
        run_cmd = [
            "run", "--rm", "--init",
            "--network", network,
            "--user", "kali",
            "--workdir", CONTAINER_WORKSPACE,
            "--entrypoint", "bash",
            "--mount", f"type=bind,source={_mount_source(workspace)},target={CONTAINER_WORKSPACE}",
            "--mount", f"type=bind,source={_mount_source(projection)},target={CONTAINER_ACCOUNTS_ROOT}",
        ]
        if network != "host":
            run_cmd += ["--add-host", "host.docker.internal:host-gateway"]
        for key, value in {**_CONTAINER_BASE_ENV, **resolved.env}.items():
            run_cmd += ["-e", f"{key}={value}"]
        run_cmd += [WORKER_IMAGE, "-lc", script]
        try:
            return _docker(*run_cmd, timeout=75)
        except FileNotFoundError as exc:
            raise ModelDiscoveryError("docker 不可用", "cli_missing") from exc
        except subprocess.TimeoutExpired as exc:
            raise ModelDiscoveryError(
                "worker 容器模型发现超时（>45s）", "timeout"
            ) from exc


_MODEL_PARSERS: dict[ModelParser, Any] = {
    "openai_models": parse_openai_models,
    "cursor_models": parse_cursor_models,
    "pi_models": parse_pi_models,
    "omp_models": parse_omp_models,
    "kimi_models": parse_kimi_models,
    "grok_models": parse_grok_models,
    "opencode_models": parse_opencode_models,
    "devin_models": parse_devin_models,
    "droid_models": parse_droid_models,
}


def _parse_discovery(engine: str, output: str) -> list[ModelOption]:
    descriptor = find_descriptor(engine)
    parser = descriptor.models.parser if descriptor is not None else None
    if parser is None:
        return []
    try:
        return _dedupe_models(_MODEL_PARSERS[parser](output))
    except (json.JSONDecodeError, TypeError, ValueError):
        return []


def discover_worker_models(
    *,
    profile: dict[str, Any],
    sessions_root: str | Path,
    backend: str,
) -> dict[str, Any]:
    """Discover one profile's models only when the operator requests it."""

    profile = dict(profile or {})
    engine = base_engine_for_profile(profile)
    profile_id = str(profile.get("id") or profile.get("name") or engine).strip()
    base = {
        "profile_id": profile_id,
        "engine": engine,
        "updated_at": time.time(),
        "models": [],
    }
    descriptor = find_descriptor(engine)
    if descriptor is not None and descriptor.models.method == "reference_catalog":
        return {
            **base,
            "ok": True,
            "source": f"{engine}_reference_catalog",
            "models": _dedupe_models(_manual_options(
                engine, list(WORKER_MODEL_OPTIONS.get(engine) or [])
            )),
            "detail": (
                f"{descriptor.identity.display_name} 没有稳定的模型清单命令；"
                "这里显示参考目录，真实可用性以手动连通测试为准"
            ),
        }
    if descriptor is None or descriptor.models.method != "cli":
        return {
            **base,
            "ok": False,
            "source": "unsupported",
            "detail": f"{engine} 当前未接入模型自动发现",
        }

    runner = _run_container_discovery if backend == "container" else _run_local_discovery
    try:
        result = runner(profile, sessions_root, bundled=False)
    except ModelDiscoveryError as exc:
        return {
            **base,
            "ok": False,
            "source": "cli",
            "detail": str(exc),
            "error_code": exc.code,
        }

    models = _parse_discovery(engine, result.stdout or "")
    source = f"{engine}_cli"
    detail = ""
    if descriptor.models.fallback_argv and (result.returncode != 0 or not models):
        remote_detail = _detail(result.returncode, result.stdout, result.stderr)
        try:
            bundled_result = runner(profile, sessions_root, bundled=True)
        except ModelDiscoveryError as exc:
            return {
                **base,
                "ok": False,
                "source": source,
                "detail": f"远程目录失败；内置目录失败: {exc}",
                "error_code": exc.code,
            }
        models = _parse_discovery(engine, bundled_result.stdout or "")
        result = bundled_result
        source = descriptor.models.fallback_source
        detail = f"远程目录不可用，已读取 CLI 内置目录；{remote_detail}"

    provider = str(profile.get("provider") or "").strip().lower()
    if provider and descriptor.models.provider_scoped:
        filtered = [
            item for item in models
            if str(item.get("provider") or "").strip().lower() == provider
            or str(item.get("id") or "").strip().lower().startswith(f"{provider}/")
        ]
        models = filtered

    if result.returncode != 0 or not models:
        output = f"{result.stdout or ''}\n{result.stderr or ''}".casefold()
        error_code = (
            "not_logged_in"
            if any(marker in output for marker in (
                "not logged in", "login required", "unauthorized",
                "authentication", "invalid api key", "http 401", "http 403",
            ))
            else "model_catalog_unsupported"
            if result.returncode == 0
            else "catalog_request_failed"
        )
        return {
            **base,
            "ok": False,
            "source": source,
            "detail": _detail(result.returncode, result.stdout, result.stderr),
            "error_code": error_code,
        }
    return {
        **base,
        "ok": True,
        "source": source,
        "models": models,
        "detail": detail or f"发现 {len(models)} 个模型",
    }
