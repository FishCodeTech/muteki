from __future__ import annotations

import hashlib
import json
from datetime import datetime
import os
import re
import subprocess
import threading
import time
from pathlib import Path
from typing import Any

from .contracts import ExecutionPolicy
from .store import SrcStoreError

IMAGE = "muteki-src-egress:local"
GATEWAY_SCHEMA = "2"
NAME_PATTERN = re.compile(r"^muteki-src-egress-[a-f0-9]{24}$")
CONTAINER_NETWORK_PATTERN = re.compile(r"^container:muteki-src-egress-[a-f0-9]{24}$")


def canonical_json(value: Any) -> bytes:
    return json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(",", ":")).encode("utf-8")


def _atomic_json(path: Path, value: Any) -> None:
    path.parent.mkdir(mode=0o2750, parents=True, exist_ok=True)
    temporary = path.with_name(f".{path.name}.{os.getpid()}.{time.time_ns()}.tmp")
    try:
        with temporary.open("w", encoding="utf-8") as handle:
            json.dump(value, handle, ensure_ascii=False, sort_keys=True)
            handle.flush()
            os.fsync(handle.fileno())
        os.chmod(temporary, 0o640)
        os.replace(temporary, path)
    finally:
        temporary.unlink(missing_ok=True)


class SrcEgressManager:
    def __init__(self, root: Path) -> None:
        self.root = Path(root).resolve()
        self.root.mkdir(mode=0o700, parents=True, exist_ok=True)
        self._lock = threading.RLock()
        self._task_locks: dict[str, threading.RLock] = {}
        self._execution_service: Any = None
        self._inspection_bridge: Any = None

    def register_execution_service(self, execution: Any) -> Any:
        from .egress_bridge import SrcInspectionBridge
        if self._execution_service is not None and type(self._execution_service) is not type(execution):
            raise ValueError("a different ExecutionService type is already registered")
        self._execution_service = execution
        if self._inspection_bridge is None:
            self._inspection_bridge = SrcInspectionBridge(self)
        return execution

    def register_src_service(self, src: Any) -> Any:
        return self.register_execution_service(src.execution)

    def start_inspection_bridge(self) -> None:
        if self._execution_service is None:
            raise SrcStoreError(
                "capability_unavailable",
                "register ExecutionService before starting inspection approval delivery",
            )
        self._inspection_bridge.start()

    def stop_inspection_bridge(self) -> None:
        if self._inspection_bridge is not None:
            self._inspection_bridge.stop()

    @staticmethod
    def _docker(*args: str, timeout: int = 60) -> subprocess.CompletedProcess[str]:
        return subprocess.run(["docker", *args], capture_output=True, text=True, check=False, timeout=timeout)

    @staticmethod
    def supported() -> bool:
        try:
            result = SrcEgressManager._docker("version", "--format", "{{.Server.Version}}", timeout=10)
        except (OSError, subprocess.TimeoutExpired):
            return False
        return result.returncode == 0

    @classmethod
    def content_supported(cls) -> bool:
        return cls.supported()

    def _task_lock(self, task_id: str) -> threading.RLock:
        with self._lock:
            return self._task_locks.setdefault(task_id, threading.RLock())

    def _name(self, task_id: str) -> str:
        return "muteki-src-egress-" + hashlib.sha256(task_id.encode("utf-8")).hexdigest()[:24]

    def _task_root(self, task_id: str) -> Path:
        path = self.root / hashlib.sha256(task_id.encode("utf-8")).hexdigest()[:24]
        path.mkdir(mode=0o700, parents=True, exist_ok=True)
        if path.is_symlink() or not path.resolve().is_relative_to(self.root):
            raise SrcStoreError("capability_unavailable", "SRC gateway state root is invalid")
        for subdir in ("traffic", "inspection_requests", "inspection_decisions", "inspection_grants"):
            (path / subdir).mkdir(mode=0o2770, parents=True, exist_ok=True)
        gid = os.getgid()
        os.chown(path, -1, gid)
        for candidate in [path, *path.rglob("*")]:
            if candidate.is_symlink():
                continue
            try:
                os.chown(candidate, -1, gid)
                if candidate.is_dir():
                    candidate.chmod(0o2770)
                elif candidate.name.endswith(".key"):
                    candidate.chmod(0o640)
                else:
                    candidate.chmod(0o660)
            except FileNotFoundError:
                continue
        return path

    def _policy_data(self, policy: ExecutionPolicy) -> dict[str, Any]:
        data = policy.model_dump(mode="json")
        activation = dict(data.pop("activation", {}) or {})
        data["environment_generation"] = int(
            activation.get("environment_generation") or policy.version
        )
        exceptions = list(policy.environment_requirements.get("service_exceptions") or [])
        if policy.network_mode == "enforced" and not exceptions:
            raise SrcStoreError(
                "capability_unavailable",
                "enforced Scope requires explicit control/model provider service exceptions",
            )
        for item in exceptions:
            if not isinstance(item, dict) or not item.get("host") or not item.get("port"):
                raise SrcStoreError("capability_unavailable", "invalid infrastructure service exception")
        return data

    @staticmethod
    def _digest(data: dict[str, Any]) -> str:
        return hashlib.sha256(canonical_json(data)).hexdigest()

    @staticmethod
    def _source_digest() -> str:
        context = Path(__file__).resolve().parents[2] / "docker" / "src-egress"
        digest = hashlib.sha256()
        for name in ("Dockerfile", "gateway.py", "http_proxy.py", "proxy_process.py"):
            digest.update(name.encode("utf-8"))
            digest.update((context / name).read_bytes())
        return digest.hexdigest()

    def _inspect(self, name: str) -> dict[str, Any] | None:
        result = self._docker("inspect", name)
        if result.returncode:
            return None
        try:
            rows = json.loads(result.stdout)
            return rows[0] if rows else None
        except (ValueError, IndexError):
            return None

    @classmethod
    def mount_for_gateway(cls, network: str) -> tuple[str, str]:
        """Return the verified host mount and container gateway for a Worker."""
        if not CONTAINER_NETWORK_PATTERN.fullmatch(str(network or "")):
            raise SrcStoreError("capability_unavailable", "unmanaged container network is not an SRC gateway")
        name = str(network).split(":", 1)[1]
        manager = cls.__new__(cls)
        inspected = manager._inspect(name)
        if not inspected:
            raise SrcStoreError("capability_unavailable", "SRC gateway container is unavailable")
        labels = inspected.get("Config", {}).get("Labels") or {}
        if labels.get("muteki.src.gateway_schema") != GATEWAY_SCHEMA:
            raise SrcStoreError("authorization_stale", "SRC gateway schema is stale")
        for mount in inspected.get("Mounts") or []:
            if mount.get("Destination") == "/run/src" and mount.get("Type") == "bind":
                source = str(mount.get("Source") or "")
                if source:
                    return source, name
        raise SrcStoreError("capability_unavailable", "SRC gateway state mount is missing")

    @classmethod
    def record_for_gateway(cls, requested_network: str) -> tuple[str, str, str]:
        match = re.fullmatch(r"src-record:(muteki-src-egress-[a-f0-9]{24})", str(requested_network or ""))
        if not match:
            raise SrcStoreError("capability_unavailable", "unmanaged declared-record network")
        mount, name = cls.mount_for_gateway(f"container:{match.group(1)}")
        inspected = cls.__new__(cls)._inspect(name) or {}
        address = str(inspected.get("NetworkSettings", {}).get("IPAddress") or "")
        if not address:
            raise SrcStoreError("capability_unavailable", "declared-record gateway address is unavailable")
        return mount, name, f"http://{address}:18080"

    def current(self, task_id: str, policy: ExecutionPolicy | None = None) -> dict[str, Any]:
        name = self._name(task_id)
        inspected = self._inspect(name)
        base = {"container": name, "network": f"container:{name}"}
        if not inspected or not inspected.get("State", {}).get("Running"):
            return {**base, "network_enforced": False, "content_enforced": False, "state": "unavailable"}
        labels = inspected.get("Config", {}).get("Labels") or {}
        try:
            health = json.loads((self._task_root(task_id) / "health.json").read_text(encoding="utf-8"))
        except (OSError, ValueError):
            return {**base, "network_enforced": False, "content_enforced": False, "state": "unverified"}
        try:
            started_at = datetime.fromisoformat(str(inspected["State"]["StartedAt"]).replace("Z", "+00:00")).timestamp()
            health_fresh = float(health.get("verified_at") or 0) + 0.5 >= started_at
        except (KeyError, TypeError, ValueError):
            health_fresh = False
        if not health_fresh:
            return {**base, "network_enforced": False, "content_enforced": False,
                    "state": "stale_health", "health": health}
        digest_matches = policy is None or health.get("digest") == self._digest(self._policy_data(policy))
        labels_match = (
            labels.get("muteki.src.task_id") == task_id
            and labels.get("muteki.src.gateway_schema") == GATEWAY_SCHEMA
        )
        capabilities = inspected.get("HostConfig", {}).get("CapAdd") or []
        network_capable = (
            health.get("state") == "ready" and digest_matches and labels_match
            and "NET_ADMIN" in capabilities and health.get("network_enforced") is True
        )
        network_valid = network_capable and (policy is None or policy.network_mode == "enforced")
        inspection = dict(health.get("inspection") or {})
        content_valid = network_capable and inspection.get("state") == "ready" and (
            str((policy.inspection_requirements.get("mode") if policy else inspection.get("mode")) or "off") != "enforce"
            or inspection.get("enforce_ready") is True
        )
        return {
            **base,
            "network_enforced": network_valid,
            "content_enforced": content_valid,
            "state": "ready" if network_capable else "configuration_mismatch",
            "health": health,
            "generation": health.get("generation"),
            "inspection": inspection,
        }

    def activate(self, task_id: str, policy: ExecutionPolicy) -> dict[str, Any]:
        inspection_mode = str(policy.inspection_requirements.get("mode") or "off")
        if policy.network_mode != "enforced" and inspection_mode == "off":
            return {"network_enforced": False, "content_enforced": False, "state": "declared"}
        if policy.network_mode != "enforced" and inspection_mode == "enforce":
            raise SrcStoreError(
                "inspection_unavailable",
                "content enforce requires the enforced Worker network or a separately verified managed channel",
            )
        with self._task_lock(task_id):
            if not self.supported():
                raise SrcStoreError("capability_unavailable", "Docker is unavailable for enforced Scope")
            data = self._policy_data(policy)
            digest = self._digest(data)
            name = self._name(task_id)
            root = self._task_root(task_id)
            _atomic_json(root / "policy.json", data)
            (root / "health.json").unlink(missing_ok=True)
            (root / "proxy-health.json").unlink(missing_ok=True)
            context = Path(__file__).resolve().parents[2] / "docker" / "src-egress"
            source_digest = self._source_digest()
            inspected = self._inspect(name)
            if inspected is None:
                build = self._docker("build", "-t", IMAGE, str(context), timeout=300)
                if build.returncode:
                    raise SrcStoreError(
                        "capability_unavailable",
                        f"SRC gateway image build failed: {build.stderr or build.stdout}",
                    )
                created = self._docker(
                    "run", "-d", "--name", name, "--cap-add", "NET_ADMIN",
                    "--group-add", str(os.getgid()), "--env", f"MUTEKI_SRC_GID={os.getgid()}",
                    "--add-host", "host.docker.internal:host-gateway",
                    "--label", f"muteki.src.task_id={task_id}",
                    "--label", f"muteki.src.gateway_schema={GATEWAY_SCHEMA}",
                    "--label", f"muteki.src.policy_digest={digest}",
                    "--label", f"muteki.src.source_digest={source_digest}",
                    "-v", f"{root}:/run/src", IMAGE,
                )
                if created.returncode:
                    raise SrcStoreError("capability_unavailable", f"SRC gateway start failed: {created.stderr.strip()}")
            else:
                labels = inspected.get("Config", {}).get("Labels") or {}
                if labels.get("muteki.src.source_digest") != source_digest:
                    raise SrcStoreError(
                        "authorization_stale",
                        "gateway source changed; stop dependent Runs before replacing it",
                    )
                if labels.get("muteki.src.task_id") != task_id:
                    raise SrcStoreError("task_binding_invalid", "gateway belongs to another Task")
                if labels.get("muteki.src.gateway_schema") != GATEWAY_SCHEMA:
                    raise SrcStoreError("authorization_stale", "gateway schema changed; stop dependent Runs before replacing it")
                if not inspected.get("State", {}).get("Running"):
                    started = self._docker("start", name)
                    if started.returncode:
                        raise SrcStoreError("capability_unavailable", started.stderr.strip())
                else:
                    reloaded = self._docker("kill", "--signal", "HUP", name)
                    if reloaded.returncode:
                        raise SrcStoreError("capability_unavailable", reloaded.stderr.strip())
            deadline = time.monotonic() + 30.0
            current: dict[str, Any] = {}
            while time.monotonic() < deadline:
                current = self.current(task_id, policy)
                if current.get("state") == "ready" and current.get("content_enforced"):
                    address = (self._inspect(name) or {}).get("NetworkSettings", {}).get("IPAddress", "")
                    if self._execution_service is not None:
                        self.start_inspection_bridge()
                    return {
                        **current,
                        "network_enforced": policy.network_mode == "enforced",
                        "network": f"container:{name}" if policy.network_mode == "enforced" else "",
                        "http_proxy": f"http://{address}:18080" if address else "",
                    }
                health = current.get("health") or {}
                if health.get("state") == "failed":
                    break
                time.sleep(0.1)
            raise SrcStoreError(
                "capability_unavailable",
                "SRC gateway configuration was not verified",
                detail={"current": current, "expected_digest": digest},
            )

    def recover_enforced(self, task_id: str, policy: ExecutionPolicy) -> dict[str, Any]:
        inspection_mode = str(policy.inspection_requirements.get("mode") or "off")
        if policy.network_mode != "enforced" and inspection_mode == "off":
            return {
                "network_enforced": False, "content_enforced": False,
                "state": "declared", "recovered": False,
            }
        result = self.activate(task_id, policy)
        expected_generation = int(policy.activation.get("generation") or policy.version)
        actual_digest = str((result.get("health") or {}).get("digest") or "")
        if actual_digest != self._digest(self._policy_data(policy)):
            raise SrcStoreError("authorization_stale", "recovered gateway policy digest does not match")
        if int(result.get("generation") or 0) != expected_generation:
            raise SrcStoreError("authorization_stale", "recovered gateway generation does not match")
        if self._execution_service is not None:
            self.start_inspection_bridge()
        return {**result, "recovered": True}

    def inspection_requests(self, task_id: str) -> list[dict[str, Any]]:
        if self._execution_service is None:
            raise SrcStoreError(
                "capability_unavailable", "ExecutionService is required for persistent inspection approvals")
        return self._inspection_bridge.requests(task_id)

    def decide_inspection(
        self, task_id: str, request_id: str, *, approved: bool, actor_id: str,
        task_scope: bool = False,
    ) -> Any:
        if self._execution_service is None:
            raise SrcStoreError(
                "capability_unavailable", "ExecutionService is required for persistent inspection approvals")
        return self._inspection_bridge.decide(
            task_id, request_id, approved=approved, actor_id=actor_id, task_scope=task_scope,
        )

    def revoke_inspection_grant(self, task_id: str, digest: str, *, actor_id: str) -> None:
        if self._execution_service is None:
            raise SrcStoreError(
                "capability_unavailable", "ExecutionService is required for persistent inspection approvals")
        self._inspection_bridge.revoke_grant(task_id, digest, actor_id=actor_id)

    def stop_all(self) -> None:
        self.stop_inspection_bridge()
        result = self._docker("ps", "-aq", "--filter", "name=^/muteki-src-egress-")
        if result.returncode:
            return
        names = result.stdout.split()
        if names:
            self._docker("stop", *names)


__all__ = [
    "SrcEgressManager", "IMAGE", "GATEWAY_SCHEMA", "NAME_PATTERN",
    "CONTAINER_NETWORK_PATTERN",
]
