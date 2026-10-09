"""Cursor account usage read with the Cursor CLI login of this host.

Cursor bills the desktop app, the CLI and background agents against one
account, so its DashboardService API provides the complete history; Muteki's
own ledger only sees the Cursor runs it started. The CLI login lives in the
macOS Keychain (``cursor-access-token`` / ``cursor-user``), or in ``auth.json``
when file storage is selected or on other platforms. CLI access tokens use
Bearer authentication, not the website's Workos session cookie. Missing
authorization returns a typed error and the read has a bounded timeout.
"""
from __future__ import annotations

import base64
import ctypes
import hashlib
import json
import math
import os
import subprocess
import sys
import threading
import time
from concurrent.futures import Future, TimeoutError as FutureTimeout
from dataclasses import dataclass
from datetime import datetime, timedelta, timezone
from pathlib import Path
from typing import Any, Callable, Mapping
from zoneinfo import ZoneInfo, ZoneInfoNotFoundError

import httpx

KEYCHAIN_SERVICE = "cursor-access-token"
KEYCHAIN_ACCOUNT = "cursor-user"
LIMITS_URL = "https://api2.cursor.sh/aiserver.v1.DashboardService/GetCurrentPeriodUsage"
EVENTS_URL = "https://api2.cursor.sh/aiserver.v1.DashboardService/GetFilteredUsageEvents"
PAGE_SIZE = 1000
TOKEN_CACHE_SECONDS = 300
KEYCHAIN_WAIT_SECONDS = 30
REQUEST_TIMEOUT_SECONDS = 10
HISTORY_DEADLINE_SECONDS = 60
RESULT_CACHE_SECONDS = 60
WINDOW_DAYS = (1, 7, 30, 90)
SETTINGS_FILE = "_usage_settings.json"

# Cursor's plan pools. Overall combines the other two; it is not a third quota.
LIMIT_WINDOWS = (
    ("totalPercentUsed", ("总体", "Overall"),
     ("两个额度池的合计用量，不是第三个额度。", "Combined usage across both pools, not a third quota.")),
    ("autoPercentUsed", ("Cursor 模型", "Cursor models"),
     ("Grok 与 Composer 优先使用这个池；Auto 可使用任一池。", "Grok and Composer use this pool first. Auto can use either pool.")),
    ("apiPercentUsed", ("其他模型", "Other models"),
     ("Claude、GPT、Gemini 使用这个池；Grok 与 Composer 用完自己的池后也会落到这里。", "Claude, GPT and Gemini use this pool. Grok and Composer fall back to it.")),
)

_ERR_SEC_ITEM_NOT_FOUND = -25300
_ERR_SEC_USER_CANCELED = -128
_ERR_SEC_AUTH_FAILED = -25293
_ERR_SEC_INTERACTION_NOT_ALLOWED = -25308


class CursorUsageError(Exception):
    """A typed failure; ``code`` drives the UI and ``message`` is shown as-is.

    Messages never include tokens or response bodies.
    """

    def __init__(self, code: str, message: str, *, action: str | None = None):
        super().__init__(message)
        self.code = code
        self.message = message
        self.action = action

    def view(self) -> dict[str, Any]:
        return {"code": self.code, "message": self.message, **({"action": self.action} if self.action else {})}


# ── macOS Keychain ──────────────────────────────────────────────────────────

