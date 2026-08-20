from __future__ import annotations

import re
from dataclasses import dataclass
from typing import Any

from muteki.epistemic.contracts import canonical_digest


EVAL_CONTRACT_VERSION = 2
_SHA256_RE = re.compile(r"^[0-9a-f]{64}$")


def _exact_text(value: object, name: str) -> str:
    if type(value) is not str:
        raise TypeError(f"{name} must be a string")
    if not value or value != value.strip():
        raise ValueError(f"{name} must be non-empty canonical text")
    return value


def _lower_sha256(value: object, name: str, *, allow_empty: bool = False) -> str:
    if type(value) is not str:
        raise TypeError(f"{name} must be a string")
    if allow_empty and not value:
        return value
    if not _SHA256_RE.fullmatch(value):
        raise ValueError(f"{name} must be a lowercase sha256 digest")
    return value


def _dimension_tuple(
    value: object, name: str, *, allow_empty: bool
) -> tuple[tuple[str, int], ...]:
    if type(value) is not tuple:
        raise TypeError(f"{name} must be a built-in tuple")
    if not value and not allow_empty:
        raise ValueError(f"{name} must not be empty")
    validated: list[tuple[str, int]] = []
    for index, item in enumerate(value):
        if type(item) is not tuple or len(item) != 2:
            raise TypeError(f"{name}[{index}] must be a two-item built-in tuple")
        axis = _exact_text(item[0], f"{name}[{index}].axis")
        amount = item[1]
        if type(amount) is not int or amount < 0:
            raise ValueError(f"{name}[{index}].amount must be a non-negative integer")
        validated.append((axis, amount))
    axes = tuple(axis for axis, _ in validated)
    if len(axes) != len(set(axes)):
        raise ValueError(f"{name} axes must be unique")
    if axes != tuple(sorted(axes)):
        raise ValueError(f"{name} axes must be in canonical sorted order")
    return tuple(validated)


@dataclass(frozen=True, slots=True)
class TrialIdentity:
    study_id: str
    trial_id: str
    intention_id: str
    protocol_version: int

    def __post_init__(self) -> None:
        for name in ("study_id", "trial_id", "intention_id"):
            object.__setattr__(self, name, _exact_text(getattr(self, name), name))
        if type(self.protocol_version) is not int or self.protocol_version <= 0:
            raise ValueError("protocol_version must be a positive integer")

    @property
    def digest(self) -> str:
        return canonical_digest(
            {
                "contract_version": EVAL_CONTRACT_VERSION,
                "intention_id": self.intention_id,
                "protocol_version": self.protocol_version,
                "study_id": self.study_id,
                "trial_id": self.trial_id,
            }
        )


@dataclass(frozen=True, slots=True)
class TrialAssignment:
    identity: TrialIdentity
    challenge_id: str
    arm: str
    pair_id: str
    budget: tuple[tuple[str, int], ...]
    policy_digest: str

    def __post_init__(self) -> None:
        if type(self.identity) is not TrialIdentity:
            raise TypeError("identity must be TrialIdentity")
        for name in ("challenge_id", "arm", "pair_id"):
            object.__setattr__(self, name, _exact_text(getattr(self, name), name))
        object.__setattr__(
            self,
            "budget",
            _dimension_tuple(self.budget, "budget", allow_empty=False),
        )
        object.__setattr__(
            self,
            "policy_digest",
            _lower_sha256(self.policy_digest, "policy_digest"),
        )

    def as_dict(self) -> dict[str, Any]:
        return {
            "arm": self.arm,
            "budget": dict(self.budget),
            "challenge_id": self.challenge_id,
            "identity_digest": self.identity.digest,
            "pair_id": self.pair_id,
            "policy_digest": self.policy_digest,
        }

    @property
    def digest(self) -> str:
        """Canonical digest that receipts must bind to exactly."""

        return canonical_digest(self.as_dict())


@dataclass(frozen=True, slots=True)
class EvalStudyManifest:
    study_id: str
    assignments: tuple[TrialAssignment, ...]
    oracle_policy_digest: str
    contract_version: int = EVAL_CONTRACT_VERSION

    def __post_init__(self) -> None:
        object.__setattr__(self, "study_id", _exact_text(self.study_id, "study_id"))
        if (
            type(self.contract_version) is not int
            or self.contract_version != EVAL_CONTRACT_VERSION
        ):
            raise ValueError("unsupported evaluation contract version")
        if type(self.assignments) is not tuple or any(
            type(assignment) is not TrialAssignment for assignment in self.assignments
        ):
            raise TypeError("assignments must be a built-in tuple of TrialAssignment")
        object.__setattr__(
            self,
            "oracle_policy_digest",
            _lower_sha256(self.oracle_policy_digest, "oracle_policy_digest"),
        )
        ids = [assignment.identity.trial_id for assignment in self.assignments]
        intentions = [
            assignment.identity.intention_id for assignment in self.assignments
        ]
        if not self.assignments or len(ids) != len(set(ids)):
            raise ValueError("trial assignments must be pre-enumerated and unique")
        if len(intentions) != len(set(intentions)):
            raise ValueError("intention identities must be unique")
        if any(
            assignment.identity.study_id != self.study_id
            for assignment in self.assignments
        ):
            raise ValueError("cross-study assignment")
        self._validate_paired_two_arm_shape()

    def _pair_groups(self) -> dict[str, tuple[TrialAssignment, ...]]:
        groups: dict[str, list[TrialAssignment]] = {}
        for assignment in self.assignments:
            groups.setdefault(assignment.pair_id, []).append(assignment)
        return {pair_id: tuple(group) for pair_id, group in groups.items()}

    def _validate_paired_two_arm_shape(self) -> None:
        groups = self._pair_groups()
        if not any(len(group) > 1 for group in groups.values()):
            return
        if any(len(group) != 2 for group in groups.values()):
            raise ValueError(
                "paired manifests require exactly two assignments per pair_id"
            )
        arms = frozenset(assignment.arm for assignment in self.assignments)
        if len(arms) != 2:
            raise ValueError("paired manifests require exactly two study arms")
        for pair_id, group in groups.items():
            if frozenset(assignment.arm for assignment in group) != arms:
                raise ValueError(
                    f"pair {pair_id!r} must contain one assignment from each arm"
                )
            first, second = group
            if first.challenge_id != second.challenge_id:
                raise ValueError(f"pair {pair_id!r} must bind the same challenge_id")
            if first.identity.protocol_version != second.identity.protocol_version:
                raise ValueError(
                    f"pair {pair_id!r} must bind the same protocol_version"
                )
            if first.budget != second.budget:
                raise ValueError(f"pair {pair_id!r} must bind the same frozen budget")
            if first.policy_digest != second.policy_digest:
                raise ValueError(f"pair {pair_id!r} must bind the same policy_digest")

    @property
    def is_paired_two_arm(self) -> bool:
        return any(len(group) > 1 for group in self._pair_groups().values())

    @property
    def smoke_only(self) -> bool:
        """Singleton pair IDs are allowed for smoke/canary accounting only."""

        return not self.is_paired_two_arm

    @property
    def digest(self) -> str:
        return canonical_digest(
            {
                "assignments": [
                    assignment.as_dict() for assignment in self.assignments
                ],
                "contract_version": self.contract_version,
                "oracle_policy_digest": self.oracle_policy_digest,
                "study_id": self.study_id,
            }
        )

    def assignment(self, trial_id: str) -> TrialAssignment:
        trial_id = _exact_text(trial_id, "trial_id")
        for assignment in self.assignments:
            if assignment.identity.trial_id == trial_id:
                return assignment
        raise KeyError(trial_id)
