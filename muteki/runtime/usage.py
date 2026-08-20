"""Tagged, pessimistically accountable usage contracts for Protocol 2."""

from __future__ import annotations

from dataclasses import dataclass
from enum import Enum
from types import MappingProxyType
from typing import Mapping

from muteki.epistemic.contracts import canonical_digest


class UsageNotEstimable(ValueError):
    pass


class UsageStatus(str, Enum):
    OBSERVED = "observed"
    PARTIAL = "partial"
    UNKNOWN = "unknown"


def _amount(value: object, name: str) -> int:
    if type(value) is not int or value < 0:
        raise ValueError(f"{name} must be a non-negative integer")
    return value


def _axis(value: object, name: str = "usage axis") -> str:
    if type(value) is not str or not value or value != value.strip():
        raise ValueError(f"{name} must be a non-empty canonical string")
    return value


def _amount_mapping(values: Mapping[str, int], name: str) -> dict[str, int]:
    if not isinstance(values, Mapping):
        raise TypeError(f"{name} must be a mapping")
    normalized: dict[str, int] = {}
    for key, value in values.items():
        axis = _axis(key, f"{name} axis")
        normalized[axis] = _amount(value, f"{name}[{axis}]")
    return normalized


@dataclass(frozen=True, slots=True)
class UsageMeasurement:
    axis: str
    status: UsageStatus
    observed_so_far: int
    reserved_ceiling: int | None

    def __post_init__(self) -> None:
        object.__setattr__(self, "axis", _axis(self.axis))
        if type(self.status) is not UsageStatus:
            raise TypeError("status must be UsageStatus")
        object.__setattr__(
            self,
            "observed_so_far",
            _amount(self.observed_so_far, "observed_so_far"),
        )
        if self.reserved_ceiling is not None:
            object.__setattr__(
                self,
                "reserved_ceiling",
                _amount(self.reserved_ceiling, "reserved_ceiling"),
            )
        if self.status is not UsageStatus.OBSERVED and self.reserved_ceiling is None:
            raise UsageNotEstimable(
                "partial/unknown usage requires a finite reservation ceiling"
            )

    @property
    def pessimistic_amount(self) -> int:
        if self.status is UsageStatus.OBSERVED:
            return self.observed_so_far
        if self.reserved_ceiling is None:  # guarded in __post_init__
            raise UsageNotEstimable(f"{self.axis} has no finite ceiling")
        return max(self.observed_so_far, self.reserved_ceiling)

    def canonical_body(self) -> dict[str, object]:
        return {
            "axis": self.axis,
            "observed_so_far": self.observed_so_far,
            "reserved_ceiling": self.reserved_ceiling,
            "status": self.status.value,
        }


@dataclass(frozen=True, slots=True)
class UsageReport:
    measurements: tuple[UsageMeasurement, ...]

    def __post_init__(self) -> None:
        if type(self.measurements) is not tuple or any(
            type(item) is not UsageMeasurement for item in self.measurements
        ):
            raise TypeError("measurements must be a built-in tuple of UsageMeasurement")
        axes = tuple(item.axis for item in self.measurements)
        if not axes or axes != tuple(sorted(axes)):
            raise ValueError("usage axes must be complete and canonically sorted")
        if len(set(axes)) != len(axes):
            raise ValueError("usage axes must be unique")

    @classmethod
    def from_observed_and_reservation(
        cls,
        *,
        reserved: Mapping[str, int],
        observed: Mapping[str, int],
        complete_axes: frozenset[str],
    ) -> "UsageReport":
        reserved_copy = _amount_mapping(reserved, "reserved")
        observed_copy = _amount_mapping(observed, "observed")
        if not reserved_copy:
            raise ValueError("reserved usage axes are required")
        if type(complete_axes) is not frozenset or any(
            type(axis) is not str or not axis or axis != axis.strip()
            for axis in complete_axes
        ):
            raise TypeError("complete_axes must be a frozenset of canonical strings")
        if set(observed_copy) - set(reserved_copy):
            raise ValueError("observed usage contains an unreserved axis")
        if set(complete_axes) - set(reserved_copy):
            raise ValueError("complete_axes contains an unreserved axis")
        if not set(complete_axes).issubset(observed_copy):
            raise ValueError("a complete axis must have an observation")
        measurements = []
        for axis in sorted(reserved_copy):
            if axis in complete_axes:
                status = UsageStatus.OBSERVED
            elif axis in observed_copy:
                status = UsageStatus.PARTIAL
            else:
                status = UsageStatus.UNKNOWN
            measurements.append(
                UsageMeasurement(
                    axis=axis,
                    status=status,
                    observed_so_far=observed_copy.get(axis, 0),
                    reserved_ceiling=reserved_copy[axis],
                )
            )
        return cls(tuple(measurements))

    @property
    def has_unknown(self) -> bool:
        return any(item.status is UsageStatus.UNKNOWN for item in self.measurements)

    @property
    def all_observed(self) -> bool:
        return all(item.status is UsageStatus.OBSERVED for item in self.measurements)

    def validate_reservation(self, reserved: Mapping[str, int]) -> None:
        expected = _amount_mapping(reserved, "reserved")
        actual = {item.axis: item.reserved_ceiling for item in self.measurements}
        if set(actual) != set(expected) or any(
            actual[axis] != expected[axis] for axis in expected
        ):
            raise ValueError("usage report does not bind the exact reservation")

    def pessimistic_usage(self) -> MappingProxyType[str, int]:
        return MappingProxyType(
            {item.axis: item.pessimistic_amount for item in self.measurements}
        )

    def canonical_body(self) -> dict[str, object]:
        return {"measurements": [item.canonical_body() for item in self.measurements]}

    @property
    def digest(self) -> str:
        return canonical_digest(self.canonical_body())
