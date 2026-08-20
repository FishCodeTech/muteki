"""LLM structured parse of a conversational dispatch prompt.

Regex heuristics in ``drivers._infer_challenge`` / ``parse_engagement_goal``
remain the fallback. This module never raises: any failure returns ``{}`` and
the caller fills each missing field from regex or explicit form values.
"""

from __future__ import annotations

import json
import re
from typing import Any, Optional

from muteki.core.llm import LLMClient
from muteki.models.solve_graph import Category, QuantityKind

_CATEGORIES: frozenset[str] = frozenset(
    ("web", "pwn", "reverse", "crypto", "forensics", "misc"),
)
_QUANTITIES: frozenset[str] = frozenset(("first", "collect", "recon"))

_SYSTEM = (
    "You extract structured fields from an operator's dispatch message for a "
    "CTF or pentest run. Reply with a single JSON object and nothing else. "
    "Do not visit URLs or browse the web. Use null for any field you cannot "
    "determine from the text. Fields:\n"
    "  name: short human title in the prompt's language (3-12 words / 8-24 CJK chars)\n"
    "  category: one of web, pwn, reverse, crypto, forensics, misc\n"
    "  target: the primary URL or host:port to attack, or null\n"
    "  scope: in-scope assets (URL/host/path), or null\n"
    "  finding_class: vulnerability class if stated (sqli, xss, rce, idor, ssrf, "
    "or a short free-text label); generic if unspecified\n"
    "  quantity: first (stop at first valid finding), collect (gather N findings), "
    "or recon (map only, no exploit quota)\n"
    "  expected_findings: integer >= 1 when the operator asked for a count. "
    "Parse Chinese numerals (一份/两份/三份/四份/五份/十份) as 1-5/10. "
    "null when no count is given.\n"
    "  collect_until_coverage: true only for open-ended collect/recon with NO "
    "numeric quota; false when a count is given.\n"
    "Pentest of a web app is category web. A count such as '三份报告' means "
    "quantity=collect, expected_findings=3, collect_until_coverage=false."
)


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
    return data if isinstance(data, dict) else {}


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
    key = text.lower()
    if key in _CATEGORIES:
        return key  # type: ignore[return-value]
    return None


def _clean_quantity(value: Any) -> Optional[QuantityKind]:
    text = _clean_str(value, max_len=16)
    if text is None:
        return None
    key = text.lower()
    if key in _QUANTITIES:
        return key  # type: ignore[return-value]
    return None


def _clean_expected(value: Any) -> Optional[int]:
    if value is None or value is False:
        return None
    if isinstance(value, bool):
        return None
    try:
        n = int(value)
    except (TypeError, ValueError):
        return None
    if n < 1:
        return None
    return n


def _clean_bool(value: Any) -> Optional[bool]:
    if isinstance(value, bool):
        return value
    if value is None:
        return None
    text = str(value).strip().lower()
    if text in {"true", "1", "yes"}:
        return True
    if text in {"false", "0", "no"}:
        return False
    return None


def validate_dispatch_fields(data: dict[str, Any]) -> dict[str, Any]:
    """Keep only legal fields. Illegal values become absent (caller falls back)."""
    out: dict[str, Any] = {}
    name = _clean_str(data.get("name"), max_len=80)
    if name:
        out["name"] = name
    category = _clean_category(data.get("category"))
    if category:
        out["category"] = category
    target = _clean_str(data.get("target"), max_len=500)
    if target:
        out["target"] = target
    scope = _clean_str(data.get("scope"), max_len=500)
    if scope:
        out["scope"] = scope
    finding_class = _clean_str(data.get("finding_class"), max_len=64)
    if finding_class:
        out["finding_class"] = finding_class
    quantity = _clean_quantity(data.get("quantity"))
    if quantity:
        out["quantity"] = quantity
    expected = _clean_expected(data.get("expected_findings"))
    if expected is not None:
        out["expected_findings"] = expected
    until = _clean_bool(data.get("collect_until_coverage"))
    if until is not None:
        out["collect_until_coverage"] = until
    return out


async def parse_dispatch(
    prompt: str,
    goal: str,
    mode: str,
    *,
    llm: Optional[LLMClient] = None,
    model: Optional[str] = None,
) -> dict[str, Any]:
    """Return a validated field dict. Empty dict on any failure."""
    if llm is None:
        return {}
    user = (
        f"mode: {mode or 'ctf'}\n"
        f"goal: {(goal or '').strip() or '(none)'}\n"
        f"prompt:\n{(prompt or '')[:4000]}"
    )
    try:
        resp = await llm.chat(
            model=model or "deepseek-v4-pro",
            messages=[
                {"role": "system", "content": _SYSTEM},
                {"role": "user", "content": user},
            ],
            temperature=0.0,
            max_tokens=2000,
            stream=False,
        )
        return validate_dispatch_fields(_extract_json(resp.content))
    except Exception:
        return {}
