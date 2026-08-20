from __future__ import annotations

import asyncio
import json
import os
from pathlib import Path

import pytest

from muteki.solver.credential_accounts import (
    CONTAINER_ACCOUNTS_ROOT,
    CredentialAccountStore,
    account_store_root,
    runtime_env_for_engine,
)
from muteki.models.solve_graph import Challenge
from muteki.solver.cli_solver import CliSolver
from muteki.swarm.swarm import Swarm


def test_account_store_root_is_sessions_secret_side_table(tmp_path):
    assert account_store_root(tmp_path) == tmp_path / "_secrets" / "accounts"


def test_claude_container_prefers_token_file_without_reading_secret(tmp_path, monkeypatch):
    root = tmp_path / "_secrets" / "accounts"
    acct = root / "claude-main"
    acct.mkdir(parents=True)
    (acct / "CLAUDE_CODE_OAUTH_TOKEN").write_text("fake-claude-token\n")
    monkeypatch.delenv("CLAUDE_CODE_OAUTH_TOKEN", raising=False)

    resolved = runtime_env_for_engine("claude", account_root=root, container=True)

    assert resolved.account_id == "claude-main"
    assert resolved.env == {
        "CLAUDE_CODE_OAUTH_TOKEN_FILE": (
            f"{CONTAINER_ACCOUNTS_ROOT}/claude-main/CLAUDE_CODE_OAUTH_TOKEN"
        )
    }


def test_claude_local_reads_account_token_for_subprocess_env(tmp_path, monkeypatch):
    root = tmp_path / "_secrets" / "accounts"
    acct = root / "claude-main"
    acct.mkdir(parents=True)
    (acct / "CLAUDE_CODE_OAUTH_TOKEN").write_text("local-token\n")
    monkeypatch.delenv("CLAUDE_CODE_OAUTH_TOKEN", raising=False)

    resolved = runtime_env_for_engine("claude", account_root=root, container=False)

    assert resolved.env == {"CLAUDE_CODE_OAUTH_TOKEN": "local-token"}


def test_codex_uses_account_scoped_codex_home(tmp_path):
    root = tmp_path / "_secrets" / "accounts"
    codex_home = root / "codex-main" / "codex-home"
    codex_home.mkdir(parents=True)
    (codex_home / "auth.json").write_text("{}\n")

    local = runtime_env_for_engine("codex", account_root=root, container=False)
    container = runtime_env_for_engine("codex", account_root=root, container=True)

    assert local.env == {"CODEX_HOME": str(codex_home)}
    assert container.env == {"CODEX_HOME": f"{CONTAINER_ACCOUNTS_ROOT}/codex-main/codex-home"}


def test_codex_blank_account_id_skips_default_account_codex_home(tmp_path, monkeypatch):
    root = tmp_path / "_secrets" / "accounts"
    codex_home = root / "codex-main" / "codex-home"
    codex_home.mkdir(parents=True)
    (codex_home / "auth.json").write_text("{}\n")
    monkeypatch.delenv("CODEX_HOME", raising=False)

    resolved = runtime_env_for_engine(
        "codex", account_root=root, account_id="", container=False)

    assert resolved.account_id == ""
    assert resolved.env == {}


def test_codex_local_codex_home_is_absolute_when_account_root_is_relative(tmp_path, monkeypatch):
    root = Path("sessions") / "_secrets" / "accounts"
    codex_home = tmp_path / root / "codex-main" / "codex-home"
    codex_home.mkdir(parents=True)
    (codex_home / "auth.json").write_text("{}\n")
    monkeypatch.chdir(tmp_path)

    resolved = runtime_env_for_engine("codex", account_root=root, container=False)

    assert resolved.env["CODEX_HOME"] == str(codex_home.resolve())
    assert Path(resolved.env["CODEX_HOME"]).is_absolute()


def test_cursor_api_key_file_and_env_fallback(tmp_path, monkeypatch):
    root = tmp_path / "_secrets" / "accounts"
    acct = root / "cursor-main"
    acct.mkdir(parents=True)
    (acct / "CURSOR_API_KEY").write_text("cursor-secret\n")
    monkeypatch.setenv("CURSOR_API_KEY", "env-secret")

    resolved = runtime_env_for_engine("cursor", account_root=root, container=True)

    assert resolved.env == {
        "CURSOR_API_KEY_FILE": f"{CONTAINER_ACCOUNTS_ROOT}/cursor-main/CURSOR_API_KEY"
    }


def test_engine_account_id_can_be_overridden(tmp_path, monkeypatch):
    root = tmp_path / "_secrets" / "accounts"
    acct = root / "team-claude"
    acct.mkdir(parents=True)
    (acct / "CLAUDE_CODE_OAUTH_TOKEN").write_text("token\n")
    monkeypatch.setenv("MUTEKI_CLAUDE_ACCOUNT_ID", "team-claude")

    resolved = runtime_env_for_engine("claude", account_root=root, container=True)

    assert resolved.account_id == "team-claude"
    assert "team-claude" in resolved.env["CLAUDE_CODE_OAUTH_TOKEN_FILE"]


def test_runtime_env_accepts_explicit_profile_account_id(tmp_path):
    root = tmp_path / "_secrets" / "accounts"
    acct = root / "claude-team"
    acct.mkdir(parents=True)
    (acct / "CLAUDE_CODE_OAUTH_TOKEN").write_text("token\n")

    resolved = runtime_env_for_engine(
        "claude", account_root=root, account_id="claude-team", container=True)

    assert resolved.account_id == "claude-team"
    assert resolved.env == {
        "CLAUDE_CODE_OAUTH_TOKEN_FILE": (
            f"{CONTAINER_ACCOUNTS_ROOT}/claude-team/CLAUDE_CODE_OAUTH_TOKEN"
        )
    }


