"""CLI worker executor — driver argv construction, output parsing, flag extraction,
external-USD cost accounting. Pure/unit (no real CLI subprocess, no API key)."""

from __future__ import annotations

import asyncio
import hashlib
import json
import os
import subprocess
import sys
import threading
import time
from pathlib import Path

import pytest

from muteki.core.cost import Budget, CostController
from muteki.core.events import EventType
from muteki.models.solve_graph import Challenge
from muteki.solver import cli_solver
from muteki.solver.cli_driver import (
    ClaudeCodeDriver, CliResult, CodexDriver, CursorDriver, DRIVERS,
    SecurePromptUnsupported, StreamStep,
    OhMyPiDriver, PiDriver,
    apply_runtime_argv, driver_for, get_driver,
    _descendant_pids, _kill_proc_tree, _probe_health_with_creds,
)
from muteki.solver.cli_solver import CliSolver
from muteki.solver.container_exec import CONTAINER_WORKSPACE, ContainerHandle


# ── driver argv ───────────────────────────────────────────────────────────────


def test_worker_env_maps_blackboard_db_into_container_workspace(tmp_path):
    workspace = tmp_path / "workspace"
    db = workspace / "graph" / "shared_graph.db"
    db.parent.mkdir(parents=True)
    db.write_text("")

    class _Graph:
        db_path = db

    ch = Challenge(
        id="env-map",
        name="env-map",
        category="misc",
        description="path mapping",
        flag_format="flag{...}",
    )
    handle = ContainerHandle(
        run_id="env-map",
        host_workspace=str(workspace),
        container="muteki-run-env-map",
    )
    solver = CliSolver(
        None,
        ch,
        engine="claude",
        shared_graph=_Graph(),
        container=handle,
        worker_env={"HOME": f"{CONTAINER_WORKSPACE}/workers/_homes/cli-claude"},
    )

    env = solver._worker_env()

    assert env["HOME"] == f"{CONTAINER_WORKSPACE}/workers/_homes/cli-claude"
    assert env["MUTEKI_BLACKBOARD_DB"] == f"{CONTAINER_WORKSPACE}/graph/shared_graph.db"


def test_worker_env_prepends_stable_tool_path_before_host_shims(monkeypatch):
    monkeypatch.setenv(
        "PATH",
        "/custom/jenv/shims:/opt/homebrew/bin:/custom/bin",
    )
    ch = Challenge(
        id="env-path",
        name="env-path",
        category="misc",
        description="path stability",
        flag_format="flag{...}",
    )
    solver = CliSolver(None, ch, engine="claude")

    parts = solver._worker_env()["PATH"].split(":")

    assert parts[:4] == ["/usr/bin", "/bin", "/usr/sbin", "/sbin"]
    assert parts.index("/usr/bin") < parts.index("/custom/jenv/shims")
    assert parts.count("/opt/homebrew/bin") == 1
    assert "/custom/bin" in parts


def test_worker_env_blackboard_script_points_at_repo_copy_for_source_runs(tmp_path):
    """A source checkout (the test env) resolves the skill to the IN-REPO copy for
    EVERY engine — no deployed ~/.claude or ~/.agents copy that can drift out of sync
    (run-75378). The container path is still the image-baked one."""
    ch = Challenge(
        id="env-board",
        name="env-board",
        category="misc",
        description="blackboard env",
        flag_format="flag{...}",
    )

    repo_skill = (
        Path(cli_solver.__file__).resolve().parent.parent.parent
        / "skills" / "muteki-blackboard" / "blackboard.py"
    )
    assert repo_skill.is_file()  # sanity: we ARE running from a source checkout

    for engine in ("claude", "codex", "cursor", "pi", "omp", "kimi", "grok"):
        env = CliSolver(None, ch, engine=engine)._worker_env()
        assert env["MUTEKI_BLACKBOARD_SCRIPT"] == str(repo_skill)

    handle = ContainerHandle(
        run_id="env-board",
        host_workspace=str(tmp_path),
        container="muteki-run-env-board",
    )
    cont_env = CliSolver(None, ch, engine="claude", container=handle)._worker_env()
    assert cont_env["MUTEKI_BLACKBOARD_SCRIPT"] == "/usr/local/bin/blackboard.py"


def test_worker_env_does_not_fall_back_to_user_skill_directories(monkeypatch):
    """A missing project Skill fails explicitly and leaves user Skills untouched."""
    ch = Challenge(
        id="env-board-install",
        name="env-board-install",
        category="misc",
        description="blackboard env",
        flag_format="flag{...}",
    )
    # Simulate "no in-repo skill" so the install fallback path is exercised.
    monkeypatch.setattr(cli_solver, "_repo_blackboard_script", lambda: None)

    for engine in ("claude", "codex", "cursor", "pi", "omp", "kimi", "grok"):
        with pytest.raises(FileNotFoundError, match="user-level Skill"):
            CliSolver(None, ch, engine=engine)._worker_env()


def test_worker_env_exposes_current_intent_id():
    ch = Challenge(
        id="env-intent",
        name="env-intent",
        category="misc",
        description="intent id",
        flag_format="flag{...}",
    )
    solver = CliSolver(
        None,
        ch,
        engine="claude",
        mode="explore",
        intent_goal="probe /admin",
        intent_id="I-admin",
    )

    assert solver._worker_env()["MUTEKI_INTENT_ID"] == "I-admin"


# argv[0] is the RESOLVED engine binary (a pinned official path), not the bare
# name — so assert against d.bin, which is the contract these tests actually mean.
def test_claude_execute_argv_has_session_and_skip_perms():
    d = ClaudeCodeDriver()
    sess = d.new_session()
    assert sess  # claude pre-seeds a uuid session
    argv = d.build_execute("DO THE THING", sess)
    assert argv[0] == d.bin and "-p" in argv
    assert "--dangerously-skip-permissions" in argv
    assert "--session-id" in argv and sess in argv
    assert argv[-1] == "DO THE THING"  # prompt is last, after --


def test_claude_resume_uses_dash_r():
    d = ClaudeCodeDriver()
    argv = d.build_resume("CONCLUDE", "sess-123")
    assert argv[:3] == [d.bin, "-r", "sess-123"]
    assert "--dangerously-skip-permissions" in argv


def test_codex_execute_and_resume():
    d = CodexDriver()
    # Offline removes native search plus Desktop's app/browser/plugin tool
    # providers while preserving CODEX_HOME auth and Skills.
    ex = d.build_execute("GO", None, web_access=False)
    assert ex[:2] == [d.bin, "exec"]
    assert "--dangerously-bypass-approvals-and-sandbox" in ex
    assert 'web_search="disabled"' in ex
    assert "code_mode" in ex
    assert "browser_use" in ex
    assert "plugins" in ex
    rs = d.build_resume("CONCLUDE", "abc", web_access=False)
    assert rs[:4] == [d.bin, "exec", "resume", "abc"]
    assert "code_mode" in rs


def test_secret_prompt_invocations_are_stdin_only_and_non_persistent():
    from muteki.solver.cli_driver import SecurePromptUnsupported

    secret = "operator-secret-argv-998877"

    claude = ClaudeCodeDriver()
    assert claude.secure_prompt_transport is True
    claude_argv = claude.build_execute_stdin(secret, claude.new_session(), stream=True)
    assert secret not in "\0".join(claude_argv)
    assert "--no-session-persistence" in claude_argv
    assert "--bare" not in claude_argv
    assert claude_argv[-1] == "--"

    codex = CodexDriver()
    assert codex.secure_prompt_transport is True
    codex_argv = codex.build_execute_stdin(secret, None, stream=True)
    assert secret not in "\0".join(codex_argv)
    assert "--ephemeral" in codex_argv
    assert codex_argv[-1] == "-"

    wrapped = driver_for({"id": "claude-secure", "engine": "claude", "model": "x"})
    assert wrapped.secure_prompt_transport is True
    wrapped_argv = wrapped.build_execute_stdin(secret, None, stream=True)
    assert secret not in "\0".join(wrapped_argv)
    assert wrapped_argv[wrapped_argv.index("--model") + 1] == "x"
    assert "--bare" not in wrapped_argv

    injected = driver_for({
        "id": "claude-key", "engine": "claude", "model": "x",
        "credential_kind": "engine_key", "credential_account": "claude-main",
    })
    injected_argv = injected.build_execute_stdin(secret, None, stream=True)
    assert "--bare" in injected_argv

    # Cursor's installed CLI reads a missing headless positional from stdin, but
    # exposes no no-persistence mode; exact secret delivery must fail closed.
    cursor = CursorDriver()
    assert cursor.secure_prompt_transport is False
    with pytest.raises(SecurePromptUnsupported, match="cannot disable.*persistence"):
        cursor.build_execute_stdin(secret, None, stream=True)


def test_secret_result_session_id_is_never_marked_resumable():
    ch = Challenge(id="secret-session", name="secret", category="misc")
    solver = CliSolver(None, ch, engine="claude")
    solver._control_secret_values = ["one-shot-secret"]
    solver._mark_session_if_live(
        CliResult(text="completed", session="in-memory-only-session"))
    assert solver._session_established is False


def test_secure_prompt_preflight_validates_installed_help_contract(monkeypatch):
    from muteki.solver import cli_driver as mod

    mod._SECURE_HELP_CACHE.clear()
    calls = []

    def fake_run(argv, **kwargs):
        calls.append(argv)
        if "exec" in argv:
            text = "--ephemeral Run without persisting; prompt is read from stdin"
        else:
            text = "--print --bare --no-session-persistence"
        return subprocess.CompletedProcess(argv, 0, stdout=text, stderr="")

    monkeypatch.setattr(mod.subprocess, "run", fake_run)
    claude = ClaudeCodeDriver()
    claude._bin = "/missing/test-claude"
    codex = CodexDriver()
    codex._bin = "/missing/test-codex"
    assert claude.secure_prompt_preflight() == (True, "")
    assert codex.secure_prompt_preflight() == (True, "")
    assert len(calls) == 2

    mod._SECURE_HELP_CACHE.clear()
    monkeypatch.setattr(
        mod.subprocess, "run",
        lambda argv, **kwargs: subprocess.CompletedProcess(
            argv, 0, stdout="--print only", stderr=""))
    ok, detail = claude.secure_prompt_preflight()
    assert ok is False
    assert "--no-session-persistence" in detail


def test_codex_search_is_a_global_flag_before_exec():
    # --search must precede the `exec` subcommand (it's a global codex flag),
    # otherwise codex errors on an unknown exec flag.
    d = CodexDriver()
    ex = d.build_execute("GO", None, web_access=True)
    assert ex[0] == d.bin and "--search" in ex
    assert ex.index("--search") < ex.index("exec")


# ── cursor-agent driver (the third engine) ───────────────────────────────────

def test_cursor_execute_argv_headless_print():
    d = CursorDriver()
    assert d.new_session() is None  # cursor assigns the chat id itself
    argv = d.build_execute("DO THE THING", None, stream=True)
    assert argv[0] == d.bin and "-p" in argv
    assert "--force" in argv and "--trust" in argv
    assert argv[argv.index("--output-format") + 1] == "stream-json"
    assert argv[-1] == "DO THE THING"  # prompt is the trailing positional


def test_cursor_execute_non_stream_uses_json():
    d = CursorDriver()
    argv = d.build_execute("GO", None, stream=False)
    assert argv[argv.index("--output-format") + 1] == "json"


def test_cursor_resume_uses_resume_flag():
    d = CursorDriver()
    argv = d.build_resume("CONCLUDE", "chat-abc", stream=True)
    assert argv[0] == d.bin and "--resume" in argv
    assert argv[argv.index("--resume") + 1] == "chat-abc"
    assert argv[-1] == "CONCLUDE"


# ── self-check / health probe (FE-healthcheck-page) ──────────────────────────
# All three engines now send a REAL one-turn hello (symmetric), retry once on a
# transient miss, and return a classified detail. These mock subprocess.run so
# they stay pure (no real CLI, no key) — consistent with the rest of this file.

import subprocess as _sp  # noqa: E402


def _CP(rc: int, out: str = "", err: str = "") -> "_sp.CompletedProcess":
    return _sp.CompletedProcess(args=[], returncode=rc, stdout=out, stderr=err)


def test_all_engines_send_a_real_hello_probe():
    # the symmetry fix: codex/cursor used to only check --version. Now every
    # driver builds a non-empty one-turn argv carrying the hello prompt.
    for drv in (ClaudeCodeDriver(), CodexDriver(), CursorDriver()):
        argv = drv._hello_argv()
        assert argv, f"{drv.name} has no hello probe"
        assert drv.HELLO_PROMPT in argv, f"{drv.name} probe omits the hello prompt"


def test_claude_hello_ok_requires_result_envelope():
    d = ClaudeCodeDriver()
    assert d._hello_ok(_CP(0, '{"result":"OK"}')) is True
    assert d._hello_ok(_CP(0, "no envelope")) is False     # exit 0 but no turn
    assert d._hello_ok(_CP(7, '{"result":"OK"}')) is False  # nonzero wins


def test_codex_hello_ok_accepts_stream_markers():
    d = CodexDriver()
    assert d._hello_ok(_CP(0, '{"type":"item.completed","item":{"type":"agent_message"}}')) is True
    assert d._hello_ok(_CP(0, '{"type":"turn.completed","usage":{}}')) is True
    assert d._hello_ok(_CP(0, "nothing useful")) is False
    assert d._hello_ok(_CP(1, "agent_message")) is False    # nonzero wins


def test_codex_hello_ok_tolerates_post_turn_mcp_shutdown_error():
    d = CodexDriver()
    stdout = (
        '{"type":"thread.started","thread_id":"t"}\n'
        '{"type":"turn.started"}\n'
        '{"type":"item.completed","item":{"type":"agent_message","text":"OK"}}\n'
        '{"type":"turn.completed","usage":{}}\n'
    )
    stderr = "MCP startup failed: HTTP 401: {}, when send initialize request"

    assert d._hello_ok(_CP(1, stdout, stderr)) is True


def test_codex_hello_probe_allows_transport_fallback_window(monkeypatch):
    d = CodexDriver()
    _ = d.bin
    seen = {}

    def fake_run(argv, **kw):
        seen["timeout"] = kw.get("timeout")
        return _CP(0, '{"type":"turn.completed","usage":{}}')

    monkeypatch.setattr("muteki.solver.cli_driver.subprocess.run", fake_run)
    ok, detail = d.health_detail()

    assert ok is True and detail == ""
    assert seen["timeout"] >= 120


def test_health_detail_retries_once_then_succeeds(monkeypatch):
    # a single transient miss must NOT report red — retry recovers it.
    d = ClaudeCodeDriver()
    _ = d.bin  # resolve+cache the binary BEFORE we mock run (resolution probes too)
    calls = {"n": 0}

    def fake_run(argv, **kw):
        calls["n"] += 1
        if calls["n"] == 1:
            return _CP(1, "", "rate limit (overloaded)")  # transient
        return _CP(0, '{"result":"OK"}')                   # recovered

    monkeypatch.setattr("muteki.solver.cli_driver.subprocess.run", fake_run)
    monkeypatch.setattr("muteki.solver.cli_driver.time.sleep", lambda *_: None)
    ok, detail = d.health_detail()
    assert ok is True and detail == ""
    assert calls["n"] == 2  # exactly one retry


def test_health_detail_classifies_persistent_failure(monkeypatch):
    d = ClaudeCodeDriver()
    _ = d.bin  # cache the binary before mocking run

    def fake_run(argv, **kw):
        return _CP(1, "", "Invalid API key (401)")

    monkeypatch.setattr("muteki.solver.cli_driver.subprocess.run", fake_run)
    monkeypatch.setattr("muteki.solver.cli_driver.time.sleep", lambda *_: None)
    ok, detail = d.health_detail()
    assert ok is False
    # the real reason is surfaced, NOT a blanket "check login / quota"
    assert "401" in detail and "exited 1" in detail


def test_health_detail_classifies_timeout(monkeypatch):
    d = CursorDriver()
    _ = d.bin  # cache the binary before mocking run

    def fake_run(argv, **kw):
        raise _sp.TimeoutExpired(cmd="x", timeout=60)

    monkeypatch.setattr("muteki.solver.cli_driver.subprocess.run", fake_run)
    monkeypatch.setattr("muteki.solver.cli_driver.time.sleep", lambda *_: None)
    ok, detail = d.health_detail()
    assert ok is False and "timed out" in detail


def test_healthcheck_bool_delegates_to_detail(monkeypatch):
    # back-compat: the swarm still calls the bool healthcheck(); it must mirror
    # health_detail()'s verdict.
    d = CodexDriver()
    monkeypatch.setattr(d, "health_detail", lambda: (True, ""))
    assert d.healthcheck() is True
    monkeypatch.setattr(d, "health_detail", lambda: (False, "nope"))
    assert d.healthcheck() is False


def test_engine_bar_codex_health_uses_host_default_auth_not_stale_account(tmp_path, monkeypatch):
    root = tmp_path / "_secrets" / "accounts"
    codex_home = root / "codex-main" / "codex-home"
    codex_home.mkdir(parents=True)
    (codex_home / "auth.json").write_text("{}\n")
    monkeypatch.delenv("CODEX_HOME", raising=False)
    seen = {}
    d = CodexDriver()

    def fake_health_detail():
        import os
        seen["CODEX_HOME"] = os.environ.get("CODEX_HOME")
        return True, ""

    monkeypatch.setattr(d, "health_detail", fake_health_detail)

    assert _probe_health_with_creds("codex", d, str(root)) == (True, "")
    assert seen["CODEX_HOME"] is None


def test_engine_status_is_cheap_and_does_not_deep_probe(monkeypatch):
    import muteki.solver.cli_driver as cli_driver

    monkeypatch.setenv("MUTEKI_CLAUDE_BIN", "/usr/bin/claude")
    monkeypatch.setattr(cli_driver, "_runs_ok", lambda _path: True)

    def fail_deep_probe():
        raise AssertionError("/api/engines must not spend a model turn")

    monkeypatch.setattr(cli_driver.DRIVERS["claude"], "health_detail", fail_deep_probe)

    rows = cli_driver.engine_status(
        profiles=[{
            "id": "claude-main",
            "name": "claude-main",
            "engine": "claude",
            "transport": "claude_code",
            "model": "sonnet",
        }],
    )

    assert rows == [{
        "engine": "claude",
        "bin": "/usr/bin/claude",
        "available": True,
        "healthy": None,
        "health_detail": "",
        "profile_id": "claude-main",
        "profile_name": "claude-main",
        "model": "sonnet",
        "backend": "local",
    }]


def test_engine_status_lists_each_dispatched_worker(monkeypatch):
    import muteki.solver.cli_driver as cli_driver

    monkeypatch.setenv("MUTEKI_PI_BIN", "/usr/bin/pi")
    monkeypatch.setattr(cli_driver, "_runs_ok", lambda _path: True)

    rows = cli_driver.engine_status(
        profiles=[
            {
                "id": "seat_pi_lan",
                "name": "seat_pi_lan",
                "label": "pi-lan-deepseek",
                "engine": "pi",
                "model": "deepseek-local",
            },
            {
                "id": "seat_pi_ollama",
                "name": "seat_pi_ollama",
                "label": "pi-ollama",
                "engine": "pi",
                "model": "deepseek-cloud",
            },
        ],
    )

    assert [(row["engine"], row["profile_id"], row["profile_name"]) for row in rows] == [
        ("pi", "seat_pi_lan", "pi-lan-deepseek"),
        ("pi", "seat_pi_ollama", "pi-ollama"),
    ]


def test_health_detail_falls_back_to_version_when_no_hello(monkeypatch):
    # a hypothetical driver with no cheap dry-run (empty _hello_argv) degrades to
    # the --version liveness check rather than reporting red.
    d = CodexDriver()
    _ = d.bin  # cache the binary before mocking run
    monkeypatch.setattr(d, "_hello_argv", lambda: [])

    def fake_run(argv, **kw):
        assert "--version" in argv
        return _CP(0, "codex-cli 0.137.0")

    monkeypatch.setattr("muteki.solver.cli_driver.subprocess.run", fake_run)
    ok, detail = d.health_detail()
    assert ok is True and detail == ""


def test_cursor_model_env_adds_flag(monkeypatch):
    monkeypatch.setenv("MUTEKI_CURSOR_MODEL", "sonnet-4.5-thinking")
    d = CursorDriver()
    argv = d.build_execute("GO", None)
    assert argv[argv.index("--model") + 1] == "sonnet-4.5-thinking"
    monkeypatch.delenv("MUTEKI_CURSOR_MODEL", raising=False)
    assert "--model" not in d.build_execute("GO", None)


def test_cursor_parse_single_json():
    d = CursorDriver()
    out = ('{"type":"result","subtype":"success","is_error":false,'
           '"result":"FOUND_FLAG=flag{cursor}","session_id":"c1","duration_ms":1200,'
           '"usage":{"inputTokens":21732,"outputTokens":30,'
           '"cacheReadTokens":5408,"cacheWriteTokens":0}}')
    r = d.parse(out, "")
    assert r.text == "FOUND_FLAG=flag{cursor}"
    assert r.session == "c1"
    assert r.cost_usd is None  # subscription-backed
    # tokens still recorded (fresh + cache buckets) for the deck's usage column
    assert r.input_tokens == 21732 + 5408 + 0 and r.output_tokens == 30


def test_cursor_parse_stream_jsonl_recovers_result():
    d = CursorDriver()
    dump = "\n".join([
        '{"type":"system","subtype":"init","session_id":"c9","model":"x"}',
        '{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"working"}]},"session_id":"c9"}',
        '{"type":"result","subtype":"success","result":"FOUND_FLAG=flag{stream}","session_id":"c9","usage":{"inputTokens":100,"outputTokens":7}}',
    ])
    r = d.parse(dump, "")
    assert "FOUND_FLAG=flag{stream}" in r.text
    assert r.session == "c9" and r.cost_usd is None
    assert r.input_tokens == 100 and r.output_tokens == 7


def test_cursor_parse_stream_line_shapes():
    d = CursorDriver()
    # system init → session
    s0 = d.parse_stream_line('{"type":"system","subtype":"init","session_id":"c1"}')
    assert s0 and s0.kind == "session" and s0.session == "c1"
    # assistant text → reasoning
    s1 = d.parse_stream_line('{"type":"assistant","message":{"content":[{"type":"text","text":"reading README"}]}}')
    assert s1 and s1.kind == "reasoning" and "reading" in s1.text
    # tool_call started (readToolCall) → tool with the path
    s2 = d.parse_stream_line('{"type":"tool_call","subtype":"started","call_id":"x","tool_call":{"readToolCall":{"args":{"path":"a.txt"}}}}')
    assert (s2 and s2.kind == "tool" and s2.tool == "read"
            and "a.txt" in s2.text and s2.call_id == "x")
    # tool_call started (function shape) → tool with the name
    s3 = d.parse_stream_line('{"type":"tool_call","subtype":"started","tool_call":{"function":{"name":"shell","arguments":"curl x"}}}')
    assert s3 and s3.kind == "tool" and s3.tool == "shell" and "curl" in s3.text
    # tool_call completed → tool_result with the content
    s4 = d.parse_stream_line('{"type":"tool_call","subtype":"completed","tool_call":{"readToolCall":{"args":{"path":"a.txt"},"result":{"success":{"content":"hello"}}}}}')
    assert s4 and s4.kind == "tool_result" and "hello" in s4.text
    # noise → None
    assert d.parse_stream_line("not json") is None


def test_claude_parse_stream_steps_emits_all_blocks():
    """#18 regression: a claude assistant message with MULTIPLE content blocks must
    yield a StreamStep for EVERY block. The old parse_stream_line returned at the first
    block, so a FOUND_FLAG/VERIFIED_FACT in a later block never propagated live (only
    via the final parse()). The streaming runner now uses parse_stream_steps."""
    d = ClaudeCodeDriver()
    line = ('{"type":"assistant","message":{"content":['
            '{"type":"text","text":"let me check the response"},'
            '{"type":"tool_use","id":"call-1","name":"Bash",'
            '"input":{"command":"curl x"}},'
            '{"type":"text","text":"FOUND_FLAG=flag{multi_block}"}]}}')
    steps = d.parse_stream_steps(line)
    assert len(steps) == 3, f"all 3 blocks must emit, got {len(steps)}"
    assert steps[0].kind == "reasoning"
    assert (steps[1].kind == "tool" and steps[1].tool == "Bash"
            and steps[1].call_id == "call-1")
    # the LAST block (the one with the flag) must be present — this is the bug fix
    assert any("FOUND_FLAG=flag{multi_block}" in s.text for s in steps)
    # back-compat: parse_stream_line still returns the FIRST step
    first = d.parse_stream_line(line)
    assert first and first.kind == "reasoning"


def test_cursor_parse_stream_steps_emits_all_text_blocks():
    """#18 regression for cursor: multiple text blocks in one assistant message must
    all emit (a later block's FOUND_FLAG was lost by the first-block return)."""
    d = CursorDriver()
    line = ('{"type":"assistant","message":{"content":['
            '{"type":"text","text":"probing"},'
            '{"type":"text","text":"VERIFIED_FACT=admin panel at /admin"},'
            '{"type":"text","text":"FOUND_FLAG=flag{cursor_multi}"}]}}')
    steps = d.parse_stream_steps(line)
    assert len(steps) == 3
    assert any("FOUND_FLAG=flag{cursor_multi}" in s.text for s in steps)
    assert any("VERIFIED_FACT=" in s.text for s in steps)


def test_cursor_resolves_cursor_agent_binary(monkeypatch):
    # the engine "cursor" must resolve to the `cursor-agent` basename on PATH,
    # not a bare `cursor` (which is the GUI launcher).
    from muteki.solver import cli_driver as mod
    monkeypatch.delenv("MUTEKI_CURSOR_BIN", raising=False)
    monkeypatch.setattr(mod, "_KNOWN_GOOD", {"cursor": []})
    seen = {}

    def _fake_which(name):
        seen["name"] = name
        return ["/x/cursor-agent"]

    monkeypatch.setattr(mod, "_which_all", _fake_which)
    monkeypatch.setattr(mod, "_looks_bad", lambda p: False)
    monkeypatch.setattr(mod, "_runs_ok", lambda p: True)
    assert mod.resolve_engine_bin("cursor") == "/x/cursor-agent"
    assert seen["name"] == "cursor-agent"  # scanned for cursor-agent, not cursor


def test_cursor_bare_name_fallback_is_cursor_agent(monkeypatch):
    from muteki.solver import cli_driver as mod
    monkeypatch.delenv("MUTEKI_CURSOR_BIN", raising=False)
    monkeypatch.setattr(mod, "_KNOWN_GOOD", {"cursor": []})
    monkeypatch.setattr(mod, "_which_all", lambda name: [])
    assert mod.resolve_engine_bin("cursor") == "cursor-agent"


# ── offline / web-access toggle (clean eval hygiene) ─────────────────────────

def test_claude_offline_denies_web_tools():
    d = ClaudeCodeDriver()
    online = d.build_execute("GO", d.new_session(), web_access=True)
    assert "--disallowed-tools" not in online  # web on by default
    offline = d.build_execute("GO", d.new_session(), web_access=False)
    assert "--disallowed-tools" in offline
    i = offline.index("--disallowed-tools")
    assert "WebSearch" in offline[i:i + 3] and "WebFetch" in offline[i:i + 3]
    # the deny flags must come before the prompt sentinel, not after --
    assert offline.index("--disallowed-tools") < offline.index("--")


def test_claude_resume_respects_offline():
    d = ClaudeCodeDriver()
    rs = d.build_resume("CONCLUDE", "s1", web_access=False)
    assert "--disallowed-tools" in rs


def test_codex_web_is_opt_in():
    # codex exec has NO web tool unless --search is passed → offline by default.
    d = CodexDriver()
    assert "--search" not in d.build_execute("GO", None, web_access=False)
    assert "--search" in d.build_execute("GO", None, web_access=True)


def test_codex_offline_kb_isolation_skips_user_mcp_config():
    d = CodexDriver()
    offline = d.build_execute(
        "GO", None, web_access=False, kb_access=False)
    assert "--ignore-user-config" in offline
    online = d.build_execute(
        "GO", None, web_access=True, kb_access=True)
    assert "--ignore-user-config" not in online


def test_claude_endpoint_preserves_offline_web_isolation():
    profile = {
        "id": "deepseek-claude",
        "name": "deepseek-claude",
        "engine": "claude",
        "base_url": "https://api.deepseek.example/anthropic",
    }
    d = driver_for(profile)
    assert d.offline_web_isolation is True
    argv = d.build_execute("GO", d.new_session(), web_access=False)
    deny = argv.index("--disallowed-tools")
    prompt = argv.index("--")
    assert "WebSearch" in argv[deny:prompt]
    assert "WebFetch" in argv[deny:prompt]


def test_cursor_offline_uses_acp_while_online_keeps_force():
    driver = CursorDriver()
    assert driver.offline_web_isolation is True
    offline = driver.build_execute("GO", None, web_access=False)
    assert "offline_acp_bridge.py" in " ".join(offline)
    assert "--force" not in offline
    online = driver.build_execute("GO", None, web_access=True)
    assert "--force" in online
    assert "--trust" in online


# ── pi / oh-my-pi drivers (the pi CLI family) ────────────────────────────────
# Both speak the same headless protocol: `[bin, -p, --mode, json, ...flags, PROMPT]`
# (trailing positional, no `--`), an engine-assigned session scraped from the
# {"type":"session"} header, and JSONL events parsed into CliResult/StreamStep.

def _pi_usage(**over):
    u = {"input": 1200, "output": 42, "cacheRead": 300, "cacheWrite": 100,
         "totalTokens": 1642,
         "cost": {"input": 0.001, "output": 0.0004, "cacheRead": 0.0001,
                  "cacheWrite": 0.0002, "total": 0.0017}}
    u.update(over)
    return u


def _pi_run_dump() -> str:
    """A realistic two-tool pi --mode json transcript (event shapes verified
    against pi 0.84.1 / omp 17.2.12 live output)."""
    usage = _pi_usage()
    assistant = {"role": "assistant",
                 "content": [{"type": "text", "text": "FOUND_FLAG=flag{pi_ok}"}],
                 "api": "openai-completions", "provider": "muteki",
                 "model": "deepseek-v4-flash:0731-cloud",
                 "usage": usage, "stopReason": "stop", "timestamp": 3}
    return "\n".join([
        '{"type":"session","version":3,"id":"pi-sess-1",'
        '"timestamp":"2026-08-11T10:57:20.215Z","cwd":"/work"}',
        '{"type":"agent_start"}',
        '{"type":"turn_start"}',
        '{"type":"message_start","message":{"role":"user","content":[{"type":"text","text":"GO"}],"timestamp":1}}',
        '{"type":"message_end","message":{"role":"user","content":[{"type":"text","text":"GO"}],"timestamp":1}}',
        '{"type":"message_start","message":{"role":"assistant","content":[],"api":"openai-completions","provider":"muteki","timestamp":2}}',
        '{"type":"message_update","assistantMessageEvent":{"type":"text_delta","contentIndex":0,"delta":"FOUND_FLAG=flag{pi_ok}"}}',
        json.dumps({"type": "tool_execution_start", "toolCallId": "call-1",
                    "toolName": "bash", "args": {"command": "cat flag.txt"}}),
        json.dumps({"type": "tool_execution_end", "toolCallId": "call-1",
                    "toolName": "bash", "isError": False,
                    "result": {"content": [{"type": "text", "text": "flag{pi_ok}\n"}]}}),
        json.dumps({"type": "message_end", "message": assistant}),
        json.dumps({"type": "turn_end", "message": assistant, "toolResults": []}),
        json.dumps({"type": "agent_end", "messages": [
            {"role": "user", "content": [{"type": "text", "text": "GO"}]},
            assistant]}),
    ])


