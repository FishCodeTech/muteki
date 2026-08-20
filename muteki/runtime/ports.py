"""Narrow authority ports used by every Protocol 2 composition root.

This module contains contracts only.  It must not import SQLite, the legacy shared
graph, web adapters, or concrete process/container launchers.
"""

from __future__ import annotations

from collections.abc import Mapping, Sequence
from dataclasses import dataclass, field
from typing import Any, Protocol, runtime_checkable

from muteki.runtime.contracts import AttemptIdentity, AttemptPermit, ExecutionScope, LeaseIdentity


@dataclass(frozen=True, slots=True)
class CandidateEnvelope:
    candidate_id: str
    lease: LeaseIdentity
    kind: str
    payload: Mapping[str, Any] = field(default_factory=dict)
    artifact_digests: tuple[str, ...] = ()


@dataclass(frozen=True, slots=True)
class CaptureChunk:
    capture_id: str
    lease: LeaseIdentity
    stream: str
    ordinal: int
    raw_digest: str
    byte_count: int
    terminal: bool = False


@dataclass(frozen=True, slots=True)
class PlatformPermit:
    operation_id: str
    permit_id: str
    reservation_ids: tuple[str, ...]
    conflict_keys: tuple[str, ...]
    expires_at_ns: int


@runtime_checkable
class CandidateIngressPort(Protocol):
    def submit_candidate(self, candidate: CandidateEnvelope) -> str: ...


@runtime_checkable
class CanonicalReadPort(Protocol):
    def read_run(self, run_id: str) -> Mapping[str, Any]: ...
    def read_facts(self, run_id: str, *, after_seq: int = 0) -> Sequence[Mapping[str, Any]]: ...
    def head(self, run_id: str) -> tuple[int, str]: ...


@runtime_checkable
class SearchStatePort(Protocol):
    def query_legacy_candidates(self, *, run_id: str) -> Sequence[Mapping[str, Any]]: ...
    def claim_legacy_occurrence(self, *, run_id: str, occurrence_id: str,
                                worker_id: str, lease_until_ns: int) -> bool: ...
    def admit_attempt(self, *, attempt: AttemptIdentity,
                      requested_budget: Mapping[str, int],
                      conflict_keys: Sequence[str]) -> AttemptPermit: ...


@runtime_checkable
class OperatorAuthorityPort(Protocol):
    def submit_operator_command(self, *, run_id: str, command_id: str,
                                action: str, payload: Mapping[str, Any]) -> str: ...


@runtime_checkable
class CapturePort(Protocol):
    def capture(self, *, lease: LeaseIdentity, stream: str,
                ordinal: int, data: bytes, terminal: bool = False) -> CaptureChunk: ...


@runtime_checkable
class GateCommitPort(Protocol):
    def commit_gate_evaluation(self, *, candidate_id: str,
                               gate_input_digest: str,
                               policy_digest: str) -> str: ...


@runtime_checkable
class RunLifecyclePort(Protocol):
    def start_execution(self, *, run_id: str,
                        idempotency_key: str) -> ExecutionScope: ...
    def request_archive(self, *, run_id: str, idempotency_key: str) -> str: ...


@runtime_checkable
class RunViewPort(Protocol):
    def get_run_view(self, run_id: str) -> Mapping[str, Any]: ...
    def events(self, run_id: str, *, after_seq: int = 0) -> Sequence[Mapping[str, Any]]: ...


@runtime_checkable
class AttemptLauncherPort(Protocol):
    async def launch_attempt(self, permit: AttemptPermit) -> str: ...
    async def terminate_attempt(self, lease: LeaseIdentity, *, reason: str) -> str: ...


@runtime_checkable
class PlatformAdmissionPort(Protocol):
    def admit_platform_operation(self, *, operation_id: str,
                                 requested_budget: Mapping[str, int],
                                 conflict_keys: Sequence[str]) -> PlatformPermit: ...


@runtime_checkable
class PlatformLauncherPort(Protocol):
    async def launch_platform_operation(self, permit: PlatformPermit) -> str: ...


@runtime_checkable
class PlatformSupervisorPort(Protocol):
    async def terminate_platform_operation(self, operation_id: str, *, reason: str) -> str: ...
    def status(self, operation_id: str) -> Mapping[str, Any]: ...


@runtime_checkable
class NetworkPolicyPort(Protocol):
    def apply_and_readback(self, *, operation_id: str,
                           policy: Mapping[str, Any]) -> str: ...


@runtime_checkable
class EgressReceiptPort(Protocol):
    def record_egress(self, *, lease: LeaseIdentity, destination: str,
                      policy_digest: str, allowed: bool) -> str: ...
