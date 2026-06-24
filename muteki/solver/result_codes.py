"""Shared result vocabulary for solver intent conclusions."""

from __future__ import annotations

RESULT_SOLVED = "solved"
RESULT_TIMED_OUT = "timed_out"
RESULT_CANCELLED = "cancelled"
RESULT_OOM = "oom"
RESULT_STEERED = "steered"
RESULT_DEAD_END = "dead_end"
RESULT_EXPLORED = "explored"
RESULT_ROUTE_SUPPRESSED = "route_suppressed"
RESULT_SUPERSEDED = "superseded"
RESULT_LANE_DEFERRED = "lane_deferred"
RESULT_LANE_BLOCKED = "lane_blocked"
RESULT_CLOSED_BY_SOLVE = "closed_by_solve"
RESULT_REVIEWED = "reviewed"

GENUINE_GIVEUP_CODES = frozenset({RESULT_DEAD_END})

TRANSIENT_CODES = frozenset({
    RESULT_TIMED_OUT,
    RESULT_CANCELLED,
    RESULT_OOM,
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
