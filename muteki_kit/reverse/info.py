"""Binary info + disassembly — LIEF (parse) + capstone (disasm any arch).

Format/arch detection picks the right decompiler; capstone disassembles raw byte
blobs / shellcode the model extracts. All native on arm64 for x86-64 targets.
"""

from __future__ import annotations

import re
from typing import Optional

from pydantic import BaseModel

from muteki_kit.result import save_artifact

_FLAG_RE = re.compile(rb"[A-Za-z0-9_]{0,15}\{[^}]{1,200}\}")


class BinInfo(BaseModel):
    format: str = ""
    arch: str = ""
    bits: int = 0
    endian: str = ""
    entrypoint: int = 0
    imports: list[str] = []
    exports: list[str] = []
    nx: Optional[bool] = None
    pie: Optional[bool] = None
    canary: Optional[bool] = None
    notes: str = ""


class SectionInfo(BaseModel):
    name: str
    size: int = 0
    offset: int = 0
    virtual_address: int = 0
    entropy: float = 0.0
    flags: str = ""


class SectionDump(BaseModel):
    found: bool
    name: str = ""
    size: int = 0
    artifact_id: Optional[str] = None
    head_hex: str = ""
    printable: str = ""
    flag: Optional[str] = None
    notes: str = ""


def info(path: str) -> BinInfo:
    """Parse an ELF/PE/Mach-O: format, arch, protections, imports."""
    try:
        import lief

        b = lief.parse(path)
        if b is None:
            return BinInfo(notes="LIEF could not parse; not a known binary format")
        fmt = type(b).__module__.split(".")[-1]  # ELF / PE / MachO
        arch = str(getattr(b.header, "machine_type", "")) or str(getattr(b.header, "architecture", ""))
        bits = 64 if "64" in str(getattr(b.header, "identity_class", "")) or b.is_pie else 0
        imports = []
        try:
            imports = [s.name for s in b.imported_functions][:60]
        except Exception:
            pass
        # protections via pwntools (richer checksec) if available
        nx = pie = canary = None
        try:
            from pwn import ELF

            e = ELF(path, checksec=False)
            nx, pie, canary = e.nx, e.pie, e.canary
        except Exception:
            pie = getattr(b, "is_pie", None)
        res = BinInfo(format=fmt, arch=str(arch), bits=bits or 64,
                      entrypoint=int(getattr(b, "entrypoint", 0)),
                      imports=imports, nx=nx, pie=pie, canary=canary)
        print(f"[info] {fmt} {arch} pie={pie} nx={nx} canary={canary} "
              f"entry=0x{res.entrypoint:x} imports={imports[:12]}")
        return res
    except Exception as e:
        return BinInfo(notes=f"info error: {e}")


def sections(path: str) -> list[SectionInfo]:
    """List binary sections, surfacing odd/high-entropy sections for hidden bytes."""
    try:
        import lief

        b = lief.parse(path)
        if b is None:
            print("[sections] LIEF could not parse this file")
            return []
        standard = {
            ".text", ".rodata", ".data", ".bss", ".plt", ".got", ".got.plt",
            ".dynamic", ".dynsym", ".dynstr", ".interp", ".eh_frame",
            ".eh_frame_hdr", ".comment", ".symtab", ".strtab", ".shstrtab",
            ".init", ".fini", ".init_array", ".fini_array", ".rela.dyn",
            ".rela.plt", ".gnu.hash", ".gnu.version", ".gnu.version_r",
            ".note.gnu.build-id",
        }
        out: list[SectionInfo] = []
        suspicious: list[SectionInfo] = []
        for sec in getattr(b, "sections", []):
            si = SectionInfo(
                name=str(getattr(sec, "name", "")),
                size=int(getattr(sec, "size", 0) or 0),
                offset=int(getattr(sec, "offset", 0) or 0),
                virtual_address=int(getattr(sec, "virtual_address", 0) or 0),
                entropy=float(getattr(sec, "entropy", 0.0) or 0.0),
                flags=str(getattr(sec, "flags", "")),
            )
            out.append(si)
            if si.name and si.name not in standard and si.size > 0:
                suspicious.append(si)
        print(f"[sections] {len(out)} sections")
        for si in out[:30]:
            print(f"  {si.name:24} size={si.size:<7} off=0x{si.offset:x} "
                  f"vaddr=0x{si.virtual_address:x} entropy={si.entropy:.2f}")
        if suspicious:
            print("[sections] suspicious/nonstandard:")
            for si in suspicious[:12]:
                print(f"  {si.name} size={si.size} off=0x{si.offset:x} entropy={si.entropy:.2f}")
        return out
    except Exception as e:
        print(f"[sections] error: {e}")
        return []


def dump_section(path: str, name: str, *, max_inline: int = 512) -> SectionDump:
    """Dump a section by exact/substring name. Full bytes go to an artifact."""
    try:
        import lief

        b = lief.parse(path)
        if b is None:
            return SectionDump(found=False, name=name, notes="LIEF could not parse")
        secs = list(getattr(b, "sections", []))
        sec = next((s for s in secs if getattr(s, "name", "") == name), None)
        if sec is None:
            sec = next((s for s in secs if name in getattr(s, "name", "")), None)
        if sec is None:
            return SectionDump(found=False, name=name, notes="section not found")
        data = bytes(getattr(sec, "content", []) or [])
        aid = save_artifact(data, suffix=".bin") if data else None
        head = data[:max_inline]
        printable = "".join(chr(b) if 32 <= b < 127 else "." for b in head)
        flag = None
        m = _FLAG_RE.search(data)
        if m:
            flag = m.group(0).decode("latin-1", "replace")
            print(f"[section:{sec.name}] FLAG-shaped: {flag}")
        print(f"[section:{sec.name}] size={len(data)} artifact={aid}")
        print(f"[section:{sec.name}] head hex: {head.hex()}")
        print(f"[section:{sec.name}] printable: {printable}")
        return SectionDump(
            found=True,
            name=str(getattr(sec, "name", name)),
            size=len(data),
            artifact_id=aid,
            head_hex=head.hex(),
            printable=printable,
            flag=flag,
            notes="full section bytes saved as artifact; try XOR/add/sub/rotate transforms",
        )
    except Exception as e:
        return SectionDump(found=False, name=name, notes=f"dump_section error: {e}")


def disasm(code: bytes, *, arch: str = "x86", bits: int = 64,
           base: int = 0x1000) -> str:
    """Disassemble a raw byte blob (shellcode / extracted bytes) with capstone."""
    import capstone

    arch_map = {
        ("x86", 64): (capstone.CS_ARCH_X86, capstone.CS_MODE_64),
        ("x86", 32): (capstone.CS_ARCH_X86, capstone.CS_MODE_32),
        ("arm", 64): (capstone.CS_ARCH_ARM64, capstone.CS_MODE_ARM),
        ("arm", 32): (capstone.CS_ARCH_ARM, capstone.CS_MODE_ARM),
        ("mips", 32): (capstone.CS_ARCH_MIPS, capstone.CS_MODE_MIPS32),
    }
    cs_arch, cs_mode = arch_map.get((arch, bits), (capstone.CS_ARCH_X86, capstone.CS_MODE_64))
    md = capstone.Cs(cs_arch, cs_mode)
    lines = [f"0x{i.address:x}:\t{i.mnemonic}\t{i.op_str}"
             for i in md.disasm(code, base)]
    out = "\n".join(lines)
    print(f"[disasm] {len(lines)} instrs:\n{out[:600]}")
    return out
