"""Neutral challenge-target address parsing.

Resolve a credential-free host/port endpoint from a challenge target string.
This module performs pure string parsing only: no network I/O, no health
classification, no probing.
"""

from __future__ import annotations

from dataclasses import dataclass
from typing import Optional
from urllib.parse import urlsplit


_DEFAULT_PORTS = {
    "ftp": 21,
    "http": 80,
    "https": 443,
    "ssh": 22,
    "ws": 80,
    "wss": 443,
}


@dataclass(frozen=True, slots=True)
class TargetEndpoint:
    target: str
    host: str
    port: int


def target_endpoint(raw_target: str) -> Optional[TargetEndpoint]:
    """Resolve a credential-free TCP endpoint from a challenge target."""
    raw = str(raw_target or "").strip()
    if not raw:
        return None
    try:
        parsed = urlsplit(raw if "://" in raw else f"//{raw}")
        host = str(parsed.hostname or "").strip().lower()
        if not host:
            return None
        try:
            port = parsed.port
        except ValueError:
            return None
        scheme = str(parsed.scheme or "").strip().lower()
        if port is None:
            port = _DEFAULT_PORTS.get(scheme)
        if port is None or not (1 <= int(port) <= 65535):
            return None
        shown_host = f"[{host}]" if ":" in host else host
        authority = f"{shown_host}:{int(port)}"
        label = f"{scheme}://{authority}" if scheme else authority
        return TargetEndpoint(label, host, int(port))
    except ValueError:
        return None
