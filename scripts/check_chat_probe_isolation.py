"""System probes share native state; explicit stored probes remain isolated."""
from __future__ import annotations

import json
from pathlib import Path
import sys
import tempfile
from types import SimpleNamespace
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from apps.web.worker_models import _private_model_probe_env, _run_local_discovery, probe_worker_model
from muteki.external_agents.grok import GrokAcpAdapter
from muteki.platform.contracts.external_agents import SessionStart


with tempfile.TemporaryDirectory(prefix="muteki-probe-isolation-") as temp:
    root = Path(temp).resolve()
    host = root / "operator"
    (host / ".grok").mkdir(parents=True)
    auth = host / ".grok/auth.json"
    auth.write_text('{"fixture":"original"}')

    async def refresh_metadata(_binary, env):
        assert env["HOME"] == str(host)
        assert not env.get("MUTEKI_CHAT_PRIVATE_ROOT")
        assert json.loads(auth.read_text())["fixture"] == "original"
        return {"models": {"availableModels": []}}

    with patch("pathlib.Path.home", return_value=host), \
         patch("pwd.getpwuid", return_value=SimpleNamespace(pw_dir=str(host), pw_name="operator")), \
         patch("apps.web.worker_models.runtime_env_for_engine", return_value=SimpleNamespace(env={})), \
         patch("apps.web.worker_models.driver_for", return_value=SimpleNamespace(bin="grok", env_extra=lambda: {})), \
         patch("apps.web.worker_models._grok_model_metadata", side_effect=refresh_metadata):
        result = _run_local_discovery({"engine": "grok"}, root / "state")
        assert result.returncode == 0 and json.loads(auth.read_text())["fixture"] == "original"
        native = _private_model_probe_env("grok", root / "state", "", {})
        assert native["HOME"] == str(host) and not native.get("GROK_HOME")
        env = _private_model_probe_env("grok", root / "state", "fixture", {"GROK_HOME": str(host / ".grok")})
        adapter = GrokAcpAdapter(runtime_root=root / "runtime")
        request = SessionStart(agent_session_id="old-session")
        old = root / "runtime/old-session"
        old.mkdir(parents=True)
        (old / "auth.json").symlink_to(auth)
        prepared = adapter._prepare_session_environment(request, env, str(root))
        destination = Path(prepared["GROK_HOME"]) / "auth.json"
        assert destination.resolve().is_relative_to(Path(env["MUTEKI_CHAT_PRIVATE_ROOT"]))
        destination.write_text('{"fixture":"session-refresh"}')
        assert json.loads(auth.read_text())["fixture"] == "original"
        cursor = _private_model_probe_env("cursor", root / "state", "fixture", {"CURSOR_API_KEY": "fake-test-key"})
        assert cursor["AGENT_CLI_CREDENTIAL_STORE"] == "memory"
        assert Path(cursor["HOME"]).is_relative_to(root / "state")

    calls = []
    def model_process(_argv, **kwargs):
        assert kwargs["env"]["HOME"] == str(host)
        assert not kwargs["env"].get("MUTEKI_CHAT_PRIVATE_ROOT")
        calls.append(True)
        return SimpleNamespace(returncode=0, stdout="Hello", stderr="")

    with patch("pathlib.Path.home", return_value=host), \
         patch("pwd.getpwuid", return_value=SimpleNamespace(pw_dir=str(host), pw_name="operator")), \
         patch("apps.web.worker_models._run_owned_process", side_effect=model_process):
        probe_worker_model(profile={"engine": "grok"}, model="grok-4.7", sessions_root=root / "state", backend="local")
    assert calls

print("PASS: native system discovery/model tests, explicit stored-probe isolation, old Grok symlink repair, Cursor memory credentials")

# Explicit environments are authoritative and never inspect ambient user files.
from muteki.solver.credential_accounts import detect_system_login, runtime_env_for_engine
with tempfile.TemporaryDirectory(prefix="muteki-login-detection-") as temp:
    root = Path(temp)
    home = root / "home"
    kimi = root / "selected-kimi"
    (kimi / "credentials").mkdir(parents=True)
    (kimi / "credentials/kimi-code.json").write_text("{}")
    (home / ".factory").mkdir(parents=True)
    (home / ".factory/auth.v2.loginkeychain").write_text("fixture")
    assert detect_system_login("kimi", {"HOME": str(home), "KIMI_CODE_HOME": str(kimi)}) == "present"
    assert detect_system_login("kimi", {"HOME": str(home)}) == "absent"
    assert detect_system_login("droid", {"HOME": str(home)}) == "present"
    assert detect_system_login("droid", {}) == "unknown"
    assert detect_system_login("codex", {}) == "unknown"
    assert detect_system_login("devin", {}) == "unknown"
    for engine in ("opencode", "pi", "omp"):
        resolved = runtime_env_for_engine(engine, account_id="", env={"HOME": str(home)}, agent_state_dir=root / "unused")
        assert not resolved.env
    assert not (root / "unused").exists()
    resolved = runtime_env_for_engine("codex", account_root=root / "accounts", account_id="missing", env={"OPENAI_API_KEY": "ambient-test"})
    assert not resolved.env.get("OPENAI_API_KEY")
