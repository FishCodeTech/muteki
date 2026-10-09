"""Credential-only exchange between local desktop environments.

Native login homes, models, probe results and runtime files never enter this
store. This is same-user logical isolation, not an OS security boundary.
"""
from __future__ import annotations

from contextlib import contextmanager
import json
import os
from pathlib import Path
import tempfile
import uuid

FILES = frozenset({"API_KEY", "CURSOR_API_KEY", "BASE_URL", "ENGINE", "PROVIDER"})
MARKER = "SHARED_CREDENTIAL.json"


class SharedCredentialError(ValueError):
    code = "credential.shared.unavailable"


class SharedCredentialStore:
    def __init__(self, root: str | Path):
        self.root = Path(root)
        try:
            self.root.mkdir(parents=True, exist_ok=True, mode=0o700)
        except OSError as exc:
            raise SharedCredentialError(f"共享凭据目录不可用：{exc}") from exc
        self.path = self.root / "credentials.json"

    @contextmanager
    def locked(self):
        with (self.root / "credentials.lock").open("a+b") as handle:
            if os.name == "nt":
                import msvcrt
                if handle.seek(0, 2) == 0:
                    handle.write(b"0"); handle.flush()
                handle.seek(0); msvcrt.locking(handle.fileno(), msvcrt.LK_LOCK, 1)
            else:
                import fcntl
                fcntl.flock(handle.fileno(), fcntl.LOCK_EX)
            try:
                yield
            finally:
                if os.name == "nt":
                    handle.seek(0); msvcrt.locking(handle.fileno(), msvcrt.LK_UNLCK, 1)
                else:
                    fcntl.flock(handle.fileno(), fcntl.LOCK_UN)

    def read(self):
        try:
            return self._read()
        except (OSError, ValueError, KeyError, TypeError) as exc:
            raise SharedCredentialError(f"共享凭据无法读取，未回退到宿主登录：{exc}") from exc

    def _read(self):
        if not self.path.exists():
            return {"version": 1, "accounts": {}}
        value = json.loads(self.path.read_text())
        if value.get("version") != 1 or not isinstance(value.get("accounts"), dict):
            raise ValueError("credential.shared.schema_unsupported")
        for account in value["accounts"].values():
            if not isinstance(account, dict) or not isinstance(account.get("owner"), str) or not isinstance(account.get("revision"), str) or not isinstance(account.get("files"), dict) or not set(account["files"]) <= FILES or not all(isinstance(v, str) for v in account["files"].values()):
                raise ValueError("credential.shared.material_invalid")
        return value

    def publish(self, account_id: str, files: dict[str, str] | None, owner: str):
        if files is not None and (not set(files) <= FILES or not (set(files) & {"API_KEY", "CURSOR_API_KEY"})):
            raise ValueError("credential.shared.authentication_unsupported")
        with self.locked():
            value = self.read()
            old = value["accounts"].get(account_id)
            if old and old["owner"] != owner:
                raise ValueError("credential.shared.owner_conflict")
            if files is None:
                value["accounts"].pop(account_id, None)
            elif not old or old["files"] != files:
                value["accounts"][account_id] = {"owner": owner, "revision": uuid.uuid4().hex, "files": files}
            with tempfile.NamedTemporaryFile(mode="w", dir=self.root, delete=False) as handle:
                temporary = Path(handle.name)
                try:
                    os.chmod(temporary, 0o600)
                    json.dump(value, handle)
                    handle.flush(); os.fsync(handle.fileno())
                    os.replace(temporary, self.path)
                finally:
                    temporary.unlink(missing_ok=True)
