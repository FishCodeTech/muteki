from __future__ import annotations

import json
import os

import pytest
import muteki.epistemic.authority as authority_module
import muteki.runtime.permit_resolver as permit_resolver_module

from muteki.epistemic.authority import GateAuthority
from muteki.epistemic.broker import CaptureSession
from muteki.epistemic.cas import ReceiptCAS
from muteki.epistemic.contracts import canonical_digest
from muteki.epistemic.sqlite_store import (
    CommandEvent,
    EpistemicSQLiteStore,
    IntegrityError,
    OutboxIntent,
    ProjectionMutation,
)
from muteki.runtime.admission import AdmissionRequest, SearchAdmission
from muteki.runtime.closure import ClosureResolutionError, resolve_s4e_closure
from muteki.runtime.contracts import (
    AttemptIdentity,
    EffectClass,
    ExecutionScope,
    LeaseIdentity,
)
from muteki.runtime.controller import BootRecoveryCapability, LiveHealthGuard
from muteki.runtime.effects import EffectLedger
from muteki.runtime.permit_resolver import CanonicalPermitResolver
from muteki.runtime.progress import ProgressKind, ProgressLedger, ProgressOccurrence
from muteki.runtime.usage import UsageReport


POLICY_DIGEST = "f" * 64
FLAG_FORMAT = r"flag\{[^}]+\}"


class _Artifacts:
    pass


def _component_bodies(store: EpistemicSQLiteStore):
    events = store.event_rows()

    def rows(kind):
        return [row for row in events if row["kind"] == kind]

    admissions = rows("ATTEMPT_ADMITTED")
    launches = rows("WORKER_LAUNCH_PREPARED")
    captures = rows("CAPTURE_CHUNK_SEALED")
    manifests = rows("CAPTURE_MANIFEST_ADVANCED")
    accepted = rows("FLAG_ACCEPTED")
    settled = rows("BUDGET_SETTLED")
    accepted_capture_digests = {
        row["payload"]["capture_event_digest"] for row in accepted
    }
    schema = {
        "name": "muteki-s4e-closure",
        "version": 1,
        "required_components": (
            "canonical_permit",
            "capture_manifest",
            "gate_input",
            "orphan_summary",
            "usage_closure",
        ),
    }
    components = {
        "canonical_permit": canonical_digest(
            {
                "admission_event_digests": [row["event_digest"] for row in admissions],
                "launch_event_digests": [row["event_digest"] for row in launches],
            }
        ),
        "capture_manifest": canonical_digest(
            {
                "capture_event_digests": [row["event_digest"] for row in captures],
                "manifest_event_digests": [row["event_digest"] for row in manifests],
                "paired": True,
            }
        ),
        "gate_input": canonical_digest(
            {
                "accepted_event_digests": [row["event_digest"] for row in accepted],
                "capture_event_digests": sorted(accepted_capture_digests),
                "resolves": True,
            }
        ),
        "orphan_summary": canonical_digest(
            {
                "ambiguous_attempt_ids": [],
                "orphaned_permit_ids": [],
                "worker_unknown": False,
                "complete": True,
            }
        ),
        "s4e_schema": canonical_digest(schema),
        "usage_closure": canonical_digest(
            {
                "settled_event_digests": [row["event_digest"] for row in settled],
                "unknown_event_digests": [],
                "complete": True,
            }
        ),
    }
    return components, schema


