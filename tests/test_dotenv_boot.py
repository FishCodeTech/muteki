"""load_env(): optional .env loading without breaking shell-env precedence."""

from __future__ import annotations

import importlib

import muteki.core.dotenv_boot as boot


def _fresh():
    """Reload the module so the per-process `_loaded` latch resets per test."""
    return importlib.reload(boot)


def test_missing_file_is_noop(tmp_path, monkeypatch):
    m = _fresh()
    assert m.load_env(tmp_path / "nope.env") is False


def test_loads_values(tmp_path, monkeypatch):
    m = _fresh()
    env = tmp_path / ".env"
    env.write_text("MUTEKI_TEST_FOO=from_file\n")
    monkeypatch.delenv("MUTEKI_TEST_FOO", raising=False)
    assert m.load_env(env) is True
    import os
    assert os.environ.get("MUTEKI_TEST_FOO") == "from_file"


def test_shell_env_wins(tmp_path, monkeypatch):
    """A var already set in the environment must NOT be overridden by the file."""
    m = _fresh()
    env = tmp_path / ".env"
    env.write_text("MUTEKI_TEST_BAR=from_file\n")
    monkeypatch.setenv("MUTEKI_TEST_BAR", "from_shell")
    m.load_env(env)
    import os
    assert os.environ.get("MUTEKI_TEST_BAR") == "from_shell"


def test_idempotent_latch(tmp_path):
    """Second call is a no-op (returns False) even with a valid file present."""
    m = _fresh()
    env = tmp_path / ".env"
    env.write_text("MUTEKI_TEST_BAZ=1\n")
    assert m.load_env(env) is True
    assert m.load_env(env) is False
