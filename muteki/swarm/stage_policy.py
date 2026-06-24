"""First-class stage policy for the two-stage swarm coordinator."""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any, Optional


@dataclass(frozen=True)
class BudgetPolicy:
    max_total_workers: Optional[int] = None
    cost_budget_usd: Optional[float] = None


@dataclass(frozen=True)
class StagePolicy:
    prepare: dict[str, Any] = field(default_factory=dict)
    race: dict[str, Any] = field(default_factory=dict)
    # the main multi-phase solve loop's policy (wall_clock_budget etc).
    coordinator: dict[str, Any] = field(default_factory=dict)
    budgets: BudgetPolicy = field(default_factory=BudgetPolicy)

    @staticmethod
    def from_config(value: Any) -> "StagePolicy":
        if isinstance(value, StagePolicy):
            return value
        raw = value if isinstance(value, dict) else {}
        budgets = raw.get("budgets") if isinstance(raw.get("budgets"), dict) else {}
        max_total = budgets.get("max_total_workers")
        cost_budget = budgets.get("cost_budget_usd")
        return StagePolicy(
            prepare=dict(raw.get("prepare") or {}),
            race=dict(raw.get("race") or {}),
            coordinator=dict(raw.get("coordinator") or {}),
            budgets=BudgetPolicy(
                max_total_workers=(int(max_total) if max_total not in (None, "", 0) else None),
                cost_budget_usd=(float(cost_budget) if cost_budget not in (None, "", 0) else None),
            ),
        )

    def model_dump(self) -> dict[str, Any]:
        return {
            "prepare": dict(self.prepare),
            "race": dict(self.race),
            "coordinator": dict(self.coordinator),
            "budgets": {
                "max_total_workers": self.budgets.max_total_workers,
                "cost_budget_usd": self.budgets.cost_budget_usd,
            },
        }
