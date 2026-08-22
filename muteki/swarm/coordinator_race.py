"""run()/race, engine health probing, profile selection, worker spawn.

Split out of ``swarm.py`` (code-health G1) as a mixin of ``Swarm``. Every method
body is byte-for-byte the original; the mixin is composed back into ``Swarm`` so
behavior and the public surface are unchanged. Instance state built in
``Swarm.__init__`` is resolved through the composed class at runtime.
"""

# ruff: noqa: F401
from __future__ import annotations

import asyncio
import hashlib
import inspect
import json
import os
import re
import shutil
import tempfile
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
    normalize_reasoning_effort,
    profile_names,
    apply_worker_identity_env,
    worker_identity_fields,
)
from muteki.solver.workspace import cleanup_worker_scratch, ensure_workspace
from muteki.swarm.insight_bus import InsightBus
from muteki.swarm.stage_policy import StagePolicy
from muteki.swarm.shared_graph import SharedGraph, SQLiteSharedGraph, canonicalize_lane
from muteki.swarm.swarm_support import (
    _STANDING_MAX,
    _PENDING_HELP_MAX,
    WorkerBudgetExhausted,
    WorkerSpawnRejected,
    RequiredContextUnavailable,
    ControlShutdownIncomplete,
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


class _RaceHealthMixin:
    async def run(self) -> SwarmOutcome:
        # Single authoritative teardown for the run's worker container, covering EVERY
        # exit path of every solve mode: the coordinator's race-scout fast-path return,
        # its main-loop finally, the race-only path, and exceptions. The per-method
        # cleanups cancel worker tasks but the CONTAINER (and its idle supervisor) must
        # be removed exactly when the run truly ends — doing it here guarantees a
        # solved / stopped / budget-exhausted / errored run never leaks a container.
        # Cheap + idempotent when there's no container (local backend) or it's already
        # gone. A later resolve()/standby re-creates a fresh container via ensure_container.
        try:
            # Startup projections belong inside the teardown fence. A provider,
            # graph, or telemetry failure must not bypass container cleanup.
            await self._reconcile_blackboard_skill()
            await self._reconcile_standing_guidance()
            await self._reconcile_control_continuations()
            if self.coordinator and self.executor == "cli":
                return await self._run_coordinator()
            return await self._run_race()
        finally:
            if (not self._shutdown_owners_incomplete()
                    and (self.worker_backend == "container"
                         or self._container_handle is not None)):
                try:
                    from muteki.solver.container_exec import teardown_container
                    removed = await asyncio.to_thread(
                        teardown_container, self.run_id, remove=True)
                except Exception:
                    removed = False
                if removed is not True:
                    self._mark_shutdown_incomplete("container_absence")
                    if not getattr(self, "_deferred_control_finalization", None):
                        self._retain_control_shutdown_owner(
                            winner=None, flag=None, goal_complete=False,
                            per_solver={})
                    raise ControlShutdownIncomplete(
                        "container teardown could not be proven")
            if not self._shutdown_owners_incomplete():
                for state_dir in list(getattr(self, "_agent_state_dirs", set())):
                    shutil.rmtree(state_dir, ignore_errors=True)
                getattr(self, "_agent_state_dirs", set()).clear()

    @staticmethod
    def _cancel_solver(solver: Any) -> bool:
        """Stop a solver's underlying work (kills a CLI worker's subprocess). A
        plain task.cancel() only unschedules the asyncio task — the shelled CLI
        agent kept running. Solvers that don't expose cancel() (code-driven) are a
        no-op here; the task cancel still stops them between turns."""
        if solver is None:
            return False
        fn = getattr(solver, "cancel", None)
        if callable(fn):
            try:
                result = fn()
                # New runtime-aware solvers return False when the kill request did
                # not cross the control boundary.  Legacy solvers return None after
                # accepting the request; preserve that request-level contract.
                return result is not False
            except Exception:
                return False
        return False

    async def _run_race(self) -> SwarmOutcome:
        solvers = self._build_solvers()
        hitl_task: Optional[asyncio.Task] = None
        tasks: dict[asyncio.Task[SolveOutcome], Any] = {}
        per_solver: dict[str, SolveOutcome] = {}
        winner: Optional[str] = None
        flag: Optional[str] = None

        try:
            if self.hitl_inbox is not None:
                hitl_task = asyncio.create_task(
                    self._supervise_control_drain(), name="hitl-drain")
            for solver in solvers:
                task = await self._schedule_control_worker(
                    solver, name=solver.solver_id)
                tasks[task] = solver
        except BaseException:
            for task, solver in tasks.items():
                if not task.done():
                    self._cancel_solver(solver)
                    task.cancel()
            if tasks:
                await asyncio.gather(*tasks, return_exceptions=True)
            for solver in solvers:
                await self._retire_worker_account(
                    solver,
                    intent_id=str(
                        getattr(solver, "intent_id_assigned", "")
                        or getattr(solver, "_intent_id", "") or ""),
                    reason="race task acquisition aborted",
                )
            if hitl_task is not None:
                hitl_task.cancel()
                await asyncio.gather(hitl_task, return_exceptions=True)
            if self._shutdown_owners_incomplete():
                self._retain_control_shutdown_owner(
                    winner=None, flag=None, goal_complete=False, per_solver={})
                raise ControlShutdownIncomplete(
                    "race acquisition cleanup incomplete")
            raise

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
                        if bool(getattr(s, "_remote_start_uncertain", False)):
                            self._mark_shutdown_incomplete("remote_start_uncertain")
                            for other in pending:
                                self._cancel_solver(tasks.get(other))
                                other.cancel()
                            raise ControlShutdownIncomplete(
                                "remote worker start outcome is uncertain")
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
                await self._retire_worker_account(
                    s,
                    intent_id=str(
                        getattr(s, "intent_id_assigned", "")
                        or getattr(s, "_intent_id", "") or ""),
                    reason="race worker shutdown",
                )
            # tear down the HITL drain background task too
            if hitl_task is not None:
                hitl_task.cancel()
                await asyncio.gather(hitl_task, return_exceptions=True)
            if self._shutdown_owners_incomplete():
                self._retain_control_shutdown_owner(
                    winner=winner, flag=flag, goal_complete=False,
                    per_solver=per_solver)
                raise ControlShutdownIncomplete(
                    "control shutdown incomplete; runtime owner retained")
            # container backend: one run-level Docker execution node is shared by
            # workers; force-remove it here so cancels do not leave a stale runtime.
            if self.worker_backend == "container" or self._container_handle is not None:
                try:
                    from muteki.solver.container_exec import teardown_container
                    removed = await asyncio.to_thread(
                        teardown_container, self.run_id, remove=True)
                except Exception:
                    removed = False
                if removed is not True:
                    self._mark_shutdown_incomplete("container_absence")
                    self._retain_control_shutdown_owner(
                        winner=winner, flag=flag, goal_complete=False,
                        per_solver=per_solver)
                    raise ControlShutdownIncomplete(
                        "container teardown could not be proven")

        # the winning Solver already broadcast FlagFound to the global bus; the
        # swarm just reports the aggregate outcome.
        if winner is not None:
            # persist the winner's CLI session handle so a post-solve standby
            # driver can resume the SAME worker for a human follow-up.
            self._persist_winner(
                per_solver.get(winner), flag, worker_id=str(winner or ""))
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

    def _startup_health_verdict(
        self, name: str, role: str,
    ) -> tuple[bool, str] | None:
        snapshot = getattr(self, "_startup_health_snapshot", None)
        if snapshot is None:
            return None
        profile = getattr(self, "_profiles_by_name", {}).get(name)
        if profile is None:
            try:
                profile = self._profile_for_engine(
                    name, role=role, advance=False)
            except Exception:
                profile = None
        profile_id = str(
            (profile or {}).get("id")
            or (profile or {}).get("name")
            or name
        )
        ok = bool(snapshot.get(profile_id, snapshot.get(name, False)))
        return ok, (
            "startup readiness passed"
            if ok else "profile missing from startup readiness snapshot"
        )

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
        startup_verdict = self._startup_health_verdict(name, role)
        if startup_verdict is not None:
            return startup_verdict

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

    def _healthy_engines(
        self,
        *,
        role: str = "bootstrap",
        owner_loop: asyncio.AbstractEventLoop | None = None,
    ) -> list[str]:
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

        if getattr(self, "_startup_health_snapshot", None) is not None:
            engines: list[str] = []
            for candidate in candidates:
                healthy, detail = self._startup_health_verdict(candidate, role) or (
                    False, "profile missing from startup readiness snapshot")
                if healthy:
                    engines.append(candidate)
                else:
                    self._note_engine_degraded(
                        candidate,
                        detail,
                        role=role,
                        owner_loop=owner_loop,
                    )
            return engines

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
                self._note_engine_recovered(e, owner_loop=owner_loop)
            else:
                # configured to fight but the CLI can't complete a turn right now
                # (auth/quota/binary) → drop from the roster AND tell the operator
                # why, instead of vanishing the engine from the worker panel.
                self._note_engine_degraded(
                    e,
                    detail or "health check failed",
                    role=role,
                    owner_loop=owner_loop,
                )
        if engines:
            return engines
        # Health is an execution prerequisite.  An empty eligible roster is a
        # typed pause condition, never permission to launch an unprobed default.
        return []

    async def _healthy_engines_async(self, *, role: str = "bootstrap") -> list[str]:
        """Run CLI health probes off the FastAPI/coordination event loop.

        `_healthy_engines()` shells real CLI probes (`subprocess.run`, retries,
        60s hello timeouts). Calling it directly from `run()` or review scheduling
        can freeze the single uvicorn worker: all API/SSE requests queue until the
        probe returns. Keep the existing sync helper for tests/direct callers, but
        use this wrapper from async production paths.
        """
        owner_loop = asyncio.get_running_loop()
        health_check = self._healthy_engines
        try:
            parameters = inspect.signature(health_check).parameters
        except (TypeError, ValueError):
            parameters = {}
        kwargs: dict[str, Any] = {}
        if "role" in parameters:
            kwargs["role"] = role
        if "owner_loop" in parameters:
            kwargs["owner_loop"] = owner_loop
        return await asyncio.to_thread(health_check, **kwargs)

    def _context_requires_secure_prompt(
        self, *, engine: str = "", worker_id: str = "",
        intent_id: str = "", lane: str = "",
    ) -> bool:
        """Read-only scheduling preflight for applicable secret context.

        This deliberately runs before profile/account/budget/intent acquisition.
        It never resolves a secret and never reserves one-shot capacity; it only
        recognizes that the eventual prompt needs the secure stdin transport.
        """
        provider = getattr(self, "_context_provider", None)
        if not callable(provider):
            return str(intent_id or "").startswith("I-control-")
        try:
            resources = list(provider())
        except Exception:
            # An exact continuation is fail-closed. Ordinary global scheduling can
            # proceed and `_typed_context_for_worker` will simply inject nothing on
            # a transient provider read failure, matching its existing contract.
            return str(intent_id or "").startswith("I-control-")
        for resource in resources:
            scope = str(getattr(resource, "scope", "global") or "global")
            value = scope.split(":", 1)[1] if ":" in scope else scope
            applies = bool(
                scope in ("global", self.challenge.id,
                          f"challenge:{self.challenge.id}")
                or (scope.startswith("run:") and value == self.run_id)
                or (worker_id and scope.startswith(("solver:", "worker:"))
                    and value == worker_id)
                or (worker_id and scope == worker_id)
                or (scope.startswith("engine:") and value == engine)
                or (scope.startswith("intent:") and value == intent_id)
                or (intent_id and scope == intent_id)
                or (scope.startswith("lane:") and value == lane)
                or (lane and scope == lane)
            )
            if not applies:
                continue
            content = str(getattr(resource, "content", "") or "")
            kind = getattr(resource, "kind", "")
            kind_value = str(getattr(kind, "value", kind) or "")
            taint = getattr(resource, "taint", "")
            taint_value = str(getattr(taint, "value", taint) or "")
            if (content.startswith("secret://")
                    or kind_value == "secret_ref"
                    or taint_value == "secret_reference"):
                return True
        return False

    def _secure_prompt_candidate_ready(self, candidate: str, *, role: str) -> bool:
        """Capability/version preflight for a candidate, with no secret loaded."""
        from muteki.solver.cli_driver import driver_for
        try:
            profile = self._profile_for_engine(
                candidate, role=role, advance=False)
            driver = driver_for(profile or candidate)
            if not bool(getattr(driver, "secure_prompt_transport", False)):
                return False
            preflight = getattr(driver, "secure_prompt_preflight", None)
            if not callable(preflight):
                return False
            ok, _detail = preflight()
            return bool(ok)
        except Exception:
            return False

    def _pick_engine(
        self,
        running_engines: list[str],
        healthy: list[str],
        *,
        role: str = "bootstrap",
        intent_id: str = "",
        lane: str = "",
        intent: "Optional[dict]" = None,
        avoid_engines: "Optional[list[str]]" = None,
    ) -> str:
        """Heterogeneity-aware engine selection: prefer an engine NOT currently
        running, so each spawned worker covers a different blind spot. Falls back to
        least-loaded when all are running.

        When cognitive_cluster_planner is on, also bias by historical
        fact/barren productivity so complementary engines get complementary work.
        """
        available = self._healthy_role_candidates(healthy, role=role)
        secure_available: list[str] = []
        for candidate in available:
            profile = (
                self._profile_for_engine(
                    candidate, role=role, advance=False)
                if getattr(self, "worker_profiles", []) else None
            )
            transport = base_engine_for_profile(profile or candidate)
            if not self._context_requires_secure_prompt(
                    engine=transport, intent_id=intent_id, lane=lane):
                secure_available.append(candidate)
                continue
            if self._secure_prompt_candidate_ready(candidate, role=role):
                secure_available.append(candidate)
        available = secure_available
        if not available:
            raise RuntimeError(
                f"no available worker profile for role={role} and context capability")
        # Framework capability profiler (f01+): default Swarm has no hook → inert.
        effect_pick = getattr(self, "_effect_capability_pick_engine", None)
        if callable(effect_pick) and role in {"explore", "bootstrap", "review"}:
            try:
                chosen = effect_pick(
                    running_engines,
                    available,
                    role=role,
                    intent_id=intent_id,
                    lane=lane,
                    intent=intent,
                    avoid_engines=list(avoid_engines or ()),
                )
                if chosen and chosen in available:
                    return chosen
            except Exception:
                pass
        if getattr(self, "cognitive_cluster_planner", False) and role in {
            "explore", "bootstrap", "review"
        }:
            try:
                from muteki.swarm.cognitive_cluster_planner import (
                    ClusterEvidence,
                    select_engine,
                )

                evidence = ClusterEvidence.from_graph(self.shared_graph)
                return select_engine(
                    available=available,
                    running=running_engines,
                    evidence=evidence,
                    intent=intent or {},
                    avoid_engines=list(avoid_engines or ()),
                )
            except Exception:
                pass
        for e in available:
            if self._running_count_for_candidate(e, running_engines) == 0:
                return e
        # all healthy engines already running → least-loaded
        return min(available, key=lambda e: self._running_count_for_candidate(e, running_engines))

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

    def _verifier_profile_limit(self, profile: dict) -> int:
        raw = profile.get("max_verifier_running")
        if raw in (None, "", 0):
            return max(1, self._verifier_concurrency_cap())
        try:
            return max(0, int(raw))
        except (TypeError, ValueError):
            return max(1, self._verifier_concurrency_cap())

    def _profile_available(self, profile: dict, role: "Optional[str]" = None) -> bool:
        pid = profile["id"]
        if role == "review":
            return (
                self._active_review_profile_counts.get(pid, 0)
                < self._review_profile_limit(profile)
            )
        if role == "verifier":
            return (
                self._active_verifier_profile_counts.get(pid, 0)
                < self._verifier_profile_limit(profile)
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
        if role == "review":
            role_bucket = "review"
        elif role == "verifier":
            role_bucket = "verifier"
        else:
            role_bucket = "worker"
        self._active_profile_by_solver[solver_id] = pid
        self._active_profile_role_by_solver[solver_id] = role_bucket
        if role_bucket == "review":
            self._active_review_profile_counts[pid] = (
                self._active_review_profile_counts.get(pid, 0) + 1)
        elif role_bucket == "verifier":
            self._active_verifier_profile_counts[pid] = (
                self._active_verifier_profile_counts.get(pid, 0) + 1)
        else:
            self._active_profile_counts[pid] = self._active_profile_counts.get(pid, 0) + 1
        account_id = profile.get("credential_account")
        if not account_id:
            return
        self._active_account_by_solver[solver_id] = account_id

    def _register_control_worker(
        self,
        worker: Any,
        *,
        engine: str,
        intent_id: str = "",
        role: str = "worker",
    ) -> None:
        """Publish one live runtime object through the typed control registry.

        The object itself remains process-local so emergency controls can signal
        its subprocess.  Only a serializable WorkerRef is exposed to the actor.
        Registration happens before ``run()`` starts: a freeze racing worker
        startup therefore sets ``CliSolver._paused`` and its later ``_on_proc``
        freezes the process immediately instead of leaking a run window.
        """
        sid = str(getattr(worker, "solver_id", "") or "")
        if not sid:
            return
        self._live_solvers[sid] = worker
        registry = getattr(self, "worker_registry", None)
        register = getattr(registry, "register", None)
        if callable(register):
            try:
                from muteki.control import WorkerRef
                register(WorkerRef(
                    worker_id=sid,
                    engine=str(engine or getattr(worker, "engine", "") or ""),
                    intent_id=str(intent_id or getattr(worker, "intent_id", "") or ""),
                    lane=str(getattr(worker, "lane", "") or ""),
                    challenge_id=self.challenge.id,
                    status="frozen" if self._control_frozen else "running",
                    metadata={"role": role, "mode": str(getattr(worker, "mode", "") or "")},
                ))
            except Exception:
                # Registry telemetry must never block a solver from starting.
                pass
        if self._control_frozen:
            setter = getattr(worker, "_set_paused", None)
            if callable(setter):
                try:
                    setter(True)
                except Exception:
                    pass

    def _typed_context_for_worker(
        self, *, worker_id: str, engine: str, intent_id: str = "",
        lane: str = "",
    ) -> tuple[list[str], list[tuple[str, str]], str, list[dict[str, Any]]]:
        required_scope = (
            f"intent:{intent_id}"
            if str(intent_id or "").startswith("I-control-") else ""
        )
        provider = getattr(self, "_context_provider", None)
        reserver = getattr(self, "_context_reserver", None)
        releaser = getattr(self, "_context_releaser", None)
        if not callable(provider) or not callable(reserver):
            if required_scope:
                raise RequiredContextUnavailable(
                    "required operator continuation context provider is unavailable")
            return [], [], "", []
        guidance: list[str] = []
        reservations: list[tuple[str, str]] = []
        prompt_manifest: list[dict[str, Any]] = []
        required_reserved = False
        endpoint = ""
        try:
            resources = list(provider())
        except Exception:
            if required_scope:
                raise RequiredContextUnavailable(
                    "required operator continuation context is unavailable") from None
            return [], [], "", []
        if required_scope:
            try:
                all_resources = list(provider(active_only=False))
            except TypeError:
                all_resources = resources
            except Exception:
                raise RequiredContextUnavailable(
                    "required operator continuation context is unavailable") from None
            required_rows = [
                row for row in all_resources
                if str(getattr(row, "scope", "") or "") == required_scope
            ]
            active_ids = {
                str(getattr(row, "context_id", "") or "") for row in resources
            }
            if (not required_rows
                    or any(str(getattr(row, "context_id", "") or "") not in active_ids
                           for row in required_rows)):
                raise RequiredContextUnavailable(
                    "required operator continuation context is not deliverable")
        for resource in resources:
            reservation: tuple[str, str] | None = None
            is_required = bool(
                required_scope
                and str(getattr(resource, "scope", "") or "") == required_scope)
            try:
                scope = str(getattr(resource, "scope", "global") or "global")
                value = scope.split(":", 1)[1] if ":" in scope else scope
                applies = bool(
                    scope in ("global", self.challenge.id,
                              f"challenge:{self.challenge.id}")
                    or (scope.startswith("run:") and value == self.run_id)
                    or (scope.startswith(("solver:", "worker:"))
                        and value == worker_id)
                    or scope == worker_id
                    or (scope.startswith("engine:") and value == engine)
                    or (scope.startswith("intent:") and value == intent_id)
                    or (intent_id and scope == intent_id)
                    or (scope.startswith("lane:") and value == lane)
                    or (lane and scope == lane)
                )
                if not applies:
                    continue
                context_id = str(getattr(resource, "context_id", "") or "")
                if not context_id:
                    continue
                # Claim capacity before reading/materialising content.  The journal
                # transaction is the one-shot disclosure boundary, so two workers
                # built ahead of launch cannot preload the same max_bindings=1 secret.
                reservation_id = reserver(context_id, worker_id=worker_id)
                if not reservation_id:
                    if is_required:
                        raise RequiredContextUnavailable(
                            "required operator continuation context is already claimed")
                    continue
                reservation = (context_id, str(reservation_id))
                content = str(getattr(resource, "content", "") or "")
                if not content:
                    if not self._release_typed_context_reservations(
                            [reservation], worker_id):
                        raise ControlShutdownIncomplete(
                            "empty context reservation rollback unconfirmed")
                    reservation = None
                    if is_required:
                        if not self._release_typed_context_reservations(
                                reservations, worker_id):
                            raise ControlShutdownIncomplete(
                                "required context rollback unconfirmed")
                        reservations.clear()
                        raise RequiredContextUnavailable(
                            "required operator continuation context is empty")
                    continue
                is_secret_reference = content.startswith("secret://")
                resolved = self._materialize_reserved_control_text(content)
                kind = getattr(resource, "kind", "")
                kind_value = str(getattr(kind, "value", kind) or "")
                if kind_value == "endpoint":
                    endpoint = resolved
                guidance.append(resolved)
                reservations.append(reservation)
                # Keep the durable reservation attached to the exact plaintext
                # that must appear in the eventual worker prompt.  CliSolver
                # reconciles this manifest against the fully-rendered prompt before
                # Popen; merely adding a value to ``standing_guidance`` is not a
                # delivery receipt because its 4k budget can drop older values.
                prompt_manifest.append({
                    "context_id": reservation[0],
                    "reservation_id": reservation[1],
                    "text": resolved,
                    "required": is_required,
                    "kind": kind_value,
                    "secret": is_secret_reference,
                })
                if is_secret_reference:
                    self._reserved_context_secret_values[reservation] = resolved
                if is_required:
                    required_reserved = True
            except ControlShutdownIncomplete:
                raise
            except Exception as materialize_exc:
                # Never strand capacity when secret resolution/materialisation fails.
                if (reservation is not None
                        and not self._release_typed_context_reservations(
                            [reservation], worker_id)):
                    raise ControlShutdownIncomplete(
                        "materialisation reservation rollback unconfirmed"
                    ) from materialize_exc
                if is_required:
                    # The whole prompt is still pre-Popen. Roll back every earlier
                    # global/engine reservation too; otherwise a later exact secret
                    # failure strands unrelated one-shot capacity and restart falsely
                    # upgrades it to delivery_unknown.
                    if not self._release_typed_context_reservations(
                            reservations, worker_id):
                        raise ControlShutdownIncomplete(
                            "required context batch rollback unconfirmed"
                        ) from materialize_exc
                    reservations.clear()
                    raise RequiredContextUnavailable(
                        "required operator continuation context could not be materialised") from None
                continue
        if required_scope and not required_reserved:
            if not self._release_typed_context_reservations(
                    reservations, worker_id):
                raise ControlShutdownIncomplete(
                    "missing required context rollback unconfirmed")
            reservations.clear()
            raise RequiredContextUnavailable(
                "required operator continuation context was not reserved")
        return guidance, reservations, endpoint, prompt_manifest

    def _release_typed_context_reservations(
        self, reservations: list[tuple[str, str]], worker_id: str,
    ) -> bool:
        if not reservations:
            return True
        releaser = getattr(self, "_context_releaser", None)
        if not callable(releaser):
            return False
        released = True
        for context_id, reservation_id in reservations:
            try:
                if releaser(
                    context_id, worker_id=worker_id,
                    reservation_id=reservation_id) is not True:
                    released = False
            except Exception:
                released = False
        if released:
            for reservation in reservations:
                self._reserved_context_secret_values.pop(reservation, None)
        owners = getattr(self, "_context_cleanup_owners", None)
        if not isinstance(owners, dict):
            owners = {}
            self._context_cleanup_owners = owners
        owner_key = hashlib.sha256(json.dumps(
            [str(worker_id), sorted([list(row) for row in reservations])],
            separators=(",", ":"), sort_keys=True).encode("utf-8")).hexdigest()
        if released:
            owners.pop(owner_key, None)
        else:
            owners[owner_key] = (list(reservations), str(worker_id or ""))
            self._context_cleanup_incomplete = True
            self._mark_shutdown_incomplete("context_cleanup")
        self._context_cleanup_incomplete = bool(owners)
        if not owners:
            self._clear_shutdown_incomplete("context_cleanup")
        return released

    def _take_context_secret_values(
        self, reservations: list[tuple[str, str]],
    ) -> list[str]:
        values: list[str] = []
        for reservation in reservations:
            value = self._reserved_context_secret_values.pop(reservation, "")
            if value and value not in values:
                values.append(value)
        return values

    async def _run_control_worker(self, worker: Any) -> Any:
        """Run a worker; CliSolver commits reserved context from ``_on_proc``."""
        outcome = await worker.run()
        profile = getattr(getattr(worker, "driver", None), "profile", None)
        if isinstance(profile, dict):
            outcome.runtime_profile = {
                key: profile.get(key)
                for key in (
                    "id", "name", "label", "engine", "transport", "model",
                    "reasoning_effort", "credential_account", "credential_kind",
                    "credential_mode", "base_url", "wire_api",
                )
                if profile.get(key) not in (None, "")
            }
        return outcome

    async def _schedule_control_worker(
        self, worker: Any, *, name: str, intent_id: str = "",
        lane_key: str = "",
    ) -> "asyncio.Task[Any]":
        """Create one worker task as an acquisition transaction.

        ``create_task`` itself can raise (loop shutdown, injected failure, resource
        pressure). Close the un-awaited coroutine and retire every already-acquired
        context/account/intent/lane before propagating the scheduling failure.
        """
        protocol2 = getattr(self, "protocol2_session", None)
        if protocol2 is not None:
            try:
                return protocol2.schedule_worker(
                    worker, lambda: self._run_control_worker(worker), name=name)
            except BaseException as exc:
                retired = await self._retire_worker_account(
                    worker,
                    intent_id=str(
                        intent_id
                        or getattr(worker, "intent_id_assigned", "")
                        or getattr(worker, "_intent_id", "") or ""),
                    reason="Protocol 2 worker admission/scheduling failed",
                    lane_key=str(lane_key or ""),
                )
                if not retired:
                    raise ControlShutdownIncomplete(
                        "Protocol 2 scheduling failed with retained acquisition owner"
                    ) from exc
                raise
        coroutine = self._run_control_worker(worker)
        try:
            return asyncio.create_task(coroutine, name=name)
        except BaseException as exc:
            close = getattr(coroutine, "close", None)
            if callable(close):
                close()
            retired = await self._retire_worker_account(
                worker,
                intent_id=str(
                    intent_id
                    or getattr(worker, "intent_id_assigned", "")
                    or getattr(worker, "_intent_id", "") or ""),
                reason="worker task scheduling failed",
                lane_key=str(lane_key or ""),
            )
            if not retired:
                raise ControlShutdownIncomplete(
                    "worker scheduling failed with retained acquisition owner"
                ) from exc
            raise

    def _update_control_worker_status(self, worker_id: str, status: str) -> None:
        registry = getattr(self, "worker_registry", None)
        update = getattr(registry, "update", None)
        if callable(update):
            try:
                update(worker_id, status=status)
            except Exception:
                pass

    def _release_worker_account(self, solver: Any) -> bool:
        """Release a worker whose runtime never started or is proven exited.

        Callers that have scheduled ``solver.run()`` must use
        :meth:`_retire_worker_account`; an asyncio wrapper being done is not proof
        that its shielded thread/process/container has exited.
        """
        sid = getattr(solver, "solver_id", "")
        pending = list(getattr(
            solver, "_pending_control_context_reservations", []) or [])
        if pending:
            if not self._release_typed_context_reservations(
                    pending, str(sid or "")):
                return False
            solver._pending_control_context_reservations = []
        self._live_solvers.pop(sid, None)
        registry = getattr(self, "worker_registry", None)
        unregister = getattr(registry, "unregister", None)
        if callable(unregister) and sid:
            try:
                unregister(sid)
            except Exception:
                pass
        pid = self._active_profile_by_solver.pop(sid, None)
        role_bucket = self._active_profile_role_by_solver.pop(sid, "worker")
        if pid:
            if role_bucket == "review":
                self._active_review_profile_counts[pid] = max(
                    0, self._active_review_profile_counts.get(pid, 0) - 1)
            elif role_bucket == "verifier":
                self._active_verifier_profile_counts[pid] = max(
                    0, self._active_verifier_profile_counts.get(pid, 0) - 1)
            else:
                self._active_profile_counts[pid] = max(
                    0, self._active_profile_counts.get(pid, 0) - 1)
        self._active_account_by_solver.pop(sid, None)
        return True

    @staticmethod
    def _worker_runtime_exit_confirmed(solver: Any) -> bool:
        query = getattr(solver, "runtime_exit_confirmed", None)
        if not callable(query):
            # Non-CLI/test solvers have no independently-owned runtime boundary.
            return True
        try:
            return bool(query())
        except Exception:
            return False

    def _finish_worker_retirement(
        self, solver: Any, *, intent_id: str = "", reason: str = "",
        lane_key: str = "",
    ) -> bool:
        """Atomically retire durable claims, then release the worker account.

        Runtime exit alone is insufficient: if the owner-fenced intent/lane write
        fails, dropping account/context ownership would let an expired claim replay
        work that may already have executed.  False keeps the retained owner alive
        so the autonomous reaper can retry the durable transition.
        """
        if not self._worker_runtime_exit_confirmed(solver):
            raise RuntimeError("worker retirement requires runtime exit proof")
        if bool(getattr(solver, "_muteki_account_retired", False)):
            return True
        retired_refs = getattr(self, "_retired_worker_refs", None)
        if not isinstance(retired_refs, list):
            retired_refs = []
            self._retired_worker_refs = retired_refs
        if any(item is solver for item in retired_refs):
            return True
        sid = str(getattr(solver, "solver_id", "") or "")
        claimed_intent = bool(str(intent_id or ""))
        process_started = bool(
            getattr(solver, "_runtime_process_started", True))
        crossed_boundary = bool(
            getattr(solver, "_control_context_delivery_committed", False)
            or getattr(solver, "_control_context_delivery_unknown", False)
        )
        prestart = not process_started and not crossed_boundary
        # Stage 1: release pre-delivery context claims durably.  Do not clear the
        # solver's immutable reservation list or redaction ownership until every
        # idempotent release confirms its postcondition.
        if not bool(getattr(solver, "_muteki_contexts_released", False)):
            pending = list(getattr(
                solver, "_pending_control_context_reservations", []) or [])
            if pending:
                if prestart:
                    if not self._release_typed_context_reservations(pending, sid):
                        return False
                else:
                    marker = getattr(
                        solver, "_context_delivery_unknown_marker", None)
                    if not callable(marker):
                        marker = getattr(
                            self, "_context_delivery_unknown_marker", None)
                    if not callable(marker):
                        return False
                    for context_id, reservation_id in pending:
                        try:
                            marked = marker(
                                context_id, worker_id=sid,
                                reservation_id=reservation_id,
                                actor="runtime-retirement",
                                reason=("process existed without a confirmed "
                                        "context delivery receipt"),
                            )
                        except Exception:
                            return False
                        if marked is not True:
                            return False
                    solver._control_context_delivery_unknown = True
            solver._pending_control_context_reservations = []
            setattr(solver, "_muteki_contexts_released", True)

        # Stage 2: retain an exclusive lane until the old runtime is absent.  The
        # stage checkpoint prevents repeated release events while a later stage is
        # retrying.
        if (lane_key and self.shared_graph is not None
                and not bool(getattr(solver, "_muteki_lane_released", False))):
            lane_release: dict[str, Any] = {}
            try:
                lane_release = self.shared_graph.release_lane(
                    actor="coordinator", lane_key=str(lane_key), by_worker=sid)
            except Exception:
                lane_release = {}
            lane_ok = bool(lane_release.get("released"))
            active_reader = getattr(self.shared_graph, "active_lanes", None)
            if callable(active_reader):
                try:
                    active = next((
                        row for row in active_reader()
                        if str(row.get("lane_key") or "") == str(lane_key)
                    ), None)
                    lane_ok = active is None or str(
                        active.get("owner_worker") or "") != sid
                except Exception:
                    # Keep the direct release receipt when the verification read
                    # itself is temporarily unavailable.
                    pass
            if not lane_ok:
                return False
            setattr(solver, "_muteki_lane_release_result", lane_release)
            setattr(solver, "_muteki_lane_released", True)

        # Stage 3 (last durable edge): only now may a never-started intent reopen.
        # A started/uncertain intent is terminalized through an idempotent,
        # owner-fenced graph operation. Verification-read failure is fail-closed;
        # a positive event sequence alone is never accepted as the postcondition.
        if (claimed_intent and self.shared_graph is not None
                and not bool(getattr(solver, "_muteki_intent_transitioned", False))):
            state_reader = getattr(self.shared_graph, "intent_claim_state", None)

            def _desired(state: dict[str, str]) -> bool:
                if not state:
                    return True
                status = str(state.get("status") or "")
                owner = str(state.get("worker") or "")
                dispatch = str(state.get("dispatch_state") or "")
                if prestart:
                    return (
                        (status == "open" and not owner)
                        or status == "done"
                        or (status == "claimed" and owner and owner != sid)
                    )
                return status == "done" or dispatch in {"closed", "retired"}

            before: Optional[dict[str, str]] = None
            if callable(state_reader):
                try:
                    before = dict(state_reader(str(intent_id)) or {})
                except Exception:
                    return False
                if _desired(before):
                    setattr(solver, "_muteki_intent_transitioned", True)
            if not bool(getattr(solver, "_muteki_intent_transitioned", False)):
                try:
                    if prestart:
                        transition_result = self.shared_graph.release_intent_claim(
                            worker=sid, intent_id=str(intent_id),
                            reason=str(
                                reason or "worker failed before process start"),
                        )
                    else:
                        terminalizer = getattr(
                            self.shared_graph, "terminalize_intent_claim", None)
                        if callable(terminalizer):
                            transition_result = terminalizer(
                                worker=sid, intent_id=str(intent_id), reason=reason)
                        else:
                            transition_result = self.shared_graph.conclude_intent(
                                actor=sid, intent_id=str(intent_id),
                                result="cancelled", result_detail=str(reason)[:500])
                except Exception:
                    return False
                if callable(state_reader):
                    try:
                        after = dict(state_reader(str(intent_id)) or {})
                    except Exception:
                        return False
                    transition_ok = _desired(after)
                else:
                    transition_ok = bool(transition_result)
                if not transition_ok:
                    return False
                setattr(solver, "_muteki_intent_transitioned", True)

        if not self._release_worker_account(solver):
            return False
        try:
            setattr(solver, "_muteki_account_retired", True)
        except Exception:
            # Keep the object itself alive as the identity fence; raw id() values
            # can be reused by CPython after collection.
            retired_refs.append(solver)
        owners = getattr(self, "_worker_runtime_owners", {})
        reapers = getattr(self, "_worker_runtime_reapers", {})
        owners.pop(sid, None)
        reapers.pop(sid, None)
        self._worker_runtime_incomplete = bool(owners)
        if not owners:
            self._clear_shutdown_incomplete("worker_runtime")
        return True

    def _shutdown_owners_incomplete(self) -> bool:
        return bool(
            self._control_shutdown_incomplete
            or getattr(self, "_shutdown_incomplete_causes", set())
            or getattr(self, "_worker_runtime_incomplete", False)
            or getattr(self, "_context_cleanup_incomplete", False)
        )

    def _mark_shutdown_incomplete(self, cause: str) -> None:
        causes = getattr(self, "_shutdown_incomplete_causes", None)
        if not isinstance(causes, set):
            causes = set()
            self._shutdown_incomplete_causes = causes
        causes.add(str(cause or "unknown"))
        self._control_shutdown_incomplete = True

    def _clear_shutdown_incomplete(self, cause: str) -> None:
        causes = getattr(self, "_shutdown_incomplete_causes", None)
        if isinstance(causes, set):
            causes.discard(str(cause or "unknown"))
        self._control_shutdown_incomplete = bool(causes)

    def _ensure_worker_runtime_reaper(
        self, solver: Any, *, intent_id: str = "", reason: str = "",
        lane_key: str = "",
    ) -> "asyncio.Task[None]":
        sid = str(getattr(solver, "solver_id", "") or id(solver))
        owners = getattr(self, "_worker_runtime_owners", None)
        if not isinstance(owners, dict):
            owners = {}
            self._worker_runtime_owners = owners
        reapers = getattr(self, "_worker_runtime_reapers", None)
        if not isinstance(reapers, dict):
            reapers = {}
            self._worker_runtime_reapers = reapers
        owners[sid] = (
            solver, str(intent_id or ""), str(reason or ""), str(lane_key or ""))
        # Ownership exists before the reaper task does. ``create_task`` can itself
        # fail (for example while the loop is closing), and finalization must still
        # see the retained live-runtime owner and let settlement rebuild the reaper.
        self._worker_runtime_incomplete = True
        self._mark_shutdown_incomplete("worker_runtime")
        task = reapers.get(sid)
        if task is not None and not task.done():
            return task

        async def _reap() -> None:
            wait_runtime = getattr(solver, "wait_runtime_exit", None)
            while True:
                while not self._worker_runtime_exit_confirmed(solver):
                    self._cancel_solver(solver)
                    if callable(wait_runtime):
                        try:
                            waited = wait_runtime(None)
                            if inspect.isawaitable(waited):
                                await waited
                        except asyncio.CancelledError:
                            # Cancellation never proves exit. Keep the owner mapping
                            # so settle_control_shutdown can rebuild a fresh reaper.
                            raise
                        except Exception:
                            await asyncio.sleep(0.05)
                    else:
                        await asyncio.sleep(0.05)
                if self._finish_worker_retirement(
                        solver, intent_id=intent_id, reason=reason,
                        lane_key=lane_key):
                    return
                await asyncio.sleep(0.05)

        reaper_coro = _reap()
        try:
            task = asyncio.create_task(
                reaper_coro, name=f"worker-runtime-retire-{sid}")
        except BaseException:
            # Creating a Task is not guaranteed during loop teardown.  Closing the
            # unowned coroutine avoids a warning; the owner/cause registered above
            # deliberately remain so a later settlement epoch can rebuild it.
            reaper_coro.close()
            raise
        reapers[sid] = task
        return task

    def _retain_worker_retirement_owner(
        self, solver: Any, *, intent_id: str = "", reason: str = "",
        lane_key: str = "",
    ) -> None:
        """Register a never-started synchronous rollback for async settlement."""
        sid = str(getattr(solver, "solver_id", "") or id(solver))
        owners = getattr(self, "_worker_runtime_owners", None)
        if not isinstance(owners, dict):
            owners = {}
            self._worker_runtime_owners = owners
        owners[sid] = (
            solver, str(intent_id or ""), str(reason or ""), str(lane_key or ""))
        self._worker_runtime_incomplete = True
        self._mark_shutdown_incomplete("worker_runtime")

    async def _retire_worker_account(
        self, solver: Any, *, intent_id: str = "", reason: str = "",
        lane_key: str = "",
        timeout: "Optional[float]" = None,
    ) -> bool:
        """Retire a scheduled worker only after its real runtime exits.

        A bounded miss transfers ownership to an autonomous reaper and marks the
        whole run shutdown-incomplete.  The live registry, credential account and
        one-shot context reservation stay owned until that reaper obtains proof.

        ``timeout`` overrides the solver's cancel-cleanup budget (used by
        fruitless-interrupt hard-cap reap, which needs longer than the default
        2s CLI cleanup window).
        """
        if solver is None:
            return True
        if self._worker_runtime_exit_confirmed(solver):
            if self._finish_worker_retirement(
                    solver, intent_id=intent_id, reason=reason,
                    lane_key=lane_key):
                return True

        # Transfer ownership to one autonomous reaper *before* any await.  A second
        # cancellation of this cleanup coroutine can then never erase the only live
        # process-control boundary.
        if timeout is None:
            timeout_getter = getattr(solver, "_thread_cancel_cleanup_timeout", None)
            try:
                timeout = float(timeout_getter()) if callable(timeout_getter) else 2.0
            except (TypeError, ValueError):
                timeout = 2.0
        else:
            try:
                timeout = float(timeout)
            except (TypeError, ValueError):
                timeout = 2.0
        existing = self._ensure_worker_runtime_reaper(
            solver, intent_id=intent_id, reason=reason, lane_key=lane_key)
        try:
            await asyncio.wait_for(
                asyncio.shield(existing), timeout=max(0.01, timeout))
        except asyncio.TimeoutError:
            self._mark_shutdown_incomplete("worker_runtime")
            return False
        except asyncio.CancelledError:
            self._mark_shutdown_incomplete("worker_runtime")
            return False
        except Exception:
            self._mark_shutdown_incomplete("worker_runtime")
            return False
        owners = getattr(self, "_worker_runtime_owners", {})
        self._worker_runtime_incomplete = bool(owners)
        return bool(getattr(solver, "_muteki_account_retired", False))

    def _gen_suffix(self) -> str:
        """Worker-id / cwd suffix for continued runs: ``-g2`` on the second
        execution generation (web resolve), empty on a fresh start. Keeps a new
        generation's workers from colliding with the previous generation's UI
        lanes, intent ids (intent:<solver_id>), and workdirs."""
        g = int(getattr(self, "_execution_generation", 1) or 1)
        return f"-g{g}" if g > 1 else ""

    def _alloc_workdir(self, engine: str) -> "Optional[str]":
        """Carve a fresh per-worker cwd under worker_root, or return None to let
        CliSolver fall back to a system mkdtemp. The monotonic _worker_seq keeps
        two same-engine workers (race + a later explore) from colliding; the
        generation suffix keeps a continued run's workers out of the previous
        generation's directories (resolve reuses the same worker_root)."""
        if self.worker_root is None:
            return None
        if self.workspace_root is not None:
            ensure_workspace(self.workspace_root)
        self._worker_seq += 1
        wd = self.worker_root / f"cli-{engine}-{self._worker_seq}{self._gen_suffix()}"
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
        agent_state_dir: Path | None = None
        agent_state_container_path: str | None = None
        if engine in {"pi", "omp", "opencode", "dsh"}:
            if self.workspace_root is not None:
                state_root = self.workspace_root / ".muteki-agent-state"
                agent_state_dir = state_root / label
                self._agent_state_dirs.add(state_root)
            else:
                agent_state_dir = Path(tempfile.mkdtemp(
                    prefix=f"muteki-{engine}-{label}-"))
                self._agent_state_dirs.add(agent_state_dir)
            if container is not None:
                mapper = getattr(container, "to_container_path", None)
                if callable(mapper):
                    agent_state_container_path = mapper(str(agent_state_dir))
        env = runtime_env_for_engine(
            engine,
            account_root=self.credential_accounts_root,
            account_id=(profile.get("credential_account") if profile else None),
            container=container is not None,
            agent_state_dir=agent_state_dir,
            agent_state_container_path=agent_state_container_path,
            model=str((profile or {}).get("model") or ""),
        ).env
        if agent_state_dir is not None and container is not None:
            from muteki.solver.container_exec import _chown_tree_to_worker
            _chown_tree_to_worker(str(agent_state_dir))
        if profile:
            apply_worker_identity_env(env, profile)
            env["MUTEKI_WORKER_REASONING_EFFORT"] = normalize_reasoning_effort(
                profile.get("reasoning_effort"), "default")
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
        if self._container_unavailable and self.worker_backend == "container":
            self._fail_if_container_required(engine)
            return "local"
        return "container" if self.worker_backend == "container" else "local"

    def _fail_if_container_required(self, engine: str) -> None:
        """When a container backend is resolved but unavailable, refuse the
        silent local fallback EVERYWHERE unless the operator explicitly opted
        in. P2-v3 hard-failed only inside the web container; round-10 showed
        the bare-host fallback is exactly as violating: "container"-labelled
        runs executed every worker host-native (host credentials, host
        filesystem, host docker socket) while run.finished reported
        backend "container". MUTEKI_ALLOW_CONTAINER_LOCAL_FALLBACK=1 preserves
        the legacy behavior for operators who ask for it by name."""
        if (os.environ.get("MUTEKI_ALLOW_CONTAINER_LOCAL_FALLBACK") or ""
                ).strip().lower() in {"1", "true", "yes", "on"}:
            return
        if is_web_container():
            raise RuntimeError(
                f"container worker backend is unavailable for {engine!r} and this "
                f"coordinator runs inside the web container — refusing to fall back "
                f"to a host-native (local) worker (it would run with no tools / wrong "
                f"credentials). Fix the container backend: check the docker socket "
                f"mount, the worker image is pulled, and MUTEKI_CONTROL_BIND/"
                f"MUTEKI_CONTROL_HOST + the compose network are set."
            )
        raise RuntimeError(
            f"container worker backend is unavailable for {engine!r} — refusing "
            f"to silently run a host-native (local) worker (it would execute "
            f"with host credentials, host filesystem and host docker access "
            f"while the run reports backend 'container'). Fix the container "
            f"backend (docker socket reachable, worker image pulled, control "
            f"receiver up) or explicitly opt into the legacy local fallback "
            f"with MUTEKI_ALLOW_CONTAINER_LOCAL_FALLBACK=1."
        )

    def _schedule_engine_health_event(
        self,
        payload: dict[str, Any],
        *,
        owner_loop: asyncio.AbstractEventLoop | None,
    ) -> None:
        try:
            loop = asyncio.get_running_loop()
        except RuntimeError:
            if owner_loop is None:
                return

            def create_event_task() -> None:
                # Construct the coroutine only once this callback is running on the
                # owner loop's thread.
                owner_loop.create_task(self._emit_engine_degraded(payload))

            try:
                # The health roster may run under ``asyncio.to_thread``. Schedule
                # task creation back to its owning loop.
                owner_loop.call_soon_threadsafe(create_event_task)
            except RuntimeError:
                pass
        else:
            loop.create_task(self._emit_engine_degraded(payload))

    def _note_engine_degraded(
        self,
        engine: str,
        reason: str,
        *,
        role: str,
        owner_loop: asyncio.AbstractEventLoop | None = None,
    ) -> None:
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
        self._schedule_engine_health_event(payload, owner_loop=owner_loop)

    def _note_engine_recovered(
        self,
        engine: str,
        *,
        owner_loop: asyncio.AbstractEventLoop | None = None,
    ) -> None:
        """A previously-degraded engine passed its health check again — clear the
        dedup latch and tell the operator it's back so the warning state lifts."""
        if engine not in self._degraded_engines:
            return
        self._degraded_engines.pop(engine, None)
        payload = {"engine": engine, "status": "recovered", "reason": ""}
        self._schedule_engine_health_event(payload, owner_loop=owner_loop)

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
        payload = {
            "engine": engine,
            "profile": (profile or {}).get("name") or (profile or {}).get("id") or "",
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
        return {
            "backend": "local" if self._runtime_degraded else self.worker_backend,
            "network": self.worker_network if self.worker_backend == "container" else "",
            "runtime_degraded": list(self._runtime_degraded),
        }

    def _container_for_engine(self, engine: str, profile: "Optional[dict]" = None) -> "Optional[object]":
        """The run's container ContainerHandle when worker_backend=="container",
        else None (local host backend). Created lazily — worker_root must exist so
        it can be bind-mounted as the shared workspace (only the run workspace +
        control socket + account projection are mounted). Container mode surfaces
        setup failures instead of silently falling back to host execution."""
        if self._backend_for_engine(engine, profile) != "container":
            return None
        if self._container_handle is not None:
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
                network=self.worker_network,
                account_root=(str(self.credential_accounts_root)
                              if self.credential_accounts_root is not None else None),
            )
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
                         profile_role: "Optional[str]" = None,
                         lane: str = ""):
        from muteki.solver.cli_solver import CliSolver

        # Resolve the profile FIRST — BEFORE charging the spawn budget. A missing
        # profile is a recoverable rejection (WorkerSpawnRejected), not a budget
        # event: charging _spawned_total here and then bailing would leak a phantom
        # spawn toward max_total_workers (and a bare RuntimeError would crash the
        # coordinator loop, since spawn sites only catch WorkerBudgetExhausted).
        role = profile_role or (
            "review" if mode == "review" else
            "verifier" if mode == "verifier" else
            "explore" if mode == "explore" else "bootstrap")
        profile = self._profile_for_engine(engine, role=role)
        if self.worker_profiles and profile is None:
            raise WorkerSpawnRejected(
                f"no available worker profile for {engine} role={role}")
        if role == "review" and profile is not None:
            review_effort = str(
                self.review_policy.get("reasoning_effort") or "inherit"
            ).strip().lower()
            if review_effort != "inherit":
                profile = {
                    **profile,
                    "reasoning_effort": normalize_reasoning_effort(
                        review_effort, "default"),
                }
        if role == "verifier" and profile is not None:
            verifier_effort = str(
                self.verifier_policy.get("reasoning_effort") or "inherit"
            ).strip().lower()
            if verifier_effort != "inherit":
                profile = {
                    **profile,
                    "reasoning_effort": normalize_reasoning_effort(
                        verifier_effort, "default"),
                }
        transport = base_engine_for_profile(profile or engine)
        if self._context_requires_secure_prompt(
                engine=transport, intent_id=intent_id, lane=lane):
            from muteki.solver.cli_driver import driver_for
            driver = driver_for(profile or transport)
            if not bool(getattr(driver, "secure_prompt_transport", False)):
                raise WorkerSpawnRejected(
                    f"worker profile {engine} cannot securely deliver secret context")
            preflight = getattr(driver, "secure_prompt_preflight", None)
            try:
                supported, detail = preflight() if callable(preflight) else (
                    False, "secure prompt preflight unavailable")
            except Exception as exc:
                supported, detail = False, str(exc)
            if not supported:
                raise WorkerSpawnRejected(
                    f"worker profile {engine} secure prompt preflight failed: "
                    f"{str(detail or 'unsupported')[:240]}")

        # Reject an exhausted run before allocating anything, but do not COMMIT
        # the lifetime spawn budget until construction, exact-context acquisition,
        # account claiming and control registration all succeed.  Required context
        # may be temporarily unavailable for many coordinator passes; those are
        # zero-Popen attempts and must never consume max_total_workers.
        budget_kind = self._budget_exhausted()
        if budget_kind:
            self._budget_exhausted_kind = budget_kind
            raise WorkerBudgetExhausted(budget_kind)

        # UNIQUE label per spawn so the deck draws one lane per worker. Every
        # claude worker would otherwise be "cli-claude" and collapse onto a single
        # lane — you couldn't tell parallel / re-bootstrapped workers apart. We keep
        # the "cli-<engine>" prefix (the deck's workerEngine() badge keys off it)
        # and append a monotonic index. The first worker of an engine keeps the bare
        # "cli-<engine>" id for back-compat (winner bookkeeping, existing tests).
        self._label_seq[transport] = self._label_seq.get(transport, 0) + 1
        n = self._label_seq[transport]
        label = f"cli-{transport}" if n == 1 else f"cli-{transport}-{n}"
        label += self._gen_suffix()

        # explore = narrow single-intent probe; bootstrap = whole-challenge rush.
        # Both get the SHORT per-turn timeout (explore_timeout, default 720s) so a
        # stuck worker frees its max_workers slot quickly — this is the only backstop
        # now that the stall-kill is gone. A timed-out worker still gets one conclude
        # turn (min(timeout, 600s)) to summarize before dying.
        kw = {"timeout": self.explore_timeout} if mode in ("explore", "bootstrap") else {}
        if mode == "verifier":
            kw["timeout"] = int(self.verifier_policy.get("timeout") or 240)
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
        pending_next_guidance = list(self._next_worker_guidance)
        raw_guidance = list(self._standing_guidance) + pending_next_guidance
        # Framework worker shell injection (f01 declarations etc.). Default: no-op.
        fw_guide = getattr(self, "framework_worker_guidance_for_intent", None)
        if callable(fw_guide) and intent_id:
            try:
                extra = fw_guide(str(intent_id))
                if extra:
                    raw_guidance = list(raw_guidance) + list(extra)
            except Exception:
                pass
        # secret:// values have a typed ContextResource twin and may be
        # materialised only after its atomic reservation.  Legacy queues/directives
        # retain opaque refs for audit but never inject them into a prompt directly.
        guidance_for_worker = [
            self._resolve_control_text(item)
            for item in raw_guidance
            if not str(item or "").startswith("secret://")
        ]
        consumed_directive_ids: list[str] = []
        consumed_context_reservations: list[tuple[str, str]] = []

        def _scope_applies(scope: str) -> bool:
            scoped_value = scope.split(":", 1)[1] if ":" in scope else scope
            return bool(
                scope in ("global", self.challenge.id,
                          f"challenge:{self.challenge.id}")
                or (scope.startswith("run:") and scoped_value == self.run_id)
                or (scope.startswith(("solver:", "worker:"))
                    and scoped_value == label)
                or scope == label
                or (scope.startswith("engine:") and scoped_value == transport)
                or (scope.startswith("intent:") and scoped_value == intent_id)
                or (intent_id and scope == intent_id)
                or (scope.startswith("lane:") and scoped_value == lane)
                or (lane and scope == lane)
            )

        (typed_guidance, consumed_context_reservations, typed_endpoint,
         typed_prompt_manifest) = (
            self._typed_context_for_worker(
                worker_id=label, engine=transport, intent_id=intent_id,
                lane=lane))
        for injected in typed_guidance:
            if injected not in guidance_for_worker:
                guidance_for_worker.append(injected)
        # Fold only directives whose scope matches THIS worker.  Non-standing rows are
        # durable one-shot queue entries and transition to acted after construction;
        # standing rows remain active until clear_standing expires them.
        if self.shared_graph is not None:
            try:
                for directive in self.shared_graph.operator_directives(active_only=True):
                    scope = str(directive.get("scope") or "global")
                    if not _scope_applies(scope):
                        continue
                    dtext = str(directive.get("text") or "")
                    if not dtext or dtext.startswith("secret://"):
                        continue
                    injected_text = self._resolve_control_text(dtext)
                    tagged = f"[operator directive] {injected_text}"
                    if (injected_text not in guidance_for_worker
                            and tagged not in guidance_for_worker):
                        guidance_for_worker.append(tagged)
                    if not directive.get("standing"):
                        consumed_directive_ids.append(str(directive["directive_id"]))
            except Exception:
                pass
        fallback_redirect = str(self._target_redirect or "")
        if fallback_redirect.startswith("secret://"):
            fallback_redirect = ""
        effective_redirect = typed_endpoint or self._resolve_control_text(
            fallback_redirect)
        if effective_redirect:
            kw["hitl_cmd"] = {"action": "redirect", "url": effective_redirect}

        try:
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
                worker_env=self._runtime_env_for(
                    transport, label, container=container, profile=profile),
                identity=worker_identity_fields(profile),
            )
        except Exception as exc:
            if not self._release_typed_context_reservations(
                    consumed_context_reservations, label):
                raise ControlShutdownIncomplete(
                    "worker construction context rollback unconfirmed") from exc
            raise
        worker.engine = transport
        worker.lane = str(lane or "")
        if profile_role == "race" and mode == "bootstrap":
            worker._skip_bootstrap_conclude = True
        # Construction is the legacy single-shot delivery boundary: the prompt now
        # contains each matching one-shot directive.  Close those rows so future
        # workers cannot inherit them again.  Status is an auditable delivery receipt,
        # not a claim that the model obeyed the instruction.
        if self.shared_graph is not None:
            for directive_id in consumed_directive_ids:
                try:
                    self.shared_graph.update_directive_status(
                        directive_id=directive_id, status="acted",
                        bound_worker=worker.solver_id)
                except Exception:
                    pass
        worker._pending_control_context_reservations = list(
            consumed_context_reservations)
        worker._control_context_prompt_manifest = list(typed_prompt_manifest)
        worker._control_context_prompt_manifest_finalized = False
        worker._control_secret_values = self._take_context_secret_values(
            consumed_context_reservations)
        worker._context_committer = getattr(self, "_context_committer", None)
        worker._context_releaser = getattr(self, "_context_releaser", None)
        worker._context_delivery_unknown_marker = getattr(
            self, "_context_delivery_unknown_marker", None)
        spawn_budget_committed = False
        try:
            self._reserve_worker_spawn()
            spawn_budget_committed = True
            self._claim_worker_account(
                worker.solver_id, transport, profile, role=role)
            self._register_control_worker(
                worker, engine=transport, intent_id=intent_id, role=role,
            )
        except Exception as exc:
            if spawn_budget_committed:
                self._spawned_total = max(0, self._spawned_total - 1)
            if not self._release_worker_account(worker):
                self._retain_worker_retirement_owner(
                    worker, intent_id=intent_id,
                    reason="worker registration rollback")
                raise ControlShutdownIncomplete(
                    "worker registration rollback unconfirmed") from exc
            raise
        # One-shot guidance crosses its delivery boundary only with a successfully
        # registered, budgeted worker.  A pre-Popen acquisition failure leaves it
        # available for the next real spawn.
        if pending_next_guidance:
            del self._next_worker_guidance[:len(pending_next_guidance)]
        if profile_role == "race" and mode == "bootstrap":
            worker._skip_bootstrap_conclude = True
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
