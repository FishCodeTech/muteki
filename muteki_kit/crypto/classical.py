"""Classical + XOR helpers — typed, provenance-friendly.

XOR (single-byte/repeating-key with auto key-recovery), Caesar brute, Vigenere.
Recovered plaintext is printed so it lands in real stdout for the provenance gate.
"""

from __future__ import annotations

from typing import Optional

from pydantic import BaseModel

# printable-English scoring for auto key recovery
_FREQ = b"ETAOIN SHRDLU etaoin shrdlu"


class ClassicalResult(BaseModel):
    recovered: bool
    method: str = ""
    key: Optional[str] = None
    plaintext: Optional[str] = None
    notes: str = ""


def _printable_score(b: bytes) -> int:
    return sum(1 for ch in b if 32 <= ch < 127) + sum(2 for ch in b if ch in _FREQ)


def _english_score(b: bytes) -> float:
    weights = {
        " ": 13.0, "e": 12.7, "t": 9.1, "a": 8.2, "o": 7.5, "i": 7.0,
        "n": 6.7, "s": 6.3, "h": 6.1, "r": 6.0, "d": 4.3, "l": 4.0,
        "u": 2.8, "c": 2.8, "m": 2.4, "w": 2.4, "f": 2.2, "g": 2.0,
        "y": 2.0, "p": 1.9, "b": 1.5, "v": 1.0, "k": 0.8,
        "_": 2.0, "{": 2.0, "}": 2.0,
    }
    score = 0.0
    for ch in b:
        c = chr(ch).lower()
        if c in weights:
            score += weights[c]
        elif 32 <= ch < 127:
            score += 0.2
        elif ch in (9, 10, 13):
            score -= 0.2
        else:
            score -= 9.0
    low = b.lower()
    for marker in (b"flag{", b"csawctf{", b"ctf{"):
        if marker in low:
            score += 80.0
    return score / max(1, len(b))


def _hamming(a: bytes, b: bytes) -> int:
    return sum((x ^ y).bit_count() for x, y in zip(a, b))


def xor_bytes(a: bytes, b: bytes) -> bytes:
    """XOR two byte strings (cycles the shorter)."""
    if not b:
        return a
    return bytes(x ^ b[i % len(b)] for i, x in enumerate(a))


def xor_single_byte(data: bytes) -> ClassicalResult:
    """Brute the single-byte XOR key that maximizes printable-English score."""
    best_key, best_score, best_pt = 0, -1, b""
    for k in range(256):
        pt = bytes(c ^ k for c in data)
        s = _printable_score(pt)
        if s > best_score:
            best_key, best_score, best_pt = k, s, pt
    try:
        text = best_pt.decode("utf-8")
    except UnicodeDecodeError:
        text = best_pt.decode("latin-1", errors="replace")
    print(f"[xor:single] key=0x{best_key:02x} -> {best_pt!r}")
    return ClassicalResult(recovered=True, method="xor_single_byte",
                           key=f"0x{best_key:02x}", plaintext=text)


def xor_repeating_key(data: bytes, key: bytes) -> ClassicalResult:
    """XOR with a known repeating key."""
    pt = xor_bytes(data, key)
    try:
        text = pt.decode("utf-8")
    except UnicodeDecodeError:
        text = pt.decode("latin-1", errors="replace")
    print(f"[xor:repeat] key={key!r} -> {pt!r}")
    return ClassicalResult(recovered=True, method="xor_repeating_key",
                           key=key.decode("latin-1", "replace"), plaintext=text)


def xor_repeating_key_crack(
    data: bytes,
    *,
    min_key_len: int = 1,
    max_key_len: int = 40,
    top_key_sizes: int = 8,
) -> ClassicalResult:
    """Recover a repeating-key XOR key with Hamming-distance sizing + scoring.

    This is intentionally a one-call helper: CTF XOR traces often waste many
    turns rediscovering key-size guessing and per-column single-byte XOR.
    """
    if not data:
        return ClassicalResult(recovered=False, method="xor_repeating_key_crack",
                               notes="empty ciphertext")
    size_scores: list[tuple[float, int]] = []
    for key_len in range(max(1, min_key_len), max_key_len + 1):
        chunks = [data[i:i + key_len] for i in range(0, min(len(data), key_len * 8), key_len)]
        chunks = [c for c in chunks if len(c) == key_len]
        if len(chunks) < 2:
            continue
        pairs = zip(chunks, chunks[1:])
        dist = sum(_hamming(a, b) / key_len for a, b in pairs) / (len(chunks) - 1)
        size_scores.append((dist, key_len))
    candidates: list[tuple[float, bytes, bytes]] = []
    forced_sizes = list(range(max(1, min_key_len), min(max_key_len, 8) + 1))
    key_sizes = [k for _, k in sorted(size_scores)[:top_key_sizes]]
    for key_len in dict.fromkeys(key_sizes + forced_sizes):
        key = bytearray()
        for pos in range(key_len):
            column = data[pos::key_len]
            best = max(range(256), key=lambda k: _english_score(bytes(c ^ k for c in column)))
            key.append(best)
        key_b = bytes(key)
        pt = xor_bytes(data, key_b)
        candidates.append((_english_score(pt), key_b, pt))
    if not candidates:
        return ClassicalResult(recovered=False, method="xor_repeating_key_crack",
                               notes="no key sizes could be scored")
    candidates.sort(reverse=True, key=lambda item: item[0])
    for rank, (score, key, pt) in enumerate(candidates[:5], 1):
        preview = pt[:160].decode("latin-1", "replace")
        print(f"[xor:crack:{rank}] score={score:.3f} key_hex={key.hex()} pt={preview!r}")
    score, key, pt = candidates[0]
    text = pt.decode("utf-8", errors="replace")
    return ClassicalResult(recovered=True, method="xor_repeating_key_crack",
                           key=key.hex(), plaintext=text,
                           notes=f"score={score:.3f}; printed top candidates")


