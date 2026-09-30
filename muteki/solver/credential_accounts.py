"""Credential Account resolution for CLI workers.

This module keeps subscription/API credentials out of prompts, worker scratch,
and the normal worker config JSON. It resolves a small, explicit account store:

    state/_secrets/accounts/<account_id>/

Container workers see that root at /run/muteki/accounts. Local workers can use
the same files directly. Environment variables remain a developer convenience,
but the persistent path is account-scoped instead of mounting a host home dir.
"""

from __future__ import annotations

import json
import tempfile
from contextlib import contextmanager
import os
import re
import shutil
import threading
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Mapping, Optional, Iterable

from muteki.solver.engine_registry import (
    KNOWN_ENGINE_IDS,
    SUPPORTED_ENGINE_IDS,
    EngineTemporarilyUnsupportedError,
    canonical_engine_id,
)

_ACCOUNT_WRITE_LOCK = threading.RLock()


def host_discovery_enabled(env: Mapping[str, str] | None = None) -> bool:
    """Explicit service policy for automatic operator-home login discovery."""
    if os.environ.get("MUTEKI_HOST_DISCOVERY") == "0":
        return False
    return env is None or env.get("MUTEKI_HOST_DISCOVERY") != "0"


class HostDiscoveryDisabledError(ValueError):
    code = "host_discovery_disabled"


class CredentialTestRevisionError(ValueError):
    code = "credential.test.configuration_changed"


CONTAINER_ACCOUNTS_ROOT = "/run/muteki/accounts"

KNOWN_CREDENTIAL_ENGINES = KNOWN_ENGINE_IDS
SUPPORTED_CREDENTIAL_ENGINES = SUPPORTED_ENGINE_IDS
ACCOUNT_CREDENTIAL_PREFIX = "account:"
SYSTEM_CREDENTIAL_PREFIX = "system:"

# Pi 的 OpenAI-compatible provider 使用受管会话自己的显式请求策略。
# 该配置写入 PI_CODING_AGENT_DIR，不依赖宿主 Pi 的全局设置，也不影响
# 订阅账户。
_PI_PROVIDER_REQUEST_TIMEOUT_MS = 300_000
# Pi 的 provider 重试会将每次连接失败再次提交一遍请求。自定义端点明确
# 不可达时，这只会把一次约 5 秒的连接失败扩大为两次尝试；上游恢复后由下一
# 次调度健康检查重新纳入即可，因此这里保持单次请求并让真实错误尽快显现。
_PI_PROVIDER_MAX_RETRIES = 0

# Official endpoints are product/runtime configuration, not credential data.
# The credentials UI therefore never asks the operator for these values.  A
# stored BASE_URL always means an explicitly selected custom endpoint and wins
# over this table.
_OFFICIAL_BASE_URLS: dict[str, str] = {}

# A stored account must be the only credential source visible to a Conversation
# subprocess. Runtime instances used to carry a default credential and the host
# process may also expose provider variables. Before applying a stored account we
# shadow every credential/endpoint variable understood by that engine. A selected
# ``system:<engine>`` deliberately skips this reset so its host login remains
# available.
_CREDENTIAL_ENV_KEYS: dict[str, tuple[str, ...]] = {
    "claude": (
        "CLAUDE_CODE_OAUTH_TOKEN", "CLAUDE_CODE_OAUTH_TOKEN_FILE",
        "ANTHROPIC_API_KEY", "ANTHROPIC_API_KEY_FILE",
        "ANTHROPIC_AUTH_TOKEN", "ANTHROPIC_AUTH_TOKEN_FILE",
        "ANTHROPIC_BASE_URL",
    ),
    "codex": (
        "OPENAI_API_KEY", "OPENAI_API_KEY_FILE", "OPENAI_BASE_URL",
        "CODEX_HOME",
    ),
    "cursor": (
        "CURSOR_API_KEY", "CURSOR_API_KEY_FILE", "CURSOR_AUTH_TOKEN",
        "CURSOR_AUTH_TOKEN_FILE", "CURSOR_ENDPOINT",
    ),
    "pi": (
        "OPENAI_API_KEY", "OPENAI_API_KEY_FILE", "OPENAI_BASE_URL",
        "PI_CODING_AGENT_DIR", "MUTEKI_PI_PROVIDER", "MUTEKI_PI_MODEL",
    ),
    "omp": (
        "OPENAI_API_KEY", "OPENAI_API_KEY_FILE", "OPENAI_BASE_URL",
        "PI_CODING_AGENT_DIR", "MUTEKI_OMP_PROVIDER", "MUTEKI_OMP_MODEL",
    ),
    "kimi": (
        "KIMI_MODEL_API_KEY", "KIMI_MODEL_API_KEY_FILE",
        "KIMI_MODEL_BASE_URL", "KIMI_MODEL_NAME",
        "KIMI_MODEL_PROVIDER_TYPE", "KIMI_MODEL_MAX_CONTEXT_SIZE",
        "KIMI_MODEL_MAX_OUTPUT_SIZE", "KIMI_CODE_HOME",
    ),
    "grok": (
        "XAI_API_KEY", "XAI_API_KEY_FILE", "GROK_MODELS_BASE_URL",
        "GROK_HOME",
    ),
    "opencode": (
        "OPENAI_API_KEY", "OPENAI_API_KEY_FILE", "OPENAI_BASE_URL",
        "OPENCODE_API_KEY", "OPENCODE_API_KEY_FILE",
        "OPENCODE_CONFIG_CONTENT", "MUTEKI_OPENCODE_PROVIDER",
    ),
}


@dataclass(frozen=True)
class RuntimeCredentialEnv:
    """Environment to add to a worker subprocess plus its account id."""

    account_id: str
    env: dict[str, str]


@dataclass(frozen=True)
class CredentialAccount:
    account_id: str
    engine: str
    mode: str
    present: bool
    writable_state: bool
    updated_at: float | None = None
    details: dict[str, Any] | None = None


_ACCOUNT_ID_RE = re.compile(r"^[A-Za-z0-9][A-Za-z0-9_.-]{0,63}$")


def account_credential_id(account_id: str) -> str:
    """Return the stable global id for a persisted credential account."""
    value = str(account_id or "").strip()
    if not valid_account_id(value):
        raise ValueError("invalid credential account id")
    return f"{ACCOUNT_CREDENTIAL_PREFIX}{value}"


def system_credential_id(engine: str) -> str:
    """Return the stable global id for a host-login credential."""
    value = str(engine or "").strip().lower()
    if value not in KNOWN_CREDENTIAL_ENGINES:
        raise ValueError("invalid system credential engine")
    return f"{SYSTEM_CREDENTIAL_PREFIX}{value}"


def canonical_credential_id(value: Any, *, engine: str = "") -> str:
    """Normalize old account handles and new ids to one stable reference.

    Accepted compatibility inputs are a bare account id and the previously
    published ``secret://credential-accounts/<id>`` handle.  Empty values stay
    empty so callers can distinguish an old unbound record from an explicit
    ``system:<engine>`` selection.
    """
    text = str(value or "").strip()
    if not text:
        return ""
    if text.startswith("secret://credential-accounts/"):
        text = text.removeprefix("secret://credential-accounts/").strip()
    if text.startswith(ACCOUNT_CREDENTIAL_PREFIX):
        return account_credential_id(text.removeprefix(ACCOUNT_CREDENTIAL_PREFIX))
    if text.startswith(SYSTEM_CREDENTIAL_PREFIX):
        selected_engine = text.removeprefix(SYSTEM_CREDENTIAL_PREFIX).strip().lower()
        if engine and selected_engine != str(engine).strip().lower():
            raise ValueError(
                f"credential {text!r} does not match engine {engine!r}")
        return system_credential_id(selected_engine)
    return account_credential_id(text)


def account_id_from_credential_id(value: Any) -> str:
    """Return an account id for an ``account:`` reference, otherwise ``""``."""
    try:
        credential_id = canonical_credential_id(value)
    except ValueError:
        return ""
    if credential_id.startswith(ACCOUNT_CREDENTIAL_PREFIX):
        return credential_id.removeprefix(ACCOUNT_CREDENTIAL_PREFIX)
    return ""


def engine_from_system_credential_id(value: Any) -> str:
    """Return the engine encoded by a ``system:`` reference."""
    text = str(value or "").strip().lower()
    if not text.startswith(SYSTEM_CREDENTIAL_PREFIX):
        return ""
    engine = text.removeprefix(SYSTEM_CREDENTIAL_PREFIX)
    return engine if engine in KNOWN_CREDENTIAL_ENGINES else ""


def credential_env_reset(engine: str) -> dict[str, str]:
    """Return an empty-value overlay that removes stale auth for one engine.

    Subprocess launchers merge an overlay on top of their process/default
    environment. Empty values therefore shadow an old Runtime or host value
    without mutating the parent process. The selected account is applied after
    this overlay by :func:`resolve_credential_env`.
    """
    selected = str(engine or "").strip().lower()
    return {name: "" for name in _CREDENTIAL_ENV_KEYS.get(selected, ())}


def account_store_root(state_root: str | Path) -> Path:
    """Default durable account store under the coordinator-private state root."""

    return Path(state_root) / "_secrets" / "accounts"


def engine_account_id(engine: str, env: Mapping[str, str] | None = None) -> str:
    """Return the account id for an engine, overridable per engine by env."""

    e = (engine or "").strip().lower()
    if canonical_engine_id(e) == "dsh":
        raise EngineTemporarilyUnsupportedError("dsh")
    # env={} means "no overrides" (explicit empty mapping), NOT "use the host
    # env" — `env or os.environ` silently falls back to the real environment.
    source = env if env is not None else os.environ
    return (
        source.get(f"MUTEKI_{e.upper()}_ACCOUNT_ID")
        or source.get("MUTEKI_DEFAULT_ACCOUNT_ID")
        or f"{e}-main"
    )


def valid_account_id(account_id: str) -> bool:
    return bool(_ACCOUNT_ID_RE.fullmatch(account_id or ""))


