"""Default-off proof that one assigned H5 experiment reached a C6 host launch.

This module composes the existing canonical H5-eligible assignment event, ContextPacket
admission, prompt-stage CAS, argv binding, supervisor launch, and C6 host-Popen
release.  It owns no journal, executor, dispatch path, verification authority, or
learning decision.  It does not prove that a qualified runtime/V3 planner policy
selected the assignment.  A positive result proves only that the host observed a
local ``Popen`` with the exact sealed argv containing the exact staged prompt.

Child parsing and provider consumption remain unproven.  Missing, aborted, and
UNKNOWN histories are permanently non-learning and never authorize redispatch.
"""

from __future__ import annotations

import hashlib
import json
from collections.abc import Mapping
from dataclasses import dataclass
from enum import Enum
from typing import Any

from muteki.epistemic.cas import ReceiptCAS
from muteki.epistemic.cognitive_events_v1 import (
    COGNITIVE_EXPERIMENT_ASSIGNED,
    COGNITIVE_RUNTIME_CONTEXT_ASSIGNMENT_SCHEMA_ID,
    COGNITIVE_RUNTIME_EXECUTABLE_ASSIGNMENT_SCHEMA_ID,
    COGNITIVE_RUNTIME_REPRODUCTION_ASSIGNMENT_SCHEMA_ID,
    validate_runtime_context_assignment_payload_shape,
    validate_runtime_context_executable_assignment_payload_shape,
    validate_runtime_reproduction_assignment_payload_shape,
)
from muteki.epistemic.contracts import (
    canonical_digest,
    canonical_json_bytes,
    freeze_json,
)
from muteki.epistemic.sqlite_store import EpistemicSQLiteStore, IntegrityError
from muteki.runtime.cognition import (
    CognitiveContextAuthority,
    DeliveredContextPacketV1,
    PromptLaunchClaimV1,
)
from muteki.runtime.contracts import AttemptPermit
from muteki.runtime.executable_experiment_v1 import ExecutableExperimentBindingV1
from muteki.runtime.prompt_stage import (
    PromptAssemblyV1,
    PromptInvocationBindingV1,
    StagedPromptV1,
)


COGNITIVE_MATERIALIZATION_VERSION = "muteki.runtime-cognitive-materialization.v1"
COGNITIVE_EXPERIMENT_PROMPT_BLOCK_SCHEMA_ID = (
    "muteki.runtime-cognitive-experiment-prompt-block.v1"
)
COGNITIVE_REPRODUCTION_BASE_PROMPT_V1 = (
    "Execute the sealed preregistered reproduction exactly once. "
    "Use only the declared ContextPacket and executable experiment below."
)
NO_EXECUTABLE_EXPERIMENT_DIGEST = canonical_digest(
    {"kind": "NO_EXECUTABLE_EXPERIMENT", "version": 1}
)
NO_EXECUTABLE_WORKER_VIEW_DIGEST = canonical_digest(
    {"kind": "NO_EXECUTABLE_WORKER_VIEW", "version": 1}
)


def _text(value: object, name: str) -> str:
    if type(value) is not str or not value or value != value.strip():
        raise ValueError(f"{name} must be exact non-empty text")
    return value


def _digest(value: object, name: str) -> str:
    text = _text(value, name)
    if len(text) != 64 or any(
        character not in "0123456789abcdef" for character in text
    ):
        raise ValueError(f"{name} must be a lowercase sha256 digest")
    return text


class CognitiveMaterializationStatusV1(str, Enum):
    NOT_ASSIGNED = "not_assigned"
    NOT_STAGED = "not_staged"
    INCOMPLETE = "incomplete"
    PRELAUNCH_ABORTED = "prelaunch_aborted"
    UNKNOWN = "unknown"
    HOST_LAUNCH_ONLY = "host_launch_only"


