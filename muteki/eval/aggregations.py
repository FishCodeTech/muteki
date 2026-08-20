from __future__ import annotations

from collections import Counter
from dataclasses import dataclass
from typing import Iterable

from muteki.eval.manifests import EvalStudyManifest, TrialAssignment
from muteki.eval.receipts import EvalTrialReceipt, TrialOutcome


@dataclass(frozen=True, slots=True)
class StudyAggregation:
    intention_count: int
    receipt_count: int
    strict_valid_count: int
    solved_intention_count: int
    missingness: tuple[tuple[str, int], ...]
    by_arm: tuple[tuple[str, int, int], ...]
    paired_two_arm: bool

    @property
    def smoke_only(self) -> bool:
        return not self.paired_two_arm

    @property
    def is_promotion_evidence(self) -> bool:
        """Generic caller-supplied aggregates are never promotion authority."""

        return False


def _validate_receipt_binding(
    manifest: EvalStudyManifest,
    assignment: TrialAssignment,
    receipt: EvalTrialReceipt,
) -> None:
    if receipt.contract_version != manifest.contract_version:
        raise ValueError("receipt contract version does not match manifest")
    if receipt.identity != assignment.identity:
        raise ValueError("receipt identity does not match trial assignment")
    if receipt.assignment_digest != assignment.digest:
        raise ValueError("receipt assignment_digest does not match trial assignment")

    budget = dict(assignment.budget)
    usage = dict(receipt.usage.dimensions)
    unknown_axes = tuple(sorted(set(usage) - set(budget)))
    if unknown_axes:
        raise ValueError(
            f"receipt usage contains unknown frozen-budget axes: {unknown_axes!r}"
        )
    if receipt.usage.complete and tuple(usage) != tuple(budget):
        raise ValueError(
            "complete receipt usage axes must exactly match frozen budget axes"
        )
    over_budget = tuple(axis for axis, amount in usage.items() if amount > budget[axis])
    if over_budget:
        raise ValueError(f"receipt usage exceeds frozen budget axes: {over_budget!r}")


def aggregate_study(
    manifest: EvalStudyManifest, receipts: Iterable[EvalTrialReceipt]
) -> StudyAggregation:
    if type(manifest) is not EvalStudyManifest:
        raise TypeError("manifest must be EvalStudyManifest")

    assignments = {
        assignment.identity.trial_id: assignment for assignment in manifest.assignments
    }
    by_trial: dict[str, EvalTrialReceipt] = {}
    for receipt in receipts:
        if type(receipt) is not EvalTrialReceipt:
            raise TypeError("receipts must contain only EvalTrialReceipt")
        trial_id = receipt.identity.trial_id
        if trial_id in by_trial:
            raise ValueError(f"duplicate receipt for trial_id {trial_id!r}")
        assignment = assignments.get(trial_id)
        if assignment is None:
            raise ValueError(f"receipt for unknown trial_id {trial_id!r}")
        _validate_receipt_binding(manifest, assignment, receipt)
        by_trial[trial_id] = receipt

    missing = Counter()
    arms: dict[str, list[int]] = {}
    strict_valid = 0
    solved = 0
    for assignment in manifest.assignments:
        receipt = by_trial.get(assignment.identity.trial_id)
        arm = arms.setdefault(assignment.arm, [0, 0])
        arm[0] += 1
        if receipt is None:
            missing["missing_receipt"] += 1
            continue
        missing[receipt.missingness.value] += 1
        if receipt.strict_valid:
            strict_valid += 1
            if receipt.outcome is TrialOutcome.SOLVED:
                solved += 1
                arm[1] += 1
    return StudyAggregation(
        intention_count=len(manifest.assignments),
        receipt_count=len(by_trial),
        strict_valid_count=strict_valid,
        solved_intention_count=solved,
        missingness=tuple(sorted(missing.items())),
        by_arm=tuple(
            sorted((name, values[0], values[1]) for name, values in arms.items())
        ),
        paired_two_arm=manifest.is_paired_two_arm,
    )
