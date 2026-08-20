from __future__ import annotations

import asyncio

import pytest

from muteki.epistemic.authority import GateAuthority
from muteki.epistemic.broker import CandidateBroker, CaptureSession
from muteki.epistemic.cas import ReceiptCAS
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
from muteki.runtime.effects import EffectLedger
from muteki.runtime.permit_resolver import (
    CanonicalPermitResolver,
    PermitResolutionError,
)
from muteki.runtime.ports import CandidateEnvelope
from muteki.runtime.reconciliation import OrphanReconciler
from muteki.runtime.usage import UsageReport


class _Artifacts:
    def read_text(self, _artifact_id: str) -> str:
        return ""


@pytest.mark.parametrize(
    ("kind", "payload"),
    [
        (
            "START_EXECUTION",
            {"execution_generation": 2, "run_fence_epoch": 2},
        ),
        ("GOAL_COMPLETED", {"gate_receipts": (("a" * 64, "b" * 64),)}),
        ("EXECUTION_STOP_REQUESTED", {"scope_digest": "c" * 64}),
        ("EXECUTION_SCOPE_DRAINED", {"scope_digest": "c" * 64}),
        ("PROJECTION_REBUILD_VERIFIED", {"equivalent": True}),
        ("S4E_CLOSURE_ATTESTED", {"all_clean": True}),
    ],
)
def test_direct_lifecycle_authority_events_are_reserved(tmp_path, kind, payload):
    store, _admission, _scope = _runtime(tmp_path)
    before = store.event_rows(kind=kind)
    with pytest.raises(IntegrityError, match="host-only lifecycle capability"):
        store.commit_command(
            command_id=f"forged:{kind}",
            idempotency_key=f"forged:{kind}",
            command_payload=payload,
            events=[CommandEvent(
                f"event:forged:{kind}", kind, "untrusted", 9, payload
            )],
            committed_at_ns=9,
        )
    assert store.event_rows(kind=kind) == before


def _guard(*, boot_epoch: int = 1) -> LiveHealthGuard:
    guard = LiveHealthGuard()
    capability = BootRecoveryCapability(boot_epoch, boot_epoch, f"owner-{boot_epoch}")
    guard.begin_boot_finalize(capability)
    guard.open_admission(capability=capability, attestation_digest="b" * 64)
    return guard


def _runtime(tmp_path):
    store = EpistemicSQLiteStore.create(
        path=tmp_path / "runtime.db", run_id="run-1", manifest_digest="a" * 64
    )
    store.commit_command(
        command_id="ready", idempotency_key="ready", command_payload={},
        events=[CommandEvent("event:ready", "BOOT_READY", "host", 1)],
        committed_at_ns=1,
    )
    store.commit_command(
        command_id="start", idempotency_key="start", command_payload={},
        events=[CommandEvent(
            "event:start", "START_EXECUTION", "host", 2,
            {"execution_generation": 1, "run_fence_epoch": 1},
        )],
        projection_mutations=[ProjectionMutation(
            "execution_start_guard",
            {"execution_generation": 1, "run_fence_epoch": 1},
        )],
        authority_capability=store._lifecycle_commit_capability,
        committed_at_ns=2,
    )
    admission = SearchAdmission(store=store, guard=_guard())
    admission.create_branch(branch_id="root", max_attempts=8, occurred_at_ns=3)
    admission.create_budget_account(
        account_id="run",
        limits={"attempts": 8, "tokens": 100},
        occurred_at_ns=4,
    )
    return store, admission, ExecutionScope("run-1", 1, 1)


def _request(
    scope: ExecutionScope,
    *,
    attempt_id: str = "attempt-1",
    permit_id: str = "permit-1",
    conflicts: tuple[str, ...] = (),
    budget: dict[str, int] | None = None,
):
    attempt = AttemptIdentity(scope, "root", attempt_id, 1)
    lease = LeaseIdentity(attempt, f"lease-{attempt_id}", 1, 1)
    return AdmissionRequest(
        attempt=attempt,
        lease=lease,
        permit_id=permit_id,
        account_id="run",
        requested_budget=budget or {"attempts": 1, "tokens": 20},
        conflict_keys=conflicts,
        effect_class=EffectClass.OBSERVABLE,
        fingerprint=f"fingerprint-{attempt_id}",
        policy_digest="c" * 64,
        expires_at_ns=10_000,
    )


