"""Ansible Vault helpers for file-only forensics challenges.

The important bit is non-interactivity: never let `ansible-vault` prompt on
stdin inside the persistent kernel. We pass a temporary password file and
`stdin=DEVNULL`, then print any recovered plaintext for provenance.
"""

from __future__ import annotations

import re
import shutil
import subprocess
import tempfile
from pathlib import Path
from typing import Optional

from pydantic import BaseModel


_FLAG_RE = re.compile(rb"[A-Za-z0-9_]{0,15}\{[^}\r\n]{1,200}\}")


class VaultResult(BaseModel):
    found: bool
    method: str = "ansible-vault"
    password: Optional[str] = None
    plaintext: Optional[str] = None
    flag: Optional[str] = None
    notes: str = ""


def _flag(data: bytes) -> Optional[str]:
    m = _FLAG_RE.search(data)
    if not m:
        return None
    return m.group(0).decode("utf-8", "replace")


def ansible_vault_view(path: str, password: str, *, timeout: float = 20.0) -> VaultResult:
    """Run `ansible-vault view` with a candidate password, never interactively."""
    if shutil.which("ansible-vault") is None:
        msg = "ansible-vault not installed"
        print(f"[vault] {msg}")
        return VaultResult(found=False, notes=msg)
    p = Path(path)
    if not p.exists():
        return VaultResult(found=False, password=password, notes=f"missing file: {path}")

    with tempfile.NamedTemporaryFile("w", prefix="muteki-vault-pass-", delete=False) as pwf:
        pwf.write(password)
        pwf.write("\n")
        pw_path = pwf.name
    try:
        cmd = ["ansible-vault", "view", "--vault-password-file", pw_path, str(p)]
        r = subprocess.run(
            cmd,
            capture_output=True,
            stdin=subprocess.DEVNULL,
            timeout=timeout,
        )
    except (subprocess.SubprocessError, OSError) as exc:
        return VaultResult(found=False, password=password, notes=str(exc))
    finally:
        Path(pw_path).unlink(missing_ok=True)

    out = r.stdout + r.stderr
    if r.returncode != 0:
        text = out.decode("utf-8", "replace").strip()
        print(f"[vault] pass={password!r} failed: {text[:240]}")
        return VaultResult(found=False, password=password, notes=text[:500])

    flag = _flag(out)
    text = r.stdout.decode("utf-8", "replace")
    print(f"[vault] pass={password!r} decrypted {path}: {text[:1000]}")
    return VaultResult(found=bool(flag or text), password=password,
                       plaintext=text, flag=flag)


def try_ansible_vault_passwords(
    paths: list[str],
    passwords: list[str],
    *,
    timeout: float = 20.0,
) -> VaultResult:
    """Try candidate passwords against one or more Ansible Vault files."""
    paths = [str(Path(p)) for p in paths]
    passwords = [p for p in dict.fromkeys(passwords) if p]
    if not paths:
        return VaultResult(found=False, notes="no vault paths supplied")
    if not passwords:
        return VaultResult(found=False, notes="no passwords supplied")

    last_note = ""
    for path in paths:
        for password in passwords:
            res = ansible_vault_view(path, password, timeout=timeout)
            if res.flag:
                return res
            if res.found:
                return res
            last_note = res.notes
    return VaultResult(found=False, notes=last_note or "no password worked")
