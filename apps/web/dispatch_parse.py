"""LLM labeler for the left-rail challenge title and category.

A successful call returns ``{name, category}``. Transport errors and an
unusable model reply raise; this module does not invent a fallback label.
The result is display-only and must not be written onto the Swarm Challenge.
"""

from __future__ import annotations

import json
import re
from typing import Any, Optional

from muteki.core.llm import LLMClient
from muteki.models.solve_graph import Category

CATEGORIES: frozenset[str] = frozenset(
    ("web", "pwn", "reverse", "crypto", "forensics", "misc"),
)
_CATEGORY_ALIASES: dict[str, str] = {
    "web": "web",
    "pwn": "pwn",
    "reverse": "reverse",
    "rev": "reverse",
    "crypto": "crypto",
    "forensics": "forensics",
    "misc": "misc",
    "密码": "crypto",
    "加密": "crypto",
    "逆向": "reverse",
    "取证": "forensics",
    "杂项": "misc",
    "渗透": "web",
    "二进制": "pwn",
}
_PROMPT_LIMIT = 16_000

_SYSTEM = (
    "You name a CTF or pentest task for a sidebar list. Reply with a single "
    "JSON object and nothing else. Do not visit URLs or browse the web. "
    "Use null for any field you cannot determine from the text. Fields:\n"
    "  name: short human title in the prompt's language "
    "(3-12 words / 8-24 CJK chars)\n"
    "  category: one of web, pwn, reverse, crypto, forensics, misc"
)


def explicit_category(value: Any) -> str:
    """Return a legal category only when the operator sent the English enum."""
    if value is None:
        return ""
    key = str(value).strip().lower()
    if not key:
        return ""
    if key not in CATEGORIES:
        raise ValueError(f"category {value!r} is not a known category")
    return key


def _extract_json(raw: str) -> dict[str, Any]:
    text = (raw or "").strip()
    if not text:
        return {}
    fenced = re.search(r"```(?:json)?\s*(\{.*?\})\s*```", text, re.S)
    if fenced:
        text = fenced.group(1)
    else:
        start = text.find("{")
        end = text.rfind("}")
        if start < 0 or end <= start:
            return {}
        text = text[start:end + 1]
    try:
        data = json.loads(text)
    except json.JSONDecodeError:
        return {}
    if not isinstance(data, dict):
        return {}
    return data


def _clean_str(value: Any, *, max_len: int = 240) -> Optional[str]:
    if value is None:
        return None
    text = str(value).strip()
    if not text or text.lower() in {"null", "none", "n/a"}:
        return None
    return text[:max_len]


def _clean_category(value: Any) -> Optional[Category]:
    text = _clean_str(value, max_len=32)
    if text is None:
        return None
    key = _CATEGORY_ALIASES.get(text.lower()) or _CATEGORY_ALIASES.get(text)
    if key in CATEGORIES:
        return key  # type: ignore[return-value]
    raise ValueError(f"planner category {text!r} is not a known category")


def validate_dispatch_fields(data: dict[str, Any]) -> dict[str, Any]:
    """Keep only a legal title and category."""
    out: dict[str, Any] = {}
    name = _clean_str(data.get("name"), max_len=80)
    if name:
        out["name"] = name
    if "category" in data and data.get("category") is not None:
        category = _clean_category(data.get("category"))
        if category:
            out["category"] = category
    return out


async def parse_dispatch(
    prompt: str,
    *,
    llm: LLMClient,
    model: Optional[str] = None,
) -> dict[str, Any]:
    """Return ``{name, category}``. Raises on transport or unusable replies."""
    prompt_text = str(prompt or "").strip()
    if not prompt_text:
        raise ValueError("planner prompt is empty")
    model_name = model or "deepseek-v4-pro"
    resp = await llm.chat(
        model=model_name,
        messages=[
            {"role": "system", "content": _SYSTEM},
            {"role": "user", "content": prompt_text[:_PROMPT_LIMIT]},
        ],
        stream=False,
    )
    from_content = _extract_json(resp.content)
    from_reasoning = _extract_json(resp.reasoning)
    if not from_content and not from_reasoning:
        raise ValueError("planner did not return JSON")
    parsed = validate_dispatch_fields({**from_reasoning, **from_content})
    if not parsed:
        raise ValueError("planner returned no title or category")
    return parsed