def _solved_closure(tmp_path, *, malformed_goal=False, extra_gate_input=False):
    store = EpistemicSQLiteStore.create(
        path=tmp_path / "epistemic-v2.db",
        run_id="run-1",
        manifest_digest="a" * 64,
    )
    verifying_payload = {"boot_epoch": 1, "writer_epoch": 1}
    store.commit_command(
        command_id="BOOT_VERIFYING:1",
        idempotency_key="BOOT_VERIFYING:1",
        command_payload=verifying_payload,
        events=[
            CommandEvent(
                "event:BOOT_VERIFYING:1",
                "BOOT_VERIFYING",
                "host-run-factory",
                1,
                verifying_payload,
            )
        ],
        committed_at_ns=1,
    )
    ready_payload = {"attestation_digest": "b" * 64}
    store.commit_command(
        command_id="BOOT_READY:1",
        idempotency_key="BOOT_READY:1",
        command_payload=ready_payload,
        events=[
            CommandEvent(
                "event:BOOT_READY:1",
                "BOOT_READY",
                "host-run-factory",
                2,
                ready_payload,
            )
        ],
        committed_at_ns=2,
    )
    start_payload = {"execution_generation": 1, "run_fence_epoch": 1}
    store.commit_command(
        command_id="START_EXECUTION:1",
        idempotency_key="execution-start",
        command_payload=start_payload,
        events=[
            CommandEvent(
                "event:START_EXECUTION:1",
                "START_EXECUTION",
                "host-run-factory",
                3,
                start_payload,
            )
        ],
        projection_mutations=[ProjectionMutation(
            "execution_start_guard", start_payload
        )],
        authority_capability=store._lifecycle_commit_capability,
        committed_at_ns=3,
    )
    guard = LiveHealthGuard()
    capability = BootRecoveryCapability(1, 1, "owner")
    guard.begin_boot_finalize(capability)
    guard.open_admission(capability=capability, attestation_digest="b" * 64)
    admission = SearchAdmission(store=store, guard=guard)
    admission.create_branch(branch_id="root", max_attempts=1, occurred_at_ns=3)
    admission.create_budget_account(
        account_id="run-budget",
        limits={"tokens": 100, "wall_ms": 1_000},
        occurred_at_ns=4,
    )
    scope = ExecutionScope("run-1", 1, 1)
    attempt = AttemptIdentity(scope, "root", "attempt-1", 1)
    lease = LeaseIdentity(attempt, "lease-1", 1, 1)
    permit = admission.admit(
        AdmissionRequest(
            attempt=attempt,
            lease=lease,
            permit_id="permit-1",
            account_id="run-budget",
            requested_budget={"tokens": 20, "wall_ms": 500},
            conflict_keys=(),
            effect_class=EffectClass.OBSERVABLE,
            fingerprint="fixture-worker-1",
            policy_digest=POLICY_DIGEST,
            expires_at_ns=10_000,
        ),
        occurred_at_ns=5,
    )
    CanonicalPermitResolver(store=store, scope=scope).claim_launch(permit, now_ns=6)
    effects = EffectLedger(store)
    operation_id = "worker-effect:permit-1"
    effects.prepare(
        operation_id=operation_id,
        attempt_id=attempt.attempt_id,
        effect_class=permit.effect_class,
        conflict_keys=(),
        occurred_at_ns=7,
    )
    effects.transition(
        operation_id=operation_id,
        expected_state="prepared",
        new_state="dispatch_may_have_started",
        revision=1,
        occurred_at_ns=8,
    )
    cas = ReceiptCAS(tmp_path / "cas")
    capture = CaptureSession(store=store, cas=cas, permit=permit)
    gate_input = capture.seal_gate_input(
        capture_id="gate-input:1",
        candidate_id="candidate-1",
        flag="flag{verified}",
        flag_format=FLAG_FORMAT,
        policy_digest=POLICY_DIGEST,
        data=b"worker observed flag{verified}\n",
        occurred_at_ns=9,
    )
    if extra_gate_input:
        capture.seal_gate_input(
            capture_id="gate-input:unused",
            candidate_id="candidate-unused",
            flag="flag{unused}",
            flag_format=FLAG_FORMAT,
            policy_digest=POLICY_DIGEST,
            data=b"worker observed flag{unused}\n",
            occurred_at_ns=9,
        )
    gate = GateAuthority(store=store, cas=cas, artifacts=_Artifacts())
    decision = gate.evaluate(
        evaluation_id=gate.evaluation_id_for(gate_input),
        candidate_id="candidate-1",
        flag="flag{verified}",
        gate_input=gate_input,
        permit=permit,
        flag_format=FLAG_FORMAT,
        policy_digest=POLICY_DIGEST,
        occurred_at_ns=10,
    )
    assert decision.accepted
    flag_digest = canonical_digest("flag{verified}")
    ProgressLedger(store=store).record(
        ProgressOccurrence(
            occurrence_id=f"goal:{flag_digest}",
            branch_id=attempt.branch_id,
            attempt_id=attempt.attempt_id,
            kind=ProgressKind.GOAL_UNIT,
            basis_digest=decision.receipt_digest,
            canonical_seq=store.state().head_seq,
            goal_unit=flag_digest,
        ),
        occurred_at_ns=10,
    )
    effects.transition(
        operation_id=operation_id,
        expected_state="dispatch_may_have_started",
        new_state="observed",
        revision=2,
        occurred_at_ns=11,
    )
    usage = UsageReport.from_observed_and_reservation(
        reserved={"tokens": 20, "wall_ms": 500},
        observed={"tokens": 7, "wall_ms": 100},
        complete_axes=frozenset({"tokens", "wall_ms"}),
    )
    admission.settle(
        attempt_id=attempt.attempt_id,
        usage_report=usage,
        settlement_revision=1,
        occurred_at_ns=12,
    )
    admission_event = store.event_rows(kind="ATTEMPT_ADMITTED")[0]
    launch_event = store.event_rows(kind="WORKER_LAUNCH_PREPARED")[0]
    terminal_payload = {
        "admission_event_digest": admission_event["event_digest"],
        "attempt_digest": permit.lease.attempt.digest,
        "attempt_id": permit.lease.attempt.attempt_id,
        "launch_event_digest": launch_event["event_digest"],
        "lease_digest": permit.lease.digest,
        "lease_id": permit.lease.lease_id,
        "outcome": "observed",
        "permit_digest": permit.digest,
        "permit_id": permit.permit_id,
        "scope_digest": scope.digest,
    }
    store.commit_command(
        command_id="launch-terminal:permit-1",
        idempotency_key="launch-terminal:permit-1",
        command_payload=terminal_payload,
        events=[
            CommandEvent(
                "event:launch-terminal:permit-1",
                "WORKER_TERMINAL",
                "run-supervisor",
                13,
                terminal_payload,
            )
        ],
        projection_mutations=[
            ProjectionMutation(
                "worker_terminal_guard",
                {
                    **terminal_payload,
                    "attempt_digest": permit.lease.attempt.digest,
                    "terminal_event_id": "event:launch-terminal:permit-1",
                },
            )
        ],
        committed_at_ns=13,
    )
    goal_payload = {
        "gate_receipts": (
            (flag_digest, "0" * 64 if malformed_goal else decision.receipt_digest),
        )
    }
    goal_result = store.commit_command(
        command_id="GOAL_COMPLETED:1",
        idempotency_key="GOAL_COMPLETED:1",
        command_payload=goal_payload,
        events=[
            CommandEvent(
                "event:GOAL_COMPLETED:1",
                "GOAL_COMPLETED",
                "protocol2-live-session",
                14,
                goal_payload,
            )
        ],
        projection_mutations=[ProjectionMutation(
            "goal_commit_guard", goal_payload
        )],
        authority_capability=store._lifecycle_commit_capability,
        committed_at_ns=14,
    )
    drain_payload = {"scope_digest": scope.digest}
    drain_result = store.commit_command(
        command_id="EXECUTION_SCOPE_DRAINED:1",
        idempotency_key="EXECUTION_SCOPE_DRAINED:1",
        command_payload=drain_payload,
        events=[
            CommandEvent(
                "event:EXECUTION_SCOPE_DRAINED:1",
                "EXECUTION_SCOPE_DRAINED",
                "protocol2-live-session",
                15,
                drain_payload,
            )
        ],
        projection_mutations=[ProjectionMutation(
            "execution_drain_guard", drain_payload
        )],
        authority_capability=store._lifecycle_commit_capability,
        committed_at_ns=15,
    )
    before = store.runtime_projection_digest()
    after = store.rebuild_runtime_projections()
    assert before == after
    projection_payload = {
        "after": after,
        "before": before,
        "equivalent": True,
        "scope_digest": scope.digest,
    }
    projection_result = store.commit_command(
        command_id="PROJECTION_REBUILD_VERIFIED:1",
        idempotency_key="PROJECTION_REBUILD_VERIFIED:1",
        command_payload=projection_payload,
        events=[
            CommandEvent(
                "event:PROJECTION_REBUILD_VERIFIED:1",
                "PROJECTION_REBUILD_VERIFIED",
                "protocol2-live-session",
                16,
                projection_payload,
            )
        ],
        projection_mutations=[ProjectionMutation(
            "projection_verify_guard", projection_payload
        )],
        authority_capability=store._lifecycle_commit_capability,
        committed_at_ns=16,
    )
    components, schema = _component_bodies(store)
    closure_payload = {
        "all_clean": True,
        "components": components,
        "invariants": {
            "capture_pairs": True,
            "effects_close": True,
            "gate_closes": True,
            "orphan_free": True,
            "usage_closes": True,
        },
        "scope_digest": scope.digest,
        "schema": schema,
        "solved": True,
    }
    closure_result = store.commit_command(
        command_id="S4E_CLOSURE_ATTESTED:1",
        idempotency_key="S4E_CLOSURE_ATTESTED:1",
        command_payload=closure_payload,
        events=[
            CommandEvent(
                "event:S4E_CLOSURE_ATTESTED:1",
                "S4E_CLOSURE_ATTESTED",
                "protocol2-live-session",
                17,
                closure_payload,
            )
        ],
        projection_mutations=[ProjectionMutation(
            "s4e_closure_guard", closure_payload
        )],
        authority_capability=store._lifecycle_commit_capability,
        committed_at_ns=17,
    )
    chain = {
        **components,
        "execution": drain_result.receipt_digest,
        "gate": goal_result.receipt_digest,
        "projection_rebuild": projection_result.receipt_digest,
        "s4e_closure": closure_result.receipt_digest,
    }
    return store, cas, chain, gate_input.raw_digest