def test_pi_execute_argv_json_mode_trailing_prompt():
    d = PiDriver()
    assert d.new_session() is None  # the engine assigns the session id itself
    argv = d.build_execute("DO THE THING", None, stream=True)
    assert argv[0] == d.bin
    assert argv[1:3] == ["-p", "--mode"] and argv[3] == "json"
    assert "--" not in argv  # no separator: prompt is the bare trailing positional
    assert argv[-1] == "DO THE THING"
    # --mode json already streams per-step events; stream=True adds nothing
    assert argv == d.build_execute("DO THE THING", None, stream=False)


def test_pi_stdin_argv_is_ephemeral_and_prompt_free():
    secret = "exact-operator-secret"
    d = PiDriver()
    assert d.secure_prompt_transport is True
    argv = d.build_execute_stdin(secret, None, stream=True)
    assert secret not in "\0".join(argv)      # prompt rides stdin, never argv
    assert "--no-session" in argv
    assert argv[1:3] == ["-p", "--mode"]


def test_omp_stdin_argv_is_ephemeral_and_prompt_free():
    secret = "exact-operator-secret"
    d = OhMyPiDriver()
    assert d.secure_prompt_transport is True
    argv = d.build_execute_stdin(secret, None)
    assert secret not in "\0".join(argv) and "--no-session" in argv


def test_pi_secure_prompt_preflight(monkeypatch):
    from muteki.solver import cli_driver as mod
    mod._SECURE_HELP_CACHE.clear()

    def fake_run(argv, **kwargs):
        return subprocess.CompletedProcess(
            argv, 0,
            stdout="--no-session Don't save session (ephemeral)\n--mode <mode>", stderr="")

    monkeypatch.setattr(mod.subprocess, "run", fake_run)
    d = PiDriver()
    d._bin = "/missing/test-pi"
    assert d.secure_prompt_preflight() == (True, "")


def test_pi_resume_uses_session_flag():
    d = PiDriver()
    argv = d.build_resume("CONCLUDE", "019ff078-3f57", stream=True)
    assert argv[0] == d.bin
    assert argv[argv.index("--session") + 1] == "019ff078-3f57"
    assert "--mode" in argv and argv[-1] == "CONCLUDE"


def test_omp_resume_uses_resume_flag():
    d = OhMyPiDriver()
    argv = d.build_resume("CONCLUDE", "019ff078-3f57", stream=True)
    assert argv[argv.index("--resume") + 1] == "019ff078-3f57"
    assert "--session" not in argv
    assert argv[-1] == "CONCLUDE"


def test_pi_bin_resolution_env_override_and_fallback(monkeypatch):
    from muteki.solver import cli_driver as mod
    monkeypatch.setenv("MUTEKI_PI_BIN", "/custom/pi")
    assert mod.resolve_engine_bin("pi") == "/custom/pi"
    monkeypatch.delenv("MUTEKI_PI_BIN", raising=False)
    monkeypatch.setattr(mod, "_KNOWN_GOOD", {**mod._KNOWN_GOOD, "pi": []})
    monkeypatch.setattr(mod, "_which_all", lambda name: [])
    assert mod.resolve_engine_bin("pi") == "pi"


def test_omp_bin_resolution_env_override_and_fallback(monkeypatch):
    from muteki.solver import cli_driver as mod
    monkeypatch.setenv("MUTEKI_OMP_BIN", "/custom/omp")
    assert mod.resolve_engine_bin("omp") == "/custom/omp"
    monkeypatch.delenv("MUTEKI_OMP_BIN", raising=False)
    monkeypatch.setattr(mod, "_KNOWN_GOOD", {**mod._KNOWN_GOOD, "omp": []})
    monkeypatch.setattr(mod, "_which_all", lambda name: [])
    assert mod.resolve_engine_bin("omp") == "omp"


def test_omp_offline_uses_acp_with_empty_mcp_and_disabled_native_web():
    d = OhMyPiDriver()
    assert d.offline_web_isolation is True
    online = d.build_execute("GO", None, web_access=True)
    assert "offline_acp_bridge.py" not in "\0".join(online)
    offline = d.build_execute("GO", None, web_access=False)
    joined = "\0".join(offline)
    assert "offline_acp_bridge.py" in joined
    assert "omp_offline_config.yml" in joined
    assert "--agent-label\0omp" in joined
    assert offline[-1] == "GO"
    resumed = d.build_resume("CONCLUDE", "s1", web_access=False)
    assert resumed[resumed.index("--resume") + 1] == "s1"
    with pytest.raises(SecurePromptUnsupported, match="persists prompts"):
        d.build_execute_stdin("S", None, web_access=False)


def test_pi_offline_needs_no_argv_change():
    # pi's built-in tools (read/bash/edit/write/grep/find/ls) have no web access,
    # so offline is a capability claim with NO argv change.
    d = PiDriver()
    assert d.offline_web_isolation is True
    assert (d.build_execute("GO", None, web_access=False)
            == d.build_execute("GO", None, web_access=True))


def test_pi_model_provider_runtime_flags():
    d = PiDriver()
    argv = apply_runtime_argv(
        d.build_execute("GO", None),
        driver=d,
        env={
            "MUTEKI_PI_MODEL": "deepseek-v4-flash:0731-cloud",
            "MUTEKI_PI_PROVIDER": "muteki",
        },
    )
    # a model id may contain ":" — passed through verbatim
    assert argv[argv.index("--model") + 1] == "deepseek-v4-flash:0731-cloud"
    assert argv[argv.index("--provider") + 1] == "muteki"
    assert argv[-1] == "GO"  # flags never swallow the trailing prompt
    rs = apply_runtime_argv(
        d.build_resume("CONCLUDE", "s1"),
        driver=d,
        env={
            "MUTEKI_PI_MODEL": "deepseek-v4-flash:0731-cloud",
            "MUTEKI_PI_PROVIDER": "muteki",
        },
    )
    assert "--model" in rs and rs[-1] == "CONCLUDE"
    clean = d.build_execute("GO", None)
    assert "--model" not in clean and "--provider" not in clean


def test_omp_model_provider_runtime_flags(monkeypatch):
    d = OhMyPiDriver()
    argv = apply_runtime_argv(
        d.build_execute("GO", None),
        driver=d,
        env={"MUTEKI_OMP_MODEL": "gpt-5-mini", "MUTEKI_OMP_PROVIDER": "openai"},
    )
    assert argv[argv.index("--model") + 1] == "gpt-5-mini"
    assert argv[argv.index("--provider") + 1] == "openai"
    # pi env vars must NOT leak into the omp driver
    monkeypatch.setenv("MUTEKI_PI_MODEL", "pi-only")
    assert "--model" not in d.build_execute("GO", None)


def test_pi_profile_model_injected_before_prompt():
    # pi/omp argv has NO `--` separator, so the profile model must land BEFORE the
    # trailing positional prompt (generic _insert_model_arg path).
    d = driver_for({"id": "pi-sub-container", "engine": "pi",
                    "model": "deepseek-v4-flash:0731-cloud"})
    argv = d.build_execute("GO", None)
    assert argv[-1] == "GO"
    assert argv[argv.index("--model") + 1] == "deepseek-v4-flash:0731-cloud"
    # stdin argv has no prompt at all — insertion before the last flag is fine
    stdin_argv = d.build_execute_stdin("S", None)
    assert "--model" in stdin_argv and "S" not in "\0".join(stdin_argv)


def test_pi_process_env_does_not_override_profile_model(monkeypatch):
    # The selected Profile is authoritative. A stale process-global value from
    # another account/probe must not change its argv.
    monkeypatch.setenv("MUTEKI_PI_MODEL", "env-model")
    d = driver_for({"id": "pi-x", "engine": "pi", "model": "profile-model"})
    argv = d.build_execute("GO", None)
    assert argv.count("--model") == 1
    assert argv[argv.index("--model") + 1] == "profile-model"


def test_pi_parse_jsonl_full_run():
    d = PiDriver()
    r = d.parse(_pi_run_dump(), "")
    assert r.text == "FOUND_FLAG=flag{pi_ok}"
    assert r.session == "pi-sess-1"
    # input = fresh + cacheRead + cacheWrite; output straight from usage
    assert r.input_tokens == 1200 + 300 + 100
    assert r.output_tokens == 42
    assert r.cost_usd == pytest.approx(0.0017)
    assert r.num_turns == 1


def test_pi_parse_last_assistant_message_wins():
    d = PiDriver()
    first = {"role": "assistant",
             "content": [{"type": "text", "text": "working on it"}],
             "usage": _pi_usage(input=10, output=5,
                                cost={"total": 0.0001})}
    second = {"role": "assistant",
              "content": [{"type": "text", "text": "FOUND_FLAG=flag{final}"}],
              "usage": _pi_usage(input=20, output=7,
                                 cost={"total": 0.002})}
    dump = "\n".join([
        '{"type":"session","version":3,"id":"s2"}',
        json.dumps({"type": "message_end", "message": first}),
        json.dumps({"type": "turn_end", "message": first, "toolResults": []}),
        json.dumps({"type": "message_end", "message": second}),
        json.dumps({"type": "turn_end", "message": second, "toolResults": []}),
        '{"type":"agent_end","messages":[]}',
    ])
    r = d.parse(dump, "")
    assert r.text == "FOUND_FLAG=flag{final}"
    assert r.input_tokens == 20 + 300 + 100 and r.output_tokens == 7
    assert r.cost_usd == pytest.approx(0.002)
    assert r.num_turns == 2


def test_pi_parse_falls_back_to_agent_end_messages():
    # a worker killed mid-stream may miss the assistant message_end; the final
    # agent_end still carries the messages array.
    d = PiDriver()
    dump = "\n".join([
        '{"type":"session","version":3,"id":"s3"}',
        '{"type":"agent_start"}',
        json.dumps({"type": "agent_end", "messages": [
            {"role": "user", "content": [{"type": "text", "text": "GO"}]},
            {"role": "assistant",
             "content": [{"type": "text", "text": "partial answer"}],
             "usage": _pi_usage()}]}),
    ])
    r = d.parse(dump, "")
    assert r.text == "partial answer"
    assert r.session == "s3"
    assert r.num_turns == 1  # min 1 when an assistant message exists


def test_pi_parse_error_turn_tolerates_empty_content_and_zero_cost():
    # pi json mode exits 0 even when every turn errored (verified live): the
    # assistant message carries stopReason=error, empty content, zeroed usage.
    d = PiDriver()
    err = {"role": "assistant", "content": [], "stopReason": "error",
           "errorMessage": "Request timed out.",
           "usage": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0,
                     "totalTokens": 0,
                     "cost": {"input": 0, "output": 0, "cacheRead": 0,
                              "cacheWrite": 0, "total": 0}}}
    dump = "\n".join([
        '{"type":"session","version":3,"id":"s4"}',
        json.dumps({"type": "message_end", "message": err}),
        json.dumps({"type": "turn_end", "message": err, "toolResults": []}),
        json.dumps({"type": "agent_end", "messages": [err], "willRetry": False}),
    ])
    r = d.parse(dump, "")
    assert r.session == "s4"
    assert r.cost_usd is None  # cost.total 0 means "not priced", not $0.00
    assert r.input_tokens is None and r.output_tokens is None


def test_omp_parse_with_thinking_blocks():
    d = OhMyPiDriver()
    assistant = {"role": "assistant",
                 "content": [
                     {"type": "thinking",
                      "thinking": "the flag is probably in /flag.txt"},
                     {"type": "text", "text": "FOUND_FLAG=flag{omp_think}"}],
                 "usage": _pi_usage(output=77)}
    dump = "\n".join([
        '{"type":"session","version":3,"id":"omp-sess-1"}',
        '{"type":"message_update","assistantMessageEvent":{"type":"thinking_delta","contentIndex":0,"delta":"the flag is"}}',
        '{"type":"message_update","assistantMessageEvent":{"type":"text_delta","contentIndex":1,"delta":"FOUND_"}}',
        json.dumps({"type": "message_end", "message": assistant}),
        json.dumps({"type": "turn_end", "message": assistant, "toolResults": []}),
        json.dumps({"type": "agent_end", "messages": [assistant]}),
        '{"type":"auto_retry_start","attempt":1}',  # omp extras are ignored
    ])
    r = d.parse(dump, "")
    # thinking blocks are reasoning, not answer text
    assert r.text == "FOUND_FLAG=flag{omp_think}"
    assert r.session == "omp-sess-1" and r.output_tokens == 77
    assert r.num_turns == 1


def test_pi_parse_stream_steps_shapes():
    d = PiDriver()
    # session header → session step
    s0 = d.parse_stream_steps('{"type":"session","version":3,"id":"s1"}')
    assert s0 and s0[0].kind == "session" and s0[0].session == "s1"
    # streaming deltas → reasoning (text and thinking)
    s1 = d.parse_stream_steps('{"type":"message_update","assistantMessageEvent":{"type":"text_delta","contentIndex":0,"delta":"reading main.c"}}')
    assert s1 and s1[0].kind == "reasoning" and "reading main.c" in s1[0].text
    s2 = d.parse_stream_steps('{"type":"message_update","assistantMessageEvent":{"type":"thinking_delta","contentIndex":0,"delta":"hmm"}}')
    assert s2 and s2[0].kind == "reasoning" and "hmm" in s2[0].text
    # assistant message_end → reasoning with the full text
    s3 = d.parse_stream_steps('{"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"done"}]}}')
    assert s3 and s3[0].kind == "reasoning" and s3[0].text == "done"
    # a USER message_end is NOT reasoning
    assert d.parse_stream_steps('{"type":"message_end","message":{"role":"user","content":[{"type":"text","text":"hi"}]}}') == []
    # tool_execution_start: bash command / read path surfaced for the deck
    s4 = d.parse_stream_steps('{"type":"tool_execution_start","toolCallId":"c1","toolName":"bash","args":{"command":"curl x"}}')
    assert (s4 and s4[0].kind == "tool" and s4[0].tool == "bash"
            and "curl x" in s4[0].text and s4[0].call_id == "c1")
    s5 = d.parse_stream_steps('{"type":"tool_execution_start","toolCallId":"c2","toolName":"read","args":{"path":"a.txt"}}')
    assert s5 and s5[0].kind == "tool" and s5[0].tool == "read" and "a.txt" in s5[0].text
    # tool_execution_end → tool_result; MCP-shaped result content is unwrapped,
    # text truncated for the deck but raw kept full for the provenance gate.
    s6 = d.parse_stream_steps('{"type":"tool_execution_end","toolCallId":"c1","toolName":"bash","result":{"content":[{"type":"text","text":"FOUND_FLAG=flag{tool_out}"}]},"isError":false}')
    assert (s6 and s6[0].kind == "tool_result"
            and "FOUND_FLAG=flag{tool_out}" in s6[0].raw
            and s6[0].call_id == "c1")
    # isError is honored in the serialized output
    s7 = d.parse_stream_steps('{"type":"tool_execution_end","toolCallId":"c3","toolName":"bash","result":{"content":[{"type":"text","text":"boom"}]},"isError":true}')
    assert s7 and s7[0].text.startswith("[error]") and "boom" in s7[0].raw
    # unknown/extra events and noise are ignored
    assert d.parse_stream_steps('{"type":"auto_retry_start","attempt":1}') == []
    assert d.parse_stream_steps('{"type":"turn_start"}') == []
    assert d.parse_stream_steps("not json") == []


def test_pi_hello_ok_requires_assistant_text():
    d = PiDriver()
    answer = '{"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"OK"}]}}'
    assert d._hello_ok(_CP(0, answer)) is True
    assert d._hello_ok(_CP(0, '{"type":"agent_end","messages":[]}')) is False
    assert d._hello_ok(_CP(0, '{"type":"message_end","message":{"role":"assistant","content":[]}}')) is False
    # post-turn noise flipping the exit code must not false-fail (codex leniency)
    assert d._hello_ok(_CP(1, answer)) is True
    assert d._hello_ok(_CP(0, "nothing useful")) is False
    # a user-only stream never proves a model turn
    assert d._hello_ok(_CP(0, '{"type":"message_end","message":{"role":"user","content":[]}}')) is False


def test_pi_hello_probe_runs_a_real_json_turn(monkeypatch):
    d = PiDriver()
    _ = d.bin  # resolve+cache the binary BEFORE we mock run (resolution probes too)
    seen = {}

    def fake_run(argv, **kw):
        seen["argv"] = argv
        return _CP(0, '{"type":"session","version":3,"id":"s"}\n'
                      '{"type":"message_end","message":{"role":"assistant",'
                      '"content":[{"type":"text","text":"OK"}]}}')

    monkeypatch.setattr("muteki.solver.cli_driver.subprocess.run", fake_run)
    ok, detail = d.health_detail()
    assert ok is True and detail == ""
    assert seen["argv"][:4] == [d.bin, "-p", "--mode", "json"]
    assert d.HELLO_PROMPT in seen["argv"]


def test_run_cli_merges_pi_env_extra_under_overlay(monkeypatch, tmp_path):
    from muteki.solver import cli_driver as mod
    seen = {}

    def fake_run(argv, **kwargs):
        seen["env"] = kwargs.get("env") or {}
        return subprocess.CompletedProcess(
            argv, 0, stdout='{"type":"agent_end","messages":[]}', stderr="")

    monkeypatch.setattr(mod.subprocess, "run", fake_run)
    d = PiDriver()
    d._bin = "/missing/test-pi"
    mod.run_cli(d, [d.bin, "-p", "--mode", "json", "GO"],
                cwd=str(tmp_path), timeout=5, env={"OPENAI_API_KEY": "fake"})
    assert seen["env"]["PI_OFFLINE"] == "1"
    assert seen["env"]["PI_SKIP_VERSION_CHECK"] == "1"
    assert seen["env"]["OPENAI_API_KEY"] == "fake"


def test_run_cli_env_extra_yields_to_explicit_overlay(monkeypatch, tmp_path):
    from muteki.solver import cli_driver as mod
    seen = {}

    def fake_run(argv, **kwargs):
        seen["env"] = kwargs.get("env") or {}
        return subprocess.CompletedProcess(argv, 0, stdout="", stderr="")

    monkeypatch.setattr(mod.subprocess, "run", fake_run)
    d = OhMyPiDriver()
    d._bin = "/missing/test-omp"
    mod.run_cli(d, [d.bin, "-p", "--mode", "json", "GO"],
                cwd=str(tmp_path), timeout=5, env={"OMP_SKIP_SETUP": "0"})
    assert seen["env"]["OMP_SKIP_SETUP"] == "0"  # explicit overlay wins



# ── KB access (optional user-scope MCP; off unless MUTEKI_KB_MCP_NAME is set) ──

def test_claude_kb_off_uses_strict_empty_mcp_config():
    # Offline mode excludes every inherited MCP server even when no single
    # MUTEKI_KB_MCP_NAME has been configured.
    d = ClaudeCodeDriver()
    assert d.KB_TOOL_PREFIX == ""  # no KB configured
    argv = d.build_execute("GO", d.new_session(), kb_access=False)
    assert "--strict-mcp-config" in argv
    config = argv[argv.index("--mcp-config") + 1]
    assert config == '{"mcpServers":{}}'


def test_claude_kb_inherited_when_configured():
    # With a KB configured (prefix set), a default run leaves it enabled
    # (never re-mounts — that would re-trigger the trust gate) and denies nothing.
    d = ClaudeCodeDriver()
    d.KB_TOOL_PREFIX = "mcp__my-kb"  # simulate MUTEKI_KB_MCP_NAME=my-kb
    argv = d.build_execute("GO", d.new_session())
    assert "--mcp-config" not in argv
    assert d.KB_TOOL_PREFIX not in argv  # KB left enabled


def test_claude_kb_off_denies_kb_tools_when_configured():
    d = ClaudeCodeDriver()
    d.KB_TOOL_PREFIX = "mcp__my-kb"  # simulate a configured KB
    argv = d.build_execute("GO", d.new_session(), kb_access=False)
    assert "--strict-mcp-config" in argv
    assert "--disallowed-tools" in argv
    assert d.KB_TOOL_PREFIX in argv  # the whole configured KB server is denied


def test_claude_offline_and_kb_off_share_one_deny_flag():
    # both suppressions collapse into a single --disallowed-tools list
    d = ClaudeCodeDriver()
    d.KB_TOOL_PREFIX = "mcp__my-kb"  # simulate a configured KB
    argv = d.build_execute("GO", d.new_session(), web_access=False, kb_access=False)
    assert argv.count("--disallowed-tools") == 1
    i = argv.index("--disallowed-tools")
    tail = argv[i + 1:argv.index("--")]
    assert "WebSearch" in tail and "WebFetch" in tail and d.KB_TOOL_PREFIX in tail


def test_registry():
    assert set(DRIVERS) == {
        "claude", "codex", "cursor", "pi", "omp", "kimi", "grok",
        "opencode", "dsh",
    }
    assert get_driver("claude").name == "claude"
    assert get_driver("cursor").name == "cursor"
    assert get_driver("pi").name == "pi"
    assert get_driver("omp").name == "omp"


def test_kill_proc_tree_kills_setsid_escaped_orphan_and_reaps():
    """A worker child that setsid()'s out of the process group must STILL be
    killed (killpg alone misses it → orphan leaks a slot/CPU/port), and the
    parent must be reaped (no <defunct> zombie). Regression for the live-only
    worker-process leak seen in the run-0011 transcript."""
    import os, signal, subprocess, sys, time
    # parent (own session/group) spawns a setsid'd child that writes its pid and sleeps.
    parent_src = (
        "import os, time, subprocess\n"
        "c = subprocess.Popen(['python3','-c',"
        "\"import os,time;open('%s','w').write(str(os.getpid()));time.sleep(120)\"],"
        " start_new_session=True)\n"
        "time.sleep(120)\n"
    )
    cpid_file = "/tmp/_kt_test_child_%d.pid" % os.getpid()
    try:
        os.unlink(cpid_file)
    except OSError:
        pass
    proc = subprocess.Popen([sys.executable, "-c", parent_src % cpid_file],
                            start_new_session=True)
    # wait for the child to register its pid
    cpid = None
    for _ in range(40):
        try:
            cpid = int(open(cpid_file).read())
            break
        except (OSError, ValueError):
            time.sleep(0.1)
    assert cpid is not None, "setsid child never started"
    assert os.getpgid(cpid) != os.getpgid(proc.pid), "child did not escape the group"

    _kill_proc_tree(proc)

    def _alive(pid):
        try:
            os.kill(pid, 0); return True
        except ProcessLookupError:
            return False
        except PermissionError:
            return True
    # the escaped orphan must be dead
    time.sleep(0.5)
    assert not _alive(cpid), "setsid-escaped orphan survived _kill_proc_tree"
    # the parent must be reaped (poll() returns a code, not None → not a zombie)
    assert proc.poll() is not None, "parent not reaped (zombie)"
    try:
        os.unlink(cpid_file)
    except OSError:
        pass


def test_driver_for_resolves_profile_id_to_base_engine():
    """A bare profile id string ("codex-sub-container") must resolve to its base
    engine driver, NOT KeyError on DRIVERS[id]. Regression: local runs crashed
    because the engine roster holds profile ids, and driver_for(<id-string>) used
    to index DRIVERS directly."""
    assert driver_for("codex-sub-container").name == "codex"
    assert driver_for("claude-sub-container").name == "claude"
    assert driver_for("cursor-api-container").name == "cursor"
    # base engine names and transports still resolve
    assert driver_for("codex").name == "codex"
    assert driver_for("codex_cli").name == "codex"
    # a profile DICT still resolves via its transport/engine
    assert driver_for({"id": "codex-sub-container", "engine": "codex",
                       "transport": "codex_cli"}).name == "codex"


def test_driver_for_local_profile_injects_selected_model(monkeypatch):
    """A subscription/local worker profile is the scheduling unit. Its selected
    model must be used by the health probe and worker argv; otherwise an exhausted
    default model (for example Opus) can falsely degrade Claude even when Sonnet is
    available."""
    monkeypatch.setenv("MUTEKI_CLAUDE_BIN", "/usr/bin/claude")
    drv = driver_for({
        "id": "claude-sub-container",
        "name": "claude-sub-container",
        "engine": "claude",
        "transport": "claude_code",
        "credential_mode": "subscription",
        "credential_account": "",
        "runtime": "local",
        "model": "sonnet",
    })

    hello = drv._hello_argv()
    execute = drv.build_execute("PROMPT", drv.new_session())

    assert hello[hello.index("--model") + 1] == "sonnet"
    assert execute[execute.index("--model") + 1] == "sonnet"


def test_get_driver_unknown_name_raises_clear_error():
    """An unresolvable engine name gives an actionable ValueError, not a bare
    KeyError, so the failure points at the profile-id-vs-base-engine confusion."""
    import pytest
    with pytest.raises(ValueError, match="unknown engine"):
        get_driver("totally-not-an-engine")


def test_driver_for_codex_endpoint_injects_provider_before_exec(monkeypatch):
    monkeypatch.setenv("MUTEKI_CODEX_BIN", "/usr/bin/codex")
    drv = driver_for({
        "name": "deepseek-codex",
        "engine": "codex",
        "transport": "codex_cli",
        "credential_mode": "api",
        "base_url": "https://api.deepseek.example/v1",
        "wire_api": "responses",
        "model": "deepseek-chat",
    })

    argv = drv.build_execute("PROMPT", None, web_access=False)

    exec_idx = argv.index("exec")
    # `name` and `env_key` are mandatory: without `name` codex aborts config load
    # ("provider name must not be empty") so the endpoint never works; `env_key`
    # pins the bearer-token env var the Credential Account injection populates.
    assert argv[1:exec_idx] == [
        "-c", "model_provider=muteki",
        "-c", "model_providers.muteki.name=muteki",
        "-c", "model_providers.muteki.base_url=https://api.deepseek.example/v1",
        "-c", "model_providers.muteki.wire_api=responses",
        "-c", "model_providers.muteki.env_key=OPENAI_API_KEY",
        "-c", "model=deepseek-chat",
    ]
    assert argv[exec_idx] == "exec"
    assert argv.index("--json") > exec_idx
    assert "--disable" not in argv[exec_idx:]
    assert 'web_search="disabled"' not in argv[exec_idx:]

    isolated = drv.build_execute("PROMPT", None, web_access=False, kb_access=False)
    isolated_exec_idx = isolated.index("exec")
    assert "--ignore-user-config" in isolated[isolated_exec_idx:]
    assert "--disable" not in isolated[isolated_exec_idx:]


def test_codex_endpoint_health_uses_real_cli_turn(monkeypatch):
    monkeypatch.setenv("MUTEKI_CODEX_BIN", "/usr/bin/codex")
    drv = driver_for({
        "name": "deepseek-codex",
        "engine": "codex",
        "transport": "codex_cli",
        "credential_mode": "api",
        "base_url": "https://api.deepseek.example/v1",
        "wire_api": "responses",
        "model": "deepseek-chat",
    })
    seen = {}

    def fake_run(argv, **kw):
        seen["argv"] = argv
        return _CP(
            1,
            '{"type":"turn.failed","error":{"message":"tools[10].type: unknown variant `namespace`"}}',
            "",
        )

    monkeypatch.setattr("muteki.solver.cli_driver.subprocess.run", fake_run)
    monkeypatch.setattr("muteki.solver.cli_driver.time.sleep", lambda *_: None)
    ok, detail = drv.health_detail(env={"OPENAI_API_KEY": "secret"})

    assert ok is False
    exec_idx = seen["argv"].index("exec")
    assert "model_provider=muteki" in seen["argv"][:exec_idx]
    assert "model_providers.muteki.base_url=https://api.deepseek.example/v1" in seen["argv"][:exec_idx]
    assert "--ignore-user-config" in seen["argv"][exec_idx:]
    assert "--disable" not in seen["argv"][exec_idx:]
    assert "namespace" in detail


def test_claude_endpoint_health_never_places_api_key_in_subprocess_argv(monkeypatch):
    secret = "sk-health-argv-secret"
    drv = driver_for({
        "name": "deepseek-claude",
        "engine": "claude",
        "transport": "claude_cli",
        "credential_mode": "api",
        "base_url": "https://api.deepseek.example/anthropic",
        "api_key_ref": "env:DEEPSEEK_API_KEY",
        "model": "deepseek-chat",
    })
    seen = {}

    def fake_subprocess(argv, **kwargs):
        assert secret not in "\0".join(str(arg) for arg in argv)
        seen["argv"] = argv
        seen["env"] = kwargs.get("env") or {}
        return _CP(0, '{"result":"OK"}', "")

    monkeypatch.setattr(
        "muteki.solver.cli_driver.subprocess.run", fake_subprocess)
    ok, detail = drv.health_detail(env={"DEEPSEEK_API_KEY": secret})

    assert ok is True and detail == ""
    assert seen["env"]["ANTHROPIC_API_KEY"] == secret
    assert seen["env"]["ANTHROPIC_BASE_URL"] == (
        "https://api.deepseek.example/anthropic")
    assert any("Reply with exactly: OK" in str(arg) for arg in seen["argv"])


def test_endpoint_health_error_never_echoes_secret_response_body(monkeypatch):
    secret = "sk-reflected-health-secret"
    drv = driver_for({
        "name": "claude-api", "engine": "claude",
        "transport": "claude_cli", "credential_mode": "api",
        "base_url": "https://proxy.invalid",
        "api_key_ref": "env:PROXY_KEY",
    })

    monkeypatch.setattr(
        "muteki.solver.cli_driver.subprocess.run",
        lambda argv, **_kwargs: _CP(
            1, "", f"HTTP 401 mirrored x-api-key: {secret}"))
    monkeypatch.setattr("muteki.solver.cli_driver.time.sleep", lambda *_: None)
    ok, detail = drv.health_detail(env={"PROXY_KEY": secret})

    assert ok is False
    assert "HTTP 401" in detail
    assert secret not in detail


def test_driver_for_codex_keyed_profile_without_endpoint_still_injects_model(monkeypatch):
    monkeypatch.setenv("MUTEKI_CODEX_BIN", "/usr/bin/codex")
    drv = driver_for({
        "name": "codex-main",
        "engine": "codex",
        "transport": "codex_cli",
        "credential_mode": "api_key",
        "model": "gpt-5.4",
    })

    argv = drv.build_execute("PROMPT", None, web_access=False)

    assert not any("model_provider=" in arg for arg in argv)
    assert 'web_search="disabled"' in argv
    assert "--model" in argv
    assert argv[argv.index("--model") + 1] == "gpt-5.4"


