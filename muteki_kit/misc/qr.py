"""QR / barcode decoding — pyzbar + zxing-cpp (complementary decoders).

Tries both decoders (they cover different damaged/rotated cases). Decoded text
is printed for the provenance gate.
"""

from __future__ import annotations

import os
from typing import Optional

from pydantic import BaseModel


class CodeResult(BaseModel):
    found: bool
    decoder: str = ""
    text: Optional[str] = None
    type: str = ""
    notes: str = ""


def decode_image(path: str) -> CodeResult:
    """Decode any QR/barcode in an image.

    Default decoder is zxing-cpp (pure C++ static build, no external dylib).
    pyzbar shells out to libzbar, which segfaults on some Python-3.13/macOS
    pytest processes — a native crash try/except cannot catch, so libzbar is
    *opt-in only* via MUTEKI_QR_USE_PYZBAR=1, and even then only as a fallback
    when zxing-cpp finds nothing. zxing-cpp alone covers the common cases.
    """
    # zxing-cpp
    try:
        import zxingcpp
        from PIL import Image

        results = zxingcpp.read_barcodes(Image.open(path))
        for r in results:
            print(f"[qr:zxing] {r.format}: {r.text}")
            return CodeResult(found=True, decoder="zxing", text=r.text, type=str(r.format))
    except Exception as e:
        print(f"[qr:zxing] {e}")

    # pyzbar (opt-in fallback; libzbar can segfault on some setups)
    if os.environ.get("MUTEKI_QR_USE_PYZBAR", "0") == "1":
        try:
            from PIL import Image
            from pyzbar.pyzbar import decode as zbar_decode

            for sym in zbar_decode(Image.open(path)):
                text = sym.data.decode("utf-8", "replace")
                print(f"[qr:pyzbar] {sym.type}: {text}")
                return CodeResult(found=True, decoder="pyzbar", text=text, type=str(sym.type))
        except Exception as e:
            print(f"[qr:pyzbar] {e}")

    return CodeResult(found=False, notes="no code decoded; the image may be "
                      "damaged/rotated/inverted — try PIL transforms (invert, "
                      "rotate, resize, threshold) then re-decode, or rebuild a "
                      "QR from a bit-matrix if the modules are visible")