class _Security:
    """Security.framework metadata lookup, without requesting the secret."""

    def __init__(self) -> None:
        self._interaction_lock = threading.Lock()
        self.sec = ctypes.CDLL("/System/Library/Frameworks/Security.framework/Security")
        self.cf = ctypes.CDLL("/System/Library/Frameworks/CoreFoundation.framework/CoreFoundation")
        cf, sec = self.cf, self.sec
        cf.CFStringCreateWithCString.restype = ctypes.c_void_p
        cf.CFStringCreateWithCString.argtypes = [ctypes.c_void_p, ctypes.c_char_p, ctypes.c_uint32]
        cf.CFDictionaryCreate.restype = ctypes.c_void_p
        cf.CFDictionaryCreate.argtypes = [
            ctypes.c_void_p, ctypes.POINTER(ctypes.c_void_p), ctypes.POINTER(ctypes.c_void_p),
            ctypes.c_long, ctypes.c_void_p, ctypes.c_void_p,
        ]
        cf.CFDataGetLength.restype = ctypes.c_long
        cf.CFDataGetLength.argtypes = [ctypes.c_void_p]
        cf.CFDataGetBytePtr.restype = ctypes.c_void_p
        cf.CFDataGetBytePtr.argtypes = [ctypes.c_void_p]
        cf.CFRelease.restype = None
        cf.CFRelease.argtypes = [ctypes.c_void_p]
        sec.SecItemCopyMatching.restype = ctypes.c_int32
        sec.SecItemCopyMatching.argtypes = [ctypes.c_void_p, ctypes.POINTER(ctypes.c_void_p)]
        sec.SecKeychainGetUserInteractionAllowed.restype = ctypes.c_int32
        sec.SecKeychainGetUserInteractionAllowed.argtypes = [ctypes.POINTER(ctypes.c_uint8)]
        sec.SecKeychainSetUserInteractionAllowed.restype = ctypes.c_int32
        sec.SecKeychainSetUserInteractionAllowed.argtypes = [ctypes.c_uint8]

    def _constant(self, library: ctypes.CDLL, name: str) -> int:
        return ctypes.c_void_p.in_dll(library, name).value

    def copy_matching(self, service: str, account: str, *, return_data: bool) -> tuple[int, bytes | None]:
        cf, sec = self.cf, self.sec
        utf8 = 0x08000100
        service_ref = cf.CFStringCreateWithCString(None, service.encode(), utf8)
        account_ref = cf.CFStringCreateWithCString(None, account.encode(), utf8)
        keys = [self._constant(sec, "kSecClass"), self._constant(sec, "kSecAttrService"),
                self._constant(sec, "kSecAttrAccount"), self._constant(sec, "kSecMatchLimit"),
                self._constant(sec, "kSecReturnData" if return_data else "kSecReturnAttributes"),
                self._constant(sec, "kSecUseAuthenticationUI")]
        values = [self._constant(sec, "kSecClassGenericPassword"), service_ref, account_ref,
                  self._constant(sec, "kSecMatchLimitOne"), self._constant(cf, "kCFBooleanTrue"),
                  self._constant(sec, "kSecUseAuthenticationUIFail")]
        array = ctypes.c_void_p * len(keys)
        query = cf.CFDictionaryCreate(
            None, array(*keys), array(*values), len(keys),
            ctypes.addressof(ctypes.c_void_p.in_dll(cf, "kCFTypeDictionaryKeyCallBacks")),
            ctypes.addressof(ctypes.c_void_p.in_dll(cf, "kCFTypeDictionaryValueCallBacks")),
        )
        result = ctypes.c_void_p()
        try:
            # Cursor uses the legacy login keychain, which can ignore the
            # modern query UI flag. Disable interaction for this process only
            # during the read, and restore it under a lock for other readers.
            with self._interaction_lock:
                previous = ctypes.c_uint8()
                get_status = sec.SecKeychainGetUserInteractionAllowed(ctypes.byref(previous))
                set_status = sec.SecKeychainSetUserInteractionAllowed(False) if get_status == 0 else get_status
                if set_status != 0:
                    raise CursorUsageError(
                        "keychain_noninteractive_unavailable",
                        f"无法启用非交互钥匙串读取（OSStatus {set_status}）。")
                try:
                    status = sec.SecItemCopyMatching(query, ctypes.byref(result))
                finally:
                    restore_status = sec.SecKeychainSetUserInteractionAllowed(previous.value)
                    if restore_status != 0:
                        raise CursorUsageError(
                            "keychain_noninteractive_unavailable",
                            f"无法恢复钥匙串读取设置（OSStatus {restore_status}）。")
        finally:
            for ref in (query, service_ref, account_ref):
                cf.CFRelease(ref)
        if status != 0 or not result.value:
            return status, None
        try:
            if not return_data:
                return status, b""
            length = cf.CFDataGetLength(result.value)
            return status, ctypes.string_at(cf.CFDataGetBytePtr(result.value), length)
        finally:
            cf.CFRelease(result.value)


_security: _Security | None = None
_security_lock = threading.Lock()


def _security_api() -> _Security:
    global _security
    with _security_lock:
        if _security is None:
            _security = _Security()
        return _security


