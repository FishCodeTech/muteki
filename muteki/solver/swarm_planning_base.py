"""Shared base for planning-layer Swarm replacements.

Subclasses override only the planning mechanism via ``plan_round`` /
``architecture_name``. Worker spawn, flag gate, SharedGraph, and budgets are
reused from production ``Swarm`` without modifying coordinator source.
"""

from __future__ import annotations

import asyncio
import re
import time
import uuid
from typing import Any, Optional

from muteki.models.solve_graph import engagement_goal_of
from muteki.solver.planning_kernel_v1 import (
    WorkingMemory,
    fold_graph_facts,
)
from muteki.swarm.swarm import Swarm
from muteki.swarm.swarm_support import (
    ControlShutdownIncomplete,
    SwarmOutcome,
    WorkerBudgetExhausted,
    WorkerSpawnRejected,
)

_FLAG_IN_TEXT_RE = re.compile(r"flag\{[^}]{1,200}\}", re.IGNORECASE)
# Git object ids / pure hashes wrapped as flag{...} are almost never the flag
# (r17 sharpturn: dozens of flag{40-hex} noise stalled planning).
_FALSE_HASH_FLAG_RE = re.compile(
    r"^flag\{[0-9a-fA-F]{32,64}\}$", re.IGNORECASE
)


