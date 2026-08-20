from __future__ import annotations

import pytest

from muteki.epistemic.sqlite_store import (
    CommandEvent,
    EpistemicSQLiteStore,
    IntegrityError,
    ProjectionMutation,
)
from muteki.runtime.admission import AdmissionRequest, SearchAdmission
from muteki.runtime.contracts import (
    AttemptIdentity,
    EffectClass,
    ExecutionScope,
    LeaseIdentity,
)
from muteki.runtime.controller import BootRecoveryCapability, LiveHealthGuard
from muteki.runtime.usage import UsageNotEstimable, UsageReport, UsageStatus


def _admitted_runtime(tmp_path):
    store = EpistemicSQLiteStore.create(
        path=tmp_path / "runtime.db",
        run_id="run-1",
        manifest_digest="a" * 64,
    )
    store.commit_command(
        command_id="C-ready",
        idempotency_key="ready",
        command_payload={},
        committed_at_ns=1,
        events=[CommandEvent("E-ready", "BOOT_READY", "host", 1)],
    )
    store.commit_command(
        command_id="C-start",
        idempotency_key="start",
        command_payload={},
        committed_at_ns=2,
        events=[
            CommandEvent(
                "E-start",
                "START_EXECUTION",
                "host",
                2,
                {"execution_generation": 1, "run_fence_epoch": 1},
            )
        ],
        projection_mutations=[ProjectionMutation(
            "execution_start_guard",
            {"execution_generation": 1, "run_fence_epoch": 1},
        )],
        authority_capability=store._lifecycle_commit_capability,
    )
    guard = LiveHealthGuard()
    capability = BootRecoveryCapability(1, 1, "owner")
    guard.begin_boot_finalize(capability)
    guard.open_admission(capability=capability, attestation_digest="b" * 64)
    admission = SearchAdmission(store=store, guard=guard)
    admission.create_branch(branch_id="branch", max_attempts=1, occurred_at_ns=3)
    admission.create_budget_account(
        account_id="global",
        limits={"tokens": 100, "wall_ms": 1_000},
        occurred_at_ns=4,
    )
    attempt = AttemptIdentity(ExecutionScope("run-1", 1, 1), "branch", "attempt", 1)
    lease = LeaseIdentity(attempt, "lease", 1, 1)
    admission.admit(
        AdmissionRequest(
            attempt=attempt,
            lease=lease,
            permit_id="permit",
            account_id="global",
            requested_budget={"tokens": 20, "wall_ms": 500},
            conflict_keys=(),
            effect_class=EffectClass.OBSERVABLE,
            fingerprint="fingerprint",
            policy_digest="c" * 64,
            expires_at_ns=10_000,
        ),
        occurred_at_ns=5,
    )
    return store, admission


def _event_count(store):
    return len(store.event_rows())


@pytest.mark.parametrize(
    "actual_usage",
    [
        {"tokens": 3},
        {"tokens": 3, "wall_ms": 10, "cost_micro_usd": 1},
        {"tokens": True, "wall_ms": 10},
        {"tokens": "3", "wall_ms": 10},
        {"tokens": -1, "wall_ms": 10},
    ],
)
def test_compatibility_settlement_fails_closed_before_event(tmp_path, actual_usage):
    store, admission = _admitted_runtime(tmp_path)
    before = _event_count(store)
    with pytest.raises((TypeError, ValueError)):
        admission.settle(
            attempt_id="attempt",
            actual_usage=actual_usage,
            settlement_revision=1,
            occurred_at_ns=6,
        )
    assert _event_count(store) == before


