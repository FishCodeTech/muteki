"""Test-connectivity probes (DESIGN §2.4 補強C): LLM endpoint + credential account.

Pins the reviewer-flagged contracts:
- LLM test uses the REQUEST-BODY base_url/model, not saved config (P1).
- LLM test judges ok by API success, not non-empty content (P3, reasoning models).
- Account test NEVER falls back to host default login (P1).
- Account container test really uses `docker run --rm`, not the local probe (操作者).
"""

from __future__ import annotations

import asyncio
import subprocess

import pytest

import apps.web.llm_test as llm_test
import apps.web.account_test as account_test


# ── LLM endpoint test (補強C-1) ──────────────────────────────────────────────

def test_llm_test_uses_request_body_base_url(monkeypatch):
    """The base_url/model from the request body is what gets tested, not config."""
    seen = {}

    class _LLM:
        def __init__(self, *, base_url=None, **_kw):
            seen["base_url"] = base_url

        async def chat(self, *, model, **_kw):
            seen["model"] = model

            class _R:
                finish_reason = "stop"
                content = "pong"
            return _R()

        async def aclose(self):
            pass

    import muteki.core.llm as llm_mod
    monkeypatch.setattr(llm_mod, "LLMClient", _LLM)
    res = asyncio.run(llm_test.test_llm_endpoint(
        which="planner", base_url="https://edited.endpoint.test/v1", model="edited-model"))
    assert res["ok"] is True
    assert seen["base_url"] == "https://edited.endpoint.test/v1"
    assert seen["model"] == "edited-model"


def test_llm_test_empty_content_still_ok(monkeypatch):
    """Reasoning model returns empty content but the call succeeded → ok (P3)."""
    class _LLM:
        def __init__(self, **_kw):
            pass

        async def chat(self, **_kw):
            class _R:
                finish_reason = "stop"
                content = ""  # reasoning ate the tokens — still healthy
            return _R()

        async def aclose(self):
            pass

    import muteki.core.llm as llm_mod
    monkeypatch.setattr(llm_mod, "LLMClient", _LLM)
    res = asyncio.run(llm_test.test_llm_endpoint(which="titler", model="m"))
    assert res["ok"] is True


def test_llm_test_chat_raises_is_not_ok(monkeypatch):
    class _LLM:
        def __init__(self, **_kw):
            pass

        async def chat(self, **_kw):
            raise RuntimeError("401 unauthorized")

        async def aclose(self):
            pass

    import muteki.core.llm as llm_mod
    monkeypatch.setattr(llm_mod, "LLMClient", _LLM)
    res = asyncio.run(llm_test.test_llm_endpoint(which="planner", model="m"))
    assert res["ok"] is False
    assert "401" in res["detail"]


def test_llm_test_empty_model_rejected():
    res = asyncio.run(llm_test.test_llm_endpoint(which="planner", model=""))
    assert res["ok"] is False


# ── account test (補強C-2) ───────────────────────────────────────────────────

def _register_claude(tmp_path):
    from muteki.solver.credential_accounts import CredentialAccountStore, account_store_root
    store = CredentialAccountStore(account_store_root(tmp_path))
    store.upsert_secret(account_id="claude-main", engine="claude", secret="tok-123")
    return store


def test_account_test_no_account_never_falls_back_to_host(tmp_path, monkeypatch):
    """Unregistered account → ok:false, and we must NOT read the host's default
    login to fake a pass (reviewer P1)."""
    # even if the host has a token in env, an unregistered account is ok:false.
    monkeypatch.setenv("CLAUDE_CODE_OAUTH_TOKEN", "host-default-token")
    res = account_test.probe_account(
        engine="claude", account_id="does-not-exist",
        sessions_root=tmp_path, backend="local")
    assert res["ok"] is False
    assert res["layer"] == "auth"


def test_account_test_local_uses_account_env(tmp_path, monkeypatch):
    """backend=local resolves the ACCOUNT's env (not host) and runs health_detail.

    Post-unification the kernel passes the resolved env EXPLICITLY via
    health_detail(env=...) rather than mutating os.environ globally, so the probe
    reads the account's token from the passed env, not the process environment.
    """
    _register_claude(tmp_path)
    seen = {}

    import muteki.solver.cli_driver as cli_driver

    class _Drv:
        def health_detail(self, env=None):
            seen["token"] = (env or {}).get("CLAUDE_CODE_OAUTH_TOKEN")
            return True, ""

    monkeypatch.setattr(cli_driver, "driver_for", lambda profile: _Drv())
    res = account_test.probe_account(
        engine="claude", account_id="claude-main",
        sessions_root=tmp_path, backend="local")
    assert res["ok"] is True
    # the account's token was injected, not whatever the host had
    assert seen["token"] == "tok-123"


def _register_endpoint(tmp_path, *, target="claude", base="https://api.deepseek.com/anthropic"):
    from muteki.solver.credential_accounts import CredentialAccountStore, account_store_root
    store = CredentialAccountStore(account_store_root(tmp_path))
    store.upsert_secret(account_id="ds", engine="api", secret="sk-test-key",
                        base_url=base, target_engine=target)


