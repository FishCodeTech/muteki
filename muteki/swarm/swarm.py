"""L1 parallel solver swarm (§5).

Race N solvers on the SAME challenge. The first one to produce a provenance-
verified, correctly-formatted flag wins; the rest are cancelled immediately
(first-valid-flag-wins, §5.1). Solvers share verified facts + dead-ends through
the InsightBus (§5.3) so the swarm behaves as "any model can solve" -> "the
group solves", not N isolated attempts.

Heterogeneity (different models/temperatures, §5.2) means their blind spots
don't overlap; the Insight Bus means a fact one solver confirms accelerates the
others. The orchestration here is deliberately thin — the design doc is explicit
that the real edge is per-Solver cognition, not the racing harness.
"""

from __future__ import annotations

import asyncio
import hashlib
import json
import os
import re
from dataclasses import dataclass, field
from pathlib import Path
from typing import TYPE_CHECKING, Any, Optional

if TYPE_CHECKING:
    from muteki.learning.distill import TemplateStore

from muteki.core.cost import CostController
from muteki.core.event_bus import EventBus
from muteki.core.runtime_env import is_web_container
from muteki.core.events import Event, EventType, blackboard_delta_payload
from muteki.core.llm import LLMClient, ModelSpec
from muteki.models.solve_graph import Challenge
from muteki.sandbox.manager import SandboxManager
from muteki.solver.result import ArtifactStore
from muteki.solver.types import SolverConfig, SolveOutcome
from muteki.solver.credential_accounts import runtime_env_for_engine
from muteki.solver.worker_profiles import (
    base_engine_for_profile,
    coerce_nonneg_int,
    normalize_profile_roster,
    normalize_worker_profiles,
    profile_names,
)
from muteki.solver.workspace import cleanup_worker_scratch, ensure_workspace
from muteki.swarm.insight_bus import InsightBus
from muteki.swarm.stage_policy import StagePolicy
from muteki.swarm.shared_graph import SharedGraph, SQLiteSharedGraph, canonicalize_lane

# P0 defect-4: max operator standing hints kept (LRU). The cumulative text is
# injected into EVERY new worker's prompt, so an unbounded list bloated it to the
# point claude empty-exited (~36k tokens). 8 recent hints is plenty of context.


_STANDING_MAX = 8
# M6: cap the outstanding operator-help asks. Deduped on (worker, need) at the sink,
# so this only bites when many DISTINCT blockers pile up on a long never-give-up run;
# bounding it keeps the awaiting_operator count honest and memory flat.
_PENDING_HELP_MAX = 16

_CONTAINER_BLACKBOARD_SKILL = "/opt/muteki/muteki-blackboard"
_BLACKBOARD_SKILL_LINKS = (
    ".claude/skills/muteki-blackboard",
    ".agents/skills/muteki-blackboard",
    ".codex/skills/muteki-blackboard",
    ".cursor/skills-cursor/muteki-blackboard",
    ".cursor/skills/muteki-blackboard",
)


def _ensure_blackboard_skill_links(home: Path) -> None:
    """Expose the image-baked blackboard skill inside an isolated worker HOME."""
    for rel in _BLACKBOARD_SKILL_LINKS:
        link = home / rel
        try:
            if link.is_symlink():
                if os.readlink(link) == _CONTAINER_BLACKBOARD_SKILL:
                    continue
                link.unlink()
            elif link.exists():
                continue
            link.parent.mkdir(parents=True, exist_ok=True)
            link.symlink_to(_CONTAINER_BLACKBOARD_SKILL, target_is_directory=True)
        except OSError:
            continue

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


@dataclass
class SwarmOutcome:
    solved: bool
    flag: Optional[str]
    winner: Optional[str]  # solver_id that found the flag
    per_solver: dict[str, SolveOutcome] = field(default_factory=dict)
    reason: str = ""
    # multi-flag: every distinct flag the run collected (flag stays the first).
    flags: list[str] = field(default_factory=list)


