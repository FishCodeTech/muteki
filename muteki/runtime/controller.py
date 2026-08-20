"""Deterministic authority matrix and process-local live-health guard."""

from __future__ import annotations

from dataclasses import dataclass
from enum import Enum

from muteki.epistemic.folds import CanonicalState, KernelHealth, RunExecution, SearchControlMode


class GuardMode(str, Enum):
    DENY = "deny"
    PROVISION_FINALIZE_ONLY = "provision_finalize_only"
    BOOT_FINALIZE_ONLY = "boot_finalize_only"
    ALLOW_MUTATION = "allow_mutation"
    EMERGENCY_ONLY = "emergency_only"


class CommandClass(str, Enum):
    ORDINARY = "ordinary"
    DISPATCH = "dispatch"
    PROMOTION = "promotion"
    GATE = "gate"
    OUTBOX_DRAIN = "outbox_drain"
    INVALIDATION = "invalidation"
    RECOVERY = "recovery"
    EMERGENCY_STOP = "emergency_stop"


@dataclass(frozen=True, slots=True)
class BootRecoveryCapability:
    boot_epoch: int
    writer_epoch: int
    owner_nonce: str

    def __post_init__(self) -> None:
        if (
            type(self.boot_epoch) is not int
            or type(self.writer_epoch) is not int
            or self.boot_epoch < 1
            or self.writer_epoch < 1
            or type(self.owner_nonce) is not str
            or not self.owner_nonce
            or self.owner_nonce != self.owner_nonce.strip()
        ):
            raise ValueError("invalid boot recovery capability")


class AuthorityDenied(RuntimeError):
    pass


class LiveHealthGuard:
    """A persisted READY bit can never open this process-local guard."""

    def __init__(self) -> None:
        self._mode = GuardMode.DENY
        self._capability: BootRecoveryCapability | None = None
        self._attestation_digest = ""

    @property
    def mode(self) -> GuardMode:
        return self._mode

    def begin_boot_finalize(self, capability: BootRecoveryCapability) -> None:
        if self._mode is not GuardMode.DENY:
            raise AuthorityDenied("guard is already owned")
        self._capability = capability
        self._mode = GuardMode.BOOT_FINALIZE_ONLY

    def open_admission(self, *, capability: BootRecoveryCapability,
                       attestation_digest: str) -> None:
        if self._mode is not GuardMode.BOOT_FINALIZE_ONLY:
            raise AuthorityDenied("boot finalize mode is not active")
        if capability != self._capability or len(attestation_digest) != 64:
            raise AuthorityDenied("boot capability or attestation mismatch")
        self._attestation_digest = attestation_digest
        self._mode = GuardMode.ALLOW_MUTATION

    def deny(self, *, emergency_only: bool = False) -> None:
        self._mode = (GuardMode.EMERGENCY_ONLY if emergency_only else GuardMode.DENY)
        self._capability = None
        self._attestation_digest = ""

    def authorize(self, command_class: CommandClass, state: CanonicalState) -> None:
        if command_class is CommandClass.EMERGENCY_STOP:
            if self._mode in {GuardMode.EMERGENCY_ONLY, GuardMode.ALLOW_MUTATION,
                              GuardMode.BOOT_FINALIZE_ONLY}:
                return
            raise AuthorityDenied("emergency stop requires a live process owner")
        if command_class is CommandClass.RECOVERY:
            if self._mode is GuardMode.BOOT_FINALIZE_ONLY:
                return
            raise AuthorityDenied("recovery requires the boot-scoped capability")
        if self._mode is not GuardMode.ALLOW_MUTATION:
            raise AuthorityDenied("ordinary authority is closed")
        if state.kernel_health is not KernelHealth.READY:
            raise AuthorityDenied("kernel is not READY")
        if command_class in {CommandClass.ORDINARY, CommandClass.DISPATCH,
                             CommandClass.PROMOTION, CommandClass.GATE}:
            if (state.run_execution is not RunExecution.RUNNING
                    or state.search_mode is not SearchControlMode.ACTIVE):
                raise AuthorityDenied("run/search state forbids new authoritative work")
            return
        if command_class in {CommandClass.OUTBOX_DRAIN, CommandClass.INVALIDATION}:
            if state.run_execution in {
                RunExecution.RUNNING, RunExecution.QUIESCING, RunExecution.STOPPED,
                RunExecution.REOPEN_REQUIRED, RunExecution.ARCHIVED,
            }:
                return
        raise AuthorityDenied("command class is not allowed in the current state")
