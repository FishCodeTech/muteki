from __future__ import annotations

import json

import pytest

from muteki.epistemic.authority import (
    GateAuthority,
    GateInputRejected,
    PromotionAuthority,
    PromotionRequest,
    ReceiptTier,
    resolve_accepted_flag_publication,
)
from muteki.epistemic.broker import CandidateBroker, CaptureSession, StaleLease
from muteki.epistemic.cas import ReceiptCAS
from muteki.epistemic.contracts import canonical_digest
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
from muteki.runtime.controller import (
    AuthorityDenied, BootRecoveryCapability, CommandClass, LiveHealthGuard,
)
from muteki.runtime.ports import CandidateEnvelope
from muteki.runtime.permit_resolver import CanonicalPermitResolver


class _Artifacts:
    def read_text(self, _artifact_id):
        return ""


def _ready_store(tmp_path):
    store = EpistemicSQLiteStore.create(
        path=tmp_path / "epistemic-v2.db", run_id="run-1",
        manifest_digest="a" * 64)
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
    return store


def _lease():
    scope = ExecutionScope("run-1", 1, 1)
    attempt = AttemptIdentity(scope, "branch-1", "attempt-1", 1)
    return LeaseIdentity(attempt, "lease-1", 1, 1)


def _admit_and_launch(store: EpistemicSQLiteStore) -> AttemptPermit:
    guard = LiveHealthGuard()
    capability = BootRecoveryCapability(1, 1, "owner")
    guard.begin_boot_finalize(capability)
    guard.open_admission(capability=capability, attestation_digest="b" * 64)
    admission = SearchAdmission(store=store, guard=guard)
    admission.create_branch(branch_id="branch-1", max_attempts=1, occurred_at_ns=3)
    admission.create_budget_account(
        account_id="run-budget", limits={"wall_ms": 100}, occurred_at_ns=4
    )
    lease = _lease()
    permit = admission.admit(
        AdmissionRequest(
            attempt=lease.attempt,
            lease=lease,
            permit_id="permit-1",
            account_id="run-budget",
            requested_budget={"wall_ms": 100},
            conflict_keys=(),
            effect_class=EffectClass.PURE,
            fingerprint="fingerprint-1",
            policy_digest="f" * 64,
            expires_at_ns=100,
        ),
        occurred_at_ns=5,
    )
    CanonicalPermitResolver(
        store=store, scope=lease.attempt.scope
    ).claim_launch(permit, now_ns=6)
    return permit


def test_persisted_ready_cannot_open_live_guard(tmp_path):
    store = _ready_store(tmp_path)
    guard = LiveHealthGuard()
    with pytest.raises(AuthorityDenied):
        guard.authorize(CommandClass.DISPATCH, store.state())
    cap = BootRecoveryCapability(1, 1, "owner")
    guard.begin_boot_finalize(cap)
    guard.open_admission(capability=cap, attestation_digest="b" * 64)
    guard.authorize(CommandClass.DISPATCH, store.state())


def test_capture_and_candidate_are_sealed_but_cannot_promote(tmp_path):
    store = _ready_store(tmp_path)
    cas = ReceiptCAS(tmp_path / "cas")
    permit = _admit_and_launch(store)
    lease = permit.lease
    capture = CaptureSession(store, cas, permit)
    chunk = capture.capture(
        capture_id="cap-1", stream="stdout", data=b"observed bytes",
        occurred_at_ns=7, terminal=True)
    assert cas.read_verified(chunk.raw_digest) == b"observed bytes"

    broker = CandidateBroker(store=store, permit=permit)
    receipt = broker.submit_candidate(CandidateEnvelope(
        candidate_id="cand-1", lease=lease, kind="claim",
        payload={"text": "candidate only"},
        artifact_digests=(chunk.raw_digest,)), occurred_at_ns=8)
    assert len(receipt) == 64
    assert not hasattr(broker, "promote")
    stale = LeaseIdentity(lease.attempt, "lease-2", 2, 1)
    with pytest.raises(StaleLease):
        broker.submit_candidate(CandidateEnvelope(
            "cand-2", stale, "claim", {}), occurred_at_ns=9)


def test_promotion_requires_complete_independent_lineage(tmp_path):
    store = _ready_store(tmp_path)
    authority = PromotionAuthority(store)
    with pytest.raises(ValueError, match="same-model"):
        authority.promote(PromotionRequest(
            "p1", "c1", "o1", "a" * 64, "b" * 64, "c" * 64,
            "d" * 64, ReceiptTier.SAME_MODEL_REVIEW), occurred_at_ns=3)
    receipt = authority.promote(PromotionRequest(
        "p2", "c1", "o1", "a" * 64, "b" * 64, "c" * 64,
        "d" * 64, ReceiptTier.DIRECT_EXTRACTOR), occurred_at_ns=4)
    assert len(receipt) == 64