def test_resolves_actual_api_lineage_without_writing(tmp_path):
    store, cas, chain, _raw_digest = _solved_closure(tmp_path)
    event_count = len(store.event_rows())
    projection_digest = store.runtime_projection_digest()

    resolved = resolve_s4e_closure(store=store, cas=cas, receipt_chain=chain)

    assert resolved.scope_digest == ExecutionScope("run-1", 1, 1).digest
    assert resolved.attempt_count == 1
    assert resolved.capture_count == 1
    assert resolved.accepted_goal_units == 1
    assert resolved.policy_digest == POLICY_DIGEST
    assert resolved.projection_digest == projection_digest
    assert len(store.event_rows()) == event_count
    assert store.runtime_projection_digest() == projection_digest


def test_rejects_forged_receipt_shape_before_resolution(tmp_path):
    store, cas, chain, _raw_digest = _solved_closure(tmp_path)
    forged = {**chain, "canonical_permit": chain["canonical_permit"].upper()}

    with pytest.raises(ClosureResolutionError, match="lowercase sha256"):
        resolve_s4e_closure(store=store, cas=cas, receipt_chain=forged)


def test_legacy_summary_receipt_cannot_satisfy_complete_closure(tmp_path):
    store, cas, chain, _raw_digest = _solved_closure(tmp_path)
    row = store._conn.execute(
        "SELECT command_id,receipt_json FROM commands "
        "WHERE command_id='GOAL_COMPLETED:1'"
    ).fetchone()
    legacy = json.loads(row[1])
    legacy.pop("canonical_receipt")
    store._conn.execute("DROP TRIGGER commands_no_update")
    store._conn.execute(
        "UPDATE commands SET receipt_json=? WHERE command_id=?",
        (json.dumps(legacy, sort_keys=True), row[0]),
    )

    with pytest.raises(ClosureResolutionError, match="complete committing"):
        resolve_s4e_closure(store=store, cas=cas, receipt_chain=chain)


