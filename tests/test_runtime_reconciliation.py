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
from muteki.runtime.controller import (
    AuthorityDenied,
    BootRecoveryCapability,
    LiveHealthGuard,
)
from muteki.runtime.permit_resolver import (
    CanonicalPermitResolver,
    PermitResolutionError,
)
from muteki.runtime.reconciliation import (
    OrphanReconciler,
    ReconciliationDisposition,
    WorkerLifecycleState,
)


def _runtime(tmp_path):
    store = EpistemicSQLiteStore.create(
        path=tmp_path / "reconciliation.db",
        run_id="run-1",
        manifest_digest="a" * 64,
    )
    store.commit_command(
        command_id="ready",
        idempotency_key="ready",
        command_payload={},
        events=[CommandEvent("event:ready", "BOOT_READY", "host", 1)],
        committed_at_ns=1,
    )
    store.commit_command(
        command_id="start",
        idempotency_key="start",
        command_payload={},
        events=[
            CommandEvent(
                "event:start",
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
        committed_at_ns=2,
    )
    guard = LiveHealthGuard()
    capability = BootRecoveryCapability(1, 1, "owner")
    guard.begin_boot_finalize(capability)
    guard.open_admission(capability=capability, attestation_digest="b" * 64)
    admission = SearchAdmission(store=store, guard=guard)
    admission.create_branch(branch_id="branch-1", max_attempts=8, occurred_at_ns=3)
    admission.create_budget_account(
        account_id="global",
        limits={"attempts": 8, "tokens": 800},
        occurred_at_ns=4,
    )
    return store, admission, ExecutionScope("run-1", 1, 1)


def _admit(admission, scope, *, attempt_id="attempt-1", permit_id="permit-1"):
    attempt = AttemptIdentity(scope, "branch-1", attempt_id, 1)
    lease = LeaseIdentity(attempt, f"lease-{attempt_id}", 1, 1)
    return admission.admit(
        AdmissionRequest(
            attempt=attempt,
            lease=lease,
            permit_id=permit_id,
            account_id="global",
            requested_budget={"attempts": 1, "tokens": 100},
            conflict_keys=(f"resource:{attempt_id}",),
            effect_class=EffectClass.PURE,
            fingerprint=f"fingerprint:{attempt_id}",
            policy_digest="c" * 64,
            expires_at_ns=1_000,
        ),
        occurred_at_ns=5,
    )


def _launch(store, scope, permit, *, now_ns=10):
    return CanonicalPermitResolver(store=store, scope=scope).claim_launch(
        permit, now_ns=now_ns
    )


def _reconciler(store):
    guard = LiveHealthGuard()
    guard.begin_boot_finalize(BootRecoveryCapability(2, 2, "recovery-owner"))
    return OrphanReconciler(store=store, guard=guard)


def _append_terminal(store, permit_id, *, suffix="one", kind="WORKER_TERMINAL"):
    admission = next(
        row for row in store.event_rows(kind="ATTEMPT_ADMITTED")
        if row["payload"].get("permit_id") == permit_id
    )
    launch = next(
        row for row in store.event_rows(kind="WORKER_LAUNCH_PREPARED")
        if row["payload"].get("permit_id") == permit_id
    )
    admitted = admission["payload"]
    outcome = "unknown" if kind == "WORKER_UNKNOWN" else "observed"
    event_id = f"event:manual-terminal:{suffix}"
    payload = {
        "admission_event_digest": admission["event_digest"],
        "attempt_digest": admitted["attempt_digest"],
        "attempt_id": admitted["attempt_id"],
        "launch_event_digest": launch["event_digest"],
        "lease_digest": admitted["lease_digest"],
        "lease_id": admitted["lease_id"],
        "outcome": outcome,
        "permit_digest": admitted["permit_digest"],
        "permit_id": permit_id,
        "scope_digest": admitted["scope_digest"],
    }
    store.commit_command(
        command_id=f"manual-terminal:{suffix}",
        idempotency_key=f"manual-terminal:{suffix}",
        command_payload=payload,
        events=[
            CommandEvent(
                event_id,
                kind,
                "test-host",
                20,
                payload,
            )
        ],
        projection_mutations=[ProjectionMutation(
            "worker_terminal_guard",
            {**payload, "terminal_event_id": event_id},
        )],
        committed_at_ns=20,
    )


def test_not_launched_is_held_without_release_or_synthetic_terminal(tmp_path):
    store, admission, scope = _runtime(tmp_path)
    permit = _admit(admission, scope)
    reconciler = _reconciler(store)

    lifecycle = reconciler.classify(permit.permit_id)
    assert lifecycle.state is WorkerLifecycleState.NOT_LAUNCHED
    plan = reconciler.plan(permit.permit_id)
    assert plan.disposition is ReconciliationDisposition.HOLD_NOT_LAUNCHED
    assert plan.command_id is None

    before_events = store.event_rows()
    before_owners = store.lifecycle_owner_summary()
    outcome = reconciler.reconcile(permit.permit_id, occurred_at_ns=30)
    assert outcome.receipt_digest is None
    assert outcome.lifecycle_after == lifecycle
    assert store.event_rows() == before_events
    assert store.lifecycle_owner_summary() == before_owners


def test_reconciliation_mutation_requires_boot_scoped_recovery_authority(tmp_path):
    store, admission, scope = _runtime(tmp_path)
    permit = _admit(admission, scope)
    _launch(store, scope, permit)
    event_count = len(store.event_rows())

    with pytest.raises(AuthorityDenied, match="boot-scoped recovery guard"):
        OrphanReconciler(store=store).reconcile(
            permit.permit_id, occurred_at_ns=30
        )
    assert len(store.event_rows()) == event_count


def test_in_flight_orphan_atomically_becomes_unknown_and_keeps_holds(tmp_path):
    store, admission, scope = _runtime(tmp_path)
    permit = _admit(admission, scope)
    _launch(store, scope, permit)
    reconciler = _reconciler(store)

    before = reconciler.classify(permit.permit_id)
    assert before.state is WorkerLifecycleState.IN_FLIGHT_ORPHAN
    first_plan = reconciler.plan(permit.permit_id)
    assert first_plan.disposition is ReconciliationDisposition.MARK_UNKNOWN
    assert first_plan.command_id == first_plan.idempotency_key

    reopened = EpistemicSQLiteStore.open(store.path)
    try:
        restarted_plan = OrphanReconciler(store=reopened).plan(permit.permit_id)
        assert restarted_plan == first_plan
    finally:
        reopened.close()

    outcome = reconciler.reconcile(permit.permit_id, occurred_at_ns=30)
    assert outcome.receipt_digest
    assert outcome.lifecycle_after.state is WorkerLifecycleState.TERMINAL
    assert outcome.lifecycle_after.terminal_kind == "WORKER_UNKNOWN"
    assert outcome.lifecycle_after.accounting_complete is True
    assert len(store.event_rows(kind="WORKER_UNKNOWN")) == 1
    assert len(store.event_rows(kind="BUDGET_USAGE_UNKNOWN")) == 1

    attempt_state = store._conn.execute(
        "SELECT state FROM runtime_attempts WHERE attempt_id='attempt-1'"
    ).fetchone()[0]
    reservation_states = store._conn.execute(
        "SELECT state FROM budget_reservations WHERE attempt_id='attempt-1'"
    ).fetchall()
    conflict_count = store._conn.execute(
        "SELECT COUNT(*) FROM effect_conflict_holds "
        "WHERE operation_id='attempt-1' AND state='active'"
    ).fetchone()[0]
    assert attempt_state == "unknown"
    assert {row[0] for row in reservation_states} == {"unknown"}
    assert conflict_count == 1

    before_retry = store.event_rows()
    second = reconciler.reconcile(permit.permit_id, occurred_at_ns=99)
    assert second.plan.disposition is ReconciliationDisposition.ALREADY_TERMINAL
    assert second.receipt_digest is None
    assert store.event_rows() == before_retry

    try:
        _launch(store, scope, permit, now_ns=40)
    except PermitResolutionError as exc:
        assert "already launched" in str(exc)
    else:  # pragma: no cover - explicit safety assertion
        raise AssertionError("UNKNOWN permit was redispatched")

    digest = store.runtime_projection_digest()
    assert store.rebuild_runtime_projections() == digest
    replayed = reconciler.classify(permit.permit_id)
    assert replayed == outcome.lifecycle_after


def test_terminal_marker_is_terminal_but_exposes_incomplete_accounting(tmp_path):
    store, admission, scope = _runtime(tmp_path)
    permit = _admit(admission, scope)
    _launch(store, scope, permit)
    _append_terminal(store, permit.permit_id)

    reconciler = _reconciler(store)
    lifecycle = reconciler.classify(permit.permit_id)
    assert lifecycle.state is WorkerLifecycleState.TERMINAL
    assert lifecycle.terminal_kind == "WORKER_TERMINAL"
    assert lifecycle.accounting_complete is False
    assert reconciler.inventory().is_unambiguous is False
    assert (
        reconciler.plan(permit.permit_id).disposition
        is ReconciliationDisposition.HOLD_INCOMPLETE_TERMINAL
    )


def test_budget_closure_before_worker_terminal_is_a_valid_complete_lifecycle(tmp_path):
    store, admission, scope = _runtime(tmp_path)
    permit = _admit(admission, scope)
    _launch(store, scope, permit)
    admission.settle(
        attempt_id="attempt-1",
        actual_usage={"attempts": 1, "tokens": 2},
        settlement_revision=1,
        occurred_at_ns=15,
    )
    _append_terminal(store, permit.permit_id)

    lifecycle = _reconciler(store).classify(permit.permit_id)
    assert lifecycle.state is WorkerLifecycleState.TERMINAL
    assert lifecycle.accounting_complete is True
    assert lifecycle.reasons == ()
    assert (
        _reconciler(store).plan(permit.permit_id).disposition
        is ReconciliationDisposition.ALREADY_TERMINAL
    )


def test_duplicate_launch_and_terminal_markers_are_rejected_at_append(tmp_path):
    store, admission, scope = _runtime(tmp_path)
    permit = _admit(admission, scope)
    _launch(store, scope, permit)
    launch_payload = store.event_rows(kind="WORKER_LAUNCH_PREPARED")[0]["payload"]
    with pytest.raises(IntegrityError, match="compare-and-set"):
        store.commit_command(
            command_id="duplicate-launch",
            idempotency_key="duplicate-launch",
            command_payload=launch_payload,
            events=[CommandEvent(
                "event:duplicate-launch", "WORKER_LAUNCH_PREPARED",
                "other-owner", 11, launch_payload,
            )],
            projection_mutations=[ProjectionMutation(
                "attempt_launch", launch_payload
            )],
            committed_at_ns=11,
        )
    _append_terminal(store, permit.permit_id, suffix="one")
    with pytest.raises(IntegrityError, match="compare-and-append"):
        _append_terminal(
            store, permit.permit_id, suffix="two", kind="WORKER_UNKNOWN"
        )
    assert len(store.event_rows(kind="WORKER_LAUNCH_PREPARED")) == 1
    assert len(store.event_rows(kind="WORKER_TERMINAL")) == 1


def test_duplicate_admission_marker_is_rejected_at_append(tmp_path):
    store, admission, scope = _runtime(tmp_path)
    _admit(admission, scope)
    payload = store.event_rows(kind="ATTEMPT_ADMITTED")[0]["payload"]
    with pytest.raises(IntegrityError, match="authority or identity"):
        store.commit_command(
            command_id="duplicate-admission",
            idempotency_key="duplicate-admission",
            command_payload=payload,
            events=[CommandEvent(
                "event:duplicate-admission", "ATTEMPT_ADMITTED",
                "search-admission", 6, payload,
            )],
            projection_mutations=[ProjectionMutation("attempt_admit", payload)],
            committed_at_ns=6,
        )
    assert len(store.event_rows(kind="ATTEMPT_ADMITTED")) == 1


def test_incomplete_launch_and_terminal_without_launch_are_rejected(tmp_path):
    store, admission, scope = _runtime(tmp_path)
    permit = _admit(admission, scope)
    with pytest.raises(IntegrityError, match="attempt_launch semantic mutation"):
        store.commit_command(
            command_id="incomplete-launch",
            idempotency_key="incomplete-launch",
            command_payload={"permit_id": permit.permit_id},
            events=[CommandEvent(
                "event:incomplete-launch", "WORKER_LAUNCH_PREPARED",
                "legacy-owner", 10, {"permit_id": permit.permit_id},
            )],
            committed_at_ns=10,
        )
    assert _reconciler(store).classify(
        permit.permit_id
    ).state is WorkerLifecycleState.NOT_LAUNCHED

    second = _admit(
        admission,
        scope,
        attempt_id="attempt-2",
        permit_id="permit-2",
    )
    with pytest.raises(StopIteration):
        _append_terminal(store, second.permit_id, suffix="without-launch")
    assert _reconciler(store).classify(
        second.permit_id
    ).state is WorkerLifecycleState.NOT_LAUNCHED


def test_unbound_worker_marker_is_rejected_before_append(tmp_path):
    store, admission, scope = _runtime(tmp_path)
    permit = _admit(admission, scope)
    _launch(store, scope, permit)
    with pytest.raises(IntegrityError, match="terminal owner guard"):
        store.commit_command(
            command_id="unbound-marker",
            idempotency_key="unbound-marker",
            command_payload={"outcome": "unknown"},
            events=[CommandEvent(
                "event:unbound-marker", "WORKER_UNKNOWN",
                "broken-owner", 12, {"outcome": "unknown"},
            )],
            committed_at_ns=12,
        )
    assert _reconciler(store).classify(
        permit.permit_id
    ).state is WorkerLifecycleState.IN_FLIGHT_ORPHAN
