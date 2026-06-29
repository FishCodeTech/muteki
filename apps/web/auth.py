"""Web command-deck authentication (P3).

The deck was historically wide open: it bound to 127.0.0.1 and trusted any
local caller. That is fine for a single operator on a laptop but unsafe the
moment the backend is reachable from anywhere else (a remote host, a shared
box, an untrusted same-machine process) — the `/api` surface can read/write
credential accounts (Claude/Codex/Cursor subscription tokens), start/kill runs,
and inject operator commands.

This module adds a single-password gate in front of `/api`:

  * The operator sets MUTEKI_WEB_PASSWORD.
  * The browser POSTs it to /api/auth/login and gets back a short-lived,
    HMAC-signed session token (the password itself is never stored client-side).
  * Every /api request carries `Authorization: Bearer <token>`; the middleware
    verifies the signature + expiry in constant time.
  * SSE/WebSocket connections (which a browser cannot send headers on) use a
    one-time ticket minted by an authenticated POST /api/auth/ticket.

Design notes / threat model live in docs/_local/plan_p3_auth.md. The RCP
container<->host control channel (control_receiver.py) has its OWN per-run
token and is orthogonal to this — not touched here.
"""

from __future__ import annotations

import base64
import hashlib
import hmac
import os
import secrets
import time
from dataclasses import dataclass, field
from typing import Optional

# ---------------------------------------------------------------------------
# Env knobs
# ---------------------------------------------------------------------------
#   MUTEKI_WEB_PASSWORD     the operator login password. If set, auth is ENFORCED
#                           on every /api route (even on loopback).
#   MUTEKI_WEB_BIND         the bind host run.sh passes to uvicorn (it is also
#                           exported to the process env so create_app can see it;
#                           uvicorn's --host is NOT visible to the app otherwise).
#                           Used for the fail-fast: a non-loopback bind with no
#                           password set is a refuse-to-start misconfiguration.
#   MUTEKI_WEB_TOKEN_TTL    session-token lifetime in seconds (default 12h).
#   MUTEKI_WEB_AUTH_SECRET  optional explicit HMAC signing secret. If unset, a
#                           secret is derived from the password (stable across
#                           restarts so existing tokens survive a reboot) — or,
#                           if there is no password, a random per-process secret.

PASSWORD_ENV = "MUTEKI_WEB_PASSWORD"
BIND_ENV = "MUTEKI_WEB_BIND"
TTL_ENV = "MUTEKI_WEB_TOKEN_TTL"
SECRET_ENV = "MUTEKI_WEB_AUTH_SECRET"

DEFAULT_TTL_S = 12 * 3600

# Paths under the gate that must stay reachable WITHOUT a token, otherwise the
# operator could never obtain one. Everything else under /api requires auth.
#   /api/auth/login  — exchange password for a token (you have no token yet)
#   /api/health      — liveness probe (does not exist today, harmless to allow)
# NOTE: /api/auth/ticket and /api/auth/me are deliberately NOT public — minting a
# ticket or reading identity both require an already-valid token.
PUBLIC_API_PATHS = frozenset({"/api/auth/login", "/api/health"})

_LOOPBACK_HOSTS = frozenset({
    "127.0.0.1", "::1", "localhost", "0:0:0:0:0:0:0:1", "",
})


def is_loopback_host(host: Optional[str]) -> bool:
    """True if `host` is a loopback / unset bind address.

    A wildcard bind (0.0.0.0 / ::) is explicitly NOT loopback: it exposes the
    server on every interface, which is exactly the case the fail-fast guards.
    """
    h = (host or "").strip().lower()
    # strip optional IPv6 brackets and a trailing :port a user might pass in
    # MUTEKI_WEB_BIND (e.g. "[::1]:8000" or "127.0.0.1:8000"). A bare IPv6 like
    # "::1" has multiple colons and no brackets — don't mistake its last colon
    # for a port separator.
    if h.startswith("[") and "]" in h:
        h = h[1:h.index("]")]
    elif h.count(":") == 1:
        h = h.split(":", 1)[0]
    return h in _LOOPBACK_HOSTS


def _derive_secret(password: str) -> bytes:
    """Stable signing key derived from the password.

    Deriving from the password (rather than a random per-process key) means
    issued tokens survive a backend restart, and rotating the password
    invalidates all old tokens for free. A dedicated MUTEKI_WEB_AUTH_SECRET
    overrides this when an operator wants to rotate the password without logging
    everyone out (or vice-versa).
    """
    return hashlib.sha256(
        b"muteki-web-auth-v1\x00" + password.encode("utf-8")).digest()


