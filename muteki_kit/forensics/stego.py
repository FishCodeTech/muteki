"""Steganography helpers — LSB extraction + tool wrappers + a kitchen-sink scan.

Forensics is the most heterogeneous track, so the strategy is high-recall first:
`scan(path)` runs the cheap automatic checks (LSB on all channels/bit-planes,
strings, appended-data, exif) and prints anything flag-shaped. Recovered data is
PRINTED so it lands in real stdout for the provenance gate.

Pure-pip (Pillow/numpy) for the bespoke LSB work; zsteg/steghide/stegseek are
subprocessed IF installed (degrade gracefully with a note otherwise).
"""

from __future__ import annotations

import re
import shutil
import subprocess
import tempfile
from typing import Optional

import numpy as np
from PIL import Image
from pydantic import BaseModel

_FLAG_RE = re.compile(rb"[A-Za-z0-9_]{1,15}\{[^}]{1,200}\}")


class StegoResult(BaseModel):
    found: bool
    method: str = ""
    data: Optional[bytes] = None
    flag: Optional[str] = None
    notes: str = ""


def _scan_flag(data: bytes, method: str) -> Optional[StegoResult]:
    m = _FLAG_RE.search(data)
    if m:
        flag = m.group(0).decode("latin-1", "replace")
        print(f"[stego:{method}] FLAG-shaped: {flag}")
        return StegoResult(found=True, method=method, data=m.group(0), flag=flag)
    return None


def lsb_extract(path: str, *, bits: int = 1, channels: str = "RGB",
                order: str = "row") -> StegoResult:
    """Extract LSB-embedded bytes from an image. Tries the common layout: take
    the `bits` low bits of each channel in `channels`, row-major, MSB-first."""
    img = Image.open(path).convert("RGB")
    arr = np.array(img)
    chan_idx = {"R": 0, "G": 1, "B": 2}
    bitstream = []
    h, w, _ = arr.shape
    for y in range(h):
        for x in range(w):
            for ch in channels:
                val = int(arr[y, x, chan_idx[ch]])
                for b in range(bits):
                    bitstream.append((val >> b) & 1)
    # pack MSB-first into bytes
    nbytes = len(bitstream) // 8
    packed = bytearray()
    for i in range(nbytes):
        byte = 0
        for j in range(8):
            byte = (byte << 1) | bitstream[i * 8 + j]
        packed.append(byte)
    data = bytes(packed)
    hit = _scan_flag(data, f"lsb-{channels}-{bits}b")
    if hit:
        return hit
    # print a short readable head so the model can eyeball it
    head = data[:120]
    print(f"[stego:lsb] {channels} {bits}bit head: {head!r}")
    return StegoResult(found=False, method=f"lsb-{channels}-{bits}b", data=data,
                       notes="no flag in this layout; try other channels/bit order")


def _tool(name: str, args: list[str]) -> Optional[str]:
    if shutil.which(name) is None:
        return None
    try:
        r = subprocess.run([name, *args], capture_output=True, text=True, timeout=60)
        return r.stdout + r.stderr
    except (subprocess.SubprocessError, OSError):
        return None


def steghide_extract(path: str, passphrase: str = "") -> StegoResult:
    """Run steghide extraction with a candidate password and scan the result.

    Many misc/forensics CTFs give a visible/OCR/password hint first, then require
    using that exact string as the steghide passphrase. This helper makes that
    chain one call and prints recovered data for provenance.
    """
    if shutil.which("steghide") is None:
        msg = "steghide not installed; install steghide/stegseek or try another extractor"
        print(f"[stego:steghide] {msg}")
        return StegoResult(found=False, method="steghide", notes=msg)
    with tempfile.NamedTemporaryFile(prefix="muteki-steghide-", delete=False) as out:
        out_path = out.name
    try:
        cmd = ["steghide", "extract", "-sf", path, "-xf", out_path, "-p", passphrase, "-f"]
        r = subprocess.run(cmd, capture_output=True, text=True, timeout=60)
        combined = (r.stdout + r.stderr).encode("utf-8", "replace")
        if r.returncode != 0:
            text = (r.stdout + r.stderr).strip()[:500]
            print(f"[stego:steghide] pass={passphrase!r} failed: {text}")
            return StegoResult(found=False, method="steghide", notes=text)
        with open(out_path, "rb") as f:
            data = f.read()
        print(f"[stego:steghide] pass={passphrase!r} extracted {len(data)} bytes")
        preview = data[:800]
        print(preview.decode("utf-8", "replace"))
        hit = _scan_flag(data + combined, "steghide")
        if hit:
            return hit
        return StegoResult(found=bool(data), method="steghide", data=data,
                           notes="extracted data; inspect/auto_decode it for the flag")
    except (subprocess.SubprocessError, OSError) as e:
        return StegoResult(found=False, method="steghide", notes=str(e))
    finally:
        try:
            import os

            os.unlink(out_path)
        except OSError:
            pass


def try_steghide_passwords(path: str, passwords: list[str]) -> StegoResult:
    """Try a short list of high-confidence passphrases against steghide."""
    if not passwords:
        return StegoResult(found=False, method="steghide", notes="no passwords supplied")
    for pw in passwords:
        res = steghide_extract(path, pw)
        if res.found or res.flag:
            return res
    return StegoResult(found=False, method="steghide",
                       notes=f"tried {len(passwords)} passwords; no extraction")


def scan(path: str) -> StegoResult:
    """High-recall kitchen-sink: try LSB layouts + strings + appended data +
    zsteg/steghide if installed. Returns the first flag found, else a summary."""
    # 1. raw strings / appended data
    with open(path, "rb") as f:
        raw = f.read()
    hit = _scan_flag(raw, "strings")
    if hit:
        return hit

    # 2. common LSB layouts
    try:
        for ch in ("RGB", "R", "G", "B"):
            res = lsb_extract(path, bits=1, channels=ch)
            if res.found:
                return res
    except Exception as e:  # not an image, or decode error
        print(f"[stego:scan] LSB skipped: {e}")

    # 3. external tools if present
    z = _tool("zsteg", ["-a", path])
    if z:
        h = _scan_flag(z.encode(), "zsteg")
        if h:
            return h
        print(f"[stego:zsteg] (no flag) output head:\n{z[:400]}")
    elif shutil.which("zsteg") is None:
        print("[stego:scan] zsteg not installed (gem install zsteg) — skipped")

    sh = _tool("steghide", ["info", path])
    if sh:
        print(f"[stego:steghide] info:\n{sh[:300]}")

    return StegoResult(found=False, method="scan",
                       notes="no flag via strings/LSB/tools; try steghide extract "
                             "with a password, or other bit orders / palette indices")
