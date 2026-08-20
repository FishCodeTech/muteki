"""Hypothesis-ledger swarm with explicit falsification scheduling.

Planning-layer difference vs production Swarm / SwarmPEX
-------------------------------------------------------
Production: intents are free-text goals without status as scientific claims.
SwarmPEX: plans a subgoal DAG (tasks), not competing world-models.

SwarmHypoLedger: planning unit is a **named falsifiable hypothesis**. The
controller maintains a ledger (open/testing/supported/falsified/abandoned),
schedules discriminating experiments, and abandons falsified branches. Branch
merge happens only when a hypothesis is supported and yields chaining facts.
"""

from __future__ import annotations

import re
from typing import Any

from muteki.solver.planning_kernel_v1 import (
    Hypothesis,
    HypothesisStatus,
    WorkingMemory,
    apply_hypothesis_result,
    default_hypothesis_seed,
    merge_hypotheses,
    parse_hypothesis_plan,
    planner_system_hypo,
    select_hypothesis_tests,
)
from muteki.solver.swarm_planning_base import PlanningSwarmBase


class SwarmHypoLedger(PlanningSwarmBase):
    """Drop-in Swarm replacement: hypothesis ledger architecture."""

    architecture_name = "hypoledger"
    executor_timeout_s = 220
    initial_recon = True
    max_plan_rounds = 22

    def __init__(self, *args: Any, **kwargs: Any) -> None:
        super().__init__(*args, **kwargs)
        self._hypotheses: list[Hypothesis] = []
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
        if force_seed or not self._hypotheses or round_index % 2 == 0:
            await self._refresh_plan(memory, force_seed=force_seed)

        # If we have a supported hypothesis, spawn a follow-up exploit unit.
        supported = [
            h
            for h in self._hypotheses
            if h.status == HypothesisStatus.SUPPORTED
        ]
        chosen = select_hypothesis_tests(
            self._hypotheses, parallel_max=min(2, max(1, free_slots))
        )
        out: list[dict[str, Any]] = []
        timeout = int(min(self.executor_timeout_s, max(60.0, wall_remaining - 20.0)))

        if supported and free_slots > 0 and round_index > 0:
            h = supported[0]
            out.append(
                {
                    "id": f"{h.id}-exploit",
                    "goal": (
                        f"HYPOTHESIS SUPPORTED: {h.claim}. "
                        f"Exploit this to recover flag{{...}} from real output. "
                        f"Prior evidence: {'; '.join(h.evidence_for[:2])}"
                    ),
                    "mode": "explore",
                    "timeout": timeout,
                    "meta": {"kind": "exploit", "hyp_id": h.id},
                }
            )
            free_slots -= 1

        for hyp in chosen:
            if free_slots <= 0:
                break
            hyp.status = HypothesisStatus.TESTING
            out.append(
                {
                    "id": hyp.id,
                    "goal": (
                        f"TEST HYPOTHESIS [{hyp.id}]: {hyp.claim}\n"
                        f"EXPERIMENT: {hyp.test}\n"
                        "End by stating VERDICT: supported|falsified|inconclusive "
                        "and one evidence sentence. Write verified facts to the board."
                    ),
                    "mode": "explore",
                    "timeout": timeout,
                    "meta": {"kind": "test", "hyp_id": hyp.id},
                }
            )
            free_slots -= 1
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
        meta = unit.get("meta") or {}
        if meta.get("kind") == "exploit":
            if found_flag or getattr(outcome, "solved", False):
                memory.add_fact(f"exploit succeeded via {meta.get('hyp_id')}")
            elif not new_facts:
                memory.add_dead_end(
                    f"exploit failed for {meta.get('hyp_id')}: "
                    f"{str(getattr(outcome, 'reason', ''))[:120]}"
                )
            return
        hid = str(meta.get("hyp_id") or unit.get("id") or "")
        hyp = next((h for h in self._hypotheses if h.id == hid), None)
        if hyp is None:
            return
        reason = str(getattr(outcome, "reason", "") or "")
        # Prefer explicit VERDICT from worker text if present in reason/facts.
        blob = reason + "\n" + "\n".join(new_facts)
        verdict = _extract_verdict(blob)
        if found_flag:
            verdict = "supported"
        elif new_facts and verdict == "inconclusive":
            # weak positive: facts without falsification
            verdict = "supported"
        elif not new_facts and not found_flag:
            # First barren attempt stays inconclusive; a second barren attempt
            # falsifies (attempts not yet incremented — apply_hypothesis_result
            # does that).
            verdict = "falsified" if hyp.attempts >= 1 else "inconclusive"
        apply_hypothesis_result(
            memory,
            hyp,
            verdict=verdict,
            evidence=reason or (new_facts[0] if new_facts else "no evidence"),
            new_facts=new_facts,
        )

    async def _refresh_plan(
        self, memory: WorkingMemory, *, force_seed: bool = False
    ) -> None:
        self._plan_calls += 1
        seed = default_hypothesis_seed(
            self.challenge.category, self.challenge.description or ""
        )
        if force_seed and not self._hypotheses:
            self._hypotheses = list(seed.hypotheses)
            return
        ledger = _render_hypo_ledger(self._hypotheses)
        user = (
            f"{memory.render()}\n\n"
            f"hypothesis_ledger:\n{ledger}\n\n"
            "Update the ledger as JSON. Prefer new discriminating hypotheses "
            "when current open set is empty or all abandoned."
        )
        raw = await self._llm_json_plan(system=planner_system_hypo(), user=user)
        plan = parse_hypothesis_plan(raw, default_parallel=2)
        if not plan.hypotheses and not self._hypotheses:
            plan = seed
        if plan.hypotheses or plan.abandon_ids:
            self._hypotheses = merge_hypotheses(
                self._hypotheses, plan.hypotheses, plan.abandon_ids
            )
        if not self._hypotheses:
            self._hypotheses = list(seed.hypotheses)


def _render_hypo_ledger(hyps: list[Hypothesis]) -> str:
    if not hyps:
        return "(empty)"
    lines = []
    for h in hyps:
        lines.append(
            f"- {h.id} [{h.status.value}] attempts={h.attempts}: {h.claim[:140]} "
            f"| test: {h.test[:100]}"
        )
    return "\n".join(lines)


_VERDICT_RE = re.compile(
    r"VERDICT\s*[:=]\s*(supported|falsified|inconclusive)",
    re.IGNORECASE,
)


def _extract_verdict(text: str) -> str:
    m = _VERDICT_RE.search(text or "")
    if m:
        return m.group(1).lower()
    low = (text or "").lower()
    if "falsif" in low or "ruled out" in low or "not the case" in low:
        return "falsified"
    if "support" in low or "confirmed" in low:
        return "supported"
    return "inconclusive"


Swarm = SwarmHypoLedger
