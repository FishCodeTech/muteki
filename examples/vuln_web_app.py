"""A tiny intentionally-vulnerable web app used as a local CTF target.

Pure stdlib http.server (no Flask dependency). Hosts a few classic web-CTF
puzzles so we can validate the agent end-to-end without a remote platform:

  GET  /                     landing page with hints
  GET  /robots.txt           leaks a hidden path (/s3cr3t_admin)
  GET  /s3cr3t_admin         requires ?token=<base64('open sesame')>; gives a JWT puzzle
  GET  /encoded              a triple-base64 wrapped flag fragment
  POST /login {user,pass}    SQLi-ish: user=admin' OR '1'='1 bypasses -> sets a JWT cookie
  GET  /admin                requires a JWT (HS256 secret "Sn1f") with {"admin":true} -> FLAG

The full flag is assembled by solving the JWT step. Designed to be solvable with
the muteki_kit web tools (http, fuzz, encoding, jwt).
"""

from __future__ import annotations

import base64
import json
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, urlparse

from muteki_kit.web import jwt as kjwt

FLAG = "flag{w3b_t00ls_ar3_w0rk1ng}"
JWT_SECRET = "Sn1f"
ADMIN_TOKEN_PARAM = base64.b64encode(b"open sesame").decode()  # for /s3cr3t_admin

# triple-base64 of a decoy fragment (exercises auto_decode)
_frag = "the secret knock is: open sesame"
for _ in range(3):
    _frag = base64.b64encode(_frag.encode()).decode()
ENCODED_BLOB = _frag


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *args):  # silence
        pass

    def _send(self, code: int, body: str, ctype="text/html", cookies=None):
        data = body.encode()
        self.send_response(code)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(data)))
        if cookies:
            for k, v in cookies.items():
                self.send_header("Set-Cookie", f"{k}={v}; Path=/")
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self):
        u = urlparse(self.path)
        path = u.path
        qs = parse_qs(u.query)

        if path == "/":
            self._send(200, "<h1>Baby Web</h1><p>Nothing to see. Or is there? Check the usual files.</p>")
        elif path == "/robots.txt":
            self._send(200, "User-agent: *\nDisallow: /s3cr3t_admin\n", ctype="text/plain")
        elif path == "/encoded":
            self._send(200, f"<pre>{ENCODED_BLOB}</pre>")
        elif path == "/s3cr3t_admin":
            token = (qs.get("token") or [""])[0]
            if token == ADMIN_TOKEN_PARAM:
                self._send(
                    200,
                    "<p>Welcome. Login at /login (admin auth is weak), "
                    "then visit /admin with a forged JWT. The signing secret is short.</p>",
                )
            else:
                self._send(403, "<p>Forbidden. Need ?token=base64('open sesame').</p>")
        elif path == "/admin":
            # require a JWT cookie with admin:true signed by JWT_SECRET
            cookie = self.headers.get("Cookie", "")
            tok = ""
            for part in cookie.split(";"):
                part = part.strip()
                if part.startswith("auth="):
                    tok = part[len("auth=") :]
            if tok:
                # verify signature with the secret
                ver = kjwt.brute_hs256(tok, [JWT_SECRET])
                d = kjwt.decode(tok)
                if ver.found and d.payload.get("admin") is True:
                    self._send(200, f"<h1>Admin</h1><p>{FLAG}</p>")
                    return
            self._send(401, "<p>Need a valid admin JWT cookie 'auth'. Secret is weak (in COMMON_SECRETS).</p>")
        else:
            self._send(404, "<p>Not found</p>")

    def do_POST(self):
        u = urlparse(self.path)
        length = int(self.headers.get("Content-Length", 0))
        raw = self.rfile.read(length).decode() if length else ""
        # accept form or json
        params = parse_qs(raw)
        user = (params.get("user") or [""])[0]
        password = (params.get("pass") or [""])[0]
        if not user:
            try:
                j = json.loads(raw)
                user = j.get("user", "")
                password = j.get("pass", "")
            except (ValueError, json.JSONDecodeError):
                pass

        if u.path == "/login":
            # naive SQLi: classic ' OR '1'='1 bypass
            if "'" in user and "or" in user.lower() and "1" in user:
                guest = kjwt.sign_hs256({"user": "guest", "admin": False}, JWT_SECRET)
                self._send(
                    200,
                    "<p>Login bypassed. Here is your (non-admin) token. "
                    "Forge admin:true to reach /admin.</p>",
                    cookies={"auth": guest},
                )
            else:
                self._send(401, "<p>Bad credentials. Try an injection.</p>")
        else:
            self._send(404, "<p>Not found</p>")


def serve(host: str = "127.0.0.1", port: int = 0) -> tuple[ThreadingHTTPServer, str]:
    """Start the app in a background thread. Returns (server, base_url)."""
    srv = ThreadingHTTPServer((host, port), Handler)
    actual_port = srv.server_address[1]
    t = threading.Thread(target=srv.serve_forever, daemon=True)
    t.start()
    return srv, f"http://{host}:{actual_port}"


if __name__ == "__main__":
    srv, url = serve(port=8099)
    print(f"vuln web app serving at {url}  (flag is {FLAG})")
    try:
        srv.serve_forever()
    except KeyboardInterrupt:
        srv.shutdown()
