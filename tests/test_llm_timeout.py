"""The chat() wall-clock guard: a stalled stream must not hang the caller."""
import asyncio
import pytest
from muteki.core.llm import LLMClient, LLMResponse


async def test_overall_timeout_returns_not_hangs(monkeypatch):
    c = LLMClient(api_key="x", overall_timeout=0.3)

    async def _never_returns(*a, **k):
        await asyncio.sleep(60)  # simulate a stalled/half-open SSE stream
        return LLMResponse(content="late", reasoning="", tool_calls=[])

    monkeypatch.setattr(c, "_chat_stream", _never_returns)
    r = await asyncio.wait_for(
        c.chat(model="m", messages=[{"role": "user", "content": "hi"}], stream=True, run_id="t"),
        timeout=5,  # if the guard works, chat() returns at ~0.3s, well under this
    )
    assert r.finish_reason == "timeout"
    assert r.content == ""
    await c.aclose()
