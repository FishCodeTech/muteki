"""Metadata extraction — EXIF / document metadata via exiftool (or PIL fallback).

Flags often hide in EXIF comments, GPS, author fields, or XMP. Prints all
metadata so the model can scan it; surfaces any flag-shaped value.
"""

from __future__ import annotations

import re
import shutil
import subprocess
from typing import Optional

from pydantic import BaseModel

_FLAG_RE = re.compile(r"[A-Za-z0-9_]{1,15}\{[^}]{1,200}\}")


class MetaResult(BaseModel):
    found: bool
    flag: Optional[str] = None
    fields: dict = {}
    notes: str = ""


def exif(path: str) -> MetaResult:
    """Dump all metadata (exiftool if present, else PIL EXIF) and scan for a flag."""
    text = ""
    fields: dict = {}

    if shutil.which("exiftool"):
        try:
            r = subprocess.run(["exiftool", path], capture_output=True, text=True, timeout=30)
            text = r.stdout
            for line in text.splitlines():
                if ":" in line:
                    k, _, v = line.partition(":")
                    fields[k.strip()] = v.strip()
        except (subprocess.SubprocessError, OSError) as e:
            text = f"(exiftool error: {e})"
    else:
        try:
            from PIL import Image
            from PIL.ExifTags import TAGS

            img = Image.open(path)
            raw = img.getexif()
            for tag_id, val in raw.items():
                fields[TAGS.get(tag_id, tag_id)] = str(val)
            text = "\n".join(f"{k}: {v}" for k, v in fields.items())
            text += "\n(install exiftool for richer metadata: brew install exiftool)"
        except Exception as e:
            text = f"(PIL exif error: {e})"

    print(f"[meta] metadata:\n{text[:800]}")
    m = _FLAG_RE.search(text)
    if m:
        flag = m.group(0)
        print(f"[meta] FLAG in metadata: {flag}")
        return MetaResult(found=True, flag=flag, fields=fields)
    return MetaResult(found=False, fields=fields,
                      notes="no flag in metadata; check GPS, comments, XMP, or "
                            "thumbnail (exiftool -b -ThumbnailImage)")
