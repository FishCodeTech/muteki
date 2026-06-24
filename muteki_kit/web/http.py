"""AI-friendly HTTP client for web CTF — requests wrapper with session, retry,
flag extraction, and strongly-typed returns (no long-text parsing by the model).

Design (§8): self-documenting function names, Pydantic returns, condensed body +
artifact_id for the full response. The model accesses `r.status`, `r.flag`,
`r.find('regex')` as attributes/methods — never regex-greps a giant string.
"""

from __future__ import annotations

import re
from typing import Any, Optional

import requests
from pydantic import BaseModel, Field

from muteki_kit.result import save_artifact

_FLAG_RE_DEFAULT = r"[A-Za-z0-9_]+\{[^}]{1,200}\}"


class HTTPResponse(BaseModel):
    status: int
    url: str
    headers: dict[str, str] = Field(default_factory=dict)
    cookies: dict[str, str] = Field(default_factory=dict)
    body: str = ""  # possibly truncated for model consumption
    body_len: int = 0
    truncated: bool = False
    artifact_id: Optional[str] = None  # full body, peekable
    flag: Optional[str] = None  # auto-extracted if a flag-shaped token is present

    def find(self, pattern: str, group: int = 0) -> Optional[str]:
        """Regex-search the (full) body. Returns first match or None."""
        m = re.search(pattern, self._full_body, re.DOTALL)
        return m.group(group) if m else None

    def find_all(self, pattern: str) -> list[str]:
        return re.findall(pattern, self._full_body, re.DOTALL)

    # the full body is stashed privately so find() works on untruncated text
    _full_body: str = ""

    def model_post_init(self, __ctx: Any) -> None:
        if not self._full_body:
            object.__setattr__(self, "_full_body", self.body)


class HTTPClient:
    """Persistent session client. Self-documenting verbs the model calls.

        c = HTTPClient("http://target")
        r = c.get("/login")
        if r.flag: print(r.flag)
    """

    def __init__(
        self,
        base_url: str = "",
        *,
        timeout: float = 15.0,
        retries: int = 2,
        flag_pattern: str = _FLAG_RE_DEFAULT,
        body_limit: int = 4000,
        verify: bool = True,
        trust_env: bool = False,
    ) -> None:
        self.base_url = base_url.rstrip("/")
        self.timeout = timeout
        self.retries = retries
        self.flag_pattern = flag_pattern
        self.body_limit = body_limit
        self.session = requests.Session()
        self.session.verify = verify
        self.session.trust_env = trust_env

    def _url(self, path: str) -> str:
        if path.startswith("http://") or path.startswith("https://"):
            return path
        if not path.startswith("/"):
            path = "/" + path
        return self.base_url + path

    def _wrap(self, resp: requests.Response) -> HTTPResponse:
        full = resp.text or ""
        truncated = len(full) > self.body_limit
        aid = save_artifact(full, suffix=".html") if truncated else None
        flag_m = re.search(self.flag_pattern, full)
        out = HTTPResponse(
            status=resp.status_code,
            url=resp.url,
            headers={k: v for k, v in resp.headers.items()},
            cookies=resp.cookies.get_dict(),
            body=full[: self.body_limit],
            body_len=len(full),
            truncated=truncated,
            artifact_id=aid,
            flag=flag_m.group(0) if flag_m else None,
        )
        object.__setattr__(out, "_full_body", full)
        return out

    def request(self, method: str, path: str, **kw: Any) -> HTTPResponse:
        kw.setdefault("timeout", self.timeout)
        kw.setdefault("allow_redirects", True)
        last_exc: Optional[Exception] = None
        for _ in range(self.retries + 1):
            try:
                resp = self.session.request(method, self._url(path), **kw)
                return self._wrap(resp)
            except requests.RequestException as e:  # noqa: PERF203
                last_exc = e
        raise RuntimeError(f"request failed after {self.retries + 1} tries: {last_exc}")

    def get(self, path: str = "/", **kw: Any) -> HTTPResponse:
        return self.request("GET", path, **kw)

    def post(self, path: str = "/", **kw: Any) -> HTTPResponse:
        return self.request("POST", path, **kw)

    def put(self, path: str = "/", **kw: Any) -> HTTPResponse:
        return self.request("PUT", path, **kw)

    def delete(self, path: str = "/", **kw: Any) -> HTTPResponse:
        return self.request("DELETE", path, **kw)

    def set_cookie(self, name: str, value: str, domain: Optional[str] = None) -> None:
        if domain is None:
            self.session.cookies.set(name, value)
        else:
            self.session.cookies.set(name, value, domain=domain)

    def set_header(self, name: str, value: str) -> None:
        self.session.headers[name] = value
