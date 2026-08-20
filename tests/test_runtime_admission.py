from __future__ import annotations

import pytest

from muteki.epistemic.sqlite_store import (
    CommandEvent, EpistemicSQLiteStore, IntegrityError, ProjectionMutation,
)
from muteki.runtime.admission import AdmissionRequest, SearchAdmission
from muteki.runtime.contracts import (
    AttemptIdentity, EffectClass, ExecutionScope, LeaseIdentity,
)
from muteki.runtime.controller import BootRecoveryCapability, LiveHealthGuard
from muteki.runtime.effects import EffectLedger
from muteki.runtime.permit_resolver import CanonicalPermitResolver


def _runtime(tmp_path):
    store = EpistemicSQLiteStore.create(
        path=tmp_path / "runtime.db", run_id="run-1", manifest_digest="a" * 64)
    store.commit_command(
        command_id="C-ready", idempotency_key="ready", command_payload={},
        committed_at_ns=1,
        events=[CommandEvent("E-ready", "BOOT_READY", "host", 1)],
    )
    store.commit_command(
        command_id="C-start", idempotency_key="start", command_payload={},
        committed_at_ns=2,
        events=[CommandEvent("E-start", "START_EXECUTION", "host", 2,
                            {"execution_generation": 1, "run_fence_epoch": 1})],
        projection_mutations=[ProjectionMutation(
            "execution_start_guard",
            {"execution_generation": 1, "run_fence_epoch": 1},
        )],
        authority_capability=store._lifecycle_commit_capability,
    )
    guard = LiveHealthGuard()
    cap = BootRecoveryCapability(1, 1, "owner")
    guard.begin_boot_finalize(cap)
    guard.open_admission(capability=cap, attestation_digest="b" * 64)
    return store, SearchAdmission(store=store, guard=guard)


def _request(*, branch="b1", attempt="a1", permit="p1", account="child",
             budget=None, conflicts=("target:one",), fingerprint="",
             effect_class=EffectClass.OBSERVABLE):
    scope = ExecutionScope("run-1", 1, 1)
    identity = AttemptIdentity(scope, branch, attempt, 1)
    lease = LeaseIdentity(identity, f"lease-{attempt}", 1, 1)
    return AdmissionRequest(
        identity, lease, permit, account,
        budget or {"tokens": 20, "attempts": 1}, tuple(conflicts),
        effect_class, fingerprint or f"fp-{attempt}", "c" * 64, 10_000)


def _configure(admission):
    admission.create_branch(branch_id="root", max_attempts=2, occurred_at_ns=3)
    admission.set_branch_state(branch_id="root", expected_state="open",
                               new_state="resolved", revision=1, occurred_at_ns=4)
    admission.create_branch(branch_id="b1", depends_on=["root"], max_attempts=2,
                            occurred_at_ns=5)
    admission.create_budget_account(
        account_id="global", limits={"tokens": 100, "attempts": 3},
        occurred_at_ns=6)
    admission.create_budget_account(
        account_id="child", parent_id="global",
        limits={"tokens": 60, "attempts": 2}, occurred_at_ns=7)


def _launch(store, permit, *, occurred_at_ns=8):
    return CanonicalPermitResolver(
        store=store, scope=permit.lease.attempt.scope
    ).claim_launch(permit, now_ns=occurred_at_ns)


def test_admission_reserves_all_ancestors_atomically(tmp_path):
    store, admission = _runtime(tmp_path)
    _configure(admission)
    permit = admission.admit(_request(), occurred_at_ns=8)
    assert set(permit.reservation_ids) == {"p1:child", "p1:global"}
    held = {
        row[0]: store._json_map(row[1])
        for row in store._conn.execute(
            "SELECT account_id,held_json FROM budget_accounts")
    }
    assert held["child"]["tokens"] == held["global"]["tokens"] == 20