class CredentialAccountStore:
    """Small filesystem-backed account store for subscription/API workers."""

    def __init__(self, root: str | Path) -> None:
        self.root = Path(root)
        self.root.mkdir(parents=True, exist_ok=True)
        self._guard_depth = 0
        self._recover_staged_updates()
        try:
            self.root.chmod(0o700)
        except OSError:
            pass

    @contextmanager
    def _account_guard(self):
        """Serialize writers and recovery across threads and service processes."""
        with _ACCOUNT_WRITE_LOCK:
            if self._guard_depth:
                self._guard_depth += 1
                try:
                    yield
                finally:
                    self._guard_depth -= 1
                return
            lock_path = self.root.parent / f".{self.root.name}.lock"
            with lock_path.open("a+b") as handle:
                if os.name == "nt":
                    import msvcrt
                    if not handle.tell():
                        handle.write(b"0"); handle.flush()
                    handle.seek(0)
                    msvcrt.locking(handle.fileno(), msvcrt.LK_LOCK, 1)
                else:
                    import fcntl
                    fcntl.flock(handle.fileno(), fcntl.LOCK_EX)
                self._guard_depth = 1
                try:
                    yield
                finally:
                    self._guard_depth = 0
                    if os.name == "nt":
                        handle.seek(0); msvcrt.locking(handle.fileno(), msvcrt.LK_UNLCK, 1)
                    else:
                        fcntl.flock(handle.fileno(), fcntl.LOCK_UN)

    def _recover_staged_updates(self) -> None:
        with self._account_guard():
            for staging_root in self.root.parent.glob(".credential-stage-*"):
                journal = staging_root / "transaction.json"
                if not journal.is_file():
                    continue
                record = json.loads(journal.read_text(encoding="utf-8"))
                if record.get("store_root") != str(self.root.resolve()):
                    continue
                account_id = str(record.get("account_id") or "")
                if not valid_account_id(account_id):
                    raise ValueError("credential.account.recovery_invalid")
                if record.get("phase") not in {"staging", "prepared", "committed"}:
                    raise ValueError("credential.account.recovery_phase_invalid")
                live, backup = self.root / account_id, staging_root / "previous"
                if record.get("phase") != "committed":
                    if backup.exists():
                        if live.exists():
                            shutil.rmtree(live)
                        backup.rename(live)
                    elif record.get("had_previous") and not live.exists():
                        raise OSError("credential.account.recovery_material_missing")
                    elif not record.get("had_previous") and live.exists() and not (staging_root / "accounts" / account_id).exists():
                        shutil.rmtree(live)
                shutil.rmtree(staging_root)

    def list(self) -> list[dict[str, Any]]:
        accounts: list[CredentialAccount] = []
        if not self.root.exists():
            return []
        for p in sorted(self.root.iterdir(), key=lambda x: x.name):
            if not p.is_dir() or not valid_account_id(p.name):
                continue
            acct = self.inspect(p.name)
            if acct is not None:
                accounts.append(acct)
        return [self._public(a) for a in accounts]

    @property
    def _test_status_path(self) -> Path:
        return self.root.parent / "_credential_test_status.json"

    def _read_test_statuses(self) -> dict[str, dict[str, Any]]:
        try:
            raw = json.loads(self._test_status_path.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError):
            return {}
        if not isinstance(raw, dict):
            return {}
        return {
            str(key): dict(value)
            for key, value in raw.items()
            if isinstance(value, dict)
        }

    def _test_runtime_key(self, credential_id: str, runtime_instance: str) -> str:
        if ":" in runtime_instance:
            return runtime_instance
        engine = engine_from_system_credential_id(credential_id)
        account_id = account_id_from_credential_id(credential_id)
        if account_id:
            account = self.inspect(account_id)
            if account is not None:
                engine = str(self._public(account).get("worker_engine") or "")
        return f"cli.{engine}:{runtime_instance or 'default'}" if engine else runtime_instance

    def save_test_status(self, credential_id: str, result: Mapping[str, Any], *,
                         backend: str = "local", runtime_instance: str = "default",
                         expected_revision: str | None = None) -> dict[str, Any]:
        with _ACCOUNT_WRITE_LOCK:
            stable_id = canonical_credential_id(credential_id)
            account_id = account_id_from_credential_id(stable_id)
            revision = self.revision(account_id) if account_id else ""
            if expected_revision is not None and revision != expected_revision:
                raise CredentialTestRevisionError("credential.test.configuration_changed")
            runtime_key = self._test_runtime_key(stable_id, runtime_instance)
            row = {"ok": bool(result.get("ok")), "detail": str(result.get("detail") or ""),
                   "layer": str(result.get("layer") or ""),
                   "backend": "container" if backend == "container" else "local",
                   "runtime_instance": runtime_key, "configuration_revision": revision,
                   "tested_at": time.time(), "model": str(result.get("model") or "").strip()}
            statuses = self._read_test_statuses()
            statuses[f"{stable_id}\0{row['backend']}\0{runtime_key}\0{row['model']}"] = row
            self._atomic_write(self._test_status_path, json.dumps(statuses, ensure_ascii=False, indent=2))
            return dict(row)

    def last_test(self, credential_id: str, *, backend: str = "local",
                  runtime_instance: str = "default", model: str | None = None) -> dict[str, Any] | None:
        try:
            stable_id = canonical_credential_id(credential_id)
        except ValueError:
            return None
        runtime_key = self._test_runtime_key(stable_id, runtime_instance)
        account_id = account_id_from_credential_id(stable_id)
        revision = self.revision(account_id) if account_id else ""
        rows = [row for key, row in self._read_test_statuses().items()
                if (key == stable_id or key.startswith(stable_id + "\0"))
                and row.get("backend", "local") == backend
                and self._test_runtime_key(stable_id, str(row.get("runtime_instance") or "default")) == runtime_key
                and (model is None or row.get("model") == model)
                and (not row.get("configuration_revision") or row["configuration_revision"] == revision)]
        return dict(max(rows, key=lambda row: float(row.get("tested_at") or 0))) if rows else None

    def invalidate_test_status(self, credential_id: str) -> None:
        with _ACCOUNT_WRITE_LOCK:
            self._invalidate_test_status(credential_id)

    def _invalidate_test_status(self, credential_id: str) -> None:
        """Remove stale model evidence after credential metadata changes."""
        stable_id = canonical_credential_id(credential_id)
        statuses = self._read_test_statuses()
        keys = [key for key in statuses if key == stable_id or key.startswith(stable_id + "\0")]
        if not keys:
            return
        for key in keys:
            statuses.pop(key, None)
        path = self._test_status_path
        path.parent.mkdir(parents=True, exist_ok=True)
        tmp = path.with_suffix(".json.tmp")
        tmp.write_text(
            json.dumps(statuses, ensure_ascii=False, indent=2),
            encoding="utf-8",
        )
        tmp.replace(path)

    def credential_catalog(
        self,
        *,
        usage: Mapping[str, list[dict[str, Any]]] | None = None,
        catalogs: Mapping[str, Mapping[str, Any]] | None = None,
        engines: tuple[str, ...] = SUPPORTED_CREDENTIAL_ENGINES,
        environment: str = "local",
    ) -> list[dict[str, Any]]:
        """Project Worker-engine accounts and host logins into the credential API.

        Secret material remains owned by this store or by the engine's host login.
        Consumers receive stable ids and public metadata only.

        HTTP model endpoints stored as engine ``api`` (planner/titler, no
        Worker ENGINE marker) and empty ``unknown`` account dirs stay out of
        this projection. Those rows belong to ``/api/settings/model-endpoints``
        or are leftover directories, not Conversation/Worker engines.
        """
        usage = usage or {}
        catalogs = catalogs or {}
        rows: list[dict[str, Any]] = []

        def catalog_models(credential_id: str) -> list[str]:
            out: list[str] = []
            catalog = catalogs.get(credential_id) or {}
            for item in catalog.get("discovered_models") or []:
                model = str(
                    item.get("id") if isinstance(item, Mapping) else item
                ).strip()
                if model and model not in out:
                    out.append(model)
            return out

        def endpoint_unavailable(
            connection: str, catalog: Mapping[str, Any]
        ) -> str:
            """返回应从默认候选排除的端点连通性失败说明。

            ``/models`` 不受所有 OpenAI 兼容端点支持，因此 404、协议不兼容
            等目录问题不能据此禁用一个本可推理的账号。只有目录投影已经确认
            连接失败（或读超时）时，才把它从对话的默认候选中排除；后台下一次
            成功刷新会自然恢复 ``untested`` / ``ready`` 状态。
            """
            if str(connection or "") != "custom_endpoint":
                return ""
            if str(catalog.get("refresh_status") or "") != "failed":
                return ""
            error_code = str(catalog.get("error_code") or "").strip()
            detail = str(catalog.get("last_error") or "").strip()
            if error_code == "timeout":
                return detail or "读取端点模型目录超时"
            if (
                error_code == "catalog_request_failed"
                and not detail.upper().startswith("HTTP ")
            ):
                return detail or "无法连接端点"
            return ""

        for account in self.list():
            account_id = str(account.get("account_id") or "")
            if not account_id:
                continue
            stable_id = account_credential_id(account_id)
            engine = str(
                account.get("worker_engine") or account.get("engine") or ""
            ).strip().lower()
            if engine not in SUPPORTED_CREDENTIAL_ENGINES:
                continue
            present = bool(account.get("present"))
            last_test = self.last_test(stable_id, backend=environment, runtime_instance=str((catalogs.get(stable_id) or {}).get("runtime_instance") or "default"), model=str(account.get("default_model") or "") or None)
            status = "untested" if present else "missing"
            if last_test is not None:
                status = "ready" if last_test.get("ok") else "failed"
            account_usage = list(usage.get(stable_id) or [])
            candidate_models = [
                str(item).strip()
                for item in account.get("models") or []
                if str(item).strip()
            ]
            for model in catalog_models(stable_id):
                if model not in candidate_models:
                    candidate_models.append(model)
            catalog = dict(catalogs.get(stable_id) or {})
            unavailable_detail = endpoint_unavailable(
                str(account.get("connection") or ""), catalog)
            if unavailable_detail:
                # 这是非破坏性的投影状态：保留账号、模型和已通过的测试记录，
                # 只让聊天的默认候选跳过当前无法建立连接的端点。
                status = "unavailable"
            verified_model = str(
                (last_test or {}).get("model") or "").strip()
            models = [
                str(model).strip()
                for model in catalog.get("verified_models") or []
                if str(model).strip()
            ]
            if last_test and last_test.get("ok") and verified_model and verified_model not in models:
                models.append(verified_model)
            default_model = str(account.get("default_model") or "").strip()
            row = {
                "id": stable_id,
                "revision": account.get("revision", ""),
                "label": account_id,
                "engine": engine,
                "source": "stored",
                "account_id": account_id,
                "connection": str(account.get("connection") or "official"),
                "credential_format": str(
                    account.get("credential_format") or "unknown"),
                "provider": str(account.get("provider") or ""),
                "base_url": str(account.get("base_url") or ""),
                "present": present,
                "status": status,
                "models": models,
                "candidate_models": candidate_models,
                "default_model": default_model,
                "model_catalog": catalog,
                "usage": account_usage,
            }
            if unavailable_detail:
                row["status_detail"] = unavailable_detail
            if last_test is not None:
                row["last_test"] = last_test
            rows.append(row)

        for engine in engines:
            if engine not in SUPPORTED_CREDENTIAL_ENGINES:
                continue
            stable_id = system_credential_id(engine)
            detected = detect_system_login(engine)
            present = detected == "present"
            last_test = self.last_test(stable_id, backend=environment, runtime_instance=str((catalogs.get(stable_id) or {}).get("runtime_instance") or "default"))
            status = (
                "untested" if present else "missing" if detected == "absent"
                else "unknown"
            )
            if last_test is not None:
                status = "ready" if last_test.get("ok") else "failed"
            if detected == "disabled":
                status = "unavailable"
            system_usage = list(usage.get(stable_id) or [])
            catalog = dict(catalogs.get(stable_id) or {})
            candidate_models = catalog_models(stable_id)
            if engine == "devin" and present:
                from muteki.external_agents.devin import devin_model_ids

                candidate_models = devin_model_ids()
            verified_model = str(
                (last_test or {}).get("model") or "").strip()
            models = [
                str(model).strip()
                for model in catalog.get("verified_models") or []
                if str(model).strip()
            ]
            if last_test and last_test.get("ok") and verified_model and verified_model not in models:
                models.append(verified_model)
            row = {
                "id": stable_id,
                "label": f"{engine} 系统登录",
                "engine": engine,
                "source": "system",
                "connection": "official",
                "credential_format": "host_login",
                "provider": "",
                "base_url": "",
                "present": present,
                "status": status,
                "models": models,
                "candidate_models": candidate_models,
                "default_model": str(catalog.get("default_model") or ""),
                "model_catalog": catalog,
                "usage": system_usage,
            }
            if detected == "disabled":
                row["status_detail"] = "此服务未提供宿主登录发现"
                row["discovery_code"] = "host_discovery_disabled"
            if last_test is not None:
                row["last_test"] = last_test
            rows.append(row)
        return rows

    def inspect(self, account_id: str) -> CredentialAccount | None:
        if not valid_account_id(account_id):
            return None
        base = self.root / account_id
        if not base.exists() or not base.is_dir():
            return None
        updated = self._updated_at(base)
        if (base / "CLAUDE_CODE_OAUTH_TOKEN").exists():
            return CredentialAccount(
                account_id=account_id,
                engine="claude",
                mode="subscription_token",
                present=True,
                writable_state=False,
                updated_at=updated,
                details={"token_file": True, "secret_value": self._read_secret_value(base)},
            )
        if (base / "codex-home" / "auth.json").exists():
            return CredentialAccount(
                account_id=account_id,
                engine="codex",
                mode="chatgpt_auth_home",
                present=True,
                writable_state=True,
                updated_at=updated,
                details={
                    "codex_home": True,
                    "mutable_auth_home": True,
                    "secret_value": self._read_secret_value(base),
                    "provider": self._read_provider(base),
                },
            )
        if (base / "CURSOR_API_KEY").exists():
            return CredentialAccount(
                account_id=account_id,
                engine="cursor",
                mode="api_key",
                present=True,
                writable_state=False,
                updated_at=updated,
                details={"api_key_file": True, "secret_value": self._read_secret_value(base)},
            )
        if (base / "kimi-home" / "credentials" / "kimi-code.json").exists():
            return CredentialAccount(
                account_id=account_id,
                engine="kimi",
                mode="login_home",
                present=True,
                writable_state=True,
                updated_at=updated,
                details={
                    "kimi_home": True,
                    "mutable_auth_home": True,
                    "provider": self._read_provider(base),
                },
            )
        if (base / "grok-home" / "auth.json").exists():
            return CredentialAccount(
                account_id=account_id,
                engine="grok",
                mode="login_home",
                present=True,
                writable_state=True,
                updated_at=updated,
                details={
                    "grok_home": True,
                    "mutable_auth_home": True,
                    "provider": self._read_provider(base),
                },
            )
        if (base / "API_KEY").exists():
            # A custom endpoint (API_KEY + BASE_URL) is engine-agnostic on disk —
            # runtime_env_for_engine keys off the ENGINE passed in, not the account.
            # The optional ENGINE marker records which agent the operator registered
            # it FOR, so the panel can bind/display it as one of the Worker engines
            # instead of an orphan "api". No marker → legacy/programmatic "api".
            target = self._read_target_engine(base)
            base_url = self._read_base_url(base)
            return CredentialAccount(
                account_id=account_id,
                engine=target or "api",
                mode="custom_endpoint" if base_url else "api_key",
                present=True,
                writable_state=False,
                updated_at=updated,
                details={
                    "api_key_file": True,
                    "base_url": bool(base_url),
                    # base_url is non-sensitive config. secret_value remains an
                    # internal launch-time field and is stripped by _public().
                    "base_url_value": base_url,
                    "secret_value": self._read_secret_value(base),
                    "custom_endpoint": True,
                    "target_engine": target or None,
                    "provider": self._read_provider(base),
                },
            )
        return CredentialAccount(
            account_id=account_id,
            engine="unknown",
            mode="empty",
            present=False,
            writable_state=False,
            updated_at=updated,
            details={},
        )

    def revision(self, account_id: str) -> str:
        base = self.root / account_id
        marker = base / "REVISION"
        if marker.exists():
            return marker.read_text(encoding="utf-8").strip()
        # Legacy records get a metadata revision without reading secret bytes.
        return str(self._updated_at(base) or "")

    def upsert_secret(self, *, account_id: str, engine: str,
                      create_only: bool = False, expected_revision: str | None = None,
                      **fields: Any) -> dict[str, Any]:
        return self._stage_account_update(account_id,
            lambda staged: staged._write_secret(account_id=account_id, engine=engine, **fields),
            create_only=create_only, expected_revision=expected_revision)

    def _stage_account_update(self, account_id: str, writer: Any, *,
                              create_only: bool = False, expected_revision: str | None = None) -> dict[str, Any]:
        """Stage the complete replacement before touching a working account."""
        if not valid_account_id(account_id):
            raise ValueError("invalid account id")
        with self._account_guard():
            live = self.root / account_id
            if create_only and live.exists():
                raise FileExistsError("credential.account.already_exists")
            if expected_revision is not None and self.revision(account_id) != expected_revision:
                raise FileExistsError("credential.account.revision_conflict")
            staging_root = Path(tempfile.mkdtemp(prefix=".credential-stage-", dir=self.root.parent))
            backup = staging_root / "previous"
            committed = False
            installed = False
            try:
                journal = staging_root / "transaction.json"
                transaction = {"store_root": str(self.root.resolve()), "account_id": account_id,
                               "had_previous": live.exists(), "phase": "staging"}
                self._atomic_write(journal, json.dumps(transaction))
                staging = CredentialAccountStore(staging_root / "accounts")
                if live.exists():
                    shutil.copytree(live, staging.root / account_id)
                # Preserve injected I/O failures in synthetic validation too.
                staging._atomic_write = self._atomic_write
                written = writer(staging)
                prepared = staging.root / account_id
                self._atomic_write(prepared / "REVISION", os.urandom(16).hex())
                transaction["phase"] = "prepared"
                self._atomic_write(journal, json.dumps(transaction))
                if live.exists():
                    live.rename(backup)
                try:
                    prepared.rename(live)
                    installed = True
                except BaseException:
                    if backup.exists():
                        backup.rename(live)
                    raise
                self.invalidate_test_status(account_credential_id(account_id))
                account = self.inspect(account_id)
                if account is None:
                    raise OSError("credential.account.commit_unreadable")
                transaction["phase"] = "committed"
                self._atomic_write(journal, json.dumps(transaction))
                committed = True
                result = self._public(account)
                if isinstance(written, dict) and written.get("suggested_model"):
                    result["suggested_model"] = written["suggested_model"]
                return result
            except BaseException:
                if backup.exists():
                    if live.exists():
                        shutil.rmtree(live)
                    backup.rename(live)
                elif installed and live.exists():
                    shutil.rmtree(live)
                raise
            finally:
                if committed or not backup.exists():
                    shutil.rmtree(staging_root, ignore_errors=True)

    def _write_secret(
        self,
        *,
        account_id: str,
        engine: str,
        secret: str | None = None,
        codex_auth_json: str | None = None,
        base_url: str | None = None,
        target_engine: str | None = None,
        provider: str | None = None,
        target_model: str | None = None,
        models: Any = None,
        clear_base_url: bool = False,
    ) -> dict[str, Any]:
        account_id = account_id.strip()
        engine = engine.strip().lower()
        if not valid_account_id(account_id):
            raise ValueError("account_id must be 1-64 chars: letters, digits, _, ., -")
        if canonical_engine_id(engine) == "dsh":
            raise EngineTemporarilyUnsupportedError("dsh")
        if engine not in {*SUPPORTED_CREDENTIAL_ENGINES, "api"}:
            raise ValueError(
                "engine must be claude, codex, cursor, pi, omp, kimi, grok, "
                "opencode, or api")

        # EDIT support: secrets are never read back to the UI, so an operator who
        # only wants to change an endpoint's base_url / target_engine cannot
        # re-supply the key. When the incoming secret is blank AND a matching
        # account already exists on disk, fall back to the stored secret so the
        # edit preserves it. _replace_account wipes the dir, so snapshot first.
        prior = self._snapshot_material(account_id)

        requested_models: list[str] = []
        raw_models = models if isinstance(models, (list, tuple)) else []
        if target_model:
            raw_models = [target_model, *raw_models]
        for item in raw_models:
            value = str(item or "").strip()
            if value and value not in requested_models:
                requested_models.append(value)
        if not requested_models:
            try:
                prior_models = json.loads(prior.get("MODELS.json") or "[]")
            except json.JSONDecodeError:
                prior_models = []
            requested_models = [
                str(item).strip() for item in prior_models
                if str(item).strip()
            ] if isinstance(prior_models, list) else []

        existing = self.inspect(account_id)
        supplied_secret = bool(
            str(secret or "").strip() or str(codex_auth_json or "").strip())
        if (
            existing is not None
            and existing.mode in {"chatgpt_auth_home", "login_home"}
            and existing.engine == engine
            and not supplied_secret
        ):
            # Imported login homes contain refresh state beyond the one file used
            # for presence detection. A metadata-only edit must never reconstruct
            # the directory or discard any byte maintained by the CLI.
            base = self.root / account_id
            if requested_models:
                self._atomic_write(
                    base / "MODELS.json",
                    json.dumps(requested_models, ensure_ascii=False) + "\n",
                )
            if target_model is not None:
                selected_default = str(target_model or "").strip()
                if selected_default:
                    self._atomic_write(base / "DEFAULT_MODEL", selected_default + "\n")
                else:
                    try:
                        (base / "DEFAULT_MODEL").unlink()
                    except FileNotFoundError:
                        pass
            provider_name = str(provider or "").strip()
            if provider_name:
                self._atomic_write(base / "PROVIDER", provider_name + "\n")
            refreshed = self.inspect(account_id)
            assert refreshed is not None
            self.invalidate_test_status(account_credential_id(account_id))
            return self._public(refreshed)

        if engine == "claude":
            value = str(secret or "").strip() or prior.get("CLAUDE_CODE_OAUTH_TOKEN", "")
            if not value:
                raise ValueError("CLAUDE_CODE_OAUTH_TOKEN is required")
            base = self._replace_account(account_id)
            self._atomic_write(base / "CLAUDE_CODE_OAUTH_TOKEN", value + "\n")
        elif engine == "cursor":
            value = str(secret or "").strip() or prior.get("CURSOR_API_KEY", "")
            if not value:
                raise ValueError("CURSOR_API_KEY is required")
            base = self._replace_account(account_id)
            self._atomic_write(base / "CURSOR_API_KEY", value + "\n")
        elif engine in {"api", "pi", "omp", "kimi", "grok", "opencode"}:
            value = str(secret or "").strip() or prior.get("API_KEY", "")
            if not value:
                raise ValueError("API_KEY is required")
            # base_url / target_engine: a blank field on edit keeps the stored
            # value (the UI sends "" when the operator didn't touch it). An
            # explicit clear isn't expressible here, and isn't needed by the panel.
            b = "" if clear_base_url else (
                str(base_url or "").strip() or prior.get("BASE_URL", "")
            )
            if engine == "api":
                te = str(target_engine or "").strip().lower() or prior.get("ENGINE", "")
            else:
                # A direct API account is bound to its own engine by definition; the
                # ENGINE marker keeps inspect() able to bind/display it.
                te = engine
            if canonical_engine_id(te) == "dsh":
                raise EngineTemporarilyUnsupportedError("dsh")
            if te and te not in SUPPORTED_CREDENTIAL_ENGINES:
                raise ValueError(
                    "target_engine must be claude, codex, cursor, pi, omp, kimi, "
                    "grok, or opencode")
            base = self._replace_account(account_id)
            self._atomic_write(base / "API_KEY", value + "\n")
            if b:
                self._atomic_write(base / "BASE_URL", b + "\n")
            # Record which agent this endpoint is FOR so the panel can bind/display
            # it. The runtime injection stays engine-agnostic (it reads API_KEY/
            # BASE_URL regardless of this marker).
            if te:
                self._atomic_write(base / "ENGINE", te + "\n")
            provider_name = str(provider or "").strip() or prior.get("PROVIDER", "")
            if provider_name:
                self._atomic_write(base / "PROVIDER", provider_name + "\n")
        else:
            value = str(codex_auth_json or secret or "").strip() or prior.get("codex_auth_json", "")
            if not value:
                raise ValueError("codex auth.json content is required")
            # Ensure it is at least syntactically JSON before persisting.
            json.loads(value)
            base = self._replace_account(account_id)
            codex_home = base / "codex-home"
            codex_home.mkdir(parents=True, exist_ok=True)
            self._chmod_private_dir(codex_home)
            self._atomic_write(codex_home / "auth.json", value + "\n")

        if requested_models:
            self._atomic_write(
                base / "MODELS.json",
                json.dumps(requested_models, ensure_ascii=False) + "\n",
            )
        if target_model is not None:
            selected_default = str(target_model or "").strip()
            if selected_default:
                self._atomic_write(base / "DEFAULT_MODEL", selected_default + "\n")
            else:
                try:
                    (base / "DEFAULT_MODEL").unlink()
                except FileNotFoundError:
                    pass

        acct = self.inspect(account_id)
        assert acct is not None
        self.invalidate_test_status(account_credential_id(account_id))
        return self._public(acct)

    def import_host_codex_auth(self, account_id: str) -> dict[str, Any]:
        return self._stage_account_update(account_id, lambda staged: staged._write_host_codex_auth(account_id))

    def _write_host_codex_auth(self, account_id: str) -> dict[str, Any]:
        """Refresh a codex account from the HOST's ~/.codex/{auth.json,config.toml}.

        `codex login` refreshes the host's ~/.codex/auth.json, but container
        workers mount the account-store COPY — so a fresh host login never reaches
        the account until it's re-imported. This reads the host file and upserts it
        (one click from the settings page). Only meaningful on a bare host where
        ~/.codex belongs to the operator; the caller guards on is_web_container().

        Also copies ``config.toml`` when present. Custom providers (e.g. Ark with
        ``model_provider`` + ``[model_providers.*]``) live only in that file; auth
        alone leaves Codex on the default ``api.openai.com`` Responses endpoint and
        yields HTTP 401 when the key is non-OpenAI.

        Raises ValueError with an actionable message if the host auth file is
        missing or invalid (the route maps it to a 400/404).
        """
        host_codex = Path.home() / ".codex"
        host_auth = host_codex / "auth.json"
        if not host_auth.exists():
            raise ValueError(
                f"host ~/.codex/auth.json not found ({host_auth}) — run `codex login` first"
            )
        try:
            content = host_auth.read_text(encoding="utf-8")
        except OSError as exc:
            raise ValueError(f"could not read {host_auth}: {exc}") from exc
        # upsert_secret validates it's JSON, replaces the account dir, and writes
        # codex-home/auth.json — so config.toml must be copied *after* upsert.
        result = self.upsert_secret(
            account_id=account_id, engine="codex", codex_auth_json=content
        )
        host_config = host_codex / "config.toml"
        if host_config.is_file():
            dest_home = self.root / account_id / "codex-home"
            dest_home.mkdir(parents=True, exist_ok=True)
            self._chmod_private_dir(dest_home)
            dest = dest_home / "config.toml"
            shutil.copy2(host_config, dest)
            try:
                dest.chmod(0o600)
            except OSError:
                pass
        return result

    def import_host_login(self, account_id: str, engine: str) -> dict[str, Any]:
        return self._stage_account_update(account_id, lambda staged: staged._write_host_login(account_id, engine))

    def _write_host_login(self, account_id: str, engine: str) -> dict[str, Any]:
        """Import the minimal host login state used by Claude, Kimi, or Grok."""
        engine = str(engine or "").strip().lower()
        if engine not in {"claude", "kimi", "grok"}:
            raise ValueError("host login import supports claude, kimi, or grok")
        if not valid_account_id(account_id):
            raise ValueError("account_id must be 1-64 chars: letters, digits, _, ., -")

        if engine == "claude":
            import json

            settings_path = Path.home() / ".claude" / "settings.json"
            try:
                settings = json.loads(settings_path.read_text(encoding="utf-8"))
            except FileNotFoundError as exc:
                raise ValueError(
                    f"host Claude settings not found ({settings_path}); "
                    "run `claude setup-token` and paste the token instead"
                ) from exc
            except (OSError, json.JSONDecodeError) as exc:
                raise ValueError(f"could not read host Claude settings: {exc}") from exc
            settings_env = settings.get("env") if isinstance(settings, dict) else None
            settings_env = settings_env if isinstance(settings_env, dict) else {}
            bearer = str(settings_env.get("ANTHROPIC_AUTH_TOKEN") or "").strip()
            api_key = str(settings_env.get("ANTHROPIC_API_KEY") or "").strip()
            base_url = str(settings_env.get("ANTHROPIC_BASE_URL") or "").strip()
            secret = bearer or api_key
            if not secret or not base_url:
                raise ValueError(
                    "host Claude settings do not contain both ANTHROPIC_AUTH_TOKEN/"
                    "ANTHROPIC_API_KEY and ANTHROPIC_BASE_URL; run `claude setup-token` "
                    "and paste the token for an official account"
                )
            suggested_model = str(
                settings_env.get("ANTHROPIC_MODEL")
                or settings_env.get("ANTHROPIC_DEFAULT_SONNET_MODEL")
                or ""
            ).strip()
            account = self.upsert_secret(
                account_id=account_id,
                engine="api",
                secret=secret,
                base_url=base_url,
                target_engine="claude",
                provider="宿主 Claude 配置",
                target_model=suggested_model or None,
            )
            if suggested_model:
                account["suggested_model"] = suggested_model
            return account

        if engine == "kimi":
            source_root = Path.home() / ".kimi-code"
            required = source_root / "credentials" / "kimi-code.json"
            if not required.exists():
                raise ValueError(
                    f"host Kimi login not found ({required}) — run `kimi` and complete /login first"
                )
            target_name = "kimi-home"
            files = (
                Path("config.toml"),
                Path("device_id"),
                Path("credentials/kimi-code.json"),
                Path("oauth/kimi-code"),
            )
        else:
            source_root = Path.home() / ".grok"
            required = source_root / "auth.json"
            if not required.exists():
                raise ValueError(
                    f"host Grok login not found ({required}) — run `grok login` first"
                )
            target_name = "grok-home"
            files = (Path("auth.json"), Path("config.toml"))

        base = self._replace_account(account_id)
        target_root = base / target_name
        target_root.mkdir(parents=True, exist_ok=True)
        self._chmod_private_dir(target_root)
        for relative in files:
            source = source_root / relative
            if not source.exists() or not source.is_file():
                continue
            target = target_root / relative
            target.parent.mkdir(parents=True, exist_ok=True)
            self._chmod_private_dir(target.parent)
            shutil.copy2(source, target)
            try:
                target.chmod(0o600)
            except OSError:
                pass

        acct = self.inspect(account_id)
        if acct is None or not acct.present:
            raise ValueError(f"imported {engine} login is incomplete")
        return self._public(acct)

    def _replace_account(self, account_id: str) -> Path:
        base = self.root / account_id
        base.mkdir(parents=True, exist_ok=True)
        self._chmod_private_dir(base)
        self._clear_account_material(base)
        return base

    @contextmanager
    def deletion_transaction(self, account_id: str):
        """Delete material first and retain a private rollback copy until references commit."""
        if not valid_account_id(account_id):
            raise ValueError("invalid account id")
        with self._account_guard():
            live = self.root / account_id
            if not live.exists():
                raise FileNotFoundError("credential.account.not_found")
            backup_root = Path(tempfile.mkdtemp(prefix=".credential-stage-", dir=self.root.parent))
            backup = backup_root / "previous"
            committed = False
            try:
                journal = backup_root / "transaction.json"
                transaction = {"store_root": str(self.root.resolve()), "account_id": account_id,
                               "had_previous": True, "phase": "prepared"}
                self._atomic_write(journal, json.dumps(transaction))
                live.rename(backup)
                yield
                transaction["phase"] = "committed"
                self._atomic_write(journal, json.dumps(transaction))
                committed = True
            except BaseException:
                if backup.exists():
                    if live.exists():
                        shutil.rmtree(live)
                    backup.rename(live)
                raise
            finally:
                if committed or not backup.exists():
                    shutil.rmtree(backup_root, ignore_errors=True)

    def delete(self, account_id: str) -> bool:
        if not valid_account_id(account_id):
            return False
        base = self.root / account_id
        if not base.exists():
            return False
        shutil.rmtree(base)
        return True

    def delete_engine_accounts(self, engine: str) -> list[str]:
        """Delete every stored account explicitly bound to ``engine``."""
        selected = str(engine or "").strip().lower()
        removed: list[str] = []
        for row in self.list():
            account_id = str(row.get("account_id") or "")
            worker_engine = str(
                row.get("worker_engine") or row.get("engine") or ""
            ).strip().lower()
            if worker_engine != selected:
                continue
            if self.delete(account_id):
                removed.append(account_id)
        statuses = self._read_test_statuses()
        ids = {f"account:{account_id}" for account_id in removed}
        ids.add(f"system:{selected}")
        changed = False
        for stable_id in list(statuses):
            if stable_id in ids:
                statuses.pop(stable_id, None)
                changed = True
        if changed:
            path = self._test_status_path
            path.parent.mkdir(parents=True, exist_ok=True)
            tmp = path.with_suffix(".json.tmp")
            tmp.write_text(
                json.dumps(statuses, ensure_ascii=False, indent=2),
                encoding="utf-8",
            )
            tmp.replace(path)
        return removed

    def clear_all_model_state(self) -> dict[str, int]:
        """Clear pre-v2 model choices/tests before credential-scoped rebuild."""
        model_files = 0
        for row in self.list():
            base = self.root / str(row.get("account_id") or "")
            for name in ("MODELS.json", "DEFAULT_MODEL"):
                try:
                    (base / name).unlink()
                    model_files += 1
                except FileNotFoundError:
                    pass
        test_records = len(self._read_test_statuses())
        try:
            self._test_status_path.unlink()
        except FileNotFoundError:
            pass
        return {"model_files": model_files, "test_records": test_records}

    def _public(self, acct: CredentialAccount) -> dict[str, Any]:
        details = dict(acct.details or {})
        # inspect() retains this value for local runtime injection. Public account
        # metadata only needs presence and format, so never send the stored secret
        # back to the browser.
        details.pop("secret_value", None)
        base_url = str(details.get("base_url_value") or "").strip()
        target_engine = str(details.get("target_engine") or "").strip().lower()
        worker_engine = target_engine or (
            acct.engine if acct.engine in {
                "claude", "codex", "cursor", "pi", "omp", "kimi", "grok",
                "opencode", "devin", "dsh"
            } else ""
        )
        connection = (
            "custom_endpoint"
            if base_url or (acct.mode == "custom_endpoint" and acct.engine == "api")
            else "official"
        )
        credential_format = {
            "subscription_token": "oauth_token",
            "chatgpt_auth_home": "auth_json",
            "login_home": "auth_home",
            "api_key": "api_key",
            "custom_endpoint": "api_key",
        }.get(acct.mode, "unknown")
        return {
            "account_id": acct.account_id,
            "revision": self.revision(acct.account_id),
            "engine": acct.engine,
            "worker_engine": worker_engine,
            "connection": connection,
            "base_url": base_url,
            "credential_format": credential_format,
            "provider": str(details.get("provider") or "").strip(),
            "models": self._read_models(self.root / acct.account_id),
            "default_model": self._read_default_model(self.root / acct.account_id),
            "mode": acct.mode,
            "present": acct.present,
            "writable_state": acct.writable_state,
            "updated_at": acct.updated_at,
            "details": details,
        }

    @staticmethod
    def _read_target_engine(base: Path) -> str:
        """The agent a custom endpoint was registered for (ENGINE marker), or ""."""
        mp = base / "ENGINE"
        if not mp.exists():
            return ""
        try:
            marker = mp.read_text(encoding="utf-8").strip().lower()
        except OSError:
            return ""
        return marker if marker in {
            "claude", "codex", "cursor", "pi", "omp", "kimi", "grok",
            "opencode", "devin", "dsh"
        } else ""

    @staticmethod
    def _read_base_url(base: Path) -> str:
        """The custom endpoint's BASE_URL value, or "" if unset/unreadable.

        Non-sensitive (a public host) — safe to surface so the UI can display and
        edit it.
        """
        p = base / "BASE_URL"
        if not p.exists():
            return ""
        try:
            return p.read_text(encoding="utf-8").strip()
        except OSError:
            return ""

    @staticmethod
    def _read_provider(base: Path) -> str:
        p = base / "PROVIDER"
        if not p.exists():
            return ""
        try:
            return p.read_text(encoding="utf-8").strip()
        except OSError:
            return ""

    @staticmethod
    def _read_models(base: Path) -> list[str]:
        path = base / "MODELS.json"
        try:
            raw = json.loads(path.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError):
            return []
        if not isinstance(raw, list):
            return []
        out: list[str] = []
        for item in raw:
            model = str(item or "").strip()
            if model and model not in out:
                out.append(model)
        return out

    @staticmethod
    def _read_default_model(base: Path) -> str:
        try:
            return (base / "DEFAULT_MODEL").read_text(encoding="utf-8").strip()
        except OSError:
            return ""

    @staticmethod
    def _read_secret_value(base: Path) -> str:
        """The account's stored SECRET in plaintext, or "" if absent/unreadable.

        This helper is restricted to internal launch/update resolution. Public
        account projections always remove ``details.secret_value`` in _public(),
        so API responses never return OAuth tokens, API keys, or auth.json bytes.
        """
        for rel in ("CLAUDE_CODE_OAUTH_TOKEN", "CURSOR_API_KEY", "API_KEY"):
            p = base / rel
            if p.exists():
                try:
                    return p.read_text(encoding="utf-8").strip()
                except OSError:
                    return ""
        codex_auth = base / "codex-home" / "auth.json"
        if codex_auth.exists():
            try:
                return codex_auth.read_text(encoding="utf-8").strip()
            except OSError:
                return ""
        return ""

    @staticmethod
    def _updated_at(path: Path) -> float | None:
        try:
            newest = path.stat().st_mtime
            for p in path.rglob("*"):
                try:
                    newest = max(newest, p.stat().st_mtime)
                except OSError:
                    pass
            return newest
        except OSError:
            return None

    @staticmethod
    def _chmod_private_dir(path: Path) -> None:
        try:
            path.chmod(0o700)
        except OSError:
            pass

    @staticmethod
    def _atomic_write(path: Path, text: str) -> None:
        path.parent.mkdir(parents=True, exist_ok=True)
        tmp = path.with_name(f".{path.name}.{int(time.time() * 1000)}.{os.getpid()}.{os.urandom(4).hex()}.tmp")
        with tmp.open("w", encoding="utf-8") as handle:
            handle.write(text)
            handle.flush()
            os.fsync(handle.fileno())
        try:
            tmp.chmod(0o600)
        except OSError:
            pass
        tmp.replace(path)
        try:
            path.chmod(0o600)
        except OSError:
            pass

    def _snapshot_material(self, account_id: str) -> dict[str, str]:
        """Read an existing account's stored secrets/markers before a rewrite.

        Returns a dict keyed by the on-disk filename (plus the synthetic key
        ``codex_auth_json``) holding the trimmed prior values, or empty strings
        for anything absent. Used so a metadata-only edit (blank secret) can fall
        back to the stored credential instead of erroring or wiping it. Never
        raises — a fresh/unreadable account simply yields blanks.
        """
        base = self.root / account_id
        out: dict[str, str] = {}
        for rel in ("CLAUDE_CODE_OAUTH_TOKEN", "CURSOR_API_KEY", "API_KEY", "BASE_URL", "ENGINE", "PROVIDER", "MODELS.json"):
            p = base / rel
            try:
                out[rel] = p.read_text(encoding="utf-8").strip() if p.exists() else ""
            except OSError:
                out[rel] = ""
        codex_auth = base / "codex-home" / "auth.json"
        try:
            out["codex_auth_json"] = (
                codex_auth.read_text(encoding="utf-8").strip() if codex_auth.exists() else ""
            )
        except OSError:
            out["codex_auth_json"] = ""
        return out

    @staticmethod
    def _clear_account_material(base: Path) -> None:
        for rel in ("CLAUDE_CODE_OAUTH_TOKEN", "CURSOR_API_KEY", "API_KEY", "BASE_URL", "ENGINE", "PROVIDER", "MODELS.json"):
            try:
                (base / rel).unlink(missing_ok=True)
            except OSError:
                pass
        codex_home = base / "codex-home"
        if codex_home.exists():
            shutil.rmtree(codex_home, ignore_errors=True)
        # generated pi/omp provider configs (rewritten on next env resolution).
        for rel in ("pi-agent", "omp-agent", "kimi-home", "grok-home"):
            agent_dir = base / rel
            if agent_dir.exists():
                shutil.rmtree(agent_dir, ignore_errors=True)


