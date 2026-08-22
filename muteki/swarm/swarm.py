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
from muteki.solver.container_exec import WORKER_IMAGE
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
from muteki.solver.workspace import (
    cleanup_worker_scratch,
    ensure_workspace,
    materialize_input,
    run_identity,
    worker_image_identity,
)
from muteki.swarm.insight_bus import InsightBus
from muteki.swarm.stage_policy import StagePolicy
from muteki.swarm.shared_graph import SharedGraph, SQLiteSharedGraph, canonicalize_lane

# Stateless helpers, run-level constants, exception types, and the SwarmOutcome
# dataclass live in swarm_support (code-health G1). Re-exported here so existing
# call sites (`muteki.swarm.swarm.SwarmOutcome`, `WorkerSpawnRejected`,
# `_ensure_blackboard_skill_links`, `_health_cache_clear`, the `_HEALTH_*`/
# `_BLACKBOARD_*`/`_STANDING_MAX`/`_PENDING_HELP_MAX` names, …) are unchanged.
from muteki.swarm.swarm_support import (  # noqa: E402,F401
    _STANDING_MAX,
    _PENDING_HELP_MAX,
    WorkerBudgetExhausted,
    WorkerSpawnRejected,
    SwarmOutcome,
    _CONTAINER_BLACKBOARD_SKILL,
    _BLACKBOARD_SKILL_LINKS,
    _ensure_blackboard_skill_links,
    _HEALTH_PROBE_CACHE,
    _HEALTH_FAILURE_TTL_FRACTION,
    _health_cache_get,
    _health_cache_put,
    _health_cache_clear,
    _is_control_failure,
)

# The Swarm coordinator's methods are split into responsibility mixins
# (code-health G1); they are composed back into the class below, so behavior and
# the public surface are unchanged.
from muteki.swarm.coordinator_flags import _FlagsBusMixin  # noqa: E402
from muteki.swarm.coordinator_race import _RaceHealthMixin  # noqa: E402
from muteki.swarm.coordinator_dispatch import _DispatchReasonMixin  # noqa: E402
from muteki.swarm.coordinator_review import _ReviewLocksMixin  # noqa: E402
from muteki.swarm.coordinator_loop import _CoordinatorLoopMixin  # noqa: E402


def _workspace_runtime_payload(
    *,
    backend: str,
    network: str,
    run_id: str,
    web_access: bool,
    kb: bool,
    coordinator: bool,
    cli_race: bool,
    race_scout: bool,
    protocol2: bool,
    max_workers: int,
    max_total_workers: int | None,
    cost_budget_usd: float | None,
    wall_clock_budget: float,
    stage_policy: StagePolicy,
    worker_profiles: list[dict[str, Any]],
) -> dict[str, Any]:
    requested = network if backend == "container" else ""
    actual = requested
    if backend == "container" and requested == "none":
        actual = "bridge"
    image: dict[str, str] = {"name": "", "id": "", "digest": ""}
    if backend == "container":
        image = worker_image_identity(WORKER_IMAGE)
    coordinator_cfg = stage_policy.coordinator if isinstance(stage_policy.coordinator, dict) else {}
    try:
        token_budget = int(coordinator_cfg.get("token_budget") or 0)
    except (TypeError, ValueError):
        token_budget = 0
    try:
        tool_call_budget = int(coordinator_cfg.get("tool_call_budget") or 0)
    except (TypeError, ValueError):
        tool_call_budget = 0
    seats = []
    for profile in worker_profiles:
        if not isinstance(profile, dict):
            continue
        role = str(profile.get("role") or "")
        roles = profile.get("roles")
        if not role and isinstance(roles, list):
            for item in roles:
                text = str(item or "").strip()
                if text:
                    role = text
                    break
        seats.append({
            "id": str(profile.get("id") or profile.get("name") or ""),
            "name": str(profile.get("name") or ""),
            "engine": str(profile.get("engine") or ""),
            "model": str(profile.get("model") or ""),
            "credential_id": str(profile.get("credential_account") or ""),
            "role": role,
            "enabled": bool(profile.get("enabled", True)),
        })
    wall = None if wall_clock_budget == float("inf") else wall_clock_budget
    return {
        **run_identity(),
        "backend": backend,
        "network": actual,
        "network_requested": requested,
        "run_id": run_id,
        "image": image,
        "seats": seats,
        "budgets": {
            "wall_clock": wall,
            "max_workers": max_workers,
            "max_total_workers": max_total_workers,
            "cost_usd": cost_budget_usd,
            "token": token_budget,
            "tool_call": tool_call_budget,
        },
        "offline": not web_access,
        "kb": bool(kb),
        "coordinator": bool(coordinator),
        "cli_race": bool(cli_race),
        "race_scout": bool(race_scout),
        "protocol": 2 if protocol2 else 1,
    }


