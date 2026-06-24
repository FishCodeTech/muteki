"""File carving + type identification — find embedded/appended/hidden files.

magika (Google DL file-typer) for triage; binwalk (subprocess) for carving
embedded files; a pure-Python magic-byte scanner as a fallback. The single most
common forensics pattern is "a file with another file appended/embedded".
"""

from __future__ import annotations

import re
import shutil
import subprocess
from typing import Optional

from pydantic import BaseModel

# common file signatures (magic bytes) -> name, for the pure-python fallback
_SIGS = [
    (b"\x89PNG\r\n\x1a\n", "PNG"), (b"\xff\xd8\xff", "JPEG"),
    (b"PK\x03\x04", "ZIP/Office"), (b"PK\x05\x06", "ZIP(empty)"),
    (b"GIF87a", "GIF"), (b"GIF89a", "GIF"), (b"%PDF", "PDF"),
    (b"\x7fELF", "ELF"), (b"Rar!", "RAR"), (b"7z\xbc\xaf\x27\x1c", "7z"),
    (b"\x1f\x8b", "GZIP"), (b"BZh", "BZIP2"), (b"OggS", "OGG"),
    (b"RIFF", "RIFF/WAV/AVI"), (b"ID3", "MP3"), (b"\xfd7zXZ\x00", "XZ"),
]


class CarveResult(BaseModel):
    file_type: str = ""
    embedded: list[str] = []  # descriptions of embedded files found
    notes: str = ""


def identify(path: str) -> str:
    """Best file-type guess (magika if available, else magic-byte sniff)."""
    try:
        from magika import Magika

        res = Magika().identify_path(path)
        label = getattr(res.output, "label", None) or getattr(res.output, "ct_label", str(res.output))
        print(f"[carve:magika] {path} -> {label}")
        return str(label)
    except Exception:
        with open(path, "rb") as f:
            head = f.read(16)
        for sig, name in _SIGS:
            if head.startswith(sig):
                print(f"[carve:sniff] {path} -> {name}")
                return name
        print(f"[carve:sniff] {path} -> unknown ({head!r})")
        return "unknown"


def scan_embedded(path: str) -> CarveResult:
    """Find embedded/appended files. Uses binwalk if present, else a magic-byte
    scan that flags any known signature appearing AFTER offset 0 (appended data)."""
    ftype = identify(path)
    embedded: list[str] = []

    if shutil.which("binwalk"):
        try:
            r = subprocess.run(["binwalk", path], capture_output=True, text=True, timeout=60)
            print(f"[carve:binwalk]\n{r.stdout[:600]}")
            # binwalk lines: "DECIMAL HEX DESCRIPTION"
            for line in r.stdout.splitlines():
                if re.match(r"^\d+\s+0x", line.strip()) and int(line.split()[0]) > 0:
                    embedded.append(line.strip())
        except (subprocess.SubprocessError, OSError) as e:
            print(f"[carve:binwalk] error: {e}")

    # ALWAYS also run the pure-python appended-signature scan — binwalk can miss
    # simply-appended data (no valid header), which is the most common CTF case.
    with open(path, "rb") as f:
        data = f.read()
    for sig, name in _SIGS:
        idx = data.find(sig, 1)
        while idx > 0:
            desc = f"{name} signature at offset {idx} (0x{idx:x})"
            if not any(name in e for e in embedded):
                embedded.append(desc)
            idx = data.find(sig, idx + 1)
    if embedded:
        print("[carve:sniff] appended/embedded: " + "; ".join(embedded[:8]))

    return CarveResult(file_type=ftype, embedded=embedded,
                       notes="extract embedded files with `binwalk -e FILE` or by "
                             "slicing at the offset; a ZIP appended to a PNG is the "
                             "classic case")
