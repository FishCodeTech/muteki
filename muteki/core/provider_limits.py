"""Read subscription limits without starting model turns.

Provider mappings are adapted from T3 Code fd1c3386c4d60f3477ab3f13c87537848de099f5,
apps/server/src/provider/{codex,claude,grok,openCode}UsageLimits.ts.
Copyright (c) 2026 T3 Tools Inc. (MIT; see provider_limits.LICENSE).

Credential ids select the same stored account or host login as the runtime.
Successful snapshots are cached in memory; failed refreshes retain explicitly
stale windows only while that credential's configuration revision is unchanged.
"""

from __future__ import annotations

import asyncio
import copy
import hashlib
import json
import math
import os
import re
import sys
import threading
import time
from concurrent.futures import ThreadPoolExecutor
from datetime import datetime
from pathlib import Path
from typing import Any, Mapping

import httpx

from muteki.core.cursor_usage import (
    CursorUsageError, KeychainTokenReader, _SignedOut, read_limits,
    auth_file_path, read_settings, resolve_login,
)
from muteki.solver.credential_accounts import (
    CredentialAccountStore, account_store_root, host_discovery_enabled,
    resolve_credential_env,
)

_SUPPORTED = ("codex", "claude", "grok", "cursor", "opencode")
_UNSUPPORTED = ("pi", "kimi", "omp", "devin", "droid")
_NAMES = {"codex": "Codex", "claude": "Claude Code", "grok": "Grok",
          "cursor": "Cursor", "opencode": "OpenCode", "pi": "Pi", "kimi": "Kimi",
          "omp": "OMP", "devin": "Devin", "droid": "Droid"}
_SESSION = 5 * 3600
_WEEK = 7 * 86400
_MONTH = 30 * 86400
_LABELS = {"session": "会话额度", "weekly": "周额度", "monthly": "月额度", "other": "订阅额度"}


class LimitsUnavailable(Exception):
    """Typed, client-safe failure; never carries token values or HTTP bodies."""

    def __init__(self, status: str, code: str, message: str):
        super().__init__(message)
        self.status, self.code = status, code


def _number(value: Any) -> float | None:
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        return None
    return float(value) if math.isfinite(value) else None


def _reset(value: Any) -> float | None:
    number = _number(value)
    if number is not None:
        return number if number > 0 else None
    if isinstance(value, str):
        try:
            dt = datetime.fromisoformat(value.replace("Z", "+00:00"))
            return dt.timestamp() if dt.tzinfo is not None else None
        except ValueError:
            return None
    return None


def _window(identifier: str, kind: str, used: float, *, reset: Any = None,
            seconds: float | None = None, label: str | None = None) -> dict[str, Any]:
    return {"id": identifier, "kind": kind, "label": label or _LABELS[kind],
            "used_percent": max(0.0, min(100.0, used)), "reset_at": _reset(reset),
            "window_seconds": seconds}


def codex_windows(response: Mapping[str, Any]) -> list[dict[str, Any]]:
    buckets = response.get("rateLimitsByLimitId")
    snapshot = (buckets.get("codex") if isinstance(buckets, dict) else None) or response.get("rateLimits") or {}
    if snapshot.get("limitId") not in (None, "", "codex"):
        return []
    monthly = snapshot.get("planType") in ("free", "go")
    windows = []
    for identifier, fallback in (("primary", _MONTH if monthly else _SESSION), ("secondary", _WEEK)):
        row = snapshot.get(identifier)
        used = _number(row.get("usedPercent")) if isinstance(row, dict) else None
        if used is None:
            continue
        minutes = _number(row.get("windowDurationMins"))
        seconds = minutes * 60 if minutes is not None and minutes > 0 else fallback
        kind = "monthly" if seconds >= _MONTH else "weekly" if seconds >= _WEEK else "session"
        windows.append(_window(identifier, kind, used, reset=row.get("resetsAt"), seconds=seconds))
    return windows


