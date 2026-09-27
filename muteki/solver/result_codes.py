"""Shared result vocabulary for solver intent conclusions."""

from __future__ import annotations

RESULT_SOLVED = "solved"
RESULT_TIMED_OUT = "timed_out"
RESULT_CANCELLED = "cancelled"
RESULT_OOM = "oom"
RESULT_OUTPUT_LIMIT = "output_limit"
RESULT_DISK_LIMIT = "disk_limit"
RESULT_STEERED = "steered"
RESULT_DEAD_END = "dead_end"
RESULT_EXPLORED = "explored"
RESULT_ROUTE_SUPPRESSED = "route_suppressed"
RESULT_SUPERSEDED = "superseded"
RESULT_LANE_DEFERRED = "lane_deferred"
RESULT_LANE_BLOCKED = "lane_blocked"
RESULT_CLOSED_BY_SOLVE = "closed_by_solve"
RESULT_REVIEWED = "reviewed"
RESULT_HANDOFF_MISSING = "handoff_missing"

GENUINE_GIVEUP_CODES = frozenset({RESULT_DEAD_END})

TRANSIENT_CODES = frozenset({
    RESULT_TIMED_OUT,
    RESULT_CANCELLED,
    RESULT_OOM,
    RESULT_OUTPUT_LIMIT,
    RESULT_DISK_LIMIT,
    RESULT_STEERED,
    RESULT_ROUTE_SUPPRESSED,
    RESULT_SUPERSEDED,
    RESULT_LANE_DEFERRED,
    RESULT_LANE_BLOCKED,
    RESULT_CLOSED_BY_SOLVE,
})

NEUTRAL_CODES = frozenset({
    RESULT_EXPLORED,
    RESULT_REVIEWED,
    # The worker died before handing off its result: neither a genuine give-up
    # nor a transient stop — the direction itself was never adjudicated.
    RESULT_HANDOFF_MISSING,
})


def normalize_result_code(code: str) -> str:
    raw = (code or "").strip().lower()
    if ":" in raw:
        raw = raw.split(":", 1)[0].strip()
    return raw


def is_genuine_giveup(code: str) -> bool:
    return normalize_result_code(code) in GENUINE_GIVEUP_CODES


def is_transient(code: str) -> bool:
    return normalize_result_code(code) in TRANSIENT_CODES


def is_neutral(code: str) -> bool:
    return normalize_result_code(code) in NEUTRAL_CODES
