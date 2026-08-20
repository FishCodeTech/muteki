"""Pure run-pinned engine identity and independence contract.

The contract prevents a cosmetic driver/profile rename from being counted as an
independent cognitive source.  It is deliberately inert: registrations are value
objects, the receipt is a deterministic integrity digest, and the reducer writes no
store, performs no routing, and grants no verification or gate authority.

The three grades are intentionally cumulative and increasingly demanding:

* process-disjoint requires distinct session and workspace identities in one run;
* configured-source-disjoint additionally requires distinct normalized source groups
  and a real change in the bound implementation/configuration fingerprint; and
* provider-attested-disjoint additionally requires distinct non-opaque provider
  identities with separate attestation receipts.

Configured identity is not provider attestation.  In particular, an opaque provider
can reach the configured-source grade but can never reach the provider-attested grade.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from enum import Enum
from typing import Any

from muteki.epistemic.contracts import canonical_digest


COGNITIVE_ENGINE_REGISTRY_SCHEMA_ID = (
    "muteki.runtime-cognitive-engine-registry-entry.v1"
)
COGNITIVE_ENGINE_REGISTRY_RECEIPT_SCHEMA_ID = (
    "muteki.runtime-cognitive-engine-registry-receipt.v1"
)
COGNITIVE_ENGINE_INDEPENDENCE_ASSESSMENT_SCHEMA_ID = (
    "muteki.runtime-cognitive-engine-independence-assessment.v1"
)
COGNITIVE_ENGINE_INDEPENDENCE_REDUCER_VERSION = (
    "muteki.runtime-cognitive-engine-independence-reducer.v1"
)

PRODUCTION_ENABLED = False
AUTHORITY_EFFECT = "NONE"
PROVENANCE_GATE_ACCEPTED_SET = "UNCHANGED"


def _digest(value: object, name: str) -> str:
    if (
        type(value) is not str
        or len(value) != 64
        or any(character not in "0123456789abcdef" for character in value)
    ):
        raise ValueError(f"{name} must be a lowercase sha256 digest")
    return value


def _optional_digest(value: object, name: str) -> str | None:
    if value is None:
        return None
    return _digest(value, name)


@dataclass(frozen=True, slots=True)
class RunPinnedEngineIdentityV1:
    """One actual launch identity registered for exactly one canonical run.

    ``actual_launch_profile_digest`` and ``driver_executable_token_digest`` are the
    values observed at the launch boundary, not requested profile labels.  The
    registry does not prove that the supplied source group or provider attestation is
    true; it makes their exact use replayable and prevents later field substitution.
    """

    run_scope_digest: str
    launch_material_digest: str
    actual_launch_profile_digest: str
    driver_implementation_digest: str
    driver_build_digest: str
    driver_executable_token_digest: str
    engine_identity_digest: str
    provider_identity_digest: str | None
    model_identity_digest: str
    tool_policy_digest: str
    session_digest: str
    workspace_digest: str
    source_group_digest: str
    provider_attestation_receipt_digest: str | None = None
    registry_receipt_digest: str = field(init=False)

    def __post_init__(self) -> None:
        for name in (
            "run_scope_digest",
            "launch_material_digest",
            "actual_launch_profile_digest",
            "driver_implementation_digest",
            "driver_build_digest",
            "driver_executable_token_digest",
            "engine_identity_digest",
            "model_identity_digest",
            "tool_policy_digest",
            "session_digest",
            "workspace_digest",
            "source_group_digest",
        ):
            object.__setattr__(self, name, _digest(getattr(self, name), name))
        object.__setattr__(
            self,
            "provider_identity_digest",
            _optional_digest(
                self.provider_identity_digest,
                "provider_identity_digest",
            ),
        )
        object.__setattr__(
            self,
            "provider_attestation_receipt_digest",
            _optional_digest(
                self.provider_attestation_receipt_digest,
                "provider_attestation_receipt_digest",
            ),
        )
        if (
            self.provider_identity_digest is None
            and self.provider_attestation_receipt_digest is not None
        ):
            raise ValueError("an opaque provider cannot carry an attestation receipt")
        object.__setattr__(
            self,
            "registry_receipt_digest",
            canonical_digest(self.registry_receipt_body()),
        )

    @property
    def source_identity_fingerprint_digest(self) -> str:
        """Alias-resistant fingerprint; profile/session/workspace labels are excluded."""

        return canonical_digest(
            {
                "driver_build_digest": self.driver_build_digest,
                "driver_executable_token_digest": (
                    self.driver_executable_token_digest
                ),
                "driver_implementation_digest": (
                    self.driver_implementation_digest
                ),
                "engine_identity_digest": self.engine_identity_digest,
                "model_identity_digest": self.model_identity_digest,
                "provider_identity_digest": self.provider_identity_digest,
                "tool_policy_digest": self.tool_policy_digest,
            }
        )

    def canonical_body(self) -> dict[str, Any]:
        return {
            "actual_launch_profile_digest": self.actual_launch_profile_digest,
            "driver_build_digest": self.driver_build_digest,
            "driver_executable_token_digest": self.driver_executable_token_digest,
            "driver_implementation_digest": self.driver_implementation_digest,
            "engine_identity_digest": self.engine_identity_digest,
            "launch_material_digest": self.launch_material_digest,
            "model_identity_digest": self.model_identity_digest,
            "provider_attestation_receipt_digest": (
                self.provider_attestation_receipt_digest
            ),
            "provider_identity_digest": self.provider_identity_digest,
            "run_scope_digest": self.run_scope_digest,
            "schema_id": COGNITIVE_ENGINE_REGISTRY_SCHEMA_ID,
            "session_digest": self.session_digest,
            "source_group_digest": self.source_group_digest,
            "source_identity_fingerprint_digest": (
                self.source_identity_fingerprint_digest
            ),
            "tool_policy_digest": self.tool_policy_digest,
            "workspace_digest": self.workspace_digest,
        }

    @property
    def digest(self) -> str:
        return canonical_digest(self.canonical_body())

    def registry_receipt_body(self) -> dict[str, Any]:
        return {
            "authority_boundary": {
                "accepted_set_change": False,
                "production_enabled": False,
                "routing_authority": False,
                "store_write_authority": False,
                "verification_authority": False,
            },
            "entry_digest": self.digest,
            "run_scope_digest": self.run_scope_digest,
            "schema_id": COGNITIVE_ENGINE_REGISTRY_RECEIPT_SCHEMA_ID,
        }


class EngineIndependenceStateV1(str, Enum):
    PROVEN = "proven"
    NOT_DISJOINT = "not_disjoint"
    UNPROVEN = "unproven"


class EngineIndependenceGradeV1(str, Enum):
    UNPROVEN = "unproven"
    NOT_DISJOINT = "not_disjoint"
    PROCESS_DISJOINT = "process_disjoint"
    CONFIGURED_SOURCE_DISJOINT = "configured_source_disjoint"
    PROVIDER_ATTESTED_DISJOINT = "provider_attested_disjoint"


@dataclass(frozen=True, slots=True)
class EngineIndependenceAssessmentV1:
    """Replayable reducer output; never an evidence or admission command."""

    source_registry_receipt_digest: str
    reproducer_registry_receipt_digest: str
    run_scope_digest: str | None
    process_disjoint: EngineIndependenceStateV1
    configured_source_disjoint: EngineIndependenceStateV1
    provider_attested_disjoint: EngineIndependenceStateV1
    highest_grade: EngineIndependenceGradeV1
    independent_reproducer_eligible: bool
    reasons: tuple[str, ...]

    def __post_init__(self) -> None:
        for name in (
            "source_registry_receipt_digest",
            "reproducer_registry_receipt_digest",
        ):
            object.__setattr__(self, name, _digest(getattr(self, name), name))
        object.__setattr__(
            self,
            "run_scope_digest",
            _optional_digest(self.run_scope_digest, "run_scope_digest"),
        )
        for name in (
            "process_disjoint",
            "configured_source_disjoint",
            "provider_attested_disjoint",
        ):
            if type(getattr(self, name)) is not EngineIndependenceStateV1:
                raise TypeError(f"{name} must be EngineIndependenceStateV1")
        if type(self.highest_grade) is not EngineIndependenceGradeV1:
            raise TypeError("highest_grade must be EngineIndependenceGradeV1")
        if type(self.independent_reproducer_eligible) is not bool:
            raise TypeError("independent_reproducer_eligible must be an exact boolean")
        if (
            type(self.reasons) is not tuple
            or not self.reasons
            or any(type(reason) is not str or not reason for reason in self.reasons)
            or tuple(sorted(set(self.reasons))) != self.reasons
        ):
            raise ValueError("reasons must be a sorted unique non-empty string tuple")

    def canonical_body(self) -> dict[str, Any]:
        return {
            "authority_boundary": {
                "accepted_set_change": False,
                "production_enabled": False,
                "routing_authority": False,
                "store_write_authority": False,
                "verification_authority": False,
            },
            "configured_source_disjoint": self.configured_source_disjoint.value,
            "highest_grade": self.highest_grade.value,
            "independent_reproducer_eligible": (
                self.independent_reproducer_eligible
            ),
            "process_disjoint": self.process_disjoint.value,
            "provider_attested_disjoint": self.provider_attested_disjoint.value,
            "reasons": self.reasons,
            "reducer_version": COGNITIVE_ENGINE_INDEPENDENCE_REDUCER_VERSION,
            "reproducer_registry_receipt_digest": (
                self.reproducer_registry_receipt_digest
            ),
            "run_scope_digest": self.run_scope_digest,
            "schema_id": COGNITIVE_ENGINE_INDEPENDENCE_ASSESSMENT_SCHEMA_ID,
            "source_registry_receipt_digest": (
                self.source_registry_receipt_digest
            ),
        }

    @property
    def digest(self) -> str:
        return canonical_digest(self.canonical_body())


def reduce_engine_independence_v1(
    source: RunPinnedEngineIdentityV1,
    reproducer: RunPinnedEngineIdentityV1,
) -> EngineIndependenceAssessmentV1:
    """Derive conservative independence grades from two registered launches."""

    if type(source) is not RunPinnedEngineIdentityV1:
        raise TypeError("source must be RunPinnedEngineIdentityV1")
    if type(reproducer) is not RunPinnedEngineIdentityV1:
        raise TypeError("reproducer must be RunPinnedEngineIdentityV1")

    reasons: set[str] = set()
    if source.run_scope_digest != reproducer.run_scope_digest:
        reasons.add("cross_run_identity_not_comparable")
        return EngineIndependenceAssessmentV1(
            source_registry_receipt_digest=source.registry_receipt_digest,
            reproducer_registry_receipt_digest=(
                reproducer.registry_receipt_digest
            ),
            run_scope_digest=None,
            process_disjoint=EngineIndependenceStateV1.UNPROVEN,
            configured_source_disjoint=EngineIndependenceStateV1.UNPROVEN,
            provider_attested_disjoint=EngineIndependenceStateV1.UNPROVEN,
            highest_grade=EngineIndependenceGradeV1.UNPROVEN,
            independent_reproducer_eligible=False,
            reasons=tuple(sorted(reasons)),
        )

    if source.session_digest == reproducer.session_digest:
        reasons.add("shared_session")
    if source.workspace_digest == reproducer.workspace_digest:
        reasons.add("shared_workspace")
    process = (
        EngineIndependenceStateV1.PROVEN
        if not reasons
        else EngineIndependenceStateV1.NOT_DISJOINT
    )

    if process is not EngineIndependenceStateV1.PROVEN:
        configured = EngineIndependenceStateV1.NOT_DISJOINT
        reasons.add("process_disjointness_required")
    elif source.source_group_digest == reproducer.source_group_digest:
        configured = EngineIndependenceStateV1.NOT_DISJOINT
        reasons.add("shared_source_group")
    elif (
        source.source_identity_fingerprint_digest
        == reproducer.source_identity_fingerprint_digest
    ):
        configured = EngineIndependenceStateV1.NOT_DISJOINT
        reasons.add("source_group_relabel_without_source_change")
    else:
        configured = EngineIndependenceStateV1.PROVEN

    if configured is not EngineIndependenceStateV1.PROVEN:
        provider_attested = EngineIndependenceStateV1.NOT_DISJOINT
        reasons.add("configured_source_disjointness_required")
    elif (
        source.provider_identity_digest is None
        or reproducer.provider_identity_digest is None
    ):
        provider_attested = EngineIndependenceStateV1.UNPROVEN
        reasons.add("provider_identity_opaque_or_missing")
    elif (
        source.provider_attestation_receipt_digest is None
        or reproducer.provider_attestation_receipt_digest is None
    ):
        provider_attested = EngineIndependenceStateV1.UNPROVEN
        reasons.add("provider_attestation_missing")
    elif source.provider_identity_digest == reproducer.provider_identity_digest:
        provider_attested = EngineIndependenceStateV1.NOT_DISJOINT
        reasons.add("shared_provider_identity")
    elif (
        source.provider_attestation_receipt_digest
        == reproducer.provider_attestation_receipt_digest
    ):
        provider_attested = EngineIndependenceStateV1.UNPROVEN
        reasons.add("provider_attestation_receipt_reused")
    else:
        provider_attested = EngineIndependenceStateV1.PROVEN

    if provider_attested is EngineIndependenceStateV1.PROVEN:
        grade = EngineIndependenceGradeV1.PROVIDER_ATTESTED_DISJOINT
    elif configured is EngineIndependenceStateV1.PROVEN:
        grade = EngineIndependenceGradeV1.CONFIGURED_SOURCE_DISJOINT
    elif process is EngineIndependenceStateV1.PROVEN:
        grade = EngineIndependenceGradeV1.PROCESS_DISJOINT
    else:
        grade = EngineIndependenceGradeV1.NOT_DISJOINT

    eligible = configured is EngineIndependenceStateV1.PROVEN
    if eligible:
        reasons.add("configured_independent_reproducer_eligible")
    else:
        reasons.add("independent_reproducer_ineligible")

    return EngineIndependenceAssessmentV1(
        source_registry_receipt_digest=source.registry_receipt_digest,
        reproducer_registry_receipt_digest=reproducer.registry_receipt_digest,
        run_scope_digest=source.run_scope_digest,
        process_disjoint=process,
        configured_source_disjoint=configured,
        provider_attested_disjoint=provider_attested,
        highest_grade=grade,
        independent_reproducer_eligible=eligible,
        reasons=tuple(sorted(reasons)),
    )


__all__ = [
    "AUTHORITY_EFFECT",
    "COGNITIVE_ENGINE_INDEPENDENCE_ASSESSMENT_SCHEMA_ID",
    "COGNITIVE_ENGINE_INDEPENDENCE_REDUCER_VERSION",
    "COGNITIVE_ENGINE_REGISTRY_RECEIPT_SCHEMA_ID",
    "COGNITIVE_ENGINE_REGISTRY_SCHEMA_ID",
    "EngineIndependenceAssessmentV1",
    "EngineIndependenceGradeV1",
    "EngineIndependenceStateV1",
    "PRODUCTION_ENABLED",
    "PROVENANCE_GATE_ACCEPTED_SET",
    "RunPinnedEngineIdentityV1",
    "reduce_engine_independence_v1",
]