def _manual_terminal(store: EpistemicSQLiteStore, permit, *, suffix: str) -> None:
    admission = next(
        row for row in store.event_rows(kind="ATTEMPT_ADMITTED")
        if row["payload"].get("permit_id") == permit.permit_id
    )
    launch = next(
        row for row in store.event_rows(kind="WORKER_LAUNCH_PREPARED")
        if row["payload"].get("permit_id") == permit.permit_id
    )
    event_id = f"event:terminal:{suffix}"
    payload = {
        "admission_event_digest": admission["event_digest"],
        "attempt_digest": permit.lease.attempt.digest,
        "attempt_id": permit.lease.attempt.attempt_id,
        "launch_event_digest": launch["event_digest"],
        "lease_digest": permit.lease.digest,
        "lease_id": permit.lease.lease_id,
        "outcome": "observed",
        "permit_digest": permit.digest,
        "permit_id": permit.permit_id,
        "scope_digest": permit.lease.attempt.scope.digest,
    }
    store.commit_command(
        command_id=f"terminal:{suffix}",
        idempotency_key=f"terminal:{suffix}",
        command_payload=payload,
        events=[CommandEvent(
            event_id, "WORKER_TERMINAL", "test", 20, payload
        )],
        projection_mutations=[ProjectionMutation(
            "worker_terminal_guard",
            {**payload, "terminal_event_id": event_id},
        )],
        committed_at_ns=20,
    )


def test_stale_scope_admission_loses_atomic_projection_cas(tmp_path, monkeypatch):
    store, admission, scope = _runtime(tmp_path)
    other = EpistemicSQLiteStore.open(store.path)
    original = store.budget_ancestry

    def advance_scope(account_id: str):
        stop_payload = {"scope_digest": scope.digest}
        other.commit_command(
            command_id="stop-1", idempotency_key="stop-1",
            command_payload=stop_payload,
            events=[CommandEvent(
                "event:stop-1", "EXECUTION_STOP_REQUESTED", "other", 5,
                stop_payload,
            )],
            projection_mutations=[ProjectionMutation(
                "execution_stop_guard", stop_payload,
            )],
            authority_capability=other._lifecycle_commit_capability,
            committed_at_ns=5,
        )
        other.commit_command(
            command_id="drain-1", idempotency_key="drain-1",
            command_payload=stop_payload,
            events=[CommandEvent(
                "event:drain-1", "EXECUTION_SCOPE_DRAINED", "other", 6,
                stop_payload,
            )],
            projection_mutations=[ProjectionMutation(
                "execution_drain_guard", stop_payload,
            )],
            authority_capability=other._lifecycle_commit_capability,
            committed_at_ns=6,
        )
        other.commit_command(
            command_id="start-2", idempotency_key="start-2", command_payload={},
            events=[CommandEvent(
                "event:start-2", "START_EXECUTION", "other", 7,
                {"execution_generation": 2, "run_fence_epoch": 2},
            )],
            projection_mutations=[ProjectionMutation(
                "execution_start_guard",
                {"execution_generation": 2, "run_fence_epoch": 2},
            )],
            authority_capability=other._lifecycle_commit_capability,
            committed_at_ns=7,
        )
        return original(account_id)

    monkeypatch.setattr(store, "budget_ancestry", advance_scope)
    with pytest.raises(IntegrityError, match="current active execution scope"):
        admission.admit(_request(scope), occurred_at_ns=6)
    assert store.event_rows(kind="ATTEMPT_ADMITTED") == ()
    other.close()


