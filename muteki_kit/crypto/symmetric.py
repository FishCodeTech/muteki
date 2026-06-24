"""Symmetric-cipher helpers: ECB detection + padding-oracle decryption.

padding_oracle_decrypt runs the WHOLE CBC padding-oracle attack in one call (the
model can't hand-loop the byte-by-byte requests across its turn budget) — give it
an oracle(ciphertext_bytes)->bool that returns True on valid padding, plus the IV
and ciphertext, and it recovers the plaintext, printing progress.
"""

from __future__ import annotations

import binascii
from typing import Callable, Optional

from pydantic import BaseModel


class SymResult(BaseModel):
    recovered: bool
    method: str = ""
    key: Optional[str] = None
    plaintext: Optional[bytes] = None
    notes: str = ""


def detect_ecb(ciphertext: bytes, block_size: int = 16) -> SymResult:
    """ECB leaks repeated plaintext blocks as repeated ciphertext blocks."""
    blocks = [ciphertext[i:i + block_size] for i in range(0, len(ciphertext), block_size)]
    dupes = len(blocks) - len(set(blocks))
    is_ecb = dupes > 0
    print(f"[ecb] {len(blocks)} blocks, {dupes} repeated -> {'ECB likely' if is_ecb else 'not ECB'}")
    return SymResult(recovered=is_ecb, method="detect_ecb",
                     notes=f"{dupes} repeated blocks of {block_size} bytes")


def padding_oracle_decrypt(
    oracle: Callable[[bytes], bool],
    ciphertext: bytes,
    iv: bytes,
    *,
    block_size: int = 16,
) -> SymResult:
    """CBC padding-oracle attack in one call.

    oracle(iv_plus_ct or ct) -> True iff padding is valid. We pass it
    `prefix_block + target_block` (two blocks) so it decrypts one block at a time.
    Recovers the full plaintext. Prints each recovered block.
    """
    full = iv + ciphertext
    blocks = [full[i:i + block_size] for i in range(0, len(full), block_size)]
    recovered = bytearray()

    for bi in range(1, len(blocks)):
        prev, cur = blocks[bi - 1], blocks[bi]
        inter = bytearray(block_size)  # intermediate state D(cur)
        plain = bytearray(block_size)
        for pad in range(1, block_size + 1):
            pos = block_size - pad
            forged = bytearray(block_size)
            for k in range(pos + 1, block_size):
                forged[k] = inter[k] ^ pad
            found = False
            for guess in range(256):
                forged[pos] = guess
                if oracle(bytes(forged) + cur):
                    # guard against false positive on the last byte (pad=1)
                    if pad == 1:
                        forged[pos - 1] ^= 1
                        if not oracle(bytes(forged) + cur):
                            continue
                    inter[pos] = guess ^ pad
                    plain[pos] = inter[pos] ^ prev[pos]
                    found = True
                    break
            if not found:
                return SymResult(recovered=False, method="padding_oracle",
                                 notes=f"stuck at block {bi} byte {pos}; "
                                       f"recovered so far: {bytes(recovered)!r}")
        recovered.extend(plain)
        print(f"[padding_oracle] block {bi}: {bytes(plain)!r}")

    # strip PKCS#7 padding
    if recovered:
        pad = recovered[-1]
        if 0 < pad <= block_size:
            recovered = recovered[:-pad]
    print(f"[padding_oracle] plaintext: {bytes(recovered)!r}")
    return SymResult(recovered=True, method="padding_oracle", plaintext=bytes(recovered))


def decode_offset_nibbles(
    data: bytes | str,
    offset: int,
    *,
    width: Optional[int] = None,
    double_unhex: bool = False,
) -> SymResult:
    """Decode CTF encodings where each hex nibble is stored as offset+nibble.

    Some DES/weak-key tasks hide ciphertext bytes by decimal-encoding each
    hex nibble as a fixed-width integer. `double_unhex=True` handles the common
    variant where the decoded bytes are ASCII hex of the real ciphertext.
    """
    text = data.decode() if isinstance(data, bytes) else data
    text = "".join(text.split())
    if width is None:
        width = len(str(offset)) + (1 if str(offset).startswith("9") else 0)
    if width <= 0 or len(text) % width:
        return SymResult(recovered=False, method="decode_offset_nibbles",
                         notes=f"input length {len(text)} not divisible by width {width}")
    nibbles = []
    rev = {**{i: str(i) for i in range(10)}, **{i + 11: "abcdef"[i] for i in range(6)}}
    for i in range(0, len(text), width):
        val = int(text[i:i + width]) - offset
        if val not in rev:
            return SymResult(recovered=False, method="decode_offset_nibbles",
                             notes=f"bad nibble value {val} at chunk {i // width}")
        nibbles.append(rev[val])
    try:
        out = binascii.unhexlify("".join(nibbles))
        if double_unhex:
            out = binascii.unhexlify(out)
    except (binascii.Error, ValueError) as exc:
        return SymResult(recovered=False, method="decode_offset_nibbles",
                         notes=f"unhexlify failed: {exc}")
    print(f"[offset-nibbles] offset={offset} width={width} double={double_unhex} -> {out[:240]!r}")
    return SymResult(recovered=True, method="decode_offset_nibbles", plaintext=out)