def runtime_env_for_engine(
    engine: str,
    *,
    account_root: str | Path | None = None,
    account_id: str | None = None,
    container: bool = False,
    env: Mapping[str, str] | None = None,
    agent_state_dir: str | Path | None = None,
    agent_state_container_path: str | None = None,
    model: str | None = None,
) -> RuntimeCredentialEnv:
    """Resolve credential env for one engine.

    Container mode avoids sending secret values through `docker exec -e` when a
    file-backed account exists: it passes only `*_FILE` paths and lets the
    container shell export the real value inside the process. Local mode reads
    those files into the subprocess env because there is no container wrapper.
    """

    e = (engine or "").strip().lower()
    # env={} means "resolve with no ambient env" (explicit empty mapping), NOT
    # "fall back to the host env" — the old `env or os.environ` read host
    # secrets into resolutions that asked for an empty environment.
    source = env if env is not None else os.environ
    if account_id is None:
        account_id = engine_account_id(e, source)
    elif account_id != "" and not valid_account_id(account_id):
        account_id = engine_account_id(e, source)
    if not account_id and not container and not host_discovery_enabled(env):
        raise HostDiscoveryDisabledError("此服务未提供宿主登录发现；请选择已登记凭据")
    root = Path(account_root).expanduser().resolve() if account_root is not None else None
    base = root / account_id if root is not None and account_id else None
    out: dict[str, str] = {}

    if e == "claude":
        if base is not None and (base / "API_KEY").exists():
            _add_secret_file_or_env(
                out,
                base=base,
                filename="API_KEY",
                env_name="ANTHROPIC_API_KEY",
                container=container,
                container_path=_container_secret_path(account_id, "API_KEY"),
                source=source,
            )
            _add_secret_file_or_env(
                out,
                base=base,
                filename="API_KEY",
                env_name="ANTHROPIC_AUTH_TOKEN",
                container=container,
                container_path=_container_secret_path(account_id, "API_KEY"),
                source=source,
            )
            _add_base_url(out, base=base, env_name="ANTHROPIC_BASE_URL")
        else:
            _add_secret_file_or_env(
                out,
                base=base,
                filename="CLAUDE_CODE_OAUTH_TOKEN",
                env_name="CLAUDE_CODE_OAUTH_TOKEN",
                container=container,
                container_path=_container_secret_path(account_id, "CLAUDE_CODE_OAUTH_TOKEN"),
                source=source,
            )
    elif e == "codex":
        if base is not None and (base / "API_KEY").exists():
            _add_secret_file_or_env(
                out,
                base=base,
                filename="API_KEY",
                env_name="OPENAI_API_KEY",
                container=container,
                container_path=_container_secret_path(account_id, "API_KEY"),
                source=source,
            )
            _add_base_url(out, base=base, env_name="OPENAI_BASE_URL")
        codex_home = base / "codex-home" if base is not None else None
        if "OPENAI_API_KEY" not in out and "OPENAI_API_KEY_FILE" not in out and codex_home is not None and codex_home.exists():
            out["CODEX_HOME"] = (
                f"{CONTAINER_ACCOUNTS_ROOT}/{account_id}/codex-home"
                if container else str(codex_home.resolve())
            )
        elif source.get("CODEX_HOME"):
            out["CODEX_HOME"] = str(source["CODEX_HOME"])
    elif e == "cursor":
        if base is not None and (base / "API_KEY").exists():
            _add_secret_file_or_env(
                out,
                base=base,
                filename="API_KEY",
                env_name="CURSOR_API_KEY",
                container=container,
                container_path=_container_secret_path(account_id, "API_KEY"),
                source=source,
            )
            _add_base_url(out, base=base, env_name="CURSOR_ENDPOINT")
        else:
            _add_secret_file_or_env(
                out,
                base=base,
                filename="CURSOR_API_KEY",
                env_name="CURSOR_API_KEY",
                container=container,
                container_path=_container_secret_path(account_id, "CURSOR_API_KEY"),
                source=source,
            )
    elif e == "kimi":
        has_key = base is not None and (base / "API_KEY").exists()
        if has_key:
            _add_secret_file_or_env(
                out,
                base=base,
                filename="API_KEY",
                env_name="KIMI_MODEL_API_KEY",
                container=container,
                container_path=_container_secret_path(account_id, "API_KEY"),
                source=source,
            )
            _add_base_url(out, base=base, env_name="KIMI_MODEL_BASE_URL")
            selected_model = str(
                model or source.get("KIMI_MODEL_NAME") or "kimi-for-coding"
            ).strip()
            # OAuth profiles use configured aliases such as
            # ``kimi-code/kimi-for-coding``. KIMI_MODEL_NAME is the literal model
            # id sent to the API, so remove only the built-in alias namespace.
            out["KIMI_MODEL_NAME"] = selected_model.removeprefix("kimi-code/")
            base_url = CredentialAccountStore._read_base_url(base)
            provider_type = str(
                source.get("KIMI_MODEL_PROVIDER_TYPE")
                or ("openai" if base_url else "kimi")
            )
            out["KIMI_MODEL_PROVIDER_TYPE"] = provider_type
            if provider_type == "openai":
                # Kimi Code synthesizes custom OpenAI-compatible models with a
                # 256K context window unless told otherwise.  Its derived output
                # request can then exceed the 64K completion ceiling used by
                # common DeepSeek endpoints.  The documented runtime override
                # keeps custom providers at the conventional 128K window while
                # still allowing an explicit deployment value to win.
                out["KIMI_MODEL_MAX_CONTEXT_SIZE"] = str(
                    source.get("KIMI_MODEL_MAX_CONTEXT_SIZE") or 131072
                )
                out["KIMI_MODEL_MAX_OUTPUT_SIZE"] = str(
                    source.get("KIMI_MODEL_MAX_OUTPUT_SIZE") or 65536
                )
        kimi_home = base / "kimi-home" if base is not None else None
        if not has_key and kimi_home is not None and kimi_home.exists():
            out["KIMI_CODE_HOME"] = (
                f"{CONTAINER_ACCOUNTS_ROOT}/{account_id}/kimi-home"
                if container else str(kimi_home.resolve())
            )
        elif not has_key and source.get("KIMI_CODE_HOME"):
            out["KIMI_CODE_HOME"] = str(source["KIMI_CODE_HOME"])
    elif e == "grok":
        has_key = base is not None and (base / "API_KEY").exists()
        if has_key:
            _add_secret_file_or_env(
                out,
                base=base,
                filename="API_KEY",
                env_name="XAI_API_KEY",
                container=container,
                container_path=_container_secret_path(account_id, "API_KEY"),
                source=source,
            )
            _add_base_url(out, base=base, env_name="GROK_MODELS_BASE_URL")
        grok_home = base / "grok-home" if base is not None else None
        if not has_key and grok_home is not None and grok_home.exists():
            out["GROK_HOME"] = (
                f"{CONTAINER_ACCOUNTS_ROOT}/{account_id}/grok-home"
                if container else str(grok_home.resolve())
            )
        elif not has_key and source.get("GROK_HOME"):
            out["GROK_HOME"] = str(source["GROK_HOME"])
    elif e == "opencode":
        has_key = base is not None and (base / "API_KEY").exists()
        if has_key:
            _add_secret_file_or_env(
                out,
                base=base,
                filename="API_KEY",
                env_name="OPENAI_API_KEY",
                container=container,
                container_path=_container_secret_path(account_id, "API_KEY"),
                source=source,
            )
            _add_secret_file_or_env(
                out,
                base=base,
                filename="API_KEY",
                env_name="OPENCODE_API_KEY",
                container=container,
                container_path=_container_secret_path(account_id, "API_KEY"),
                source=source,
            )
            _add_base_url(out, base=base, env_name="OPENAI_BASE_URL")
            base_url = CredentialAccountStore._read_base_url(base)
            selected_model = str(model or "").strip()
            # 候选目录只供显式模型测试。运行时配置只包含本轮已经由调用方
            # 校验并选择的模型，避免手写坏 ID 被注入 OpenCode provider。
            configured_models = [selected_model] if selected_model else []
            if base_url and configured_models:
                # OpenCode requires custom OpenAI-compatible endpoints to be
                # declared as a provider with explicit models.  The inline
                # config contains only public endpoint/model metadata; the API
                # key is resolved by OpenCode from the subprocess environment.
                provider_id = "muteki"
                provider_name = (
                    CredentialAccountStore._read_provider(base)
                    or "Muteki 凭据中心"
                )
                out["OPENCODE_CONFIG_CONTENT"] = json.dumps({
                    "$schema": "https://opencode.ai/config.json",
                    "provider": {
                        provider_id: {
                            "npm": "@ai-sdk/openai-compatible",
                            "name": provider_name,
                            "options": {
                                "baseURL": base_url,
                                "apiKey": "{env:OPENCODE_API_KEY}",
                            },
                            "models": {
                                model_id: {"name": model_id}
                                for model_id in configured_models
                            },
                        },
                    },
                }, ensure_ascii=False, separators=(",", ":"))
                # The selected credential owns the provider namespace.  Model
                # ids may themselves contain '/', so the adapter must not split
                # them as OpenCode's provider/model shorthand.
                out["MUTEKI_OPENCODE_PROVIDER"] = provider_id
        elif source.get("OPENCODE_API_KEY"):
            out["OPENCODE_API_KEY"] = str(source["OPENCODE_API_KEY"])
        if agent_state_dir is not None:
            state_root = Path(agent_state_dir).expanduser().resolve()
            for dirname in ("data", "config", "cache"):
                (state_root / dirname).mkdir(parents=True, exist_ok=True)
            runtime_root = (
                str(agent_state_container_path)
                if container and agent_state_container_path else str(state_root)
            )
            out.update({
                "XDG_DATA_HOME": f"{runtime_root}/data",
                "XDG_CONFIG_HOME": f"{runtime_root}/config",
                "XDG_CACHE_HOME": f"{runtime_root}/cache",
            })
    elif e in ("pi", "omp"):
        # pi/omp reach a custom OpenAI-compatible provider through a generated
        # provider config in their agent config dir (pi: models.json; omp:
        # models.yml — hand-written YAML, no pyyaml dependency), selected via
        # PI_CODING_AGENT_DIR (both CLIs honor it). The account's API_KEY is
        # embedded in that file, so the dir is projected as a WRITABLE state dir
        # (same posture as codex-home). Pi 的 provider 配置已经包含凭据，
        # 因而不把 OPENAI_API_KEY 传给 Pi 进程及其 shell 子进程；OMP 仍需
        # 原生环境变量兼容其端点选择。
        agent_dirname = "pi-agent" if e == "pi" else "omp-agent"
        provider_env = "MUTEKI_PI_PROVIDER" if e == "pi" else "MUTEKI_OMP_PROVIDER"
        model_env = "MUTEKI_PI_MODEL" if e == "pi" else "MUTEKI_OMP_MODEL"
        has_key = base is not None and (base / "API_KEY").exists()
        base_url = CredentialAccountStore._read_base_url(base) if has_key else ""
        if has_key and base_url:
            try:
                api_key = (base / "API_KEY").read_text(encoding="utf-8").strip()
            except OSError:
                api_key = ""
            selected_model = str(model or "").strip()
            model_file = base / "MODEL"
            if not selected_model and model_file.exists():
                try:
                    selected_model = model_file.read_text(encoding="utf-8").strip()
                except OSError:
                    selected_model = ""
            if not selected_model:
                raise ValueError(
                    f"{e} 自定义端点必须指定已经通过真实测试的模型 ID")
            agent_dir = (
                Path(agent_state_dir).expanduser().resolve()
                if agent_state_dir is not None
                else base / agent_dirname
            )
            agent_dir.mkdir(parents=True, exist_ok=True)
            CredentialAccountStore._chmod_private_dir(agent_dir)
            if e == "pi":
                config_text = json.dumps({
                    "providers": {"muteki": {
                        "baseUrl": base_url,
                        "api": "openai-completions",
                        "apiKey": api_key,
                        "models": [{
                            "id": selected_model,
                            "contextWindow": 128000,
                            "maxTokens": 8192,
                        }],
                    }},
                }, indent=2) + "\n"
                config_name = "models.json"
                # Pi RPC 由自己的 SettingsManager 决定 Provider 请求的
                # timeout，而不是 Muteki Adapter 的 prompt timeout。显式
                # 固定为 5 分钟（与 Pi 当前默认一致）并保持单次请求，使
                # 受管 profile 不受宿主全局设置变化影响；连接失败则立即
                # 交给健康检查判为不可用，不在同一轮重复提交请求。
                settings_text = json.dumps({
                    "httpIdleTimeoutMs": _PI_PROVIDER_REQUEST_TIMEOUT_MS,
                    "retry": {
                        "provider": {
                            "timeoutMs": _PI_PROVIDER_REQUEST_TIMEOUT_MS,
                            "maxRetries": _PI_PROVIDER_MAX_RETRIES,
                        },
                    },
                }, indent=2) + "\n"
                CredentialAccountStore._atomic_write(
                    agent_dir / "settings.json", settings_text)
            else:
                config_text = _omp_models_yml(
                    base_url, api_key, selected_model)
                config_name = "models.yml"
            CredentialAccountStore._atomic_write(agent_dir / config_name, config_text)
            out["PI_CODING_AGENT_DIR"] = (
                str(agent_state_container_path)
                if container and agent_state_container_path
                else f"{CONTAINER_ACCOUNTS_ROOT}/{account_id}/{agent_dirname}"
                if container else str(agent_dir.resolve())
            )
            out[provider_env] = "muteki"
            if selected_model:
                out[model_env] = selected_model
            if e == "omp":
                out["OPENAI_BASE_URL"] = base_url
        # 自建 Pi provider 从私有 models.json 读取凭据。省略该环境变量可
        # 避免 Worker 的本地工具意外将 API Key 写入命令输出或运行事件。
        needs_native_key_fallback = not (e == "pi" and has_key and base_url)
        if has_key and needs_native_key_fallback:
            _add_secret_file_or_env(
                out,
                base=base,
                filename="API_KEY",
                env_name="OPENAI_API_KEY",
                container=container,
                container_path=_container_secret_path(account_id, "API_KEY"),
                source=source,
            )

    return RuntimeCredentialEnv(account_id=account_id, env=out)