def test_credential_account_store_masks_and_replaces_material(tmp_path):
    store = CredentialAccountStore(account_store_root(tmp_path))

    claude = store.upsert_secret(
        account_id="shared-main", engine="claude", secret="claude-secret")
    assert claude["account_id"] == "shared-main"
    assert claude["engine"] == "claude"
    assert "secret_value" not in claude["details"]

    cursor = store.upsert_secret(
        account_id="shared-main", engine="cursor", secret="cursor-secret")
    assert cursor["engine"] == "cursor"
    base = account_store_root(tmp_path) / "shared-main"
    assert not (base / "CLAUDE_CODE_OAUTH_TOKEN").exists()
    assert (base / "CURSOR_API_KEY").read_text(encoding="utf-8").strip() == "cursor-secret"
    assert store.list()[0]["mode"] == "api_key"


def test_credential_account_store_validates_codex_json(tmp_path):
    store = CredentialAccountStore(account_store_root(tmp_path))
    with pytest.raises(ValueError):
        store.upsert_secret(account_id="codex-main", engine="codex", secret="{bad")

    acct = store.upsert_secret(account_id="codex-main", engine="codex", secret='{"token":"x"}')
    assert acct["engine"] == "codex"
    assert acct["writable_state"] is True
    assert acct["details"]["codex_home"] is True
    assert acct["details"]["mutable_auth_home"] is True
    assert "lease" not in acct["details"]
    assert "secret_value" not in acct["details"]


def test_invalid_update_does_not_destroy_existing_account(tmp_path):
    store = CredentialAccountStore(account_store_root(tmp_path))
    store.upsert_secret(account_id="codex-main", engine="codex", secret='{"token":"old"}')
    with pytest.raises(ValueError):
        store.upsert_secret(account_id="codex-main", engine="codex", secret="{bad")

    auth = account_store_root(tmp_path) / "codex-main" / "codex-home" / "auth.json"
    assert '"old"' in auth.read_text(encoding="utf-8")


def test_custom_endpoint_account_maps_to_engine_specific_env(tmp_path):
    root = account_store_root(tmp_path)
    store = CredentialAccountStore(root)
    acct = store.upsert_secret(
        account_id="deepseek-main",
        engine="api",
        secret="deepseek-key",
        base_url="https://api.deepseek.example/v1",
    )
    assert acct["engine"] == "api"
    assert "secret_value" not in acct["details"]

    codex = runtime_env_for_engine(
        "codex", account_root=root, account_id="deepseek-main", container=True)
    assert codex.env["OPENAI_API_KEY_FILE"].endswith("/deepseek-main/API_KEY")
    assert codex.env["OPENAI_BASE_URL"] == "https://api.deepseek.example/v1"

    claude = runtime_env_for_engine(
        "claude", account_root=root, account_id="deepseek-main", container=False)
    assert claude.env["ANTHROPIC_API_KEY"] == "deepseek-key"
    assert claude.env["ANTHROPIC_AUTH_TOKEN"] == "deepseek-key"
    assert claude.env["ANTHROPIC_BASE_URL"] == "https://api.deepseek.example/v1"


def test_custom_endpoint_records_target_engine_for_binding(tmp_path):
    """A custom endpoint registered FOR an agent reports that agent (not "api") so
    the panel can bind/display it — while runtime injection stays engine-agnostic."""
    root = account_store_root(tmp_path)
    store = CredentialAccountStore(root)
    acct = store.upsert_secret(
        account_id="claude-main",
        engine="api",
        secret="endpoint-key",
        base_url="https://anthropic.example/v1",
        target_engine="claude",
    )
    assert acct["engine"] == "claude"
    assert acct["mode"] == "custom_endpoint"
    assert acct["details"]["target_engine"] == "claude"
    assert acct["details"]["custom_endpoint"] is True
    # inspect() agrees, and accountForEngine-style lookup now matches claude.
    assert store.inspect("claude-main").engine == "claude"
    assert [a for a in store.list() if a["account_id"] == "claude-main"][0]["engine"] == "claude"

    # Engine-agnostic injection is preserved: the same dir still drives claude's env.
    env = runtime_env_for_engine(
        "claude", account_root=root, account_id="claude-main", container=False).env
    assert env["ANTHROPIC_BASE_URL"] == "https://anthropic.example/v1"
    assert env["ANTHROPIC_API_KEY"] == "endpoint-key"


def test_custom_endpoint_exposes_base_url_without_secret(tmp_path):
    """Public account metadata retains Base URL and omits the stored key."""
    root = account_store_root(tmp_path)
    store = CredentialAccountStore(root)
    acct = store.upsert_secret(
        account_id="deepseek-main", engine="api", secret="sk-super-secret",
        base_url="https://api.deepseek.com/v1", target_engine="claude")

    assert acct["details"]["base_url_value"] == "https://api.deepseek.com/v1"
    assert acct["details"]["base_url"] is True
    assert "secret_value" not in acct["details"]
    # Internal runtime inspection still resolves the key; public listing does not.
    assert store.inspect("deepseek-main").details["secret_value"] == "sk-super-secret"
    listed = [a for a in store.list() if a["account_id"] == "deepseek-main"][0]
    assert listed["details"]["base_url_value"] == "https://api.deepseek.com/v1"
    assert "secret_value" not in listed["details"]


def test_custom_endpoint_without_base_url_reports_empty_value(tmp_path):
    """No BASE_URL on disk → empty value + false boolean (not a crash)."""
    root = account_store_root(tmp_path)
    store = CredentialAccountStore(root)
    acct = store.upsert_secret(account_id="bare-main", engine="api", secret="k")
    assert acct["details"]["base_url_value"] == ""
    assert acct["details"]["base_url"] is False


def test_secret_value_is_omitted_for_claude_and_cursor(tmp_path):
    """Public account metadata never returns stored credential material."""
    root = account_store_root(tmp_path)
    store = CredentialAccountStore(root)

    claude = store.upsert_secret(account_id="claude-main", engine="claude", secret="oauth-tok")
    assert "secret_value" not in claude["details"]

    cursor = store.upsert_secret(account_id="cursor-main", engine="cursor", secret="cur-key")
    assert "secret_value" not in cursor["details"]

    # an empty account (no material) has no secret_value key at all
    (root / "ghost").mkdir()
    ghost = store.inspect("ghost")
    assert ghost is not None and ghost.present is False
    assert "secret_value" not in (ghost.details or {})


