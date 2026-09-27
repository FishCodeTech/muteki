"""Coordinator policy and solver construction helpers. Moved from coordinator_flags.py."""

from __future__ import annotations

import asyncio
import time
from dataclasses import dataclass, field
from typing import TYPE_CHECKING, Any

if TYPE_CHECKING:
    pass

from muteki.solver.worker_profiles import (
    base_engine_for_profile,
    worker_identity_fields,
)
from muteki.core.events import Event, EventType, blackboard_delta_payload
from muteki.solver.types import SolveOutcome
from muteki.swarm.swarm_support import (
    WorkerBudgetExhausted,
    ControlShutdownIncomplete,
    spawn_reject_should_emit,
    _health_cache_get,
    _health_cache_put,
)


@dataclass(slots=True)
class CoordinatorRunState:
    """Mutable state shared by the coordinator's serial scheduling stages."""

    hitl_task: asyncio.Task[Any] | None = None
    tasks: dict[asyncio.Task[Any], str] = field(default_factory=dict)
    task_intents: dict[asyncio.Task[Any], str] = field(default_factory=dict)
    task_solvers: dict[asyncio.Task[Any], Any] = field(default_factory=dict)
    task_lanes: dict[asyncio.Task[Any], str] = field(default_factory=dict)
    task_started_at: dict[asyncio.Task[Any], float] = field(default_factory=dict)
    task_prog_ckpt: dict[asyncio.Task[Any], tuple[int, int]] = field(default_factory=dict)
    task_tool_count: dict[asyncio.Task[Any], int] = field(default_factory=dict)
    task_last_tool_t: dict[asyncio.Task[Any], float] = field(default_factory=dict)
    fruitless_interrupt_tasks: set[asyncio.Task[Any]] = field(default_factory=set)
    fruitless_interrupt_count: int = 0
    force_reason_after_fruitless_interrupt: bool = False
    last_fruitless_interrupt_meta: dict[str, Any] = field(default_factory=dict)
    pending_interrupt_reason_recovery: bool = False
    interrupt_rebootstrap_count: int = 0
    interrupt_empty_reason_retries: int = 0
    interrupt_chain_intent_injected: bool = False
    last_interrupt_named_artifacts: list[str] = field(default_factory=list)
    last_interrupt_replan_domain: str = ""
    solo_verify_last_t: dict[asyncio.Task[Any], float] = field(default_factory=dict)
    solo_verify_tool_ckpt: dict[asyncio.Task[Any], int] = field(default_factory=dict)
    solo_verify_count: int = 0
    per_solver: dict[str, SolveOutcome] = field(default_factory=dict)
    winner: str | None = None
    flag: str | None = None
    goal_complete: bool = False
    healthy: list[str] = field(default_factory=list)
    cold_start: bool = False
    race_missed: bool = False
    race_reasoned_wm: int = -1
    t0: float = 0.0
    last_fact_count: int = 0
    reason_fact_ckpt: int = 0
    reason_open_intent_ckpt: int = 0
    last_decided_wm: int = -1
    last_consumed_wm: int = -1
    decide_followup_pending: bool = False
    fruitless_workers: int = 0
    prog_fact_ckpt: int = 0
    prog_flag_ckpt: int = 0
    prog_report_ckpt: int = 0
    # A CTF run with no current work remains live until the goal is met, the
    # operator stops it, or an explicitly configured budget is exhausted.
    # Empty plans schedule another Decide pass. Repeated Planner transport/runtime
    # failures are bounded separately so a broken endpoint cannot spin forever.
    reason_retry_pending: bool = False
    reason_retry_count: int = 0
    reason_retry_not_before: float = 0.0
    reason_retry_failure: str = ""
    # Compatibility telemetry: each serialized CTF Decide pass is recorded as
    # an epoch. These counters do not gate future Decide passes or Worker admission.
    ctf_batch_id: int = 0
    ctf_batch_spawned: int = 0
    ctf_batch_reaped: int = 0
    ctf_batch_sealed: bool = False
    ctf_intent_batches: dict[str, int] = field(default_factory=dict)
    # Fact-count checkpoint retained for retry telemetry. CTF refill no longer
    # waits merely because unrelated ordinary Workers remain active.
    reason_duplicate_fact_ckpt: int = -1
    reason_task: asyncio.Task[int] | None = None
    reason_started_wm: int = -1
    reason_started_trigger: str = ""
    reason_next_trigger: str = ""
    reason_requested_intents: int = 0
    reason_started_max_intents: int = 0
    reason_free_slots: int = 0
    reason_queue_depth: int = 0
    reason_result_ready: bool = False
    reason_stale_refresh: bool = False
    # Planning-input markers are retained for non-CTF scheduling and metrics.
    # CTF scheduling consumes accumulated evidence at batch boundaries.
    planning_input_changed: bool = False
    reason_dispatch_barrier: bool = False
    planner_terminal_failure: bool = False
    runtime_terminal_failure: bool = False
    intent_failed_engines: dict[str, set[str]] = field(default_factory=dict)
    # A contracted Worker that reached its lease boundary must hand its
    # checkpoint to Reason before the same Intent can be dispatched again.
    # The watermark records which planner snapshot is new enough to adjudicate
    # that handoff; this matters when another Worker times out while Reason is
    # already running on an older snapshot.
    checkpoint_replan_wm: dict[str, int] = field(default_factory=dict)
    engine_role_refusals: dict[tuple[str, str], int] = field(default_factory=dict)
    role_quarantined_engines: dict[str, set[str]] = field(default_factory=dict)
    last_pause_fruitless: int = -1
    last_progress_t: float = 0.0
    last_compact_t: float = 0.0
    compact_no_progress_s: float = 1800.0
    termination_conclude_reason: str = ""
    termination_conclude_worker: str = ""
    termination_conclude_deadline: float = 0.0
    termination_conclude_requested: set[asyncio.Task[Any]] = field(
        default_factory=set
    )
    done: set[asyncio.Task[Any]] = field(default_factory=set)
    reaped_n: int = 0
    completed_for_review_n: int = 0
    open_intents: list[dict[str, Any]] = field(default_factory=list)
    need_reason: bool = False
    wm_now: int = 0
    graph_grew: bool = False
    intents_consumed: bool = False
    semantic_changed: bool = False
    reason_proposed_n: int = 0
    last_reason_error: str = ""