def _container_secret_path(account_id: str, filename: str) -> str:
    return f"{CONTAINER_ACCOUNTS_ROOT}/{account_id}/{filename}"


def _omp_models_yml(base_url: str, api_key: str, model: str) -> str:
    """Hand-written omp provider config (no pyyaml dependency). Values are
    JSON-double-quoted, which YAML 1.2 parses as flow scalars — safe for URLs and
    model ids containing ':' or '#'."""
    import json

    def _q(v: str) -> str:
        return json.dumps(str(v))

    return (
        "providers:\n"
        "  muteki:\n"
        f"    baseUrl: {_q(base_url)}\n"
        "    api: openai-completions\n"
        f"    apiKey: {_q(api_key)}\n"
        "    models:\n"
        f"      - id: {_q(model)}\n"
        "        contextWindow: 128000\n"
        "        maxTokens: 8192\n"
    )


def _add_secret_file_or_env(
    out: dict[str, str],
    *,
    base: Optional[Path],
    filename: str,
    env_name: str,
    container: bool,
    container_path: str,
    source: Mapping[str, str],
) -> None:
    if base is not None:
        p = base / filename
        if p.exists():
            if container:
                out[f"{env_name}_FILE"] = container_path
            else:
                try:
                    value = p.read_text(encoding="utf-8").strip()
                except OSError:
                    value = ""
                if value:
                    out[env_name] = value
            return
    if source.get(env_name):
        out[env_name] = str(source[env_name])


