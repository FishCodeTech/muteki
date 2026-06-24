"""Dynamic pwn — run / debug x86-64 ELFs inside the muteki-pwn Linux container.

arm64 macOS can't natively execute or gdb a Linux x86-64 binary, so this shells
into a linux/amd64 container (image `muteki-pwn`, build it with
`docker build --platform linux/amd64 -t muteki-pwn -f docker/Dockerfile.pwn .`).
The binary directory is mounted in; the helper runs a Python/pwntools script
INSIDE the container and returns its stdout. Use this to test an exploit locally
before firing at the remote, or to leak runtime addresses.

If the image isn't built, every call returns a clear note (so the model knows to
solve statically or ask for the image) instead of failing opaquely.
"""

from __future__ import annotations

import os
import shutil
import subprocess
import tempfile
from typing import Optional

from pydantic import BaseModel

_IMAGE = "muteki-pwn:latest"


class DynResult(BaseModel):
    ok: bool
    stdout: str = ""
    stderr: str = ""
    notes: str = ""


def image_available() -> bool:
    if shutil.which("docker") is None:
        return False
    r = subprocess.run(["docker", "image", "inspect", _IMAGE],
                       capture_output=True, text=True)
    return r.returncode == 0


def run_in_linux(script: str, *, mount_dir: str, timeout: float = 120.0,
                 ptrace: bool = False) -> DynResult:
    """Run a pwntools/python `script` inside the linux/amd64 container with
    `mount_dir` mounted at /work (so the script can ELF('/work/binary') and
    process() it). Returns the script's stdout.

    ptrace=True adds SYS_PTRACE + unconfined seccomp so gdb attach works."""
    if not image_available():
        return DynResult(ok=False, notes=(
            "muteki-pwn image not built — DYNAMIC pwn unavailable. Solve "
            "statically (checksec/ROP/remote) or build it: docker build "
            "--platform linux/amd64 -t muteki-pwn -f docker/Dockerfile.pwn ."))
    with tempfile.NamedTemporaryFile("w", suffix=".py", dir=mount_dir,
                                     delete=False) as f:
        f.write(script)
        script_name = os.path.basename(f.name)
    try:
        args = ["docker", "run", "--rm", "--platform", "linux/amd64",
                "-v", f"{os.path.abspath(mount_dir)}:/work", "-w", "/work"]
        if ptrace:
            args += ["--cap-add=SYS_PTRACE", "--security-opt", "seccomp=unconfined"]
        args += [_IMAGE, "python3", f"/work/{script_name}"]
        r = subprocess.run(args, capture_output=True, text=True, timeout=timeout)
        print(f"[dyn] exit={r.returncode}\nstdout:\n{r.stdout[:1500]}")
        if r.stderr.strip():
            print(f"[dyn] stderr:\n{r.stderr[:500]}")
        return DynResult(ok=r.returncode == 0, stdout=r.stdout, stderr=r.stderr)
    except subprocess.TimeoutExpired:
        return DynResult(ok=False, notes=f"timed out after {timeout}s")
    finally:
        try:
            os.unlink(os.path.join(mount_dir, script_name))
        except OSError:
            pass


def run_elf(binary_path: str, *, stdin_data: bytes = b"", timeout: float = 30.0) -> DynResult:
    """Just run an x86-64 ELF with given stdin and capture output (no exploit) —
    quick way to observe behavior. Mounts the binary's dir and process()es it."""
    mount = os.path.dirname(os.path.abspath(binary_path))
    name = os.path.basename(binary_path)
    script = (
        "from pwn import *\n"
        "context.log_level='error'\n"
        f"p=process('/work/{name}')\n"
        f"p.send({stdin_data!r})\n"
        "import sys\n"
        "try:\n    sys.stdout.write(p.recvall(timeout=5).decode('latin-1','replace'))\n"
        "except Exception as e:\n    print('recv:',e)\n"
    )
    # ensure the binary is executable inside the container
    return run_in_linux(script, mount_dir=mount, timeout=timeout)
