"""Service-owned access settings and sessions for Web, desktop and API clients.

Only password hashes and signing material are persisted under state/_secrets.
Web sessions use HttpOnly cookies; native/API clients use audience-bound tokens.
SSE/WS still use short-lived, single-use tickets.
"""
from __future__ import annotations

import asyncio
import base64
import hashlib
import hmac
import json
import logging
import os
import secrets
import tempfile
import threading
import time
from contextlib import contextmanager
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Optional
from urllib.parse import parse_qs

from starlette.requests import HTTPConnection

PASSWORD_ENV = "MUTEKI_WEB_PASSWORD"
BIND_ENV = "MUTEKI_WEB_BIND"
TTL_ENV = "MUTEKI_WEB_TOKEN_TTL"
SECRET_ENV = "MUTEKI_WEB_AUTH_SECRET"
DEFAULT_TTL_S = 12 * 3600
PUBLIC_API_PATHS = frozenset({"/api/auth/login", "/api/auth/status", "/api/health"})
_LOOPBACK_HOSTS = frozenset({"127.0.0.1", "::1", "localhost", "0:0:0:0:0:0:0:1", ""})


def is_loopback_host(host: Optional[str]) -> bool:
    h = (host or "").strip().lower()
    if h.startswith("[") and "]" in h:
        h = h[1:h.index("]")]
    elif h.count(":") == 1:
        h = h.split(":", 1)[0]
    return h in _LOOPBACK_HOSTS


def _b64u(raw: bytes) -> str:
    return base64.urlsafe_b64encode(raw).rstrip(b"=").decode("ascii")


def _b64u_decode(value: str) -> bytes:
    return base64.b64decode(value + "=" * (-len(value) % 4), altchars=b"-_", validate=True)


def _password_hash(password: str) -> str:
    salt = secrets.token_bytes(16)
    key = hashlib.scrypt(password.encode("utf-8"), salt=salt, n=32768, r=8, p=1, maxmem=64 * 1024 * 1024)
    return f"scrypt-v1${_b64u(salt)}${_b64u(key)}"


def _valid_hash(value: str) -> bool:
    try:
        version, salt, digest = value.split("$")
        return version == "scrypt-v1" and len(_b64u_decode(salt)) == 16 and len(_b64u_decode(digest)) == 64
    except (ValueError, TypeError):
        return False


def _matches_hash(value: str, password: str) -> bool:
    if not _valid_hash(value):
        return False
    _, salt, digest = value.split("$")
    key = hashlib.scrypt(password.encode("utf-8"), salt=_b64u_decode(salt), n=32768, r=8, p=1, maxmem=64 * 1024 * 1024)
    return hmac.compare_digest(key, _b64u_decode(digest))


def _read_private(path: Path) -> dict[str, Any]:
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
        valid = (isinstance(data, dict) and data.get("version") == 1
                 and data.get("mode") in {"environment", "settings"}
                 and len(_b64u_decode(data["signing_key"])) == 32
                 and isinstance(data["revision"], str) and len(data["revision"]) >= 32
                 and all(isinstance(data[k], str) and (not data[k] or _valid_hash(data[k])) for k in ("password_hash", "environment_hash"))
                 and (data["mode"] != "settings" or bool(data["password_hash"]))
                 and isinstance(data["revoked"], dict)
                 and all(isinstance(k, str) and isinstance(v, int) for k, v in data["revoked"].items())
                 and isinstance(data["secret_fingerprint"], str))
        if not valid:
            raise ValueError("invalid access configuration")
        return data
    except (OSError, ValueError, TypeError, KeyError) as exc:
        raise RuntimeError(f"访问配置无法读取：{path}。请修复配置后重启，未回退到环境变量。") from exc


def _file_version(path: Path) -> tuple[int, int, int]:
    stat = path.stat()
    return stat.st_ino, stat.st_mtime_ns, stat.st_size


