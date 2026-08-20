"""Pure, default-off witness for an outcome-blind cognitive reproduction.

The witness compares a preregistered reproduction declaration with the exact
material observed at launch.  It deliberately accepts no ``clean``, ``fresh`` or
``outcome_blind`` boolean: those properties are derived from immutable manifests,
prefix cutoffs, identity digests and the launch snapshot.

This module owns no store, CAS, admission, dispatch, verification, learning,
production-routing or provenance-gate authority.  A trustworthy launcher/store
authority must supply the facts; this reducer only makes their policy consequence
deterministic and replayable.
"""

from __future__ import annotations

from dataclasses import dataclass
from enum import Enum
from pathlib import PurePosixPath
from typing import Any

from muteki.epistemic.contracts import canonical_digest


COGNITIVE_REPRODUCTION_WITNESS_SCHEMA_ID = (
    "muteki.cognitive-reproduction-input-witness.v1"
)
COGNITIVE_REPRODUCTION_ASSESSMENT_SCHEMA_ID = (
    "muteki.cognitive-reproduction-input-assessment.v1"
)
COGNITIVE_REPRODUCTION_WITNESS_POLICY_VERSION = (
    "muteki.cognitive-reproduction-outcome-blind-policy.v1"
)

# This file is an inert evidence contract.  None of these authorities can be
# acquired by constructing a witness or receiving an OUTCOME_BLIND assessment.
PRODUCTION_ENABLED = False
CANONICAL_WRITE_AUTHORITY = False
DISPATCH_AUTHORITY = False
LEARNING_AUTHORITY = False
ACCEPTED_SET_CHANGE = False

MAX_INPUT_FILES = 256
MAX_ENVIRONMENT_ENTRIES = 128
MAX_PATH_CHARS = 1_024
MAX_ENVIRONMENT_NAME_CHARS = 128


def _digest(value: object, name: str) -> str:
    if (
        type(value) is not str
        or len(value) != 64
        or any(character not in "0123456789abcdef" for character in value)
    ):
        raise ValueError(f"{name} must be a lowercase sha256 digest")
    return value


def _non_negative_int(value: object, name: str) -> int:
    if type(value) is not int or value < 0:
        raise ValueError(f"{name} must be a non-negative exact integer")
    return value


def _positive_int(value: object, name: str) -> int:
    if type(value) is not int or value < 1:
        raise ValueError(f"{name} must be a positive exact integer")
    return value


def _relative_path(value: object) -> str:
    if type(value) is not str or not value or value != value.strip():
        raise ValueError("relative_path must be exact non-empty text")
    if len(value) > MAX_PATH_CHARS or "\\" in value:
        raise ValueError("relative_path is not a bounded POSIX path")
    path = PurePosixPath(value)
    if path.is_absolute() or value != path.as_posix() or any(
        part in {"", ".", ".."} for part in path.parts
    ):
        raise ValueError("relative_path must be canonical and traversal-free")
    return value


def _environment_name(value: object) -> str:
    if (
        type(value) is not str
        or not value
        or len(value) > MAX_ENVIRONMENT_NAME_CHARS
        or not (value[0].isalpha() or value[0] == "_")
        or any(not (character.isalnum() or character == "_") for character in value)
    ):
        raise ValueError("environment name must be a bounded canonical identifier")
    return value


class ReproductionInputModeV1(str, Enum):
    """How launch-visible non-prompt files are bound to the pre-outcome prefix."""

    EXACT_MANIFEST = "exact_manifest"
    NO_EXTERNAL_FILES = "no_external_files"


