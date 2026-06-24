"""Default worker-roster config (apps/web/worker_config.py) + operator runtime
worker commands (RunManager.post_worker_cmd). Pure/unit — no subprocess, no key."""

from __future__ import annotations

import asyncio
import os
import subprocess

import pytest

from apps.web.run_manager import RunManager
from apps.web.drivers import _missing_profile_accounts
from apps.web.drivers import build_driver
from apps.web.worker_config import (
    DEFAULT_ENGINES,
    DEFAULT_RUNTIME_PROFILES,
    DEFAULT_WORKER_PROFILES,
    WorkerConfigStore,
)


# ── default config + validation ──────────────────────────────────────────────

def test_config_defaults_when_empty(tmp_path):
    wc = WorkerConfigStore(root=tmp_path)
    cfg = wc.get()
    assert cfg["engines"] == DEFAULT_ENGINES
    assert cfg["start_workers"] == len(DEFAULT_ENGINES)
    assert cfg["max_workers"] == 10
    assert cfg["worker_backend"] == "container"
    assert cfg["race_scout"] is True
    assert cfg["race_timeout"] == 720
    assert cfg["wall_clock_budget"] == 0
    assert cfg["race_engines"] == []
    assert cfg["max_total_workers"] == 0
    assert cfg["cost_budget_usd"] == 0.0
    assert cfg["stage_policy"]["budgets"]["max_total_workers"] == 0
    assert cfg["stage_policy"]["coordinator"]["review"]["enabled"] is True
    assert cfg["stage_policy"]["coordinator"]["review"]["engine"] == "claude-sub-container"
    assert cfg["stage_policy"]["coordinator"]["review"]["candidate_spike_threshold"] == 5
    assert cfg["llm_profiles"]["planner"]["model"] == "deepseek-v4-pro"
    assert cfg["runtime_profiles"] == DEFAULT_RUNTIME_PROFILES
    assert {r["id"] for r in cfg["runtime_profiles"]} >= {
        "docker-host-target", "docker-offline", "docker-pwn-heavy"}
    assert cfg["worker_profiles"] == DEFAULT_WORKER_PROFILES
    assert cfg["overrides"] == {}


def test_config_set_engines_dedupes_and_filters(tmp_path):
    wc = WorkerConfigStore(root=tmp_path)
    cfg = wc.set(engines=["claude", "bogus", "claude", "codex"])
    assert cfg["engines"] == ["claude-sub-container", "codex-sub-container"]


def test_config_set_rejects_empty_engine_list(tmp_path):
    wc = WorkerConfigStore(root=tmp_path)
    with pytest.raises(ValueError):
        wc.set(engines=["nope", "alsо_bad"])  # nothing valid → reject


def test_config_set_rejects_nonpositive_counts(tmp_path):
    wc = WorkerConfigStore(root=tmp_path)
    with pytest.raises(ValueError):
        wc.set(start_workers=0)
    with pytest.raises(ValueError):
        wc.set(max_workers=-3)


def test_config_set_links_profile_capacity_to_max_workers(tmp_path):
    wc = WorkerConfigStore(root=tmp_path)
    cfg = wc.set(
        engines=["claude", "codex", "cursor"],
        max_workers=10,
        worker_profiles=[
            {**p, "max_running": p["max_running"]}
            for p in DEFAULT_WORKER_PROFILES
        ],
    )

    by_name = {p["name"]: p for p in cfg["worker_profiles"]}
    selected = [by_name[name] for name in cfg["engines"]]
    assert sum(int(p["max_running"]) for p in selected) == 10
    assert [p["max_running"] for p in selected] == [4, 3, 3]


def test_config_set_clamps_start_workers_to_max_workers(tmp_path):
    wc = WorkerConfigStore(root=tmp_path)
    cfg = wc.set(start_workers=7, max_workers=3)

    assert cfg["start_workers"] == 3
    assert cfg["max_workers"] == 3


