#!/usr/bin/env python3
"""Synchronize the Worker inventory and runtime version policy into its build context."""
from pathlib import Path
import shutil

root = Path(__file__).resolve().parents[1]
for name in ("engines.json", "capabilities.py", "browser_coord.py", "agent-browser.SKILL.md"):
    shutil.copyfile(root / "docker/worker-common" / name, root / "docker/worker" / name)
shutil.copyfile(
    root / "crates/server/src/agent_runtime/minimum_versions.json",
    root / "docker/worker/minimum_versions.json",
)