DES_WEAK_AND_SEMI_WEAK_KEYS = tuple(bytes.fromhex(k) for k in (
    "0101010101010101", "FEFEFEFEFEFEFEFE",
    "E0E0E0E0F1F1F1F1", "1F1F1F1F0E0E0E0E",
    "011F011F010E010E", "1F011F010E010E01",
    "01E001E001F101F1", "E001E001F101F101",
    "01FE01FE01FE01FE", "FE01FE01FE01FE01",
    "1FE01FE00EF10EF1", "E01FE01FF10EF10E",
    "1FFE1FFE0EFE0EFE", "FE1FFE1FFE0EFE0E",
    "E0FEE0FEF1FEF1FE", "FEE0FEE0FEF1FEF1",
))


def des_weak_ofb_bruteforce(
    ciphertext: bytes,
    *,
    iv: bytes = b"13371337",
    known_plaintext: Optional[bytes] = None,
    known_ciphertext: Optional[bytes] = None,
    keys: tuple[bytes, ...] = DES_WEAK_AND_SEMI_WEAK_KEYS,
) -> SymResult:
    """Try one- and two-layer DES-OFB with known weak/semi-weak DES keys."""
    try:
        from Crypto.Cipher import DES
    except Exception as exc:  # pragma: no cover - dependency is installed in CI env
        return SymResult(recovered=False, method="des_weak_ofb_bruteforce",
                         notes=f"Crypto.Cipher.DES unavailable: {exc}")

    def decrypt_pair(ct: bytes, k1: bytes, k2: Optional[bytes]) -> bytes:
        if k2 is None:
            return DES.new(k1, DES.MODE_OFB, iv=iv).decrypt(ct)
        mid = DES.new(k2, DES.MODE_OFB, iv=iv).decrypt(ct)
        return DES.new(k1, DES.MODE_OFB, iv=iv).decrypt(mid)

    candidates: list[tuple[str, bytes]] = []
    if known_plaintext is not None and known_ciphertext is not None:
        kp = known_plaintext
        if len(kp) % 8:
            kp = kp + (b"_" * (8 - (len(kp) % 8)))
        for k1 in keys:
            if decrypt_pair(known_ciphertext, k1, None) == kp:
                candidates.append((k1.hex(), decrypt_pair(ciphertext, k1, None)))
            for k2 in keys:
                if decrypt_pair(known_ciphertext, k1, k2) == kp:
                    candidates.append((f"{k1.hex()}:{k2.hex()}",
                                       decrypt_pair(ciphertext, k1, k2)))
    else:
        for k1 in keys:
            candidates.append((k1.hex(), decrypt_pair(ciphertext, k1, None)))
            for k2 in keys:
                candidates.append((f"{k1.hex()}:{k2.hex()}", decrypt_pair(ciphertext, k1, k2)))

    def score(pt: bytes) -> float:
        printable = sum(1 for b in pt if 32 <= b < 127 or b in (9, 10, 13)) / max(1, len(pt))
        bonus = 2.0 if b"flag{" in pt.lower() or b"csawctf{" in pt.lower() else 0.0
        return printable + bonus

    if not candidates:
        return SymResult(recovered=False, method="des_weak_ofb_bruteforce",
                         notes="no weak/semi-weak key matched the known plaintext")
    best_key, best_pt = max(candidates, key=lambda item: score(item[1]))
    for key, pt in sorted(candidates, key=lambda item: score(item[1]), reverse=True)[:5]:
        print(f"[des:weak-ofb] key={key} score={score(pt):.3f} pt={pt[:200]!r}")
    return SymResult(recovered=True, method="des_weak_ofb_bruteforce",
                     key=best_key, plaintext=best_pt)
