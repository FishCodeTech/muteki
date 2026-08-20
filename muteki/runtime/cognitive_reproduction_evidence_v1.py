"""Canonical prelaunch evidence for one default-off cognitive reproduction.

The contract deliberately splits two facts across distinct store capabilities:

* the declaration authority freezes the intended input and launch material after
  the durable C6 claim exists, but before the host adapter is armed; and
* the launcher authority independently snapshots the actual material immediately
  before that same adapter may cross ``subprocess.Popen``.

Neither event verifies an experiment, updates belief, dispatches work, retries an
UNKNOWN launch, or changes the hard acceptance set.  Missing source identities,
workspace provenance, or exact material equality is retained as ``HELD_UNKNOWN``.
"""

from __future__ import annotations

import hashlib
import json
import os
from collections.abc import Mapping
from dataclasses import dataclass
from pathlib import Path
from typing import Any

from muteki.epistemic.cognitive_events_v1 import (
    COGNITIVE_EXECUTION_OBSERVED,
    COGNITIVE_EXPERIMENT_ASSIGNED,
    COGNITIVE_RUNTIME_REPRODUCTION_ASSIGNMENT_SCHEMA_ID,
)
from muteki.epistemic.cas import ReceiptCAS
from muteki.epistemic.contracts import canonical_digest, canonical_json_bytes
from muteki.epistemic.sqlite_store import (
    CommandEvent,
    EpistemicSQLiteStore,
    IntegrityError,
    ProjectionMutation,
)
from muteki.runtime.c6_transport import C6LaunchMaterialV1
from muteki.runtime.cognition import (
    DeliveredContextPacketV1,
    PromptLaunchClaimV1,
)
from muteki.runtime.cognitive_reproduction_witness_v1 import (
    CognitiveReproductionWitnessV1,
    ReproductionEnvironmentEntryV1,
    ReproductionInputArtifactV1,
    ReproductionInputModeV1,
    ReproductionWitnessStatusV1,
    SourcePreOutcomeFenceV1,
    assess_cognitive_reproduction_witness,
)
from muteki.runtime.contracts import AttemptPermit
from muteki.runtime.prompt_stage import PromptInvocationBindingV1, StagedPromptV1


COGNITIVE_REPRODUCTION_PRELAUNCH_DECLARED = (
    "COGNITIVE_REPRODUCTION_PRELAUNCH_DECLARED"
)
COGNITIVE_REPRODUCTION_LAUNCH_WITNESSED = (
    "COGNITIVE_REPRODUCTION_LAUNCH_WITNESSED"
)
COGNITIVE_REPRODUCTION_DECLARATION_ACTOR = (
    "cognitive-reproduction-declaration-authority"
)
COGNITIVE_REPRODUCTION_LAUNCH_WITNESS_ACTOR = "cognitive-launch-witness-authority"
COGNITIVE_REPRODUCTION_PRELAUNCH_SCHEMA_ID = (
    "muteki.cognitive-reproduction-prelaunch-declaration.v1"
)
COGNITIVE_REPRODUCTION_LAUNCH_WITNESS_SCHEMA_ID = (
    "muteki.cognitive-reproduction-launcher-actual-witness.v1"
)
COGNITIVE_REPRODUCTION_SESSION_POLICY_VERSION = (
    "muteki.cognitive-reproduction-fresh-session-policy.v1"
)

PRODUCTION_ENABLED = False
LEARNING_ELIGIBLE = False
AUTOMATIC_REDISPATCH_PERMITTED = False
ACCEPTED_SET_CHANGE = False

_SESSION_ENV_NAME = "MUTEKI_COGNITIVE_SESSION_ID"
_REQUIRED_SESSION_ENV_NAMES = ("HOME", _SESSION_ENV_NAME)
_UNKNOWN_SOURCE_HOME_IDENTITY = canonical_digest(
    {"kind": "UNKNOWN_SOURCE_HOME_IDENTITY", "version": 1}
)
_UNKNOWN_SOURCE_SESSION_IDENTITY = canonical_digest(
    {"kind": "UNKNOWN_SOURCE_SESSION_IDENTITY", "version": 1}
)
_MISSING_ACTUAL_HOME_IDENTITY = canonical_digest(
    {"kind": "MISSING_ACTUAL_HOME_IDENTITY", "version": 1}
)
_MISSING_ACTUAL_SESSION_IDENTITY = canonical_digest(
    {"kind": "MISSING_ACTUAL_SESSION_IDENTITY", "version": 1}
)
_UNPROVEN_LOCAL_INPUT_RECEIPT = canonical_digest(
    {"kind": "UNPROVEN_LOCAL_INPUT_RECEIPT", "version": 1}
)
_MAX_WORKSPACE_ENTRIES = 256
_MAX_WORKSPACE_BYTES = 16 * 1024 * 1024


def _digest(value: object, name: str) -> str:
    if (
        type(value) is not str
        or len(value) != 64
        or any(character not in "0123456789abcdef" for character in value)
    ):
        raise ValueError(f"{name} must be a lowercase sha256 digest")
    return value


def _text(value: object, name: str) -> str:
    if type(value) is not str or not value or value != value.strip():
        raise ValueError(f"{name} must be exact non-empty text")
    return value


def _exact_environment(
    runtime_env: Mapping[str, str],
) -> tuple[ReproductionEnvironmentEntryV1, ...]:
    if not isinstance(runtime_env, Mapping):
        raise TypeError("runtime_env must be a mapping")
    entries: list[ReproductionEnvironmentEntryV1] = []
    for name, value in runtime_env.items():
        if type(name) is not str or not name or type(value) is not str:
            raise TypeError("runtime_env must contain exact string pairs")
        entries.append(
            ReproductionEnvironmentEntryV1(
                name=name,
                value_digest=hashlib.sha256(value.encode("utf-8")).hexdigest(),
            )
        )
    return tuple(sorted(entries, key=lambda item: item.name))