async def emit_scheduler_bb(
    self, state: CoordinatorRunState, kind: str, **fields: Any,
) -> None:
    if kind == "worker_spawn_rejected":
        capacity = (
            int(getattr(self, "max_workers", 0) or 0),
            int(getattr(self, "_spawned_total", 0) or 0),
            len(state.tasks),
        )
        if not spawn_reject_should_emit(
            self,
            reason=str(fields.get("reason") or ""),
            phase=str(fields.get("phase") or ""),
            engine=str(fields.get("engine") or ""),
            capacity=capacity,
        ):
            return
    if self.bus is not None:
        actor = str(fields.pop("actor", "") or "coordinator")
        await self.bus.emit(Event(
            event_type=EventType.BLACKBOARD_DELTA,
            run_id=self.run_id,
            challenge_id=self.challenge.id,
            payload=blackboard_delta_payload(
                kind, actor=actor, **fields),
        ))


def running_engines(state: CoordinatorRunState) -> list[str]:
    return list(state.tasks.values())


async def stop_for_budget(
    self, state: CoordinatorRunState, kind: str,
) -> bool:
    """Apply a budget boundary and report whether the run must stop now.

    A worker-count cap governs admission, not already admitted work.  Once the
    cap is reached, the scheduler drains current Workers and only finalizes the
    budget result after the active set is empty.  Cost remains a hard run-level
    boundary because ongoing work can continue spending it.
    """
    if kind == "worker_budget_exhausted":
        self._worker_admission_closed = True
        if not self._worker_admission_event_emitted:
            await emit_scheduler_bb(
                self, state, kind,
                spawned_total=self._spawned_total,
                max_total_workers=self.max_total_workers,
                active_workers=len(state.tasks),
                admission_closed=True,
                draining=bool(state.tasks),
            )
            self._worker_admission_event_emitted = True
        if state.tasks:
            return False
        self._budget_exhausted_kind = kind
        await emit_scheduler_bb(
            self, state, "budget_exhausted",
            budget_kind=kind,
            spawned_total=self._spawned_total,
            active_workers=0,
            drained=True,
        )
        return True

    self._budget_exhausted_kind = kind
    await emit_scheduler_bb(
        self, state, kind,
        spawned_total=self._spawned_total,
        max_total_workers=self.max_total_workers,
        cost_usd=self._current_cost_usd(),
        cost_budget_usd=self.cost_budget_usd,
    )
    await emit_scheduler_bb(
        self, state, "budget_exhausted",
        budget_kind=kind,
        spawned_total=self._spawned_total,
        cost_usd=self._current_cost_usd(),
    )
    for task in state.tasks:
        self._cancel_solver(state.task_solvers.get(task))
        task.cancel()
    return True


