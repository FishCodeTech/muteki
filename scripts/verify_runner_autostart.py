#!/usr/bin/env python3
"""Verify Web automatically starts and reuses its independently living Runner."""
import json
import os
from pathlib import Path
import signal
import socket
import subprocess
import tempfile
import time
from verify_runner_ipc import read


def main():
    binary = str(Path(".cache/target/debug/muteki-server").resolve())
    with tempfile.TemporaryDirectory(prefix="runner-auto-", dir="/tmp") as directory:
        root = Path(directory).resolve()
        control = root / "runner/control.sock"
        environment = dict(os.environ, MUTEKI_MANAGED_DESKTOP="1", MUTEKI_DESKTOP_MAINTENANCE="1")
        environment.pop("MUTEKI_RUNNER_SOCKET", None)
        web = None
        runner_pid = None
        with tempfile.TemporaryFile() as log:
            try:
                for generation in range(2):
                    with socket.socket() as listener:
                        listener.bind(("127.0.0.1", 0))
                        port = listener.getsockname()[1]
                    web = subprocess.Popen([binary, "--state-dir", str(root), "--port", str(port)],
                                           env=environment, stdout=log, stderr=log)
                    deadline = time.monotonic() + 30
                    while True:
                        if web.poll() is not None or time.monotonic() > deadline:
                            log.seek(0)
                            raise RuntimeError(log.read().decode())
                        try:
                            with socket.create_connection(("127.0.0.1", port), timeout=0.2):
                                status, health = read(control, "/v1/health")
                                assert status == 200
                                break
                        except (OSError, ConnectionError):
                            time.sleep(0.1)
                    assert health["pid"] != web.pid
                    if runner_pid is None:
                        runner_pid = health["pid"]
                    assert health["pid"] == runner_pid, "Web restart replaced live Runner"
                    web.send_signal(signal.SIGINT)
                    assert web.wait(timeout=10) == 0
                    assert read(control, "/v1/health")[1]["pid"] == runner_pid
                print(json.dumps({"automatic_start": True, "separate_pid": True,
                                  "web_restarts": 1, "runner_reused": True}, indent=2))
            finally:
                if web is not None and web.poll() is None:
                    web.kill()
                    web.wait()
                if runner_pid is not None:
                    os.kill(runner_pid, signal.SIGINT)
                    deadline = time.monotonic() + 10
                    while control.exists() and time.monotonic() < deadline:
                        time.sleep(0.05)
                    assert not control.exists(), "Runner shutdown cleanup failed"


if __name__ == "__main__":
    main()