def keychain_login_present() -> bool:
    """Whether the Cursor CLI login exists, read from attributes only (no prompt)."""
    status, _ = _security_api().copy_matching(KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT, return_data=False)
    if status in (0, _ERR_SEC_INTERACTION_NOT_ALLOWED):
        return True
    if status == _ERR_SEC_ITEM_NOT_FOUND:
        return False
    raise CursorUsageError("keychain_failed", f"无法检查 macOS 钥匙串（OSStatus {status}）。")


def _read_keychain_token() -> str | None:
    # Cursor CLI created this item with /usr/bin/security in its trusted app
    # list. Use that existing read authorization as the single data path;
    # never change the item's ACL or try another credential source on failure.
    # Capture -w privately: it must never reach a terminal, log, or error body.
    try:
        result = subprocess.run(
            ["/usr/bin/security", "find-generic-password", "-s", KEYCHAIN_SERVICE,
             "-a", KEYCHAIN_ACCOUNT, "-w"],
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=5,
            check=False,
        )
    except subprocess.TimeoutExpired:
        raise CursorUsageError(
            "keychain_timeout", "Cursor 钥匙串读取超时，其他功能可继续使用。") from None
    except OSError as exc:
        raise CursorUsageError(
            "keychain_failed", f"无法启动 macOS 钥匙串读取程序（{type(exc).__name__}）。") from None
    # security exits with the low byte of its native OSStatus. Classify only
    # numeric codes; its error wording is not part of the control protocol.
    status = result.returncode
    if status == _ERR_SEC_ITEM_NOT_FOUND % 256:
        return None
    if status == _ERR_SEC_INTERACTION_NOT_ALLOWED % 256:
        raise CursorUsageError(
            "keychain_interaction_required",
            "Cursor 登录项需要 macOS 钥匙串授权，当前读取不可用。")
    if status in (_ERR_SEC_USER_CANCELED % 256, _ERR_SEC_AUTH_FAILED % 256):
        raise CursorUsageError(
            "keychain_denied",
            "macOS 钥匙串未授权非交互访问 Cursor 登录项；账号用量暂不可读，其他功能可继续使用。")
    if status != 0:
        detail = result.stderr.decode("utf-8", errors="replace").strip()
        raise CursorUsageError("keychain_failed", f"读取 macOS 钥匙串失败（exit {status}）。{detail}")
    try:
        return result.stdout.decode("utf-8").strip() or None
    except UnicodeDecodeError:
        raise CursorUsageError(
            "token_invalid", "钥匙串中的 Cursor 登录凭据不是有效文本。重新执行 cursor-agent login 后刷新。") from None


class KeychainTokenReader:
    """One Keychain read shared by limits and history.

    Concurrent callers join the same bounded read. Successful credentials
    remain only in memory and expire after TOKEN_CACHE_SECONDS.
    """

    def __init__(self, read: Callable[[], str | None] = _read_keychain_token, *,
                 clock: Callable[[], float] = time.monotonic,
                 wait_seconds: float = KEYCHAIN_WAIT_SECONDS) -> None:
        self._read = read
        self._clock = clock
        self._wait = wait_seconds
        self._lock = threading.Lock()
        self._cached: tuple[str, float] | None = None
        self._pending: Future | None = None

    def forget(self) -> None:
        with self._lock:
            self._cached = None

    def read(self) -> str | None:
        with self._lock:
            if self._cached and self._cached[1] > self._clock():
                return self._cached[0]
            if self._pending is None:
                self._pending = Future()
                threading.Thread(target=self._run, args=(self._pending,), name="cursor-keychain", daemon=True).start()
            pending = self._pending
        try:
            return pending.result(timeout=self._wait)
        except FutureTimeout:
            raise CursorUsageError(
                "keychain_timeout", "Cursor 钥匙串读取超时，其他功能可继续使用。") from None

    def _run(self, future: Future) -> None:
        try:
            token = self._read()
        except BaseException as exc:  # delivered to every waiter, not swallowed
            with self._lock:
                self._pending = None
            future.set_exception(exc)
            return
        with self._lock:
            self._pending = None
            self._cached = (token, self._clock() + TOKEN_CACHE_SECONDS) if token else None
        future.set_result(token)