@dataclass(frozen=True, slots=True)
class AssignedExperimentPromptV1:
    """Canonical experiment block derived from one store-owned assignment."""

    assignment_event_id: str
    assignment_event_digest: str
    assignment_event_receipt_digest: str
    assignment_digest: str
    assignment_body: Mapping[str, Any]
    experiment_digest: str
    experiment_body: Mapping[str, Any]
    packet_digest: str
    packet_manifest_digest: str
    attempt_digest: str
    attempt_id: str
    permit_digest: str
    permit_id: str
    scope_digest: str
    executable_experiment: ExecutableExperimentBindingV1 | None = None
    planner_policy_selection_proven: bool = False

    def __post_init__(self) -> None:
        object.__setattr__(
            self,
            "assignment_event_id",
            _text(self.assignment_event_id, "assignment_event_id"),
        )
        for name in (
            "assignment_event_digest",
            "assignment_event_receipt_digest",
            "assignment_digest",
            "experiment_digest",
            "packet_digest",
            "packet_manifest_digest",
            "attempt_digest",
            "permit_digest",
            "scope_digest",
        ):
            object.__setattr__(self, name, _digest(getattr(self, name), name))
        for name in ("attempt_id", "permit_id"):
            object.__setattr__(self, name, _text(getattr(self, name), name))
        assignment = freeze_json(self.assignment_body, path="$.assignment_body")
        experiment = freeze_json(self.experiment_body, path="$.experiment_body")
        if not isinstance(assignment, Mapping) or not isinstance(experiment, Mapping):
            raise TypeError("assignment and experiment bodies must be mappings")
        if (
            canonical_digest(assignment) != self.assignment_digest
            or canonical_digest(experiment) != self.experiment_digest
            or assignment.get("experiment_digest") != self.experiment_digest
            or experiment.get("context_packet_digest") != self.packet_digest
            or experiment.get("scope_digest") != self.scope_digest
            or self.planner_policy_selection_proven is not False
        ):
            raise ValueError("assigned experiment prompt lineage diverged")
        if self.executable_experiment is not None:
            if type(self.executable_experiment) is not ExecutableExperimentBindingV1:
                raise TypeError(
                    "executable_experiment must be ExecutableExperimentBindingV1 or None"
                )
            self.executable_experiment.spec.validate_against_body(experiment)
        object.__setattr__(self, "assignment_body", assignment)
        object.__setattr__(self, "experiment_body", experiment)

    def canonical_body(self) -> dict[str, Any]:
        body = {
            "assignment_body": self.assignment_body,
            "assignment_digest": self.assignment_digest,
            "assignment_event_digest": self.assignment_event_digest,
            "assignment_event_id": self.assignment_event_id,
            "assignment_event_receipt_digest": self.assignment_event_receipt_digest,
            "attempt_digest": self.attempt_digest,
            "attempt_id": self.attempt_id,
            "authority_boundary": {
                "accepted_set_change": False,
                "child_consumption_proven": False,
                "learning_eligible": False,
                "provider_consumption_proven": False,
                "planner_policy_selection_proven": False,
                "verification_resolved": False,
            },
            "experiment_body": self.experiment_body,
            "experiment_digest": self.experiment_digest,
            "packet_digest": self.packet_digest,
            "packet_manifest_digest": self.packet_manifest_digest,
            "permit_digest": self.permit_digest,
            "permit_id": self.permit_id,
            "schema_id": COGNITIVE_EXPERIMENT_PROMPT_BLOCK_SCHEMA_ID,
            "scope_digest": self.scope_digest,
        }
        if self.executable_experiment is not None:
            body["executable_experiment_binding"] = (
                self.executable_experiment.canonical_body()
            )
        return body

    def worker_prompt_body(self) -> dict[str, Any]:
        """Canonical worker view with host predicates deliberately withheld."""

        body = self.canonical_body()
        executable = self.executable_experiment
        if executable is not None:
            body.pop("executable_experiment_binding")
            body["executable_experiment_worker_view"] = {
                "artifact_digest": executable.worker_view_artifact_digest,
                "byte_count": executable.worker_view_byte_count,
                "body": executable.spec.worker_view_body(),
                "digest": executable.spec.worker_view_digest,
            }
        return body

    @property
    def executable_spec_digest(self) -> str:
        if self.executable_experiment is None:
            return NO_EXECUTABLE_EXPERIMENT_DIGEST
        return self.executable_experiment.spec.digest

    @property
    def executable_worker_view_digest(self) -> str:
        if self.executable_experiment is None:
            return NO_EXECUTABLE_WORKER_VIEW_DIGEST
        return self.executable_experiment.spec.worker_view_digest

    @property
    def digest(self) -> str:
        return canonical_digest(self.canonical_body())

    def render_for_prompt(self) -> str:
        body = canonical_json_bytes(self.worker_prompt_body()).decode("utf-8")
        return "\n## Canonical assigned discriminating experiment\n" + body + "\n"


