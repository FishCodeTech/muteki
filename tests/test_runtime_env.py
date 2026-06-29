"""P2-v3: in-container detection + the "no silent container→local fallback" guard.

is_web_container() decides whether THIS process (the coordinator / web-api) runs
inside a container; when it does, a container backend that goes unavailable must
HARD-FAIL the run instead of silently launching a host-native CLI inside the web
container. On a bare host the historical local fallback is preserved.
"""

from __future__ import annotations

import pytest

from muteki.core import runtime_env
from muteki.core.runtime_env import is_web_container


def test_explicit_env_wins_truthy_and_falsy(monkeypatch):
    monkeypatch.setenv("MUTEKI_IN_CONTAINER", "1")
    assert is_web_container() is True
    monkeypatch.setenv("MUTEKI_IN_CONTAINER", "true")
    assert is_web_container() is True
    # explicit "0" force-disables even if /.dockerenv exists (test-under-docker case)
    monkeypatch.setenv("MUTEKI_IN_CONTAINER", "0")
    assert is_web_container() is False
    monkeypatch.setenv("MUTEKI_IN_CONTAINER", "false")
    assert is_web_container() is False


def test_no_env_falls_back_to_filesystem_sniff(monkeypatch):
    monkeypatch.delenv("MUTEKI_IN_CONTAINER", raising=False)
    # /.dockerenv present → container
    monkeypatch.setattr(runtime_env.os.path, "exists",
                        lambda p: p == "/.dockerenv")
    assert is_web_container() is True


def test_no_env_no_markers_is_host(monkeypatch):
    monkeypatch.delenv("MUTEKI_IN_CONTAINER", raising=False)
    monkeypatch.setattr(runtime_env.os.path, "exists", lambda p: False)
    # /proc/1/cgroup unreadable (typical on macOS host) → OSError → host
    def _boom(*a, **k):
        raise OSError("no /proc")
    monkeypatch.setattr("builtins.open", _boom)
    assert is_web_container() is False