def claude_windows(response: Mapping[str, Any]) -> list[dict[str, Any]]:
    limits = response.get("rate_limits")
    if not response.get("rate_limits_available") or not isinstance(limits, dict):
        raise LimitsUnavailable("unsupported", "no_subscription_limits", "此 Claude 账户未提供订阅额度。")
    windows = []
    for identifier, kind, seconds in (("five_hour", "session", _SESSION), ("seven_day", "weekly", _WEEK)):
        row = limits.get(identifier)
        used = _number(row.get("utilization")) if isinstance(row, dict) else None
        if used is not None:
            windows.append(_window(identifier, kind, used, reset=row.get("resets_at"), seconds=seconds))
    for row in limits.get("model_scoped") or []:
        if not isinstance(row, dict) or not isinstance(row.get("display_name"), str):
            continue
        used = _number(row.get("utilization"))
        if used is not None:
            name = row["display_name"]
            identifier = "seven_day_" + re.sub("[^a-z0-9]+", "_", name.lower())
            windows.append(_window(identifier, "weekly", used, reset=row.get("resets_at"),
                                   seconds=_WEEK, label=f"周额度 · {name}"))
    return windows


def grok_windows(response: Mapping[str, Any]) -> list[dict[str, Any]]:
    config = response.get("config") or {}
    used = _number(config.get("creditUsagePercent"))
    # xAI omits the percentage before first use. This is an empty successful
    # snapshot, not proof the account cannot report limits.
    if used is None:
        return []
    period = config.get("currentPeriod") or {}
    kind = {"WEEKLY": "weekly", "MONTHLY": "monthly"}.get(
        str(period.get("type") or "").removeprefix("USAGE_PERIOD_TYPE_"), "other")
    return [_window("subscription", kind, used, reset=period.get("end"))]


def opencode_windows(response: Mapping[str, Any]) -> list[dict[str, Any]]:
    usage = response.get("usage") or {}
    windows = []
    for identifier, kind, seconds in (("rolling", "session", _SESSION), ("weekly", "weekly", _WEEK),
                                      ("monthly", "monthly", None)):
        row = usage.get(identifier)
        used = _number(row.get("percent")) if isinstance(row, dict) else None
        if used is None or _reset(row.get("resetsAt")) is None:
            raise LimitsUnavailable("error", "invalid_response", "OpenCode Go 额度响应结构无效。")
        windows.append(_window(f"go_{identifier}", kind, used, reset=row.get("resetsAt"),
                               seconds=seconds, label=f"Go · {_LABELS[kind]}"))
    return windows


def _fingerprint(engine: str, identity: str) -> str:
    return hashlib.sha256(f"{engine}\0{identity}".encode()).hexdigest()


def _json_file(path: Path) -> dict[str, Any]:
    try:
        raw = json.loads(path.read_text(encoding="utf-8"))
    except FileNotFoundError:
        return {}
    if not isinstance(raw, dict):
        raise LimitsUnavailable("error", "invalid_credentials", "登录文件结构无效。")
    return raw


def _get_json(url: str, token: str, *, go: bool = False) -> dict[str, Any]:
    with httpx.Client(timeout=10, follow_redirects=False) as client:
        response = client.get(url, headers={"Authorization": f"Bearer {token}"})
    if go and response.status_code == 403:
        raise LimitsUnavailable("unsupported", "go_subscription_required", "此账户没有 OpenCode Go 订阅。")
    if response.status_code in (401, 403):
        raise LimitsUnavailable("unauthenticated", "login_expired", "登录已失效，请重新登录后刷新。")
    if not response.is_success:
        raise LimitsUnavailable("error", "http_error", f"额度接口返回 HTTP {response.status_code}。")
    raw = response.json()
    if not isinstance(raw, dict):
        raise LimitsUnavailable("error", "invalid_response", "额度接口响应结构无效。")
    return raw