def _add_base_url(out: dict[str, str], *, base: Optional[Path], env_name: str) -> None:
    if base is None:
        return
    p = base / "BASE_URL"
    if not p.exists():
        return
    try:
        value = p.read_text(encoding="utf-8").strip()
    except OSError:
        value = ""
    if value:
        out[env_name] = value


# Host-env login presence is cached briefly so the settings page does not
# re-read keychain/files on every navigation. Launch paths pass fresh=True.
_HOST_LOGIN_CACHE: dict[str, tuple[float, str]] = {}
_HOST_LOGIN_CACHE_TTL_S = 30.0
_HOST_LOGIN_LOCK = threading.Lock()


def invalidate_system_login_cache(engine: str | None = None) -> None:
    """Drop cached host-login presence so the next UI refresh re-probes."""
    with _HOST_LOGIN_LOCK:
        if engine is None:
            _HOST_LOGIN_CACHE.clear()
            return
        _HOST_LOGIN_CACHE.pop((engine or "").strip().lower(), None)


def detect_system_login(
    engine: str,
    env: Mapping[str, str] | None = None,
    *,
    fresh: bool = False,
) -> str:
    """Is there a usable HOST-side login for this engine? (DESIGN §2.3 補強B)

    READ-ONLY, never raises. Returns "present" / "absent" / "unknown" / "disabled". This only
    drives the local-mode credentials UI: in local mode a worker inherits the
    host HOME+env, so an unregistered account silently falls back to the host's
    existing CLI login. Container mode does NOT use this (host login isn't
    mounted) — there an account is mandatory.

    We REUSE the existing quota-path login probes (cli_driver) so the detection
    matches reality: claude's login lives in the macOS Keychain ("Claude
    Code-credentials"), NOT a file — checking only ~/.claude/.credentials.json
    would report a logged-in mac as absent.

    Host-env results (``env is None``) are cached for 30s. Pass ``fresh=True``
    at worker/conversation launch so a just-logged-out host is not treated as
    still present.
    """
    if not host_discovery_enabled(env):
        return "disabled"
    e = (engine or "").strip().lower()
    # env={} means "no env tokens" (an explicit empty mapping), NOT "use the
    # host env" — `env or os.environ` would silently fall back to the real
    # host environment and misreport a host login as present.
    cache_key = e if env is None and e else ""
    if cache_key and not fresh:
        with _HOST_LOGIN_LOCK:
            cached = _HOST_LOGIN_CACHE.get(cache_key)
        if cached is not None:
            cached_at, status = cached
            if time.monotonic() - cached_at < _HOST_LOGIN_CACHE_TTL_S:
                return status

    status = _detect_system_login_uncached(e, env)
    if cache_key:
        with _HOST_LOGIN_LOCK:
            _HOST_LOGIN_CACHE[cache_key] = (time.monotonic(), status)
    return status


