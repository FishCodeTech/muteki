from __future__ import annotations

import inspect

import pytest

from muteki.epistemic.contracts import (
    CanonicalReceipt,
    CanonicalValueError,
    EventEnvelopeV2,
    canonical_digest,
    canonical_json_bytes,
)
from muteki.runtime.contracts import AttemptIdentity, ExecutionScope, LeaseIdentity
import muteki.runtime.ports as ports


def test_canonical_json_is_order_independent_and_unicode_stable():
    left = {"z": [3, "無敵"], "a": {"b": True}}
    right = {"a": {"b": True}, "z": [3, "無敵"]}
    assert canonical_json_bytes(left) == canonical_json_bytes(right)
    assert canonical_digest(left) == canonical_digest(right)


@pytest.mark.parametrize("value", [1.0, float("nan"), b"bytes", {1: "bad-key"}])
def test_canonical_json_rejects_ambiguous_values(value):
    with pytest.raises(CanonicalValueError):
        canonical_json_bytes(value)


def test_event_and_receipt_payloads_are_immutable_and_hashable():
    source = {"nested": ["a", 1]}
    event = EventEnvelopeV2(
        event_id="E1", run_id="R1", command_id="C1", ordinal=0,
        kind="OBSERVED", actor="host", occurred_at_ns=1, payload=source,
    )
    receipt = CanonicalReceipt(
        receipt_id="RC1", run_id="R1", command_id="C1",
        kind="COMMAND_COMMITTED", payload={"event": event.digest},
    )
    source["nested"].append("mutated")
    assert event.payload["nested"] == ("a", 1)
    assert len(event.digest) == 64
    assert len(receipt.digest) == 64
    with pytest.raises(TypeError):
        event.payload["x"] = "no"  # type: ignore[index]


def test_execution_identity_layers_do_not_alias():
    scope = ExecutionScope("run-1", run_fence_epoch=2, execution_generation=3)
    attempt = AttemptIdentity(scope, "branch-1", "attempt-1", launch_ordinal=1)
    lease = LeaseIdentity(attempt, "lease-1", lease_epoch=4, worker_generation=5)
    assert len({scope.digest, attempt.digest, lease.digest}) == 3


def test_ports_have_no_legacy_or_storage_dependency():
    source = inspect.getsource(ports)
    assert "shared_graph" not in source
    assert "sqlite3" not in source
    expected = {
        "CandidateIngressPort", "CanonicalReadPort", "SearchStatePort",
        "OperatorAuthorityPort", "CapturePort", "GateCommitPort",
        "RunLifecyclePort", "RunViewPort", "AttemptLauncherPort",
        "PlatformAdmissionPort", "PlatformLauncherPort",
        "PlatformSupervisorPort", "NetworkPolicyPort", "EgressReceiptPort",
    }
    assert expected <= set(vars(ports))