@dataclass(frozen=True, slots=True)
class CognitiveExperimentPromptStageV1:
    """Exact prompt returned to the existing C6 invocation broker."""

    assigned: AssignedExperimentPromptV1
    staged: StagedPromptV1
    full_prompt: str

    def __post_init__(self) -> None:
        if type(self.assigned) is not AssignedExperimentPromptV1:
            raise TypeError("assigned must be AssignedExperimentPromptV1")
        if type(self.staged) is not StagedPromptV1:
            raise TypeError("staged must be StagedPromptV1")
        if type(self.full_prompt) is not str or not self.full_prompt:
            raise ValueError("full_prompt must be non-empty exact text")
        encoded = self.full_prompt.encode("utf-8")
        if (
            hashlib.sha256(encoded).hexdigest()
            != self.staged.assembly.full_prompt_digest
            or len(encoded) != self.staged.assembly.full_prompt_byte_count
            or self.full_prompt.count(self.assigned.render_for_prompt()) != 1
            or self.staged.attempt_digest != self.assigned.attempt_digest
            or self.staged.permit_digest != self.assigned.permit_digest
            or self.staged.assembly.packet_digest != self.assigned.packet_digest
        ):
            raise ValueError("cognitive prompt stage lineage diverged")
        executable = self.assigned.executable_experiment
        if executable is not None:
            if self.full_prompt.count(
                executable.spec.worker_view_bytes.decode("utf-8")
            ) != 1:
                raise ValueError(
                    "executable experiment worker view is not exact in the staged prompt"
                )
            if executable.spec.bytes.decode("utf-8") in self.full_prompt:
                raise ValueError("host-only predicates leaked into the worker prompt")


@dataclass(frozen=True, slots=True)
class CognitiveHostLaunchOnlyProofV1:
    """Positive local-Popen proof; deliberately not child/provider delivery."""

    assignment_event_digest: str
    assignment_event_receipt_digest: str
    assignment_digest: str
    experiment_digest: str
    executable_spec_digest: str
    executable_worker_view_digest: str
    packet_digest: str
    packet_manifest_digest: str
    prompt_artifact_digest: str
    prompt_stage_event_digest: str
    prompt_stage_receipt_digest: str
    stage_id: str
    argv_artifact_digest: str
    prompt_invocation_event_digest: str
    prompt_invocation_receipt_digest: str
    invocation_id: str
    worker_launch_event_digest: str
    worker_launch_receipt_digest: str
    prompt_launch_claim_event_digest: str
    prompt_launch_claim_receipt_digest: str
    claim_id: str
    prompt_release_event_digest: str
    prompt_release_receipt_digest: str
    launch_material_digest: str
    attempt_digest: str
    attempt_id: str
    permit_digest: str
    permit_id: str
    scope_digest: str
    verified_prefix_digest: str
    verified_prefix_head_event_digest: str
    verified_prefix_cutoff_seq: int
    authority_scope: str = "host_launch_only"
    child_consumption_proven: bool = False
    provider_consumption_proven: bool = False
    verification_resolved: bool = False
    learning_eligible: bool = False
    accepted_set_change: bool = False
    automatic_redispatch_permitted: bool = False
    planner_policy_selection_proven: bool = False

    def __post_init__(self) -> None:
        for name in (
            "assignment_event_digest",
            "assignment_event_receipt_digest",
            "assignment_digest",
            "experiment_digest",
            "executable_spec_digest",
            "executable_worker_view_digest",
            "packet_digest",
            "packet_manifest_digest",
            "prompt_artifact_digest",
            "prompt_stage_event_digest",
            "prompt_stage_receipt_digest",
            "argv_artifact_digest",
            "prompt_invocation_event_digest",
            "prompt_invocation_receipt_digest",
            "worker_launch_event_digest",
            "worker_launch_receipt_digest",
            "prompt_launch_claim_event_digest",
            "prompt_launch_claim_receipt_digest",
            "prompt_release_event_digest",
            "prompt_release_receipt_digest",
            "launch_material_digest",
            "attempt_digest",
            "permit_digest",
            "scope_digest",
            "verified_prefix_digest",
            "verified_prefix_head_event_digest",
        ):
            object.__setattr__(self, name, _digest(getattr(self, name), name))
        for name in (
            "stage_id",
            "invocation_id",
            "claim_id",
            "attempt_id",
            "permit_id",
        ):
            object.__setattr__(self, name, _text(getattr(self, name), name))
        if (
            type(self.verified_prefix_cutoff_seq) is not int
            or self.verified_prefix_cutoff_seq < 1
        ):
            raise ValueError("verified_prefix_cutoff_seq must be positive")
        if self.authority_scope != "host_launch_only":
            raise ValueError("materialization proof authority must be host_launch_only")
        if any(
            (
                self.child_consumption_proven,
                self.provider_consumption_proven,
                self.verification_resolved,
                self.learning_eligible,
                self.accepted_set_change,
                self.automatic_redispatch_permitted,
                self.planner_policy_selection_proven,
            )
        ):
            raise ValueError("host-launch-only proof overclaims authority")

    @property
    def digest(self) -> str:
        return canonical_digest(
            {name: getattr(self, name) for name in self.__dataclass_fields__}
        )