@dataclass(frozen=True, slots=True)
class ReproductionInputArtifactV1:
    """One exact file visible to the reproducer and its prefix provenance."""

    relative_path: str
    content_digest: str
    availability_receipt_digest: str
    available_at_seq: int

    def __post_init__(self) -> None:
        object.__setattr__(self, "relative_path", _relative_path(self.relative_path))
        for name in ("content_digest", "availability_receipt_digest"):
            object.__setattr__(self, name, _digest(getattr(self, name), name))
        object.__setattr__(
            self,
            "available_at_seq",
            _non_negative_int(self.available_at_seq, "available_at_seq"),
        )

    def canonical_body(self) -> dict[str, Any]:
        return {
            "availability_receipt_digest": self.availability_receipt_digest,
            "available_at_seq": self.available_at_seq,
            "content_digest": self.content_digest,
            "relative_path": self.relative_path,
        }


@dataclass(frozen=True, slots=True)
class ReproductionEnvironmentEntryV1:
    """A launch environment entry represented without exposing its value."""

    name: str
    value_digest: str

    def __post_init__(self) -> None:
        object.__setattr__(self, "name", _environment_name(self.name))
        object.__setattr__(
            self, "value_digest", _digest(self.value_digest, "value_digest")
        )

    def canonical_body(self) -> dict[str, str]:
        return {"name": self.name, "value_digest": self.value_digest}


def _input_manifest(
    value: object, name: str
) -> tuple[ReproductionInputArtifactV1, ...]:
    if type(value) is not tuple or any(
        type(item) is not ReproductionInputArtifactV1 for item in value
    ):
        raise TypeError(
            f"{name} must be an immutable tuple of exact ReproductionInputArtifactV1"
        )
    if len(value) > MAX_INPUT_FILES:
        raise ValueError(f"{name} exceeds its hard file ceiling")
    paths = tuple(item.relative_path for item in value)
    if paths != tuple(sorted(paths)) or len(paths) != len(set(paths)):
        raise ValueError(f"{name} paths must be unique and canonically sorted")
    return value


def _environment(
    value: object, name: str
) -> tuple[ReproductionEnvironmentEntryV1, ...]:
    if type(value) is not tuple or any(
        type(item) is not ReproductionEnvironmentEntryV1 for item in value
    ):
        raise TypeError(
            f"{name} must be an immutable tuple of exact "
            "ReproductionEnvironmentEntryV1"
        )
    if len(value) > MAX_ENVIRONMENT_ENTRIES:
        raise ValueError(f"{name} exceeds its hard entry ceiling")
    names = tuple(item.name for item in value)
    if names != tuple(sorted(names)) or len(names) != len(set(names)):
        raise ValueError(f"{name} names must be unique and canonically sorted")
    return value


def _allowlist(value: object) -> tuple[str, ...]:
    if type(value) is not tuple:
        raise TypeError("environment_allowlist must be an immutable tuple")
    normalized = tuple(
        _environment_name(item) for item in value
    )
    if normalized != tuple(sorted(normalized)) or len(normalized) != len(
        set(normalized)
    ):
        raise ValueError("environment_allowlist must be unique and canonically sorted")
    if len(normalized) > MAX_ENVIRONMENT_ENTRIES:
        raise ValueError("environment_allowlist exceeds its hard entry ceiling")
    return normalized


@dataclass(frozen=True, slots=True)
class SourcePreOutcomeFenceV1:
    """Canonical prefix that is strictly earlier than the source O1 observation."""

    source_assignment_event_digest: str
    cutoff_seq: int
    prefix_digest: str
    prefix_head_event_digest: str
    source_observation_seq: int
    source_workspace_identity_digest: str
    source_home_identity_digest: str
    source_session_identity_digest: str

    def __post_init__(self) -> None:
        for name in (
            "source_assignment_event_digest",
            "prefix_digest",
            "prefix_head_event_digest",
            "source_workspace_identity_digest",
            "source_home_identity_digest",
            "source_session_identity_digest",
        ):
            object.__setattr__(self, name, _digest(getattr(self, name), name))
        object.__setattr__(
            self, "cutoff_seq", _positive_int(self.cutoff_seq, "cutoff_seq")
        )
        object.__setattr__(
            self,
            "source_observation_seq",
            _positive_int(self.source_observation_seq, "source_observation_seq"),
        )

    def canonical_body(self) -> dict[str, Any]:
        return {
            "cutoff_seq": self.cutoff_seq,
            "prefix_digest": self.prefix_digest,
            "prefix_head_event_digest": self.prefix_head_event_digest,
            "source_assignment_event_digest": self.source_assignment_event_digest,
            "source_home_identity_digest": self.source_home_identity_digest,
            "source_observation_seq": self.source_observation_seq,
            "source_session_identity_digest": self.source_session_identity_digest,
            "source_workspace_identity_digest": self.source_workspace_identity_digest,
        }

    @property
    def digest(self) -> str:
        return canonical_digest(self.canonical_body())


