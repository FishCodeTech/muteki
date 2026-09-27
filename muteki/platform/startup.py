"""Web 产品启动恢复的统一报告契约。"""

from __future__ import annotations

from typing import Any

from pydantic import Field

from muteki.platform.contracts.base import ContractModel, utcnow


class StartupRecoveryStep(ContractModel):
    name: str
    state: str = "unavailable"  # ready/degraded/unavailable
    core: bool = False
    detail: str = ""
    impact: str = ""
    evidence: dict[str, Any] = Field(default_factory=dict)


class StartupRecoveryReport(ContractModel):
    started_at: Any = Field(default_factory=utcnow)
    completed_at: Any = None
    ready: bool = False
    state: str = "unavailable"
    steps: list[StartupRecoveryStep] = Field(default_factory=list)

    def add(
        self,
        name: str,
        state: str,
        *,
        core: bool = False,
        detail: str = "",
        impact: str = "",
        evidence: dict[str, Any] | None = None,
    ) -> None:
        self.steps.append(StartupRecoveryStep(
            name=name,
            state=state,
            core=core,
            detail=detail,
            impact=impact,
            evidence=dict(evidence or {}),
        ))

    def finish(self) -> "StartupRecoveryReport":
        core_ready = all(
            step.state == "ready" for step in self.steps if step.core)
        any_degraded = any(
            step.state != "ready" for step in self.steps if not step.core)
        self.ready = core_ready
        self.state = (
            "unavailable" if not core_ready else
            "degraded" if any_degraded else
            "ready"
        )
        self.completed_at = utcnow()
        return self


__all__ = ["StartupRecoveryReport", "StartupRecoveryStep"]