class ProviderLimits:
    """Synchronous API for FastAPI threadpool routes; no prompt is ever sent."""

    def __init__(self, state_root: str | Path, *, env: Mapping[str, str] | None = None,
                 keychain: KeychainTokenReader | None = None, cache_seconds: float = 60):
        self.state_root = Path(state_root)
        self.env = dict(os.environ if env is None else env)
        self.keychain = keychain or KeychainTokenReader()
        self.cache_seconds = cache_seconds
        self._lock = threading.Lock()
        self._cached_at = 0.0
        self._completed_at = 0.0
        self._cached: dict[str, Any] | None = None
        self._last: dict[str, dict[str, Any]] = {}
        self._results: dict[str, tuple[float, dict[str, Any], Any]] = {}
        self._signature: tuple = ()

    def _accounts(self) -> list[dict[str, Any]]:
        store = CredentialAccountStore(account_store_root(self.state_root))
        rows = []
        for account in store.list():
            engine = str(account.get("worker_engine") or account.get("engine") or "")
            if engine not in _SUPPORTED + _UNSUPPORTED:
                continue
            rows.append({**account, "id": "account:" + account["account_id"],
                         "engine": engine, "label": account["account_id"], "source": "stored"})
        for engine in _SUPPORTED + _UNSUPPORTED:
            row = {"id": "system:" + engine, "engine": engine, "source": "system",
                   "label": f"{_NAMES[engine]} · 本机登录"}
            try:
                row["revision"] = self._system_revision(engine)
            except OSError as exc:
                # Revision checks must have the same per-account isolation as
                # probes. An unreadable Claude home cannot hide Codex limits.
                row["revision"] = f"unreadable:{type(exc).__name__}"
                row["_revision_error"] = type(exc).__name__
            rows.append(row)
        return rows

    def _system_revision(self, engine: str) -> str:
        """Invalidate stale quota when a host switches its credential material."""
        if not host_discovery_enabled(self.env):
            return "host_discovery_disabled"
        home = Path(self.env.get("HOME") or Path.home())
        roots = {"codex": Path(self.env.get("CODEX_HOME") or home / ".codex"),
                 "claude": Path(self.env.get("CLAUDE_CONFIG_DIR") or home / ".claude"),
                 "grok": Path(self.env.get("GROK_HOME") or home / ".grok"),
                 "opencode": Path(self.env.get("XDG_DATA_HOME") or home / ".local" / "share") / "opencode"}
        files = [roots[engine] / name for name in ("auth.json", ".credentials.json", "settings.json", "config.toml")] if engine in roots else []
        if engine == "cursor":
            files = [auth_file_path(self.env, sys.platform, home), self.state_root / "_usage_settings.json"]
        stats = []
        for path in files:
            try:
                stat = path.stat()
                stats.append((str(path), stat.st_mtime_ns, stat.st_size))
            except FileNotFoundError:
                stats.append((str(path), None, None))
        return _fingerprint(engine, json.dumps([self.env, stats], sort_keys=True))

    def read(self, refresh: bool = False, credential_id: str | None = None) -> dict[str, Any]:
        requested_at = time.monotonic()
        with self._lock:
            now = time.monotonic()
            accounts = self._accounts()
            signature = tuple((row["id"], row.get("revision")) for row in accounts)
            if signature != self._signature:
                self._cached = None
                self._signature = signature
            if credential_id is not None and not any(row["id"] == credential_id for row in accounts):
                raise ValueError("unknown credential id")
            if self._cached is not None and (self._completed_at >= requested_at or
                    (not refresh and now - self._cached_at < self.cache_seconds)):
                result = copy.deepcopy(self._cached)
                if credential_id is not None:
                    result["accounts"] = [row for row in result["accounts"] if credential_id in row["credential_ids"]]
                return result
            def cached_or_probe(account: dict[str, Any]) -> dict[str, Any]:
                cached = self._results.get(account["id"])
                if cached and cached[2] == account.get("revision") and (
                        cached[0] >= requested_at or (not refresh and now - cached[0] < self.cache_seconds)):
                    return copy.deepcopy(cached[1])
                return self._read_account(account)

            if credential_id is not None:
                accounts = [row for row in accounts if row["id"] == credential_id]
                rows = [cached_or_probe(accounts[0])]
            else:
                with ThreadPoolExecutor(max_workers=5, thread_name_prefix="provider-limits") as pool:
                    rows = list(pool.map(cached_or_probe, accounts))
            merged: dict[str, dict[str, Any]] = {}
            for row in rows:
                fingerprint = row.pop("_fingerprint", None)
                group = f"{row['engine']}:{fingerprint}" if fingerprint else row["id"]
                if group not in merged:
                    row["credential_ids"] = [row["id"]]
                    merged[group] = row
                else:
                    previous = merged[group]
                    previous["credential_ids"].append(row["id"])
                    if row["status"] == "ok" and (previous["status"] != "ok" or
                            (row["updated_at"] or 0) > (previous["updated_at"] or 0)):
                        merged[group] = {**row, "id": previous["id"], "label": previous["label"],
                                         "credential_ids": previous["credential_ids"]}
            result = {"as_of": time.time(), "accounts": list(merged.values()),
                      "supported_engines": list(_SUPPORTED), "cache_seconds": self.cache_seconds}
            if credential_id is None:
                self._cached = copy.deepcopy(result)
                self._cached_at = min((self._results[row["id"]][0] for row in accounts), default=time.monotonic())
                self._completed_at = time.monotonic()
            else:
                # Rebuild pooling from every per-credential snapshot on the
                # next full read. Other aliases retain their snapshot when the
                # refreshed credential has switched to another account.
                self._cached = None
            return result

    def _read_account(self, account: dict[str, Any]) -> dict[str, Any]:
        engine, identifier = account["engine"], account["id"]
        row = {"id": identifier, "engine": engine, "label": account["label"],
               "status": "ok", "windows": [], "updated_at": None, "error": None,
               "error_code": None, "stale": False, "checked_at": time.time()}
        try:
            if engine in _UNSUPPORTED:
                raise LimitsUnavailable("unsupported", "provider_not_supported", "暂未支持额度查询；Cost 和 Tokens 仍统计此引擎。")
            if account["source"] == "system" and not host_discovery_enabled(self.env):
                raise LimitsUnavailable("unauthenticated", "host_discovery_disabled", "此服务未启用本机登录发现。")
            if account.get("_revision_error"):
                raise LimitsUnavailable("error", "credential_metadata_unreadable",
                                        f"无法读取此账户的登录文件元数据（{account['_revision_error']}）。")
            if account.get("connection") == "custom_endpoint":
                raise LimitsUnavailable("unsupported", "custom_endpoint", "自定义端点暂不提供订阅额度。")
            if account.get("credential_format") == "api_key" and engine != "opencode":
                raise LimitsUnavailable("unsupported", "api_key", "API key 账户不提供订阅额度。")
            env = dict(self.env)
            if account["source"] == "stored":
                if not account.get("present"):
                    raise LimitsUnavailable("unauthenticated", "credential_missing", "账户登录凭据缺失。")
                overlay = resolve_credential_env(identifier, engine=engine, sessions_root=self.state_root).env
                env.update(overlay)
            windows, fingerprint = self._probe(engine, env, account)
            row.update(windows=windows, updated_at=time.time(), _fingerprint=fingerprint)
            self._last[identifier] = {"revision": account.get("revision"), "row": copy.deepcopy(row)}
        except Exception as exc:
            status, code, message = self._safe_error(exc)
            row.update(status=status, error_code=code, error=message)
            previous = self._last.get(identifier)
            failure_identity = getattr(exc, "credential_fingerprint", None)
            if (status == "error" and previous and previous["revision"] == account.get("revision")
                    and (failure_identity is None or failure_identity == previous["row"].get("_fingerprint"))):
                row.update(windows=copy.deepcopy(previous["row"]["windows"]),
                           updated_at=previous["row"]["updated_at"], stale=True,
                           _fingerprint=previous["row"].get("_fingerprint"))
        self._results[identifier] = (time.monotonic(), copy.deepcopy(row), account.get("revision"))
        return row

    @staticmethod
    def _safe_error(exc: Exception) -> tuple[str, str, str]:
        if isinstance(exc, LimitsUnavailable):
            return exc.status, exc.code, str(exc)
        if isinstance(exc, CursorUsageError):
            status = ("unsupported" if exc.code in {"limits_unsupported", "api_key_only"}
                      else "unauthenticated" if exc.code in {"login_missing", "login_unavailable", "keychain_disabled"}
                      else "error")
            return status, exc.code, str(exc)
        if isinstance(exc, _SignedOut):
            return "unauthenticated", "login_expired", "Cursor 登录已失效，请重新登录。"
        if isinstance(exc, (TimeoutError, httpx.TimeoutException)) or isinstance(exc.__cause__, TimeoutError):
            return "error", "timeout", "额度读取超时，请稍后刷新。"
        if isinstance(exc, FileNotFoundError):
            return "error", "cli_missing", "所需 CLI 未安装或路径不可用。"
        if isinstance(exc, httpx.RequestError):
            return "error", "network_error", f"额度接口连接失败（{type(exc).__name__}）。"
        # Raw SDK/RPC/HTTP errors can include credentials; expose category only.
        from muteki.external_agents.codex import JsonRpcError
        if isinstance(exc, JsonRpcError):
            return "error", "rpc_error", f"额度读取失败（JSON-RPC {exc.code}）。"
        return "error", "probe_failed", f"额度读取失败（{type(exc).__name__}）。"

    def _probe(self, engine: str, env: dict[str, str], account: dict[str, Any]) -> tuple[list, str | None]:
        if engine == "codex":
            return asyncio.run(self._codex(env))
        if engine == "claude":
            return asyncio.run(self._claude(env, account))
        if engine == "grok":
            return self._grok(env, account)
        if engine == "opencode":
            return self._opencode(env, account)
        if engine == "cursor":
            if account["source"] != "system":
                raise LimitsUnavailable("unsupported", "host_login_required", "Cursor 额度需使用本机持久化 CLI 登录。")
            if env.get("CURSOR_API_ENDPOINT", "https://api2.cursor.sh").rstrip("/") != "https://api2.cursor.sh":
                raise LimitsUnavailable("unsupported", "custom_endpoint", "Cursor 自定义端点的额度暂未支持。")
            login = resolve_login(keychain_enabled=read_settings(self.state_root)["cursor_account_usage_enabled"],
                                  keychain=self.keychain, env=env, platform=sys.platform)
            try:
                with httpx.Client(timeout=10, follow_redirects=False) as client:
                    limits = read_limits(login, client)
            except Exception as exc:
                exc.credential_fingerprint = login.account_key
                raise
            return ([{**row, "label": row["label"][0], "description": row["description"][0],
                      "aggregate": row["id"] == "totalPercentUsed", "reset_at": row.get("resets_at"), "window_seconds": None}
                     for row in limits["windows"]], login.account_key)
        raise LimitsUnavailable("unsupported", "provider_not_supported", "暂未支持额度查询。")

    async def _codex(self, env: dict[str, str]) -> tuple[list, str | None]:
        from muteki.external_agents.codex import CodexPeer
        from muteki.solver.cli_engines.bins import resolve_engine_bin

        if env.get("OPENAI_API_KEY") or env.get("OPENAI_BASE_URL"):
            raise LimitsUnavailable("unsupported", "api_key", "当前 Codex 环境使用 API key 或自定义端点。")
        root = Path(env.get("CODEX_HOME") or Path(env.get("HOME") or Path.home()) / ".codex")
        auth = _json_file(root / "auth.json")
        if not auth:
            raise LimitsUnavailable("unauthenticated", "login_missing", "请先登录此 Codex 账户。")
        peer = None
        try:
            async with asyncio.timeout(20):
                peer = await CodexPeer.spawn([resolve_engine_bin("codex"), "app-server", "--listen", "stdio://"],
                                             env=env, cwd=str(self.state_root.resolve()))
                await peer.request("initialize", {"clientInfo": {"name": "muteki-usage", "title": "Muteki Usage", "version": "1"}}, timeout=10)
                await peer.notify("initialized")
                identity = await peer.request("account/read", {"refreshToken": False}, timeout=10)
                account = identity.get("account") or {}
                if account.get("type") != "chatgpt":
                    raise LimitsUnavailable("unsupported", "subscription_required", "此 Codex 登录不是 ChatGPT 订阅账户。")
                response = await peer.request("account/rateLimits/read", {}, timeout=10)
            key = str((auth.get("tokens") or {}).get("account_id") or str(account.get("email") or "").strip().lower())
            return codex_windows(response), _fingerprint("codex", key) if key else None
        finally:
            if peer is not None:
                await peer.close()

    async def _claude(self, env: dict[str, str], account: dict[str, Any]) -> tuple[list, str | None]:
        import anyio
        from claude_agent_sdk import ClaudeAgentOptions, ClaudeSDKClient
        from muteki.external_agents.claude_transport import owned_claude_transport
        from muteki.solver.cli_engines.bins import resolve_engine_bin

        if any(env.get(key) for key in ("ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN", "ANTHROPIC_BASE_URL")):
            raise LimitsUnavailable("unsupported", "api_key", "当前 Claude 环境使用 API key 或自定义端点。")
        if account["source"] == "stored":
            # The selected OAuth token must never fall through to host Keychain
            # or user settings. Use the same runtime credential, with its own home.
            if not env.get("CLAUDE_CODE_OAUTH_TOKEN"):
                raise LimitsUnavailable("unauthenticated", "credential_missing", "账户 OAuth token 缺失。")
            config = self.state_root / "_secrets" / "usage_probes" / account["account_id"]
            config.mkdir(parents=True, exist_ok=True, mode=0o700)
            env["CLAUDE_CONFIG_DIR"] = str(config.resolve())
        options = ClaudeAgentOptions(cli_path=resolve_engine_bin("claude"), env=env,
                                     cwd=str(self.state_root.resolve()), setting_sources=[],
                                     strict_mcp_config=True, mcp_servers={}, tools=[],
                                     extra_args={"no-session-persistence": None})
        transport = owned_claude_transport(options, session_id="usage:" + account["id"])
        client = ClaudeSDKClient(options=options, transport=transport)
        try:
            with anyio.fail_after(20):
                await client.connect()  # Streaming connection only, no user message.
                info = await client.get_server_info() or {}
                query = client._query
                if query is None:
                    raise LimitsUnavailable("error", "sdk_unavailable", "Claude SDK 未建立额度控制通道。")
                response = await query._send_control_request({"subtype": "get_usage", "skip_behaviors": True}, timeout=10)
                identity = info.get("account") or {}
            key = str(identity.get("email") or "").strip().lower() or env.get("CLAUDE_CODE_OAUTH_TOKEN") or ""
            return claude_windows(response), _fingerprint("claude", key) if key else None
        finally:
            await client.disconnect()

    def _grok(self, env: dict[str, str], account: dict[str, Any]) -> tuple[list, str | None]:
        selectors = ("XAI_API_KEY", "GROK_OIDC_ISSUER", "GROK_OIDC_CLIENT_ID", "GROK_OAUTH2_ISSUER",
                     "GROK_OAUTH2_CLIENT_ID", "GROK_OAUTH2_PRINCIPAL_TYPE", "GROK_OAUTH2_PRINCIPAL_ID",
                     "GROK_AUTH_PROVIDER_COMMAND", "GROK_LOCAL_AUTH", "GROK_CLI_CHAT_PROXY_BASE_URL",
                     "GROK_MODELS_BASE_URL", "GROK_CONFIG", "GROK_CONFIG_PATH")
        if any(env.get(key, "").strip() for key in selectors):
            raise LimitsUnavailable("unsupported", "custom_auth", "此 Grok 登录配置暂不支持额度读取。")
        root = Path(env.get("GROK_HOME") or Path(env.get("HOME") or Path.home()) / ".grok")
        for path in (root / "config.toml", root / "managed_config.toml", root / "requirements.toml",
                     Path("/etc/grok/managed_config.toml"), Path("/etc/grok/requirements.toml")):
            if path.exists() and re.search(r'''^\s*(?:\[\[?\s*)?["']?(?:auth|grok_com_config|endpoints)["']?\s*[.\]=]''', path.read_text(), re.M):
                raise LimitsUnavailable("unsupported", "custom_auth", "此 Grok 配置选择了其他账户或端点。")
        # GROK_AUTH is an inline credential override, so omit it for a stored home.
        raw = json.loads(env["GROK_AUTH"]) if account["source"] == "system" and env.get("GROK_AUTH") else _json_file(root / "auth.json")
        credential = raw.get("https://auth.x.ai::b1a00492-073a-47ea-816f-4c329264a828") or raw.get("https://accounts.x.ai/sign-in") or {}
        if credential.get("auth_mode") == "api_key":
            raise LimitsUnavailable("unsupported", "api_key", "Grok API key 账户不提供订阅额度。")
        token = str(credential.get("key") or "").strip()
        if not token:
            raise LimitsUnavailable("unauthenticated", "login_missing", "请先登录此 Grok 账户。")
        response = _get_json("https://cli-chat-proxy.grok.com/v1/billing?format=credits", token)
        return grok_windows(response), _fingerprint("grok", str(credential.get("email") or "").strip().lower() or token)

    def _opencode(self, env: dict[str, str], account: dict[str, Any]) -> tuple[list, str | None]:
        if env.get("OPENCODE_SERVER_URL") or env.get("OPENAI_BASE_URL"):
            raise LimitsUnavailable("unsupported", "external_server", "外部 OpenCode 服务或自定义端点的额度暂未支持。")
        if account["source"] == "stored":
            # An OpenCode runtime can target many providers. Only an explicitly
            # identified Go/Zen credential may be sent to opencode.ai.
            if account.get("provider") not in {"opencode-go", "opencode", "opencode-zen"}:
                raise LimitsUnavailable("unsupported", "go_credential_required", "此凭据未标记为 OpenCode Go / Zen，暂不查询订阅额度。")
            token = env.get("OPENCODE_API_KEY", "").strip()
        else:
            data = Path(env.get("XDG_DATA_HOME") or Path(env.get("HOME") or Path.home()) / ".local" / "share")
            raw = json.loads(env["OPENCODE_AUTH_CONTENT"]) if env.get("OPENCODE_AUTH_CONTENT") else _json_file(data / "opencode" / "auth.json")
            credential = raw.get("opencode-go") or {}
            token = str((credential.get("key") if credential.get("type") == "api" else env.get("OPENCODE_API_KEY")) or "").strip()
        if not token:
            raise LimitsUnavailable("unsupported", "go_subscription_required", "暂未检测到 OpenCode Go 订阅登录。")
        response = _get_json("https://opencode.ai/zen/go/v1/usage", token, go=True)
        return opencode_windows(response), _fingerprint("opencode-go", token)
