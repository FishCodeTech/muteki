#!/usr/bin/env python3
"""Worker release smoke for GitHub Actions (and local pull verification).

Runs BEFORE multi-arch index / latest publication. Failure must block the
digest artifact upload so merge cannot publish that architecture.

Checks (generic product surface — no provider credentials):
  1. Supervisor Hello handshake (hello=1, version=muteki-runtime-agent/2)
  2. Health
  3. StartWorker /bin/true → clean exit
  4. StartWorker sleep with timeout_sec=1 → timed_out classification
  5. Blackboard helper runnable via StartWorker
  6. Credential-account projection mount readable
  7. Core engine CLI paths resolve (eight container engines)
  8. TeardownRun

Usage:
  python3 scripts/smoke_worker_release.py \\
      --image ghcr.io/<owner>/muteki-worker-slim@sha256:... \\
      --variant slim|full [--platform linux/amd64|linux/arm64]
"""

from __future__ import annotations

import argparse
import json
import os
import secrets
import shutil
import socket
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path
from typing import Any, Optional


AGENT_VERSION = "muteki-runtime-agent/2"
ENGINE_BINS = (
    ("claude", "claude"),
    ("codex", "codex"),
    ("cursor", "/home/kali/.local/bin/cursor-agent"),
    ("pi", "pi"),
    ("omp", "/home/kali/.local/bin/omp"),
    ("kimi", "kimi"),
    ("grok", "/home/kali/.grok/bin/grok"),
    ("opencode", "opencode"),
    ("droid", "droid"),
)


class SmokeError(RuntimeError):
    pass


def frame_rc(frame: dict[str, Any], default: int = 0) -> int:
    """Exit rc from a worker frame.

    Go encodes Rc with json omitempty, so successful exits omit the field.
    Treat missing/None as default (0), never as failure via truthy checks.
    """
    if "rc" not in frame or frame["rc"] is None:
        return default
    return int(frame["rc"])


class SmokeListener:
    """Minimal host-side RCP receiver for one smoke run."""

    def __init__(self, run_id: str, token: str, bind_host: str = "0.0.0.0") -> None:
        self.run_id = run_id
        self.token = token
        self.bind_host = bind_host
        self._sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        self._sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        self._sock.bind((bind_host, 0))
        self._sock.listen(1)
        self.port = self._sock.getsockname()[1]
        self._conn: Optional[socket.socket] = None
        self._reader: Optional[threading.Thread] = None
        self._lock = threading.Lock()
        self._buf = b""
        self._frames: list[dict[str, Any]] = []
        self._event = threading.Event()
        self.hello: Optional[dict[str, Any]] = None
        self._closed = False

    def accept(self, timeout: float = 60.0) -> None:
        self._sock.settimeout(timeout)
        conn, _addr = self._sock.accept()
        conn.settimeout(60.0)
        self._conn = conn
        line = self._readline(conn)
        hello = json.loads(line)
        if hello.get("hello") != 1:
            raise SmokeError(f"bad hello marker: {hello!r}")
        if hello.get("run_id") != self.run_id:
            raise SmokeError(f"run_id mismatch: {hello!r}")
        if hello.get("token") != self.token:
            raise SmokeError("token mismatch")
        version = str(hello.get("version") or "")
        if version != AGENT_VERSION:
            raise SmokeError(f"unexpected agent version {version!r}")
        self.hello = hello
        conn.sendall(b'{"ok":true}\n')
        self._reader = threading.Thread(target=self._read_loop, daemon=True)
        self._reader.start()

    def _readline(self, conn: socket.socket) -> str:
        while b"\n" not in self._buf:
            chunk = conn.recv(65536)
            if not chunk:
                raise SmokeError("connection closed before hello")
            self._buf += chunk
        line, self._buf = self._buf.split(b"\n", 1)
        return line.decode("utf-8", errors="replace")

    def _read_loop(self) -> None:
        assert self._conn is not None
        conn = self._conn
        try:
            while not self._closed:
                while b"\n" not in self._buf:
                    chunk = conn.recv(65536)
                    if not chunk:
                        return
                    self._buf += chunk
                line, self._buf = self._buf.split(b"\n", 1)
                if not line.strip():
                    continue
                frame = json.loads(line.decode("utf-8", errors="replace"))
                with self._lock:
                    self._frames.append(frame)
                    self._event.set()
        except OSError:
            return

    def send(self, obj: dict[str, Any]) -> None:
        if self._conn is None:
            raise SmokeError("no connection")
        self._conn.sendall((json.dumps(obj) + "\n").encode("utf-8"))

    def wait_frame(self, predicate, timeout: float = 30.0) -> dict[str, Any]:
        deadline = time.time() + timeout
        while time.time() < deadline:
            with self._lock:
                for index, frame in enumerate(self._frames):
                    if predicate(frame):
                        self._frames.pop(index)
                        return frame
                self._event.clear()
            remaining = deadline - time.time()
            if remaining <= 0:
                break
            self._event.wait(timeout=min(1.0, remaining))
        raise SmokeError(f"timeout waiting for frame ({timeout}s)")

    def request(self, op: str, *, timeout: float = 30.0, **fields: Any) -> dict[str, Any]:
        req_id = secrets.randbelow(1_000_000_000) + 1
        payload = {"op": op, "req_id": req_id, **fields}
        self.send(payload)
        return self.wait_frame(
            lambda frame: int(frame.get("req_id") or 0) == req_id
            and frame.get("t") in {"resp", "started", "exit", "stdin", "out", "err"},
            timeout=timeout,
        )

    def close(self) -> None:
        self._closed = True
        try:
            if self._conn is not None:
                self._conn.close()
        except OSError:
            pass
        try:
            self._sock.close()
        except OSError:
            pass