class PlanningSwarmBase(Swarm):
    """Drop-in Swarm subclass with a lean planner→executor coordinator loop."""

    architecture_name: str = "planning_base"
    executor_timeout_s: int = 240
    planner_temperature: float = 0.3
    max_plan_rounds: int = 24
    initial_recon: bool = True
    # Short-horizon executors multiply spawn count vs one long bootstrap.
    # Inherit Swarm's default (12) and the harness will starve mid-solve —
    # observed: life with 13 facts exited budget_exhausted at ~331s.
    planning_max_total_workers: int = 96

    def __init__(self, *args: Any, **kwargs: Any) -> None:
        super().__init__(*args, **kwargs)
        # Raise lifetime spawn cap for planning architectures only.
        try:
            current = int(self.max_total_workers or 0)
        except (TypeError, ValueError):
            current = 0
        if current < self.planning_max_total_workers:
            # current==0 means "unset/unlimited" in some paths; still pin a
            # high finite cap so soft-extend logic has a baseline.
            self.max_total_workers = int(self.planning_max_total_workers)

    async def _run_coordinator(self) -> SwarmOutcome:
        self._operator_event = asyncio.Event()
        self._coord_sinks = list(getattr(self, "_coord_sinks", []) or [])
        self._pending_help = list(getattr(self, "_pending_help", []) or [])
        self._run_finalized = False
        per_solver: dict[str, Any] = {}
        winner: Optional[str] = None
        flag: Optional[str] = None
        started = time.monotonic()
        memory = WorkingMemory(
            challenge_brief=(
                f"{self.challenge.name} [{self.challenge.category}] "
                f"{(self.challenge.description or '')[:400]}"
            )
        )
        active: dict[asyncio.Task, Any] = {}
        terminal_reason = ""
        hitl_task: Optional[asyncio.Task[Any]] = None
        if self.hitl_inbox is not None:
            hitl_task = asyncio.create_task(
                self._supervise_control_drain(), name="hitl-drain")

        async def _stop_control_drain() -> None:
            nonlocal hitl_task
            if hitl_task is None:
                return
            task, hitl_task = hitl_task, None
            task.cancel()
            await asyncio.gather(task, return_exceptions=True)

        try:
            await self._emit_coord_bb(
                "planning_arch_start",
                architecture=self.architecture_name,
                challenge=self.challenge.name,
                category=self.challenge.category,
            )
            await self._planning_bootstrap(memory, active, per_solver, started)

            # Wall-clock first. max_plan_rounds is a soft force-seed cadence,
            # not a hard stop — r17 sharpturn burned 28 empty waits (~3s) while
            # BOOT/S1 still ran, then cancel-drained to ~480s with $0.06/$8 left.
            round_i = 0
            while True:
                await self._planning_wait_while_paused()
                if getattr(self, "_operator_stop", False):
                    terminal_reason = "operator_stop"
                    break
                self._sync_findings_from_graph()
                if self._goal_satisfied():
                    terminal_reason = (
                        "goal_met" if self._pentest_product() else "solved"
                    )
                    break
                if self._pentest_product() and self._coverage_complete():
                    self._coverage_exhausted = True
                    terminal_reason = "coverage_complete"
                    break
                if self._budget_elapsed(started) >= float(self.wall_clock_budget):
                    terminal_reason = "budget_exhausted"
                    break
                kind = self._budget_exhausted()
                if kind == "worker_budget_exhausted" and (
                    float(self.wall_clock_budget) - self._budget_elapsed(started)
                    > 45.0
                ):
                    cur = int(getattr(self, "max_total_workers", 0) or 0)
                    bump = max(24, int(self.max_workers) * 4)
                    self.max_total_workers = cur + bump
                    await self._emit_coord_bb(
                        "planning_worker_budget_extend",
                        architecture=self.architecture_name,
                        from_cap=cur,
                        to_cap=int(self.max_total_workers),
                        spawned=int(getattr(self, "_spawned_total", 0) or 0),
                    )
                    kind = None
                if kind:
                    terminal_reason = str(kind)
                    await self._emit_coord_bb(
                        "planning_budget_stop",
                        architecture=self.architecture_name,
                        kind=str(kind),
                        spawned=int(getattr(self, "_spawned_total", 0) or 0),
                        max_total_workers=int(
                            getattr(self, "max_total_workers", 0) or 0
                        ),
                    )
                    break

                # Reap finished workers before planning.
                await self._reap_finished(
                    active, per_solver, memory, started
                )
                self._sync_flags_from_graph()
                self._sync_findings_from_graph()
                if getattr(self.challenge, "mode", "ctf") == "pentest":
                    await self._drain_report_pipeline()
                if self._found_flags:
                    fresh = self._record_flags(*list(self._found_flags))
                    if fresh and self._goal_satisfied() and not self._pentest_product():
                        winner = winner or next(iter(per_solver), None)
                        flag = self._found_flags[0]
                        terminal_reason = "solved"
                        break
                if self._goal_satisfied():
                    winner = winner or next(iter(per_solver), None)
                    if self._found_flags:
                        flag = flag or self._found_flags[0]
                    terminal_reason = (
                        "goal_met" if self._pentest_product() else "solved"
                    )
                    break
                if self._pentest_product() and self._coverage_complete():
                    self._coverage_exhausted = True
                    terminal_reason = "coverage_complete"
                    break

                for f in fold_graph_facts(self.shared_graph, limit=24):
                    memory.add_fact(f)
                # Scan the full working memory — flags buried mid-list must not
                # be missed because we only looked at the tail (life false FAIL).
                self._promote_flags_from_texts(
                    list(memory.facts) + list(memory.receipts),
                    actor="planner",
                    memory=memory,
                )
                self._sync_flags_from_graph()
                self._sync_findings_from_graph()
                for fl in list(self._found_flags or []):
                    if fl not in memory.flags:
                        memory.flags.append(fl)
                if self._goal_satisfied():
                    winner = winner or next(iter(per_solver), None)
                    if self._found_flags:
                        flag = self._found_flags[0]
                    terminal_reason = (
                        "goal_met" if self._pentest_product() else "solved"
                    )
                    break
                if self._pentest_product() and self._coverage_complete():
                    self._coverage_exhausted = True
                    terminal_reason = "coverage_complete"
                    break

                slots = max(0, int(self.max_workers) - len(active))
                if slots <= 0:
                    await self._wait_any(active, timeout=2.0)
                    round_i += 1
                    continue

                repro_items = self._verifier_dispatch_items(
                    timeout=min(int(self.executor_timeout_s), 300)
                )
                force_seed = (
                    round_i >= int(self.max_plan_rounds)
                    or (
                        round_i > 0
                        and round_i
                        % max(8, max(1, int(self.max_plan_rounds) // 3))
                        == 0
                        and len(memory.facts) < 2
                    )
                )
                dispatch: list[dict[str, Any]] = list(repro_items[:slots])
                remaining = slots - len(dispatch)
                if remaining > 0:
                    planned = await self.plan_round(
                        memory=memory,
                        round_index=round_i,
                        free_slots=remaining,
                        wall_remaining=float(self.wall_clock_budget)
                        - self._budget_elapsed(started),
                        active_count=len(active),
                        force_seed=force_seed,
                    )
                    dispatch.extend(list(planned or []))
                if not dispatch:
                    if not active:
                        # Keep seeding until wall is gone — never idle-exit early.
                        dispatch = await self.plan_round(
                            memory=memory,
                            round_index=round_i,
                            free_slots=slots,
                            wall_remaining=float(self.wall_clock_budget)
                            - self._budget_elapsed(started),
                            active_count=0,
                            force_seed=True,
                        )
                    if not dispatch:
                        if active:
                            await self._wait_any(active, timeout=3.0)
                            round_i += 1
                            continue
                        if (
                            float(self.wall_clock_budget)
                            - self._budget_elapsed(started)
                            > 30.0
                        ):
                            await asyncio.sleep(2.0)
                            round_i += 1
                            continue
                        terminal_reason = terminal_reason or "no_plan"
                        break

                for item in dispatch:
                    if len(active) >= int(self.max_workers):
                        break
                    if self._budget_exhausted() == "worker_budget_exhausted":
                        cur = int(getattr(self, "max_total_workers", 0) or 0)
                        self.max_total_workers = cur + max(
                            12, int(self.max_workers) * 2
                        )
                    if self._budget_exhausted():
                        break
                    spawned = await self._spawn_planned_worker(
                        item, memory=memory
                    )
                    if spawned is None:
                        continue
                    task, worker = spawned
                    active[task] = worker
                    sid = str(getattr(worker, "solver_id", "") or "")
                    await self._emit_coord_bb(
                        "worker_spawned",
                        architecture=self.architecture_name,
                        worker=sid,
                        solver_id=sid,
                        unit_id=str(item.get("id") or ""),
                        mode=str(item.get("mode") or "explore"),
                        active=len(active),
                        max_workers=int(self.max_workers),
                    )
                    await self._emit_coord_bb(
                        "planning_dispatch",
                        architecture=self.architecture_name,
                        unit_id=str(item.get("id") or ""),
                        goal=str(item.get("goal") or "")[:200],
                        mode=str(item.get("mode") or "explore"),
                        timeout=int(item.get("timeout") or self.executor_timeout_s),
                        round=round_i,
                        worker=sid,
                        active=len(active),
                    )

                if active:
                    await self._wait_any(active, timeout=2.0)
                round_i += 1

            # Drain remaining workers on exit.
            await self._cancel_all(active, per_solver)
            await _stop_control_drain()
            if self._shutdown_owners_incomplete():
                self._retain_control_shutdown_owner(
                    winner=winner,
                    flag=flag,
                    goal_complete=False,
                    per_solver=per_solver,
                )
                raise ControlShutdownIncomplete(
                    "planning worker shutdown incomplete; runtime owner retained"
                )

            self._sync_flags_from_graph()
            self._sync_findings_from_graph()
            if self._found_flags:
                self._record_flags(*list(self._found_flags))
                flag = flag or self._found_flags[0]
            if self._goal_satisfied():
                winner = winner or next(iter(per_solver), None)
                terminal_reason = (
                    "goal_met" if self._pentest_product() else "solved"
                )

            goal_complete = self._goal_satisfied()
            await self._finalize_coordinator_run(
                winner=winner,
                flag=flag,
                goal_complete=goal_complete,
                per_solver=per_solver,
                terminal_reason=terminal_reason
                or ("solved" if goal_complete and not self._pentest_product()
                    else "goal_met" if goal_complete
                    else "budget_exhausted"),
            )
            if goal_complete:
                return SwarmOutcome(
                    True,
                    flag,
                    winner,
                    per_solver,
                    terminal_reason or (
                        "goal_met" if self._pentest_product() else "solved"
                    ),
                    flags=list(self._found_flags),
                )
            return SwarmOutcome(
                False,
                flag,
                winner,
                per_solver,
                terminal_reason or "budget_exhausted",
                flags=list(self._found_flags),
            )
        except BaseException as exc:
            await self._cancel_all(active, per_solver)
            await _stop_control_drain()
            if self._shutdown_owners_incomplete():
                self._retain_control_shutdown_owner(
                    winner=winner,
                    flag=flag,
                    goal_complete=False,
                    per_solver=per_solver,
                )
                if isinstance(exc, ControlShutdownIncomplete):
                    raise
                raise ControlShutdownIncomplete(
                    "planning worker shutdown incomplete; runtime owner retained"
                ) from exc
            try:
                await self._finalize_coordinator_run(
                    winner=winner,
                    flag=flag,
                    goal_complete=False,
                    per_solver=per_solver,
                    terminal_reason=(
                        "operator_stop"
                        if (getattr(self, "_operator_stop", False)
                            or isinstance(exc, asyncio.CancelledError))
                        else "planning_error"
                    ),
                )
            except Exception:
                pass
            raise
        finally:
            await _stop_control_drain()

    async def _planning_wait_while_paused(self) -> None:
        """Hold dispatch while PAUSE/FREEZE is active; RESUME/THAW wakes it."""
        event = self._operator_event
        while (getattr(self, "_operator_paused", False)
               and not getattr(self, "_operator_stop", False)):
            if event is None:
                await asyncio.sleep(0.1)
                continue
            event.clear()
            if (not getattr(self, "_operator_paused", False)
                    or getattr(self, "_operator_stop", False)):
                continue
            await event.wait()

    async def plan_round(
        self,
        *,
        memory: WorkingMemory,
        round_index: int,
        free_slots: int,
        wall_remaining: float,
        active_count: int,
        force_seed: bool = False,
    ) -> list[dict[str, Any]]:
        """Return dispatch items: {id, goal, mode, timeout, meta}."""
        raise NotImplementedError

    async def _planning_bootstrap(
        self,
        memory: WorkingMemory,
        active: dict,
        per_solver: dict,
        started: float,
    ) -> None:
        if not self.initial_recon:
            return
        # Dual overlapping bootstrap: previously we awaited ONE worker for
        # ~55% of wall before any second spawn — hard challenges had zero
        # time-overlap. Keep a long-horizon primary rush, but also launch a
        # second diversifying worker immediately and return to the plan loop
        # so chain/plan units can overlap with bootstrap.
        boot_timeout = int(
            min(600, max(300, float(self.wall_clock_budget) * 0.45))
        )
        pentest = getattr(self.challenge, "mode", "ctf") == "pentest"
        if pentest:
            goal_text = (getattr(self.challenge, "goal", "") or "").strip()
            if not goal_text:
                goal_text = engagement_goal_of(self.challenge).raw or (
                    f"Find and prove exploitable vulnerabilities in "
                    f"{getattr(self.challenge, 'name', 'the target')}"
                )
            primary_goal = (
                "BOOTSTRAP RUSH: pursue the engagement goal end-to-end against "
                f"the live target. Goal: {goal_text}. Write verified observations "
                "from real responses. Emit SUBMIT_REPORT only for a complete "
                "exploit report file. Do not inventory attachments or "
                "search the host repository. If you cannot finish, leave "
                "concrete facts and stop cleanly."
            )
            alt_goal = (
                "PARALLEL BOOTSTRAP (diverse angle): independently pursue a "
                f"DIFFERENT in-scope HTTP path or parameter toward `{goal_text}`. "
                "Do not wait for the other worker. Write verified facts; emit "
                "SUBMIT_REPORT only for a complete exploit report file."
            )
        else:
            primary_goal = (
                "BOOTSTRAP RUSH: solve the challenge end-to-end if possible. "
                "Inventory player files, pursue the most direct path to "
                "flag{...} from real command output, and write every verified "
                "observation to the blackboard as you go. If you cannot finish, "
                "leave concrete facts and stop cleanly."
            )
            alt_goal = (
                "PARALLEL BOOTSTRAP (diverse angle): independently attack "
                "a DIFFERENT path from the primary rush — alternate file, "
                "encoding, or protocol. Do not wait for the other worker. "
                "Write verified facts; extract flag{...} from real output."
            )
        primary = {
            "id": "BOOT",
            "goal": primary_goal,
            "mode": "bootstrap",
            "timeout": boot_timeout,
        }
        spawned = await self._spawn_planned_worker(primary, memory=memory)
        if spawned is None:
            return
        task, worker = spawned
        active[task] = worker
        sid = str(getattr(worker, "solver_id", "") or "")
        await self._emit_coord_bb(
            "worker_spawned",
            architecture=self.architecture_name,
            worker=sid,
            solver_id=sid,
            unit_id="BOOT",
            mode="bootstrap",
            active=len(active),
            max_workers=int(self.max_workers),
            spawn_ts=time.time(),
        )
        if int(self.max_workers) >= 2 and len(active) < int(self.max_workers):
            alt = {
                "id": "BOOT2",
                "goal": alt_goal,
                "mode": "bootstrap",
                "timeout": boot_timeout,
            }
            spawned2 = await self._spawn_planned_worker(alt, memory=memory)
            if spawned2 is not None:
                task2, worker2 = spawned2
                active[task2] = worker2
                sid2 = str(getattr(worker2, "solver_id", "") or "")
                await self._emit_coord_bb(
                    "worker_spawned",
                    architecture=self.architecture_name,
                    worker=sid2,
                    solver_id=sid2,
                    unit_id="BOOT2",
                    mode="bootstrap",
                    active=len(active),
                    max_workers=int(self.max_workers),
                    spawn_ts=time.time(),
                    parallel_bootstrap=True,
                )
                await self._emit_coord_bb(
                    "planning_dispatch",
                    architecture=self.architecture_name,
                    unit_id="BOOT2",
                    goal=str(alt["goal"])[:200],
                    mode="bootstrap",
                    timeout=boot_timeout,
                    round=-1,
                    worker=sid2,
                    active=len(active),
                )
        # Brief settle only — do NOT burn half the wall awaiting bootstrap.
        await self._wait_any(active, timeout=8.0)
        await self._reap_finished(active, per_solver, memory, started)
        self._sync_flags_from_graph()
        self._sync_findings_from_graph()
        if self._found_flags:
            self._record_flags(*list(self._found_flags))
        self._promote_flags_from_texts(
            list(memory.facts) + list(memory.receipts),
            actor="bootstrap",
            memory=memory,
        )

    async def _spawn_planned_worker(
        self, item: dict[str, Any], *, memory: WorkingMemory
    ) -> Optional[tuple[asyncio.Task, Any]]:
        healthy = await self._healthy_engines_async(role="explore")
        if not healthy:
            healthy = await self._healthy_engines_async(role="bootstrap")
        if not healthy:
            return None
        running = [
            str(getattr(w, "engine", "") or getattr(w, "solver_id", ""))
            for w in list(getattr(self, "_live_solvers", {}).values())
        ]
        try:
            engine = self._pick_engine(running, healthy, role="explore")
        except Exception:
            engine = healthy[0]
        goal = str(item.get("goal") or "").strip()
        if not goal:
            return None
        mode = str(item.get("mode") or "explore")
        existing_iid = str(item.get("intent_id") or "").strip()
        if mode == "verifier" and not existing_iid:
            return None
        intent_id = existing_iid or f"I-plan-{uuid.uuid4().hex[:10]}"
        timeout = int(item.get("timeout") or self.executor_timeout_s)
        # Inject compressed working memory as standing guidance for this worker only.
        packet = memory.render()
        prior = list(self._next_worker_guidance)
        unit_id = str(item.get("id") or "")
        if mode == "verifier":
            self._next_worker_guidance = [
                f"[architecture={self.architecture_name}] Reproduce the assigned "
                "report only. Run the replay yourself. Do not hunt new bugs.",
            ]
        elif mode == "bootstrap" or unit_id == "BOOT":
            # Do NOT say "only this unit" — that regressed easy end-to-end solves
            # (life PASS→FAIL) by narrowing the bootstrap rush.
            self._next_worker_guidance = [
                f"[architecture={self.architecture_name}] Primary bootstrap worker.",
                goal,
                packet,
            ]
        else:
            self._next_worker_guidance = [
                f"[architecture={self.architecture_name}] Execute ONLY this unit.",
                f"[unit={unit_id}] {goal}",
                packet,
            ]
        if self.shared_graph is not None and not existing_iid:
            try:
                self.shared_graph.propose_intent(
                    actor="planner",
                    intent_id=intent_id,
                    goal=goal[:500],
                )
            except Exception:
                pass
        intent_goal = goal if mode == "verifier" else goal[:800]
        worker_mode = (
            mode if mode in {"bootstrap", "explore", "respond", "verifier"}
            else "explore"
        )
        try:
            worker = self._make_cli_worker(
                engine,
                mode=worker_mode,
                intent_goal=intent_goal,
                intent_id=intent_id,
                timeout_override=timeout,
                profile_role="explore" if mode != "bootstrap" else "bootstrap",
            )
        except (WorkerBudgetExhausted, WorkerSpawnRejected):
            self._next_worker_guidance = prior
            return None
        except Exception:
            self._next_worker_guidance = prior
            return None
        finally:
            # _make_cli_worker consumes next_worker_guidance; restore residual.
            self._next_worker_guidance = prior
        # The planning path creates its own one-off intent instead of taking one
        # from coordinator_dispatch.  Persist the owner before the asyncio task can
        # reach a real CLI process.  Retirement uses that same owner fence to close
        # a started/cancelled intent; starting an unclaimed worker leaves an open,
        # ownerless row that can never satisfy the post-start retirement contract.
        if self.shared_graph is not None:
            try:
                claimed = bool(self.shared_graph.claim_intent(
                    worker=str(worker.solver_id), intent_id=intent_id,
                ))
            except Exception:
                claimed = False
            if not claimed:
                await self._retire_worker_account(
                    worker,
                    reason="planning intent claim failed before scheduling",
                )
                return None
        try:
            task = await self._schedule_control_worker(
                worker, name=worker.solver_id, intent_id=intent_id
            )
        except Exception:
            return None
        # Stash unit metadata for reap.
        worker._planning_unit = dict(item)
        worker._planning_intent_id = intent_id
        return task, worker

    async def _reap_finished(
        self,
        active: dict,
        per_solver: dict,
        memory: WorkingMemory,
        started: float,
    ) -> None:
        done = [t for t in list(active) if t.done()]
        for task in done:
            worker = active.pop(task)
            try:
                outcome = task.result()
            except asyncio.CancelledError:
                continue
            except Exception as exc:
                from muteki.models.solve_graph import SolveGraph
                from muteki.solver.types import SolveOutcome

                graph = getattr(worker, "graph", None)
                if graph is None:
                    graph = SolveGraph(challenge=self.challenge)
                outcome = SolveOutcome(
                    False, None, 0, graph, f"error: {exc}"
                )
            sid = str(getattr(worker, "solver_id", "") or "worker")
            per_solver[sid] = outcome
            unit = getattr(worker, "_planning_unit", None) or {}
            summary = str(getattr(outcome, "reason", "") or "")[:200]
            # Deterministic observation fold: harvest tool outputs into the graph
            # before reading facts (planning-layer memory update, not an interrupt).
            try:
                import os

                from muteki.swarm.fruitless_interrupt_v1 import (
                    collect_named_artifacts,
                    commit_harvested_facts,
                    harvest_artifact_tool_facts,
                )

                arts = collect_named_artifacts(self.shared_graph)
                prev_h = os.environ.get("MUTEKI_FRUITLESS_INTERRUPT_HARVEST")
                prev_fi = os.environ.get("MUTEKI_FRUITLESS_INTERRUPT")
                try:
                    os.environ["MUTEKI_FRUITLESS_INTERRUPT"] = "1"
                    os.environ["MUTEKI_FRUITLESS_INTERRUPT_HARVEST"] = "1"
                    rows = harvest_artifact_tool_facts(worker, arts, limit=6)
                finally:
                    if prev_h is None:
                        os.environ.pop("MUTEKI_FRUITLESS_INTERRUPT_HARVEST", None)
                    else:
                        os.environ["MUTEKI_FRUITLESS_INTERRUPT_HARVEST"] = prev_h
                    if prev_fi is None:
                        os.environ.pop("MUTEKI_FRUITLESS_INTERRUPT", None)
                    else:
                        os.environ["MUTEKI_FRUITLESS_INTERRUPT"] = prev_fi
                if rows:
                    commit_harvested_facts(
                        self.shared_graph, actor=sid, rows=rows
                    )
            except Exception:
                pass
            new_facts = fold_graph_facts(self.shared_graph, limit=16)
            before = set(memory.facts)
            for f in new_facts:
                memory.add_fact(f)
            gained = [f for f in memory.facts if f not in before]
            found = ""
            if getattr(outcome, "flag", None):
                found = str(outcome.flag)
            elif getattr(outcome, "flags", None):
                flags = list(outcome.flags or [])
                if flags:
                    found = str(flags[0])
            success = bool(getattr(outcome, "solved", False) or found or gained)
            receipt = (
                f"unit={unit.get('id')}: solved={bool(getattr(outcome, 'solved', False))} "
                f"facts+={len(gained)} {summary}"
            )
            memory.add_receipt(receipt)
            unit_mode = str(unit.get("mode") or "")
            if unit_mode != "verifier" and not success:
                memory.add_dead_end(
                    f"unit {unit.get('id')} barren: {_clip_local(goal_of(unit), 120)}"
                )
            if unit_mode != "verifier":
                await self.on_unit_finished(
                    memory=memory,
                    unit=unit,
                    outcome=outcome,
                    new_facts=gained,
                    found_flag=found,
                )
            intent_id = str(getattr(worker, "_planning_intent_id", "") or "")
            try:
                await self._retire_worker_account(
                    worker, intent_id=intent_id, reason="planning_reap"
                )
            except Exception:
                pass
            if found:
                self._record_flags(found)
            # Promote flags buried in harvested facts / receipts (observed life
            # regression: flag sat inside fact text + xxd harvest while the
            # planner kept chaining and exited budget_exhausted unsolved).
            promoted = self._promote_flags_from_texts(
                [summary, receipt] + gained + list(memory.facts[-8:]),
                actor=sid,
                memory=memory,
            )
            if promoted and not found:
                found = promoted[0]

    def _promote_flags_from_texts(
        self,
        texts: list[str],
        *,
        actor: str,
        memory: WorkingMemory | None = None,
    ) -> list[str]:
        """Compatibility hook; text cannot promote a flag into accepted state."""
        return []

    @staticmethod
    def _flags_from_hex_blob(text: str) -> list[str]:
        """Best-effort decode of contiguous hex runs that yield flag{...}."""
        found: list[str] = []
        for run in re.findall(r"[0-9a-fA-F]{16,4000}", text or ""):
            if len(run) % 2:
                run = run[:-1]
            try:
                raw = bytes.fromhex(run)
            except ValueError:
                continue
            decoded = raw.decode("utf-8", errors="ignore")
            found.extend(_FLAG_IN_TEXT_RE.findall(decoded))
        return found

    async def on_unit_finished(
        self,
        *,
        memory: WorkingMemory,
        unit: dict[str, Any],
        outcome: Any,
        new_facts: list[str],
        found_flag: str,
    ) -> None:
        """Hook for architecture-specific ledger updates."""
        return None

    async def _wait_any(self, active: dict, *, timeout: float) -> None:
        if not active:
            await asyncio.sleep(min(0.5, timeout))
            return
        try:
            await asyncio.wait(
                set(active.keys()),
                timeout=timeout,
                return_when=asyncio.FIRST_COMPLETED,
            )
        except Exception:
            await asyncio.sleep(min(0.5, timeout))

    async def _cancel_all(self, active: dict, per_solver: dict) -> None:
        for task, worker in list(active.items()):
            if not task.done():
                try:
                    self._cancel_solver(worker)
                except Exception:
                    pass
                task.cancel()
        if active:
            await asyncio.gather(*list(active.keys()), return_exceptions=True)
        for task, worker in list(active.items()):
            sid = str(getattr(worker, "solver_id", "") or "worker")
            if sid not in per_solver:
                from muteki.models.solve_graph import SolveGraph
                from muteki.solver.types import SolveOutcome

                graph = getattr(worker, "graph", None)
                if graph is None:
                    graph = SolveGraph(challenge=self.challenge)
                per_solver[sid] = SolveOutcome(
                    False, None, 0, graph, "cancelled"
                )
            try:
                await self._retire_worker_account(
                    worker,
                    intent_id=str(
                        getattr(worker, "_planning_intent_id", "") or ""
                    ),
                    reason="planning_shutdown",
                )
            except Exception:
                pass
        active.clear()

    async def _llm_json_plan(self, *, system: str, user: str) -> str:
        if self.llm is None:
            return ""
        try:
            resp = await self.llm.chat(
                model=self.reason_model,
                messages=[
                    {"role": "system", "content": system},
                    {"role": "user", "content": user},
                ],
                temperature=self.planner_temperature,
                max_tokens=None,
                stream=False,
                run_id=self.run_id,
                challenge_id=self.challenge.id,
                solver_id=f"planner:{self.architecture_name}",
            )
            return str(getattr(resp, "content", "") or "")
        except Exception:
            return ""


def goal_of(unit: dict[str, Any]) -> str:
    return str(unit.get("goal") or "")


def _clip_local(text: str, n: int) -> str:
    body = str(text or "").strip()
    if len(body) <= n:
        return body
    return body[: n - 1] + "…"
