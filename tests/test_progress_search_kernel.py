from __future__ import annotations

import pytest

from muteki.epistemic.sqlite_store import CommandEvent, EpistemicSQLiteStore, IntegrityError
from muteki.runtime.admission import AdmissionRequest, SearchAdmission
from muteki.runtime.contracts import (
    AttemptIdentity, EffectClass, ExecutionScope, LeaseIdentity,
)
from muteki.runtime.controller import BootRecoveryCapability, LiveHealthGuard
from muteki.runtime.progress import (
    ProgressKind, ProgressLedger, ProgressOccurrence, ProgressProjection,
)
from muteki.runtime.search import FailureKind, LoopContract, RecoveryAction, SearchKernel


def test_candidate_and_activity_do_not_clear_branch_stagnation():
    projection = ProgressProjection().mark_attempt_barren("b1")
    for index, kind in enumerate((ProgressKind.ACTIVITY, ProgressKind.CANDIDATE), start=1):
        projection = projection.apply(ProgressOccurrence(
            f"o{index}", "b1", "a1", kind, f"d{index}", index))
    assert projection.branches["b1"].barren_attempts == 1
    projection = projection.apply(ProgressOccurrence(
        "o3", "b1", "a1", ProgressKind.INFORMATION, "d3", 3))
    assert projection.branches["b1"].barren_attempts == 0


def test_unrelated_branch_information_does_not_clear_other_branch():
    projection = ProgressProjection().mark_attempt_barren("b1")
    projection = projection.apply(ProgressOccurrence(
        "o1", "b2", "a2", ProgressKind.INFORMATION, "d", 1))
    assert projection.branches["b1"].barren_attempts == 1
    assert projection.branches["b2"].information_head == 1


def test_goal_units_are_distinct_and_occurrences_dedupe():
    projection = ProgressProjection(expected_goal_units=2)
    first = ProgressOccurrence("o1", "b1", "a1", ProgressKind.GOAL_UNIT,
                               "d1", 1, "flag-one")
    projection = projection.apply(first).apply(first)
    projection = projection.apply(ProgressOccurrence(
        "o2", "b1", "a1", ProgressKind.GOAL_UNIT, "d2", 2, "flag-one"))
    assert not projection.goal_complete
    projection = projection.apply(ProgressOccurrence(
        "o3", "b1", "a1", ProgressKind.GOAL_UNIT, "d3", 3, "flag-two"))
    assert projection.goal_complete


def test_loop_contract_child_cannot_expand_parent():
    parent = LoopContract(3, 2, 1000)
    assert parent.child(max_attempts=2, max_barren_attempts=1, max_wall_ms=900)
    with pytest.raises(ValueError):
        parent.child(max_attempts=4, max_barren_attempts=1, max_wall_ms=900)


def test_failure_attribution_does_not_close_on_infra_or_unknown():
    kernel = SearchKernel.__new__(SearchKernel)
    assert kernel.failure_action(branch_id="b1", failure=FailureKind.INFRA_FAILURE) \
        is RecoveryAction.PAUSE_INFRA
    assert kernel.failure_action(branch_id="b1", failure=FailureKind.UNKNOWN) \
        is RecoveryAction.HOLD_RECONCILIATION
    assert kernel.failure_action(branch_id="b1", failure=FailureKind.HYPOTHESIS_REFUTED) \
        is RecoveryAction.CLOSE_BRANCH


def test_progress_ledger_rebuilds_from_canonical_events(tmp_path):
    store = EpistemicSQLiteStore.create(
        path=tmp_path / "progress.db", run_id="run-1", manifest_digest="a" * 64)
    ledger = ProgressLedger(store=store)
    occurrence = ProgressOccurrence(
        "o1", "b1", "a1", ProgressKind.INFORMATION, "d" * 64, 7)
    first = ledger.record(occurrence, occurred_at_ns=1)
    second = ledger.record(occurrence, occurred_at_ns=2)
    assert first == second
    rebuilt = ProgressLedger(store=store)
    assert rebuilt.projection.branches["b1"].information_head == 7
    assert rebuilt.projection.branches["b1"].information_count == 1


def test_barren_attempt_is_canonical_and_rebuildable(tmp_path):
    store = EpistemicSQLiteStore.create(
        path=tmp_path / "barren.db", run_id="run-1", manifest_digest="a" * 64)
    ledger = ProgressLedger(store=store)
    first = ledger.mark_attempt_barren(
        branch_id="b1", attempt_id="a1", occurred_at_ns=1)
    second = ledger.mark_attempt_barren(
        branch_id="b1", attempt_id="a1", occurred_at_ns=2)
    assert first == second
    rebuilt = ProgressLedger(store=store)
    assert rebuilt.projection.branches["b1"].barren_attempts == 1
