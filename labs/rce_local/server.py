#!/usr/bin/env python3
"""Local net-tools console. Binds 127.0.0.1 only."""

from __future__ import annotations

import json
import subprocess
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, urlparse

HOST = "127.0.0.1"
PORT = 18082

INDEX = b"""<!doctype html>
<html><head><title>Net Tools</title></head>
<body>
<h1>Net Tools</h1>
<p>Hostname lookup for operators on this box.</p>
<form method="get" action="/lookup">
<label>hostname <input name="hostname" value=""></label>
<button type="submit">Lookup</button>
</form>
</body></html>
"""


class LabHandler(BaseHTTPRequestHandler):
    server_version = "nettools/0.1"

    def log_message(self, fmt: str, *args) -> None:
        sys.stderr.write("%s - %s\n" % (self.address_string(), fmt % args))

    def _send(self, code: int, body: bytes, content_type: str) -> None:
        self.send_response(code)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self) -> None:  # noqa: N802
        parsed = urlparse(self.path)
        if parsed.path in {"/", "/index.html"}:
            self._send(200, INDEX, "text/html; charset=utf-8")
            return
        if parsed.path == "/health":
            self._send(200, b'{"ok":true}', "application/json")
            return
        if parsed.path != "/lookup":
            self._send(404, b'{"error":"not found"}', "application/json")
            return
        qs = parse_qs(parsed.query)
        hostname = (qs.get("hostname") or [""])[0]
        if not hostname:
            self._send(400, b'{"error":"hostname required"}', "application/json")
            return
        try:
            proc = subprocess.run(
                f"getent hosts {hostname} || ping -c 1 -W 1 {hostname}",
                shell=True,
                capture_output=True,
                text=True,
                timeout=4,
            )
        except subprocess.TimeoutExpired:
            payload = {"hostname": hostname, "ok": False, "stdout": "", "stderr": "timeout"}
            self._send(504, json.dumps(payload).encode("utf-8"), "application/json")
            return
        payload = {
            "hostname": hostname,
            "ok": proc.returncode == 0,
            "stdout": proc.stdout[-4000:],
            "stderr": proc.stderr[-1000:],
        }
        self._send(200, json.dumps(payload).encode("utf-8"), "application/json")

    def do_POST(self) -> None:  # noqa: N802
        parsed = urlparse(self.path)
        if parsed.path != "/lookup":
            self._send(404, b'{"error":"not found"}', "application/json")
            return
        length = int(self.headers.get("Content-Length") or "0")
        raw = self.rfile.read(length) if length else b""
        hostname = ""
        ctype = (self.headers.get("Content-Type") or "").lower()
        if "json" in ctype:
            try:
                hostname = str(json.loads(raw.decode("utf-8") or "{}").get("hostname") or "")
            except json.JSONDecodeError:
                hostname = ""
        else:
            hostname = (parse_qs(raw.decode("utf-8", "replace")).get("hostname") or [""])[0]
        self.path = "/lookup?hostname=" + hostname
        self.do_GET()


def main() -> None:
    httpd = ThreadingHTTPServer((HOST, PORT), LabHandler)
    sys.stderr.write("rce-lab listening on http://%s:%s\n" % (HOST, PORT))
    httpd.serve_forever()


if __name__ == "__main__":
    main()
