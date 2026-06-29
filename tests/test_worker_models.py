from __future__ import annotations

import os
import subprocess
from types import SimpleNamespace

from apps.web.worker_models import (
    WORKER_MODEL_OPTIONS,
    probe_worker_model,
    worker_model_options_payload,
)


def test_worker_model_options_are_static_and_custom_enabled() -> None:
    payload = worker_model_options_payload()

    assert payload["allow_custom"] is True
    assert {m["id"] for m in payload["models"]["claude"]} >= {"sonnet", "opus"}
    assert {m["id"] for m in payload["models"]["codex"]} >= {"gpt-5.5", "gpt-5.4-mini"}
    assert {m["id"] for m in payload["models"]["cursor"]} >= {"auto", "composer-2.5-fast"}
    assert payload["models"] == WORKER_MODEL_OPTIONS


def test_probe_worker_model_injects_profile_model_and_account_env(tmp_path, monkeypatch) -> None:
    root = tmp_path / "_secrets" / "accounts" / "claude-main"
    root.mkdir(parents=True)
    (root / "CLAUDE_CODE_OAUTH_TOKEN").write_text("oauth-token\n")
    seen: dict[str, object] = {}

    def fake_run(argv, **kwargs):
        seen["argv"] = argv
        seen["token"] = os.environ.get("CLAUDE_CODE_OAUTH_TOKEN")
        return subprocess.CompletedProcess(argv, 0, '{"result":"OK"}\n', "")

    monkeypatch.setattr(subprocess, "run", fake_run)

    res = probe_worker_model(
        profile={
            "id": "claude-sub",
            "name": "claude-sub",
            "engine": "claude",
            "transport": "claude_code",
            "credential_account": "claude-main",
            "runtime": "local",
        },
        model="opus",
        sessions_root=tmp_path,
        backend="local",
    )

    assert res["ok"] is True
    assert res["model"] == "opus"
    assert seen["token"] == "oauth-token"
    assert "--model" in seen["argv"]
    assert seen["argv"][seen["argv"].index("--model") + 1] == "opus"


def test_probe_worker_model_allows_local_system_login_without_registered_account(
    tmp_path, monkeypatch
) -> None:
    seen: dict[str, object] = {}

    def fake_run(argv, **kwargs):
        seen["argv"] = argv
        return subprocess.CompletedProcess(argv, 0, '{"result":"OK"}\n', "")

    monkeypatch.setattr(subprocess, "run", fake_run)

    res = probe_worker_model(
        profile={
            "id": "claude-local",
            "engine": "claude",
            "transport": "claude_code",
            "credential_account": "",
            "runtime": "local",
        },
        model="sonnet",
        sessions_root=tmp_path,
        backend="local",
    )

    assert res["ok"] is True
    assert "--model" in seen["argv"]


def test_probe_worker_model_does_not_default_local_codex_to_stale_account(
    tmp_path, monkeypatch
) -> None:
    codex_home = tmp_path / "_secrets" / "accounts" / "codex-main" / "codex-home"
    codex_home.mkdir(parents=True)
    (codex_home / "auth.json").write_text('{"stale": true}\n')
    seen: dict[str, object] = {}

    def fake_run(argv, **kwargs):
        seen["argv"] = argv
        seen["codex_home"] = os.environ.get("CODEX_HOME")
        return subprocess.CompletedProcess(
            argv,
            0,
            '{"type":"thread.started","thread_id":"t"}\n'
            '{"type":"turn.completed","usage":{}}\n',
            "",
        )

    monkeypatch.setattr(subprocess, "run", fake_run)

    res = probe_worker_model(
        profile={
            "id": "codex-local",
            "engine": "codex",
            "transport": "codex_cli",
            "credential_account": "",
            "runtime": "local",
        },
        model="gpt-5.5",
        sessions_root=tmp_path,
        backend="local",
    )

    assert res["ok"] is True
    assert seen["codex_home"] is None
    assert "--model" in seen["argv"]
    assert seen["argv"][seen["argv"].index("--model") + 1] == "gpt-5.5"


