"""LLM client unit tests with mocked transport (no key, deterministic).

Covers the reasoning-model handling and streaming tool-call reassembly that the
meta-executor depends on. A separate live smoke test (test_llm_live.py) hits the
real endpoint when MUTEKI_DEEPSEEK_API_KEY is set.
"""

import asyncio
import json

import httpx
import pytest

from muteki.core.cost import CostController
from muteki.core.event_bus import EventBus
from muteki.core.events import EventType
from muteki.core.llm import LLMClient


def _sse(chunks: list[dict]) -> bytes:
    body = ""
    for c in chunks:
        body += f"data: {json.dumps(c)}\n\n"
    body += "data: [DONE]\n\n"
    return body.encode()


def _client_with(handler, **kw) -> LLMClient:
    transport = httpx.MockTransport(handler)
    c = LLMClient(api_key="test", **kw)
    c._client = httpx.AsyncClient(transport=transport, trust_env=False)
    return c


async def test_nonstreaming_splits_reasoning_and_content() -> None:
    def handler(request: httpx.Request) -> httpx.Response:
        return httpx.Response(
            200,
            json={
                "choices": [
                    {
                        "message": {
                            "role": "assistant",
                            "content": "the answer is 4",
                            "reasoning_content": "2+2 is 4",
                        },
                        "finish_reason": "stop",
                    }
                ],
                "usage": {"prompt_tokens": 10, "completion_tokens": 8},
                "model": "deepseek-v4-pro",
            },
        )

    async with _client_with(handler) as c:
        r = await c.chat(
            model="deepseek-v4-pro",
            messages=[{"role": "user", "content": "2+2?"}],
            stream=False,
        )
    assert r.content == "the answer is 4"
    assert r.reasoning == "2+2 is 4"
    assert r.input_tokens == 10 and r.output_tokens == 8
    assert not r.has_tool_calls


async def test_nonstreaming_tool_calls_parsed() -> None:
    def handler(request: httpx.Request) -> httpx.Response:
        return httpx.Response(
            200,
            json={
                "choices": [
                    {
                        "message": {
                            "role": "assistant",
                            "content": "",
                            "tool_calls": [
                                {
                                    "id": "call_1",
                                    "function": {
                                        "name": "run_python",
                                        "arguments": '{"code": "print(2+2)"}',
                                    },
                                }
                            ],
                        },
                        "finish_reason": "tool_calls",
                    }
                ],
                "usage": {"prompt_tokens": 5, "completion_tokens": 12},
            },
        )

    async with _client_with(handler) as c:
        r = await c.chat(
            model="deepseek-v4-pro",
            messages=[{"role": "user", "content": "compute"}],
            tools=[{"type": "function", "function": {"name": "run_python", "parameters": {}}}],
            stream=False,
        )
    assert r.has_tool_calls
    tc = r.tool_calls[0]
    assert tc.name == "run_python"
    assert tc.parsed_args() == {"code": "print(2+2)"}


async def test_streaming_reassembles_fragmented_tool_call_and_emits() -> None:
    # tool call arguments fragmented across deltas, as the real API does
    chunks = [
        {"choices": [{"delta": {"role": "assistant", "reasoning_content": "I should "}}]},
        {"choices": [{"delta": {"reasoning_content": "call the tool"}}]},
        {"choices": [{"delta": {"tool_calls": [{"index": 0, "id": "c1", "function": {"name": "run_python", "arguments": "{\"code\""}}]}}]},
        {"choices": [{"delta": {"tool_calls": [{"index": 0, "function": {"arguments": ": \"print(1)\"}"}}]}}]},
        {"choices": [{"delta": {}, "finish_reason": "tool_calls"}], "usage": {"prompt_tokens": 7, "completion_tokens": 20}},
    ]

    def handler(request: httpx.Request) -> httpx.Response:
        return httpx.Response(200, content=_sse(chunks), headers={"content-type": "text/event-stream"})

    bus = EventBus()
    cost = CostController()
    reasoning_events = []

    async def consume() -> None:
        async for e in bus.subscribe():
            if e.event_type is EventType.REASONING_DELTA:
                reasoning_events.append(e.payload["text"])
            if e.event_type is EventType.REASONING_DELTA and "call the tool" in e.payload["text"]:
                return

    async with _client_with(handler, bus=bus, cost=cost) as c:
        t = asyncio.create_task(consume())
        await asyncio.sleep(0.02)
        r = await c.chat(
            model="deepseek-v4-pro",
            messages=[{"role": "user", "content": "go"}],
            tools=[{"type": "function", "function": {"name": "run_python", "parameters": {}}}],
            stream=True,
            run_id="r1",
            solver_id="s1",
        )
        await asyncio.wait_for(t, timeout=5)

    assert r.has_tool_calls
    tc = r.tool_calls[0]
    assert tc.name == "run_python"
    assert tc.parsed_args() == {"code": "print(1)"}  # reassembled correctly
    assert r.finish_reason == "tool_calls"
    assert "".join(reasoning_events) == "I should call the tool"
    # cost recorded
    assert cost.global_usd() > 0