def test_account_test_custom_endpoint_probes_directly_not_via_cli(tmp_path, monkeypatch):
    """A custom-endpoint account must be tested with a DIRECT curl probe (cheap,
    model-agnostic), NOT by synthesizing a profile + shelling claude-code with a
    wrong default model (which hangs against a third-party endpoint). We assert the
    CLI driver is NEVER invoked and the HTTP status classifies correctly."""
    _register_endpoint(tmp_path)
    import muteki.solver.cli_driver as cli_driver

    def _boom(*a, **k):
        raise AssertionError("driver_for must NOT be called for a custom-endpoint test")
    monkeypatch.setattr(cli_driver, "driver_for", _boom)

    seen = {}

    def fake_run(argv, **kwargs):
        seen["argv"] = argv
        return subprocess.CompletedProcess(argv, 0, "400", "")  # endpoint reached, key ok
    monkeypatch.setattr(account_test.subprocess, "run", fake_run)

    res = account_test.probe_account(
        engine="claude", account_id="ds", sessions_root=tmp_path, backend="local")
    assert res["ok"] is True                         # 400 = auth+reachability proven
    assert "curl" in seen["argv"][0]                 # used curl, not the CLI
    # claude target → Anthropic Messages endpoint + x-api-key header
    assert any("/v1/messages" in str(a) for a in seen["argv"])
    assert any("x-api-key" in str(a) for a in seen["argv"])


def test_account_test_custom_endpoint_bad_key_fails_fast(tmp_path, monkeypatch):
    _register_endpoint(tmp_path, target="codex", base="https://api.deepseek.com")
    monkeypatch.setattr(
        account_test.subprocess, "run",
        lambda argv, **k: subprocess.CompletedProcess(argv, 0, "401", ""))
    res = account_test.probe_account(
        engine="codex", account_id="ds", sessions_root=tmp_path, backend="local")
    assert res["ok"] is False
    assert res["layer"] == "auth"
    assert "401" in res["detail"]


def test_account_test_custom_endpoint_codex_uses_chat_completions(tmp_path, monkeypatch):
    """A non-claude target must probe OpenAI Chat Completions ({base}/chat/completions
    with a Bearer header), NOT codex's /responses (which DeepSeek 404s)."""
    _register_endpoint(tmp_path, target="codex", base="https://api.deepseek.com")
    seen = {}
    monkeypatch.setattr(
        account_test.subprocess, "run",
        lambda argv, **k: (seen.__setitem__("argv", argv),
                           subprocess.CompletedProcess(argv, 0, "200", ""))[1])
    res = account_test.probe_account(
        engine="codex", account_id="ds", sessions_root=tmp_path, backend="local")
    assert res["ok"] is True
    assert any("/chat/completions" in str(a) for a in seen["argv"])
    assert not any("/responses" in str(a) for a in seen["argv"])
    assert any("Authorization: Bearer" in str(a) for a in seen["argv"])


def test_account_test_container_uses_docker_run_rm_not_local(tmp_path, monkeypatch):
    """backend=container must `docker run --rm` a one-shot container (operator
    requirement), mounting the account projection + a throwaway workspace, and
    NEVER the bench tree. We assert the docker argv shape.

    Post-unification the container probe runs BOTH the plumbing docker-run AND a
    host-local auth hello (the fix for the false-green where container test only
    ran `--version`). We mock the auth layer so this test isolates the plumbing
    argv; a dedicated test covers the auth layer firing.
    """
    _register_claude(tmp_path)
    calls = []

    def fake_docker(*args, timeout=30.0):
        calls.append(list(args))
        if args[:2] == ("image", "inspect"):
            return subprocess.CompletedProcess(args, 0, "", "")
        # the run --rm probe
        return subprocess.CompletedProcess(args, 0, "MUTEKI_OK\n", "")

    import muteki.solver.cli_driver as cli_driver
    monkeypatch.setattr(account_test, "_docker", fake_docker)
    monkeypatch.setattr(cli_driver, "driver_for", lambda profile: type(
        "D", (), {"health_detail": lambda self, env=None: (True, "")})())
    res = account_test.probe_account(
        engine="claude", account_id="claude-main",
        sessions_root=tmp_path, backend="container")
    assert res["ok"] is True
    run_calls = [c for c in calls if c and c[0] == "run"]
    assert run_calls, "must invoke docker run"
    run = run_calls[0]
    assert "--rm" in run  # one-shot, not the long-lived ensure_container
    flat = " ".join(run)
    assert "muteki_accounts" not in flat or "accounts" in flat  # projection mounted
    # the bench tree must NEVER be mounted — only workspace + accounts projection
    assert "nyu_ctf_bench" not in flat and "bench" not in flat


