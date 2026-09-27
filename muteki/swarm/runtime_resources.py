"""Run-owned detached process helpers for reusable Worker capabilities."""

from __future__ import annotations

import os
import signal
import socket
import subprocess
from pathlib import Path
from typing import Any


def launch_runtime_resource(
    *,
    command: str,
    cwd: str,
    env: dict[str, str],
    log_path: str,
    backend: str = "local",
    container_name: str = "",
) -> dict[str, Any]:
    """Launch a process outside the Worker process group.

    Commands remain Worker-selected; the host only owns lifetime, logging and
    process identity.  Container resources use docker exec's detached mode so
    the process is not tied to the Worker RCP session.
    """
    clean_command = str(command or "").strip()
    if not clean_command or len(clean_command) > 4096:
        raise ValueError("runtime resource command must be 1..4096 characters")
    log = Path(log_path)
    log.parent.mkdir(parents=True, exist_ok=True)
    if backend == "container":
        if not container_name:
            raise ValueError("container_name is required for container resources")
        args = ["docker", "exec", "-w", cwd]
        for key, value in sorted(env.items()):
            args.extend(["-e", f"{key}={value}"])
        args.extend([
            container_name, "/bin/sh", "-lc",
            'setsid /bin/sh -lc "$1" >>"$2" 2>&1 </dev/null & echo $!',
            "muteki-runtime", clean_command, str(log),
        ])
        result = subprocess.run(args, check=True, capture_output=True, text=True)
        pid = int((result.stdout or "").strip().splitlines()[-1])
        return {"backend": backend, "pid": pid, "container_name": container_name}

    with log.open("ab", buffering=0) as stream:
        proc = subprocess.Popen(
            ["/bin/sh", "-lc", f"exec {clean_command}"],
            cwd=cwd,
            env=env,
            stdin=subprocess.DEVNULL,
            stdout=stream,
            stderr=subprocess.STDOUT,
            start_new_session=True,
            close_fds=True,
        )
    return {"backend": "local", "pid": int(proc.pid), "container_name": ""}


def tcp_health(host: str, port: int, timeout: float = 1.5, *,
               backend: str = "local", container_name: str = "") -> bool:
    if backend == "container":
        if not container_name:
            return False
        code = (
            "import socket,sys;s=socket.socket();s.settimeout(float(sys.argv[3]));"
            "s.connect((sys.argv[1],int(sys.argv[2])));s.close()"
        )
        result = subprocess.run(
            ["docker", "exec", container_name, "python3", "-c", code,
             str(host), str(int(port)), str(float(timeout))],
            capture_output=True, text=True,
        )
        return result.returncode == 0
    try:
        with socket.create_connection((host, int(port)), timeout=timeout):
            return True
    except OSError:
        return False


def allocate_runtime_port(*, backend: str = "local", container_name: str = "") -> int:
    if backend == "container":
        result = subprocess.run(
            ["docker", "exec", container_name, "python3", "-c",
             "import socket;s=socket.socket();s.bind(('127.0.0.1',0));print(s.getsockname()[1]);s.close()"],
            check=True, capture_output=True, text=True,
        )
        return int((result.stdout or "").strip())
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return int(sock.getsockname()[1])


def stop_runtime_resource(resource: dict[str, Any]) -> bool:
    """Best-effort shutdown of one locally owned resource."""
    backend = str(resource.get("backend") or "")
    cleanup_command = str(resource.get("cleanup_command") or "").strip()
    if cleanup_command:
        cwd = str(resource.get("cwd") or "") or None
        env = {
            str(key): str(value)
            for key, value in dict(resource.get("env") or {}).items()
        }
        try:
            if backend == "container" and resource.get("container_name"):
                args = ["docker", "exec"]
                if cwd:
                    args.extend(["-w", cwd])
                for key, value in sorted(env.items()):
                    args.extend(["-e", f"{key}={value}"])
                args.extend([
                    str(resource["container_name"]), "/bin/sh", "-lc",
                    cleanup_command,
                ])
                subprocess.run(args, capture_output=True, text=True, timeout=10)
            elif backend == "local":
                subprocess.run(
                    ["/bin/sh", "-lc", cleanup_command], cwd=cwd,
                    # Keep cleanup inside the resource's sealed environment.
                    # Falling back to the coordinator environment would expose
                    # control-plane credentials when an old/partial row has no
                    # recorded variables.
                    env=env, capture_output=True, text=True, timeout=10,
                )
        except (OSError, subprocess.SubprocessError):
            pass
    try:
        pid = int(resource.get("pid") or 0)
    except (TypeError, ValueError):
        return False
    if pid <= 1:
        return False
    if backend == "container":
        container_name = str(resource.get("container_name") or "")
        if not container_name:
            return False
        result = subprocess.run(
            ["docker", "exec", container_name, "kill", "-TERM", f"-{pid}"],
            capture_output=True, text=True,
        )
        if result.returncode != 0:
            result = subprocess.run(
                ["docker", "exec", container_name, "kill", "-TERM", str(pid)],
                capture_output=True, text=True,
            )
        return result.returncode == 0
    if backend != "local":
        return False
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return True
    except OSError:
        pass
    try:
        os.killpg(pid, signal.SIGTERM)
        return True
    except ProcessLookupError:
        return True
    except OSError:
        try:
            os.kill(pid, signal.SIGTERM)
            return True
        except ProcessLookupError:
            return True
        except OSError:
            return False
