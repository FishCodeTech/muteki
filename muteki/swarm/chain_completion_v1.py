"""Env-gated chain-completion / progress-brief force for the coordinator.

Baseline NYU A/B (DeepSeek, 10 file-based challenges) showed failed runs share
one shape: a single bootstrap intent, many tools, zero ``fact_added`` events,
Reason suppressed because ``reason_state`` was unchanged after a fruitless
worker.  Enabling ``MUTEKI_CHAIN_COMPLETION=1`` forces a Reason cycle after a
fruitless reap and injects a short progress brief into standing guidance so the
planner must propose a *new* follow-up rather than go silent.

Default OFF.  No promotion authority.
"""

from __future__ import annotations

import os
from typing import Any, Final

ENV_FLAG: Final = "MUTEKI_CHAIN_COMPLETION"
BRIEF_PREFIX: Final = "[chain-completion progress brief]"


def enabled() -> bool:
    return os.environ.get(ENV_FLAG, "").strip() in {"1", "true", "TRUE", "yes"}


def build_progress_brief(
    *,
    fact_count: int,
    flag_count: int,
    fruitless_workers: int,
    open_intents: int,
    last_goals: list[str] | None = None,
) -> str:
    goals = [g.strip() for g in (last_goals or []) if g and g.strip()]
    tried = "; ".join(goals[:3]) if goals else "(none recorded)"
    return (
        f"{BRIEF_PREFIX} verified_facts={fact_count} flags={flag_count} "
        f"fruitless_workers={fruitless_workers} open_intents={open_intents}. "
        f"Recently attempted goals (do NOT paraphrase): {tried}. "
        "The previous worker finished without new verified facts. "
        "FORBIDDEN: whole-challenge bootstrap / 'solve the challenge' paraphrases. "
        "REQUIRED: ≥1 NEW concrete discriminating experiment naming ONE "
        "artifact/hypothesis/check (≤20 words). Prefer verify/falsify over "
        "broad explore."
    )


def should_force_reason(
    *,
    just_reaped: bool,
    slots_free: bool,
    graph_grew: bool,
    flag_count: int,
    need_reason_already: bool,
    open_intents: int = 0,
) -> bool:
    if not enabled():
        return False
    if need_reason_already:
        return False
    if not just_reaped or not slots_free:
        return False
    if open_intents > 0:
        # Queue still has work — do not pile another Reason cycle.
        return False
    if graph_grew:
        return False
    if flag_count > 0:
        return False
    return True


def recent_concluded_goals(shared_graph: Any, *, limit: int = 3) -> list[str]:
    if shared_graph is None:
        return []
    try:
        rows = shared_graph.barren_concluded_goal_texts()
    except Exception:
        rows = []
    out: list[str] = []
    for goal in rows or []:
        text = str(goal or "").strip()
        if text and text not in out:
            out.append(text)
        if len(out) >= limit:
            break
    if out:
        return out
    # Fallback: open/done intents on the board.
    try:
        intents = shared_graph.list_intents()  # type: ignore[attr-defined]
    except Exception:
        try:
            snap = shared_graph.snapshot()
            intents = snap.get("intents") if isinstance(snap, dict) else []
        except Exception:
            intents = []
    for intent in intents or []:
        if not isinstance(intent, dict):
            continue
        text = str(intent.get("goal") or "").strip()
        if text and text not in out:
            out.append(text)
        if len(out) >= limit:
            break
    return out


__all__ = [
    "BRIEF_PREFIX",
    "ENV_FLAG",
    "build_progress_brief",
    "enabled",
    "recent_concluded_goals",
    "should_force_reason",
]