print("PASS: scoped Kimi/Droid detection, explicit empty env, native state paths, no stored-account ambient fallback")

from muteki.core.runtime_env import native_user_environment
with tempfile.TemporaryDirectory(prefix="muteki-native-environment-") as temp:
    root = Path(temp).resolve()
    native = root / "native"
    private = root / "agent-private"
    poisoned = {"HOME": str(private), "MUTEKI_CHAT_PRIVATE_ROOT": str(private),
                "XDG_CONFIG_HOME": str(private / ".config"),
                "XDG_DATA_HOME": str(private / ".local/share"),
                "CODEX_HOME": str(private / ".codex"), "KIMI_CODE_HOME": str(private / ".kimi-code")}
    with patch("pwd.getpwuid", return_value=SimpleNamespace(pw_dir=str(native), pw_name="operator")), patch.dict("os.environ", poisoned, clear=True):
        for supplied in (None, {}, dict(poisoned)):
            actual = native_user_environment(supplied)
            assert actual["HOME"] == str(native)
            assert actual["XDG_CONFIG_HOME"] == str(native / ".config")
            assert actual["XDG_DATA_HOME"] == str(native / ".local/share")
            assert actual["CODEX_HOME"] == actual["KIMI_CODE_HOME"] == ""
            assert not actual["MUTEKI_CHAT_PRIVATE_ROOT"]
        selected = {"CODEX_HOME": str(root / "registered/codex-home")}
        assert native_user_environment(selected)["CODEX_HOME"] == selected["CODEX_HOME"]
        candidate = {**poisoned, "MUTEKI_HOST_DISCOVERY": "0"}
        assert native_user_environment(candidate) == candidate
    assert not native.exists()
print("PASS: polluted inherited HOME/XDG/provider roots normalized, explicit account/candidate roots preserved")

with tempfile.TemporaryDirectory(prefix="muteki-devin-native-login-") as temp:
    root = Path(temp).resolve()
    native = root / "native"
    credential = native / ".local/share/devin/credentials.toml"
    credential.parent.mkdir(parents=True)
    credential.write_text("fixture")
    poisoned = {"HOME": str(root / "private"), "XDG_DATA_HOME": str(root / "private/data")}
    with patch("pwd.getpwuid", return_value=SimpleNamespace(pw_dir=str(native), pw_name="operator")), \
         patch.dict("os.environ", poisoned, clear=True), \
         patch("muteki.external_agents.devin.subprocess.run", side_effect=AssertionError("Native credentials should avoid a network probe")):
        assert detect_system_login("devin", fresh=True) == "present"
        assert detect_system_login("devin", {}) == "unknown"
        assert detect_system_login("devin", {"MUTEKI_HOST_DISCOVERY": "0"}) == "disabled"
print("PASS: Devin detection consumes normalized native state and respects explicit empty/candidate environments")

import os
with tempfile.TemporaryDirectory(prefix="muteki-default-local-account-") as temp:
    root = Path(temp).resolve()
    native = root / "native"
    accounts = root / "accounts"
    selected = accounts / "selected"
    selected.mkdir(parents=True)
    (selected / "API_KEY").write_text("selected-test-key")
    ambient = {"HOME": str(root / "private"), "OPENAI_API_KEY": "ambient-test-key", "CLAUDE_CODE_OAUTH_TOKEN": "ambient-oauth"}
    with patch("pwd.getpwuid", return_value=SimpleNamespace(pw_dir=str(native), pw_name="operator")), patch.dict(os.environ, ambient, clear=True):
        for supplied in (None, os.environ):
            resolved = runtime_env_for_engine("codex", account_root=accounts, env=supplied)
            assert resolved.account_id == ""
            assert resolved.env["HOME"] == str(native)
            assert resolved.env["XDG_DATA_HOME"] == str(native / ".local/share")
            assert "OPENAI_API_KEY" not in resolved.env  # Inherited unchanged at launch.
        empty = runtime_env_for_engine("codex", account_root=accounts, env={})
        assert empty.account_id == "" and empty.env == {}
        explicit = runtime_env_for_engine("codex", account_root=accounts, account_id="selected")
        assert explicit.env["OPENAI_API_KEY"] == "selected-test-key"
        configured = runtime_env_for_engine("codex", account_root=accounts, env={"MUTEKI_CODEX_ACCOUNT_ID": "selected", **ambient})
        assert configured.account_id == "selected" and configured.env["OPENAI_API_KEY"] == "selected-test-key"
        implicit_container = runtime_env_for_engine("codex", account_root=accounts, container=True)
        assert implicit_container.account_id == "codex-main"
        assert "OPENAI_API_KEY" not in implicit_container.env
        legacy_container = runtime_env_for_engine("claude", account_root=accounts, container=True)
        assert legacy_container.account_id == "claude-main"
        assert legacy_container.env["CLAUDE_CODE_OAUTH_TOKEN"] == "ambient-oauth"
        explicit_container = runtime_env_for_engine("codex", account_root=accounts, account_id="selected", container=True)
        assert explicit_container.env["OPENAI_API_KEY_FILE"].endswith("/selected/API_KEY")
        assert "OPENAI_API_KEY" not in explicit_container.env
    assert not native.exists()
print("PASS: default local system account, configured accounts, explicit empty env, preserved implicit container contract")