def test_custom_endpoint_without_target_engine_stays_api(tmp_path):
    """Back-compat: no target agent → engine "api" (legacy/programmatic accounts)."""
    root = account_store_root(tmp_path)
    store = CredentialAccountStore(root)
    acct = store.upsert_secret(
        account_id="shared-endpoint", engine="api", secret="k", base_url="https://x/v1")
    assert acct["engine"] == "api"
    assert acct["details"]["target_engine"] is None


def test_custom_endpoint_invalid_target_engine_rejected(tmp_path):
    root = account_store_root(tmp_path)
    store = CredentialAccountStore(root)
    with pytest.raises(ValueError):
        store.upsert_secret(
            account_id="x-main", engine="api", secret="k", target_engine="gpt")


def test_resaving_account_clears_stale_engine_marker(tmp_path):
    """Switching a marked custom-endpoint account back to a subscription token must
    not leave a stale ENGINE marker that mislabels it."""
    root = account_store_root(tmp_path)
    store = CredentialAccountStore(root)
    store.upsert_secret(
        account_id="claude-main", engine="api", secret="k",
        base_url="https://x/v1", target_engine="claude")
    assert (root / "claude-main" / "ENGINE").exists()
    again = store.upsert_secret(
        account_id="claude-main", engine="claude", secret="oauth-token")
    assert not (root / "claude-main" / "ENGINE").exists()
    assert again["mode"] == "subscription_token"


def test_blank_secret_edit_preserves_custom_endpoint_key(tmp_path):
    """Metadata-only edit: re-saving a custom endpoint with a blank secret keeps the
    stored API_KEY while updating base_url — the UI never reads the key back, so a
    base_url change must not require re-pasting it."""
    root = account_store_root(tmp_path)
    store = CredentialAccountStore(root)
    store.upsert_secret(
        account_id="deepseek-main", engine="api", secret="kept-key",
        base_url="https://old.example/v1", target_engine="claude")

    edited = store.upsert_secret(
        account_id="deepseek-main", engine="api", secret="",
        base_url="https://new.example/v1")

    base = root / "deepseek-main"
    assert (base / "API_KEY").read_text(encoding="utf-8").strip() == "kept-key"
    assert (base / "BASE_URL").read_text(encoding="utf-8").strip() == "https://new.example/v1"
    # target_engine left blank on edit → preserved from the prior save.
    assert edited["details"]["target_engine"] == "claude"
    assert (base / "ENGINE").read_text(encoding="utf-8").strip() == "claude"


def test_blank_secret_edit_preserves_target_engine_when_base_url_unchanged(tmp_path):
    """Re-pointing only the target engine (claude→codex) with a blank secret and
    blank base_url keeps both the key and the base_url."""
    root = account_store_root(tmp_path)
    store = CredentialAccountStore(root)
    store.upsert_secret(
        account_id="ep-main", engine="api", secret="kept",
        base_url="https://x/v1", target_engine="claude")

    edited = store.upsert_secret(
        account_id="ep-main", engine="api", secret="", target_engine="codex")

    base = root / "ep-main"
    assert (base / "API_KEY").read_text(encoding="utf-8").strip() == "kept"
    assert (base / "BASE_URL").read_text(encoding="utf-8").strip() == "https://x/v1"
    assert edited["details"]["target_engine"] == "codex"


def test_blank_secret_edit_preserves_claude_and_cursor_tokens(tmp_path):
    """Blank-secret re-save of a subscription/cursor account keeps the token rather
    than erroring on the required-secret guard."""
    root = account_store_root(tmp_path)
    store = CredentialAccountStore(root)

    store.upsert_secret(account_id="claude-main", engine="claude", secret="oauth-x")
    store.upsert_secret(account_id="claude-main", engine="claude", secret="")
    assert (root / "claude-main" / "CLAUDE_CODE_OAUTH_TOKEN").read_text(
        encoding="utf-8").strip() == "oauth-x"

    store.upsert_secret(account_id="cursor-main", engine="cursor", secret="cur-x")
    store.upsert_secret(account_id="cursor-main", engine="cursor", secret="")
    assert (root / "cursor-main" / "CURSOR_API_KEY").read_text(
        encoding="utf-8").strip() == "cur-x"


def test_blank_secret_edit_preserves_codex_auth_json(tmp_path):
    """Blank-secret re-save of a codex account keeps the stored auth.json."""
    root = account_store_root(tmp_path)
    store = CredentialAccountStore(root)
    store.upsert_secret(account_id="codex-main", engine="codex", secret='{"token":"keep"}')
    store.upsert_secret(account_id="codex-main", engine="codex", secret="")
    auth = root / "codex-main" / "codex-home" / "auth.json"
    assert '"keep"' in auth.read_text(encoding="utf-8")


def test_blank_secret_on_new_account_still_errors(tmp_path):
    """The preserve path must NOT weaken account creation: a blank secret with no
    prior account on disk still raises the required-secret error."""
    root = account_store_root(tmp_path)
    store = CredentialAccountStore(root)
    for engine in ("claude", "cursor", "api", "codex"):
        with pytest.raises(ValueError):
            store.upsert_secret(account_id=f"fresh-{engine}", engine=engine, secret="")


def test_local_runtime_does_not_override_host_home(tmp_path):
    ch = Challenge(
        id="home-local",
        name="home-local",
        category="misc",
        description="local home",
        flag_format="flag{...}",
    )
    swarm = Swarm(ch, [], llm=None, sandbox=None, worker_root=tmp_path / "workers")

    env = swarm._runtime_env_for("claude", "cli-claude", container=None)

    assert "HOME" not in env