@dataclass(frozen=True, slots=True)
class CognitiveReproductionWitnessV1:
    """Preregistered declaration plus exact launch-time materialization facts."""

    source_fence: SourcePreOutcomeFenceV1

    input_mode: ReproductionInputModeV1
    declared_input_manifest: tuple[ReproductionInputArtifactV1, ...]
    actual_input_manifest: tuple[ReproductionInputArtifactV1, ...]
    declared_prompt_template_digest: str
    actual_prompt_template_digest: str

    declared_workspace_identity_digest: str
    actual_workspace_identity_digest: str
    declared_home_identity_digest: str
    actual_home_identity_digest: str
    declared_session_identity_digest: str
    actual_session_identity_digest: str
    resumed_from_session_digest: str | None

    environment_allowlist: tuple[str, ...]
    declared_environment: tuple[ReproductionEnvironmentEntryV1, ...]
    actual_environment: tuple[ReproductionEnvironmentEntryV1, ...]

    declared_blackboard_cutoff_seq: int
    actual_blackboard_cutoff_seq: int
    declared_memory_cutoff_seq: int
    actual_memory_cutoff_seq: int

    declared_launch_material_digest: str
    actual_launch_material_digest: str
    declared_launch_cwd_digest: str
    actual_launch_cwd_digest: str
    declared_launch_profile_digest: str
    actual_launch_profile_digest: str

    def __post_init__(self) -> None:
        if type(self.source_fence) is not SourcePreOutcomeFenceV1:
            raise TypeError("source_fence must be exact SourcePreOutcomeFenceV1")
        if type(self.input_mode) is not ReproductionInputModeV1:
            raise TypeError("input_mode must be exact ReproductionInputModeV1")
        for name in ("declared_input_manifest", "actual_input_manifest"):
            object.__setattr__(
                self, name, _input_manifest(getattr(self, name), name)
            )
        for name in (
            "declared_prompt_template_digest",
            "actual_prompt_template_digest",
            "declared_workspace_identity_digest",
            "actual_workspace_identity_digest",
            "declared_home_identity_digest",
            "actual_home_identity_digest",
            "declared_session_identity_digest",
            "actual_session_identity_digest",
            "declared_launch_material_digest",
            "actual_launch_material_digest",
            "declared_launch_cwd_digest",
            "actual_launch_cwd_digest",
            "declared_launch_profile_digest",
            "actual_launch_profile_digest",
        ):
            object.__setattr__(self, name, _digest(getattr(self, name), name))
        if self.resumed_from_session_digest is not None:
            object.__setattr__(
                self,
                "resumed_from_session_digest",
                _digest(
                    self.resumed_from_session_digest,
                    "resumed_from_session_digest",
                ),
            )
        object.__setattr__(
            self,
            "environment_allowlist",
            _allowlist(self.environment_allowlist),
        )
        for name in ("declared_environment", "actual_environment"):
            object.__setattr__(self, name, _environment(getattr(self, name), name))
        for name in (
            "declared_blackboard_cutoff_seq",
            "actual_blackboard_cutoff_seq",
            "declared_memory_cutoff_seq",
            "actual_memory_cutoff_seq",
        ):
            object.__setattr__(
                self, name, _non_negative_int(getattr(self, name), name)
            )

    @property
    def declared_input_manifest_digest(self) -> str:
        return canonical_digest(
            tuple(item.canonical_body() for item in self.declared_input_manifest)
        )

    @property
    def actual_input_manifest_digest(self) -> str:
        return canonical_digest(
            tuple(item.canonical_body() for item in self.actual_input_manifest)
        )

    @property
    def environment_allowlist_digest(self) -> str:
        return canonical_digest(
            {
                "names": self.environment_allowlist,
                "schema_id": "muteki.reproduction-environment-allowlist.v1",
            }
        )

    @property
    def declared_environment_digest(self) -> str:
        return canonical_digest(
            tuple(item.canonical_body() for item in self.declared_environment)
        )

    @property
    def actual_environment_digest(self) -> str:
        return canonical_digest(
            tuple(item.canonical_body() for item in self.actual_environment)
        )

    @property
    def actual_launch_binding_digest(self) -> str:
        """Bind every launch-visible channel assessed by this contract."""

        return canonical_digest(
            {
                "blackboard_cutoff_seq": self.actual_blackboard_cutoff_seq,
                "cwd_digest": self.actual_launch_cwd_digest,
                "environment_digest": self.actual_environment_digest,
                "home_identity_digest": self.actual_home_identity_digest,
                "input_manifest_digest": self.actual_input_manifest_digest,
                "launch_material_digest": self.actual_launch_material_digest,
                "memory_cutoff_seq": self.actual_memory_cutoff_seq,
                "profile_digest": self.actual_launch_profile_digest,
                "prompt_template_digest": self.actual_prompt_template_digest,
                "resumed_from_session_digest": self.resumed_from_session_digest,
                "session_identity_digest": self.actual_session_identity_digest,
                "workspace_identity_digest": self.actual_workspace_identity_digest,
            }
        )

    def canonical_body(self) -> dict[str, Any]:
        return {
            "accepted_set_change": ACCEPTED_SET_CHANGE,
            "actual_blackboard_cutoff_seq": self.actual_blackboard_cutoff_seq,
            "actual_environment": tuple(
                item.canonical_body() for item in self.actual_environment
            ),
            "actual_environment_digest": self.actual_environment_digest,
            "actual_home_identity_digest": self.actual_home_identity_digest,
            "actual_input_manifest": tuple(
                item.canonical_body() for item in self.actual_input_manifest
            ),
            "actual_input_manifest_digest": self.actual_input_manifest_digest,
            "actual_launch_binding_digest": self.actual_launch_binding_digest,
            "actual_launch_cwd_digest": self.actual_launch_cwd_digest,
            "actual_launch_material_digest": self.actual_launch_material_digest,
            "actual_launch_profile_digest": self.actual_launch_profile_digest,
            "actual_memory_cutoff_seq": self.actual_memory_cutoff_seq,
            "actual_prompt_template_digest": self.actual_prompt_template_digest,
            "actual_session_identity_digest": self.actual_session_identity_digest,
            "actual_workspace_identity_digest": self.actual_workspace_identity_digest,
            "declared_blackboard_cutoff_seq": self.declared_blackboard_cutoff_seq,
            "declared_environment": tuple(
                item.canonical_body() for item in self.declared_environment
            ),
            "declared_environment_digest": self.declared_environment_digest,
            "declared_home_identity_digest": self.declared_home_identity_digest,
            "declared_input_manifest": tuple(
                item.canonical_body() for item in self.declared_input_manifest
            ),
            "declared_input_manifest_digest": self.declared_input_manifest_digest,
            "declared_launch_cwd_digest": self.declared_launch_cwd_digest,
            "declared_launch_material_digest": self.declared_launch_material_digest,
            "declared_launch_profile_digest": self.declared_launch_profile_digest,
            "declared_memory_cutoff_seq": self.declared_memory_cutoff_seq,
            "declared_prompt_template_digest": self.declared_prompt_template_digest,
            "declared_session_identity_digest": self.declared_session_identity_digest,
            "declared_workspace_identity_digest": (
                self.declared_workspace_identity_digest
            ),
            "environment_allowlist": self.environment_allowlist,
            "environment_allowlist_digest": self.environment_allowlist_digest,
            "input_mode": self.input_mode.value,
            "resumed_from_session_digest": self.resumed_from_session_digest,
            "schema_id": COGNITIVE_REPRODUCTION_WITNESS_SCHEMA_ID,
            "source_fence": self.source_fence.canonical_body(),
            "source_fence_digest": self.source_fence.digest,
        }

    @property
    def digest(self) -> str:
        return canonical_digest(self.canonical_body())


