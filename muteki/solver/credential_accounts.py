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
    source = env or os.environ
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
        if (base / "API_KEY").exists():
            # A custom endpoint (API_KEY + BASE_URL) is engine-agnostic on disk —
            # runtime_env_for_engine keys off the ENGINE passed in, not the account.
            # The optional ENGINE marker records which agent the operator registered
            # it FOR, so the panel can bind/display it (claude/codex/cursor) instead
            # of an orphan "api". No marker → legacy/programmatic "api".
            target = self._read_target_engine(base)
            base_url = self._read_base_url(base)
            return CredentialAccount(
                account_id=account_id,
                engine=target or "api",
                mode="custom_endpoint",
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
    ) -> dict[str, Any]:
        account_id = account_id.strip()
        engine = engine.strip().lower()
        if not valid_account_id(account_id):
            raise ValueError("account_id must be 1-64 chars: letters, digits, _, ., -")
        if engine not in {"claude", "codex", "cursor", "api"}:
            raise ValueError("engine must be claude, codex, cursor, or api")

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
        elif engine == "api":
            value = str(secret or "").strip() or prior.get("API_KEY", "")
            if not value:
                raise ValueError("API_KEY is required")
            # base_url / target_engine: a blank field on edit keeps the stored
            # value (the UI sends "" when the operator didn't touch it). An
            # explicit clear isn't expressible here, and isn't needed by the panel.
            b = str(base_url or "").strip() or prior.get("BASE_URL", "")
            te = str(target_engine or "").strip().lower() or prior.get("ENGINE", "")
            if te and te not in {"claude", "codex", "cursor"}:
                raise ValueError("target_engine must be claude, codex, or cursor")
            base = self._replace_account(account_id)
            self._atomic_write(base / "API_KEY", value + "\n")
            if b:
                self._atomic_write(base / "BASE_URL", b + "\n")
            # Record which agent this endpoint is FOR so the panel can bind/display
            # it. The runtime injection stays engine-agnostic (it reads API_KEY/
            # BASE_URL regardless of this marker).
            if te:
                self._atomic_write(base / "ENGINE", te + "\n")
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
        return {
            "account_id": acct.account_id,
            "engine": acct.engine,
            "mode": acct.mode,
            "present": acct.present,
            "writable_state": acct.writable_state,
            "updated_at": acct.updated_at,
            "details": acct.details or {},
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
        return marker if marker in {"claude", "codex", "cursor"} else ""

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
        tmp = path.with_name(f".{path.name}.{int(time.time() * 1000)}.tmp")
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
        for rel in ("CLAUDE_CODE_OAUTH_TOKEN", "CURSOR_API_KEY", "API_KEY", "BASE_URL", "ENGINE"):
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
        for rel in ("CLAUDE_CODE_OAUTH_TOKEN", "CURSOR_API_KEY", "API_KEY", "BASE_URL", "ENGINE"):
            try:
                (base / rel).unlink(missing_ok=True)
            except OSError:
                pass
        codex_home = base / "codex-home"
        if codex_home.exists():
            shutil.rmtree(codex_home, ignore_errors=True)


def runtime_env_for_engine(
    engine: str,
    *,
    account_root: str | Path | None = None,
    account_id: str | None = None,
    container: bool = False,
    env: Mapping[str, str] | None = None,
) -> RuntimeCredentialEnv:
    """Resolve credential env for one engine.

    Container mode avoids sending secret values through `docker exec -e` when a
    file-backed account exists: it passes only `*_FILE` paths and lets the
    container shell export the real value inside the process. Local mode reads
    those files into the subprocess env because there is no container wrapper.
    """

    e = (engine or "").strip().lower()
    source = env or os.environ
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

    return RuntimeCredentialEnv(account_id=account_id, env=out)


def _container_secret_path(account_id: str, filename: str) -> str:
    return f"{CONTAINER_ACCOUNTS_ROOT}/{account_id}/{filename}"


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
    source = env or os.environ

    if e == "claude":
        # env token wins (explicit), else the keychain/file probe.
        if source.get("CLAUDE_CODE_OAUTH_TOKEN") or source.get("ANTHROPIC_API_KEY"):
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

    return "unknown"


# Filenames whose containing dir must be WRITABLE inside the container so the CLI
# can refresh state in place (codex ChatGPT-auth refreshes CODEX_HOME/auth.json).
_WRITABLE_STATE_DIRS = ("codex-home",)


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
