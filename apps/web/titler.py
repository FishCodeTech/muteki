"""Auto-title a solve conversation from the operator's opening prompt.

ChatGPT/Claude-style: the rail row starts as a "new conversation" placeholder,
then a short title quietly replaces it. We ask deepseek-v4-flash (cheap, fast)
for a 3-6 word title IN THE PROMPT'S OWN LANGUAGE. If the model is slow, errors,
or returns junk, we fall back to the first few words of the prompt — so the rail
ALWAYS shows something readable, never a bare run id.

The call is fire-and-forget from the start endpoint: it never blocks swarm
launch. On success it emits RUN_TITLED on the run's bus, which the rail picks up
(both via SSE and the /api/runs poll).
"""

from __future__ import annotations

import re
from typing import Optional

from muteki.core.events import Event, EventType
from muteki.core.event_bus import EventBus
from muteki.core.llm import LLMClient

TITLE_MODEL = "deepseek-v4-flash"

_SYSTEM = (
    "You name chat conversations. Given the user's opening message, reply with a "
    "SHORT title of 3 to 6 words that captures its topic. Use the SAME LANGUAGE as "
    "the message. No quotes, no punctuation at the end, no prefixes like 'Title:'. "
    "Do not visit URLs, follow links, or browse the web; simply extract the topic "
    "from the text. Reply with the title only."
)

_REFUSAL_STARTS = (
    "抱歉", "对不起", "我无法", "不能访问", "无法访问",
    "I cannot", "I can't", "I'm sorry", "Sorry",
)


def fallback_title(prompt: str, max_words: int = 6, max_chars: int = 48) -> str:
    """First few words of the prompt — the always-available degraded title.

    Collapses whitespace, strips a leading flag-format/url noise, and caps length
    so the rail row stays one line. CJK text has no spaces, so for those we cap by
    characters instead of words.
    """
    text = re.sub(r"\s+", " ", (prompt or "").strip())
    if not text:
        return ""
    # CJK-ish (no spaces): just clip by characters.
    if " " not in text:
        return text[:max_chars]
    title = " ".join(text.split(" ")[:max_words])
    return title[:max_chars]


def _clean(raw: str, prompt: str) -> str:
    """Sanitize the model's answer; fall back to the prompt head if it's unusable."""
    title = (raw or "").strip().strip("\"'“”‘’").strip()
    # one line only; drop a trailing period the model sometimes adds
    title = title.splitlines()[0].strip().rstrip(".。") if title else ""
    # reject empty or absurdly long answers (model ignored the instruction)
    if not title or len(title) > 80 or any(title.startswith(r) for r in _REFUSAL_STARTS):
        return fallback_title(prompt)
    return title


async def generate_title(
    prompt: str,
    *,
    llm: Optional[LLMClient] = None,
    bus: Optional[EventBus] = None,
    run_id: Optional[str] = None,
    model: Optional[str] = None,
    base_url: Optional[str] = None,
) -> str:
    """Return a short title for `prompt`; emit RUN_TITLED on `bus` if given.

    Never raises: any LLM failure degrades to `fallback_title`. The caller runs
    this as a detached task, so swallowing errors here keeps a flaky title API
    from surfacing as an unhandled-task warning.

    `base_url` overrides the titler endpoint (DESIGN §2.2 補强A) — empty/None =
    default DeepSeek. The API key is NOT passed here; it stays in .env. Only used
    when `llm` is not injected (we own the client lifecycle).
    """
    title = fallback_title(prompt)
    owns_llm = llm is None
    try:
        client = llm or (
            LLMClient(base_url=base_url) if (base_url or "").strip() else LLMClient()
        )
        try:
            resp = await client.chat(
                model=model or TITLE_MODEL,
                messages=[
                    {"role": "system", "content": _SYSTEM},
                    {"role": "user", "content": prompt[:2000]},
                ],
                temperature=0.3,
                max_tokens=2000,  # reasoning model: tokens go to reasoning first
                stream=False,
            )
            title = _clean(resp.content, prompt)
        finally:
            if owns_llm:
                await client.aclose()
    except Exception:
        # keep the fallback title; titling must never break a dispatch
        title = title or fallback_title(prompt)

    if bus is not None and run_id is not None and title:
        await bus.emit(
            Event(
                event_type=EventType.RUN_TITLED,
                run_id=run_id,
                payload={"title": title},
            )
        )
    return title