def test_driver_for_claude_endpoint_healthcheck_runs_real_cli(monkeypatch):
    seen = {}

    def fake_run(argv, **kwargs):
        seen["argv"] = argv
        seen["env"] = kwargs.get("env") or {}
        return _CP(0, '{"result":"OK"}', "")

    monkeypatch.setattr(subprocess, "run", fake_run)
    monkeypatch.setenv("CLAUDE_ENDPOINT_TOKEN", "secret")
    drv = driver_for({
        "name": "claude-api",
        "engine": "claude",
        "transport": "claude_code",
        "credential_mode": "api",
        "base_url": "https://anthropic-proxy.example",
        "api_key_ref": "env:CLAUDE_ENDPOINT_TOKEN",
    })

    assert drv.healthcheck() is True
    assert any("Reply with exactly: OK" in str(arg) for arg in seen["argv"])
    assert seen["env"]["ANTHROPIC_BASE_URL"] == "https://anthropic-proxy.example"
    assert seen["env"]["ANTHROPIC_API_KEY"] == "secret"


def test_endpoint_healthcheck_resolves_file_backed_key(monkeypatch, tmp_path):
    """#5: a FILE-backed Credential Account (api_key_ref empty, secret in a file)
    must still get an auth header on the health probe. The old _api_key() only
    handled env: refs and returned '' for file-backed → probe sent no auth header
    (false-negative health even though the live worker authenticates fine)."""
    seen = {}

    def fake_run(argv, **kwargs):
        seen["argv"] = argv
        seen["env"] = kwargs.get("env") or {}
        return subprocess.CompletedProcess(
            argv, 0,
            '{"type":"turn.completed","usage":{}}\n',
            "",
        )
    monkeypatch.setattr(subprocess, "run", fake_run)

    # (a) explicit file: ref
    keyfile = tmp_path / "API_KEY"
    keyfile.write_text("file-secret-123\n")
    drv = driver_for({
        "name": "deepseek-codex-api", "engine": "codex", "transport": "codex_cli",
        "credential_mode": "api", "base_url": "https://ds.example",
        "api_key_ref": f"file:{keyfile}",
    })
    assert drv.healthcheck() is True
    assert seen["env"]["OPENAI_API_KEY"] == "file-secret-123", \
        "#5: file: api_key_ref must be read and injected for the Codex CLI"

    # (b) no ref, but the credential-injection *_API_KEY_FILE env is set (the
    # container path) → still resolved.
    seen.clear()
    monkeypatch.setenv("OPENAI_API_KEY_FILE", str(keyfile))
    drv2 = driver_for({
        "name": "ds2", "engine": "codex", "transport": "codex_cli",
        "credential_mode": "api", "base_url": "https://ds.example",
    })
    assert drv2.healthcheck() is True
    assert seen["env"]["OPENAI_API_KEY"] == "file-secret-123", \
        "#5: *_API_KEY_FILE env fallback must be read for the Codex CLI probe"


# ── engine binary resolution (pin official, skip broken third-party) ──────────
# A broken `@cometix/claude-code` repackage earlier on PATH crashes at load and
# would silently degrade the swarm; the resolver must skip it and pin a runnable
# official binary. These tests drive the resolver with fakes so they don't depend
# on what's actually installed on the host.

def test_resolve_prefers_env_override(monkeypatch):
    from muteki.solver import cli_driver as mod
    monkeypatch.setenv("MUTEKI_CLAUDE_BIN", "/custom/path/claude")
    # env override wins outright — no PATH scan, no run probe
    assert mod.resolve_engine_bin("claude") == "/custom/path/claude"


def test_resolve_skips_known_bad_repackage(monkeypatch):
    from muteki.solver import cli_driver as mod
    monkeypatch.delenv("MUTEKI_CLAUDE_BIN", raising=False)
    # no known-good location exists in this fake world
    monkeypatch.setattr(mod, "_KNOWN_GOOD", {"claude": []})
    # PATH has the broken cometix build first, then a good one
    bad = "/n/node_modules/@cometix/claude-code/cli.js"
    good = "/opt/official/claude"
    monkeypatch.setattr(mod, "_which_all", lambda name: [bad, good])
    # cometix realpath looks bad; the good one runs
    monkeypatch.setattr(mod, "_runs_ok", lambda p: p == good)
    assert mod.resolve_engine_bin("claude") == good


def test_resolve_known_good_location_wins_over_path(monkeypatch):
    from muteki.solver import cli_driver as mod
    monkeypatch.delenv("MUTEKI_CLAUDE_BIN", raising=False)
    good = "/blessed/claude"
    monkeypatch.setattr(mod, "_KNOWN_GOOD", {"claude": [good]})
    monkeypatch.setattr(mod.Path, "exists", lambda self: str(self) == good)
    monkeypatch.setattr(mod, "_looks_bad", lambda p: False)
    monkeypatch.setattr(mod, "_runs_ok", lambda p: True)
    # PATH scan would return something else, but the known-good location is checked first
    monkeypatch.setattr(mod, "_which_all", lambda name: ["/somewhere/else/claude"])
    assert mod.resolve_engine_bin("claude") == good


def test_resolve_falls_back_to_bare_name_when_all_broken(monkeypatch):
    from muteki.solver import cli_driver as mod
    monkeypatch.delenv("MUTEKI_CLAUDE_BIN", raising=False)
    monkeypatch.setattr(mod, "_KNOWN_GOOD", {"claude": []})
    monkeypatch.setattr(mod, "_which_all", lambda name: ["/bad/claude"])
    monkeypatch.setattr(mod, "_runs_ok", lambda p: False)  # nothing runs
    # last resort: the bare name (preserves old behavior, no worse than before)
    assert mod.resolve_engine_bin("claude") == "claude"


def test_looks_bad_flags_cometix():
    from muteki.solver.cli_driver import _looks_bad
    assert _looks_bad("/x/node_modules/@cometix/claude-code/cli.js") is True
    assert _looks_bad("/opt/homebrew/bin/claude") is False


def test_driver_bin_is_cached(monkeypatch):
    from muteki.solver import cli_driver as mod
    calls = []
    monkeypatch.setattr(mod, "resolve_engine_bin",
                        lambda name: calls.append(name) or f"/resolved/{name}")
    d = ClaudeCodeDriver()
    d._bin = None  # ensure a clean resolve
    assert d.bin == "/resolved/claude"
    assert d.bin == "/resolved/claude"  # second access
    assert calls == ["claude"]  # resolved exactly once, then cached


# ── output parsing ───────────────────────────────────────────────────────────

def test_claude_parse_json():
    d = ClaudeCodeDriver()
    out = ('{"result":"FOUND_FLAG=flag{x}","session_id":"s1",'
           '"total_cost_usd":1.23,"num_turns":7,'
           '"usage":{"input_tokens":4,"cache_read_input_tokens":1200,'
           '"cache_creation_input_tokens":300,"output_tokens":5}}')
    r = d.parse(out, "")
    assert r.text == "FOUND_FLAG=flag{x}"
    assert r.session == "s1" and r.cost_usd == 1.23 and r.num_turns == 7
    # tokens captured for the deck's usage column: fresh + both cache buckets
    assert r.input_tokens == 4 + 1200 + 300 and r.output_tokens == 5


def test_codex_parse_real_0133_jsonl():
    # codex 0.133.0 wraps the assistant message in item.completed → item.agent_message,
    # and the session id arrives as thread.started.thread_id (not in stderr).
    d = CodexDriver()
    out = "\n".join([
        '{"type":"thread.started","thread_id":"th-abc"}',
        '{"type":"turn.started"}',
        '{"type":"item.completed","item":{"id":"i0","type":"agent_message","text":"FOUND_FLAG=flag{codex}"}}',
        '{"type":"turn.completed","usage":{"input_tokens":10,"output_tokens":5}}',
    ])
    r = d.parse(out, "")
    assert "FOUND_FLAG=flag{codex}" in r.text
    assert r.session == "th-abc"   # captured from thread.started
    assert r.num_turns == 1


def test_codex_subscription_derives_cost_from_tokens():
    # Subscription codex (0.137) reports NO total_cost_usd — only per-turn token
    # usage. The driver must re-derive an API-equivalent dollar cost so codex
    # workers stop contributing $0 to the deck's cost figure.
    from muteki.core.cost import PRICES, CODEX_CACHED_INPUT_PER_M
    d = CodexDriver()
    out = "\n".join([
        '{"type":"thread.started","thread_id":"th-x"}',
        '{"type":"item.completed","item":{"type":"agent_message","text":"OK"}}',
        '{"type":"turn.completed","usage":{"input_tokens":26910,'
        '"cached_input_tokens":3456,"output_tokens":24,"reasoning_output_tokens":17}}',
    ])
    r = d.parse(out, "")
    assert r.input_tokens == 26910
    assert r.output_tokens == 24 + 17          # reasoning folded into output
    p = PRICES["codex"]
    fresh_in = 26910 - 3456
    expected = (fresh_in / 1_000_000 * p.input_per_m
                + 3456 / 1_000_000 * CODEX_CACHED_INPUT_PER_M
                + (24 + 17) / 1_000_000 * p.output_per_m)
    assert r.cost_usd == pytest.approx(expected)
    assert r.cost_usd > 0    # the whole point: codex is no longer free


def test_codex_legacy_total_cost_usd_still_wins():
    # When an (older) codex DOES report total_cost_usd, that authoritative dollar
    # figure must win over the token-derived estimate.
    d = CodexDriver()
    out = '{"msg":{"type":"agent_message","message":"hi","total_cost_usd":0.5}}'
    r = d.parse(out, "")
    assert r.cost_usd == 0.5


def test_claude_killed_loser_recovers_tokens_from_assistant_event():
    # A race-loser claude worker is killpg'd mid-run → it NEVER emits the final
    # {type:result}. Each assistant event carries a cumulative usage block, so the
    # parser must fall back to the LAST one — otherwise the loser's real spend
    # (it ran many tool turns) silently vanishes from the ledger.
    d = ClaudeCodeDriver()
    out = "\n".join([
        '{"type":"system","subtype":"init","session_id":"s-killed"}',
        '{"type":"assistant","message":{"usage":{"input_tokens":4646,"cache_creation_input_tokens":26944,"cache_read_input_tokens":0,"output_tokens":1}}}',
        '{"type":"assistant","message":{"usage":{"input_tokens":5000,"cache_read_input_tokens":30000,"cache_creation_input_tokens":0,"output_tokens":820}}}',
        # killpg here — no {"type":"result"}
    ])
    r = d.parse(out, "")
    assert r.session == "s-killed"
    # latest assistant usage wins: 5000 + 30000 cache_read + 0 cache_creation = 35000
    assert r.input_tokens == 35000 and r.output_tokens == 820


def test_claude_final_result_wins_over_intermediate_usage():
    # When the run completes normally, the final result's usage is authoritative —
    # the intermediate-assistant fallback must NOT override it.
    d = ClaudeCodeDriver()
    out = "\n".join([
        '{"type":"assistant","message":{"usage":{"input_tokens":100,"output_tokens":5}}}',
        '{"type":"result","result":"OK","session_id":"s1","total_cost_usd":0.19,"usage":{"input_tokens":999,"cache_read_input_tokens":1,"output_tokens":50}}',
    ])
    r = d.parse(out, "")
    assert r.input_tokens == 1000 and r.output_tokens == 50 and r.cost_usd == 0.19


def test_codex_killed_preserves_completed_turns():
    # A codex worker killed mid-3rd-turn keeps the usage of the 2 turns that DID
    # complete (each turn.completed is its own line, already summed). Only the
    # in-progress turn is lost (unavoidable — codex reports usage per finished turn).
    d = CodexDriver()
    out = "\n".join([
        '{"type":"thread.started","thread_id":"t1"}',
        '{"type":"turn.completed","usage":{"input_tokens":1000,"cached_input_tokens":0,"output_tokens":100}}',
        '{"type":"turn.completed","usage":{"input_tokens":2000,"cached_input_tokens":500,"output_tokens":200}}',
        # killed mid 3rd turn — no more turn.completed
    ])
    r = d.parse(out, "")
    assert r.input_tokens == 3000 and r.output_tokens == 300 and r.cost_usd > 0


def test_claude_parse_non_json_tolerant():
    d = ClaudeCodeDriver()
    r = d.parse("not json at all", "stderr tail")
    assert "not json" in r.text  # falls back to raw text, never crashes


# ── flag extraction + provenance gate (the moat is preserved) ────────────────

def _cli_solver(challenge, **kw):
    spec = type("S", (), {"solver_id": "cli-1"})()
    return CliSolver(spec, challenge, **kw)


def test_cli_solver_offline_flag_threads_through():
    # web_access=False on the solver → the built execute argv denies web tools.
    ch = Challenge(id="t", name="t", category="web", flag_format=r"flag\{.*?\}",
                   target="http://x")
    s = _cli_solver(ch, web_access=False, kb=False)
    assert s.web_access is False
    argv = s.driver.build_execute(s._build_prompt(), s.driver.new_session(),
                                  web_access=s.web_access, kb_access=s.kb)
    assert "--disallowed-tools" in argv


def test_protocol2_canary_prompt_is_neutral_and_has_no_legacy_team_protocol():
    ch = Challenge(
        id="neutral", name="neutral fixture", category="misc",
        description="Read fixture.txt and return its token.",
        flag_format=r"flag\{[a-z0-9_]+\}")
    solver = _cli_solver(ch, web_access=False, kb=False)
    solver._protocol2_mode = True
    solver._staged_files = ["fixture.txt"]
    prompt = solver._build_prompt()
    assert "neutral local runtime conformance check" in prompt
    assert "expert CTF solver" not in prompt
    assert "Share findings with your team" not in prompt
    assert "blackboard skill" not in prompt
    assert "submit-flag '<token>'" in prompt


def test_cli_solver_kb_off_by_default_when_no_kb_configured():
    # Out of the box (no MUTEKI_KB_MCP_NAME) the KB is inert regardless of kb=...:
    # self.kb is False and the prompt teaches no KB tool.
    ch = Challenge(id="t", name="t", category="web", flag_format=r"flag\{.*?\}")
    s = _cli_solver(ch, kb=True, engine="claude")
    assert s.kb is False  # no KB configured → off even though kb=True was requested
    assert "knowledge-base tool" not in s._build_prompt()


def test_cli_solver_kb_disabled_keeps_prompt_clean():
    ch = Challenge(id="t", name="t", category="web", flag_format=r"flag\{.*?\}")
    s = _cli_solver(ch, kb=False)
    assert s.kb is False
    assert "knowledge-base tool" not in s._build_prompt()


def test_cli_solver_kb_on_for_claude_teaches_dispatch_when_configured(monkeypatch):
    # With a KB configured (MUTEKI_KB_MCP_NAME) the claude worker inherits it →
    # kb stays on and the prompt teaches dispatch, naming the configured server.
    import muteki.solver.cli_solver as cs
    monkeypatch.setattr(cs, "KB_MCP_NAME", "my-kb")
    monkeypatch.setattr(cs, "_KB_PROMPT",
                        "\nYou ALSO have a `my-kb` knowledge-base tool ...\n")
    ch = Challenge(id="t", name="t", category="web", flag_format=r"flag\{.*?\}")
    s = _cli_solver(ch, kb=True, engine="claude")
    assert s.kb is True
    assert "my-kb" in s._build_prompt()


def test_cli_solver_kb_off_for_codex():
    # codex doesn't inherit claude's user-scope KB → kb forced off even if asked.
    ch = Challenge(id="t", name="t", category="web", flag_format=r"flag\{.*?\}")
    s = _cli_solver(ch, kb=True, engine="codex")
    assert s.kb is False
    assert "knowledge-base tool" not in s._build_prompt()


def test_extract_flag_prefers_found_flag_marker():
    ch = Challenge(id="t", name="t", category="web", flag_format=r"flag\{.*?\}")
    s = _cli_solver(ch)
    text = "lots of noise\nFOUND_FLAG=flag{real_one}\nmore noise"
    assert s._extract_flag(text) == "flag{real_one}"


def test_extract_flag_ignores_none_marker():
    ch = Challenge(id="t", name="t", category="web", flag_format=r"flag\{.*?\}")
    s = _cli_solver(ch)
    assert s._extract_flag("FOUND_FLAG=NONE\nno flag here") is None


def test_extract_flag_does_NOT_blind_scan_prose():
    # POLICY CHANGE (run-4305): we no longer blind-scan the transcript for a
    # flag_format-shaped token. A flag mentioned only in prose, with no FOUND_FLAG=
    # marker, is NOT a claim — extracting it was the source of every false positive
    # (run-1619/run-3613/run-4305). The flag is the worker's explicit claim only.
    ch = Challenge(id="t", name="t", category="web", flag_format=r"flag\{.*?\}")
    s = _cli_solver(ch)
    assert s._extract_flag("the answer is flag{from_body} somewhere") is None
    # only an explicit marker counts:
    assert s._extract_flag("FOUND_FLAG=flag{from_body}") == "flag{from_body}"


def test_extract_flag_ignores_prose_sentence_run_4305():
    # the exact run-4305 shape: a reasoning sentence whose greedy {…} the old scan
    # scooped up as a flag. No marker → None.
    ch = Challenge(id="t", name="t", category="crypto",
                   flag_format=r"[A-Za-z0-9_]{0,15}\{[^}]{1,200}\}")
    s = _cli_solver(ch)
    prose = ("The output starts with `flag{`, which means the plaintext may be "
             "further transformed after Avali Scratch, not just `flag{...}` directly.")
    assert s._extract_flag(prose) is None


def test_gate_rejects_flag_not_in_output():
    # provenance: a flag the worker only NAMES but isn't in its real output fails.
    ch = Challenge(id="t", name="t", category="web", flag_format=r"flag\{.*?\}")
    s = _cli_solver(ch)
    assert s._flag_ok("flag{hallucinated}", "the worker rambled, no flag in output") is False
    assert s._flag_ok("flag{seen}", "stdout had flag{seen} in it") is True


def test_launder_still_rejects_grep_from_disk_steal():
    """#3: the run-11551 grep-from-disk launder MUST stay rejected. The worker reads
    another run's persisted log/winner and restates the flag as its own — a READ action
    next to an internal-storage filename, scanned over the whole output (the restate can
    be arbitrarily far from the read)."""
    ch = Challenge(id="t", name="t", category="web", flag_format=r"flag\{.*?\}")
    s = _cli_solver(ch)
    steal = ("$ grep -r flag /workspace/eval_runs/run-11550/winner.json\n"
             "...much later...\n"
             "I recovered the flag: flag{stolen}\n")
    assert s._flag_ok("flag{stolen}", steal) is False
    # the engine's own history dir is an unambiguous tell on its own (no read verb needed)
    assert s._flag_ok("flag{x}", "found it in ~/.codex/sessions/abc flag{x}") is False
    # a sibling-process harvest phrasing
    assert s._flag_ok("flag{y}", "pulled flag{y} from a teammate process title") is False


def test_launder_does_not_reject_genuine_flag_mentioning_path():
    """#3: a GENUINE flag whose transcript merely MENTIONS an internal-storage path
    (a target that serves /winner.json, a forensics blob containing the string) must
    NOT be rejected. The old context-free match false-rejected these (Rivulet-class:
    real flags refused, operator stuck at a false 1/4)."""
    ch = Challenge(id="t", name="t", category="web", flag_format=r"flag\{.*?\}")
    s = _cli_solver(ch)
    # target serves a path literally named winner.json — no read action by the worker
    served = ("GET /winner.json HTTP/1.1 -> 200\n"
              "the response body contained flag{real_recovered}\n")
    assert s._flag_ok("flag{real_recovered}", served) is True
    # a forensics challenge whose artifact string mentions shared_graph.db, flag found
    forensic = ("strings dump mentions a file named shared_graph.db in the pcap\n"
                "but the actual flag decoded from the payload is flag{from_pcap}\n")
    # Within one execution, the conservative read+internal-name combination remains a
    # rejection. The record-boundary regression below only prevents unrelated tools
    # from contributing one half of that signature each.
    assert s._flag_ok("flag{from_pcap}", forensic) is False


def test_launder_signals_do_not_cross_tool_record_boundaries_round15():
    """Round-15 F: a board read in one tool must not poison a later real extraction.

    The old flattened corpus joined ``read`` from one output with
    ``shared_graph.db`` from another and rejected a CRC-derived flag that appeared in
    genuine command output. Each ambiguous signature now has to occur within the same
    command+output execution record.
    """
    ch = Challenge(id="t", name="t", category="forensics",
                   flag_format=r"flag\{.*?\}")
    s = _cli_solver(ch)
    s._persist_raw_tool_output(
        "Loaded teammate notes for the next step.\n",
        command="python /usr/local/bin/blackboard.py read")
    s._persist_raw_tool_output(
        "Workspace graph is /workspace/graph/shared_graph.db\n",
        command="pwd")
    recovered = "Flag: flag{crc_recovered}\nFOUND_FLAG=flag{crc_recovered}\n"
    s._persist_raw_tool_output(recovered, command="python decode_crc.py Evidence.zip")

    assert s._flag_ok("flag{crc_recovered}", s._provenance_corpus()) is True


def test_launder_command_and_output_share_one_tool_record():
    """A real steal stays blocked when the read verb is in the command and the
    internal-storage filename is printed in that same execution's output."""
    ch = Challenge(id="t", name="t", category="forensics",
                   flag_format=r"flag\{.*?\}")
    s = _cli_solver(ch)
    output = ("opened /workspace/graph/shared_graph.db\n"
              "FOUND_FLAG=flag{stolen}\n")
    s._persist_raw_tool_output(output, command="python helper.py read")

    assert s._flag_ok("flag{stolen}", s._provenance_corpus()) is False


def test_launder_unrelated_record_does_not_poison_flag_evidence():
    ch = Challenge(id="t", name="t", category="forensics",
                   flag_format=r"flag\{.*?\}")
    s = _cli_solver(ch)
    s._persist_raw_tool_output(
        "opened /workspace/graph/shared_graph.db\n",
        command="python helper.py read")
    recovered = "FOUND_FLAG=flag{clean_later}\n"
    s._persist_raw_tool_output(recovered, command="python decode.py")

    assert s._flag_ok("flag{clean_later}", s._provenance_corpus()) is True


def test_live_duplicate_output_uses_newest_execution_identity():
    ch = Challenge(id="t", name="t", category="forensics",
                   flag_format=r"flag\{.*?\}")
    s = _cli_solver(ch)
    duplicated = "FOUND_FLAG=flag{same_output}\n"
    s._persist_raw_tool_output(
        duplicated, command="python helper.py read shared_graph.db")
    s._persist_raw_tool_output(duplicated, command="python decode.py Evidence.zip")

    assert s._flag_ok("flag{same_output}", duplicated) is True


def test_emit_step_pairs_interleaved_tool_results_by_call_id():
    from muteki.solver.cli_driver import StreamStep

    ch = Challenge(id="t", name="t", category="forensics",
                   flag_format=r"flag\{.*?\}")
    s = _cli_solver(ch)
    asyncio.run(s._emit_step(StreamStep(
        "tool", text="python helper.py read shared_graph.db", call_id="tainted")))
    asyncio.run(s._emit_step(StreamStep(
        "tool", text="python decode.py Evidence.zip", call_id="clean")))
    real = "FOUND_FLAG=flag{interleaved_clean}\n"
    asyncio.run(s._emit_step(StreamStep(
        "tool_result", text=real, raw=real, call_id="clean")))
    asyncio.run(s._emit_step(StreamStep(
        "tool_result", text="no candidate\n", raw="no candidate\n",
        call_id="tainted")))

    _submit_captured_flags(s)
    assert s._already_found == {"flag{interleaved_clean}"}
    assert s._raw_tool_commands == [
        "python decode.py Evidence.zip",
        "python helper.py read shared_graph.db",
    ]


def test_emit_step_mixed_call_ids_fall_back_to_oldest_pending_tool():
    from muteki.solver.cli_driver import StreamStep

    ch = Challenge(id="t", name="t", category="forensics",
                   flag_format=r"flag\{.*?\}")
    s = _cli_solver(ch)
    asyncio.run(s._emit_step(StreamStep(
        "tool", text="python helper.py read shared_graph.db", call_id="started-id")))
    claimed = "FOUND_FLAG=flag{mixed_id_stolen}\n"
    asyncio.run(s._emit_step(StreamStep(
        "tool_result", text=claimed, raw=claimed)))

    assert s._already_found == set()
    assert s._raw_tool_commands == ["python helper.py read shared_graph.db"]


def test_emit_step_empty_result_consumes_pending_tool_call():
    from muteki.solver.cli_driver import StreamStep

    ch = Challenge(id="t", name="t", category="forensics",
                   flag_format=r"flag\{.*?\}")
    s = _cli_solver(ch)
    asyncio.run(s._emit_step(StreamStep(
        "tool", text="python helper.py read shared_graph.db", call_id="empty")))
    asyncio.run(s._emit_step(StreamStep("tool_result", call_id="empty")))
    asyncio.run(s._emit_step(StreamStep(
        "tool", text="python decode.py Evidence.zip")))
    recovered = "FOUND_FLAG=flag{after_empty_result}\n"
    asyncio.run(s._emit_step(StreamStep(
        "tool_result", text=recovered, raw=recovered)))

    _submit_captured_flags(s)
    assert s._already_found == {"flag{after_empty_result}"}
    assert s._raw_tool_commands == ["python decode.py Evidence.zip"]


def test_emit_step_unknown_result_id_is_unattributed_and_fail_closed():
    from muteki.solver.cli_driver import StreamStep

    ch = Challenge(id="t", name="t", category="forensics",
                   flag_format=r"flag\{.*?\}")
    bus = _CaptureBus()
    s = _cli_solver(ch, bus=bus)
    stale_command = "python helper.py read shared_graph.db"
    asyncio.run(s._emit_step(StreamStep(
        "tool", text=stale_command, call_id="stale")))
    recovered = "FOUND_FLAG=flag{clean_unknown_id}\n"
    asyncio.run(s._emit_step(StreamStep(
        "tool_result", text=recovered, raw=recovered, call_id="different")))

    assert s._already_found == set()
    assert s._raw_tool_outputs == [recovered]
    assert s._raw_tool_commands == [""]
    assert s._raw_tool_attributed == [False]
    assert s._provenance_corpus() == ""
    assert s._flag_ok("flag{clean_unknown_id}", recovered) is False
    assert s._pending_tool_calls == [("stale", stale_command)]
    unverified = [
        event for event in bus.events
        if (event.event_type is EventType.BLACKBOARD_DELTA
            and event.payload.get("kind") == "flag_unverified")
    ]
    assert len(unverified) == 1
    assert "unattributed" in unverified[0].payload["reason"]


def test_emit_step_unknown_result_keeps_pending_for_later_exact_match():
    from muteki.solver.cli_driver import StreamStep

    ch = Challenge(id="t", name="t", category="forensics",
                   flag_format=r"flag\{.*?\}")
    s = _cli_solver(ch)
    command = "python decode.py Evidence.zip"
    asyncio.run(s._emit_step(StreamStep(
        "tool", text=command, call_id="known")))
    unknown = "FOUND_FLAG=flag{unknown_rejected}\n"
    asyncio.run(s._emit_step(StreamStep(
        "tool_result", text=unknown, raw=unknown, call_id="other")))
    known = "FOUND_FLAG=flag{known_accepted}\n"
    asyncio.run(s._emit_step(StreamStep(
        "tool_result", text=known, raw=known, call_id="known")))

    _submit_captured_flags(s)
    assert s._already_found == {"flag{known_accepted}"}
    assert s._raw_tool_outputs == [unknown, known]
    assert s._raw_tool_commands == ["", command]
    assert s._raw_tool_attributed == [False, True]
    assert s._pending_tool_calls == []
    assert "flag{unknown_rejected}" not in s._provenance_corpus()
    assert "flag{known_accepted}" in s._provenance_corpus()


def test_run_streaming_serializes_attribution_across_slow_sink(monkeypatch, tmp_path):
    from muteki.core.event_bus import EventBus
    from muteki.solver import cli_solver as mod
    from muteki.solver.cli_driver import CliResult, StreamStep

    bus = EventBus()

    async def slow_sink(event):
        if event.event_type is EventType.TOOL_CALL_START:
            await asyncio.sleep(0.02)

    bus.add_sink(slow_sink)
    ch = Challenge(id="t", name="t", category="forensics",
                   flag_format=r"flag\{.*?\}")
    s = _cli_solver(ch, bus=bus)

    def fake_stream(*args, on_step, **kwargs):
        on_step(StreamStep(
            "tool", text="python helper.py read shared_graph.db", call_id="old"))
        on_step(StreamStep("tool_result", call_id="old"))
        on_step(StreamStep("tool", text="python decode.py Evidence.zip"))
        on_step(StreamStep(
            "tool_result", text="decoded\n", raw="decoded\n"))
        return CliResult(text="decoded\n")

    monkeypatch.setattr(mod, "run_cli_streaming", fake_stream)
    asyncio.run(s._run_streaming(
        ["true"], cwd=str(tmp_path), timeout=5, argv_is_final=True))

    assert s._raw_tool_outputs == ["decoded\n"]
    assert s._raw_tool_commands == ["python decode.py Evidence.zip"]
    assert s._raw_tool_attributed == [True]
    assert s._pending_tool_calls == []


def test_protocol2_capture_failure_keeps_local_provenance_and_call_pending():
    ch = Challenge(id="t", name="t", category="forensics",
                   flag_format=r"flag\{.*?\}")
    s = _cli_solver(ch)
    s._protocol2_mode = True
    s._protocol2_capture_callback = lambda _raw: (_ for _ in ()).throw(
        RuntimeError("capture failed"))

    async def drive():
        await s._emit_step(StreamStep(
            "tool", text="python solve.py", call_id="p2-call"))
        with pytest.raises(RuntimeError, match="capture failed"):
            await s._emit_step(StreamStep(
                "tool_result", text="FOUND_FLAG=flag{unsealed}\n",
                raw="FOUND_FLAG=flag{unsealed}\n", call_id="p2-call"))

    asyncio.run(drive())

    assert s._raw_tool_outputs == []
    assert s._raw_tool_commands == []
    assert s._raw_tool_attributed == []
    assert s._raw_tool_capture_receipts == []
    assert s._raw_tool_outputs_chars == 0
    assert s._pending_tool_calls == [("p2-call", "python solve.py")]
    assert s._already_found == set()


def test_protocol2_capture_receipt_must_match_exact_raw_digest():
    s = _cli_solver(Challenge(id="t", name="t", category="forensics"))
    s._protocol2_mode = True
    s._protocol2_capture_callback = lambda _raw: "not-the-raw-digest"

    with pytest.raises(RuntimeError, match="Protocol2CaptureReceiptMismatch"):
        s._persist_raw_tool_output("decoded\n", command="python solve.py")

    assert s._raw_tool_outputs == []
    assert s._raw_tool_capture_receipts == []


