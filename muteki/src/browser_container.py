from __future__ import annotations

import hashlib
import json
import os
import subprocess
from pathlib import Path
from typing import Any

from .browser import AgentBrowserTaskDriver
from .egress import CONTAINER_NETWORK_PATTERN
from .store import SrcStoreError

IMAGE = "muteki-src-browser:local"


class DockerAgentBrowserTaskDriver(AgentBrowserTaskDriver):
    """Runs Task browser daemons inside the verified Task gateway netns."""

    def __init__(self, executable: str = "/usr/local/bin/agent-browser") -> None:
        super().__init__(executable)
        self._environments: dict[Path, dict[str, Any]] = {}

    @staticmethod
    def _docker(*args: str, timeout: int = 60) -> subprocess.CompletedProcess[str]:
        return subprocess.run(["docker", *args], capture_output=True, text=True, check=False, timeout=timeout)

    @staticmethod
    def supported() -> bool:
        try:
            result = DockerAgentBrowserTaskDriver._docker("version", "--format", "{{.Server.Version}}", timeout=10)
        except (OSError, subprocess.TimeoutExpired):
            return False
        return result.returncode == 0

    @staticmethod
    def _name(task_root: Path) -> str:
        return "muteki-src-browser-" + hashlib.sha256(str(task_root).encode()).hexdigest()[:24]

    def _inspect(self, name: str) -> dict[str, Any] | None:
        result = self._docker("inspect", name)
        if result.returncode:
            return None
        try:
            rows = json.loads(result.stdout)
            return rows[0] if rows else None
        except (ValueError, IndexError):
            return None

    def configure(self, task_root: Path, environment: dict[str, Any], *, _rebuild: bool = False) -> None:
        self._environments[Path(task_root)] = dict(environment)
        if environment.get("network_mode") != "enforced":
            return
        if not self.supported():
            raise SrcStoreError("capability_unavailable", "Docker is required for an enforced Task browser")
        network = str(environment.get("network") or "")
        if not CONTAINER_NETWORK_PATTERN.fullmatch(network):
            raise SrcStoreError("capability_unavailable", "Task browser gateway network is not managed")
        gateway = self._inspect(network.split(":", 1)[1])
        if not gateway or not gateway.get("State", {}).get("Running"):
            raise SrcStoreError("capability_unavailable", "Task browser gateway is not running")
        name = self._name(task_root)
        inspected = self._inspect(name)
        image_result = self._docker("image", "inspect", "--format", "{{.Id}}", IMAGE)
        if image_result.returncode:
            context = Path(__file__).resolve().parents[2] / "docker" / "src-browser"
            build = self._docker("build", "-t", IMAGE, str(context), timeout=600)
            if build.returncode:
                raise SrcStoreError(
                    "capability_unavailable",
                    f"Task browser image build failed: {build.stderr or build.stdout}",
                )
            image_result = self._docker("image", "inspect", "--format", "{{.Id}}", IMAGE)
        if image_result.returncode or not image_result.stdout.strip():
            raise SrcStoreError("capability_unavailable", "Task browser image identity is unavailable")
        image_id = image_result.stdout.strip()
        if inspected is not None:
            labels = inspected.get("Config", {}).get("Labels") or {}
            if labels.get("muteki.src.browser_image_id") != image_id:
                removed = self._docker("rm", "-f", name)
                if removed.returncode:
                    raise SrcStoreError("capability_unavailable", f"Task browser image update failed: {removed.stderr.strip()}")
                inspected = None
        if inspected is None:
            args = [
                "run", "-d", "--name", name,
                "--user", f"{os.getuid()}:{os.getgid()}",
                "--network", network,
                "--label", f"muteki.src.browser_root={task_root}",
                "--label", "muteki.src.browser_schema=1",
                "--label", f"muteki.src.browser_image_id={image_id}",
                "-v", f"{task_root}:{task_root}",
            ]
            gateway_mount = str(environment.get("gateway_mount") or "")
            if gateway_mount:
                ca_file = Path(gateway_mount) / "ca.pem"
                if ca_file.is_file():
                    args.extend(["-v", f"{ca_file}:/run/src/ca.pem:ro"])
            created = self._docker(*args, IMAGE, timeout=60)
            if created.returncode:
                raise SrcStoreError("capability_unavailable", f"Task browser container start failed: {created.stderr.strip()}")
            inspected = self._inspect(name)
        elif not inspected.get("State", {}).get("Running"):
            started = self._docker("start", name)
            if started.returncode:
                raise SrcStoreError("capability_unavailable", started.stderr.strip())
            inspected = self._inspect(name)
        actual_network = str((inspected or {}).get("HostConfig", {}).get("NetworkMode") or "")
        accepted_networks = {network, f"container:{gateway.get('Id', '')}"}
        if actual_network not in accepted_networks:
            if _rebuild:
                raise SrcStoreError("capability_unavailable", "Task browser network rebuild did not apply the gateway")
            removed = self._docker("rm", "-f", name)
            if removed.returncode:
                raise SrcStoreError("capability_unavailable", f"Task browser rebuild failed: {removed.stderr.strip()}")
            self._environments.pop(Path(task_root), None)
            self.configure(task_root, environment, _rebuild=True)
            return
        if str((inspected or {}).get("Config", {}).get("Labels", {}).get("muteki.src.browser_root") or "") != str(task_root):
            raise SrcStoreError("task_binding_invalid", "Task browser container belongs to another root")

    def _environment(self, task_root: Path) -> dict[str, Any]:
        return self._environments.get(Path(task_root), {"network_mode": "declared"})

    def _run(
        self, task_root: Path, context_id: str, profile_path: Path,
        *args: str, timeout: int = 60,
    ) -> subprocess.CompletedProcess[str]:
        environment = self._environment(task_root)
        environment_values = self._env(task_root, context_id, profile_path)
        proxy = str(environment.get("http_proxy") or "")
        if proxy:
            environment_values.update({
                "HTTP_PROXY": proxy, "HTTPS_PROXY": proxy,
                "http_proxy": proxy, "https_proxy": proxy,
            })
        if environment.get("ca_path"):
            environment_values["NODE_EXTRA_CA_CERTS"] = str(environment["ca_path"])
        if environment.get("network_mode") != "enforced":
            try:
                result = subprocess.run(
                    [self.executable, *args], capture_output=True, text=True,
                    check=False, timeout=timeout, env=environment_values,
                )
            except (OSError, subprocess.TimeoutExpired) as exc:
                raise SrcStoreError(
                    "capability_unavailable", f"Task browser command failed: {type(exc).__name__}: {exc}",
                ) from exc
            if result.returncode:
                detail = result.stderr.strip() or result.stdout.strip() or f"exit {result.returncode}"
                raise SrcStoreError("capability_unavailable", f"Task browser {args[0]} failed: {detail}")
            return result
        allowed = {
            "HOME", "PATH", "XDG_CONFIG_HOME", "XDG_CACHE_HOME", "XDG_DATA_HOME",
            "AGENT_BROWSER_SESSION", "AGENT_BROWSER_NAMESPACE", "AGENT_BROWSER_PROFILE",
            "AGENT_BROWSER_SOCKET_DIR", "AGENT_BROWSER_EXECUTABLE_PATH",
            "HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "NO_PROXY",
            "http_proxy", "https_proxy", "all_proxy", "no_proxy",
            "SSL_CERT_FILE", "SSL_CERT_DIR", "NODE_EXTRA_CA_CERTS",
        }
        environment_values = {key: value for key, value in environment_values.items() if key in allowed}
        command = ["env", *[f"{key}={value}" for key, value in environment_values.items()], self.executable, *args]
        name = self._name(task_root)
        try:
            result = self._docker("exec", name, *command, timeout=timeout)
        except (OSError, subprocess.TimeoutExpired) as exc:
            raise SrcStoreError("capability_unavailable", f"Task browser container command failed: {type(exc).__name__}: {exc}") from exc
        if result.returncode:
            detail = result.stderr.strip() or result.stdout.strip() or f"exit {result.returncode}"
            raise SrcStoreError("capability_unavailable", f"Task browser {args[0]} failed: {detail}")
        return result

    def ensure_context(self, task_root: Path, context_id: str, profile_path: Path) -> dict[str, Any]:
        environment = self._environment(task_root)
        if environment.get("ca_path"):
            command = [
                "exec", self._name(task_root), "certutil", "-A",
                "-n", "Muteki SRC Task CA", "-t", "CT,C,C",
                "-d", f"sql:{profile_path}", "-i", str(environment["ca_path"]),
            ] if environment.get("network_mode") == "enforced" else [
                "certutil", "-A", "-n", "Muteki SRC Task CA", "-t", "CT,C,C",
                "-d", f"sql:{profile_path}", "-i", str(environment["ca_path"]),
            ]
            result = self._docker(*command) if command[0] == "exec" else subprocess.run(command, capture_output=True, text=True, check=False)
            if result.returncode:
                if environment.get("inspection_mode") == "enforce":
                    raise SrcStoreError("inspection_unavailable", f"Task browser CA setup failed: {result.stderr.strip()}")
                environment.setdefault("coverage_limitations", []).append("browser_ca_unavailable")
        actual = super().ensure_context(task_root, context_id, profile_path)
        return {
            **actual,
            "network_mode": environment.get("network_mode", "declared"),
            "network": environment.get("network", "host"),
            "container": self._name(task_root) if environment.get("network_mode") == "enforced" else "",
            "inspection_mode": environment.get("inspection_mode", "off"),
            "policy_digest": environment.get("policy_digest", ""),
            "coverage_limitations": list(environment.get("coverage_limitations") or []),
        }

    def shutdown(self, task_root: Path) -> None:
        name = self._name(task_root)
        if self._inspect(name) is not None:
            result = self._docker("stop", name)
            if result.returncode:
                raise SrcStoreError("capability_unavailable", f"Task browser shutdown failed: {result.stderr.strip()}")

    def close_task(self, task_root: Path, context_id: str, profile_path: Path) -> None:
        super().close_task(task_root, context_id, profile_path)
        if self._environment(task_root).get("network_mode") == "enforced":
            result = self._docker("stop", self._name(task_root))
            if result.returncode:
                raise SrcStoreError("capability_unavailable", f"Task browser container stop failed: {result.stderr.strip()}")


__all__ = ["DockerAgentBrowserTaskDriver", "IMAGE"]
