#!/usr/bin/env python3
"""Exercise Rust Web-side handles against a separate real Runner and Docker."""
import os
from pathlib import Path
import signal
import subprocess
import tempfile
import time
import json
import http.server
import socket as sockets
import socketserver
import threading
from verify_runner_ipc import Connection


def fault_proxy(path, upstream):
    """Lose one real successful start reply, after the Runner has executed it."""
    observed = {"dropped_start_reply": False, "start_requests": {}}

    class Handler(http.server.BaseHTTPRequestHandler):
        def log_message(self, *_):
            pass

        def forward(self):
            body = self.rfile.read(int(self.headers.get("Content-Length", "0")))
            connection = Connection(upstream)
            try:
                connection.request(self.command, self.path, body=body or None,
                                   headers={"Content-Type": "application/json"})
                response = connection.getresponse()
                result = response.read()
                if self.path == "/v1/workers/start":
                    command = json.loads(body)["command_id"]
                    counts = observed["start_requests"]
                    counts[command] = counts.get(command, 0) + 1
                    if response.status == 200 and not observed["dropped_start_reply"]:
                        observed["dropped_start_reply"] = True
                        self.close_connection = True
                        self.connection.shutdown(sockets.SHUT_RDWR)
                        return
                self.send_response(response.status)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(result)))
                self.end_headers()
                self.wfile.write(result)
            finally:
                connection.close()

        do_GET = forward
        do_POST = forward

    server = socketserver.UnixStreamServer(str(path), Handler)
    path.chmod(0o600)
    thread = threading.Thread(target=server.serve_forever, kwargs={"poll_interval": 0.05}, daemon=True)
    thread.start()
    return server, thread, observed


def main():
    binary = str(Path(".cache/target/debug/muteki-server").resolve())
    with tempfile.TemporaryDirectory(prefix="runner-web-", dir="/tmp") as directory:
        root = Path(directory).resolve()
        socket = root / "runner/control.sock"
        environment = dict(os.environ, MUTEKI_TEST_RUNNER_STATE_DIR=str(root),
                           MUTEKI_RUNNER_SOCKET=str(socket), RUSTC_WRAPPER="")
        with tempfile.TemporaryFile() as log:
            runner = subprocess.Popen([binary, "runner", "--state-dir", str(root)],
                                      stdout=log, stderr=log, env=environment)
            proxy = None
            try:
                deadline = time.monotonic() + 15
                while not socket.exists():
                    if runner.poll() is not None or time.monotonic() > deadline:
                        log.seek(0)
                        raise RuntimeError(log.read().decode())
                    time.sleep(0.05)
                proxy_path = root / "runner/fault-proxy.sock"
                proxy, thread, observed = fault_proxy(proxy_path, socket)
                environment["MUTEKI_RUNNER_SOCKET"] = str(proxy_path)
                subprocess.run(["cargo", "test", "-p", "muteki-server", "--lib",
                    "web_handle_executes_and_streams_through_runner", "--offline", "--", "--ignored"],
                    env=environment, check=True)
                assert observed["dropped_start_reply"]
                assert all(count == 1 for count in observed["start_requests"].values()), observed
                print(json.dumps({"real_start_reply_lost": True, "no_start_replay": True,
                                  "start_request_count": len(observed["start_requests"])}))
            finally:
                if proxy is not None:
                    proxy.shutdown()
                    proxy.server_close()
                    thread.join(timeout=5)
                if runner.poll() is None:
                    runner.send_signal(signal.SIGINT)
                    try:
                        runner.wait(timeout=10)
                    except subprocess.TimeoutExpired:
                        runner.kill()
                        runner.wait()
                names = subprocess.check_output(["docker", "ps", "-a", "--filter",
                    f"label=muteki.state_dir={root}", "--format", "{{.Names}}"], text=True).splitlines()
                if names:
                    subprocess.run(["docker", "rm", "-f", *names], check=True)


if __name__ == "__main__":
    main()