def test_config_persists_across_reload(tmp_path):
    WorkerConfigStore(root=tmp_path).set(engines=["claude"], start_workers=1,
                                         max_workers=4, worker_backend="container",
                                         race_scout=False, race_timeout=300,
                                         wall_clock_budget=1800,
                                         race_engines=["claude"],
                                         max_total_workers=11,
                                         cost_budget_usd=1.25,
                                         stage_policy={
                                             "coordinator": {
                                                 "wall_clock_budget": 1800,
                                                 "review": {
                                                     "enabled": True,
                                                     "engine": "codex-sub-container",
                                                     "timeout": 333,
                                                     "after_fruitless_workers": 2,
                                                     "candidate_spike_threshold": 4,
                                                 },
                                             }
                                         },
                                         llm_profiles={
                                             "planner": {"provider": "deepseek", "model": "planner-x"},
                                             "titler": {"provider": "deepseek", "model": "titler-x"},
                                         })
    cfg = WorkerConfigStore(root=tmp_path).get()  # fresh load from disk
    assert cfg["engines"] == ["claude-sub-container"]
    assert cfg["start_workers"] == 1 and cfg["max_workers"] == 4
    assert cfg["worker_backend"] == "container"
    assert cfg["race_scout"] is False
    assert cfg["race_timeout"] == 300
    assert cfg["wall_clock_budget"] == 1800
    assert cfg["race_engines"] == ["claude-sub-container"]
    assert cfg["max_total_workers"] == 11
    assert cfg["cost_budget_usd"] == 1.25
    assert cfg["stage_policy"]["race"]["engines"] == ["claude-sub-container"]
    assert cfg["stage_policy"]["coordinator"]["review"]["engine"] == "codex-sub-container"
    assert cfg["stage_policy"]["coordinator"]["review"]["timeout"] == 333
    assert cfg["stage_policy"]["coordinator"]["review"]["after_fruitless_workers"] == 2
    assert cfg["stage_policy"]["coordinator"]["review"]["candidate_spike_threshold"] == 4
    assert cfg["llm_profiles"]["titler"]["model"] == "titler-x"


# ── per-category override + resolve ──────────────────────────────────────────

def test_resolve_uses_default_without_override(tmp_path):
    wc = WorkerConfigStore(root=tmp_path)
    wc.set(engines=["cursor", "claude", "codex"], start_workers=3,
           worker_backend="container", race_timeout=240)
    r = wc.resolve("web")
    assert r["engines"] == ["cursor-api-container", "claude-sub-container", "codex-sub-container"]
    assert r["start_workers"] == 3
    assert r["worker_backend"] == "container"
    assert r["race_timeout"] == 240
    assert r["worker_profiles"] == wc.get()["worker_profiles"]
    assert r["runtime_profiles"] == wc.get()["runtime_profiles"]


def test_resolve_applies_category_override(tmp_path):
    wc = WorkerConfigStore(root=tmp_path)
    wc.set(engines=["cursor", "claude", "codex"],
           overrides={"pwn": {"engines": ["claude", "codex"], "start_workers": 2}})
    r = wc.resolve("pwn")
    assert r["engines"] == ["claude-sub-container", "codex-sub-container"]
    assert r["start_workers"] == 2
    # a category with no override still gets the defaults
    assert wc.resolve("web")["engines"] == [
        "cursor-api-container", "claude-sub-container", "codex-sub-container"]


def test_resolve_override_start_workers_defaults_to_engine_count(tmp_path):
    wc = WorkerConfigStore(root=tmp_path)
    wc.set(overrides={"crypto": {"engines": ["claude"]}})  # no start_workers
    assert wc.resolve("crypto")["start_workers"] == 1


# ── RunManager.post_worker_cmd (operator runtime control) ────────────────────