def xor_known_plaintext(
    ciphertext: bytes,
    known_plaintext: bytes,
    *,
    target: Optional[bytes] = None,
    offset: int = 0,
    repeat: bool = False,
) -> ClassicalResult:
    """Recover an XOR keystream segment from known plaintext and apply it.

    If `target` is omitted, decrypts `ciphertext`. With `repeat=True`, the
    recovered stream is cycled; otherwise only the known-length segment is used.
    """
    if offset < 0:
        return ClassicalResult(recovered=False, method="xor_known_plaintext",
                               notes="offset must be non-negative")
    segment = ciphertext[offset:offset + len(known_plaintext)]
    if len(segment) != len(known_plaintext):
        return ClassicalResult(recovered=False, method="xor_known_plaintext",
                               notes="known plaintext does not fit ciphertext")
    stream = bytes(c ^ p for c, p in zip(segment, known_plaintext))
    if repeat:
        for period in range(1, len(stream) + 1):
            if all(stream[i] == stream[i % period] for i in range(len(stream))):
                stream = stream[:period]
                break
    src = ciphertext if target is None else target
    if repeat:
        pt = xor_bytes(src, stream)
    else:
        end = min(len(src), len(stream))
        pt = bytes(src[i] ^ stream[i] for i in range(end))
    text = pt.decode("utf-8", errors="replace")
    print(f"[xor:known] stream_hex={stream.hex()} repeat={repeat} -> {pt[:240]!r}")
    return ClassicalResult(recovered=True, method="xor_known_plaintext",
                           key=stream.hex(), plaintext=text)


_FLAG_WORDS = ("flag{", "ctf{", "csawctf{", "flag", "the", "key")


def caesar_bruteforce(text: str, *, flag_markers: tuple = _FLAG_WORDS) -> ClassicalResult:
    """Try all 26 Caesar shifts; surface the one that looks like a flag/English.
    Prints all 26 candidates so the model can also pick by eye."""
    def _shift_char(ch: str, shift: int) -> str:
        if not ch.isalpha():
            return ch
        base = ord("A") if ch.isupper() else ord("a")
        return chr((ord(ch) - base + shift) % 26 + base)

    cands = []
    for shift in range(26):
        out = "".join(_shift_char(ch, shift) for ch in text)
        print(f"[caesar:{shift:02d}] {out}")
        cands.append((shift, out))
    # a shift is a "hit" if it contains a flag-ish word (case-insensitive)
    for shift, out in cands:
        low = out.lower()
        if any(w in low for w in flag_markers):
            return ClassicalResult(recovered=True, method="caesar",
                                   key=str(shift), plaintext=out)
    # fallback: highest printable/English score
    best = max(cands, key=lambda c: _printable_score(c[1].encode("latin-1", "replace")))
    return ClassicalResult(recovered=False, method="caesar", key=str(best[0]),
                           plaintext=best[1],
                           notes="no flag-word match; returning best-English shift")


def vigenere_decrypt(text: str, key: str) -> ClassicalResult:
    """Decrypt Vigenere with a known key (letters only; others pass through)."""
    out = []
    ki = 0
    for ch in text:
        if ch.isalpha():
            base = ord("A") if ch.isupper() else ord("a")
            k = ord(key[ki % len(key)].lower()) - ord("a")
            out.append(chr((ord(ch) - base - k) % 26 + base))
            ki += 1
        else:
            out.append(ch)
    pt = "".join(out)
    print(f"[vigenere] key={key!r} -> {pt}")
    return ClassicalResult(recovered=True, method="vigenere", key=key, plaintext=pt)
