"""Unit tests for the profile_health kernel — the single source of truth shared
by the dispatch precheck and the settings self-check. These pin the behaviour
that the false-green bug violated (engine-dimension vs profile-dimension health)
and the corrections Codex's review forced (auth always host-local; present
semantics; endpoint-only profiles; disabled ≠ green)."""

from __future__ import annotations

import pytest

from muteki.solver.credential_accounts import CredentialAccountStore, account_store_root
from muteki.solver.profile_health import (
    ProfileHealth,
    evaluate_profile_health,
    needs_auth_probe,
)


def _register_claude(tmp_path):
    store = CredentialAccountStore(account_store_root(tmp_path))
    store.upsert_secret(account_id="claude-main", engine="claude", secret="tok-123")
    return store


# ── binding layer ────────────────────────────────────────────────────────────

def test_disabled_profile_is_disabled_not_green(tmp_path):
    """A disabled profile reports status='disabled' (ok for gating) — NOT a plain
    green 'ok' that would imply it was checked and passed."""
    h = evaluate_profile_health(
        {"name": "x", "engine": "codex", "enabled": False},
        backend="container", sessions_root=tmp_path, depth="binding",
    )
    assert h.status == "disabled"
    assert h.ok is True  # disabled never blocks a run


def test_container_empty_binding_no_default_is_blocked(tmp_path):
    """Container mode requires resolvable credential material. With an empty
    binding AND no default account on disk, the profile is blocked at the cheap
    binding layer — before any probe. (This is the codex-local:<missing> failure
    mode when codex-main isn't present.)"""
    # tmp_path has NO accounts registered → the codex-main default isn't present.
    h = evaluate_profile_health(
        {"name": "codex-local", "engine": "codex", "credential_account": "",
         "credential_mode": "subscription", "enabled": True},
        backend="container", sessions_root=tmp_path, depth="binding",
    )
    assert h.status == "blocked"
    assert h.layer == "binding"
    assert h.blocker  # human-readable reason present
    assert h.ok is False


def test_container_empty_binding_resolves_default_account(tmp_path):
    """The auth.json case (operator's point): an empty credential_account is NOT a
    hard block when the engine's DEFAULT account ({engine}-main) is present — the
    container projects all accounts and the worker resolves the default. The
    binding layer passes; an expired token would then surface at the auth layer."""
    # register the default codex account (codex-main) — auth.json-style presence.
    store = CredentialAccountStore(account_store_root(tmp_path))
    store.upsert_secret(account_id="codex-main", engine="codex",
                        codex_auth_json='{"tokens": {"access_token": "x"}}')
    h = evaluate_profile_health(
        {"name": "codex-local", "engine": "codex", "credential_account": "",
         "credential_mode": "subscription", "enabled": True},
        backend="container", sessions_root=tmp_path, depth="binding",
    )
    assert h.status == "ok"
    assert h.ok is True


def test_container_bound_present_account_passes_binding(tmp_path):
    _register_claude(tmp_path)
    h = evaluate_profile_health(
        {"name": "claude-local", "engine": "claude", "credential_account": "claude-main", "enabled": True},
        backend="container", sessions_root=tmp_path, depth="binding",
    )
    assert h.status == "ok"


def test_present_semantics_empty_account_dir_is_blocked(tmp_path):
    """Codex #8 (intentional): an account dir that EXISTS but holds no credential
    file is present=False → binding failure, not a fall-through to the probe. The
    old precheck only checked `is None` and would have probed it."""
    # create the account directory with no credential file inside
    root = account_store_root(tmp_path)
    (root / "ghost-acct").mkdir(parents=True, exist_ok=True)
    store = CredentialAccountStore(root)
    acct = store.inspect("ghost-acct")
    assert acct is not None and acct.present is False  # precondition

    h = evaluate_profile_health(
        {"name": "p", "engine": "claude", "credential_account": "ghost-acct", "enabled": True},
        backend="container", sessions_root=tmp_path, depth="binding",
    )
    assert h.status == "blocked"
    assert h.layer == "binding"


# ── needs_auth_probe invariant (endpoint-only profiles) ──────────────────────

def test_needs_auth_probe_endpoint_only_profile():
    """An endpoint profile that doesn't strictly require a stored account must
    still be probed (the second clause of the historical OR). Dropping it would
    silently regress endpoint profiles."""
    endpoint_profile = {
        "name": "deepseek", "engine": "codex",
        "credential_mode": "api", "base_url": "https://api.deepseek.com/v1",
    }
    assert needs_auth_probe(endpoint_profile, backend="local") is True