def test_closed_dependency_budget_and_conflict_fail_without_attempt(tmp_path):
    store, admission = _runtime(tmp_path)
    admission.create_branch(branch_id="blocked", max_attempts=1,
                            depends_on=["missing"], occurred_at_ns=3)
    admission.create_budget_account(
        account_id="child", limits={"tokens": 10, "attempts": 1},
        occurred_at_ns=4)
    with pytest.raises(IntegrityError, match="dependency"):
        admission.admit(_request(branch="blocked", budget={"tokens": 1, "attempts": 1}),
                        occurred_at_ns=5)
    assert store._conn.execute("SELECT COUNT(*) FROM runtime_attempts").fetchone()[0] == 0

    admission.create_branch(branch_id="b1", max_attempts=2, occurred_at_ns=6)
    with pytest.raises(IntegrityError, match="oversell"):
        admission.admit(_request(budget={"tokens": 11, "attempts": 1}),
                        occurred_at_ns=7)
    assert store._conn.execute("SELECT COUNT(*) FROM runtime_attempts").fetchone()[0] == 0


def test_settlement_releases_holds_and_records_debt(tmp_path):
    store, admission = _runtime(tmp_path)
    _configure(admission)
    permit = admission.admit(_request(), occurred_at_ns=8)
    _launch(store, permit, occurred_at_ns=9)
    admission.settle(attempt_id="a1", actual_usage={"tokens": 70, "attempts": 1},
                     settlement_revision=1, occurred_at_ns=9)
    rows = store._conn.execute(
        "SELECT account_id,held_json,settled_json,debt FROM budget_accounts").fetchall()
    by_id = {row[0]: row for row in rows}
    assert store._json_map(by_id["child"][1])["tokens"] == 0
    assert store._json_map(by_id["child"][2])["tokens"] == 70
    assert by_id["child"][3] == 1
    assert store._conn.execute(
        "SELECT COUNT(*) FROM effect_conflict_holds WHERE operation_id='a1'"
    ).fetchone()[0] == 0
    with pytest.raises(IntegrityError, match="debt"):
        admission.admit(_request(attempt="a2", permit="p2", conflicts=("target:two",)),
                        occurred_at_ns=10)


def test_unknown_effect_holds_conflict_and_cannot_retry(tmp_path):
    store, admission = _runtime(tmp_path)
    _configure(admission)
    permit = admission.admit(_request(), occurred_at_ns=8)
    _launch(store, permit, occurred_at_ns=9)
    effects = EffectLedger(store)
    effects.prepare(operation_id="op1", attempt_id="a1",
                    effect_class=EffectClass.OBSERVABLE,
                    conflict_keys=["target:one"], occurred_at_ns=10)
    effects.transition(operation_id="op1", expected_state="prepared",
                       new_state="dispatch_may_have_started", revision=1,
                       occurred_at_ns=10)
    effects.transition(operation_id="op1", expected_state="dispatch_may_have_started",
                       new_state="unknown", revision=2, occurred_at_ns=11)
    assert store._conn.execute(
        "SELECT state FROM effect_conflict_holds WHERE conflict_key='target:one'"
    ).fetchone()[0] == "unknown"
    with pytest.raises(ValueError, match="fresh admitted attempt"):
        effects.retry_confirmed_not_applied(operation_id="op1", revision=3,
                                            occurred_at_ns=12)


def test_confirmed_not_applied_still_requires_fresh_admission_to_retry(tmp_path):
    store, admission = _runtime(tmp_path)
    _configure(admission)
    permit = admission.admit(
        _request(effect_class=EffectClass.IDEMPOTENT), occurred_at_ns=8
    )
    _launch(store, permit, occurred_at_ns=9)
    effects = EffectLedger(store)
    effects.prepare(operation_id="op1", attempt_id="a1",
                    effect_class=EffectClass.IDEMPOTENT,
                    conflict_keys=["target:one"], occurred_at_ns=10)
    effects.transition(operation_id="op1", expected_state="prepared",
                       new_state="confirmed_not_applied", revision=1,
                       occurred_at_ns=10)
    with pytest.raises(ValueError, match="fresh admitted attempt"):
        effects.retry_confirmed_not_applied(
            operation_id="op1", revision=2, occurred_at_ns=11
        )
    attempts = store._conn.execute(
        "SELECT ordinal,state FROM effect_attempts WHERE operation_id='op1' "
        "ORDER BY ordinal").fetchall()
    assert attempts == [(1, "confirmed_not_applied")]