# ── Login resolution ────────────────────────────────────────────────────────

@dataclass(frozen=True)
class CursorLogin:
    token: str
    source: str  # "env" | "keychain" | "auth_file"
    subject: str

    @property
    def account_key(self) -> str:
        return hashlib.sha256(self.subject.encode()).hexdigest()


def uses_keychain(env: Mapping[str, str], platform: str) -> bool:
    """macOS logins live in the Keychain unless the CLI was told to use files."""
    return (platform == "darwin" and env.get("AGENT_CLI_CREDENTIAL_STORE", "") not in {"file", "memory"}
            and not env.get("CURSOR_AUTH_TOKEN", "").strip()
            and not env.get("CURSOR_API_KEY", "").strip())


def auth_file_path(env: Mapping[str, str], platform: str, home: Path) -> Path:
    home = Path(env.get("USERPROFILE" if platform == "win32" else "HOME") or home)
    if platform == "darwin":
        return home / ".cursor" / "auth.json"
    if platform == "win32":
        return Path(env.get("APPDATA") or home / "AppData" / "Roaming") / "Cursor" / "auth.json"
    config = env.get("XDG_CONFIG_HOME", "").strip()
    return (Path(config) if config and Path(config).is_absolute() else home / ".config") / "cursor" / "auth.json"


def _jwt_subject(token: str) -> str:
    try:
        payload = token.split(".")[1]
        claims = json.loads(base64.urlsafe_b64decode(payload + "=" * (-len(payload) % 4)))
        subject = claims.get("sub") if isinstance(claims, dict) else None
    except (IndexError, ValueError, UnicodeDecodeError):
        subject = None
    if not isinstance(subject, str) or not subject.split("|")[-1]:
        raise CursorUsageError("token_invalid", "Cursor 登录凭据格式无法识别。重新执行 cursor-agent login 后刷新。")
    return subject


def resolve_login(*, keychain_enabled: bool, keychain: KeychainTokenReader,
                  env: Mapping[str, str] = os.environ, platform: str = sys.platform,
                  home: Path | None = None) -> CursorLogin:
    # UsageService in current T3 nightly excludes environment credentials and
    # in-memory CLI logins. History belongs to the host's persisted CLI login;
    # it must not switch accounts based on an engine's injected credential.
    if env.get("CURSOR_AUTH_TOKEN", "").strip():
        raise CursorUsageError("login_unavailable", "服务进程设置了 CURSOR_AUTH_TOKEN；账号历史需要此服务上的持久化 Cursor CLI 登录。")
    if env.get("CURSOR_API_KEY", "").strip():
        raise CursorUsageError(
            "api_key_only",
            "服务进程设置了 CURSOR_API_KEY，它可能属于另一个账号。读取账号用量需要此服务上的持久化 Cursor CLI 登录。")
    store = env.get("AGENT_CLI_CREDENTIAL_STORE", "")
    if store == "memory":
        raise CursorUsageError(
            "login_missing", "AGENT_CLI_CREDENTIAL_STORE=memory 时 Cursor CLI 不保存登录；账号历史需要持久化 CLI 登录。")
    if uses_keychain(env, platform):
        if not keychain_enabled:
            raise CursorUsageError(
                "keychain_disabled",
                "Cursor 账号用量未开启。开启后 Muteki 会使用已有的钥匙串读取授权获取 Cursor CLI 登录。",
                action="enable_cursor_keychain")
        token = (keychain.read() or "").strip()
        source = "keychain"
        if not token:
            raise CursorUsageError(
                "login_missing", "钥匙串中没有 Cursor CLI 登录。在运行 Muteki 服务的电脑上执行 cursor agent login 完成浏览器登录后刷新；引擎的 API key 不等于账号用量登录。")
    else:
        path = auth_file_path(env, platform, home or Path.home())
        source = "auth_file"
        try:
            raw = json.loads(path.read_text(encoding="utf-8"))
        except FileNotFoundError:
            raise CursorUsageError(
                "login_missing", f"未找到 Cursor CLI 登录（{path}）。执行 cursor-agent login 后刷新。") from None
        except (OSError, ValueError) as exc:
            raise CursorUsageError("auth_file_unreadable", f"无法读取 {path}：{type(exc).__name__}") from None
        token = str(raw.get("accessToken") or "").strip() if isinstance(raw, dict) else ""
        if not token:
            raise CursorUsageError("login_missing", f"{path} 中没有 Cursor 登录。执行 cursor-agent login 后刷新。")
    return CursorLogin(token=token, source=source, subject=_jwt_subject(token))