def test_protocol2_capture_commits_receipt_with_local_provenance():
    raw = "decoded\n"
    digest = __import__("hashlib").sha256(raw.encode()).hexdigest()
    s = _cli_solver(Challenge(id="t", name="t", category="forensics"))
    s._protocol2_mode = True

    def capture(value):
        assert value == raw
        assert s._raw_tool_outputs == []
        return digest

    s._protocol2_capture_callback = capture
    assert s._persist_raw_tool_output(raw, command="python solve.py") == digest
    assert s._raw_tool_outputs == [raw]
    assert s._raw_tool_capture_receipts == [digest]


def test_protocol2_run_streaming_cleans_up_before_capture_failure(
    monkeypatch, tmp_path,
):
    from muteki.solver import cli_solver as mod

    s = _cli_solver(Challenge(
        id="t", name="t", category="forensics", flag_format=r"flag\{.*?\}"))
    s._protocol2_mode = True
    s._protocol2_capture_callback = lambda _raw: (_ for _ in ()).throw(
        RuntimeError("capture failed"))

    def fake_stream(*args, on_step, **kwargs):
        on_step(StreamStep(
            "tool", text="python solve.py", call_id="p2-call"))
        on_step(StreamStep(
            "tool_result", text="FOUND_FLAG=flag{unsealed}\n",
            raw="FOUND_FLAG=flag{unsealed}\n", call_id="p2-call"))
        return CliResult(text="done")

    monkeypatch.setattr(mod, "run_cli_streaming", fake_stream)

    with pytest.raises(RuntimeError, match="Protocol2CaptureOrIngressFailed"):
        asyncio.run(s._run_streaming(
            ["true"], cwd=str(tmp_path), timeout=5, argv_is_final=True))

    assert s._turn_active is False
    assert s._current_workdir is None
    assert s._raw_tool_outputs == []
    assert s._pending_tool_calls == [("p2-call", "python solve.py")]


def test_run_streaming_retires_loop_owned_control_monitor(monkeypatch, tmp_path):
    from muteki.solver import cli_solver as mod

    ch = Challenge(id="t", name="t", category="forensics")
    s = _cli_solver(ch)
    started = asyncio.Event()
    release = asyncio.Event()
    mutations = []

    async def delayed_drain():
        started.set()
        await release.wait()
        mutations.append("late-monitor")

    s._drain_control_async = delayed_drain

    def fake_stream(*args, **kwargs):
        time.sleep(0.02)
        return CliResult(text="done")

    monkeypatch.setattr(mod, "run_cli_streaming", fake_stream)

    async def drive():
        result = await s._run_streaming(
            ["true"], cwd=str(tmp_path), timeout=5, argv_is_final=True)
        assert result.text == "done"
        assert started.is_set()
        release.set()
        await asyncio.sleep(0.05)
        assert mutations == []

    asyncio.run(drive())


def test_run_streaming_cancellation_retires_step_callbacks(monkeypatch, tmp_path):
    from muteki.core.event_bus import EventBus
    from muteki.solver import cli_solver as mod
    from muteki.solver.cli_driver import CliResult, StreamStep

    bus = EventBus()

    async def slow_sink(event):
        if event.event_type is EventType.TOOL_CALL_START:
            await asyncio.sleep(0.1)

    bus.add_sink(slow_sink)
    ch = Challenge(id="t", name="t", category="forensics",
                   flag_format=r"flag\{.*?\}")
    s = _cli_solver(ch, bus=bus)

    def fake_stream(*args, on_step, cancel_event, **kwargs):
        on_step(StreamStep("tool", text="python solve.py", call_id="late"))
        output = "FOUND_FLAG=flag{must_not_arrive_after_cancel}\n"
        on_step(StreamStep(
            "tool_result", text=output, raw=output, call_id="late"))
        cancel_event.wait(1)
        return CliResult(text=output, cancelled=True)

    monkeypatch.setattr(mod, "run_cli_streaming", fake_stream)

    async def drive():
        task = asyncio.create_task(s._run_streaming(
            ["true"], cwd=str(tmp_path), timeout=5, argv_is_final=True))
        await asyncio.sleep(0.01)
        task.cancel()
        with pytest.raises(asyncio.CancelledError):
            await task
        assert s._raw_tool_outputs == []
        assert s._already_found == set()
        await asyncio.sleep(0.2)
        assert s._raw_tool_outputs == []
        assert s._already_found == set()

    asyncio.run(drive())


def test_run_streaming_post_return_repeated_cancel_waits_for_teardown(
    monkeypatch, tmp_path,
):
    from muteki.solver import cli_solver as mod

    s = _cli_solver(Challenge(id="t", name="t", category="forensics"))
    callback_started = asyncio.Event()
    first_callback_cancel = asyncio.Event()
    second_callback_cancel = asyncio.Event()
    late_mutations = []

    async def stubborn_emit(_step):
        callback_started.set()
        try:
            await asyncio.Event().wait()
        except asyncio.CancelledError:
            first_callback_cancel.set()
        try:
            await asyncio.Event().wait()
        except asyncio.CancelledError:
            second_callback_cancel.set()
        late_mutations.append("retired")

    s._emit_step = stubborn_emit

    def fake_stream(*args, on_step, **kwargs):
        on_step(StreamStep("tool", text="python solve.py", call_id="late"))
        return CliResult(text="done")

    monkeypatch.setattr(mod, "run_cli_streaming", fake_stream)

    async def drive():
        task = asyncio.create_task(s._run_streaming(
            ["true"], cwd=str(tmp_path), timeout=5, argv_is_final=True))
        await asyncio.wait_for(callback_started.wait(), timeout=1)
        # _turn_active becomes false only after the blocking runner returned normally;
        # the callback keeps the independently owned retirement task pending.
        while s._turn_active:
            await asyncio.sleep(0)
        assert not task.done()

        task.cancel("first")
        await asyncio.wait_for(first_callback_cancel.wait(), timeout=1)
        assert not task.done()
        task.cancel("second")
        with pytest.raises(asyncio.CancelledError) as caught:
            await asyncio.wait_for(task, timeout=1)

        assert caught.value.args == ("first",)
        assert second_callback_cancel.is_set()
        assert late_mutations == ["retired"]
        assert s._turn_active is False
        assert s._current_workdir is None
        await asyncio.sleep(0)
        assert not any(
            not candidate.done()
            and getattr(candidate.get_coro(), "__qualname__", "").endswith(
                ("emit_in_order", "_monitor", "_heartbeat", "retire_turn"))
            for candidate in asyncio.all_tasks()
            if candidate is not asyncio.current_task()
        )

    asyncio.run(drive())


def test_run_streaming_caller_cancel_wins_protocol2_step_failure(
    monkeypatch, tmp_path,
):
    from muteki.solver import cli_solver as mod

    s = _cli_solver(Challenge(id="t", name="t", category="forensics"))
    s._protocol2_mode = True
    callback_started = asyncio.Event()

    async def fail_when_cancelled(_step):
        callback_started.set()
        try:
            await asyncio.Event().wait()
        except asyncio.CancelledError:
            raise RuntimeError("delayed protocol2 failure")

    s._emit_step = fail_when_cancelled

    def fake_stream(*args, on_step, **kwargs):
        on_step(StreamStep("tool", text="python solve.py", call_id="p2"))
        return CliResult(text="done")

    monkeypatch.setattr(mod, "run_cli_streaming", fake_stream)

    async def drive():
        task = asyncio.create_task(s._run_streaming(
            ["true"], cwd=str(tmp_path), timeout=5, argv_is_final=True))
        await asyncio.wait_for(callback_started.wait(), timeout=1)
        while s._turn_active:
            await asyncio.sleep(0)
        task.cancel("caller wins")
        with pytest.raises(asyncio.CancelledError) as caught:
            await asyncio.wait_for(task, timeout=1)
        assert caught.value.args == ("caller wins",)
        assert s._turn_active is False
        assert s._current_workdir is None

    asyncio.run(drive())


# ── multi-flag worker layer (Phase 2) ────────────────────────────────────────

def test_extract_flags_all_markers_deduped():
    ch = Challenge(id="t", name="t", category="web", flag_format=r"flag\{.*?\}")
    s = _cli_solver(ch)
    txt = ("FOUND_FLAG=flag{a}\nnoise\nFOUND_FLAG=flag{b}\n"
           "FOUND_FLAG=flag{a}\nFOUND_FLAG=NONE\n")
    assert s._extract_flags(txt) == ["flag{a}", "flag{b}"]  # order kept, dedup, NONE skip
    # _extract_flag (single, back-compat) still returns the LAST marker
    assert s._extract_flag(txt) == "flag{a}"


def test_extract_flags_empty_when_no_markers():
    s = _cli_solver(Challenge(id="t", name="t", category="web"))
    assert s._extract_flags("prose mentioning flag{...} but no marker") == []


def test_accept_flag_dedups_against_already_found():
    s = _cli_solver(Challenge(id="t", name="t", category="web"))
    s._validated_flag_submissions.update({"flag{a}", "flag{b}"})
    # first accept is new; the same flag again is a no-op (no double broadcast)
    assert asyncio.run(s._accept_flag("flag{a}")) is True
    assert asyncio.run(s._accept_flag("flag{a}")) is False
    assert asyncio.run(s._accept_flag("flag{b}")) is True
    assert s.graph.flags == ["flag{a}", "flag{b}"] and s.graph.flag == "flag{a}"
    assert s._already_found == {"flag{a}", "flag{b}"}


# ── flag provenance gate (run-75379) ──────────────────────────────────────────
# Three regressions: (a) a reasoning-only FOUND_FLAG that never appears in any tool
# output is rejected; (b) a flag past char 600 of REAL command output is still
# accepted (the gate sees the untruncated raw, not the deck-truncated chunk); (c) a
# flag in a nested-ssh remote stdout is accepted when that stdout is captured.

def _flag_solver():
    ch = Challenge(id="t", name="t", category="web", flag_format=r"flag\{.*?\}")
    return _cli_solver(ch, bus=_CaptureBus())


def _submit_captured_flags(solver) -> list[str]:
    """Model a validated submit-flag call after real output was captured.

    Most tests in this section isolate stream attribution and provenance without
    creating a SQLite graph. The Blackboard event/decision path has dedicated
    integration tests below; this helper applies the same final authority bit.
    """
    provenance = solver._provenance_corpus()
    candidates = [
        *solver._extract_flags(provenance),
        *solver._poc_flag_literals(provenance),
    ]
    accepted: list[str] = []

    async def drive() -> None:
        for flag in dict.fromkeys(candidates):
            if flag in solver._already_found:
                continue
            if not solver._flag_ok(flag, provenance):
                continue
            solver._validated_flag_submissions.add(flag)
            if await solver._accept_flag(flag):
                solver._stream_accepted.append(flag)
                accepted.append(flag)

    asyncio.run(drive())
    return accepted


def _emit_attributed_tool_result(
    solver, raw: str, *, command: str = "python solve.py",
    text: "str | None" = None, call_id: str = "test-call",
    submit: bool = True,
) -> None:
    from muteki.solver.cli_driver import StreamStep

    async def drive():
        await solver._emit_step(StreamStep(
            "tool", text=command, call_id=call_id))
        await solver._emit_step(StreamStep(
            "tool_result", text=raw if text is None else text,
            raw=raw, call_id=call_id))

    asyncio.run(drive())
    if submit:
        _submit_captured_flags(solver)


def test_reasoning_replay_ignores_incremental_suffix():
    assert cli_solver._is_reasoning_replay("The answer is 42", "2") is False
    assert cli_solver._is_reasoning_replay("The answer is 42", "42") is False
    assert cli_solver._is_reasoning_replay("aa", "a") is False
    assert cli_solver._is_reasoning_replay("The answer is 42", "The answer is 42") is True
    assert cli_solver._is_reasoning_replay("The", "The answer is 42") is True
    assert cli_solver._is_reasoning_replay("", "The") is False


def test_emit_step_forwards_suffix_token_then_seals_full_snapshot():
    from muteki.core.event_bus import EventBus
    from muteki.solver.cli_driver import StreamStep

    bus = EventBus()
    seen: list = []

    async def sink(ev):
        seen.append(ev)

    bus.add_sink(sink)
    ch = Challenge(id="t", name="t", category="misc", flag_format=r"flag\{.*?\}")
    s = _cli_solver(ch, bus=bus)

    async def drive():
        await s._emit_step(StreamStep("reasoning", text="The answer is 4"))
        await s._emit_step(StreamStep("reasoning", text="2"))
        await s._emit_step(StreamStep("reasoning", text="The answer is 42"))

    asyncio.run(drive())
    reasoning = [
        ev for ev in seen if ev.event_type is EventType.REASONING_DELTA
    ]
    assert [ev.payload.get("text") for ev in reasoning] == [
        "The answer is 4", "2", "",
    ]
    assert reasoning[2].payload.get("turn_end") is True
    assert s._reasoning_turn_acc == ""


def test_pi_parse_marks_thinking_delta():
    from muteki.solver.cli_driver import PiDriver

    d = PiDriver()
    thinking = d.parse_stream_steps(
        '{"type":"message_update","assistantMessageEvent":'
        '{"type":"thinking_delta","delta":"pondering"}}')
    assert thinking and thinking[0].kind == "reasoning"
    assert thinking[0].thinking is True
    answer = d.parse_stream_steps(
        '{"type":"message_update","assistantMessageEvent":'
        '{"type":"text_delta","delta":"the answer"}}')
    assert answer and answer[0].thinking is False


def test_emit_step_thinking_not_accumulated_then_answer_snapshot_seals():
    """Pi/OMP message_end repeats ONLY the answer text. With thinking mixed
    into the replay accumulator that snapshot is an unsealable suffix and the
    answer would be appended twice; thinking must display but not accumulate."""
    from muteki.core.event_bus import EventBus
    from muteki.solver.cli_driver import StreamStep

    bus = EventBus()
    seen: list = []

    async def sink(ev):
        seen.append(ev)

    bus.add_sink(sink)
    ch = Challenge(id="t", name="t", category="misc", flag_format=r"flag\{.*?\}")
    s = _cli_solver(ch, bus=bus)

    async def drive():
        await s._emit_step(
            StreamStep("reasoning", text="let me think ", thinking=True))
        await s._emit_step(StreamStep("reasoning", text="OK"))
        # message_end snapshot: answer-only, thinking excluded.
        await s._emit_step(StreamStep("reasoning", text="OK"))

    asyncio.run(drive())
    reasoning = [
        ev for ev in seen if ev.event_type is EventType.REASONING_DELTA
    ]
    assert [ev.payload.get("text") for ev in reasoning] == [
        "let me think ", "OK", "",
    ]
    assert reasoning[2].payload.get("turn_end") is True
    assert s._reasoning_turn_acc == ""


def test_reasoning_only_flag_is_rejected_run75379():
    """(a) The exact run-75379 BUG①: a worker restates `FOUND_FLAG=flag{x}` in its
    REASONING (its own claim), and that value appears in NO tool output. The old live
    path passed the reasoning chunk to _stream_markers and gated the flag against the
    SAME chunk (`flag in raw_output` where raw_output IS the claim) → trivially true →
    hallucinated flag laundered through prose. Now reasoning can't source a flag."""
    from muteki.solver.cli_driver import StreamStep
    s = _flag_solver()
    hallucination = ("I confirmed from the real output 2 flags. "
                     "FOUND_FLAG=flag{090099b7-e350-424a-9d68-b5310495403e}")
    asyncio.run(s._emit_step(StreamStep("reasoning", text=hallucination)))
    # NOT accepted: a reasoning chunk is never a flag source.
    assert s._already_found == set()
    assert s._stream_accepted == []
    # and the raw-output corpus stayed empty (reasoning is not command output).
    assert s._raw_tool_outputs == []


def test_tool_result_flag_in_real_output_is_accepted_run75379():
    """The legitimate counterpart to (a): the SAME flag, when it appears in real
    command output (a tool_result), IS accepted — provenance traces to evidence."""
    from muteki.solver.cli_driver import StreamStep
    s = _flag_solver()
    real = "root@dc:~# type flag.txt\nFOUND_FLAG=flag{real-from-output}\n"
    _emit_attributed_tool_result(s, real, command="type flag.txt")
    assert s._already_found == {"flag{real-from-output}"}
    assert s._stream_accepted == ["flag{real-from-output}"]


def test_materialized_operator_secret_is_redacted_from_events_graph_and_artifacts(
        tmp_path):
    from muteki.solver.cli_driver import StreamStep
    from muteki.solver.result import ArtifactStore

    secret = "p4ss-EXACT-9988776655"
    bus = _CaptureBus()
    artifacts = ArtifactStore(root=tmp_path / "secret-redaction-artifacts")
    ch = Challenge(id="secret-redaction", name="secret", category="web")
    solver = CliSolver(
        None, ch, bus=bus, artifacts=artifacts,
        driver=_StubDriver(""), engine="claude", kb=False)
    solver._control_secret_values = [secret]
    reasoning = f"VERIFIED_FACT=credential {secret} authenticated"
    tool_output = (
        f"curl -H 'Authorization: Bearer {secret}' /admin\n"
        f"DEADEND=token {secret} was rejected on legacy endpoint\n")

    asyncio.run(solver._emit_step(StreamStep("reasoning", text=reasoning)))
    _emit_attributed_tool_result(
        solver, tool_output, command="curl -H '<secret>' /admin")

    event_dump = json.dumps(
        [event.model_dump(mode="json") for event in bus.events],
        ensure_ascii=False, default=str)
    graph_dump = solver.graph.model_dump_json()
    artifact_dump = "\n".join(
        path.read_text(encoding="utf-8", errors="replace")
        for path in artifacts.root.glob("*") if path.is_file())
    assert secret not in event_dump
    assert secret not in graph_dump
    assert secret not in artifact_dump
    assert "<REDACTED_OPERATOR_SECRET>" in event_dump
    # Raw execution provenance remains in memory for the hard flag gate only.
    assert secret in solver._provenance_corpus()


def test_secret_redactor_survives_wrapper_account_release_for_late_steps(
        tmp_path):
    from muteki.solver.cli_driver import StreamStep
    from muteki.solver.result import ArtifactStore
    from muteki.swarm.swarm import Swarm
    from muteki.core.llm import ModelSpec
    from muteki.sandbox.manager import SandboxManager

    secret = "late-secret-112233445566"
    bus = _CaptureBus()
    challenge = Challenge(id="late-secret", name="late", category="web")
    solver = CliSolver(
        None, challenge, bus=bus,
        artifacts=ArtifactStore(root=tmp_path / "late-secret-artifacts"),
        driver=_StubDriver(""), engine="claude", kb=False)
    solver._control_secret_values = [secret]
    swarm = Swarm(
        challenge, [ModelSpec(solver_id="seat", model="mock")],
        llm=None,
        sandbox=SandboxManager(root=tmp_path / "late-secret-sbx"),
        artifacts=solver.artifacts)
    swarm._release_worker_account(solver)
    asyncio.run(solver._emit_step(StreamStep(
        "reasoning", text=f"late callback echoed {secret}")))
    dump = json.dumps(
        [event.model_dump(mode="json") for event in bus.events], default=str)
    assert secret not in dump
    assert solver._control_secret_values == [secret]


def test_secret_redactor_covers_short_parsed_credential_components():
    solver = CliSolver(
        None, Challenge(id="short-secret", name="short", category="web"),
        driver=_StubDriver(""), engine="claude", kb=False)
    solver._control_secret_values = ["password=abc", "ssh://u:p@host.invalid"]

    redacted = solver._redact_control_secrets("worker echoed abc and p exactly")

    assert " abc " not in redacted
    assert " p " not in redacted
    assert redacted.count("<REDACTED_OPERATOR_SECRET>") == 2


def test_secret_redactor_short_exact_decision_tokens_preserve_telemetry():
    solver = CliSolver(
        None, Challenge(id="short-answer", name="short", category="web"),
        driver=_StubDriver(""), engine="claude", kb=False)
    solver._control_secret_values = ["A", "1"]

    redacted = solver._redact_control_secrets(
        "answer A accepted; worker ACTIVE; phase 1 complete; phase 10 pending")

    assert "answer A accepted" not in redacted
    assert "phase 1 complete" not in redacted
    assert "worker ACTIVE" in redacted
    assert "phase 10 pending" in redacted
    assert redacted.count("<REDACTED_OPERATOR_SECRET>") == 2


def test_flag_past_char_600_still_accepted_via_untruncated_raw_run75379():
    """(b) Codex's hidden killer: the stream driver truncates the live tool_result
    chunk to 600 chars. A flag that appears PAST char 600 of a command's output is
    absent from the truncated `text`, but the gate must see the full `raw`. Without the
    raw-output gate this real flag would be silently dropped."""
    from muteki.solver.cli_driver import StreamStep
    s = _flag_solver()
    flag = "flag{past-the-600-char-cutoff}"
    # mimic exactly what the drivers now produce: text truncated to 600, raw full.
    full = ("A" * 900) + f"\nFOUND_FLAG={flag}\n"
    step = StreamStep("tool_result", text=full[:600], raw=full)
    assert flag not in step.text          # truncated chunk genuinely lacks the flag
    assert flag in step.raw               # but the raw output carries it
    _emit_attributed_tool_result(
        s, full, text=step.text, command="python recover.py")
    assert s._already_found == {flag}     # accepted because the gate saw raw


def test_nested_ssh_remote_stdout_flag_accepted_when_captured_run75379():
    """(c) Nested `ssh root@VPS 'cat flag.txt'`: the remote flag is in the REMOTE
    stdout, which the outer ssh forwards into the local tool output. When that output
    is captured (StreamStep.raw), the flag is gateable and accepted — the run-75379
    flag04 false-negative is fixed."""
    from muteki.solver.cli_driver import StreamStep
    s = _flag_solver()
    flag = "flag{ebca91d7-from-pivoted-dc}"
    # the outer ssh command's captured output = the remote host's stdout.
    remote = (f"root@workstation:~# ssh root@10.0.0.6 'cat /root/flag.txt'\n"
              f"{flag}\nFOUND_FLAG={flag}\n")
    _emit_attributed_tool_result(
        s, remote, command="ssh root@10.0.0.6 'cat /root/flag.txt'")
    assert s._already_found == {flag}


def test_stream_markers_allow_flags_false_blocks_flag_but_keeps_facts():
    """Unit: allow_flags=False (the reasoning path) extracts facts/dead-ends but NEVER
    a flag, even if the chunk literally contains FOUND_FLAG= with the value present."""
    s = _flag_solver()
    chunk = "FOUND_FLAG=flag{should-not-take}\nVERIFIED_FACT=port 8080 is open\n"
    asyncio.run(s._stream_markers(chunk, allow_flags=False))
    assert s._already_found == set()           # flag blocked
    # the fact still went out
    assert any(e.payload.get("kind") == "fact_added"
               for e in s.bus.events if e.event_type is EventType.BLACKBOARD_DELTA)


def test_stream_markers_extracts_and_gates_from_flag_provenance():
    """Unit: when flag_provenance is given, flags are BOTH extracted from and gated
    against THAT corpus, not the (possibly truncated) display chunk. The FOUND_FLAG
    marker can sit past char 600 of `text`, so reading it out of `text` would miss it
    entirely — it must come from the raw provenance."""
    s = _flag_solver()
    # the display chunk has no marker at all; the raw provenance carries the real one
    # (e.g. the marker landed past the 600-char truncation point).
    display = "...output truncated for the deck..."
    raw = "the command printed FOUND_FLAG=flag{from-raw} to stdout\n"
    _emit_attributed_tool_result(s, raw, text=display)
    assert s._already_found == {"flag{from-raw}"}

    # and a marker whose value is corroborated by a launder signature is rejected: a
    # FOUND_FLAG= present in prose next to a read-from-disk steal.
    s2 = _flag_solver()
    steal = ("$ grep -r flag /workspace/eval_runs/run-1/winner.json\n"
             "FOUND_FLAG=flag{stolen}\n")
    asyncio.run(s2._stream_markers(steal, flag_provenance=steal))
    assert s2._already_found == set()   # _flag_ok launder gate rejects the disk-steal


def test_surface_unverified_flags_emits_for_untraceable_claim_run75379():
    """A FOUND_FLAG the worker CLAIMED that traces to NO captured output is surfaced to
    the operator as `flag_unverified` (not silently dropped, not auto-solved) — the
    nested-ssh false-negative guard."""
    s = _flag_solver()
    transcript = "FOUND_FLAG=flag{claimed-no-trace}\nI read it on the DC.\n"
    asyncio.run(s._surface_unverified_flags(transcript))
    unv = [e for e in s.bus.events
           if e.event_type is EventType.BLACKBOARD_DELTA
           and e.payload.get("kind") == "flag_unverified"]
    assert len(unv) == 1
    assert unv[0].payload.get("flag") == "flag{claimed-no-trace}"
    assert unv[0].payload.get("reason")   # operator-facing reason present
    # an ACCEPTED flag is verified, not unverified — no event for it.
    s2 = _flag_solver()
    s2._validated_flag_submissions.add("flag{accepted}")
    asyncio.run(s2._accept_flag("flag{accepted}"))
    asyncio.run(s2._surface_unverified_flags("FOUND_FLAG=flag{accepted}\n"))
    assert not [e for e in s2.bus.events
                if e.event_type is EventType.BLACKBOARD_DELTA
                and e.payload.get("kind") == "flag_unverified"]


def test_assistant_final_is_not_part_of_provenance_corpus():
    """A terminal assistant claim must not become its own execution witness."""
    s = _flag_solver()
    s._persist_raw_tool_output("HTTP/1.1 200 OK\nno flag here\n")
    final = "I solved it.\nFOUND_FLAG=flag{assistant-only}\n"

    assert "flag{assistant-only}" not in s._provenance_corpus()
    asyncio.run(s._stream_markers(
        final, flag_provenance=s._provenance_corpus()))
    assert s._already_found == set()


def test_operator_flag_echo_is_tainted_even_when_it_reaches_tool_output():
    """Operator context has no truth privilege and cannot be laundered via `echo`."""
    from muteki.solver.cli_driver import StreamStep

    ch = Challenge(id="t", name="t", category="web", flag_format=r"flag\{.*?\}")
    bus = _CaptureBus()
    s = CliSolver(
        None, ch, bus=bus, driver=_StubDriver(""), engine="claude", kb=False,
        standing_guidance=["possible answer: flag{operator-supplied}"],
    )
    echoed = "$ echo FOUND_FLAG=flag{operator-supplied}\n" \
             "FOUND_FLAG=flag{operator-supplied}\n"

    _emit_attributed_tool_result(
        s, echoed, command="echo FOUND_FLAG=flag{operator-supplied}")

    assert "flag{operator-supplied}" in s._provenance_corpus()  # real echo stdout
    assert s._already_found == set()  # but its value originated with the operator
    assert s._stream_accepted == []


def test_operator_redirect_url_flag_is_tainted_when_target_echoes_it():
    """Redirect URLs are operator context too. A server/curl echo of the URL
    cannot launder an operator-supplied candidate through raw tool provenance."""
    from muteki.solver.cli_driver import StreamStep

    candidate = "flag{from-operator-url}"
    url = f"http://target.invalid/next/{candidate}"
    ch = Challenge(id="t", name="t", category="web", flag_format=r"flag\{.*?\}")
    s = CliSolver(
        None, ch, bus=_CaptureBus(), driver=_StubDriver(""), engine="claude",
        kb=False, hitl_cmd={"action": "redirect", "url": url},
    )
    echoed = f"$ curl {url}\nHTTP/1.1 200 OK\nredirected={url}\n"

    _emit_attributed_tool_result(s, echoed, command=f"curl {url}")

    assert candidate in s._provenance_corpus()
    assert s._flag_from_operator_context(candidate) is True
    assert s._already_found == set()
    assert s._stream_accepted == []


def test_bootstrap_assistant_only_flag_does_not_solve(monkeypatch):
    """End-to-end regression: CliResult.text is a claim, not raw target output."""
    from muteki.solver import cli_solver as mod
    from muteki.solver.cli_driver import CliResult

    bus = _CaptureBus()
    ch = Challenge(id="t", name="t", category="web", flag_format=r"flag\{.*?\}")
    s = CliSolver(
        None, ch, bus=bus, driver=_StubDriver(""), engine="claude", kb=False)
    canned = lambda *a, **k: CliResult(
        text="FOUND_FLAG=flag{assistant-only}\n", session="sess-x")
    monkeypatch.setattr(mod, "run_cli_streaming", canned)
    monkeypatch.setattr(mod, "run_cli", canned)

    out = asyncio.run(s.run())

    assert out.solved is False
    assert "flag_found" not in _bb_kinds(bus.events)
    assert "flag_unverified" in _bb_kinds(bus.events)


def test_persist_raw_tool_output_ring_trims_to_cap():
    """The raw-output corpus is bounded; a chatty run can't balloon memory. The most
    recent output (where a just-found flag lives) is kept when trimming."""
    s = _flag_solver()
    s._RAW_OUTPUT_CHAR_CAP = 1000  # shrink for the test
    for i in range(20):
        s._persist_raw_tool_output("X" * 200)
    assert s._raw_tool_outputs_chars <= 1000
    # the freshest chunk survived
    s._persist_raw_tool_output("FOUND_FLAG=flag{freshest}")
    assert "flag{freshest}" in s._provenance_corpus()


# ── driver-level: tool_result carries untruncated raw (run-75379) ─────────────

def test_claude_driver_tool_result_raw_is_untruncated():
    flag = "flag{deep-in-claude-output}"
    body = ("B" * 900) + f" FOUND_FLAG={flag}"
    line = json.dumps({"type": "user", "message": {
        "content": [{"type": "tool_result", "tool_use_id": "call-raw",
                     "content": body}]}})
    steps = ClaudeCodeDriver().parse_stream_steps(line)
    tr = [s for s in steps if s.kind == "tool_result"][0]
    assert len(tr.text) == 600 and flag not in tr.text   # deck chunk truncated
    assert flag in tr.raw                                  # gate sees full output
    assert tr.call_id == "call-raw"


def test_codex_driver_tool_result_raw_is_untruncated():
    flag = "flag{deep-in-codex-output}"
    body = ("C" * 900) + f" FOUND_FLAG={flag}"
    line = json.dumps({"type": "item.completed", "item": {
        "id": "item-raw", "type": "command_execution",
        "aggregated_output": body}})
    step = CodexDriver().parse_stream_line(line)
    assert step.kind == "tool_result"
    assert len(step.text) == 600 and flag not in step.text
    assert flag in step.raw
    assert step.call_id == "item-raw"


def test_cursor_driver_tool_result_raw_is_untruncated():
    flag = "flag{deep-in-cursor-output}"
    body = ("D" * 900) + f" FOUND_FLAG={flag}"
    tc = {"shell": {"result": {"success": {"content": body}}}}
    line = json.dumps(
        {"type": "tool_call", "subtype": "completed", "call_id": "cursor-raw",
         "tool_call": tc})
    steps = CursorDriver().parse_stream_steps(line)
    tr = [s for s in steps if s.kind == "tool_result"][0]
    assert len(tr.text) == 600 and flag not in tr.text
    assert flag in tr.raw
    assert tr.call_id == "cursor-raw"