def test_needs_auth_probe_bare_subscription_local_no_login(monkeypatch):
    """A bare subscription with NO system login still needs a probe (keyed mode
    path); with a present login it does not (zero-probe fast path)."""
    monkeypatch.setattr(
        "muteki.solver.profile_health.detect_system_login", lambda engine, env=None: "present"
    )
    sub = {"name": "claude-local", "engine": "claude", "credential_mode": "subscription"}
    # subscription is not a keyed mode and login present → no probe needed locally
    assert needs_auth_probe(sub, backend="local") is False
    # but in container it ALWAYS needs an account/probe
    assert needs_auth_probe(sub, backend="container") is True


# ── auth layer ───────────────────────────────────────────────────────────────

def test_auth_layer_pins_profile_model(tmp_path, monkeypatch):
    """Divergence③ regression: the auth hello must use the PROFILE's pinned model,
    i.e. driver_for receives the profile dict (→ ProfileDriver), not a bare engine
    string."""
    _register_claude(tmp_path)
    seen = {}

    class _Drv:
        def health_detail(self, env=None):
            return True, "ok"

    def _capture(arg):
        seen["arg"] = arg
        return _Drv()

    monkeypatch.setattr("muteki.solver.cli_driver.driver_for", _capture)
    h = evaluate_profile_health(
        {"name": "claude-local", "engine": "claude",
         "credential_account": "claude-main", "model": "claude-opus-4-8",
         "credential_mode": "api_key", "enabled": True},
        backend="local", sessions_root=tmp_path, depth="auth",
    )
    assert h.status == "ok"
    # driver_for got the full profile dict (so ProfileDriver can pin the model),
    # NOT a bare engine string.
    assert isinstance(seen["arg"], dict)
    assert seen["arg"].get("model") == "claude-opus-4-8"


def test_auth_layer_env_is_always_container_false(tmp_path, monkeypatch):
    """CRITICAL fix: the auth probe runs as a HOST-LOCAL subprocess even for a
    container-backend profile, so the credential env MUST be resolved with
    container=False (a container=True overlay points at in-container paths that
    don't exist on the host)."""
    _register_claude(tmp_path)
    seen = {}

    def _fake_env(engine, *, account_root, account_id, container):
        seen["container"] = container
        class _R:  # minimal RuntimeCredentialEnv stand-in
            env = {"CLAUDE_CODE_OAUTH_TOKEN": "tok-123"}
        return _R()

    monkeypatch.setattr("muteki.solver.profile_health.runtime_env_for_engine", _fake_env)
    monkeypatch.setattr(
        "muteki.solver.cli_driver.driver_for",
        lambda profile: type("D", (), {"health_detail": lambda self, env=None: (True, "ok")})(),
    )
    # plumbing must PASS so the auth layer is reached (container plumbing gates
    # before auth — itself a correct ordering we rely on).
    monkeypatch.setattr(
        "muteki.solver.profile_health._probe_container_plumbing",
        lambda **kw: (True, None, "plumbing ok"),
    )
    # backend=container, but the auth env resolution must STILL be container=False
    evaluate_profile_health(
        {"name": "claude-local", "engine": "claude",
         "credential_account": "claude-main", "credential_mode": "api_key", "enabled": True},
        backend="container", sessions_root=tmp_path, depth="auth",
    )
    assert seen["container"] is False


def test_auth_failure_reports_auth_layer(tmp_path, monkeypatch):
    """Divergence② regression: a 403/expired token surfaces as status=auth_failed
    (layer=auth), not a green pass — the thing the container '--version'-only probe
    used to miss."""
    _register_claude(tmp_path)
    monkeypatch.setattr(
        "muteki.solver.cli_driver.driver_for",
        lambda profile: type("D", (), {"health_detail": lambda self, env=None: (False, "api_error_status:403")})(),
    )
    h = evaluate_profile_health(
        {"name": "claude-local", "engine": "claude",
         "credential_account": "claude-main", "credential_mode": "api_key", "enabled": True},
        backend="local", sessions_root=tmp_path, depth="auth",
    )
    assert h.status == "auth_failed"
    assert h.layer == "auth"
    assert "403" in h.detail


def test_binding_depth_never_probes(tmp_path, monkeypatch):
    """depth='binding' must NOT fire the auth hello (it's the cheap badge path)."""
    _register_claude(tmp_path)
    called = {"n": 0}

    def _boom(profile):
        called["n"] += 1
        raise AssertionError("driver_for must not be called at binding depth")

    monkeypatch.setattr("muteki.solver.cli_driver.driver_for", _boom)
    h = evaluate_profile_health(
        {"name": "claude-local", "engine": "claude",
         "credential_account": "claude-main", "credential_mode": "api_key", "enabled": True},
        backend="local", sessions_root=tmp_path, depth="binding",
    )
    assert h.status == "ok"
    assert called["n"] == 0