@dataclass(frozen=True, slots=True)
class CognitiveMaterializationResolutionV1:
    status: CognitiveMaterializationStatusV1
    proof: CognitiveHostLaunchOnlyProofV1 | None
    block_reasons: tuple[str, ...]
    host_launch_only: bool
    child_consumption_proven: bool = False
    provider_consumption_proven: bool = False
    verification_resolved: bool = False
    learning_eligible: bool = False
    accepted_set_change: bool = False
    automatic_redispatch_permitted: bool = False
    planner_policy_selection_proven: bool = False

    def __post_init__(self) -> None:
        if type(self.status) is not CognitiveMaterializationStatusV1:
            raise TypeError("status must be CognitiveMaterializationStatusV1")
        if type(self.block_reasons) is not tuple or any(
            type(reason) is not str or not reason for reason in self.block_reasons
        ):
            raise TypeError("block_reasons must be exact non-empty strings")
        positive = self.status is CognitiveMaterializationStatusV1.HOST_LAUNCH_ONLY
        if positive != (type(self.proof) is CognitiveHostLaunchOnlyProofV1):
            raise ValueError("positive materialization status/proof diverged")
        if positive != self.host_launch_only:
            raise ValueError("host_launch_only flag diverged from status")
        if positive and self.block_reasons:
            raise ValueError("positive host launch cannot carry block reasons")
        if not positive and not self.block_reasons:
            raise ValueError("non-positive materialization requires a block reason")
        if any(
            (
                self.child_consumption_proven,
                self.provider_consumption_proven,
                self.verification_resolved,
                self.learning_eligible,
                self.accepted_set_change,
                self.automatic_redispatch_permitted,
                self.planner_policy_selection_proven,
            )
        ):
            raise ValueError("materialization resolution overclaims authority")