def test_post_worker_cmd_requires_live_run(tmp_path):
    mgr = RunManager(sessions_root=tmp_path)
    run = mgr.create("run-x")
    # no live task → rejected (a finished/ghost run has no coordinator to act)
    assert asyncio.run(mgr.post_worker_cmd("run-x", "spawn", engine="claude")) is False


def test_post_worker_cmd_enqueues_for_live_run(tmp_path):
    mgr = RunManager(sessions_root=tmp_path)
    run = mgr.create("run-y")

    async def go():
        run.task = asyncio.create_task(asyncio.sleep(3600))
        ok_spawn = await mgr.post_worker_cmd("run-y", "spawn", engine="cursor")
        ok_kill = await mgr.post_worker_cmd("run-y", "kill", solver_id="cli-cursor")
        run.task.cancel()
        return ok_spawn, ok_kill, [run.worker_cmds.get_nowait(),
                                   run.worker_cmds.get_nowait()]

    ok_spawn, ok_kill, cmds = asyncio.run(go())
    assert ok_spawn is True and ok_kill is True
    assert cmds[0] == {"action": "spawn", "engine": "cursor"}
    assert cmds[1] == {"action": "kill", "solver_id": "cli-cursor"}


def test_post_worker_cmd_unknown_run_returns_false(tmp_path):
    mgr = RunManager(sessions_root=tmp_path)
    assert asyncio.run(mgr.post_worker_cmd("ghost", "spawn", engine="claude")) is False


def test_config_rejects_invalid_backend_and_profiles(tmp_path):
    wc = WorkerConfigStore(root=tmp_path)
    with pytest.raises(ValueError):
        wc.set(worker_backend="vm")
    with pytest.raises(ValueError):
        wc.set(runtime_profiles=[{"id": "bad", "backend": "vm"}])
    with pytest.raises(ValueError):
        wc.set(worker_profiles=[{"id": "bad", "engine": "deepseek"}])


def test_config_accepts_profile_schema(tmp_path):
    wc = WorkerConfigStore(root=tmp_path)
    cfg = wc.set(
        runtime_profiles=[{"id": "docker-web", "backend": "container", "label": "Docker Web"}],
        worker_profiles=[{
            "id": "claude-api-container",
            "engine": "claude",
            "transport": "claude_code",
            "auth": "oauth_token",
            "credential_account": "claude-team",
            "runtime": "docker-web",
            "roles": ["bootstrap", "explore"],
            "race": False,
            "max_running": 3,
            "priority": 7,
            "model": "sonnet",
            "enabled": True,
        }],
    )
    assert cfg["runtime_profiles"][0]["backend"] == "container"
    p = cfg["worker_profiles"][0]
    assert p["credential_account"] == "claude-team"
    assert p["name"] == "claude-api-container"
    assert p["credential_mode"] == "oauth_token"
    assert p["roles"] == ["bootstrap", "explore", "review"]
    assert p["race"] is False
    assert p["max_running"] == 3
    assert p["priority"] == 7
    assert p["model"] == "sonnet"


def test_worker_profiles_accept_review_role_and_default_roles_include_review(tmp_path):
    wc = WorkerConfigStore(root=tmp_path)
    cfg = wc.set(
        runtime_profiles=[{"id": "local", "backend": "local", "label": "Local"}],
        worker_profiles=[{
            "id": "review-codex",
            "engine": "codex",
            "runtime": "local",
            "roles": ["review"],
            "enabled": True,
        }],
        engines=["review-codex"],
        stage_policy={"coordinator": {"review": {"engine": "review-codex"}}},
    )
    assert cfg["worker_profiles"][0]["roles"] == ["review"]
    assert cfg["stage_policy"]["coordinator"]["review"]["engine"] == "review-codex"

    cfg2 = WorkerConfigStore(root=tmp_path / "other").set(
        runtime_profiles=[{"id": "local", "backend": "local", "label": "Local"}],
        worker_profiles=[{
            "id": "default-roles",
            "engine": "claude",
            "runtime": "local",
            "enabled": True,
        }],
        engines=["default-roles"],
    )
    assert "review" in cfg2["worker_profiles"][0]["roles"]


