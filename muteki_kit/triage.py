"""Auto triage — track classification, file/strings/entropy, attachment unpack (§8).

Cheap first-pass recon: classify a challenge into a track from its metadata and
any attachments, and surface obvious leads (strings, file type, entropy).
"""

from __future__ import annotations

import math
import re
from pathlib import Path
from typing import Optional

from pydantic import BaseModel, Field

_CATEGORY_HINTS = {
    "web": ["http", "url", "cookie", "login", "xss", "sql", "jwt", "flask", "php", "api", "website", "request"],
    "pwn": ["overflow", "binary", "elf", "ret2", "rop", "shellcode", "nc ", "stack", "heap", "libc", "exploit"],
    "reverse": ["reverse", "decompile", "disassemble", "binary", "crackme", "obfuscat", "license", "keygen"],
    "crypto": ["rsa", "aes", "encrypt", "cipher", "key", "xor", "ecc", "lattice", "prime", "modulus", "decrypt"],
    "forensics": ["pcap", "memory", "dump", "disk", "stego", "image", "recover", "carve", "wireshark", "volatility"],
    "misc": ["encode", "decode", "base64", "qr", "audio", "esolang", "jail", "brainfuck"],
}


class TriageResult(BaseModel):
    category_guess: str
    scores: dict[str, int] = Field(default_factory=dict)
    file_types: dict[str, str] = Field(default_factory=dict)  # path -> magic-ish desc
    notable_strings: list[str] = Field(default_factory=list)
    entropy: dict[str, float] = Field(default_factory=dict)
    detail: str = ""


def shannon_entropy(data: bytes) -> float:
    if not data:
        return 0.0
    freq = {b: data.count(b) for b in set(data)}
    n = len(data)
    return -sum((c / n) * math.log2(c / n) for c in freq.values())


def _sniff_type(path: Path) -> str:
    try:
        head = path.read_bytes()[:16]
    except OSError:
        return "unreadable"
    sigs = {
        b"\x7fELF": "ELF binary",
        b"PK\x03\x04": "ZIP archive",
        b"\x89PNG": "PNG image",
        b"\xff\xd8\xff": "JPEG image",
        b"GIF8": "GIF image",
        b"%PDF": "PDF document",
        b"\x1f\x8b": "gzip",
        b"BZh": "bzip2",
        b"Rar!": "RAR archive",
    }
    for sig, desc in sigs.items():
        if head.startswith(sig):
            return desc
    if head.startswith(b"MZ"):
        return "PE/DOS executable"
    # text?
    try:
        head.decode("utf-8")
        return "text/ascii"
    except UnicodeDecodeError:
        return "binary/unknown"


def _grep_strings(path: Path, min_len: int = 5, limit: int = 40) -> list[str]:
    try:
        raw = path.read_bytes()
    except OSError:
        return []
    found = re.findall(rb"[\x20-\x7e]{%d,}" % min_len, raw)
    out: list[str] = []
    for f in found:
        s = f.decode("ascii", "replace")
        # prioritize flag-shaped / url / interesting tokens
        out.append(s)
        if len(out) >= limit:
            break
    # bubble up anything flag-shaped
    flagged = [s for s in out if re.search(r"\{.*\}|flag|http|password|key", s, re.I)]
    rest = [s for s in out if s not in flagged]
    return (flagged + rest)[:limit]


def classify(
    name: str = "",
    description: str = "",
    attachments: Optional[list[str]] = None,
    category_hint: Optional[str] = None,
) -> TriageResult:
    text = f"{name} {description}".lower()
    scores = {cat: 0 for cat in _CATEGORY_HINTS}
    for cat, hints in _CATEGORY_HINTS.items():
        for h in hints:
            if h in text:
                scores[cat] += 1

    file_types: dict[str, str] = {}
    entropy: dict[str, float] = {}
    notable: list[str] = []
    for ap in attachments or []:
        p = Path(ap)
        if not p.exists():
            continue
        ft = _sniff_type(p)
        file_types[ap] = ft
        # bias category by file type
        if "ELF" in ft or "PE" in ft:
            scores["pwn"] += 1
            scores["reverse"] += 1
        elif "image" in ft or "PDF" in ft or "ZIP" in ft:
            scores["forensics"] += 1
        try:
            data = p.read_bytes()[:65536]
            entropy[ap] = round(shannon_entropy(data), 3)
        except OSError:
            pass
        notable.extend(_grep_strings(p))

    if category_hint and category_hint in scores:
        scores[category_hint] += 5

    guess = max(scores, key=lambda k: scores[k]) if any(scores.values()) else (category_hint or "misc")
    return TriageResult(
        category_guess=guess,
        scores=scores,
        file_types=file_types,
        entropy=entropy,
        notable_strings=notable[:40],
        detail=f"classified as {guess}",
    )
