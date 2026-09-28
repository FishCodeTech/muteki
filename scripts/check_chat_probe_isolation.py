"""Model discovery and old supervised session homes must not mutate host state."""
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
        private_auth = Path(env["GROK_HOME"]) / "auth.json"
        assert private_auth.resolve() != auth.resolve()
        private_auth.write_text('{"fixture":"refreshed"}')
        return {"models": {"availableModels": []}}

    with patch("pathlib.Path.home", return_value=host), \
         patch("apps.web.worker_models.runtime_env_for_engine", return_value=SimpleNamespace(env={})), \
         patch("apps.web.worker_models.driver_for", return_value=SimpleNamespace(bin="grok", env_extra=lambda: {})), \
         patch("apps.web.worker_models._grok_model_metadata", side_effect=refresh_metadata):
        result = _run_local_discovery({"engine": "grok"}, root / "state")
        assert result.returncode == 0 and json.loads(auth.read_text())["fixture"] == "original"
        env = _private_model_probe_env("grok", root / "state", "", {})
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
        assert Path(kwargs["env"]["HOME"]).is_relative_to(root / "state")
        calls.append(True)
        return SimpleNamespace(returncode=0, stdout="Hello", stderr="")

    with patch("pathlib.Path.home", return_value=host), \
         patch("apps.web.worker_models.subprocess.run", side_effect=model_process):
        probe_worker_model(profile={"engine": "grok"}, model="grok-4.7", sessions_root=root / "state", backend="local")
    assert calls

print("PASS: discovery token refresh, old Grok symlink repair, private model test homes, Cursor memory credentials")