def test_attempt_fingerprint_remains_covered_after_terminal(tmp_path):
    store, admission = _runtime(tmp_path)
    _configure(admission)
    permit = admission.admit(_request(conflicts=()), occurred_at_ns=8)
    _launch(store, permit, occurred_at_ns=9)
    admission.settle(attempt_id="a1", actual_usage={"tokens": 1, "attempts": 1},
                     settlement_revision=1, occurred_at_ns=9)
    duplicate = _request(attempt="a2", permit="p2", conflicts=(),
                         fingerprint="fp-a1")
    with pytest.raises(IntegrityError, match="fingerprint"):
        admission.admit(duplicate, occurred_at_ns=10)


def test_runtime_projections_rebuild_from_canonical_events(tmp_path):
    store, admission = _runtime(tmp_path)
    _configure(admission)
    admission.admit(_request(conflicts=()), occurred_at_ns=8)
    admission.settle(attempt_id="a1", actual_usage={"tokens": 3, "attempts": 1},
                     settlement_revision=1, occurred_at_ns=9)
    before = store.runtime_projection_digest()
    after = store.rebuild_runtime_projections()
    assert after == before


def test_budget_projection_rejects_incomplete_or_untyped_usage(tmp_path):
    store, admission = _runtime(tmp_path)
    _configure(admission)
    admission.admit(_request(conflicts=()), occurred_at_ns=8)
    before = _event_count(store)
    invalid = (
        {"tokens": 1},
        {"tokens": 1, "attempts": 1, "extra": 1},
        {"tokens": -1, "attempts": 1},
        {"tokens": True, "attempts": 1},
    )
    for usage in invalid:
        with pytest.raises((IntegrityError, ValueError)):
            admission.settle(
                attempt_id="a1", actual_usage=usage,
                settlement_revision=1, occurred_at_ns=9,
            )
    assert _event_count(store) == before
    assert store._conn.execute(
        "SELECT state FROM runtime_attempts WHERE attempt_id='a1'"
    ).fetchone()[0] == "reserved"


def test_budget_terminal_and_unknown_transitions_are_one_way(tmp_path):
    store, admission = _runtime(tmp_path)
    _configure(admission)
    admission.admit(_request(conflicts=()), occurred_at_ns=8)
    admission.settle(
        attempt_id="a1", actual_usage={"tokens": 1, "attempts": 1},
        settlement_revision=1, occurred_at_ns=9,
    )
    before = _event_count(store)
    with pytest.raises(IntegrityError, match="active"):
        admission.settle(
            attempt_id="a1", actual_usage={"tokens": 1, "attempts": 1},
            settlement_revision=2, occurred_at_ns=10,
        )
    with pytest.raises(IntegrityError, match="active"):
        admission.hold_unknown_usage(
            attempt_id="a1", revision=2, occurred_at_ns=10,
        )
    with pytest.raises(IntegrityError, match="canonical admission"):
        admission.hold_unknown_usage(
            attempt_id="missing", revision=1, occurred_at_ns=10,
        )
    assert _event_count(store) == before


def test_effect_prepare_requires_active_matching_permit(tmp_path):
    store, admission = _runtime(tmp_path)
    _configure(admission)
    permit = admission.admit(_request(conflicts=()), occurred_at_ns=8)
    _launch(store, permit, occurred_at_ns=9)
    effects = EffectLedger(store)
    with pytest.raises(IntegrityError, match="effect class"):
        effects.prepare(
            operation_id="wrong-class", attempt_id="a1",
            effect_class=EffectClass.IDEMPOTENT, conflict_keys=[],
            occurred_at_ns=9,
        )
    admission.settle(
        attempt_id="a1", actual_usage={"tokens": 1, "attempts": 1},
        settlement_revision=1, occurred_at_ns=10,
    )
    with pytest.raises(IntegrityError, match="running admitted"):
        effects.prepare(
            operation_id="after-terminal", attempt_id="a1",
            effect_class=EffectClass.OBSERVABLE, conflict_keys=[],
            occurred_at_ns=11,
        )


def _prepared_effects(tmp_path, *, effect_class=EffectClass.OBSERVABLE,
                      conflicts=("target:one",)):
    store, admission = _runtime(tmp_path)
    _configure(admission)
    permit = admission.admit(
        _request(conflicts=conflicts, effect_class=effect_class),
        occurred_at_ns=8,
    )
    _launch(store, permit, occurred_at_ns=9)
    effects = EffectLedger(store)
    effects.prepare(operation_id="op1", attempt_id="a1",
                    effect_class=effect_class,
                    conflict_keys=list(conflicts), occurred_at_ns=9)
    return store, effects