class ReproductionWitnessStatusV1(str, Enum):
    OUTCOME_BLIND = "outcome_blind"
    INELIGIBLE = "ineligible"


class ReproductionWitnessReasonV1(str, Enum):
    SOURCE_PREFIX_NOT_PRE_OUTCOME = "source_prefix_not_pre_outcome"
    MISSING_DECLARED_INPUT_MANIFEST = "missing_declared_input_manifest"
    UNEXPECTED_EXTERNAL_INPUT_MANIFEST = "unexpected_external_input_manifest"
    INPUT_MANIFEST_CHANGED = "input_manifest_changed"
    INPUT_AFTER_SOURCE_PREFIX = "input_after_source_prefix"
    PROMPT_TEMPLATE_CHANGED = "prompt_template_changed"
    WORKSPACE_REUSED = "workspace_reused"
    HOME_REUSED = "home_reused"
    SESSION_REUSED = "session_reused"
    WORKSPACE_IDENTITY_CHANGED = "workspace_identity_changed"
    HOME_IDENTITY_CHANGED = "home_identity_changed"
    SESSION_IDENTITY_CHANGED = "session_identity_changed"
    REPRODUCTION_IDENTITIES_ALIAS = "reproduction_identities_alias"
    RESUME_ATTEMPTED = "resume_attempted"
    ENVIRONMENT_CHANGED = "environment_changed"
    ENVIRONMENT_NOT_ALLOWLISTED = "environment_not_allowlisted"
    BLACKBOARD_CUTOFF_CHANGED = "blackboard_cutoff_changed"
    BLACKBOARD_AFTER_SOURCE_PREFIX = "blackboard_after_source_prefix"
    MEMORY_CUTOFF_CHANGED = "memory_cutoff_changed"
    MEMORY_AFTER_SOURCE_PREFIX = "memory_after_source_prefix"
    LAUNCH_MATERIAL_CHANGED = "launch_material_changed"
    LAUNCH_CWD_CHANGED = "launch_cwd_changed"
    LAUNCH_CWD_NOT_FRESH_WORKSPACE = "launch_cwd_not_fresh_workspace"
    LAUNCH_PROFILE_CHANGED = "launch_profile_changed"


