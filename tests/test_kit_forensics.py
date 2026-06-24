"""muteki_kit.forensics — stego / pcap / carving / metadata on synthetic fixtures.

Builds fixtures in-memory (LSB-embedded PNG, ZIP-appended-to-PNG) so the tests
run in CI without external sample files. No network.
"""

import io
import struct
import zlib

import numpy as np
import pytest
from PIL import Image

from muteki_kit.forensics import identify, lsb_extract, scan, scan_embedded
from muteki_kit.forensics.vault import ansible_vault_view, try_ansible_vault_passwords
from muteki_kit.forensics.stego import steghide_extract, try_steghide_passwords
from muteki_kit.forensics.metadata import exif


def _make_lsb_png(tmp_path, secret: bytes):
    """Embed `secret` bytes in the LSB of an RGB image (RGB order, MSB-first)."""
    bits = []
    for byte in secret:
        for i in range(8):
            bits.append((byte >> (7 - i)) & 1)
    npix = (len(bits) + 2) // 3 + 10
    side = int(npix ** 0.5) + 2
    arr = np.random.randint(0, 256, (side, side, 3), dtype=np.uint8)
    flat = arr.reshape(-1)
    for i, b in enumerate(bits):
        flat[i] = (flat[i] & 0xFE) | b
    img = Image.fromarray(flat.reshape(side, side, 3), "RGB")
    p = tmp_path / "secret.png"
    img.save(p)
    return str(p)


def test_lsb_extract_recovers_flag(tmp_path) -> None:
    flag = b"flag{lsb_steganography}"
    path = _make_lsb_png(tmp_path, flag)
    r = lsb_extract(path, bits=1, channels="RGB")
    assert r.found and r.flag == "flag{lsb_steganography}"


def test_scan_finds_strings_flag(tmp_path) -> None:
    # a flag in plain bytes appended to an image
    img = Image.new("RGB", (8, 8), (1, 2, 3))
    p = tmp_path / "s.png"
    img.save(p)
    with open(p, "ab") as f:
        f.write(b"flag{appended_in_strings}")
    r = scan(str(p))
    assert r.found and r.flag == "flag{appended_in_strings}"


def test_identify_png(tmp_path) -> None:
    p = tmp_path / "x.png"
    Image.new("RGB", (4, 4)).save(p)
    t = identify(str(p))
    assert "png" in t.lower()


def test_scan_embedded_finds_appended_zip(tmp_path) -> None:
    # PNG with a ZIP appended (the classic carve case)
    p = tmp_path / "carrier.png"
    Image.new("RGB", (8, 8)).save(p)
    zip_sig = b"PK\x03\x04" + b"\x00" * 26 + b"fakezipcontent"
    with open(p, "ab") as f:
        f.write(zip_sig)
    r = scan_embedded(str(p))
    # binwalk or the pure-python scanner should flag the appended ZIP signature
    assert any("PK" in e or "Zip" in e or "ZIP" in e or "zip" in e.lower() for e in r.embedded) \
        or "PK" in r.notes or r.embedded != []


def test_exif_no_crash_on_plain_image(tmp_path) -> None:
    p = tmp_path / "plain.png"
    Image.new("RGB", (4, 4)).save(p)
    r = exif(str(p))
    assert r.found is False  # no flag, but must not crash


def test_pcap_find_flag(tmp_path) -> None:
    # build a minimal pcap with a TCP packet carrying a flag (scapy)
    from scapy.all import IP, TCP, Ether, wrpcap

    pkt = Ether() / IP(src="1.1.1.1", dst="2.2.2.2") / TCP(sport=1234, dport=80) / b"GET /flag{pcap_stream_flag} HTTP/1.1"
    p = tmp_path / "c.pcap"
    wrpcap(str(p), [pkt])
    from muteki_kit.forensics import find_flag
    r = find_flag(str(p))
    assert r.found and r.flag == "flag{pcap_stream_flag}"


def test_steghide_helpers_degrade_when_tool_missing(tmp_path, monkeypatch) -> None:
    p = tmp_path / "carrier.jpg"
    p.write_bytes(b"not really a jpeg")
    monkeypatch.setenv("PATH", "")
    r = steghide_extract(str(p), "candidate")
    assert r.found is False and "steghide not installed" in r.notes
    rr = try_steghide_passwords(str(p), ["candidate"])
    assert rr.found is False


def test_ansible_vault_helpers_degrade_when_tool_missing(tmp_path, monkeypatch) -> None:
    p = tmp_path / "vault.yml"
    p.write_text("$ANSIBLE_VAULT;1.1;AES256\n00\n")
    monkeypatch.setenv("PATH", "")
    r = ansible_vault_view(str(p), "candidate")
    assert r.found is False and "ansible-vault not installed" in r.notes
    rr = try_ansible_vault_passwords([str(p)], ["candidate"])
    assert rr.found is False


def test_ansible_vault_helper_uses_noninteractive_password_file(tmp_path, monkeypatch) -> None:
    fake = tmp_path / "ansible-vault"
    fake.write_text(
        "#!/bin/sh\n"
        "case \"$1\" in view) shift;; *) exit 2;; esac\n"
        "while [ \"$1\" ]; do\n"
        "  if [ \"$1\" = \"--vault-password-file\" ]; then shift; pw=$(/bin/cat \"$1\"); shift; break; fi\n"
        "  shift\n"
        "done\n"
        "test \"$pw\" = candidate || exit 1\n"
        "printf 'secret: flag{vault_noninteractive}\\n'\n"
    )
    fake.chmod(0o755)
    monkeypatch.setenv("PATH", str(tmp_path))
    p = tmp_path / "vault.yml"
    p.write_text("$ANSIBLE_VAULT;1.1;AES256\n00\n")
    r = try_ansible_vault_passwords([str(p)], ["wrong", "candidate"])
    assert r.found and r.flag == "flag{vault_noninteractive}"
