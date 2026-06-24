"""Static + network pwn helpers — pwntools-based, native on macOS.

Static binary analysis (ELF parsing, protections, ROP gadgets, cyclic offsets)
and REMOTE interaction (talk to a `nc` service) both run natively on arm64 — they
read bytes / speak TCP, they don't execute the target. DYNAMIC work (running the
ELF, gdb) is in dynamic.py via the muteki-pwn Linux container.
"""

from __future__ import annotations

import os
from typing import Optional

from pydantic import BaseModel

os.environ.setdefault("PWNLIB_NOTERM", "1")
os.environ.setdefault("TERM", "dumb")


class ElfInfo(BaseModel):
    arch: str = ""
    bits: int = 0
    pie: Optional[bool] = None
    nx: Optional[bool] = None
    canary: Optional[bool] = None
    relro: str = ""
    symbols: dict[str, int] = {}  # name -> address (subset)
    got: dict[str, int] = {}
    plt: dict[str, int] = {}
    notes: str = ""


def checksec(path: str) -> ElfInfo:
    """Protections + key symbols of an ELF (pwntools, no execution)."""
    from pwn import ELF, context

    context.log_level = "error"
    e = ELF(path, checksec=False)
    syms = {k: v for k, v in list(e.symbols.items())[:200] if isinstance(v, int)}
    got = {k: v for k, v in e.got.items()} if hasattr(e, "got") else {}
    plt = {k: v for k, v in e.plt.items()} if hasattr(e, "plt") else {}
    info = ElfInfo(
        arch=e.arch, bits=e.bits, pie=e.pie, nx=e.nx, canary=e.canary,
        relro=str(e.relro), symbols=syms, got=dict(list(got.items())[:60]),
        plt=dict(list(plt.items())[:60]),
    )
    print(f"[checksec] {e.arch}/{e.bits} pie={e.pie} nx={e.nx} canary={e.canary} "
          f"relro={e.relro}")
    interesting = {k: hex(v) for k, v in syms.items()
                   if any(w in k for w in ("win", "flag", "system", "shell", "main", "vuln"))}
    if interesting:
        print(f"[checksec] interesting symbols: {interesting}")
    return info


def find_gadgets(path: str, query: str = "") -> list[str]:
    """ROP gadgets (pwntools ROP). `query` filters (e.g. 'pop rdi')."""
    from pwn import ELF, ROP, context

    context.log_level = "error"
    rop = ROP(ELF(path, checksec=False))
    gadgets = []
    for addr, g in rop.gadgets.items():
        line = f"0x{addr:x}: {'; '.join(g.insns)}"
        if not query or query.lower() in line.lower():
            gadgets.append(line)
    print(f"[rop] {len(gadgets)} gadgets" + (f" matching '{query}'" if query else "")
          + ":\n" + "\n".join(gadgets[:25]))
    return gadgets


def cyclic_offset(path_or_pattern, target: bytes) -> int:
    """Find the offset of `target` (the value that landed in RIP/a register) in a
    cyclic pattern — the classic 'how many bytes to the return address'."""
    from pwn import cyclic_find

    off = cyclic_find(target)
    print(f"[cyclic] offset of {target!r}: {off}")
    return off


def make_cyclic(n: int = 200) -> bytes:
    from pwn import cyclic

    pat = cyclic(n)
    print(f"[cyclic] {n}-byte pattern generated")
    return pat


class RemoteResult(BaseModel):
    sent: list[str] = []
    received: str = ""
    flag: Optional[str] = None


def remote_interact(host: str, port: int, sends: list[bytes],
                    *, timeout: float = 10.0, flag_re: str = r"[A-Za-z0-9_]{1,15}\{[^}]{1,200}\}") -> RemoteResult:
    """Talk to a remote pwn service: send each payload, collect output, scan for a
    flag. For scripted interaction; for a live exploit loop use pwn.remote directly."""
    import re

    from pwn import context, remote

    context.log_level = "error"
    io = remote(host, port, timeout=timeout)
    out = b""
    try:
        for payload in sends:
            io.send(payload)
            try:
                out += io.recv(timeout=timeout)
            except EOFError:
                break
    finally:
        io.close()
    text = out.decode("latin-1", "replace")
    print(f"[remote] received {len(out)} bytes:\n{text[:800]}")
    m = re.search(flag_re, text)
    flag = m.group(0) if m else None
    if flag:
        print(f"[remote] FLAG: {flag}")
    return RemoteResult(sent=[p.decode("latin-1", "replace")[:60] for p in sends],
                        received=text[:2000], flag=flag)