def test_gate_uses_hardcoded_provenance_and_commits_before_outbox(tmp_path):
    store = _ready_store(tmp_path)
    cas = ReceiptCAS(tmp_path / "cas")
    permit = _admit_and_launch(store)
    capture = CaptureSession(store, cas, permit)
    gate = GateAuthority(store=store, cas=cas, artifacts=_Artifacts())
    accepted_input = capture.seal_gate_input(
        capture_id="gate-input-1", candidate_id="cand-1", flag="flag{real}",
        flag_format=r"flag\{[^}]+\}", policy_digest="f" * 64,
        data=b"tool output: flag{real}\n", occurred_at_ns=7,
    )
    accepted = gate.evaluate(
        evaluation_id=gate.evaluation_id_for(accepted_input),
        candidate_id="cand-1", flag="flag{real}",
        gate_input=accepted_input, permit=permit,
        flag_format=r"flag\{[^}]+\}", policy_digest="f" * 64,
        occurred_at_ns=8)
    rejected_input = capture.seal_gate_input(
        capture_id="gate-input-2", candidate_id="cand-2", flag="flag{fake}",
        flag_format=r"flag\{[^}]+\}", policy_digest="f" * 64,
        data=b"assistant guessed it", occurred_at_ns=9,
    )
    rejected = gate.evaluate(
        evaluation_id=gate.evaluation_id_for(rejected_input),
        candidate_id="cand-2", flag="flag{fake}",
        gate_input=rejected_input, permit=permit,
        flag_format=r"flag\{[^}]+\}", policy_digest="f" * 64,
        occurred_at_ns=10)
    assert accepted.accepted is True
    assert rejected.accepted is False
    topics = [row[0] for row in store._conn.execute(
        "SELECT topic FROM immutable_outbox ORDER BY outbox_id")]
    assert topics == ["flag.accepted"]


def _accepted_publication(tmp_path):
    store = _ready_store(tmp_path)
    cas = ReceiptCAS(tmp_path / "cas")
    permit = _admit_and_launch(store)
    capture = CaptureSession(store, cas, permit)
    gate = GateAuthority(store=store, cas=cas, artifacts=_Artifacts())
    flag = "flag{real}"
    flag_digest = canonical_digest(flag)
    candidate_id = canonical_digest({
        "attempt": permit.lease.attempt.digest,
        "flag": flag_digest,
        "kind": "flag-candidate-v1",
    })
    gate_input = capture.seal_gate_input(
        capture_id="gate-input-publication",
        candidate_id=candidate_id,
        flag=flag,
        flag_format=r"flag\{[^}]+\}",
        policy_digest=permit.policy_digest,
        data=b"tool output: flag{real}\n",
        occurred_at_ns=7,
    )
    gate.evaluate(
        evaluation_id=gate.evaluation_id_for(gate_input),
        candidate_id=candidate_id,
        flag=flag,
        gate_input=gate_input,
        permit=permit,
        flag_format=r"flag\{[^}]+\}",
        policy_digest=permit.policy_digest,
        occurred_at_ns=8,
    )
    return store, cas, permit, flag


def test_accepted_publication_recovers_exact_authority_candidate_id(tmp_path):
    store = _ready_store(tmp_path)
    cas = ReceiptCAS(tmp_path / "cas")
    permit = _admit_and_launch(store)
    capture = CaptureSession(store, cas, permit)
    gate = GateAuthority(store=store, cas=cas, artifacts=_Artifacts())
    flag = "flag{exact_candidate}"
    gate_input = capture.seal_gate_input(
        capture_id="gate-input-exact-candidate",
        candidate_id="candidate-a",
        flag=flag,
        flag_format=r"flag\{[^}]+\}",
        policy_digest=permit.policy_digest,
        data=b"tool output: flag{exact_candidate}\n",
        occurred_at_ns=7,
    )
    decision = gate.evaluate(
        evaluation_id=gate.evaluation_id_for(gate_input),
        candidate_id="candidate-a",
        flag=flag,
        gate_input=gate_input,
        permit=permit,
        flag_format=r"flag\{[^}]+\}",
        policy_digest=permit.policy_digest,
        occurred_at_ns=8,
    )

    publication = resolve_accepted_flag_publication(
        store=store,
        cas=cas,
        attempt_digest=permit.lease.attempt.digest,
        flag=flag,
    )

    assert publication.candidate_id == "candidate-a"
    assert publication.evaluation_id == gate.evaluation_id_for(gate_input)
    assert publication.gate_receipt_digest == decision.receipt_digest


def test_accepted_publication_rejects_wrong_attempt_and_flag(tmp_path):
    store, cas, permit, flag = _accepted_publication(tmp_path)
    with pytest.raises(GateInputRejected, match="attempt digest is malformed"):
        resolve_accepted_flag_publication(
            store=store,
            cas=cas,
            attempt_digest="G" * 64,
            flag=flag,
        )
    with pytest.raises(GateInputRejected, match="same-attempt"):
        resolve_accepted_flag_publication(
            store=store,
            cas=cas,
            attempt_digest=permit.lease.attempt.digest,
            flag="flag{other}",
        )


def test_accepted_publication_rejects_missing_flag_object(tmp_path):
    store, cas, permit, flag = _accepted_publication(tmp_path)
    row = store._conn.execute(
        "SELECT payload_json FROM immutable_outbox WHERE topic='flag.accepted'"
    ).fetchone()
    digest = json.loads(row[0])["flag_object_digest"]
    path = cas.objects / digest[:2] / digest[2:]
    path.unlink()
    with pytest.raises(GateInputRejected, match="flag object"):
        resolve_accepted_flag_publication(
            store=store,
            cas=cas,
            attempt_digest=permit.lease.attempt.digest,
            flag=flag,
        )
