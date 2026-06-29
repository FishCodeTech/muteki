"""P3 web auth: unit tests for the token/password/ticket primitives, plus
integration tests of the FastAPI gate (middleware + login/ticket + SSE/WS).

The gate is enforced iff MUTEKI_WEB_PASSWORD is set. With no password and a
loopback bind, the deck behaves exactly as before (open). A non-loopback bind
with no password is a refuse-to-start misconfiguration.
"""

import time

import pytest
from fastapi import FastAPI, WebSocket
from fastapi.testclient import TestClient

from apps.web import auth as A
from apps.web.run_manager import RunManager
from apps.web.server import create_app


# ---------------------------------------------------------------------------
# Pure primitives
# ---------------------------------------------------------------------------
def _cfg(password="pw", bind="127.0.0.1", ttl=3600):
    return A.AuthConfig(
        password=password, bind_host=bind, ttl_s=ttl,
        secret=A._derive_secret(password) if password else b"x" * 32)


def test_is_loopback_host():
    for h in ("127.0.0.1", "::1", "localhost", "", "LOCALHOST", "[::1]", "127.0.0.1:8000"):
        assert A.is_loopback_host(h), h
    for h in ("0.0.0.0", "::", "192.168.1.5", "example.com", "10.0.0.1"):
        assert not A.is_loopback_host(h), h


def test_token_roundtrip_and_expiry():
    cfg = _cfg(ttl=100)
    now = 1000.0
    tok = A.issue_token(cfg, now=now)
    assert A.verify_token(cfg, tok, now=now)
    assert A.verify_token(cfg, tok, now=now + 99)
    # expired
    assert not A.verify_token(cfg, tok, now=now + 101)


def test_token_rejected_under_different_secret():
    a = _cfg(password="alpha")
    b = _cfg(password="bravo")
    tok = A.issue_token(a)
    assert A.verify_token(a, tok)
    assert not A.verify_token(b, tok)  # password change invalidates old tokens


def test_token_rejects_tampering_and_garbage():
    cfg = _cfg()
    tok = A.issue_token(cfg)
    assert not A.verify_token(cfg, None)
    assert not A.verify_token(cfg, "")
    assert not A.verify_token(cfg, "not-a-token")
    assert not A.verify_token(cfg, tok + "x")  # mangled signature
    payload, sig = tok.split(".", 1)
    assert not A.verify_token(cfg, payload + ".AAAA")  # wrong sig
    assert not A.verify_token(cfg, "....")


def test_check_password_constant_time_paths():
    cfg = _cfg(password="s3cret")
    assert A.check_password(cfg, "s3cret")
    assert not A.check_password(cfg, "s3cre")
    assert not A.check_password(cfg, "s3cret ")
    assert not A.check_password(cfg, "")
    assert not A.check_password(cfg, None)
    # disabled config: never matches
    assert not A.check_password(_cfg(password=""), "")


def test_bearer_from_header():
    assert A.bearer_from_header("Bearer abc") == "abc"
    assert A.bearer_from_header("bearer abc") == "abc"
    assert A.bearer_from_header("Bearer   abc  ") == "abc"
    assert A.bearer_from_header("abc") is None
    assert A.bearer_from_header("") is None
    assert A.bearer_from_header(None) is None


def test_ticket_is_single_use_and_expires():
    store = A.TicketStore(ttl_s=10)
    now = 500.0
    tk = store.mint(now=now)
    # redeem once → ok; second redeem → fail (consumed)
    assert store.redeem(tk, now=now)
    assert not store.redeem(tk, now=now)
    # expired ticket
    tk2 = store.mint(now=now)
    assert not store.redeem(tk2, now=now + 11)
    # garbage
    assert not store.redeem(None, now=now)
    assert not store.redeem("nope", now=now)


# ---------------------------------------------------------------------------
# AuthConfig.from_env + fail-fast
# ---------------------------------------------------------------------------
def test_from_env_disabled_when_no_password(monkeypatch):
    monkeypatch.delenv(A.PASSWORD_ENV, raising=False)
    monkeypatch.delenv(A.BIND_ENV, raising=False)
    cfg = A.AuthConfig.from_env()
    assert not cfg.enabled
    cfg.fail_fast_check()  # loopback (unset) + no password → fine


def test_fail_fast_raises_on_public_bind_without_password(monkeypatch):
    monkeypatch.delenv(A.PASSWORD_ENV, raising=False)
    monkeypatch.setenv(A.BIND_ENV, "0.0.0.0")
    cfg = A.AuthConfig.from_env()
    with pytest.raises(RuntimeError, match="non-loopback"):
        cfg.fail_fast_check()


