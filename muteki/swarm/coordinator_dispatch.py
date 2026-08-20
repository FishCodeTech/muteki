"""Worker-command apply, open-intent queue, capacity, and the Reason phase.

Split out of ``swarm.py`` (code-health G1) as a mixin of ``Swarm``. Every method
body is byte-for-byte the original; the mixin is composed back into ``Swarm`` so
behavior and the public surface are unchanged. Instance state built in
``Swarm.__init__`` is resolved through the composed class at runtime.
"""

# ruff: noqa: F401
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
    worker_identity_event_fields,
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


class _DispatchReasonMixin:
    async def _apply_worker_cmds(
        self,
        *,
        tasks: dict,
        task_solvers: dict,
        healthy: list[str],
        running_engines_fn,
        emit_bb,
    ) -> None:
        """Drain operator spawn/kill worker commands onto the LIVE coordinator
        state (BE-worker-management runtime control). Mutates tasks/task_solvers
        in place. A spawn adds a fresh bootstrap worker for the requested engine
        (capped at max_workers; engine must be in the roster or currently healthy);
        a kill cancels the worker whose solver_id matches (it's reaped next loop)."""
        if self.worker_cmds is None:
            return

        def _finish_queue_item(cmd: dict) -> None:
            if cmd.get("_queue_item_finished"):
                return
            cmd["_queue_item_finished"] = True
            self.worker_cmds.task_done()

        def _ack(
            cmd: dict,
            *,
            state: str,
            detail: str,
            target_ids: Optional[list[str]] = None,
            code: str = "",
            **metadata: Any,
        ) -> None:
            future = cmd.get("_control_ack")
            if future is not None and not future.done():
                future.set_result(
                    {
                        "state": state,
                        "detail": detail,
                        "target_ids": list(target_ids or []),
                        "metadata": {"code": code, **metadata},
                    }
                )
            _finish_queue_item(cmd)

        async def _report(kind: str, **fields: Any) -> None:
            try:
                await emit_bb(kind, **fields)
            except Exception:
                pass

        while not self.worker_cmds.empty():
            try:
                cmd = self.worker_cmds.get_nowait()
            except asyncio.QueueEmpty:
                break
            if not isinstance(cmd, dict):
                self.worker_cmds.task_done()
                continue
            # Claim is published before the first await/effect. A timed-out parent
            # can atomically remove an unclaimed envelope; once this flips true it
            # must wait for our terminal ACK instead of returning UNKNOWN ahead of
            # a late spawn.
            cmd["_control_started"] = True
            if cmd.get("_control_cancel_requested"):
                _ack(
                    cmd,
                    state="unknown",
                    detail="worker command retired before execution",
                    code="worker_command_cancelled_before_effect",
                )
                continue
            action = cmd.get("action")
            if action == "spawn":
                if not self._ordinary_capacity_available(tasks):
                    _ack(
                        cmd,
                        state="failed",
                        detail="worker capacity exhausted",
                        code="max_workers",
                    )
                    await _report("worker_spawn_rejected", reason="max_workers")
                    continue
                try:
                    requested = cmd.get("engine")
                    if requested and self.worker_profiles:
                        matches = [
                            e
                            for e in normalize_profile_roster(
                                [requested], self.worker_profiles
                            )
                            if e in self.engines and self._healthy_matches(e, healthy)
                        ]
                        if not matches:
                            _ack(
                                cmd,
                                state="failed",
                                detail="requested worker profile is unavailable",
                                code="unavailable_profile",
                            )
                            await _report(
                                "worker_spawn_rejected",
                                reason="unavailable_profile",
                                engine=str(requested),
                            )
                            continue
                        engine = matches[0]
                    else:
                        engine = requested or self._pick_engine(
                            running_engines_fn(), healthy, role="bootstrap"
                        )
                except RuntimeError as exc:
                    _ack(
                        cmd,
                        state="failed",
                        detail="worker selection failed",
                        code="worker_selection_failed",
                    )
                    await _report("worker_spawn_rejected", reason=str(exc))
                    continue
                if self.worker_profiles:
                    unknown = engine not in self.engines or not self._healthy_matches(
                        str(engine), healthy
                    )
                else:
                    unknown = engine not in self.engines and engine not in healthy
                if unknown:
                    _ack(
                        cmd,
                        state="failed",
                        detail="unknown worker engine",
                        code="unknown_engine",
                    )
                    await _report(
                        "worker_spawn_rejected",
                        reason="unknown_engine",
                        engine=str(engine),
                    )
                    continue
                if not self._engine_available_for_role(str(engine), "bootstrap"):
                    _ack(
                        cmd,
                        state="failed",
                        detail="worker profile capacity exhausted",
                        code="profile_capacity",
                    )
                    await _report(
                        "worker_spawn_rejected",
                        reason="profile_capacity",
                        engine=str(engine),
                    )
                    continue
                try:
                    w = self._make_cli_worker(engine, mode="bootstrap")
                except WorkerSpawnRejected as exc:
                    _ack(
                        cmd,
                        state="failed",
                        detail="worker spawn rejected",
                        code="worker_spawn_rejected",
                    )
                    await _report(
                        "worker_spawn_rejected",
                        reason=str(exc),
                        engine=str(engine),
                        phase="operator",
                    )
                    continue
                except WorkerBudgetExhausted as exc:
                    _ack(
                        cmd,
                        state="failed",
                        detail="worker budget exhausted",
                        code="worker_budget_exhausted",
                    )
                    await _report(
                        "worker_spawn_rejected",
                        reason=str(exc),
                        spawned_total=self._spawned_total,
                        max_total_workers=self.max_total_workers,
                        cost_usd=self._current_cost_usd(),
                        cost_budget_usd=self.cost_budget_usd,
                    )
                    continue
                try:
                    t = await self._schedule_control_worker(
                        w, name=f"operator-{engine}"
                    )
                except ControlShutdownIncomplete:
                    raise
                except Exception as exc:
                    _ack(
                        cmd,
                        state="failed",
                        detail="worker scheduling failed",
                        code="worker_schedule_failed",
                    )
                    await _report(
                        "worker_spawn_rejected",
                        reason=str(exc),
                        engine=str(engine),
                        phase="operator",
                    )
                    continue
                tasks[t] = engine
                task_solvers[t] = w
                _ack(
                    cmd,
                    state="effect_observed",
                    detail="worker registered",
                    target_ids=[w.solver_id],
                    effect="worker_spawned",
                    worker_id=w.solver_id,
                    engine=str(engine),
                )
                await _report(
                    "worker_spawned",
                    worker=w.solver_id,
                    phase="operator",
                    worker_role="worker",
                    **worker_identity_event_fields(w),
                )
            elif action == "kill":
                sid = str(cmd.get("solver_id") or "")
                matched = False
                for t, w in list(task_solvers.items()):
                    if getattr(w, "solver_id", None) != sid:
                        continue
                    matched = True
                    delivered = self._cancel_solver(w)
                    if delivered:
                        t.cancel()
                        _ack(
                            cmd,
                            state="effect_observed",
                            detail="worker cancellation requested",
                            target_ids=[sid],
                            effect="worker_cancel_requested",
                            process_exit_confirmed=False,
                        )
                        await _report("worker_killed", worker=sid)
                    else:
                        _ack(
                            cmd,
                            state="unknown",
                            detail="worker cancellation could not be delivered",
                            code="worker_cancel_failed",
                            process_exit_confirmed=False,
                        )
                    break
                if not matched:
                    _ack(
                        cmd,
                        state="unknown",
                        detail="worker was not found",
                        code="worker_not_found",
                    )
            else:
                _ack(
                    cmd,
                    state="failed",
                    detail="unknown worker command",
                    code="unknown_worker_command",
                )

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
                        r = (
                            p.get("reason") or p.get("text") or e.get("reason") or ""
                        ).strip()
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
            "probes."
        )
        if deadends:
            body = "\n".join(f"  - {d}" for d in deadends[-12:])
            return (
                f"{head}\n\nAlready ruled out (do NOT retry these — pick "
                f"something else):\n{body}"
            )
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
        state_port = getattr(self, "_search_state_port", None)
        if state_port is None:
            return []
        try:
            rows = state_port.query_legacy_candidates(run_id=self.run_id)
            out: list[dict] = []
            inferred_lanes: list[tuple[str, str, str]] = []
            seen_routes: set[str] = set()
            for r in rows:
                wc = str(r.get("worker_class") or "code")
                route = str(r.get("route_hash") or "")
                if (
                    route
                    and wc not in {"verifier", "review"}
                    and hasattr(self.shared_graph, "is_route_suppressed")
                    and self.shared_graph.is_route_suppressed(route)
                ):
                    continue
                if route and wc not in {"verifier", "review"}:
                    if route in seen_routes:
                        continue
                    seen_routes.add(route)
                lane_key = str(r.get("lane_key") or "")
                risk_class = str(r.get("risk_class") or "")
                resource_key = str(r.get("resource_key") or "")
                # E: dispatch preflight — skip an intent whose declared resource is
                # currently locked by ANOTHER worker (route around it, don't collide).
                if resource_key and hasattr(
                    self.shared_graph, "check_resource_conflicts"
                ):
                    try:
                        conflict = self.shared_graph.check_resource_conflicts(
                            resource_key=resource_key
                        )
                        if conflict.get("conflict"):
                            continue
                    except Exception:
                        pass
                if not lane_key:
                    hint = self._lane_hint_from_text(
                        str(r.get("goal") or ""), require_control_hint=True
                    )
                    lane_key = str(hint.get("lane_key") or "")
                    if lane_key:
                        risk_class = str(hint.get("risk_class") or risk_class or "")
                        inferred_lanes.append(
                            (
                                lane_key,
                                risk_class,
                                str(r.get("intent_id") or ""),
                            )
                        )
                out.append(
                    {
                        "intent_id": r.get("intent_id"),
                        "goal": r.get("goal"),
                        "worker_class": wc,
                        "route_hash": route,
                        "branch_id": r.get("branch_id") or "",
                        "priority": int(r.get("priority") or 0),
                        "lane_key": lane_key,
                        "risk_class": risk_class,
                        "resource_key": resource_key,
                        "from_facts": list(r.get("from_facts") or []),
                        "depends_on": list(r.get("depends_on") or []),
                    }
                )
            if inferred_lanes:
                state_port.apply_legacy_lane_inferences(
                    run_id=self.run_id, inferences=inferred_lanes
                )
            return out
        except Exception:
            return []

    def _ordinary_open_queue_depth(
        self, open_intents: Optional[list[dict]] = None
    ) -> int:
        intents = self._open_intents() if open_intents is None else open_intents
        return sum(
            1
            for it in intents
            if str(it.get("worker_class") or "code") in {"code", "shell_agent"}
        )

    def _reason_backpressure_active(self, open_intents: list[dict]) -> bool:
        return self._ordinary_open_queue_depth(open_intents) >= max(
            1, 2 * self.max_workers
        )

    def _active_review_count(self) -> int:
        # Keep done review tasks counted until the coordinator reap path releases
        # their profile/account claim. Dropping them here creates a split-brain
        # window: global review capacity looks free while the profile-specific
        # review counter is still occupied, which surfaces as a bogus
        # "configured review engine unavailable" rejection.
        return len(self._active_review_tasks)

    def _ordinary_task_count(self, tasks: dict) -> int:
        self._active_review_count()
        return sum(
            1 for t in tasks
            if t not in self._active_review_tasks
            and t not in self._active_verifier_tasks
        )

    def _ordinary_capacity_available(self, tasks: dict) -> bool:
        cap = int(self.max_workers)
        try:
            from muteki.swarm.solo_depth_verify_v1 import (
                enabled as _solo_on,
                max_ordinary_workers as _solo_cap,
            )
            if _solo_on():
                cap = min(cap, int(_solo_cap()))
        except Exception:
            pass
        return self._ordinary_task_count(tasks) < cap

    def _review_capacity_available(self) -> bool:
        return self._active_review_count() < int(
            self.review_policy.get("max_concurrent") or 1
        )

    def _pending_report_repro_count(self) -> int:
        if self.shared_graph is None or not hasattr(
                self.shared_graph, "pending_report_repros"):
            return 0
        try:
            return len(self.shared_graph.pending_report_repros() or [])
        except Exception:
            return 0

    def _verifier_concurrency_cap(self) -> int:
        """One verifier per pending repro; max_concurrent > 0 is a hard cap.

        0 / unset means auto (match the pending-report queue).
        """
        pending = max(1, self._pending_report_repro_count())
        raw = self.verifier_policy.get("max_concurrent")
        try:
            configured = int(raw) if raw not in (None, "") else 0
        except (TypeError, ValueError):
            configured = 0
        if configured > 0:
            return max(1, min(configured, pending))
        return pending

    def _active_verifier_count(self) -> int:
        return len(self._active_verifier_tasks)

    def _verifier_capacity_available(self) -> bool:
        if not self.verifier_policy.get("enabled", True):
            return False
        if self._verifier_workers_spawned >= int(
                self.verifier_policy.get("max_verifier_workers") or 24):
            return False
        return self._active_verifier_count() < self._verifier_concurrency_cap()

    def _dispatchable_open_intents(self, open_intents: list[dict]) -> list[dict]:
        review_free = self._review_capacity_available()
        verifier_free = self._verifier_capacity_available()
        if review_free and verifier_free:
            return open_intents
        out: list[dict] = []
        for it in open_intents:
            wc = str(it.get("worker_class") or "code")
            if wc == "review" and not review_free:
                continue
            if wc == "verifier" and not verifier_free:
                continue
            out.append(it)
        return out

    def _capacity_dispatchable_open_intents(
        self, open_intents: list[dict], tasks: dict
    ) -> list[dict]:
        ordinary_free = self._ordinary_capacity_available(tasks)
        review_free = self._review_capacity_available()
        verifier_free = self._verifier_capacity_available()
        out: list[dict] = []
        for it in open_intents:
            wc = str(it.get("worker_class") or "code")
            if wc == "review":
                if review_free:
                    out.append(it)
            elif wc == "verifier":
                if verifier_free:
                    out.append(it)
            elif ordinary_free:
                out.append(it)
        return out

    async def _run_reason(self) -> int:
        """Reason phase: pro model reads the board, proposes intents. Returns the
        number of new intents proposed. Advisory — never raises into the loop.

        Side effect: stashes the latest verdict/drift in self._last_reason so the
        coordinator can act on a course_correct (phase 7: adaptive re-bootstrap)."""
        from muteki.solver.reason import PlannerFailure, PlannerFailureKind

        if self.shared_graph is None or self.llm is None:
            self._last_reason = None
            self._last_planner_failure = PlannerFailure(
                PlannerFailureKind.UNAVAILABLE,
                "shared graph or planner client is unavailable",
            )
            return 0
        try:
            from muteki.solver.reason import (
                dispatch_intents,
                run_reason,
            )

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
            try:
                from muteki.swarm.context_firewall_v1 import (
                    enabled as _cf_on,
                    fold_reason_context as _cf_fold,
                )
                if _cf_on():
                    summary = _cf_fold(
                        self.shared_graph,
                        list(self._standing_guidance),
                    )
                else:
                    summary = self.shared_graph.to_reason_summary(
                        standing_guidance=list(self._standing_guidance)
                    )
            except Exception:
                summary = self.shared_graph.to_reason_summary(
                    standing_guidance=list(self._standing_guidance)
                )
            try:
                fact_index = self.shared_graph.fact_pin_context()
            except Exception:
                fact_index = ""
            # Framework Sense prefix (f02 world-model etc.). Default Swarm: no-op.
            fw_prefix = getattr(self, "framework_reason_context_prefix", None)
            if callable(fw_prefix):
                try:
                    extra = str(fw_prefix() or "")
                    if extra:
                        summary = f"{extra}\n\n{summary}"
                except Exception:
                    pass
            # Framework class-side declaration override (env keys are cleared by
            # A/B harnesses). Default Swarm has neither attribute nor catalog → inert.
            decl_mode = getattr(self, "reason_declaration_mode", None)
            decl_catalog = None
            catalog_fn = getattr(self, "_declaration_target_catalog_v2", None)
            if callable(catalog_fn) and decl_mode:
                try:
                    decl_catalog = catalog_fn()
                except Exception:
                    decl_catalog = None
            result = await run_reason(
                llm=self.llm,
                model=self.reason_model,
                graph_summary=summary,
                fact_index=fact_index,
                max_intents=4,
                run_id=self.run_id,
                challenge_id=self.challenge.id,
                # pentest → judge completion against the operator's engagement goal
                # (CTF passes mode="ctf" + no goal → the prompt is byte-identical).
                mode=getattr(self.challenge, "mode", "ctf"),
                goal=(getattr(self.challenge, "goal", "") or None),
                scope=(getattr(self.challenge, "scope", "") or None),
                # Production coordination always uses the ordinary prompt. Offline
                # studies may annotate a frozen copy after this method returns.
                cognitive_shadow=False,
                declaration_mode=decl_mode,
                declaration_target_catalog_v2=decl_catalog,
            )
            self._last_reason = result
            try:
                pins = getattr(result, "pinned_facts", []) or []
                if pins:
                    self.shared_graph.pin_facts(
                        actor="reason",
                        fact_seqs=list(pins),
                        reason="reason model selected durable retention facts",
                    )
            except Exception:
                pass
            proposed = dispatch_intents(self.shared_graph, result, actor="reason")
            on_proposed = getattr(self, "framework_on_intents_proposed", None)
            if callable(on_proposed):
                try:
                    on_proposed(proposed)
                except Exception:
                    pass
            failure = result.planner_failure
            if not proposed and result.intents and failure is None:
                failure = PlannerFailure(
                    PlannerFailureKind.NEEDS_NEW_INFORMATION,
                    "all proposed intents were already covered or not reopenable",
                )
            self._last_planner_failure = failure
            for it in proposed:
                if self.bus is not None:
                    await self.bus.emit(
                        Event(
                            event_type=EventType.BLACKBOARD_DELTA,
                            run_id=self.run_id,
                            challenge_id=self.challenge.id,
                            payload=blackboard_delta_payload(
                                "intent_proposed",
                                actor="reason",
                                intent_id=it["intent_id"],
                                goal=it["goal"],
                                worker_class=it["worker_class"],
                                from_facts=it.get("from_facts", []),
                                declares=it.get("declares"),
                            ),
                        )
                    )
                # zh gist for the (often long, English) Reason goal — reuse the
                # planner's own llm client; fire-and-forget so planning isn't held up.
                self._summarize_intent_async(it["intent_id"], it["goal"])
            return len(proposed)
        except Exception as exc:
            self._last_reason = None
            self._last_planner_failure = PlannerFailure(
                PlannerFailureKind.EXCEPTION,
                f"{type(exc).__name__}: {exc}"[:500],
            )
            return 0

    def _summarize_intent_async(self, intent_id: str, goal: str) -> None:
        """Fire-and-forget a deepseek-flash zh gist for a Reason intent goal."""
        if self.bus is None or len((goal or "").strip()) < 48:
            return
        from muteki.solver.summarizer import summarize_node

        try:
            asyncio.create_task(
                summarize_node(
                    goal,
                    node_kind="intent",
                    intent_id=intent_id,
                    shared_graph=self.shared_graph,
                    llm=self.llm,
                    bus=self.bus,
                    run_id=self.run_id,
                    challenge_id=self.challenge.id,
                )
            )
        except RuntimeError:
            pass

    async def _emit_coord_bb(self, kind: str, **fields) -> None:
        """Coordinator-scoped blackboard delta (shared by the race-scout phase and
        the main loop's local _emit_bb)."""
        if self.bus is None:
            return
        try:
            await self.bus.emit(
                Event(
                    event_type=EventType.BLACKBOARD_DELTA,
                    run_id=self.run_id,
                    challenge_id=self.challenge.id,
                    payload=blackboard_delta_payload(
                        kind, actor="coordinator", **fields
                    ),
                )
            )
        except Exception:
            pass
