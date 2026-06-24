"""Decompilation + disassembly via radare2 (r2pipe) — cross-architecture.

radare2 analyzes ANY architecture regardless of host (VERIFIED: arm64 macOS
fully decompiles a Linux x86-64 ELF — no Docker needed for STATIC RE). Large
pseudocode/disasm is ALWAYS routed to an artifact (never inlined into context —
a 10KB decompilation truncated mid-stream makes the model hallucinate the rest).

If radare2's decompiler plugin (pdc/pdg) isn't present, falls back to annotated
disassembly (pdf), which is still readable structured text.
"""

from __future__ import annotations

import shutil
from typing import Optional

from pydantic import BaseModel

from muteki_kit.result import save_artifact


class DecompileResult(BaseModel):
    ok: bool
    function: str = ""
    pseudocode_artifact: Optional[str] = None  # peek this; never inlined
    head: str = ""  # first ~40 lines, safe to show inline
    functions: list[str] = []  # discovered function names
    notes: str = ""


def _open(path: str):
    import r2pipe

    r = r2pipe.open(path, flags=["-2"])  # -2 = no stderr noise
    r.cmd("e bin.cache=true")
    r.cmd("aaa")  # full analysis
    return r


def list_functions(path: str) -> list[str]:
    """All discovered function names (for the model to pick what to decompile)."""
    r = _open(path)
    try:
        fns = r.cmdj("aflj") or []
        names = [f.get("name", "") for f in fns]
        print(f"[r2] {len(names)} functions: {', '.join(names[:25])}")
        return names
    finally:
        r.quit()


def decompile(path: str, function: str = "main") -> DecompileResult:
    """Decompile one function. Pseudocode -> artifact (peek it); head shown inline.

    Use list_functions() first, then decompile the interesting one — peek the
    artifact with a query for the FLAG check / comparison logic instead of
    reading the whole thing."""
    r = _open(path)
    try:
        fns = [f.get("name", "") for f in (r.cmdj("aflj") or [])]
        # resolve the function (exact, or fuzzy contains)
        target = function
        if function not in fns:
            cand = next((f for f in fns if function in f or f.endswith("." + function)), None)
            target = cand or "main"
        # try the decompiler (pdc/pdg), fall back to annotated disasm (pdf)
        code = ""
        for cmd in (f"pdc @ {target}", f"pdg @ {target}", f"pdf @ {target}"):
            code = r.cmd(cmd) or ""
            if code.strip() and "Cannot" not in code[:40]:
                break
        if not code.strip():
            return DecompileResult(ok=False, function=target, functions=fns,
                                   notes="no output; try a different function or disasm")
        aid = save_artifact(code, suffix=".c")
        head = "\n".join(code.splitlines()[:40])
        print(f"[r2] decompiled {target} ({len(code)} chars) -> artifact {aid}")
        print(f"[r2] head:\n{head}")
        return DecompileResult(ok=True, function=target, pseudocode_artifact=aid,
                               head=head, functions=fns,
                               notes=f"full pseudocode in artifact {aid} — peek it "
                                     f"(query='flag'/'cmp'/'strcmp') instead of inlining")
    finally:
        r.quit()


def strings(path: str, *, min_len: int = 4) -> list[str]:
    """Extract strings (r2 izz, falls back to the `strings` binary)."""
    try:
        r = _open(path)
        try:
            out = r.cmdj("izzj") or []
            ss = [s.get("string", "") for s in out]
        finally:
            r.quit()
    except Exception:
        ss = []
    if not ss and shutil.which("strings"):
        import subprocess

        ss = subprocess.run(["strings", "-n", str(min_len), path],
                            capture_output=True, text=True).stdout.splitlines()
    # surface flag-shaped strings first
    flaggy = [s for s in ss if "{" in s and "}" in s]
    if flaggy:
        print(f"[r2:strings] flag-shaped: {flaggy[:5]}")
    print(f"[r2:strings] {len(ss)} strings (first 20): {ss[:20]}")
    return ss