def _detect_system_login_uncached(
    e: str, env: Mapping[str, str] | None,
) -> str:
    source = env if env is not None else os.environ

    if e == "devin":
        from muteki.external_agents.devin import devin_login_status

        return devin_login_status(env=env)

    if e == "claude":
        # Settings/listing only reads env, keychain, and the credentials file.
        # Never spawn `claude` here — a settings page load is not a login probe.
        if (source.get("CLAUDE_CODE_OAUTH_TOKEN") or source.get("ANTHROPIC_AUTH_TOKEN")
                or source.get("ANTHROPIC_API_KEY")):
            return "present"
        try:
            from muteki.solver.cli_driver import _claude_oauth  # lazy: avoid cycle
            return "present" if _claude_oauth() is not None else "absent"
        except Exception:
            return "unknown"

    if e == "codex":
        if source.get("OPENAI_API_KEY"):
            return "present"
        try:
            # An explicit CODEX_HOME is authoritative — don't also fall back to
            # ~/.codex (that would let a host login mask an empty CODEX_HOME).
            codex_home = source.get("CODEX_HOME")
            root = Path(codex_home) if codex_home else (Path.home() / ".codex")
            return "present" if (root / "auth.json").exists() else "absent"
        except Exception:
            return "unknown"

    if e == "cursor":
        if source.get("CURSOR_API_KEY"):
            return "present"
        try:
            from muteki.solver.cli_driver import _cursor_session_cookie  # lazy
            return "present" if _cursor_session_cookie() is not None else "absent"
        except Exception:
            return "unknown"

    if e == "pi":
        try:
            root = Path.home() / ".pi" / "agent"
            return ("present"
                    if (root / "auth.json").exists() or (root / "models.json").exists()
                    else "absent")
        except Exception:
            return "unknown"

    if e == "omp":
        try:
            return "present" if (Path.home() / ".omp" / "agent").exists() else "absent"
        except Exception:
            return "unknown"

    if e == "kimi":
        if source.get("KIMI_MODEL_API_KEY") and source.get("KIMI_MODEL_NAME"):
            return "present"
        try:
            root = Path.home() / ".kimi-code"
            return (
                "present"
                if (root / "credentials" / "kimi-code.json").exists()
                else "absent"
            )
        except Exception:
            return "unknown"

    if e == "grok":
        if source.get("XAI_API_KEY"):
            return "present"
        try:
            root = Path.home() / ".grok"
            return "present" if (root / "auth.json").exists() else "absent"
        except Exception:
            return "unknown"

    if e == "opencode":
        if source.get("OPENCODE_API_KEY") or source.get("OPENAI_API_KEY"):
            return "present"
        try:
            return ("present" if (Path.home() / ".local" / "share" / "opencode" / "auth.json").exists()
                    else "absent")
        except Exception:
            return "unknown"

    return "unknown"