@dataclass(frozen=True, slots=True)
class CognitiveReproductionAssessmentV1:
    """Typed, replayable result derived only from an exact witness."""

    witness_digest: str
    status: ReproductionWitnessStatusV1
    reason_codes: tuple[ReproductionWitnessReasonV1, ...]

    def __post_init__(self) -> None:
        object.__setattr__(
            self, "witness_digest", _digest(self.witness_digest, "witness_digest")
        )
        if type(self.status) is not ReproductionWitnessStatusV1:
            raise TypeError("status must be ReproductionWitnessStatusV1")
        if type(self.reason_codes) is not tuple or any(
            type(reason) is not ReproductionWitnessReasonV1
            for reason in self.reason_codes
        ):
            raise TypeError("reason_codes must be an immutable typed tuple")
        if len(self.reason_codes) != len(set(self.reason_codes)):
            raise ValueError("reason_codes must be unique")
        if (self.status is ReproductionWitnessStatusV1.OUTCOME_BLIND) != (
            not self.reason_codes
        ):
            raise ValueError("status and reason_codes diverged")

    def canonical_body(self) -> dict[str, Any]:
        return {
            "accepted_set_change": ACCEPTED_SET_CHANGE,
            "canonical_write_authority": CANONICAL_WRITE_AUTHORITY,
            "dispatch_authority": DISPATCH_AUTHORITY,
            "learning_authority": LEARNING_AUTHORITY,
            "policy_version": COGNITIVE_REPRODUCTION_WITNESS_POLICY_VERSION,
            "production_enabled": PRODUCTION_ENABLED,
            "reason_codes": tuple(reason.value for reason in self.reason_codes),
            "schema_id": COGNITIVE_REPRODUCTION_ASSESSMENT_SCHEMA_ID,
            "status": self.status.value,
            "witness_digest": self.witness_digest,
        }

    @property
    def digest(self) -> str:
        return canonical_digest(self.canonical_body())