def sync_worker_start_marks(self, state: CoordinatorRunState) -> None:
    now = time.monotonic()
    fact_count = self._verified_fact_count()
    flag_count = len(self._found_flags)
    for task in state.tasks:
        if task not in state.task_started_at:
            state.task_started_at[task] = now
            state.task_prog_ckpt[task] = (fact_count, flag_count)

@staticmethod
def _clean_review_policy(value: Any) -> dict[str, Any]:
    configured = isinstance(value, dict)
    raw = value if configured else {}
    # Keep Review live on unresolved work. The capacity/cooldown gates below
    # bound its cost, while these triggers ensure an independent arbiter sees
    # race misses, repeated failure, planner course corrections, and accumulating
    # candidate evidence before the run drifts or stalls.
    defaults = {
        "enabled": configured,
        "engine": "",
        "reasoning_effort": "inherit",
        "after_race": False,
        "after_fruitless_workers": 3,
        "after_duplicate_intents": 2,
        "on_course_correct": False,
        "on_candidate_spike": False,
        "on_operator_hint": False,
        "on_evidence_conflict": True,
        "on_semantic_duplicate": False,
        "every_completed_workers": 0,
        "candidate_spike_threshold": 5,
        "max_concurrent": 1,
        "allow_review_fallback": False,
        "cooldown_events": 8,
        "timeout": 90,
        "max_review_workers": 12,
        "max_challenges_per_cycle": 8,
    }
    out = dict(defaults)
    for key in ("enabled", "after_race", "on_course_correct",
                "on_candidate_spike", "on_operator_hint", "on_evidence_conflict",
                "on_semantic_duplicate", "allow_review_fallback"):
        if key in raw:
            out[key] = bool(raw.get(key))
    if raw.get("engine"):
        out["engine"] = str(raw.get("engine")).strip()
    review_effort = str(raw.get("reasoning_effort") or "inherit").strip().lower()
    if review_effort in {
        "inherit", "default", "none", "minimal", "low", "medium",
        "high", "xhigh", "max",
    }:
        out["reasoning_effort"] = review_effort
    for key in ("after_fruitless_workers", "after_duplicate_intents",
                "every_completed_workers", "candidate_spike_threshold",
                "max_concurrent", "max_challenges_per_cycle",
                "cooldown_events", "timeout", "max_review_workers"):
        if key in raw:
            try:
                out[key] = max(0, int(raw.get(key)))
            except (TypeError, ValueError):
                pass
    out.update({
        "after_race": False,
        "on_course_correct": False,
        "on_operator_hint": False,
        "on_semantic_duplicate": False,
        "every_completed_workers": 0,
        "max_concurrent": 1,
    })
    out["timeout"] = max(30, min(90, int(out["timeout"])))
    return out


@staticmethod
def _clean_verifier_policy(value: Any) -> dict[str, Any]:
    configured = isinstance(value, dict)
    raw = value if configured else {}
    defaults = {
        "enabled": True,
        "engine": "",
        "reasoning_effort": "inherit",
        "max_concurrent": 0,
        "allow_verifier_fallback": False,
        "timeout": 240,
        "max_verifier_workers": 24,
    }
    out = dict(defaults)
    for key in ("enabled", "allow_verifier_fallback"):
        if key in raw:
            out[key] = bool(raw.get(key))
    if raw.get("engine"):
        out["engine"] = str(raw.get("engine")).strip()
    verifier_effort = str(raw.get("reasoning_effort") or "inherit").strip().lower()
    if verifier_effort in {
        "inherit", "default", "none", "minimal", "low", "medium",
        "high", "xhigh", "max",
    }:
        out["reasoning_effort"] = verifier_effort
    for key in ("max_concurrent", "timeout", "max_verifier_workers"):
        if key in raw:
            try:
                out[key] = max(0, int(raw.get(key)))
            except (TypeError, ValueError):
                pass
    return out