def test_settlement_between_launch_resolution_and_append_blocks_launch(
    tmp_path, monkeypatch
):
    store, admission, scope = _runtime(tmp_path)
    permit = admission.admit(_request(scope), occurred_at_ns=5)
    other = EpistemicSQLiteStore.open(store.path)
    other_admission = SearchAdmission(store=other, guard=_guard(boot_epoch=2))
    resolver = CanonicalPermitResolver(store=store, scope=scope)
    original = resolver._resolve_locked

    def settle_after_resolution(resolved_permit, *, now_ns):
        result = original(resolved_permit, now_ns=now_ns)
        other_admission.settle(
            attempt_id="attempt-1",
            actual_usage={"attempts": 1, "tokens": 1},
            settlement_revision=1,
            occurred_at_ns=6,
        )
        return result

    monkeypatch.setattr(resolver, "_resolve_locked", settle_after_resolution)
    with pytest.raises(PermitResolutionError):
        resolver.claim_launch(permit, now_ns=7)
    assert store.event_rows(kind="WORKER_LAUNCH_PREPARED") == ()
    other.close()


def test_terminal_interleaving_blocks_capture_and_gate_append(tmp_path, monkeypatch):
    store, admission, scope = _runtime(tmp_path)
    permit = admission.admit(_request(scope), occurred_at_ns=5)
    CanonicalPermitResolver(store=store, scope=scope).claim_launch(permit, now_ns=6)
    cas = ReceiptCAS(tmp_path / "cas")
    capture = CaptureSession(store, cas, permit)
    other = EpistemicSQLiteStore.open(store.path)
    original_seal = cas.seal_bytes
    fired = False

    def terminal_before_capture(data: bytes):
        nonlocal fired
        sealed = original_seal(data)
        if not fired:
            fired = True
            _manual_terminal(other, permit, suffix="capture-race")
        return sealed

    monkeypatch.setattr(cas, "seal_bytes", terminal_before_capture)
    with pytest.raises(IntegrityError, match="after worker terminal"):
        capture.capture(
            capture_id="capture-race", stream="stdout", data=b"bytes",
            occurred_at_ns=7,
        )
    assert store.event_rows(kind="CAPTURE_CHUNK_SEALED") == ()
    other.close()


def test_repeated_gate_input_is_one_canonical_occurrence(tmp_path):
    store, admission, scope = _runtime(tmp_path)
    permit = admission.admit(_request(scope), occurred_at_ns=5)
    CanonicalPermitResolver(store=store, scope=scope).claim_launch(permit, now_ns=6)
    cas = ReceiptCAS(tmp_path / "cas")
    capture = CaptureSession(store, cas, permit)
    gate_input = capture.seal_gate_input(
        capture_id="gate-input", candidate_id="candidate", flag="flag{real}",
        flag_format=r"flag\{[^}]+\}", policy_digest="c" * 64,
        data=b"output flag{real}", occurred_at_ns=7,
    )
    gate = GateAuthority(store=store, cas=cas, artifacts=_Artifacts())
    evaluation_id = gate.evaluation_id_for(gate_input)
    first = gate.evaluate(
        evaluation_id=evaluation_id, candidate_id="candidate", flag="flag{real}",
        gate_input=gate_input, permit=permit, flag_format=r"flag\{[^}]+\}",
        policy_digest="c" * 64, occurred_at_ns=8,
    )
    second = gate.evaluate(
        evaluation_id=evaluation_id, candidate_id="candidate", flag="flag{real}",
        gate_input=gate_input, permit=permit, flag_format=r"flag\{[^}]+\}",
        policy_digest="c" * 64, occurred_at_ns=8,
    )
    assert second.receipt_digest == first.receipt_digest
    assert len(store.event_rows(kind="FLAG_ACCEPTED")) == 1


