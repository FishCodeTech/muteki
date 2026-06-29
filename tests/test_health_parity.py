"""Health-parity tests with TEETH (not a tautology).

After unification, dispatch and the settings self-check both call ONE kernel
(evaluate_profile_health). Asserting "they return the same verdict" would prove
only shared wiring — there is one code path. The bug class that CAN still occur
is the two callers transforming inputs DIFFERENTLY before the kernel (e.g.
resolving a different effective backend for the same profile → a verdict that
predicts the wrong thing). So we test:

  1. resolve_worker_backend — a golden table (normalization, independent of any
     caller): precedence, the container_dockerexec alias, invalid→local fallback,
     and the web-container override.
  2. dispatch arg-construction — the (profile, backend, depth) tuple the dispatch
     precheck feeds the kernel for each selected enabled profile.
  3. settings endpoint arg-construction — the (profile, backend, depth) tuple the
     /api/settings/profiles/{id}/health endpoint feeds the kernel.

Tests 2 and 3 both assert against the SAME golden expected tuple, so their
agreement is implicit — no separate "tuple == tuple" assertion needed (that
would be the weaker, redundant check Codex flagged)."""

from __future__ import annotations

import asyncio

import pytest

from apps.web.worker_config import (
    DEFAULT_WORKER_BACKEND,
    WorkerConfigStore,
    backend_for_profile,
    resolve_worker_backend,
)


# ── 1. resolve_worker_backend golden table ───────────────────────────────────

@pytest.mark.parametrize("kw,expected", [
    # precedence: request > config > env > default
    (dict(request_backend="local", config_backend="container", in_web_container=False), "local"),
    (dict(config_backend="local", env_backend="container", in_web_container=False), "local"),
    (dict(env_backend="container", in_web_container=False), "container"),
    (dict(in_web_container=False), DEFAULT_WORKER_BACKEND),
    # container_dockerexec alias → container
    (dict(request_backend="container_dockerexec", in_web_container=False), "container"),
    # invalid (non-empty, unknown) → local fallback (on a bare host)
    (dict(request_backend="bogus", in_web_container=False), "local"),
    # all empty → fall through to the default (container), NOT local
    (dict(request_backend="", config_backend="", env_backend="", in_web_container=False), DEFAULT_WORKER_BACKEND),
    # web-container override: local is forced to container, ALWAYS
    (dict(config_backend="local", in_web_container=True), "container"),
    (dict(request_backend="local", in_web_container=True), "container"),
    (dict(request_backend="bogus", in_web_container=True), "container"),  # invalid→local→override
    # container is unaffected by the override
    (dict(config_backend="container", in_web_container=True), "container"),
])
def test_resolve_worker_backend_golden(kw, expected):
    assert resolve_worker_backend(**kw) == expected


# ── per-profile runtime→backend mapping ──────────────────────────────────────

_RUNTIME_PROFILES = [
    {"id": "local", "backend": "local"},
    {"id": "docker-web", "backend": "container"},
]

@pytest.mark.parametrize("runtime,worker_backend,in_web,expected", [
    ("docker-web", "local", False, "container"),    # runtime backend wins over global
    ("local", "container", False, "local"),         # runtime backend wins over global
    ("local", "container", True, "container"),       # web-container override on top
    ("", "container", False, "container"),           # no runtime → fall back to global
    ("unknown-rt", "local", False, "local"),         # unknown runtime → global
])
def test_backend_for_profile_golden(runtime, worker_backend, in_web, expected):
    got = backend_for_profile(
        {"runtime": runtime}, runtime_profiles=_RUNTIME_PROFILES,
        worker_backend=worker_backend, in_web_container=in_web,
    )
    assert got == expected


# ── 2. dispatch arg-construction ─────────────────────────────────────────────

def _capture_kernel(monkeypatch):
    """Patch the kernel to record every (profile_id, backend, depth) it's asked to
    evaluate, and return a green verdict so the caller proceeds."""
    from muteki.solver.profile_health import ProfileHealth

    calls: list[tuple[str, str, str]] = []

    def _fake(profile, *, backend, sessions_root, depth="auth"):
        pid = str(profile.get("name") or profile.get("id") or profile.get("engine"))
        calls.append((pid, backend, depth))
        return ProfileHealth(
            profile_id=pid, engine=str(profile.get("engine") or ""), backend=backend,
            status="ok", layer=None, blocker=None, detail="ok",
            model=str(profile.get("model") or ""), account_id=str(profile.get("credential_account") or ""),
        )

    # dispatch imports the kernel lazily INSIDE _missing_profile_accounts, so patch
    # the source symbol.
    monkeypatch.setattr("muteki.solver.profile_health.evaluate_profile_health", _fake)
    return calls