def _engine_healthcheck_cached(self, name: str, role: str) -> bool:
    """bool liveness for one engine, served from the shared health-probe cache
    (same TTL as _healthy_engines) so the race path doesn't re-shell a CLI we
    just verified on the coordinator path (or a prior dispatch). On a miss it
    probes once and caches the verdict."""
    startup_verdict = self._startup_health_verdict(name, role)
    if startup_verdict is not None:
        return startup_verdict[0]


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
        # engines whose healthcheck passes. ONE worker per healthy engine — independent of
        # the lineup size — so they genuinely race the same challenge (the
        # lineup specs only supply solver_id labels, cycled).
        engines = [e for e in self.engines if _healthy(e, "race")]
    else:
        # A failed selected engine is explicit; never substitute another CLI.
        if _healthy(self.cli_engine, "bootstrap"):
            engines = [self.cli_engine]
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
        transport = base_engine_for_profile(profile or engine)
        if (self._context_requires_secure_prompt(engine=transport)
                and not self._secure_prompt_candidate_ready(
                    engine, role=role)):
            # Secret delivery capability is a scheduling constraint, not a
            # failed spawn. Skip before charging the lifetime worker budget or
            # reserving one-shot context so a higher-priority Cursor profile
            # cannot starve a secure Claude/Codex racer.
            continue
        try:
            self._reserve_worker_spawn()
        except WorkerBudgetExhausted:
            break
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
            label += self._gen_suffix()
        else:
            label = f"cli-{transport}"
        expected_solver_id = (
            label if self.cli_race
            else str(getattr(specs[i % len(specs)], "solver_id", "") or label)
        )
        (typed_guidance, typed_context_reservations, typed_endpoint,
         typed_prompt_manifest) = (
            self._typed_context_for_worker(
                worker_id=expected_solver_id, engine=transport))
        try:
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
                target_epoch=self._target_epoch,
                standing_guidance=typed_guidance,
                hitl_cmd=({"action": "redirect", "url": typed_endpoint}
                          if typed_endpoint else None),
                container=container,
                worker_env=self._runtime_env_for(
                    transport, label, container=container, profile=profile),
                identity=worker_identity_fields(profile),
            # EXEC-01: Profile→Adapter 解析与 AgentSession 监督（CLI 路径不变）。
                session_supervisor=self._worker_session_supervisor(),
                worker_profile=profile,
            )
        except Exception as exc:
            rollback_ok = self._release_typed_context_reservations(
                typed_context_reservations, expected_solver_id)
            for built in workers:
                if not self._release_worker_account(built):
                    self._retain_worker_retirement_owner(
                        built, reason="race worker construction rollback")
                    rollback_ok = False
            if not rollback_ok:
                raise ControlShutdownIncomplete(
                    "race worker construction rollback unconfirmed") from exc
            raise
        worker.engine = transport
        worker._pending_control_context_reservations = list(
            typed_context_reservations)
        worker._control_context_prompt_manifest = list(typed_prompt_manifest)
        worker._control_context_prompt_manifest_finalized = False
        worker._control_secret_values = self._take_context_secret_values(
            typed_context_reservations)
        worker._context_committer = getattr(self, "_context_committer", None)
        worker._context_releaser = getattr(self, "_context_releaser", None)
        worker._context_delivery_unknown_marker = getattr(
            self, "_context_delivery_unknown_marker", None)
        try:
            self._claim_worker_account(
                worker.solver_id, transport, profile, role=role)
            self._register_control_worker(
                worker, engine=transport, role=role,
                intent_id=str(getattr(worker, "intent_id", "") or ""),
            )
        except Exception as exc:
            rollback_ok = self._release_worker_account(worker)
            if not rollback_ok:
                self._retain_worker_retirement_owner(
                    worker, reason="race worker registration rollback")
            for built in workers:
                if not self._release_worker_account(built):
                    self._retain_worker_retirement_owner(
                        built, reason="race worker registration rollback")
                    rollback_ok = False
            if not rollback_ok:
                raise ControlShutdownIncomplete(
                    "race worker registration rollback unconfirmed") from exc
            raise
        workers.append(worker)
    return workers


async def _reconcile_blackboard_skill(self) -> None:
    """Legacy hook kept for call-order compatibility.

    The skill is projected when each Worker builds its environment.  This hook
    deliberately performs no user-home writes.
    """
    return