def test_worker_profile_migrates_execution_roles_to_review(tmp_path):
    wc = WorkerConfigStore(root=tmp_path)
    cfg = wc.set(
        engines=["legacy-claude"],
        worker_profiles=[{
            "id": "legacy-claude",
            "engine": "claude",
            "transport": "claude_code",
            "runtime": "local",
            "roles": ["race", "bootstrap", "explore"],
        }],
    )

    roles = cfg["worker_profiles"][0]["roles"]
    assert roles == ["race", "bootstrap", "explore", "review"]


def test_config_preserves_blank_credential_account_for_local_subscription_cli(tmp_path):
    wc = WorkerConfigStore(root=tmp_path)
    cfg = wc.set(
        runtime_profiles=[{"id": "local", "backend": "local", "label": "Local"}],
        worker_profiles=[{
            "id": "codex-local",
            "engine": "codex",
            "transport": "codex_cli",
            "auth": "subscription",
            "credential_account": "",
            "runtime": "local",
            "roles": ["race", "bootstrap", "explore"],
            "enabled": True,
        }],
        engines=["codex-local"],
    )

    assert cfg["worker_profiles"][0]["credential_account"] == ""


def test_config_rejects_duplicate_profile_ids_and_unknown_runtime(tmp_path):
    wc = WorkerConfigStore(root=tmp_path)
    with pytest.raises(ValueError):
        wc.set(worker_profiles=[
            {"id": "dup", "engine": "claude", "runtime": "local"},
            {"id": "dup", "engine": "codex", "runtime": "local"},
        ])
    with pytest.raises(ValueError):
        wc.set(worker_profiles=[
            {"id": "bad-runtime", "engine": "claude", "runtime": "missing"},
        ])


def test_missing_profile_accounts_detects_container_and_api_profiles(tmp_path, monkeypatch):
    monkeypatch.setattr("apps.web.drivers.detect_system_login", lambda engine: "absent")
    missing = _missing_profile_accounts(
        worker_profiles=[
            {"id": "claude-sub", "engine": "claude", "runtime": "docker-web",
             "auth": "subscription", "credential_account": "claude-main", "enabled": True},
            {"id": "local-sub", "engine": "claude", "runtime": "local",
             "auth": "subscription", "credential_account": "unused", "enabled": True},
            {"id": "deepseek", "engine": "codex", "runtime": "local",
             "auth": "api_key", "credential_account": "deepseek-main", "enabled": True},
        ],
        runtime_profiles=[
            {"id": "local", "backend": "local"},
            {"id": "docker-web", "backend": "container"},
        ],
        sessions_root=tmp_path,
    )
    assert "claude-sub:claude-main" in missing
    assert "deepseek:deepseek-main" in missing
    assert all("local-sub" not in x for x in missing)


def test_missing_profile_accounts_allows_local_system_login_without_account(tmp_path, monkeypatch):
    monkeypatch.setattr("apps.web.drivers.detect_system_login", lambda engine: "present")
    monkeypatch.setattr("apps.web.drivers.driver_for", lambda profile: type(
        "D", (), {"health_detail": lambda self, env=None: (True, "")})())

    missing = _missing_profile_accounts(
        worker_profiles=[{
            "id": "cursor-api-local",
            "engine": "cursor",
            "runtime": "local",
            "auth": "api_key",
            "credential_account": "cursor-main",
            "enabled": True,
        }],
        runtime_profiles=[{"id": "local", "backend": "local"}],
        sessions_root=tmp_path,
    )

    assert missing == []