def test_cursor_endpoint_is_inserted_before_prompt():
    ch = Challenge(
        id="cursor-endpoint",
        name="cursor-endpoint",
        category="misc",
        description="cursor endpoint",
        flag_format="flag{...}",
    )
    solver = CliSolver(None, ch, engine="cursor")
    argv = ["/usr/bin/cursor-agent", "-p", "--output-format", "json", "--force", "PROMPT"]

    out = solver._apply_runtime_argv(
        argv, {"CURSOR_ENDPOINT": "https://cursor-endpoint.example"})

    assert out[-3:] == ["--endpoint", "https://cursor-endpoint.example", "PROMPT"]


def test_profile_model_is_inserted_before_prompt_for_cli_drivers():
    ch = Challenge(
        id="profile-model",
        name="profile-model",
        category="misc",
        description="profile model",
        flag_format="flag{...}",
    )
    solver = CliSolver(None, ch, engine="codex")
    argv = ["/usr/bin/codex", "exec", "--json", "--skip-git-repo-check", "PROMPT"]

    out = solver._apply_runtime_argv(argv, {"MUTEKI_WORKER_MODEL": "deepseek-reasoner"})

    assert out[-3:] == ["--model", "deepseek-reasoner", "PROMPT"]


def test_pi_provider_and_model_are_inserted_before_prompt():
    ch = Challenge(
        id="pi-provider",
        name="pi-provider",
        category="misc",
        description="pi provider",
        flag_format="flag{...}",
    )
    solver = CliSolver(None, ch, engine="pi")
    argv = ["/usr/bin/pi", "-p", "--mode", "json", "PROMPT"]

    out = solver._apply_runtime_argv(argv, {
        "MUTEKI_PI_PROVIDER": "muteki",
        "MUTEKI_PI_MODEL": "deepseek-v4-flash:0731-cloud",
    })

    assert out[-5:] == ["--provider", "muteki",
                        "--model", "deepseek-v4-flash:0731-cloud", "PROMPT"]


def test_omp_provider_and_model_are_inserted_before_prompt():
    ch = Challenge(
        id="omp-provider",
        name="omp-provider",
        category="misc",
        description="omp provider",
        flag_format="flag{...}",
    )
    solver = CliSolver(None, ch, engine="omp")
    argv = ["/usr/local/bin/omp", "-p", "--mode", "json", "PROMPT"]

    out = solver._apply_runtime_argv(argv, {
        "MUTEKI_OMP_PROVIDER": "muteki",
        "MUTEKI_OMP_MODEL": "deepseek-v4-flash:0731-cloud",
    })

    assert out[-5:] == ["--provider", "muteki",
                        "--model", "deepseek-v4-flash:0731-cloud", "PROMPT"]


def test_swarm_profile_roles_and_capacity_are_hard_limits(tmp_path):
    ch = Challenge(
        id="profile-capacity",
        name="profile-capacity",
        category="misc",
        description="profile capacity",
        flag_format="flag{...}",
    )
    swarm = Swarm(
        ch, [], llm=None, sandbox=None,
        worker_root=tmp_path / "run" / "workspace" / "workers",
        worker_profiles=[
            {
                "id": "codex-main",
                "engine": "codex",
                "runtime": "local",
                "credential_account": "codex-main",
                "auth": "subscription",
                "roles": ["bootstrap", "explore"],
                "race": False,
                "max_running": 1,
                "enabled": True,
            }
        ],
    )

    assert swarm._profile_for_engine("codex", role="race", advance=False) is None
    profile = swarm._profile_for_engine("codex", role="bootstrap")
    assert profile is not None
    swarm._claim_worker_account("cli-codex", "codex", profile)

    assert swarm._profile_for_engine("codex", role="bootstrap", advance=False) is None
    assert swarm._engine_available_for_role("codex", "bootstrap") is False
    with pytest.raises(RuntimeError):
        swarm._make_cli_worker("codex", mode="bootstrap")

    class Done:
        solver_id = "cli-codex"

    swarm._release_worker_account(Done())
    assert swarm._engine_available_for_role("codex", "bootstrap") is True


def test_review_profile_capacity_is_isolated_from_explore_capacity(tmp_path):
    ch = Challenge(
        id="profile-review-capacity",
        name="profile-review-capacity",
        category="misc",
        description="profile capacity",
        flag_format="flag{...}",
    )
    swarm = Swarm(
        ch, [], llm=None, sandbox=None,
        worker_root=tmp_path / "run" / "workspace" / "workers",
        worker_profiles=[
            {
                "id": "claude-main",
                "engine": "claude",
                "runtime": "local",
                "credential_account": "claude-main",
                "auth": "subscription",
                "roles": ["bootstrap", "explore", "review"],
                "race": False,
                "max_running": 1,
                "enabled": True,
            }
        ],
        stage_policy={"coordinator": {"review": {"enabled": True, "max_concurrent": 1}}},
    )

    review_profile = swarm._profile_for_engine("claude", role="review")
    assert review_profile is not None
    swarm._claim_worker_account(
        "cli-claude-review", "claude", review_profile, role="review")

    assert swarm._profile_for_engine("claude", role="explore", advance=False) is not None
    assert swarm._engine_available_for_role("claude", "explore") is True
    assert swarm._profile_for_engine("claude", role="review", advance=False) is None


@pytest.mark.asyncio
async def test_done_review_task_keeps_slot_until_profile_release(tmp_path):
    ch = Challenge(
        id="profile-review-reap-window",
        name="profile-review-reap-window",
        category="misc",
        description="profile capacity",
        flag_format="flag{...}",
    )
    swarm = Swarm(
        ch, [], llm=None, sandbox=None,
        worker_root=tmp_path / "run" / "workspace" / "workers",
        worker_profiles=[
            {
                "id": "claude-main",
                "engine": "claude",
                "runtime": "local",
                "credential_account": "claude-main",
                "roles": ["review"],
                "enabled": True,
            }
        ],
        stage_policy={
            "coordinator": {
                "review": {
                    "enabled": True,
                    "engine": "claude-main",
                    "max_concurrent": 1,
                    "allow_review_fallback": False,
                }
            }
        },
    )
    profile = swarm._profile_for_engine("claude-main", role="review")
    assert profile is not None
    swarm._claim_worker_account(
        "cli-claude-review", "claude", profile, role="review")

    async def finished_review():
        return None

    task = asyncio.create_task(finished_review())
    await task
    assert task.done()
    swarm._active_review_tasks.add(task)

    assert swarm._review_capacity_available() is False
    assert task in swarm._active_review_tasks

    class Done:
        solver_id = "cli-claude-review"

    swarm._release_worker_account(Done())
    swarm._active_review_tasks.discard(task)
    assert swarm._review_capacity_available() is True
    assert swarm._select_review_engine(["claude"]) == "claude-main"