def test_probe_worker_model_runs_real_worker_container_when_web_is_containerized(
    tmp_path, monkeypatch
) -> None:
    # In a compose deploy the WEB process runs inside a container that does NOT
    # ship the engine CLIs (claude/codex/cursor) — those live only in the WORKER
    # image. A container-backend model probe must therefore run a real one-shot
    # worker container and complete the same minimal hello turn there.
    monkeypatch.setenv("MUTEKI_IN_CONTAINER", "1")
    codex_home = tmp_path / "_secrets" / "accounts" / "codex-main" / "codex-home"
    codex_home.mkdir(parents=True)
    (codex_home / "auth.json").write_text('{"ok": true}\n')
    seen: dict[str, object] = {}

    def fake_docker(*args, timeout=30):
        if args[:2] == ("image", "inspect"):
            return subprocess.CompletedProcess(["docker", *args], 0, "", "")
        if args and args[0] == "run":
            seen["run_args"] = args
            return subprocess.CompletedProcess(
                ["docker", *args],
                0,
                '{"type":"thread.started","thread_id":"t"}\n'
                '{"type":"turn.completed","usage":{}}\n',
                "",
            )
        raise AssertionError(f"unexpected docker command: {args}")

    import apps.web.worker_models as worker_models
    monkeypatch.setattr(worker_models, "_docker", fake_docker)

    res = probe_worker_model(
        profile={
            "id": "codex-seat",
            "engine": "codex",
            "transport": "codex_cli",
            # Regression: a legacy/imported Codex auth.json seat can carry
            # credential_mode=api_key while still using CODEX_HOME. That must not
            # make the probe treat it like a custom endpoint and drop --model.
            "credential_mode": "api_key",
            "credential_account": "codex-main",
            "runtime": "container",
        },
        model="gpt-5.4",
        sessions_root=tmp_path,
        backend="container",
    )

    assert res["ok"] is True
    assert res["backend"] == "container"
    assert res["engine"] == "codex"
    assert res["model"] == "gpt-5.4"
    run_args = seen["run_args"]
    assert run_args[0] == "run"
    assert "--entrypoint" in run_args and "bash" in run_args
    assert "--user" in run_args and "kali" in run_args
    assert any(str(a).startswith("type=bind") and "/run/muteki/accounts" in str(a) for a in run_args)
    assert "gpt-5.4" in " ".join(str(a) for a in run_args)


def test_probe_worker_model_container_maps_codex_home_seed(
    tmp_path, monkeypatch
) -> None:
    monkeypatch.setenv("MUTEKI_IN_CONTAINER", "1")
    codex_home = tmp_path / "_secrets" / "accounts" / "codex-main" / "codex-home"
    codex_home.mkdir(parents=True)
    (codex_home / "auth.json").write_text('{"ok": true}\n')
    seen: dict[str, object] = {}

    def fake_runtime_env_for_engine(*_args, **_kwargs):
        return SimpleNamespace(
            account_id="codex-main",
            env={
                "MUTEKI_CODEX_HOME_SEED": "/run/muteki/accounts/codex-main/codex-home",
            },
        )

    def fake_docker(*args, timeout=30):
        if args[:2] == ("image", "inspect"):
            return subprocess.CompletedProcess(["docker", *args], 0, "", "")
        if args and args[0] == "run":
            seen["run_args"] = args
            return subprocess.CompletedProcess(
                ["docker", *args],
                0,
                '{"type":"turn.completed","usage":{}}\n',
                "",
            )
        raise AssertionError(f"unexpected docker command: {args}")

    import apps.web.worker_models as worker_models
    monkeypatch.setattr(worker_models, "runtime_env_for_engine", fake_runtime_env_for_engine)
    monkeypatch.setattr(worker_models, "_docker", fake_docker)

    res = probe_worker_model(
        profile={
            "id": "codex-seat",
            "engine": "codex",
            "transport": "codex_cli",
            "credential_account": "codex-main",
            "runtime": "container",
        },
        model="gpt-5.4",
        sessions_root=tmp_path,
        backend="container",
    )

    assert res["ok"] is True
    joined = " ".join(str(a) for a in seen["run_args"])
    assert "MUTEKI_CODEX_HOME_SEED=/run/muteki/accounts/codex-main/codex-home" in joined
    assert "CODEX_HOME" in joined
    assert 'cp -R "$MUTEKI_CODEX_HOME_SEED"/. "$CODEX_HOME"/' in joined