def test_worker_config_accepts_api_endpoint_profile_names(tmp_path):
    wc = WorkerConfigStore(root=tmp_path)
    cfg = wc.set(
        runtime_profiles=[{"id": "local", "backend": "local"}],
        worker_profiles=[{
            "id": "deepseek-codex",
            "name": "deepseek-codex",
            "engine": "codex",
            "transport": "codex_cli",
            "credential_mode": "api",
            "credential_account": "deepseek-main",
            "base_url": "https://api.deepseek.example/v1",
            "api_key_ref": "env:MUTEKI_DEEPSEEK_API_KEY",
            "wire_api": "responses",
            "runtime": "local",
            "model": "deepseek-chat",
        }],
        engines=["deepseek-codex"],
    )

    assert cfg["engines"] == ["deepseek-codex"]
    p = cfg["worker_profiles"][0]
    assert p["engine"] == "codex"
    assert p["base_url"] == "https://api.deepseek.example/v1"
    assert p["api_key_ref"] == "env:MUTEKI_DEEPSEEK_API_KEY"


def test_profile_endpoint_healthcheck_uses_endpoint_url(tmp_path, monkeypatch):
    root = tmp_path / "_secrets" / "accounts" / "deepseek-main"
    root.mkdir(parents=True)
    (root / "API_KEY").write_text("secret\n")

    seen = {}

    def fake_run(argv, **kwargs):
        seen["argv"] = argv
        return subprocess.CompletedProcess(argv, 0, "{}", "")

    monkeypatch.setattr(subprocess, "run", fake_run)
    missing = _missing_profile_accounts(
        worker_profiles=[{
            "id": "deepseek-codex",
            "name": "deepseek-codex",
            "engine": "codex",
            "transport": "codex_cli",
            "credential_mode": "api",
            "credential_account": "deepseek-main",
            "base_url": "https://api.deepseek.example/v1",
            "api_key_ref": "env:MUTEKI_DEEPSEEK_API_KEY",
            "runtime": "local",
            "enabled": True,
        }],
        runtime_profiles=[{"id": "local", "backend": "local"}],
        sessions_root=tmp_path,
    )

    assert missing == []
    assert "https://api.deepseek.example/v1/responses" in seen["argv"]


def test_profile_account_probe_runs_minimal_model_with_injected_account(tmp_path, monkeypatch):
    root = tmp_path / "_secrets" / "accounts" / "claude-main"
    root.mkdir(parents=True)
    (root / "CLAUDE_CODE_OAUTH_TOKEN").write_text("oauth-token\n")

    seen = {}

    def fake_run(argv, **kwargs):
        seen["argv"] = argv
        # the probe now passes the credential env EXPLICITLY to the subprocess
        # (env=) instead of mutating the global os.environ — that's what makes
        # parallel probes safe. Read it from the passed env, not os.environ.
        seen["token"] = (kwargs.get("env") or {}).get("CLAUDE_CODE_OAUTH_TOKEN")
        return subprocess.CompletedProcess(argv, 0, '{"result":"OK"}\n', "")

    monkeypatch.setattr(subprocess, "run", fake_run)
    missing = _missing_profile_accounts(
        worker_profiles=[{
            "id": "claude-sub",
            "name": "claude-sub",
            "engine": "claude",
            "transport": "claude_code",
            "credential_mode": "subscription",
            "credential_account": "claude-main",
            "runtime": "docker-web",
            "enabled": True,
        }],
        runtime_profiles=[{"id": "docker-web", "backend": "container"}],
        sessions_root=tmp_path,
    )

    assert missing == []
    assert seen["token"] == "oauth-token"
    assert any("Reply with exactly: OK" in str(x) for x in seen["argv"])


