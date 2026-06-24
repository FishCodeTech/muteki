"""muteki_kit pwn track — static/network (native) + dynamic (Linux container).

Static analysis (checksec/ROP/cyclic) and remote interaction run native on macOS
via pwntools. Dynamic execution/debugging of x86-64 ELFs uses the muteki-pwn
linux/amd64 container (build: docker build --platform linux/amd64 -t muteki-pwn
-f docker/Dockerfile.pwn .). Typed Results + provenance printing.
"""

import os

# The sandbox kernel replaces sys.stdout/sys.stderr with a JSON stream tap that
# intentionally has no fileno(). Pwntools' terminal layer probes termcap at import
# time and crashes in that environment unless it is disabled before any `pwn`
# import. Warmup imports muteki_kit.pwn first, so this also protects solver code
# that later does `from pwn import ...` directly.
os.environ.setdefault("PWNLIB_NOTERM", "1")
os.environ.setdefault("TERM", "dumb")

from muteki_kit.pwn.dynamic import (
    DynResult,
    image_available,
    run_elf,
    run_in_linux,
)
from muteki_kit.pwn.elf import (
    ElfInfo,
    RemoteResult,
    checksec,
    cyclic_offset,
    find_gadgets,
    make_cyclic,
    remote_interact,
)

__all__ = [
    "ElfInfo", "checksec", "find_gadgets", "cyclic_offset", "make_cyclic",
    "RemoteResult", "remote_interact",
    "DynResult", "image_available", "run_in_linux", "run_elf",
]