def test_cursor_driver_shell_completion_reads_actual_stdout_shape():
    flag = "flag{cursor-shell-stdout}"
    body = f"FOUND_FLAG={flag}\n"
    tc = {"shellToolCall": {
        "args": {"command": "python solve.py"},
        "result": {"success": {
            "exitCode": 0,
            "stdout": body,
            "stderr": "warning\n",
            "interleavedOutput": body + "warning\n",
        }},
    }}
    line = json.dumps({
        "type": "tool_call", "subtype": "completed",
        "call_id": "cursor-shell", "tool_call": tc,
    })

    steps = CursorDriver().parse_stream_steps(line)
    result = [step for step in steps if step.kind == "tool_result"][0]

    assert result.raw == body + "warning\n"
    assert flag in result.text
    assert result.call_id == "cursor-shell"


def test_cursor_shell_started_completed_pair_admits_real_flag():
    flag = "flag{cursor-paired-output}"
    started = json.dumps({
        "type": "tool_call", "subtype": "started", "call_id": "cursor-pair",
        "tool_call": {"shellToolCall": {
            "args": {"command": "python solve.py"}}},
    })
    completed = json.dumps({
        "type": "tool_call", "subtype": "completed", "call_id": "cursor-pair",
        "tool_call": {"shellToolCall": {"result": {"success": {
            "exitCode": 0,
            "stdout": f"FOUND_FLAG={flag}\n",
            "stderr": "",
            "interleavedOutput": f"FOUND_FLAG={flag}\n",
        }}}},
    })
    driver = CursorDriver()
    solver = _flag_solver()

    async def drive():
        for line in (started, completed):
            for step in driver.parse_stream_steps(line):
                await solver._emit_step(step)

    asyncio.run(drive())

    _submit_captured_flags(solver)
    assert solver._already_found == {flag}
    assert solver._raw_tool_commands == ["python solve.py"]
    assert solver._raw_tool_attributed == [True]


def test_cursor_failed_shell_result_keeps_real_stdout_provenance():
    flag = "flag{cursor-output-before-nonzero-exit}"
    started = json.dumps({
        "type": "tool_call", "subtype": "started", "call_id": "cursor-failure",
        "tool_call": {"shellToolCall": {
            "args": {"command": "python solve_then_fail.py"}}},
    })
    completed = json.dumps({
        "type": "tool_call", "subtype": "completed", "call_id": "cursor-failure",
        "tool_call": {"shellToolCall": {"result": {
            "case": "failure",
            "value": {
                "exitCode": 1,
                "stdout": f"FOUND_FLAG={flag}\n",
                "stderr": "post-processing failed\n",
                "interleavedOutput": (
                    f"FOUND_FLAG={flag}\npost-processing failed\n"),
            },
        }}},
    })
    driver = CursorDriver()
    solver = _flag_solver()

    async def drive():
        for line in (started, completed):
            for step in driver.parse_stream_steps(line):
                await solver._emit_step(step)

    asyncio.run(drive())

    _submit_captured_flags(solver)
    assert solver._already_found == {flag}
    assert solver._raw_tool_commands == ["python solve_then_fail.py"]
    assert solver._raw_tool_attributed == [True]
    assert "post-processing failed" in solver._provenance_corpus()


def _cursor_spill_step(path: str, *, call_id: str = "cursor-spill",
                       size_bytes: int = 90000):
    line = json.dumps({
        "type": "tool_call", "subtype": "completed", "call_id": call_id,
        "tool_call": {"shellToolCall": {"result": {
            "case": "success",
            "value": {
                "exitCode": 0,
                "stdout": "",
                "stderr": "",
                "interleavedOutput": "",
                "outputLocation": {
                    "filePath": path,
                    "lineCount": 12000,
                    "sizeBytes": size_bytes,
                },
            },
        }}},
    })
    return CursorDriver().parse_stream_steps(line)[0]


def test_cursor_spilled_shell_result_keeps_location_metadata_only():
    result = _cursor_spill_step(
        "/workspace/.cursor/FOUND_FLAG=flag{path_is_not_output}.txt")

    assert result.kind == "tool_result"
    assert result.raw == ""
    assert result.text == ""
    assert result.spill_path.endswith("flag{path_is_not_output}.txt")
    assert result.spill_size_bytes == 90000
    assert result.spill_line_count == 12000
    assert result.call_id == "cursor-spill"


def test_cursor_spilled_shell_result_loads_real_local_output(tmp_path):
    workdir = tmp_path / "worker"
    spill = workdir / ".cursor" / "shell-output.txt"
    spill.parent.mkdir(parents=True)
    flag = "flag{cursor_spill_loaded}"
    spill.write_text(f"decoded\nFOUND_FLAG={flag}\n")
    solver = _flag_solver()
    solver._current_workdir = workdir.resolve()

    async def drive():
        await solver._emit_step(StreamStep(
            "tool", text="python solve.py", call_id="cursor-spill"))
        await solver._emit_step(_cursor_spill_step(
            str(spill), size_bytes=spill.stat().st_size))

    asyncio.run(drive())

    _submit_captured_flags(solver)
    assert solver._raw_tool_outputs == [spill.read_text()]
    assert solver._raw_tool_commands == ["python solve.py"]
    assert solver._raw_tool_attributed == [True]
    assert solver._already_found == {flag}


@pytest.mark.parametrize("escape_kind", ["parent", "sibling", "symlink"])
def test_cursor_spilled_shell_result_rejects_escape_paths(tmp_path, escape_kind):
    workdir = tmp_path / "workspace" / "workers" / "cli-cursor"
    workdir.mkdir(parents=True)
    outside = tmp_path / "workspace" / "graph" / "shared_graph.db"
    outside.parent.mkdir(parents=True)
    outside.write_text("FOUND_FLAG=flag{must_not_be_read}\n")
    if escape_kind == "parent":
        reported = "../../graph/shared_graph.db"
    elif escape_kind == "sibling":
        reported = str(outside)
    else:
        link = workdir / "spill.txt"
        link.symlink_to(outside)
        reported = str(link)
    bus = _CaptureBus()
    solver = _cli_solver(
        Challenge(id="t", name="t", category="forensics",
                  flag_format=r"flag\{.*?\}"), bus=bus)
    solver._current_workdir = workdir.resolve()

    async def drive():
        await solver._emit_step(StreamStep(
            "tool", text="python solve.py", call_id="cursor-spill"))
        await solver._emit_step(_cursor_spill_step(reported))

    asyncio.run(drive())

    assert solver._raw_tool_outputs == []
    assert solver._already_found == set()
    assert solver._pending_tool_calls == []
    result_events = [
        event for event in bus.events
        if event.event_type is EventType.TOOL_CALL_RESULT
    ]
    assert result_events[-1].payload["result"]["spill"]["status"] == "rejected"


def test_cursor_spill_translates_exact_container_worker_cwd(tmp_path):
    workspace = tmp_path / "workspace"
    workdir = workspace / "workers" / "cli-cursor"
    spill = workdir / ".cursor" / "shell-output.txt"
    spill.parent.mkdir(parents=True)
    flag = "flag{cursor_container_spill}"
    spill.write_text(f"FOUND_FLAG={flag}\n")
    handle = ContainerHandle(
        run_id="spill", host_workspace=str(workspace),
        container="muteki-run-spill")
    solver = _flag_solver()
    solver.container = handle
    solver._current_workdir = workdir.resolve()
    container_spill = (
        f"{CONTAINER_WORKSPACE}/workers/cli-cursor/.cursor/shell-output.txt")

    async def drive():
        await solver._emit_step(StreamStep(
            "tool", text="python solve.py", call_id="cursor-spill"))
        await solver._emit_step(_cursor_spill_step(container_spill))

    asyncio.run(drive())

    _submit_captured_flags(solver)
    assert solver._raw_tool_outputs == [spill.read_text()]
    assert solver._already_found == {flag}


def test_single_flag_prompt_has_no_multiflag_block():
    # expected_flags=1 (default) → the prompt must NOT carry the multi-flag block,
    # keeping single-flag runs byte-identical.
    ch = Challenge(id="t", name="t", category="web")
    s = _cli_solver(ch)
    p = s._build_prompt()
    assert "find them ALL" not in p and "has 1 flags" not in p


def test_multiflag_prompt_announces_count_and_found():
    ch = Challenge(id="t", name="t", category="web", expected_flags=3)
    s = _cli_solver(ch)
    s._already_found.add("flag{got1}")
    p = s._build_prompt()
    assert "has 3 flags" in p and "find them ALL" in p
    assert "flag{got1}" in p  # already-found list injected so it doesn't re-hunt


def test_expected_flags_helper_clamps():
    assert _cli_solver(Challenge(id="t", name="t", category="web"))._expected_flags() == 1
    assert _cli_solver(Challenge(id="t", name="t", category="web",
                                 expected_flags=0))._expected_flags() == 1
    assert _cli_solver(Challenge(id="t", name="t", category="web",
                                 expected_flags=4))._expected_flags() == 4


# ── external-USD cost accounting (shelled CLI bills in dollars) ───────────────

def test_add_external_usd_bumps_ledger_and_emits():
    events = []

    class _Bus:
        async def emit(self, ev): events.append(ev)

    cost = CostController(bus=_Bus(), budget=Budget(global_usd=10.0))
    spent = asyncio.run(cost.add_external_usd(
        1.43, run_id="r", solver_id="cli-1", challenge_id="c",
        input_tokens=1000, output_tokens=200))
    assert spent == 1.43
    assert cost.global_usd() == 1.43
    # a COST_UPDATE was emitted for the deck, carrying the token breakdown so the
    # deck's token-usage column has per-scope counts alongside the $ figure.
    cu = [e for e in events if e.event_type is EventType.COST_UPDATE]
    assert cu, "expected a COST_UPDATE"
    p = cu[-1].payload
    assert p["tokens"] == 1200 and p["input_tokens"] == 1000 and p["output_tokens"] == 200
    assert p["usd"] == 1.43


def test_add_external_usd_feeds_budget_breaker():
    cost = CostController(budget=Budget(per_solver_usd=2.0))
    asyncio.run(cost.add_external_usd(2.5, run_id="r", solver_id="cli-1"))
    assert cost.over_budget("solver:cli-1") is True


def test_add_external_usd_records_tokens_at_zero_cost():
    # cursor path: subscription-backed (usd=0) but reports token usage. The tokens
    # must land in the ledger / COST_UPDATE so the deck's token column counts them,
    # while $ stays flat.
    events = []

    class _Bus:
        async def emit(self, ev): events.append(ev)

    cost = CostController(bus=_Bus())
    asyncio.run(cost.add_external_usd(
        0.0, run_id="r", solver_id="cli-cursor-1", challenge_id="c",
        input_tokens=27140, output_tokens=30))
    assert cost.global_usd() == 0.0          # no dollars from cursor
    p = [e for e in events if e.event_type is EventType.COST_UPDATE][-1].payload
    assert p["tokens"] == 27170 and p["input_tokens"] == 27140 and p["output_tokens"] == 30


# ── blackboard collaboration lifecycle (OneNote board) ───────────────────────

class _CaptureBus:
    def __init__(self): self.events = []
    async def emit(self, ev): self.events.append(ev)


class _StubDriver:
    """A CLI driver that returns a canned transcript — no subprocess."""
    name = "claude"
    def __init__(self, text): self._text = text
    def new_session(self): return "sess-x"
    def build_execute(self, *a, **k): return ["true"]
    def build_resume(self, *a, **k): return ["true"]
    def parse(self, *a, **k): raise AssertionError("parse unused")
    def parse_stream_line(self, *a, **k): return None


def _bb_kinds(events):
    return [e.payload.get("kind") for e in events
            if e.event_type is EventType.BLACKBOARD_DELTA]


def _worker_statuses(events):
    return [e for e in events if e.event_type is EventType.WORKER_STATUS]


def _run_cli_solver(monkeypatch, transcript, *, intent_id=""):
    """Run a CliSolver with the streaming runner stubbed to return `transcript`
    (CliSolver streams when a bus is present). Returns the bus + solver."""
    from muteki.solver import cli_solver as mod
    from muteki.solver.cli_driver import CliResult
    bus = _CaptureBus()
    ch = Challenge(id="t", name="login", category="web", flag_format=r"flag\{.*?\}",
                   target="http://x")
    drv = _StubDriver(transcript)
    s = CliSolver(
        None, ch, bus=bus, driver=drv, engine="claude", kb=False,
        intent_id=intent_id,
    )
    def canned(*a, **k):
        # This helper models a successful command-backed solve.  CliResult.text is the
        # assistant's final claim; explicitly seed the independent tool-output corpus
        # instead of relying on that claim to self-prove provenance.
        s._persist_raw_tool_output(transcript)
        s._validated_flag_submissions.update(s._extract_flags(transcript))
        return CliResult(text=transcript, session="sess-x")
    monkeypatch.setattr(mod, "run_cli_streaming", canned)
    monkeypatch.setattr(mod, "run_cli", canned)  # the no-bus fallback path
    asyncio.run(s.run())
    return bus, s


def test_cli_solver_emits_full_intent_lifecycle_on_solve(monkeypatch):
    bus, s = _run_cli_solver(monkeypatch, "did the thing\nFOUND_FLAG=flag{real}\n")
    kinds = _bb_kinds(bus.events)
    # The claim lifecycle and gated flag are sufficient.  A transcript closing
    # line must not be manufactured into a fact merely to decorate the solve.
    assert kinds[0] == "intent_proposed"
    assert "intent_claimed" in kinds
    assert "fact_added" not in kinds
    assert "intent_concluded" in kinds
    assert "flag_found" in kinds
    # concluded must say solved
    concl = [e for e in bus.events
             if e.event_type is EventType.BLACKBOARD_DELTA
             and e.payload.get("kind") == "intent_concluded"][0]
    assert concl.payload.get("result") == "solved"
    # the claimed intent's worker is this solver (so the board links its facts)
    claimed = [e for e in bus.events
               if e.event_type is EventType.BLACKBOARD_DELTA
               and e.payload.get("kind") == "intent_claimed"][0]
    assert claimed.payload.get("worker") == s.solver_id


def test_bootstrap_uses_assigned_planning_intent_for_full_lifecycle(monkeypatch):
    bus, _solver = _run_cli_solver(
        monkeypatch,
        "poked around, found nothing useful\n",
        intent_id="I-plan-assigned",
    )
    intent_events = [
        event for event in bus.events
        if event.event_type is EventType.BLACKBOARD_DELTA
        and event.payload.get("kind") in {
            "intent_proposed", "intent_claimed", "intent_concluded",
        }
    ]
    assert intent_events
    assert {
        event.payload.get("intent_id") for event in intent_events
    } == {"I-plan-assigned"}


def test_cli_solver_concludes_explored_on_miss_without_dead_end(monkeypatch):
    bus, s = _run_cli_solver(monkeypatch, "poked around, found nothing useful\n")
    kinds = _bb_kinds(bus.events)
    assert "intent_proposed" in kinds and "intent_claimed" in kinds
    assert "dead_end" not in kinds
    concl = [e for e in bus.events
             if e.event_type is EventType.BLACKBOARD_DELTA
             and e.payload.get("kind") == "intent_concluded"][0]
    assert concl.payload.get("result") == "explored"
    assert "found no verified flag" in concl.payload.get("result_detail", "")
    assert "flag_found" not in kinds


def test_record_fact_db_failure_does_not_emit_blackboard_fact():
    bus = _CaptureBus()
    ch = Challenge(id="t", name="login", category="web", flag_format=r"flag\{.*?\}",
                   target="http://x")

    class RejectingGraph:
        def add_evidence(self, **kw):
            return -1

    s = CliSolver(
        None,
        ch,
        bus=bus,
        driver=_StubDriver(""),
        engine="claude",
        kb=False,
        shared_graph=RejectingGraph(),
    )

    fact_seq = asyncio.run(
        s._record_fact("fact that failed to persist", verified=True, artifact_id="aid"))

    assert fact_seq == -1
    assert "fact_added" not in _bb_kinds(bus.events)


def test_cli_solver_worker_status_reports_timeout(monkeypatch):
    from muteki.solver import cli_solver as mod
    from muteki.solver.cli_driver import CliResult

    bus = _CaptureBus()
    ch = Challenge(id="t", name="login", category="web", flag_format=r"flag\{.*?\}",
                   target="http://x")
    drv = _StubDriver("")
    s = CliSolver(None, ch, bus=bus, driver=drv, engine="claude", kb=False)
    calls = {"n": 0}

    def fake_stream(*a, **k):
        calls["n"] += 1
        if calls["n"] == 1:
            return CliResult(text="still working\n", session="sess-x", timed_out=True)
        return CliResult(text="FOUND_FLAG=NONE\n", session="sess-x")

    monkeypatch.setattr(mod, "run_cli_streaming", fake_stream)
    monkeypatch.setattr(mod, "run_cli", fake_stream)
    asyncio.run(s.run())

    statuses = _worker_statuses(bus.events)
    assert statuses[0].payload == {
        "online": True,
        "status": "online",
        "reason": "started",
        "engine": "claude",
        "session": "",
        "worker_role": "bootstrap",
    }
    # once the worker's CLI session id is known it re-emits status carrying it, so
    # the deck can surface a resume command (`claude -r <id>`) for manual attach.
    assert any(s.payload.get("session") == "sess-x" for s in statuses)
    assert statuses[-1].payload["online"] is False
    assert statuses[-1].payload["status"] == "offline"
    assert statuses[-1].payload["reason"] == "timeout"
    kinds = _bb_kinds(bus.events)
    assert "dead_end" not in kinds
    concl = [e for e in bus.events
             if e.event_type is EventType.BLACKBOARD_DELTA
             and e.payload.get("kind") == "intent_concluded"][0]
    assert concl.payload.get("result") == "timed_out"
    assert "timed out" in concl.payload.get("result_detail", "").lower()


def test_cli_streaming_emits_busy_heartbeat_during_silent_turn(monkeypatch, tmp_path):
    from muteki.solver import cli_solver as mod
    from muteki.solver.cli_driver import CliResult

    bus = _CaptureBus()
    ch = Challenge(id="t", name="login", category="web", flag_format=r"flag\{.*?\}",
                   target="http://x")
    s = CliSolver(None, ch, bus=bus, driver=_StubDriver(""), engine="claude",
                  kb=False)

    def silent_stream(*a, **k):
        time.sleep(0.07)
        return CliResult(text="no flag\n", session="sess-x")

    monkeypatch.setattr(mod, "_WORKER_HEARTBEAT_SECONDS", 0.02)
    monkeypatch.setattr(mod, "run_cli_streaming", silent_stream)

    asyncio.run(s._run_streaming(["true"], cwd=str(tmp_path), timeout=5))

    statuses = _worker_statuses(bus.events)
    assert any(
        ev.payload.get("online") is True
        and ev.payload.get("status") == "online"
        and ev.payload.get("reason") == "busy"
        for ev in statuses
    )


def test_cli_solver_worker_status_reports_asyncio_cancel(monkeypatch):
    bus = _CaptureBus()
    ch = Challenge(id="t", name="login", category="web", flag_format=r"flag\{.*?\}",
                   target="http://x")
    s = CliSolver(None, ch, bus=bus, driver=_StubDriver(""), engine="claude", kb=False)

    async def cancelled_bootstrap():
        raise asyncio.CancelledError()

    monkeypatch.setattr(s, "_run_bootstrap", cancelled_bootstrap)

    async def drive():
        try:
            await s.run()
        except asyncio.CancelledError:
            return
        raise AssertionError("run() did not propagate cancellation")

    asyncio.run(drive())
    statuses = _worker_statuses(bus.events)
    assert statuses[0].payload["online"] is True
    assert statuses[-1].payload["online"] is False
    assert statuses[-1].payload["reason"] == "cancelled"


# ── Explore mode (one intent at a time) ──────────────────────────────────────

def test_cli_solver_explore_mode_produces_structured_facts():
    ch = Challenge(id="t", name="login", category="web", flag_format=r"flag\{.*?\}",
                   target="http://x")
    s = _cli_solver(ch, mode="explore", intent_goal="try SQLi on /login")
    assert s.mode == "explore"
    assert s.intent_goal == "try SQLi on /login"
    # the explore prompt includes the intent goal
    prompt = s._build_explore_prompt()
    assert "try SQLi on /login" in prompt
    assert "VERIFIED_FACT=" in prompt and "DEADEND=" in prompt


def test_extract_structured_facts_parses_markers():
    ch = Challenge(id="t", name="t", category="web", flag_format=r"flag\{.*?\}")
    s = _cli_solver(ch)
    text = (
        "probing /login ...\n"
        "VERIFIED_FACT=login form has no CSRF token\n"
        "VERIFIED_FACT=admin:admin returns 302 → /dashboard\n"
        "DEADEND=XSS on search param is sanitized server-side\n"
        "FOUND_FLAG=flag{easy}\n"
    )
    facts, deadends = s._extract_structured_facts(text)
    assert facts == ["login form has no CSRF token",
                     "admin:admin returns 302 → /dashboard"]
    assert deadends == ["XSS on search param is sanitized server-side"]


def test_extract_structured_facts_empty_on_no_markers():
    ch = Challenge(id="t", name="t", category="web", flag_format=r"flag\{.*?\}")
    s = _cli_solver(ch)
    facts, deadends = s._extract_structured_facts("just prose, no markers")
    assert facts == [] and deadends == []


def test_explore_emits_intent_claimed_and_structured_facts(monkeypatch):
    from muteki.solver import cli_solver as mod
    from muteki.solver.cli_driver import CliResult
    bus = _CaptureBus()
    ch = Challenge(id="t", name="login", category="web", flag_format=r"flag\{.*?\}",
                   target="http://x")
    drv = _StubDriver("")
    transcript = (
        "probed /login\n"
        "VERIFIED_FACT=admin:admin works on /login\n"
        "DEADEND=no SQLi on search param\n"
    )
    s = CliSolver(None, ch, bus=bus, driver=drv, engine="claude", kb=False,
                  mode="explore", intent_goal="try credentials on /login",
                  intent_id="I42")
    canned = lambda *a, **k: CliResult(text=transcript, session="sess-x")
    monkeypatch.setattr(mod, "run_cli_streaming", canned)
    monkeypatch.setattr(mod, "run_cli", canned)
    asyncio.run(s.run())
    kinds = _bb_kinds(bus.events)
    assert "intent_claimed" in kinds
    assert "dead_end" in kinds
    assert "intent_concluded" in kinds
    # structured fact was written
    sg_events = [e for e in bus.events
                 if e.event_type is EventType.SHARED_GRAPH_DELTA]
    assert any("admin:admin" in (e.payload.get("fact") or "") for e in sg_events)


def test_explore_prompt_includes_intent_graph_neighborhood(tmp_path):
    from muteki.swarm.shared_graph import SQLiteSharedGraph

    ch = Challenge(id="t", name="login", category="web", flag_format=r"flag\{.*?\}",
                   target="http://x")
    g = SQLiteSharedGraph.open(db_path=tmp_path / "g.db", challenge=ch)
    root = g.add_evidence(actor="cli-a", source="curl",
                          fact="login leaks admin cookie", verified=True)
    g.propose_intent(actor="reason", intent_id="I-main", goal="use admin cookie",
                     from_fact_seqs=[root])
    g.propose_intent(actor="reason", intent_id="I-sibling", goal="test cookie on /api",
                     from_fact_seqs=[root])
    s = _cli_solver(ch, mode="explore", intent_goal="use admin cookie",
                    intent_id="I-main", shared_graph=g)

    prompt = s._build_explore_prompt()

    assert "Intent graph neighborhood" in prompt
    assert "login leaks admin cookie" in prompt
    assert "I-sibling" in prompt and "test cookie on /api" in prompt
    g.close()


def test_explore_solved_concludes_intent_without_fact_seq(monkeypatch, tmp_path):
    """#13 regression: an explore worker that ACCEPTS a flag but records NO fact-seq
    (only FOUND_FLAG, no VERIFIED_FACT → _last_fact_seq stays unset) must STILL conclude
    its intent (status='done'). The solved branch used to be gated on `lfs is not None`,
    so such an intent stayed status='claimed'; its lease expired and the already-solved
    direction was re-dispatched (run-11190 churn). The other three exits already
    concluded unconditionally — this was the last gated one."""
    from muteki.solver import cli_solver as mod
    from muteki.solver.cli_driver import CliResult

    ch, g = _real_graph(tmp_path)
    g.propose_intent(actor="reason", intent_id="I-solve", goal="probe /admin")
    g.claim_intent(worker="cli-1", intent_id="I-solve", lease_s=1000.0)
    assert "I-solve" in {i for i in _open_or_claimed_ids(g)}

    s = _cli_solver(ch, kb=False, shared_graph=g, mode="explore",
                    intent_goal="probe /admin", intent_id="I-solve")
    # transcript has ONLY a flag — no VERIFIED_FACT → _last_fact_seq never set → lfs None
    def canned(*a, **k):
        raw = "target verifier printed FOUND_FLAG=flag{got_it}\n"
        s._persist_raw_tool_output(raw)
        s._validated_flag_submissions.add("flag{got_it}")
        return CliResult(text="FOUND_FLAG=flag{got_it}\n", session="sess-x")
    monkeypatch.setattr(mod, "run_cli_streaming", canned)
    monkeypatch.setattr(mod, "run_cli", canned)
    out = asyncio.run(s.run())
    assert out.solved is True
    # the intent must be concluded (NOT left open/claimed for re-dispatch)
    assert s._last_fact_seq <= 0, "test premise: no fact-seq was recorded"
    assert _intent_status(g, "I-solve") == "done", \
        "a solved explore intent with no fact-seq must still be concluded"


def _open_or_claimed_ids(g):
    import sqlite3
    con = sqlite3.connect(g.db_path)
    try:
        return [r[0] for r in con.execute(
            "SELECT intent_id FROM intents WHERE status IN ('open','claimed')")]
    finally:
        con.close()


def _intent_status(g, intent_id):
    import sqlite3
    con = sqlite3.connect(g.db_path)
    try:
        row = con.execute("SELECT status FROM intents WHERE intent_id=?",
                          (intent_id,)).fetchone()
        return row[0] if row else None
    finally:
        con.close()


def test_explore_conclude_fallback_fires_on_no_markers(monkeypatch):
    """If the main explore pass produces no structured markers, the conclude
    fallback fires (build_resume with EXPLORE_CONCLUDE_PROMPT) and the
    conclude output is parsed for markers."""
    from muteki.solver import cli_solver as mod
    from muteki.solver.cli_driver import CliResult
    bus = _CaptureBus()
    ch = Challenge(id="t", name="t", category="web", flag_format=r"flag\{.*?\}",
                   target="http://x")
    drv = _StubDriver("")
    call_count = {"n": 0}
    def fake_stream(*a, **k):
        call_count["n"] += 1
        if call_count["n"] == 1:
            # first call: explore, no markers
            return CliResult(text="just probing around\n", session="sess-x")
        # second call: conclude fallback, produces markers
        return CliResult(text="DEADEND=login page has WAF, gave up\n", session="sess-x")
    monkeypatch.setattr(mod, "run_cli_streaming", fake_stream)
    monkeypatch.setattr(mod, "run_cli", fake_stream)
    s = CliSolver(None, ch, bus=bus, driver=drv, engine="claude", kb=False,
                  mode="explore", intent_goal="probe /login")
    asyncio.run(s.run())
    assert call_count["n"] == 2  # main + conclude fallback
    kinds = _bb_kinds(bus.events)
    assert "dead_end" in kinds


# ── live streaming (the deck shows the worker working, not a dead pause) ──────

def test_claude_parse_stream_line_shapes():
    d = ClaudeCodeDriver()
    # assistant text → reasoning step
    s1 = d.parse_stream_line('{"type":"assistant","message":{"content":[{"type":"text","text":"probing /login"}]}}')
    assert s1 and s1.kind == "reasoning" and "probing" in s1.text
    # assistant tool_use → tool step (with the command as detail)
    s2 = d.parse_stream_line('{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash","input":{"command":"curl -s http://x"}}]}}')
    assert s2 and s2.kind == "tool" and s2.tool == "Bash" and "curl" in s2.text
    # tool_result → tool_result step
    s3 = d.parse_stream_line('{"type":"user","message":{"content":[{"type":"tool_result","content":"HTTP 200 OK"}]}}')
    assert s3 and s3.kind == "tool_result" and "200" in s3.text
    # session id surfaced
    s4 = d.parse_stream_line('{"type":"system","subtype":"init","session_id":"abc"}')
    assert s4 and s4.kind == "session" and s4.session == "abc"
    # noise → None
    assert d.parse_stream_line("not json") is None


def test_codex_parse_stream_line_shapes():
    d = CodexDriver()
    assert d.parse_stream_line('{"type":"thread.started","thread_id":"th1"}').session == "th1"
    cmd = d.parse_stream_line('{"type":"item.started","item":{"type":"command_execution","command":"curl x"}}')
    assert cmd and cmd.kind == "tool" and "curl" in cmd.text
    res = d.parse_stream_line('{"type":"item.completed","item":{"type":"command_execution","aggregated_output":"hi out"}}')
    assert res and res.kind == "tool_result" and "hi out" in res.text
    msg = d.parse_stream_line('{"type":"item.completed","item":{"type":"agent_message","text":"thinking..."}}')
    assert msg and msg.kind == "reasoning" and "thinking" in msg.text


def test_claude_stream_parse_recovers_result_from_jsonl():
    # parse() must still pull the final flag/cost/session out of a stream-json dump
    d = ClaudeCodeDriver()
    dump = "\n".join([
        '{"type":"system","subtype":"init","session_id":"s9"}',
        '{"type":"assistant","message":{"content":[{"type":"text","text":"found it"}]}}',
        '{"type":"result","result":"FOUND_FLAG=flag{stream}","total_cost_usd":0.5,"num_turns":4,"session_id":"s9"}',
    ])
    r = d.parse(dump, "")
    assert "FOUND_FLAG=flag{stream}" in r.text
    assert r.session == "s9" and r.cost_usd == 0.5 and r.num_turns == 4


def test_run_cli_streaming_fires_on_step_per_line(tmp_path):
    # end-to-end: a fake echo command emits two JSONL lines; on_step sees both,
    # and parse() builds the final result from the accumulated stdout.
    from muteki.solver.cli_driver import run_cli_streaming, StreamStep
    d = ClaudeCodeDriver()
    line1 = '{"type":"assistant","message":{"content":[{"type":"text","text":"step one"}]}}'
    line2 = '{"type":"result","result":"FOUND_FLAG=flag{ok}","session_id":"z"}'
    script = tmp_path / "fake.sh"
    script.write_text(f"#!/bin/sh\necho '{line1}'\necho '{line2}'\n")
    script.chmod(0o755)
    seen = []
    res = run_cli_streaming(d, ["/bin/sh", str(script)], cwd=str(tmp_path),
                            timeout=10, on_step=lambda s: seen.append(s.kind))
    assert "reasoning" in seen  # the live step fired
    assert "FOUND_FLAG=flag{ok}" in res.text  # final result still parsed
    assert res.session == "z"