def resolve_credential_env(
    credential_id: Any,
    *,
    engine: str,
    sessions_root: str | Path,
    container: bool = False,
    model: str = "",
    agent_state_dir: str | Path | None = None,
    agent_state_container_path: str | None = None,
) -> RuntimeCredentialEnv:
    """Resolve a stable global credential id at the process-launch boundary.

    The returned environment is never persisted.  Stored accounts are read from
    ``CredentialAccountStore``; ``system:<engine>`` deliberately returns an empty
    overlay so the child inherits the host CLI login in the normal way.
    """
    selected_engine = str(engine or "").strip().lower()
    if selected_engine == "devin":
        stable_id = canonical_credential_id(credential_id, engine="devin")
        if container or stable_id not in {"", "system:devin"}:
            raise ValueError("Devin CLI 仅支持本机系统登录凭据")
        if detect_system_login("devin", fresh=True) != "present":
            raise ValueError("请先在本机执行 devin auth login")
        return RuntimeCredentialEnv(account_id="", env={})
    if selected_engine not in SUPPORTED_CREDENTIAL_ENGINES:
        raise ValueError(f"unknown credential engine: {selected_engine!r}")
    stable_id = canonical_credential_id(credential_id, engine=selected_engine)
    if not stable_id:
        # Old Thread records predate explicit credential selection.  Their
        # compatibility meaning is the engine's host login.
        stable_id = system_credential_id(selected_engine)

    system_engine = engine_from_system_credential_id(stable_id)
    if system_engine:
        if container:
            raise ValueError("system credentials are unavailable in container workers")
        detected = detect_system_login(system_engine, fresh=True)
        if detected == "disabled":
            raise HostDiscoveryDisabledError("此服务未提供宿主登录发现；请选择已登记凭据")
        if detected == "absent":
            raise ValueError(f"宿主 {system_engine} 登录态不可用")
        return RuntimeCredentialEnv(account_id="", env={})

    account_id = account_id_from_credential_id(stable_id)
    store = CredentialAccountStore(account_store_root(sessions_root))
    account = store.inspect(account_id) if account_id else None
    if account is None or not account.present:
        raise ValueError(f"凭据账号不可用：{account_id or stable_id}")
    target_engine = str((account.details or {}).get("target_engine") or "").lower()
    account_engine = target_engine or str(account.engine or "").lower()
    if account_engine not in {"", "api", "unknown", selected_engine}:
        raise ValueError(
            f"凭据 {stable_id} 属于 {account_engine}，不能用于 {selected_engine}")
    resolved = runtime_env_for_engine(
        selected_engine,
        account_root=account_store_root(sessions_root),
        account_id=account_id,
        container=container,
        # Explicit account resolution must not fall through to ambient API keys.
        env={},
        agent_state_dir=agent_state_dir,
        agent_state_container_path=agent_state_container_path,
        model=model,
    )
    return RuntimeCredentialEnv(
        account_id=resolved.account_id,
        env={**credential_env_reset(selected_engine), **resolved.env},
    )