def _environment_identity(
    environment: tuple[ReproductionEnvironmentEntryV1, ...],
    name: str,
    *,
    missing: str,
) -> str:
    matches = tuple(item for item in environment if item.name == name)
    if not matches:
        return missing
    if len(matches) != 1:
        raise ValueError("environment identity is ambiguous")
    return canonical_digest({"name": name, "value_digest": matches[0].value_digest})


@dataclass(frozen=True, slots=True)
class _WorkspaceSnapshotV1:
    artifacts: tuple[ReproductionInputArtifactV1, ...]
    complete: bool
    reason_codes: tuple[str, ...]
    cwd_digest: str
    workspace_identity_digest: str

    @property
    def input_mode(self) -> ReproductionInputModeV1:
        return (
            ReproductionInputModeV1.NO_EXTERNAL_FILES
            if not self.artifacts and self.complete
            else ReproductionInputModeV1.EXACT_MANIFEST
        )

    @property
    def manifest_digest(self) -> str:
        return canonical_digest(tuple(item.canonical_body() for item in self.artifacts))

    def canonical_body(self) -> dict[str, Any]:
        return {
            "artifacts": tuple(item.canonical_body() for item in self.artifacts),
            "complete": self.complete,
            "cwd_digest": self.cwd_digest,
            "input_mode": self.input_mode.value,
            "manifest_digest": self.manifest_digest,
            "reason_codes": self.reason_codes,
            "workspace_identity_digest": self.workspace_identity_digest,
        }


def _snapshot_workspace(*, cwd: str, source_cutoff_seq: int) -> _WorkspaceSnapshotV1:
    if type(cwd) is not str or not cwd or not os.path.isabs(cwd):
        raise ValueError("reproduction cwd must be an absolute path")
    root = Path(cwd)
    cwd_digest = canonical_digest({"cwd": cwd})
    reasons: list[str] = []
    artifacts: list[ReproductionInputArtifactV1] = []
    total_bytes = 0
    if not root.is_dir() or root.is_symlink():
        return _WorkspaceSnapshotV1(
            artifacts=(),
            complete=False,
            reason_codes=("workspace_not_plain_directory",),
            cwd_digest=cwd_digest,
            workspace_identity_digest=cwd_digest,
        )
    try:
        paths = tuple(sorted(root.rglob("*"), key=lambda item: item.as_posix()))
    except OSError:
        paths = ()
        reasons.append("workspace_enumeration_failed")
    if len(paths) > _MAX_WORKSPACE_ENTRIES:
        reasons.append("workspace_entry_limit_exceeded")
        paths = paths[:_MAX_WORKSPACE_ENTRIES]
    for path in paths:
        relative = path.relative_to(root).as_posix()
        if path.is_symlink() or not path.is_file():
            reasons.append("workspace_contains_unbound_entry")
            continue
        try:
            before = path.stat()
            raw = path.read_bytes()
            after = path.stat()
        except OSError:
            reasons.append("workspace_file_read_failed")
            continue
        total_bytes += len(raw)
        if total_bytes > _MAX_WORKSPACE_BYTES:
            reasons.append("workspace_byte_limit_exceeded")
            break
        if (
            before.st_dev,
            before.st_ino,
            before.st_size,
            before.st_mtime_ns,
        ) != (
            after.st_dev,
            after.st_ino,
            after.st_size,
            after.st_mtime_ns,
        ):
            reasons.append("workspace_file_changed_during_snapshot")
            continue
        # Local files have no canonical pre-outcome availability receipt.  They
        # remain visible in the exact manifest, but are conservatively placed
        # after the source fence so the pure witness cannot call them eligible.
        artifacts.append(
            ReproductionInputArtifactV1(
                relative_path=relative,
                content_digest=hashlib.sha256(raw).hexdigest(),
                availability_receipt_digest=_UNPROVEN_LOCAL_INPUT_RECEIPT,
                available_at_seq=source_cutoff_seq + 1,
            )
        )
    artifacts_tuple = tuple(sorted(artifacts, key=lambda item: item.relative_path))
    return _WorkspaceSnapshotV1(
        artifacts=artifacts_tuple,
        complete=not reasons,
        reason_codes=tuple(dict.fromkeys(reasons)),
        cwd_digest=cwd_digest,
        workspace_identity_digest=cwd_digest,
    )


@dataclass(frozen=True, slots=True)
class CanonicalReproductionEvidenceOccurrenceV1:
    event_id: str
    event_digest: str
    receipt_digest: str
    seq: int
    payload: Mapping[str, Any]

    def __post_init__(self) -> None:
        object.__setattr__(self, "event_id", _text(self.event_id, "event_id"))
        object.__setattr__(
            self, "event_digest", _digest(self.event_digest, "event_digest")
        )
        object.__setattr__(
            self, "receipt_digest", _digest(self.receipt_digest, "receipt_digest")
        )
        if type(self.seq) is not int or self.seq < 1:
            raise ValueError("seq must be a positive exact integer")
        if not isinstance(self.payload, Mapping):
            raise TypeError("payload must be a mapping")


def _row_occurrence(
    *, store: EpistemicSQLiteStore, kind: str, event_id: str
) -> CanonicalReproductionEvidenceOccurrenceV1:
    rows = tuple(
        row
        for row in store.event_rows(kind=kind)
        if row["event_id"] == event_id
    )
    if len(rows) != 1:
        raise IntegrityError("canonical reproduction evidence occurrence is absent")
    row = rows[0]
    return CanonicalReproductionEvidenceOccurrenceV1(
        event_id=row["event_id"],
        event_digest=row["event_digest"],
        receipt_digest=store.receipt_digest_for_event(row["event_digest"]),
        seq=row["seq"],
        payload=row["payload"],
    )