# ── Dashboard API ───────────────────────────────────────────────────────────

class _SignedOut(Exception):
    def __init__(self, status: int):
        self.status = status


def _post(client: httpx.Client, url: str, **kwargs: Any) -> Any:
    try:
        response = client.post(url, **kwargs)
    except httpx.TimeoutException:
        raise CursorUsageError("request_timeout", "Cursor 账号接口请求超时。") from None
    except httpx.HTTPError as exc:
        raise CursorUsageError("request_failed", f"无法连接 Cursor 账号接口（{type(exc).__name__}）。") from None
    if response.status_code in (401, 403):
        raise _SignedOut(response.status_code)
    if response.is_redirect:
        raise CursorUsageError(
            "request_redirected", f"Cursor 账号接口返回 HTTP {response.status_code} 重定向，未转发登录凭据。")
    if response.status_code != 200:
        raise CursorUsageError("request_failed", f"Cursor 账号接口返回 HTTP {response.status_code}。")
    try:
        return response.json()
    except ValueError:
        raise CursorUsageError("invalid_response", "Cursor 账号接口返回的不是 JSON。") from None


def _number(value: Any) -> float | None:
    if isinstance(value, bool):
        return None
    if isinstance(value, (int, float)) and math.isfinite(value):
        return float(value)
    if isinstance(value, str) and value.strip():
        try:
            parsed = float(value)
        except ValueError:
            return None
        return parsed if math.isfinite(parsed) else None
    return None


def read_limits(login: CursorLogin, client: httpx.Client) -> dict[str, Any]:
    body = _post(client, LIMITS_URL, content=b"{}", headers={
        "Authorization": f"Bearer {login.token}",
        "Content-Type": "application/json",
        "connect-protocol-version": "1",
        "x-cursor-client-type": "cli",
    })
    if not isinstance(body, dict):
        raise CursorUsageError("invalid_response", "Cursor 额度接口返回的结构无法识别。")
    plan = body.get("planUsage") if isinstance(body.get("planUsage"), dict) else {}
    cycle_end = _number(body.get("billingCycleEnd"))
    resets_at = cycle_end / 1000 if cycle_end and cycle_end > 0 else None
    windows = []
    for window_id, label, description in LIMIT_WINDOWS:
        used = _number(plan.get(window_id))
        if used is None:
            continue
        windows.append({"id": window_id, "kind": "monthly", "label": label, "description": description,
                        "used_percent": min(100.0, max(0.0, used)), "resets_at": resets_at})
    if not windows:
        raise CursorUsageError("limits_unsupported", "这个 Cursor 账号没有返回月度额度。")
    return {"windows": windows, "resets_at": resets_at}


def _canonical_hash(value: Any) -> str:
    encoded = json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=False)
    return hashlib.sha256(encoded.encode()).hexdigest()


def _boundary_overlap(previous: list[str], current: list[str]) -> int:
    """Longest suffix of ``previous`` that is a prefix of ``current`` (KMP)."""
    sequence = [*current, "", *previous]
    lengths = [0] * len(sequence)
    for index in range(1, len(sequence)):
        length = lengths[index - 1]
        while length > 0 and sequence[index] != sequence[length]:
            length = lengths[length - 1]
        if sequence[index] == sequence[length]:
            length += 1
        lengths[index] = length
    return lengths[-1] if sequence else 0


def _tokens(value: Any) -> int:
    number = _number(value)
    return int(number) if number is not None and number > 0 else 0