def test_probe_worker_model_container_reports_model_rejection(
    tmp_path, monkeypatch
) -> None:
    monkeypatch.setenv("MUTEKI_IN_CONTAINER", "1")
    root = tmp_path / "_secrets" / "accounts" / "claude-main"
    root.mkdir(parents=True)
    (root / "CLAUDE_CODE_OAUTH_TOKEN").write_text("oauth-token\n")

    def fake_docker(*args, timeout=30):
        if args[:2] == ("image", "inspect"):
            return subprocess.CompletedProcess(["docker", *args], 0, "", "")
        if args and args[0] == "run":
            return subprocess.CompletedProcess(
                ["docker", *args],
                1,
                "",
                "unknown model: claude-nope\n",
            )
        raise AssertionError(f"unexpected docker command: {args}")

    import apps.web.worker_models as worker_models
    monkeypatch.setattr(worker_models, "_docker", fake_docker)

    res = probe_worker_model(
        profile={
            "id": "claude-seat",
            "engine": "claude",
            "transport": "claude_code",
            "credential_account": "claude-main",
            "runtime": "container",
        },
        model="claude-nope",
        sessions_root=tmp_path,
        backend="container",
    )

    assert res["ok"] is False
    assert res["backend"] == "container"
    assert res["layer"] == "auth"
    assert "unknown model" in res["detail"]


def test_probe_worker_model_container_claude_endpoint_uses_custom_model(
    tmp_path, monkeypatch
) -> None:
    monkeypatch.setenv("MUTEKI_IN_CONTAINER", "1")
    root = tmp_path / "_secrets" / "accounts" / "deepseek-main"
    root.mkdir(parents=True)
    (root / "API_KEY").write_text("deepseek-key\n")
    (root / "BASE_URL").write_text("https://api.deepseek.example/anthropic\n")
    seen: dict[str, object] = {}

    def fake_docker(*args, timeout=30):
        if args[:2] == ("image", "inspect"):
            return subprocess.CompletedProcess(["docker", *args], 0, "", "")
        if args and args[0] == "run":
            seen["run_args"] = args
            return subprocess.CompletedProcess(["docker", *args], 0, '{"result":"OK"}\n', "")
        raise AssertionError(f"unexpected docker command: {args}")

    import apps.web.worker_models as worker_models
    monkeypatch.setattr(worker_models, "_docker", fake_docker)

    res = probe_worker_model(
        profile={
            "id": "claude-ds",
            "engine": "claude",
            "transport": "claude_code",
            "credential_account": "deepseek-main",
            "credential_mode": "api_key",
            "base_url": "https://api.deepseek.example/anthropic",
            "runtime": "container",
        },
        model="deepseek-v4-pro",
        sessions_root=tmp_path,
        backend="container",
    )

    assert res["ok"] is True
    joined = " ".join(str(a) for a in seen["run_args"])
    assert "--model deepseek-v4-pro" in joined
    assert "ANTHROPIC_BASE_URL=https://api.deepseek.example/anthropic" in joined


def test_probe_worker_model_still_probes_host_for_local_backend_in_container(
    tmp_path, monkeypatch
) -> None:
    # The defer is gated on backend == "container". An explicit local-backend
    # probe (operator chose host semantics) must still shell the host CLI even
    # if MUTEKI_IN_CONTAINER happens to be set — the guard must not over-reach.
    monkeypatch.setenv("MUTEKI_IN_CONTAINER", "1")
    root = tmp_path / "_secrets" / "accounts" / "claude-main"
    root.mkdir(parents=True)
    (root / "CLAUDE_CODE_OAUTH_TOKEN").write_text("oauth-token\n")
    ran: dict[str, object] = {}

    def fake_run(argv, **kwargs):
        ran["argv"] = argv
        return subprocess.CompletedProcess(argv, 0, '{"result":"OK"}\n', "")

    monkeypatch.setattr(subprocess, "run", fake_run)

    res = probe_worker_model(
        profile={
            "id": "claude-sub",
            "engine": "claude",
            "transport": "claude_code",
            "credential_account": "claude-main",
            "runtime": "local",
        },
        model="opus",
        sessions_root=tmp_path,
        backend="local",
    )

    assert res["ok"] is True
    assert ran.get("argv") is not None  # host probe DID run
