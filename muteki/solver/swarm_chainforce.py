"""Forced chain-completion swarm (coverage, not ranking).

Planning-layer difference
-------------------------
Production / prior rounds: Reason reorders open intents; after the first fact
lands, coverage stalls (R9: ranking fixes order, not coverage).

SwarmChainForce: every verified fact **deterministically synthesizes** exactly
one follow-up subgoal (no LLM required for the chain edge). The Planner LLM
may add/abandon branches, but the chain edge is rule-forced. Executors stay
short-horizon; working memory is firewalled. This is structurally a
"forced chain + context firewall" architecture, distinct from free-form PEX
DAG planning and from hypothesis falsification.
"""

from __future__ import annotations

from typing import Any

from muteki.solver.planning_kernel_v1 import (
    Subgoal,
    UnitStatus,
    WorkingMemory,
    apply_executor_receipt,
    bootstrap_recon_goal,
    merge_subgoals,
    parse_mission_plan,
    planner_system_pex,
    ready_subgoals,
    select_dispatch,
)
from muteki.solver.swarm_planning_base import PlanningSwarmBase


class SwarmChainForce(PlanningSwarmBase):
    """Drop-in Swarm: forced fact→next-step chain completion."""

    architecture_name = "chainforce"
    executor_timeout_s = 540
    initial_recon = True
    max_plan_rounds = 48
    # Prefer burning budget on overlapping workers over early exit.
    planning_max_total_workers = 96

    def __init__(self, *args: Any, **kwargs: Any) -> None:
        super().__init__(*args, **kwargs)
        self._subgoals: list[Subgoal] = []
        self._fact_fingerprint: frozenset[str] = frozenset()
        self._chain_seq = 0

    def _is_pentest(self) -> bool:
        return getattr(self.challenge, "mode", "ctf") == "pentest"

    def _seed_goal(self) -> str:
        return bootstrap_recon_goal(
            self.challenge.category,
            self.challenge.description or "",
            mode=getattr(self.challenge, "mode", "ctf") or "ctf",
            engagement_goal=getattr(self.challenge, "goal", "") or "",
        )

    def _forced_chain_goal(self, fact: str) -> str:
        if self._is_pentest():
            return (
                "FORCED CHAIN STEP: given the verified fact "
                f"`{fact[:180]}`, take the single next action "
                "that advances toward a complete exploit report "
                "(SUBMIT_REPORT=path from real request/response) "
                "on the live target. Record new facts from real "
                "responses; do not repeat the prior step."
            )
        return (
            "FORCED CHAIN STEP: given the verified fact "
            f"`{fact[:180]}`, perform the single most likely "
            "next decode/transform/extraction that advances "
            "toward flag{...}. Record new facts; do not repeat "
            "the prior step."
        )

    def _exploit_from_facts_goal(self, fact_blob: str) -> str:
        if self._is_pentest():
            return (
                "EXPLOIT FROM FACTS: using ONLY these verified facts — "
                f"{fact_blob[:700]} — perform the decisive step that "
                "yields a complete exploit report (SUBMIT_REPORT=path from real "
                "request/response) from real command output. "
                "Do not restart recon."
            )
        return (
            "EXPLOIT FROM FACTS: using ONLY these verified facts — "
            f"{fact_blob[:700]} — perform the decisive decode/"
            "decrypt/extract that yields flag{{...}} from real "
            "command output. Do not restart recon."
        )

    def _alternate_goal(self) -> str:
        if self._is_pentest():
            g = (getattr(self.challenge, "goal", "") or "").strip() or "the engagement goal"
            return (
                "ALTERNATE ANGLE: no open units remain. Hit a different "
                f"in-scope HTTP path or parameter toward `{g}`. One "
                "experiment; write verified facts from real responses. "
                "Do not search the host repository."
            )
        return (
            "ALTERNATE ANGLE: no open units remain. Attack a "
            "structurally different recovery path (git object "
            "recovery / alternate carve / competing decode). "
            "One experiment; write verified facts."
        )

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
        # Forced chain edge from new facts.
        fp = frozenset(memory.facts)
        if fp != self._fact_fingerprint:
            new_facts = [f for f in memory.facts if f not in self._fact_fingerprint]
            self._fact_fingerprint = fp
            for fact in new_facts[-2:]:
                self._chain_seq += 1
                sid = f"C{self._chain_seq}"
                self._subgoals.append(
                    Subgoal(
                        id=sid,
                        goal=self._forced_chain_goal(fact),
                        rationale="forced-chain",
                    )
                )

        if not self._subgoals:
            self._subgoals = [
                Subgoal(
                    id="S1",
                    goal=self._seed_goal(),
                    rationale="seed",
                )
            ]
        elif force_seed:
            # Append a fresh angle — never wipe OPEN/ACTIVE ledger (r17: force
            # seed replaced S1 while BOOT still ran and starved coverage).
            self._chain_seq += 1
            self._subgoals.append(
                Subgoal(
                    id=f"R{self._chain_seq}",
                    goal=self._seed_goal() + " Prefer a path not already attempted in dead-ends.",
                    rationale="forced-reseed",
                )
            )
        elif round_index > 0 and round_index % 3 == 0:
            await self._optional_planner_refresh(memory)

        # Coverage→closure: once enough facts exist without a flag, force an
        # exploit unit that must synthesize from the working memory (R10 gap).
        if (
            len(memory.facts) >= 3
            and not self._goal_satisfied()
            and free_slots > 0
            and not any(
                s.status == UnitStatus.OPEN and "EXPLOIT FROM FACTS" in s.goal
                for s in self._subgoals
            )
        ):
            self._chain_seq += 1
            fact_blob = "; ".join(memory.facts[-5:])
            self._subgoals.append(
                Subgoal(
                    id=f"E{self._chain_seq}",
                    goal=self._exploit_from_facts_goal(fact_blob),
                    rationale="forced-exploit",
                )
            )

        # Only halt on verified completion. Unverified flag{sha1…} noise in
        # working memory must NOT freeze the planner (r17 sharpturn).
        if self._goal_satisfied():
            return []

        ready = ready_subgoals(self._subgoals)
        if not ready and free_slots > 0 and not self._goal_satisfied():
            self._chain_seq += 1
            self._subgoals.append(
                Subgoal(
                    id=f"A{self._chain_seq}",
                    goal=self._alternate_goal(),
                    rationale="forced-alternate-idle",
                )
            )
            ready = ready_subgoals(self._subgoals)

        # Use every free slot up to 3 for true time-overlap on hard challenges.
        chosen = select_dispatch(
            ready, parallel_max=min(3, max(1, free_slots))
        )
        timeout = int(
            min(self.executor_timeout_s, max(90.0, wall_remaining - 15.0))
        )
        out: list[dict[str, Any]] = []
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
        # If barren, force an alternate angle (coverage, not reorder).
        if not success:
            self._chain_seq += 1
            self._subgoals.append(
                Subgoal(
                    id=f"A{self._chain_seq}",
                    goal=(
                        "ALTERNATE ANGLE: the previous unit produced no new "
                        "facts. Try a structurally different check "
                        "(different tool, different file offset, or a "
                        "competing decode hypothesis). One experiment only."
                    ),
                    rationale="forced-alternate",
                )
            )

    async def _optional_planner_refresh(self, memory: WorkingMemory) -> None:
        ledger = "\n".join(
            f"- {s.id} [{s.status.value}] {s.goal[:120]}" for s in self._subgoals[-8:]
        )
        user = (
            f"{memory.render()}\n\nledger:\n{ledger}\n\n"
            "Optionally add NEW subgoals or abandon ids as JSON. "
            "Forced-chain edges are already handled — focus on coverage gaps."
        )
        raw = await self._llm_json_plan(system=planner_system_pex(), user=user)
        plan = parse_mission_plan(raw, default_parallel=2)
        if plan.subgoals or plan.abandon_ids:
            self._subgoals = merge_subgoals(
                self._subgoals, plan.subgoals, plan.abandon_ids
            )


Swarm = SwarmChainForce
