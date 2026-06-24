"""muteki_kit.misc — QR / brainfuck / encoding (synthetic fixtures, no network)."""

import pytest

from muteki_kit.misc import (
    decode_image,
    looks_like_brainfuck,
    run_brainfuck,
)


def test_brainfuck_hello() -> None:
    # classic Brainfuck "Hello World!" (prefix is enough to verify execution)
    hw = ("++++++++[>++++[>++>+++>+++>+<<<<-]>+>+>->>+[<]<-]>>.>---.+++++++.."
          "+++.>>.<-.<.+++.------.--------.>>+.>++.")
    r = run_brainfuck(hw)
    assert r.ran and r.output.startswith("Hello World!")


def test_brainfuck_detect() -> None:
    assert looks_like_brainfuck("+++[>+++<-]>.") is True
    assert looks_like_brainfuck("just some normal english text here") is False


def test_brainfuck_handles_unbalanced() -> None:
    r = run_brainfuck("+++[>")  # unbalanced
    assert r.ran is False and "unbalanced" in r.notes


def test_qr_decode(tmp_path) -> None:
    # generate a QR with a flag and decode it round-trip (skip if qrcode absent)
    qrcode = pytest.importorskip("qrcode")
    img = qrcode.make("flag{qr_code_decoded}")
    p = tmp_path / "q.png"
    img.save(str(p))
    r = decode_image(str(p))
    assert r.found and r.text == "flag{qr_code_decoded}"


def test_qr_no_crash_on_blank(tmp_path) -> None:
    from PIL import Image
    p = tmp_path / "blank.png"
    Image.new("RGB", (32, 32), (255, 255, 255)).save(p)
    r = decode_image(str(p))
    assert r.found is False  # nothing to decode, but no crash