def test_review_engine_profile_id_uses_base_engine_health(tmp_path):
    ch = Challenge(
        id="profile-review-id-health",
        name="profile-review-id-health",
        category="misc",
        description="profile review id health",
        flag_format="flag{...}",
    )
    swarm = Swarm(
        ch, [], llm=None, sandbox=None,
        worker_root=tmp_path / "run" / "workspace" / "workers",
        worker_profiles=[
            {
                "id": "claude-sub-container",
                "engine": "claude",
                "runtime": "docker-web",
                "credential_account": "claude-main",
                "auth": "subscription",
                "roles": ["review"],
                "race": False,
                "max_running": 1,
                "enabled": True,
            }
        ],
        stage_policy={
            "coordinator": {
                "review": {
                    "enabled": True,
                    "engine": "claude-sub-container",
                    "max_concurrent": 1,
                    "allow_review_fallback": False,
                }
            }
        },
    )

    assert swarm._healthy_matches("claude-sub-container", ["claude"]) is True
    assert swarm._healthy_matches("claude-sub-container", ["claude-sub-container"]) is True
    assert swarm._select_review_engine(["claude"]) == "claude-sub-container"
    assert swarm._select_review_engine(["claude-sub-container"]) == "claude-sub-container"


def test_pick_engine_uses_configured_profile_roster_with_base_health(tmp_path):
    ch = Challenge(
        id="profile-pick-base-health",
        name="profile-pick-base-health",
        category="misc",
        description="profile pick base health",
        flag_format="flag{...}",
    )
    swarm = Swarm(
        ch, [], llm=None, sandbox=None,
        worker_root=tmp_path / "run" / "workspace" / "workers",
        worker_profiles=[
            {
                "id": "claude-sub-container",
                "engine": "claude",
                "runtime": "docker-web",
                "roles": ["bootstrap", "explore", "review"],
                "enabled": True,
            },
            {
                "id": "codex-sub-container",
                "engine": "codex",
                "runtime": "docker-web",
                "roles": ["bootstrap", "explore", "review"],
                "enabled": True,
            },
        ],
    )

    assert swarm.engines == ["claude-sub-container", "codex-sub-container"]
    assert swarm._healthy_role_candidates(["claude", "codex"], role="bootstrap") == [
        "claude-sub-container",
        "codex-sub-container",
    ]
    assert swarm._pick_engine([], ["claude", "codex"], role="bootstrap") == "claude-sub-container"
    assert swarm._pick_engine(["claude-sub-container"], ["claude", "codex"], role="bootstrap") == "codex-sub-container"


@pytest.mark.asyncio
async def test_worker_cmd_spawn_base_engine_resolves_to_configured_profile(
    tmp_path, monkeypatch,
):
    ch = Challenge(
        id="profile-worker-cmd-base",
        name="profile-worker-cmd-base",
        category="misc",
        description="profile worker cmd base",
        flag_format="flag{...}",
    )
    queue: asyncio.Queue = asyncio.Queue()
    swarm = Swarm(
        ch, [], llm=None, sandbox=None,
        worker_root=tmp_path / "run" / "workspace" / "workers",
        worker_profiles=[
            {
                "id": "claude-sub-container",
                "engine": "claude",
                "runtime": "docker-web",
                "roles": ["bootstrap", "explore", "review"],
                "enabled": True,
            }
        ],
        worker_cmds=queue,
    )
    spawned: list[str] = []
    emitted: list[tuple[str, dict]] = []

    class FakeWorker:
        solver_id = "cli-claude"

        async def run(self):
            await asyncio.sleep(3600)

    def fake_make(engine, **kwargs):
        spawned.append(engine)
        return FakeWorker()

    async def emit_bb(kind, **fields):
        emitted.append((kind, fields))

    monkeypatch.setattr(swarm, "_make_cli_worker", fake_make)
    await queue.put({"action": "spawn", "engine": "claude"})
    tasks: dict[asyncio.Task, str] = {}
    task_solvers: dict[asyncio.Task, FakeWorker] = {}

    try:
        await swarm._apply_worker_cmds(
            tasks=tasks,
            task_solvers=task_solvers,
            healthy=["claude"],
            running_engines_fn=lambda: [],
            emit_bb=emit_bb,
        )
        assert spawned == ["claude-sub-container"]
        assert list(tasks.values()) == ["claude-sub-container"]
        assert emitted[-1][0] == "worker_spawned"
    finally:
        for task in tasks:
            task.cancel()
        if tasks:
            await asyncio.gather(*tasks.keys(), return_exceptions=True)


def test_ordinary_profile_capacity_is_isolated_from_review_capacity(tmp_path):
    ch = Challenge(
        id="profile-ordinary-capacity",
        name="profile-ordinary-capacity",
        category="misc",
        description="profile capacity",
        flag_format="flag{...}",
    )
    swarm = Swarm(
        ch, [], llm=None, sandbox=None,
        worker_root=tmp_path / "run" / "workspace" / "workers",
        worker_profiles=[
            {
                "id": "claude-main",
                "engine": "claude",
                "runtime": "local",
                "credential_account": "claude-main",
                "auth": "subscription",
                "roles": ["bootstrap", "explore", "review"],
                "race": False,
                "max_running": 1,
                "max_review_running": 2,
                "enabled": True,
            }
        ],
        stage_policy={"coordinator": {"review": {"enabled": True, "max_concurrent": 1}}},
    )

    ordinary_profile = swarm._profile_for_engine("claude", role="explore")
    assert ordinary_profile is not None
    swarm._claim_worker_account(
        "cli-claude-explore", "claude", ordinary_profile, role="explore")

    assert swarm._profile_for_engine("claude", role="explore", advance=False) is None
    first_review = swarm._profile_for_engine("claude", role="review", advance=False)
    assert first_review is not None
    swarm._claim_worker_account(
        "cli-claude-review-1", "claude", first_review, role="review")
    second_review = swarm._profile_for_engine("claude", role="review", advance=False)
    assert second_review is not None
    swarm._claim_worker_account(
        "cli-claude-review-2", "claude", second_review, role="review")
    assert swarm._profile_for_engine("claude", role="review", advance=False) is None