def test_closure_tail_rejects_concurrent_ordinary_append(tmp_path):
    store, cas, chain, _raw_digest = _solved_closure(tmp_path)
    other = EpistemicSQLiteStore.open(store.path)
    try:
        with pytest.raises(IntegrityError, match="permanently seals"):
            other.commit_command(
                command_id="diagnostic:late",
                idempotency_key="diagnostic:late",
                command_payload={"text": "late"},
                events=[CommandEvent(
                    "event:diagnostic:late",
                    "DIAGNOSTIC_RECORDED",
                    "diagnostic",
                    99,
                    {"text": "late"},
                )],
                committed_at_ns=99,
            )
        with pytest.raises(IntegrityError, match="permanently seals"):
            other.commit_command(
                command_id="goal:invalidated:late",
                idempotency_key="goal:invalidated:late",
                command_payload={"reason": "late"},
                events=[CommandEvent(
                    "event:goal:invalidated:late",
                    "GOAL_INVALIDATED",
                    "untrusted",
                    100,
                    {"reason": "late"},
                )],
                committed_at_ns=100,
            )
        resolved = resolve_s4e_closure(store=store, cas=cas, receipt_chain=chain)
        assert resolved.closure_receipt_digest == chain["s4e_closure"]
    finally:
        other.close()