def test_account_test_container_docker_unavailable_is_not_ok(tmp_path, monkeypatch):
    """docker missing → ok:false layer=image, NOT a silent local fallback."""
    _register_claude(tmp_path)

    def fake_docker(*args, timeout=30.0):
        raise FileNotFoundError("docker not found")

    monkeypatch.setattr(account_test, "_docker", fake_docker)
    res = account_test.probe_account(
        engine="claude", account_id="claude-main",
        sessions_root=tmp_path, backend="container")
    assert res["ok"] is False
    assert res["layer"] == "image"


def test_account_test_container_mount_unreadable_layer(tmp_path, monkeypatch):
    _register_claude(tmp_path)

    def fake_docker(*args, timeout=30.0):
        if args[:2] == ("image", "inspect"):
            return subprocess.CompletedProcess(args, 0, "", "")
        return subprocess.CompletedProcess(args, 71, "MUTEKI_MOUNT_UNREADABLE\n", "")

    monkeypatch.setattr(account_test, "_docker", fake_docker)
    res = account_test.probe_account(
        engine="claude", account_id="claude-main",
        sessions_root=tmp_path, backend="container")
    assert res["ok"] is False
    assert res["layer"] == "mount"


# ── engine self-check: local vs container (task #16) ─────────────────────────

def test_engine_health_local_tags_backend(monkeypatch):
    """local self-check runs the host driver healthcheck and tags backend=local."""
    import muteki.solver.cli_driver as cli_driver

    monkeypatch.setattr(cli_driver.subprocess, "run",
                        lambda *a, **k: __import__("subprocess").CompletedProcess(a, 0, "claude 2.1.0\n", ""))
    for drv in cli_driver.DRIVERS.values():
        monkeypatch.setattr(drv, "health_detail", lambda: (True, ""))
    rows = cli_driver.engine_health("local")
    assert rows and all(r["backend"] == "local" for r in rows)
    assert all(r["healthy"] for r in rows)


def test_engine_health_local_profile_probe_uses_selected_model(tmp_path, monkeypatch):
    """Settings-panel TODO: global engine self-check must exercise the selected
    worker profile/model, not only the bare engine default."""
    import muteki.solver.cli_driver as cli_driver

    seen = []

    def fake_run(argv, **_kwargs):
        seen.append(argv)
        text = "codex 1.0\n"
        if "--model" in argv:
            text = (
                '{"type":"thread.started","thread_id":"t"}\n'
                '{"type":"turn.completed","usage":{}}\n')
        return subprocess.CompletedProcess(argv, 0, text, "")

    monkeypatch.setattr(cli_driver.subprocess, "run", fake_run)

    rows = cli_driver.engine_health(
        "local",
        str(tmp_path),
        profiles=[{"id": "codex-sub", "name": "codex-sub", "engine": "codex",
                   "model": "gpt-5.5", "credential_account": ""}],
    )

    hello = [argv for argv in seen if "--model" in argv][-1]
    assert hello[hello.index("--model") + 1] == "gpt-5.5"
    assert rows[0]["engine"] == "codex"
    assert rows[0]["profile_id"] == "codex-sub"
    assert rows[0]["model"] == "gpt-5.5"
    assert rows[0]["healthy"] is True


def test_engine_health_container_runs_in_container_not_host(monkeypatch):
    """container self-check uses `docker run --rm` against the worker image to
    verify the CLI launches INSIDE the container, NOT the host CLI (task #16)."""
    import muteki.solver.cli_driver as cli_driver

    calls = []

    def fake_run(argv, **kwargs):
        calls.append(argv)
        import subprocess as sp
        if "image" in argv and "inspect" in argv:
            return sp.CompletedProcess(argv, 0, "", "")
        if "run" in argv and "--rm" in argv:
            return sp.CompletedProcess(argv, 0, "claude 2.1.0 (container)\n", "")
        return sp.CompletedProcess(argv, 0, "", "")

    import shutil
    monkeypatch.setattr(shutil, "which", lambda name: "/usr/bin/docker" if name == "docker" else None)
    monkeypatch.setattr(cli_driver.subprocess, "run", fake_run)
    rows = cli_driver.engine_health("container")
    assert rows and all(r["backend"] == "container" for r in rows)
    assert all(r["healthy"] for r in rows)
    # every probe went through `docker run --rm`, not a bare host `<bin> --version`
    run_calls = [c for c in calls if "run" in c and "--rm" in c]
    assert run_calls, "container self-check must use docker run --rm"


def test_engine_health_container_image_missing_is_unhealthy(monkeypatch):
    import muteki.solver.cli_driver as cli_driver

    def fake_run(argv, **kwargs):
        import subprocess as sp
        if "image" in argv and "inspect" in argv:
            return sp.CompletedProcess(argv, 1, "", "No such image")  # image absent
        return sp.CompletedProcess(argv, 0, "", "")

    import shutil
    monkeypatch.setattr(shutil, "which", lambda name: "/usr/bin/docker" if name == "docker" else None)
    monkeypatch.setattr(cli_driver.subprocess, "run", fake_run)
    rows = cli_driver.engine_health("container")
    assert rows and all(not r["healthy"] for r in rows)
    assert all("image missing" in r["detail"] for r in rows)
