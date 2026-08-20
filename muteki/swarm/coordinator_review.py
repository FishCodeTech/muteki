"""Race scout, review scheduling, lane/resource locks, and review proposals.

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
import time
import time
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


class _ReviewLocksMixin:
    async def _run_race_scout(
        self,
        healthy: list[str],
        *,
        adopt_verifiers: "tuple[dict, dict, dict] | None" = None,
    ):
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
        tasks: dict[asyncio.Task[Any], Any] = {}
        try:
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
                    await self._emit_coord_bb(
                        str(exc), spawned_total=self._spawned_total,
                        max_total_workers=self.max_total_workers,
                        cost_usd=self._current_cost_usd(),
                        cost_budget_usd=self.cost_budget_usd)
                    break
            if not workers:
                return None, None, {}
            for w in workers:
                await self._emit_coord_bb("worker_spawned", worker=w.solver_id,
                                          phase="race", worker_role="worker",
                                          **worker_identity_event_fields(w))
                task = await self._schedule_control_worker(
                    w, name=f"race-{w.solver_id}")
                tasks[task] = w
        except BaseException:
            # Worker construction is an acquisition transaction. If the Nth build,
            # spawn event, or task creation fails, every earlier reservation/runtime
            # must be cancelled and released before the exception escapes.
            for task, worker in tasks.items():
                if not task.done():
                    self._cancel_solver(worker)
                    task.cancel()
            if tasks:
                await asyncio.gather(*tasks.keys(), return_exceptions=True)
            for worker in workers:
                await self._retire_worker_account(
                    worker,
                    intent_id=str(
                        getattr(worker, "intent_id_assigned", "")
                        or getattr(worker, "_intent_id", "") or ""),
                    reason="race-scout acquisition aborted",
                )
            if self._shutdown_owners_incomplete():
                raise ControlShutdownIncomplete(
                    "race-scout runtime exit remains unconfirmed")
            raise
        op_task = (asyncio.create_task(self._operator_event.wait(), name="race-operator-stop")
                   if self._operator_event is not None else None)
        results_by_worker: dict[Any, Any] = {}
        race_deadline = time.monotonic() + float(self.race_timeout)
        verifier_tasks: dict[asyncio.Task[Any], Any] = {}
        verifier_task_solvers: dict[asyncio.Task[Any], Any] = {}
        race_force_end = False
        race_finished_workers: set[str] = set()

        async def _cancel_pending_race(reason: str) -> None:
            nonlocal race_force_end
            race_force_end = True
            for t, w in list(tasks.items()):
                if t in pending and not t.done():
                    self._cancel_solver(w)
                    t.cancel()
            try:
                await self._emit_coord_bb("race_phase_ended", reason=reason)
            except Exception:
                pass

        try:
            pending = set(tasks.keys())
            while pending and not race_force_end:
                remaining = race_deadline - time.monotonic()
                if remaining <= 0:
                    await _cancel_pending_race("race wall-clock deadline")
                    break
                if getattr(self.challenge, "mode", "ctf") == "pentest":
                    await self._drain_report_pipeline()
                    await self._reap_verifier_tasks(
                        verifier_tasks, verifier_task_solvers,
                        emit_bb=self._emit_coord_bb)
                    await self._maybe_dispatch_verifiers(
                        healthy, verifier_tasks, verifier_task_solvers,
                        emit_bb=self._emit_coord_bb)
                    if self._pentest_race_submission_quota_met():
                        await _cancel_pending_race("pentest submission quota met")
                        break
                wait_set = set(pending)
                for vt in list(verifier_tasks.keys()):
                    if not vt.done():
                        wait_set.add(vt)
                if op_task is not None and not op_task.done():
                    wait_set.add(op_task)
                wait_timeout = min(2.0, max(0.05, remaining))
                done, _ = await asyncio.wait(
                    wait_set, timeout=wait_timeout,
                    return_when=asyncio.FIRST_COMPLETED)
                if op_task is not None and op_task in done:
                    if self._operator_stop:
                        for t, w in tasks.items():
                            if not t.done():
                                self._cancel_solver(w)
                                t.cancel()
                        break
                    op_task = asyncio.create_task(
                        self._operator_event.wait(), name="race-operator-stop")
                    done.discard(op_task)
                if getattr(self.challenge, "mode", "ctf") == "pentest":
                    await self._reap_verifier_tasks(
                        verifier_tasks, verifier_task_solvers,
                        emit_bb=self._emit_coord_bb)
                for t in [d for d in done if d in pending]:
                    pending.discard(t)
                    try:
                        results_by_worker[tasks[t]] = t.result()
                    except BaseException as exc:  # noqa: BLE001
                        results_by_worker[tasks[t]] = exc
                    finished_n = len(tasks) - len(pending)
                    w = tasks[t]
                    res = results_by_worker[w]
                    result_label = (
                        "cancelled" if isinstance(res, asyncio.CancelledError)
                        else ("error" if isinstance(res, BaseException)
                              else ("solved" if getattr(res, "solved", False) else "done")))
                    sid = getattr(w, "solver_id", None) or "cli-?"
                    race_finished_workers.add(sid)
                    try:
                        await self._emit_coord_bb(
                            "race_worker_finished",
                            worker=sid,
                            finished=finished_n,
                            total=len(tasks),
                            result=result_label,
                        )
                    except Exception:
                        pass
                    if getattr(self.challenge, "mode", "ctf") == "pentest":
                        await self._drain_report_pipeline()
                        await self._maybe_dispatch_verifiers(
                            healthy, verifier_tasks, verifier_task_solvers,
                            emit_bb=self._emit_coord_bb)
                if self._flags_complete():
                    await _cancel_pending_race("flags complete")
                    break
        finally:
            # Verifier boundary handoff: race verifiers were just dispatched to
            # reproduce submitted reports — do NOT abort reproduction at the
            # race boundary. Reap whatever finished during shutdown, then hand
            # the still-running tasks to the main coordinator loop (same
            # lifecycle as a verifier the main loop dispatched itself: it waits
            # on them, reaps them, retires their accounts and frees their
            # _active_verifier_tasks slots). Cancel + retire only when the run
            # ends right here (flags complete), when the operator stopped (the
            # main loop would otherwise keep them alive through the post-race
            # Reason pass before it notices the stop), or no adoption target.
            try:
                await self._reap_verifier_tasks(
                    verifier_tasks, verifier_task_solvers,
                    emit_bb=self._emit_coord_bb)
            except Exception:
                pass
            if verifier_tasks:
                adopt = (
                    adopt_verifiers is not None
                    and not self._flags_complete()
                    and not self._operator_stop
                )
                if adopt:
                    tasks_d, solvers_d, intents_d = adopt_verifiers
                    for t, w in list(verifier_tasks.items()):
                        engine = str(
                            getattr(getattr(w, "driver", None), "name", "")
                            or "verifier")
                        tasks_d[t] = engine
                        solvers_d[t] = w
                        intents_d[t] = str(
                            getattr(w, "intent_id_assigned", "")
                            or getattr(w, "_intent_id", "") or "")
                    try:
                        await self._emit_coord_bb(
                            "race_verifiers_adopted",
                            count=len(verifier_tasks))
                    except Exception:
                        pass
                else:
                    for t, w in list(verifier_tasks.items()):
                        if not t.done():
                            self._cancel_solver(w)
                            t.cancel()
                    await asyncio.gather(
                        *verifier_tasks.keys(), return_exceptions=True)
                    for t, w in list(verifier_tasks.items()):
                        self._active_verifier_tasks.discard(t)
                        await self._retire_worker_account(
                            w,
                            intent_id=str(
                                getattr(w, "intent_id_assigned", "")
                                or getattr(w, "_intent_id", "") or ""),
                            reason="race-scout verifier shutdown",
                        )
            verifier_tasks.clear()
            verifier_task_solvers.clear()
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
                await self._retire_worker_account(
                    w,
                    intent_id=str(
                        getattr(w, "intent_id_assigned", "")
                        or getattr(w, "_intent_id", "") or ""),
                    reason="race-scout worker shutdown",
                )
            if self._shutdown_owners_incomplete():
                raise ControlShutdownIncomplete(
                    "race-scout runtime exit remains unconfirmed")

        winner: "Optional[str]" = None
        flag: "Optional[str]" = None
        per_solver: "dict[str, SolveOutcome]" = {}
        finished_race_n = len(race_finished_workers)
        total_race = len(workers)
        for w in workers:
            res = results_by_worker.get(w, asyncio.CancelledError())
            sid = getattr(w, "solver_id", None) or "cli-?"
            if sid not in race_finished_workers:
                finished_race_n += 1
                if isinstance(res, asyncio.CancelledError):
                    result_label = "cancelled"
                elif isinstance(res, BaseException):
                    result_label = "error"
                else:
                    result_label = (
                        "solved" if getattr(res, "solved", False) else "done")
                race_finished_workers.add(sid)
                try:
                    await self._emit_coord_bb(
                        "race_worker_finished",
                        worker=sid,
                        finished=finished_race_n,
                        total=total_race,
                        result=result_label,
                    )
                except Exception:
                    pass
            if isinstance(res, BaseException):
                finish_label = (
                    "cancelled" if isinstance(res, asyncio.CancelledError)
                    else "error")
                await self._emit_coord_bb("worker_finished", worker=sid,
                                          result=finish_label, phase="race")
                continue
            per_solver[sid] = res
            await self._emit_coord_bb(
                "worker_finished", worker=sid, phase="race",
                result="solved" if getattr(res, "solved", False) else "done")
            # tally every flag this worker produced (multi-flag safe). _record_flags
            # dedups; the flags are already on the shared graph via _accept_flag.
            self._record_flags(*(getattr(res, "flags", None) or
                                 ([res.flag] if getattr(res, "flag", None) else [])))
            if getattr(self.challenge, "mode", "ctf") == "pentest":
                await self._drain_report_pipeline()
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

        if getattr(self.challenge, "mode", "ctf") == "pentest":
            await self._drain_report_pipeline()
            self._sync_findings_from_graph()

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

    def _select_verifier_engine(self, healthy: list[str]) -> str:
        configured = str(self.verifier_policy.get("engine") or "").strip()
        if configured:
            candidates: list[str] = []
            if self.worker_profiles:
                candidates = normalize_profile_roster(
                    [configured], self.worker_profiles)
                if configured in getattr(self, "_profiles_by_name", {}):
                    candidates = [configured] + [
                        c for c in candidates if c != configured]
            else:
                candidates = [configured]
            for e in candidates:
                if (self._healthy_matches(e, healthy)
                        and self._engine_available_for_role(e, "verifier")):
                    return e
            if (self._healthy_matches(configured, healthy)
                    and self._engine_available_for_role(configured, "verifier")):
                return configured
            if not self.verifier_policy.get("allow_verifier_fallback", False):
                raise RuntimeError(
                    f"configured verifier engine unavailable: {configured}")
        return self._pick_engine([], healthy, role="verifier")

    async def _reap_verifier_tasks(
        self,
        verifier_tasks: dict,
        verifier_task_solvers: dict,
        *,
        emit_bb,
    ) -> None:
        done = [t for t in list(verifier_tasks.keys()) if t.done()]
        for t in done:
            w = verifier_task_solvers.get(t) or verifier_tasks.get(t)
            sid = getattr(w, "solver_id", None) or "cli-?"
            try:
                outcome = t.result()
                await emit_bb(
                    "worker_finished", worker=sid,
                    result="solved" if getattr(outcome, "solved", False) else "done",
                    phase="verifier")
            except BaseException:
                await emit_bb("worker_finished", worker=sid, result="error",
                              phase="verifier")
            if w is not None:
                await self._retire_worker_account(
                    w,
                    intent_id=str(
                        getattr(w, "intent_id_assigned", "")
                        or getattr(w, "_intent_id", "") or ""),
                    reason="verifier task complete",
                )
            verifier_tasks.pop(t, None)
            verifier_task_solvers.pop(t, None)
            self._active_verifier_tasks.discard(t)
            if getattr(self.challenge, "mode", "ctf") == "pentest":
                await self._drain_report_pipeline()

    async def _maybe_dispatch_verifiers(
        self,
        healthy: list[str],
        verifier_tasks: dict,
        verifier_task_solvers: dict,
        *,
        emit_bb,
    ) -> bool:
        spawned = False
        for _ in range(24):
            if getattr(self.challenge, "mode", "ctf") != "pentest":
                return spawned
            if not self.verifier_policy.get("enabled", True):
                return spawned
            if not self._verifier_capacity_available():
                return spawned
            claimed_ids = {
                str(getattr(w, "intent_id_assigned", "")
                    or getattr(w, "_intent_id", "") or "")
                for w in verifier_task_solvers.values()
            }
            open_rows = [
                row for row in (self._verifier_dispatch_items() or [])
                if str(row.get("intent_id") or "").strip()
                and str(row.get("intent_id") or "") not in claimed_ids
            ]
            if not open_rows:
                return spawned
            row = open_rows[0]
            iid = str(row["intent_id"])
            try:
                engine = self._select_verifier_engine(healthy)
            except RuntimeError as exc:
                await emit_bb("worker_spawn_rejected", reason=str(exc),
                              phase="verifier", intent_id=iid)
                return spawned
            try:
                w = self._make_cli_worker(
                    engine, mode="verifier",
                    intent_goal=str(row.get("goal") or ""),
                    intent_id=iid)
            except WorkerSpawnRejected as exc:
                await emit_bb("worker_spawn_rejected", reason=str(exc),
                              engine=str(engine), phase="verifier", intent_id=iid)
                return spawned
            except WorkerBudgetExhausted as exc:
                await emit_bb(str(exc), spawned_total=self._spawned_total,
                              max_total_workers=self.max_total_workers,
                              cost_usd=self._current_cost_usd(),
                              cost_budget_usd=self.cost_budget_usd)
                return spawned
            won = False
            try:
                won = self.shared_graph.claim_intent(
                    worker=w.solver_id, intent_id=iid,
                    lease_s=float(row.get("timeout") or 240) + 300.0)
            except Exception:
                won = False
            if not won:
                await self._retire_worker_account(
                    w, reason="verifier intent claim not acquired")
                return spawned
            t = await self._schedule_control_worker(
                w, name=f"verifier-{w.solver_id}")
            verifier_tasks[t] = w
            verifier_task_solvers[t] = w
            # Race verifiers count against the verifier concurrency cap too —
            # keep the add/discard pair balanced with _reap_verifier_tasks and
            # the boundary handoff in the race finally block.
            self._active_verifier_tasks.add(t)
            self._verifier_workers_spawned += 1
            await emit_bb(
                "worker_spawned", worker=w.solver_id, phase="verifier",
                worker_role="verifier", intent_id=iid,
                **worker_identity_event_fields(w))
            spawned = True
        return spawned

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
        return _ReviewLocksMixin._lane_hint_from_text(need, worker=worker)

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
        t = await self._schedule_control_worker(
            w, name=f"review-{engine}")
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
                      phase="review", worker_role="review",
                      **worker_identity_event_fields(w))
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
                        kind = str(payload.get("kind") or "no_action")
                        summary = str(payload.get("summary") or "")
                        raw_route = str(payload.get("route_hash") or "")
                        route_hash = (
                            self.shared_graph.normalize_route_hash(raw_route)
                            if raw_route else ""
                        )
                        evidence_seqs = [
                            int(x) for x in payload.get("evidence_seqs", [])
                            if isinstance(x, int)
                        ]
                        intent_ids = [str(x) for x in payload.get("intent_ids", []) if x]
                        recommended_actions = [
                            str(x) for x in payload.get("recommended_actions", []) if x
                        ]
                        applied_seq = self.shared_graph.add_review_finding(
                            actor="coordinator",
                            kind=kind,
                            severity=str(payload.get("severity") or "info"),
                            summary=summary,
                            evidence_seqs=evidence_seqs,
                            intent_ids=intent_ids,
                            route_hash=route_hash,
                            branch_id=str(payload.get("branch_id") or ""),
                            recommended_actions=recommended_actions,
                        )
                        finding_id = SQLiteSharedGraph.review_finding_identity(
                            kind, summary, route_hash)
                        await emit_bb("review_finding", seq=applied_seq,
                                      finding_id=finding_id,
                                      finding_kind=kind,
                                      severity=str(payload.get("severity") or "info"),
                                      summary=summary,
                                      route_hash=route_hash,
                                      branch_id=str(payload.get("branch_id") or ""),
                                      recommended_actions=recommended_actions,
                                      evidence_seqs=evidence_seqs,
                                      intent_ids=intent_ids,
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
        t = await self._schedule_control_worker(
            w, name=f"review-directive-{engine}")
        tasks[t] = engine
        task_solvers[t] = w
        await emit_bb("coordinator_directive", action="rebootstrap",
                      directive=text[:500], priority=directive.get("priority", "normal"))
        await emit_bb("worker_spawned", worker=w.solver_id,
                      phase="review_directive", worker_role="worker",
                      **worker_identity_event_fields(w))
        return True
