"""Execution-layer network policy readback and sealed egress receipts."""

from __future__ import annotations

from collections.abc import Mapping, Sequence
from dataclasses import dataclass
from typing import Protocol

from muteki.epistemic.contracts import canonical_digest
from muteki.epistemic.sqlite_store import CommandEvent, EpistemicSQLiteStore
from muteki.runtime.contracts import LeaseIdentity


class NetworkPolicyAdapter(Protocol):
    def apply(self, policy: Mapping) -> Mapping: ...
    def readback(self) -> Mapping: ...


class NetworkPolicyUnknown(RuntimeError):
    pass


@dataclass(frozen=True, slots=True)
class EnforcedNetworkPolicy:
    policy_digest: str
    mode: str
    allowlist: tuple[str, ...]
    enforcement_receipt_digest: str


class NetworkPolicyAuthority:
    def __init__(self, *, store: EpistemicSQLiteStore,
                 adapter: NetworkPolicyAdapter) -> None:
        self._store = store
        self._adapter = adapter

    def apply_and_readback(self, *, operation_id: str, mode: str,
                           allowlist: Sequence[str], occurred_at_ns: int) -> EnforcedNetworkPolicy:
        if mode not in {"none", "allowlist"}:
            raise ValueError("network policy mode must be none or allowlist")
        policy = {"allowlist": tuple(sorted(set(allowlist))), "mode": mode}
        expected = canonical_digest(policy)
        applied = self._adapter.apply(policy)
        readback = self._adapter.readback()
        if canonical_digest(applied) != expected or canonical_digest(readback) != expected:
            raise NetworkPolicyUnknown("execution policy apply/readback mismatch")
        result = self._store.commit_command(
            command_id=f"network-policy:{operation_id}",
            idempotency_key=f"network-policy:{operation_id}",
            command_payload={"operation_id": operation_id, "policy": policy},
            events=[CommandEvent(
                f"event:network-policy:{operation_id}", "NETWORK_POLICY_ENFORCED",
                "network-policy-authority", occurred_at_ns,
                {"operation_id": operation_id, "policy_digest": expected,
                 "readback_digest": canonical_digest(readback)})],
            committed_at_ns=occurred_at_ns,
        )
        return EnforcedNetworkPolicy(expected, mode, tuple(policy["allowlist"]),
                                     result.receipt_digest)

    def record_egress(self, *, receipt_id: str, lease: LeaseIdentity,
                      destination: str, policy: EnforcedNetworkPolicy,
                      occurred_at_ns: int, observed: bool = False,
                      observation_digest: str = "") -> str:
        allowed = (policy.mode == "allowlist" and destination in policy.allowlist)
        if observed and len(observation_digest) != 64:
            raise NetworkPolicyUnknown("observed egress requires a sealed readback")
        result = self._store.commit_command(
            command_id=f"egress:{receipt_id}", idempotency_key=f"egress:{receipt_id}",
            command_payload={"allowed": allowed, "destination": destination,
                             "lease_digest": lease.digest,
                             "observed": bool(observed),
                             "observation_digest": observation_digest,
                             "policy_digest": policy.policy_digest},
            events=[CommandEvent(
                f"event:egress:{receipt_id}", "EGRESS_RECEIPT",
                "network-policy-authority", occurred_at_ns,
                {"allowed": allowed, "destination": destination,
                 "lease_digest": lease.digest,
                 "observed": bool(observed),
                 "observation_digest": observation_digest,
                 "policy_digest": policy.policy_digest})],
            committed_at_ns=occurred_at_ns,
        )
        return result.receipt_digest
