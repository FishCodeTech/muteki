"""planner/titler base_url consumption (DESIGN §2.2 補強A).

Both LLM-call paths must forward a configured `base_url` to LLMClient — a fix
that touches ONLY one path silently falls back to DeepSeek on the other, so
these tests assert BOTH planner (drivers.py) and titler (titler.py) wiring.

The API key is never carried in config; it stays in .env. We only assert the
endpoint override flows through.
"""

from __future__ import annotations

import asyncio

import pytest

import apps.web.titler as titler_mod


class _RecordingLLM:
    """Stands in for LLMClient — records the base_url it was built with and
    returns a canned non-empty title so generate_title's happy path runs."""

    seen_base_url: "str | None" = None

    def __init__(self, *, base_url: str | None = None, **_kw) -> None:
        type(self).seen_base_url = base_url

    async def chat(self, *a, **k):
        class _Resp:
            content = "A Title"
        return _Resp()

    async def aclose(self) -> None:
        pass


def test_titler_forwards_base_url(monkeypatch):
    """generate_title(base_url=...) constructs LLMClient with that base_url."""
    _RecordingLLM.seen_base_url = None
    monkeypatch.setattr(titler_mod, "LLMClient", _RecordingLLM)
    title = asyncio.run(titler_mod.generate_title(
        "solve this challenge", model="titler-x",
        base_url="https://api.openai-compat.test/v1"))
    assert title == "A Title"
    assert _RecordingLLM.seen_base_url == "https://api.openai-compat.test/v1"


def test_titler_no_base_url_uses_default(monkeypatch):
    """Empty base_url → LLMClient built with no base_url override (= DeepSeek)."""
    _RecordingLLM.seen_base_url = "sentinel"
    monkeypatch.setattr(titler_mod, "LLMClient", _RecordingLLM)
    asyncio.run(titler_mod.generate_title("hi", model="titler-x", base_url=""))
    assert _RecordingLLM.seen_base_url is None


def test_planner_forwards_base_url(monkeypatch):
    """build_driver's coordinator path constructs LLMClient with the planner
    profile's base_url. We patch LLMClient where drivers.py imports it (lazy
    import inside build_driver) and stop the run right after construction."""
    import muteki.core.llm as llm_mod

    seen = {}

    class _LLM:
        def __init__(self, *, base_url: str | None = None, **_kw):
            seen["base_url"] = base_url

        async def __aenter__(self):
            # abort the run right here — we only care that base_url was passed.
            # MUST be a BaseException, not Exception: drivers.py wraps the
            # coordinator LLMClient setup in `except Exception` (→ llm=None and
            # the run continues into the swarm, which would hang). A
            # BaseException escapes that guard and unwinds the run immediately.
            raise _StopHere()

        async def __aexit__(self, *a):
            return False

    class _StopHere(BaseException):
        pass

    monkeypatch.setattr(llm_mod, "LLMClient", _LLM)

    from apps.web.drivers import build_driver
    from apps.web.run_manager import RunManager

    import tempfile
    with tempfile.TemporaryDirectory() as td:
        mgr = RunManager(sessions_root=td)
        driver = build_driver({
            "prompt": "solve me",
            "coordinator": True,
            "engines": ["local-claude"],
            # local runtime → no credential account required (host login),
            # so build_driver reaches the coordinator LLMClient construction.
            "worker_backend": "local",
            "runtime_profiles": [{"id": "local", "backend": "local"}],
            "worker_profiles": [{
                "id": "local-claude", "name": "local-claude",
                "engine": "claude", "transport": "claude_code",
                "credential_mode": "subscription", "credential_account": "claude-main",
                "runtime": "local", "enabled": True,
            }],
            "llm_profiles": {
                "planner": {"provider": "deepseek", "model": "p-x",
                            "base_url": "https://planner.endpoint.test/v1"},
                "titler": {"provider": "deepseek", "model": "t-x", "base_url": ""},
            },
        }, mgr=mgr)
        run = mgr.create("planner-base-url")
        with pytest.raises(_StopHere):
            asyncio.run(driver(run))

    assert seen.get("base_url") == "https://planner.endpoint.test/v1"
