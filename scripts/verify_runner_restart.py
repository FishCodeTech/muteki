#!/usr/bin/env python3
"""Kill the real managed Runner during execution and verify honest reconciliation."""
import http.client
import json
import os
from pathlib import Path
import signal
import socket
import subprocess
import tempfile
import time
import uuid
from verify_runner_ipc import read


def wait_value(control, route, predicate, timeout=40):
    deadline = time.monotonic() + timeout
    last = None
    while time.monotonic() < deadline:
        try:
            status, last = read(control, route)
            if status == 200 and predicate(last):
                return last
        except (OSError, http.client.HTTPException):
            pass
        time.sleep(0.1)
    raise AssertionError((route, last))


def main():
    binary = str(Path(".cache/target/debug/muteki-server").resolve())
    with tempfile.TemporaryDirectory(prefix="runner-restart-", dir="/tmp") as directory:
        root = Path(directory).resolve()
        control = root / "runner/control.sock"
        environment = dict(os.environ, MUTEKI_MANAGED_DESKTOP="1", MUTEKI_DESKTOP_MAINTENANCE="1")
        environment.pop("MUTEKI_RUNNER_SOCKET", None)
        with socket.socket() as listener:
            listener.bind(("127.0.0.1", 0))
            port = listener.getsockname()[1]
        with tempfile.TemporaryFile() as log:
            web = subprocess.Popen([binary, "--state-dir", str(root), "--port", str(port)],
                                   env=environment, stdout=log, stderr=log)
            runner_pid = None
            try:
                health = wait_value(control, "/v1/health", lambda _: True)
                runner_pid = health["pid"]
                run = "restart-" + uuid.uuid4().hex[:12]
                settings = {"worker_container_scope": "run", "worker_network": "bridge"}
                status, handle = read(control, "/v1/containers/ensure", {"run_id": run,
                    "owner_run_id": run, "settings": settings, "generation": 1})
                assert status == 200, handle
                contract = read(control, f"/v1/runs/{run}")[1]["run"]
                identity = contract["container_id"]
                command = {"command_id": uuid.uuid4().hex, "run_id": run, "generation": 1,
                    "spec": {"argv": ["/bin/sh", "-c", "printf 'once\\n' >> executions.txt; printf 'ready\\n'; sleep 60"],
                    "cwd": handle["container_workspace"], "env": {}, "timeout_sec": 90}}
                status, started = read(control, "/v1/workers/start", command)
                assert status == 200, started
                wait_value(control, f"/v1/workers/{started['worker_id']}/events", lambda p:
                           any(f["t"] == "out" and f["line"] == "ready" for f in p["frames"]))
                old_pid = runner_pid
                os.kill(old_pid, signal.SIGKILL)
                health = wait_value(control, "/v1/health", lambda h: h["pid"] != old_pid)
                runner_pid = health["pid"]
                recovered = wait_value(control, f"/v1/runs/{run}", lambda r: r["run"]["state"] == "stopped")
                receipt = read(control, f"/v1/commands/{command['command_id']}")[1]
                assert receipt["state"] == "outcome_unknown" and receipt["execution_stopped"]
                assert "exit" not in receipt, "Supervisor exit must not fabricate a Worker exit receipt"
                assert receipt["stop_evidence"]["container_id"] == identity
                physical = json.loads(subprocess.check_output(["docker", "inspect", identity,
                                      "--format", "{{json .State}}"], text=True))
                assert not physical["Running"] and physical["Pid"] == 0
                assert read(control, "/v1/workers/start", command)[0] == 409
                status, new_handle = read(control, "/v1/containers/ensure", {"run_id": run,
                    "owner_run_id": run, "settings": settings, "generation": 2})
                assert status == 200, new_handle
                next_command = {"command_id": uuid.uuid4().hex, "run_id": run, "generation": 2,
                    "spec": {"argv": ["/bin/true"], "cwd": new_handle["container_workspace"],
                             "env": {}, "timeout_sec": 10}}
                status, next_worker = read(control, "/v1/workers/start", next_command)
                assert status == 200, next_worker
                page = wait_value(control, f"/v1/workers/{next_worker['worker_id']}/events", lambda p:
                                  any(f["t"] == "exit" for f in p["frames"]))
                assert next(f for f in page["frames"] if f["t"] == "exit")["rc"] == 0
                assert (Path(handle["host_workspace"]) / "executions.txt").read_text().splitlines() == ["once"]
                assert read(control, f"/v1/containers/{run}/teardown", {"generation": 2})[0] == 200
                print(json.dumps({"runner_killed_during_worker": True, "automatically_restarted": True,
                    "container_exit_independently_verified": True, "worker_outcome": "unknown",
                    "no_fabricated_worker_exit": True, "old_start_rejected": True,
                    "new_generation_completed": True, "original_execution_count": 1}, indent=2))
            finally:
                if web.poll() is None:
                    web.send_signal(signal.SIGINT)
                    try:
                        web.wait(timeout=10)
                    except subprocess.TimeoutExpired:
                        web.kill()
                        web.wait()
                try:
                    runner_pid = read(control, "/v1/health")[1]["pid"]
                except (OSError, http.client.HTTPException):
                    pass
                if runner_pid is not None:
                    try:
                        os.kill(runner_pid, signal.SIGINT)
                    except ProcessLookupError:
                        pass
                    deadline = time.monotonic() + 10
                    while control.exists() and time.monotonic() < deadline:
                        time.sleep(0.05)
                names = subprocess.check_output(["docker", "ps", "-a", "--filter",
                    f"label=muteki.state_dir={root}", "--format", "{{.Names}}"], text=True).splitlines()
                if names:
                    subprocess.run(["docker", "rm", "-f", *names], check=True)


if __name__ == "__main__":
    main()