def docker(*args: str, check: bool = True, timeout: float = 120.0) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        ["docker", *args],
        capture_output=True,
        text=True,
        encoding="utf-8",
        errors="replace",
        timeout=timeout,
        check=check,
    )


def host_gateway_args() -> list[str]:
    # Linux GitHub runners need the explicit host-gateway mapping.
    return ["--add-host", "host.docker.internal:host-gateway"]


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--image", required=True)
    parser.add_argument("--variant", choices=("slim", "full"), required=True)
    parser.add_argument("--platform", default="")
    parser.add_argument("--keep", action="store_true")
    args = parser.parse_args()
    run_id = f"smoke-{secrets.token_hex(6)}"
    listener = SmokeListener(run_id, secrets.token_hex(32))
    tmp = Path(tempfile.mkdtemp(prefix="muteki-smoke-"))
    control_dir, workspace = tmp / "control", tmp / "workspace"
    account = tmp / "accounts" / "smoke-account"
    state = account / "codex-home"
    for path in (control_dir, workspace, state):
        path.mkdir(parents=True)
    state.chmod(0o777)
    (control_dir / "token").write_text(listener.token)
    (control_dir / "token").chmod(0o600)
    (account / "API_KEY").write_text("synthetic-smoke-key")
    (account / "API_KEY").chmod(0o444)
    name = f"muteki-release-{run_id}"

    def start(argv: list[str], **limits: Any) -> str:
        frame = listener.request("StartWorker", spec={
            "argv": argv, "cwd": "/home/kali/workspace", "timeout_sec": 30,
            "tag": "release-smoke", **limits,
        })
        if frame.get("t") != "started" or frame.get("error") or not frame.get("worker_id"):
            raise SmokeError(f"Worker start failed: {frame}")
        return str(frame["worker_id"])

    def finish(worker_id: str, expected: str = "success", timeout: float = 40) -> list[str]:
        lines: list[str] = []
        deadline = time.monotonic() + timeout
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise SmokeError("Worker did not produce an exit frame")
            frame = listener.wait_frame(lambda f: f.get("worker_id") == worker_id
                                        and f.get("t") in {"out", "err", "exit"}, remaining)
            if frame.get("t") != "exit":
                lines.append(str(frame.get("line") or ""))
                continue
            causes = [k for k in ("oom", "timed_out", "output_limit", "disk_limit") if frame.get(k)]
            if expected == "success":
                if frame_rc(frame) != 0 or causes:
                    raise SmokeError(f"Worker failed: {frame}; output={lines[-3:]}")
            elif expected == "killed":
                if frame.get("signalled") != 9 or causes:
                    raise SmokeError(f"cancel classification failed: {frame}")
            elif causes != [expected]:
                raise SmokeError(f"expected {expected}, received {frame}")
            return lines

    result = 1
    try:
        run_cmd = ["run", "-d", "--name", name, "--memory", "512m", "--cpus", "2",
                   "--pids-limit", "256", "--security-opt", "no-new-privileges=true",
                   *host_gateway_args(),
                   "--mount", f"type=bind,source={control_dir},target=/run/muteki/control",
                   "--mount", f"type=bind,source={account.parent},target=/run/muteki/accounts,readonly",
                   "--mount", f"type=bind,source={state},target=/run/muteki/accounts/smoke-account/codex-home",
                   "--mount", f"type=bind,source={workspace},target=/home/kali/workspace",
                   "--entrypoint", "/opt/muteki/runtime_agent", args.image,
                   "--workspace", "/home/kali/workspace",
                   "--connect", f"host.docker.internal:{listener.port}", "--run-id", run_id]
        if args.platform:
            run_cmd[1:1] = ["--platform", args.platform]
        docker(*run_cmd, timeout=60)
        listener.accept()
        health = listener.request("Health")
        if health.get("ok") is not True or health.get("version") != AGENT_VERSION:
            raise SmokeError(f"Health failed: {health}")
        print("OK Supervisor Hello / Health")
        finish(start(["/bin/true"]))
        finish(start(["python3", "/usr/local/bin/blackboard.py", "--help"]))
        print("OK Worker exit / Blackboard")

        finish(start(["/bin/sh", "-c", "set -eu; test \"$(id -u)\" != 0; "
                      "grep -Eq 'NoNewPrivs:[[:space:]]*1' /proc/self/status; "
                      "if sudo -n true 2>/dev/null; then exit 1; fi; "
                      "test -r /run/muteki/accounts/smoke-account/API_KEY; "
                      "if echo bad > /run/muteki/accounts/smoke-account/API_KEY 2>/dev/null; then exit 1; fi; "
                      "echo refreshed > /run/muteki/accounts/smoke-account/codex-home/state; "
                      "test ! -e /run/muteki/accounts/unselected-account"]))
        print("OK unprivileged Worker / read-only credential / writable refresh state")
        if args.variant == "full":
            finish(start(["/bin/sh", "-c", "set -eu; "
                          "nmap --version >/dev/null; gdb --version >/dev/null; "
                          "if sudo -n dpkg --configure -a 2>/dev/null; then exit 1; fi"]))
            print("OK full default profile: nmap / gdb runnable, package privilege denied")
        checks = "; ".join(f'command -v "{path}" >/dev/null || exit 1' for _, path in ENGINE_BINS)
        finish(start(["/bin/sh", "-c", checks]))
        print("OK all eight engine paths")

        finish(start(["/bin/sleep", "10"], timeout_sec=1), "timed_out")
        long = finish(start(["python3", "-c", "print('x'*(6*1024*1024))"],
                            output_limit_bytes=8*1024*1024))
        if sum(len(line) for line in long) != 6*1024*1024:
            raise SmokeError("long output line was truncated")
        finish(start(["python3", "-c", "import sys,time;sys.stdout.write('x'*(4*1024*1024));sys.stdout.flush();time.sleep(10)"],
                     output_limit_bytes=128*1024), "output_limit")
        finish(start(["python3", "-c", "open('disk-smoke.bin','wb').write(b'x'*(1024*1024))"],
                     disk_limit_bytes=128*1024), "disk_limit")
        print("OK timeout / long output / output limit / fast disk growth")

        survivor = start(["/bin/sh", "-c", "sleep 1; echo SURVIVED"])
        cancelled = start(["/bin/sleep", "10"])
        reply = listener.request("Signal", worker_id=cancelled, signal="KILL")
        if reply.get("ok") is not True:
            raise SmokeError(f"cancel rejected: {reply}")
        finish(cancelled, "killed")
        if "SURVIVED" not in finish(survivor):
            raise SmokeError("cancelling a Worker affected its sibling")
        print("OK cancellation isolation")
        # Fill the pages: zero-initialized bytearray/calloc can remain lazy and
        # exit successfully without ever approaching the cgroup memory limit.
        finish(start(["python3", "-c", "x=[b'x'*(32*1024*1024) for _ in range(64)]"], timeout_sec=60),
               "oom", timeout=75)
        print("OK cgroup OOM attribution")
        teardown = listener.request("TeardownRun")
        if teardown.get("ok") is not True:
            raise SmokeError(f"TeardownRun failed: {teardown}")
        print("OK TeardownRun")
        if args.variant == "full":
            elevated_script = r"""set -eu
nmap --version >/dev/null
gdb --version >/dev/null
test "$(id -u)" != 0
test "$(sudo -n id -u)" = 0
pkg="$(mktemp -d)"
mkdir -p "$pkg/DEBIAN" "$pkg/usr/local/bin"
printf '%s\n' \
  'Package: muteki-privilege-smoke' \
  'Version: 1.0' \
  'Section: misc' \
  'Priority: optional' \
  'Architecture: all' \
  'Maintainer: Muteki Smoke <smoke@example.invalid>' \
  'Description: synthetic package installation check' > "$pkg/DEBIAN/control"
printf '#!/bin/sh\necho PACKAGE_INSTALLED\n' > "$pkg/usr/local/bin/muteki-privilege-smoke"
chmod +x "$pkg/usr/local/bin/muteki-privilege-smoke"
dpkg-deb --build "$pkg" /tmp/muteki-privilege-smoke.deb >/dev/null
sudo -n dpkg -i /tmp/muteki-privilege-smoke.deb >/dev/null
test "$(muteki-privilege-smoke)" = PACKAGE_INSTALLED
"""
            elevated_cmd = ["run", "--rm", "--network", "none", "--hostname", "localhost",
                            "--memory", "512m",
                            "--pids-limit", "256", "--user", "kali", "--entrypoint",
                            "/bin/sh", args.image, "-c", elevated_script]
            if args.platform:
                elevated_cmd[1:1] = ["--platform", args.platform]
            elevated = docker(*elevated_cmd, check=False, timeout=120)
            if elevated.returncode != 0:
                raise SmokeError(
                    "full elevated profile failed nmap / gdb / package install: "
                    f"{elevated.stdout[-1000:]} {elevated.stderr[-1000:]}"
                )
            print("OK full elevated profile: nmap / gdb runnable, package installed")
        result = 0
    except Exception as exc:
        print(f"SMOKE FAILED: {exc}", file=sys.stderr)
        try:
            logs = docker("logs", name, check=False, timeout=20)
            sys.stderr.write((logs.stdout or "") + (logs.stderr or ""))
        except Exception:
            pass
    finally:
        listener.close()
        try:
            docker("rm", "-f", name, check=False, timeout=60)
            inspect = docker("inspect", name, check=False, timeout=20)
            absent = inspect.returncode != 0 and "no such" in inspect.stderr.lower()
            if not absent:
                print("SMOKE FAILED: container cleanup not confirmed", file=sys.stderr)
                result = 1
            elif not args.keep:
                # TeardownRun terminates the supervisor, so docker exec cannot
                # repair the Worker-owned bind mount at this point. Use a short
                # one-shot container from the verified image before host cleanup.
                restore_cmd = ["run", "--rm", "--network", "none", "--user", "root",
                               "--security-opt", "no-new-privileges=true",
                               "--mount", f"type=bind,source={workspace},target=/cleanup/workspace",
                               "--mount", f"type=bind,source={state},target=/cleanup/state",
                               "--entrypoint", "/bin/chown", args.image,
                               "-R", f"{os.getuid()}:{os.getgid()}",
                               "/cleanup/workspace", "/cleanup/state"]
                if args.platform:
                    restore_cmd[1:1] = ["--platform", args.platform]
                ownership = docker(*restore_cmd, check=False, timeout=60)
                if ownership.returncode != 0:
                    raise SmokeError(f"workspace ownership restore failed: {ownership.stderr.strip()}")
                shutil.rmtree(tmp)
        except Exception as exc:
            print(f"SMOKE FAILED: cleanup: {exc}", file=sys.stderr)
            result = 1
    if result == 0:
        print(f"SMOKE PASSED: {args.variant} {args.image}")
    return result


if __name__ == "__main__":
    sys.exit(main())