def _one_event(
    *, store: EpistemicSQLiteStore, kind: str, predicate: Mapping[str, Any]
) -> dict[str, Any]:
    rows = tuple(
        row
        for row in store.event_rows(kind=kind)
        if all(row["payload"].get(name) == value for name, value in predicate.items())
    )
    if len(rows) != 1:
        raise IntegrityError(f"{kind} lineage is absent or ambiguous")
    return rows[0]


def _event_by_digest(
    *, store: EpistemicSQLiteStore, kind: str, event_digest: str
) -> dict[str, Any]:
    rows = tuple(
        row
        for row in store.event_rows(kind=kind)
        if row["event_digest"] == event_digest
    )
    if len(rows) != 1:
        raise IntegrityError(f"{kind} digest lineage is absent or ambiguous")
    return rows[0]


def _material_environment(
    body: Mapping[str, Any],
) -> tuple[ReproductionEnvironmentEntryV1, ...]:
    raw = body.get("environment")
    if type(raw) not in {list, tuple}:
        raise IntegrityError("launch material environment is not canonical")
    try:
        return tuple(
            ReproductionEnvironmentEntryV1(
                name=item["name"], value_digest=item["value_digest"]
            )
            for item in raw
        )
    except (KeyError, TypeError, ValueError) as exc:
        raise IntegrityError("launch material environment is malformed") from exc


def _source_facts(
    *,
    store: EpistemicSQLiteStore,
    cas: ReceiptCAS,
    assignment_row: Mapping[str, Any],
) -> tuple[dict[str, Any], SourcePreOutcomeFenceV1, dict[str, Any]]:
    assignment = assignment_row["payload"]
    source_assignment = _event_by_digest(
        store=store,
        kind=COGNITIVE_EXPERIMENT_ASSIGNED,
        event_digest=assignment["source_assignment_event_digest"],
    )
    source_observation = _event_by_digest(
        store=store,
        kind=COGNITIVE_EXECUTION_OBSERVED,
        event_digest=assignment["source_observation_event_digest"],
    )
    proof = source_observation["payload"].get("host_launch_proof_body")
    if not isinstance(proof, Mapping):
        raise IntegrityError("source observation has no host launch proof")
    source_claim = _event_by_digest(
        store=store,
        kind="CONTEXT_PROMPT_LAUNCH_CLAIMED",
        event_digest=proof.get("prompt_launch_claim_event_digest"),
    )
    try:
        raw_material = cas.read_verified(
            source_claim["payload"]["launch_material_digest"]
        )
        source_material = canonical_json_bytes(json.loads(raw_material))
        if source_material != raw_material:
            raise ValueError("not canonical")
        material_body = json.loads(raw_material)
    except (KeyError, TypeError, ValueError) as exc:
        raise IntegrityError("source launch material cannot be replayed") from exc
    environment = _material_environment(material_body)
    source_home_known = any(item.name == "HOME" for item in environment)
    source_session_known = any(item.name == _SESSION_ENV_NAME for item in environment)
    source_home_identity = _environment_identity(
        environment,
        "HOME",
        missing=_UNKNOWN_SOURCE_HOME_IDENTITY,
    )
    source_session_identity = _environment_identity(
        environment,
        _SESSION_ENV_NAME,
        missing=_UNKNOWN_SOURCE_SESSION_IDENTITY,
    )
    source_payload = source_assignment["payload"]
    fence = SourcePreOutcomeFenceV1(
        source_assignment_event_digest=source_assignment["event_digest"],
        cutoff_seq=source_payload["decision_cutoff_seq"],
        prefix_digest=source_payload["decision_prefix_digest"],
        prefix_head_event_digest=source_payload["decision_head_event_digest"],
        source_observation_seq=source_observation["seq"],
        source_workspace_identity_digest=material_body["cwd_digest"],
        source_home_identity_digest=source_home_identity,
        source_session_identity_digest=source_session_identity,
    )
    facts = {
        "assignment_event_digest": source_assignment["event_digest"],
        "assignment_event_receipt_digest": store.receipt_digest_for_event(
            source_assignment["event_digest"]
        ),
        "assignment_seq": source_assignment["seq"],
        "home_identity_known": source_home_known,
        "launch_claim_event_digest": source_claim["event_digest"],
        "launch_claim_event_receipt_digest": store.receipt_digest_for_event(
            source_claim["event_digest"]
        ),
        "launch_material_digest": source_claim["payload"][
            "launch_material_digest"
        ],
        "launch_profile_digest": source_claim["payload"]["profile_digest"],
        "observation_event_digest": source_observation["event_digest"],
        "observation_event_receipt_digest": store.receipt_digest_for_event(
            source_observation["event_digest"]
        ),
        "observation_seq": source_observation["seq"],
        "session_identity_known": source_session_known,
    }
    return facts, fence, material_body


def _assignment_for_permit(
    *, store: EpistemicSQLiteStore, permit: AttemptPermit
) -> dict[str, Any] | None:
    rows = tuple(
        row
        for row in store.event_rows(kind=COGNITIVE_EXPERIMENT_ASSIGNED)
        if row["payload"].get("permit_digest") == permit.digest
    )
    if not rows:
        return None
    if len(rows) != 1:
        raise IntegrityError("cognitive reproduction assignment is ambiguous")
    if (
        rows[0]["payload"].get("schema_id")
        != COGNITIVE_RUNTIME_REPRODUCTION_ASSIGNMENT_SCHEMA_ID
    ):
        return None
    return rows[0]