def _event_count(store):
    return store._conn.execute("SELECT COUNT(*) FROM events").fetchone()[0]


def test_unknown_reconciliation_requires_independent_observer_receipt(tmp_path):
    store, effects = _prepared_effects(tmp_path)
    effects.transition(operation_id="op1", expected_state="prepared",
                       new_state="dispatch_may_have_started", revision=1,
                       occurred_at_ns=10)
    effects.transition(operation_id="op1", expected_state="dispatch_may_have_started",
                       new_state="unknown", revision=2, occurred_at_ns=11)
    before = _event_count(store)
    for new_state in ("prepared", "dispatch_may_have_started", "unknown"):
        with pytest.raises(ValueError, match="illegal effect transition"):
            effects.transition(operation_id="op1", expected_state="unknown",
                               new_state=new_state, revision=3,
                               occurred_at_ns=12)
    assert _event_count(store) == before
    for new_state in ("observed", "confirmed_not_applied"):
        with pytest.raises(ValueError, match="independent observer receipt"):
            effects.transition(
                operation_id="op1", expected_state="unknown",
                new_state=new_state, revision=3, occurred_at_ns=12,
            )
    assert store._conn.execute(
        "SELECT state FROM effect_operations WHERE operation_id='op1'"
    ).fetchone()[0] == "unknown"
    assert store._conn.execute(
        "SELECT COUNT(*) FROM effect_conflict_holds WHERE operation_id='op1'"
    ).fetchone()[0] == 1


def test_illegal_effect_transitions_rejected(tmp_path):
    # prepared -> observed
    store, effects = _prepared_effects(tmp_path / "p2o")
    before = _event_count(store)
    with pytest.raises(ValueError, match="illegal effect transition"):
        effects.transition(operation_id="op1", expected_state="prepared",
                           new_state="observed", revision=1, occurred_at_ns=10)
    assert _event_count(store) == before

    # observed is terminal for transition calls
    store, effects = _prepared_effects(tmp_path / "obs")
    effects.transition(operation_id="op1", expected_state="prepared",
                       new_state="dispatch_may_have_started", revision=1,
                       occurred_at_ns=10)
    effects.transition(operation_id="op1", expected_state="dispatch_may_have_started",
                       new_state="observed", revision=2, occurred_at_ns=11)
    before = _event_count(store)
    for new_state in ("confirmed_not_applied", "unknown", "prepared", "observed",
                      "dispatch_may_have_started"):
        with pytest.raises(ValueError, match="illegal effect transition"):
            effects.transition(operation_id="op1", expected_state="observed",
                               new_state=new_state, revision=3, occurred_at_ns=12)
    assert _event_count(store) == before

    # confirmed_not_applied is terminal for transition calls
    store, effects = _prepared_effects(tmp_path / "cna")
    effects.transition(operation_id="op1", expected_state="prepared",
                       new_state="confirmed_not_applied", revision=1,
                       occurred_at_ns=10)
    before = _event_count(store)
    for new_state in ("prepared", "dispatch_may_have_started", "observed",
                      "unknown", "confirmed_not_applied"):
        with pytest.raises(ValueError, match="illegal effect transition"):
            effects.transition(
                operation_id="op1", expected_state="confirmed_not_applied",
                new_state=new_state, revision=2, occurred_at_ns=11)
    assert _event_count(store) == before


