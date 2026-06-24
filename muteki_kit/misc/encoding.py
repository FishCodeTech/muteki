"""Auto multi-layer encoding detection/decoding (CyberChef-style) — §8 misc.

The single most common easy-challenge primitive: a flag wrapped in N layers of
base64/hex/url/rot13/etc. `auto_decode` does a bounded best-first search peeling
layers until a flag-shaped token (or printable plaintext) appears.
"""

from __future__ import annotations

import base64
import binascii
import codecs
import re
import urllib.parse
from typing import Callable, Optional

from pydantic import BaseModel, Field

_FLAG_RE = re.compile(r"[A-Za-z0-9_]+\{[^}]{1,200}\}")


class DecodeStep(BaseModel):
    codec: str
    preview: str


class DecodeResult(BaseModel):
    success: bool
    flag: Optional[str] = None
    plaintext: Optional[str] = None
    chain: list[DecodeStep] = Field(default_factory=list)  # codecs applied, in order
    detail: str = ""


# --- individual reversible decoders. Each raises on "not this codec". --------

def _b64(s: str) -> str:
    t = s.strip()
    # tolerate urlsafe + missing padding
    t = t.replace("-", "+").replace("_", "/")
    pad = (-len(t)) % 4
    raw = base64.b64decode(t + "=" * pad, validate=False)
    out = raw.decode("utf-8", errors="strict")
    if not out:
        raise ValueError("empty")
    return out


def _b32(s: str) -> str:
    t = s.strip().upper()
    pad = (-len(t)) % 8
    return base64.b32decode(t + "=" * pad).decode("utf-8", "strict")


def _hex(s: str) -> str:
    t = re.sub(r"\s+", "", s.strip())
    if len(t) % 2 or not re.fullmatch(r"[0-9a-fA-F]+", t):
        raise ValueError("not hex")
    return binascii.unhexlify(t).decode("utf-8", "strict")


def _url(s: str) -> str:
    out = urllib.parse.unquote(s)
    if out == s:
        raise ValueError("no url-encoding")
    return out


def _rot13(s: str) -> str:
    return codecs.decode(s, "rot_13")


def _ascii85(s: str) -> str:
    return base64.a85decode(s.strip()).decode("utf-8", "strict")


def _b85(s: str) -> str:
    return base64.b85decode(s.strip()).decode("utf-8", "strict")


def _binary(s: str) -> str:
    bits = re.sub(r"\s+", "", s.strip())
    if not re.fullmatch(r"[01]+", bits) or len(bits) % 8:
        raise ValueError("not binary")
    return "".join(chr(int(bits[i : i + 8], 2)) for i in range(0, len(bits), 8))


def _decimal(s: str) -> str:
    parts = s.strip().split()
    if not parts or not all(p.isdigit() for p in parts):
        raise ValueError("not decimal codepoints")
    vals = [int(p) for p in parts]
    if any(v > 0x10FFFF for v in vals):
        raise ValueError("out of range")
    return "".join(chr(v) for v in vals)


_DECODERS: dict[str, Callable[[str], str]] = {
    "base64": _b64,
    "base32": _b32,
    "hex": _hex,
    "url": _url,
    "rot13": _rot13,
    "ascii85": _ascii85,
    "base85": _b85,
    "binary": _binary,
    "decimal": _decimal,
}


def _printable_ratio(s: str) -> float:
    if not s:
        return 0.0
    printable = sum(1 for ch in s if 32 <= ord(ch) < 127 or ch in "\t\n\r")
    return printable / len(s)


def find_flag(text: str) -> Optional[str]:
    m = _FLAG_RE.search(text)
    return m.group(0) if m else None


def auto_decode(
    data: str,
    *,
    max_depth: int = 8,
    flag_only: bool = False,
) -> DecodeResult:
    """Best-first peel of encoding layers until a flag (or clean plaintext).

    Returns the decode chain so the result is explainable / reproducible.
    """
    # BFS over (current_text, chain). Avoid revisiting identical states.
    from collections import deque

    seen: set[str] = set()
    start = data.strip()
    q: deque[tuple[str, list[DecodeStep]]] = deque([(start, [])])

    # check the input itself first
    f0 = find_flag(start)
    if f0:
        return DecodeResult(success=True, flag=f0, plaintext=start, chain=[], detail="flag in input")

    best_plain: Optional[tuple[str, list[DecodeStep], float]] = None

    while q:
        text, chain = q.popleft()
        if len(chain) >= max_depth:
            continue
        for name, fn in _DECODERS.items():
            try:
                decoded = fn(text)
            except Exception:
                continue
            if not decoded or decoded == text or decoded in seen:
                continue
            seen.add(decoded)
            step = DecodeStep(codec=name, preview=decoded[:80])
            new_chain = chain + [step]
            flag = find_flag(decoded)
            if flag:
                return DecodeResult(
                    success=True,
                    flag=flag,
                    plaintext=decoded,
                    chain=new_chain,
                    detail=f"flag after {' -> '.join(s.codec for s in new_chain)}",
                )
            ratio = _printable_ratio(decoded)
            if ratio > 0.9 and (best_plain is None or len(new_chain) < len(best_plain[1])):
                best_plain = (decoded, new_chain, ratio)
            q.append((decoded, new_chain))

    if not flag_only and best_plain is not None:
        return DecodeResult(
            success=False,
            plaintext=best_plain[0],
            chain=best_plain[1],
            detail="no flag found; returning most-printable decode",
        )
    return DecodeResult(success=False, detail="no decode produced a flag or clean plaintext")
