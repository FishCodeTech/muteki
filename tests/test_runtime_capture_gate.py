from __future__ import annotations

import pytest

from muteki.epistemic.authority import GateAuthority, GateInputRejected
from muteki.epistemic.broker import CaptureIntegrityError, CaptureSession
from muteki.epistemic.cas import ReceiptCAS
from muteki.epistemic.sqlite_store import (
    CommandEvent, EpistemicSQLiteStore, ProjectionMutation,
)
from muteki.runtime.admission import AdmissionRequest, SearchAdmission
from muteki.runtime.contracts import (
    AttemptIdentity,
    AttemptPermit,
    EffectClass,
    ExecutionScope,
    LeaseIdentity,
)
from muteki.runtime.controller import BootRecoveryCapability, LiveHealthGuard
from muteki.runtime.permit_resolver import CanonicalPermitResolver


POLICY_DIGEST = "f" * 64
FLAG_FORMAT = r"flag\{[^}]+\}"


class _Artifacts:
    def read_text(self, _artifact_id: str) -> str:
        return ""


def _ready_admission(tmp_path) -> tuple[
    EpistemicSQLiteStore, ReceiptCAS, SearchAdmission
]:
    store = EpistemicSQLiteStore.create(
        path=tmp_path / "epistemic-v2.db",
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
    admission.create_branch(branch_id="root", max_attempts=4, occurred_at_ns=3)
    admission.create_budget_account(
        account_id="run-budget", limits={"wall_ms": 1_000}, occurred_at_ns=4
    )
    return store, ReceiptCAS(tmp_path / "cas"), admission


def _admit(
    admission: SearchAdmission,
    *,
    store: EpistemicSQLiteStore,
    ordinal: int,
    occurred_at_ns: int,
) -> AttemptPermit:
    scope = ExecutionScope("run-1", 1, 1)
    attempt = AttemptIdentity(scope, "root", f"attempt-{ordinal}", ordinal)
    lease = LeaseIdentity(attempt, f"lease-{ordinal}", 1, ordinal)
    permit = admission.admit(
        AdmissionRequest(
            attempt=attempt,
            lease=lease,
            permit_id=f"permit-{ordinal}",
            account_id="run-budget",
            requested_budget={"wall_ms": 100},
            conflict_keys=(),
            effect_class=EffectClass.PURE,
            fingerprint=f"fingerprint-{ordinal}",
            policy_digest=POLICY_DIGEST,
            expires_at_ns=10_000,
        ),
        occurred_at_ns=occurred_at_ns,
    )
    CanonicalPermitResolver(store=store, scope=scope).claim_launch(
        permit, now_ns=occurred_at_ns + 1
    )
    return permit


def _seal_gate_input(
    capture: CaptureSession,
    *,
    candidate_id: str = "candidate-a",
    flag: str = "flag{real}",
    occurred_at_ns: int = 10,
):
    return capture.seal_gate_input(
        capture_id=f"gate-input:{candidate_id}",
        candidate_id=candidate_id,
        flag=flag,
        flag_format=FLAG_FORMAT,
        policy_digest=POLICY_DIGEST,
        data=f"observed output: {flag}\n".encode(),
        occurred_at_ns=occurred_at_ns,
    )


def test_gate_accepts_only_canonical_permit_bound_cas_input(tmp_path):
    store, cas, admission = _ready_admission(tmp_path)
    permit = _admit(admission, store=store, ordinal=1, occurred_at_ns=5)
    gate_input = _seal_gate_input(CaptureSession(store, cas, permit))

    gate = GateAuthority(store=store, cas=cas, artifacts=_Artifacts())
    decision = gate.evaluate(
        evaluation_id=gate.evaluation_id_for(gate_input),
        candidate_id="candidate-a",
        flag="flag{real}",
        gate_input=gate_input,
        permit=permit,
        flag_format=FLAG_FORMAT,
        policy_digest=POLICY_DIGEST,
        occurred_at_ns=11,
    )

    assert decision.accepted is True
    accepted = store.event_rows(kind="FLAG_ACCEPTED")
    assert len(accepted) == 1
    assert accepted[0]["payload"]["attempt_digest"] == permit.lease.attempt.digest
    assert accepted[0]["payload"]["capture_event_digest"] == (
        gate_input.capture_event_digest
    )
    assert accepted[0]["payload"]["manifest_digest"] == gate_input.manifest_digest
    assert cas.read_verified(gate_input.raw_digest) == b"observed output: flag{real}\n"


def test_protocol2_gate_never_trusts_mutable_legacy_artifact_fallback(tmp_path):
    class MutableArtifacts:
        def read_text(self, _artifact_id: str) -> str:
            return "flag{artifact-only}"

    store, cas, admission = _ready_admission(tmp_path)
    permit = _admit(admission, store=store, ordinal=1, occurred_at_ns=5)
    capture = CaptureSession(store, cas, permit)
    gate_input = capture.seal_gate_input(
        capture_id="artifact-reference",
        candidate_id="candidate-artifact",
        flag="flag{artifact-only}",
        flag_format=FLAG_FORMAT,
        policy_digest=POLICY_DIGEST,
        data=b"saved artifact_deadbeef12",
        occurred_at_ns=10,
    )
    gate = GateAuthority(store=store, cas=cas, artifacts=MutableArtifacts())
    decision = gate.evaluate(
        evaluation_id=gate.evaluation_id_for(gate_input),
        candidate_id="candidate-artifact",
        flag="flag{artifact-only}",
        gate_input=gate_input,
        permit=permit,
        flag_format=FLAG_FORMAT,
        policy_digest=POLICY_DIGEST,
        occurred_at_ns=11,
    )
    assert decision.accepted is False


def test_gate_rejects_raw_bytes_and_cross_identity_rebinding(tmp_path):
    store, cas, admission = _ready_admission(tmp_path)
    permit_a = _admit(admission, store=store, ordinal=1, occurred_at_ns=5)
    permit_b = _admit(admission, store=store, ordinal=2, occurred_at_ns=7)
    gate_input_a = _seal_gate_input(CaptureSession(store, cas, permit_a))
    gate = GateAuthority(store=store, cas=cas, artifacts=_Artifacts())

    with pytest.raises(TypeError, match="GateInputReference"):
        gate.evaluate(
            evaluation_id="raw",
            candidate_id="candidate-a",
            flag="flag{real}",
            gate_input=b"observed output: flag{real}",  # type: ignore[arg-type]
            permit=permit_a,
            flag_format=FLAG_FORMAT,
            policy_digest=POLICY_DIGEST,
            occurred_at_ns=11,
        )
    with pytest.raises(GateInputRejected, match="rebound"):
        gate.evaluate(
            evaluation_id=gate.evaluation_id_for(gate_input_a),
            candidate_id="candidate-b",
            flag="flag{real}",
            gate_input=gate_input_a,
            permit=permit_a,
            flag_format=FLAG_FORMAT,
            policy_digest=POLICY_DIGEST,
            occurred_at_ns=11,
        )
    with pytest.raises(GateInputRejected, match="rebound"):
        gate.evaluate(
            evaluation_id=gate.evaluation_id_for(gate_input_a),
            candidate_id="candidate-a",
            flag="flag{other}",
            gate_input=gate_input_a,
            permit=permit_a,
            flag_format=FLAG_FORMAT,
            policy_digest=POLICY_DIGEST,
            occurred_at_ns=11,
        )
    with pytest.raises(GateInputRejected, match="rebound"):
        gate.evaluate(
            evaluation_id=gate.evaluation_id_for(gate_input_a),
            candidate_id="candidate-a",
            flag="flag{real}",
            gate_input=gate_input_a,
            permit=permit_b,
            flag_format=FLAG_FORMAT,
            policy_digest=POLICY_DIGEST,
            occurred_at_ns=11,
        )
    assert not store.event_rows(kind="FLAG_ACCEPTED")
    assert not store.event_rows(kind="FLAG_REJECTED")


def test_capture_manifest_ordinal_is_durable_across_session_restart(tmp_path):
    store, cas, admission = _ready_admission(tmp_path)
    permit = _admit(admission, store=store, ordinal=1, occurred_at_ns=5)
    first_session = CaptureSession(store, cas, permit)
    first = first_session.capture(
        capture_id="capture-0",
        stream="tool_result",
        data=b"first",
        occurred_at_ns=6,
    )
    restarted_session = CaptureSession(store, cas, permit)
    second = restarted_session.capture(
        capture_id="capture-1",
        stream="stdout",
        data=b"second",
        occurred_at_ns=7,
        terminal=True,
    )

    assert (first.ordinal, second.ordinal) == (0, 1)
    manifests = store.event_rows(kind="CAPTURE_MANIFEST_ADVANCED")
    assert [row["payload"]["ordinal"] for row in manifests] == [0, 1]
    assert manifests[1]["payload"]["previous_manifest_digest"] == (
        manifests[0]["payload"]["manifest_digest"]
    )
    with pytest.raises(CaptureIntegrityError, match="terminal"):
        restarted_session.capture(
            capture_id="capture-2",
            stream="stderr",
            data=b"too late",
            occurred_at_ns=8,
        )


def test_capture_rejects_duplicate_id_and_terminal_execution_scope(tmp_path):
    store, cas, admission = _ready_admission(tmp_path)
    permit = _admit(admission, store=store, ordinal=1, occurred_at_ns=5)
    capture = CaptureSession(store, cas, permit)
    capture.capture(
        capture_id="capture-0",
        stream="tool_result",
        data=b"first",
        occurred_at_ns=6,
    )
    with pytest.raises(CaptureIntegrityError, match="already been sealed"):
        capture.capture(
            capture_id="capture-0",
            stream="tool_result",
            data=b"replayed id",
            occurred_at_ns=7,
        )
    with pytest.raises(TypeError, match="exact boolean"):
        capture.capture(
            capture_id="capture-bool-confusion",
            stream="stdout",
            data=b"not terminal",
            occurred_at_ns=7,
            terminal=1,  # type: ignore[arg-type]
        )

    stop_payload = {"scope_digest": permit.lease.attempt.scope.digest}
    store.commit_command(
        command_id="execution-stop",
        idempotency_key="execution-stop",
        command_payload=stop_payload,
        events=[CommandEvent(
            "E-stop", "EXECUTION_STOP_REQUESTED", "host", 8, stop_payload
        )],
        projection_mutations=[ProjectionMutation(
            "execution_stop_guard", stop_payload
        )],
        authority_capability=store._lifecycle_commit_capability,
        committed_at_ns=8,
    )
    with pytest.raises(CaptureIntegrityError, match="not the current running scope"):
        capture.capture(
            capture_id="capture-1",
            stream="stdout",
            data=b"after completion",
            occurred_at_ns=9,
        )


def test_capture_requires_a_canonically_admitted_exact_permit(tmp_path):
    store, cas, admission = _ready_admission(tmp_path)
    permit = _admit(admission, store=store, ordinal=1, occurred_at_ns=5)
    forged = AttemptPermit(
        permit_id=permit.permit_id,
        lease=permit.lease,
        policy_digest=permit.policy_digest,
        reservation_ids=permit.reservation_ids,
        effect_class=permit.effect_class,
        expires_at_ns=permit.expires_at_ns,
        constraints={**permit.constraints, "forged": True},
    )
    with pytest.raises(CaptureIntegrityError, match="exactly one canonical"):
        CaptureSession(store, cas, forged)
