"""titler base_url consumption (DESIGN §2.2 補強A).

The API key is never carried in config; it stays in .env. We only assert the
endpoint override flows through.
"""

from __future__ import annotations

import asyncio

import apps.web.titler as titler_mod


class _RecordingLLM:
    """Stands in for LLMClient — records the base_url it was built with and
    returns a canned non-empty title so generate_title's happy path runs."""

    seen_base_url: "str | None" = None
    seen_temperature_mode: "str | None" = None
    seen_temperature_value: "float | None" = None

    def __init__(
        self,
        *,
        base_url: str | None = None,
        temperature_mode: str | None = None,
        temperature_value: float | None = None,
        **_kw,
    ) -> None:
        type(self).seen_base_url = base_url
        type(self).seen_temperature_mode = temperature_mode
        type(self).seen_temperature_value = temperature_value

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


def test_titler_forwards_custom_temperature(monkeypatch):
    _RecordingLLM.seen_temperature_mode = None
    _RecordingLLM.seen_temperature_value = None
    monkeypatch.setattr(titler_mod, "LLMClient", _RecordingLLM)
    asyncio.run(titler_mod.generate_title(
        "solve this challenge", model="titler-x",
        temperature_mode="custom", temperature=1))
    assert _RecordingLLM.seen_temperature_mode == "custom"
    assert _RecordingLLM.seen_temperature_value == 1.0
