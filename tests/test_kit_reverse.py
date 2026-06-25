"""muteki_kit.reverse — disasm (capstone, no binary) + r2 integration if present.

capstone disasm needs no external binary, so it always runs in CI. The r2 decompile
path is exercised against a tiny ELF we build with the system toolchain IF gcc can
emit one; otherwise that test skips (CI without a compiler still covers disasm).
"""

import shutil
import subprocess

import pytest

from muteki_kit.reverse import disasm, info
from muteki_kit.reverse.info import BinInfo


def test_disasm_x86_64_bytes() -> None:
    # mov eax, 1 ; ret  (x86-64)
    code = b"\xb8\x01\x00\x00\x00\xc3"
    out = disasm(code, arch="x86", bits=64)
    assert "mov" in out and "ret" in out


def test_disasm_arm64_bytes() -> None:
    # ret on arm64 = 0xd65f03c0
    code = b"\xc0\x03\x5f\xd6"
    out = disasm(code, arch="arm", bits=64)
    assert "ret" in out.lower()


def test_info_handles_non_binary(tmp_path) -> None:
    p = tmp_path / "notabin.txt"
    p.write_text("just text, not an ELF")
    r = info(str(p))
    assert isinstance(r, BinInfo)  # graceful: returns a result, doesn't raise


@pytest.mark.skipif(shutil.which("gcc") is None and shutil.which("cc") is None,
                    reason="no C compiler to build a test ELF")
@pytest.mark.skipif(shutil.which("radare2") is None,
                    reason="radare2 binary not in PATH (r2pipe needs it)")
def test_decompile_real_binary(tmp_path) -> None:
    # build a tiny native binary and decompile its main (r2 is cross-arch, so a
    # native arm64 binary works fine as the decompile target)
    import r2pipe  # noqa: F401  (skip cleanly if r2pipe import fails)

    src = tmp_path / "t.c"
    src.write_text('#include <string.h>\nint main(int c,char**v){'
                   'return c>1 && strcmp(v[1],"secret")==0 ? 0 : 1;}\n')
    binp = tmp_path / "t.bin"
    cc = shutil.which("gcc") or shutil.which("cc")
    r = subprocess.run([cc, str(src), "-o", str(binp)], capture_output=True, text=True)
    if r.returncode != 0:
        pytest.skip(f"compile failed: {r.stderr[:200]}")

    from muteki_kit.reverse import decompile, dump_section, list_functions, sections, strings

    fns = list_functions(str(binp))
    assert any("main" in f for f in fns)
    ss = strings(str(binp))
    assert any("secret" in s for s in ss)  # the comparison target is in strings
    sects = sections(str(binp))
    assert any(s.name in {".text", "__text"} for s in sects)
    dump = dump_section(str(binp), "rodata")
    if not dump.found:
        dump = dump_section(str(binp), "cstring")
    assert dump.found and "secret" in dump.printable
    d = decompile(str(binp), "main")
    assert d.ok and d.pseudocode_artifact  # pseudocode went to an artifact
    assert "main" in d.function
