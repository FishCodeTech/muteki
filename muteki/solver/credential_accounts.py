"""Credential Account resolution for CLI workers.

This module keeps subscription/API credentials out of prompts, worker scratch,
and the normal worker config JSON. It resolves a small, explicit account store:

    sessions/_secrets/accounts/<account_id>/

Container workers see that root at /run/muteki/accounts. Local workers can use
the same files directly. Environment variables remain a developer convenience,
but the persistent path is account-scoped instead of mounting a host home dir.
"""

from __future__ import annotations

import os
import re
import shutil
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Mapping, Optional


CONTAINER_ACCOUNTS_ROOT = "/run/muteki/accounts"


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


def account_store_root(sessions_root: str | Path) -> Path:
    """Default durable account store under the web sessions root."""

    return Path(sessions_root) / "_secrets" / "accounts"


def engine_account_id(engine: str, env: Mapping[str, str] | None = None) -> str:
    """Return the account id for an engine, overridable per engine by env."""

    e = (engine or "").strip().lower()
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
        try:
            self.root.chmod(0o700)
        except OSError:
            pass

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
                details={"kimi_home": True, "mutable_auth_home": True},
            )
        if (base / "grok-home" / "auth.json").exists():
            return CredentialAccount(
                account_id=account_id,
                engine="grok",
                mode="login_home",
                present=True,
                writable_state=True,
                updated_at=updated,
                details={"grok_home": True, "mutable_auth_home": True},
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
                    # base_url is non-sensitive config (a public host); secret_value
                    # is the API key (see _read_secret_value's security note). Both
                    # are echoed so the panel can show & edit them in place.
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

    def upsert_secret(
        self,
        *,
        account_id: str,
        engine: str,
        secret: str | None = None,
        codex_auth_json: str | None = None,
        base_url: str | None = None,
        target_engine: str | None = None,
        provider: str | None = None,
        clear_base_url: bool = False,
    ) -> dict[str, Any]:
        account_id = account_id.strip()
        engine = engine.strip().lower()
        if not valid_account_id(account_id):
            raise ValueError("account_id must be 1-64 chars: letters, digits, _, ., -")
        if engine not in {
            "claude", "codex", "cursor", "pi", "omp", "kimi", "grok",
            "opencode", "dsh", "api",
        }:
            raise ValueError(
                "engine must be claude, codex, cursor, pi, omp, kimi, grok, "
                "opencode, dsh, or api")

        # EDIT support: secrets are never read back to the UI, so an operator who
        # only wants to change an endpoint's base_url / target_engine cannot
        # re-supply the key. When the incoming secret is blank AND a matching
        # account already exists on disk, fall back to the stored secret so the
        # edit preserves it. _replace_account wipes the dir, so snapshot first.
        prior = self._snapshot_material(account_id)

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
        elif engine in {"api", "pi", "omp", "kimi", "grok", "opencode", "dsh"}:
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
            if te and te not in {
                "claude", "codex", "cursor", "pi", "omp", "kimi", "grok",
                "opencode", "dsh",
            }:
                raise ValueError(
                    "target_engine must be claude, codex, cursor, pi, omp, kimi, "
                    "grok, opencode, or dsh")
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
            import json
            json.loads(value)
            base = self._replace_account(account_id)
            codex_home = base / "codex-home"
            codex_home.mkdir(parents=True, exist_ok=True)
            self._chmod_private_dir(codex_home)
            self._atomic_write(codex_home / "auth.json", value + "\n")

        acct = self.inspect(account_id)
        assert acct is not None
        return self._public(acct)

    def import_host_codex_auth(self, account_id: str) -> dict[str, Any]:
        """Refresh a codex account from the HOST's ~/.codex/auth.json.

        `codex login` refreshes the host's ~/.codex/auth.json, but container
        workers mount the account-store COPY — so a fresh host login never reaches
        the account until it's re-imported. This reads the host file and upserts it
        (one click from the settings page). Only meaningful on a bare host where
        ~/.codex belongs to the operator; the caller guards on is_web_container().

        Raises ValueError with an actionable message if the host file is missing
        or invalid (the route maps it to a 400/404).
        """
        host_auth = Path.home() / ".codex" / "auth.json"
        if not host_auth.exists():
            raise ValueError(
                f"host ~/.codex/auth.json not found ({host_auth}) — run `codex login` first"
            )
        try:
            content = host_auth.read_text(encoding="utf-8")
        except OSError as exc:
            raise ValueError(f"could not read {host_auth}: {exc}") from exc
        # upsert_secret validates it's JSON and writes codex-home/auth.json.
        return self.upsert_secret(
            account_id=account_id, engine="codex", codex_auth_json=content
        )

    def import_host_login(self, account_id: str, engine: str) -> dict[str, Any]:
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
            account = self.upsert_secret(
                account_id=account_id,
                engine="api",
                secret=secret,
                base_url=base_url,
                target_engine="claude",
                provider="宿主 Claude 配置",
            )
            suggested_model = str(
                settings_env.get("ANTHROPIC_MODEL")
                or settings_env.get("ANTHROPIC_DEFAULT_SONNET_MODEL")
                or ""
            ).strip()
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

    def delete(self, account_id: str) -> bool:
        if not valid_account_id(account_id):
            return False
        base = self.root / account_id
        if not base.exists():
            return False
        shutil.rmtree(base)
        return True

    @staticmethod
    def _public(acct: CredentialAccount) -> dict[str, Any]:
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
                "opencode", "dsh"
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
            "engine": acct.engine,
            "worker_engine": worker_engine,
            "connection": connection,
            "base_url": base_url,
            "credential_format": credential_format,
            "provider": str(details.get("provider") or "").strip(),
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
            "opencode", "dsh"
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
    def _read_secret_value(base: Path) -> str:
        """The account's stored SECRET in plaintext, or "" if absent/unreadable.

        ⚠️ SECURITY POSTURE: this deliberately returns the raw credential
        (OAuth token / API key / codex auth.json) so the settings UI can ECHO it
        into the edit form (operator request: "show it, let me edit it"). It is
        therefore included in the JSON of the (password-authenticated) credential
        endpoints and visible in the browser's Network tab. Callers that don't
        want the plaintext must strip details.secret_value before forwarding.

        For codex the value is the auth.json contents (the form edits it as JSON);
        for the other engines it's the single-line token/key.
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
        tmp.write_text(text, encoding="utf-8")
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
        for rel in ("CLAUDE_CODE_OAUTH_TOKEN", "CURSOR_API_KEY", "API_KEY", "BASE_URL", "ENGINE", "PROVIDER"):
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
        for rel in ("CLAUDE_CODE_OAUTH_TOKEN", "CURSOR_API_KEY", "API_KEY", "BASE_URL", "ENGINE", "PROVIDER"):
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
    elif e == "dsh":
        if base is not None and (base / "API_KEY").exists():
            _add_secret_file_or_env(
                out,
                base=base,
                filename="API_KEY",
                env_name="DEEPSEEK_API_KEY",
                container=container,
                container_path=_container_secret_path(account_id, "API_KEY"),
                source=source,
            )
            _add_base_url(out, base=base, env_name="DEEPSEEK_BASE_URL")
        elif source.get("DEEPSEEK_API_KEY") or source.get("MUTEKI_DEEPSEEK_API_KEY"):
            out["DEEPSEEK_API_KEY"] = str(
                source.get("DEEPSEEK_API_KEY")
                or source.get("MUTEKI_DEEPSEEK_API_KEY")
                or ""
            )
        if agent_state_dir is not None:
            state_root = Path(agent_state_dir).expanduser().resolve()
            sessions = state_root / "sessions"
            sessions.mkdir(parents=True, exist_ok=True)
            runtime_root = (
                str(agent_state_container_path)
                if container and agent_state_container_path else str(state_root)
            )
            out["DSH_SESSION_ROOT"] = f"{runtime_root}/sessions"
            out["DSH_TELEMETRY_DISABLED"] = "1"
    elif e in ("pi", "omp"):
        # pi/omp reach a custom OpenAI-compatible provider through a generated
        # provider config in their agent config dir (pi: models.json; omp:
        # models.yml — hand-written YAML, no pyyaml dependency), selected via
        # PI_CODING_AGENT_DIR (both CLIs honor it). The account's API_KEY is
        # embedded in that file, so the dir is projected as a WRITABLE state dir
        # (same posture as codex-home). OPENAI_API_KEY stays as a harmless native
        # fallback; omp additionally honors OPENAI_BASE_URL natively.
        import json
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
                            "id": selected_model or "default",
                            "contextWindow": 128000,
                            "maxTokens": 8192,
                        }],
                    }},
                }, indent=2) + "\n"
                config_name = "models.json"
            else:
                config_text = _omp_models_yml(
                    base_url, api_key, selected_model or "default")
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


def detect_system_login(engine: str, env: Mapping[str, str] | None = None) -> str:
    """Is there a usable HOST-side login for this engine? (DESIGN §2.3 補強B)

    READ-ONLY, never raises. Returns "present" / "absent" / "unknown". This only
    drives the local-mode credentials UI: in local mode a worker inherits the
    host HOME+env, so an unregistered account silently falls back to the host's
    existing CLI login. Container mode does NOT use this (host login isn't
    mounted) — there an account is mandatory.

    We REUSE the existing quota-path login probes (cli_driver) so the detection
    matches reality: claude's login lives in the macOS Keychain ("Claude
    Code-credentials"), NOT a file — checking only ~/.claude/.credentials.json
    would report a logged-in mac as absent.
    """
    e = (engine or "").strip().lower()
    # env={} means "no env tokens" (an explicit empty mapping), NOT "use the
    # host env" — `env or os.environ` would silently fall back to the real
    # host environment and misreport a host login as present.
    source = env if env is not None else os.environ

    if e == "claude":
        # Explicit credentials win. The host CLI may also load a gateway token
        # from ~/.claude/settings.json, so its own auth status is authoritative
        # for the normal local-mode probe.
        if (source.get("CLAUDE_CODE_OAUTH_TOKEN") or source.get("ANTHROPIC_AUTH_TOKEN")
                or source.get("ANTHROPIC_API_KEY")):
            return "present"
        if env is None:
            try:
                import json
                import subprocess
                from muteki.solver.cli_driver import resolve_engine_bin

                probe = subprocess.run(
                    [resolve_engine_bin("claude"), "auth", "status", "--json"],
                    capture_output=True,
                    text=True,
                    encoding="utf-8",
                    errors="replace",
                    timeout=8,
                )
                if probe.returncode == 0:
                    status = json.loads(probe.stdout or "{}")
                    if isinstance(status, dict) and status.get("loggedIn") is True:
                        return "present"
            except Exception:
                pass
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

    if e == "dsh":
        return (
            "present"
            if source.get("DEEPSEEK_API_KEY") or source.get("MUTEKI_DEEPSEEK_API_KEY")
            else "absent"
        )

    return "unknown"


# Filenames whose containing dir must be WRITABLE inside the container so the CLI
# can refresh state in place (codex ChatGPT-auth refreshes CODEX_HOME/auth.json;
# pi/omp write session/settings state into their PI_CODING_AGENT_DIR).
_WRITABLE_STATE_DIRS = (
    "codex-home", "pi-agent", "omp-agent", "kimi-home", "grok-home",
)


def project_account_root(src_root: str | Path, dest_root: str | Path) -> Path:
    """Stage a container-READABLE projection of the account store (#14, #15).

    The host account store holds 0600 files owned by the host user; a container
    worker runs as a different uid ('kali') and cannot read them through a plain
    read-only bind mount (#15), and codex needs CODEX_HOME/auth.json to be WRITABLE
    so it can refresh its OAuth token in place (#14 — the raw store mount is
    read-only and must stay so).

    This copies the store into `dest_root` (a per-run, gitignored, ephemeral dir
    under the run workspace) with permissions the container user can use:
      - static secret files (API keys / OAuth tokens) → 0644 (readable, not writable
        by the worker — the worker only reads them);
      - writable-state dirs (codex-home) → dir 0777 + files 0666 so the CLI can
        rewrite auth.json after a token refresh.
    The HOST store is never modified and never made world-writable; this projection
    is the only thing the container sees. Returns dest_root.
    """
    src = Path(src_root)
    dest = Path(dest_root)
    if dest.exists():
        shutil.rmtree(dest, ignore_errors=True)
    dest.mkdir(parents=True, exist_ok=True)
    try:
        os.chmod(dest, 0o755)
    except OSError:
        pass
    if not src.exists():
        return dest
    for account_dir in sorted(p for p in src.iterdir() if p.is_dir()):
        if not valid_account_id(account_dir.name):
            continue
        out_account = dest / account_dir.name
        out_account.mkdir(parents=True, exist_ok=True)
        try:
            os.chmod(out_account, 0o755)
        except OSError:
            pass
        for item in account_dir.iterdir():
            target = out_account / item.name
            if item.is_dir():
                writable = item.name in _WRITABLE_STATE_DIRS
                shutil.copytree(item, target, dirs_exist_ok=True)
                _chmod_tree(target, dir_mode=0o777 if writable else 0o755,
                            file_mode=0o666 if writable else 0o644)
            elif item.is_file():
                shutil.copy2(item, target)
                try:
                    os.chmod(target, 0o644)
                except OSError:
                    pass
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
