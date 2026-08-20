from __future__ import annotations

import subprocess

from apps.web.worker_config import WorkerConfigStore
from apps.web.worker_models import (
    parse_grok_models,
    parse_kimi_models,
    parse_omp_models,
    parse_openai_models,
    probe_worker_model,
    worker_model_options_payload,
)
from muteki.solver.cli_driver import driver_for
from muteki.solver.identity_model import migrate_legacy_config, seat_to_legacy_profile
from muteki.solver.worker_profiles import normalize_worker_profile


def _profile(engine: str, model: str, effort: str) -> dict:
    return {
        "id": engine,
        "name": engine,
        "engine": engine,
        "transport": engine,
        "credential_account": "",
        "runtime": "local",
        "roles": ["race", "review"],
        "model": model,
        "reasoning_effort": effort,
        "enabled": True,
    }


def test_reasoning_effort_is_translated_for_every_worker_cli(monkeypatch) -> None:
    for engine in ("claude", "codex", "cursor", "pi", "omp", "kimi", "grok"):
        monkeypatch.setenv(f"MUTEKI_{engine.upper()}_BIN", f"/test/{engine}")

    cases = {
        "claude": ("claude-sonnet-4-6", "xhigh", ["--effort", "xhigh"]),
        "codex": ("gpt-5.6-sol", "max", ['model_reasoning_effort="max"']),
        "cursor": ("gpt-5.3-codex", "high", ["gpt-5.3-codex-high"]),
        "pi": ("deepseek-v4", "minimal", ["--thinking", "minimal"]),
        "omp": ("ollama/deepseek-v4", "none", ["--thinking", "off"]),
        "kimi": ("kimi-code/k3", "max", []),
        "grok": ("grok-4.6", "xhigh", ["--reasoning-effort", "xhigh"]),
    }
    for engine, (model, effort, expected) in cases.items():
        drv = driver_for(_profile(engine, model, effort))
        argv = drv.build_execute(
            "PING", None, web_access=False, kb_access=False, stream=True)
        assert "--model" in argv
        assert argv[argv.index("--model") + 1].startswith(model)
        for token in expected:
            assert token in argv

    codex_argv = driver_for(_profile("codex", "gpt-5.6-luna", "max")).build_execute(
        "PING", None, web_access=False, kb_access=False)
    assert "--ignore-user-config" in codex_argv
    assert 'web_search="disabled"' in codex_argv
    assert "code_mode" in codex_argv
    assert "browser_use" in codex_argv
    assert "plugins" in codex_argv

    kimi = driver_for(_profile("kimi", "kimi-code/k3", "max"))
    assert kimi.env_extra()["KIMI_MODEL_THINKING_EFFORT"] == "max"
    grok_argv = driver_for(_profile("grok", "grok-4.6", "high")).build_execute(
        "PING", None, web_access=False, kb_access=False)
    assert grok_argv[:3] == [
        "env",
        "GROK_CLAUDE_MCPS_ENABLED=false",
        "GROK_CURSOR_MCPS_ENABLED=false",
    ]
    assert "--no-subagents" in grok_argv
    assert "--disable-web-search" in grok_argv
    assert "--agent" in grok_argv
    assert "grok_offline_agent.md" in grok_argv[grok_argv.index("--agent") + 1]
    assert grok_argv[grok_argv.index("--deny") + 1] == "MCPTool"
    assert "--disallowed-tools" in grok_argv
    denied = grok_argv[grok_argv.index("--disallowed-tools") + 1]
    assert "search_tool" in denied
    assert "use_tool" in denied


def test_reasoning_effort_survives_profile_and_identity_round_trip() -> None:
    profile = normalize_worker_profile(_profile("kimi", "kimi-code/k3", "high"))
    assert profile is not None
    assert profile["reasoning_effort"] == "high"

    migrated = migrate_legacy_config(
        worker_profiles=[profile],
    )
    seat = migrated.seats[0].to_dict()
    assert seat["reasoning_effort"] == "high"
    legacy = seat_to_legacy_profile(
        seat, migrated.credentials[0].to_dict())
    assert legacy["reasoning_effort"] == "high"

    stage = WorkerConfigStore._clean_stage_policy({
        "coordinator": {"review": {"reasoning_effort": "xhigh"}},
    }, {})
    assert stage["coordinator"]["review"]["reasoning_effort"] == "xhigh"


def test_model_catalog_and_discovery_preserve_reasoning_capabilities() -> None:
    payload = worker_model_options_payload()
    assert {"kimi", "grok"} <= set(payload["models"])
    sol = next(m for m in payload["models"]["codex"] if m["id"] == "gpt-5.6-sol")
    assert sol["reasoning"]["levels"] == ["low", "medium", "high", "xhigh", "max"]
    assert "ultra" not in sol["reasoning"]["levels"]
    cursor_auto = next(m for m in payload["models"]["cursor"] if m["id"] == "auto")
    assert cursor_auto["reasoning"]["supported"] is False

    codex = parse_openai_models(
        '{"models":[{"slug":"gpt-x","display_name":"GPT X",'
        '"default_reasoning_level":"high","supported_reasoning_levels":'
        '[{"effort":"low"},{"effort":"max"},{"effort":"ultra"}]}]}')
    assert codex[0]["reasoning"]["levels"] == ["low", "max"]
    assert parse_kimi_models('{"models":{"kimi-code/k3":{}},"providers":{}}')[0]["id"] == "kimi-code/k3"
    assert parse_grok_models("Available models:\n  * grok-4.6 (default)\n  - grok-4.5\n")
    assert parse_omp_models(
        '{"models":[{"selector":"ollama/deepseek","name":"DeepSeek",'
        '"reasoning":true}]}')[0]["reasoning"]["supported"] is True


def test_kimi_model_probe_applies_selected_effort(monkeypatch, tmp_path) -> None:
    monkeypatch.setenv("MUTEKI_KIMI_BIN", "/test/kimi")
    captured: dict[str, object] = {}

    def fake_run(argv, **kwargs):
        captured["argv"] = list(argv)
        captured["effort"] = kwargs["env"].get("KIMI_MODEL_THINKING_EFFORT")
        return subprocess.CompletedProcess(
            argv, 0,
            stdout='{"role":"assistant","content":"OK"}\n',
            stderr="",
        )

    monkeypatch.setattr("apps.web.worker_models.subprocess.run", fake_run)
    result = probe_worker_model(
        profile=_profile("kimi", "kimi-code/k3", "max"),
        model="kimi-code/k3",
        reasoning_effort="max",
        sessions_root=tmp_path,
        backend="local",
    )

    assert result["ok"] is True
    assert captured["effort"] == "max"
    argv = captured["argv"]
    assert argv[0] == "/test/kimi"
    assert "--agent-file" in argv
    assert argv[argv.index("--output-format") + 1] == "stream-json"
    assert argv[argv.index("--model") + 1] == "kimi-code/k3"
    assert argv[-2:] == ["-p", "Reply with exactly: OK"]