def assess_cognitive_reproduction_witness(
    witness: CognitiveReproductionWitnessV1,
) -> CognitiveReproductionAssessmentV1:
    """Derive outcome-blind eligibility without trusting a caller assertion."""

    if type(witness) is not CognitiveReproductionWitnessV1:
        raise TypeError("witness must be exact CognitiveReproductionWitnessV1")

    reasons: list[ReproductionWitnessReasonV1] = []
    fence = witness.source_fence
    if fence.cutoff_seq >= fence.source_observation_seq:
        reasons.append(
            ReproductionWitnessReasonV1.SOURCE_PREFIX_NOT_PRE_OUTCOME
        )
    if (
        witness.input_mode is ReproductionInputModeV1.EXACT_MANIFEST
        and not witness.declared_input_manifest
    ):
        reasons.append(
            ReproductionWitnessReasonV1.MISSING_DECLARED_INPUT_MANIFEST
        )
    if (
        witness.input_mode is ReproductionInputModeV1.NO_EXTERNAL_FILES
        and (witness.declared_input_manifest or witness.actual_input_manifest)
    ):
        reasons.append(
            ReproductionWitnessReasonV1.UNEXPECTED_EXTERNAL_INPUT_MANIFEST
        )
    if witness.declared_input_manifest != witness.actual_input_manifest:
        reasons.append(ReproductionWitnessReasonV1.INPUT_MANIFEST_CHANGED)
    if any(
        item.available_at_seq > fence.cutoff_seq
        for item in (*witness.declared_input_manifest, *witness.actual_input_manifest)
    ):
        reasons.append(ReproductionWitnessReasonV1.INPUT_AFTER_SOURCE_PREFIX)
    if (
        witness.declared_prompt_template_digest
        != witness.actual_prompt_template_digest
    ):
        reasons.append(ReproductionWitnessReasonV1.PROMPT_TEMPLATE_CHANGED)

    if fence.source_workspace_identity_digest in {
        witness.declared_workspace_identity_digest,
        witness.actual_workspace_identity_digest,
    }:
        reasons.append(ReproductionWitnessReasonV1.WORKSPACE_REUSED)
    if fence.source_home_identity_digest in {
        witness.declared_home_identity_digest,
        witness.actual_home_identity_digest,
    }:
        reasons.append(ReproductionWitnessReasonV1.HOME_REUSED)
    if fence.source_session_identity_digest in {
        witness.declared_session_identity_digest,
        witness.actual_session_identity_digest,
    }:
        reasons.append(ReproductionWitnessReasonV1.SESSION_REUSED)
    if (
        witness.declared_workspace_identity_digest
        != witness.actual_workspace_identity_digest
    ):
        reasons.append(ReproductionWitnessReasonV1.WORKSPACE_IDENTITY_CHANGED)
    if witness.declared_home_identity_digest != witness.actual_home_identity_digest:
        reasons.append(ReproductionWitnessReasonV1.HOME_IDENTITY_CHANGED)
    if witness.declared_session_identity_digest != witness.actual_session_identity_digest:
        reasons.append(ReproductionWitnessReasonV1.SESSION_IDENTITY_CHANGED)
    if len(
        {
            witness.actual_workspace_identity_digest,
            witness.actual_home_identity_digest,
            witness.actual_session_identity_digest,
        }
    ) != 3:
        reasons.append(ReproductionWitnessReasonV1.REPRODUCTION_IDENTITIES_ALIAS)
    if witness.resumed_from_session_digest is not None:
        reasons.append(ReproductionWitnessReasonV1.RESUME_ATTEMPTED)

    if witness.declared_environment != witness.actual_environment:
        reasons.append(ReproductionWitnessReasonV1.ENVIRONMENT_CHANGED)
    allowed = set(witness.environment_allowlist)
    if any(item.name not in allowed for item in witness.actual_environment):
        reasons.append(ReproductionWitnessReasonV1.ENVIRONMENT_NOT_ALLOWLISTED)

    if (
        witness.declared_blackboard_cutoff_seq
        != witness.actual_blackboard_cutoff_seq
    ):
        reasons.append(ReproductionWitnessReasonV1.BLACKBOARD_CUTOFF_CHANGED)
    if witness.actual_blackboard_cutoff_seq > fence.cutoff_seq:
        reasons.append(
            ReproductionWitnessReasonV1.BLACKBOARD_AFTER_SOURCE_PREFIX
        )
    if witness.declared_memory_cutoff_seq != witness.actual_memory_cutoff_seq:
        reasons.append(ReproductionWitnessReasonV1.MEMORY_CUTOFF_CHANGED)
    if witness.actual_memory_cutoff_seq > fence.cutoff_seq:
        reasons.append(ReproductionWitnessReasonV1.MEMORY_AFTER_SOURCE_PREFIX)

    if (
        witness.declared_launch_material_digest
        != witness.actual_launch_material_digest
    ):
        reasons.append(ReproductionWitnessReasonV1.LAUNCH_MATERIAL_CHANGED)
    if witness.declared_launch_cwd_digest != witness.actual_launch_cwd_digest:
        reasons.append(ReproductionWitnessReasonV1.LAUNCH_CWD_CHANGED)
    if witness.actual_launch_cwd_digest != witness.actual_workspace_identity_digest:
        reasons.append(
            ReproductionWitnessReasonV1.LAUNCH_CWD_NOT_FRESH_WORKSPACE
        )
    if witness.declared_launch_profile_digest != witness.actual_launch_profile_digest:
        reasons.append(ReproductionWitnessReasonV1.LAUNCH_PROFILE_CHANGED)

    reason_codes = tuple(dict.fromkeys(reasons))
    status = (
        ReproductionWitnessStatusV1.OUTCOME_BLIND
        if not reason_codes
        else ReproductionWitnessStatusV1.INELIGIBLE
    )
    return CognitiveReproductionAssessmentV1(
        witness_digest=witness.digest,
        status=status,
        reason_codes=reason_codes,
    )


__all__ = [
    "ACCEPTED_SET_CHANGE",
    "CANONICAL_WRITE_AUTHORITY",
    "COGNITIVE_REPRODUCTION_ASSESSMENT_SCHEMA_ID",
    "COGNITIVE_REPRODUCTION_WITNESS_POLICY_VERSION",
    "COGNITIVE_REPRODUCTION_WITNESS_SCHEMA_ID",
    "CognitiveReproductionAssessmentV1",
    "CognitiveReproductionWitnessV1",
    "DISPATCH_AUTHORITY",
    "LEARNING_AUTHORITY",
    "PRODUCTION_ENABLED",
    "ReproductionEnvironmentEntryV1",
    "ReproductionInputArtifactV1",
    "ReproductionInputModeV1",
    "ReproductionWitnessReasonV1",
    "ReproductionWitnessStatusV1",
    "SourcePreOutcomeFenceV1",
    "assess_cognitive_reproduction_witness",
]
