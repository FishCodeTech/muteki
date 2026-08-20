"""Solo-depth + periodic verify/harvest gate (NYU A/B round 16+).

Structural break from the failed fruitless-interrupt stack
-----------------------------------------------------------
Interrupt stacking cancels long workers, then injects textual packets so Reason
re-proposes intents — a **scheduler patch** on multi-worker swarm tax. This
module instead keeps a **single deep worker** alive and periodically folds
tool outputs into the shared graph (deterministic harvest) **without cancel**.
Progress is "verify into memory", not "kill and replan".

Default OFF: ``MUTEKI_SOLO_DEPTH_VERIFY=1`` to enable.
When enabled, fruitless interrupt should stay OFF (runner enforces).
No promotion authority.
"""

from __future__ import annotations

import os
import time
from typing import Any, Final

ENV_FLAG: Final = "MUTEKI_SOLO_DEPTH_VERIFY"
ENV_PERIOD_S: Final = "MUTEKI_SOLO_DEPTH_VERIFY_PERIOD_S"
ENV_MIN_NEW_TOOLS: Final = "MUTEKI_SOLO_DEPTH_VERIFY_MIN_NEW_TOOLS"

DEFAULT_PERIOD_S: Final = 90.0
DEFAULT_MIN_NEW_TOOLS: Final = 3


def enabled(flag: bool | None = None) -> bool:
    if flag is not None:
        return bool(flag)
    raw = (os.environ.get(ENV_FLAG) or "").strip().lower()
    return raw in {"1", "true", "yes", "on"}


def period_seconds() -> float:
    raw = (os.environ.get(ENV_PERIOD_S) or "").strip()
    try:
        val = float(raw) if raw else DEFAULT_PERIOD_S
    except ValueError:
        val = DEFAULT_PERIOD_S
    return max(20.0, val)


def min_new_tools() -> int:
    raw = (os.environ.get(ENV_MIN_NEW_TOOLS) or "").strip()
    try:
        val = int(raw) if raw else DEFAULT_MIN_NEW_TOOLS
    except ValueError:
        val = DEFAULT_MIN_NEW_TOOLS
    return max(1, val)


def max_ordinary_workers() -> int:
    """Solo-depth: at most one ordinary (non-review) worker."""
    return 1 if enabled() else 10**9


def should_run_verify_gate(
    *,
    now_mono: float,
    last_verify_mono: float,
    tools_now: int,
    tools_at_last_verify: int,
    period_s: float | None = None,
    min_tools: int | None = None,
) -> bool:
    """True when enough wall time elapsed AND worker produced new tool calls."""
    if not enabled():
        return False
    period = period_seconds() if period_s is None else max(20.0, float(period_s))
    need = min_new_tools() if min_tools is None else max(1, int(min_tools))
    if float(now_mono) - float(last_verify_mono) < period:
        return False
    return int(tools_now) - int(tools_at_last_verify) >= need


def run_live_verify_harvest(
    solver: Any,
    shared_graph: Any,
    *,
    named_artifacts: list[str] | None,
    actor: str = "cli-claude",
) -> dict[str, Any]:
    """Harvest Named-artifact tool outputs into the graph without canceling.

    Reuses fruitless-interrupt harvest helpers but tags source as solo-depth.
    """
    from muteki.swarm.fruitless_interrupt_v1 import (
        collect_named_artifacts,
        commit_harvested_facts,
        harvest_artifact_tool_facts,
    )

    arts = list(named_artifacts or [])
    if not arts:
        arts = collect_named_artifacts(shared_graph)
    # Temporarily allow harvest even when interrupt harvest env is off.
    prev = os.environ.get("MUTEKI_FRUITLESS_INTERRUPT_HARVEST")
    prev_fi = os.environ.get("MUTEKI_FRUITLESS_INTERRUPT")
    try:
        os.environ["MUTEKI_FRUITLESS_INTERRUPT"] = "1"
        os.environ["MUTEKI_FRUITLESS_INTERRUPT_HARVEST"] = "1"
        rows = harvest_artifact_tool_facts(solver, arts, limit=6)
    finally:
        if prev is None:
            os.environ.pop("MUTEKI_FRUITLESS_INTERRUPT_HARVEST", None)
        else:
            os.environ["MUTEKI_FRUITLESS_INTERRUPT_HARVEST"] = prev
        if prev_fi is None:
            os.environ.pop("MUTEKI_FRUITLESS_INTERRUPT", None)
        else:
            os.environ["MUTEKI_FRUITLESS_INTERRUPT"] = prev_fi

    # Drop rows already present on the board (periodic gate is idempotent).
    existing: set[str] = set()
    try:
        snap = shared_graph.snapshot() if shared_graph is not None else None
        facts = []
        if isinstance(snap, dict):
            facts = snap.get("facts") or snap.get("evidence") or []
        elif snap is not None:
            facts = getattr(snap, "facts", None) or getattr(snap, "evidence", None) or []
        for item in facts or []:
            if isinstance(item, dict):
                text = str(item.get("text") or item.get("fact") or "")
            else:
                text = str(getattr(item, "text", None) or item or "")
            if text:
                existing.add(text[:200].lower())
    except Exception:
        existing = set()
    fresh = [
        row for row in rows
        if str(row.get("fact") or "")[:200].lower() not in existing
    ]
    for row in fresh:
        row["source_tag"] = "solo_depth_verify_harvest"

    seqs = commit_harvested_facts(
        shared_graph, actor=actor or "cli-claude", rows=fresh
    )
    return {
        "harvested": len(seqs),
        "fact_seqs": seqs,
        "artifacts": [str(r.get("artifact") or "") for r in fresh],
        "checks": [str(r.get("check") or "") for r in fresh],
        "skipped_dupes": max(0, len(rows) - len(fresh)),
        "ts": time.time(),
    }


__all__ = [
    "DEFAULT_MIN_NEW_TOOLS",
    "DEFAULT_PERIOD_S",
    "ENV_FLAG",
    "ENV_MIN_NEW_TOOLS",
    "ENV_PERIOD_S",
    "enabled",
    "max_ordinary_workers",
    "min_new_tools",
    "period_seconds",
    "run_live_verify_harvest",
    "should_run_verify_gate",
]
