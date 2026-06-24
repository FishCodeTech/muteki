"""muteki_kit.misc.encoding — multi-layer auto-decode."""

import base64

from muteki_kit.misc.encoding import auto_decode, find_flag


def test_find_flag() -> None:
    assert find_flag("blah flag{abc_123} blah") == "flag{abc_123}"
    assert find_flag("CTF{x}") == "CTF{x}"
    assert find_flag("nothing here") is None


def test_single_layer_base64() -> None:
    enc = base64.b64encode(b"flag{base64_once}").decode()
    r = auto_decode(enc)
    assert r.success and r.flag == "flag{base64_once}"
    assert [s.codec for s in r.chain] == ["base64"]


def test_triple_layer_base64() -> None:
    s = "flag{deep}"
    for _ in range(3):
        s = base64.b64encode(s.encode()).decode()
    r = auto_decode(s)
    assert r.success and r.flag == "flag{deep}"
    assert len(r.chain) == 3


def test_mixed_hex_then_base64() -> None:
    inner = base64.b64encode(b"flag{mixed_layers}").decode()
    hexed = inner.encode().hex()
    r = auto_decode(hexed)
    assert r.success and r.flag == "flag{mixed_layers}"
    codecs = [s.codec for s in r.chain]
    assert codecs[0] == "hex" and "base64" in codecs


def test_rot13_then_base64() -> None:
    import codecs as _c

    inner = base64.b64encode(b"flag{rot_then_b64}").decode()
    rotted = _c.encode(inner, "rot_13")
    r = auto_decode(rotted)
    assert r.success and r.flag == "flag{rot_then_b64}"


def test_binary_encoding() -> None:
    msg = "flag{bits}"
    bits = "".join(format(ord(ch), "08b") for ch in msg)
    r = auto_decode(bits)
    assert r.success and r.flag == "flag{bits}"


def test_flag_already_present() -> None:
    r = auto_decode("here it is flag{plain}")
    assert r.success and r.flag == "flag{plain}"
    assert r.chain == []


def test_no_flag_returns_best_plaintext() -> None:
    enc = base64.b64encode(b"just some readable text without a flag").decode()
    r = auto_decode(enc)
    assert r.success is False
    assert "readable text" in (r.plaintext or "")
