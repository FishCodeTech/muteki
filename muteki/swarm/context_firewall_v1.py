"""Context firewall / working-memory fold for Reason (NYU A/B round 17+).

Structural break
----------------
Failed interrupt stacking stuffed *more* guidance text into Reason after
cancels. This module does the opposite: Reason only sees a **compressed
working-memory packet** (top facts, open intents, flags, one brief) — long
standing_guidance stacks and sprawling attempted-intent sections are folded
away. Main Agent (Reason) gets a Context Manager view, not the raw board dump.

Default OFF: ``MUTEKI_CONTEXT_FIREWALL=1``.
"""

from __future__ import annotations

import os
import re
from typing import Any, Final

ENV_FLAG: Final = "MUTEKI_CONTEXT_FIREWALL"
ENV_MAX_FACTS: Final = "MUTEKI_CONTEXT_FIREWALL_MAX_FACTS"
ENV_MAX_INTENTS: Final = "MUTEKI_CONTEXT_FIREWALL_MAX_INTENTS"

DEFAULT_MAX_FACTS: Final = 8
DEFAULT_MAX_INTENTS: Final = 3
PACKET_PREFIX: Final = "[context-firewall working memory]"


def enabled(flag: bool | None = None) -> bool:
    if flag is not None:
        return bool(flag)
    raw = (os.environ.get(ENV_FLAG) or "").strip().lower()
    return raw in {"1", "true", "yes", "on"}


def max_facts() -> int:
    raw = (os.environ.get(ENV_MAX_FACTS) or "").strip()
    try:
        val = int(raw) if raw else DEFAULT_MAX_FACTS
    except ValueError:
        val = DEFAULT_MAX_FACTS
    return max(2, min(24, val))


def max_intents() -> int:
    raw = (os.environ.get(ENV_MAX_INTENTS) or "").strip()
    try:
        val = int(raw) if raw else DEFAULT_MAX_INTENTS
    except ValueError:
        val = DEFAULT_MAX_INTENTS
    return max(1, min(8, val))


def _clip(text: str, n: int = 160) -> str:
    body = re.sub(r"\s+", " ", str(text or "")).strip()
    if len(body) <= n:
        return body
    return body[: max(0, n - 1)] + "…"


def fold_reason_context(
    shared_graph: Any,
    standing_guidance: list[str] | None = None,
    *,
    max_fact_n: int | None = None,
    max_intent_n: int | None = None,
) -> str:
    """Build a short Reason packet; falls back to full summary if graph missing."""
    if shared_graph is None:
        return ""
    if not enabled():
        try:
            return shared_graph.to_reason_summary(
                standing_guidance=list(standing_guidance or [])
            )
        except Exception:
            return ""

    fact_n = max_facts() if max_fact_n is None else int(max_fact_n)
    intent_n = max_intents() if max_intent_n is None else int(max_intent_n)

    chal = getattr(shared_graph, "challenge", None)
    name = str(getattr(chal, "name", "") or getattr(chal, "id", "") or "challenge")
    cat = str(getattr(chal, "category", "") or "")
    desc = _clip(str(getattr(chal, "description", "") or ""), 220)

    facts: list[str] = []
    flags: list[str] = []
    opens: list[str] = []
    try:
        snap = shared_graph.snapshot()
        raw_facts = []
        if isinstance(snap, dict):
            raw_facts = snap.get("facts") or snap.get("evidence") or []
            flags = [str(f) for f in (snap.get("flags") or []) if f][:4]
            intents = snap.get("intents") or []
        else:
            raw_facts = getattr(snap, "facts", None) or []
            flags = [str(f) for f in (getattr(snap, "flags", None) or []) if f][:4]
            intents = getattr(snap, "intents", None) or []
        for item in raw_facts:
            if isinstance(item, dict):
                text = str(item.get("text") or item.get("fact") or "")
                seq = item.get("seq") or item.get("fact_seq")
            else:
                text = str(getattr(item, "text", None) or item or "")
                seq = getattr(item, "seq", None)
            if not text.strip():
                continue
            label = f"[#{seq}] " if seq not in (None, "") else ""
            facts.append(label + _clip(text, 140))
            if len(facts) >= fact_n:
                break
        for intent in intents:
            if not isinstance(intent, dict):
                continue
            state = str(intent.get("state") or "").lower()
            if state not in {"open", "claimed", "proposed", ""}:
                continue
            goal = _clip(str(intent.get("goal") or ""), 100)
            if goal:
                opens.append(f"{state or 'open'}: {goal}")
            if len(opens) >= intent_n:
                break
    except Exception:
        pass

    # Keep only the newest firewall / solo / interrupt packet from guidance.
    guide = ""
    for item in reversed(list(standing_guidance or [])):
        s = str(item or "").strip()
        if not s:
            continue
        if s.startswith(PACKET_PREFIX) or "working packet" in s or "working memory" in s:
            guide = _clip(s, 400)
            break
    if not guide and standing_guidance:
        guide = _clip(str(standing_guidance[-1]), 240)

    facts_s = "\n".join(f"- {f}" for f in facts) if facts else "- (none yet)"
    opens_s = "\n".join(f"- {g}" for g in opens) if opens else "- (none)"
    flags_s = ", ".join(flags) if flags else "(none)"

    return (
        f"{PACKET_PREFIX}\n"
        f"Challenge: {name}" + (f" [{cat}]" if cat else "") + "\n"
        f"Brief: {desc or '(none)'}\n"
        f"Flags captured: {flags_s}\n"
        f"Working facts (max {fact_n}, newest/relevant first):\n{facts_s}\n"
        f"Open intents (max {intent_n}):\n{opens_s}\n"
        f"Operator/WM note: {guide or '(none)'}\n"
        "FORBIDDEN: re-propose whole-challenge bootstrap; ignore Known facts; "
        "dump long tool transcripts into the plan.\n"
        "REQUIRED: ONE concrete next experiment that cites a Working fact or "
        "Named artifact and states the expected observable."
    )


__all__ = [
    "DEFAULT_MAX_FACTS",
    "DEFAULT_MAX_INTENTS",
    "ENV_FLAG",
    "ENV_MAX_FACTS",
    "ENV_MAX_INTENTS",
    "PACKET_PREFIX",
    "enabled",
    "fold_reason_context",
    "max_facts",
    "max_intents",
]