# Filenames whose containing dir must be WRITABLE inside the container so the CLI
# can refresh state in place (codex ChatGPT-auth refreshes CODEX_HOME/auth.json;
# pi/omp write session/settings state into their PI_CODING_AGENT_DIR).
_WRITABLE_STATE_DIRS = (
    "codex-home", "pi-agent", "omp-agent", "kimi-home", "grok-home",
)


def project_account_root(
    src_root: str | Path,
    dest_root: str | Path,
    *,
    account_ids: Iterable[str] | None = None,
) -> Path:
    """Update selected Run credentials in place, preserving refreshed CLI state.

    An omitted/empty selection projects no accounts. The destination directory
    inode is stable so existing bind mounts remain valid. Container launch mounts
    this root read-only, with only CLI state directories mounted read-write.
    """
    src, dest = Path(src_root), Path(dest_root)
    allow = {str(a).strip() for a in (account_ids or ()) if str(a).strip()}
    if any(not valid_account_id(a) for a in allow):
        raise ValueError("invalid projected account id")
    if dest.is_symlink():
        raise ValueError("account projection cannot be a symlink")
    dest.mkdir(parents=True, exist_ok=True)
    os.chmod(dest, 0o755)

    def remove(path: Path) -> None:
        if path.is_symlink() or not path.is_dir():
            path.unlink(missing_ok=True)
        else:
            shutil.rmtree(path)

    for child in dest.iterdir():
        if child.name not in allow:
            remove(child)
    for account_id in sorted(allow):
        account_dir = src / account_id
        if account_dir.is_symlink():
            raise ValueError("source account cannot be a symlink")
        if not account_dir.is_dir():
            raise ValueError(f"projected account does not exist: {account_id}")
        out = dest / account_id
        if out.is_symlink():
            raise ValueError("projected account cannot be a symlink")
        out.mkdir(exist_ok=True)
        os.chmod(out, 0o755)
        names = {item.name for item in account_dir.iterdir()} | set(_WRITABLE_STATE_DIRS)
        for old in out.iterdir():
            if old.name not in names:
                remove(old)
        for name in sorted(names):
            source, target = account_dir / name, out / name
            if source.is_symlink() or target.is_symlink():
                raise ValueError("credential projection cannot follow symlinks")
            writable = name in _WRITABLE_STATE_DIRS
            if writable and source.exists() and not source.is_dir():
                raise ValueError("credential state path must be a directory")
            if writable and target.is_dir():
                continue  # keep OAuth refresh/session state and its bind-mount inode
            if source.is_dir():
                if target.exists():
                    remove(target)
                shutil.copytree(source, target, ignore=lambda directory, entries: [
                    entry for entry in entries if (Path(directory) / entry).is_symlink()
                ])
                _chmod_tree(target, dir_mode=0o777 if writable else 0o755,
                            file_mode=0o666 if writable else 0o444)
            elif source.is_file():
                if target.exists():
                    remove(target)
                shutil.copy2(source, target)
                os.chmod(target, 0o444)
            elif writable:
                target.mkdir(exist_ok=True)
                os.chmod(target, 0o777)
    return dest


def _chmod_tree(root: Path, *, dir_mode: int, file_mode: int) -> None:
    for p in root.rglob("*"):
        try:
            os.chmod(p, dir_mode if p.is_dir() else file_mode)
        except OSError:
            pass
    try:
        os.chmod(root, dir_mode)
    except OSError:
        pass


# Cloud Agent / Cursor Secrets → pi-ollama account (custom OpenAI-compatible).
# Install-time bootstrap often soft-skips because secrets inject at agent start;
# call this again from web/runtime boot so the account appears without a rebuild.
PI_OLLAMA_ACCOUNT_ID = "pi-ollama"
PI_OLLAMA_DEFAULT_BASE_URL = "https://ollama.com/v1"
PI_OLLAMA_DEFAULT_MODEL = "deepseek-v4.1-flash"
PI_OLLAMA_PROVIDER = "ollama"


def ensure_pi_ollama_account_from_env(
    state_root: str | Path | None = None,
    *,
    env: Mapping[str, str] | None = None,
) -> dict[str, Any] | None:
    """Materialize ``pi-ollama`` from ``PI_API_KEY`` / ``PI_BASE_URL`` / ``PI_MODEL``.

    Returns the public account dict when written, or ``None`` when the key is
    missing (soft skip). Never logs or returns the API key value.
    """
    source: Mapping[str, str] = env if env is not None else os.environ
    api_key = str(source.get("PI_API_KEY") or "").strip()
    if not api_key:
        return None

    base_url = (
        str(source.get("PI_BASE_URL") or "").strip() or PI_OLLAMA_DEFAULT_BASE_URL
    )
    model = str(source.get("PI_MODEL") or "").strip() or PI_OLLAMA_DEFAULT_MODEL
    root = Path(
        state_root
        if state_root is not None
        else (
            source.get("MUTEKI_STATE_ROOT")
            or source.get("MUTEKI_STATE_DIR")
            or "state"
        )
    )
    store = CredentialAccountStore(account_store_root(root))
    return store.upsert_secret(
        account_id=PI_OLLAMA_ACCOUNT_ID,
        engine="api",
        secret=api_key,
        base_url=base_url,
        target_engine="pi",
        provider=PI_OLLAMA_PROVIDER,
        target_model=model,
        models=[model],
    )