def test_fail_fast_ok_on_public_bind_with_password(monkeypatch):
    monkeypatch.setenv(A.PASSWORD_ENV, "pw")
    monkeypatch.setenv(A.BIND_ENV, "0.0.0.0")
    cfg = A.AuthConfig.from_env()
    assert cfg.enabled
    cfg.fail_fast_check()  # password present → ok to expose


def test_create_app_raises_on_public_bind_without_password(monkeypatch, tmp_path):
    monkeypatch.delenv(A.PASSWORD_ENV, raising=False)
    monkeypatch.setenv(A.BIND_ENV, "0.0.0.0")
    with pytest.raises(RuntimeError, match="non-loopback"):
        create_app(RunManager(sessions_root=str(tmp_path / "s")))


# ---------------------------------------------------------------------------
# Integration: the gate via TestClient
# ---------------------------------------------------------------------------
@pytest.fixture
def client_open(monkeypatch, tmp_path):
    """Auth disabled (no password) → everything open, as before."""
    monkeypatch.delenv(A.PASSWORD_ENV, raising=False)
    monkeypatch.delenv(A.BIND_ENV, raising=False)
    app = create_app(RunManager(sessions_root=str(tmp_path / "s")))
    return TestClient(app)


@pytest.fixture
def client_auth(monkeypatch, tmp_path):
    monkeypatch.setenv(A.PASSWORD_ENV, "letmein")
    monkeypatch.delenv(A.BIND_ENV, raising=False)
    app = create_app(RunManager(sessions_root=str(tmp_path / "s")))
    return TestClient(app)


def test_open_mode_allows_unauthenticated_api(client_open):
    assert client_open.get("/api/runs").status_code == 200
    # login route returns auth_required=False so the frontend skips the gate
    r = client_open.post("/api/auth/login", json={})
    assert r.status_code == 200 and r.json()["auth_required"] is False


def test_protected_route_401_without_token(client_auth):
    r = client_auth.get("/api/runs")
    assert r.status_code == 401
    assert r.json()["error"] == "unauthorized"


def test_login_wrong_password_401(client_auth):
    r = client_auth.post("/api/auth/login", json={"password": "nope"})
    assert r.status_code == 401


def test_login_then_authenticated_access(client_auth):
    r = client_auth.post("/api/auth/login", json={"password": "letmein"})
    assert r.status_code == 200
    token = r.json()["token"]
    assert token
    h = {"Authorization": f"Bearer {token}"}
    assert client_auth.get("/api/runs", headers=h).status_code == 200
    assert client_auth.get("/api/auth/me", headers=h).status_code == 200


def test_bad_token_rejected(client_auth):
    h = {"Authorization": "Bearer garbage.sig"}
    assert client_auth.get("/api/runs", headers=h).status_code == 401


def test_login_is_public_but_ticket_and_me_are_gated(client_auth):
    # login reachable without a token
    assert client_auth.post("/api/auth/login", json={"password": "x"}).status_code == 401
    # (401 = wrong password, NOT "blocked by gate" — the route ran)
    # ticket + me require a valid token
    assert client_auth.post("/api/auth/ticket").status_code == 401
    assert client_auth.get("/api/auth/me").status_code == 401


def test_ticket_mint_requires_auth_then_works(client_auth):
    token = client_auth.post("/api/auth/login", json={"password": "letmein"}).json()["token"]
    h = {"Authorization": f"Bearer {token}"}
    r = client_auth.post("/api/auth/ticket", headers=h)
    assert r.status_code == 200
    assert r.json()["ticket"]


def test_options_preflight_not_blocked(client_auth):
    # A bare OPTIONS must not be 401'd (CORS preflight / no-Origin same-origin).
    r = client_auth.options("/api/runs")
    assert r.status_code != 401


def test_cross_origin_401_carries_cors_header(client_auth):
    # Regression: the auth middleware short-circuits with its own 401, bypassing
    # CORSMiddleware's response path. Without mirroring the allow-origin header, a
    # browser at :3001 sees a CORS network error instead of a 401 and can't show
    # the login form (it was silently letting users straight in). The 401 MUST
    # echo Access-Control-Allow-Origin for an allowed cross-origin caller.
    r = client_auth.get("/api/runs", headers={"Origin": "http://localhost:3001"})
    assert r.status_code == 401
    assert r.headers.get("access-control-allow-origin") == "http://localhost:3001"
    # A disallowed origin gets no allow-origin header (browser blocks it anyway).
    r2 = client_auth.get("/api/runs", headers={"Origin": "http://evil.example"})
    assert r2.status_code == 401
    assert "access-control-allow-origin" not in {k.lower() for k in r2.headers}