def test_offline_rejects_custom_endpoint_profiles(tmp_path):
    mgr = RunManager(sessions_root=tmp_path)
    driver = build_driver({
        "prompt": "solve http://example.test",
        "offline": True,
        "engines": ["deepseek-codex"],
        "worker_profiles": [{
            "id": "deepseek-codex",
            "name": "deepseek-codex",
            "engine": "codex",
            "transport": "codex_cli",
            "credential_mode": "api",
            "credential_account": "deepseek-main",
            "base_url": "https://api.deepseek.example/v1",
            "api_key_ref": "env:MUTEKI_DEEPSEEK_API_KEY",
            "runtime": "local",
            "enabled": True,
        }],
        "runtime_profiles": [{"id": "local", "backend": "local"}],
    }, mgr=mgr)
    run = mgr.create("endpoint-offline")

    with pytest.raises(RuntimeError, match="offline eval cannot use custom endpoint"):
        asyncio.run(driver(run))


# ── llm_profiles base_url (DESIGN §2.2 補強A) ────────────────────────────────

def test_llm_profiles_base_url_accepted_and_normalized(tmp_path):
    """planner/titler accept an OpenAI-compatible base_url; garbage → "". The
    API key is NOT stored here (stays in .env)."""
    WorkerConfigStore(root=tmp_path).set(llm_profiles={
        "planner": {"provider": "deepseek", "model": "planner-x",
                    "base_url": "  https://api.openai-compat.test/v1  "},
        "titler": {"provider": "deepseek", "model": "titler-x", "base_url": 12345},
    })
    cfg = WorkerConfigStore(root=tmp_path).get()  # fresh load from disk
    # trimmed, persisted
    assert cfg["llm_profiles"]["planner"]["base_url"] == "https://api.openai-compat.test/v1"
    # non-string garbage normalizes to empty (= default DeepSeek), never crashes
    assert cfg["llm_profiles"]["titler"]["base_url"] == ""
    # no api key leaked into config under any common name
    assert "api_key" not in cfg["llm_profiles"]["planner"]
    assert "key" not in cfg["llm_profiles"]["planner"]


def test_llm_profiles_base_url_defaults_empty(tmp_path):
    cfg = WorkerConfigStore(root=tmp_path).get()
    assert cfg["llm_profiles"]["planner"]["base_url"] == ""
    assert cfg["llm_profiles"]["titler"]["base_url"] == ""


# ── runtime-environment write-back (DESIGN §5) ───────────────────────────────

def test_set_runtime_environment_unifies_all_enabled_profiles(tmp_path):
    """Choosing local/container rewrites EVERY enabled profile's runtime —
    including profiles only referenced by a category override, not in the default
    engines (reviewer P2). Otherwise an override keeps an old runtime and the
    "displayed local, actually container" bug returns."""
    store = WorkerConfigStore(root=tmp_path)
    # seed profiles: one default + one only used by a pwn override, with DIFFERENT
    # runtimes so we can prove both get rewritten.
    store.set(
        worker_profiles=[
            {"id": "claude-sub", "name": "claude-sub", "engine": "claude",
             "transport": "claude_code", "credential_mode": "subscription",
             "credential_account": "claude-main", "runtime": "docker-web",
             "enabled": True},
            {"id": "codex-override", "name": "codex-override", "engine": "codex",
             "transport": "codex_cli", "credential_mode": "subscription",
             "credential_account": "codex-main", "runtime": "docker-pwn-heavy",
             "enabled": True},
        ],
        engines=["claude-sub"],  # only claude-sub is a DEFAULT engine
    )
    store.set(overrides={"pwn": {"engines": ["codex-override"]}})

    # flip the whole run to local
    cfg = store.set_runtime_environment(backend="local", runtime_id="local")
    assert cfg["worker_backend"] == "local"
    runtimes = {p["id"]: p["runtime"] for p in cfg["worker_profiles"]}
    # BOTH the default engine AND the override-only profile got rewritten
    assert runtimes["claude-sub"] == "local"
    assert runtimes["codex-override"] == "local"

    # flip to a container recipe
    cfg = store.set_runtime_environment(backend="container", runtime_id="docker-offline")
    assert cfg["worker_backend"] == "container"
    runtimes = {p["id"]: p["runtime"] for p in cfg["worker_profiles"]}
    assert runtimes["claude-sub"] == "docker-offline"
    assert runtimes["codex-override"] == "docker-offline"