def test_closure_must_be_the_only_event_and_mutation_in_its_command(tmp_path):
    store = EpistemicSQLiteStore.create(
        path=tmp_path / "epistemic-v2.db",
        run_id="run-closure-shape",
        manifest_digest="a" * 64,
    )
    payload = {"scope_digest": "b" * 64}
    before = store.event_rows()
    with pytest.raises(IntegrityError, match="sole event and sole mutation"):
        store.commit_command(
            command_id="closure-with-tail",
            idempotency_key="closure-with-tail",
            command_payload=payload,
            events=[
                CommandEvent(
                    "event:closure-with-tail",
                    "S4E_CLOSURE_ATTESTED",
                    "lifecycle-authority",
                    1,
                    payload,
                ),
                CommandEvent(
                    "event:diagnostic-after-closure",
                    "DIAGNOSTIC_RECORDED",
                    "diagnostic",
                    1,
                    {"text": "must not share the closure command"},
                ),
            ],
            projection_mutations=[
                ProjectionMutation("s4e_closure_guard", payload)
            ],
            authority_capability=store._lifecycle_commit_capability,
            committed_at_ns=1,
        )
    assert store.event_rows() == before
    with pytest.raises(IntegrityError, match="cannot emit outbox effects"):
        store.commit_command(
            command_id="closure-with-outbox",
            idempotency_key="closure-with-outbox",
            command_payload=payload,
            events=[
                CommandEvent(
                    "event:closure-with-outbox",
                    "S4E_CLOSURE_ATTESTED",
                    "lifecycle-authority",
                    2,
                    payload,
                )
            ],
            outbox=[OutboxIntent("outbox:late", "diagnostic", {"late": True})],
            projection_mutations=[
                ProjectionMutation("s4e_closure_guard", payload)
            ],
            authority_capability=store._lifecycle_commit_capability,
            committed_at_ns=2,
        )
    assert store.event_rows() == before


def test_rejects_count_clean_but_malformed_goal_lineage(tmp_path):
    with pytest.raises(IntegrityError, match="gate receipt does not resolve"):
        _solved_closure(tmp_path, malformed_goal=True)


def test_rejects_cas_mutation_even_when_attestation_digests_still_match(tmp_path):
    store, cas, chain, raw_digest = _solved_closure(tmp_path)
    path = cas.objects / raw_digest[:2] / raw_digest[2:]
    os.chmod(path, 0o600)
    path.write_bytes(b"mutated after sealing")

    with pytest.raises(ClosureResolutionError, match="CAS object"):
        resolve_s4e_closure(store=store, cas=cas, receipt_chain=chain)


def test_rejects_accepted_gate_with_forged_actor(tmp_path, monkeypatch):
    original = authority_module.CommandEvent

    def forged_actor(*args, **kwargs):
        event = original(*args, **kwargs)
        if event.kind == "FLAG_ACCEPTED":
            return original(
                event.event_id,
                event.kind,
                "not-the-hardcoded-gate",
                event.occurred_at_ns,
                event.payload,
            )
        return event

    monkeypatch.setattr(authority_module, "CommandEvent", forged_actor)
    store, cas, chain, _raw_digest = _solved_closure(tmp_path)

    with pytest.raises(ClosureResolutionError, match="authority provenance"):
        resolve_s4e_closure(store=store, cas=cas, receipt_chain=chain)


def test_rejects_unconsumed_gate_input_capture(tmp_path):
    store, cas, chain, _raw_digest = _solved_closure(tmp_path, extra_gate_input=True)

    with pytest.raises(ClosureResolutionError, match="every gate-input capture"):
        resolve_s4e_closure(store=store, cas=cas, receipt_chain=chain)


def test_rejects_launch_event_recorded_after_permit_expiry(tmp_path, monkeypatch):
    original = permit_resolver_module.CommandEvent
    original_commit = EpistemicSQLiteStore.commit_command

    def expired_launch(*args, **kwargs):
        event = original(*args, **kwargs)
        if event.kind == "WORKER_LAUNCH_PREPARED":
            return original(
                event.event_id,
                event.kind,
                event.actor,
                20_000,
                event.payload,
            )
        return event

    monkeypatch.setattr(permit_resolver_module, "CommandEvent", expired_launch)

    def late_commit(self, *args, **kwargs):
        if any(
            event.kind == "WORKER_LAUNCH_PREPARED" for event in kwargs.get("events", ())
        ):
            kwargs["committed_at_ns"] = 20_000
        return original_commit(self, *args, **kwargs)

    monkeypatch.setattr(EpistemicSQLiteStore, "commit_command", late_commit)
    store, cas, chain, _raw_digest = _solved_closure(tmp_path)

    with pytest.raises(ClosureResolutionError, match="permit lifetime"):
        resolve_s4e_closure(store=store, cas=cas, receipt_chain=chain)


def test_rejects_accepted_gate_without_immutable_outbox(tmp_path, monkeypatch):
    original = EpistemicSQLiteStore.commit_command

    def drop_gate_outbox(self, *args, **kwargs):
        if any(event.kind == "FLAG_ACCEPTED" for event in kwargs.get("events", ())):
            kwargs["outbox"] = ()
        return original(self, *args, **kwargs)

    monkeypatch.setattr(EpistemicSQLiteStore, "commit_command", drop_gate_outbox)
    with pytest.raises(IntegrityError, match="exactly one immutable outbox"):
        _solved_closure(tmp_path)
