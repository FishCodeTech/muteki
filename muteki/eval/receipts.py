from __future__ import annotations

from dataclasses import dataclass, field
from enum import Enum

from muteki.epistemic.contracts import canonical_digest
from muteki.eval.manifests import (
    EVAL_CONTRACT_VERSION,
    TrialIdentity,
    _dimension_tuple,
    _exact_text,
    _lower_sha256,
)


class TrialOutcome(str, Enum):
    SOLVED = "solved"
    UNSOLVED = "unsolved"
    NOT_STARTED = "not_started"
    INFRA_FAILURE = "infra_failure"
    POLICY_BLOCKED = "policy_blocked"


class MissingnessVerdict(str, Enum):
    COMPLETE = "complete"
    PRE_PROVISION_CRASH = "pre_provision_crash"
    POST_PROVISION_PRE_LAUNCH = "post_provision_pre_launch"
    POST_LAUNCH_INCOMPLETE = "post_launch_incomplete"
    RECEIPT_INCOMPLETE = "receipt_incomplete"


@dataclass(frozen=True, slots=True)
class UsageCounterStream:
    dimensions: tuple[tuple[str, int], ...] = ()
    complete: bool = False

    def __post_init__(self) -> None:
        object.__setattr__(
            self,
            "dimensions",
            _dimension_tuple(self.dimensions, "usage dimensions", allow_empty=True),
        )
        if type(self.complete) is not bool:
            raise TypeError("usage complete must be a boolean")
        if self.complete and not self.dimensions:
            raise ValueError("complete usage dimensions must not be empty")


@dataclass(frozen=True, slots=True)
class EvalTrialReceipt:
    identity: TrialIdentity
    assignment_digest: str
    outcome: TrialOutcome
    missingness: MissingnessVerdict
    provision_receipt_digest: str = ""
    launch_receipt_digest: str = ""
    provider_receipt_digest: str = ""
    trace_digest: str = ""
    gate_receipt_digest: str = ""
    oracle_receipt_digest: str = ""
    policy_receipt_digest: str = ""
    usage: UsageCounterStream = field(default_factory=UsageCounterStream)
    infra_axis: str = "ok"
    contract_version: int = EVAL_CONTRACT_VERSION

    def __post_init__(self) -> None:
        if type(self.identity) is not TrialIdentity:
            raise TypeError("identity must be TrialIdentity")
        object.__setattr__(
            self,
            "assignment_digest",
            _lower_sha256(self.assignment_digest, "assignment_digest"),
        )
        if type(self.outcome) is not TrialOutcome:
            raise TypeError("outcome must be TrialOutcome")
        if type(self.missingness) is not MissingnessVerdict:
            raise TypeError("missingness must be MissingnessVerdict")
        for name in (
            "provision_receipt_digest",
            "launch_receipt_digest",
            "provider_receipt_digest",
            "trace_digest",
            "gate_receipt_digest",
            "oracle_receipt_digest",
            "policy_receipt_digest",
        ):
            object.__setattr__(
                self,
                name,
                _lower_sha256(getattr(self, name), name, allow_empty=True),
            )
        if type(self.usage) is not UsageCounterStream:
            raise TypeError("usage must be UsageCounterStream")
        object.__setattr__(
            self, "infra_axis", _exact_text(self.infra_axis, "infra_axis")
        )
        if (
            type(self.contract_version) is not int
            or self.contract_version != EVAL_CONTRACT_VERSION
        ):
            raise ValueError("unsupported evaluation contract version")

    @property
    def strict_valid(self) -> bool:
        """Return structural completeness only, never canonical receipt authority.

        ``aggregate_study`` additionally checks the frozen assignment and budget,
        but this generic layer deliberately cannot prove that digest-shaped fields
        resolve in a run store.  Its aggregation is always non-promotion evidence.
        """

        if self.contract_version != EVAL_CONTRACT_VERSION:
            return False
        if self.missingness is not MissingnessVerdict.COMPLETE:
            return False
        required = (
            self.provision_receipt_digest,
            self.launch_receipt_digest,
            self.provider_receipt_digest,
            self.trace_digest,
            self.oracle_receipt_digest,
            self.policy_receipt_digest,
        )
        if not all(required) or not self.usage.complete:
            return False
        if self.outcome is TrialOutcome.SOLVED and not self.gate_receipt_digest:
            return False
        return True

    @property
    def digest(self) -> str:
        return canonical_digest(
            {
                "assignment_digest": self.assignment_digest,
                "contract_version": self.contract_version,
                "gate_receipt_digest": self.gate_receipt_digest,
                "identity_digest": self.identity.digest,
                "infra_axis": self.infra_axis,
                "launch_receipt_digest": self.launch_receipt_digest,
                "missingness": self.missingness.value,
                "oracle_receipt_digest": self.oracle_receipt_digest,
                "outcome": self.outcome.value,
                "policy_receipt_digest": self.policy_receipt_digest,
                "provider_receipt_digest": self.provider_receipt_digest,
                "provision_receipt_digest": self.provision_receipt_digest,
                "trace_digest": self.trace_digest,
                "usage": dict(self.usage.dimensions),
                "usage_complete": self.usage.complete,
            }
        )