def test_direct_false_gate_commit_lacks_host_authority_capability(tmp_path):
    store, admission, scope = _runtime(tmp_path)
    permit = admission.admit(_request(scope), occurred_at_ns=5)
    CanonicalPermitResolver(store=store, scope=scope).claim_launch(permit, now_ns=6)
    cas = ReceiptCAS(tmp_path / "cas")
    gate_input = CaptureSession(store, cas, permit).seal_gate_input(
        capture_id="false-input", candidate_id="candidate", flag="flag{absent}",
        flag_format=r"flag\{[^}]+\}", policy_digest="c" * 64,
        data=b"output without the claim", occurred_at_ns=7,
    )
    event_payload = {
        "accepted": True,
        "attempt_digest": gate_input.attempt_digest,
        "candidate_id": gate_input.candidate_id,
        "capture_event_digest": gate_input.capture_event_digest,
        "evaluation_id": "forged",
        "flag_digest": gate_input.flag_digest,
        "flag_format_digest": gate_input.flag_format_digest,
        "lease_digest": gate_input.lease_digest,
        "manifest_digest": gate_input.manifest_digest,
        "permit_digest": gate_input.permit_digest,
        "policy_digest": gate_input.policy_digest,
        "raw_digest": gate_input.raw_digest,
        "snapshot_digest": "d" * 64,
    }
    guard = {
        "action": "gate",
        "attempt_digest": gate_input.attempt_digest,
        "attempt_id": permit.lease.attempt.attempt_id,
        "candidate_id": gate_input.candidate_id,
        "capture_event_digest": gate_input.capture_event_digest,
        "flag_digest": gate_input.flag_digest,
        "flag_format_digest": gate_input.flag_format_digest,
        "lease_digest": gate_input.lease_digest,
        "lease_id": permit.lease.lease_id,
        "manifest_digest": gate_input.manifest_digest,
        "permit_digest": gate_input.permit_digest,
        "permit_id": permit.permit_id,
        "policy_digest": gate_input.policy_digest,
        "raw_digest": gate_input.raw_digest,
        "scope_digest": scope.digest,
        "snapshot_digest": "d" * 64,
    }
    with pytest.raises(IntegrityError, match="GateAuthority capability"):
        store.commit_command(
            command_id="forged-gate", idempotency_key="forged-gate",
            command_payload=event_payload,
            events=[CommandEvent(
                "event:forged-gate", "FLAG_ACCEPTED", "not-gate", 8,
                event_payload,
            )],
            projection_mutations=[ProjectionMutation("attempt_io_guard", guard)],
            committed_at_ns=8,
        )
    assert store.event_rows(kind="FLAG_ACCEPTED") == ()


def test_retained_candidate_callback_cannot_write_after_terminal(tmp_path):
    store, admission, scope = _runtime(tmp_path)
    permit = admission.admit(_request(scope), occurred_at_ns=5)
    CanonicalPermitResolver(store=store, scope=scope).claim_launch(permit, now_ns=6)
    broker = CandidateBroker(store=store, permit=permit)
    _manual_terminal(store, permit, suffix="candidate-terminal")
    with pytest.raises(IntegrityError, match="after worker terminal"):
        broker.submit_candidate(
            CandidateEnvelope("candidate", permit.lease, "fact", {"text": "lead"}),
            occurred_at_ns=7,
        )
    assert store.event_rows(kind="CANDIDATE_REPORTED") == ()


def test_terminal_interleaving_blocks_gate_append(tmp_path, monkeypatch):
    store, admission, scope = _runtime(tmp_path)
    permit = admission.admit(_request(scope), occurred_at_ns=5)
    CanonicalPermitResolver(store=store, scope=scope).claim_launch(permit, now_ns=6)
    cas = ReceiptCAS(tmp_path / "cas")
    capture = CaptureSession(store, cas, permit)
    gate_input = capture.seal_gate_input(
        capture_id="gate-input", candidate_id="candidate", flag="flag{real}",
        flag_format=r"flag\{[^}]+\}", policy_digest="c" * 64,
        data=b"output flag{real}", occurred_at_ns=7,
    )
    gate = GateAuthority(store=store, cas=cas, artifacts=_Artifacts())
    other = EpistemicSQLiteStore.open(store.path)
    original_read = cas.read_verified
    fired = False

    def terminal_before_gate(digest: str):
        nonlocal fired
        data = original_read(digest)
        if not fired:
            fired = True
            _manual_terminal(other, permit, suffix="gate-race")
        return data

    monkeypatch.setattr(cas, "read_verified", terminal_before_gate)
    with pytest.raises(IntegrityError, match="after worker terminal"):
        gate.evaluate(
            evaluation_id=gate.evaluation_id_for(gate_input),
            candidate_id="candidate", flag="flag{real}", gate_input=gate_input,
            permit=permit, flag_format=r"flag\{[^}]+\}",
            policy_digest="c" * 64, occurred_at_ns=8,
        )
    assert store.event_rows(kind="FLAG_ACCEPTED") == ()
    other.close()