async def test_streaming_emits_content_and_reasoning_separately() -> None:
    chunks = [
        {"choices": [{"delta": {"reasoning_content": "thinking..."}}]},
        {"choices": [{"delta": {"content": "Hello"}}]},
        {"choices": [{"delta": {"content": " world"}}], "usage": {"prompt_tokens": 3, "completion_tokens": 4}},
        {"choices": [{"delta": {}, "finish_reason": "stop"}]},
    ]

    def handler(request: httpx.Request) -> httpx.Response:
        return httpx.Response(200, content=_sse(chunks), headers={"content-type": "text/event-stream"})

    bus = EventBus()
    text_events, reasoning_events = [], []

    async def consume() -> None:
        seen = 0
        async for e in bus.subscribe():
            if e.event_type is EventType.TEXT_MESSAGE_DELTA:
                text_events.append(e.payload["text"])
            elif e.event_type is EventType.REASONING_DELTA:
                reasoning_events.append(e.payload["text"])
            seen += 1
            if seen >= 3:
                return

    async with _client_with(handler, bus=bus) as c:
        t = asyncio.create_task(consume())
        await asyncio.sleep(0.02)
        r = await c.chat(
            model="deepseek-v4-flash",
            messages=[{"role": "user", "content": "hi"}],
            stream=True,
            run_id="r1",
        )
        await asyncio.wait_for(t, timeout=5)

    assert r.content == "Hello world"
    assert r.reasoning == "thinking..."
    assert "".join(text_events) == "Hello world"
    assert reasoning_events == ["thinking..."]


async def test_max_tokens_omitted_when_none() -> None:
    """max_tokens=None must drop the field from the request body entirely (let the
    API use the model's own maximum). The Reason planner relies on this: a small cap
    on a reasoning model is spent on reasoning_content first and truncates the JSON
    answer (run-7349: 0 intents → endless retry_bootstrap)."""
    captured: dict = {}

    def handler(request: httpx.Request) -> httpx.Response:
        captured["body"] = json.loads(request.content)
        return httpx.Response(
            200,
            json={"choices": [{"message": {"role": "assistant", "content": "{}"},
                               "finish_reason": "stop"}],
                  "usage": {"prompt_tokens": 1, "completion_tokens": 1},
                  "model": "deepseek-v4-pro"},
        )

    async with _client_with(handler) as c:
        await c.chat(model="deepseek-v4-pro",
                     messages=[{"role": "user", "content": "x"}],
                     max_tokens=None, stream=False)
    assert "max_tokens" not in captured["body"], \
        "max_tokens=None must omit the cap so reasoning output isn't truncated"


async def test_max_tokens_sent_when_set() -> None:
    """A concrete max_tokens is still passed through (back-compat for capped calls)."""
    captured: dict = {}

    def handler(request: httpx.Request) -> httpx.Response:
        captured["body"] = json.loads(request.content)
        return httpx.Response(
            200,
            json={"choices": [{"message": {"role": "assistant", "content": "ok"},
                               "finish_reason": "stop"}],
                  "usage": {"prompt_tokens": 1, "completion_tokens": 1},
                  "model": "deepseek-v4-flash"},
        )

    async with _client_with(handler) as c:
        await c.chat(model="deepseek-v4-flash",
                     messages=[{"role": "user", "content": "x"}],
                     max_tokens=512, stream=False)
    assert captured["body"].get("max_tokens") == 512
