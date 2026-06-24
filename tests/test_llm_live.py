"""Live smoke test against the real DeepSeek endpoint.

Skipped unless MUTEKI_DEEPSEEK_API_KEY is set. Proves the client truly talks to
the model, gets non-empty content (reasoning budget respected), and drives a
real tool call.
"""

import os

import pytest

from muteki.core.cost import CostController
from muteki.core.event_bus import EventBus
from muteki.core.llm import LLMClient

pytestmark = pytest.mark.skipif(
    not os.environ.get("MUTEKI_DEEPSEEK_API_KEY"),
    reason="set MUTEKI_DEEPSEEK_API_KEY to run live LLM tests",
)


async def test_live_flash_returns_content() -> None:
    cost = CostController()
    async with LLMClient(cost=cost) as c:
        r = await c.chat(
            model="deepseek-v4-flash",
            messages=[{"role": "user", "content": "Reply with exactly the word: PONG"}],
            stream=True,
            max_tokens=2000,  # generous so content isn't starved by reasoning
            run_id="live",
        )
    assert "PONG" in r.content.upper()
    assert cost.global_usd() > 0


async def test_live_pro_tool_call() -> None:
    async with LLMClient() as c:
        r = await c.chat(
            model="deepseek-v4-pro",
            messages=[
                {"role": "user", "content": "Use the run_python tool to print 2+2. You must call the tool."}
            ],
            tools=[
                {
                    "type": "function",
                    "function": {
                        "name": "run_python",
                        "description": "execute python and return stdout",
                        "parameters": {
                            "type": "object",
                            "properties": {"code": {"type": "string"}},
                            "required": ["code"],
                        },
                    },
                }
            ],
            stream=True,
            max_tokens=3000,
            run_id="live",
        )
    assert r.has_tool_calls
    assert r.tool_calls[0].name == "run_python"
    assert "code" in r.tool_calls[0].parsed_args()