def _launch_lineage(
    *,
    store: EpistemicSQLiteStore,
    permit: AttemptPermit,
    staged: StagedPromptV1,
    invocation: PromptInvocationBindingV1,
    claim: PromptLaunchClaimV1,
) -> dict[str, Any]:
    stage = _one_event(
        store=store,
        kind="CONTEXT_PROMPT_STAGED",
        predicate={"stage_id": staged.stage_id, "permit_digest": permit.digest},
    )
    invocation_row = _one_event(
        store=store,
        kind="CONTEXT_PROMPT_INVOCATION_BOUND",
        predicate={
            "invocation_id": invocation.invocation_id,
            "permit_digest": permit.digest,
        },
    )
    claim_row = _one_event(
        store=store,
        kind="CONTEXT_PROMPT_LAUNCH_CLAIMED",
        predicate={"claim_id": claim.claim_id, "permit_digest": permit.digest},
    )
    return {
        "claim_event_digest": claim_row["event_digest"],
        "claim_event_receipt_digest": store.receipt_digest_for_event(
            claim_row["event_digest"]
        ),
        "claim_seq": claim_row["seq"],
        "invocation_event_digest": invocation_row["event_digest"],
        "invocation_event_receipt_digest": store.receipt_digest_for_event(
            invocation_row["event_digest"]
        ),
        "invocation_seq": invocation_row["seq"],
        "stage_event_digest": stage["event_digest"],
        "stage_event_receipt_digest": store.receipt_digest_for_event(
            stage["event_digest"]
        ),
        "stage_seq": stage["seq"],
    }


def _launch_snapshot_body(
    *,
    staged: StagedPromptV1,
    invocation: PromptInvocationBindingV1,
    claim: PromptLaunchClaimV1,
    material: C6LaunchMaterialV1,
    cwd: str,
    runtime_env: Mapping[str, str],
    source_cutoff_seq: int,
) -> dict[str, Any]:
    environment = _exact_environment(runtime_env)
    workspace = _snapshot_workspace(cwd=cwd, source_cutoff_seq=source_cutoff_seq)
    home_present = any(item.name == "HOME" for item in environment)
    session_present = any(item.name == _SESSION_ENV_NAME for item in environment)
    body = {
        "argv_artifact_digest": invocation.argv_artifact_digest,
        "argv_byte_count": invocation.argv_byte_count,
        "claim_id": claim.claim_id,
        "cwd_digest": workspace.cwd_digest,
        "environment": tuple(item.canonical_body() for item in environment),
        "environment_allowlist": tuple(item.name for item in environment),
        "environment_digest": canonical_digest(
            tuple(item.canonical_body() for item in environment)
        ),
        "environment_inherited": False,
        "full_prompt_byte_count": staged.assembly.full_prompt_byte_count,
        "home_identity_digest": _environment_identity(
            environment,
            "HOME",
            missing=_MISSING_ACTUAL_HOME_IDENTITY,
        ),
        "home_identity_present": home_present,
        # The current C6 backend is an unrestricted host Popen.  Exact cwd/env
        # binding does not prove that cwd-external files, network, MCP, caches or
        # provider-side sessions were unavailable to the child.
        "input_channel_containment": "host_popen_uncontained",
        "input_snapshot": workspace.canonical_body(),
        "invocation_id": invocation.invocation_id,
        "launch_material_digest": material.digest,
        "launch_profile_digest": material.profile.digest,
        "prompt_artifact_digest": staged.assembly.full_prompt_digest,
        "resumed_from_session_digest": None,
        "session_identity_digest": _environment_identity(
            environment,
            _SESSION_ENV_NAME,
            missing=_MISSING_ACTUAL_SESSION_IDENTITY,
        ),
        "session_identity_present": session_present,
        "session_policy_version": COGNITIVE_REPRODUCTION_SESSION_POLICY_VERSION,
        "stage_id": staged.stage_id,
        "workspace_identity_digest": workspace.workspace_identity_digest,
    }
    if (
        material.argv_artifact_digest != invocation.argv_artifact_digest
        or material.cwd_digest != workspace.cwd_digest
        or material.environment
        != tuple((item.name, item.value_digest) for item in environment)
        or material.profile.digest != claim.profile_digest
        or material.digest != claim.launch_material_digest
    ):
        raise IntegrityError("reproduction launch snapshot diverges from durable claim")
    return body


def _artifact_from_body(body: Mapping[str, Any]) -> ReproductionInputArtifactV1:
    return ReproductionInputArtifactV1(
        relative_path=body["relative_path"],
        content_digest=body["content_digest"],
        availability_receipt_digest=body["availability_receipt_digest"],
        available_at_seq=body["available_at_seq"],
    )


def _environment_from_body(
    body: Mapping[str, Any],
) -> ReproductionEnvironmentEntryV1:
    return ReproductionEnvironmentEntryV1(
        name=body["name"], value_digest=body["value_digest"]
    )


