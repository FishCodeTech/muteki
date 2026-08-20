"""Stateless module-level helpers for the swarm coordinator.

Split out of ``swarm.py`` (code-health G1): the blackboard-skill link wiring, the
process-wide health-probe cache, and the control-failure classifier are
self-contained (no ``Swarm`` state) and are re-exported from ``swarm`` so existing
call sites (`muteki.swarm.swarm._ensure_blackboard_skill_links`,
`_health_cache_clear`, …) are unchanged.
"""

from __future__ import annotations

import os
from dataclasses import dataclass, field
from pathlib import Path
from typing import Optional

from muteki.solver.types import SolveOutcome


# P0 defect-4: max operator standing hints kept (LRU). The cumulative text is
# injected into EVERY new worker's prompt, so an unbounded list bloated it to the
# point claude empty-exited (~36k tokens). 8 recent hints is plenty of context.
_STANDING_MAX = 8
# M6: cap the outstanding operator-help asks. Deduped on (worker, need) at the sink,
# so this only bites when many DISTINCT blockers pile up on a long never-give-up run;
# bounding it keeps the awaiting_operator count honest and memory flat.
_PENDING_HELP_MAX = 16


class WorkerBudgetExhausted(RuntimeError):
    pass


class WorkerSpawnRejected(RuntimeError):
    """A worker spawn was rejected for a recoverable reason (no available profile
    for the engine/role) BEFORE any budget was consumed. Distinct from
    WorkerBudgetExhausted (a terminal run-level cap) — the coordinator skips this
    one spawn and keeps going, and crucially the spawn-count budget is NOT charged
    (the worker was never created). Spawn sites catch this and emit
    worker_spawn_rejected instead of crashing the loop on a bare RuntimeError."""
    pass


class RequiredContextUnavailable(WorkerSpawnRejected):
    """An exact operator continuation is temporarily not deliverable.

    The intent must remain open, but it must not head-of-line block unrelated
    intents while its secret/context dependency is repaired.
    """
    pass


class ControlShutdownIncomplete(RuntimeError):
    """A fenced control handler ignored shutdown cancellation and still owns state.

    Callers must retain the Swarm/runtime owner and must not finalize its graph until
    the orphan set exits. This is intentionally distinct from a routine cancellation.
    """
    pass


@dataclass
class SwarmOutcome:
    solved: bool
    flag: Optional[str]
    winner: Optional[str]  # solver_id that found the flag
    per_solver: dict[str, SolveOutcome] = field(default_factory=dict)
    reason: str = ""
    # multi-flag: every distinct flag the run collected (flag stays the first).
    flags: list[str] = field(default_factory=list)


_CONTAINER_BLACKBOARD_SKILL = "/opt/muteki/muteki-blackboard"
_BLACKBOARD_SKILL_LINKS = (
    ".claude/skills/muteki-blackboard",
    ".agents/skills/muteki-blackboard",
    ".codex/skills/muteki-blackboard",
    ".cursor/skills-cursor/muteki-blackboard",
    ".cursor/skills/muteki-blackboard",
)


def _ensure_blackboard_skill_links(home: Path) -> None:
    """Compatibility no-op.

    Container and local Workers now receive project-local skill projections in
    their cwd.  Keeping the isolated HOME free of Muteki-managed skills prevents a
    resumed agent session from inheriting framework-specific behavior globally.
    """
    del home


# ── shared health-probe cache ────────────────────────────────────────────────
# `Swarm._healthy_engines` shells a REAL one-turn CLI hello per engine on EVERY
# dispatch (subprocess.run, up to a 60s/150s timeout + a retry, run SERIALLY).
# That whole-roster probe sits on the critical path BEFORE the first worker spawns
# and the first RUN_STARTED reaches the deck — so a fresh dispatch "freezes for ~a
# minute" with the rail stuck on WORKER 0/0 until it returns.
#
# This module-level cache memoizes the (ok, detail) verdict per probe-identity
# (engine + role + resolved account) for a short TTL, so a SECOND dispatch — or a
# sibling run in the same server, or a re-bootstrap round — reuses the roster we
# JUST verified instead of re-shelling every CLI. A successful probe is the strong
# signal (auth+quota+backend all round-tripped seconds ago); a FAILURE is cached
# too but for a shorter window so a recovered engine rejoins quickly. monotonic
# clock only (Date.now is banned in this codebase). Keyed process-wide so it
# survives across Swarm instances; bounded by natural roster size (a handful of
# engines × roles), so no eviction needed.
_HEALTH_PROBE_CACHE: dict[tuple, "tuple[float, bool, str]"] = {}
# failures expire faster than successes: a transiently-unhealthy engine (cold
# binary, jittery websocket) should get re-probed soon, while a healthy verdict can
# coast the full TTL.
_HEALTH_FAILURE_TTL_FRACTION = 0.25


def _health_cache_get(key: tuple, ttl: float, now: float) -> "tuple[bool, str] | None":
    """Return the cached (ok, detail) for `key` if still fresh, else None. A failed
    verdict expires at a fraction of the TTL so recovery is detected quickly."""
    hit = _HEALTH_PROBE_CACHE.get(key)
    if hit is None:
        return None
    stamped, ok, detail = hit
    horizon = ttl if ok else ttl * _HEALTH_FAILURE_TTL_FRACTION
    if now - stamped > horizon:
        return None
    return ok, detail


def _health_cache_put(key: tuple, ok: bool, detail: str, now: float) -> None:
    _HEALTH_PROBE_CACHE[key] = (now, ok, detail)


def _health_cache_clear() -> None:
    """Drop every cached verdict. Used by tests so a stubbed roster never leaks
    across cases, and available to callers that know auth just changed."""
    _HEALTH_PROBE_CACHE.clear()


def _is_control_failure(exc: BaseException) -> bool:
    """True if `exc` is a Runtime Control Plane failure (the rcp supervisor died or
    its reverse link dropped mid-worker) — surfaced as a ControlError from
    control_client. Matched by class name to avoid importing control_client here
    (and to catch it however it's wrapped). Such failures are runtime_degraded, not
    ordinary worker crashes."""
    for e in (exc, getattr(exc, "__cause__", None), getattr(exc, "__context__", None)):
        if e is not None and type(e).__name__ == "ControlError":
            return True
    return False
