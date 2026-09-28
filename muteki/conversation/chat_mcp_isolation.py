"""Filesystem-isolated stdio MCP process launch, separate from platform hosts."""
from __future__ import annotations

import json
from pathlib import Path
import platform
import shutil

from muteki.extensions.isolation import build_isolation_plan, IsolationUnavailable
from muteki.platform.contracts.extensions import ExtensionManifest, ExtensionPermissions


def isolated_mcp_command(argv: list[str], package: Path, state: Path, env: dict[str, str], network: bool) -> list[str]:
    if platform.system() == "Darwin":
        plan = build_isolation_plan(
            ExtensionManifest(permissions=ExtensionPermissions(
                filesystem=["state-read", "state-write"], network=["mcp-network"] if network else [])),
            argv, package_dir=package, state_dir=state, workspace_root=None, env=env,
        )
        profile = Path(plan.profile_path)
        source = profile.read_text().replace("(deny process-fork)", "(allow process-fork)")
        # npm/uv wrappers legitimately spawn their server. Every descendant
        # inherits the same state-only write boundary.
        for root in ("/opt/homebrew", "/usr/local", "/bin", "/usr", str(package), str(state)):
            source += f"\n(allow file-read* (subpath {json.dumps(root)}))"
            source += f"\n(allow process-exec (subpath {json.dumps(root)}))"
        profile.write_text(source + "\n")
        return plan.command
    if platform.system() == "Linux" and (bwrap := shutil.which("bwrap")):
        binary = shutil.which(argv[0], path=env.get("PATH"))
        if not binary:
            raise IsolationUnavailable("MCP executable unavailable")
        command = [bwrap, "--die-with-parent", "--new-session", "--unshare-all"]
        if network:
            command += ["--share-net"]
        # Read-only host resources support existing local server installations;
        # a private HOME and the sole writable bind prevent global installs.
        command += ["--ro-bind", "/", "/", "--bind", str(state), str(state),
                    "--proc", "/proc", "--dev", "/dev", "--chdir", str(state), "--", binary, *argv[1:]]
        return command
    raise IsolationUnavailable("当前系统没有 stdio MCP 隔离器；可使用 HTTP MCP，Linux 可配置 bubblewrap")
