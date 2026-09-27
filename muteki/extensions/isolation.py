"""Extension 子进程的操作系统级隔离计划。"""

from __future__ import annotations

import json
import os
import platform
import resource
import shutil
import subprocess
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

from muteki.platform.contracts.extensions import ExtensionManifest


class IsolationUnavailable(RuntimeError):
    pass


@dataclass
class ExtensionIsolationPlan:
    backend: str
    enforcement: str
    command: list[str]
    profile_path: str = ""
    filesystem_roots: list[str] = field(default_factory=list)
    network_targets: list[str] = field(default_factory=list)
    resource_limits: dict[str, int] = field(default_factory=dict)
    subprocess_policy: str = "denied"

    def public_view(self) -> dict[str, Any]:
        return {
            "backend": self.backend,
            "enforcement": self.enforcement,
            "filesystem_roots": list(self.filesystem_roots),
            "network_targets": list(self.network_targets),
            "resource_limits": dict(self.resource_limits),
            "subprocess_policy": self.subprocess_policy,
        }


def _scheme_string(value: str) -> str:
    return json.dumps(str(value))


def _resolve_executable(command: list[str], env: dict[str, str]) -> list[str]:
    if not command:
        raise IsolationUnavailable("extension command is empty")
    executable = shutil.which(command[0], path=env.get("PATH"))
    if executable is None:
        raise IsolationUnavailable(
            f"extension executable is unavailable: {command[0]!r}")
    resolved = str(Path(executable).resolve())
    # pyenv shim 会再次执行宿主 shell。使用解释器报告的真实 executable，
    # 以便 profile 只授权一个确定的运行时根。
    if Path(command[0]).name.startswith("python"):
        try:
            result = subprocess.run(
                [executable, "-c", "import sys;print(sys.executable)"],
                capture_output=True, text=True, timeout=5, check=True,
                env=env,
            )
            candidate = result.stdout.strip().splitlines()[0]
            if candidate:
                resolved = str(Path(candidate).resolve())
        except Exception:
            pass
    return [resolved, *command[1:]]


def _network_rules(targets: list[str]) -> list[str]:
    """Build Seatbelt outbound rules for declared network hosts.

    Modern macOS ``sandbox-exec`` only accepts ``remote ip`` hosts of ``*`` or
    ``localhost`` — literal resolved A/AAAA addresses fail with
    ``host must be * or localhost``. Host allowlisting therefore stays in
    ``PermissionChecker.check_network``; the OS profile only gates whether any
    outbound IP traffic is possible once the manifest declares at least one
    network target.
    """
    if not targets:
        return []
    for target in targets:
        host = target.strip().lower()
        if host.startswith("*."):
            raise IsolationUnavailable(
                f"wildcard network target cannot be enforced by sandbox-exec: {target}"
            )
    return [
        '(allow network-outbound '
        '(literal "/private/var/run/mDNSResponder"))',
        '(allow network-outbound (remote ip "*:*"))',
    ]


def _macos_crypto_read_roots(executable: Path) -> set[str]:
    """Paths Homebrew/CPython need to load ``_ssl`` and verify TLS certs.

    Without these, ``urllib`` fails inside sandbox-exec with
    ``unknown url type: https`` because ``libssl`` cannot be opened.
    """
    roots: set[str] = set()
    resolved = executable.resolve()
    for parent in resolved.parents:
        # /opt/homebrew/Cellar/python@… → allow the whole brew prefix for
        # openssl kegs linked from opt/.
        if parent.name in {"Cellar", "opt"} and parent.parent.is_dir():
            roots.add(str(parent.parent))
            break
    for candidate in (
        Path("/opt/homebrew/opt/openssl@3"),
        Path("/opt/homebrew/opt/openssl"),
        Path("/usr/local/opt/openssl@3"),
        Path("/usr/local/opt/openssl"),
        Path("/private/etc/ssl"),
        Path("/etc/ssl"),
    ):
        if candidate.exists():
            roots.add(str(candidate.resolve()))
    return roots