def test_swarm_container_failure_hard_fails_by_default(tmp_path, monkeypatch):
    """New contract (round-10 approval): a container backend that fails on a
    bare host must HARD-FAIL by default — never silently fall back to
    host-native workers."""
    ch = Challenge(id="runtime-degraded", name="runtime-degraded", category="misc",
                   description="", flag_format="flag{...}")

    def boom(*args, **kwargs):
        raise RuntimeError("docker unavailable")

    import muteki.solver.container_exec as ce
    monkeypatch.setattr(ce, "ensure_container", boom)
    monkeypatch.delenv("MUTEKI_ALLOW_CONTAINER_LOCAL_FALLBACK", raising=False)

    swarm = Swarm(
        ch, [], llm=None, sandbox=None, bus=None,
        worker_root=tmp_path / "run" / "workspace" / "workers",
        worker_backend="container",
    )
    import pytest as _pytest

    with _pytest.raises(RuntimeError, match="refusing to silently run"):
        swarm._container_for_engine("claude")
    # the failure was still recorded loudly before the raise
    assert swarm._runtime_degraded
    assert "docker unavailable" in swarm._runtime_degraded[0]["reason"]


def test_swarm_container_failure_falls_back_only_with_opt_in(tmp_path, monkeypatch):
    """The legacy degraded+fallback path survives ONLY under the explicit
    operator opt-in env."""
    ch = Challenge(id="runtime-degraded", name="runtime-degraded", category="misc",
                   description="", flag_format="flag{...}")
    events = []

    class FakeBus:
        async def emit(self, ev):
            events.append(ev)

    def boom(*args, **kwargs):
        raise RuntimeError("docker unavailable")

    import muteki.solver.container_exec as ce
    monkeypatch.setattr(ce, "ensure_container", boom)
    monkeypatch.setenv("MUTEKI_ALLOW_CONTAINER_LOCAL_FALLBACK", "1")

    swarm = Swarm(
        ch, [], llm=None, sandbox=None, bus=FakeBus(),
        worker_root=tmp_path / "run" / "workspace" / "workers",
        worker_backend="container",
    )

    async def go():
        assert swarm._container_for_engine("claude") is None
        await asyncio.sleep(0)

    asyncio.run(go())
    assert swarm._backend_for_engine("claude") == "local"
    assert swarm._runtime_degraded[0]["status"] == "degraded"
    assert events[0].payload["kind"] == "runtime_degraded"
    assert "docker unavailable" in events[0].payload["reason"]


# ── detect_system_login (DESIGN §2.3 補強B) ──────────────────────────────────

def test_detect_system_login_claude_uses_keychain_not_file(monkeypatch):
    """claude login lives in the macOS Keychain, NOT a file. Detection must go
    through _claude_oauth (keychain+file) — a file-only check would report a
    logged-in mac as absent (reviewer P2). With NO ~/.claude file but a keychain
    token present, this must be 'present'."""
    from muteki.solver import credential_accounts as ca
    import muteki.solver.cli_driver as cli_driver

    # no env token → forces the _claude_oauth probe path
    monkeypatch.delenv("CLAUDE_CODE_OAUTH_TOKEN", raising=False)
    monkeypatch.delenv("ANTHROPIC_API_KEY", raising=False)
    # keychain has a token (no file involved)
    monkeypatch.setattr(cli_driver, "_claude_oauth", lambda: ("kc-token", 0))
    assert ca.detect_system_login("claude", env={}) == "present"

    # keychain empty AND no file → absent
    monkeypatch.setattr(cli_driver, "_claude_oauth", lambda: None)
    assert ca.detect_system_login("claude", env={}) == "absent"


def test_detect_system_login_claude_env_token_wins(monkeypatch):
    from muteki.solver import credential_accounts as ca
    assert ca.detect_system_login(
        "claude", env={"CLAUDE_CODE_OAUTH_TOKEN": "x"}) == "present"


def test_detect_system_login_codex_checks_auth_json(monkeypatch, tmp_path):
    from muteki.solver import credential_accounts as ca
    codex_home = tmp_path / "codex"
    codex_home.mkdir()
    assert ca.detect_system_login(
        "codex", env={"CODEX_HOME": str(codex_home)}) == "absent"
    (codex_home / "auth.json").write_text("{}")
    assert ca.detect_system_login(
        "codex", env={"CODEX_HOME": str(codex_home)}) == "present"
    # env key also counts as present
    assert ca.detect_system_login(
        "codex", env={"OPENAI_API_KEY": "sk-x"}) == "present"


def test_detect_system_login_cursor_uses_session_probe(monkeypatch):
    from muteki.solver import credential_accounts as ca
    import muteki.solver.cli_driver as cli_driver
    monkeypatch.setattr(cli_driver, "_cursor_session_cookie", lambda: "WorkosCursorSessionToken=u%3A%3At")
    assert ca.detect_system_login("cursor", env={}) == "present"
    monkeypatch.setattr(cli_driver, "_cursor_session_cookie", lambda: None)
    assert ca.detect_system_login("cursor", env={}) == "absent"
    assert ca.detect_system_login("cursor", env={"CURSOR_API_KEY": "k"}) == "present"