def test_partial_report_is_tagged_and_pessimistically_settled(tmp_path):
    store, admission = _admitted_runtime(tmp_path)
    report = UsageReport.from_observed_and_reservation(
        reserved={"tokens": 20, "wall_ms": 500},
        observed={"tokens": 7, "wall_ms": 120},
        complete_axes=frozenset({"wall_ms"}),
    )
    first = admission.settle(
        attempt_id="attempt",
        usage_report=report,
        settlement_revision=1,
        occurred_at_ns=6,
    )
    event = store.event_rows(kind="BUDGET_SETTLED")[0]["payload"]
    assert event["actual_usage"] == {"tokens": 20, "wall_ms": 120}
    assert event["reservation_ids"] == ["permit:global"]
    assert event["usage_report_digest"] == report.digest
    assert [item["status"] for item in event["usage_report"]["measurements"]] == [
        UsageStatus.PARTIAL.value,
        UsageStatus.OBSERVED.value,
    ]
    # The same command is still a canonical idempotent retry, not a second charge.
    second = admission.settle(
        attempt_id="attempt",
        usage_report=report,
        settlement_revision=1,
        occurred_at_ns=6,
    )
    assert second == first
    assert len(store.event_rows(kind="BUDGET_SETTLED")) == 1


def test_report_must_bind_exact_reservation_ceiling(tmp_path):
    store, admission = _admitted_runtime(tmp_path)
    report = UsageReport.from_observed_and_reservation(
        reserved={"tokens": 21, "wall_ms": 500},
        observed={"tokens": 1, "wall_ms": 10},
        complete_axes=frozenset({"tokens", "wall_ms"}),
    )
    before = _event_count(store)
    with pytest.raises(ValueError, match="exact reservation"):
        admission.settle(
            attempt_id="attempt",
            usage_report=report,
            settlement_revision=1,
            occurred_at_ns=6,
        )
    assert _event_count(store) == before


def test_unknown_report_cannot_settle_and_is_held_with_full_receipt(tmp_path):
    store, admission = _admitted_runtime(tmp_path)
    report = UsageReport.from_observed_and_reservation(
        reserved={"tokens": 20, "wall_ms": 500},
        observed={"wall_ms": 120},
        complete_axes=frozenset({"wall_ms"}),
    )
    with pytest.raises(UsageNotEstimable, match="UNKNOWN usage cannot settle"):
        admission.settle(
            attempt_id="attempt",
            usage_report=report,
            settlement_revision=1,
            occurred_at_ns=6,
        )
    admission.hold_unknown_usage(
        attempt_id="attempt",
        revision=1,
        occurred_at_ns=6,
        usage_report=report,
    )
    event = store.event_rows(kind="BUDGET_USAGE_UNKNOWN")[0]["payload"]
    assert event["held_usage"] == {"tokens": 20, "wall_ms": 120}
    assert event["usage_report_digest"] == report.digest
    assert (
        store._conn.execute(
            "SELECT state FROM runtime_attempts WHERE attempt_id='attempt'"
        ).fetchone()[0]
        == "unknown"
    )
    with pytest.raises(IntegrityError, match="no longer active"):
        admission.settle(
            attempt_id="attempt",
            actual_usage={"tokens": 1, "wall_ms": 120},
            settlement_revision=2,
            occurred_at_ns=7,
        )


def test_default_unknown_hold_never_invents_zero_usage(tmp_path):
    store, admission = _admitted_runtime(tmp_path)
    admission.hold_unknown_usage(attempt_id="attempt", revision=1, occurred_at_ns=6)
    payload = store.event_rows(kind="BUDGET_USAGE_UNKNOWN")[0]["payload"]
    assert payload["held_usage"] == {"tokens": 20, "wall_ms": 500}
    assert {item["status"] for item in payload["usage_report"]["measurements"]} == {
        UsageStatus.UNKNOWN.value
    }


@pytest.mark.parametrize("revision", [0, -1, True, "1"])
def test_settlement_revision_is_exact_positive_integer(tmp_path, revision):
    store, admission = _admitted_runtime(tmp_path)
    before = _event_count(store)
    with pytest.raises(ValueError, match="positive integer"):
        admission.settle(
            attempt_id="attempt",
            actual_usage={"tokens": 1, "wall_ms": 10},
            settlement_revision=revision,
            occurred_at_ns=6,
        )
    assert _event_count(store) == before