def _macos_plan(
    manifest: ExtensionManifest,
    command: list[str],
    *,
    package_dir: Path,
    state_dir: Path,
    workspace_root: Path | None,
    env: dict[str, str],
) -> ExtensionIsolationPlan:
    sandbox = shutil.which("sandbox-exec")
    if not sandbox:
        raise IsolationUnavailable("sandbox-exec is unavailable")
    resolved_command = _resolve_executable(command, env)
    executable = Path(resolved_command[0]).resolve()
    runtime_root = executable.parent.parent
    read_roots = {
        str(package_dir.resolve()),
        str(state_dir.resolve()),
        str(runtime_root),
        "/System", "/usr", "/bin", "/sbin", "/Library",
        "/private/var/db/timezone", "/dev",
        *_macos_crypto_read_roots(executable),
    }
    if workspace_root is not None and any(
        token in manifest.permissions.filesystem
        for token in ("workspace-read", "workspace-write")
    ):
        read_roots.add(str(workspace_root.resolve()))
    write_roots = {str(state_dir.resolve())}
    if workspace_root is not None and "workspace-write" in manifest.permissions.filesystem:
        write_roots.add(str(workspace_root.resolve()))
    lines = [
        "(version 1)",
        "(deny default)",
        # system.sb 只开放 Darwin 运行时所需的基础 IPC/loader 操作；文件与
        # 网络仍由下方根目录和 remote ip 规则限定。
        '(import "system.sb")',
        "(allow process-info*)",
        # 扩展主进程由宿主创建；进入 profile 后禁止其 fork 出额外进程。
        "(deny process-fork)",
        "(allow sysctl-read)",
        "(allow mach-lookup)",
        f"(allow process-exec (subpath {_scheme_string(str(runtime_root))}))",
        "(allow file-read-metadata)",
    ]
    for root in sorted(read_roots):
        lines.append(
            f"(allow file-read* (subpath {_scheme_string(root)}))")
    for root in sorted(write_roots):
        lines.append(
            f"(allow file-write* (subpath {_scheme_string(root)}))")
    lines.extend([
        '(allow file-write* (literal "/dev/null"))',
        '(allow file-write* (literal "/dev/tty"))',
    ])
    lines.extend(_network_rules(list(manifest.permissions.network)))
    profile_path = state_dir / ".extension-sandbox.sb"
    profile_path.parent.mkdir(parents=True, exist_ok=True)
    profile_path.write_text("\n".join(lines) + "\n", encoding="utf-8")
    os.chmod(profile_path, 0o600)
    limits = {
        "cpu_seconds": 300,
        "open_files": 128,
        "file_size_bytes": 64 * 1024 * 1024,
    }
    return ExtensionIsolationPlan(
        backend="macos-sandbox-exec",
        enforcement="enforced",
        command=[sandbox, "-f", str(profile_path), *resolved_command],
        profile_path=str(profile_path),
        filesystem_roots=sorted(read_roots),
        network_targets=list(manifest.permissions.network),
        resource_limits=limits,
        subprocess_policy="denied",
    )


def build_isolation_plan(
    manifest: ExtensionManifest,
    command: list[str],
    *,
    package_dir: Path,
    state_dir: Path,
    workspace_root: Path | None,
    env: dict[str, str],
) -> ExtensionIsolationPlan:
    if platform.system() == "Darwin":
        return _macos_plan(
            manifest, command,
            package_dir=package_dir,
            state_dir=state_dir,
            workspace_root=workspace_root,
            env=env,
        )
    raise IsolationUnavailable(
        "no enforced extension sandbox backend is configured for this platform")


def apply_resource_limits(limits: dict[str, int]) -> None:
    """子进程 exec 前设置资源上限。"""
    pairs = (
        (resource.RLIMIT_CPU, limits.get("cpu_seconds")),
        # Darwin 的 RLIMIT_AS 在 posix_spawn/preexec 路径会返回 EINVAL；
        # 支持该限制的平台才配置。
        (resource.RLIMIT_AS, limits.get("address_space_bytes")),
        (resource.RLIMIT_NOFILE, limits.get("open_files")),
        (resource.RLIMIT_FSIZE, limits.get("file_size_bytes")),
    )
    for kind, value in pairs:
        if value:
            resource.setrlimit(kind, (int(value), int(value)))


__all__ = [
    "ExtensionIsolationPlan",
    "IsolationUnavailable",
    "apply_resource_limits",
    "build_isolation_plan",
]
