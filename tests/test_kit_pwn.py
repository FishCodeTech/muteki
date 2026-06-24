"""muteki_kit.pwn — static analysis (native pwntools) + dynamic gating.

Static (checksec/cyclic) runs against a locally-compiled binary in CI. The
dynamic helper is asserted to degrade gracefully when the muteki-pwn image is
absent (so the model gets a clear note, not an opaque failure).
"""

import shutil
import subprocess
import sys

import pytest

from muteki_kit.pwn import (
    cyclic_offset,
    image_available,
    make_cyclic,
    run_elf,
)


def test_cyclic_offset_roundtrip() -> None:
    pat = make_cyclic(200)
    assert len(pat) == 200
    # take a 4-byte chunk at a known offset and verify cyclic_offset finds it
    chunk = pat[40:44]
    off = cyclic_offset(None, chunk)
    assert off == 40


def test_pwntools_import_survives_kernel_stream_tap() -> None:
    """Regression for live pwn traces: the kernel stream tap has no fileno()."""
    code = r'''
import io
import os
import sys

os.environ.pop("PWNLIB_NOTERM", None)

class Tap(io.TextIOBase):
    def write(self, s):
        return len(s)
    def flush(self):
        pass

old = sys.stdout
sys.stdout = Tap()
try:
    from muteki_kit.pwn import make_cyclic
    from pwn import cyclic
    data = make_cyclic(16)
finally:
    sys.stdout = old

print(len(data), len(cyclic(16)))
'''
    r = subprocess.run([sys.executable, "-c", code], capture_output=True, text=True, timeout=20)
    assert r.returncode == 0, r.stderr
    assert r.stdout.strip() == "16 16"


def _find_corpus_elf():
    """A real x86-64 ELF from the NYU corpus, if the clone is present."""
    import os
    from pathlib import Path

    root = Path(os.environ.get("MUTEKI_NYU_CTF_DIR",
                               str(Path.home() / "Desktop" / "nyu_ctf_bench")))
    if not root.exists():
        return None
    import subprocess as sp
    for f in list((root / "test").rglob("*"))[:2000]:
        if f.is_file() and f.stat().st_size > 1000:
            try:
                out = sp.run(["file", str(f)], capture_output=True, text=True, timeout=5).stdout
                if "ELF 64-bit" in out and "x86-64" in out:
                    return str(f)
            except (sp.SubprocessError, OSError):
                continue
    return None


def test_checksec_on_real_elf() -> None:
    # checksec works on x86-64 ELFs cross-platform (pwntools reads bytes, no exec).
    elf = _find_corpus_elf()
    if elf is None:
        pytest.skip("no x86-64 ELF available (NYU corpus not present)")
    from muteki_kit.pwn import checksec

    info = checksec(elf)
    assert info.arch and info.bits == 64
    assert info.nx is not None  # protections were read


def test_dynamic_degrades_gracefully_without_image(tmp_path) -> None:
    # if the muteki-pwn image isn't built, run_elf returns a clear note, no crash
    if image_available():
        pytest.skip("muteki-pwn image present; degradation path not exercised")
    binp = tmp_path / "x"
    binp.write_bytes(b"\x7fELF" + b"\x00" * 100)
    r = run_elf(str(binp))
    assert r.ok is False and "muteki-pwn" in r.notes
