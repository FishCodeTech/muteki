"""Fail-closed Protocol 2 canary admission; no production default-on switch."""

from __future__ import annotations

from dataclasses import dataclass
from enum import Enum
from typing import Mapping


CANARY_DIGEST_VERSION = 2


class CanaryLevel(str, Enum):
    SYNTHETIC = "synthetic"
    IN_PROCESS = "in_process"
    LIVE_LOCAL = "live_local"


class CanaryRejected(RuntimeError):
    pass


@dataclass(frozen=True, slots=True)
class CanaryEvidence:
    level: CanaryLevel
    receipts: Mapping[str, str]
    fault_suite_green: bool
    gate_equivalent: bool
    projection_rebuild_equivalent: bool


_REQUIRED = {
    CanaryLevel.SYNTHETIC: {"baseline", "schema", "fault_suite"},
    CanaryLevel.IN_PROCESS: {"baseline", "schema", "fault_suite", "kernel", "cas"},
    CanaryLevel.LIVE_LOCAL: {
        "baseline",
        "schema",
        "fault_suite",
        "kernel",
        "cas",
        "admission",
        "network_policy",
        "egress",
        "egress_observation",
        "eval_assignment",
        "platform_admission",
        "platform_supervisor",
        "platform_cleanup",
    },
}

# An older LIVE_LOCAL receipt remains readable for audit, but it is not sufficient
# to re-enable production after the S4-E audit.  These keys are emitted only by the
# hardened host path once their canonical objects have been resolved.
S4E_LIVE_REQUIRED = frozenset(
    {
        "canonical_permit",
        "capture_manifest",
        "execution",
        "gate",
        "gate_input",
        "orphan_summary",
        "projection_rebuild",
        "s4e_closure",
        "s4e_schema",
        "usage_closure",
    }
)


def missing_s4e_receipts(receipts: Mapping[str, str]) -> tuple[str, ...]:
    return tuple(sorted(S4E_LIVE_REQUIRED - set(receipts)))


def admit_canary(evidence: CanaryEvidence) -> str:
    if type(evidence.level) is not CanaryLevel:
        raise CanaryRejected("canary level is malformed")
    if not isinstance(evidence.receipts, Mapping):
        raise CanaryRejected("canary receipts are malformed")
    missing = sorted(_REQUIRED[evidence.level] - set(evidence.receipts))
    if missing:
        raise CanaryRejected(f"missing canary receipts: {', '.join(missing)}")
    if type(evidence.fault_suite_green) is not bool or not evidence.fault_suite_green:
        raise CanaryRejected("fault suite is not green")
    if type(evidence.gate_equivalent) is not bool or not evidence.gate_equivalent:
        raise CanaryRejected("hardcoded gate equivalence failed")
    if (
        type(evidence.projection_rebuild_equivalent) is not bool
        or not evidence.projection_rebuild_equivalent
    ):
        raise CanaryRejected("projection rebuild equivalence failed")
    if any(
        type(name) is not str
        or not name
        or name != name.strip()
        or type(digest) is not str
        or len(digest) != 64
        or any(character not in "0123456789abcdef" for character in digest)
        for name, digest in evidence.receipts.items()
    ):
        raise CanaryRejected("receipt digest is malformed")
    from muteki.epistemic.contracts import canonical_digest

    return canonical_digest(
        {
            "canary_digest_version": CANARY_DIGEST_VERSION,
            "fault_suite_green": evidence.fault_suite_green,
            "gate_equivalent": evidence.gate_equivalent,
            "level": evidence.level.value,
            "projection_rebuild_equivalent": (
                evidence.projection_rebuild_equivalent
            ),
            "receipts": dict(evidence.receipts),
        }
    )