def _witness_from_launches(
    *,
    source_fence_body: Mapping[str, Any],
    declared: Mapping[str, Any],
    actual: Mapping[str, Any],
) -> CognitiveReproductionWitnessV1:
    fence = SourcePreOutcomeFenceV1(
        source_assignment_event_digest=source_fence_body[
            "source_assignment_event_digest"
        ],
        cutoff_seq=source_fence_body["cutoff_seq"],
        prefix_digest=source_fence_body["prefix_digest"],
        prefix_head_event_digest=source_fence_body["prefix_head_event_digest"],
        source_observation_seq=source_fence_body["source_observation_seq"],
        source_workspace_identity_digest=source_fence_body[
            "source_workspace_identity_digest"
        ],
        source_home_identity_digest=source_fence_body[
            "source_home_identity_digest"
        ],
        source_session_identity_digest=source_fence_body[
            "source_session_identity_digest"
        ],
    )
    declared_snapshot = declared["input_snapshot"]
    actual_snapshot = actual["input_snapshot"]
    return CognitiveReproductionWitnessV1(
        source_fence=fence,
        input_mode=ReproductionInputModeV1(declared_snapshot["input_mode"]),
        declared_input_manifest=tuple(
            _artifact_from_body(item) for item in declared_snapshot["artifacts"]
        ),
        actual_input_manifest=tuple(
            _artifact_from_body(item) for item in actual_snapshot["artifacts"]
        ),
        declared_prompt_template_digest=declared["prompt_artifact_digest"],
        actual_prompt_template_digest=actual["prompt_artifact_digest"],
        declared_workspace_identity_digest=declared[
            "workspace_identity_digest"
        ],
        actual_workspace_identity_digest=actual["workspace_identity_digest"],
        declared_home_identity_digest=declared["home_identity_digest"],
        actual_home_identity_digest=actual["home_identity_digest"],
        declared_session_identity_digest=declared["session_identity_digest"],
        actual_session_identity_digest=actual["session_identity_digest"],
        resumed_from_session_digest=actual["resumed_from_session_digest"],
        environment_allowlist=tuple(declared["environment_allowlist"]),
        declared_environment=tuple(
            _environment_from_body(item) for item in declared["environment"]
        ),
        actual_environment=tuple(
            _environment_from_body(item) for item in actual["environment"]
        ),
        declared_blackboard_cutoff_seq=fence.cutoff_seq,
        actual_blackboard_cutoff_seq=fence.cutoff_seq,
        declared_memory_cutoff_seq=fence.cutoff_seq,
        actual_memory_cutoff_seq=fence.cutoff_seq,
        declared_launch_material_digest=declared["launch_material_digest"],
        actual_launch_material_digest=actual["launch_material_digest"],
        declared_launch_cwd_digest=declared["cwd_digest"],
        actual_launch_cwd_digest=actual["cwd_digest"],
        declared_launch_profile_digest=declared["launch_profile_digest"],
        actual_launch_profile_digest=actual["launch_profile_digest"],
    )


def reconstruct_reproduction_witness(
    *,
    source_fence_body: Mapping[str, Any],
    declared_launch: Mapping[str, Any],
    actual_launch: Mapping[str, Any],
) -> CognitiveReproductionWitnessV1:
    """Replay a witness from the two separately authorized event bodies."""

    return _witness_from_launches(
        source_fence_body=source_fence_body,
        declared=declared_launch,
        actual=actual_launch,
    )


def _validate_launch_snapshot_body(body: Mapping[str, Any]) -> None:
    p = dict(body)
    expected = {
        "argv_artifact_digest",
        "argv_byte_count",
        "canonical_lineage",
        "claim_id",
        "cwd_digest",
        "environment",
        "environment_allowlist",
        "environment_digest",
        "environment_inherited",
        "full_prompt_byte_count",
        "home_identity_digest",
        "home_identity_present",
        "input_channel_containment",
        "input_snapshot",
        "invocation_id",
        "launch_material_digest",
        "launch_profile_digest",
        "prompt_artifact_digest",
        "resumed_from_session_digest",
        "session_identity_digest",
        "session_identity_present",
        "session_policy_version",
        "stage_id",
        "workspace_identity_digest",
    }
    if set(p) != expected:
        raise ValueError("reproduction launch snapshot shape is not versioned")
    for name in (
        "argv_artifact_digest",
        "cwd_digest",
        "environment_digest",
        "home_identity_digest",
        "launch_material_digest",
        "launch_profile_digest",
        "prompt_artifact_digest",
        "session_identity_digest",
        "workspace_identity_digest",
    ):
        _digest(p[name], name)
    if (
        type(p["argv_byte_count"]) is not int
        or p["argv_byte_count"] < 1
        or type(p["full_prompt_byte_count"]) is not int
        or p["full_prompt_byte_count"] < 1
        or p["environment_inherited"] is not False
        or type(p["home_identity_present"]) is not bool
        or type(p["session_identity_present"]) is not bool
        or p["resumed_from_session_digest"] is not None
        or p["input_channel_containment"] != "host_popen_uncontained"
        or p["session_policy_version"]
        != COGNITIVE_REPRODUCTION_SESSION_POLICY_VERSION
    ):
        raise ValueError("reproduction launch session policy diverged")
    environment = tuple(_environment_from_body(item) for item in p["environment"])
    if tuple(item.name for item in environment) != tuple(p["environment_allowlist"]):
        raise ValueError("environment allowlist does not bind the exact environment")
    if canonical_digest(tuple(item.canonical_body() for item in environment)) != p[
        "environment_digest"
    ]:
        raise ValueError("environment digest is false")
    snapshot = p["input_snapshot"]
    if not isinstance(snapshot, Mapping) or set(snapshot) != {
        "artifacts",
        "complete",
        "cwd_digest",
        "input_mode",
        "manifest_digest",
        "reason_codes",
        "workspace_identity_digest",
    }:
        raise ValueError("workspace snapshot shape is not versioned")
    artifacts = tuple(_artifact_from_body(item) for item in snapshot["artifacts"])
    if (
        canonical_digest(tuple(item.canonical_body() for item in artifacts))
        != snapshot["manifest_digest"]
        or snapshot["cwd_digest"] != p["cwd_digest"]
        or snapshot["workspace_identity_digest"]
        != p["workspace_identity_digest"]
        or type(snapshot["complete"]) is not bool
        or type(snapshot["reason_codes"]) not in {list, tuple}
        or snapshot["input_mode"]
        != (
            ReproductionInputModeV1.NO_EXTERNAL_FILES.value
            if not artifacts and snapshot["complete"]
            else ReproductionInputModeV1.EXACT_MANIFEST.value
        )
    ):
        raise ValueError("workspace snapshot digest or mode is false")
    lineage = p["canonical_lineage"]
    if not isinstance(lineage, Mapping) or set(lineage) != {
        "claim_event_digest",
        "claim_event_receipt_digest",
        "claim_seq",
        "invocation_event_digest",
        "invocation_event_receipt_digest",
        "invocation_seq",
        "stage_event_digest",
        "stage_event_receipt_digest",
        "stage_seq",
    }:
        raise ValueError("canonical launch lineage shape is not versioned")
    for name in (
        "claim_event_digest",
        "claim_event_receipt_digest",
        "invocation_event_digest",
        "invocation_event_receipt_digest",
        "stage_event_digest",
        "stage_event_receipt_digest",
    ):
        _digest(lineage[name], name)
    if not (
        type(lineage["stage_seq"]) is int
        and type(lineage["invocation_seq"]) is int
        and type(lineage["claim_seq"]) is int
        and 0 < lineage["stage_seq"] < lineage["invocation_seq"] < lineage["claim_seq"]
    ):
        raise ValueError("canonical launch lineage ordering is false")


