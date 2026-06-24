"""muteki_kit.triage — classification + file sniff + entropy."""

from pathlib import Path

from muteki_kit.triage import classify, shannon_entropy


def test_classify_web_from_description() -> None:
    r = classify(name="baby login", description="bypass the login form via the http API")
    assert r.category_guess == "web"
    assert r.scores["web"] > 0


def test_classify_crypto() -> None:
    r = classify(name="rsa-baby", description="recover the plaintext given n, e, and ciphertext")
    assert r.category_guess == "crypto"


def test_category_hint_biases() -> None:
    r = classify(name="???", description="no useful words", category_hint="forensics")
    assert r.category_guess == "forensics"


def test_file_sniff_and_entropy(tmp_path: Path) -> None:
    elf = tmp_path / "a.bin"
    elf.write_bytes(b"\x7fELF" + b"\x00" * 100)
    r = classify(name="x", description="binary", attachments=[str(elf)])
    assert "ELF" in r.file_types[str(elf)]
    # ELF attachment biases toward pwn/reverse
    assert r.scores["pwn"] >= 1 or r.scores["reverse"] >= 1


def test_strings_surface_flag(tmp_path: Path) -> None:
    f = tmp_path / "blob.dat"
    f.write_bytes(b"\x00\x01garbage\x00flag{in_strings}\x00more")
    r = classify(name="x", description="y", attachments=[str(f)])
    assert any("flag{in_strings}" in s for s in r.notable_strings)


def test_entropy_bounds() -> None:
    assert shannon_entropy(b"") == 0.0
    assert shannon_entropy(b"aaaa") == 0.0  # zero entropy
    assert shannon_entropy(bytes(range(256))) > 7.9  # near-max
