"""Tests for the new Credential/Seat/Environment identity model + legacy migration.

Locks the Phase-A foundation (plan_settings_identity_refactor.md §3.3/§3.7/§5.0):
deterministic ids, kind mapping, the empty-binding default-account fallback that
mirrors the backend (profile_health.py:159), and the container×system_inherit
legality rule.
"""

from __future__ import annotations

from muteki.solver.identity_model import (
    Credential,
    Seat,
    credential_id_for,
    seat_id_for,
    kind_from_mode,
    is_legal_combo,
    migrate_legacy_config,
    seat_to_legacy_profile,
    seats_to_legacy_profiles,
)
from muteki.solver.worker_profiles import resolve_seat_ref


# ── deterministic ids ────────────────────────────────────────────────────────

def test_seat_and_credential_ids_are_deterministic_and_prefixed():
    a = seat_id_for("claude", legacy_name="claude-local")
    b = seat_id_for("claude", legacy_name="claude-local")
    assert a == b
    assert a.startswith("seat_claude_") and len(a) == len("seat_claude_") + 6
    c = credential_id_for("codex", legacy_account_id="codex-main")
    assert c == credential_id_for("codex", legacy_account_id="codex-main")
    assert c.startswith("cred_codex_")


def test_distinct_inputs_yield_distinct_ids():
    assert seat_id_for("claude", legacy_name="a") != seat_id_for("claude", legacy_name="b")
    assert credential_id_for("claude", legacy_account_id="x") != credential_id_for(
        "cursor", legacy_account_id="x"
    )


# ── kind mapping ─────────────────────────────────────────────────────────────

def test_kind_from_mode_collapses_engine_official_creds():
    assert kind_from_mode("subscription_token") == "engine_key"
    assert kind_from_mode("chatgpt_auth_home") == "engine_key"
    assert kind_from_mode("api_key") == "engine_key"
    assert kind_from_mode("custom_endpoint") == "custom_endpoint"
    # unknown present credential → engine_key (not endpoint)
    assert kind_from_mode("something_new") == "engine_key"


# ── §3.7 legality: container forbids system_inherit ──────────────────────────

def test_container_forbids_system_inherit():
    assert is_legal_combo(kind="system_inherit", backend="local") is True
    assert is_legal_combo(kind="system_inherit", backend="container") is False
    assert is_legal_combo(kind="engine_key", backend="container") is True
    assert is_legal_combo(kind="custom_endpoint", backend="container") is True


# ── migration: empty binding + present default account → engine_key ──────────

def test_empty_binding_with_present_default_account_becomes_engine_key():
    """The codex-local case: credential_account='' but codex-main exists on disk
    → bind to codex-main as engine_key (mirrors backend fallback), NOT inherit."""
    res = migrate_legacy_config(
        worker_profiles=[{
            "id": "codex-local", "name": "codex-local", "engine": "codex",
            "credential_mode": "subscription", "credential_account": "",
            "runtime": "docker-web", "model": "gpt-5.4", "enabled": True,
        }],
        runtime_profiles=[{"id": "docker-web", "backend": "container"}],
        account_modes={"codex-main": "chatgpt_auth_home"},
    )
    assert len(res.credentials) == 1
    cred = res.credentials[0]
    assert cred.kind == "engine_key"
    assert cred.secret_ref == "codex-main"
    # the bare-engine default account id is aliased to the new credential id
    assert res.credential_alias.get("codex-main") == cred.id


def test_empty_binding_with_no_default_account_becomes_system_inherit():
    """No bound account AND no default on disk → host login inherit."""
    res = migrate_legacy_config(
        worker_profiles=[{
            "id": "claude-local", "name": "claude-local", "engine": "claude",
            "credential_mode": "subscription", "credential_account": "",
            "runtime": "local", "enabled": True,
        }],
        runtime_profiles=[{"id": "local", "backend": "local"}],
        account_modes={},  # nothing on disk
    )
    assert res.credentials[0].kind == "system_inherit"
    assert res.credentials[0].secret_ref == ""


def test_base_url_profile_becomes_custom_endpoint():
    res = migrate_legacy_config(
        worker_profiles=[{
            "id": "claude-ds", "name": "claude-ds", "engine": "claude",
            "credential_mode": "api_key", "credential_account": "claude-ds",
            "base_url": "https://api.deepseek.com/anthropic", "wire_api": "",
            "runtime": "local", "model": "deepseek-v4-pro", "enabled": True,
        }],
        runtime_profiles=[{"id": "local", "backend": "local"}],
        account_modes={"claude-ds": "custom_endpoint"},
    )
    cred = res.credentials[0]
    assert cred.kind == "custom_endpoint"
    assert cred.base_url == "https://api.deepseek.com/anthropic"
    assert cred.target_engine == "claude"


# ── migration preserves model, race, roles, priority ─────────────────────────

def test_migration_preserves_seat_fields():
    res = migrate_legacy_config(
        worker_profiles=[{
            "id": "claude-local", "name": "claude-local", "engine": "claude",
            "credential_account": "claude-main", "runtime": "docker-web",
            "model": "claude-opus-4-8", "roles": ["race", "review"], "race": False,
            "max_running": 3, "priority": 10, "enabled": True,
        }],
        runtime_profiles=[{"id": "docker-web", "backend": "container"}],
        account_modes={"claude-main": "subscription_token"},
    )
    seat = res.seats[0]
    assert seat.model == "claude-opus-4-8"
    assert seat.race is False           # explicit race-opt-out survives
    assert seat.roles == ["race", "review"]
    assert seat.max_running == 3
    assert seat.priority == 10
    assert res.seat_alias["claude-local"] == seat.id