@contextmanager
def _file_guard(path: Path | None):
    if path is None:
        yield
        return
    path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
    with path.with_suffix(".lock").open("a+b") as lock:
        os.chmod(lock.name, 0o600)
        if os.name == "nt":
            import msvcrt
            if lock.seek(0, 2) == 0:
                lock.write(b"\0")
                lock.flush()
            lock.seek(0)
            msvcrt.locking(lock.fileno(), msvcrt.LK_LOCK, 1)
        else:
            import fcntl
            fcntl.flock(lock.fileno(), fcntl.LOCK_EX)
        try:
            yield
        finally:
            if os.name == "nt":
                lock.seek(0)
                msvcrt.locking(lock.fileno(), msvcrt.LK_UNLCK, 1)
            else:
                fcntl.flock(lock.fileno(), fcntl.LOCK_UN)


def _write_private(path: Path, data: dict[str, Any]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
    with tempfile.NamedTemporaryFile(mode="w", encoding="utf-8", dir=path.parent, delete=False) as tmp:
        temporary = Path(tmp.name)
        try:
            os.chmod(temporary, 0o600)
            json.dump(data, tmp, separators=(",", ":"))
            tmp.flush()
            os.fsync(tmp.fileno())
            os.replace(temporary, path)
        finally:
            temporary.unlink(missing_ok=True)


@dataclass
class AuthConfig:
    password: str = field(default="", repr=False)  # compatibility for an in-memory caller; never persisted
    bind_host: str = ""
    ui_bind_host: str = ""
    ttl_s: int = DEFAULT_TTL_S
    secret: bytes = field(default=b"", repr=False)
    service_id: str = "local"
    _path: Path | None = field(default=None, repr=False)
    _state: dict[str, Any] = field(default_factory=dict, repr=False)
    _path_version: tuple[int, int, int] | None = field(default=None, repr=False)
    _explicit_secret: str = field(default="", repr=False)
    _lock: Any = field(default_factory=threading.RLock, repr=False)
    _failures: dict[str, list[float]] = field(default_factory=dict, repr=False)

    @property
    def enabled(self) -> bool:
        self.refresh()
        return bool(self.effective_hash or self.password) or os.environ.get("MUTEKI_MANAGED_DESKTOP") == "1"

    @property
    def effective_hash(self) -> str:
        return str(self._state.get("password_hash" if self.source == "settings" else "environment_hash", ""))

    @property
    def source(self) -> str:
        return "settings" if self._state.get("mode") == "settings" else "environment"

    @property
    def revision(self) -> str:
        self.refresh()
        return str(self._state.get("revision", "ephemeral"))

    @property
    def cookie_name(self) -> str:
        return "muteki_session_" + hashlib.sha256(self.service_id.encode()).hexdigest()[:16]

    @classmethod
    def from_env(cls, state_root: Path | str | None = None, *, service_id: str = "local") -> "AuthConfig":
        password = (os.environ.get(PASSWORD_ENV) or "").strip()
        explicit = (os.environ.get(SECRET_ENV) or "").strip()
        try:
            ttl = int(os.environ.get(TTL_ENV) or "")
        except (ValueError, TypeError):
            ttl = DEFAULT_TTL_S
        ttl = ttl if ttl > 0 else DEFAULT_TTL_S
        path = Path(state_root) / "_secrets" / "web-access.json" if state_root is not None else None
        with _file_guard(path):
            existed = path is not None and path.exists()
            if existed:
                data = _read_private(path)
                os.chmod(path, 0o600)
            else:
                data = {"version": 1, "mode": "environment", "password_hash": "", "environment_hash": "",
                        "signing_key": _b64u(secrets.token_bytes(32)), "revision": secrets.token_hex(24),
                        "revoked": {}, "secret_fingerprint": ""}
            env_changed = bool(password) != bool(data["environment_hash"]) or bool(password and not _matches_hash(data["environment_hash"], password))
            secret_fingerprint = hashlib.sha256(explicit.encode()).hexdigest() if explicit else ""
            if env_changed:
                data["environment_hash"] = _password_hash(password) if password else ""
            secret_changed = secret_fingerprint != data["secret_fingerprint"]
            if (env_changed and data["mode"] == "environment") or secret_changed:
                data["revision"] = secrets.token_hex(24)
                data["revoked"] = {}
            data["secret_fingerprint"] = secret_fingerprint
            if path is not None and (not existed or env_changed or secret_changed):
                _write_private(path, data)
            key = _b64u_decode(data["signing_key"])
            # Explicit secrets supplement the per-installation random key.
            secret = hmac.digest(key, explicit.encode(), "sha256")
            return cls(bind_host=(os.environ.get(BIND_ENV) or "").strip(), ttl_s=ttl,
                       secret=secret, service_id=service_id, _explicit_secret=explicit, _path_version=_file_version(path) if path is not None else None, ui_bind_host=(os.environ.get("MUTEKI_UI_HOST") or "").strip(), _path=path, _state=data)

    def refresh(self) -> None:
        if self._path is None:
            return
        with self._lock:
            try:
                version = _file_version(self._path)
            except OSError as exc:
                raise RuntimeError("访问配置不可用，服务已拒绝继续认证。") from exc
            if version == self._path_version:
                return
            data = _read_private(self._path)
            fingerprint = hashlib.sha256(self._explicit_secret.encode()).hexdigest() if self._explicit_secret else ""
            if fingerprint != data["secret_fingerprint"]:
                raise RuntimeError("认证签名环境已变更，请重启当前服务进程。")
            self.secret = hmac.digest(_b64u_decode(data["signing_key"]), self._explicit_secret.encode(), "sha256")
            self._state, self._path_version = data, version

    @property
    def can_disable(self) -> bool:
        return os.environ.get("MUTEKI_MANAGED_DESKTOP") != "1" and is_loopback_host(self.bind_host) and is_loopback_host(self.ui_bind_host)

    def fail_fast_check(self) -> None:
        if not self.enabled and not self.can_disable:
            raise RuntimeError(f"Refusing to start: non-loopback bind {self.bind_host!r} requires an access password. Set {PASSWORD_ENV}, or configure a password in Settings on loopback first.")

    def settings(self) -> dict[str, Any]:
        with self._lock:
            self.refresh()
            return {"auth_required": self.enabled, "source": self.source if self.enabled else "none",
                    "environment_configured": bool(self._state.get("environment_hash") or self.password),
                    "has_override": self.source == "settings", "session_ttl_seconds": self.ttl_s,
                    "can_disable": self.can_disable, "service_id": self.service_id}

    def update_password(self, *, action: str, current_password: str = "", new_password: str = "") -> None:
        with self._lock, _file_guard(self._path):
            self.refresh()
            if (self.effective_hash or self.password) and not check_password(self, current_password):
                raise AccessSettingsError("auth.current_password_invalid", "当前密码不正确。", "current_password", 403)
            if action == "set":
                try:
                    valid_password = isinstance(new_password, str) and len(new_password) >= 8 and len(new_password.encode()) <= 1024
                except UnicodeError:
                    valid_password = False
                if not valid_password:
                    raise AccessSettingsError("auth.password_invalid", "新密码需为 8 个字符以上，且不超过 1024 字节。", "new_password", 422)
                password_hash = _password_hash(new_password)
                mode = "settings"
            elif action == "inherit":
                if not self._state.get("environment_hash") and not self.can_disable:
                    raise AccessSettingsError("auth.password_required", "当前服务对外开放，恢复环境变量前需先配置环境变量密码。", "action", 409)
                password_hash, mode = "", "environment"
            else:
                raise AccessSettingsError("auth.action_invalid", "请选择设置密码或恢复环境变量。", "action", 422)
            candidate = {**self._state, "mode": mode, "password_hash": password_hash,
                         "revision": secrets.token_hex(24), "revoked": {}}
            if self._path is not None:
                _write_private(self._path, candidate)
                self._path_version = _file_version(self._path)
            self._state = candidate

    def revoke(self, token: str | None) -> None:
        with self._lock, _file_guard(self._path):
            self.refresh()
            payload = token_payload(self, token)
            if payload is None:
                return
            revoked = {k: v for k, v in self._state.get("revoked", {}).items() if v > time.time()}
            revoked[payload["jti"]] = payload["exp"]
            candidate = {**self._state, "revoked": revoked}
            if self._path is not None:
                _write_private(self._path, candidate)
                self._path_version = _file_version(self._path)
            self._state = candidate

    def allow_login(self, peer: str) -> bool:
        with self._lock:
            now = time.monotonic()
            self._failures = {k: [ts for ts in v if now - ts < 60] for k, v in self._failures.items() if v and now - v[-1] < 60}
            return len(self._failures.get(peer, [])) < 10 and len(self._failures) < 2048

    def login_failed(self, peer: str) -> None:
        with self._lock:
            self._failures.setdefault(peer, []).append(time.monotonic())


class AccessSettingsError(ValueError):
    def __init__(self, code: str, message: str, field: str, status: int):
        super().__init__(message)
        self.code, self.field, self.status = code, field, status


def issue_token(cfg: AuthConfig, *, now: Optional[float] = None) -> str:
    ts = int(now if now is not None else time.time())
    with cfg._lock:
        payload = json.dumps({"v": 2, "aud": cfg.service_id, "rev": cfg.revision,
                              "iat": ts, "exp": ts + cfg.ttl_s, "jti": secrets.token_urlsafe(24)}, separators=(",", ":")).encode()
        return _b64u(payload) + "." + _b64u(hmac.digest(cfg.secret, payload, "sha256"))


def token_payload(cfg: AuthConfig, token: str | None, *, now: float | None = None) -> dict[str, Any] | None:
    if not isinstance(token, str) or not token or len(token) > 4096:
        return None
    try:
        payload_b64, sig_b64 = token.split(".", 1)
        payload, sig = _b64u_decode(payload_b64), _b64u_decode(sig_b64)
        with cfg._lock:
            cfg.refresh()
            if not hmac.compare_digest(sig, hmac.digest(cfg.secret, payload, "sha256")):
                return None
            data = json.loads(payload)
            ts = now if now is not None else time.time()
            if (data.get("v") != 2 or data.get("aud") != cfg.service_id or data.get("rev") != cfg.revision
                    or not isinstance(data.get("exp"), int) or not isinstance(data.get("iat"), int)
                    or not isinstance(data.get("jti"), str) or not data["jti"]
                    or ts >= data["exp"] or data["iat"] > ts + 30
                    or data["jti"] in cfg._state.get("revoked", {})):
                return None
            return data
    except (ValueError, TypeError, AttributeError):
        return None


def verify_token(cfg: AuthConfig, token: Optional[str], *, now: Optional[float] = None) -> bool:
    return token_payload(cfg, token, now=now) is not None


def check_password(cfg: AuthConfig, password: Optional[str]) -> bool:
    if not isinstance(password, str):
        return False
    try:
        if len(password.encode("utf-8")) > 1024:
            return False
    except UnicodeError:
        return False
    with cfg._lock:
        cfg.refresh()
        if cfg.effective_hash:
            return _matches_hash(cfg.effective_hash, password)
        return bool(cfg.password) and hmac.compare_digest(password.encode(), cfg.password.encode())


def bearer_from_header(authorization: Optional[str]) -> Optional[str]:
    if not authorization:
        return None
    parts = authorization.split(None, 1)
    return parts[1].strip() if len(parts) == 2 and parts[0].lower() == "bearer" else None


def request_token(cfg: AuthConfig, request: Any) -> str | None:
    return bearer_from_header(request.headers.get("Authorization")) or request.cookies.get(cfg.cookie_name)


def allowed_web_origins() -> set[str]:
    port = os.environ.get("MUTEKI_UI_PORT", "3001")
    origins = {f"http://127.0.0.1:{port}", f"http://localhost:{port}", f"http://[::1]:{port}"}
    origins.update(value.strip().rstrip("/") for value in os.environ.get("MUTEKI_WEB_ORIGINS", "").split(",") if value.strip())
    return origins


def browser_request_error(cfg: AuthConfig, request: Any) -> str | None:
    """Bearer clients are independent of cookies. Browser mutations require CSRF proof."""
    if request.method in {"GET", "HEAD", "OPTIONS"}:
        return None
    origin = request.headers.get("origin")
    if origin and origin != str(request.base_url).rstrip("/") and origin not in allowed_web_origins():
        return "请求来源不受信任，请从工作台页面重试。"
    cookie = request.cookies.get(cfg.cookie_name)
    if cookie and not bearer_from_header(request.headers.get("Authorization")) and request.headers.get("X-Muteki-CSRF") != "1":
        return "缺少访问校验，请刷新工作台后重试。"
    if origin and not bearer_from_header(request.headers.get("Authorization")) and request.headers.get("X-Muteki-CSRF") != "1":
        return "缺少访问校验，请刷新工作台后重试。"
    return None


@dataclass
class TicketStore:
    ttl_s: float = 30.0
    _tickets: dict[str, float] = field(default_factory=dict)
    _bindings: dict[str, str] = field(default_factory=dict, repr=False)

    def mint(self, *, now: Optional[float] = None, token: str | None = None) -> str:
        ts = now if now is not None else time.time()
        self._evict(ts)
        ticket = secrets.token_urlsafe(32)
        self._tickets[ticket] = ts + self.ttl_s
        if token:
            self._bindings[ticket] = token
        return ticket

    def redeem(self, ticket: Optional[str], *, now: Optional[float] = None, scope: dict | None = None) -> bool:
        if not ticket:
            return False
        ts = now if now is not None else time.time()
        self._evict(ts)
        exp = self._tickets.pop(ticket, None)
        token = self._bindings.pop(ticket, None)
        valid = exp is not None and ts < exp
        if valid and token and scope is not None:
            scope["muteki_auth_session"] = token
        return valid

    def clear(self) -> None:
        self._tickets.clear()
        self._bindings.clear()

    def _evict(self, now: float) -> None:
        for key in [k for k, expiry in self._tickets.items() if expiry <= now]:
            self._tickets.pop(key, None)
            self._bindings.pop(key, None)


class _SessionEnded(Exception):
    pass


class ServiceSessionMiddleware:
    """Expire established SSE/WS sessions as well as subsequent API requests."""

    def __init__(self, app: Any, config: AuthConfig):
        self.app, self.config = app, config

    async def __call__(self, scope: dict, receive: Any, send: Any) -> None:
        if scope["type"] not in {"http", "websocket"} or not scope.get("path", "").startswith("/api/"):
            await self.app(scope, receive, send)
            return
        cfg = self.config
        revision = cfg.revision
        connection = HTTPConnection(scope)
        query = parse_qs(scope.get("query_string", b"").decode("ascii", errors="replace"))
        token = request_token(cfg, connection) or (query.get("token") or [None])[0]
        started, ended, revoked = False, False, False
        monitor = None
        owner = asyncio.current_task()

        def valid() -> bool:
            with cfg._lock:
                try:
                    return revision == cfg.revision and (not cfg.enabled or verify_token(cfg, scope.get("muteki_auth_session") or token))
                except RuntimeError as exc:
                    logging.getLogger(__name__).error("auth.config_unavailable: %s", exc)
                    return False

        async def watch() -> None:
            nonlocal revoked
            while True:
                await asyncio.sleep(1)
                if not valid():
                    revoked = True
                    owner.cancel()
                    return

        async def guarded_send(message: dict) -> None:
            nonlocal started, ended, monitor
            if message["type"] == "websocket.accept" or (message["type"] == "http.response.start"
                    and any(name.lower() == b"content-type" and b"text/event-stream" in value for name, value in message.get("headers", []))):
                started = True
                monitor = asyncio.create_task(watch())
            if started and message["type"] in {"http.response.body", "websocket.send"} and not valid():
                raise _SessionEnded()
            await send(message)
            if message["type"] == "websocket.close" or (message["type"] == "http.response.body" and not message.get("more_body", False)):
                ended = True

        async def close() -> None:
            if not started or ended:
                return
            if scope["type"] == "websocket":
                await send({"type": "websocket.close", "code": 4401, "reason": "Session expired"})
            else:
                await send({"type": "http.response.body", "body": b"", "more_body": False})

        try:
            await self.app(scope, receive, guarded_send)
        except _SessionEnded:
            await close()
        except asyncio.CancelledError:
            if not revoked:
                raise
            await close()
        finally:
            if monitor is not None:
                monitor.cancel()
                await asyncio.gather(monitor, return_exceptions=True)
