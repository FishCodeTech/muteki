"""Run native package hooks with the chat package's private write boundary."""
from __future__ import annotations

import json
import os
from pathlib import Path
import sys

if __name__ == "__main__":
    sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
    from muteki.conversation.chat_mcp_isolation import isolated_mcp_command
    config = json.loads(Path(sys.argv[1]).read_text())
    root, state = Path(config["root"]), Path(config["state"])
    state.mkdir(parents=True, exist_ok=True)
    env = {k: v for k, v in os.environ.items() if k in {"PATH", "LANG", "LC_ALL", "SYSTEMROOT"}}
    env.update(config.get("env", {}))
    env.update(HOME=str(state), TMPDIR=str(state), PLUGIN_ROOT=str(root), PLUGIN_DATA=str(state),
               CLAUDE_PLUGIN_ROOT=str(root), CLAUDE_PLUGIN_DATA=str(state), CODEX_PLUGIN_ROOT=str(root))
    argv = isolated_mcp_command(["/bin/sh", "-c", config["command"]], root, state, env, bool(config.get("network")))
    os.chdir(state)
    os.execve(argv[0], argv, env)