class CognitiveExperimentMaterializationV1:
    """Narrow composer/resolver over the existing store, CAS, and C6 authority."""

    def __init__(self, *, store: EpistemicSQLiteStore, cas: ReceiptCAS) -> None:
        if type(store) is not EpistemicSQLiteStore:
            raise TypeError("store must be exactly EpistemicSQLiteStore")
        if not isinstance(cas, ReceiptCAS):
            raise TypeError("cas must be ReceiptCAS")
        self._store = store
        self._cas = cas
        self._context = CognitiveContextAuthority(store=store, cas=cas)

    @staticmethod
    def _blocked(
        status: CognitiveMaterializationStatusV1, reason: str
    ) -> CognitiveMaterializationResolutionV1:
        return CognitiveMaterializationResolutionV1(
            status=status,
            proof=None,
            block_reasons=(_text(reason, "block reason"),),
            host_launch_only=False,
        )

    def _rows(self, *, kind: str, cutoff_seq: int) -> tuple[dict[str, Any], ...]:
        return tuple(
            row for row in self._store.event_rows(kind=kind) if row["seq"] <= cutoff_seq
        )

    def _assignment_rows(
        self, *, permit: AttemptPermit, cutoff_seq: int
    ) -> tuple[dict[str, Any], ...]:
        return tuple(
            row
            for row in self._rows(
                kind=COGNITIVE_EXPERIMENT_ASSIGNED, cutoff_seq=cutoff_seq
            )
            if row["payload"].get("permit_digest") == permit.digest
        )

    def resolve_assigned(
        self,
        *,
        delivered: DeliveredContextPacketV1,
        permit: AttemptPermit,
        cutoff_seq: int | None = None,
    ) -> AssignedExperimentPromptV1:
        if type(delivered) is not DeliveredContextPacketV1:
            raise TypeError("delivered must be DeliveredContextPacketV1")
        if type(permit) is not AttemptPermit:
            raise TypeError("permit must be AttemptPermit")
        cutoff = self._store.state().head_seq if cutoff_seq is None else cutoff_seq
        if type(cutoff) is not int or cutoff < 1:
            raise ValueError("cutoff_seq must be a positive exact integer")
        rows = self._assignment_rows(permit=permit, cutoff_seq=cutoff)
        if len(rows) != 1:
            raise IntegrityError("runtime-context assignment does not resolve uniquely")
        row = rows[0]
        payload = row["payload"]
        schema_id = payload.get("schema_id")
        executable_schema_ids = {
            COGNITIVE_RUNTIME_EXECUTABLE_ASSIGNMENT_SCHEMA_ID,
            COGNITIVE_RUNTIME_REPRODUCTION_ASSIGNMENT_SCHEMA_ID,
        }
        if schema_id != COGNITIVE_RUNTIME_CONTEXT_ASSIGNMENT_SCHEMA_ID:
            if schema_id not in executable_schema_ids:
                raise IntegrityError(
                    "eval-v2 assignment cannot be read as runtime-context materialization"
                )
        try:
            if schema_id == COGNITIVE_RUNTIME_REPRODUCTION_ASSIGNMENT_SCHEMA_ID:
                validate_runtime_reproduction_assignment_payload_shape(payload)
            elif schema_id == COGNITIVE_RUNTIME_EXECUTABLE_ASSIGNMENT_SCHEMA_ID:
                validate_runtime_context_executable_assignment_payload_shape(payload)
            else:
                validate_runtime_context_assignment_payload_shape(payload)
        except (TypeError, ValueError) as exc:
            raise IntegrityError(
                "runtime-context assignment payload cannot be replayed"
            ) from exc
        packet = delivered.binding
        expected = {
            "attempt_digest": permit.lease.attempt.digest,
            "attempt_id": permit.lease.attempt.attempt_id,
            "permit_digest": permit.digest,
            "permit_id": permit.permit_id,
            "scope_digest": permit.lease.attempt.scope.digest,
        }
        if (
            any(payload.get(name) != value for name, value in expected.items())
            or permit.constraints.get("context_packet") != packet.canonical_body()
            or payload.get("context_packet_binding_body") != packet.canonical_body()
            or payload["experiment_body"].get("context_packet_digest")
            != packet.packet_digest
        ):
            raise IntegrityError(
                "runtime-context assignment is rebound from packet/attempt/permit"
            )
        admissions = tuple(
            item
            for item in self._rows(kind="ATTEMPT_ADMITTED", cutoff_seq=cutoff)
            if item["payload"].get("attempt_id") == permit.lease.attempt.attempt_id
            and item["payload"].get("permit_digest") == permit.digest
        )
        if len(admissions) != 1 or admissions[0]["seq"] + 1 != row["seq"]:
            raise IntegrityError(
                "runtime-context assignment is not the atomic admission companion"
            )
        assignment_receipt = self._store.resolve_receipt_for_event(row["event_digest"])
        admission_receipt = self._store.resolve_receipt_for_event(
            admissions[0]["event_digest"]
        )
        if assignment_receipt.command_id != admission_receipt.command_id:
            raise IntegrityError(
                "runtime-context assignment/admission command lineage diverged"
            )
        executable_experiment = None
        if schema_id in executable_schema_ids:
            try:
                executable_experiment = ExecutableExperimentBindingV1.from_canonical(
                    payload["executable_experiment_binding_body"]
                )
                executable_experiment.resolve(self._cas)
                executable_experiment.spec.validate_against_body(
                    payload["experiment_body"]
                )
            except (KeyError, TypeError, ValueError) as exc:
                raise IntegrityError(
                    "runtime executable experiment CAS binding cannot be replayed"
                ) from exc
        return AssignedExperimentPromptV1(
            assignment_event_id=row["event_id"],
            assignment_event_digest=row["event_digest"],
            assignment_event_receipt_digest=assignment_receipt.digest,
            assignment_digest=payload["assignment_digest"],
            assignment_body=payload["assignment_body"],
            experiment_digest=payload["experiment_digest"],
            experiment_body=payload["experiment_body"],
            packet_digest=packet.packet_digest,
            packet_manifest_digest=packet.manifest_digest,
            attempt_digest=permit.lease.attempt.digest,
            attempt_id=permit.lease.attempt.attempt_id,
            permit_digest=permit.digest,
            permit_id=permit.permit_id,
            scope_digest=permit.lease.attempt.scope.digest,
            executable_experiment=executable_experiment,
        )

    def stage_assigned_prompt(
        self,
        *,
        delivered: DeliveredContextPacketV1,
        permit: AttemptPermit,
        base_prompt: str,
        occurred_at_ns: int,
    ) -> CognitiveExperimentPromptStageV1:
        """Put the canonical experiment block in the exact existing prompt CAS."""

        if type(base_prompt) is not str or not base_prompt:
            raise ValueError("base_prompt must be non-empty exact text")
        assigned = self.resolve_assigned(delivered=delivered, permit=permit)
        assignment_rows = self._assignment_rows(
            permit=permit,
            cutoff_seq=self._store.state().head_seq,
        )
        if (
            len(assignment_rows) == 1
            and assignment_rows[0]["payload"].get("schema_id")
            == COGNITIVE_RUNTIME_REPRODUCTION_ASSIGNMENT_SCHEMA_ID
            and base_prompt != COGNITIVE_REPRODUCTION_BASE_PROMPT_V1
        ):
            raise ValueError(
                "reproduction prompt must use the fixed outcome-blind template"
            )
        context_block = delivered.render_for_prompt()
        experiment_block = assigned.render_for_prompt()
        if context_block in base_prompt or experiment_block in base_prompt:
            raise ValueError(
                "base_prompt cannot predeclare canonical materialization blocks"
            )
        separator = "" if base_prompt.endswith("\n") else "\n"
        full_prompt = base_prompt + separator + context_block + experiment_block
        staged = self._context.stage_prompt(
            delivered=delivered,
            permit=permit,
            prompt=full_prompt,
            transport="argv",
            occurred_at_ns=occurred_at_ns,
        )
        raw = self._cas.read_verified(staged.assembly.full_prompt_digest)
        if raw != full_prompt.encode("utf-8"):
            raise IntegrityError("cognitive materialization prompt CAS diverged")
        return CognitiveExperimentPromptStageV1(
            assigned=assigned,
            staged=staged,
            full_prompt=full_prompt,
        )

    def resolve_host_launch_only(
        self,
        *,
        delivered: DeliveredContextPacketV1,
        permit: AttemptPermit,
    ) -> CognitiveMaterializationResolutionV1:
        """Resolve the exact canonical chain without granting learning authority."""

        state = self._store.state()
        cutoff = state.head_seq
        prefix = self._store.receipt_field_resolver(
            cutoff_seq=cutoff
        ).verify_complete_through(cutoff)
        assignment_rows = self._assignment_rows(permit=permit, cutoff_seq=cutoff)
        if not assignment_rows:
            return self._blocked(
                CognitiveMaterializationStatusV1.NOT_ASSIGNED,
                "canonical runtime-context assignment is missing",
            )
        assigned = self.resolve_assigned(
            delivered=delivered,
            permit=permit,
            cutoff_seq=cutoff,
        )
        stage_rows = tuple(
            row
            for row in self._rows(kind="CONTEXT_PROMPT_STAGED", cutoff_seq=cutoff)
            if row["payload"].get("permit_digest") == permit.digest
            and row["payload"].get("packet_digest") == assigned.packet_digest
        )
        if not stage_rows:
            return self._blocked(
                CognitiveMaterializationStatusV1.NOT_STAGED,
                "assigned experiment has no ContextPacket prompt stage",
            )
        if len(stage_rows) != 1:
            raise IntegrityError("assigned experiment prompt stage is ambiguous")
        stage_row = stage_rows[0]
        stage_payload = stage_row["payload"]
        try:
            assembly = PromptAssemblyV1(
                packet_digest=assigned.packet_digest,
                context_block_digest=stage_payload.get("context_block_digest"),
                full_prompt_digest=stage_payload.get("prompt_artifact_digest"),
                full_prompt_byte_count=stage_payload.get("prompt_byte_count"),
                transport=stage_payload.get("transport"),
            )
            staged = StagedPromptV1(
                attempt_digest=assigned.attempt_digest,
                permit_digest=assigned.permit_digest,
                assembly=assembly,
                stage_id=stage_payload.get("stage_id"),
            )
        except (TypeError, ValueError) as exc:
            raise IntegrityError(
                "assigned experiment prompt stage is malformed"
            ) from exc
        if stage_payload.get("assembly_digest") != assembly.digest:
            raise IntegrityError("assigned experiment prompt assembly digest is false")
        prompt_raw = self._cas.read_verified(assembly.full_prompt_digest)
        try:
            full_prompt = prompt_raw.decode("utf-8")
        except UnicodeDecodeError as exc:
            raise IntegrityError("assigned experiment prompt is not UTF-8") from exc
        if (
            len(prompt_raw) != assembly.full_prompt_byte_count
            or hashlib.sha256(prompt_raw).hexdigest() != assembly.full_prompt_digest
            or full_prompt.count(delivered.render_for_prompt()) != 1
            or full_prompt.count(assigned.render_for_prompt()) != 1
        ):
            raise IntegrityError(
                "exact assigned experiment did not enter the ContextPacket prompt CAS"
            )

        invocation_rows = tuple(
            row
            for row in self._rows(
                kind="CONTEXT_PROMPT_INVOCATION_BOUND", cutoff_seq=cutoff
            )
            if row["payload"].get("stage_id") == staged.stage_id
            and row["payload"].get("permit_digest") == permit.digest
        )
        if not invocation_rows:
            return self._blocked(
                CognitiveMaterializationStatusV1.INCOMPLETE,
                "assigned prompt has no argv invocation binding",
            )
        if len(invocation_rows) != 1:
            raise IntegrityError("assigned experiment prompt invocation is ambiguous")
        invocation_row = invocation_rows[0]
        invocation_payload = invocation_row["payload"]
        try:
            invocation = PromptInvocationBindingV1(
                staged=staged,
                argv_artifact_digest=invocation_payload.get("argv_artifact_digest"),
                argv_byte_count=invocation_payload.get("argv_byte_count"),
                prompt_argument_count=invocation_payload.get("prompt_argument_count"),
                invocation_id=invocation_payload.get("invocation_id"),
            )
        except (TypeError, ValueError) as exc:
            raise IntegrityError(
                "assigned experiment prompt invocation is malformed"
            ) from exc
        argv_raw = self._cas.read_verified(invocation.argv_artifact_digest)
        try:
            argv = json.loads(argv_raw)
            replayed_invocation, replayed_raw = PromptInvocationBindingV1.bind_argv(
                staged=staged, argv=argv
            )
        except (TypeError, ValueError, json.JSONDecodeError) as exc:
            raise IntegrityError("assigned experiment argv is not replayable") from exc
        if replayed_invocation != invocation or replayed_raw != argv_raw:
            raise IntegrityError("assigned experiment argv binding diverged")

        terminal_rows = tuple(
            row
            for kind in (
                "CONTEXT_PROMPT_RELEASED",
                "CONTEXT_PROMPT_PRELAUNCH_ABORTED",
                "CONTEXT_PROMPT_UNKNOWN",
            )
            for row in self._rows(kind=kind, cutoff_seq=cutoff)
            if row["payload"].get("stage_id") == staged.stage_id
            and row["payload"].get("permit_digest") == permit.digest
        )
        if not terminal_rows:
            return self._blocked(
                CognitiveMaterializationStatusV1.INCOMPLETE,
                "assigned prompt invocation has no host terminal",
            )
        if len(terminal_rows) != 1:
            raise IntegrityError("assigned experiment prompt terminal is ambiguous")
        terminal = terminal_rows[0]
        if terminal["kind"] == "CONTEXT_PROMPT_UNKNOWN":
            return self._blocked(
                CognitiveMaterializationStatusV1.UNKNOWN,
                "host launch state is UNKNOWN and cannot redispatch",
            )
        if terminal["kind"] == "CONTEXT_PROMPT_PRELAUNCH_ABORTED":
            return self._blocked(
                CognitiveMaterializationStatusV1.PRELAUNCH_ABORTED,
                "host launch was known prelaunch-aborted",
            )

        launch_rows = tuple(
            row
            for row in self._rows(kind="WORKER_LAUNCH_PREPARED", cutoff_seq=cutoff)
            if row["payload"].get("permit_digest") == permit.digest
        )
        claim_rows = tuple(
            row
            for row in self._rows(
                kind="CONTEXT_PROMPT_LAUNCH_CLAIMED", cutoff_seq=cutoff
            )
            if row["payload"].get("stage_id") == staged.stage_id
            and row["payload"].get("permit_digest") == permit.digest
        )
        if len(launch_rows) != 1 or len(claim_rows) != 1:
            raise IntegrityError(
                "released assigned prompt has no unique worker launch and claim"
            )
        launch_row = launch_rows[0]
        claim_row = claim_rows[0]
        claim_payload = claim_row["payload"]
        try:
            claim = PromptLaunchClaimV1(
                staged=staged,
                invocation=invocation,
                claim_id=claim_payload.get("claim_id"),
                launch_material_digest=claim_payload.get("launch_material_digest"),
                profile_digest=claim_payload.get("profile_digest"),
            )
        except (TypeError, ValueError) as exc:
            raise IntegrityError("assigned experiment host claim is malformed") from exc
        self._context._validate_claim_launch_material(claim=claim)
        release_payload = terminal["payload"]
        expected_release = {
            "claim_id": claim.claim_id,
            "invocation_id": invocation.invocation_id,
            "launch_material_digest": claim.launch_material_digest,
            "packet_digest": assigned.packet_digest,
            "permit_digest": permit.digest,
            "prompt_invocation_event_digest": invocation_row["event_digest"],
            "prompt_launch_claim_event_digest": claim_row["event_digest"],
            "prompt_stage_event_digest": stage_row["event_digest"],
            "stage_id": staged.stage_id,
            "target_attempt_id": assigned.attempt_id,
            "worker_launch_event_digest": launch_row["event_digest"],
        }
        if any(
            release_payload.get(name) != value
            for name, value in expected_release.items()
        ):
            raise IntegrityError("assigned experiment host release lineage diverged")
        expected_invocation = {
            "packet_digest": assigned.packet_digest,
            "permit_digest": permit.digest,
            "prompt_stage_event_digest": stage_row["event_digest"],
            "worker_launch_event_digest": launch_row["event_digest"],
        }
        if any(
            invocation_payload.get(name) != value
            for name, value in expected_invocation.items()
        ):
            raise IntegrityError("assigned experiment invocation lineage diverged")
        expected_claim = {
            "invocation_id": invocation.invocation_id,
            "packet_digest": assigned.packet_digest,
            "permit_digest": permit.digest,
            "prompt_invocation_event_digest": invocation_row["event_digest"],
            "prompt_stage_event_digest": stage_row["event_digest"],
            "worker_launch_event_digest": launch_row["event_digest"],
        }
        if any(
            claim_payload.get(name) != value for name, value in expected_claim.items()
        ):
            raise IntegrityError("assigned experiment host claim lineage diverged")
        launch_expected = {
            "attempt_digest": permit.lease.attempt.digest,
            "attempt_id": permit.lease.attempt.attempt_id,
            "lease_digest": permit.lease.digest,
            "lease_id": permit.lease.lease_id,
            "permit_digest": permit.digest,
            "permit_id": permit.permit_id,
            "scope_digest": permit.lease.attempt.scope.digest,
        }
        if any(
            launch_row["payload"].get(name) != value
            for name, value in launch_expected.items()
        ):
            raise IntegrityError("assigned experiment worker launch is rebound")

        # Reuse the existing full C6 closure resolver as the final independent
        # replay of stage, argv, claim, release, CAS, and receipt lineage.
        self._context.require_verified_prompt_stages(delivered=delivered, permit=permit)
        stage_receipt = self._store.resolve_receipt_for_event(stage_row["event_digest"])
        invocation_receipt = self._store.resolve_receipt_for_event(
            invocation_row["event_digest"]
        )
        launch_receipt = self._store.resolve_receipt_for_event(
            launch_row["event_digest"]
        )
        claim_receipt = self._store.resolve_receipt_for_event(claim_row["event_digest"])
        release_receipt = self._store.resolve_receipt_for_event(
            terminal["event_digest"]
        )
        proof = CognitiveHostLaunchOnlyProofV1(
            assignment_event_digest=assigned.assignment_event_digest,
            assignment_event_receipt_digest=assigned.assignment_event_receipt_digest,
            assignment_digest=assigned.assignment_digest,
            experiment_digest=assigned.experiment_digest,
            executable_spec_digest=assigned.executable_spec_digest,
            executable_worker_view_digest=assigned.executable_worker_view_digest,
            packet_digest=assigned.packet_digest,
            packet_manifest_digest=assigned.packet_manifest_digest,
            prompt_artifact_digest=assembly.full_prompt_digest,
            prompt_stage_event_digest=stage_row["event_digest"],
            prompt_stage_receipt_digest=stage_receipt.digest,
            stage_id=staged.stage_id,
            argv_artifact_digest=invocation.argv_artifact_digest,
            prompt_invocation_event_digest=invocation_row["event_digest"],
            prompt_invocation_receipt_digest=invocation_receipt.digest,
            invocation_id=invocation.invocation_id,
            worker_launch_event_digest=launch_row["event_digest"],
            worker_launch_receipt_digest=launch_receipt.digest,
            prompt_launch_claim_event_digest=claim_row["event_digest"],
            prompt_launch_claim_receipt_digest=claim_receipt.digest,
            claim_id=claim.claim_id,
            prompt_release_event_digest=terminal["event_digest"],
            prompt_release_receipt_digest=release_receipt.digest,
            launch_material_digest=claim.launch_material_digest,
            attempt_digest=assigned.attempt_digest,
            attempt_id=assigned.attempt_id,
            permit_digest=assigned.permit_digest,
            permit_id=assigned.permit_id,
            scope_digest=assigned.scope_digest,
            verified_prefix_digest=prefix.digest,
            verified_prefix_head_event_digest=prefix.head_event_digest,
            verified_prefix_cutoff_seq=prefix.cutoff_seq,
        )
        return CognitiveMaterializationResolutionV1(
            status=CognitiveMaterializationStatusV1.HOST_LAUNCH_ONLY,
            proof=proof,
            block_reasons=(),
            host_launch_only=True,
        )


__all__ = [
    "COGNITIVE_EXPERIMENT_PROMPT_BLOCK_SCHEMA_ID",
    "COGNITIVE_MATERIALIZATION_VERSION",
    "COGNITIVE_REPRODUCTION_BASE_PROMPT_V1",
    "NO_EXECUTABLE_EXPERIMENT_DIGEST",
    "AssignedExperimentPromptV1",
    "CognitiveExperimentMaterializationV1",
    "CognitiveExperimentPromptStageV1",
    "CognitiveHostLaunchOnlyProofV1",
    "CognitiveMaterializationResolutionV1",
    "CognitiveMaterializationStatusV1",
]