def validate_prelaunch_declaration_payload_shape(payload: Mapping[str, Any]) -> None:
    p = dict(payload)
    expected = {
        "accepted_set_change",
        "actual_launch_witnessed",
        "automatic_redispatch_permitted",
        "declared_launch",
        "declared_launch_digest",
        "learning_eligible",
        "permit_digest",
        "permit_id",
        "production_enabled",
        "reproduction_assignment",
        "run_id",
        "schema_id",
        "scope_digest",
        "source",
        "source_fence",
        "source_fence_digest",
        "world_epoch_digest",
    }
    if set(p) != expected:
        raise ValueError("prelaunch declaration payload shape is not versioned")
    if (
        p["schema_id"] != COGNITIVE_REPRODUCTION_PRELAUNCH_SCHEMA_ID
        or p["production_enabled"] is not False
        or p["learning_eligible"] is not False
        or p["actual_launch_witnessed"] is not False
        or p["automatic_redispatch_permitted"] is not False
        or p["accepted_set_change"] is not False
    ):
        raise ValueError("prelaunch declaration authority boundary diverged")
    for name in (
        "declared_launch_digest",
        "permit_digest",
        "scope_digest",
        "source_fence_digest",
        "world_epoch_digest",
    ):
        _digest(p[name], name)
    if canonical_digest(p["declared_launch"]) != p["declared_launch_digest"]:
        raise ValueError("declared launch digest is false")
    if canonical_digest(p["source_fence"]) != p["source_fence_digest"]:
        raise ValueError("source fence digest is false")
    _validate_launch_snapshot_body(p["declared_launch"])


def validate_launch_witness_payload_shape(payload: Mapping[str, Any]) -> None:
    p = dict(payload)
    expected = {
        "accepted_set_change",
        "actual_launch",
        "actual_launch_digest",
        "automatic_redispatch_permitted",
        "declaration_event_digest",
        "declaration_event_receipt_digest",
        "declaration_payload_digest",
        "evidence_status",
        "learning_eligible",
        "permit_digest",
        "permit_id",
        "policy_reason_codes",
        "production_enabled",
        "run_id",
        "schema_id",
        "scope_digest",
        "witness_assessment",
        "witness_assessment_digest",
        "witness_body",
        "witness_digest",
        "world_epoch_digest",
    }
    if set(p) != expected:
        raise ValueError("launch witness payload shape is not versioned")
    if (
        p["schema_id"] != COGNITIVE_REPRODUCTION_LAUNCH_WITNESS_SCHEMA_ID
        or p["production_enabled"] is not False
        or p["learning_eligible"] is not False
        or p["automatic_redispatch_permitted"] is not False
        or p["accepted_set_change"] is not False
        or p["evidence_status"] not in {"preregistered_exact_shadow", "held_unknown"}
        or type(p["policy_reason_codes"]) not in {list, tuple}
    ):
        raise ValueError("launch witness authority boundary diverged")
    for name in (
        "actual_launch_digest",
        "declaration_event_digest",
        "declaration_event_receipt_digest",
        "declaration_payload_digest",
        "permit_digest",
        "scope_digest",
        "witness_assessment_digest",
        "witness_digest",
        "world_epoch_digest",
    ):
        _digest(p[name], name)
    if canonical_digest(p["actual_launch"]) != p["actual_launch_digest"]:
        raise ValueError("actual launch digest is false")
    if canonical_digest(p["witness_body"]) != p["witness_digest"]:
        raise ValueError("reproduction witness digest is false")
    if (
        canonical_digest(p["witness_assessment"])
        != p["witness_assessment_digest"]
    ):
        raise ValueError("reproduction assessment digest is false")
    _validate_launch_snapshot_body(p["actual_launch"])