def test_environments_mapped_from_runtime_profiles():
    res = migrate_legacy_config(
        worker_profiles=[],
        runtime_profiles=[
            {"id": "local", "backend": "local", "label": "Local host"},
            {"id": "docker-web", "backend": "container", "label": "Docker web",
             "network": "bridge", "memory": "12g", "cpus": "4", "pids_limit": 2048},
        ],
        account_modes={},
    )
    envs = {e.id: e for e in res.environments}
    assert envs["local"].backend == "local"
    assert envs["docker-web"].backend == "container"
    assert envs["docker-web"].network == "bridge"
    assert envs["docker-web"].pids_limit == 2048


# ── adapter: new model → legacy profile (round-trip faithfulness) ────────────

def test_adapter_round_trip_preserves_scheduler_fields():
    """legacy → migrate → adapt back must preserve the fields the swarm/drivers read."""
    legacy = {
        "id": "claude-local", "name": "claude-local", "engine": "claude",
        "credential_mode": "subscription", "credential_account": "claude-main",
        "runtime": "docker-web", "model": "claude-opus-4-8",
        "roles": ["race", "review"], "race": True,
        "max_running": 3, "max_review_running": 0, "priority": 10, "enabled": True,
    }
    res = migrate_legacy_config(
        worker_profiles=[legacy],
        runtime_profiles=[{"id": "docker-web", "backend": "container"}],
        account_modes={"claude-main": "subscription_token"},
    )
    back = seats_to_legacy_profiles(
        [s.to_dict() for s in res.seats],
        [c.to_dict() for c in res.credentials],
        [e.to_dict() for e in res.environments],
    )[0]
    assert back["engine"] == "claude"
    assert back["model"] == "claude-opus-4-8"
    assert back["credential_account"] == "claude-main"
    assert back["runtime"] == "docker-web"
    assert back["roles"] == ["race", "review"]
    assert back["max_running"] == 3
    assert back["priority"] == 10


def test_adapter_system_inherit_clears_credential_account():
    """A host-inherit credential must adapt to an EMPTY credential_account so the
    driver inherits the host login (no *_FILE injection)."""
    cred = Credential(id="cred_claude_x", label="claude 系统登录", engine="claude",
                      kind="system_inherit", secret_ref="").to_dict()
    seat = Seat(id="seat_claude_x", label="c", engine="claude",
                credential_id="cred_claude_x", environment_id="local").to_dict()
    env = {"id": "local", "backend": "local"}
    prof = seat_to_legacy_profile(seat, cred, env)
    assert prof["credential_account"] == ""
    assert prof["credential_mode"] == "subscription"


def test_adapter_custom_endpoint_wires_base_url():
    cred = Credential(id="cred_claude_ds", label="ds", engine="claude",
                      kind="custom_endpoint", secret_ref="claude-ds",
                      target_engine="claude",
                      base_url="https://api.deepseek.com/anthropic", wire_api="").to_dict()
    seat = Seat(id="seat_claude_ds", label="ds", engine="claude",
                credential_id="cred_claude_ds", environment_id="local",
                model="deepseek-v4-pro").to_dict()
    prof = seat_to_legacy_profile(seat, cred, {"id": "local", "backend": "local"})
    assert prof["base_url"] == "https://api.deepseek.com/anthropic"
    assert prof["credential_account"] == "claude-ds"
    assert prof["model"] == "deepseek-v4-pro"


# ── resolve_seat_ref: old name / hyphen alias / new id / bare engine ─────────

def test_resolve_seat_ref_all_forms():
    seats = [{"id": "seat_claude_ab12cd", "label": "claude-local", "engine": "claude"}]
    alias = {"claude-local": "seat_claude_ab12cd", "claude-sub-container": "seat_claude_ab12cd"}
    # new id
    assert resolve_seat_ref("seat_claude_ab12cd", seats=seats, alias_table=alias) == "seat_claude_ab12cd"
    # legacy name (via label)
    assert resolve_seat_ref("claude-local", seats=seats, alias_table=alias) == "seat_claude_ab12cd"
    # legacy hyphen canonical alias (via alias table)
    assert resolve_seat_ref("claude-sub-container", seats=seats, alias_table=alias) == "seat_claude_ab12cd"
    # bare engine with exactly one seat
    assert resolve_seat_ref("claude", seats=seats, alias_table=alias) == "seat_claude_ab12cd"
    # unknown → None (never silently swallowed)
    assert resolve_seat_ref("nope", seats=seats, alias_table=alias) is None


def test_resolve_seat_ref_bare_engine_ambiguous_returns_none():
    seats = [
        {"id": "seat_claude_1", "label": "a", "engine": "claude"},
        {"id": "seat_claude_2", "label": "b", "engine": "claude"},
    ]
    # two claude seats → bare "claude" is ambiguous → None (caller fans out)
    assert resolve_seat_ref("claude", seats=seats) is None


def test_migration_never_raises_on_malformed_entries():
    res = migrate_legacy_config(
        worker_profiles=[None, {}, {"engine": "bogus"}, 42,
                         {"id": "ok", "name": "ok", "engine": "claude",
                          "credential_account": "claude-main", "runtime": "local"}],
        runtime_profiles=[None, {"no_id": True}, {"id": "local", "backend": "local"}],
        account_modes={"claude-main": "subscription_token"},
    )
    # only the one valid profile survives
    assert len(res.seats) == 1
    assert res.seats[0].label == "ok"