def test_sse_events_rejects_without_credentials(client_auth):
    # The security-critical assertion: an unauthenticated SSE open is rejected
    # with 401 BEFORE any run handle is created (the check is the first
    # statement in events(), ahead of manager.get/create). This returns
    # immediately — no streaming — so it's safe to assert synchronously.
    r = client_auth.get("/api/runs/run-xyz/events")
    assert r.status_code == 401
    # A bad ticket query param is also rejected.
    r = client_auth.get("/api/runs/run-xyz/events?ticket=bogus")
    assert r.status_code == 401


# NOTE: the SSE *positive* path (valid ticket accepted) cannot be tested with
# Starlette's TestClient — events() is an infinite live tail that never EOFs, and
# the TestClient portal deadlocks tearing that down. The repo tests SSE against a
# REAL uvicorn server (see test_web_server.py::_Server); we do the same below so
# the assertion is real, not a TestClient artifact.
async def test_sse_events_accepts_valid_ticket_real_server(monkeypatch, tmp_path):
    import asyncio
    import socket
    import threading

    import httpx
    import uvicorn

    monkeypatch.setenv(A.PASSWORD_ENV, "letmein")
    monkeypatch.delenv(A.BIND_ENV, raising=False)
    app = create_app(RunManager(sessions_root=str(tmp_path / "s")))

    sock = socket.socket()
    sock.bind(("127.0.0.1", 0))
    port = sock.getsockname()[1]
    sock.close()
    cfg = uvicorn.Config(app, host="127.0.0.1", port=port, log_level="warning")
    srv = uvicorn.Server(cfg)
    th = threading.Thread(target=srv.run, daemon=True)
    th.start()
    try:
        for _ in range(100):
            if srv.started:
                break
            await asyncio.sleep(0.05)
        base = f"http://127.0.0.1:{port}"
        async with httpx.AsyncClient(base_url=base, timeout=10, trust_env=False) as c:
            # no credentials → 401
            r = await c.get("/api/runs/r1/events")
            assert r.status_code == 401
            # login → ticket → SSE accepted; read one frame then bail
            tok = (await c.post("/api/auth/login", json={"password": "letmein"})).json()["token"]
            h = {"Authorization": f"Bearer {tok}"}
            tk = (await c.post("/api/auth/ticket", headers=h)).json()["ticket"]

            # The gate decision is in the response STATUS, which arrives with the
            # headers — before any SSE body frame. So assert the status and close
            # immediately; don't wait for the 10s ping. (200 = ticket accepted.)
            async def _status(ticket):
                async with c.stream("GET", f"/api/runs/r1/events?ticket={ticket}") as resp:
                    return resp.status_code

            assert await asyncio.wait_for(_status(tk), timeout=8) == 200
            # ticket is single-use: a second open with the same ticket → 401
            assert await asyncio.wait_for(_status(tk), timeout=8) == 401
    finally:
        srv.should_exit = True
        th.join(timeout=5)


def test_ws_terminal_rejects_without_ticket(client_auth):
    # Pre-create the run so a rejection is unambiguously about auth, not 4004.
    client_auth_token = client_auth.post(
        "/api/auth/login", json={"password": "letmein"}).json()["token"]
    h = {"Authorization": f"Bearer {client_auth_token}"}
    # mint a run via the API so manager.get(run_id) is not None
    rid = client_auth.post("/api/runs", headers=h).json()["run_id"]
    from starlette.websockets import WebSocketDisconnect
    with pytest.raises(WebSocketDisconnect) as ei:
        with client_auth.websocket_connect(f"/api/runs/{rid}/terminal"):
            pass
    assert ei.value.code == 4401


def test_ws_terminal_accepts_with_ticket(client_auth):
    token = client_auth.post(
        "/api/auth/login", json={"password": "letmein"}).json()["token"]
    h = {"Authorization": f"Bearer {token}"}
    rid = client_auth.post("/api/runs", headers=h).json()["run_id"]
    tk = client_auth.post("/api/auth/ticket", headers=h).json()["ticket"]
    # Should connect (then close cleanly when we exit the context).
    with client_auth.websocket_connect(f"/api/runs/{rid}/terminal?ticket={tk}"):
        pass
