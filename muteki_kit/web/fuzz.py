"""Directory & parameter fuzzing for web CTF — §8 web.fuzz.

A lightweight ffuf-style fuzzer built on HTTPClient (no external binary). Good
enough to find hidden endpoints / params in challenge apps. Returns typed hits.
"""

from __future__ import annotations

from concurrent.futures import ThreadPoolExecutor
from typing import Optional

from pydantic import BaseModel, Field

from muteki_kit.web.http import HTTPClient

COMMON_PATHS = [
    "admin", "login", "robots.txt", "flag", "flag.txt", "secret", "secret.txt",
    "backup", "backup.zip", ".git/config", ".git/HEAD", "config", "config.php",
    "api", "api/flag", "debug", "test", "uploads", "static", "hidden", "dev",
    "index.php", "index.html", "phpinfo.php", "server-status", ".env", "db",
    "console", "shell", "cmd", "source", "src", "www.zip", "site.zip",
]


class FuzzHit(BaseModel):
    path: str
    status: int
    length: int
    flag: Optional[str] = None


class FuzzResult(BaseModel):
    hits: list[FuzzHit] = Field(default_factory=list)
    tried: int = 0
    flag: Optional[str] = None

    def interesting(self) -> list[FuzzHit]:
        """Hits that aren't 404 — the leads worth following."""
        return [h for h in self.hits if h.status not in (404,)]


def fuzz_paths(
    base_url: str,
    wordlist: Optional[list[str]] = None,
    *,
    extensions: Optional[list[str]] = None,
    concurrency: int = 12,
    ignore_status: tuple[int, ...] = (404,),
) -> FuzzResult:
    words = list(wordlist or COMMON_PATHS)
    if extensions:
        words = words + [f"{w}{ext}" for w in words for ext in extensions]

    client = HTTPClient(base_url, retries=0, timeout=8.0)

    def probe(path: str) -> Optional[FuzzHit]:
        try:
            r = client.get("/" + path.lstrip("/"))
        except RuntimeError:
            return None
        if r.status in ignore_status:
            return None
        return FuzzHit(path=path, status=r.status, length=r.body_len, flag=r.flag)

    hits: list[FuzzHit] = []
    found_flag: Optional[str] = None
    with ThreadPoolExecutor(max_workers=concurrency) as ex:
        for hit in ex.map(probe, words):
            if hit is not None:
                hits.append(hit)
                if hit.flag and not found_flag:
                    found_flag = hit.flag
    return FuzzResult(hits=hits, tried=len(words), flag=found_flag)