def test_run_cli_streaming_pipes_secret_without_putting_it_in_argv(tmp_path):
    from muteki.solver.cli_driver import run_cli_streaming

    secret = "stdin-only-secret-5544332211"
    seen_argv = []
    delivery = []
    # Claude's parser tolerates a plain line as its final text fallback.
    res = run_cli_streaming(
        ClaudeCodeDriver(),
        ["/bin/sh", "-c", "IFS= read -r line; printf '%s\\n' \"$line\""],
        cwd=str(tmp_path), timeout=10, on_step=lambda _s: None,
        on_proc=lambda proc: seen_argv.extend(proc.args), stdin_text=secret + "\n",
        on_stdin_delivered=lambda: delivery.append("delivered"),
        on_stdin_uncertain=lambda: delivery.append("uncertain"),
    )
    assert secret in res.text
    assert secret not in "\0".join(str(arg) for arg in seen_argv)
    assert delivery == ["delivered"]


def test_run_cli_streaming_broken_stdin_is_uncertain_not_delivered(tmp_path):
    from muteki.solver.cli_driver import run_cli_streaming

    delivery = []
    # Larger than the pipe buffer + an immediate-exit child makes the writer observe
    # BrokenPipe. Popen alone must never manufacture a positive delivery receipt.
    run_cli_streaming(
        ClaudeCodeDriver(), ["/bin/sh", "-c", "exit 0"], cwd=str(tmp_path), timeout=10,
        on_step=lambda _s: None, stdin_text="x" * (4 * 1024 * 1024),
        on_stdin_delivered=lambda: delivery.append("delivered"),
        on_stdin_uncertain=lambda: delivery.append("uncertain"),
    )
    assert delivery == ["uncertain"]


# ── dispatcher control: blackboard context + cancel + pause ───────────────────

class _StubGraph:
    """A minimal SharedGraph stand-in: a fixed to_summary() + capture writes."""
    def __init__(self, summary):
        self._summary = summary
        self.facts = []
        self.dead_ends = []
        self.db_path = "/tmp/none.db"
    def to_summary(self, *a, **k): return self._summary
    def add_evidence(self, **kw): self.facts.append(kw); return 1
    def add_dead_end(self, **kw): self.dead_ends.append(kw); return 1


def test_board_context_fallback_inlines_when_no_file_written():
    # Board file-handoff: when the loop hasn't written the board file yet
    # (_board_file_written is falsey), _board_context falls back to an INLINE
    # bounded summary so the worker is never blind. _StubGraph only has to_summary,
    # so this exercises exactly that fallback path.
    ch = Challenge(id="t", name="login", category="web", flag_format=r"flag\{.*?\}",
                   target="http://x")
    sg = _StubGraph("FACTS:\n- admin panel at /admin\nDEAD ENDS:\n- /login has a WAF")
    s = _cli_solver(ch, kb=False, shared_graph=sg)
    boot = s._build_prompt()
    expl = _cli_solver(ch, kb=False, shared_graph=sg, mode="explore",
                       intent_goal="probe /admin")._build_explore_prompt()
    for prompt in (boot, expl):
        assert "Shared team board" in prompt
        assert "admin panel at /admin" in prompt        # inline fallback carries facts
        assert "/login has a WAF" in prompt


def test_flag_hint_token_mode_does_not_say_brace(monkeypatch):
    # run-11189: a token/collect challenge's prompt must NOT tell the worker the
    # flag is shaped like flag{...} (it has none) — that suppresses FOUND_FLAG=.
    ch_tok = Challenge(id="t", name="ladder", category="misc", flag_format="token",
                       multi_flag=True, expected_flags=14)
    p = _cli_solver(ch_tok, kb=False)._build_prompt()
    # the token hint tells the worker the flag is a bare token, NOT shaped like flag{...}
    assert "bare token" in p
    assert "shaped like flag{...}" not in p   # the old misleading instruction is gone
    # brace challenge keeps the exact old wording.
    ch_brace = Challenge(id="t", name="web", category="web", flag_format=r"flag\{.*?\}",
                         target="http://x")
    pb = _cli_solver(ch_brace, kb=False)._build_prompt()
    assert "shaped like flag{...}" in pb

    # multi-flag is a collection mode, not a token-format signal. Common CTF
    # challenges can require several ordinary flag{...} values.
    ch_multi_brace = Challenge(
        id="t", name="multi-web", category="web",
        flag_format=r"[A-Za-z0-9_]{0,15}\{[^}]{1,200}\}",
        multi_flag=True, expected_flags=3, target="http://x")
    pm = _cli_solver(ch_multi_brace, kb=False)._build_prompt()
    assert "shaped like flag{...}" in pm
    assert "bare token" not in pm


def test_flag_hint_uses_custom_wrapper_hint():
    ch = Challenge(
        id="t",
        name="custom",
        category="web",
        target="http://x",
        flag_format_hint="WMCTF{...}",
    )
    prompt = _cli_solver(ch, kb=False)._build_prompt()
    assert "The flag is shaped like WMCTF{...}" in prompt
    assert "The flag is shaped like flag{...}" not in prompt


def test_board_context_pointer_when_file_written():
    # When the loop HAS written the board file, the prompt carries a POINTER to
    # ./.muteki_board.md (+ the bounded credential digest), NOT the full inline body.
    ch = Challenge(id="t", name="login", category="web", flag_format=r"flag\{.*?\}",
                   target="http://x")
    sg = _StubGraph("FACTS:\n- admin panel at /admin")
    s = _cli_solver(ch, kb=False, shared_graph=sg)
    s._board_file_written = True            # simulate the loop's per-turn write
    prompt = s._build_prompt()
    assert "Shared team board" in prompt
    assert ".muteki_board.md" in prompt     # the pointer
    assert "READ IT FIRST" in prompt
    assert 'python3 "$MUTEKI_BLACKBOARD_SCRIPT" read-review' in prompt
    assert 'python3 "$MUTEKI_BLACKBOARD_SCRIPT" read-deadends' in prompt
    assert 'python3 "$MUTEKI_BLACKBOARD_SCRIPT" read-facts' in prompt
    assert 'python3 "$MUTEKI_BLACKBOARD_SCRIPT" write-fact "<fact>" --verified' in prompt
    # the full inline fact body is NOT dumped into the prompt (it's in the file)
    assert "admin panel at /admin" not in prompt


def test_board_context_empty_when_no_graph_or_empty_summary():
    ch = Challenge(id="t", name="t", category="web", flag_format=r"flag\{.*?\}")
    # no shared graph at all → no board section
    assert "Shared team board" not in _cli_solver(ch, kb=False)._build_prompt()
    # graph present but board empty → still no section (don't inject noise)
    s = _cli_solver(ch, kb=False, shared_graph=_StubGraph("   "))
    assert "Shared team board" not in s._build_prompt()


# ── Board file-handoff (DESIGN_board_file_handoff) ───────────────────────────
# Full board written to a workdir file + pointer/digest in the prompt; the chain
# is derived from VERIFIED fact TEXT (P2A), never from graph edges. These use a
# REAL SQLiteSharedGraph so the file body / extraction is exercised end to end.

import tempfile  # noqa: E402
from pathlib import Path as _P  # noqa: E402

from muteki.swarm.shared_graph import SQLiteSharedGraph  # noqa: E402
from muteki.solver.workspace import materialize_shared_artifact  # noqa: E402


def _real_graph(tmp_path, facts=(), deadends=()):
    ch = Challenge(id="t", name="ghost", category="misc", points=0,
                   flag_format=r"flag\{.*?\}")
    g = SQLiteSharedGraph(str(_P(tmp_path) / "sg.db"), ch)
    for actor, fact, verified in facts:
        g.add_evidence(actor=actor, source=actor.split("-")[-1], fact=fact,
                       verified=verified)
    for reason in deadends:
        g.add_dead_end(actor="cli-x", reason=reason)
    return ch, g


def _blackboard_submit(solver: CliSolver, flag: str) -> subprocess.CompletedProcess:
    if solver._flag_submission_dir is None:
        solver._flag_submission_dir = _P(tempfile.mkdtemp(
            prefix="muteki-flag-submission-test-"))
    env = {
        **os.environ,
        "MUTEKI_BLACKBOARD_DB": str(solver.shared_graph.db_path),
        "MUTEKI_WORKER_ID": solver.solver_id,
        "MUTEKI_FLAG_SUBMISSION_DIR": str(solver._flag_submission_dir),
    }
    return subprocess.run(
        [sys.executable, solver._blackboard_script_path(), "submit-flag", flag],
        capture_output=True, text=True, env=env, timeout=30, check=True)


def test_submit_flag_is_the_only_protocol1_acceptance_path(tmp_path):
    flag = "flag{api_only_acceptance}"
    ch, graph = _real_graph(tmp_path)
    solver = _cli_solver(ch, kb=False, shared_graph=graph, bus=_CaptureBus())
    raw = f"decoder output\nFOUND_FLAG={flag}\n"

    _emit_attributed_tool_result(
        solver, raw, command="python decode.py", submit=False)
    assert solver._already_found == set()
    assert graph.snapshot().flags == []

    submitted = _blackboard_submit(solver, flag)
    assert flag not in submitted.stdout
    asyncio.run(solver._drain_blackboard_flag_submissions())

    assert solver._already_found == {flag}
    assert solver._stream_accepted == [flag]
    assert graph.snapshot().flags == [flag]
    decisions = [
        event for event in graph.events()
        if event["kind"] == "flag_submission_decision"
    ]
    assert decisions[-1]["payload"]["accepted"] is True


def test_submit_confirmation_cannot_prove_its_own_candidate(tmp_path):
    flag = "flag{confirmation_is_not_evidence}"
    ch, graph = _real_graph(tmp_path)
    solver = _cli_solver(ch, kb=False, shared_graph=graph, bus=_CaptureBus())
    submitted = _blackboard_submit(solver, flag)
    assert flag not in submitted.stdout

    _emit_attributed_tool_result(
        solver,
        submitted.stdout,
        command=(f"python3 $MUTEKI_BLACKBOARD_SCRIPT submit-flag '{flag}'"),
        submit=False,
    )

    assert solver._already_found == set()
    assert graph.snapshot().flags == []
    decisions = [
        event for event in graph.events()
        if event["kind"] == "flag_submission_decision"
    ]
    assert decisions[-1]["payload"]["accepted"] is False
    assert decisions[-1]["payload"]["code"] == "provenance_rejected"


def test_write_board_file_full_untruncated(tmp_path):
    # the file holds ALL facts with no [-16]/[:2000] truncation, creds on top.
    facts = [("cli-c", f"ghost{i} login succeeds with password PW{i}aB; whoami ghost{i}",
              True) for i in range(40)]  # 40 > the old 16-fact cap
    ch, g = _real_graph(tmp_path, facts=facts)
    s = _cli_solver(ch, kb=False, shared_graph=g)
    wd = _P(tempfile.mkdtemp())
    assert s._write_board_file(wd) is True
    body = (wd / ".muteki_board.md").read_text()
    assert "muteki-team-board" in body            # sentinel
    assert "Recovered credentials" in body         # P2A section on top
    assert "PW0aB" in body and "PW39aB" in body     # FIRST and LAST fact both present
    import shutil; shutil.rmtree(wd, ignore_errors=True)


def test_write_board_file_collision_does_not_clobber(tmp_path):
    ch, g = _real_graph(tmp_path, facts=[("cli-c", "ghost1 password PWaB1xyz works, whoami ghost1", True)])
    s = _cli_solver(ch, kb=False, shared_graph=g)
    wd = _P(tempfile.mkdtemp())
    (wd / ".muteki_board.md").write_text("CHALLENGE DATA not a board")
    assert s._write_board_file(wd) is False          # refuses to clobber
    assert "CHALLENGE DATA" in (wd / ".muteki_board.md").read_text()
    import shutil; shutil.rmtree(wd, ignore_errors=True)


def test_write_board_file_rewrites_own_file(tmp_path):
    # our own board file (sentinel present) IS overwritten on the next turn → fresh.
    ch, g = _real_graph(tmp_path, facts=[("cli-c", "ghost1 password PWaB1xyz works, whoami ghost1", True)])
    s = _cli_solver(ch, kb=False, shared_graph=g)
    wd = _P(tempfile.mkdtemp())
    assert s._write_board_file(wd) is True
    g.add_evidence(actor="cli-c", source="c",
                   fact="ghost2 password PWaB2xyz works, logged in as ghost2", verified=True)
    assert s._write_board_file(wd) is True           # overwrites own file
    assert "ghost2:PWaB2xyz" in (wd / ".muteki_board.md").read_text()
    import shutil; shutil.rmtree(wd, ignore_errors=True)


def test_write_board_file_is_run_level_single_file_symlinked_to_workers(tmp_path):
    ch, g = _real_graph(tmp_path, facts=[("cli-c", "ghost1 password PWaB1xyz works, whoami ghost1", True)])
    s = _cli_solver(ch, kb=False, shared_graph=g)
    workspace = _P(tmp_path) / "workspace"
    wd1 = workspace / "workers" / "cli-claude-1"
    wd2 = workspace / "workers" / "cli-codex-2"
    assert s._write_board_file(wd1) is True
    assert s._write_board_file(wd2) is True
    root_board = workspace / ".muteki_board.md"
    assert root_board.exists()
    assert (wd1 / ".muteki_board.md").is_symlink()
    assert (wd2 / ".muteki_board.md").is_symlink()
    assert (wd1 / ".muteki_board.md").resolve() == root_board
    assert (wd2 / ".muteki_board.md").resolve() == root_board
    assert root_board.stat().st_ino == (wd1 / ".muteki_board.md").resolve().stat().st_ino


def test_defect0_accept_flag_records_on_shared_graph(tmp_path):
    """P0 defect-0: _accept_flag must write the flag to the SHARED graph (not only
    the worker's local SolveGraph), so reason/board/progress can read real flag
    progress from the graph. Before the fix, shared_graph.flag_found had 0 callers
    and snapshot().flags was always empty (run-11190 RUN_FINISHED empty flags)."""
    ch, g = _real_graph(tmp_path)
    s = _cli_solver(ch, kb=False, shared_graph=g)
    assert g.snapshot().flags == []                       # nothing yet
    s._validated_flag_submissions.update({
        "flag{real_one}", "flag{second}"})
    assert asyncio.run(s._accept_flag("flag{real_one}")) is True
    assert g.snapshot().flags == ["flag{real_one}"]       # now on the SHARED graph
    # idempotent: same flag again is a no-op (dedup), graph still has exactly one
    assert asyncio.run(s._accept_flag("flag{real_one}")) is False
    assert g.snapshot().flags == ["flag{real_one}"]
    # a second distinct flag accumulates (multi-flag)
    assert asyncio.run(s._accept_flag("flag{second}")) is True
    assert g.snapshot().flags == ["flag{real_one}", "flag{second}"]


@pytest.mark.parametrize(
    ("protocol2", "legacy_visible"),
    [(False, True), (True, False)],
    ids=["protocol1", "protocol2"],
)
def test_accepted_flag_protocol_channel_matrix(
    tmp_path, protocol2, legacy_visible,
):
    """Protocol 2 accepts canonically but leaves every legacy flag channel silent."""
    from muteki.swarm.insight_bus import InsightBus, InsightKind

    flag = "flag{channel_matrix}"
    raw = f"decoder output\nFOUND_FLAG={flag}\n"
    challenge, shared_graph = _real_graph(tmp_path)
    event_bus = _CaptureBus()
    insight_bus = InsightBus(challenge_id=challenge.id)
    solver = _cli_solver(
        challenge,
        kb=False,
        bus=event_bus,
        insight=insight_bus,
        shared_graph=shared_graph,
    )
    gate_calls = []
    capture_calls = []
    if protocol2:
        solver._protocol2_mode = True

        def capture(value):
            capture_calls.append(value)
            return hashlib.sha256(value.encode()).hexdigest()

        def canonical_gate(candidate, provenance):
            gate_calls.append((candidate, provenance))
            return True

        solver._protocol2_capture_callback = capture
        solver._protocol2_gate_callback = canonical_gate

    # Content-bearing legacy events are private in Protocol 2, but ordinary
    # lifecycle telemetry must remain available in both protocols.
    asyncio.run(solver._emit(
        EventType.WORKER_LIFECYCLE, phase="running", state="active"))
    _emit_attributed_tool_result(
        solver, raw, command="python solve.py", call_id="channel-matrix")

    # The hardcoded provenance gate ran in both protocols. Protocol 2 additionally
    # reached its GateAuthority callback before the private acceptance bookkeeping.
    assert solver._already_found == {flag}
    assert solver._stream_accepted == [flag]
    assert capture_calls == ([raw] if protocol2 else [])
    assert gate_calls == ([(flag, raw)] if protocol2 else [])
    assert any(
        event.event_type is EventType.WORKER_LIFECYCLE
        and event.payload == {"phase": "running", "state": "active"}
        for event in event_bus.events
    )

    legacy_channels = {
        "tool_result_flag": any(
            event.event_type is EventType.TOOL_CALL_RESULT
            and flag in json.dumps(event.payload, ensure_ascii=False)
            for event in event_bus.events
        ),
        "solve_graph_flag": flag in solver.graph.flags,
        "shared_graph_flag_found": any(
            event["kind"] == "flag_found"
            and event["payload"].get("flag") == flag
            for event in shared_graph.events()
        ),
        "solvegraph_delta_flag": any(
            event.event_type is EventType.SOLVE_GRAPH_DELTA
            and event.payload.get("kind") == "flag"
            and event.payload.get("flag") == flag
            for event in event_bus.events
        ),
        "insight_event_flag_found": any(
            event.event_type is EventType.INSIGHT_BUS_EVENT
            and event.payload.get("kind") == "FlagFound"
            and event.payload.get("flag") == flag
            for event in event_bus.events
        ),
        "blackboard_delta_flag_found": any(
            event.event_type is EventType.BLACKBOARD_DELTA
            and event.payload.get("kind") == "flag_found"
            and event.payload.get("flag") == flag
            for event in event_bus.events
        ),
        "insight_bus_flag": any(
            insight.kind is InsightKind.FLAG and insight.text == flag
            for insight in insight_bus.history
        ),
    }
    assert legacy_channels == {
        channel: legacy_visible for channel in legacy_channels
    }

    if protocol2:
        # Protocol2RunSession consumes this private same-attempt list, resolves the
        # committed flag.accepted outbox, and hands it to the typed reconciler.
        assert solver._accepted_flags_for_outcome() == [flag]
        assert solver._protocol2_accepted_flags == [flag]
    else:
        assert solver._accepted_flags_for_outcome() == [flag]
        assert shared_graph.snapshot().flags == [flag]


def test_defect1_solved_claim_downgraded_not_verified(tmp_path):
    """P0 defect-1: a bare 'solved / 已解 / task complete' claim must NOT become a
    VERIFIED evidence fact (run-42599: 28 solved-like verified facts poisoned the
    board). It's downgraded to an unverified candidate; only the flag gate decides
    completion."""
    ch, g = _real_graph(tmp_path)
    s = _cli_solver(ch, kb=False, shared_graph=g)

    # a completion CLAIM with no flag → recorded UNVERIFIED (downgraded)
    asyncio.run(s._record_fact("the challenge is solved, task complete",
                               verified=True, artifact_id=""))
    snap = g.snapshot()
    claims = [e for e in g.events() if e["kind"] == "fact_added"]
    assert claims and claims[-1]["verified"] is False, \
        "a solved-claim must be downgraded to unverified, not trusted as evidence"

    # a 已解 claim (zh) likewise downgraded
    asyncio.run(s._record_fact("已解，本来就不需要打 .154", verified=True, artifact_id=""))
    zh = [e for e in g.events() if e["kind"] == "fact_added"][-1]
    assert zh["verified"] is False

    # REAL evidence (no completion claim) stays verified — not over-broad
    asyncio.run(s._record_fact("admin panel reachable at /admin, returns 200",
                               verified=True, artifact_id="art1"))
    real = [e for e in g.events() if e["kind"] == "fact_added"][-1]
    assert real["verified"] is True, "concrete evidence must stay verified"

    # once this worker holds a real gated flag, its claims are earned → not downgraded
    s._validated_flag_submissions.add("flag{got_it}")
    asyncio.run(s._accept_flag("flag{got_it}"))
    asyncio.run(s._record_fact("challenge solved — flag recovered",
                               verified=True, artifact_id="art2"))
    earned = [e for e in g.events() if e["kind"] == "fact_added"][-1]
    assert earned["verified"] is True


def test_defect2_progress_block_reads_shared_graph(tmp_path):
    """P0 defect-2: multi-flag PROGRESS block reads N/total from the SHARED graph
    (defect-0 made it the durable source) — so a worker sees flags a teammate found,
    not just its own _already_found. Single-flag challenge → empty (byte-identical)."""
    ch = Challenge(id="t", name="multi", category="web", points=0,
                   flag_format=r"flag\{.*?\}", expected_flags=3)
    _, g = _real_graph(tmp_path)
    s = _cli_solver(ch, kb=False, shared_graph=g)

    # a teammate found one flag → it's on the shared graph, NOT this worker's set
    g.flag_found(actor="cli-sibling", flag="flag{one}")
    block = s._team_context_block()
    assert "3 flags" in block and "1/3 captured" in block and "2 remaining" in block
    assert "flag{one}" in block                      # teammate's flag surfaced
    # and it's injected into BOTH prompt builders (was only _build_prompt before)
    assert "1/3 captured" in s._build_prompt()
    assert "1/3 captured" in s._build_explore_prompt()


def test_defect2_single_flag_block_empty(tmp_path):
    """defect-2: a single-flag challenge gets NO progress block (byte-identical to
    the pre-multi-flag prompt)."""
    ch = Challenge(id="t", name="single", category="web", points=0,
                   flag_format=r"flag\{.*?\}")  # expected_flags defaults to 1
    _, g = _real_graph(tmp_path)
    s = _cli_solver(ch, kb=False, shared_graph=g)
    assert s._team_context_block() == ""


def test_defect8_unbacked_evidence_downgraded_to_candidate(tmp_path):
    """P0 defect-8: a VERIFIED evidence fact with NO provenance artifact is an
    unbacked assertion (the no-evidence hallucination) → downgraded to an unverified
    candidate. A fact WITH an artifact stays verified."""
    ch, g = _real_graph(tmp_path)
    s = _cli_solver(ch, kb=False, shared_graph=g)

    # no artifact → downgraded
    asyncio.run(s._record_fact("the database is sqlite", verified=True, artifact_id=""))
    no_art = [e for e in g.events() if e["kind"] == "fact_added"][-1]
    assert no_art["verified"] is False, "unbacked verified fact must be downgraded"

    # with artifact → stays verified
    asyncio.run(s._record_fact("admin:hunter2 logs in (HTTP 302 to /dashboard)",
                               verified=True, artifact_id="art-real"))
    art = [e for e in g.events() if e["kind"] == "fact_added"][-1]
    assert art["verified"] is True, "evidence backed by an artifact stays verified"


def test_write_board_file_no_graph_returns_false():
    s = _cli_solver(Challenge(id="t", name="t", category="web",
                              flag_format=r"flag\{.*?\}"), kb=False)
    wd = _P(tempfile.mkdtemp())
    assert s._write_board_file(wd) is False
    assert not (wd / ".muteki_board.md").exists()
    assert s._board_context() == ""                  # no dangling pointer
    import shutil; shutil.rmtree(wd, ignore_errors=True)


def test_write_board_file_failure_falls_back_to_inline(tmp_path):
    # if the write raises, _board_context must NOT emit a pointer — it inlines.
    ch, g = _real_graph(tmp_path, facts=[("cli-c", "ghost1 password PWaB1xyz works, whoami ghost1", True)])
    s = _cli_solver(ch, kb=False, shared_graph=g)
    # point the write at a path that can't be created (a file as a parent dir)
    bad_parent = _P(tempfile.mktemp()); bad_parent.write_text("x")
    assert s._write_board_file(bad_parent / "sub") is False
    assert s._board_file_written is False
    prompt = s._build_prompt()
    assert ".muteki_board.md" not in prompt          # NO dangling pointer
    assert "Shared team board" in prompt             # but inline fallback present
    import os as _os; _os.remove(bad_parent)


def test_canonical_credentials_verified_only_and_skips_failures(tmp_path):
    facts = [
        ("cli-c", "ghost1 login SUCCEEDS with password W3lc0m3T0Gh0st, whoami ghost1", True),
        ("cli-c", "ghost2 password a1e7c9d4f2b8 is DENIED for ghost2 login", True),   # FAIL
        ("cli-c", "ghost3 guess maybe ghost3pw unverified", False),                    # unverified
        ("cli-c", "H1dd3nInSh4dow successfully authenticates as ghost3, whoami ghost3", True),
        ("cli-c", "ghost1 hex file ./- contains a1e7c9d4f2b8, a decoy fragment", True),# decoy
    ]
    ch, g = _real_graph(tmp_path, facts=facts)
    creds = g.canonical_credentials()
    got = {c["entity"]: c["value"] for c in creds}
    assert got.get("ghost1") == "W3lc0m3T0Gh0st"
    assert got.get("ghost3") == "H1dd3nInSh4dow"
    assert "ghost2" not in got                        # the DENIED guess is NOT promoted
    # the failed/decoy token never appears as a recovered credential value
    assert all(c["value"] != "a1e7c9d4f2b8" for c in creds)


def test_canonical_credentials_rejects_ssh_config_flags(tmp_path):
    # run-10070 regression: an SSH-options fact produced a false-positive
    # `ghost0:Authentication=no` row. SSH/config flag assignments are not passwords.
    facts = [
        ("cli-c", "ghost0/ghost1 登录成功；除 -o PubkeyAuthentication=no -o "
                  "StrictHostKeyChecking=no 外还需 IdentitiesOnly=yes", True),
        ("cli-c", "ghost1 login succeeds with password W3lc0m3T0Gh0st; whoami ghost1", True),
    ]
    ch, g = _real_graph(tmp_path, facts=facts)
    creds = {c["entity"]: c["value"] for c in g.canonical_credentials()}
    assert "ghost0" not in creds or "uthentication" not in creds.get("ghost0", "")
    assert all("=no" not in c["value"] and "=yes" not in c["value"]
               for c in g.canonical_credentials())
    assert creds.get("ghost1") == "W3lc0m3T0Gh0st"     # real cred preserved


def test_canonical_credentials_attributes_to_unlocked_entity(tmp_path):
    # run-10067 case: a cred found in ghost2's home that UNLOCKS ghost3 must be
    # attributed to ghost3, not ghost2 (the box it was discovered on). The
    # "authenticates as ghostN" target wins over the leading "found in" entity.
    facts = [
        ("cli-c", "D4shIsN0tAFl4g successfully authenticates as ghost2; whoami ghost2", True),
        ("cli-c", "ghost2 hidden lead .source_omega contains H1dd3nInSh4dow; "
                  "it authenticates as ghost3", True),
    ]
    ch, g = _real_graph(tmp_path, facts=facts)
    creds = {c["entity"]: c["value"] for c in g.canonical_credentials()}
    assert creds.get("ghost2") == "D4shIsN0tAFl4g"
    assert creds.get("ghost3") == "H1dd3nInSh4dow"     # NOT mis-attributed to ghost2


def test_canonical_credentials_newest_wins_per_entity(tmp_path):
    facts = [
        ("cli-c", "ghost2 password OLDvalA1 works, whoami ghost2", True),
        ("cli-c", "correction: ghost2 password NEWvalB2 authenticates, logged in", True),
    ]
    ch, g = _real_graph(tmp_path, facts=facts)
    creds = {c["entity"]: c["value"] for c in g.canonical_credentials()}
    assert creds["ghost2"] == "NEWvalB2"              # newest verified fact wins


def test_board_file_carries_full_untruncated_brief(tmp_path):
    # run-10070 regression: the SSH/target block lives deep in the challenge
    # description (past the 300-char to_summary cap). The board FILE must carry the
    # full brief so a worker reads the target there instead of grepping session
    # files. Build a long description with the connection block near the end.
    head = "Ghost wargame. " + ("filler narrative. " * 60)   # > 300 chars of preamble
    ssh = "SSH Access Host 204.168.229.209 Port 2222 User ghost0 Password ghost0"
    ch = Challenge(id="t", name="ghost", category="misc", points=0,
                   flag_format=r"flag\{.*?\}", description=head + ssh)
    g = SQLiteSharedGraph(str(_P(tmp_path) / "sg.db"), ch)
    body = g.to_board_markdown()
    assert "204.168.229.209" in body          # the host survives (not capped at 300)
    assert "Password ghost0" in body
    assert "Challenge brief (full" in body


def test_poc_save_quarantines_flag_and_secret_literals(tmp_path):
    ch = Challenge(id="poc-q", name="poc-q", category="web",
                   flag_format=r"flag\{[^}]+\}")
    root = _P(tmp_path) / "run" / "workspace"
    wd = root / "workers" / "cli-1"
    wd.mkdir(parents=True)
    g = SQLiteSharedGraph(str(root / "graph" / "shared_graph.db"), ch)
    poc = wd / "poc.py"
    poc.write_text("print('flag{stale_from_prior}')\nAPI_KEY='sk-secret-value-12345'\n")
    s = _cli_solver(ch, kb=False, shared_graph=g, workdir=str(wd))

    asyncio.run(s._stream_markers("POC_SAVE=poc.py|python poc.py|available|first cut\n"))

    rows = g.pocs()
    assert rows[0]["status"] == "quarantined"
    saved = (root / rows[0]["path"]).read_text()
    assert "flag{stale_from_prior}" not in saved
    assert "sk-secret-value-12345" not in saved
    assert "<PRIOR_FLAG>" in saved and "<SECRET>" in saved


def test_inherited_poc_mounts_under_inherited_and_claims(tmp_path):
    ch = Challenge(id="poc-inherit", name="poc-inherit", category="web",
                   flag_format=r"flag\{[^}]+\}")
    root = _P(tmp_path) / "run" / "workspace"
    src_dir = _P(tmp_path) / "src"
    src_dir.mkdir()
    src = src_dir / "poc.py"
    src.write_text("print('hit target')\n")
    g = SQLiteSharedGraph(str(root / "graph" / "shared_graph.db"), ch)
    art = materialize_shared_artifact(root, src, name="poc.py", kind="poc",
                                      status="available")
    g.save_poc(actor="cli-a", poc_id="poc-abc", path=art["path"],
               artifact_id=art["sha256"], entry_command="python poc.py",
               status="available", note="works on /admin", name="poc.py")
    wd = root / "workers" / "cli-2"
    wd.mkdir(parents=True)
    s = _cli_solver(ch, kb=False, shared_graph=g, workdir=str(wd))

    s._stage_attachments(wd)

    inherited = wd / "inherited" / "poc-abc" / "poc.py"
    assert inherited.is_symlink()
    assert inherited.resolve() == (root / art["path"]).resolve()
    assert g.pocs()[0]["status"] == "wip"
    assert "python poc.py" in s._poc_prompt_block()