def test_effect_owner_collision_and_legacy_retry_are_closed(tmp_path):
    store, admission, scope = _runtime(tmp_path)
    first = admission.admit(
        _request(scope, conflicts=("resource",)), occurred_at_ns=5
    )
    second = admission.admit(
        _request(
            scope, attempt_id="attempt-2", permit_id="permit-2", conflicts=()
        ),
        occurred_at_ns=6,
    )
    CanonicalPermitResolver(store=store, scope=scope).claim_launch(first, now_ns=7)
    CanonicalPermitResolver(store=store, scope=scope).claim_launch(second, now_ns=8)
    assert first and second
    effects = EffectLedger(store)
    with pytest.raises(IntegrityError, match="conflict keys"):
        effects.prepare(
            operation_id="attempt-1", attempt_id="attempt-2",
            effect_class=EffectClass.OBSERVABLE,
            conflict_keys=["resource"], occurred_at_ns=7,
        )
    effects.prepare(
        operation_id="effect-1", attempt_id="attempt-1",
        effect_class=EffectClass.OBSERVABLE,
        conflict_keys=["resource"], occurred_at_ns=8,
    )
    effects.transition(
        operation_id="effect-1", expected_state="prepared",
        new_state="confirmed_not_applied", revision=1, occurred_at_ns=9,
    )
    with pytest.raises(ValueError, match="fresh admitted attempt"):
        effects.retry_confirmed_not_applied(
            operation_id="effect-1", revision=2, occurred_at_ns=10
        )


def test_unknown_over_ceiling_updates_hold_and_debt(tmp_path):
    store, admission, scope = _runtime(tmp_path)
    admission.admit(_request(scope), occurred_at_ns=5)
    report = UsageReport.from_observed_and_reservation(
        reserved={"attempts": 1, "tokens": 20},
        observed={"tokens": 200},
        complete_axes=frozenset(),
    )
    admission.hold_unknown_usage(
        attempt_id="attempt-1", revision=1, occurred_at_ns=6,
        usage_report=report,
    )
    held_json, debt = store._conn.execute(
        "SELECT held_json,debt FROM budget_accounts WHERE account_id='run'"
    ).fetchone()
    assert store._json_map(held_json)["tokens"] == 200
    assert debt == 1
    with pytest.raises(IntegrityError, match="debt"):
        admission.admit(
            _request(scope, attempt_id="attempt-2", permit_id="permit-2"),
            occurred_at_ns=7,
        )


async def test_supervisor_quiesce_rejects_new_launch(tmp_path):
    from muteki.runtime.supervisor import LaunchRejected, RunSupervisor

    store, admission, scope = _runtime(tmp_path)
    permit = admission.admit(_request(scope), occurred_at_ns=5)
    supervisor = RunSupervisor(store=store, scope=scope)
    supervisor.quiesce()
    with pytest.raises(LaunchRejected, match="quiescing"):
        supervisor.spawn_owned(permit, lambda: asyncio.sleep(0), now_ns=6)
    assert store.event_rows(kind="WORKER_LAUNCH_PREPARED") == ()


def test_orphan_reconcile_loses_race_to_real_terminal(tmp_path, monkeypatch):
    store, admission, scope = _runtime(tmp_path)
    permit = admission.admit(_request(scope), occurred_at_ns=5)
    CanonicalPermitResolver(store=store, scope=scope).claim_launch(permit, now_ns=6)
    other = EpistemicSQLiteStore.open(store.path)
    guard = LiveHealthGuard()
    guard.begin_boot_finalize(BootRecoveryCapability(2, 2, "recovery"))
    reconciler = OrphanReconciler(store=store, guard=guard)
    original_commit = store.commit_command

    def terminal_then_commit(**kwargs):
        _manual_terminal(other, permit, suffix="reconcile-race")
        return original_commit(**kwargs)

    monkeypatch.setattr(store, "commit_command", terminal_then_commit)
    with pytest.raises(IntegrityError, match="compare-and-append"):
        reconciler.reconcile(permit.permit_id, occurred_at_ns=30)
    assert len(store.event_rows(kind="WORKER_TERMINAL")) == 1
    assert store.event_rows(kind="WORKER_UNKNOWN") == ()
    other.close()