def read_history(login: CursorLogin, since_ms: int, until_ms: int, client: httpx.Client) -> list[dict[str, Any]]:
    """All usage events in the window, page boundaries reconciled.

    The dashboard has no event ID and pages can overlap when new events land
    mid-read; ``totalUsageEventsCount`` says how many rows to drop, and only
    exact repeats at a page boundary are dropped.
    """
    deadline = time.monotonic() + HISTORY_DEADLINE_SECONDS
    pages: list[list[Any]] = []
    total: int | None = None
    page = 1
    while True:
        if page > (1000 if total is None else math.ceil(total / PAGE_SIZE) * 2 + 1):
            raise CursorUsageError("invalid_response", "Cursor 账号历史的分页数量超出预期。")
        if time.monotonic() > deadline:
            raise CursorUsageError("request_timeout", f"读取 Cursor 账号历史超过 {HISTORY_DEADLINE_SECONDS} 秒。")
        body = _post(client, EVENTS_URL, headers={
            "Authorization": f"Bearer {login.token}",
            "Content-Type": "application/json",
            "connect-protocol-version": "1",
            "x-cursor-client-type": "cli",
        }, json={"page": page, "pageSize": PAGE_SIZE, "startDate": str(since_ms), "endDate": str(until_ms)})
        if not isinstance(body, dict) or any(key in body for key in ("error", "message", "code")):
            raise CursorUsageError("invalid_response", "Cursor 账号历史返回了错误结构。")
        count = 0 if not body else body.get("totalUsageEventsCount")
        events = [] if not body or set(body) == {"totalUsageEventsCount"} else body.get("usageEventsDisplay")
        if (count is not None and (not isinstance(count, int) or isinstance(count, bool) or count < 0
                                   or (total is not None and count != total))) \
                or not isinstance(events, list) or len(events) > PAGE_SIZE \
                or (count is None and not isinstance(body.get("usageEventsDisplay"), list)):
            raise CursorUsageError("invalid_response", "Cursor 账号历史的分页前后不一致。")
        if isinstance(count, int):
            total = count
        pages.append(events)
        if len(events) < PAGE_SIZE:
            break
        page += 1
    raw_count = sum(len(events) for events in pages)
    if total is not None and raw_count < total:
        raise CursorUsageError("invalid_response", "Cursor 账号历史的分页不完整。")
    removals = 0 if total is None else raw_count - total
    previous: list[str] = []
    records: list[dict[str, Any]] = []
    for events in pages:
        keys = [_canonical_hash(event) for event in events] if removals > 0 else []
        dropped = min(removals, _boundary_overlap(previous, keys))
        removals -= dropped
        previous = keys
        for raw in events[dropped:]:
            event = raw if isinstance(raw, dict) else {}
            usage = event.get("tokenUsage")
            if usage is None:
                continue
            if not isinstance(usage, dict):
                raise CursorUsageError("invalid_response", "Cursor 账号历史中的用量字段无法识别。")
            for key in ("inputTokens", "outputTokens", "cacheReadTokens", "cacheWriteTokens", "totalCents"):
                if key in usage and (_number(usage[key]) is None or _number(usage[key]) < 0):
                    raise CursorUsageError("invalid_response", "Cursor 账号历史中的用量数值无效。")
            at_ms = _number(event.get("timestamp"))
            model = event.get("model")
            if at_ms is None or not isinstance(model, str) or not model:
                raise CursorUsageError("invalid_response", "Cursor 账号历史中有无法识别的事件。")
            if at_ms < since_ms or at_ms > until_ms:
                continue
            cents = _number(usage.get("totalCents"))
            session = event.get("conversationId")
            records.append({
                "at_ms": at_ms, "model": model,
                "session": session if isinstance(session, str) else "",
                "uncached_input": _tokens(usage.get("inputTokens")),
                "cache_read": _tokens(usage.get("cacheReadTokens")),
                "cache_write": _tokens(usage.get("cacheWriteTokens")),
                "output": _tokens(usage.get("outputTokens")),
                "reported_cost": cents / 100 if cents is not None else None,
            })
    if removals:
        raise CursorUsageError("invalid_response", "Cursor 账号历史的分页边界无法对齐。")
    return records


# ── Window and aggregation ──────────────────────────────────────────────────

@dataclass(frozen=True)
class UsageWindow:
    days: int
    time_zone: str
    since_ms: int
    until_ms: int
    resolution: str  # "hour" | "day"
    slots: tuple[str, ...]