def test_stage_attachment_preserves_cas_alias_basename(tmp_path):
    """A run-local by-name symlink resolves to a digest object, but the worker
    contract must remain the harmless player filename rather than leaking the hash
    or the operator's original source path."""
    from muteki.solver.workspace import materialize_input

    source_root = _P(tmp_path) / "operator-benchmark-with-solutions"
    source_root.mkdir()
    source = source_root / "ciphertext.txt"
    source.write_text("player bytes")
    root = _P(tmp_path) / "run" / "workspace"
    row = materialize_input(root, source, name=source.name)
    ch = Challenge(
        id="cas-alias", name="cas-alias", category="crypto",
        attachments=[str(row["by_name"])],
    )
    wd = root / "workers" / "cli-1"
    wd.mkdir(parents=True)
    s = _cli_solver(ch, kb=False, workdir=str(wd))

    assert s._stage_attachments(wd) == ["ciphertext.txt"]
    assert (wd / "ciphertext.txt").read_text() == "player bytes"


def test_offline_prompt_forbids_benchmark_and_solution_traversal(tmp_path):
    ch = Challenge(
        id="offline-boundary", name="offline-boundary", category="crypto",
        attachments=[str(_P(tmp_path) / "ciphertext.txt")],
    )
    s = _cli_solver(ch, kb=False, web_access=False)
    s._staged_files = ["ciphertext.txt"]

    prompt = s._build_prompt()

    assert "Offline black-box evaluation boundary" in prompt
    assert "Do NOT inspect parent directories" in prompt
    assert "reference solvers" in prompt


def test_to_board_markdown_has_creds_facts_and_intents(tmp_path):
    ch, g = _real_graph(
        tmp_path,
        facts=[("cli-c", "ghost1 password PWaB1xyz works, whoami ghost1", True)],
        deadends=["port 9999 closed"])
    g.propose_intent(actor="reason", intent_id="I1", goal="probe ghost2 home dir")
    body = g.to_board_markdown()
    assert "Recovered credentials" in body
    assert "ghost1:PWaB1xyz" in body
    assert "port 9999 closed" in body                 # dead-ends rendered
    assert "Open intents" in body and "probe ghost2 home dir" in body


def test_planner_gets_untruncated_summary(tmp_path):
    # P1.5: to_summary(max_evidence=10**9) shows ALL facts (the planner was capped
    # at 16, the re-work generator was blind on a long chain).
    facts = [("cli-c", f"fact number {i} confirmed", True) for i in range(30)]
    ch, g = _real_graph(tmp_path, facts=facts)
    full = g.to_summary(max_evidence=10**9)
    assert "fact number 0 confirmed" in full          # earliest survives
    assert "fact number 29 confirmed" in full
    capped = g.to_summary()                            # default 16 → early ones dropped
    assert "fact number 0 confirmed" not in capped


def test_run_loop_writes_board_file_into_worker_cwd(monkeypatch, tmp_path):
    # END-TO-END: a real CliSolver.run() loop, real SQLiteSharedGraph pre-seeded
    # with the unlock chain, explicit workdir. Assert .muteki_board.md actually
    # lands in the worker's cwd with the credential chain — the full P1 wiring.
    from muteki.solver import cli_solver as mod
    from muteki.solver.cli_driver import CliResult

    facts = [("cli-c", f"ghost{i} login succeeds with password PWvalue{i}; whoami ghost{i}",
              True) for i in range(20)]   # 20 > old 16-cap → proves no truncation
    ch, g = _real_graph(tmp_path, facts=facts)
    wd = _P(tempfile.mkdtemp())
    bus = _CaptureBus()
    s = CliSolver(None, ch, bus=bus, driver=_StubDriver(""), engine="claude",
                  kb=False, shared_graph=g, workdir=str(wd))
    canned = lambda *a, **k: CliResult(text="FOUND_FLAG=NONE\n", session="sess-x")
    monkeypatch.setattr(mod, "run_cli_streaming", canned)
    monkeypatch.setattr(mod, "run_cli", canned)
    asyncio.run(s.run())

    board = wd / ".muteki_board.md"
    assert board.exists(), "board file was not written into the worker cwd"
    body = board.read_text()
    assert "ghost0:PWvalue0" in body and "ghost19:PWvalue19" in body  # first+last, no truncation
    assert "muteki-team-board" in body
    import shutil; shutil.rmtree(wd, ignore_errors=True)


def test_bootstrap_extracts_structured_facts(monkeypatch):
    # bug #1 fix: bootstrap workers now contribute structured facts/dead-ends to
    # the board AS THEY GO (via output markers), not just one end-of-run summary.
    from muteki.solver import cli_solver as mod
    from muteki.solver.cli_driver import CliResult
    bus = _CaptureBus()
    ch = Challenge(id="t", name="login", category="web", flag_format=r"flag\{.*?\}",
                   target="http://x")
    drv = _StubDriver("")
    sg = _StubGraph("")
    transcript = (
        "probing the app\n"
        "VERIFIED_FACT=session cookie is a Flask zlib blob\n"
        "DEADEND=UNION on trackingID errors out\n"
        "still working...\n"
    )
    s = CliSolver(None, ch, bus=bus, driver=drv, engine="claude", kb=False,
                  shared_graph=sg)
    canned = lambda *a, **k: CliResult(text=transcript, session="sess-x")
    monkeypatch.setattr(mod, "run_cli_streaming", canned)
    monkeypatch.setattr(mod, "run_cli", canned)
    asyncio.run(s.run())
    # the verified fact landed on the shared graph + the deck
    assert any("Flask zlib blob" in f.get("fact", "") for f in sg.facts)
    assert any("UNION on trackingID" in d.get("reason", "") for d in sg.dead_ends)
    kinds = _bb_kinds(bus.events)
    assert "fact_added" in kinds and "dead_end" in kinds


def test_cli_solver_cancel_sets_event_and_kills_procs():
    # bug #2 fix: cancel() flips the cancel flag AND force-kills any live subproc.
    ch = Challenge(id="t", name="t", category="web", flag_format=r"flag\{.*?\}")
    s = _cli_solver(ch, kb=False)

    class _FakeProc:
        def __init__(self): self.killed = False; self.pid = 999999
        def kill(self): self.killed = True

    p = _FakeProc()
    s._on_proc(p)                      # register a live subprocess
    assert not s._cancel_event.is_set()
    s.cancel()
    assert s._cancel_event.is_set()    # the streaming watcher will see this
    assert p.killed is True            # and the subprocess is killed directly


def test_on_proc_kills_immediately_if_already_cancelled():
    # race: cancel fired before the subprocess registered → kill it on register.
    ch = Challenge(id="t", name="t", category="web", flag_format=r"flag\{.*?\}")
    s = _cli_solver(ch, kb=False)
    s.cancel()                          # cancel BEFORE any proc exists

    class _FakeProc:
        # pid 999999 doesn't exist, so _signal_proc's os.getpgid() raises and it
        # falls through to proc.kill() — matching the sibling test above. (pid=1 is
        # init: as root os.killpg(getpgid(1), SIGKILL) is permitted and returns early,
        # so proc.kill() is never reached and the test fails ONLY when run as root.)
        def __init__(self): self.killed = False; self.pid = 999999
        def kill(self): self.killed = True

    p = _FakeProc()
    s._on_proc(p)
    assert p.killed is True


def test_cancel_aggregates_runtime_signal_failures_without_short_circuiting():
    ch = Challenge(id="t", name="t", category="web", flag_format=r"flag\{.*?\}")
    s = _cli_solver(ch, kb=False)
    first, second = object(), object()
    s._live_procs.update((first, second))
    attempted = []
    s._signal_proc = lambda proc, _sig: (attempted.append(proc) or proc is second)

    assert s.cancel() is False
    assert set(attempted) == {first, second}
    assert s._cancel_event.is_set()


def test_secret_context_commits_only_after_stdin_handoff():
    ch = Challenge(id="t", name="t", category="web", flag_format=r"flag\{.*?\}")
    solver = _cli_solver(ch, kb=False)
    solver._pending_control_context_reservations = [("CTX-secret", "RSV-one")]
    commits = []
    deliveries = []
    solver._context_committer = lambda context_id, **kw: (
        commits.append((context_id, kw["reservation_id"])) or True)
    solver._context_delivery_callback = deliveries.append

    class _FakeProc:
        pid = 999999

        def poll(self):
            return None

        def kill(self):
            return None

    proc = _FakeProc()
    solver._on_proc(proc, context_via_stdin=True)
    assert commits == []
    assert deliveries == []
    assert solver._pending_control_context_reservations

    assert solver._commit_control_context_delivery(
        actor="test-stdin-delivered") is True
    assert commits == [("CTX-secret", "RSV-one")]
    assert deliveries == [True]
    assert solver._pending_control_context_reservations == []


def test_failed_secret_stdin_handoff_is_delivery_unknown_and_kills_worker():
    ch = Challenge(id="t", name="t", category="web", flag_format=r"flag\{.*?\}")
    solver = _cli_solver(ch, kb=False)
    solver._pending_control_context_reservations = [("CTX-secret", "RSV-two")]
    marked = []
    deliveries = []
    solver._context_delivery_unknown_marker = lambda context_id, **kw: (
        marked.append((context_id, kw["reservation_id"], kw["actor"])) or True)
    solver._context_delivery_callback = deliveries.append

    class _FakeProc:
        pid = 999999

        def __init__(self):
            self.killed = False

        def poll(self):
            return None

        def kill(self):
            self.killed = True

    proc = _FakeProc()
    solver._on_proc(proc, context_via_stdin=True)
    solver._mark_control_context_delivery_unknown(
        actor="test-stdin-failed", reason="broken pipe")

    assert marked == [("CTX-secret", "RSV-two", "test-stdin-failed")]
    assert deliveries == [False]
    assert solver._control_context_delivery_unknown is True
    assert solver._pending_control_context_reservations == []
    assert proc.killed is True


def test_late_secret_stdin_callback_uses_immutable_reservation_batch():
    """Clearing mutable pending state cannot launder a late handoff as success."""
    ch = Challenge(id="t", name="t", category="web", flag_format=r"flag\{.*?\}")
    solver = _cli_solver(ch, kb=False)
    batch = (("CTX-secret", "RSV-late"),)
    solver._pending_control_context_reservations = list(batch)
    commits = []
    marked = []
    deliveries = []
    # Model an early owner release: the journal no longer accepts the reservation,
    # but the writer thread later reports that plaintext crossed stdin.
    solver._context_committer = lambda context_id, **kw: (
        commits.append((context_id, kw["reservation_id"])) or False)
    solver._context_delivery_unknown_marker = lambda context_id, **kw: (
        marked.append((context_id, kw["reservation_id"])) or True)
    solver._context_delivery_callback = deliveries.append
    solver._pending_control_context_reservations = []

    assert solver._commit_control_context_delivery(
        actor="late-stdin", reservations=batch) is False
    assert commits == [("CTX-secret", "RSV-late")]
    assert marked == [("CTX-secret", "RSV-late")]
    assert deliveries == [False]
    assert solver._control_context_delivery_unknown is True
    assert solver._control_context_delivery_committed is False


def test_to_thread_cancellation_kills_fake_proc_and_awaits_delayed_cleanup(
        monkeypatch):
    """Cancelling the asyncio wrapper must kill the real child and give the
    underlying runner thread time to reap it before the wrapper disappears."""
    ch = Challenge(id="t", name="t", category="web", flag_format=r"flag\{.*?\}")
    s = _cli_solver(ch, kb=False)
    monkeypatch.setenv("MUTEKI_CLI_CANCEL_CLEANUP_TIMEOUT", "0.5")

    started = threading.Event()
    runner_finished = threading.Event()

    class _FakeProc:
        def __init__(self):
            self.pid = 999999
            self.killed = False
            self.alive = True

        def kill(self):
            self.killed = True

        def poll(self):
            return None if self.alive else 0

    proc = _FakeProc()

    def blocking_runner():
        s._on_proc(proc)
        started.set()
        while not proc.killed:
            time.sleep(0.001)
        # Adversarial delayed cleanup: asyncio cancellation has happened, but the
        # runner still owns its thread/process teardown for a short interval.
        time.sleep(0.04)
        proc.alive = False
        runner_finished.set()
        return "done"

    async def drive():
        task = asyncio.create_task(
            s._to_thread_with_cancel_cleanup(blocking_runner))
        while not started.is_set():
            await asyncio.sleep(0.001)
        t0 = time.monotonic()
        task.cancel()
        with pytest.raises(asyncio.CancelledError):
            await task
        return time.monotonic() - t0

    elapsed = asyncio.run(drive())
    assert proc.killed is True
    assert runner_finished.is_set()
    assert elapsed >= 0.03, "outer task must await bounded runner cleanup"
    assert proc not in s._live_procs
    assert s.runtime_exit_confirmed() is True


def test_to_thread_cleanup_timeout_retains_unproven_live_proc(monkeypatch):
    """A bounded cleanup timeout must not erase the final handle to a child whose
    runner thread is still alive; the late done callback prunes it after poll()."""
    ch = Challenge(id="t", name="t", category="web", flag_format=r"flag\{.*?\}")
    s = _cli_solver(ch, kb=False)
    monkeypatch.setenv("MUTEKI_CLI_CANCEL_CLEANUP_TIMEOUT", "0.01")

    started = threading.Event()
    release = threading.Event()

    class _FakeProc:
        def __init__(self):
            self.pid = 999999
            self.killed = False
            self.alive = True

        def kill(self):
            self.killed = True

        def poll(self):
            return None if self.alive else 0

    proc = _FakeProc()

    def stuck_runner():
        s._on_proc(proc)
        started.set()
        release.wait(timeout=2)
        proc.alive = False
        return "done"

    async def drive():
        task = asyncio.create_task(s._to_thread_with_cancel_cleanup(stuck_runner))
        while not started.is_set():
            await asyncio.sleep(0.001)
        task.cancel()
        with pytest.raises(asyncio.CancelledError):
            await task
        assert proc.killed is True
        assert proc in s._live_procs, "live handle must survive cleanup timeout"
        assert s.runtime_exit_confirmed() is False
        assert await s.wait_runtime_exit(0.005) is False
        release.set()
        assert await s.wait_runtime_exit(0.5) is True

    asyncio.run(drive())
    assert proc not in s._live_procs
    assert s.runtime_exit_confirmed() is True


def test_rcp_transport_return_without_exit_frame_never_launders_runtime_exit():
    from muteki.solver.control_client import (
        ControlError, _RcpProc, confirm_run_absent,
    )

    ch = Challenge(id="t", name="t", category="web", flag_format=r"flag\{.*?\}")
    s = _cli_solver(ch, kb=False)

    class _Link:
        def signal(self, worker_id, name, **kwargs): return False

    proc = _RcpProc(_Link(), "w-unknown-exit", run_id="run-unknown-exit")

    def failed_transport():
        s._on_proc(proc)
        raise ControlError("exit frame never arrived")

    async def drive():
        with pytest.raises(ControlError, match="never arrived"):
            await s._to_thread_with_cancel_cleanup(failed_transport)

    asyncio.run(drive())
    assert proc._exit_confirmed is False
    assert proc in s._live_procs
    assert s.runtime_exit_confirmed() is False
    confirm_run_absent("run-unknown-exit")
    assert s.runtime_exit_confirmed() is True


def test_run_cli_streaming_cancel_event_kills_process(tmp_path):
    # bug #2 fix at the runner level: a long-running subprocess is killed promptly
    # when the cancel_event fires mid-run (not left running until timeout).
    import threading
    import time
    from muteki.solver.cli_driver import run_cli_streaming

    d = ClaudeCodeDriver()
    # a script that would run for 30s, emitting a line then sleeping
    script = tmp_path / "slow.sh"
    script.write_text('#!/bin/sh\necho \'{"type":"assistant","message":{"content":[{"type":"text","text":"working"}]}}\'\nsleep 30\n')
    script.chmod(0o755)

    cancel = threading.Event()
    # fire cancel shortly after start
    threading.Timer(0.5, cancel.set).start()
    t0 = time.time()
    res = run_cli_streaming(d, ["/bin/sh", str(script)], cwd=str(tmp_path),
                            timeout=30, on_step=lambda s: None, cancel_event=cancel)
    elapsed = time.time() - t0
    assert res.cancelled is True
    assert elapsed < 5  # killed promptly, NOT after the 30s sleep/timeout


def test_run_cli_streaming_bare_timeout_kills_silent_process(tmp_path):
    """Finding #4 regression: a BARE call (no cancel/steer events) with a SILENT,
    long-running process must still hit the timeout and be killed. The watcher used to
    only start when cancel_event/steer_event was present, so this call had NO timeout
    enforcement at all — and a zero-stdout process blocked `for line in proc.stdout`
    forever. Uses a real subprocess sleep (NOT mocked) and a tiny timeout."""
    import time
    from muteki.solver.cli_driver import run_cli_streaming

    d = ClaudeCodeDriver()
    t0 = time.time()
    # `sleep 30` emits nothing on stdout — the only way out is the watcher's timeout.
    res = run_cli_streaming(d, ["sleep", "30"], cwd=str(tmp_path),
                            timeout=1, on_step=lambda s: None)
    elapsed = time.time() - t0
    assert res.timed_out is True, "a silent over-budget bare call must report timed_out"
    assert elapsed < 8, f"timeout must fire promptly, took {elapsed:.1f}s"


def test_run_cli_streaming_does_not_hang_on_orphaned_stderr(tmp_path):
    """A CLI may spawn a background sidecar that inherits stderr after the parent
    exits. The streaming runner must not block forever in proc.stderr.read(), or the
    worker stays online and keeps its engine/profile lock."""
    import time
    from muteki.solver.cli_driver import run_cli_streaming

    d = ClaudeCodeDriver()
    line = '{"type":"result","result":"FOUND_FLAG=flag{ok}","session_id":"z"}'
    script = tmp_path / "orphan_stderr.sh"
    script.write_text(
        "#!/bin/sh\n"
        "python3 - <<'PY' >/dev/null &\n"
        "import sys, time\n"
        "sys.stderr.write('sidecar still owns stderr\\n')\n"
        "sys.stderr.flush()\n"
        "time.sleep(6)\n"
        "PY\n"
        f"echo '{line}'\n"
    )
    script.chmod(0o755)

    t0 = time.time()
    res = run_cli_streaming(d, ["/bin/sh", str(script)], cwd=str(tmp_path),
                            timeout=30, on_step=lambda s: None)
    elapsed = time.time() - t0

    assert "FOUND_FLAG=flag{ok}" in res.text
    assert elapsed < 3, f"stderr sidecar must not hold the worker lock ({elapsed:.1f}s)"


def test_run_cli_streaming_paused_time_excluded_from_timeout(tmp_path):
    """M7: a worker SIGSTOP-frozen by the operator must NOT be killed as timed_out
    just for being paused. With paused_event set for longer than the timeout, the
    active (unpaused) elapsed stays under budget, so the process is not timed out;
    once we clear the event, the now-running clock trips the timeout normally."""
    import threading
    import time
    from muteki.solver.cli_driver import run_cli_streaming

    d = ClaudeCodeDriver()
    paused = threading.Event()
    paused.set()                       # frozen from the start
    # Hold the freeze for ~2.5s (> the 1s timeout), then release. While the event is
    # set, active_elapsed() must stay ~0 so the watcher does NOT fire.
    threading.Timer(2.5, paused.clear).start()
    t0 = time.time()
    res = run_cli_streaming(d, ["sleep", "30"], cwd=str(tmp_path),
                            timeout=1, on_step=lambda s: None, paused_event=paused)
    elapsed = time.time() - t0
    # It DID eventually time out (after the freeze lifted), but only AFTER the paused
    # window — proving the paused interval was excluded from the 1s budget.
    assert res.timed_out is True
    assert elapsed >= 2.5, (
        f"timeout fired during the freeze (elapsed {elapsed:.1f}s) — paused time was "
        "NOT excluded from the budget")
    assert elapsed < 8, f"timeout must still fire promptly after resume, took {elapsed:.1f}s"


def test_m9_owned_scratch_cleaned_on_cancel(tmp_path):
    """M9: a mkdtemp scratch dir the worker owns is removed even when the run is
    CANCELLED mid-body (the in-method rmtree only ran on the no-flag fall-through;
    cancel/exception used to skip it and leak the dir)."""
    import asyncio
    from muteki.models.solve_graph import Challenge

    ch = Challenge(id="t", name="t", category="web", flag_format=r"flag\{.*?\}")
    s = _cli_solver(ch, kb=False)
    scratch = tmp_path / "muteki-cli-scratch"
    scratch.mkdir()
    (scratch / "junk.txt").write_text("x")

    async def _fake_bootstrap():
        s._owned_scratch = scratch          # as the real mkdtemp branch would
        raise asyncio.CancelledError()

    s._run_bootstrap = _fake_bootstrap

    async def _go():
        try:
            await s.run()
        except asyncio.CancelledError:
            pass

    asyncio.run(_go())
    assert not scratch.exists(), "an owned scratch dir must be cleaned on a cancelled run"


def test_m9_solved_winner_scratch_is_kept(tmp_path):
    """M9 guard: a SOLVED worker's scratch is its winner artifact (the swarm resumes
    the session from it) — it must NOT be deleted when it's the returned workdir."""
    import asyncio
    from muteki.models.solve_graph import Challenge
    from muteki.solver.types import SolveOutcome

    ch = Challenge(id="t", name="t", category="web", flag_format=r"flag\{.*?\}")
    s = _cli_solver(ch, kb=False)
    scratch = tmp_path / "winner-scratch"
    scratch.mkdir()

    async def _fake_bootstrap():
        s._owned_scratch = scratch
        return SolveOutcome(True, "flag{win}", 1, s.graph, "solved",
                            workdir=str(scratch))

    s._run_bootstrap = _fake_bootstrap
    asyncio.run(s.run())
    assert scratch.exists(), "a solved worker's winner scratch must be preserved"


def test_on_proc_freezes_subprocess_registered_while_paused():
    """M8: if the operator paused the worker before a subprocess registered (e.g. the
    pause landed in the gap before the conclude-fallback subprocess started), _on_proc
    must SIGSTOP the new process so the pause doesn't silently leak across the turn
    boundary and let a paused worker keep running."""
    import signal as _signal
    ch = Challenge(id="t", name="t", category="web", flag_format=r"flag\{.*?\}")
    s = _cli_solver(ch, kb=False)
    s._paused = True                    # operator paused before this proc exists

    sent = []

    class _FakeProc:
        def __init__(self): self.pid = 4242
        def kill(self): sent.append(_signal.SIGKILL)

    # capture whatever signal _on_proc routes through _signal_proc
    s._signal_proc = lambda p, sig: sent.append(sig)
    s._on_proc(_FakeProc())
    assert _signal.SIGSTOP in sent, "a proc registered while paused must be SIGSTOP'd"
    assert _signal.SIGKILL not in sent, "must freeze, not kill, a paused worker's proc"


def test_set_paused_requires_real_signal_confirmation():
    """Logical freeze state cannot change when the OS/container signal boundary
    cannot prove delivery."""
    ch = Challenge(id="t", name="t", category="web", flag_format=r"flag\{.*?\}")
    s = _cli_solver(ch, kb=False)

    class _FakeProc:
        pid = 4242

    proc = _FakeProc()
    s._live_procs.add(proc)
    s._signal_proc = lambda _proc, _sig: False

    assert s._set_paused(True) is False
    assert s._paused is False
    assert not s._paused_event.is_set()


def test_pause_resume_via_insight_bus_signals_process(monkeypatch):
    # bug #3 fix: a HITL pause GUIDANCE on the InsightBus reaches the live worker
    # and SIGSTOPs its subprocess; resume SIGCONTs it. We capture the signals.
    from muteki.swarm.insight_bus import InsightBus
    ch = Challenge(id="t", name="t", category="web", flag_format=r"flag\{.*?\}")
    bus_insight = InsightBus(challenge_id="t")
    s = _cli_solver(ch, kb=False, insight=bus_insight)
    s._insight_inbox = bus_insight.subscribe(s.solver_id)

    class _FakeProc:
        def __init__(self): self.pid = 4242
    s._live_procs.add(_FakeProc())

    signals = []
    monkeypatch.setattr("muteki.solver.cli_solver.os.kill",
                        lambda pid, sig: signals.append((pid, sig)))

    async def drive():
        await bus_insight.guidance("", action="pause", target="global")
        s._drain_control()
        await bus_insight.guidance("", action="resume", target="global")
        s._drain_control()
    asyncio.run(drive())
    import signal as _sig
    assert (4242, _sig.SIGSTOP) in signals
    assert (4242, _sig.SIGCONT) in signals
    assert s._paused is False  # ended resumed


def test_live_guidance_accepts_engine_intent_and_lane_selectors():
    from muteki.swarm.insight_bus import InsightBus

    ch = Challenge(id="t", name="t", category="web", flag_format=r"flag\{.*?\}")
    for target, attr in (
        ("engine:claude", ("engine", "claude")),
        ("intent:I-live", ("intent_id_assigned", "I-live")),
        ("lane:exclusive:web", ("lane", "exclusive:web")),
    ):
        insight = InsightBus(challenge_id="t")
        solver = _cli_solver(ch, kb=False, insight=insight)
        solver.engine = "claude"
        setattr(solver, *attr)
        solver._insight_inbox = insight.subscribe(solver.solver_id)
        solver._turn_active = True

        async def _send():
            await insight.guidance(
                "switch route", action="redirect", target=target,
                url="http://selector.invalid")

        asyncio.run(_send())
        solver._drain_control()
        assert solver._target_override == "http://selector.invalid"
        assert solver._steer_event.is_set()

def test_live_markers_stream_to_board_and_insight_bus():
    # bug #1 (full fix): a VERIFIED_FACT= seen MID-RUN is pushed to the shared graph
    # AND broadcast on the InsightBus immediately, so a racing teammate sees it now.
    from muteki.solver.cli_driver import StreamStep
    from muteki.swarm.insight_bus import InsightBus, InsightKind
    ch = Challenge(id="t", name="t", category="web", flag_format=r"flag\{.*?\}")
    sg = _StubGraph("")
    insight = InsightBus(challenge_id="t")
    teammate = insight.subscribe("cli-other")     # a racing sibling
    s = _cli_solver(ch, kb=False, shared_graph=sg, insight=insight)

    async def drive():
        # the marker arrives as a live tool-result step
        await s._emit_step(StreamStep("tool_result",
                                      text=("curl showed: admin cookie is base64 JSON\n"
                                            "VERIFIED_FACT=admin cookie is base64 JSON\n")))
        # a second identical marker (echo) must NOT double-write
        await s._emit_step(StreamStep("tool_result",
                                      text=("curl showed: admin cookie is base64 JSON\n"
                                            "VERIFIED_FACT=admin cookie is base64 JSON\n")))
    asyncio.run(drive())

    # written to the shared graph exactly once (deduped)
    assert len(sg.facts) == 1
    assert "admin cookie is base64 JSON" in sg.facts[0]["fact"]
    # and the racing teammate received it on the InsightBus as a FACT
    got = teammate.get_nowait()
    assert got.kind is InsightKind.FACT
    assert "admin cookie is base64 JSON" in got.text


def test_live_verified_fact_without_witness_is_candidate_only():
    from muteki.solver.cli_driver import StreamStep
    from muteki.swarm.insight_bus import InsightBus
    ch = Challenge(id="t", name="t", category="web", flag_format=r"flag\{.*?\}")
    sg = _StubGraph("")
    insight = InsightBus(challenge_id="t")
    teammate = insight.subscribe("cli-other")
    s = _cli_solver(ch, kb=False, shared_graph=sg, insight=insight)

    asyncio.run(s._emit_step(StreamStep(
        "tool_result", text="VERIFIED_FACT=admin cookie is base64 JSON\n")))

    assert len(sg.facts) == 1
    assert sg.facts[0]["verified"] is False
    assert teammate.empty()


def test_worker_fact_witness_metadata_does_not_replace_witness_check():
    from muteki.solver.cli_driver import StreamStep
    from muteki.swarm.insight_bus import InsightBus
    ch = Challenge(id="t", name="t", category="web", flag_format=r"flag\{.*?\}")
    sg = _StubGraph("")
    insight = InsightBus(challenge_id="t")
    teammate = insight.subscribe("cli-other")
    s = _cli_solver(ch, kb=False, shared_graph=sg, insight=insight)

    asyncio.run(s._emit_step(StreamStep(
        "tool_result",
        text=("VERIFIED_FACT=admin cookie is base64 JSON\n"
              "FACT_WITNESS=curl -i /login showed the cookie\n"))))

    assert len(sg.facts) == 1
    assert sg.facts[0]["witness"] == "curl -i /login showed the cookie"
    assert sg.facts[0]["verified"] is False
    assert teammate.empty()


def test_live_dead_end_marker_broadcasts_to_insight_bus():
    from muteki.solver.cli_driver import StreamStep
    from muteki.swarm.insight_bus import InsightBus, InsightKind
    ch = Challenge(id="t", name="t", category="web", flag_format=r"flag\{.*?\}")
    sg = _StubGraph("")
    insight = InsightBus(challenge_id="t")
    teammate = insight.subscribe("cli-other")
    s = _cli_solver(ch, kb=False, shared_graph=sg, insight=insight)

    asyncio.run(s._emit_step(StreamStep(
        "tool_result", text="DEADEND=/login is rate-limited, brute force won't work\n")))

    assert len(sg.dead_ends) == 1
    got = teammate.get_nowait()
    assert got.kind is InsightKind.DEAD_END
    assert "rate-limited" in got.text


# ── Operator steering: multi-turn loop + guidance capture + target/standing ──
# (HITL fusion: hint/redirect/focus reach a live worker via the
#  next resume turn — headless CLIs can't take input mid-turn.)

def _steer_solver(**kw):
    ch = Challenge(id="t", name="login", category="web", flag_format=r"flag\{.*?\}",
                   target="http://old")
    return _cli_solver(ch, kb=False, **kw)


def test_drain_control_nonstanding_hint_does_not_steer_live_worker():
    """A normal operator hint is additive guidance, not an interrupt. It must be
    recorded for future prompts without killing the currently running worker."""
    from muteki.swarm.insight_bus import InsightBus
    insight = InsightBus(challenge_id="t")
    s = _steer_solver(insight=insight)
    s._insight_inbox = insight.subscribe(s.solver_id)
    s._turn_active = True   # a subprocess turn is running
    asyncio.run(insight.guidance("try /admin", action="hint", target="global"))
    s._drain_control()
    assert not s._steer_event.is_set()     # hint must not kill the current pass
    assert "try /admin" in s._standing_guidance  # recorded for the remainder


def test_drain_control_nonstanding_hint_no_steer_without_active_turn():
    """The steer-kill is gated: a hint replayed from history (no turn running yet)
    must NOT steer-kill a not-yet-started subprocess (the run-40726 regression)."""
    from muteki.swarm.insight_bus import InsightBus
    insight = InsightBus(challenge_id="t")
    s = _steer_solver(insight=insight)
    s._insight_inbox = insight.subscribe(s.solver_id)
    s._turn_active = False  # no subprocess turn → steer must be suppressed
    asyncio.run(insight.guidance("try /admin", action="hint", target="global"))
    s._drain_control()
    assert not s._steer_event.is_set()     # gated off — no premature kill


