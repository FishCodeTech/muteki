from __future__ import annotations

import os
import subprocess

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