def make_window(days: int, time_zone: str, now: float | None = None) -> UsageWindow:
    """Past 24h is a rolling window by hour; longer windows are calendar days."""
    if days not in WINDOW_DAYS:
        raise ValueError(f"days 只能是 {', '.join(map(str, WINDOW_DAYS))}")
    try:
        zone = ZoneInfo(time_zone)
    except (ZoneInfoNotFoundError, ValueError):
        raise ValueError(f"无法识别的时区：{time_zone}") from None
    current = datetime.fromtimestamp(now if now is not None else time.time(), zone)
    if days == 1:
        since = current - timedelta(hours=24)
        slot = since.replace(minute=0, second=0, microsecond=0)
        slots = []
        while slot <= current:
            slots.append(slot.strftime("%Y-%m-%dT%H:00"))
            slot = (slot.astimezone(timezone.utc) + timedelta(hours=1)).astimezone(zone)
        resolution = "hour"
    else:
        first = current.date() - timedelta(days=days - 1)
        since = datetime(first.year, first.month, first.day, tzinfo=zone)
        slots = [(first + timedelta(days=offset)).isoformat() for offset in range(days)]
        resolution = "day"
    return UsageWindow(days, time_zone, int(since.timestamp() * 1000), int(current.timestamp() * 1000),
                       resolution, tuple(slots))


_TOKEN_FIELDS = ("uncached_input", "cache_read", "cache_write", "output")


def _empty_totals() -> dict[str, Any]:
    return {**{field: 0 for field in _TOKEN_FIELDS}, "total_tokens": 0, "reported_cost": None,
            "events": 0, "unpriced_events": 0, "sessions": 0}


def _add(target: dict[str, Any], record: dict[str, Any]) -> None:
    for field in _TOKEN_FIELDS:
        target[field] += record[field]
    target["total_tokens"] += sum(record[field] for field in _TOKEN_FIELDS)
    target["events"] += 1
    if record["reported_cost"] is None:
        target["unpriced_events"] += 1
    else:
        target["reported_cost"] = (target["reported_cost"] or 0.0) + record["reported_cost"]


def summarize(records: list[dict[str, Any]], window: UsageWindow) -> dict[str, Any]:
    zone = ZoneInfo(window.time_zone)
    totals = _empty_totals()
    models: dict[str, dict[str, Any]] = {}
    model_sessions: dict[str, set[str]] = {}
    sessions: set[str] = set()
    series = {slot: {"slot": slot, "reported_cost": 0.0, "total_tokens": 0} for slot in window.slots}
    for record in records:
        _add(totals, record)
        entry = models.setdefault(record["model"], {"model": record["model"], **_empty_totals()})
        _add(entry, record)
        if record["session"]:
            sessions.add(record["session"])
            model_sessions.setdefault(record["model"], set()).add(record["session"])
        moment = datetime.fromtimestamp(record["at_ms"] / 1000, zone)
        slot = moment.strftime("%Y-%m-%dT%H:00") if window.resolution == "hour" else moment.date().isoformat()
        if slot in series:
            series[slot]["reported_cost"] += record["reported_cost"] or 0.0
            series[slot]["total_tokens"] += sum(record[field] for field in _TOKEN_FIELDS)
    totals["sessions"] = len(sessions)
    for model, entry in models.items():
        entry["sessions"] = len(model_sessions.get(model, ()))
    ordered = sorted(models.values(), key=lambda item: (-(item["reported_cost"] or 0.0), -item["total_tokens"]))
    return {"totals": totals, "models": ordered, "series": [series[slot] for slot in window.slots]}


# ── Settings and service ────────────────────────────────────────────────────

def read_settings(state_root: str | Path) -> dict[str, Any]:
    path = Path(state_root) / SETTINGS_FILE
    try:
        raw = json.loads(path.read_text(encoding="utf-8"))
    except FileNotFoundError:
        raw = {}
    raw = raw if isinstance(raw, dict) else {}
    return {"cursor_account_usage_enabled": raw.get("cursor_account_usage_enabled") is True}


def write_settings(state_root: str | Path, settings: dict[str, Any]) -> None:
    path = Path(state_root) / SETTINGS_FILE
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_suffix(".json.tmp")
    tmp.write_text(json.dumps(settings, ensure_ascii=False, indent=2), encoding="utf-8")
    tmp.replace(path)


