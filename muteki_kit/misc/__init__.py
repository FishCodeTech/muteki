"""muteki_kit misc track — encoding/decoding, QR/barcode, esolangs, audio."""

from muteki_kit.misc import encoding
from muteki_kit.misc.encoding import auto_decode, find_flag
from muteki_kit.misc.esolang import EsoResult, looks_like_brainfuck, run_brainfuck
from muteki_kit.misc.qr import CodeResult, decode_image

__all__ = [
    "encoding", "auto_decode", "find_flag",
    "CodeResult", "decode_image",
    "EsoResult", "looks_like_brainfuck", "run_brainfuck",
]
