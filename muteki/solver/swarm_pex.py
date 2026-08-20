"""Planner–Executor swarm with context-firewall working memory.

Planning-layer difference vs production Swarm
---------------------------------------------
Production Swarm: Reason reorders open intents on a long-lived multi-worker
race; workers keep long tool traces; replan is opportunistic.

SwarmPEX: an explicit Planner emits a **dependency-aware subgoal DAG**; short-
horizon Executors each receive exactly one subgoal plus a compressed working-
memory packet (never full transcripts). After each executor, receipts fold into
memory and the Planner may abandon / chain / parallelize. This is D-CIPHER-style
Planner–Executor + Claude-Code context firewall, not an interrupt/timeout knob.
"""

from __future__ import annotations

from typing import Any

from muteki.solver.planning_kernel_v1 import (
    MissionPlan,
    Subgoal,
    UnitStatus,
    WorkingMemory,
    apply_executor_receipt,
    bootstrap_recon_goal,
    default_mission_seed,
    merge_subgoals,
    parse_mission_plan,
    planner_system_pex,
    ready_subgoals,
    select_dispatch,
)
from muteki.solver.swarm_planning_base import PlanningSwarmBase


class SwarmPEX(PlanningSwarmBase):
    """Drop-in Swarm replacement: Planner–Executor architecture."""

    architecture_name = "pex"
    # Deep chain steps need room; planning difference is DAG+firewall, not
    # artificially tiny timeouts that starve reverse/crypto chains.
    executor_timeout_s = 480
    initial_recon = True
    max_plan_rounds = 20

    def __init__(self, *args: Any, **kwargs: Any) -> None:
        super().__init__(*args, **kwargs)
        self._subgoals: list[Subgoal] = []
        self._avoid_goals: set[str] = set()
        self._plan_calls = 0

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
        ready = ready_subgoals(self._subgoals)
        need_plan = (
            force_seed
            or not self._subgoals
            or (not ready and active_count == 0)
            or round_index == 0
            or (round_index % 2 == 0 and free_slots > 0)
        )
        if need_plan:
            await self._refresh_plan(memory, force_seed=force_seed)
            ready = ready_subgoals(self._subgoals)

        parallel = 2
        if self._subgoals:
            # use last plan's parallel if stored on notes via first open
            parallel = min(2, max(1, free_slots))
        chosen = select_dispatch(
            ready, parallel_max=min(parallel, free_slots), avoid_goals=self._avoid_goals
        )
        out: list[dict[str, Any]] = []
        timeout = int(min(self.executor_timeout_s, max(60.0, wall_remaining - 20.0)))
        for sub in chosen:
            sub.status = UnitStatus.ACTIVE
            out.append(
                {
                    "id": sub.id,
                    "goal": sub.goal,
                    "mode": "explore",
                    "timeout": timeout,
                    "meta": {"rationale": sub.rationale},
                }
            )
        return out

    async def on_unit_finished(
        self,
        *,
        memory: WorkingMemory,
        unit: dict[str, Any],
        outcome: Any,
        new_facts: list[str],
        found_flag: str,
    ) -> None:
        sid = str(unit.get("id") or "")
        sub = next((s for s in self._subgoals if s.id == sid), None)
        if sub is None:
            return
        success = bool(getattr(outcome, "solved", False) or found_flag or new_facts)
        apply_executor_receipt(
            memory,
            sub,
            new_facts=new_facts,
            success=success,
            summary=str(getattr(outcome, "reason", "") or "")[:200],
            found_flag=found_flag,
        )
        if not success:
            self._avoid_goals.add(sub.goal.strip().lower())

    async def _refresh_plan(
        self, memory: WorkingMemory, *, force_seed: bool = False
    ) -> None:
        self._plan_calls += 1
        seed = default_mission_seed(
            self.challenge.category,
            self.challenge.description or "",
            mode=getattr(self.challenge, "mode", "ctf") or "ctf",
            engagement_goal=getattr(self.challenge, "goal", "") or "",
        )
        if force_seed and not self._subgoals:
            self._subgoals = list(seed.subgoals)
            return

        ledger = _render_subgoal_ledger(self._subgoals)
        user = (
            f"{memory.render()}\n\n"
            f"current_subgoal_ledger:\n{ledger}\n\n"
            "Emit an updated JSON plan. Prefer chaining from verified facts. "
            "If ledger already has a viable next OPEN subgoal, you may return "
            "an empty subgoals list and only abandon dead ones."
        )
        raw = await self._llm_json_plan(system=planner_system_pex(), user=user)
        plan = parse_mission_plan(raw, default_parallel=2)
        if not plan.subgoals and not self._subgoals:
            plan = seed
        if plan.subgoals or plan.abandon_ids:
            self._subgoals = merge_subgoals(
                self._subgoals, plan.subgoals, plan.abandon_ids
            )
        if not self._subgoals:
            # last resort: single recon
            self._subgoals = [
                Subgoal(
                    id="S1",
                    goal=bootstrap_recon_goal(
                        self.challenge.category,
                        self.challenge.description or "",
                        mode=getattr(self.challenge, "mode", "ctf") or "ctf",
                        engagement_goal=getattr(self.challenge, "goal", "") or "",
                    ),
                )
            ]


def _render_subgoal_ledger(subgoals: list[Subgoal]) -> str:
    if not subgoals:
        return "(empty)"
    lines = []
    for s in subgoals:
        deps = ",".join(s.depends_on) if s.depends_on else "-"
        lines.append(
            f"- {s.id} [{s.status.value}] deps={deps} attempts={s.attempts}: "
            f"{s.goal[:160]}"
        )
    return "\n".join(lines)


# Alias expected by harness class-path loading.
Swarm = SwarmPEX