def test_nonpositive_revision_rejected_at_api_and_projection(tmp_path):
    store, effects = _prepared_effects(tmp_path)
    before = _event_count(store)
    before_cmds = store._conn.execute("SELECT COUNT(*) FROM commands").fetchone()[0]
    for revision in (0, -1):
        with pytest.raises(ValueError, match="revision must be greater than zero"):
            effects.transition(operation_id="op1", expected_state="prepared",
                               new_state="dispatch_may_have_started",
                               revision=revision, occurred_at_ns=10)
        with pytest.raises(ValueError, match="revision must be greater than zero"):
            effects.retry_confirmed_not_applied(
                operation_id="op1", revision=revision, occurred_at_ns=10)
    assert _event_count(store) == before

    # Direct ProjectionMutation cannot bypass revision validation.
    for revision in (0, -1):
        for kind, state in (
            ("effect_transition", "dispatch_may_have_started"),
            ("effect_retry", "prepared"),
        ):
            payload = {"operation_id": "op1", "revision": revision}
            if kind == "effect_transition":
                payload.update(expected_state="prepared", new_state=state)
            suffix = f"{kind}:{revision}"
            with pytest.raises(ValueError, match="revision must be greater than zero"):
                store.commit_command(
                    command_id=f"bypass:{suffix}",
                    idempotency_key=f"bypass:{suffix}",
                    command_payload=payload,
                    events=[CommandEvent(f"E-bypass:{suffix}", "EFFECT_TEST",
                                         "test", 10, payload)],
                    projection_mutations=[ProjectionMutation(kind, payload)],
                    committed_at_ns=10,
                )
    assert _event_count(store) == before
    assert store._conn.execute(
        "SELECT COUNT(*) FROM commands").fetchone()[0] == before_cmds
    assert store._conn.execute(
        "SELECT state FROM effect_operations WHERE operation_id='op1'"
    ).fetchone()[0] == "prepared"


def test_direct_projection_illegal_edge_rolls_back(tmp_path):
    store, effects = _prepared_effects(tmp_path)
    before = _event_count(store)
    before_cmds = store._conn.execute("SELECT COUNT(*) FROM commands").fetchone()[0]
    with pytest.raises(
        IntegrityError, match="diverges from its semantic mutation|illegal effect transition"
    ):
        store.commit_command(
            command_id="bypass:p2o", idempotency_key="bypass:p2o",
            command_payload={"op": "illegal"},
            events=[CommandEvent("E-bypass-p2o", "EFFECT_OBSERVED", "test", 10,
                                 {"operation_id": "op1"})],
            projection_mutations=[ProjectionMutation("effect_transition", {
                "operation_id": "op1", "expected_state": "prepared",
                "new_state": "observed", "revision": 1,
            })],
            committed_at_ns=10,
        )
    assert _event_count(store) == before
    assert store._conn.execute(
        "SELECT COUNT(*) FROM commands").fetchone()[0] == before_cmds
    assert store._conn.execute(
        "SELECT state FROM effect_operations WHERE operation_id='op1'"
    ).fetchone()[0] == "prepared"

    # Observed -> confirmed_not_applied via direct mutation (API also rejects).
    effects.transition(operation_id="op1", expected_state="prepared",
                       new_state="dispatch_may_have_started", revision=1,
                       occurred_at_ns=11)
    effects.transition(operation_id="op1", expected_state="dispatch_may_have_started",
                       new_state="observed", revision=2, occurred_at_ns=12)
    mid = _event_count(store)
    with pytest.raises(
        IntegrityError, match="diverges from its semantic mutation|illegal effect transition"
    ):
        store.commit_command(
            command_id="bypass:o2c", idempotency_key="bypass:o2c",
            command_payload={},
            events=[CommandEvent("E-bypass-o2c", "EFFECT_CONFIRMED_NOT_APPLIED",
                                 "test", 13, {})],
            projection_mutations=[ProjectionMutation("effect_transition", {
                "operation_id": "op1", "expected_state": "observed",
                "new_state": "confirmed_not_applied", "revision": 3,
            })],
            committed_at_ns=13,
        )
    assert _event_count(store) == mid
    assert store._conn.execute(
        "SELECT state FROM effect_operations WHERE operation_id='op1'"
    ).fetchone()[0] == "observed"


def test_effect_ledger_rebuild_digest_equivalence(tmp_path):
    store, effects = _prepared_effects(tmp_path)
    effects.transition(operation_id="op1", expected_state="prepared",
                       new_state="dispatch_may_have_started", revision=1,
                       occurred_at_ns=10)
    effects.transition(operation_id="op1", expected_state="dispatch_may_have_started",
                       new_state="unknown", revision=2, occurred_at_ns=11)
    canonical_events = store.event_rows()
    before = store.runtime_projection_digest()
    after = store.rebuild_runtime_projections()
    assert after == before
    assert store.event_rows() == canonical_events
    attempts = store._conn.execute(
        "SELECT ordinal,state FROM effect_attempts WHERE operation_id='op1' "
        "ORDER BY ordinal").fetchall()
    assert attempts == [(1, "unknown")]