@dataclass
class AuthConfig:
    """Resolved auth configuration for one server instance."""

    password: str = ""
    bind_host: str = ""
    ttl_s: int = DEFAULT_TTL_S
    secret: bytes = b""

    @property
    def enabled(self) -> bool:
        """Auth is enforced iff a password is configured."""
        return bool(self.password)

    @classmethod
    def from_env(cls) -> "AuthConfig":
        password = (os.environ.get(PASSWORD_ENV) or "").strip()
        bind_host = (os.environ.get(BIND_ENV) or "").strip()
        try:
            ttl = int(os.environ.get(TTL_ENV) or "")
        except (TypeError, ValueError):
            ttl = DEFAULT_TTL_S
        if ttl <= 0:
            ttl = DEFAULT_TTL_S
        explicit_secret = (os.environ.get(SECRET_ENV) or "").strip()
        if explicit_secret:
            secret = hashlib.sha256(explicit_secret.encode("utf-8")).digest()
        elif password:
            secret = _derive_secret(password)
        else:
            # No password → auth disabled → secret is never used to verify a real
            # session, but generate a random one so the type stays non-empty.
            secret = secrets.token_bytes(32)
        return cls(password=password, bind_host=bind_host, ttl_s=ttl, secret=secret)

    def fail_fast_check(self) -> None:
        """Refuse to start in an obviously-unsafe configuration.

        Non-loopback bind + no password = the wide-open `/api` surface is
        reachable from the network. That is almost never intended; make it a
        loud startup error instead of a silent exposure.
        """
        if not self.password and not is_loopback_host(self.bind_host):
            raise RuntimeError(
                f"Refusing to start: bound to non-loopback host "
                f"{self.bind_host!r} with no {PASSWORD_ENV} set — the /api "
                f"surface (including credential accounts) would be exposed "
                f"unauthenticated. Set {PASSWORD_ENV}, or bind to 127.0.0.1."
            )


# ---------------------------------------------------------------------------
# Session tokens: HMAC-signed "<exp>.<sig>"  (stateless, expiring)
# ---------------------------------------------------------------------------
# Token = base64url(payload) + "." + base64url(hmac_sha256(secret, payload))
# payload = b"v1\x00" + str(exp_unix_seconds). We keep it tiny; there is exactly
# one principal (the operator), so the token carries no identity beyond "valid".


def _b64u(raw: bytes) -> str:
    return base64.urlsafe_b64encode(raw).rstrip(b"=").decode("ascii")


def _b64u_decode(s: str) -> bytes:
    pad = "=" * (-len(s) % 4)
    return base64.urlsafe_b64decode(s + pad)


def _sign(secret: bytes, payload: bytes) -> bytes:
    return hmac.new(secret, payload, hashlib.sha256).digest()


def issue_token(cfg: AuthConfig, *, now: Optional[float] = None) -> str:
    """Mint a signed session token valid for cfg.ttl_s seconds."""
    ts = int(now if now is not None else time.time())
    exp = ts + cfg.ttl_s
    payload = b"v1\x00" + str(exp).encode("ascii")
    sig = _sign(cfg.secret, payload)
    return _b64u(payload) + "." + _b64u(sig)


def verify_token(cfg: AuthConfig, token: Optional[str], *,
                 now: Optional[float] = None) -> bool:
    """Constant-time verify a session token's signature and expiry."""
    if not token:
        return False
    try:
        payload_b64, sig_b64 = token.split(".", 1)
        payload = _b64u_decode(payload_b64)
        sig = _b64u_decode(sig_b64)
    except (ValueError, Exception):  # malformed token
        return False
    expected = _sign(cfg.secret, payload)
    if not hmac.compare_digest(sig, expected):
        return False
    if not payload.startswith(b"v1\x00"):
        return False
    try:
        exp = int(payload[3:].decode("ascii"))
    except (ValueError, UnicodeDecodeError):
        return False
    ts = now if now is not None else time.time()
    return ts < exp


def check_password(cfg: AuthConfig, password: Optional[str]) -> bool:
    """Constant-time password comparison (avoid timing oracle on length/prefix)."""
    if not cfg.password:
        return False
    return hmac.compare_digest(
        (password or "").encode("utf-8"), cfg.password.encode("utf-8"))


def bearer_from_header(authorization: Optional[str]) -> Optional[str]:
    """Extract the token from an `Authorization: Bearer <token>` header."""
    if not authorization:
        return None
    parts = authorization.split(None, 1)
    if len(parts) == 2 and parts[0].lower() == "bearer":
        return parts[1].strip()
    return None


# ---------------------------------------------------------------------------
# One-time tickets for SSE / WebSocket
# ---------------------------------------------------------------------------
# A browser EventSource / WebSocket cannot set an Authorization header. Rather
# than smuggle the long-lived session token through the URL query string (which
# leaks into referers, history and proxy logs), an authenticated client mints a
# single-use, short-TTL ticket and redeems it once on connect.


@dataclass
class TicketStore:
    ttl_s: float = 30.0
    _tickets: dict[str, float] = field(default_factory=dict)  # ticket -> expiry

    def mint(self, *, now: Optional[float] = None) -> str:
        ts = now if now is not None else time.time()
        self._evict(ts)
        ticket = secrets.token_urlsafe(32)
        self._tickets[ticket] = ts + self.ttl_s
        return ticket

    def redeem(self, ticket: Optional[str], *, now: Optional[float] = None) -> bool:
        """Consume a ticket. Single-use: a redeemed ticket is removed."""
        if not ticket:
            return False
        ts = now if now is not None else time.time()
        self._evict(ts)
        exp = self._tickets.pop(ticket, None)
        if exp is None:
            return False
        return ts < exp

    def _evict(self, now: float) -> None:
        if len(self._tickets) < 256:
            # cheap fast-path; only sweep when the table grows
            stale = [k for k, e in self._tickets.items() if e <= now]
        else:
            stale = [k for k, e in self._tickets.items() if e <= now]
        for k in stale:
            self._tickets.pop(k, None)