class CursorAccountUsage:
    """Reads limits and history for the host's Cursor CLI login, cached for a minute."""

    def __init__(self, state_root: str | Path, *, keychain: KeychainTokenReader | None = None,
                 env: Mapping[str, str] = os.environ, platform: str = sys.platform,
                 client_factory: Callable[[], httpx.Client] | None = None) -> None:
        self.state_root = state_root
        self.keychain = keychain or KeychainTokenReader()
        self.env = env
        self.platform = platform
        self.client_factory = client_factory or (
            lambda: httpx.Client(timeout=REQUEST_TIMEOUT_SECONDS, follow_redirects=False))
        # Reads are serialized so a slow Keychain prompt or history read is
        # shared instead of repeated; the cache lock is never held across I/O.
        self._read_lock = threading.Lock()
        self._cache_lock = threading.Lock()
        self._cache: dict[tuple, tuple[float, dict[str, Any]]] = {}

    def settings_view(self) -> dict[str, Any]:
        settings = read_settings(self.state_root)
        keychain = uses_keychain(self.env, self.platform)
        present: bool | None = None
        error: dict[str, Any] | None = None
        if keychain:
            try:
                present = keychain_login_present()
            except CursorUsageError as exc:
                error = exc.view()
        return {**settings, "keychain": {"applies": keychain, "login_present": present, "error": error},
                "auth_file": None if keychain else str(auth_file_path(self.env, self.platform, Path.home()))}

    def update_settings(self, *, cursor_account_usage_enabled: bool) -> dict[str, Any]:
        with self._cache_lock:
            write_settings(self.state_root, {**read_settings(self.state_root),
                                             "cursor_account_usage_enabled": cursor_account_usage_enabled})
            self._cache.clear()
        if not cursor_account_usage_enabled:
            self.keychain.forget()
        return self.settings_view()

    def _cached(self, key: tuple) -> dict[str, Any] | None:
        with self._cache_lock:
            cached = self._cache.get(key)
            return cached[1] if cached and cached[0] > time.monotonic() else None

    def read(self, window: UsageWindow, *, refresh: bool = False) -> dict[str, Any]:
        settings = read_settings(self.state_root)
        enabled = settings["cursor_account_usage_enabled"]
        key = (window.days, window.time_zone, enabled)
        if not refresh and (cached := self._cached(key)) is not None:
            return cached
        with self._read_lock:
            if not refresh and (cached := self._cached(key)) is not None:
                return cached
            payload = self._read(window, enabled)
        with self._cache_lock:
            self._cache[key] = (time.monotonic() + RESULT_CACHE_SECONDS, payload)
        return payload

    def _read(self, window: UsageWindow, enabled: bool) -> dict[str, Any]:
        base = {"read_at": time.time(), "enabled": enabled,
                "window": {"days": window.days, "time_zone": window.time_zone, "since_ms": window.since_ms,
                           "until_ms": window.until_ms, "resolution": window.resolution}}
        try:
            login = resolve_login(keychain_enabled=enabled, keychain=self.keychain, env=self.env, platform=self.platform)
        except CursorUsageError as exc:
            return {**base, "source": None, "account": None, "error": exc.view(),
                    "limits": None, "limits_error": None, "history": None, "history_error": None}
        with self.client_factory() as client:
            login, limits, limits_error = self._attempt(login, lambda current: read_limits(current, client))
            login, records, history_error = self._attempt(
                login, lambda current: read_history(current, window.since_ms, window.until_ms, client))
        return {**base, "source": login.source, "account": login.account_key[:12], "error": None,
                "limits": limits, "limits_error": limits_error,
                "history": summarize(records, window) if records is not None else None,
                "history_error": history_error,
                # Numerical metadata for the unified usage aggregator. The
                # HTTP endpoint excludes this internal key from its response.
                "_records": records}

    def _attempt(self, login: CursorLogin, call: Callable[[CursorLogin], Any]):
        """T3 uses one persisted login identity for this account snapshot."""
        try:
            return login, call(login), None
        except CursorUsageError as exc:
            return login, None, exc.view()
        except _SignedOut as signed_out:
            status = signed_out.status
        return login, None, CursorUsageError(
            "signed_out", f"Cursor 拒绝了当前登录（HTTP {status}）。执行 cursor-agent login 重新登录后刷新。").view()