class CognitiveReproductionDeclarationAuthorityV1:
    """The sole store capability that may freeze the pre-Popen declaration."""

    def __init__(self, *, store: EpistemicSQLiteStore, cas: ReceiptCAS) -> None:
        if type(store) is not EpistemicSQLiteStore or type(cas) is not ReceiptCAS:
            raise TypeError("declaration authority requires exact store and CAS")
        self._store = store
        self._cas = cas

    def declare_if_reproduction(
        self,
        *,
        delivered: DeliveredContextPacketV1,
        permit: AttemptPermit,
        staged: StagedPromptV1,
        invocation: PromptInvocationBindingV1,
        claim: PromptLaunchClaimV1,
        material: C6LaunchMaterialV1,
        cwd: str,
        runtime_env: Mapping[str, str],
        occurred_at_ns: int,
    ) -> CanonicalReproductionEvidenceOccurrenceV1 | None:
        assignment_row = _assignment_for_permit(store=self._store, permit=permit)
        if assignment_row is None:
            return None
        if type(delivered) is not DeliveredContextPacketV1:
            raise TypeError("delivered must be exact DeliveredContextPacketV1")
        if (
            type(staged) is not StagedPromptV1
            or type(invocation) is not PromptInvocationBindingV1
            or type(claim) is not PromptLaunchClaimV1
            or type(material) is not C6LaunchMaterialV1
        ):
            raise TypeError("reproduction declaration launch objects are malformed")
        if type(occurred_at_ns) is not int or occurred_at_ns < 0:
            raise ValueError("occurred_at_ns must be non-negative")
        assignment = assignment_row["payload"]
        source, fence, _source_material = _source_facts(
            store=self._store,
            cas=self._cas,
            assignment_row=assignment_row,
        )
        lineage = _launch_lineage(
            store=self._store,
            permit=permit,
            staged=staged,
            invocation=invocation,
            claim=claim,
        )
        declared_launch = _launch_snapshot_body(
            staged=staged,
            invocation=invocation,
            claim=claim,
            material=material,
            cwd=cwd,
            runtime_env=runtime_env,
            source_cutoff_seq=fence.cutoff_seq,
        )
        declared_launch["canonical_lineage"] = lineage
        reproduction_assignment = {
            "assignment_event_digest": assignment_row["event_digest"],
            "assignment_event_receipt_digest": self._store.receipt_digest_for_event(
                assignment_row["event_digest"]
            ),
            "assignment_seq": assignment_row["seq"],
            "experiment_digest": assignment["experiment_digest"],
            "reproduction_kernel_digest": assignment["reproduction_kernel_digest"],
        }
        payload = {
            "accepted_set_change": ACCEPTED_SET_CHANGE,
            "actual_launch_witnessed": False,
            "automatic_redispatch_permitted": AUTOMATIC_REDISPATCH_PERMITTED,
            "declared_launch": declared_launch,
            "declared_launch_digest": canonical_digest(declared_launch),
            "learning_eligible": LEARNING_ELIGIBLE,
            "permit_digest": permit.digest,
            "permit_id": permit.permit_id,
            "production_enabled": PRODUCTION_ENABLED,
            "reproduction_assignment": reproduction_assignment,
            "run_id": self._store.run_id,
            "schema_id": COGNITIVE_REPRODUCTION_PRELAUNCH_SCHEMA_ID,
            "scope_digest": permit.lease.attempt.scope.digest,
            "source": source,
            "source_fence": fence.canonical_body(),
            "source_fence_digest": fence.digest,
            "world_epoch_digest": assignment["world_epoch_digest"],
        }
        validate_prelaunch_declaration_payload_shape(payload)
        event_id = f"event:cognitive-reproduction-prelaunch:{permit.permit_id}"
        self._store.commit_command(
            command_id=f"cognitive-reproduction-prelaunch:{permit.permit_id}",
            idempotency_key=f"cognitive-reproduction-prelaunch:{permit.permit_id}",
            command_payload=payload,
            events=(
                CommandEvent(
                    event_id,
                    COGNITIVE_REPRODUCTION_PRELAUNCH_DECLARED,
                    COGNITIVE_REPRODUCTION_DECLARATION_ACTOR,
                    occurred_at_ns,
                    payload,
                ),
            ),
            projection_mutations=(
                ProjectionMutation(
                    "cognitive_reproduction_prelaunch_declare_guard", payload
                ),
            ),
            authority_capability=(
                self._store._cognitive_reproduction_declaration_commit_capability
            ),
            required_prior_event=(
                "CONTEXT_PROMPT_LAUNCH_CLAIMED",
                {"claim_id": claim.claim_id, "permit_digest": permit.digest},
            ),
            forbid_prior_events=(
                (
                    COGNITIVE_REPRODUCTION_PRELAUNCH_DECLARED,
                    {"permit_digest": permit.digest},
                ),
                (
                    COGNITIVE_REPRODUCTION_LAUNCH_WITNESSED,
                    {"permit_digest": permit.digest},
                ),
                ("CONTEXT_PROMPT_RELEASED", {"permit_digest": permit.digest}),
                ("CONTEXT_PROMPT_UNKNOWN", {"permit_digest": permit.digest}),
                (
                    "CONTEXT_PROMPT_PRELAUNCH_ABORTED",
                    {"permit_digest": permit.digest},
                ),
            ),
            committed_at_ns=occurred_at_ns,
        )
        return _row_occurrence(
            store=self._store,
            kind=COGNITIVE_REPRODUCTION_PRELAUNCH_DECLARED,
            event_id=event_id,
        )


