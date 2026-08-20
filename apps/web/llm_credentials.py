"""Private API-key storage for the coordinator planner and conversation titler."""

from __future__ import annotations

import os
import tempfile
from pathlib import Path


LLM_PROFILE_NAMES = {"planner", "titler"}


class LlmCredentialStore:
    """Store per-profile keys outside the normal worker configuration JSON."""

    def __init__(self, sessions_root: str | Path) -> None:
        self.root = Path(sessions_root) / "_secrets" / "llm_profiles"
        self.root.mkdir(parents=True, exist_ok=True)
        try:
            self.root.chmod(0o700)
        except OSError:
            pass

    @staticmethod
    def _profile(which: str) -> str:
        profile = (which or "").strip().lower()
        if profile not in LLM_PROFILE_NAMES:
            raise ValueError("which must be planner or titler")
        return profile

    def _path(self, which: str) -> Path:
        return self.root / self._profile(which) / "API_KEY"

    def saved_key(self, which: str) -> str:
        path = self._path(which)
        try:
            return path.read_text(encoding="utf-8").strip()
        except OSError:
            return ""

    def resolve(self, which: str) -> str:
        return self.saved_key(which) or os.environ.get("MUTEKI_DEEPSEEK_API_KEY", "").strip()

    def source(self, which: str) -> str:
        if self.saved_key(which):
            return "saved"
        if os.environ.get("MUTEKI_DEEPSEEK_API_KEY", "").strip():
            return "environment"
        return "missing"

    def save(self, which: str, api_key: str) -> None:
        value = str(api_key or "").strip()
        if not value:
            raise ValueError("API Key 不能为空")
        path = self._path(which)
        path.parent.mkdir(parents=True, exist_ok=True)
        try:
            path.parent.chmod(0o700)
        except OSError:
            pass
        fd, tmp_name = tempfile.mkstemp(prefix=".API_KEY.", dir=str(path.parent))
        try:
            with os.fdopen(fd, "w", encoding="utf-8") as handle:
                handle.write(value + "\n")
                handle.flush()
                os.fsync(handle.fileno())
            os.chmod(tmp_name, 0o600)
            os.replace(tmp_name, path)
        finally:
            try:
                os.unlink(tmp_name)
            except FileNotFoundError:
                pass

    def clear(self, which: str) -> None:
        path = self._path(which)
        try:
            path.unlink()
        except FileNotFoundError:
            pass