def test_detect_system_login_never_raises_on_probe_failure(monkeypatch):
    from muteki.solver import credential_accounts as ca
    import muteki.solver.cli_driver as cli_driver

    def _boom():
        raise RuntimeError("keychain exploded")

    monkeypatch.delenv("CLAUDE_CODE_OAUTH_TOKEN", raising=False)
    monkeypatch.setattr(cli_driver, "_claude_oauth", _boom)
    assert ca.detect_system_login("claude", env={}) == "unknown"
    assert ca.detect_system_login("bogus-engine", env={}) == "unknown"


# ── import-from-host codex auth (one-click re-auth after `codex login`) ───────

def test_import_host_codex_auth_reads_host_file(tmp_path, monkeypatch):
    """import_host_codex_auth reads the HOST ~/.codex/auth.json and upserts it into
    the account store (the fix for: codex login refreshes the host file but the
    container mounts the account COPY)."""
    fake_home = tmp_path / "home"
    (fake_home / ".codex").mkdir(parents=True)
    (fake_home / ".codex" / "auth.json").write_text(
        '{"tokens": {"access_token": "fresh-xyz"}, "last_refresh": "2026-06-26T14:11:00Z"}'
    )
    monkeypatch.setattr(Path, "home", classmethod(lambda cls: fake_home))

    store = CredentialAccountStore(account_store_root(tmp_path))
    acct = store.import_host_codex_auth("codex-main")
    assert acct["account_id"] == "codex-main"
    assert acct["present"] is True
    # the account-store copy now holds the fresh host content
    written = (account_store_root(tmp_path) / "codex-main" / "codex-home" / "auth.json").read_text()
    assert "fresh-xyz" in written


def test_import_host_codex_auth_missing_host_file_raises(tmp_path, monkeypatch):
    fake_home = tmp_path / "home"
    fake_home.mkdir()
    monkeypatch.setattr(Path, "home", classmethod(lambda cls: fake_home))
    store = CredentialAccountStore(account_store_root(tmp_path))
    with pytest.raises(ValueError, match="not found"):
        store.import_host_codex_auth("codex-main")


# ── pi / omp accounts (custom OpenAI-compatible provider via agent config) ────

def test_pi_runtime_env_writes_models_json_and_provider_env(tmp_path, monkeypatch):
    root = tmp_path / "_secrets" / "accounts"
    acct = root / "pi-main"
    acct.mkdir(parents=True)
    (acct / "API_KEY").write_text("fake-pi-key\n")
    (acct / "BASE_URL").write_text("https://ollama.example.com/v1\n")
    (acct / "MODEL").write_text("deepseek-v4-flash:0731-cloud\n")
    monkeypatch.delenv("OPENAI_API_KEY", raising=False)

    resolved = runtime_env_for_engine("pi", account_root=root, container=False)

    cfg = json.loads((acct / "pi-agent" / "models.json").read_text())
    prov = cfg["providers"]["muteki"]
    assert prov["baseUrl"] == "https://ollama.example.com/v1"
    assert prov["api"] == "openai-completions"
    assert prov["apiKey"] == "fake-pi-key"
    assert prov["models"] == [{
        "id": "deepseek-v4-flash:0731-cloud",
        "contextWindow": 128000,
        "maxTokens": 8192,
    }]
    # 0600 like every other account secret file
    assert (os.stat(acct / "pi-agent" / "models.json").st_mode & 0o777) == 0o600
    assert resolved.env["PI_CODING_AGENT_DIR"] == str((acct / "pi-agent").resolve())
    assert resolved.env["MUTEKI_PI_PROVIDER"] == "muteki"
    assert resolved.env["MUTEKI_PI_MODEL"] == "deepseek-v4-flash:0731-cloud"
    # harmless native fallback, value inline in local mode
    assert resolved.env["OPENAI_API_KEY"] == "fake-pi-key"


def test_pi_runtime_env_materializes_profile_model_in_private_state(tmp_path):
    root = tmp_path / "_secrets" / "accounts"
    acct = root / "pi-main"
    acct.mkdir(parents=True)
    (acct / "API_KEY").write_text("fake-pi-key\n")
    (acct / "BASE_URL").write_text("https://ollama.example.com/v1\n")
    (acct / "MODEL").write_text("legacy-model\n")
    private_state = tmp_path / "run-state" / "pi-agent"

    resolved = runtime_env_for_engine(
        "pi",
        account_root=root,
        account_id="pi-main",
        container=False,
        env={},
        agent_state_dir=private_state,
        model="profile-model",
    )

    assert resolved.env["PI_CODING_AGENT_DIR"] == str(private_state.resolve())
    assert resolved.env["MUTEKI_PI_MODEL"] == "profile-model"
    cfg = json.loads((private_state / "models.json").read_text())
    assert cfg["providers"]["muteki"]["models"][0]["id"] == "profile-model"
    assert not (acct / "pi-agent").exists()


def test_pi_runtime_env_container_maps_agent_dir_into_mount(tmp_path, monkeypatch):
    root = tmp_path / "_secrets" / "accounts"
    acct = root / "pi-main"
    acct.mkdir(parents=True)
    (acct / "API_KEY").write_text("fake-pi-key\n")
    (acct / "BASE_URL").write_text("https://ollama.example.com/v1\n")
    monkeypatch.delenv("OPENAI_API_KEY", raising=False)

    resolved = runtime_env_for_engine("pi", account_root=root, container=True)

    # container path mapping mirrors CODEX_HOME: env points at the mount, the
    # models.json is still written HOST-side (the projection carries it in).
    assert resolved.env["PI_CODING_AGENT_DIR"] == (
        f"{CONTAINER_ACCOUNTS_ROOT}/pi-main/pi-agent")
    assert (acct / "pi-agent" / "models.json").exists()
    # no MODEL file → no MUTEKI_PI_MODEL, and the config falls back to "default"
    assert "MUTEKI_PI_MODEL" not in resolved.env
    cfg = json.loads((acct / "pi-agent" / "models.json").read_text())
    assert cfg["providers"]["muteki"]["models"][0]["id"] == "default"
    # secret goes through the _FILE indirection, never `docker exec -e`
    assert resolved.env["OPENAI_API_KEY_FILE"] == (
        f"{CONTAINER_ACCOUNTS_ROOT}/pi-main/API_KEY")
    assert "OPENAI_API_KEY" not in resolved.env