class Swarm(
    _FlagsBusMixin,
    _RaceHealthMixin,
    _DispatchReasonMixin,
    _ReviewLocksMixin,
    _CoordinatorLoopMixin,
):
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
        # per-turn timeout (s) for an EXPLORE or BOOTSTRAP worker's turn-1. Short,
        # because this is the ONLY backstop that frees a max_workers slot held by a
        # stuck worker (replacing the old stall-kill). A timed-out worker still gets
        # one conclude turn (min(timeout, 600s)) to summarize before dying.
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
        race_scout: bool = True,  # whole-layer on/off
        race_engines: "Optional[list[str]]" = None,  # which engines race (None = all)
        race_timeout: int = 720,  # short timeout (breadth recon, not deep dig)
        race_rounds: int = 1,  # one round (>1 reintroduces accumulation)
        # cold-start signal (run-75379 BUG④). race-scout is a cold-start warmup for an
        # EMPTY graph; on a reopen/resume of a populated graph (33+ verified facts) it
        # re-races a solved challenge and burns fresh workers. Callers that relaunch on
        # an existing graph_dir (web `resolve`, a standby restart) should pass
        # cold_start=False. Default True keeps fresh runs byte-identical. This is only
        # the EXPLICIT hint — the coordinator ALSO falls back to a graph-state check
        # (_is_cold_start), so a relaunch that forgets to set this is still protected.
        cold_start: bool = True,
        # Which execution generation this Swarm instance belongs to (web resolve
        # bumps it per relaunch; a fresh start is 1). Generation > 1 suffixes every
        # minted worker id (cli-pi → cli-pi-g2) and worker cwd so a continued run's
        # workers never collide with the previous generation's lanes / intents /
        # directories. Default 1 keeps every existing caller byte-identical.
        execution_generation: int = 1,
        # Cognitive cluster planner: reorder open intents + bias engine pick using
        # graph evidence (dead-ends, barren goals, fact continuity, heterogeneity).
        # Default OFF — enable explicitly or via MUTEKI_COGNITIVE_CLUSTER_PLANNER=1.
        cognitive_cluster_planner: bool = False,
        # ── worker execution backend ─────────────────────────────────────────
        # "local"  → workers shell out on the HOST (default; unchanged).
        # "container" → workers run inside the run's isolated Docker execution
        #   node, which mounts ONLY the run workspace and account-scoped credential
        #   material. The image is tool-only; credentials are injected at runtime.
        worker_backend: str = "local",
        worker_network: str = "bridge",
        worker_profiles: "Optional[list[dict]]" = None,
        startup_health_snapshot: "Optional[dict[str, bool]]" = None,
        credential_accounts_root: "Optional[Path]" = None,
        stage_policy: "Optional[dict[str, Any] | StagePolicy]" = None,
        max_total_workers: "Optional[int]" = None,
        cost_budget_usd: "Optional[float]" = None,
        llm_profiles: "Optional[dict[str, Any]]" = None,
        # Optional control-plane registry shared with the web RunManager.  It is
        # deliberately a projection of live worker identity, never evidence.
        worker_registry: "Optional[Any]" = None,
        # Resolve opaque secret:// references only at the final worker injection
        # boundary.  The callback's plaintext result must never enter the graph,
        # command journal, event bus, or coordinator diagnostics.
        secret_resolver: "Optional[Any]" = None,
        context_provider: "Optional[Any]" = None,
        context_binder: "Optional[Any]" = None,
        context_reserver: "Optional[Any]" = None,
        context_committer: "Optional[Any]" = None,
        context_releaser: "Optional[Any]" = None,
        context_delivery_unknown_marker: "Optional[Any]" = None,
        context_status_provider: "Optional[Any]" = None,
        context_expirer: "Optional[Any]" = None,
        standing_clear_provider: "Optional[Any]" = None,
        control_state_provider: "Optional[Any]" = None,
        # Protocol 2 live-canary authority.  When present every CLI worker task is
        # admitted/owned by this session; there is no parallel create_task path.
        protocol2_session: "Optional[Any]" = None,
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
        self.worker_registry = worker_registry
        self._secret_resolver = secret_resolver
        self._context_provider = context_provider
        self._context_binder = context_binder
        self._context_reserver = context_reserver
        self._context_committer = context_committer
        self._context_releaser = context_releaser
        self._context_delivery_unknown_marker = context_delivery_unknown_marker
        # Plaintext resolved from a reserved secret:// resource lives only between
        # materialisation and worker construction. Keys are reservation tuples; the
        # value is transferred to CliSolver's exact-output redactor and popped here.
        self._reserved_context_secret_values: "dict[tuple[str, str], str]" = {}
        self._context_status_provider = context_status_provider
        self._context_expirer = context_expirer
        self._standing_clear_provider = standing_clear_provider
        self._control_state_provider = control_state_provider
        self.protocol2_session = protocol2_session
        # Runtime objects stay process-local; the registry above only exposes
        # serializable WorkerRef rows.  Keeping both lets emergency freeze/cancel
        # touch the real subprocess while API/status readers remain decoupled.
        self._live_solvers: dict[str, Any] = {}
        self._control_frozen = False
        self._freeze_started_at: dict[str, float] = {}
        self._freeze_suspensions: dict[str, str] = {}
        self._budget_suspended_total = 0.0
        self._budget_suspend_started: "Optional[float]" = None
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
            self.stage_policy.coordinator.get("review")
        )
        self.verifier_policy = self._clean_verifier_policy(
            self.stage_policy.coordinator.get("verifier")
        )
        self._last_review_seq = 0
        self._last_review_proposal_seq = 0
        self._last_directive_seq = 0
        # E: last resource-lock event seq surfaced as a board delta (workers acquire
        # locks directly via the blackboard skill; the coordinator mirrors them to UI).
        self._last_resource_seq = 0
        self._active_review_tasks: set[asyncio.Task] = set()
        self._active_verifier_tasks: set[asyncio.Task] = set()
        self._review_workers_spawned = 0
        self._verifier_workers_spawned = 0
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
            or "deepseek-v4-flash"
        )
        self.max_total_workers = (
            int(max_total_workers)
            if max_total_workers not in (None, 0)
            else self.stage_policy.budgets.max_total_workers
        )
        self.cost_budget_usd = (
            float(cost_budget_usd)
            if cost_budget_usd not in (None, 0)
            else self.stage_policy.budgets.cost_budget_usd
        )
        self._spawned_total = 0
        self._budget_exhausted_kind: str | None = None
        self.worker_profiles = self._clean_worker_profiles(worker_profiles)
        # engine roster (deduped) — now profile names. Legacy values like "claude"
        # expand to every enabled claude profile.
        if self.worker_profiles:
            roster = (
                normalize_profile_roster(engines, self.worker_profiles)
                if engines
                else []
            )
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
            _prio = {
                str(p["name"]): (
                    coerce_nonneg_int(p.get("priority"), 100),
                    str(p["name"]),
                )
                for p in self.worker_profiles
            }
            self.engines.sort(key=lambda e: _prio.get(e, (10**9, e)))
        else:
            roster = engines if engines else ["claude", "codex", "pi", "omp"]
            seen: set[str] = set()
            self.engines = [e for e in roster if not (e in seen or seen.add(e))]
        self._profiles_by_name: dict[str, dict] = {
            p["name"]: p for p in self.worker_profiles
        }
        self._startup_health_snapshot = (
            {
                str(profile_id): bool(ok)
                for profile_id, ok in startup_health_snapshot.items()
            }
            if startup_health_snapshot is not None
            else None
        )
        self._profiles_by_engine: dict[str, list[dict]] = {}
        for p in self.worker_profiles:
            self._profiles_by_engine.setdefault(p["engine"], []).append(p)
        for profiles in self._profiles_by_engine.values():
            profiles.sort(
                key=lambda p: (coerce_nonneg_int(p.get("priority"), 100), p["id"])
            )
        self._profile_rr: dict[str, int] = {}
        self._active_profile_by_solver: dict[str, str] = {}
        self._active_profile_role_by_solver: dict[str, str] = {}
        self._active_profile_counts: dict[str, int] = {}
        self._active_review_profile_counts: dict[str, int] = {}
        self._active_verifier_profile_counts: dict[str, int] = {}
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
        self._execution_generation = max(1, int(execution_generation or 1))
        self.race_timeout = int(race_timeout)
        self.race_rounds = max(1, int(race_rounds))
        _rseen: set[str] = set()
        if self.worker_profiles and race_engines is not None:
            _rroster = normalize_profile_roster(race_engines, self.worker_profiles)
        else:
            _rroster = race_engines if race_engines is not None else self.engines
        self.race_engines = [
            e
            for e in _rroster
            if e in self.engines and not (e in _rseen or _rseen.add(e))
        ]
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
        from muteki.swarm.cognitive_cluster_planner import planner_enabled_from_env

        self.cognitive_cluster_planner = bool(
            cognitive_cluster_planner or planner_enabled_from_env()
        )
        # worker_root: a persistent per-run dir under which each CLI worker gets
        # its OWN cwd (worker_root/{solver_id}-{n}/) instead of a system $TMPDIR
        # mkdtemp. The web driver points this at sessions/{id}/workspace/workers/
        # so a run's worker scratch (staged attachments, agent-extracted files,
        # PoCs) lives under the run's folder — inspectable after the run and
        # cleaned up with it. None → fall back to mkdtemp (TUI / tests).
        self.worker_root = Path(worker_root) if worker_root is not None else None
        self.workspace_root = (
            self.worker_root.parent if self.worker_root is not None else None
        )
        if self.workspace_root is not None:
            ensure_workspace(
                self.workspace_root,
                runtime=_workspace_runtime_payload(
                    backend=worker_backend,
                    network=worker_network if worker_network in {"bridge", "host", "none"} else "bridge",
                    run_id=self.run_id,
                    web_access=web_access,
                    kb=kb,
                    coordinator=coordinator,
                    cli_race=cli_race,
                    race_scout=self.race_scout,
                    protocol2=protocol2_session is not None,
                    max_workers=max_workers,
                    max_total_workers=self.max_total_workers,
                    cost_budget_usd=self.cost_budget_usd,
                    wall_clock_budget=wall_clock_budget,
                    stage_policy=self.stage_policy,
                    worker_profiles=self.worker_profiles,
                ),
            )
            # Provision player-facing files before the shared graph is opened.  The
            # graph/board must never retain the operator's source path (for example a
            # benchmark directory containing challenge.json, README solutions, or a
            # reference solver).  Workers see only immutable run-local CAS aliases.
            # This is an information-boundary fix, not a flag-gate relaxation.
            provisioned: list[str] = []
            for attachment in challenge.attachments or []:
                source = Path(attachment)
                try:
                    row = materialize_input(
                        self.workspace_root, source, name=source.name
                    )
                except (OSError, FileNotFoundError) as exc:
                    raise RuntimeError(
                        f"AttachmentProvisioningFailed: {source.name}"
                    ) from exc
                provisioned.append(str(Path(row["by_name"]).absolute()))
            if provisioned:
                challenge = challenge.model_copy(update={"attachments": provisioned})
                self.challenge = challenge
        self.credential_accounts_root = (
            Path(credential_accounts_root).expanduser().resolve()
            if credential_accounts_root is not None
            else None
        )
        # worker execution backend: "local" (host subprocess) or "container" (workers
        # run in the run's Kali tool container for a consistent toolchain). The
        # ContainerHandle is created lazily on first worker spawn (worker_root first).
        self.worker_backend = worker_backend
        self.worker_network = (
            worker_network if worker_network in {"bridge", "host", "none"} else "bridge"
        )
        self._container_handle = (
            None  # set lazily by _container() when backend=container
        )
        self._container_unavailable = False
        self._runtime_degraded: list[dict[str, Any]] = []
        self._agent_state_dirs: set[Path] = set()
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
            os.environ.get("MUTEKI_HEALTH_PROBE_TTL", "120") or 120
        )
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
        # A cancellation-suppressing control callback is fenced from claiming new
        # commands, but may still own its current state mutation. Shutdown waits a
        # bounded interval and then reports incomplete while retaining these refs;
        # it must never silently finalize underneath the orphan.
        self.control_shutdown_timeout = 2.0
        self._control_shutdown_incomplete = False
        self._shutdown_incomplete_causes: "set[str]" = set()
        self._control_orphan_tasks: "set[asyncio.Task[Any]]" = set()
        # Independently-live CLI runner/process owners are not control callbacks.
        # The HITL supervisor may cancel its own orphan handlers, but must never
        # cancel these runtime reapers.
        self._worker_runtime_reapers: "dict[str, asyncio.Task[Any]]" = {}
        self._worker_runtime_owners: "dict[str, tuple[Any, str, str, str]]" = {}
        self._worker_runtime_incomplete = False
        self._retired_worker_refs: "list[Any]" = []
        self._context_cleanup_owners: "dict[str, tuple[list[tuple[str, str]], str]]" = {}
        self._context_cleanup_incomplete = False
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
        self._last_reason = None
        self._last_planner_failure = None
        # GRACEFUL_DRAIN forbids new dispatch while the ordinary reap loop keeps
        # collecting in-flight workers. It is not a soft-pause wait latch.
        self._operator_draining: bool = False
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
        if self.protocol2_session is not None:
            # Protocol 2 workers report through the candidate broker/capture
            # callbacks installed by Protocol2RunSession. The legacy graph is not
            # opened even as a fallback; JSONL/SSE remains display-only.
            self._graph_dir = None
            self._search_state_port = None
        else:
            try:
                # graph_dir (web driver) keeps the DB OUTSIDE sandbox.root so it
                # survives sandbox.shutdown_all()'s rmtree of the sandbox root. Falls
                # back to the sandbox tree when unset (TUI / tests, where ephemeral
                # is fine).
                if graph_dir is not None:
                    base = Path(graph_dir)
                    base.mkdir(parents=True, exist_ok=True)
                    db_path = base / "shared_graph.db"
                elif self.workspace_root is not None:
                    # Constructor-level tests and non-Web composition roots may supply
                    # a persistent worker_root without a SandboxManager. The run
                    # workspace is still an explicit authority location; derive the
                    # sibling graph path instead of dereferencing sandbox=None.
                    base = self.workspace_root / "graph"
                    base.mkdir(parents=True, exist_ok=True)
                    db_path = base / "shared_graph.db"
                else:
                    db_path = self.sandbox.root / self.run_id / "shared_graph.db"
                # remember where durable per-run state lives (sibling of graph/) so a
                # post-solve standby can find private continuation state and the
                # shared graph again.
                self._graph_dir = Path(graph_dir) if graph_dir is not None else None
                self.shared_graph = SQLiteSharedGraph.open(
                    db_path=db_path,
                    challenge=challenge,
                    artifacts=artifacts,
                )
                from muteki.swarm.state_port import V1SearchStatePort

                self._search_state_port = V1SearchStatePort(
                    run_id=self.run_id, graph=self.shared_graph
                )
                state_provider = getattr(self, "_control_state_provider", None)
                if callable(state_provider):
                    state = state_provider()
                    if (
                        str(getattr(getattr(state, "mode", None), "value", ""))
                        == "active"
                    ):
                        self.shared_graph.recover_suspended_leases()
            except Exception as exc:
                # Evidence authority is a prerequisite.  Continuing with
                # shared_graph=None launders an infrastructure failure into an empty
                # business state and lets workers run without the provenance spine.
                self.shared_graph = None
                self._search_state_port = None
                raise RuntimeError(f"SharedGraphUnavailable: {exc}") from exc
        prepare = getattr(self, "framework_prepare_hook", None)
        if callable(prepare):
            try:
                prepare()
            except Exception:
                pass
        self._last_graph_event_seq = 0
        self._graph_bridge_failures: dict[int, int] = {}
        # multi-flag: the authoritative dedup set of flags collected so far. The
        # run is "solved" once it holds expected_flags distinct flags. For a
        # single-flag challenge (expected_flags=1) the first flag fills it and
        # _flags_complete() flips true immediately — byte-identical to the old
        # "first flag wins" behaviour.
        self._found_flags: list[str] = []
        self._found_findings: list[dict] = []
        self._found_reports: list[dict] = []
        self._coverage_exhausted: bool = False


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
        challenge,
        lineup,
        llm=llm,
        sandbox=sandbox,
        bus=bus,
        cost=cost,
        artifacts=artifacts,
        config=config,
        run_id=run_id,
    ).run()