class CognitiveReproductionLaunchWitnessAuthorityV1:
    """Launcher-owned actual snapshot; it cannot mint a declaration."""

    def __init__(self, *, store: EpistemicSQLiteStore) -> None:
        if type(store) is not EpistemicSQLiteStore:
            raise TypeError("launch witness authority requires exact store")
        self._store = store

    def witness_if_declared(
        self,
        *,
        declaration: CanonicalReproductionEvidenceOccurrenceV1 | None,
        permit: AttemptPermit,
        staged: StagedPromptV1,
        invocation: PromptInvocationBindingV1,
        claim: PromptLaunchClaimV1,
        material: C6LaunchMaterialV1,
        cwd: str,
        runtime_env: Mapping[str, str],
        occurred_at_ns: int,
    ) -> CanonicalReproductionEvidenceOccurrenceV1 | None:
        if declaration is None:
            return None
        if type(declaration) is not CanonicalReproductionEvidenceOccurrenceV1:
            raise TypeError("declaration must be a canonical occurrence")
        if type(occurred_at_ns) is not int or occurred_at_ns < 0:
            raise ValueError("occurred_at_ns must be non-negative")
        declared_payload = dict(declaration.payload)
        validate_prelaunch_declaration_payload_shape(declared_payload)
        fence_body = declared_payload["source_fence"]
        actual_launch = _launch_snapshot_body(
            staged=staged,
            invocation=invocation,
            claim=claim,
            material=material,
            cwd=cwd,
            runtime_env=runtime_env,
            source_cutoff_seq=fence_body["cutoff_seq"],
        )
        actual_launch["canonical_lineage"] = _launch_lineage(
            store=self._store,
            permit=permit,
            staged=staged,
            invocation=invocation,
            claim=claim,
        )
        witness = _witness_from_launches(
            source_fence_body=fence_body,
            declared=declared_payload["declared_launch"],
            actual=actual_launch,
        )
        assessment = assess_cognitive_reproduction_witness(witness)
        reasons = list(
            f"witness:{reason.value}" for reason in assessment.reason_codes
        )
        source = declared_payload["source"]
        if source["home_identity_known"] is not True:
            reasons.append("source_home_identity_unknown")
        if source["session_identity_known"] is not True:
            reasons.append("source_session_identity_unknown")
        if actual_launch["home_identity_present"] is not True:
            reasons.append("reproduction_home_identity_missing")
        if actual_launch["session_identity_present"] is not True:
            reasons.append("reproduction_session_identity_missing")
        if actual_launch["input_snapshot"]["complete"] is not True:
            reasons.append("workspace_snapshot_incomplete")
        if actual_launch["input_channel_containment"] != "sealed_containment":
            reasons.append("external_input_channel_containment_unproven")
        if (
            canonical_digest(actual_launch)
            != declared_payload["declared_launch_digest"]
        ):
            reasons.append("declared_actual_launch_material_changed")
        reason_codes = tuple(dict.fromkeys(reasons))
        evidence_status = (
            "preregistered_exact_shadow"
            if not reason_codes
            and assessment.status is ReproductionWitnessStatusV1.OUTCOME_BLIND
            else "held_unknown"
        )
        payload = {
            "accepted_set_change": ACCEPTED_SET_CHANGE,
            "actual_launch": actual_launch,
            "actual_launch_digest": canonical_digest(actual_launch),
            "automatic_redispatch_permitted": AUTOMATIC_REDISPATCH_PERMITTED,
            "declaration_event_digest": declaration.event_digest,
            "declaration_event_receipt_digest": declaration.receipt_digest,
            "declaration_payload_digest": canonical_digest(declared_payload),
            "evidence_status": evidence_status,
            "learning_eligible": LEARNING_ELIGIBLE,
            "permit_digest": permit.digest,
            "permit_id": permit.permit_id,
            "policy_reason_codes": reason_codes,
            "production_enabled": PRODUCTION_ENABLED,
            "run_id": self._store.run_id,
            "schema_id": COGNITIVE_REPRODUCTION_LAUNCH_WITNESS_SCHEMA_ID,
            "scope_digest": permit.lease.attempt.scope.digest,
            "witness_assessment": assessment.canonical_body(),
            "witness_assessment_digest": assessment.digest,
            "witness_body": witness.canonical_body(),
            "witness_digest": witness.digest,
            "world_epoch_digest": declared_payload["world_epoch_digest"],
        }
        validate_launch_witness_payload_shape(payload)
        event_id = f"event:cognitive-reproduction-witness:{permit.permit_id}"
        self._store.commit_command(
            command_id=f"cognitive-reproduction-witness:{permit.permit_id}",
            idempotency_key=f"cognitive-reproduction-witness:{permit.permit_id}",
            command_payload=payload,
            events=(
                CommandEvent(
                    event_id,
                    COGNITIVE_REPRODUCTION_LAUNCH_WITNESSED,
                    COGNITIVE_REPRODUCTION_LAUNCH_WITNESS_ACTOR,
                    occurred_at_ns,
                    payload,
                ),
            ),
            projection_mutations=(
                ProjectionMutation(
                    "cognitive_reproduction_launch_witness_guard", payload
                ),
            ),
            authority_capability=(
                self._store._cognitive_reproduction_launch_witness_commit_capability
            ),
            required_prior_event=(
                COGNITIVE_REPRODUCTION_PRELAUNCH_DECLARED,
                {
                    "permit_digest": permit.digest,
                    "declared_launch_digest": declared_payload[
                        "declared_launch_digest"
                    ],
                },
            ),
            forbid_prior_events=(
                (
                    COGNITIVE_REPRODUCTION_LAUNCH_WITNESSED,
                    {"permit_digest": permit.digest},
                ),
                ("CONTEXT_PROMPT_RELEASED", {"permit_digest": permit.digest}),
                ("CONTEXT_PROMPT_UNKNOWN", {"permit_digest": permit.digest}),
                (
                    "CONTEXT_PROMPT_PRELAUNCH_ABORTED",
                    {"permit_digest": permit.digest},
                ),
            ),
            committed_at_ns=occurred_at_ns,
        )
        return _row_occurrence(
            store=self._store,
            kind=COGNITIVE_REPRODUCTION_LAUNCH_WITNESSED,
            event_id=event_id,
        )


__all__ = [
    "ACCEPTED_SET_CHANGE",
    "AUTOMATIC_REDISPATCH_PERMITTED",
    "COGNITIVE_REPRODUCTION_DECLARATION_ACTOR",
    "COGNITIVE_REPRODUCTION_LAUNCH_WITNESSED",
    "COGNITIVE_REPRODUCTION_LAUNCH_WITNESS_ACTOR",
    "COGNITIVE_REPRODUCTION_LAUNCH_WITNESS_SCHEMA_ID",
    "COGNITIVE_REPRODUCTION_PRELAUNCH_DECLARED",
    "COGNITIVE_REPRODUCTION_PRELAUNCH_SCHEMA_ID",
    "COGNITIVE_REPRODUCTION_SESSION_POLICY_VERSION",
    "CanonicalReproductionEvidenceOccurrenceV1",
    "CognitiveReproductionDeclarationAuthorityV1",
    "CognitiveReproductionLaunchWitnessAuthorityV1",
    "LEARNING_ELIGIBLE",
    "PRODUCTION_ENABLED",
    "validate_launch_witness_payload_shape",
    "validate_prelaunch_declaration_payload_shape",
    "reconstruct_reproduction_witness",
]
