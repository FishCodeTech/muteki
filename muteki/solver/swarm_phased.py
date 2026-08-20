"""Recursive phase-decomposition swarm with budget-aware pruning.

Planning-layer difference vs production / PEX / HypoLedger
----------------------------------------------------------
Production: continuous intent churn without typed stages.
PEX: free-form subgoal DAG.
HypoLedger: competing hypotheses.

SwarmPhased: a **deterministic phase machine** (RECON→ANALYZE→EXPLOIT→VERIFY)
allocates worker parallelism and timeouts per phase; the LLM only proposes
within-phase goals. Phase transitions and pruning are rule-based from fact
counts / fruitless rounds / candidate flags — budget-aware branch pruning
without relying on interrupt knobs.
"""

from __future__ import annotations

from typing import Any

from muteki.solver.planning_kernel_v1 import (
    Phase,
    Subgoal,
    UnitStatus,
    WorkingMemory,
    apply_executor_receipt,
    bootstrap_recon_goal,
    merge_subgoals,
    next_phase,
    parse_mission_plan,
    phase_parallel_budget,
    phase_timeout_seconds,
    planner_system_phase,
    ready_subgoals,
    select_dispatch,
)
from muteki.solver.swarm_planning_base import PlanningSwarmBase


class SwarmPhased(PlanningSwarmBase):
    """Drop-in Swarm replacement: phased recursive decomposition."""

    architecture_name = "phased"
    executor_timeout_s = 240
    initial_recon = False  # phase machine owns recon
    max_plan_rounds = 24

    def __init__(self, *args: Any, **kwargs: Any) -> None:
        super().__init__(*args, **kwargs)
        self._phase = Phase.RECON
        self._subgoals: list[Subgoal] = []
        self._fruitless_rounds = 0
        self._phase_started_round = 0
        self._rounds_in_phase = 0

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
        # Phase transition before planning.
        prev = self._phase
        self._phase = next_phase(
            self._phase,
            fact_count=len(memory.facts),
            fruitless_rounds=self._fruitless_rounds,
            has_candidate_flag=bool(memory.flags),
            phase_budget_exhausted=self._rounds_in_phase >= _phase_round_cap(
                self._phase
            ),
        )
        if self._phase != prev:
            await self._emit_coord_bb(
                "planning_phase_transition",
                architecture=self.architecture_name,
                from_phase=prev.value,
                to_phase=self._phase.value,
                facts=len(memory.facts),
                fruitless=self._fruitless_rounds,
            )
            # Context firewall at phase boundary: keep facts/dead_ends, drop receipts.
            memory.receipts = memory.receipts[-2:]
            self._subgoals = [
                s
                for s in self._subgoals
                if s.status in {UnitStatus.DONE, UnitStatus.FAILED}
            ]
            self._fruitless_rounds = 0
            self._phase_started_round = round_index
            self._rounds_in_phase = 0
        else:
            self._rounds_in_phase += 1

        if self._phase == Phase.DONE:
            return []

        if force_seed or not ready_subgoals(self._subgoals) or round_index == self._phase_started_round:
            await self._refresh_phase_plan(memory)

        parallel = phase_parallel_budget(
            self._phase, max_workers=min(int(self.max_workers), free_slots or 1)
        )
        ready = ready_subgoals(self._subgoals)
        chosen = select_dispatch(ready, parallel_max=min(parallel, free_slots))
        timeout = phase_timeout_seconds(
            self._phase, wall_remaining=wall_remaining
        )
        out: list[dict[str, Any]] = []
        for sub in chosen:
            sub.status = UnitStatus.ACTIVE
            out.append(
                {
                    "id": sub.id,
                    "goal": f"[PHASE {self._phase.value.upper()}] {sub.goal}",
                    "mode": "bootstrap" if self._phase == Phase.RECON else "explore",
                    "timeout": timeout,
                    "meta": {"phase": self._phase.value},
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
        # strip phase prefix ids may still match
        sub = next((s for s in self._subgoals if s.id == sid), None)
        if sub is None:
            # try bare id without phase noise
            for s in self._subgoals:
                if s.status == UnitStatus.ACTIVE:
                    sub = s
                    break
        if sub is None:
            if not new_facts and not found_flag:
                self._fruitless_rounds += 1
            else:
                self._fruitless_rounds = 0
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
        if success:
            self._fruitless_rounds = 0
        else:
            self._fruitless_rounds += 1
            # budget-aware prune: abandon sibling open goals sharing barren prefix
            if self._fruitless_rounds >= 2:
                for s in self._subgoals:
                    if s.status == UnitStatus.OPEN and s.id != sub.id:
                        # keep at most one alternative
                        pass
                # drop excess open goals beyond 1
                open_ids = [
                    s.id for s in self._subgoals if s.status == UnitStatus.OPEN
                ]
                for extra in open_ids[1:]:
                    for s in self._subgoals:
                        if s.id == extra:
                            s.status = UnitStatus.ABANDONED

    async def _refresh_phase_plan(self, memory: WorkingMemory) -> None:
        if self._phase == Phase.RECON and not self._subgoals:
            self._subgoals = [
                Subgoal(
                    id="P-R1",
                    goal=bootstrap_recon_goal(
                        self.challenge.category,
                        self.challenge.description or "",
                        mode=getattr(self.challenge, "mode", "ctf") or "ctf",
                        engagement_goal=getattr(self.challenge, "goal", "") or "",
                    ),
                    rationale="phase-seed",
                )
            ]
            if phase_parallel_budget(Phase.RECON, max_workers=2) > 1:
                self._subgoals.append(
                    Subgoal(
                        id="P-R2",
                        goal=(
                            "Secondary RECON: hash/identify encodings in player "
                            "files; list suspicious offsets or magic — facts only."
                        ),
                        rationale="phase-seed-diverse",
                    )
                )
            return

        user = (
            f"{memory.render()}\n\n"
            f"phase={self._phase.value}\n"
            f"fruitless_rounds={self._fruitless_rounds}\n"
            "Propose within-phase subgoals as JSON."
        )
        raw = await self._llm_json_plan(
            system=planner_system_phase(self._phase), user=user
        )
        plan = parse_mission_plan(raw, default_parallel=2)
        if plan.subgoals:
            # namespace ids with phase prefix to avoid collisions
            for s in plan.subgoals:
                if not s.id.startswith("P-"):
                    s.id = f"P-{self._phase.value[:1].upper()}{s.id}"
            self._subgoals = merge_subgoals(
                self._subgoals, plan.subgoals, plan.abandon_ids
            )
        elif not ready_subgoals(self._subgoals):
            self._subgoals.append(
                Subgoal(
                    id=f"P-{self._phase.value}-auto",
                    goal=_fallback_goal(self._phase, self.challenge.category),
                )
            )


def _phase_round_cap(phase: Phase) -> int:
    return {
        Phase.RECON: 2,
        Phase.ANALYZE: 3,
        Phase.EXPLOIT: 4,
        Phase.VERIFY: 2,
        Phase.DONE: 0,
    }.get(phase, 3)


def _fallback_goal(phase: Phase, category: str) -> str:
    if phase == Phase.ANALYZE:
        return (
            "ANALYZE: from verified facts, name the exact next decode/transform "
            "and execute it once; record the intermediate result as a fact."
        )
    if phase == Phase.EXPLOIT:
        return (
            "EXPLOIT: apply the best supported transform to recover flag{...} "
            "from real command output (no guessing)."
        )
    if phase == Phase.VERIFY:
        return (
            "VERIFY: re-derive the candidate flag from artifacts/commands and "
            "confirm flag{...} format from real output."
        )
    return bootstrap_recon_goal(category, "")


Swarm = SwarmPhased