class Swarm:
    """Runs a lineup of solvers against one challenge, first-valid-flag wins."""

    def __init__(
        self,
        challenge: Challenge,
        lineup: list[ModelSpec],
        *,
        llm: LLMClient,
        sandbox: SandboxManager,
        bus: Optional[EventBus] = None,
        cost: Optional[CostController] = None,
        artifacts: Optional[ArtifactStore] = None,
        config: Optional[SolverConfig] = None,
        run_id: Optional[str] = None,
        knowledge: Optional["TemplateStore"] = None,
        hitl_inbox: "Optional[asyncio.Queue]" = None,
        # operator worker commands (spawn/kill a specific engine on demand). The
        # coordinator loop drains this each tick. None → no runtime worker control.
        worker_cmds: "Optional[asyncio.Queue]" = None,
        executor: str = "cli",
        cli_engine: str = "claude",
        cli_race: bool = False,
        # the engine roster this swarm may use (race + coordinator pick from it,
        # filtered by healthcheck). Default keeps the historical claude+codex pair
        # so existing tests/behavior are unchanged; the web driver passes the full
        # ["cursor","claude","codex"] roster for a three-engine race.
        engines: "Optional[list[str]]" = None,
        web_access: bool = True,
        kb: bool = True,
        coordinator: bool = False,
        graph_dir: "Optional[Path]" = None,
        worker_root: "Optional[Path]" = None,
        max_workers: int = 10,
        start_workers: int = 2,
        reason_model: str = "deepseek-v4-pro",
        stall_seconds: float = 120.0,  # retained for back-compat; no longer used to
        #   reclaim workers (see _run_coordinator note). Safe to ignore.
        # how many NEW explore workers the coordinator may spawn per loop iteration.
        # 1 = smooth ramp (a slot refills within one ~2s poll anyway); higher values
        # re-introduce the "spawn a burst that shares a fate" problem (run-7352).
        explore_spawn_batch: int = 1,
        # per-turn timeout (s) for an EXPLORE worker's turn-1. Short, because an
        # explore is a narrow single-intent probe — this is the ONLY backstop that
        # frees a slot held by a stuck explore (replacing the old stall-kill).
        # bootstrap/retry keep the long default (whole-challenge rush).
        explore_timeout: int = 720,
        # no-progress backpressure, ALL modes: after this many CONSECUTIVE worker
        # completions with NO new fact (incl. candidates) and NO new flag, the
        # coordinator soft-PAUSES for the operator instead of burning tokens
        # forever. Formerly collect_barren_limit, collect-mode-only and counted
        # idle re-bootstrap rounds — which left single-flag and known-count
        # chained runs (run-11189: expected_flags=15) with NO spend cap, and
        # lived in the fully-idle branch so an intent-churn spike (run-11190:
        # 238 workers) never even reached it. Counting fruitless WORKERS at reap
        # time catches both shapes. Soft pause: no worker kill, any operator
        # input resumes. Generous default — late-stage exploit grinding can
        # legitimately go several workers without a new fact. 0 disables.
        barren_limit: int = 8,
        # NO time limit by default: a CTF challenge has a guaranteed unique
        # solution, so the swarm must NEVER give up on its own — it keeps spawning
        # fresh attempts until it solves or the operator stops it. A clean/offline
        # eval can still cap this by passing a finite budget.
        wall_clock_budget: float = float("inf"),
        # ── race-scout layer (DESIGN_race_scout_layer.md) ────────────────────
        # A one-round, multi-engine, SINGLE-SHOT race in front of the coordinator
        # loop: 3 fresh single-shot bootstrap workers (claude/codex/cursor) probe the
        # whole challenge in parallel. If any captures the flag → fast path (skip the
        # coordinator loop). Else their facts land on the shared graph and the
        # coordinator takes over warm, not from an empty graph. All bounds are
        # configurable; race_scout=False is byte-identical to the plain coordinator.
        race_scout: bool = True,                       # whole-layer on/off
        race_engines: "Optional[list[str]]" = None,    # which engines race (None = all)
        race_timeout: int = 720,                       # short timeout (breadth recon, not deep dig)
        race_rounds: int = 1,                          # one round (>1 reintroduces accumulation)
        # cold-start signal (run-75379 BUG④). race-scout is a cold-start warmup for an
        # EMPTY graph; on a reopen/resume of a populated graph (33+ verified facts) it
        # re-races a solved challenge and burns fresh workers. Callers that relaunch on
        # an existing graph_dir (web `resolve`, a standby restart) should pass
        # cold_start=False. Default True keeps fresh runs byte-identical. This is only
        # the EXPLICIT hint — the coordinator ALSO falls back to a graph-state check
        # (_is_cold_start), so a relaunch that forgets to set this is still protected.
        cold_start: bool = True,
        # ── worker execution backend ─────────────────────────────────────────
        # "local"  → workers shell out on the HOST (default; unchanged).
        # "container" → workers run inside the run's isolated Docker execution
        #   node, which mounts ONLY the run workspace and account-scoped credential
        #   material. The image is tool-only; credentials are injected at runtime.
        worker_backend: str = "local",
        runtime_profiles: "Optional[list[dict]]" = None,
        worker_profiles: "Optional[list[dict]]" = None,
        credential_accounts_root: "Optional[Path]" = None,
        stage_policy: "Optional[dict[str, Any] | StagePolicy]" = None,
        max_total_workers: "Optional[int]" = None,
        cost_budget_usd: "Optional[float]" = None,
        llm_profiles: "Optional[dict[str, Any]]" = None,
    ) -> None:
        self.challenge = challenge
        self.lineup = lineup
        self.llm = llm
        self.sandbox = sandbox
        self.bus = bus
        self.cost = cost
        self.artifacts = artifacts
        self.config = config
        self.run_id = run_id or challenge.id
        # executor: vestigial knob (CLI is the only path now — shelled claude/codex
        # agentic workers). Kept for call-site compatibility; always builds
        # CliSolvers. The moat is the provenance gate + shared_graph + reason.
        self.executor = executor
        self.cli_engine = cli_engine
        # cli_race: heterogeneous CLI swarm — claude AND codex race the SAME
        # challenge, first to pass the provenance gate wins (their blind spots
        # don't overlap → higher solve rate). Degrades to single-CLI when one
        # engine's healthcheck fails (e.g. codex usage-limited).
        self.cli_race = cli_race
        self.stage_policy = StagePolicy.from_config(stage_policy)
        self.llm_profiles = dict(llm_profiles or {})
        self.review_policy = self._clean_review_policy(
            self.stage_policy.coordinator.get("review"))
        self._last_review_seq = 0
        self._last_review_proposal_seq = 0
        self._last_directive_seq = 0
        # E: last resource-lock event seq surfaced as a board delta (workers acquire
        # locks directly via the blackboard skill; the coordinator mirrors them to UI).
        self._last_resource_seq = 0
        self._active_review_tasks: set[asyncio.Task] = set()
        self._review_workers_spawned = 0
        self._queued_review_requests: list[dict[str, str]] = []
        self._pending_uncertainty_reviews: list[dict[str, Any]] = []
        self._completed_workers_since_review = 0
        self._last_candidate_review_count = 0
        if self.stage_policy.race:
            if "enabled" in self.stage_policy.race:
                race_scout = bool(self.stage_policy.race["enabled"])
            if self.stage_policy.race.get("engines") is not None:
                race_engines = list(self.stage_policy.race.get("engines") or [])
            if self.stage_policy.race.get("timeout") is not None:
                race_timeout = int(self.stage_policy.race["timeout"])
        # 0 (or unset) means UNLIMITED, matching max_total_workers / cost_budget_usd
        # below and the drivers.py convention (0 ⇄ inf). A bare `is not None` here
        # used to turn a 0 budget into a literal 0s deadline → instant
        # budget_exhausted. Only a POSITIVE value caps the wall clock.
        _scb = self.stage_policy.coordinator.get("wall_clock_budget")
        if _scb is not None:
            wall_clock_budget = float(_scb) if float(_scb) > 0 else float("inf")
        if self.llm_profiles.get("planner", {}).get("model"):
            reason_model = str(self.llm_profiles["planner"]["model"])
        # short-task model for hand-raise translation (and any future cheap zh helper):
        # the configured titler, else the planner, else the summarizer's flash default.
        self.titler_model = (
            str(self.llm_profiles.get("titler", {}).get("model") or "")
            or str(self.llm_profiles.get("planner", {}).get("model") or "")
            or "deepseek-v4-flash")
        self.max_total_workers = (
            int(max_total_workers) if max_total_workers not in (None, 0) else
            self.stage_policy.budgets.max_total_workers
        )
        self.cost_budget_usd = (
            float(cost_budget_usd) if cost_budget_usd not in (None, 0) else
            self.stage_policy.budgets.cost_budget_usd
        )
        self._spawned_total = 0
        self._budget_exhausted_kind: str | None = None
        self.worker_profiles = self._clean_worker_profiles(worker_profiles)
        self.runtime_profiles = self._clean_runtime_profiles(runtime_profiles)
        # engine roster (deduped) — now profile names. Legacy values like "claude"
        # expand to every enabled claude profile.
        if self.worker_profiles:
            roster = normalize_profile_roster(engines, self.worker_profiles) if engines else []
            self.engines = roster or profile_names(self.worker_profiles)
            # Order the roster by each profile's (priority, name). The dispatcher
            # (_pick_engine → _healthy_role_candidates) walks self.engines in order
            # and prefers the first not-currently-running candidate, so roster
            # ORDER == dispatch preference. Without this sort the priority field is
            # dead on the dispatch path (the roster kept its assembly order, which
            # for an explicit profile-name list is just declaration order). Sorting
            # here makes priority authoritative for BOTH classic-race lineup and
            # coordinator dispatch, and matches what the drag-drop composer writes
            # (top card = lowest priority number = picked first). Stable + total:
            # unknown names (defensive) sink to the end deterministically by name.
            # coerce_nonneg_int (NOT `priority or 100`): priority 0 is a legal,
            # MEANINGFUL value (highest precedence) reachable via hand-edited JSON /
            # API import — `0 or 100` would silently demote it to the default and
            # sink the top-priority profile. coerce also guards a non-int string.
            _prio = {str(p["name"]): (coerce_nonneg_int(p.get("priority"), 100), str(p["name"]))
                     for p in self.worker_profiles}
            self.engines.sort(key=lambda e: _prio.get(e, (10**9, e)))
        else:
            roster = engines if engines else ["claude", "codex"]
            seen: set[str] = set()
            self.engines = [e for e in roster if not (e in seen or seen.add(e))]
        self._profiles_by_name: dict[str, dict] = {p["name"]: p for p in self.worker_profiles}
        self._profiles_by_engine: dict[str, list[dict]] = {}
        for p in self.worker_profiles:
            self._profiles_by_engine.setdefault(p["engine"], []).append(p)
        for profiles in self._profiles_by_engine.values():
            profiles.sort(key=lambda p: (coerce_nonneg_int(p.get("priority"), 100), p["id"]))
        self._profile_rr: dict[str, int] = {}
        self._active_profile_by_solver: dict[str, str] = {}
        self._active_profile_role_by_solver: dict[str, str] = {}
        self._active_profile_counts: dict[str, int] = {}
        self._active_review_profile_counts: dict[str, int] = {}
        self._active_account_by_solver: dict[str, str] = {}
        # race-scout config. race_engines defaults to the full roster; pass a subset
        # to disable a worker (e.g. ["claude","codex"] drops cursor). Deduped, and
        # restricted to known engines so a typo can't silently launch nothing weird.
        self.race_scout = bool(race_scout)
        # explicit cold-start hint (see ctor arg + _is_cold_start). May also be
        # supplied via stage_policy.race["cold_start"] so config-driven relaunches
        # can flip it without a constructor kwarg.
        self.cold_start = bool(cold_start)
        if self.stage_policy.race and "cold_start" in self.stage_policy.race:
            self.cold_start = bool(self.stage_policy.race["cold_start"])
        self.race_timeout = int(race_timeout)
        self.race_rounds = max(1, int(race_rounds))
        _rseen: set[str] = set()
        if self.worker_profiles and race_engines is not None:
            _rroster = normalize_profile_roster(race_engines, self.worker_profiles)
        else:
            _rroster = race_engines if race_engines is not None else self.engines
        self.race_engines = [e for e in _rroster
                             if e in self.engines and not (e in _rseen or _rseen.add(e))]
        # web_access=False → workers run offline (no WebSearch/WebFetch) for a
        # clean bench eval. kb → let workers use the optional KB MCP
        # (MUTEKI_KB_MCP_NAME), if one is configured.
        self.web_access = web_access
        self.kb = kb
        # coordinator: evidence-driven loop (seed workers -> plan from graph ->
        # dispatch focused workers -> ...) with heterogeneity-aware dynamic worker
        # scaling and graph-change-driven planning. Off by
        # default so existing race behavior (and tests) are unchanged; the web driver
        # opts in.
        self.coordinator = coordinator
        # worker_root: a persistent per-run dir under which each CLI worker gets
        # its OWN cwd (worker_root/{solver_id}-{n}/) instead of a system $TMPDIR
        # mkdtemp. The web driver points this at sessions/{id}/workspace/workers/
        # so a run's worker scratch (staged attachments, agent-extracted files,
        # PoCs) lives under the run's folder — inspectable after the run and
        # cleaned up with it. None → fall back to mkdtemp (TUI / tests).
        self.worker_root = Path(worker_root) if worker_root is not None else None
        self.workspace_root = self.worker_root.parent if self.worker_root is not None else None
        if self.workspace_root is not None:
            ensure_workspace(self.workspace_root, runtime={
                "backend": worker_backend,
                "run_id": self.run_id,
            })
        self.credential_accounts_root = (
            Path(credential_accounts_root).expanduser().resolve()
            if credential_accounts_root is not None else None
        )
        # worker execution backend: "local" (host subprocess) or "container" (workers
        # run in the run's Kali tool container for a consistent toolchain). The
        # ContainerHandle is created lazily on first worker spawn (worker_root first).
        self.worker_backend = worker_backend
        self._container_handle = None  # set lazily by _container() when backend=container
        self._container_runtime_id = ""  # runtime profile id the container was built with (#11)
        self._container_unavailable = False
        self._runtime_degraded: list[dict[str, Any]] = []
        # engines dropped from the roster by a dispatch-time health-check failure
        # (e.g. cursor headless auth lapsed). engine -> reason. Used to dedup the
        # engine_degraded event (emit once per transition, not once per spawn).
        self._degraded_engines: dict[str, str] = {}
        # health-probe cache: each `_healthy_engines` call shells a REAL one-turn CLI
        # hello per engine (subprocess.run, up to a 60–150s timeout), which is what
        # made dispatch "freeze for ~a minute" before any worker spawned. Cache the
        # (ok, detail) verdict per probe-identity (engine + role + resolved account)
        # for a short TTL so back-to-back dispatches / re-bootstraps don't re-probe a
        # roster we just verified. Keyed on the SHARED process-wide cache below so
        # sibling runs in the same server reuse it too. monotonic clock only.
        self._health_probe_ttl = float(
            os.environ.get("MUTEKI_HEALTH_PROBE_TTL", "120") or 120)
        self._worker_seq = 0  # monotonic suffix so two workers never share a cwd
        # per-engine monotonic label counter → unique solver_id per spawn so the
        # deck draws one lane per worker (1st keeps the bare "cli-<engine>" id).
        self._label_seq: dict[str, int] = {}
        self.max_workers = max_workers
        self.start_workers = start_workers
        self.reason_model = reason_model
        self.stall_seconds = stall_seconds
        self.explore_spawn_batch = max(1, int(explore_spawn_batch))
        self.explore_timeout = int(explore_timeout)
        self.barren_limit = int(barren_limit)
        self.wall_clock_budget = wall_clock_budget
        self.knowledge = knowledge  # §16: recall prior + distill on solve
        # Persistent operator "standing" guidance (VPS/SSH creds, global constraints).
        # The coordinator holds the canonical list so EVERY worker — including ones
        # spawned AFTER the operator gave the hint — gets it injected into its turn-1
        # prompt. Before this, standing only reached a worker via its live InsightBus
        # inbox, which lands AFTER turn-1's prompt is already built (and many explore
        # workers finish in one turn), so late-spawned workers never saw the VPS hint.
        self._standing_guidance: "list[str]" = []
        # ── intent-level HITL (single-shot migration, M-3) ────────────────────
        # Workers are single-shot now (DESIGN_single_shot_migration.md): they don't
        # resume to absorb operator guidance mid-run. So a non-standing hint/redirect
        # can no longer steer a LIVE worker — it must reach the NEXT spawned one.
        # _target_redirect holds an operator-supplied new target URL (applied to every
        # subsequent worker); _next_worker_guidance holds one-shot hint/redirect text
        # consumed by the next _make_cli_worker spawn, then cleared. This is the
        # accepted granularity degrade: turn-level live steering → intent-level.
        self._target_redirect: "Optional[str]" = None
        self._next_worker_guidance: "list[str]" = []
        # ── operator-blocked state (worker raised its hand / env down) ────────
        # When a worker emits a HITL_REQUEST (NEED_INPUT / env_down), the coordinator
        # stops re-spawning that dead-end direction and WAITS for the operator instead
        # of burning tokens retrying a blocker no agent can clear (no VPS, expired
        # target). _pending_help holds the outstanding asks; _operator_event is set by
        # _drain_hitl on ANY operator command, which unblocks the wait.
        self._pending_help: "list[dict]" = []
        # M11: idempotency guard so the coordinator's run finalization (persist winner +
        # close shared_graph + RUN_FINISHED + worker-dir cleanup) runs EXACTLY once,
        # whether the loop returns normally OR is cancelled/errors out through the finally.
        self._run_finalized = False
        # L3: bus sinks the coordinator added (help / submit-gate), detached on finalize
        # so a reused bus (standby/resolve restart re-entering the coordinator) doesn't
        # accumulate Swarm-closing sinks across cycles.
        self._coord_sinks: "list" = []
        self._operator_event: "Optional[asyncio.Event]" = None
        # operator STOP: a `stop`/`complete` HITL command ends the coordinator loop
        # gracefully (distinct from a steer, which only guides workers). Needed for
        # challenges that never yield a gated flag — without it the "never give up"
        # re-bootstrap runs forever (run-10070: 74 workers on an already-solved box).
        self._operator_stop: bool = False
        # operator PAUSE (#5): a `pause` HITL command SOFT-pauses the coordinator —
        # it stops spawning NEW workers and waits, but does NOT terminate the run
        # (distinct from stop). `resume` clears it. This is the meaningful "pause" for
        # a single-shot architecture (freezing one about-to-exit worker is near
        # worthless); the operator's intent is "stop burning budget on new workers
        # while I look / wait". The wait reuses _operator_event (set by any command).
        self._operator_paused: bool = False
        self.insight = InsightBus(challenge.id)
        # HITL: a queue the frontend posts human commands onto (hint/redirect/
        # pause/resume, scoped global or per-solver). A background task drains it
        # into insight.guidance() so the broadcast reaches every solver's inbox.
        self.hitl_inbox = hitl_inbox
        self.worker_cmds = worker_cmds
        # P-A: ONE shared, event-sourced, evidence-bearing graph for the swarm.
        # InsightBus stays the write-NOTIFY channel; this is the persistent
        # global state every solver writes to (and reason/flywheel read from).
        self.shared_graph: Optional[SharedGraph] = None
        try:
            # graph_dir (web driver) keeps the DB OUTSIDE sandbox.root so it
            # survives sandbox.shutdown_all()'s rmtree of the sandbox root. Falls
            # back to the sandbox tree when unset (TUI / tests, where ephemeral
            # is fine).
            if graph_dir is not None:
                base = Path(graph_dir)
                base.mkdir(parents=True, exist_ok=True)
                db_path = base / "shared_graph.db"
            else:
                db_path = self.sandbox.root / self.run_id / "shared_graph.db"
            # remember where durable per-run state lives (sibling of graph/) so a
            # post-solve standby can find winner.json + the shared graph again.
            self._graph_dir = Path(graph_dir) if graph_dir is not None else None
            self.shared_graph = SQLiteSharedGraph.open(
                db_path=db_path, challenge=challenge, artifacts=artifacts,
            )
        except Exception:
            # shared graph is additive — never block solving if it can't open.
            self.shared_graph = None
        self._last_graph_event_seq = 0
        self._graph_bridge_failures: dict[int, int] = {}
        # multi-flag: the authoritative dedup set of flags collected so far. The
        # run is "solved" once it holds expected_flags distinct flags. For a
        # single-flag challenge (expected_flags=1) the first flag fills it and
        # _flags_complete() flips true immediately — byte-identical to the old
        # "first flag wins" behaviour.
        self._found_flags: list[str] = []

    @staticmethod
    def _clean_review_policy(value: Any) -> dict[str, Any]:
        configured = isinstance(value, dict)
        raw = value if configured else {}
        defaults = {
            "enabled": configured,
            "engine": "",
            "after_race": True,
            "after_fruitless_workers": 3,
            "after_duplicate_intents": 2,
            "on_course_correct": True,
            "on_reason_dry": True,
            "on_candidate_spike": True,
            "on_operator_hint": True,
            "every_completed_workers": 6,
            "candidate_spike_threshold": 5,
            "max_concurrent": 1,
            "allow_review_fallback": False,
            "cooldown_events": 8,
            "timeout": 420,
            "max_review_workers": 12,
            "max_challenges_per_cycle": 8,
        }
        out = dict(defaults)
        for key in ("enabled", "after_race", "on_course_correct", "on_reason_dry",
                    "on_candidate_spike", "on_operator_hint", "allow_review_fallback"):
            if key in raw:
                out[key] = bool(raw.get(key))
        if raw.get("engine"):
            out["engine"] = str(raw.get("engine")).strip()
        for key in ("after_fruitless_workers", "after_duplicate_intents",
                    "every_completed_workers", "candidate_spike_threshold",
                    "max_concurrent", "max_challenges_per_cycle",
                    "cooldown_events", "timeout", "max_review_workers"):
            if key in raw:
                try:
                    out[key] = max(0, int(raw.get(key)))
                except (TypeError, ValueError):
                    pass
        return out

    def _expected_flags(self) -> int:
        return max(1, getattr(self.challenge, "expected_flags", 1) or 1)

    def _multi_flag(self) -> bool:
        return bool(getattr(self.challenge, "multi_flag", False))

    def _flags_complete(self) -> bool:
        """Is the run's flag objective met? This is the SAVE-vs-FINISH decoupling
        (run-10070): saving a flag (_record_flags) must not finish a collect-mode run
        the way it finishes a single-flag run.

        - single-flag (multi_flag=False, the default): `len >= expected_flags`, which
          with expected_flags=1 finishes on the first gated flag — byte-identical to
          the old behavior.
        - collect mode with a known count (multi_flag=True, expected_flags>1): finish
          once N distinct flags are collected.
        - collect mode with UNKNOWN count (multi_flag=True, expected_flags<=1): NEVER
          finish by count. Flags still save + display; the run ends only on operator
          STOP or the coordinator's no-progress pause. A saved flag is not a finish."""
        if self._multi_flag() and self._expected_flags() <= 1:
            return False
        return len(self._found_flags) >= self._expected_flags()

    def _record_flags(self, *flags: Optional[str]) -> list[str]:
        """Add flags to the dedup set; return the ones that were NEW (so the caller
        can broadcast each exactly once)."""
        fresh: list[str] = []
        for f in flags:
            if f and f not in self._found_flags:
                self._found_flags.append(f)
                fresh.append(f)
        return fresh

    def _sync_flags_from_graph(self) -> list[str]:
        """Reconcile the in-memory flag set with the AUTHORITATIVE shared-graph
        snapshot, returning the flags that were newly absorbed (for one-time
        broadcast). This is the fix for the run-75379 split-brain (BUG②).

        Every worker writes each accepted flag to the shared graph via _accept_flag
        → shared_graph.flag_found, and the graph snapshot is what the UI / planner /
        finalize already trust. But _found_flags (the in-memory list _flags_complete
        reads) is fed ONLY from reaped `outcome.flags`, so a flag that reached the
        graph via a path that never delivered a clean outcome — a worker cancelled
        after it accepted a flag (reaped as CancelledError, line ~3615), an
        error-reaped worker, or the live-broadcast/DB-bridge path — stays invisible
        to the completion check. In run-75379 the graph held 4 valid flags (5 found,
        1 operator-invalidated) while _found_flags was stuck at 2, so _flags_complete()
        never fired and the run spawned ~55 post-solve waves until operator stop.

        Reconciling against snapshot().flags makes the graph the single source of
        truth for completion:
          - ADD any flag the graph holds but _found_flags is missing.
          - DROP any flag the operator explicitly INVALIDATED (snapshot already
            excludes it), so a blacklisted false positive (e.g. 090099b7) can never
            count toward expected_flags (BUG③ cross-check).
        Absent-from-snapshot-but-not-invalidated flags are LEFT in place: a silent
        flag_found DB-write failure (the `except: pass` in _accept_flag) must not
        let a genuinely-held flag vanish from the count."""
        if self.shared_graph is None:
            return []
        try:
            graph_flags = list(getattr(self.shared_graph.snapshot(), "flags", []) or [])
            invalidated = self.shared_graph.invalidated_flags()
        except Exception:
            return []
        # DROP operator-invalidated flags from the in-memory set (and never let one
        # back in below). reopen_after_false_positive removes it from the snapshot
        # too, so this only matters for a flag already absorbed before invalidation.
        if invalidated:
            self._found_flags = [f for f in self._found_flags if f not in invalidated]
        # ADD any authoritative flag the in-memory set is missing.
        fresh = self._record_flags(*(f for f in graph_flags if f not in invalidated))
        return fresh

    def _engine_healthcheck_cached(self, name: str, role: str) -> bool:
        """bool liveness for one engine, served from the shared health-probe cache
        (same TTL as _healthy_engines) so the race path doesn't re-shell a CLI we
        just verified on the coordinator path (or a prior dispatch). On a miss it
        probes once and caches the verdict."""
        import time

        ttl = self._health_probe_ttl
        if ttl <= 0:
            return self._probe_engine_health(name, role)[0]
        now = time.monotonic()
        key = self._health_probe_key(name, role)
        cached = _health_cache_get(key, ttl, now)
        if cached is not None:
            return cached[0]
        ok, detail = self._probe_engine_health(name, role)
        _health_cache_put(key, ok, detail, now)
        return ok

    def _build_solvers(self) -> list:
        from muteki.solver.cli_driver import driver_for
        from muteki.solver.cli_solver import CliSolver

        def _healthy(name: str, role: str) -> bool:
            return self._engine_healthcheck_cached(name, role)

        if self.cli_race:
            # race the configured engine roster (heterogeneous). Keep only the
            # engines whose healthcheck passes; if none do, fall back to claude so
            # the swarm still runs. ONE worker per healthy engine — independent of
            # the lineup size — so they genuinely race the same challenge (the
            # lineup specs only supply solver_id labels, cycled).
            engines = [e for e in self.engines if _healthy(e, "race")]
            if not engines and not self.worker_profiles:
                engines = ["claude"]
        else:
            # single engine; degrade to claude if the chosen one is unhealthy.
            if _healthy(self.cli_engine, "bootstrap"):
                engines = [self.cli_engine]
            elif not self.worker_profiles:
                engines = ["claude"]
            else:
                engines = []

        # race mode → spec=None so each worker's id is cli-<engine> (distinct);
        # single mode → use the lineup spec so existing labels are preserved.
        specs = self.lineup or [None]
        # A race runs several solvers under one run; each solver's end is
        # worker-level (WORKER_FINISHED). _run_race emits the single run-level
        # RUN_FINISHED when the whole race settles, so 2 racers don't fire 2
        # run-level finishes (same conflation as the coordinator path).
        workers = []
        for i, engine in enumerate(engines):
            # resolve profile BEFORE charging budget (same #3 leak fix as
            # _make_cli_worker): a missing profile must `continue` WITHOUT having
            # incremented _spawned_total, else it leaks toward max_total_workers.
            role = "race" if self.cli_race else "bootstrap"
            profile = self._profile_for_engine(engine, role=role)
            if self.worker_profiles and profile is None:
                continue
            try:
                self._reserve_worker_spawn()
            except WorkerBudgetExhausted:
                break
            transport = base_engine_for_profile(profile or engine)
            # solver_id labelling differs by mode:
            #  - race mode: spec=None, so without help every same-base-engine racer
            #    (e.g. 3 codex profiles each pinned to a different model) collapses
            #    onto one solver_id "cli-codex" and their event lanes /
            #    _active_profile_by_solver / account release maps overwrite each
            #    other. Apply the same _label_seq scheme as _make_cli_worker (the
            #    classic cli_race path bypasses it): first worker of a base engine
            #    keeps the bare "cli-<engine>" id (winner bookkeeping / existing
            #    tests), the rest get "-2", "-3", … . This is the bug fix.
            #  - single mode: the lineup spec already supplies a distinct solver_id
            #    label (preserved by passing solver_label=None → spec.solver_id wins
            #    in CliSolver). Don't override it.
            if self.cli_race:
                self._label_seq[transport] = self._label_seq.get(transport, 0) + 1
                n = self._label_seq[transport]
                label = f"cli-{transport}" if n == 1 else f"cli-{transport}-{n}"
            else:
                label = f"cli-{transport}"
            workdir = self._alloc_workdir(engine)
            container = self._container_for_engine(engine, profile)
            worker = CliSolver(
                None if self.cli_race else specs[i % len(specs)],
                self.challenge, bus=self.bus, cost=self.cost,
                artifacts=self.artifacts, config=self.config, run_id=self.run_id,
                insight=self.insight, knowledge=self.knowledge,
                shared_graph=self.shared_graph, engine=transport,
                driver=driver_for(profile or transport),
                web_access=self.web_access, kb=self.kb,
                workdir=workdir,
                lifecycle_scope="worker",
                solver_label=label if self.cli_race else None,
                container=container,
                worker_env=self._runtime_env_for(transport, label, container=container, profile=profile),
            )
            self._claim_worker_account(worker.solver_id, transport, profile, role=role)
            workers.append(worker)
        return workers

    async def _reconcile_blackboard_skill(self) -> None:
        """Once per run, BEFORE any worker launches: make sure the deployed user-scope
        blackboard skill copies match the repo source, re-syncing stale/missing ones.

        Source runs invoke the repo skill directly, but a worker CLI can still
        auto-discover a ROTTED user-scope copy for unprompted skill use — run-75378
        shipped workers a skill missing the whole G0-G4 + lifecycle landing because the
        deployed copy was never re-synced after the repo skill advanced. This closes
        that gap loudly: anything actually re-synced is printed AND emitted as a board
        delta so a silent drift can never recur unnoticed. Containerized workers use the
        image-baked skill, so skip them. Best-effort — never blocks the run."""
        if self.worker_backend == "container":
            return
        try:
            from muteki.solver.cli_solver import sync_deployed_blackboard_skills
            rows = await asyncio.to_thread(sync_deployed_blackboard_skills)
        except Exception:
            return
        synced = [r for r in rows if r.get("status") == "synced"]
        errored = [r for r in rows if r.get("status") == "error"]
        if synced:
            for r in synced:
                print(f"[blackboard-skill] re-synced stale deployed copy "
                      f"{r['path']} (was {r.get('was')} → {r.get('now')})")
            await self._emit_bb_bus(
                "skill_resynced",
                summary=(f"deployed muteki-blackboard skill was stale at "
                         f"{len(synced)} location(s); re-synced from repo source"),
                paths=[r["path"] for r in synced])
        for r in errored:
            print(f"[blackboard-skill] WARNING: could not sync {r['path']}: "
                  f"{r.get('error')}")

    async def _emit_bb_bus(self, kind: str, **fields) -> None:
        """Emit one BLACKBOARD_DELTA from anywhere (finalize, resolve, etc.) — the
        coordinator loop has its own `_emit_bb` closure, but lifecycle transitions at
        run finish happen outside it and must still reach the JSONL/SSE stream the UI
        reads. Best-effort; a bus failure never masks the outcome."""
        if self.bus is None:
            return
        try:
            await self.bus.emit(Event(
                event_type=EventType.BLACKBOARD_DELTA, run_id=self.run_id,
                challenge_id=self.challenge.id,
                payload=blackboard_delta_payload(kind, actor="coordinator", **fields)))
        except Exception:
            pass

    async def _emit_finalize_lifecycle_deltas(self, result: dict, reason: str) -> None:
        """J/刀2: mirror release_claims_for_finalize's DB transitions onto the bus so
        the deck stops showing finalized intents as live. The DB write already
        happened (and was recorded as an event row in shared_graph); this re-emits it
        as a BLACKBOARD_DELTA the client reducer folds (intent_state_changed)."""
        if not isinstance(result, dict):
            return
        closed = [str(x) for x in (result.get("closed_intents") or []) if x]
        resumed = [str(x) for x in (result.get("resumed_intents") or []) if x]
        if closed:
            await self._emit_bb_bus(
                "intent_state_changed", intent_id=",".join(closed),
                dispatch_state="closed", close_reason="closed_by_solve",
                stop_reason="solved")
        if resumed:
            await self._emit_bb_bus(
                "intent_state_changed", intent_id=",".join(resumed),
                dispatch_state="resume", stop_reason=reason)

    _GRAPH_BRIDGE_KINDS = {
        "fact_added",
        "dead_end",
        "intent_proposed",
        "intent_claimed",
        "intent_concluded",
        "intent_state_changed",
        "flag_found",
        "poc_saved",
        "poc_claimed",
        "poc_concluded",
        "review_finding",
        "route_suppressed",
        "route_reopened",
        "branch_split",
        "branch_resolved",
        "coordinator_directive",
    }

    @staticmethod
    def _split_ids(value: Any) -> list[str]:
        return [x.strip() for x in str(value or "").split(",") if x.strip()]

    def _graph_event_to_bb(self, ev: dict) -> list[tuple[str, dict]]:
        seq = int(ev.get("seq") or 0)
        kind = str(ev.get("kind") or "")
        actor = str(ev.get("actor") or "")
        p = dict(ev.get("payload") or {})
        if kind == "fact_added":
            return [("fact_added", {
                "fact": p.get("fact", ""),
                "source": p.get("source", ""),
                "source_solver": p.get("source_solver") or actor,
                "verified": bool(ev.get("verified")),
                "confidence": ev.get("confidence", 1.0),
                "verifier": p.get("verifier", ""),
                "witness": p.get("witness", ""),
                "artifact_id": ev.get("artifact_id"),
                "fact_seq": seq,
                "route_hash": p.get("route_hash", ""),
                "intent_id": p.get("intent_id", ""),
            })]
        if kind == "dead_end":
            return [("dead_end", {
                "reason": p.get("reason", ""),
                "dead_end_seq": seq,
            })]
        if kind == "intent_proposed":
            fields = dict(p)
            fields["intent_id"] = p.get("intent_id", "")
            fields["goal"] = p.get("goal", "")
            fields["intent_seq"] = seq
            return [("intent_proposed", fields)]
        if kind == "intent_claimed":
            return [("intent_claimed", {
                "intent_id": p.get("intent_id", ""),
                "worker": actor,
                "intent_seq": seq,
            })]
        if kind == "intent_concluded":
            out = []
            for iid in self._split_ids(p.get("intent_id")):
                out.append(("intent_concluded", {
                    "intent_id": iid,
                    "worker": actor,
                    "result": p.get("result", ""),
                    "result_detail": p.get("result_detail", ""),
                    "to_fact_seq": p.get("to_fact_seq"),
                    "intent_seq": seq,
                }))
            return out
        if kind == "intent_state_changed":
            out = []
            for iid in self._split_ids(p.get("intent_id")):
                fields = dict(p)
                fields["intent_id"] = iid
                fields["intent_seq"] = seq
                out.append(("intent_state_changed", fields))
            return out
        if kind == "flag_found":
            fields = dict(p)
            fields["flag_seq"] = seq
            return [("flag_found", fields)]
        if kind in {"poc_saved", "poc_claimed", "poc_concluded"}:
            fields = dict(p)
            fields["seq"] = seq
            return [(kind, fields)]
        if kind == "review_finding":
            fields = dict(p)
            fields["seq"] = seq
            if "kind" in fields:
                fields["finding_kind"] = fields.pop("kind")
            return [("review_finding", fields)]
        if kind in {"route_suppressed", "route_reopened", "branch_split",
                    "branch_resolved", "coordinator_directive"}:
            fields = dict(p)
            fields["seq"] = seq
            return [(kind, fields)]
        return []

    async def _drain_graph_to_bus(self, *, emit_bb) -> None:
        if self.shared_graph is None:
            return
        try:
            events = self.shared_graph.events_since(
                self._last_graph_event_seq,
                kinds=sorted(self._GRAPH_BRIDGE_KINDS),
            )
        except Exception:
            return
        for ev in events:
            seq = int(ev.get("seq") or 0)
            emissions = self._graph_event_to_bb(ev)
            try:
                for kind, fields in emissions:
                    await emit_bb(kind, **fields)
            except Exception:
                fails = self._graph_bridge_failures.get(seq, 0) + 1
                self._graph_bridge_failures[seq] = fails
                if fails >= 3:
                    self._last_graph_event_seq = max(self._last_graph_event_seq, seq)
                    self._graph_bridge_failures.pop(seq, None)
                    continue
                return
            self._last_graph_event_seq = max(self._last_graph_event_seq, seq)
            self._graph_bridge_failures.pop(seq, None)

    async def _emit_run_finished(self, *, flag: "Optional[str]", solved: bool,
                                 reason: str = "finished") -> None:
        """Emit the ONE run-level RUN_FINISHED for this swarm run. Sub-workers emit
        WORKER_FINISHED (worker-level), so this is the single signal that flips the
        deck/rail to 'finished'. Best-effort: a bus failure must not mask the
        outcome the caller is about to return.

        Payload carries `flag` (first, back-compat), `flags` (all collected), and
        `expected_flags` so the deck can render N/total + decide solved-vs-partial."""
        self._cleanup_finished_worker_dirs()
        if self.bus is None:
            return
        try:
            runtime_meta = self._runtime_metadata_for()
            await self.bus.emit(Event(
                event_type=EventType.RUN_FINISHED, run_id=self.run_id,
                challenge_id=self.challenge.id,
                payload={"flag": flag, "flags": list(self._found_flags),
                         "expected_flags": self._expected_flags(),
                         "multi_flag": self._multi_flag(),
                         "solved": solved,
                         "reason": reason,
                         **runtime_meta}))
        except Exception:
            pass

    async def _finalize_coordinator_run(
        self, *, winner: "Optional[str]", flag: "Optional[str]",
        goal_complete: bool, per_solver: "dict[str, SolveOutcome]",
        terminal_reason: str = "") -> None:
        """M11: persist the winner, close the shared graph (release the SQLite WAL/-shm
        handles), and emit the single run-level RUN_FINISHED (which also sweeps
        non-winner worker scratch dirs). Idempotent via _run_finalized — safe to call
        from BOTH the normal-return path and the coordinator's finally, so a cancelled
        / errored run still frees its DB handle and cleans scratch instead of leaking
        them (the cleanup used to sit AFTER the finally, on the normal path only)."""
        if self._run_finalized:
            return
        self._run_finalized = True
        # L3: detach the coordinator's bus sinks so a reused bus doesn't keep them.
        if self.bus is not None and self._coord_sinks:
            for sink in self._coord_sinks:
                try:
                    self.bus.remove_sink(sink)
                except Exception:
                    pass
            self._coord_sinks = []
        if winner is not None:
            self._persist_winner(per_solver.get(winner), flag)
        solved = winner is not None or goal_complete or self._flags_complete()
        reason = (terminal_reason or "").strip()
        if not reason:
            if solved:
                reason = "solved" if winner is not None or self._flags_complete() else "goal_met"
            elif self._operator_stop:
                reason = "operator_stop"
            elif self._budget_exhausted_kind:
                reason = "budget_exhausted"
            else:
                reason = "runtime_failure"
        if self.shared_graph is not None:
            try:
                snap = self.shared_graph.snapshot()
                self._record_flags(*getattr(snap, "flags", []))
            except Exception:
                pass
            finalize_reason = (
                reason if reason in {"solved", "operator_stop", "budget_exhausted", "runtime_failure"}
                else ("solved" if solved else "runtime_failure"))
            try:
                fin = self.shared_graph.release_claims_for_finalize(  # type: ignore[attr-defined]
                    reason=finalize_reason)
                # 刀2: mirror the resume/closed transition onto the bus BEFORE close()
                # so the deck doesn't keep rendering these intents as live work.
                await self._emit_finalize_lifecycle_deltas(fin, finalize_reason)
                await self._drain_graph_to_bus(emit_bb=self._emit_bb_bus)
            except Exception:
                pass
            try:
                self.shared_graph.close()
            except Exception:
                pass
        solved = winner is not None or goal_complete or self._flags_complete()
        if solved and (not terminal_reason or reason == "runtime_failure"):
            reason = "solved" if winner is not None or self._flags_complete() else "goal_met"
        finish_flag = self._found_flags[0] if self._found_flags else (
            flag if winner is not None else None)
        await self._emit_run_finished(flag=finish_flag, solved=solved,
                                      reason=reason)

    def _cleanup_finished_worker_dirs(self) -> None:
        """Remove failed/finished worker scratch while preserving durable run data.

        The workspace root keeps shared/, inputs/, graph/, final/, manifest.json,
        and winner.json.  Only non-winner worker cwd directories under workers/ are
        removed at run finish to avoid long coordinator runs accumulating hundreds
        of duplicate scratch trees.
        """
        if self.worker_root is None:
            return
        keep: list[str] = []
        if self.workspace_root is not None:
            winner = self.workspace_root / "winner.json"
            try:
                data = json.loads(winner.read_text(encoding="utf-8"))
                workdir = data.get("workdir")
                if workdir:
                    keep.append(Path(str(workdir)).name)
            except Exception:
                pass
        cleanup_worker_scratch(self.worker_root, keep=keep)

    async def _drain_hitl(self) -> None:
        """Background: pull human commands off hitl_inbox and broadcast them to
        every solver via the InsightBus. Runs until cancelled. Each item is a
        dict {target, action, text} (the shape RunManager.post_hitl enqueues)."""
        if self.hitl_inbox is None:
            return
        while True:
            cmd = await self.hitl_inbox.get()
            try:
                if not isinstance(cmd, dict):
                    continue
                text = cmd.get("text") or cmd.get("hint") or ""
                action = cmd.get("action") or "hint"
                target = cmd.get("target") or "global"
                # operator STOP/COMPLETE: end the run gracefully. Unlike a steer
                # (which only guides workers), this terminates the coordinator loop —
                # the lever for a challenge that never yields a gated flag. Wake the
                # coordinator so it checks the flag at its next iteration boundary.
                if action in ("stop", "complete"):
                    self._operator_stop = True
                    self._pending_help = []
                    if self._operator_event is not None:
                        self._operator_event.set()
                    continue
                # operator PAUSE/RESUME (#5): soft-pause the coordinator's spawn loop.
                # pause sets a flag the loop checks at its top (no new workers until
                # resume); it does NOT kill running workers or end the run. resume
                # clears it and wakes the loop. This is the contract that actually fits
                # a single-shot swarm — see _operator_paused. Still broadcast on the
                # InsightBus below (the deck reflects pause/resume; a live standby
                # worker also gets it). We don't `continue` for resume so it falls
                # through to the wake (set _operator_event) at the bottom.
                if action == "pause":
                    self._operator_paused = True
                    # surface it on the board so the rail shows "paused"
                    await self._emit_coord_bb(
                        "operator_paused",
                        reason="operator paused the swarm "
                               "(no new workers until resume)")
                    await self.insight.guidance(
                        text, action="pause", target=target, standing=False)
                    continue
                if action == "resume":
                    self._operator_paused = False
                    if self._operator_event is not None:
                        self._operator_event.set()
                    await self.insight.guidance(
                        text, action="resume", target=target, standing=False)
                    continue
                # DISMISS a worker's hand-raise (NEED_INPUT) WITHOUT supplying the
                # resource: the operator judges the ask a false alarm / not worth
                # answering. The swarm must NOT stay frozen waiting on a blocker the
                # operator won't clear. Clear the pending ask (scoped to target),
                # record a dead-end so a re-spawned worker doesn't immediately re-raise
                # the same thing, unfreeze the workers, and wake the coordinator. No
                # resource is injected (distinct from a hint/redirect that answers it).
                if action in ("dismiss", "dismiss_help"):
                    if target == "global":
                        dismissed = list(self._pending_help)
                        self._pending_help = []
                    else:
                        scoped = target.split(":", 1)[-1] if ":" in target else target
                        dismissed = [h for h in self._pending_help
                                     if str(h.get("worker", "")) == scoped]
                        self._pending_help = [h for h in self._pending_help
                                              if str(h.get("worker", "")) != scoped]
                    for h in dismissed:
                        need = str(h.get("need", "")).strip()
                        if need:
                            try:
                                await self.insight.dead_end(
                                    "coordinator",
                                    f"operator dismissed the ask «{need[:160]}» — "
                                    f"not supplying it; do not re-raise")
                            except Exception:
                                pass
                    self._operator_paused = False
                    if self._operator_event is not None:
                        self._operator_event.set()
                    # SIGCONT the workers we froze on the hand-raise so the swarm
                    # resumes instead of sitting paused on a dismissed blocker.
                    try:
                        await self.insight.guidance(
                            "", action="resume", target="global", standing=False)
                    except Exception:
                        pass
                    await self._emit_coord_bb(
                        "help_dismissed",
                        reason=f"operator dismissed {len(dismissed)} hand-raise(s)"
                               f"{'' if target == 'global' else ' for ' + target}",
                        count=len(dismissed))
                    continue
                # P0 defect-4: clear standing guidance. The list is only-grew before,
                # so an operator who dropped several corrections could not retract a
                # stale one (and the cumulative text bloated every new worker's prompt
                # → claude 36k-token empty-exit). clear_standing wipes all, or one by
                # exact text match (cmd["text"]).
                if action in ("clear_standing", "reset_guidance"):
                    if text:
                        self._standing_guidance = [
                            s for s in self._standing_guidance if s != text]
                    else:
                        self._standing_guidance = []
                    continue
                if action == "mark_false":
                    flag = str(cmd.get("flag") or "").strip()
                    if not flag and text:
                        m = re.search(r"[A-Za-z0-9_]{0,15}\{[^}]{1,200}\}", str(text))
                        flag = m.group(0) if m else str(text).strip()
                    if not flag and self._found_flags:
                        flag = self._found_flags[0]
                    if flag:
                        self._found_flags = [f for f in self._found_flags if f != flag]
                        info = {"dead_end_reason": f"false positive: {flag}",
                                "reopened": []}
                        if self.shared_graph is not None:
                            try:
                                info = self.shared_graph.reopen_after_false_positive(
                                    actor="operator", flag=flag)
                            except Exception:
                                pass
                        await self._emit_coord_bb(
                            "dead_end", reason=info.get("dead_end_reason")
                            or f"false positive: {flag}")
                        for iid in info.get("reopened", []) or []:
                            await self._emit_coord_bb("intent_reopened", intent_id=iid)
                        await self._emit_coord_bb("flag_invalidated", flag=flag)
                        if self.bus is not None:
                            try:
                                await self.bus.emit(Event(
                                    event_type=EventType.RUN_REOPENED,
                                    run_id=self.run_id,
                                    challenge_id=self.challenge.id,
                                    payload={"flag": flag},
                                ))
                            except Exception:
                                pass
                        if self._operator_event is not None:
                            self._operator_event.set()
                    continue
                # `url` is the NEW target a redirect carries (distinct from `target`,
                # which is the SCOPE: global / solver:<id>). `standing` marks
                # persistent background guidance (VPS/SSH creds) for all workers.
                url = cmd.get("url") or cmd.get("target_url") or ""
                standing = bool(cmd.get("standing", False))
                # persist standing guidance on the coordinator so workers spawned
                # LATER inherit it at turn-1 (live workers also get it via the
                # InsightBus broadcast below). Dedupe so re-sends don't pile up.
                if standing and text and text not in self._standing_guidance:
                    self._standing_guidance.append(text)
                    # P0 defect-4: LRU cap — keep only the most recent N standing
                    # hints so the cumulative text can't bloat every new worker's
                    # prompt unbounded (the 36k-token claude empty-exit). The per-
                    # worker char budget (cli_solver _standing_block) is the second
                    # guard; this bounds the count at the source.
                    if len(self._standing_guidance) > _STANDING_MAX:
                        self._standing_guidance = self._standing_guidance[-_STANDING_MAX:]
                # M-3 (single-shot migration): a NON-standing hint/redirect can no
                # longer steer a live (single-shot) worker — route it to the NEXT
                # spawned worker. A redirect url becomes the new target for every
                # subsequent worker; hint/redirect text is one-shot guidance the next
                # spawn folds in. (standing already flows via _standing_guidance.)
                if not standing:
                    if url:
                        self._target_redirect = url
                    if text and text not in self._next_worker_guidance:
                        self._next_worker_guidance.append(text)
                # B: record the steer as a FIRST-CLASS OperatorDirective (not a fake
                # low-confidence candidate + ordinary intent). The directive carries a
                # preemption policy; soft_rebind (default) supersedes unclaimed
                # conflicting intents so the next worker batch picks up the new
                # direction, without killing a live worker. graceful_drain / force_cancel
                # are honored where the operator explicitly asks for them.
                preempt = str(cmd.get("preempt_policy")
                              or cmd.get("preemption") or "").strip().lower()
                if text and self.shared_graph is not None and action in (
                        "hint", "focus", "redirect", "directive", "correction"):
                    try:
                        info = self.shared_graph.add_operator_directive(
                            actor="operator", action=action, text=text,
                            scope=target or "global", standing=standing,
                            preempt_policy=preempt or "soft_rebind",
                        )
                        directive_id = info["directive_id"]
                        policy = info["preempt_policy"]
                        # bind: open a directive-tagged intent the next batch can claim.
                        self.shared_graph.propose_intent(
                            actor="operator", intent_id=f"I-{directive_id}", goal=text,
                            payload={"source": "operator_directive", "action": action,
                                     "directive_id": directive_id,
                                     "priority": "operator"},
                        )
                        self.shared_graph.update_directive_status(
                            directive_id=directive_id, status="bound",
                            generated_intent_id=f"I-{directive_id}")
                        await self._emit_coord_bb(
                            "operator_directive_changed", directive_id=directive_id,
                            action=action, text=text, status="bound",
                            preemption=policy, intent_id=f"I-{directive_id}")
                        # soft_rebind / graceful_drain / force_cancel: retire UNCLAIMED
                        # conflicting "ask operator" directions (the redirect obsoletes
                        # them). Live workers are only touched on graceful_drain (a
                        # GUIDANCE drain signal) / force_cancel (handled via _drain below).
                        if policy in ("soft_rebind", "graceful_drain", "force_cancel"):
                            for needle in ("operator", "ask", "request"):
                                try:
                                    self.shared_graph.supersede_open_intents(
                                        actor="coordinator", match=needle,
                                        reason=f"superseded by operator directive {directive_id}")
                                except Exception:
                                    pass
                        if policy == "graceful_drain":
                            try:
                                await self.insight.guidance(
                                    text, action="graceful_drain", target=target,
                                    standing=False)
                            except Exception:
                                pass
                        if self.review_policy.get("on_operator_hint", True):
                            self._queue_review_request(
                                trigger="operator_hint",
                                directive=(
                                    f"Operator {action} directive was added: {text}. "
                                    "Audit whether this should become a route change, "
                                    "branch split, fact challenge, or focused worker directive."
                                ),
                            )
                    except Exception:
                        pass
                # still broadcast on the InsightBus: the deck's event log + a live
                # standby worker consume it. A racing single-shot worker ignores it
                # (it has no resume turn) — that's the accepted intent-level degrade.
                await self.insight.guidance(
                    text, action=action, target=target, url=url, standing=standing)
                # wake the coordinator if it had paused (the operator supplied input).
                if self._operator_event is not None:
                    self._operator_event.set()
                # M5: clear the "waiting for help" asks SCOPED to the command's target.
                # A global command answers every pending ask; a solver-scoped one
                # (target == "solver:<id>") only clears that worker's ask, so a hint
                # addressed to worker B no longer wipes worker A's still-unmet blocker
                # (which would resolve awaiting_operator with no real answer and resume
                # hurling workers at A's wall). Keep the rest pending.
                if target == "global":
                    self._pending_help = []
                else:
                    scoped = target.split(":", 1)[-1] if ":" in target else target
                    self._pending_help = [
                        h for h in self._pending_help
                        if str(h.get("worker", "")) != scoped]
                # M3: RETIRE the now-obsolete "ask the operator for X" intents ONLY when
                # the operator actually SUPPLIED A RESOURCE — a redirect url, standing
                # guidance, or hint text (run-11190: 238-worker loop re-asking for the
                # L2 SSH password after it was supplied). A bare default-action hint with
                # no content used to run this sweep too, and its broad substring needles
                # (operator/unlock/dashboard) could wrongly retire a legitimate in-flight
                # intent on a totally unrelated hint. Gate on a resource being present;
                # for a solver-scoped command, only retire that worker's blocked intents.
                gave_resource = bool(url) or standing or bool(text)
                if self.shared_graph is not None and gave_resource:
                    superseded = 0
                    for needle in ("operator", "ssh password", "dashboard",
                                   "unlock"):
                        try:
                            superseded += len(self.shared_graph.supersede_open_intents(
                                actor="coordinator", match=needle,
                                reason=f"operator supplied input ({action})"))
                        except Exception:
                            pass
                    if superseded:
                        try:
                            await self.insight.dead_end(
                                "coordinator",
                                f"retired {superseded} obsolete 'ask-operator' "
                                f"intent(s) after operator input")
                        except Exception:
                            pass
            except Exception:
                # a malformed command must never kill the drain loop
                continue

    async def run(self) -> SwarmOutcome:
        # Single authoritative teardown for the run's worker container, covering EVERY
        # exit path of every solve mode: the coordinator's race-scout fast-path return,
        # its main-loop finally, the race-only path, and exceptions. The per-method
        # cleanups cancel worker tasks but the CONTAINER (and its idle supervisor) must
        # be removed exactly when the run truly ends — doing it here guarantees a
        # solved / stopped / budget-exhausted / errored run never leaks a container.
        # Cheap + idempotent when there's no container (local backend) or it's already
        # gone. A later resolve()/standby re-creates a fresh container via ensure_container.
        await self._reconcile_blackboard_skill()
        try:
            if self.coordinator and self.executor == "cli":
                return await self._run_coordinator()
            return await self._run_race()
        finally:
            if self.worker_backend == "container" or self._container_handle is not None:
                try:
                    from muteki.solver.container_exec import teardown_container
                    await asyncio.to_thread(teardown_container, self.run_id, remove=True)
                except Exception:
                    pass

    @staticmethod
    def _cancel_solver(solver: Any) -> None:
        """Stop a solver's underlying work (kills a CLI worker's subprocess). A
        plain task.cancel() only unschedules the asyncio task — the shelled CLI
        agent kept running. Solvers that don't expose cancel() (code-driven) are a
        no-op here; the task cancel still stops them between turns."""
        if solver is None:
            return
        fn = getattr(solver, "cancel", None)
        if callable(fn):
            try:
                fn()
            except Exception:
                pass

    async def _run_race(self) -> SwarmOutcome:
        solvers = self._build_solvers()
        hitl_task: Optional[asyncio.Task] = None
        if self.hitl_inbox is not None:
            hitl_task = asyncio.create_task(self._drain_hitl(), name="hitl-drain")
        tasks: dict[asyncio.Task[SolveOutcome], Any] = {
            asyncio.create_task(s.run(), name=s.solver_id): s for s in solvers
        }
        per_solver: dict[str, SolveOutcome] = {}
        winner: Optional[str] = None
        flag: Optional[str] = None

        pending = set(tasks.keys())
        try:
            while pending:
                done, pending = await asyncio.wait(
                    pending, return_when=asyncio.FIRST_COMPLETED
                )
                for t in done:
                    s = tasks[t]
                    try:
                        outcome = t.result()
                    except asyncio.CancelledError:
                        continue
                    except Exception as e:  # a solver crashing must not kill the swarm
                        per_solver[s.solver_id] = SolveOutcome(
                            False, None, 0, s.graph, f"error: {e}"
                        )
                        continue
                    per_solver[s.solver_id] = outcome
                    # multi-flag: fold every flag this solver produced into the
                    # run's dedup set (the worker already broadcast each to its
                    # siblings; this is the authoritative tally for completion).
                    self._record_flags(*(outcome.flags or
                                         ([outcome.flag] if outcome.flag else [])))
                    if self._flags_complete() and winner is None:
                        winner = s.solver_id
                        flag = self._found_flags[0]
                        # enough flags collected — stop the rest of the swarm. Tell
                        # any still-running sibling to die (ALL_FLAGS_FOUND), then
                        # cancel the SOLVER (kills its CLI subprocess) + the task.
                        try:
                            await self.insight.all_flags_found(
                                "swarm", count=len(self._found_flags))
                        except Exception:
                            pass
                        for other in pending:
                            self._cancel_solver(tasks.get(other))
                            other.cancel()
                # split-brain reconcile (BUG②): a sibling cancelled right after it
                # accepted a flag is reaped as CancelledError above and never tallies
                # its flag — fold the authoritative graph snapshot in so completion
                # still fires (and a blacklisted flag is dropped).
                if winner is None:
                    self._sync_flags_from_graph()
                    if self._flags_complete():
                        winner = s.solver_id
                        flag = self._found_flags[0] if self._found_flags else None
                        try:
                            await self.insight.all_flags_found(
                                "swarm", count=len(self._found_flags))
                        except Exception:
                            pass
                        for other in pending:
                            self._cancel_solver(tasks.get(other))
                            other.cancel()
                if winner is not None:
                    break
        finally:
            # whether we won, errored, or were cancelled from above, make sure no
            # solver task is left running (cancel subprocess + task, then drain).
            leftover = [t for t in tasks if not t.done()]
            for t in leftover:
                self._cancel_solver(tasks.get(t))
                t.cancel()
            if leftover:
                await asyncio.gather(*leftover, return_exceptions=True)
            for s in tasks.values():
                self._release_worker_account(s)
            # tear down the HITL drain background task too
            if hitl_task is not None:
                hitl_task.cancel()
                await asyncio.gather(hitl_task, return_exceptions=True)
            # container backend: one run-level Docker execution node is shared by
            # workers; force-remove it here so cancels do not leave a stale runtime.
            if self.worker_backend == "container" or self._container_handle is not None:
                try:
                    from muteki.solver.container_exec import teardown_container
                    await asyncio.to_thread(teardown_container, self.run_id, remove=True)
                except Exception:
                    pass

        # the winning Solver already broadcast FlagFound to the global bus; the
        # swarm just reports the aggregate outcome.
        if winner is not None:
            # persist the winner's CLI session handle so a post-solve standby
            # driver can resume the SAME worker for a human follow-up.
            self._persist_winner(per_solver.get(winner), flag)
            # §16 flywheel: distill the winning trace into a reusable template.
            # P-E: prefer the SHARED graph's event log (verified evidence chain),
            # falling back to the winner's private graph if no shared graph.
            if self.knowledge is not None:
                from muteki.learning.distill import (
                    distill_from_events, distill_and_store,
                )
                try:
                    if self.shared_graph is not None:
                        tpl = distill_from_events(self.shared_graph, winner=winner)
                        self.knowledge.save(tpl)
                    else:
                        distill_and_store(
                            per_solver[winner].graph, self.knowledge, winner=winner)
                except Exception:  # distillation must never fail a solved run
                    pass
            if self.shared_graph is not None:
                try:
                    self.shared_graph.close()
                except Exception:
                    pass
            await self._emit_run_finished(flag=flag, solved=True)
            return SwarmOutcome(True, flag, winner, per_solver, "solved",
                                flags=list(self._found_flags))
        if self.shared_graph is not None:
            try:
                self.shared_graph.close()
            except Exception:
                pass
        await self._emit_run_finished(flag=None, solved=False)
        return SwarmOutcome(False, None, None, per_solver, "no solver found a flag")

    # ════════════════════════════════════════════════════════════════════════
    # Coordinator: evidence-driven plan / dispatch loop
    #   seed workers (rush) -> plan from graph -> dispatch per-intent workers -> plan -> ...
    # with: heterogeneity-aware worker selection, graph-change-driven planning
    # (anti-stall), provenance facts, dead-end-as-first-class, first-valid-flag-wins.
    # ════════════════════════════════════════════════════════════════════════

    def _health_probe_key(self, name: str, role: str) -> tuple:
        """A stable cache identity for one engine's health probe: the resolved base
        engine + the profile id + the credential account it would authenticate with.
        A different account/profile (or role mapping to a different profile) gets its
        OWN cache slot, so swapping credentials always re-probes."""
        try:
            profile = self._profile_for_engine(name, role=role, advance=False)
        except Exception:  # noqa: BLE001
            profile = None
        base = base_engine_for_profile(profile) if profile else name
        profile_id = str((profile or {}).get("id") or "")
        account = str((profile or {}).get("credential_account") or "")
        return (base, profile_id, account, str(self.credential_accounts_root or ""))

    def _probe_engine_health(self, name: str, role: str) -> "tuple[bool, str]":
        """Shell ONE real one-turn CLI hello for `name` and return (ok, detail).
        detail names the failure mode (e.g. "Authentication required") so a
        degrade-time drop is explainable, not silent. This is the slow part (a
        subprocess.run with a 60–150s timeout + retry); callers parallelize +
        cache around it."""
        # When the coordinator runs INSIDE the web container (compose deploy), the
        # engine CLI binary is NOT present here — it lives only in the worker image.
        # Shelling a host-local hello fails with "binary not found on PATH" and the
        # engine is wrongly dropped from the roster ("no available worker profile"),
        # so no worker ever spawns. The real worker container has the CLI and does
        # real auth on spawn; defer to it instead of false-failing here. Mirrors the
        # dispatch precheck guard in profile_health.evaluate_profile_health.
        from muteki.core.runtime_env import is_web_container
        if is_web_container():
            return True, "deferred to worker container"
        from muteki.solver.cli_driver import driver_for
        try:
            profile = self._profile_for_engine(name, role=role, advance=False)
            # Inject the SAME credential env a live worker gets (the cursor headless
            # CLI authenticates only via CURSOR_API_KEY — a bare probe reports
            # "Authentication required" and the engine is wrongly dropped from the
            # roster). runtime_env_for_engine keys off the BASE engine
            # (claude/codex/cursor), NOT the profile name — `name` here can be a
            # profile id like "cursor-api-container", which would miss the cursor
            # branch and inject nothing. Mirror _make_cli_worker exactly.
            base = base_engine_for_profile(profile) if profile else name
            overlay = runtime_env_for_engine(
                base,
                account_root=self.credential_accounts_root,
                account_id=(profile.get("credential_account") if profile else None),
                container=False,
            ).env
            # Pass the COMPLETE env to the probe explicitly (os.environ + overlay)
            # instead of the old global os.environ patch — that global mutation was
            # only safe serially, and these probes now run in PARALLEL. An explicit
            # env keeps each engine's credentials isolated to its own subprocess.
            env = {**os.environ, **overlay}
            return driver_for(profile or name).health_detail(env=env)
        except Exception as exc:  # noqa: BLE001
            return False, str(exc)[:160]

    def _healthy_engines(self, *, role: str = "bootstrap") -> list[str]:
        # The whole-roster health check. Two latency fixes vs. the old serial loop
        # (the "dispatch freezes for ~a minute" symptom):
        #   1. CACHE — reuse a (ok, detail) verdict we computed for the SAME probe
        #      identity within the TTL, so a back-to-back dispatch / re-bootstrap /
        #      sibling run skips re-shelling every CLI.
        #   2. PARALLEL — probe every cache-MISS engine concurrently in a thread
        #      pool instead of one-after-another. Roster latency drops from
        #      sum(probes) to max(probe). Order/side-effects are unchanged: results
        #      are reassembled in roster order and degrade/recover fire exactly as
        #      before.
        import time
        from concurrent.futures import ThreadPoolExecutor

        now = time.monotonic()
        ttl = self._health_probe_ttl
        # engines this role is actually configured to use (cheap config gate FIRST —
        # never spend a probe on a role-unavailable engine; that's a config decision,
        # not a fault). Preserve roster order for deterministic output.
        candidates = [e for e in self.engines if self._engine_available_for_role(e, role)]

        results: dict[str, "tuple[bool, str]"] = {}
        to_probe: list[str] = []
        for e in candidates:
            cached = (
                _health_cache_get(self._health_probe_key(e, role), ttl, now)
                if ttl > 0 else None
            )
            if cached is not None:
                results[e] = cached
            else:
                to_probe.append(e)

        if to_probe:
            # one probe each, all at once. A single engine is just a direct call (no
            # pool overhead); 2+ fan out. Each shells its own subprocess, so threads
            # (not async) are the right tool and the GIL is released during the wait.
            if len(to_probe) == 1:
                fresh = {to_probe[0]: self._probe_engine_health(to_probe[0], role)}
            else:
                with ThreadPoolExecutor(max_workers=len(to_probe)) as pool:
                    fresh = dict(zip(
                        to_probe,
                        pool.map(lambda e: self._probe_engine_health(e, role), to_probe),
                    ))
            for e, verdict in fresh.items():
                if ttl > 0:
                    _health_cache_put(self._health_probe_key(e, role), verdict[0],
                                      verdict[1], now)
                results[e] = verdict

        engines: list[str] = []
        for e in candidates:
            healthy, detail = results[e]
            if healthy:
                engines.append(e)
                self._note_engine_recovered(e)
            else:
                # configured to fight but the CLI can't complete a turn right now
                # (auth/quota/binary) → drop from the roster AND tell the operator
                # why, instead of vanishing the engine from the worker panel.
                self._note_engine_degraded(e, detail or "health check failed", role=role)
        if engines:
            return engines
        return ["claude"] if not self.worker_profiles else []

    async def _healthy_engines_async(self, *, role: str = "bootstrap") -> list[str]:
        """Run CLI health probes off the FastAPI/coordination event loop.

        `_healthy_engines()` shells real CLI probes (`subprocess.run`, retries,
        60s hello timeouts). Calling it directly from `run()` or review scheduling
        can freeze the single uvicorn worker: all API/SSE requests queue until the
        probe returns. Keep the existing sync helper for tests/direct callers, but
        use this wrapper from async production paths.
        """
        try:
            return await asyncio.to_thread(self._healthy_engines, role=role)
        except TypeError:
            # A number of tests monkeypatch `_healthy_engines` as a no-arg lambda.
            return await asyncio.to_thread(self._healthy_engines)

    def _pick_engine(
        self,
        running_engines: list[str],
        healthy: list[str],
        *,
        role: str = "bootstrap",
    ) -> str:
        """Heterogeneity-aware engine selection: prefer an engine NOT currently
        running, so each spawned worker covers a different blind spot. Falls back to
        least-loaded when all are running."""
        available = self._healthy_role_candidates(healthy, role=role)
        if not available:
            raise RuntimeError(f"no available worker profile for role={role}")
        for e in available:
            if self._running_count_for_candidate(e, running_engines) == 0:
                return e
        # all healthy engines already running → least-loaded
        return min(available, key=lambda e: self._running_count_for_candidate(e, running_engines))

    @staticmethod
    def _clean_runtime_profiles(value: "Optional[list[dict]]") -> dict[str, dict]:
        out: dict[str, dict] = {}
        if not isinstance(value, list):
            return out
        for item in value:
            if not isinstance(item, dict):
                continue
            rid = str(item.get("id") or "").strip()
            backend = str(item.get("backend") or "").strip()
            if rid and backend in {"local", "container"}:
                out[rid] = {
                    "id": rid,
                    "backend": backend,
                    "network": str(item.get("network") or "bridge"),
                    "memory": str(item.get("memory") or ""),
                    "cpus": str(item.get("cpus") or ""),
                    "pids_limit": int(item.get("pids_limit") or 0),
                }
        return out

    def _runtime_for_engine(self, engine: str, profile: "Optional[dict]" = None) -> "Optional[dict]":
        profile = profile if profile is not None else self._profile_for_engine(engine)
        if not profile:
            return None
        return self.runtime_profiles.get(profile.get("runtime") or "")

    @staticmethod
    def _clean_worker_profiles(value: "Optional[list[dict]]") -> list[dict]:
        return normalize_worker_profiles(value, defaults=[])

    @staticmethod
    def _profile_allows_role(profile: dict, role: "Optional[str]") -> bool:
        if role is None:
            return True
        if role == "race" and profile.get("race") is False:
            return False
        roles = profile.get("roles") or []
        return role in roles

    def _review_profile_limit(self, profile: dict) -> int:
        raw = profile.get("max_review_running")
        if raw in (None, "", 0):
            raw = self.review_policy.get("max_concurrent") or 1
        try:
            return max(0, int(raw))
        except (TypeError, ValueError):
            return max(1, int(self.review_policy.get("max_concurrent") or 1))

    def _profile_available(self, profile: dict, role: "Optional[str]" = None) -> bool:
        pid = profile["id"]
        if role == "review":
            return (
                self._active_review_profile_counts.get(pid, 0)
                < self._review_profile_limit(profile)
            )
        return self._active_profile_counts.get(pid, 0) < int(profile.get("max_running") or 1)

    def _profile_for_engine(
        self,
        engine: str,
        *,
        role: "Optional[str]" = None,
        advance: bool = True,
    ) -> "Optional[dict]":
        if engine in getattr(self, "_profiles_by_name", {}):
            profiles = [self._profiles_by_name[engine]]
        else:
            profiles = self._profiles_by_engine.get(engine) or []
        if not profiles:
            return None
        start = self._profile_rr.get(engine, 0)
        for off in range(len(profiles)):
            idx = (start + off) % len(profiles)
            p = profiles[idx]
            if not self._profile_allows_role(p, role):
                continue
            if not self._profile_available(p, role=role):
                continue
            if advance:
                self._profile_rr[engine] = (idx + 1) % len(profiles)
            return p
        return None

    def _engine_available_for_role(self, engine: str, role: str) -> bool:
        profiles_by_engine = getattr(self, "_profiles_by_engine", {})
        profiles_by_name = getattr(self, "_profiles_by_name", {})
        if engine not in profiles_by_name and engine not in profiles_by_engine:
            return True
        return self._profile_for_engine(engine, role=role, advance=False) is not None

    def _healthy_matches(self, engine_or_profile: str, healthy: list[str]) -> bool:
        """Healthy rosters may contain either base engine ids (claude/codex/cursor)
        or concrete worker profile ids, depending on whether they came from a live
        probe or a caller-supplied health list. Treat both forms as equivalent for
        scheduling decisions."""
        if engine_or_profile in healthy:
            return True
        profile = getattr(self, "_profiles_by_name", {}).get(engine_or_profile)
        base = base_engine_for_profile(profile or engine_or_profile)
        return base in healthy

    def _healthy_role_candidates(self, healthy: list[str], *, role: str) -> list[str]:
        """Configured, healthy, capacity-available scheduling units for `role`.

        When worker profiles are enabled, the scheduler's unit is the profile id
        from settings. Base engine names are only compatibility input to health and
        manual-spawn paths; they are normalized back to the configured profile
        roster before a worker is selected.
        """
        if getattr(self, "worker_profiles", []):
            roster = list(getattr(self, "engines", []))
        else:
            roster = list(healthy)
        out: list[str] = []
        seen: set[str] = set()
        for e in roster:
            if e in seen:
                continue
            seen.add(e)
            if not self._healthy_matches(e, healthy):
                continue
            if self._engine_available_for_role(e, role):
                out.append(e)
        return out

    def _running_count_for_candidate(self, candidate: str, running_engines: list[str]) -> int:
        profile = getattr(self, "_profiles_by_name", {}).get(candidate)
        base = base_engine_for_profile(profile or candidate)
        n = 0
        for running in running_engines:
            if running == candidate:
                n += 1
                continue
            running_profile = getattr(self, "_profiles_by_name", {}).get(running)
            running_base = base_engine_for_profile(running_profile or running)
            if running_base == base:
                n += 1
        return n

    def _claim_worker_account(
        self, solver_id: str, engine: str, profile: "Optional[dict]",
        role: "Optional[str]" = None,
    ) -> None:
        if not profile:
            return
        pid = profile["id"]
        role_bucket = "review" if role == "review" else "worker"
        self._active_profile_by_solver[solver_id] = pid
        self._active_profile_role_by_solver[solver_id] = role_bucket
        if role_bucket == "review":
            self._active_review_profile_counts[pid] = (
                self._active_review_profile_counts.get(pid, 0) + 1)
        else:
            self._active_profile_counts[pid] = self._active_profile_counts.get(pid, 0) + 1
        account_id = profile.get("credential_account")
        if not account_id:
            return
        self._active_account_by_solver[solver_id] = account_id

    def _release_worker_account(self, solver: Any) -> None:
        sid = getattr(solver, "solver_id", "")
        pid = self._active_profile_by_solver.pop(sid, None)
        role_bucket = self._active_profile_role_by_solver.pop(sid, "worker")
        if pid:
            if role_bucket == "review":
                self._active_review_profile_counts[pid] = max(
                    0, self._active_review_profile_counts.get(pid, 0) - 1)
            else:
                self._active_profile_counts[pid] = max(
                    0, self._active_profile_counts.get(pid, 0) - 1)
        self._active_account_by_solver.pop(sid, None)

    def _alloc_workdir(self, engine: str) -> "Optional[str]":
        """Carve a fresh per-worker cwd under worker_root, or return None to let
        CliSolver fall back to a system mkdtemp. The monotonic _worker_seq keeps
        two same-engine workers (race + a later explore) from colliding."""
        if self.worker_root is None:
            return None
        if self.workspace_root is not None:
            ensure_workspace(self.workspace_root, runtime={
                "backend": self.worker_backend,
                "run_id": self.run_id,
            })
        self._worker_seq += 1
        wd = self.worker_root / f"cli-{engine}-{self._worker_seq}"
        try:
            wd.mkdir(parents=True, exist_ok=True)
        except OSError:
            return None  # unwritable → fall back to mkdtemp, never block the run
        return str(wd)

    def _runtime_env_for(
        self,
        engine: str,
        label: str,
        *,
        container: "Optional[object]",
        profile: "Optional[dict]" = None,
    ) -> dict[str, str]:
        """Per-worker runtime env: Credential Account plus isolated HOME."""
        env = runtime_env_for_engine(
            engine,
            account_root=self.credential_accounts_root,
            account_id=(profile.get("credential_account") if profile else None),
            container=container is not None,
        ).env
        if profile:
            env["MUTEKI_WORKER_PROFILE_ID"] = profile["id"]
            env["MUTEKI_CREDENTIAL_ACCOUNT_ID"] = profile["credential_account"]
            if profile.get("model"):
                env["MUTEKI_WORKER_MODEL"] = str(profile["model"])
        if self.worker_root is not None and container is not None:
            base = (self.workspace_root or self.worker_root.parent)
            home_host = base / "homes" / label
            try:
                home_host.mkdir(parents=True, exist_ok=True)
            except OSError:
                return env
            _ensure_blackboard_skill_links(home_host)
            from muteki.solver.container_exec import _chown_tree_to_worker
            _chown_tree_to_worker(str(home_host))
            mapper = getattr(container, "to_container_path", None)
            if callable(mapper):
                try:
                    env["HOME"] = mapper(str(home_host))
                except Exception:
                    env["HOME"] = str(home_host)
            else:
                env["HOME"] = str(home_host)
        return env

    def _backend_for_engine(self, engine: str, profile: "Optional[dict]" = None) -> str:
        profile = profile if profile is not None else self._profile_for_engine(engine)
        if profile:
            runtime = self._runtime_for_engine(engine, profile)
            if runtime:
                if self._container_unavailable and runtime["backend"] == "container":
                    self._fail_if_container_required(engine)
                    return "local"
                return runtime["backend"]
        if self._container_unavailable and self.worker_backend == "container":
            self._fail_if_container_required(engine)
            return "local"
        return "container" if self.worker_backend == "container" else "local"

    def _fail_if_container_required(self, engine: str) -> None:
        """P2-v3: when the coordinator runs INSIDE the web container, a container
        backend that has gone unavailable must NOT silently degrade to local — that
        would launch a host-native agent CLI inside the web container (no tools,
        wrong creds, broken isolation), violating the "web container ⇒ worker
        container" invariant. Hard-fail the run instead so the operator sees the
        real cause (docker socket unreachable / image missing / network name wrong)
        rather than a subtly-wrong run. On a bare host this is a no-op and the
        historical local fallback stands."""
        if is_web_container():
            raise RuntimeError(
                f"container worker backend is unavailable for {engine!r} and this "
                f"coordinator runs inside the web container — refusing to fall back "
                f"to a host-native (local) worker (it would run with no tools / wrong "
                f"credentials). Fix the container backend: check the docker socket "
                f"mount, the worker image is pulled, and MUTEKI_CONTROL_BIND/"
                f"MUTEKI_CONTROL_HOST + the compose network are set."
            )

    def _note_engine_degraded(self, engine: str, reason: str, *, role: str) -> None:
        """An engine failed its dispatch-time health check and was dropped from the
        roster. Emit an `engine_degraded` blackboard delta ONCE per transition (not
        once per spawn) so the operator sees WHY the engine never showed up, instead
        of it silently vanishing from the worker panel."""
        reason = (reason or "health check failed")[:300]
        if self._degraded_engines.get(engine) == reason:
            return  # already announced this exact failure — don't spam the timeline
        self._degraded_engines[engine] = reason
        payload = {
            "engine": engine,
            "status": "degraded",
            "reason": reason,
            "role": role,
        }
        try:
            loop = asyncio.get_running_loop()
            loop.create_task(self._emit_engine_degraded(payload))
        except RuntimeError:
            pass

    def _note_engine_recovered(self, engine: str) -> None:
        """A previously-degraded engine passed its health check again — clear the
        dedup latch and tell the operator it's back so the warning state lifts."""
        if engine not in self._degraded_engines:
            return
        self._degraded_engines.pop(engine, None)
        payload = {"engine": engine, "status": "recovered", "reason": ""}
        try:
            loop = asyncio.get_running_loop()
            loop.create_task(self._emit_engine_degraded(payload))
        except RuntimeError:
            pass

    async def _emit_engine_degraded(self, payload: dict[str, Any]) -> None:
        if self.bus is None:
            return
        try:
            await self.bus.emit(Event(
                event_type=EventType.BLACKBOARD_DELTA,
                run_id=self.run_id,
                challenge_id=self.challenge.id,
                payload=blackboard_delta_payload(
                    "engine_degraded", actor="coordinator", **payload),
            ))
        except Exception:
            pass

    async def _emit_runtime_degraded(self, payload: dict[str, Any]) -> None:
        if self.bus is None:
            return
        try:
            await self.bus.emit(Event(
                event_type=EventType.BLACKBOARD_DELTA,
                run_id=self.run_id,
                challenge_id=self.challenge.id,
                payload=blackboard_delta_payload(
                    "runtime_degraded", actor="coordinator", **payload),
            ))
        except Exception:
            pass

    def _record_runtime_degraded(
        self,
        *,
        engine: str,
        profile: "Optional[dict]",
        reason: str,
        requested_backend: str,
        fallback_backend: str = "local",
    ) -> None:
        runtime = self._runtime_for_engine(engine, profile) or {}
        payload = {
            "engine": engine,
            "profile": (profile or {}).get("name") or (profile or {}).get("id") or "",
            "runtime": runtime.get("id") or "",
            "requested_backend": requested_backend,
            "backend": fallback_backend,
            "status": "degraded",
            "reason": reason[:300],
        }
        self._runtime_degraded.append(payload)
        try:
            loop = asyncio.get_running_loop()
            loop.create_task(self._emit_runtime_degraded(payload))
        except RuntimeError:
            pass

    def _runtime_metadata_for(self, outcome: "Optional[SolveOutcome]" = None) -> dict[str, Any]:
        engine = getattr(outcome, "engine", "") if outcome is not None else ""
        profile = self._profile_for_engine(engine, advance=False) if engine else None
        runtime = self._runtime_for_engine(engine, profile) if engine else None
        return {
            "backend": "local" if self._runtime_degraded else self.worker_backend,
            "runtime": (runtime or {}).get("id") or "",
            "runtime_degraded": list(self._runtime_degraded),
        }

    def _container_for_engine(self, engine: str, profile: "Optional[dict]" = None) -> "Optional[object]":
        """The run's container ContainerHandle when worker_backend=="container",
        else None (local host backend). Created lazily — worker_root must exist so
        it can be bind-mounted as the shared workspace (only the run workspace +
        control socket + account projection are mounted). Container mode surfaces
        setup failures instead of silently falling back to host execution, so a
        requested runtime is either honored or reported, never quietly downgraded."""
        if self._backend_for_engine(engine, profile) != "container":
            return None
        requested_runtime = (self._runtime_for_engine(engine, profile) or {})
        requested_rt_id = requested_runtime.get("id") or (profile or {}).get("runtime") or ""
        if self._container_handle is not None:
            # one-container-per-run: a later engine whose profile asks for a
            # DIFFERENT runtime (e.g. docker-offline vs the docker-web the container
            # was built with) cannot get a second container. Don't silently inherit
            # the first profile's isolation — surface it as runtime_degraded so the
            # operator knows this worker's requested network/resource isolation was
            # NOT honored (#11). The worker still runs in the existing container.
            if (requested_rt_id and self._container_runtime_id
                    and requested_rt_id != self._container_runtime_id):
                self._record_runtime_degraded(
                    engine=engine, profile=profile,
                    reason=(f"run container already built with runtime "
                            f"'{self._container_runtime_id}'; requested "
                            f"'{requested_rt_id}' not honored "
                            f"(one container per run)"),
                    requested_backend="container")
            return self._container_handle
        if self.worker_root is None:
            self._container_unavailable = True
            self._record_runtime_degraded(
                engine=engine, profile=profile,
                reason="container worker_backend requires worker_root",
                requested_backend="container")
            self._fail_if_container_required(engine)  # P2-v3: no local fallback in-container
            return None
        try:
            from muteki.solver.container_exec import ensure_container
            # Mount the whole run workspace, not only workspace/workers: the shared
            # graph lives in workspace/graph and the blackboard skill needs it.
            self._container_handle = ensure_container(
                self.run_id,
                str(self.workspace_root or self.worker_root),
                network=str((self._runtime_for_engine(engine, profile) or {}).get("network") or "bridge"),
                memory=str((self._runtime_for_engine(engine, profile) or {}).get("memory") or "") or None,
                cpus=str((self._runtime_for_engine(engine, profile) or {}).get("cpus") or "") or None,
                pids_limit=int((self._runtime_for_engine(engine, profile) or {}).get("pids_limit") or 0) or None,
                account_root=(str(self.credential_accounts_root)
                              if self.credential_accounts_root is not None else None),
            )
            # remember which runtime profile this run's single container was built
            # from, so a later engine requesting a different runtime is flagged
            # degraded rather than silently inheriting these settings (#11).
            self._container_runtime_id = requested_rt_id
        except Exception as exc:  # noqa: BLE001
            self._container_unavailable = True
            self._record_runtime_degraded(
                engine=engine, profile=profile,
                reason=f"container worker backend failed for {self.run_id}: {exc}",
                requested_backend="container")
            self._fail_if_container_required(engine)  # P2-v3: no local fallback in-container
            return None
        return self._container_handle

    def _current_cost_usd(self) -> float:
        ledger = getattr(self.cost, "_global", None)
        return float(getattr(ledger, "usd", 0.0) or 0.0)

    def _budget_exhausted(self) -> str | None:
        if self.max_total_workers is not None and self._spawned_total >= self.max_total_workers:
            return "worker_budget_exhausted"
        if self.cost_budget_usd is not None and self._current_cost_usd() >= self.cost_budget_usd:
            return "cost_budget_exhausted"
        return None

    def _reserve_worker_spawn(self) -> None:
        kind = self._budget_exhausted()
        if kind:
            self._budget_exhausted_kind = kind
            raise WorkerBudgetExhausted(kind)
        self._spawned_total += 1

    def _make_cli_worker(self, engine: str, *, mode: str, intent_goal: str = "",
                         intent_id: str = "", timeout_override: "Optional[int]" = None,
                         profile_role: "Optional[str]" = None):
        from muteki.solver.cli_solver import CliSolver

        # Resolve the profile FIRST — BEFORE charging the spawn budget. A missing
        # profile is a recoverable rejection (WorkerSpawnRejected), not a budget
        # event: charging _spawned_total here and then bailing would leak a phantom
        # spawn toward max_total_workers (and a bare RuntimeError would crash the
        # coordinator loop, since spawn sites only catch WorkerBudgetExhausted).
        role = profile_role or (
            "review" if mode == "review" else
            "explore" if mode == "explore" else "bootstrap")
        profile = self._profile_for_engine(engine, role=role)
        if self.worker_profiles and profile is None:
            raise WorkerSpawnRejected(
                f"no available worker profile for {engine} role={role}")

        # Budget is charged ONLY after we know we'll actually build a worker.
        self._reserve_worker_spawn()

        # UNIQUE label per spawn so the deck draws one lane per worker. Every
        # claude worker would otherwise be "cli-claude" and collapse onto a single
        # lane — you couldn't tell parallel / re-bootstrapped workers apart. We keep
        # the "cli-<engine>" prefix (the deck's workerEngine() badge keys off it)
        # and append a monotonic index. The first worker of an engine keeps the bare
        # "cli-<engine>" id for back-compat (winner bookkeeping, existing tests).
        transport = base_engine_for_profile(profile or engine)
        self._label_seq[transport] = self._label_seq.get(transport, 0) + 1
        n = self._label_seq[transport]
        label = f"cli-{transport}" if n == 1 else f"cli-{transport}-{n}"

        # explore = narrow single-intent probe → SHORT per-turn timeout, so a stuck
        # explore frees its slot quickly (this is the only backstop now that the
        # stall-kill is gone). bootstrap/retry = whole-challenge rush → keep the long
        # default (CliSolver's timeout=2400).
        kw = {"timeout": self.explore_timeout} if mode == "explore" else {}
        if mode == "review":
            kw["timeout"] = int(self.review_policy.get("timeout") or 420)
        # race-scout: a bootstrap worker gets the SHORT race_timeout (breadth recon,
        # not deep dig) when the caller overrides it. Explicit override wins.
        if timeout_override is not None:
            kw["timeout"] = int(timeout_override)

        # M-3 (single-shot migration): fold any pending intent-level operator
        # guidance into THIS spawn (workers can't be steered live anymore).
        #  - one-shot hint/redirect text → injected with standing (then consumed).
        #  - a redirect url → handed via hitl_cmd so the worker's _target_override
        #    points at the new target (CliSolver reads hitl_cmd["url"]).
        guidance_for_worker = list(self._standing_guidance) + list(self._next_worker_guidance)
        self._next_worker_guidance = []  # one-shot: consumed by this spawn
        # B: fold active operator directives into the worker prompt as highest-priority
        # steering (deduped against guidance already present). They persist across
        # spawns (table-backed) so a worker spawned after the directive still gets it.
        if self.shared_graph is not None:
            try:
                for dtext in self.shared_graph.active_operator_directive_texts():
                    tagged = f"[operator directive] {dtext}"
                    if dtext not in guidance_for_worker and tagged not in guidance_for_worker:
                        guidance_for_worker.append(tagged)
            except Exception:
                pass
        if self._target_redirect:
            kw["hitl_cmd"] = {"action": "redirect", "url": self._target_redirect}

        workdir = self._alloc_workdir(engine)
        container = self._container_for_engine(engine, profile)
        from muteki.solver.cli_driver import driver_for
        worker = CliSolver(
            None, self.challenge, bus=self.bus, cost=self.cost,
            artifacts=self.artifacts, config=self.config, run_id=self.run_id,
            insight=self.insight, knowledge=self.knowledge,
            shared_graph=self.shared_graph, engine=transport,
            driver=driver_for(profile or transport),
            web_access=self.web_access, kb=self.kb,
            workdir=workdir,
            mode=mode, intent_goal=intent_goal, intent_id=intent_id,
            solver_label=label, **kw,
            # hand the worker the operator's standing guidance + any one-shot
            # intent-level guidance so its (single) prompt already carries VPS/SSH
            # creds, corrections, etc. (copy: the worker must not mutate the
            # coordinator's canonical list).
            standing_guidance=guidance_for_worker,
            # multi-flag: seed the already-found set so a re-bootstrapped worker's
            # turn-1 prompt lists the flags the run already has and hunts the rest
            # (empty for a single-flag run → no effect).
            found_flags=list(self._found_flags),
            # swarm sub-worker: its end is worker-level (WORKER_FINISHED), NOT the
            # run's. The coordinator owns the single run-level RUN_FINISHED so a
            # worker ending mid-run doesn't make the deck show "已结束" while the
            # coordinator is still re-bootstrapping (the run-7345 bug).
            lifecycle_scope="worker",
            # container backend (None → local host subprocess, default).
            container=container,
            worker_env=self._runtime_env_for(transport, label, container=container, profile=profile),
        )
        self._claim_worker_account(worker.solver_id, transport, profile, role=role)
        return worker

    def _verified_fact_count(self) -> int:
        if self.shared_graph is None:
            return 0
        try:
            return sum(1 for e in self.shared_graph.snapshot().evidence
                       if getattr(e, "verified", False))
        except Exception:
            return 0

    def _prior_intent_count(self) -> int:
        """Durable count of intents this challenge's graph has EVER held — the
        graph-state half of the cold-start check. Operator pre-seeding writes facts,
        not intents, so any intent means a prior run already dispatched here."""
        if self.shared_graph is None:
            return 0
        try:
            return int(self.shared_graph.prior_intent_count())
        except Exception:
            return 0

    def _is_cold_start(self) -> bool:
        """Is this launch a genuine cold start (an empty graph that should be warmed
        by a race-scout round), or a resume/reopen continuing on prior work?

        run-75379 BUG④: race-scout is a cold-start warmup. On a reopen of a populated
        graph it re-races a challenge that already has dozens of verified facts (and
        sometimes flags), spawning fresh bootstrap workers that burn budget re-doing
        solved work. The web reopen path (run_manager.resolve) already passes
        race_scout=False, but ANY other relaunch on an existing graph_dir (a standby
        restart, a direct Swarm(graph_dir=<existing>), a future caller) stayed exposed
        because the race block was unconditional. This makes the guard an INVARIANT of
        the coordinator itself.

        Two signals, explicit-first (Codex 对审):
        - EXPLICIT: self.cold_start (constructor / stage_policy). Authoritative when it
          says "resume" (False) — never race a caller-declared resume. Honored when it
          says "cold" (True) EXCEPT the graph already shows prior solve activity, which
          means a relaunch forgot to flip it (the bug we're closing).
        - GRAPH-STATE backstop: prior intents OR already-found flags. An operator MAY
          pre-seed *facts* into a fresh cold run, so fact-emptiness alone would
          misclassify that as a resume — which is why the backstop keys on INTENTS and
          FLAGS (only a prior run produces those), not on facts."""
        if not self.cold_start:
            return False  # caller explicitly declared a resume/reopen
        if self._found_flags:
            return False  # a flag is already in hand — not a fresh graph
        if self._prior_intent_count() > 0:
            return False  # a prior run already planned/dispatched on this graph
        return True

    def _total_fact_count(self) -> int:
        """All facts incl. unverified candidates — the barren-backpressure progress
        signal. Candidates count as engagement: a late-stage worker grinding an
        exploit often emits only candidates, and pausing on it would be a false
        positive (the Reason trigger keeps using the stricter verified count).

        Deliberately RAW (append-only) and monotonic: this is a *progress
        checkpoint* (`tfc > prog_fact_ckpt`), not a live queue depth. If it dropped
        when facts retire, the barren detector would false-positive `grew=False` and
        wrongly inflate fruitless_workers. Lifecycle-aware counting belongs on the
        *candidate* count below, which drives review/visibility, not progress."""
        if self.shared_graph is None:
            return 0
        try:
            return sum(1 for e in self.shared_graph.events()
                       if e.get("kind") == "fact_added")
        except Exception:
            return 0

    def _candidate_fact_count(self) -> int:
        """LIVE unverified-candidate count (刀3): lifecycle-aware via
        active_candidates(), so a rejected / merged / superseded candidate stops
        counting. This drives the candidate-spike review trigger and is the number
        the board reflects — it must shrink when a candidate is retired, unlike the
        raw progress checkpoint above. Falls back to the raw event scan only if the
        lifecycle view is unavailable."""
        if self.shared_graph is None:
            return 0
        try:
            # active_candidates() already excludes verified + retired/terminal facts,
            # so its length IS the live unverified-candidate count.
            return len(self.shared_graph.active_candidates())
        except Exception:
            try:
                return sum(1 for e in self.shared_graph.events()
                           if e.get("kind") == "fact_added" and not e.get("verified"))
            except Exception:
                return 0

    async def _apply_worker_cmds(self, *, tasks: dict, task_solvers: dict,
                                 healthy: list[str], running_engines_fn, emit_bb) -> None:
        """Drain operator spawn/kill worker commands onto the LIVE coordinator
        state (BE-worker-management runtime control). Mutates tasks/task_solvers
        in place. A spawn adds a fresh bootstrap worker for the requested engine
        (capped at max_workers; engine must be in the roster or currently healthy);
        a kill cancels the worker whose solver_id matches (it's reaped next loop)."""
        if self.worker_cmds is None:
            return
        while not self.worker_cmds.empty():
            try:
                cmd = self.worker_cmds.get_nowait()
            except asyncio.QueueEmpty:
                break
            if not isinstance(cmd, dict):
                continue
            action = cmd.get("action")
            if action == "spawn":
                if not self._ordinary_capacity_available(tasks):
                    await emit_bb("worker_spawn_rejected", reason="max_workers")
                    continue
                try:
                    requested = cmd.get("engine")
                    if requested and self.worker_profiles:
                        matches = [
                            e for e in normalize_profile_roster([requested], self.worker_profiles)
                            if e in self.engines and self._healthy_matches(e, healthy)
                        ]
                        if not matches:
                            await emit_bb("worker_spawn_rejected",
                                          reason="unavailable_profile",
                                          engine=str(requested))
                            continue
                        engine = matches[0]
                    else:
                        engine = requested or self._pick_engine(
                            running_engines_fn(), healthy, role="bootstrap")
                except RuntimeError as exc:
                    await emit_bb("worker_spawn_rejected", reason=str(exc))
                    continue
                # only spawn an engine in the configured roster or one currently
                # healthy — never silently launch something offline mode dropped.
                if self.worker_profiles:
                    unknown = engine not in self.engines or not self._healthy_matches(str(engine), healthy)
                else:
                    unknown = engine not in self.engines and engine not in healthy
                if unknown:
                    await emit_bb("worker_spawn_rejected",
                                  reason="unknown_engine", engine=str(engine))
                    continue
                if not self._engine_available_for_role(str(engine), "bootstrap"):
                    await emit_bb("worker_spawn_rejected",
                                  reason="profile_capacity", engine=str(engine))
                    continue
                try:
                    w = self._make_cli_worker(engine, mode="bootstrap")
                except WorkerSpawnRejected as exc:
                    await emit_bb("worker_spawn_rejected", reason=str(exc),
                                  engine=str(engine), phase="operator")
                    continue
                except WorkerBudgetExhausted as exc:
                    await emit_bb(str(exc), spawned_total=self._spawned_total,
                                  max_total_workers=self.max_total_workers,
                                  cost_usd=self._current_cost_usd(),
                                  cost_budget_usd=self.cost_budget_usd)
                    continue
                t = asyncio.create_task(w.run(), name=f"operator-{engine}")
                tasks[t] = engine
                task_solvers[t] = w
                await emit_bb("worker_spawned", worker=w.solver_id,
                              phase="operator", worker_role="worker")
            elif action == "kill":
                sid = cmd.get("solver_id")
                for t, w in list(task_solvers.items()):
                    if getattr(w, "solver_id", None) == sid:
                        self._cancel_solver(w)
                        t.cancel()
                        await emit_bb("worker_killed", worker=sid)
                        break

    def _retry_goal(self) -> str:
        """Course-correction goal for a re-bootstrap.

        A retry_bootstrap worker runs the SAME _run_bootstrap path as the initial
        rush — same 80 turns, same prompt — so it CAN go just as deep. The only
        difference is this goal text, injected as a "Course correction" block. The
        old wording ("re-examine assumptions / try a different angle / from scratch")
        made the agent treat the run as exploratory reconsideration: it did a few
        probes, saw the board already covered them, and concluded "nothing new" in
        seconds (run-7349: retry workers did 0-5 tool calls vs 24-32 for bootstrap).

        So we push the OPPOSITE: the board's verified facts are a HEAD-START to build
        on, not re-derive; pick the most promising half-finished attack chain and
        DRIVE IT TO A WORKING EXPLOIT / the flag, exactly like a first-time solve.
        Dead-ends are listed only as "already ruled out — don't waste time there"."""
        deadends: list[str] = []
        sg = getattr(self, "shared_graph", None)
        if sg is not None:
            try:
                for e in sg.events():
                    if e.get("kind") == "dead_end":
                        # the reason lives in the event's JSON payload, not at the
                        # top level — reading e.get("reason") always returned "" so
                        # the dead-end list was silently empty before this fix.
                        p = e.get("payload") or {}
                        r = (p.get("reason") or p.get("text")
                             or e.get("reason") or "").strip()
                        if r:
                            deadends.append(r[:160])
            except Exception:
                deadends = []
        head = (
            "This challenge HAS a solution and is NOT yet solved. The shared board "
            "above already has verified facts — treat them as a HEAD-START, not work "
            "to redo. Pick the most promising lead or half-finished attack chain and "
            "DRIVE IT ALL THE WAY to a working exploit and the flag — run real "
            "commands, chain the steps, do not stop at recon. Go as deep as a "
            "first-time solve (you have the full turn budget). Only treat the run as "
            "done when you have the flag from real output or have genuinely exhausted "
            "this lead. If a lead is truly dead, switch to a different bug class / "
            "endpoint and push that to completion too — do not conclude after a few "
            "probes.")
        if deadends:
            body = "\n".join(f"  - {d}" for d in deadends[-12:])
            return (f"{head}\n\nAlready ruled out (do NOT retry these — pick "
                    f"something else):\n{body}")
        return head

    def _open_intents(self) -> list[dict]:
        """Intents available to (re)dispatch: never-claimed (status='open') PLUS any
        claimed intent whose LEASE EXPIRED (its worker died/stalled and never
        concluded). Closing this lease loop is what lets the swarm recover an intent
        abandoned by a stuck worker — without it, a worker that hangs holding a claim
        would orphan that intent forever (claim_intent already honors expired leases,
        but the coordinator never re-read them, so they were lost)."""
        if self.shared_graph is None:
            return []
        import time as _time
        now = _time.time()
        try:
            with self.shared_graph._lock:  # type: ignore[attr-defined]
                rows = self.shared_graph._conn.execute(  # type: ignore[attr-defined]
                    "SELECT intent_id, goal, worker_class, route_hash, branch_id, "
                    "priority, lane_key, risk_class, resource_key FROM intents "
                    # A/J: only dispatch_state='active' intents are dispatchable;
                    # resume/retired/closed are held back even when status='open'.
                    "WHERE dispatch_state='active' AND (status='open' "
                    "   OR (status='claimed' AND lease_until IS NOT NULL "
                    "       AND lease_until < ?)) "
                    # run-75377: solving intents (MS17-010, RBCD, …) starved behind a
                    # backlog of verify/review housekeeping at equal priority. Sort
                    # housekeeping (verifier/review) LAST so flag-bearing work dispatches
                    # first; priority/created_seq still break ties within each class.
                    "ORDER BY CASE WHEN worker_class IN ('verifier','review') "
                    "         THEN 1 ELSE 0 END, priority DESC, created_seq",
                    (now,),
                ).fetchall()
            out: list[dict] = []
            inferred_lanes: list[tuple[str, str, str]] = []
            seen_routes: set[str] = set()
            for r in rows:
                wc = r[2] or "code"
                route = r[3] or ""
                if (route and wc not in {"verifier", "review"}
                        and hasattr(self.shared_graph, "is_route_suppressed")
                        and self.shared_graph.is_route_suppressed(route)):
                    continue
                if route and wc not in {"verifier", "review"}:
                    if route in seen_routes:
                        continue
                    seen_routes.add(route)
                lane_key = r[6] or ""
                risk_class = r[7] or ""
                resource_key = r[8] or ""
                # E: dispatch preflight — skip an intent whose declared resource is
                # currently locked by ANOTHER worker (route around it, don't collide).
                if (resource_key and hasattr(self.shared_graph, "check_resource_conflicts")):
                    try:
                        conflict = self.shared_graph.check_resource_conflicts(
                            resource_key=resource_key)
                        if conflict.get("conflict"):
                            continue
                    except Exception:
                        pass
                if not lane_key:
                    hint = self._lane_hint_from_text(
                        str(r[1] or ""), require_control_hint=True)
                    lane_key = str(hint.get("lane_key") or "")
                    if lane_key:
                        risk_class = str(hint.get("risk_class") or risk_class or "")
                        inferred_lanes.append((lane_key, risk_class, str(r[0] or "")))
                out.append({
                    "intent_id": r[0],
                    "goal": r[1],
                    "worker_class": wc,
                    "route_hash": route,
                    "branch_id": r[4] or "",
                    "priority": int(r[5] or 0),
                    "lane_key": lane_key,
                    "risk_class": risk_class,
                    "resource_key": resource_key,
                })
            if inferred_lanes:
                with self.shared_graph._lock:  # type: ignore[attr-defined]
                    for lane_key, risk_class, intent_id in inferred_lanes:
                        self.shared_graph._conn.execute(  # type: ignore[attr-defined]
                            "UPDATE intents SET lane_key=?, risk_class=? "
                            "WHERE challenge_id=? AND intent_id=? "
                            "AND (lane_key IS NULL OR lane_key='')",
                            (lane_key, risk_class or lane_key.split(":", 1)[0],
                             self.challenge.id, intent_id),
                        )
                    self.shared_graph._conn.commit()  # type: ignore[attr-defined]
            return out
        except Exception:
            return []

    def _ordinary_open_queue_depth(self, open_intents: Optional[list[dict]] = None) -> int:
        intents = self._open_intents() if open_intents is None else open_intents
        return sum(
            1 for it in intents
            if str(it.get("worker_class") or "code") in {"code", "shell_agent"}
        )

    def _reason_backpressure_active(self, open_intents: list[dict]) -> bool:
        return self._ordinary_open_queue_depth(open_intents) >= max(1, 2 * self.max_workers)

    def _active_review_count(self) -> int:
        # Keep done review tasks counted until the coordinator reap path releases
        # their profile/account claim. Dropping them here creates a split-brain
        # window: global review capacity looks free while the profile-specific
        # review counter is still occupied, which surfaces as a bogus
        # "configured review engine unavailable" rejection.
        return len(self._active_review_tasks)

    def _ordinary_task_count(self, tasks: dict) -> int:
        self._active_review_count()
        return sum(1 for t in tasks if t not in self._active_review_tasks)

    def _ordinary_capacity_available(self, tasks: dict) -> bool:
        return self._ordinary_task_count(tasks) < self.max_workers

    def _review_capacity_available(self) -> bool:
        return self._active_review_count() < int(self.review_policy.get("max_concurrent") or 1)

    def _dispatchable_open_intents(self, open_intents: list[dict]) -> list[dict]:
        if self._review_capacity_available():
            return open_intents
        return [
            it for it in open_intents
            if str(it.get("worker_class") or "code") != "review"
        ]

    def _capacity_dispatchable_open_intents(
        self, open_intents: list[dict], tasks: dict
    ) -> list[dict]:
        ordinary_free = self._ordinary_capacity_available(tasks)
        review_free = self._review_capacity_available()
        out: list[dict] = []
        for it in open_intents:
            wc = str(it.get("worker_class") or "code")
            if wc == "review":
                if review_free:
                    out.append(it)
            elif ordinary_free:
                out.append(it)
        return out

    async def _run_reason(self) -> int:
        """Reason phase: pro model reads the board, proposes intents. Returns the
        number of new intents proposed. Advisory — never raises into the loop.

        Side effect: stashes the latest verdict/drift in self._last_reason so the
        coordinator can act on a course_correct (phase 7: adaptive re-bootstrap)."""
        if self.shared_graph is None or self.llm is None:
            return 0
        try:
            from muteki.solver.reason import run_reason, dispatch_intents
            # P1.5: un-blind the planner. The default max_evidence=16 hard-capped
            # Reason at the last 16 facts (swarm re-planned against a truncated view
            # and kept dispatching re-work — a co-equal root cause of the long-chain
            # re-discovery in run-10067). to_reason_summary renders the FULL board
            # (all facts AND all dead-ends — the old call left dead-ends clipped to
            # the last 8) PLUS the in-flight and attempted-with-results intent
            # sections, so the planner stops re-proposing directions that are
            # already running or already concluded (run-11190 paraphrase churn).
            # [#seq] fact labels survive — they are Reason's `from`-citation
            # mechanism (the {fact_ids} allow-list a plan may cite).
            summary = self.shared_graph.to_reason_summary(
                standing_guidance=list(self._standing_guidance))
            try:
                fact_index = self.shared_graph.fact_pin_context()
            except Exception:
                fact_index = ""
            result = await run_reason(
                llm=self.llm, model=self.reason_model, graph_summary=summary,
                fact_index=fact_index,
                max_intents=4, run_id=self.run_id, challenge_id=self.challenge.id,
                # pentest → judge completion against the operator's engagement goal
                # (CTF passes mode="ctf" + no goal → the prompt is byte-identical).
                mode=getattr(self.challenge, "mode", "ctf"),
                goal=(getattr(self.challenge, "goal", "") or None),
            )
            self._last_reason = result
            try:
                pins = getattr(result, "pinned_facts", []) or []
                if pins:
                    self.shared_graph.pin_facts(
                        actor="reason", fact_seqs=list(pins),
                        reason="reason model selected durable retention facts")
            except Exception:
                pass
            proposed = dispatch_intents(self.shared_graph, result, actor="reason")
            for it in proposed:
                if self.bus is not None:
                    await self.bus.emit(Event(
                        event_type=EventType.BLACKBOARD_DELTA,
                        run_id=self.run_id, challenge_id=self.challenge.id,
                        payload=blackboard_delta_payload(
                            "intent_proposed", actor="reason",
                            intent_id=it["intent_id"], goal=it["goal"],
                            worker_class=it["worker_class"],
                            from_facts=it.get("from_facts", [])),
                    ))
                # zh gist for the (often long, English) Reason goal — reuse the
                # planner's own llm client; fire-and-forget so planning isn't held up.
                self._summarize_intent_async(it["intent_id"], it["goal"])
            return len(proposed)
        except Exception:
            return 0

    def _summarize_intent_async(self, intent_id: str, goal: str) -> None:
        """Fire-and-forget a deepseek-flash zh gist for a Reason intent goal."""
        if self.bus is None or len((goal or "").strip()) < 48:
            return
        from muteki.solver.summarizer import summarize_node
        try:
            asyncio.create_task(summarize_node(
                goal, node_kind="intent", intent_id=intent_id,
                shared_graph=self.shared_graph, llm=self.llm,
                bus=self.bus, run_id=self.run_id, challenge_id=self.challenge.id))
        except RuntimeError:
            pass

    async def _emit_coord_bb(self, kind: str, **fields) -> None:
        """Coordinator-scoped blackboard delta (shared by the race-scout phase and
        the main loop's local _emit_bb)."""
        if self.bus is None:
            return
        try:
            await self.bus.emit(Event(
                event_type=EventType.BLACKBOARD_DELTA, run_id=self.run_id,
                challenge_id=self.challenge.id,
                payload=blackboard_delta_payload(kind, actor="coordinator", **fields)))
        except Exception:
            pass

    async def _run_race_scout(self, healthy: list[str]):
        """Race-scout layer (DESIGN_race_scout_layer.md): ONE round of fresh
        single-shot bootstrap workers (one per race engine) probing the whole
        challenge IN PARALLEL. Each runs to its own natural exit (single-shot, short
        race_timeout) and lands its facts/flag on the shared graph. Returns
        (winner_id, flag, per_solver). On the FAST PATH winner_id is set (a worker
        captured the flag and the run is flags-complete); else (None, None,
        per_solver) → the facts are on the graph and the caller falls through to the
        coordinator loop, warm. per_solver carries the race workers' outcomes either
        way.

        Single-shot + no global-signal reclaim: this never reintroduces the run-7352
        death spiral (red line). One round only — race_rounds>1 is intentionally not
        looped here (it would reintroduce accumulation)."""
        engines = [
            e for e in (self.race_engines or self.engines)
            if (self._healthy_matches(e, healthy)
                and self._engine_available_for_role(e, "race"))
        ]
        if not engines:
            return None, None, {}
        await self._emit_coord_bb("race_started", engines=list(engines),
                                  timeout=self.race_timeout)
        workers = []
        for e in engines:
            try:
                workers.append(self._make_cli_worker(
                    e, mode="bootstrap", timeout_override=self.race_timeout,
                    profile_role="race"))
            except WorkerSpawnRejected as exc:
                await self._emit_coord_bb("worker_spawn_rejected", reason=str(exc),
                                          engine=str(e), phase="race")
                continue
            except WorkerBudgetExhausted as exc:
                await self._emit_coord_bb(str(exc), spawned_total=self._spawned_total,
                                          max_total_workers=self.max_total_workers,
                                          cost_usd=self._current_cost_usd(),
                                          cost_budget_usd=self.cost_budget_usd)
                break
        if not workers:
            return None, None, {}
        for w in workers:
            await self._emit_coord_bb("worker_spawned", worker=w.solver_id,
                                      phase="race", worker_role="worker")
        tasks = {asyncio.create_task(w.run(), name=f"race-{w.solver_id}"): w for w in workers}
        op_task = (asyncio.create_task(self._operator_event.wait(), name="race-operator-stop")
                   if self._operator_event is not None else None)
        results_by_worker: dict[Any, Any] = {}
        try:
            pending = set(tasks.keys())
            while pending:
                wait_set = set(pending)
                if op_task is not None and not op_task.done():
                    wait_set.add(op_task)
                done, _ = await asyncio.wait(wait_set, return_when=asyncio.FIRST_COMPLETED)
                if op_task is not None and op_task in done:
                    if self._operator_stop:
                        for t, w in tasks.items():
                            if not t.done():
                                self._cancel_solver(w)
                                t.cancel()
                        break
                    op_task = asyncio.create_task(self._operator_event.wait(), name="race-operator-stop")
                    done.discard(op_task)
                for t in [d for d in done if d in pending]:
                    pending.discard(t)
                    try:
                        results_by_worker[tasks[t]] = t.result()
                    except BaseException as exc:  # noqa: BLE001
                        results_by_worker[tasks[t]] = exc
        finally:
            for t, w in tasks.items():
                if not t.done():
                    self._cancel_solver(w)
                    t.cancel()
            if tasks:
                await asyncio.gather(*tasks.keys(), return_exceptions=True)
            if op_task is not None:
                op_task.cancel()
                await asyncio.gather(op_task, return_exceptions=True)
            for w in workers:
                self._release_worker_account(w)

        winner: "Optional[str]" = None
        flag: "Optional[str]" = None
        per_solver: "dict[str, SolveOutcome]" = {}
        for w in workers:
            res = results_by_worker.get(w, asyncio.CancelledError())
            sid = getattr(w, "solver_id", None) or "cli-?"
            if isinstance(res, BaseException):
                await self._emit_coord_bb("worker_finished", worker=sid,
                                          result="error", phase="race")
                continue
            per_solver[sid] = res
            await self._emit_coord_bb(
                "worker_finished", worker=sid, phase="race",
                result="solved" if getattr(res, "solved", False) else "done")
            # tally every flag this worker produced (multi-flag safe). _record_flags
            # dedups; the flags are already on the shared graph via _accept_flag.
            self._record_flags(*(getattr(res, "flags", None) or
                                 ([res.flag] if getattr(res, "flag", None) else [])))
            if self._flags_complete() and winner is None:
                winner, flag = sid, self._found_flags[0]

        # split-brain reconcile (BUG②): a race worker cancelled right after it
        # accepted a flag is reaped as CancelledError above and never tallied — fold
        # the authoritative graph snapshot in so completion still fires.
        if winner is None:
            self._sync_flags_from_graph()
            if self._flags_complete():
                winner = "race"
                flag = self._found_flags[0] if self._found_flags else None

        await self._emit_coord_bb(
            "race_concluded", solved=winner is not None,
            flags=len(self._found_flags))
        return winner, flag, per_solver

    def _current_graph_seq(self) -> int:
        if self.shared_graph is None:
            return 0
        try:
            evs = self.shared_graph.events()
            return int(evs[-1]["seq"]) if evs else 0
        except Exception:
            return 0

    def _select_review_engine(self, healthy: list[str]) -> str:
        configured = str(self.review_policy.get("engine") or "").strip()
        if configured:
            candidates: list[str] = []
            if self.worker_profiles:
                candidates = normalize_profile_roster([configured], self.worker_profiles)
                if configured in getattr(self, "_profiles_by_name", {}):
                    candidates = [configured] + [c for c in candidates if c != configured]
            else:
                candidates = [configured]
            for e in candidates:
                if self._healthy_matches(e, healthy) and self._engine_available_for_role(e, "review"):
                    return e
            if (self._healthy_matches(configured, healthy)
                    and self._engine_available_for_role(configured, "review")):
                return configured
            if not self.review_policy.get("allow_review_fallback", False):
                raise RuntimeError(
                    f"configured review engine unavailable: {configured}")
        return self._pick_engine([], healthy, role="review")

    def _queue_review_request(self, *, trigger: str, directive: str) -> None:
        if not self.review_policy.get("enabled", True):
            return
        trigger = (trigger or "review").strip()[:80]
        directive = (directive or "").strip()
        if not directive:
            return
        item = {"trigger": trigger, "directive": directive}
        if item in self._queued_review_requests:
            return
        self._queued_review_requests.append(item)
        if len(self._queued_review_requests) > 16:
            self._queued_review_requests = self._queued_review_requests[-16:]

    @staticmethod
    def _lane_hint_from_text(text: str, *, worker: str = "",
                             require_control_hint: bool = False) -> dict[str, Any]:
        text = text or ""
        low = text.lower()
        direct = re.search(
            r"\b(?P<risk>[a-z_][a-z0-9_-]*):tcp:"
            r"(?P<port>\*|[1-9]\d{0,4})@"
            r"(?P<host>(?:\d{1,3}\.){3}\d{1,3}|[a-z0-9][a-z0-9.-]{0,252})\b",
            low,
        )
        if direct:
            lane, confidence, degradation_reason = canonicalize_lane(
                host=direct.group("host"),
                port=None if direct.group("port") == "*" else direct.group("port"),
                service="",
                risk_class=direct.group("risk"),
            )
            risk_class = lane.split(":", 1)[0] if lane else direct.group("risk")
            return {
                "lane_key": lane,
                "risk_class": risk_class,
                "confidence": confidence,
                "degradation_reason": degradation_reason,
                "reason": text[:1000],
                "owner_worker": worker,
            }
        if require_control_hint and not any(k in low for k in (
            "lane", "destructive", "exclusive", "serialize", "serialized",
            "sequential", "one request", "single request", "single-request",
            "rate-limit", "rate sensitive", "rate-sensitive", "holds the",
            "under the", "同一", "独占", "串行", "序列化",
        )):
            return {"lane_key": "", "risk_class": "", "confidence": 0.0,
                    "degradation_reason": "no_control_hint", "reason": text[:1000],
                    "owner_worker": worker}
        host = ""
        m = re.search(r"\b(?:\d{1,3}\.){3}\d{1,3}\b", text)
        if m:
            host = m.group(0)
        else:
            hm = re.search(r"\b([a-z0-9][a-z0-9.-]+\.[a-z]{2,})\b", low)
            if hm:
                host = hm.group(1)
        if require_control_hint and not host:
            return {"lane_key": "", "risk_class": "", "confidence": 0.0,
                    "degradation_reason": "no_host", "reason": text[:1000],
                    "owner_worker": worker}
        service = ""
        port: str | int | None = None
        if any(k in low for k in ("smb", "445", "eternalblue", "ms17", "relay", "responder")):
            service, port = "smb", 445
        elif "winrm" in low or "5985" in low:
            service, port = "winrm", 5985
        elif "rdp" in low or "3389" in low:
            service, port = "rdp", 3389
        elif "http" in low or "web" in low:
            service = "https" if "https" in low or "443" in low else "http"
            port = 443 if service == "https" else 80
        pm = re.search(r"(?<!\d)([1-9]\d{1,4})(?!\d)", low)
        if pm and not port:
            try:
                p = int(pm.group(1))
                if 0 < p <= 65535:
                    port = p
            except ValueError:
                port = None
        risk = "relay_service" if any(k in low for k in ("relay", "responder")) else "destructive"
        lane, confidence, degradation_reason = canonicalize_lane(
            host=host, port=port, service=service, risk_class=risk)
        return {
            "lane_key": lane,
            "risk_class": risk,
            "confidence": confidence,
            "degradation_reason": degradation_reason,
            "reason": text[:1000],
            "owner_worker": worker,
        }

    @staticmethod
    def _lane_proposal_from_need(need: str, worker: str = "") -> dict[str, Any]:
        return Swarm._lane_hint_from_text(need, worker=worker)

    @staticmethod
    def _mechanical_need_kind(text: str) -> str:
        low = (text or "").lower()
        if any(k in low for k in (
            "ask operator", "operator decide", "need a decision from",
            "需要 operator",
        )):
            return "operator_directive_needed"
        if any(k in low for k in (
            "exclusive", "serialize", "another worker", "same target",
            "stop hammering", "独占", "序列化", "其他 worker", "其它 worker",
        )):
            return "lane_lock_request"
        if any(k in low for k in (
            "dead end", "dead-end", "route dead", "route failed",
            "known dead", "no longer viable", "repeated failures",
            "走死", "已知失败",
        )):
            return "route_dead_end"
        if any(k in low for k in (
            "unreachable", "connection refused", "refused", "timed out",
            "timeout", "expired", "instance", "502", "503", "down",
            "credential", "vps", "attachment", "token", "runtime",
            "container", "凭据", "附件",
        )):
            return "external_blocker"
        return "worker_uncertainty"

    @classmethod
    def _rechecked_need_kind(cls, need_text: str, proposed_kind: str) -> str:
        valid = {
            "external_blocker",
            "operator_directive_needed",
            "lane_lock_request",
            "route_dead_end",
            "worker_uncertainty",
        }
        proposed = (proposed_kind or "").strip().lower()
        if proposed not in valid:
            return cls._mechanical_need_kind(need_text)
        if proposed == "external_blocker":
            return cls._mechanical_need_kind(need_text)
        return proposed

    async def _consume_lane_release(self, rel: dict, *, emit_bb) -> None:
        if not rel:
            return
        lane = str(rel.get("lane_key") or "")
        for iid in rel.get("revived", []) or []:
            try:
                await emit_bb("lane_revived", intent_id=str(iid), lane_key=lane)
            except Exception:
                pass
        for iid in rel.get("escalated", []) or []:
            self._queue_review_request(
                trigger="lane_blocked",
                directive=(
                    f"lane {lane} 上 intent {iid} 长期争用；"
                    "请审查当前路线，提出绕开该资源或重新排序的 NEXT_INTENT。"
                ),
            )

    async def _maybe_start_review(
        self,
        *,
        trigger: str,
        directive: str,
        healthy: list[str],
        tasks: dict,
        task_solvers: dict,
        emit_bb,
    ) -> bool:
        if not self.review_policy.get("enabled", True):
            return False
        if self._flags_complete():
            return False
        if not self._review_capacity_available():
            return False
        if self._review_workers_spawned >= int(self.review_policy.get("max_review_workers") or 12):
            return False
        seq = self._current_graph_seq()
        cooldown = int(self.review_policy.get("cooldown_events") or 0)
        if (self._last_review_seq > 0
                and seq <= self._last_review_seq + cooldown
                and trigger != "course_correct"):
            return False
        try:
            engine = self._select_review_engine(healthy)
        except RuntimeError as exc:
            await emit_bb("worker_spawn_rejected", reason=str(exc), phase="review")
            return False
        try:
            w = self._make_cli_worker(
                engine, mode="review", intent_goal=directive)
        except WorkerSpawnRejected as exc:
            await emit_bb("worker_spawn_rejected", reason=str(exc),
                          engine=str(engine), phase="review")
            return False
        except WorkerBudgetExhausted as exc:
            await emit_bb(str(exc), spawned_total=self._spawned_total,
                          max_total_workers=self.max_total_workers,
                          cost_usd=self._current_cost_usd(),
                          cost_budget_usd=self.cost_budget_usd)
            return False
        t = asyncio.create_task(w.run(), name=f"review-{engine}")
        tasks[t] = engine
        task_solvers[t] = w
        self._active_review_tasks.add(t)
        self._review_workers_spawned += 1
        self._last_review_seq = seq
        self._completed_workers_since_review = 0
        self._last_candidate_review_count = self._candidate_fact_count()
        await emit_bb("review_started", trigger=trigger, worker=w.solver_id,
                      engine=str(engine), directive=directive[:300])
        await emit_bb("worker_spawned", worker=w.solver_id,
                      phase="review", worker_role="review")
        return True

    async def _maybe_run_queued_review(
        self,
        *,
        healthy: list[str],
        tasks: dict,
        task_solvers: dict,
        emit_bb,
    ) -> bool:
        if not self._queued_review_requests:
            return False
        req = self._queued_review_requests[0]
        started = await self._maybe_start_review(
            trigger=req.get("trigger", "queued_review"),
            directive=req.get("directive", ""),
            healthy=healthy, tasks=tasks,
            task_solvers=task_solvers, emit_bb=emit_bb,
        )
        if started:
            self._queued_review_requests.pop(0)
            return True
        return False

    async def _drain_resource_locks(self, *, emit_bb) -> None:
        """E: mirror new resource_locked / resource_released events (workers acquire
        them via the blackboard skill) onto the board as resource_lock_changed deltas
        so the deck renders held resource locks live."""
        if self.shared_graph is None:
            return
        try:
            events = self.shared_graph.events()
        except Exception:
            return
        for ev in events:
            seq = int(ev.get("seq") or 0)
            if seq <= self._last_resource_seq:
                continue
            kind = ev.get("kind")
            if kind not in ("resource_locked", "resource_released"):
                continue
            self._last_resource_seq = max(self._last_resource_seq, seq)
            p = dict(ev.get("payload") or {})
            try:
                await emit_bb(
                    "resource_lock_changed",
                    lock_id=p.get("lock_id", ""),
                    resource_key=p.get("resource_key", ""),
                    scope=p.get("scope", "activity"),
                    risk_class=p.get("risk_class", ""),
                    owner_worker=p.get("owner_worker") or ev.get("actor", ""),
                    status=("released" if kind == "resource_released" else "active"))
            except Exception:
                pass

    async def _drain_review_proposals(self, *, emit_bb, fruitless_workers: int = 0) -> int:
        if self.shared_graph is None:
            return 0
        try:
            events = self.shared_graph.events()
        except Exception:
            return 0
        proposals = [
            e for e in events
            if e.get("kind") == "review_proposal"
            and int(e.get("seq") or 0) > self._last_review_proposal_seq
        ]
        if not proposals:
            return 0
        applied = 0
        # run-75377: a single review cycle could emit dozens of FACT_CHALLENGE /
        # NEXT_INTENT, flooding the backlog with new (mostly verify) intents that then
        # starved solving. Cap the per-cycle fan-out of intent-creating markers; the
        # rest of the cycle only records REVIEW_FINDING. Eliminate-only markers
        # (FACT_MERGE/SUPERSEDE/REJECT, REVIEW_FINDING) are NOT counted — they shrink
        # backlog, not grow it. Counter is local so it resets every drain cycle.
        # A configured 0 genuinely disables challenge fan-out for the cycle (0 >= 0 is
        # immediately true, so every challenge-creating marker is recorded-only); don't
        # collapse it to the default. _clean_review_policy already supplies 8 when the
        # key is absent, so this get() only falls back for a non-dict review_policy.
        raw_budget = self.review_policy.get("max_challenges_per_cycle", 8)
        try:
            challenge_budget = max(0, int(raw_budget))
        except (TypeError, ValueError):
            challenge_budget = 8
        fanout_used = 0
        for ev in proposals:
            seq = int(ev.get("seq") or 0)
            self._last_review_proposal_seq = max(self._last_review_proposal_seq, seq)
            p = dict(ev.get("payload") or {})
            marker = str(p.get("marker") or "").upper()
            payload = dict(p.get("payload") or {})
            tier = str(p.get("tier") or "tier1")
            accepted = False
            reason = ""
            applied_seq: Optional[int] = None
            try:
                if tier == "tier2" and marker == "ROUTE_SUPPRESS":
                    route = str(payload.get("route_hash") or "")
                    failures = 0
                    try:
                        failures = int(self.shared_graph.genuine_failures_for_route(route))  # type: ignore[attr-defined]
                    except Exception:
                        failures = 0
                    confidence = float(payload.get("confidence", 1.0) or 1.0)
                    accepted = failures >= 3 and confidence >= 0.80
                    reason = f"failures={failures}, confidence={confidence:.2f}"
                    if accepted:
                        info = self.shared_graph.suppress_route(
                            actor="coordinator",
                            route_hash=route,
                            label=str(payload.get("label") or ""),
                            reason=str(payload.get("reason") or ""),
                            until=str(payload.get("until") or "new_evidence"),
                            matching_intents=[
                                str(x) for x in payload.get("matching_intents", []) if x
                            ],
                        )
                        applied_seq = int(info.get("seq") or 0) or None
                        await emit_bb("route_suppressed", **info,
                                      label=str(payload.get("label") or ""),
                                      reason=str(payload.get("reason") or ""),
                                      proposal_seq=seq)
                elif tier == "tier2" and marker == "LANE_LOCK":
                    lane = str(payload.get("lane_key") or "")
                    owner = str(payload.get("owner_worker") or payload.get("worker") or "coordinator")
                    accepted = bool(lane) and not self.shared_graph.is_lane_held_by_other(  # type: ignore[attr-defined]
                        lane, owner)
                    reason = "lane available" if accepted else "lane already held"
                    if accepted:
                        info = self.shared_graph.lock_lane(  # type: ignore[attr-defined]
                            actor="coordinator",
                            lane_key=lane,
                            risk_class=str(payload.get("risk_class") or ""),
                            owner_worker=owner,
                            owner_intent=str(payload.get("owner_intent") or ""),
                        )
                        accepted = bool(info.get("acquired"))
                        reason = "lane locked" if accepted else "lane already held"
                        if accepted:
                            applied_seq = int(info.get("seq") or 0) or None
                            directive_seq = self.shared_graph.add_coordinator_directive(
                                actor="coordinator",
                                action="lane_lock",
                                directive=(
                                    f"lane {info.get('lane_key')} is exclusively held by {owner}; "
                                    "do not start destructive/exclusive work on that resource."
                                ),
                                priority="high",
                            )
                            await emit_bb("lane_locked", **info,
                                          proposal_seq=seq,
                                          directive_seq=directive_seq)
                elif tier == "tier2" and marker == "LANE_UNLOCK":
                    lane = str(payload.get("lane_key") or "")
                    accepted = bool(lane)
                    reason = "lane released" if accepted else "empty lane_key"
                    if accepted:
                        info = self.shared_graph.release_lane(  # type: ignore[attr-defined]
                            actor="coordinator", lane_key=lane,
                            by_worker=str(payload.get("owner_worker") or ""),
                        )
                        applied_seq = int(info.get("seq") or 0) or None
                        await emit_bb("lane_released", **info, proposal_seq=seq)
                        await self._consume_lane_release(info, emit_bb=emit_bb)
                elif tier == "tier2" and marker == "COORDINATOR_DIRECTIVE":
                    action = str(payload.get("action") or "").strip() or "note"
                    accepted = (
                        action == "rebootstrap"
                        and self.barren_limit > 0
                        and fruitless_workers >= self.barren_limit
                    )
                    reason = (
                        f"fruitless_workers={fruitless_workers}, "
                        f"barren_limit={self.barren_limit}"
                    )
                    if accepted:
                        applied_seq = self.shared_graph.add_coordinator_directive(
                            actor="coordinator",
                            action=action,
                            directive=str(payload.get("directive") or ""),
                            priority=str(payload.get("priority") or "normal"),
                            route_hash=str(payload.get("route_hash") or ""),
                        )
                        await emit_bb("coordinator_directive", seq=applied_seq,
                                      proposal_seq=seq, **payload)
                else:
                    accepted = True
                    if marker == "REVIEW_FINDING":
                        applied_seq = self.shared_graph.add_review_finding(
                            actor="coordinator",
                            kind=str(payload.get("kind") or "no_action"),
                            severity=str(payload.get("severity") or "info"),
                            summary=str(payload.get("summary") or ""),
                            evidence_seqs=[
                                int(x) for x in payload.get("evidence_seqs", [])
                                if isinstance(x, int)
                            ],
                            intent_ids=[str(x) for x in payload.get("intent_ids", []) if x],
                            route_hash=str(payload.get("route_hash") or ""),
                            branch_id=str(payload.get("branch_id") or ""),
                            recommended_actions=[
                                str(x) for x in payload.get("recommended_actions", []) if x
                            ],
                        )
                        await emit_bb("review_finding", seq=applied_seq,
                                      finding_kind=str(payload.get("kind") or "no_action"),
                                      severity=str(payload.get("severity") or "info"),
                                      summary=str(payload.get("summary") or ""),
                                      route_hash=str(payload.get("route_hash") or ""),
                                      branch_id=str(payload.get("branch_id") or ""),
                                      proposal_seq=seq)
                    elif marker == "FACT_CHALLENGE":
                        if fanout_used >= challenge_budget:
                            accepted = False
                            reason = (f"fan-out budget exhausted "
                                      f"({challenge_budget}/cycle)")
                            await emit_bb("review_fanout_skipped", marker=marker,
                                          proposal_seq=seq, budget=challenge_budget)
                        else:
                            info = self.shared_graph.challenge_fact(
                                actor="coordinator",
                                fact_seq=int(payload.get("fact_seq")),
                                reason=str(payload.get("reason") or ""),
                                verification_goal=str(payload.get("verification_goal") or ""),
                            )
                            applied_seq = int(info.get("seq") or 0) or None
                            fanout_used += 1
                            await emit_bb("fact_challenged", **info, proposal_seq=seq)
                    elif marker == "FACT_REVALIDATION":
                        applied_seq = self.shared_graph.revalidate_fact(
                            actor="coordinator",
                            fact_seq=int(payload.get("fact_seq")),
                            reason=str(payload.get("reason") or ""),
                        )
                        await emit_bb("fact_revalidated", seq=applied_seq,
                                      fact_seq=int(payload.get("fact_seq")),
                                      reason=str(payload.get("reason") or ""),
                                      proposal_seq=seq)
                    elif marker == "FACT_REJECT":
                        # A: review proved a candidate false → retire it. Only the
                        # candidate view dims; the originating event stays (audit).
                        # Reviewer can never set solved / kill workers, so this is
                        # safe to auto-adopt alongside challenge/revalidate.
                        fseq = int(payload.get("fact_seq"))
                        applied_seq = self.shared_graph.reject_fact(
                            actor="coordinator", fact_seq=fseq,
                            reason=str(payload.get("reason") or ""))
                        await emit_bb("fact_rejected", seq=applied_seq,
                                      fact_seq=fseq,
                                      reason=str(payload.get("reason") or ""),
                                      proposal_seq=seq)
                    elif marker == "FACT_MERGE":
                        # A: fold a duplicate finding into its canonical fact.
                        from_seq = int(payload.get("fact_seq")
                                       if payload.get("fact_seq") is not None
                                       else payload.get("from_fact_seq"))
                        to_seq = int(payload.get("to_fact_seq") or 0)
                        applied_seq = self.shared_graph.merge_fact(
                            actor="coordinator", from_fact_seq=from_seq,
                            to_fact_seq=to_seq,
                            reason=str(payload.get("reason") or ""))
                        if applied_seq is not None and applied_seq < 0:
                            accepted = False
                            reason = "merge into self / invalid to_fact_seq"
                            applied_seq = None
                        else:
                            await emit_bb("fact_merged", seq=applied_seq,
                                          from_fact_seq=from_seq, to_fact_seq=to_seq,
                                          reason=str(payload.get("reason") or ""),
                                          proposal_seq=seq)
                    elif marker == "FACT_SUPERSEDE":
                        # A: a newer fact replaces this one → retire the old.
                        fseq = int(payload.get("fact_seq"))
                        by_seq = payload.get("by_fact_seq") or payload.get("to_fact_seq")
                        applied_seq = self.shared_graph.supersede_fact(
                            actor="coordinator", fact_seq=fseq,
                            reason=str(payload.get("reason") or ""),
                            by_fact_seq=int(by_seq) if by_seq is not None else None)
                        await emit_bb("fact_superseded", seq=applied_seq,
                                      fact_seq=fseq,
                                      reason=str(payload.get("reason") or ""),
                                      proposal_seq=seq)
                    elif marker == "ROUTE_REOPEN":
                        info = self.shared_graph.reopen_route(
                            actor="coordinator",
                            route_hash=str(payload.get("route_hash") or ""),
                            reason=str(payload.get("reason") or ""),
                            intent_goal=str(payload.get("intent_goal") or payload.get("goal") or ""),
                        )
                        applied_seq = int(info.get("seq") or 0) or None
                        await emit_bb("route_reopened", **info,
                                      reason=str(payload.get("reason") or ""),
                                      proposal_seq=seq)
                    elif marker == "BRANCH_SPLIT":
                        info = self.shared_graph.split_branch(
                            actor="coordinator",
                            title=str(payload.get("title") or ""),
                            branches=list(payload.get("branches") or []),
                        )
                        applied_seq = int(info.get("seq") or 0) or None
                        await emit_bb("branch_split", **info,
                                      title=str(payload.get("title") or ""),
                                      proposal_seq=seq)
                    elif marker == "BRANCH_RESOLVE":
                        info = self.shared_graph.resolve_branch(  # type: ignore[attr-defined]
                            actor="coordinator",
                            branch_id=str(payload.get("branch_id") or ""),
                            reason=str(payload.get("reason") or ""),
                            status=str(payload.get("status") or "resolved"),
                        )
                        applied_seq = int(info.get("seq") or 0) or None
                        await emit_bb("branch_resolved", **info, proposal_seq=seq)
                    elif marker == "NEXT_INTENT":
                        goal = str(payload.get("goal") or "").strip()
                        if not goal:
                            accepted = False
                            reason = "empty goal"
                        elif fanout_used >= challenge_budget:
                            accepted = False
                            reason = (f"fan-out budget exhausted "
                                      f"({challenge_budget}/cycle)")
                            await emit_bb("review_fanout_skipped", marker=marker,
                                          proposal_seq=seq, budget=challenge_budget)
                        else:
                            iid = str(payload.get("id") or payload.get("intent_id") or "")
                            if not iid:
                                iid = "I-review-" + hashlib.sha1(
                                    goal.encode("utf-8", "ignore")
                                ).hexdigest()[:8]
                            wc = str(payload.get("worker_class") or "code")
                            lane_key = str(payload.get("lane_key") or "").strip()
                            risk_class = str(payload.get("risk_class") or "").strip()
                            if not lane_key:
                                lane_hint = self._lane_hint_from_text(
                                    goal, require_control_hint=True)
                                lane_key = str(lane_hint.get("lane_key") or "")
                                if lane_key and not risk_class:
                                    risk_class = str(lane_hint.get("risk_class") or "")
                            applied_seq = self.shared_graph.propose_intent(
                                actor="coordinator", intent_id=iid, goal=goal,
                                payload={
                                    "worker_class": wc,
                                    "route_hash": str(payload.get("route_hash") or ""),
                                    "branch_id": str(payload.get("branch_id") or ""),
                                    "lane_key": lane_key,
                                    "risk_class": risk_class,
                                    "rationale": str(payload.get("rationale") or "review proposed"),
                                    "depends_on": [
                                        str(x) for x in payload.get("depends_on", []) if x
                                    ],
                                },
                                from_fact_seqs=[
                                    int(x) for x in payload.get("from", [])
                                    if isinstance(x, int)
                                ] or None,
                            )
                            fanout_used += 1
                            await emit_bb("intent_proposed", intent_id=iid,
                                          goal=goal, worker_class=wc,
                                          route_hash=str(payload.get("route_hash") or ""),
                                          branch_id=str(payload.get("branch_id") or ""),
                                          lane_key=lane_key,
                                          risk_class=risk_class,
                                          proposal_seq=seq)
                    else:
                        accepted = False
                        reason = f"unsupported marker {marker}"
                decision = "accepted" if accepted else "deferred"
                self.shared_graph.decide_review_proposal(  # type: ignore[attr-defined]
                    actor="coordinator", proposal_seq=seq, decision=decision,
                    reason=reason, applied_seq=applied_seq)
                await emit_bb("review_proposal_decision", proposal_seq=seq,
                              marker=marker, decision=decision, reason=reason,
                              applied_seq=applied_seq)
                if accepted:
                    applied += 1
            except Exception as exc:  # noqa: BLE001
                try:
                    self.shared_graph.decide_review_proposal(  # type: ignore[attr-defined]
                        actor="coordinator", proposal_seq=seq,
                        decision="rejected", reason=str(exc)[:500])
                    await emit_bb("review_proposal_decision", proposal_seq=seq,
                                  marker=marker, decision="rejected",
                                  reason=str(exc)[:500])
                except Exception:
                    pass
        return applied

    async def _spawn_rebootstrap_from_directive(
        self,
        *,
        healthy: list[str],
        tasks: dict,
        task_solvers: dict,
        running_engines_fn,
        emit_bb,
    ) -> bool:
        if self.shared_graph is None or not self._ordinary_capacity_available(tasks):
            return False
        try:
            directive = self.shared_graph.latest_unconsumed_directive_seq(
                after_seq=self._last_directive_seq, action="rebootstrap")
        except Exception:
            directive = None
        if not directive:
            return False
        text = str(directive.get("directive") or "").strip()
        self._last_directive_seq = int(directive.get("seq") or self._last_directive_seq)
        if not text:
            return False
        try:
            engine = self._pick_engine(running_engines_fn(), healthy, role="bootstrap")
        except RuntimeError as exc:
            await emit_bb("worker_spawn_rejected", reason=str(exc),
                          phase="review_directive")
            return False
        try:
            w = self._make_cli_worker(engine, mode="bootstrap", intent_goal=text)
        except WorkerSpawnRejected as exc:
            await emit_bb("worker_spawn_rejected", reason=str(exc),
                          engine=str(engine), phase="review_directive")
            return False
        except WorkerBudgetExhausted as exc:
            await emit_bb(str(exc), spawned_total=self._spawned_total,
                          max_total_workers=self.max_total_workers,
                          cost_usd=self._current_cost_usd(),
                          cost_budget_usd=self.cost_budget_usd)
            return False
        t = asyncio.create_task(w.run(), name=f"review-directive-{engine}")
        tasks[t] = engine
        task_solvers[t] = w
        await emit_bb("coordinator_directive", action="rebootstrap",
                      directive=text[:500], priority=directive.get("priority", "normal"))
        await emit_bb("worker_spawned", worker=w.solver_id,
                      phase="review_directive", worker_role="worker")
        return True

    async def _run_coordinator(self) -> SwarmOutcome:
        """The evidence-driven plan / dispatch loop. See class header."""
        import time

        # event the coordinator waits on when it pauses for operator help; set by
        # _drain_hitl on any operator command.
        self._operator_event = asyncio.Event()
        # sink: capture workers' HITL_REQUEST (NEED_INPUT / env_down) off the shared
        # bus so the coordinator knows a direction is blocked on the operator. Mirror
        # of RunManager's meta sink. Best-effort; never raises into a worker's emit.
        if self.bus is not None:
            async def _help_sink(ev: Event) -> None:
                if ev.event_type is EventType.HITL_REQUEST:
                    # M6: dedup on (worker, need) and cap the list. The per-worker
                    # marker dedup is per-worker, so the SAME blocker raised by N
                    # workers (or re-emitted by a re-bootstrapped worker) used to
                    # append N entries — inflating the awaiting_operator `count`,
                    # pushing the earliest (often most important) asks past the
                    # [-3:] summary window, and growing unbounded on a never-give-up
                    # run. Keyed dedup + cap fixes all three.
                    payload = dict(ev.payload or {})
                    need_kind = str(payload.get("need_kind") or "external_blocker")
                    need_text = str(payload.get("need", "")).strip()
                    worker = str(payload.get("worker", ""))
                    need_kind = self._rechecked_need_kind(need_text, need_kind)
                    payload["need_kind"] = need_kind
                    # F: persist the classification (need_kind) so the deck can render
                    # auto-resolving kinds differently from a true operator blocker,
                    # and the audit trail shows how each hand-raise was triaged.
                    if self.shared_graph is not None and need_text:
                        try:
                            self.shared_graph.add_hitl_request(
                                worker=worker or "worker", need=need_text,
                                need_kind=need_kind,
                                classification_confidence=float(
                                    payload.get("classification_confidence") or 1.0),
                                status=("awaiting_operator"
                                        if need_kind == "external_blocker"
                                        else "auto_resolved"))
                        except Exception:
                            pass
                    # emit the classification delta OUT-OF-BAND (scheduled, not awaited):
                    # this sink runs INSIDE bus.emit, so awaiting another emit here would
                    # re-enter the bus and reorder/deadlock the NEED_INPUT pause path.
                    try:
                        asyncio.create_task(self._emit_coord_bb(
                            "hitl_classified", worker=worker, need=need_text[:200],
                            need_kind=need_kind,
                            pauses_behavior=(need_kind == "external_blocker")))
                    except Exception:
                        pass
                    if need_kind == "lane_lock_request":
                        if self.shared_graph is not None:
                            lane_payload = self._lane_proposal_from_need(need_text, worker)
                            if lane_payload.get("lane_key"):
                                try:
                                    self.shared_graph.add_review_proposal(
                                        actor=worker or "worker",
                                        marker="LANE_LOCK",
                                        payload=lane_payload,
                                        tier="tier2",
                                    )
                                except Exception:
                                    pass
                            else:
                                self._pending_uncertainty_reviews.append(payload)
                        return
                    if need_kind == "route_dead_end":
                        if self.shared_graph is not None:
                            try:
                                route = SQLiteSharedGraph.normalize_route_hash(
                                    "", label=need_text)
                                self.shared_graph.add_review_proposal(
                                    actor=worker or "worker",
                                    marker="ROUTE_SUPPRESS",
                                    payload={
                                        "route_hash": route,
                                        "label": need_text[:120],
                                        "reason": need_text[:1000],
                                        "confidence": 0.85,
                                    },
                                    tier="tier2",
                                )
                            except Exception:
                                pass
                        return
                    if need_kind == "worker_uncertainty":
                        if self.shared_graph is not None:
                            try:
                                self.shared_graph.add_evidence(
                                    actor=worker or "worker",
                                    source="need_input",
                                    fact=f"Worker uncertainty: {need_text[:500]}",
                                    verified=False,
                                    confidence=0.30,
                                )
                            except Exception:
                                pass
                        self._pending_uncertainty_reviews.append(payload)
                        return
                    key = (str(payload.get("worker", "")),
                           str(payload.get("need", "")).strip())
                    for h in self._pending_help:
                        if (str(h.get("worker", "")),
                                str(h.get("need", "")).strip()) == key:
                            break  # already pending — don't duplicate
                    else:
                        self._pending_help.append(payload)
                        if len(self._pending_help) > _PENDING_HELP_MAX:
                            del self._pending_help[
                                : len(self._pending_help) - _PENDING_HELP_MAX]
                        # translate the (often English) hand-raise to zh in the
                        # background so the operator reads it more easily — same
                        # fire-and-forget pattern as node summaries; the deck swaps
                        # the card text to zh when HITL_TRANSLATED arrives. Only the
                        # FIRST occurrence of a (worker, need) is translated (we're in
                        # the dedup-miss branch), so a re-raise won't re-translate.
                        if self.llm is not None:
                            try:
                                from muteki.solver.summarizer import translate_need
                                asyncio.create_task(translate_need(
                                    str(payload.get("need", "")),
                                    worker=str(payload.get("worker", "")),
                                    model=self.titler_model, llm=self.llm,
                                    bus=self.bus, run_id=self.run_id,
                                    challenge_id=self.challenge.id))
                            except Exception:
                                pass
            try:
                self.bus.add_sink(_help_sink)
                self._coord_sinks.append(_help_sink)  # L3: detach on finalize
            except Exception:
                pass

        # ── submission gate (only when the target rate-limits its verifier) ──────
        # Serialize submissions across the swarm: when a worker declares
        # READY_TO_SUBMIT (board delta kind "ready_to_submit"), broadcast
        # SUBMIT_LOCKED so every OTHER worker holds its own submission, then
        # auto-release after a short lease (the submitting worker runs the verifier
        # within its current turn and can't explicitly hand the lock back). A worker
        # that detects a cooldown broadcasts VERIFIER_LOCKED independently
        # (cli_solver._maybe_broadcast_lockout) — the workers honor it via
        # _drain_control. The coordinator's job is the GRANT-time serialization +
        # not piling on redundant submitters. Default-off: ordinary CTFs never
        # register this sink, so their path is byte-identical.
        self._submit_lock_until = 0.0
        if self.bus is not None and getattr(self.challenge, "verifier_rate_limited", False):
            async def _submit_gate_sink(ev: Event) -> None:
                if ev.event_type is not EventType.BLACKBOARD_DELTA:
                    return
                p = ev.payload or {}
                if p.get("kind") != "ready_to_submit":
                    return
                now = time.time()
                # if a submission is already in flight (lease not elapsed), let the
                # worker's own SUBMIT_LOCKED broadcast handle the new declarer; don't
                # double-announce. Otherwise open a serialization window.
                if now < self._submit_lock_until:
                    return
                self._submit_lock_until = now + 90.0  # one submission's worth of lease
                actor = p.get("actor") or "worker"
                try:
                    # tell every OTHER worker to hold its submission while `actor`
                    # runs the verifier. (The worker also broadcasts this itself;
                    # the coordinator dedups + enforces the serialization window.)
                    await self.insight.submit_locked(actor)
                except Exception:
                    pass
            try:
                self.bus.add_sink(_submit_gate_sink)
                self._coord_sinks.append(_submit_gate_sink)  # L3: detach on finalize
            except Exception:
                pass

        hitl_task: Optional[asyncio.Task] = None
        if self.hitl_inbox is not None:
            hitl_task = asyncio.create_task(self._drain_hitl(), name="hitl-drain")

        healthy = await self._healthy_engines_async()
        tasks: dict[asyncio.Task, str] = {}        # task -> engine
        task_intents: dict[asyncio.Task, str] = {}  # task -> intent_id
        task_solvers: dict[asyncio.Task, Any] = {}  # task -> CliSolver (to cancel)
        task_lanes: dict[asyncio.Task, str] = {}   # task -> exclusive lane key
        per_solver: dict[str, SolveOutcome] = {}
        winner: Optional[str] = None
        flag: Optional[str] = None
        # pentest mode has no flag — the run completes when the Reason phase judges
        # the engagement GOAL met (verdict=complete) from verified findings. CTF
        # leaves this False and ends only on a gated flag (winner).
        goal_complete = False

        # ── race-scout layer (DESIGN_race_scout_layer.md) ────────────────────
        # ONE round of fresh single-shot bootstrap workers (one per race engine) in
        # parallel BEFORE the coordinator loop. FAST PATH: a worker captures the flag →
        # finish here, skip the coordinator loop (the simple-challenge speed of the
        # original muteki race). SLOW PATH: no flag → their facts are on the shared
        # graph and we fall through to the coordinator loop warm (Reason plans from
        # real facts, not an empty graph). Disabled (race_scout=False) →
        # byte-identical to the plain loop.
        #
        # run-75379 BUG④ — INVARIANT GUARD: race-scout only ever runs on a genuine
        # COLD start. On a reopen/resume of a populated graph (prior intents/flags) we
        # skip the race entirely and go straight to the Reason/Explore loop on the
        # existing evidence. _is_cold_start is the load-bearing guard (explicit
        # cold_start hint + a graph-state backstop), so a relaunch that forgot to pass
        # race_scout=False / cold_start=False is still protected — the web reopen path
        # (run_manager.resolve) keeps passing race_scout=False as harmless redundancy.
        cold_start = self._is_cold_start()
        if self.race_scout and cold_start:
            race_winner, race_flag, race_solvers = await self._run_race_scout(healthy)
            per_solver.update(race_solvers)
            if race_winner is not None and self._flags_complete():
                # fast path: reuse the winner-exit shape (persist + close + RUN_FINISHED)
                # via the shared M11 finalizer (idempotent).
                winner, flag = race_winner, race_flag
                if hitl_task is not None:
                    hitl_task.cancel()
                    await asyncio.gather(hitl_task, return_exceptions=True)
                await self._finalize_coordinator_run(
                    winner=winner, flag=flag, goal_complete=False, per_solver=per_solver)
                return SwarmOutcome(True, flag, winner, per_solver,
                                    "solved via race-scout",
                                    flags=list(self._found_flags))
            # slow path: facts already on the shared graph; fall through to the
            # main coordinator loop.
            await self._emit_coord_bb(
                "phase_transition", **{"from": "race", "to": "coordinator"},
                facts_seeded=self._total_fact_count(),
                flags=len(self._found_flags),
            )
            if self.review_policy.get("after_race", False):
                await self._maybe_start_review(
                    trigger="after_race",
                    directive=(
                        "Race scout ended without completing the challenge. Audit "
                        "the seeded facts, repeated routes, challenged assumptions, "
                        "and propose suppress/reopen/branch/directive actions before "
                        "the coordinator expands the search."
                    ),
                    healthy=healthy, tasks=tasks, task_solvers=task_solvers,
                    emit_bb=self._emit_coord_bb,
                )
        elif self.race_scout and not cold_start:
            # WARM START (race-scout configured on, but this is a resume/reopen of a
            # populated graph): skip the race AND its after_race review — there was no
            # race to audit. Just announce the warm entry so the deck/board reflects
            # that the coordinator picked up on existing evidence, then fall through to
            # the same Reason/Explore loop the slow path uses. The loop's own
            # graph-change Reason trigger plans from the carried-over facts on tick 1.
            await self._emit_coord_bb(
                "phase_transition", **{"from": "resume", "to": "coordinator"},
                facts_seeded=self._total_fact_count(),
                flags=len(self._found_flags),
            )

        # monotonic clock comes in via time.monotonic() — allowed (not Date.now)
        t0 = time.monotonic()
        last_fact_count = 0
        # ── (A) reason checkpoint: graph-change trigger ───────────────────────
        # Snapshot of (facts, open_intents) at the last reason. Reason fires when the
        # graph GREW (new fact) or open intents were CONSUMED — not on a fixed stall.
        # This is what lets the swarm keep producing intents → keep filling slots,
        # instead of idling at 2 workers while one slowly emits facts (the
        # "permanently 2 workers" bug: progress was SUPPRESSING expansion).
        reason_fact_ckpt = 0
        reason_open_intent_ckpt = 0
        # no-progress backpressure (run-10070: 48 barren Reason rounds; run-11190:
        # a 238-worker spike the old collect-only, idle-branch guardrail could not
        # reach because open intents kept the loop busy — same structural miss as
        # the run-11189 NEED_INPUT pause). Count CONSECUTIVE worker COMPLETIONS
        # that produced NO new fact (incl. candidates) AND NO new flag; at
        # barren_limit, soft-PAUSE for the operator at the TOP of the loop (fires
        # busy or idle). Keyed on zero-new-evidence per finished worker, NOT a
        # global-fact-stall timer, so it can't death-spiral a deep-exploit worker
        # that's mid-setup (run-7352 lesson: never time-based, never kill).
        fruitless_workers = 0
        prog_fact_ckpt = 0
        prog_flag_ckpt = 0
        # H: long-run compaction trigger. Track when the board last grew; if too long
        # passes with no progress (or fruitless workers pile up past 2× barren_limit),
        # compact the graph (retire stale closed intents) and reset the barren count.
        last_progress_t = time.monotonic()
        last_compact_t = 0.0
        compact_no_progress_s = float(getattr(self, "compact_no_progress_s", 1800.0))
        self._last_candidate_review_count = self._candidate_fact_count()
        # NOTE: there is intentionally NO per-worker stall timer here anymore. The old
        # design steer-killed a worker that produced no GLOBAL verified fact for
        # stall_seconds — but a worker deep in an exploit chain (build listener → FTP
        # PORT bounce → capture RESP) legitimately emits no fact for minutes, and once
        # the easy facts were mined the global clock froze, so every freshly-spawned
        # worker was born already "stalled" and steer-killed in seconds (run-7352
        # death spiral). There is no stall-kill at all; the loop relies on
        # Reason/Explore separation + per-OODA whole-graph re-read. Worker reclaim is
        # now: natural exit (conclude / max_turns / per-turn timeout — explore gets a
        # SHORT timeout as the only backstop) + intent-lease expiry (an abandoned
        # intent's claim expires so _open_intents re-dispatches it).

        async def _emit_bb(kind: str, **fields):
            if self.bus is not None:
                await self.bus.emit(Event(
                    event_type=EventType.BLACKBOARD_DELTA, run_id=self.run_id,
                    challenge_id=self.challenge.id,
                    payload=blackboard_delta_payload(kind, actor="coordinator", **fields)))

        def _running_engines() -> list[str]:
            return list(tasks.values())

        async def _stop_for_budget(kind: str) -> None:
            self._budget_exhausted_kind = kind
            await _emit_bb(kind, spawned_total=self._spawned_total,
                           max_total_workers=self.max_total_workers,
                           cost_usd=self._current_cost_usd(),
                           cost_budget_usd=self.cost_budget_usd)
            await _emit_bb("budget_exhausted", budget_kind=kind,
                           spawned_total=self._spawned_total,
                           cost_usd=self._current_cost_usd())
            for other in tasks:
                self._cancel_solver(task_solvers.get(other))
                other.cancel()

        # ── 刀4: revive resume-parked intents on a CONTINUED run ─────────────
        # A prior non-solved finalize parked this run's in-flight intents in
        # dispatch_state='resume' (held back so a relaunch doesn't immediately
        # re-hurl workers at mid-flight directions). Now that the operator chose to
        # continue ("继续做题" → resolve relaunches the swarm on the SAME graph),
        # flip them back to 'active' so they're dispatchable again instead of
        # stranded forever. No-op on a fresh run (no resume rows). Emits the
        # transition onto the bus so the deck un-dims them.
        if self.shared_graph is not None:
            try:
                revived = self.shared_graph.revive_resume_intents(actor="coordinator")
                if revived:
                    await _emit_bb("intent_state_changed",
                                   intent_id=",".join(revived),
                                   dispatch_state="active")
            except Exception:
                pass

        # ── Phase: Bootstrap — start_workers heterogeneous rush workers ──────
        for i in range(min(self.start_workers, max(1, len(healthy)))):
            if not self._ordinary_capacity_available(tasks):
                break
            try:
                engine = self._pick_engine(_running_engines(), healthy, role="bootstrap")
            except RuntimeError as exc:
                await _emit_bb("worker_spawn_rejected", reason=str(exc), phase="bootstrap")
                break
            try:
                w = self._make_cli_worker(engine, mode="bootstrap")
            except WorkerSpawnRejected as exc:
                await _emit_bb("worker_spawn_rejected", reason=str(exc),
                               engine=str(engine), phase="bootstrap")
                break
            except WorkerBudgetExhausted as exc:
                await _stop_for_budget(str(exc))
                break
            t = asyncio.create_task(w.run(), name=f"bootstrap-{engine}")
            tasks[t] = engine
            task_solvers[t] = w
            await _emit_bb("worker_spawned", worker=w.solver_id,
                           phase="bootstrap", worker_role="worker")

        try:
            while tasks:
                done, _pending = await asyncio.wait(
                    set(tasks.keys()), timeout=self.config_poll_interval(),
                    return_when=asyncio.FIRST_COMPLETED)

                # reap finished workers. reaped_n counts every worker that RAN to
                # an end (incl. errors — spent budget either way) for the barren
                # backpressure below; cancelled workers were killed, not fruitless.
                reaped_n = 0
                completed_for_review_n = 0
                for t in done:
                    is_review_task = t in self._active_review_tasks
                    engine = tasks.pop(t)
                    intent_id = task_intents.pop(t, None)
                    solver = task_solvers.pop(t, None)
                    self._release_worker_account(solver)
                    # key per-solver outcomes by the worker's UNIQUE solver_id (e.g.
                    # cli-claude-2), not the bare engine — otherwise two same-engine
                    # workers (race + a later explore) clobber each other's record.
                    sid = getattr(solver, "solver_id", None) or f"cli-{engine}"
                    lane_key = task_lanes.pop(t, "")
                    if lane_key and self.shared_graph is not None:
                        try:
                            rel = self.shared_graph.release_lane(  # type: ignore[attr-defined]
                                actor="coordinator", lane_key=lane_key, by_worker=sid)
                            await self._consume_lane_release(rel, emit_bb=_emit_bb)
                        except Exception:
                            pass
                    try:
                        outcome = t.result()
                    except asyncio.CancelledError:
                        continue
                    except Exception as e:
                        per_solver[sid] = SolveOutcome(
                            False, None, 0, None, f"error: {e}")
                        # A control-plane failure (the in-container supervisor died /
                        # the reverse link dropped mid-worker) is NOT an ordinary
                        # worker crash — surface it as runtime_degraded so the operator
                        # sees the runtime broke (roadmap 972 / §8). We never silently
                        # switch to local: the worker just failed, container-backed.
                        if _is_control_failure(e):
                            self._record_runtime_degraded(
                                engine=engine, profile=None,
                                reason=f"runtime supervisor/link failed mid-worker: {e}",
                                requested_backend="container",
                                fallback_backend="none")
                        await _emit_bb("worker_finished", worker=sid,
                                       result="error")
                        if t in self._active_review_tasks:
                            self._active_review_tasks.discard(t)
                            await _emit_bb("review_finished", worker=sid,
                                           result="error")
                        if not is_review_task:
                            completed_for_review_n += 1
                        reaped_n += 1
                        continue
                    reaped_n += 1
                    if not is_review_task:
                        completed_for_review_n += 1
                    per_solver[outcome_id := sid] = outcome
                    await _emit_bb("worker_finished", worker=outcome_id,
                                   result="solved" if outcome.solved else "done")
                    if t in self._active_review_tasks:
                        self._active_review_tasks.discard(t)
                        await _emit_bb("review_finished", worker=outcome_id,
                                       result="done")
                    # multi-flag: tally every flag this worker produced. The run is
                    # done only once we hold expected_flags — until then a flag is
                    # NOT a stop signal; the loop keeps spawning/exploring to find
                    # the rest (re-bootstrap naturally continues; the new workers'
                    # prompts carry the already-found list via _record_flags →
                    # standing injection below).
                    self._record_flags(*(outcome.flags or
                                         ([outcome.flag] if outcome.flag else [])))
                    if self._flags_complete() and winner is None:
                        winner, flag = outcome_id, self._found_flags[0]
                        try:
                            await self.insight.all_flags_found(
                                "coordinator", count=len(self._found_flags))
                        except Exception:
                            pass
                        # kill the losing workers' subprocesses, not just their tasks
                        for other in tasks:
                            self._cancel_solver(task_solvers.get(other))
                            other.cancel()

                # ── split-brain reconcile (BUG②) ─────────────────────────────
                # The per-reap tally above only sees flags carried back in a clean
                # `outcome.flags`. A flag can reach the shared graph (and the UI /
                # planner) via a path that never delivered one — a worker cancelled
                # after it accepted a flag (reaped as CancelledError above), an
                # error-reaped worker, or the live DB→bus bridge. Sync the in-memory
                # set with the authoritative snapshot every iteration so completion
                # fires on the real flag count and a blacklisted flag is dropped
                # (run-75379: graph held 4 valid flags, _found_flags stuck at 2).
                if winner is None:
                    self._sync_flags_from_graph()
                    if self._flags_complete():
                        winner = "coordinator"
                        flag = self._found_flags[0] if self._found_flags else None
                        try:
                            await self.insight.all_flags_found(
                                "coordinator", count=len(self._found_flags))
                        except Exception:
                            pass
                        await _emit_bb("all_flags_found",
                                       flags=len(self._found_flags))
                        for other in tasks:
                            self._cancel_solver(task_solvers.get(other))
                            other.cancel()

                if winner is not None:
                    break

                # ── progress tracking (for the reason graph-change trigger) ──
                now = time.monotonic()
                fc = self._verified_fact_count()
                if fc > last_fact_count:
                    last_fact_count = fc

                # ── barren backpressure accounting (ALL modes) ───────────────
                # Per finished worker: did the board grow at all since the last
                # completion? Candidates count (engagement ≠ fruitless); flags
                # count; dead-ends deliberately do NOT — the run-11190 spike
                # workers wrote dead-ends while burning 238 slots on a solved wall.
                if reaped_n and self.barren_limit > 0:
                    tfc = self._total_fact_count()
                    grew = (tfc > prog_fact_ckpt
                            or len(self._found_flags) > prog_flag_ckpt)
                    fruitless_workers = (0 if grew
                                         else fruitless_workers + reaped_n)
                    if grew:
                        last_progress_t = time.monotonic()  # H: reset no-progress timer
                    prog_fact_ckpt = max(prog_fact_ckpt, tfc)
                    prog_flag_ckpt = max(prog_flag_ckpt, len(self._found_flags))

                if completed_for_review_n:
                    self._completed_workers_since_review += completed_for_review_n

                if self._operator_stop:
                    # operator pressed stop / marked complete — end gracefully,
                    # keeping all recovered knowledge (the board persists). Distinct
                    # from budget_exhausted so the FE can show "stopped by operator".
                    await _emit_bb("operator_stopped",
                                   flags=len(self._found_flags))
                    for other in tasks:
                        self._cancel_solver(task_solvers.get(other))
                        other.cancel()
                    break

                if now - t0 > self.wall_clock_budget:
                    self._budget_exhausted_kind = "wall_clock_budget_exhausted"
                    await _emit_bb("budget_exhausted", elapsed=int(now - t0))
                    for other in tasks:
                        self._cancel_solver(task_solvers.get(other))
                        other.cancel()
                    break

                budget_kind = self._budget_exhausted()
                if budget_kind:
                    await _stop_for_budget(budget_kind)
                    break

                if await self._spawn_rebootstrap_from_directive(
                    healthy=healthy, tasks=tasks, task_solvers=task_solvers,
                    running_engines_fn=_running_engines, emit_bb=_emit_bb):
                    continue

                while self._pending_uncertainty_reviews:
                    p = self._pending_uncertainty_reviews.pop(0)
                    worker = str(p.get("worker") or "worker")
                    need = str(p.get("need") or "").strip()
                    self._queue_review_request(
                        trigger="worker_uncertainty",
                        directive=(
                            f"worker {worker} 不确定：{need[:500]}；"
                            "请审查当前事实/候选/意图，给出可执行的 NEXT_INTENT 或路线修正。"
                        ),
                    )

                if await self._maybe_run_queued_review(
                    healthy=healthy, tasks=tasks,
                    task_solvers=task_solvers, emit_bb=_emit_bb):
                    continue

                if await self._drain_review_proposals(
                    emit_bb=_emit_bb, fruitless_workers=fruitless_workers):
                    continue

                # E: surface any resource locks workers acquired since last tick.
                await self._drain_resource_locks(emit_bb=_emit_bb)
                await self._drain_graph_to_bus(emit_bb=_emit_bb)

                if self.review_policy.get("on_candidate_spike", True):
                    candidate_count = self._candidate_fact_count()
                    threshold = int(self.review_policy.get("candidate_spike_threshold") or 0)
                    threshold = max(1, threshold)
                    candidate_delta = candidate_count - self._last_candidate_review_count
                    if (candidate_delta >= threshold
                            and await self._maybe_start_review(
                                trigger="candidate_spike",
                                directive=(
                                    f"{candidate_delta} new unverified candidate facts accumulated "
                                    "since the last review. Audit semantic duplicates, challenge weak "
                                    "facts, suppress repeated routes, and propose verifier/code branches."
                                ),
                                healthy=healthy, tasks=tasks,
                                task_solvers=task_solvers, emit_bb=_emit_bb)):
                        continue

                every_completed = int(self.review_policy.get("every_completed_workers") or 0)
                if (every_completed > 0
                        and self._completed_workers_since_review >= every_completed
                        and await self._maybe_start_review(
                            trigger="every_completed_workers",
                            directive=(
                                f"{self._completed_workers_since_review} ordinary workers completed "
                                "since the last review. Audit whether the swarm is repeating a route, "
                                "needs a branch split, or should rebootstrap from a sharper directive."
                            ),
                            healthy=healthy, tasks=tasks,
                            task_solvers=task_solvers, emit_bb=_emit_bb)):
                    continue

                # ── operator SOFT-PAUSE (#5): the operator pressed pause. Stop
                # spawning NEW workers and wait for resume — but do NOT kill running
                # workers or end the run (that's stop). This is the meaningful "pause"
                # for a single-shot swarm. The wait is interruptible: resume sets
                # _operator_event; stop sets _operator_stop (handled at loop top next
                # iteration); a finite budget still expires (offline eval safety).
                if self._operator_paused and self.bus is not None:
                    self._operator_event.clear()
                    if self.wall_clock_budget == float("inf"):
                        await self._operator_event.wait()
                    else:
                        remaining = self.wall_clock_budget - (time.monotonic() - t0)
                        try:
                            await asyncio.wait_for(self._operator_event.wait(),
                                                   timeout=max(0.0, remaining))
                        except asyncio.TimeoutError:
                            # L6: balance the paused-state bracket so the FE clears its
                            # "awaiting operator / paused" banner on this exit too.
                            self._budget_exhausted_kind = "wall_clock_budget_exhausted"
                            self._operator_paused = False
                            await _emit_bb("operator_resumed")
                            await _emit_bb("budget_exhausted",
                                           elapsed=int(time.monotonic() - t0))
                            for other in tasks:
                                self._cancel_solver(task_solvers.get(other))
                                other.cancel()
                            break
                    if self._operator_stop:
                        # L6: balance the paused-state bracket (see above).
                        self._operator_paused = False
                        await _emit_bb("operator_resumed")
                        await _emit_bb("operator_stopped",
                                       flags=len(self._found_flags))
                        for other in tasks:
                            self._cancel_solver(task_solvers.get(other))
                            other.cancel()
                        break
                    await _emit_bb("operator_resumed")
                    # same `while tasks:` guard as the other resume paths: if every
                    # worker finished while paused, seed one bootstrap so the loop
                    # lives on instead of falling out of `while tasks:`.
                    if not tasks:
                        try:
                            engine = self._pick_engine(_running_engines(), healthy, role="bootstrap")
                        except RuntimeError as exc:
                            await _emit_bb("worker_spawn_rejected", reason=str(exc),
                                           phase="resume_bootstrap")
                            continue
                        try:
                            w = self._make_cli_worker(
                                engine, mode="bootstrap",
                                intent_goal=self._retry_goal())
                        except WorkerSpawnRejected as exc:
                            await _emit_bb("worker_spawn_rejected", reason=str(exc),
                                           engine=str(engine), phase="resume_bootstrap")
                            break
                        except WorkerBudgetExhausted as exc:
                            await _stop_for_budget(str(exc))
                            break
                        t = asyncio.create_task(
                            w.run(), name=f"resume-bootstrap-{engine}")
                        tasks[t] = engine
                        task_solvers[t] = w
                        await _emit_bb("worker_spawned", worker=w.solver_id,
                                       phase="resume_bootstrap", worker_role="worker")
                    continue

                # ── operator-blocked: a worker raised its hand (NEED_INPUT / env_down)
                # — pause HERE, at the top of the loop body, BEFORE any spawning. The
                # old pause lived only in the "fully idle" branch (not tasks and not
                # open_intents), but the never-give-up Reason engine keeps minting
                # intents, so the swarm is never idle and the pause never fired
                # (run-11189: 3 NEED_INPUTs, 0 awaiting_operator, ~30 min hurling fresh
                # workers at the same no-dashboard-token wall). Pausing here is
                # phase-independent: as long as an ask is outstanding we wait for the
                # operator instead of spawning more doomed workers. The wait is
                # interruptible — _drain_hitl sets _operator_event on ANY operator
                # command (and clears _pending_help), and STOP wakes us to break.
                if self._pending_help and self.bus is not None:
                    # Per-ask clip raised 120→300 (+ ellipsis on truncation) so the
                    # amber "awaiting operator" banner shows enough of each ask to be
                    # actionable. The full text rides the HITL_REQUEST card, which the
                    # worker now emits without truncation; this is just the summary.
                    def _clip(s: str) -> str:
                        s = str(s)
                        return s if len(s) <= 300 else (s[:300] + " …")
                    needs = "; ".join(
                        _clip(h.get("need", "")) for h in self._pending_help[-3:])
                    # LOST-WAKEUP FIX: clear the event BEFORE emitting / awaiting.
                    # _drain_hitl runs concurrently; if the operator answers during the
                    # `await` of the awaiting_operator emit below, it sets _operator_event
                    # and clears _pending_help. Clearing the event here (after that set)
                    # would swallow the answer and — under the live inf budget — block
                    # forever (the run-11189 deadlock class). So clear first, emit, then
                    # re-check _pending_help: if the operator already answered, skip the
                    # wait entirely.
                    self._operator_event.clear()
                    await _emit_bb("awaiting_operator", reason=needs,
                                   count=len(self._pending_help))
                    if not self._pending_help or self._operator_stop:
                        # answered (or stopped) in the emit window — don't wait/freeze.
                        self._operator_paused = False
                        # L6: balance the awaiting_operator bracket so the FE clears its
                        # "awaiting operator" banner even on this no-wait exit.
                        await _emit_bb("operator_resumed")
                        if self._operator_stop:
                            await _emit_bb("operator_stopped",
                                           flags=len(self._found_flags))
                            for other in tasks:
                                self._cancel_solver(task_solvers.get(other))
                                other.cancel()
                            break
                        continue
                    # FREEZE the live workers while we wait for the operator. A worker
                    # raised its hand (NEED_INPUT) — it should genuinely stop, not keep
                    # burning the wrong path / cost while the operator decides (the
                    # run-0011 "决策不暂停 worker,都在继续工作" symptom). insight pause
                    # SIGSTOPs every live worker's process group; we SIGCONT them after
                    # the operator answers (or on timeout/stop, where the cancel below
                    # kills them anyway).
                    self._operator_paused = True
                    try:
                        await self.insight.guidance(
                            "", action="pause", target="global", standing=False)
                    except Exception:
                        pass
                    # A worker explicitly asked for help, so blocking is correct —
                    # but an unattended run with a finite wall_clock_budget (offline
                    # eval) must still be able to exhaust its budget rather than hang
                    # forever waiting for an operator who isn't there (same reasoning
                    # as the barren pause below). With an infinite budget (the live
                    # default) we block indefinitely, exactly as before.
                    if self.wall_clock_budget == float("inf"):
                        await self._operator_event.wait()  # blocks until operator acts
                    else:
                        remaining = self.wall_clock_budget - (time.monotonic() - t0)
                        try:
                            await asyncio.wait_for(self._operator_event.wait(),
                                                   timeout=max(0.0, remaining))
                        except asyncio.TimeoutError:
                            # L6: balance the paused-state bracket so the FE clears its
                            # "awaiting operator / paused" banner on this exit too.
                            self._budget_exhausted_kind = "wall_clock_budget_exhausted"
                            self._operator_paused = False
                            await _emit_bb("operator_resumed")
                            await _emit_bb("budget_exhausted",
                                           elapsed=int(time.monotonic() - t0))
                            for other in tasks:
                                self._cancel_solver(task_solvers.get(other))
                                other.cancel()
                            break
                    if self._operator_stop:
                        # L6: balance the paused-state bracket (see above).
                        self._operator_paused = False
                        await _emit_bb("operator_resumed")
                        await _emit_bb("operator_stopped",
                                       flags=len(self._found_flags))
                        for other in tasks:
                            self._cancel_solver(task_solvers.get(other))
                            other.cancel()
                        break
                    # operator responded → UNFREEZE the workers we SIGSTOP'd above so
                    # any still-running worker resumes with the operator's input in
                    # play (a non-stop answer also folds a standing hint in). Stop /
                    # timeout paths above already cancelled the workers, so this only
                    # matters on the normal answer path.
                    self._operator_paused = False
                    try:
                        await self.insight.guidance(
                            "", action="resume", target="global", standing=False)
                    except Exception:
                        pass
                    # _pending_help already cleared by _drain_hitl,
                    # the standing hint folded into future workers. If every worker
                    # finished while we were paused, `tasks` is now empty — and the
                    # loop guard `while tasks:` (plus asyncio.wait, which rejects an
                    # empty set) would END the run instead of resuming with the new
                    # input. So when idle-after-wake, spawn a fresh bootstrap worker
                    # (seeded with _retry_goal + the standing hint) BEFORE looping, to
                    # keep `tasks` non-empty. If workers are still running, just
                    # re-poll from the top.
                    await _emit_bb("operator_resumed")
                    if not tasks:
                        try:
                            engine = self._pick_engine(_running_engines(), healthy, role="bootstrap")
                        except RuntimeError as exc:
                            await _emit_bb("worker_spawn_rejected", reason=str(exc),
                                           phase="resume_bootstrap")
                            continue
                        try:
                            w = self._make_cli_worker(
                                engine, mode="bootstrap",
                                intent_goal=self._retry_goal())
                        except WorkerSpawnRejected as exc:
                            await _emit_bb("worker_spawn_rejected", reason=str(exc),
                                           engine=str(engine), phase="resume_bootstrap")
                            break
                        except WorkerBudgetExhausted as exc:
                            await _stop_for_budget(str(exc))
                            break
                        t = asyncio.create_task(
                            w.run(), name=f"resume-bootstrap-{engine}")
                        tasks[t] = engine
                        task_solvers[t] = w
                        await _emit_bb("worker_spawned", worker=w.solver_id,
                                       phase="resume_bootstrap", worker_role="worker")
                    continue

                # ── H: long-run COMPACT — retire stale closed intents so the planner
                # board doesn't grow unbounded. Fires on a hard fruitless threshold
                # (2× barren_limit) OR a long no-progress window. Never touches facts
                # (design §12: compaction must not collapse a candidate into a fact);
                # only already-concluded, fact-less intents are retired. Resets the
                # barren counter so the run continues fresh after compaction.
                now_mono = time.monotonic()
                no_progress_elapsed = now_mono - last_progress_t
                compact_due = (
                    self.shared_graph is not None
                    and (now_mono - last_compact_t) > 60.0  # don't thrash
                    and (
                        (self.barren_limit > 0 and fruitless_workers >= 2 * self.barren_limit)
                        or (compact_no_progress_s > 0 and no_progress_elapsed >= compact_no_progress_s)
                    )
                )
                if compact_due:
                    trigger = ("fruitless_workers"
                               if fruitless_workers >= 2 * self.barren_limit
                               else "no_progress_time")
                    try:
                        info = self.shared_graph.compact_graph(
                            actor="coordinator", trigger=trigger,
                            summary=(f"compacted after {fruitless_workers} fruitless "
                                     f"workers / {int(no_progress_elapsed)}s no progress"))
                        last_compact_t = now_mono
                        last_progress_t = now_mono
                        fruitless_workers = 0  # compact resets backpressure
                        if self.bus is not None:
                            try:
                                await self.bus.emit(Event(
                                    event_type=EventType.GRAPH_COMPACTED,
                                    run_id=self.run_id, challenge_id=self.challenge.id,
                                    payload={"compact_id": info.get("compact_id"),
                                             "trigger": trigger,
                                             "retired_intents": len(info.get("retired_intent_ids") or []),
                                             "summary": info.get("summary", "")}))
                            except Exception:
                                pass
                        await _emit_bb(
                            "graph_compacted", compact_id=info.get("compact_id"),
                            trigger=trigger,
                            retired=len(info.get("retired_intent_ids") or []))
                        # 刀6: mirror the per-intent retirement onto the bus so the
                        # deck folds dispatchState→retired (the DB event row alone
                        # never reaches the SSE/JSONL stream the UI reads).
                        retired_ids = [str(x) for x in (info.get("retired_intent_ids") or []) if x]
                        if retired_ids:
                            await _emit_bb(
                                "intent_state_changed",
                                intent_id=",".join(retired_ids),
                                dispatch_state="retired",
                                compact_id=info.get("compact_id"))
                    except Exception:
                        pass

                # ── barren backpressure pause: too many consecutive fruitless
                # workers → soft-pause for the operator BEFORE spawning more. Sits
                # here (top of loop, after the NEED_INPUT pause) so it fires even
                # while intents are open / workers are running — the old version
                # lived in the fully-idle branch and a busy churn spike never
                # reached it. Emits kind "collect_idle" (the historical name) so
                # the deck's existing paused-state handling applies unchanged.
                # Soft: no worker kill; any operator command resumes.
                if (self.barren_limit > 0
                        and fruitless_workers >= self.barren_limit
                        and not self._pending_help and self.bus is not None):
                    fruitless_review_after = int(self.review_policy.get("after_fruitless_workers") or 0)
                    if (fruitless_review_after > 0
                            and fruitless_workers >= fruitless_review_after
                            and await self._maybe_start_review(
                                trigger="fruitless_workers",
                                directive=(f"{fruitless_workers} consecutive workers produced no new fact or flag; "
                                           "audit repeated routes/dead-end amnesia and propose suppression or a corrected directive."),
                                healthy=healthy, tasks=tasks,
                                task_solvers=task_solvers, emit_bb=_emit_bb)):
                        fruitless_workers = 0
                        continue
                    await _emit_bb(
                        "collect_idle",
                        reason=(f"{fruitless_workers} consecutive workers finished "
                                f"with no new fact or flag; "
                                f"{len(self._found_flags)} flags collected — "
                                "paused for the operator (STOP to finish, or send "
                                "a hint/input to continue)"),
                        flags=len(self._found_flags),
                        fruitless_workers=fruitless_workers)
                    self._operator_event.clear()
                    # Wait for the operator — but NOT unconditionally. Unlike the
                    # NEED_INPUT pause (a worker explicitly asked, so blocking is
                    # correct), this pause is autonomous, and a run with a finite
                    # wall_clock_budget (offline eval) must still be able to exhaust
                    # it rather than hang forever waiting for an operator who isn't
                    # there. With an infinite budget (the live default) we wait on the
                    # operator BUT also self-wake on real graph progress (see below).
                    if self.wall_clock_budget == float("inf"):
                        # ④ collect_idle must NOT block ONLY on the operator. The pause
                        # fires after N fruitless workers, but other in-flight workers
                        # (or the DB→bus bridge) can still land a NEW verified fact /
                        # flag, or a fresh dispatchable SOLVING intent can appear — in
                        # run-75377 the coordinator sat in this wait for 33 min while
                        # facts were still arriving and never re-dispatched. Poll on a
                        # short timeout and self-wake on real progress, treating it like
                        # an operator resume. The operator event still wins instantly.
                        _idle_fact_ckpt = self._verified_fact_count()
                        _idle_flag_ckpt = len(self._found_flags)
                        while not self._operator_event.is_set():
                            try:
                                await asyncio.wait_for(
                                    self._operator_event.wait(), timeout=15.0)
                                break
                            except asyncio.TimeoutError:
                                pass
                            if (self._verified_fact_count() > _idle_fact_ckpt
                                    or len(self._found_flags) > _idle_flag_ckpt
                                    or self._open_intents()):
                                break
                    else:
                        remaining = self.wall_clock_budget - (time.monotonic() - t0)
                        try:
                            await asyncio.wait_for(self._operator_event.wait(),
                                                   timeout=max(0.0, remaining))
                        except asyncio.TimeoutError:
                            # L6: balance the paused-state bracket so the FE clears its
                            # "awaiting operator / paused" banner on this exit too.
                            self._budget_exhausted_kind = "wall_clock_budget_exhausted"
                            self._operator_paused = False
                            await _emit_bb("operator_resumed")
                            await _emit_bb("budget_exhausted",
                                           elapsed=int(time.monotonic() - t0))
                            for other in tasks:
                                self._cancel_solver(task_solvers.get(other))
                                other.cancel()
                            break
                    if self._operator_stop:
                        # L6: balance the paused-state bracket (see above).
                        self._operator_paused = False
                        await _emit_bb("operator_resumed")
                        await _emit_bb("operator_stopped",
                                       flags=len(self._found_flags))
                        for other in tasks:
                            self._cancel_solver(task_solvers.get(other))
                            other.cancel()
                        break
                    await _emit_bb("operator_resumed")
                    fruitless_workers = 0
                    # same `while tasks:` guard as the NEED_INPUT resume above: if
                    # everything finished while paused, seed one bootstrap worker
                    # (with the operator's fresh standing hint) so the loop lives on.
                    if not tasks:
                        try:
                            engine = self._pick_engine(_running_engines(), healthy, role="bootstrap")
                        except RuntimeError as exc:
                            await _emit_bb("worker_spawn_rejected", reason=str(exc),
                                           phase="resume_bootstrap")
                            continue
                        try:
                            w = self._make_cli_worker(
                                engine, mode="bootstrap",
                                intent_goal=self._retry_goal())
                        except WorkerSpawnRejected as exc:
                            await _emit_bb("worker_spawn_rejected", reason=str(exc),
                                           engine=str(engine), phase="resume_bootstrap")
                            break
                        except WorkerBudgetExhausted as exc:
                            await _stop_for_budget(str(exc))
                            break
                        t = asyncio.create_task(
                            w.run(), name=f"resume-bootstrap-{engine}")
                        tasks[t] = engine
                        task_solvers[t] = w
                        await _emit_bb("worker_spawned", worker=w.solver_id,
                                       phase="resume_bootstrap", worker_role="worker")
                    continue

                # NOTE: no stall-reclaim loop here anymore (see the top-of-loop note).
                # A worker is reclaimed only by exiting on its own (explore has a short
                # per-turn timeout) or by its intent's lease expiring → re-dispatched.

                # ── operator worker control: spawn/kill a specific engine on demand
                await self._apply_worker_cmds(
                    tasks=tasks, task_solvers=task_solvers, healthy=healthy,
                    running_engines_fn=_running_engines, emit_bb=_emit_bb)

                # ── (A) Reason — GRAPH-CHANGE trigger ────────────────────────
                # Reason fires when the graph changed since the last reason: a new
                # fact was confirmed, OR open intents were consumed to zero, OR the
                # swarm went fully idle (nothing running, nothing queued). Purely
                # graph/idle driven — there is no time-based "stalled" trigger
                # anymore. Reason is now the sole expansion engine, so it must keep
                # producing fresh directions; goal-hash intent dedup (reason.py)
                # stops it re-proposing the same batch.
                just_reaped = len(done) > 0
                slots_free = self._ordinary_capacity_available(tasks)
                open_intents = self._open_intents()
                graph_grew = (last_fact_count > reason_fact_ckpt)
                intents_consumed = (reason_open_intent_ckpt > 0 and len(open_intents) == 0)

                need_reason = (
                    # graph-change driven (the real expansion engine):
                    (slots_free and (graph_grew or intents_consumed)) or
                    # a worker finished and there's nothing queued → plan next wave:
                    (just_reaped and slots_free and not open_intents) or
                    # genuinely empty (no work running, nothing queued) → re-plan:
                    (slots_free and not open_intents and len(tasks) == 0)
                )
                if need_reason and self._reason_backpressure_active(open_intents):
                    await _emit_bb(
                        "reason_skipped",
                        trigger="queue_backpressure",
                        open_intents=len(open_intents),
                        ordinary_open_intents=self._ordinary_open_queue_depth(open_intents),
                        max_workers=self.max_workers,
                    )
                    need_reason = False
                if need_reason:
                    trigger = ("graph" if (graph_grew or intents_consumed) else "idle")
                    await _emit_bb("reason_start", trigger=trigger)
                    n = await self._run_reason()
                    open_intents = self._open_intents()
                    # checkpoint the graph state we just reasoned over (A).
                    reason_fact_ckpt = last_fact_count
                    reason_open_intent_ckpt = len(open_intents)
                    # dropped_dup = intents Reason emitted that dispatch refused as
                    # duplicates (goal-hash exact + near-duplicate filter) — surfaced
                    # so a "planner is only re-proposing old directions" round is
                    # visible on the blackboard instead of silently shrinking.
                    _rr = getattr(self, "_last_reason", None)
                    rr_intents = len(getattr(_rr, "intents", []) or [])
                    dropped_dup = max(0, rr_intents - n)
                    await _emit_bb("reason_done", proposed=n,
                                   dropped_dup=dropped_dup)
                    if (dropped_dup >= int(self.review_policy.get("after_duplicate_intents") or 0)
                            and await self._maybe_start_review(
                                trigger="duplicate_intents",
                                directive=f"Reason dropped {dropped_dup} duplicate intent(s); audit route loop and suppress repeated directions.",
                                healthy=healthy, tasks=tasks,
                                task_solvers=task_solvers, emit_bb=_emit_bb)):
                        continue

                    # ── pentest completion: no flag exists, so the engagement is
                    # done when Reason judges the GOAL met from verified findings.
                    # (CTF never takes this branch — it ends on a gated flag.)
                    rr = getattr(self, "_last_reason", None)
                    if (getattr(self.challenge, "mode", "ctf") == "pentest"
                            and rr is not None
                            and getattr(rr, "verdict", "") == "complete"):
                        goal_complete = True
                        await _emit_bb("goal_complete",
                                       why=getattr(rr, "complete_why", "")[:300])
                        for other in tasks:
                            self._cancel_solver(task_solvers.get(other))
                            other.cancel()
                        break
                    if (getattr(self.challenge, "mode", "ctf") != "pentest"
                            and rr is not None
                            and getattr(rr, "verdict", "") == "complete"
                            and self._flags_complete()):
                        if winner is None:
                            winner = "coordinator"
                            flag = self._found_flags[0] if self._found_flags else None
                        await _emit_bb(
                            "goal_complete",
                            why=getattr(rr, "complete_why", "")[:300],
                            flags=len(self._found_flags))
                        for other in tasks:
                            self._cancel_solver(task_solvers.get(other))
                            other.cancel()
                        break

                    # ── Phase 7: adaptive re-bootstrap ──────────────────────
                    # If Reason says the run DRIFTED (course_correct), a fresh
                    # whole-challenge rush from the corrected direction often
                    # beats stepping through narrow Explores. Spawn ONE bootstrap
                    # worker seeded with the drift, if a slot is free. (Bootstrap is
                    # not a one-time phase: it can re-fire on a course correction.)
                    reason_res = getattr(self, "_last_reason", None)
                    drift = getattr(reason_res, "drift", "") if reason_res else ""
                    verdict = getattr(reason_res, "verdict", "") if reason_res else ""
                    if verdict == "course_correct" and drift:
                        if (self.review_policy.get("on_course_correct", True)
                                and await self._maybe_start_review(
                                    trigger="course_correct", directive=drift,
                                    healthy=healthy, tasks=tasks,
                                    task_solvers=task_solvers, emit_bb=_emit_bb)):
                            continue
                        if not self._ordinary_capacity_available(tasks):
                            continue
                        try:
                            engine = self._pick_engine(_running_engines(), healthy, role="bootstrap")
                        except RuntimeError as exc:
                            await _emit_bb("worker_spawn_rejected", reason=str(exc),
                                           phase="rebootstrap")
                            continue
                        try:
                            w = self._make_cli_worker(
                                engine, mode="bootstrap", intent_goal=drift)
                        except WorkerSpawnRejected as exc:
                            await _emit_bb("worker_spawn_rejected", reason=str(exc),
                                           engine=str(engine), phase="rebootstrap")
                            break
                        except WorkerBudgetExhausted as exc:
                            await _stop_for_budget(str(exc))
                            break
                        t = asyncio.create_task(
                            w.run(), name=f"rebootstrap-{engine}")
                        tasks[t] = engine
                        task_solvers[t] = w
                        await _emit_bb("worker_spawned", worker=w.solver_id,
                                       phase="rebootstrap", worker_role="worker")

                # ── Phase: Explore — fill free slots with intent workers ─────
                # Spawn at most `explore_spawn_batch` (default 1) per loop iteration,
                # NOT a whole burst that fills every free slot at once. The poll
                # interval (~2s) means a slot refills within ~2s anyway, so the swarm
                # ramps smoothly instead of launching 10 workers in one tick that then
                # share a fate (run-7352: a 10-worker burst that all died together,
                # and 10 concurrent connections to one target tripped its rate limit).
                open_intents = self._dispatchable_open_intents(open_intents)
                open_intents = self._capacity_dispatchable_open_intents(open_intents, tasks)
                spawned_this_round = 0
                while open_intents and spawned_this_round < self.explore_spawn_batch:
                    intent = open_intents.pop(0)
                    iid = intent["intent_id"]
                    worker_class = str(intent.get("worker_class") or "code")
                    worker_mode = "review" if worker_class == "review" else "explore"
                    worker_role = "review" if worker_class == "review" else "explore"
                    try:
                        engine = self._pick_engine(_running_engines(), healthy, role=worker_role)
                    except RuntimeError as exc:
                        await _emit_bb("worker_spawn_rejected", reason=str(exc),
                                       phase=worker_mode, intent_id=iid)
                        open_intents.insert(0, intent)
                        break
                    # build the worker FIRST so we can claim the intent under ITS
                    # unique solver_id — that makes the worker the intent's OWNER, so
                    # conclude_intent's owner-fence lets exactly this worker (not a
                    # later re-spawn that took over an expired lease) conclude it.
                    try:
                        w = self._make_cli_worker(
                            engine, mode=worker_mode,
                            intent_goal=intent["goal"], intent_id=iid)
                    except WorkerSpawnRejected as exc:
                        # intent not claimed yet (claim happens after build) → just
                        # skip this spawn; the intent stays open for a later worker.
                        await _emit_bb("worker_spawn_rejected", reason=str(exc),
                                       engine=str(engine), phase=worker_mode)
                        break
                    except WorkerBudgetExhausted as exc:
                        await _stop_for_budget(str(exc))
                        break
                    # atomic claim (guards double-claim). Lease MUST outlast the
                    # explore worker's own per-turn timeout (+margin for conclude) —
                    # else a still-running worker's lease lapses, _open_intents
                    # re-dispatches the intent, and its later conclusion is fenced out.
                    won = False
                    try:
                        won = self.shared_graph.claim_intent(
                            worker=w.solver_id, intent_id=iid,
                            lease_s=float(self.explore_timeout) + 300.0)
                    except Exception:
                        won = False
                    if not won:
                        self._release_worker_account(w)
                        continue  # someone else holds a live claim; drop this worker
                    lane_key = str(intent.get("lane_key") or "")
                    locked_lane = ""
                    if (lane_key and worker_mode != "review"
                            and self.shared_graph is not None):
                        try:
                            lock = self.shared_graph.lock_lane(  # type: ignore[attr-defined]
                                actor="coordinator",
                                lane_key=lane_key,
                                risk_class=str(intent.get("risk_class") or ""),
                                owner_worker=w.solver_id,
                                owner_intent=iid,
                                lease_s=float(self.explore_timeout) + 300.0,
                            )
                        except Exception:
                            lock = {"acquired": False, "held_seq": 0}
                        if not lock.get("acquired"):
                            try:
                                self.shared_graph.defer_intent_for_lane(  # type: ignore[attr-defined]
                                    actor="coordinator",
                                    intent_id=iid,
                                    lane_key=lane_key,
                                    against_locked_seq=int(lock.get("held_seq") or 0),
                                )
                                await _emit_bb(
                                    "intent_lane_deferred",
                                    intent_id=iid,
                                    lane_key=lane_key,
                                    held_by=str(lock.get("held_by") or ""),
                                    held_seq=int(lock.get("held_seq") or 0),
                                )
                            except Exception:
                                pass
                            self._release_worker_account(w)
                            continue
                        locked_lane = str(lock.get("lane_key") or lane_key)
                        try:
                            self.shared_graph.add_coordinator_directive(  # type: ignore[attr-defined]
                                actor="coordinator",
                                action="lane_lock",
                                directive=(
                                    f"lane {locked_lane} is exclusively held by {w.solver_id}; "
                                    "do not start destructive/exclusive work on that resource."
                                ),
                                priority="high",
                            )
                        except Exception:
                            pass
                        await _emit_bb("lane_locked", **lock, intent_id=iid)
                    t = asyncio.create_task(w.run(), name=f"{worker_mode}-{engine}")
                    tasks[t] = engine
                    task_solvers[t] = w
                    task_intents[t] = iid
                    if locked_lane:
                        task_lanes[t] = locked_lane
                    if worker_mode == "review":
                        self._active_review_tasks.add(t)
                        self._review_workers_spawned += 1
                    spawned_this_round += 1
                    await _emit_bb("worker_spawned", worker=w.solver_id,
                                   phase=worker_mode, intent_id=iid,
                                   worker_role=("review" if worker_mode == "review" else "worker"))
                    open_intents = self._capacity_dispatchable_open_intents(open_intents, tasks)

                # ── liveness: if nothing is running and no intents, keep going.
                # A CTF challenge HAS a unique solution, so "Reason ran dry" does
                # NOT mean unsolvable — it means the current evidence didn't spark a
                # fresh angle. We do not declare exhausted; we force a Reason, and if
                # it still produces nothing, we RE-BOOTSTRAP a fresh whole-challenge
                # attempt (seeded with the board's dead-ends so it tries a NEW angle,
                # not the same path). The only stop reasons are: solved, the operator
                # stops the run, or a finite wall_clock_budget (off by default).
                if not tasks and not open_intents:
                    # NOTE: BOTH auto-pauses that used to live in this fully-idle
                    # branch now sit at the TOP of the loop body, because the
                    # never-give-up Reason engine keeps the swarm busy and an
                    # idle-only pause never fires: the operator-blocked NEED_INPUT
                    # pause (run-11189) and the barren no-progress backpressure
                    # (run-11190's 238-worker spike — formerly collect-mode-only
                    # idle-round counting; now fruitless-worker counting at reap,
                    # all modes). Search "operator-blocked" / "barren backpressure".
                    n = await self._run_reason()
                    open_intents = self._open_intents()
                    if not open_intents:
                        if (n == 0 and self.review_policy.get("on_reason_dry", True)
                                and await self._maybe_start_review(
                                    trigger="reason_dry",
                                    directive="Reason produced no schedulable intents; audit the graph for loops, challenged assumptions, suppressed/reopenable routes, or a needed operator ask.",
                                    healthy=healthy, tasks=tasks,
                                    task_solvers=task_solvers, emit_bb=_emit_bb)):
                            continue
                        try:
                            engine = self._pick_engine(_running_engines(), healthy, role="bootstrap")
                        except RuntimeError as exc:
                            await _emit_bb("worker_spawn_rejected", reason=str(exc),
                                           phase="retry_bootstrap")
                            break
                        try:
                            w = self._make_cli_worker(
                                engine, mode="bootstrap",
                                intent_goal=self._retry_goal())
                        except WorkerSpawnRejected as exc:
                            await _emit_bb("worker_spawn_rejected", reason=str(exc),
                                           engine=str(engine), phase="retry_bootstrap")
                            break
                        except WorkerBudgetExhausted as exc:
                            await _stop_for_budget(str(exc))
                            break
                        t = asyncio.create_task(
                            w.run(), name=f"retry-bootstrap-{engine}")
                        tasks[t] = engine
                        task_solvers[t] = w
                        await _emit_bb("worker_spawned", worker=w.solver_id,
                                       phase="retry_bootstrap", worker_role="worker")
                        await _emit_bb("retry", reason="reason dry — re-bootstrapping "
                                       "a fresh attempt (CTF has a unique solution)")
        finally:
            leftover = [t for t in tasks if not t.done()]
            for t in leftover:
                self._cancel_solver(task_solvers.get(t))
                t.cancel()
            if leftover:
                await asyncio.gather(*leftover, return_exceptions=True)
            if task_lanes and self.shared_graph is not None:
                for t, lane_key in list(task_lanes.items()):
                    solver = task_solvers.get(t)
                    sid = getattr(solver, "solver_id", "") or ""
                    try:
                        rel = self.shared_graph.release_lane(  # type: ignore[attr-defined]
                            actor="coordinator", lane_key=lane_key, by_worker=sid)
                        await self._consume_lane_release(rel, emit_bb=_emit_bb)
                    except Exception:
                        pass
                    task_lanes.pop(t, None)
            if hitl_task is not None:
                hitl_task.cancel()
                await asyncio.gather(hitl_task, return_exceptions=True)
            # M11: if we are leaving via cancel/exception, finalize HERE so the shared
            # graph handle is closed and worker scratch is swept even on the error path
            # (the post-finally finalize below only runs on a clean return). Idempotent.
            try:
                await self._finalize_coordinator_run(
                    winner=winner, flag=flag, goal_complete=goal_complete,
                    per_solver=per_solver)
            except Exception:
                pass

        # M11: finalize (persist winner + close graph + RUN_FINISHED + scratch sweep).
        # Idempotent — the finally above already called this on a cancel/error exit, so
        # the normal path is a no-op here; on a clean return THIS is the call that runs.
        await self._finalize_coordinator_run(
            winner=winner, flag=flag, goal_complete=goal_complete, per_solver=per_solver)
        if winner is not None:
            return SwarmOutcome(True, flag, winner, per_solver, "solved",
                                flags=list(self._found_flags))
        if goal_complete:
            # pentest: engagement goal met (verified findings), no flag to carry.
            return SwarmOutcome(True, None, None, per_solver, "goal_met")
        if self._budget_exhausted_kind:
            return SwarmOutcome(False, None, None, per_solver,
                                "budget_exhausted")
        return SwarmOutcome(False, None, None, per_solver,
                            "coordinator: no verified flag")

    def config_poll_interval(self) -> float:
        """How long asyncio.wait blocks before re-checking stall/intents. Short
        enough to be responsive, long enough not to busy-spin."""
        return 2.0

    def _persist_winner(self, outcome: "Optional[SolveOutcome]", flag: "Optional[str]") -> None:
        """Write the winner's CLI continuation handle to workspace/winner.json so a
        post-solve standby driver can resume the SAME session for a human
        follow-up. Best-effort: a write failure must never fail a solved run.

        Needs graph_dir (web runs) — winner.json lands beside graph/ (a sibling of
        the sandbox root, so sandbox.shutdown_all()'s rmtree can't delete it). TUI
        / test runs without graph_dir simply skip persistence (no standby there)."""
        if self._graph_dir is None or outcome is None:
            return
        session = getattr(outcome, "session", None)
        # only CLI workers carry a session; without one there's nothing to resume.
        if not session:
            return
        try:
            import json
            payload = {
                "engine": getattr(outcome, "engine", "") or "",
                "session": session,
                "workdir": getattr(outcome, "workdir", "") or "",
                "flag": flag or outcome.flag or "",
                # multi-flag: every flag the run collected (the run's authoritative
                # set, not just this one worker's). `flag` stays the first.
                "flags": list(self._found_flags) or (
                    [flag] if flag else (outcome.flags or [])),
                "challenge": self.challenge.model_dump(),
                **self._runtime_metadata_for(outcome),
            }
            dest = self._graph_dir.parent / "winner.json"
            dest.parent.mkdir(parents=True, exist_ok=True)
            dest.write_text(json.dumps(payload, ensure_ascii=False, indent=2))
        except Exception:
            pass


async def run_swarm(
    challenge: Challenge,
    lineup: list[ModelSpec],
    *,
    llm: LLMClient,
    sandbox: SandboxManager,
    bus: Optional[EventBus] = None,
    cost: Optional[CostController] = None,
    artifacts: Optional[ArtifactStore] = None,
    config: Optional[SolverConfig] = None,
    run_id: Optional[str] = None,
) -> SwarmOutcome:
    """Functional entry point mirroring §5.4's run_swarm signature."""
    return await Swarm(
        challenge, lineup, llm=llm, sandbox=sandbox, bus=bus, cost=cost,
        artifacts=artifacts, config=config, run_id=run_id,
    ).run()
