#!/usr/bin/env python3
"""Local authorization lab for finding_ok (127.0.0.1 only).

Two identities (alice, bob). GET /notes/1 without a session returns 401;
any valid session returns 200 for the same resource.
"""

from __future__ import annotations

import json
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, urlparse

HOST = "127.0.0.1"
PORT = 18081

USERS = {"alice": "alicepass", "bob": "bobpass"}
NOTES = {
    "1": {"id": 1, "owner": "alice", "body": "alice-note-1"},
    "2": {"id": 2, "owner": "bob", "body": "bob-note-2"},
}


def _session(handler: BaseHTTPRequestHandler) -> str:
    raw = handler.headers.get("Cookie") or ""
    for part in raw.split(";"):
        k, _, v = part.strip().partition("=")
        if k == "sess":
            return v.strip()
    return ""


class LabHandler(BaseHTTPRequestHandler):
    server_version = "idor-lab/1.0"

    def log_message(self, fmt: str, *args) -> None:
        sys.stderr.write("%s - %s\n" % (self.address_string(), fmt % args))

    def _send(self, code: int, body: bytes, content_type: str = "application/json") -> None:
        self.send_response(code)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def _json(self, code: int, payload: dict) -> None:
        self._send(code, json.dumps(payload).encode("utf-8"))

    def do_GET(self) -> None:  # noqa: N802
        parsed = urlparse(self.path)
        if parsed.path in {"/", "/health"}:
            self._json(200, {"ok": True, "lab": "idor_local"})
            return
        if parsed.path.startswith("/notes/"):
            nid = parsed.path.rsplit("/", 1)[-1]
            note = NOTES.get(nid)
            if note is None:
                self._json(404, {"error": "not found"})
                return
            user = _session(self)
            if not user or user not in USERS:
                self._send(401, b"401 Unauthorized")
                return
            self._json(200, note)
            return
        self._json(404, {"error": "not found"})

    def do_POST(self) -> None:  # noqa: N802
        parsed = urlparse(self.path)
        if parsed.path != "/login":
            self._json(404, {"error": "not found"})
            return
        length = int(self.headers.get("Content-Length") or 0)
        raw = self.rfile.read(length) if length else b""
        ctype = (self.headers.get("Content-Type") or "").lower()
        user = password = ""
        if "application/json" in ctype:
            try:
                data = json.loads(raw.decode("utf-8") or "{}")
            except json.JSONDecodeError:
                data = {}
            user = str(data.get("user") or "")
            password = str(data.get("password") or "")
        else:
            form = parse_qs(raw.decode("utf-8"), keep_blank_values=True)
            user = (form.get("user") or [""])[0]
            password = (form.get("password") or [""])[0]
        if USERS.get(user) != password:
            self._json(401, {"error": "bad credentials"})
            return
        body = json.dumps({"ok": True, "user": user}).encode("utf-8")
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Set-Cookie", f"sess={user}; Path=/")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


def main() -> None:
    httpd = ThreadingHTTPServer((HOST, PORT), LabHandler)
    print(f"idor_local listening on http://{HOST}:{PORT}", flush=True)
    httpd.serve_forever()


if __name__ == "__main__":
    main()
