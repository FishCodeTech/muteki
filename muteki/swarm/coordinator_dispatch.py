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
from muteki.solver.ctf_fgs import apply_ctf_decide_frontier
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
    spawn_reject_should_emit,
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
    def _auto_dispatch_enabled(self) -> bool:
        return str(getattr(self, "dispatch_mode", "fixed")) == "auto"

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
        """Intents explicitly available for dispatch (status='open').

        A claimed Intent remains owned until runtime retirement releases/reopens it.
        Wall-clock expiry never transfers a live Worker's ownership.
        """
        if self.shared_graph is None:
            return []
        state_port = getattr(self, "_search_state_port", None)
        if state_port is None:
            return []
        try:
            rows = state_port.query_legacy_candidates(run_id=self.run_id)
            out: list[dict] = []
            seen_routes: set[str] = set()
            ctf_mode = getattr(self.challenge, "mode", "ctf") == "ctf"
            for r in rows:
                wc = "code" if ctf_mode else str(r.get("worker_class") or "code")
                route = "" if ctf_mode else str(r.get("route_hash") or "")
                if (
                    not ctf_mode
                    and
                    route
                    and wc not in {"verifier", "review"}
                    and hasattr(self.shared_graph, "is_route_suppressed")
                    and self.shared_graph.is_route_suppressed(route)
                ):
                    continue
                if not ctf_mode and route and wc not in {"verifier", "review"}:
                    if route in seen_routes:
                        continue
                    seen_routes.add(route)
                lane_key = "" if ctf_mode else str(r.get("lane_key") or "")
                risk_class = "" if ctf_mode else str(r.get("risk_class") or "")
                resource_key = "" if ctf_mode else str(r.get("resource_key") or "")
                # A lane is itself a stable resource key. Workers may acquire
                # that resource from inside a turn before a coordinator lane
                # exists, so check both views here.
                conflict_resource = resource_key or lane_key
                if conflict_resource and hasattr(
                    self.shared_graph, "check_resource_conflicts"
                ):
                    try:
                        conflict = self.shared_graph.check_resource_conflicts(
                            resource_key=conflict_resource,
                            lane_key=lane_key,
                        )
                        if conflict.get("conflict"):
                            continue
                    except Exception:
                        pass
                out.append(
                    {
                        "intent_id": r.get("intent_id"),
                        "goal": r.get("goal"),
                        "worker_class": wc,
                        "route_hash": route,
                        "branch_id": "" if ctf_mode else r.get("branch_id") or "",
                        "priority": int(r.get("priority") or 0),
                        "lane_key": lane_key,
                        "risk_class": risk_class,
                        "resource_key": resource_key,
                        "from_facts": list(r.get("from_facts") or []),
                        "depends_on": [] if ctf_mode else list(r.get("depends_on") or []),
                        "expected_observable": "" if ctf_mode else str(r.get("expected_observable") or ""),
                        "stop_condition": "" if ctf_mode else str(r.get("stop_condition") or ""),
                        "coverage_key": "" if ctf_mode else str(r.get("coverage_key") or ""),
                        "requires_capabilities": [] if ctf_mode else list(
                            r.get("requires_capabilities") or []),
                        "value_claim": {} if ctf_mode else dict(r.get("value_claim") or {}),
                        "priority_reason": "" if ctf_mode else str(r.get("priority_reason") or ""),
                    }
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
            1, 2 * self._ordinary_capacity_limit()
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

    def _ordinary_capacity_limit(self) -> int:
        # max_workers is the one run-wide concurrency ceiling.  Review and
        # verifier processes consume the same capacity; role-specific limits below
        # may narrow it further but can never expand it.
        configured = int(self.max_workers)
        return max(0, int(self.max_workers))

    def _total_active_count(self, tasks: Optional[dict] = None) -> int:
        # _live_solvers includes workers constructed before their asyncio task is
        # installed and completed workers whose account/claims are not retired yet.
        # Both must continue occupying capacity.  Some verifier paths use a
        # separate task map, hence the max rather than trusting either view alone.
        live = len(getattr(self, "_live_solvers", {}) or {})
        scheduled = len(tasks or {})
        return max(live, scheduled)

    def _total_free_slots(self, tasks: Optional[dict] = None) -> int:
        return max(
            0, self._ordinary_capacity_limit() - self._total_active_count(tasks))

    def _ordinary_free_slots(self, tasks: dict) -> int:
        return self._total_free_slots(tasks)

    def _ordinary_planning_slots(self, tasks: dict) -> int:
        return self._ordinary_free_slots(tasks)

    def _ordinary_capacity_available(self, tasks: dict) -> bool:
        return self._ordinary_free_slots(tasks) > 0

    def _review_capacity_available(self, tasks: Optional[dict] = None) -> bool:
        return (
            self._total_free_slots(tasks) > 0
            and self._active_review_count() < int(
                self.review_policy.get("max_concurrent") or 1)
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

        0 / unset means auto. Keep automatic verification proportional to the
        ordinary solve capacity so a burst of challenged facts cannot create
        more verifier processes than the run can productively feed.
        """
        pending = max(1, self._pending_report_repro_count())
        raw = self.verifier_policy.get("max_concurrent")
        try:
            configured = int(raw) if raw not in (None, "") else 0
        except (TypeError, ValueError):
            configured = 0
        if configured > 0:
            return max(1, min(configured, pending))
        auto_cap = max(1, self._ordinary_capacity_limit() // 3)
        return min(pending, auto_cap)

    def _active_verifier_count(self) -> int:
        return len(self._active_verifier_tasks)

    def _verifier_capacity_available(self, tasks: Optional[dict] = None) -> bool:
        # ``verifier.enabled`` is the operator switch for the Pentest report
        # reproduction pipeline.  CTF fact challenges are part of the
        # coordinator's evidence-integrity loop: disabling report reproduction
        # must not strand every Candidate fact behind an unstartable verifier
        # Intent.
        if (
            getattr(self.challenge, "mode", "ctf") != "ctf"
            and not self.verifier_policy.get("enabled", True)
        ):
            return False
        if self._total_free_slots(tasks) <= 0:
            return False
        if self._verifier_workers_spawned >= int(
                self.verifier_policy.get("max_verifier_workers") or 24):
            return False
        return self._active_verifier_count() < self._verifier_concurrency_cap()

    def _dispatchable_open_intents(
        self, open_intents: list[dict], tasks: Optional[dict] = None,
    ) -> list[dict]:
        review_free = self._review_capacity_available(tasks)
        verifier_free = self._verifier_capacity_available(tasks)
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
        review_free = self._review_capacity_available(tasks)
        verifier_free = self._verifier_capacity_available(tasks)
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

    def _has_dispatchable_open_intents(self, tasks: dict) -> bool:
        """Whether an open intent can start under the current role policy."""
        open_intents = self._dispatchable_open_intents(
            self._open_intents(), tasks)
        return bool(self._capacity_dispatchable_open_intents(open_intents, tasks))

    def _capture_reason_attempt(self, result: Any, attempt_index: int) -> dict[str, Any]:
        """Persist one exact planner reply and return event-safe diagnostics."""
        diagnostics = getattr(result, "diagnostics", None)
        raw_response = str(getattr(diagnostics, "raw_response", "") or "")
        artifact_id = ""
        if raw_response and self.artifacts is not None:
            try:
                artifact_id = str(
                    self.artifacts.put(raw_response, suffix=".reason.txt") or ""
                )
            except Exception:
                artifact_id = ""
        draft_id = ""
        draft_receipts: list[dict[str, Any]] = []
        for note in getattr(result, "audit_notes", []) or []:
            if not isinstance(note, dict):
                continue
            if str(note.get("kind") or "") != "draft_receipts":
                continue
            draft_id = str(note.get("draft_id") or "")
            raw_ops = note.get("operations")
            if isinstance(raw_ops, list):
                draft_receipts = [
                    row for row in raw_ops[:32] if isinstance(row, dict)
                ]
            break
        return {
            "attempt_index": int(attempt_index),
            "draft_id": draft_id,
            "draft_receipts": draft_receipts,
            "response_status": str(
                getattr(diagnostics, "response_status", "not_recorded")
                or "not_recorded"
            ),
            "timed_out": bool(getattr(diagnostics, "timed_out", False)),
            "finish_reason": str(getattr(diagnostics, "finish_reason", "") or ""),
            "raw_response_artifact_id": artifact_id,
            "raw_response_sha256": str(
                getattr(diagnostics, "raw_response_sha256", "") or ""
            ),
            "raw_response_chars": int(
                getattr(diagnostics, "response_chars", 0) or 0
            ),
            "parse_status": str(
                getattr(diagnostics, "parse_status", "not_recorded")
                or "not_recorded"
            ),
            "parse_detail": str(
                getattr(diagnostics, "parse_detail", "") or ""
            )[:500],
            "raw_intent_count": int(
                getattr(diagnostics, "raw_intent_count", 0) or 0
            ),
            "parsed_intent_count": int(
                getattr(diagnostics, "parsed_intent_count", 0) or 0
            ),
            "input_tokens": int(
                getattr(diagnostics, "input_tokens", 0) or 0
            ),
            "output_tokens": int(
                getattr(diagnostics, "output_tokens", 0) or 0
            ),
        }

    async def _run_reason(self, *, max_intents: int | None = None) -> int:
        """Reason phase: pro model reads the board, proposes intents. Returns the
        number of new intents proposed. Advisory — never raises into the loop.

        Side effect: stashes the latest verdict/drift in self._last_reason so the
        coordinator can act on a course_correct (phase 7: adaptive re-bootstrap)."""
        from muteki.solver.reason import PlannerFailure, PlannerFailureKind

        self._last_reason_attempts = []
        self._last_dispatch_decisions = []
        self._last_reason_superseded = []
        self._last_reason_preemptions = []

        if self.shared_graph is None:
            unavailable_detail = "shared graph is unavailable"
            self._last_reason = None
            self._last_planner_failure = PlannerFailure(
                PlannerFailureKind.UNAVAILABLE,
                unavailable_detail,
            )
            self._last_reason_attempts = [{
                "attempt_index": 1,
                "response_status": "unavailable",
                "timed_out": False,
                "finish_reason": "",
                "raw_response_artifact_id": "",
                "raw_response_sha256": "",
                "raw_response_chars": 0,
                "parse_status": "not_run_unavailable",
                "parse_detail": unavailable_detail,
                "raw_intent_count": 0,
                "parsed_intent_count": 0,
            }]
            return 0
        if (
            self.llm is None
            and getattr(self.challenge, "mode", "ctf") != "ctf"
        ):
            unavailable_detail = str(
                getattr(self, "planner_unavailable_detail", "") or
                "planner client is unavailable"
            )
            self._last_reason = None
            self._last_planner_failure = PlannerFailure(
                PlannerFailureKind.UNAVAILABLE,
                unavailable_detail,
            )
            self._last_reason_attempts = [{
                "attempt_index": 1,
                "response_status": "unavailable",
                "timed_out": False,
                "finish_reason": "",
                "raw_response_artifact_id": "",
                "raw_response_sha256": "",
                "raw_response_chars": 0,
                "parse_status": "not_run_unavailable",
                "parse_detail": unavailable_detail,
                "raw_intent_count": 0,
                "parsed_intent_count": 0,
            }]
            return 0
        try:
            from muteki.solver.reason import (
                REASON_KEEP_RECENT_TOKENS,
                build_reason_prompt,
                compact_reason_context,
                dispatch_intents,
                estimate_reason_messages_tokens,
                reason_failure_is_context_overflow,
                run_reason,
            )
            from muteki.solver.pi_decide import (
                build_ctf_pi_decide_prompt,
                run_ctf_pi_reason,
            )
            from muteki.core.prompt_assembly import resolve_prompt_budget

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
            compact_summary = ""
            compact_cutoff_seq = 0
            try:
                epochs = list(self.shared_graph.compact_epochs() or [])
                latest = next((
                    row for row in reversed(epochs)
                    if str(row.get("trigger") or "") == "reason_context"
                ), None)
                if latest:
                    compact_summary = str(latest.get("summary") or "")
                    compact_cutoff_seq = int(latest.get("cutoff_seq") or 0)
            except Exception:
                compact_summary = ""
                compact_cutoff_seq = 0
            if getattr(self.challenge, "mode", "ctf") == "ctf":
                compact_summary = ""
                compact_cutoff_seq = 0

            def _render_graph_context() -> str:
                return self.shared_graph.to_reason_summary(
                    standing_guidance=list(self._standing_guidance),
                    compact_summary=compact_summary,
                    compact_cutoff_seq=compact_cutoff_seq,
                )

            def _render_stable_graph_context() -> tuple[str, int]:
                """Render against a quiet semantic watermark.

                Graph renderers use several bounded reads.  Re-render when a
                Worker commits between them so Decide never receives a mixture
                of the old and new frontier.
                """
                rendered = ""
                watermark_after = 0
                for _attempt in range(3):
                    try:
                        watermark_before = int(
                            self.shared_graph.semantic_graph_watermark() or 0
                        )
                    except Exception:
                        watermark_before = 0
                    rendered = _render_graph_context()
                    try:
                        watermark_after = int(
                            self.shared_graph.semantic_graph_watermark() or 0
                        )
                    except Exception:
                        watermark_after = watermark_before
                    if watermark_before == watermark_after:
                        break
                return rendered, watermark_after

            try:
                summary = _render_graph_context()
            except Exception:
                summary = self.shared_graph.to_reason_summary(
                    standing_guidance=list(self._standing_guidance),
                    compact_summary=compact_summary,
                    compact_cutoff_seq=compact_cutoff_seq,
                )
            reason_summary_chars = len(summary)

            # The first Reason pass may run before any worker has written facts.
            # Give it the operator-visible task directly so CTF planning is based
            # on the complete challenge brief and pentest planning is based on the
            # normalized engagement contract produced from the same prose input.
            challenge = self.challenge
            contract = getattr(challenge, "task_contract", None)
            raw_instruction = (
                str(getattr(contract, "raw_instruction", "") or "").strip()
                or challenge.description.strip()
            )
            if contract is not None:
                attachments = [
                    str(item.summary or item.name)
                    for item in contract.attachments
                ]
            else:
                attachments = [Path(str(item)).name for item in challenge.attachments]
            task_lines = [
                "## Operator task",
                f"mode: {challenge.mode}",
                f"raw instruction: {raw_instruction}",
            ]
            if challenge.target:
                task_lines.append(f"target: {challenge.target}")
            if attachments:
                task_lines.append(f"attachments: {', '.join(attachments)}")
            if challenge.mode == "pentest":
                engagement = challenge.engagement
                task_lines.extend([
                    f"authorized scope: {challenge.scope or ''}",
                    f"completion kind: {engagement.completion_kind}",
                    f"outcome predicate: {engagement.outcome_predicate}",
                    f"expected qualifying reports: {engagement.expected_findings}",
                    f"coverage until operator decision: {engagement.collect_until_coverage}",
                ])
            elif challenge.multi_flag:
                task_lines.append(
                    f"flag completion: collect {challenge.expected_flags or 'unknown'} distinct flags"
                )
            task_context = "\n".join(task_lines)
            summary = f"{task_context}\n\n{summary}"
            retry_note = getattr(self, "_reason_retry_note", None)
            retry_note_chars = 0
            retry_context = ""
            if isinstance(retry_note, dict) and retry_note.get("retry_index"):
                rejected = [
                    f"- {str(item.get('reason') or 'unknown')}: "
                    f"{str(item.get('goal') or '')}"
                    for item in list(retry_note.get("rejections") or [])
                    if isinstance(item, dict)
                ]
                retry_context = (
                    "## Planner retry\n"
                    f"retry index: {int(retry_note['retry_index'])}\n"
                    f"previous failure: {str(retry_note.get('failure') or 'empty_plan')}\n"
                    "The run goal is still incomplete. Propose executable Steps "
                    "that avoid the rejected ownership, stage, method, and coverage "
                    "combinations below.\n"
                    + ("\n".join(rejected) if rejected else "- no detailed rejection")
                )
                retry_note_chars = len(retry_context)
                summary = f"{summary}\n\n{retry_context}"
            requested = max_intents
            if requested is None:
                requested = getattr(
                    self, "_reason_max_intents_override", None
                )
            planning_limit = self._ordinary_capacity_limit()
            requested_intents = planning_limit if requested is None else int(requested)
            requested_intents = max(
                0,
                min(planning_limit, requested_intents),
            )
            ctf_pi_decide = (
                getattr(self.challenge, "mode", "ctf") == "ctf"
            )

            def _assemble_summary(graph_body: str) -> str:
                if ctf_pi_decide:
                    return graph_body
                value = f"{task_context}\n\n{graph_body}"
                if retry_context:
                    value = f"{value}\n\n{retry_context}"
                return value

            async def _compact_reason_graph(tokens_before: int) -> bool:
                nonlocal compact_summary, compact_cutoff_seq, summary, reason_summary_chars
                try:
                    cutoff = self.shared_graph.reason_compaction_cutoff(
                        REASON_KEEP_RECENT_TOKENS,
                        after_seq=compact_cutoff_seq,
                    )
                    if cutoff <= compact_cutoff_seq:
                        return False
                    source = self.shared_graph.to_reason_summary(
                        standing_guidance=list(self._standing_guidance),
                        compact_summary="",
                        compact_cutoff_seq=compact_cutoff_seq,
                    )
                    compacted = await compact_reason_context(
                        llm=self.llm,
                        model=self.reason_model,
                        graph_context=source,
                        previous_summary=compact_summary,
                        run_id=self.run_id,
                        challenge_id=self.challenge.id,
                    )
                    cited = {
                        int(value) for value in re.findall(
                            r"\[#(\d+)\]", compacted.summary)
                    }
                    known = {
                        int(event.get("seq") or 0)
                        for event in self.shared_graph.events()
                    }
                    if not cited.issubset(known):
                        raise RuntimeError(
                            "reason context compaction cited unknown graph events")
                    self.shared_graph.record_reason_context_compaction(
                        actor="reason",
                        cutoff_seq=cutoff,
                        summary=compacted.summary,
                        tokens_before=tokens_before,
                    )
                    compact_summary = compacted.summary
                    compact_cutoff_seq = cutoff
                    graph_body = _render_graph_context()
                    reason_summary_chars = len(graph_body)
                    summary = _assemble_summary(graph_body)
                    await self._emit_coord_bb(
                        "reason_context_compacted",
                        cutoff_seq=cutoff,
                        tokens_before=tokens_before,
                        summary_chars=len(compacted.summary),
                        input_tokens=compacted.input_tokens,
                        output_tokens=compacted.output_tokens,
                    )
                    return True
                except Exception as exc:
                    await self._emit_coord_bb(
                        "reason_context_compact_failed",
                        error=f"{type(exc).__name__}: {exc}"[:500],
                        tokens_before=tokens_before,
                    )
                    return False

            graph_body, reason_context_wm = _render_stable_graph_context()
            self._last_reason_context_wm = reason_context_wm
            reason_summary_chars = len(graph_body)
            summary = _assemble_summary(graph_body)
            reason_budget = (
                None
                if ctf_pi_decide
                else resolve_prompt_budget(self.reason_model, role="reason")
            )
            preview_messages = (
                build_ctf_pi_decide_prompt(
                    summary, max_intents=requested_intents
                )
                if ctf_pi_decide
                else build_reason_prompt(
                    summary,
                    max_intents=requested_intents,
                    goal=None,
                    mode=getattr(self.challenge, "mode", "ctf"),
                    scope=(getattr(self.challenge, "scope", "") or None),
                )
            )
            estimated_reason_tokens = estimate_reason_messages_tokens(preview_messages)
            compacted_this_pass = False
            compact_trigger_tokens = (
                min(
                    reason_budget.input_budget_tokens,
                    reason_budget.context_window_tokens - 32_768,
                )
                if reason_budget is not None
                else 0
            )
            if (
                not ctf_pi_decide
                and estimated_reason_tokens > compact_trigger_tokens
            ):
                compacted_this_pass = await _compact_reason_graph(
                    estimated_reason_tokens)

            reason_args = {
                "llm": self.llm,
                "model": self.reason_model,
                "graph_summary": summary,
                "max_intents": requested_intents,
                "run_id": self.run_id,
                "challenge_id": self.challenge.id,
                # pentest → judge completion against the operator's engagement goal
                # (CTF passes mode="ctf" + no goal → prompt remains unchanged).
                "mode": getattr(self.challenge, "mode", "ctf"),
                "goal": None,
                "scope": (getattr(self.challenge, "scope", "") or None),
            }

            async def _call_reason() -> Any:
                if not ctf_pi_decide:
                    return await run_reason(**reason_args)
                planner_profile = dict(
                    (getattr(self, "llm_profiles", {}) or {}).get("planner")
                    or {}
                )
                endpoint_id = str(
                    planner_profile.get("endpoint_id")
                    or planner_profile.get("credential_id")
                    or ""
                ).strip()
                account_id = (
                    endpoint_id.split(":", 1)[1]
                    if endpoint_id.startswith(("endpoint:", "account:"))
                    else endpoint_id
                )
                state_base = Path(
                    getattr(self, "workspace_root", None)
                    or getattr(self, "worker_root", None)
                    or Path.cwd()
                )
                return await run_ctf_pi_reason(
                    graph_summary=str(reason_args["graph_summary"]),
                    max_intents=int(reason_args["max_intents"]),
                    model=self.reason_model,
                    account_root=getattr(
                        self, "credential_accounts_root", None
                    ),
                    account_id=account_id,
                    state_root=state_base / ".muteki-agent-state" / "reason",
                    shared_graph=self.shared_graph,
                )
            # Context manifest for the assembled Reason prompt. Measurement-only;
            # a manifest failure must never break planning.
            try:
                manifest_messages = (
                    build_ctf_pi_decide_prompt(
                        summary,
                        max_intents=int(reason_args["max_intents"]),
                    )
                    if ctf_pi_decide
                    else build_reason_prompt(
                        summary,
                        max_intents=reason_args["max_intents"],
                        goal=reason_args["goal"],
                        mode=reason_args["mode"],
                        scope=reason_args["scope"],
                            )
                )
                section_chars: dict[str, int] = {
                    "system": sum(
                        len(str(m.get("content") or ""))
                        for m in manifest_messages
                        if m.get("role") == "system"
                    ),
                    "reason_summary": reason_summary_chars,
                }
                if ctf_pi_decide:
                    section_chars["workspace_files"] = len(
                        _ctf_workspace_files()
                    )
                else:
                    section_chars["operator_task"] = len(task_context)
                if retry_note_chars:
                    section_chars["planner_retry"] = retry_note_chars
                if compact_summary:
                    section_chars["historical_checkpoint"] = len(compact_summary)
                await self._emit_coord_bb(
                    "context_manifest",
                    role="reason",
                    worker_id="reason",
                    intent_id="",
                    sections=list(section_chars),
                    section_chars=section_chars,
                    total_chars=sum(
                        len(str(m.get("content") or ""))
                        for m in manifest_messages
                    ),
                    fact_seqs=[],
                    intent_ids=[],
                    artifact_ids=[],
                    full_board_included=ctf_pi_decide,
                    estimated_tokens=estimate_reason_messages_tokens(
                        manifest_messages),
                    context_window_tokens=(
                        reason_budget.context_window_tokens
                        if reason_budget is not None
                        else 0
                    ),
                    compact_cutoff_seq=compact_cutoff_seq,
                )
            except Exception:
                pass
            result = await _call_reason()
            self._last_reason_attempts.append(
                self._capture_reason_attempt(result, 1)
            )
            if (not ctf_pi_decide
                    and reason_failure_is_context_overflow(result)
                    and not compacted_this_pass
                    and await _compact_reason_graph(estimated_reason_tokens)):
                reason_args["graph_summary"] = summary
                try:
                    reason_context_wm = int(
                        self.shared_graph.semantic_graph_watermark() or 0
                    )
                    self._last_reason_context_wm = reason_context_wm
                except Exception:
                    pass
                result = await _call_reason()
                self._last_reason_attempts.append(
                    self._capture_reason_attempt(result, 2)
                )
            # No identical-prompt auto-retry on timeout/failure: the caller's
            # failure contract records the failed pass and consumes the
            # watermark instead of re-firing the same request.
            self._last_reason = result
            try:
                pins = getattr(result, "pinned_facts", []) or []
                if pins and getattr(self.challenge, "mode", "ctf") != "ctf":
                    self.shared_graph.pin_facts(
                        actor="reason",
                        fact_seqs=list(pins),
                        reason="reason model selected durable retention facts",
                    )
            except Exception:
                pass
            superseded: list[str] = []
            ctf_mode = getattr(self.challenge, "mode", "ctf") == "ctf"
            requested_supersede = list(
                getattr(result, "supersede_intents", []) or []
            )
            claimed_supersede: dict[str, dict[str, Any]] = {}
            if requested_supersede and not ctf_mode:
                try:
                    requested_ids = {
                        str(intent_id or "").strip()
                        for intent_id in requested_supersede
                        if str(intent_id or "").strip()
                    }
                    lane_rows = {
                        str(row.get("intent_id") or ""): dict(row)
                        for row in self.shared_graph.active_lane_intent_rows()
                    }
                    active_rows = {
                        str(row.get("intent_id") or ""): dict(row)
                        for row in self.shared_graph.coverage_intent_rows()
                        if (
                            str(row.get("status") or "")
                            in {"open", "claimed"}
                            and str(row.get("dispatch_state") or "") == "active"
                        )
                    }
                    for intent_id in requested_ids:
                        claim = dict(
                            self.shared_graph.intent_claim_state(intent_id) or {}
                        )
                        if str(claim.get("status") or "") != "claimed":
                            continue
                        claimed_supersede[intent_id] = {
                            **claim,
                            **active_rows.get(intent_id, {}),
                            **lane_rows.get(intent_id, {}),
                            "intent_id": intent_id,
                        }
                except Exception:
                    claimed_supersede = {}
            reprioritized: list[str] = []
            raw_priorities = dict(
                getattr(result, "reprioritize_intents", {}) or {}
            )
            if raw_priorities:
                try:
                    reprioritized = self.shared_graph.reprioritize_open_intents(
                        actor="reason",
                        priorities=raw_priorities,
                    )
                except Exception:
                    reprioritized = []
                if reprioritized:
                    await self._emit_coord_bb(
                        "intent_state_changed",
                        intent_id=",".join(reprioritized),
                        priority_changed=True,
                    )
            if ctf_mode:
                try:
                    apply_ctf_decide_frontier(result, self.shared_graph)
                except Exception:
                    pass
            dispatch_decisions: list[dict[str, Any]] = []
            proposed = dispatch_intents(
                self.shared_graph,
                result,
                actor="reason",
                decision_log=dispatch_decisions,
            )
            if ctf_mode and requested_supersede:
                try:
                    superseded = self.shared_graph.supersede_open_intent_ids(
                        actor="reason",
                        intent_ids=requested_supersede,
                        reason=str(
                            getattr(result, "supersede_why", "") or ""
                        ),
                    )
                except Exception:
                    superseded = []
                if superseded:
                    await self._emit_coord_bb(
                        "intent_state_changed",
                        intent_id=",".join(superseded),
                        dispatch_state="closed",
                        status="done",
                        result="superseded",
                        reason=str(
                            getattr(result, "supersede_why", "") or ""
                        )[:1000],
                    )
            try:
                verified_fact_ids = {
                    int(row.get("fact_seq") or 0)
                    for row in (self.shared_graph.verified_evidence() or [])
                    if int(row.get("fact_seq") or 0) > 0
                }
            except Exception:
                verified_fact_ids = set()
            evidence_backed_replacements = [
                row for row in proposed
                if (
                    str(row.get("priority") or "") == "high"
                    and any(
                        int(seq) in verified_fact_ids
                        for seq in (row.get("from_facts") or [])
                    )
                )
            ]
            if (
                not ctf_mode
                and requested_supersede
                and evidence_backed_replacements
            ):
                try:
                    superseded = self.shared_graph.supersede_open_intent_ids(
                        actor="reason",
                        intent_ids=requested_supersede,
                        reason=str(getattr(result, "supersede_why", "") or ""),
                    )
                except Exception:
                    superseded = []
                if superseded:
                    await self._emit_coord_bb(
                        "intent_state_changed",
                        intent_id=",".join(superseded),
                        dispatch_state="closed",
                        status="done",
                        result="superseded",
                        reason=str(
                            getattr(result, "supersede_why", "") or ""
                        )[:1000],
                    )
            self._last_reason_superseded = superseded
            supersede_why = str(
                getattr(result, "supersede_why", "") or ""
            ).strip()
            if claimed_supersede and supersede_why:
                try:
                    replacement_by_lane = {
                        self.shared_graph.normalize_lane_key(
                            str(row.get("lane_key") or "")
                        ): row
                        for row in proposed
                        if (
                            row in evidence_backed_replacements
                            and str(row.get("lane_key") or "").strip()
                        )
                    }
                    replacement_by_intent = {
                        str(row.get("dup_of") or ""): row
                        for row in proposed
                        if (
                            row in evidence_backed_replacements
                            and str(row.get("reopen_because") or "").strip()
                            and str(row.get("dup_of") or "").strip()
                            and bool(row.get("refreshes_claimed_context"))
                        )
                    }
                except Exception:
                    replacement_by_lane = {}
                    replacement_by_intent = {}
                high_replacements = evidence_backed_replacements
                preemptions: list[dict[str, Any]] = []
                for intent_id, old in claimed_supersede.items():
                    try:
                        lane_key = self.shared_graph.normalize_lane_key(
                            str(old.get("lane_key") or "")
                        )
                    except Exception:
                        lane_key = ""
                    replacement = (
                        replacement_by_intent.get(intent_id)
                        or replacement_by_lane.get(lane_key)
                    )
                    if replacement is None:
                        try:
                            old_sources = {
                                int(row.get("seq") or 0)
                                for row in self.shared_graph.intent_source_facts(
                                    intent_id
                                )
                                if int(row.get("seq") or 0) > 0
                            }
                        except Exception:
                            old_sources = set()
                        replacement = next(
                            (
                                row for row in high_replacements
                                if {
                                    int(seq)
                                    for seq in row.get("from_facts", [])
                                    if int(seq) > 0
                                } - old_sources
                            ),
                            None,
                        )
                    if replacement is None:
                        continue
                    preemptions.append({
                        "intent_id": intent_id,
                        "worker": str(old.get("worker") or ""),
                        "lane_key": lane_key,
                        "replacement_intent_id": str(
                            (replacement or {}).get("intent_id") or ""
                        ),
                        "reason": supersede_why[:1000],
                    })
                self._last_reason_preemptions = preemptions
            self._last_dispatch_decisions = dispatch_decisions
            failure = result.planner_failure
            if not proposed and result.intents and failure is None:
                failure = PlannerFailure(
                    PlannerFailureKind.NEEDS_NEW_INFORMATION,
                    "all proposed intents were already covered or not reopenable",
                )
            self._last_planner_failure = failure
            for it in proposed:
                # Graph projection is the single source of intent_proposed events.
                # Keep the optional display summary, but do not emit the mutation a
                # second time on the event bus.
                self._summarize_intent_async(it["intent_id"], it["goal"])
            return len(proposed)
        except Exception as exc:
            self._last_reason = None
            self._last_dispatch_decisions = []
            self._last_planner_failure = PlannerFailure(
                PlannerFailureKind.EXCEPTION,
                f"{type(exc).__name__}: {exc}"[:500],
            )
            if not self._last_reason_attempts:
                self._last_reason_attempts = [{
                    "attempt_index": 1,
                    "response_status": "error",
                    "timed_out": False,
                    "finish_reason": "exception",
                    "raw_response_artifact_id": "",
                    "raw_response_sha256": "",
                    "raw_response_chars": 0,
                    "parse_status": "not_run_exception",
                    "parse_detail": f"{type(exc).__name__}: {exc}"[:500],
                    "raw_intent_count": 0,
                    "parsed_intent_count": 0,
                }]
            return 0

    def _reason_event_fields(self, proposed: int) -> dict[str, Any]:
        """Stable diagnostics for every Reason pass, including race miss."""
        result = getattr(self, "_last_reason", None)
        intents = len(getattr(result, "intents", []) or [])
        parse_rejections = list(
            getattr(result, "intent_parse_rejections", []) or []
        )
        decisions = [
            *parse_rejections,
            *list(getattr(self, "_last_dispatch_decisions", []) or []),
        ]
        decisions.sort(key=lambda item: (
            int(item.get("raw_index", -1)),
            0 if item.get("stage") == "parse" else 1,
        ))
        drop_reasons: dict[str, int] = {}
        for decision in decisions:
            if decision.get("outcome") == "accepted":
                continue
            code = str(decision.get("reason_code") or "unknown")
            drop_reasons[code] = drop_reasons.get(code, 0) + 1
        duplicate_codes = {
            "declared_duplicate",
            "equivalent_step",
            "active_step_stage",
            "active_branch_stage",
            "active_coverage_method",
            "producer_successor_queued",
            "producer_successor_exists",
            "repeated_lane_evidence",
            "active_lane",
            "active_route",
            "storage_duplicate",
        }
        orphan_codes = {
            "orphan_no_source",
        }
        from muteki.solver.reason import PlannerFailureKind
        failure = getattr(self, "_last_planner_failure", None)
        failure_kind = getattr(failure, "kind", "")
        if hasattr(failure_kind, "value"):
            failure_kind = failure_kind.value
        failure_detail = str(getattr(failure, "detail", "") or "")[:300]
        reason_failed = str(failure_kind or "") in (
            PlannerFailureKind.EXCEPTION.value,
            PlannerFailureKind.TIMEOUT.value,
        )
        attempts = list(getattr(self, "_last_reason_attempts", []) or [])
        last_attempt = attempts[-1] if attempts else {}
        progress_sections = getattr(result, "progress_sections", {}) or {}
        return {
            "proposed": int(proposed or 0),
            "status": "failed" if reason_failed else "ok",
            "error": (
                f"{failure_kind}: {failure_detail}"[:300]
                if reason_failed else ""
            ),
            "dropped_total": sum(
                1 for decision in decisions
                if decision.get("outcome") != "accepted"
            ),
            "dropped_dup": sum(
                count for code, count in drop_reasons.items()
                if code in duplicate_codes
            ),
            "dropped_orphan": sum(
                count for code, count in drop_reasons.items()
                if code in orphan_codes
            ),
            "drop_reasons": drop_reasons,
            "superseded": len(
                getattr(self, "_last_reason_superseded", []) or []
            ),
            "superseded_intent_ids": list(
                getattr(self, "_last_reason_superseded", []) or []
            ),
            "preempt_requested": len(
                getattr(self, "_last_reason_preemptions", []) or []
            ),
            "preempted_intent_ids": [
                str(item.get("intent_id") or "")
                for item in (
                    getattr(self, "_last_reason_preemptions", []) or []
                )
            ],
            "intent_decisions": decisions,
            "raw_response_status": str(
                last_attempt.get("response_status") or "not_recorded"
            ),
            "raw_response_finish_reason": str(
                last_attempt.get("finish_reason") or ""
            ),
            "raw_response_artifact_id": str(
                last_attempt.get("raw_response_artifact_id") or ""
            ),
            "raw_response_sha256": str(
                last_attempt.get("raw_response_sha256") or ""
            ),
            "raw_response_chars": int(
                last_attempt.get("raw_response_chars") or 0
            ),
            "timed_out": bool(last_attempt.get("timed_out", False)),
            "timeout_occurred": any(
                bool(attempt.get("timed_out", False)) for attempt in attempts
            ),
            "parse_status": str(
                last_attempt.get("parse_status") or "not_recorded"
            ),
            "parse_detail": str(last_attempt.get("parse_detail") or ""),
            "raw_intent_count": int(last_attempt.get("raw_intent_count") or 0),
            "parsed_intent_count": int(
                last_attempt.get("parsed_intent_count") or intents
            ),
            "reason_attempts": attempts,
            "planner_failure": str(failure_kind or ""),
            "planner_failure_detail": failure_detail,
            "progress_summary": str(
                getattr(result, "progress_summary", "") or ""
            )[:1600],
            "progress_sections": {
                key: [str(item)[:500] for item in progress_sections.get(key, [])[:3]]
                for key in ("confirmed", "active", "blocked", "next")
            },
            "progress_verdict": str(getattr(result, "verdict", "") or ""),
        }

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
        if kind == "worker_spawn_rejected":
            cap = (
                int(getattr(self, "max_workers", 0) or 0),
                int(getattr(self, "_spawned_total", 0) or 0),
            )
            if not spawn_reject_should_emit(
                self,
                reason=str(fields.get("reason") or ""),
                phase=str(fields.get("phase") or ""),
                engine=str(fields.get("engine") or ""),
                capacity=cap,
            ):
                return
        if self.bus is None:
            return
        try:
            actor = str(fields.pop("actor", "") or "coordinator")
            await self.bus.emit(
                Event(
                    event_type=EventType.BLACKBOARD_DELTA,
                    run_id=self.run_id,
                    challenge_id=self.challenge.id,
                    payload=blackboard_delta_payload(
                        kind, actor=actor, **fields
                    ),
                )
            )
        except Exception:
            pass