def test_drain_control_redirect_sets_steer_event_no_buffer():
    """A non-standing redirect ENDS the current pass (intent-level kill) — gated on
    an active turn — and no longer buffers guidance (the swarm hands the new target
    to the next spawned worker)."""
    from muteki.swarm.insight_bus import InsightBus
    insight = InsightBus(challenge_id="t")
    s = _steer_solver(insight=insight)
    s._insight_inbox = insight.subscribe(s.solver_id)
    s._turn_active = True
    asyncio.run(insight.guidance("challenge moved", action="redirect",
                                 url="http://new", target="global"))
    s._drain_control()
    assert s._steer_event.is_set()       # redirect still ends the current pass


def test_drain_control_standing_guidance_does_not_steer_live_worker():
    from muteki.swarm.insight_bus import InsightBus
    insight = InsightBus(challenge_id="t")
    s = _steer_solver(insight=insight)
    s._insight_inbox = insight.subscribe(s.solver_id)
    s._turn_active = True   # live subprocess is running
    asyncio.run(insight.guidance("ssh root@1.2.3.4", action="hint",
                                 target="global", standing=True))
    s._drain_control()
    assert s._standing_guidance == ["ssh root@1.2.3.4"]
    # Standing guidance is background context (VPS/SSH creds, global constraints):
    # keep it for this/future prompts, but do not kill an otherwise healthy live turn.
    assert not s._steer_event.is_set()


def test_steer_event_independent_of_cancel():
    # cancel() and the steer path are distinct signals: cancel means die, steer
    # (the _steer_event the _drain_control hint/redirect branch sets) means end the
    # current pass without marking the worker dead. Neither implies the other.
    s = _steer_solver()
    s.cancel()
    assert s._cancel_event.is_set()
    assert not s._steer_event.is_set()           # cancel must not steer
    s2 = _steer_solver()
    s2._steer_event.set()
    assert s2._steer_event.is_set()
    assert not s2._cancel_event.is_set()         # steer must not cancel


def test_target_override_used_in_prompt():
    s = _steer_solver()
    assert "Target: http://old" in s._build_prompt()
    s._target_override = "http://new"
    p = s._build_prompt()
    assert "Target: http://new" in p and "http://old" not in p


def test_standing_guidance_injected_into_prompt():
    s = _steer_solver()
    s._standing_guidance = ["use VPS ssh root@1.2.3.4"]
    p = s._build_prompt()
    assert "use VPS ssh root@1.2.3.4" in p
    assert "Operator standing guidance" in p
    # explore prompt too
    s2 = _steer_solver(mode="explore", intent_goal="x")
    s2._standing_guidance = ["use VPS ssh root@1.2.3.4"]
    assert "use VPS ssh root@1.2.3.4" in s2._build_explore_prompt()


# ── SINGLE-SHOT migration (DESIGN_single_shot_migration.md, M-1) ──────────────
# The worker no longer lives across turns accumulating context. The three tests
# below replace the retired multi-turn-loop tests (loops-on-guidance / respects-
# max-turns / steered-continues): a worker now runs ONE execute pass; mid-run
# operator guidance does NOT resume it (intent-level HITL → the NEXT spawned
# worker absorbs it); the ONLY second subprocess call is the conclude fallback.
def test_single_shot_buffered_guidance_does_not_resume(monkeypatch):
    """Migration: operator guidance dropped mid-run no longer resumes this live
    worker. The worker finishes its one execute pass; guidance reaches the next
    spawned worker, not a resume turn."""
    from muteki.solver import cli_solver as mod
    from muteki.solver.cli_driver import CliResult
    bus = _CaptureBus()
    ch = Challenge(id="t", name="login", category="web", flag_format=r"flag\{.*?\}",
                   target="http://x")
    s = CliSolver(None, ch, bus=bus, driver=_StubDriver(""), engine="claude", kb=False)
    calls = {"n": 0}

    def fake_stream(driver, argv, **k):
        calls["n"] += 1
        # operator drops guidance during the execute pass — recorded for the next
        # spawned worker (single-shot), must NOT trigger a resume of THIS worker.
        s._standing_guidance.append("try /admin")
        s._persist_raw_tool_output(
            "target verifier printed FOUND_FLAG=flag{got_it_first_pass}\n")
        s._validated_flag_submissions.add("flag{got_it_first_pass}")
        return CliResult(text="FOUND_FLAG=flag{got_it_first_pass}\n", session="sess-x")

    monkeypatch.setattr(mod, "run_cli_streaming", fake_stream)
    monkeypatch.setattr(mod, "run_cli", fake_stream)
    asyncio.run(s.run())
    assert calls["n"] == 1                        # one execute pass, NO resume
    assert "flag_found" in _bb_kinds(bus.events)


def test_single_shot_one_conclude_fallback_on_timeout(monkeypatch):
    """Migration: a timeout without enough flags triggers AT MOST one conclude
    fallback (the single-shot model) — exactly two subprocess calls, never a loop."""
    from muteki.solver import cli_solver as mod
    from muteki.solver.cli_driver import CliResult
    bus = _CaptureBus()
    ch = Challenge(id="t", name="login", category="web", flag_format=r"flag\{.*?\}",
                   target="http://x")
    s = CliSolver(None, ch, bus=bus, driver=_StubDriver(""), engine="claude", kb=False)
    s._session_established = True                  # allow the conclude resume
    calls = {"n": 0}

    def fake_stream(*a, **k):
        calls["n"] += 1
        if calls["n"] == 1:
            return CliResult(text="nothing yet\n", session="sess-x", timed_out=True)
        return CliResult(text="VERIFIED_FACT=admin panel at /admin\n", session="sess-x")

    monkeypatch.setattr(mod, "run_cli_streaming", fake_stream)
    monkeypatch.setattr(mod, "run_cli", fake_stream)
    asyncio.run(s.run())
    assert calls["n"] == 2                         # execute + ONE conclude, no loop


def test_single_shot_cancel_skips_conclude(monkeypatch):
    """Migration: a cancel (sibling won / stop) ends the worker immediately — no
    conclude fallback, no resume."""
    from muteki.solver import cli_solver as mod
    from muteki.solver.cli_driver import CliResult
    bus = _CaptureBus()
    ch = Challenge(id="t", name="login", category="web", flag_format=r"flag\{.*?\}",
                   target="http://x")
    s = CliSolver(None, ch, bus=bus, driver=_StubDriver(""), engine="claude", kb=False)
    calls = {"n": 0}

    def fake_stream(*a, **k):
        calls["n"] += 1
        # cancelled AND timed_out: cancel must win → no conclude fallback.
        return CliResult(text="killed\n", session="sess-x", cancelled=True, timed_out=True)

    monkeypatch.setattr(mod, "run_cli_streaming", fake_stream)
    monkeypatch.setattr(mod, "run_cli", fake_stream)
    asyncio.run(s.run())
    assert calls["n"] == 1                         # cancelled → die now, no conclude


def test_single_shot_steer_skips_conclude_and_deadend(monkeypatch):
    """A steer ends this pass so the coordinator can spawn a guided worker. It must
    not resume into CONCLUDE or record a misleading no-output dead-end."""
    from muteki.solver import cli_solver as mod
    from muteki.solver.cli_driver import CliResult
    bus = _CaptureBus()
    ch = Challenge(id="t", name="login", category="web", flag_format=r"flag\{.*?\}",
                   target="http://x")
    s = CliSolver(None, ch, bus=bus, driver=_StubDriver(""), engine="claude", kb=False)
    calls = {"n": 0}

    def fake_stream(*a, **k):
        calls["n"] += 1
        return CliResult(text="", session="sess-x", steered=True)

    monkeypatch.setattr(mod, "run_cli_streaming", fake_stream)
    monkeypatch.setattr(mod, "run_cli", fake_stream)
    asyncio.run(s.run())
    assert calls["n"] == 1
    assert "dead_end" not in _bb_kinds(bus.events)
    assert _worker_statuses(bus.events)[-1].payload["reason"] == "steered"


def test_explore_steer_skips_conclude_and_deadend(monkeypatch):
    from muteki.solver import cli_solver as mod
    from muteki.solver.cli_driver import CliResult
    bus = _CaptureBus()
    ch = Challenge(id="t", name="login", category="web", flag_format=r"flag\{.*?\}",
                   target="http://x")
    s = CliSolver(
        None, ch, bus=bus, driver=_StubDriver(""), engine="claude", kb=False,
        mode="explore", intent_goal="try redirected target")
    calls = {"n": 0}

    def fake_stream(*a, **k):
        calls["n"] += 1
        return CliResult(text="", session="sess-x", steered=True)

    monkeypatch.setattr(mod, "run_cli_streaming", fake_stream)
    monkeypatch.setattr(mod, "run_cli", fake_stream)
    asyncio.run(s.run())
    assert calls["n"] == 1
    assert "dead_end" not in _bb_kinds(bus.events)
    assert _worker_statuses(bus.events)[-1].payload["reason"] == "steered"


def test_m4_no_flag_worker_exits_clean_without_deadend(monkeypatch):
    """M-4 (never-give-up at swarm layer): a single-shot worker that finds no flag
    does NOT keep retrying or rationalize a false 'solved' — it exits cleanly after
    its one pass, returns unsolved, and concludes a DEAD-END for the board. 'Give
    up' is now a clean swarm-level decision (re-bootstrap), not worker self-hypnosis."""
    from muteki.solver import cli_solver as mod
    from muteki.solver.cli_driver import CliResult
    bus = _CaptureBus()
    ch = Challenge(id="t", name="login", category="web", flag_format=r"flag\{.*?\}",
                   target="http://x")
    s = CliSolver(None, ch, bus=bus, driver=_StubDriver(""), engine="claude", kb=False)
    calls = {"n": 0}

    def fake_stream(*a, **k):
        calls["n"] += 1
        # honest "couldn't crack it" — NO flag, NO false-solved claim.
        return CliResult(text="probed /admin, no auth bypass found\n", session="sess-x")

    monkeypatch.setattr(mod, "run_cli_streaming", fake_stream)
    monkeypatch.setattr(mod, "run_cli", fake_stream)
    outcome = asyncio.run(s.run())
    assert calls["n"] == 1                         # one pass, no retry loop
    assert outcome.solved is False                 # honest: not solved
    kinds = _bb_kinds(bus.events)
    assert "dead_end" not in kinds                 # no explicit DEADEND= marker
    concl = [e for e in bus.events
             if e.event_type is EventType.BLACKBOARD_DELTA
             and e.payload.get("kind") == "intent_concluded"][0]
    assert concl.payload.get("result") == "explored"
    assert "flag_found" not in kinds               # never a false-solved claim


def test_cancelled_turn_does_not_resume(monkeypatch):
    from muteki.solver import cli_solver as mod
    from muteki.solver.cli_driver import CliResult
    bus = _CaptureBus()
    ch = Challenge(id="t", name="login", category="web", flag_format=r"flag\{.*?\}",
                   target="http://x")
    s = CliSolver(None, ch, bus=bus, driver=_StubDriver(""), engine="claude", kb=False)
    calls = {"n": 0}

    def fake_stream(*a, **k):
        calls["n"] += 1
        return CliResult(text="killed\n", session="sess-x", cancelled=True)

    monkeypatch.setattr(mod, "run_cli_streaming", fake_stream)
    monkeypatch.setattr(mod, "run_cli", fake_stream)
    asyncio.run(s.run())
    assert calls["n"] == 1                        # cancelled → no resume


def test_hitl_cmd_url_sets_target_override():
    ch = Challenge(id="t", name="login", category="web", flag_format=r"flag\{.*?\}",
                   target="http://old")
    s = _cli_solver(ch, kb=False, mode="respond", resume_session="sess",
                    hitl_cmd={"action": "redirect", "url": "http://new"})
    assert s._target_override == "http://new"
    assert "Target: http://new" in s._build_prompt()


def test_standby_redirect_endpoint_crosses_the_actual_prompt_boundary(tmp_path):
    from muteki.solver.cli_driver import CliResult

    captured = {}

    class _CaptureResumeDriver(_StubDriver):
        def build_resume(self, prompt, *args, **kwargs):
            captured["prompt"] = prompt
            return ["true"]

    endpoint = "https://private-target.invalid/next"
    ch = Challenge(
        id="redirect-standby", name="redirect", category="web",
        flag_format=r"flag\{.*?\}", target="https://old.invalid",
    )
    solver = CliSolver(
        None, ch, bus=_CaptureBus(), driver=_CaptureResumeDriver(""),
        engine="claude", kb=False, mode="respond", resume_session="sess",
        workdir=str(tmp_path),
        hitl_cmd={"action": "redirect", "url": endpoint},
    )

    async def _finished(_argv, *, cwd, timeout, stdin_text=None):
        return CliResult(text="redirect received", session="sess")

    solver._run_streaming = _finished
    asyncio.run(solver._run_respond())

    assert endpoint in captured["prompt"]
    assert "new target endpoint" in captured["prompt"]


def test_standby_writeup_forbids_new_investigation_and_uses_short_timeout(tmp_path):
    from muteki.solver.cli_driver import CliResult

    captured = {}

    class _CaptureResumeDriver(_StubDriver):
        def build_resume(self, prompt, *args, **kwargs):
            captured["prompt"] = prompt
            return ["true"]

    ch = Challenge(
        id="writeup-standby", name="writeup", category="web",
        flag_format=r"flag\{.*?\}",
    )
    solver = CliSolver(
        None, ch, bus=_CaptureBus(), driver=_CaptureResumeDriver(""),
        engine="claude", kb=False, mode="respond", resume_session="sess",
        workdir=str(tmp_path), timeout=2400,
        hitl_cmd={"action": "writeup", "followup_id": "F-writeup"},
    )

    async def _finished(_argv, *, cwd, timeout, stdin_text=None):
        captured["timeout"] = timeout
        return CliResult(text="## 漏洞点\n已确认。", session="sess")

    solver._run_streaming = _finished
    outcome = asyncio.run(solver._run_respond())

    assert "Do not run commands" in captured["prompt"]
    assert "search the filesystem" in captured["prompt"]
    assert captured["timeout"] == 300
    assert outcome.reply.startswith("## 漏洞点")


def test_standby_resume_releases_optional_context_absent_from_prompt(tmp_path):
    """A resume Popen cannot commit a context omitted from that exact turn."""
    from muteki.solver.cli_driver import CliResult

    built_prompts = []
    released = []
    committed = []

    class _CaptureResumeDriver(_StubDriver):
        def build_resume(self, prompt, *args, **kwargs):
            built_prompts.append(prompt)
            return ["true"]

    ch = Challenge(
        id="resume-manifest-optional", name="resume-manifest", category="misc",
        flag_format=r"flag\{.*?\}",
    )
    solver = CliSolver(
        None, ch, bus=_CaptureBus(), driver=_CaptureResumeDriver(""),
        engine="claude", kb=False, mode="respond", resume_session="sess",
        workdir=str(tmp_path), hitl_cmd={"action": "ask", "text": "status?"},
    )
    reservation = ("CTX-resume-optional", "RSV-resume-optional")
    omitted = "OPTIONAL CONTEXT THAT IS NOT PART OF THIS RESUME TURN"
    solver._pending_control_context_reservations = [reservation]
    solver._control_context_prompt_manifest = [{
        "context_id": reservation[0],
        "reservation_id": reservation[1],
        "text": omitted,
        "required": False,
        "kind": "clue",
        "secret": False,
    }]
    solver._context_releaser = lambda context_id, **kw: (
        released.append((context_id, kw["reservation_id"])) or True)
    solver._context_committer = lambda context_id, **kw: (
        committed.append((context_id, kw["reservation_id"])) or True)

    class _ExitedProc:
        pid = None

        def poll(self):
            return 0

        def kill(self):
            return None

    async def _finished(_argv, *, cwd, timeout, stdin_text=None):
        # Exercise the real Popen delivery callback after argv construction.  The
        # omitted reservation must already have been removed from its snapshot.
        solver._on_proc(_ExitedProc())
        return CliResult(text="answered", session="sess")

    solver._run_streaming = _finished
    asyncio.run(solver._run_respond())

    assert len(built_prompts) == 1
    assert omitted not in built_prompts[0]
    assert released == [reservation]
    assert committed == []
    assert solver._pending_control_context_reservations == []
    assert solver._control_context_delivery_committed is False
    solver._prune_finished_procs()


def test_standby_resume_missing_required_context_aborts_before_argv_or_popen(
        tmp_path):
    """An exact continuation absent from a resume prompt fails at zero Popen."""
    built = []
    started = []
    released = []
    committed = []

    class _CaptureResumeDriver(_StubDriver):
        def build_resume(self, prompt, *args, **kwargs):
            built.append(prompt)
            return ["true"]

    ch = Challenge(
        id="resume-manifest-required", name="resume-manifest", category="misc",
        flag_format=r"flag\{.*?\}",
    )
    solver = CliSolver(
        None, ch, bus=_CaptureBus(), driver=_CaptureResumeDriver(""),
        engine="claude", kb=False, mode="respond", resume_session="sess",
        workdir=str(tmp_path), hitl_cmd={"action": "ask", "text": "status?"},
    )
    reservation = ("CTX-resume-required", "RSV-resume-required")
    solver._pending_control_context_reservations = [reservation]
    solver._control_context_prompt_manifest = [{
        "context_id": reservation[0],
        "reservation_id": reservation[1],
        "text": "EXACT REQUIRED CONTINUATION ABSENT FROM ASK PROMPT",
        "required": True,
        "kind": "clue",
        "secret": False,
    }]
    solver._context_releaser = lambda context_id, **kw: (
        released.append((context_id, kw["reservation_id"])) or True)
    solver._context_committer = lambda context_id, **kw: (
        committed.append((context_id, kw["reservation_id"])) or True)

    async def _must_not_start(*args, **kwargs):
        started.append(True)
        raise AssertionError("worker process must not start")

    solver._run_streaming = _must_not_start
    with pytest.raises(
            RuntimeError, match="required operator continuation context is absent"):
        asyncio.run(solver._run_respond())

    assert built == []
    assert started == []
    assert released == []
    assert committed == []
    assert solver._runtime_process_started is False
    assert solver._pending_control_context_reservations == [reservation]


def test_run_cli_streaming_steer_event_kills_and_flags(tmp_path):
    import threading
    import time as _t
    from muteki.solver.cli_driver import run_cli_streaming, get_driver
    drv = get_driver("claude")
    steer = threading.Event()

    def fire():
        _t.sleep(0.3); steer.set()
    threading.Thread(target=fire, daemon=True).start()
    t0 = _t.time()
    res = run_cli_streaming(drv, ["sleep", "10"], cwd=str(tmp_path), timeout=30,
                            on_step=lambda s: None, steer_event=steer)
    assert res.steered is True
    assert res.cancelled is False
    assert _t.time() - t0 < 5                     # killed promptly, not at timeout


def test_local_streaming_runner_reports_complete_stdin_handoff(tmp_path):
    from muteki.solver.cli_driver import CliResult, run_cli_streaming

    class _Driver:
        name = "test"

        def parse_stream_steps(self, _line):
            return []

        def parse(self, out, err):
            return CliResult(text=out, raw_stderr=err)

    secret = "stdin-boundary-445566"
    argv = ["/bin/sh", "-c", "IFS= read -r line; printf '%s\\n' \"$line\""]
    lifecycle = []
    result = run_cli_streaming(
        _Driver(), argv, cwd=str(tmp_path), timeout=5,
        on_step=lambda _step: None,
        on_proc=lambda _proc: lifecycle.append("popen"),
        on_stdin_delivered=lambda: lifecycle.append("stdin"),
        on_stdin_uncertain=lambda: lifecycle.append("unknown"),
        stdin_text=secret + "\n",
    )

    assert secret not in "\0".join(argv)
    assert lifecycle == ["popen", "stdin"]
    assert secret in result.text


def test_prestart_cancelled_stdin_handoff_reports_unknown(tmp_path):
    from muteki.solver.cli_driver import CliResult, run_cli_streaming

    class _Driver:
        name = "test"

        def parse_stream_steps(self, _line):
            return []

        def parse(self, out, err):
            return CliResult(text=out, raw_stderr=err)

    cancel = threading.Event()
    cancel.set()
    lifecycle = []
    run_cli_streaming(
        _Driver(), ["/bin/cat"], cwd=str(tmp_path), timeout=5,
        on_step=lambda _step: None, cancel_event=cancel,
        on_proc=lambda _proc: lifecycle.append("popen"),
        on_stdin_delivered=lambda: lifecycle.append("stdin"),
        on_stdin_uncertain=lambda: lifecycle.append("unknown"),
        stdin_text="must-not-be-replayed",
    )
    assert lifecycle == ["popen", "unknown"]


# ── _extract_flag must not surface placeholders (run-1619 false-positive) ─────
def test_extract_flag_skips_placeholder_in_prose():
    ch = Challenge(id="t", name="t", category="web",
                   flag_format=r"[A-Za-z0-9_]{0,15}\{[^}]{1,200}\}")
    s = _cli_solver(ch)
    # worker mentions the template in prose but never recovered a flag
    text = "The /admin HTML did not contain flag{...}. scanning for flag{...} more."
    assert s._extract_flag(text) is None


def test_extract_flag_picks_real_flag_over_placeholder():
    ch = Challenge(id="t", name="t", category="web",
                   flag_format=r"[A-Za-z0-9_]{0,15}\{[^}]{1,200}\}")
    s = _cli_solver(ch)
    # placeholder appears first, the real flag later — must return the real one
    text = "looking for flag{...}\n...\nFOUND_FLAG=dalctf{r3al_one_h3re}"
    assert s._extract_flag(text) == "dalctf{r3al_one_h3re}"


def test_extract_flag_rejects_found_flag_marker_placeholder():
    ch = Challenge(id="t", name="t", category="web",
                   flag_format=r"[A-Za-z0-9_]{0,15}\{[^}]{1,200}\}")
    s = _cli_solver(ch)
    # even an explicit FOUND_FLAG= marker is rejected if it's a placeholder
    assert s._extract_flag("FOUND_FLAG=flag{...}") is None
    assert s._extract_flag("FOUND_FLAG=<flag>") is None


def test_extract_flag_rejects_found_flag_marker_code_expression():
    ch = Challenge(id="t", name="t", category="web",
                   flag_format=r"[A-Za-z0-9_]{0,15}\{[^}]{1,200}\}")
    s = _cli_solver(ch)
    text = 'print(f"FOUND_FLAG={out3[i:j].decode()}")'
    assert s._extract_flag(text) is None
    assert s._extract_flags(text) == []


def test_extract_need_inputs_parses_marker():
    ch = Challenge(id="t", name="t", category="web", flag_format=r"flag\{.*?\}")
    s = _cli_solver(ch)
    text = (
        "trying reverse shell...\n"
        "NEED_INPUT=a public VPS I can SSH to (I'm behind NAT)\n"
        "VERIFIED_FACT=target is behind NAT\n"
    )
    needs = s._extract_need_inputs(text)
    assert needs == ["a public VPS I can SSH to (I'm behind NAT)"]


def test_extract_need_request_preserves_worker_reported_kind():
    ch = Challenge(id="t", name="t", category="web", flag_format=r"flag\{.*?\}")
    s = _cli_solver(ch)
    text = (
        "NEED_INPUT=I need the operator to pick between two approaches\n"
        "NEED_KIND=operator_directive_needed\n"
    )
    assert s._extract_need_requests(text) == [
        ("I need the operator to pick between two approaches",
         "operator_directive_needed")
    ]


def test_stream_markers_uses_worker_reported_need_kind():
    import asyncio
    from muteki.core.events import EventType
    bus = _CaptureBus()
    ch = Challenge(id="t", name="t", category="web", flag_format=r"flag\{.*?\}",
                   target="http://x")
    s = CliSolver(None, ch, bus=bus, driver=_StubDriver(""), engine="claude", kb=False)
    asyncio.run(s._stream_markers(
        "NEED_INPUT=I need the operator to decide whether to burn the exploit\n"
        "NEED_KIND=operator_directive_needed\n"))
    reqs = [e for e in bus.events if e.event_type is EventType.HITL_REQUEST]
    assert reqs[0].payload.get("need_kind") == "operator_directive_needed"


def test_need_input_request_id_is_scoped_to_execution_occurrence():
    import asyncio
    from muteki.core.events import EventType

    ch = Challenge(id="t", name="t", category="web", flag_format=r"flag\{.*?\}",
                   target="http://x")
    text = "NEED_INPUT=provide the dashboard token\n"

    first_bus = _CaptureBus()
    first = CliSolver(
        None, ch, bus=first_bus, driver=_StubDriver(""), engine="claude", kb=False,
        run_id="run-1", execution_occurrence="attempt-1", resolve_epoch=1,
    )
    asyncio.run(first._stream_markers(text))
    first_request = next(
        event for event in first_bus.events
        if event.event_type is EventType.HITL_REQUEST)

    second_bus = _CaptureBus()
    second = CliSolver(
        None, ch, bus=second_bus, driver=_StubDriver(""), engine="claude", kb=False,
        run_id="run-1", execution_occurrence="attempt-2", resolve_epoch=2,
    )
    asyncio.run(second._stream_markers(text))
    second_request = next(
        event for event in second_bus.events
        if event.event_type is EventType.HITL_REQUEST)

    assert first_request.payload["request_id"].startswith("DR-")
    assert first_request.payload["request_id"] != second_request.payload["request_id"]
    assert first_request.payload["execution_occurrence"] == "attempt-1"
    assert first_request.payload["resolve_epoch"] == "1"
    # Reconstructing the same semantic identity inside one execution is stable.
    replay_id, _ = first._decision_request_identity(
        "provide the dashboard token", "external_blocker")
    assert replay_id == first_request.payload["request_id"]


def test_stream_markers_emits_hitl_request_on_need_input():
    """A NEED_INPUT= marker must surface a HITL_REQUEST event (the worker raising
    its hand) + a need_input blackboard delta — the dead HITL_REQUEST path is now
    wired."""
    import asyncio
    from muteki.core.events import EventType
    bus = _CaptureBus()
    ch = Challenge(id="t", name="t", category="web", flag_format=r"flag\{.*?\}",
                   target="http://x")
    s = CliSolver(None, ch, bus=bus, driver=_StubDriver(""), engine="claude", kb=False)
    asyncio.run(s._stream_markers(
        "NEED_INPUT=the target is connection-refused, instance may be expired\n"))
    reqs = [e for e in bus.events if e.event_type is EventType.HITL_REQUEST]
    assert len(reqs) == 1
    assert "connection-refused" in reqs[0].payload.get("need", "")
    # env-flavored need is classified env_down for the deck label
    assert reqs[0].payload.get("kind") == "env_down"
    assert reqs[0].payload.get("need_kind") == "external_blocker"
    # also dropped a board marker the coordinator polls
    assert "need_input" in _bb_kinds(bus.events)


def test_need_kind_field_is_separate_from_legacy_kind():
    import asyncio
    from muteki.core.events import EventType
    bus = _CaptureBus()
    ch = Challenge(id="t", name="t", category="web", flag_format=r"flag\{.*?\}",
                   target="http://x")
    s = CliSolver(None, ch, bus=bus, driver=_StubDriver(""), engine="claude", kb=False)
    asyncio.run(s._stream_markers(
        "NEED_INPUT=need exclusive access; another worker is hammering the same target\n"))
    reqs = [e for e in bus.events if e.event_type is EventType.HITL_REQUEST]
    assert reqs[0].payload.get("kind") == "need_input"
    assert reqs[0].payload.get("need_kind") == "lane_lock_request"


def test_need_kind_routes_dead_end_separately():
    import asyncio
    from muteki.core.events import EventType
    bus = _CaptureBus()
    ch = Challenge(id="t", name="t", category="web", flag_format=r"flag\{.*?\}",
                   target="http://x")
    s = CliSolver(None, ch, bus=bus, driver=_StubDriver(""), engine="claude", kb=False)
    asyncio.run(s._stream_markers(
        "NEED_INPUT=this exploit route is a dead end after repeated failures\n"))
    reqs = [e for e in bus.events if e.event_type is EventType.HITL_REQUEST]
    assert reqs[0].payload.get("kind") == "need_input"
    assert reqs[0].payload.get("need_kind") == "route_dead_end"


def test_need_input_in_worker_prompts():
    """Both the bootstrap and explore prompts must teach the worker the NEED_INPUT
    escape hatch."""
    ch = Challenge(id="t", name="t", category="web", flag_format=r"flag\{.*?\}",
                   target="http://x")
    s = _cli_solver(ch)
    assert "NEED_INPUT=" in s._build_prompt()
    s2 = _cli_solver(ch)
    s2.mode = "explore"; s2.intent_goal = "probe"
    assert "NEED_INPUT=" in s2._build_explore_prompt()


def test_review_mode_parses_actions_but_never_accepts_flags(monkeypatch, tmp_path):
    """Review-Arbiter is a control worker: it may emit route/fact/intent actions,
    but it must not solve the run even if its transcript contains FOUND_FLAG."""
    from muteki.solver import cli_solver as mod
    from muteki.solver.cli_driver import CliResult
    from muteki.swarm.shared_graph import SQLiteSharedGraph

    bus = _CaptureBus()
    ch = Challenge(id="t", name="login", category="web", flag_format=r"flag\{.*?\}",
                   target="http://x")
    sg = SQLiteSharedGraph.open(db_path=tmp_path / "g.db", challenge=ch)
    fseq = sg.add_evidence(actor="cli-a", source="a", fact="JWT likely HS256",
                           verified=True, artifact_id="a1")
    transcript = (
        'REVIEW_FINDING={"kind":"route_loop","severity":"blocker",'
        '"summary":"login SQLi repeated","route_hash":"web:login:sqli"}\n'
        f'FACT_CHALLENGE={{"fact_seq":{fseq},"reason":"no header proof",'
        '"verification_goal":"Decode a real JWT header from captured output."}\n'
        'ROUTE_SUPPRESS={"route_hash":"web:login:sqli","label":"login SQLi",'
        '"reason":"three repeats","matching_intents":[]}\n'
        'NEXT_INTENT={"worker_class":"verifier","goal":"Verify JWT alg from real token"}\n'
        'FOUND_FLAG=flag{review_must_not_win}\n'
    )
    s = CliSolver(None, ch, bus=bus, shared_graph=sg,
                  driver=_StubDriver(transcript), engine="claude", kb=False,
                  mode="review")
    monkeypatch.setattr(
        mod, "run_cli_streaming",
        lambda *a, **k: CliResult(text=transcript, session="sess-review"))

    out = asyncio.run(s.run())

    assert out.solved is False
    assert sg.snapshot().flag is None
    kinds = [e["kind"] for e in sg.events()]
    assert "review_proposal" in kinds
    assert "review_finding" not in kinds
    assert "fact_challenged" not in kinds
    assert "route_suppressed" not in kinds
    bb = _bb_kinds(bus.events)
    assert "review_proposal" in bb
