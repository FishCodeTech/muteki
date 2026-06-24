"""muteki_kit reverse track — decompile / disasm / binary info.

radare2 (r2pipe) for cross-architecture decompilation (arm64 host decompiles
x86-64 ELFs natively — no Docker for STATIC RE), capstone for raw-byte disasm,
LIEF + pwntools for ELF/PE/Mach-O parsing + protections. Large pseudocode goes
to peekable artifacts, never inlined.
"""

from muteki_kit.reverse.decompile import (
    DecompileResult,
    decompile,
    list_functions,
    strings,
)
from muteki_kit.reverse.info import (
    BinInfo,
    SectionDump,
    SectionInfo,
    disasm,
    dump_section,
    info,
    sections,
)

__all__ = [
    "DecompileResult", "decompile", "list_functions", "strings",
    "BinInfo", "SectionInfo", "SectionDump", "disasm", "info", "sections", "dump_section",
]