def test_pi_runtime_env_api_key_only_sets_openai_fallback(tmp_path, monkeypatch):
    root = tmp_path / "_secrets" / "accounts"
    acct = root / "pi-main"
    acct.mkdir(parents=True)
    (acct / "API_KEY").write_text("fake-pi-key\n")
    monkeypatch.delenv("OPENAI_API_KEY", raising=False)

    resolved = runtime_env_for_engine("pi", account_root=root, container=False)

    # no BASE_URL → no provider config is generated at all
    assert resolved.env == {"OPENAI_API_KEY": "fake-pi-key"}
    assert not (acct / "pi-agent").exists()


def test_omp_runtime_env_writes_models_yml(tmp_path, monkeypatch):
    root = tmp_path / "_secrets" / "accounts"
    acct = root / "omp-main"
    acct.mkdir(parents=True)
    (acct / "API_KEY").write_text("fake-omp-key\n")
    (acct / "BASE_URL").write_text("https://ollama.example.com/v1\n")
    (acct / "MODEL").write_text("deepseek-v4-flash:0731-cloud\n")
    monkeypatch.delenv("OPENAI_API_KEY", raising=False)

    resolved = runtime_env_for_engine("omp", account_root=root, container=False)

    text = (acct / "omp-agent" / "models.yml").read_text()
    assert "providers:\n  muteki:\n" in text
    assert 'baseUrl: "https://ollama.example.com/v1"' in text
    assert "api: openai-completions" in text
    assert 'apiKey: "fake-omp-key"' in text
    assert '- id: "deepseek-v4-flash:0731-cloud"' in text
    assert "contextWindow: 128000" in text and "maxTokens: 8192" in text
    assert (os.stat(acct / "omp-agent" / "models.yml").st_mode & 0o777) == 0o600
    assert resolved.env["PI_CODING_AGENT_DIR"] == str((acct / "omp-agent").resolve())
    assert resolved.env["MUTEKI_OMP_PROVIDER"] == "muteki"
    assert resolved.env["MUTEKI_OMP_MODEL"] == "deepseek-v4-flash:0731-cloud"
    assert resolved.env["OPENAI_API_KEY"] == "fake-omp-key"
    # omp honors OPENAI_BASE_URL natively — set alongside the provider config
    assert resolved.env["OPENAI_BASE_URL"] == "https://ollama.example.com/v1"


def test_upsert_secret_accepts_pi_and_omp(tmp_path):
    store = CredentialAccountStore(account_store_root(tmp_path))
    pi = store.upsert_secret(
        account_id="pi-main", engine="pi",
        secret="fake-pi-key", base_url="https://ollama.example.com/v1")
    assert pi["engine"] == "pi" and pi["present"] is True
    root = account_store_root(tmp_path)
    assert (root / "pi-main" / "API_KEY").read_text().strip() == "fake-pi-key"
    assert (root / "pi-main" / "BASE_URL").read_text().strip() == (
        "https://ollama.example.com/v1")
    # a pi/omp account self-marks its ENGINE so inspect() binds it to the engine
    assert (root / "pi-main" / "ENGINE").read_text().strip() == "pi"

    omp = store.upsert_secret(account_id="omp-main", engine="omp", secret="fake-omp-key")
    assert omp["engine"] == "omp" and omp["present"] is True
    assert (root / "omp-main" / "ENGINE").read_text().strip() == "omp"


def test_pi_agent_dir_is_projected_writable_like_codex_home(tmp_path):
    from muteki.solver.credential_accounts import (
        _WRITABLE_STATE_DIRS, project_account_root)
    assert "pi-agent" in _WRITABLE_STATE_DIRS
    assert "omp-agent" in _WRITABLE_STATE_DIRS

    root = tmp_path / "_secrets" / "accounts"
    agent_dir = root / "pi-main" / "pi-agent"
    agent_dir.mkdir(parents=True)
    (agent_dir / "models.json").write_text("{}\n")

    projection = tmp_path / "projection"
    project_account_root(root, projection)
    projected = projection / "pi-main" / "pi-agent"
    assert (projected / "models.json").exists()
    # writable state: dir 0777 + files 0666 (the CLI rewrites state in place)
    assert (os.stat(projected).st_mode & 0o777) == 0o777
    assert (os.stat(projected / "models.json").st_mode & 0o777) == 0o666


def test_reupsert_clears_generated_pi_omp_provider_configs(tmp_path):
    store = CredentialAccountStore(account_store_root(tmp_path))
    root = account_store_root(tmp_path)
    store.upsert_secret(
        account_id="pi-main", engine="pi",
        secret="fake-pi-key", base_url="https://ollama.example.com/v1")
    runtime_env_for_engine("pi", account_root=root, container=False)
    assert (root / "pi-main" / "pi-agent" / "models.json").exists()

    # a fresh upsert wipes the generated config (it regenerates on next resolve)
    store.upsert_secret(account_id="pi-main", engine="pi", secret="fake-pi-key-2")
    assert not (root / "pi-main" / "pi-agent").exists()


def test_detect_system_login_pi_and_omp(tmp_path, monkeypatch):
    from muteki.solver import credential_accounts as ca
    fake_home = tmp_path / "home"
    fake_home.mkdir()
    monkeypatch.setattr(Path, "home", classmethod(lambda cls: fake_home))

    assert ca.detect_system_login("pi", env={}) == "absent"
    assert ca.detect_system_login("omp", env={}) == "absent"
    pi_agent = fake_home / ".pi" / "agent"
    pi_agent.mkdir(parents=True)
    (pi_agent / "auth.json").write_text("{}")
    assert ca.detect_system_login("pi", env={}) == "present"
    (fake_home / ".omp" / "agent").mkdir(parents=True)
    assert ca.detect_system_login("omp", env={}) == "present"
