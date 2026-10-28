"""One finite execution deadline shared by calls and their finalization tail."""

from __future__ import annotations

import math
import time
from dataclasses import dataclass
from datetime import datetime, timedelta, timezone
from typing import Any, Mapping


class ExecutionBudgetExpired(RuntimeError):
    """No positive lease remains for the requested action."""

    code = "execution_budget_exhausted"


def _seconds(value: Any, name: str) -> float:
    if isinstance(value, bool):
        raise ValueError(f"{name} must be finite nonnegative seconds")
    try:
        number = float(value)
    except (TypeError, ValueError) as exc:
        raise ValueError(f"{name} must be finite nonnegative seconds") from exc
    if not math.isfinite(number) or number < 0:
        raise ValueError(f"{name} must be finite nonnegative seconds")
    return number


@dataclass(frozen=True, slots=True)
class BudgetSlices:
    execution_seconds: float
    conclude_seconds: float
    finalization_seconds: float


@dataclass(frozen=True, slots=True)
class ExecutionBudget:
    """A monotonic lease with a UTC deadline for process recovery.

    Zero is an exhausted budget. It never represents an unlimited timeout.
    Call limits are positive floats; callers must not round them up or pass an
    exhausted lease as zero to runtimes that interpret zero as unlimited.
    """

    deadline_monotonic: float
    deadline_utc: datetime
    conclude_seconds: float = 0.0
    finalization_seconds: float = 0.0

    def __post_init__(self) -> None:
        if not math.isfinite(self.deadline_monotonic):
            raise ValueError("deadline_monotonic must be finite")
        if self.deadline_utc.utcoffset() is None:
            raise ValueError("deadline_utc must include a timezone")
        object.__setattr__(self, "deadline_utc", self.deadline_utc.astimezone(timezone.utc))
        object.__setattr__(
            self, "conclude_seconds", _seconds(self.conclude_seconds, "conclude_seconds"),
        )
        object.__setattr__(
            self, "finalization_seconds",
            _seconds(self.finalization_seconds, "finalization_seconds"),
        )

    @classmethod
    def start(
        cls, total_seconds: float, *, conclude_seconds: float = 0.0,
        finalization_seconds: float = 0.0,
    ) -> ExecutionBudget:
        seconds = _seconds(total_seconds, "total_seconds")
        now_monotonic = time.monotonic()
        now_utc = datetime.now(timezone.utc)
        return cls(
            deadline_monotonic=now_monotonic + seconds,
            deadline_utc=now_utc + timedelta(seconds=seconds),
            conclude_seconds=conclude_seconds,
            finalization_seconds=finalization_seconds,
        )

    def remaining_seconds(self, *, reserve_seconds: float = 0.0) -> float:
        reserve = _seconds(reserve_seconds, "reserve_seconds")
        return max(0.0, self.deadline_monotonic - time.monotonic() - reserve)

    @property
    def expired(self) -> bool:
        return self.remaining_seconds() <= 0.0

    def require_action(self, *, reserve_seconds: float = 0.0) -> float:
        """Admit an action only when a positive lease survives its reserved tail."""
        remaining = self.remaining_seconds(reserve_seconds=reserve_seconds)
        if remaining <= 0.0:
            raise ExecutionBudgetExpired("No execution time remains for this action")
        return remaining

    def call_limit_seconds(
        self, max_seconds: float | None = None, *, reserve_seconds: float = 0.0,
    ) -> float:
        """Clamp one call to the live deadline; raise instead of returning zero."""
        remaining = self.require_action(reserve_seconds=reserve_seconds)
        limit = (
            remaining if max_seconds is None
            else min(remaining, _seconds(max_seconds, "max_seconds"))
        )
        if limit <= 0.0:
            raise ExecutionBudgetExpired("No execution time was allocated to this call")
        return limit

    def split_tail(self) -> BudgetSlices:
        """Reserve finalization first, then conclusion, within the current lease."""
        remaining = self.remaining_seconds()
        finalization = min(remaining, self.finalization_seconds)
        conclude = min(max(0.0, remaining - finalization), self.conclude_seconds)
        return BudgetSlices(
            execution_seconds=max(0.0, remaining - finalization - conclude),
            conclude_seconds=conclude,
            finalization_seconds=finalization,
        )

    def to_record(self) -> dict[str, Any]:
        """Store no monotonic timestamp, which cannot survive a host restart."""
        return {
            "version": 1,
            "deadline_utc": self.deadline_utc.isoformat(),
            "remaining_seconds": self.remaining_seconds(),
            "conclude_seconds": self.conclude_seconds,
            "finalization_seconds": self.finalization_seconds,
        }

    @classmethod
    def from_record(cls, record: Mapping[str, Any]) -> ExecutionBudget:
        """Restore time left, never the original duration or more than saved.

        UTC charges downtime. The saved remaining time caps recovery if the wall
        clock has moved backwards; a new monotonic lease then owns runtime time.
        """
        if record.get("version") != 1:
            raise ValueError("Unsupported execution budget record version")
        try:
            deadline = datetime.fromisoformat(str(record["deadline_utc"]).replace("Z", "+00:00"))
            saved_remaining = _seconds(record["remaining_seconds"], "remaining_seconds")
            conclude = _seconds(record["conclude_seconds"], "conclude_seconds")
            finalization = _seconds(record["finalization_seconds"], "finalization_seconds")
        except KeyError as exc:
            raise ValueError(f"Execution budget record is missing {exc.args[0]}") from exc
        if deadline.utcoffset() is None:
            raise ValueError("deadline_utc must include a timezone")
        now_monotonic = time.monotonic()
        now_utc = datetime.now(timezone.utc)
        remaining = min(saved_remaining, max(0.0, (deadline - now_utc).total_seconds()))
        return cls(
            deadline_monotonic=now_monotonic + remaining,
            deadline_utc=deadline,
            conclude_seconds=conclude,
            finalization_seconds=finalization,
        )