def test_set_runtime_environment_renames_builtin_profiles_for_backend(tmp_path):
    store = WorkerConfigStore(root=tmp_path)

    local = store.set_runtime_environment(backend="local", runtime_id="local")
    local_ids = [p["id"] for p in local["worker_profiles"]]
    assert local_ids == ["claude-local", "codex-local", "cursor-api-local"]
    assert local["engines"] == local_ids
    assert local["stage_policy"]["coordinator"]["review"]["engine"] == "claude-local"
    assert all(p["runtime"] == "local" for p in local["worker_profiles"])
    assert all(not p["id"].endswith("-container") for p in local["worker_profiles"])

    container = store.set_runtime_environment(backend="container", runtime_id="docker-web")
    container_ids = [p["id"] for p in container["worker_profiles"]]
    assert container_ids == [
        "claude-sub-container",
        "codex-sub-container",
        "cursor-api-container",
    ]
    assert container["engines"] == container_ids
    assert container["stage_policy"]["coordinator"]["review"]["engine"] == "claude-sub-container"
    assert all(p["runtime"] == "docker-web" for p in container["worker_profiles"])


def test_get_remaps_stale_builtin_refs_to_current_backend(tmp_path):
    raw = {
        "worker_backend": "local",
        "engines": ["claude-sub-container", "codex-sub-container"],
        "race_engines": ["claude-sub-container"],
        "stage_policy": {
            "race": {"enabled": True, "timeout": 720, "engines": ["codex-sub-container"]},
            "coordinator": {"review": {"enabled": True, "engine": "claude-sub-container"}},
        },
        "worker_profiles": [
            {**DEFAULT_WORKER_PROFILES[0], "id": "claude-local", "name": "claude-local", "runtime": "local"},
            {**DEFAULT_WORKER_PROFILES[1], "id": "codex-local", "name": "codex-local", "runtime": "local"},
        ],
    }
    root = tmp_path / "_worker_config.json"
    root.write_text(__import__("json").dumps(raw), encoding="utf-8")

    cfg = WorkerConfigStore(root=tmp_path).get()

    assert cfg["engines"] == ["claude-local", "codex-local"]
    assert cfg["race_engines"] == ["claude-local"]
    assert cfg["stage_policy"]["race"]["engines"] == ["codex-local"]
    assert cfg["stage_policy"]["coordinator"]["review"]["engine"] == "claude-local"


def test_set_remaps_stale_builtin_refs_to_submitted_backend_profiles(tmp_path):
    store = WorkerConfigStore(root=tmp_path)
    local = store.set_runtime_environment(backend="local", runtime_id="local")

    cfg = store.set(
        engines=["claude-sub-container", "codex-sub-container", "cursor-api-container"],
        worker_profiles=local["worker_profiles"],
        stage_policy={
            "race": {"enabled": True, "timeout": 720, "engines": ["codex-sub-container"]},
            "coordinator": {"review": {"enabled": True, "engine": "claude-sub-container"}},
        },
    )

    assert cfg["engines"] == ["claude-local", "codex-local", "cursor-api-local"]
    assert cfg["stage_policy"]["race"]["engines"] == ["codex-local"]
    assert cfg["stage_policy"]["coordinator"]["review"]["engine"] == "claude-local"


def test_set_runtime_environment_rejects_backend_runtime_mismatch(tmp_path):
    store = WorkerConfigStore(root=tmp_path)
    with pytest.raises(ValueError, match="not 'local'"):
        store.set_runtime_environment(backend="local", runtime_id="docker-web")
    with pytest.raises(ValueError, match="unknown runtime"):
        store.set_runtime_environment(backend="container", runtime_id="nope")