def test_dispatch_feeds_kernel_selected_enabled_profiles_at_auth_depth(tmp_path, monkeypatch):
    from apps.web.drivers import _missing_profile_accounts

    monkeypatch.setattr("muteki.core.runtime_env.is_web_container", lambda: False)
    calls = _capture_kernel(monkeypatch)

    profiles = [
        {"id": "claude-local", "name": "claude-local", "engine": "claude",
         "runtime": "docker-web", "credential_account": "claude-main", "enabled": True},
        {"id": "codex-local", "name": "codex-local", "engine": "codex",
         "runtime": "docker-web", "credential_account": "", "enabled": True},
        {"id": "disabled-one", "name": "disabled-one", "engine": "cursor",
         "runtime": "local", "credential_account": "cursor-main", "enabled": False},
    ]
    _missing_profile_accounts(
        worker_profiles=profiles, runtime_profiles=_RUNTIME_PROFILES, sessions_root=tmp_path,
    )

    by_pid = {c[0]: c for c in calls}
    # enabled profiles are evaluated; the disabled one is filtered out BEFORE the kernel
    assert "disabled-one" not in by_pid
    # GOLDEN: docker-web runtime → container backend; dispatch probes at auth depth
    assert by_pid["claude-local"] == ("claude-local", "container", "auth")
    assert by_pid["codex-local"] == ("codex-local", "container", "auth")


# ── 3. settings endpoint arg-construction ────────────────────────────────────

def test_settings_endpoint_feeds_kernel_same_tuple_at_auth_depth(tmp_path, monkeypatch):
    """The POST /profiles/{id}/health endpoint must feed the kernel the SAME
    (profile, backend, depth) tuple dispatch does for that profile — backend
    resolved from SERVER context, depth=auth. Asserted against the same golden
    tuple as the dispatch test above."""
    from starlette.testclient import TestClient
    from apps.web.server import create_app
    from apps.web.run_manager import RunManager

    monkeypatch.setattr("muteki.core.runtime_env.is_web_container", lambda: False)
    monkeypatch.delenv("MUTEKI_WEB_PASSWORD", raising=False)

    # seed a config with the docker-web profiles
    wc = WorkerConfigStore(root=tmp_path)
    wc.set(
        engines=["claude-local"],
        worker_backend="container",
        runtime_profiles=_RUNTIME_PROFILES,
        worker_profiles=[
            {"id": "claude-local", "name": "claude-local", "engine": "claude",
             "transport": "claude_code", "runtime": "docker-web",
             "credential_account": "claude-main", "enabled": True},
        ],
    )
    # after the identity migration the stored profile's id is the new seat id;
    # the old name "claude-local" survives in the alias table. The endpoint must
    # accept the old name (the "测连通" button still posts it) and resolve it.
    stored = wc.get()
    seat_id = stored["seat_alias"]["claude-local"]

    calls = _capture_kernel(monkeypatch)

    app = create_app(RunManager(sessions_root=str(tmp_path)))
    with TestClient(app) as c:
        r = c.post("/api/settings/profiles/claude-local/health")
        assert r.status_code == 200

    by_pid = {c[0]: c for c in calls}
    # GOLDEN: the endpoint feeds the kernel the SAME (profile_id, backend, depth)
    # tuple dispatch does — backend=container (docker-web runtime), depth=auth. The
    # profile_id is the migrated seat id (production dispatch reads the same migrated
    # config via worker_config.resolve(), so both sides agree on this id → parity).
    assert by_pid[seat_id] == (seat_id, "container", "auth")


def test_settings_batch_endpoint_uses_binding_depth(tmp_path, monkeypatch):
    """The badge batch endpoint must use the CHEAP binding depth, not auth (no
    wall of CLI hellos on modal open)."""
    from starlette.testclient import TestClient
    from apps.web.server import create_app
    from apps.web.run_manager import RunManager

    monkeypatch.setattr("muteki.core.runtime_env.is_web_container", lambda: False)
    monkeypatch.delenv("MUTEKI_WEB_PASSWORD", raising=False)

    wc = WorkerConfigStore(root=tmp_path)
    wc.set(
        engines=["claude-local"],
        worker_backend="container",
        runtime_profiles=_RUNTIME_PROFILES,
        worker_profiles=[
            {"id": "claude-local", "name": "claude-local", "engine": "claude",
             "transport": "claude_code", "runtime": "docker-web",
             "credential_account": "claude-main", "enabled": True},
        ],
    )
    calls = _capture_kernel(monkeypatch)

    app = create_app(RunManager(sessions_root=str(tmp_path)))
    with TestClient(app) as c:
        r = c.get("/api/settings/profiles/health")
        assert r.status_code == 200

    assert calls, "batch endpoint must evaluate at least one profile"
    assert all(depth == "binding" for (_pid, _backend, depth) in calls)
