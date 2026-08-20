"""Execution identity and permit contracts independent of any storage backend."""

from __future__ import annotations

from dataclasses import dataclass, field
from enum import Enum
from typing import Any, Mapping

from muteki.epistemic.contracts import canonical_digest, freeze_json


NO_CONTEXT_PACKET_DIGEST = canonical_digest(
    {"kind": "NO_CONTEXT_PACKET", "version": 1}
)
_C6_EVALUATION_ARMS = frozenset(
    {
        "current_s4_baseline",
        "deterministic_decision_need_c6",
        "model_decision_need_r0_c6",
    }
)


def _text(value: str, name: str) -> str:
    if type(value) is not str or not value or value != value.strip():
        raise ValueError(f"{name} must be a non-empty canonical string")
    return value


def _digest(value: str, name: str) -> str:
    normalized = _text(value, name)
    if len(normalized) != 64 or any(ch not in "0123456789abcdef" for ch in normalized):
        raise ValueError(f"{name} must be a lowercase sha256 digest")
    return normalized


def _is_digest_text(value: object) -> bool:
    return (
        type(value) is str
        and len(value) == 64
        and all(ch in "0123456789abcdef" for ch in value)
    )


@dataclass(frozen=True, slots=True)
class ExecutionScope:
    run_id: str
    run_fence_epoch: int
    execution_generation: int

    def __post_init__(self) -> None:
        object.__setattr__(self, "run_id", _text(self.run_id, "run_id"))
        if (
            type(self.run_fence_epoch) is not int
            or type(self.execution_generation) is not int
            or self.run_fence_epoch < 1
            or self.execution_generation < 1
        ):
            raise ValueError("execution scope epochs start at 1")

    @property
    def digest(self) -> str:
        return canonical_digest(
            {
                "execution_generation": self.execution_generation,
                "run_fence_epoch": self.run_fence_epoch,
                "run_id": self.run_id,
            }
        )


@dataclass(frozen=True, slots=True)
class AttemptIdentity:
    scope: ExecutionScope
    branch_id: str
    attempt_id: str
    launch_ordinal: int

    def __post_init__(self) -> None:
        object.__setattr__(self, "branch_id", _text(self.branch_id, "branch_id"))
        object.__setattr__(self, "attempt_id", _text(self.attempt_id, "attempt_id"))
        if type(self.launch_ordinal) is not int or self.launch_ordinal < 1:
            raise ValueError("launch_ordinal starts at 1")

    @property
    def digest(self) -> str:
        return canonical_digest(
            {
                "attempt_id": self.attempt_id,
                "branch_id": self.branch_id,
                "launch_ordinal": self.launch_ordinal,
                "scope_digest": self.scope.digest,
            }
        )


@dataclass(frozen=True, slots=True)
class LeaseIdentity:
    attempt: AttemptIdentity
    lease_id: str
    lease_epoch: int
    worker_generation: int

    def __post_init__(self) -> None:
        object.__setattr__(self, "lease_id", _text(self.lease_id, "lease_id"))
        if (
            type(self.lease_epoch) is not int
            or type(self.worker_generation) is not int
            or self.lease_epoch < 1
            or self.worker_generation < 1
        ):
            raise ValueError("lease and worker generations start at 1")

    @property
    def digest(self) -> str:
        return canonical_digest(
            {
                "attempt_digest": self.attempt.digest,
                "lease_epoch": self.lease_epoch,
                "lease_id": self.lease_id,
                "worker_generation": self.worker_generation,
            }
        )


class EffectClass(str, Enum):
    PURE = "pure"
    IDEMPOTENT = "idempotent"
    OBSERVABLE = "observable"
    NON_IDEMPOTENT = "non_idempotent"


@dataclass(frozen=True, slots=True)
class ContextPacketAdmissionBindingV1:
    """Attempt-bound C6 packet identity carried by admission and the permit.

    This is lineage only.  It grants no read, write, dispatch, progress, effect,
    verification, or gate authority.  The corresponding packet must already have
    a unique canonical ``CONTEXT_PACKET_COMPILED`` predecessor.
    """

    target_attempt_id: str
    decision_id: str
    decision_receipt_digest: str
    compiler_receipt_digest: str
    compilation_event_receipt_digest: str
    packet_digest: str
    manifest_digest: str
    cutoff_seq: int
    compiler_version: str
    feature_state_digest: str
    accepted_set_change: bool = False

    def __post_init__(self) -> None:
        object.__setattr__(
            self,
            "target_attempt_id",
            _text(self.target_attempt_id, "target_attempt_id"),
        )
        object.__setattr__(self, "decision_id", _text(self.decision_id, "decision_id"))
        for name in (
            "decision_receipt_digest",
            "compiler_receipt_digest",
            "compilation_event_receipt_digest",
            "packet_digest",
            "manifest_digest",
            "feature_state_digest",
        ):
            object.__setattr__(self, name, _digest(getattr(self, name), name))
        if type(self.cutoff_seq) is not int or self.cutoff_seq < 1:
            raise ValueError("cutoff_seq must be a positive exact integer")
        object.__setattr__(
            self,
            "compiler_version",
            _text(self.compiler_version, "compiler_version"),
        )
        if type(self.accepted_set_change) is not bool:
            raise TypeError("accepted_set_change must be an exact boolean")
        if self.accepted_set_change:
            raise ValueError("ContextPacket cannot change the gate accepted set")

    def canonical_body(self) -> dict[str, Any]:
        return {
            "accepted_set_change": self.accepted_set_change,
            "compilation_event_receipt_digest": (
                self.compilation_event_receipt_digest
            ),
            "compiler_receipt_digest": self.compiler_receipt_digest,
            "compiler_version": self.compiler_version,
            "cutoff_seq": self.cutoff_seq,
            "decision_id": self.decision_id,
            "decision_receipt_digest": self.decision_receipt_digest,
            "feature_state_digest": self.feature_state_digest,
            "manifest_digest": self.manifest_digest,
            "packet_digest": self.packet_digest,
            "target_attempt_id": self.target_attempt_id,
        }

    @property
    def digest(self) -> str:
        return canonical_digest(self.canonical_body())


@dataclass(frozen=True, slots=True)
class EvaluationExecutionBindingV1:
    """Frozen evaluator identity for a single-phase shadow execution.

    The binding is inert metadata: it grants no dispatch, budget, evidence, feature,
    or gate authority.  Its purpose is to make cross-study/arm/config relabelling
    mechanically detectable by a later terminal-trial resolver.  Model-R0 is
    intentionally excluded: its observer must run and be accounted before the
    executor packet exists, which requires a role-aware multiphase v2 contract.
    """

    study_manifest_digest: str
    assignment_digest: str
    assignment_receipt_digest: str
    arm_id: str
    arm_config_digest: str
    source_registry_digest: str
    source_registry_receipt_digest: str
    randomization_receipt_digest: str
    feature_state_receipt_digest: str
    budget_point_digest: str
    compiler_digest: str
    context_digest: str
    context_packet_digest: str
    worktree_digest: str
    environment_digest: str
    offline_policy_digest: str
    price_table_digest: str
    checker_commitment_digest: str
    evaluator_ledger_anchor_digest: str
    feature_version: str = "c6-shadow-v1"
    split: str = "fresh_holdout"
    mode: str = "shadow"

    def __post_init__(self) -> None:
        for name in (
            "study_manifest_digest",
            "assignment_digest",
            "assignment_receipt_digest",
            "arm_config_digest",
            "source_registry_digest",
            "source_registry_receipt_digest",
            "randomization_receipt_digest",
            "feature_state_receipt_digest",
            "budget_point_digest",
            "compiler_digest",
            "context_digest",
            "context_packet_digest",
            "worktree_digest",
            "environment_digest",
            "offline_policy_digest",
            "price_table_digest",
            "checker_commitment_digest",
            "evaluator_ledger_anchor_digest",
        ):
            object.__setattr__(self, name, _digest(getattr(self, name), name))
        object.__setattr__(self, "arm_id", _text(self.arm_id, "arm_id"))
        if self.arm_id not in _C6_EVALUATION_ARMS:
            raise ValueError("arm_id is not one of the frozen C6 evaluation arms")
        if self.arm_id == "model_decision_need_r0_c6":
            raise ValueError(
                "model R0 requires the v2 multiphase observer-to-executor binding"
            )
        object.__setattr__(
            self, "feature_version", _text(self.feature_version, "feature_version")
        )
        object.__setattr__(self, "split", _text(self.split, "split"))
        object.__setattr__(self, "mode", _text(self.mode, "mode"))
        if self.split != "fresh_holdout":
            raise ValueError("evaluation execution must use the fresh_holdout split")
        if self.mode != "shadow":
            raise ValueError("evaluation execution binding is shadow-only")
        if self.arm_id == "current_s4_baseline":
            if self.context_packet_digest != NO_CONTEXT_PACKET_DIGEST:
                raise ValueError("baseline arm must bind the canonical NO_PACKET digest")
        elif self.context_packet_digest == NO_CONTEXT_PACKET_DIGEST:
            raise ValueError("candidate C6 arm must bind a real ContextPacket digest")

    def _run_manifest_body(self) -> dict[str, str | bool]:
        return {
            "accepted_set_change": False,
            "arm_config_digest": self.arm_config_digest,
            "arm_id": self.arm_id,
            "assignment_digest": self.assignment_digest,
            "assignment_receipt_digest": self.assignment_receipt_digest,
            "budget_point_digest": self.budget_point_digest,
            "checker_commitment_digest": self.checker_commitment_digest,
            "compiler_digest": self.compiler_digest,
            "context_digest": self.context_digest,
            "context_packet_digest": self.context_packet_digest,
            "environment_digest": self.environment_digest,
            "evaluator_ledger_anchor_digest": self.evaluator_ledger_anchor_digest,
            "feature_version": self.feature_version,
            "feature_state_receipt_digest": self.feature_state_receipt_digest,
            "mode": self.mode,
            "offline_policy_digest": self.offline_policy_digest,
            "price_table_digest": self.price_table_digest,
            "randomization_receipt_digest": self.randomization_receipt_digest,
            "source_registry_digest": self.source_registry_digest,
            "source_registry_receipt_digest": self.source_registry_receipt_digest,
            "split": self.split,
            "study_manifest_digest": self.study_manifest_digest,
            "worktree_digest": self.worktree_digest,
        }

    @property
    def run_manifest_digest(self) -> str:
        return canonical_digest(
            {
                "binding": self._run_manifest_body(),
                "schema_id": "muteki.c6-eval-run-manifest.v1",
            }
        )

    def canonical_body(self) -> dict[str, str | bool]:
        return {
            **self._run_manifest_body(),
            "run_manifest_digest": self.run_manifest_digest,
        }

    @property
    def digest(self) -> str:
        return canonical_digest(self.canonical_body())


RUNTIME_EVALUATION_V2_AUTHORITY_FIELDS = frozenset(
    {
        "accepted_set_change",
        "admission_authority",
        "dispatch_authority",
        "gate_authority",
        "invocation_delivery_authority",
        "observer_gate_authority",
        "observer_progress_authority",
        "output_capture_authority",
        "production_authority",
        "production_enabled",
        "promotion_authority",
        "terminal_authority",
        "terminal_evidence_authority",
    }
)
RUNTIME_EVALUATION_BINDING_V2_SCHEMA_ID = (
    "muteki.runtime-evaluation-binding.v2"
)
RUNTIME_EVALUATION_EXECUTABLE_SPLITS_V2 = frozenset(
    {"architecture_search", "development", "fresh_holdout"}
)
_EVALUATION_ASSIGNMENT_V2_SCHEMA_ID = "muteki.c6-eval-assignment-binding.v2"
_EVALUATION_RUN_MANIFEST_V2_SCHEMA_ID = "muteki.c6-eval-run-manifest.v2"
_EVALUATION_ROOT_BUDGET_V2_SCHEMA_ID = "muteki.c6-assignment-root-budget.v2"
_EVALUATION_ROLE_PLAN_V2_SCHEMA_ID = "muteki.c6-trial-role-plan.v2"
_EVALUATION_ROLE_SLOT_V2_SCHEMA_ID = "muteki.c6-trial-role-slot.v2"
_EVALUATION_ATTEMPT_ROLE_V2_SCHEMA_ID = "muteki.c6-attempt-role-binding.v2"
_EVALUATION_INVOCATION_V2_SCHEMA_ID = "muteki.c6-attempt-invocation-spec.v2"
_EVALUATION_FEATURE_VERSION_V2 = "muteki.c6-eval-execution.v2"
_EVALUATION_SPLITS_V2 = {
    "architecture_search",
    "development",
    "fresh_holdout",
    "sealed_final",
}
_EVALUATION_BUDGET_AXES_V2 = {
    "attempts",
    "context_tokens",
    "cost_micro_usd",
    "output_bytes",
    "tokens",
    "tool_calls",
    "wall_ms",
    "worker_ms",
}
_EVALUATION_ASSIGNMENT_FIELDS_V2 = {
    "accepted_set_change", "arm_id", "arm_config_digest",
    "assignment_budget_digest", "assignment_id",
    "assignment_issue_receipt_digest", "assignment_object_digest",
    "author_group_digest", "budget_point_digest", "checker_commitment_digest",
    "compiler_digest", "context_policy_digest", "decision_need_policy_digest",
    "environment_digest", "expected_run_id", "feature_state_receipt_digest",
    "feature_version", "fixture_commitment_digest", "fixture_digest",
    "independence_cluster_digest", "mode", "offline_policy_digest",
    "predecision_input_digest", "price_table_digest",
    "randomization_journal_anchor_digest", "randomized_study_digest",
    "role_plan", "role_plan_digest", "run_manifest_digest", "schema_id",
    "source_family_digest", "source_registry_digest",
    "source_registry_receipt_digest", "source_root_digest", "split",
    "stage_digest", "study_ledger_freeze_anchor_digest",
    "study_manifest_digest", "subgroup_metadata_digest", "worktree_digest",
}
_EVALUATION_ATTEMPT_ROLE_FIELDS_V2 = {
    "accepted_set_change", "assignment_binding_digest", "attempt_id",
    "attempt_identity_digest", "branch_id", "execution_generation",
    "input_spec", "invocation_spec", "launch_ordinal", "ordinal",
    "permit_digest", "permit_id", "policy_digest", "preallocation_digest",
    "prerequisite_attempt_binding_digests",
    "prerequisite_terminal_event_digests", "role", "role_budget_digest",
    "run_fence_epoch", "run_id", "run_manifest_digest", "schema_id",
    "scope_digest", "slot_id",
}
_EVALUATION_INVOCATION_FIELDS_V2 = {
    "arm_config_digest", "argv_spec_digest", "effect_policy_digest",
    "engine_id", "executable_artifact_digest", "executable_version_digest",
    "fallback_allowed", "invocation_ordinal", "knowledge_base_enabled",
    "materialized_prompt_byte_count", "materialized_prompt_cas_receipt_digest",
    "materialized_prompt_digest", "model_id", "native_web_enabled",
    "network_policy_digest", "offline_policy_digest",
    "output_capture_policy_digest", "prompt_in_argv", "prompt_template_digest",
    "provider_config_digest", "provider_egress_only", "read_policy_digest",
    "resume", "role_policy_digest", "sandbox_policy_digest", "schema_id",
    "session_persistence", "target_attempt_id", "tool_policy_digest",
    "transport_mode",
}


@dataclass(frozen=True, slots=True)
class RuntimeEvaluationBindingV2:
    """Opaque evaluator facts reduced to identities the runtime can enforce.

    The runtime never imports the cognitive/evaluator package.  An evaluator-side
    adapter validates its rich objects and freezes their complete canonical bodies
    here; admission and launch then reason only about generic run, permit, role,
    prerequisite, and budget identities.
    """

    assignment_body: Mapping[str, Any]
    assignment_binding_digest: str
    attempt_role_body: Mapping[str, Any]
    attempt_role_binding_digest: str
    run_manifest_digest: str
    expected_run_id: str
    arm_id: str
    split: str
    root_budget_body: Mapping[str, Any]
    root_budget_digest: str
    slot_id: str
    role: str
    ordinal: int
    preallocation_digest: str
    run_id: str
    run_fence_epoch: int
    execution_generation: int
    branch_id: str
    launch_ordinal: int
    attempt_id: str
    attempt_identity_digest: str
    permit_id: str
    permit_digest: str
    scope_digest: str
    prerequisite_slot_ids: tuple[str, ...]
    prerequisite_attempt_ids: tuple[str, ...]
    prerequisite_attempt_binding_digests: tuple[str, ...]
    prerequisite_terminal_event_digests: tuple[str, ...]
    role_budget: Mapping[str, int]
    role_budget_digest: str
    policy_digest: str
    authority_flags: Mapping[str, bool]
    schema_id: str = RUNTIME_EVALUATION_BINDING_V2_SCHEMA_ID

    def __post_init__(self) -> None:
        for name in (
            "assignment_binding_digest",
            "attempt_role_binding_digest",
            "run_manifest_digest",
            "root_budget_digest",
            "preallocation_digest",
            "attempt_identity_digest",
            "permit_digest",
            "scope_digest",
            "role_budget_digest",
            "policy_digest",
        ):
            object.__setattr__(self, name, _digest(getattr(self, name), name))
        for name in (
            "expected_run_id",
            "arm_id",
            "split",
            "slot_id",
            "role",
            "run_id",
            "branch_id",
            "attempt_id",
            "permit_id",
        ):
            object.__setattr__(self, name, _text(getattr(self, name), name))
        for name in (
            "ordinal",
            "run_fence_epoch",
            "execution_generation",
            "launch_ordinal",
        ):
            value = getattr(self, name)
            if type(value) is not int or value < 1:
                raise ValueError(f"{name} must be a positive integer")
        if self.role not in {"observer", "executor"} or self.slot_id != self.role:
            raise ValueError("runtime evaluation role/slot identity is invalid")
        if self.split not in _EVALUATION_SPLITS_V2:
            raise ValueError("runtime evaluation split is unsupported")
        if self.run_id != self.expected_run_id:
            raise ValueError("runtime evaluation binding is rebound to another run")
        assignment = dict(self.assignment_body)
        attempt_role = dict(self.attempt_role_body)
        root_budget = dict(self.root_budget_body)
        if not assignment or not attempt_role or not root_budget:
            raise ValueError("runtime evaluation opaque bodies are required")
        object.__setattr__(self, "assignment_body", freeze_json(assignment))
        object.__setattr__(self, "attempt_role_body", freeze_json(attempt_role))
        object.__setattr__(self, "root_budget_body", freeze_json(root_budget))
        if canonical_digest(assignment) != self.assignment_binding_digest:
            raise ValueError("assignment body does not match its digest")
        if canonical_digest(attempt_role) != self.attempt_role_binding_digest:
            raise ValueError("attempt-role body does not match its digest")
        if canonical_digest(root_budget) != self.root_budget_digest:
            raise ValueError("root-budget body does not match its digest")
        if set(assignment) != _EVALUATION_ASSIGNMENT_FIELDS_V2:
            raise ValueError("assignment body shape is not versioned")
        manifest_binding = {
            name: value
            for name, value in assignment.items()
            if name not in {"role_plan", "run_manifest_digest", "schema_id"}
        }
        if (
            assignment.get("run_manifest_digest") != self.run_manifest_digest
            or self.run_manifest_digest
            != canonical_digest(
                {
                    "binding": manifest_binding,
                    "schema_id": _EVALUATION_RUN_MANIFEST_V2_SCHEMA_ID,
                }
            )
            or assignment.get("expected_run_id") != self.expected_run_id
            or assignment.get("arm_id") != self.arm_id
            or assignment.get("accepted_set_change") is not False
            or assignment.get("schema_id") != _EVALUATION_ASSIGNMENT_V2_SCHEMA_ID
            or assignment.get("feature_version")
            != _EVALUATION_FEATURE_VERSION_V2
            or assignment.get("mode") != "shadow"
            or assignment.get("split") != self.split
        ):
            raise ValueError("assignment body is rebound to runtime identity")
        role_plan = assignment.get("role_plan")
        if type(role_plan) is not dict or set(role_plan) != {
            "arm_id",
            "root_budget",
            "schema_id",
            "slots",
        }:
            raise ValueError("assignment role plan shape is not versioned")
        if (
            role_plan.get("arm_id") != self.arm_id
            or role_plan.get("schema_id") != _EVALUATION_ROLE_PLAN_V2_SCHEMA_ID
            or assignment.get("role_plan_digest") != canonical_digest(role_plan)
            or canonical_digest(role_plan.get("root_budget"))
            != self.root_budget_digest
            or canonical_digest(role_plan.get("root_budget"))
            != canonical_digest(root_budget)
        ):
            raise ValueError("assignment role plan/root binding is false")
        if (
            set(root_budget) != {"assignment_budget", "reserve", "schema_id"}
            or root_budget.get("schema_id")
            != _EVALUATION_ROOT_BUDGET_V2_SCHEMA_ID
        ):
            raise ValueError("root budget shape is not versioned")

        def budget_map(value: object, name: str) -> dict[str, int]:
            if not isinstance(value, Mapping):
                raise ValueError(f"{name} must be a mapping")
            result: dict[str, int] = {}
            for axis, amount in value.items():
                if (
                    type(axis) is not str
                    or not axis
                    or axis != axis.strip()
                    or type(amount) is not int
                    or amount < 0
                ):
                    raise ValueError(
                        f"{name} must contain canonical non-negative axes"
                    )
                result[axis] = amount
            if not result:
                raise ValueError(f"{name} is required")
            return result

        assignment_budget = budget_map(
            root_budget["assignment_budget"], "assignment budget"
        )
        reserve_budget = budget_map(root_budget["reserve"], "reserve budget")
        if set(assignment_budget) != set(reserve_budget):
            raise ValueError("root reserve axes diverge from assignment budget")
        if set(assignment_budget) != _EVALUATION_BUDGET_AXES_V2:
            raise ValueError("assignment budget axes are not the frozen v2 axes")
        if assignment.get("assignment_budget_digest") != canonical_digest(
            {"budget": assignment_budget}
        ):
            raise ValueError("assignment budget digest is false")
        slots = role_plan.get("slots")
        if type(slots) is not list or not slots:
            raise ValueError("assignment role slots must be a non-empty JSON list")
        seen_slots: set[str] = set()
        seen_attempts: set[str] = set()
        seen_preallocations: set[str] = set()
        slot_sequence: list[tuple[str, str, list[str]]] = []
        allocated = {axis: 0 for axis in assignment_budget}
        selected_slot: Mapping[str, Any] | None = None
        for expected_ordinal, raw_slot in enumerate(slots, start=1):
            if type(raw_slot) is not dict or set(raw_slot) != {
                "child_budget",
                "input_kind",
                "ordinal",
                "preallocated_attempt_id",
                "preallocation_digest",
                "prerequisite_slot_ids",
                "role",
                "role_policy_digest",
                "schema_id",
                "slot_id",
            }:
                raise ValueError("assignment role slot shape is not versioned")
            slot_id = raw_slot["slot_id"]
            slot_attempt = raw_slot["preallocated_attempt_id"]
            slot_preallocation = raw_slot["preallocation_digest"]
            prerequisites = raw_slot["prerequisite_slot_ids"]
            if (
                type(slot_id) is not str
                or not slot_id
                or slot_id != raw_slot["role"]
                or raw_slot["role"] not in {"observer", "executor"}
                or raw_slot["schema_id"] != _EVALUATION_ROLE_SLOT_V2_SCHEMA_ID
                or type(slot_attempt) is not str
                or not slot_attempt
                or type(raw_slot["input_kind"]) is not str
                or not raw_slot["input_kind"]
                or not _is_digest_text(slot_preallocation)
                or not _is_digest_text(raw_slot["role_policy_digest"])
                or raw_slot["ordinal"] != expected_ordinal
                or type(prerequisites) is not list
                or any(type(item) is not str or not item for item in prerequisites)
                or any(item not in seen_slots for item in prerequisites)
                or len(prerequisites) != len(set(prerequisites))
                or slot_id in seen_slots
                or slot_attempt in seen_attempts
                or slot_preallocation in seen_preallocations
            ):
                raise ValueError("assignment role slot identity/ordering is false")
            child = budget_map(raw_slot["child_budget"], "child role budget")
            if set(child) != set(assignment_budget) or child.get("attempts") != 1:
                raise ValueError("child role budget axes/attempt ceiling diverge")
            for axis, amount in child.items():
                allocated[axis] += amount
            seen_slots.add(slot_id)
            seen_attempts.add(slot_attempt)
            seen_preallocations.add(slot_preallocation)
            slot_sequence.append(
                (slot_id, str(raw_slot["role"]), list(prerequisites))
            )
            if slot_id == self.slot_id:
                selected_slot = raw_slot
        if self.arm_id == "model_decision_need_r0_c6":
            if slot_sequence != [
                ("observer", "observer", []),
                ("executor", "executor", ["observer"]),
            ]:
                raise ValueError("model-R0 role plan is not observer then executor")
        elif slot_sequence != [("executor", "executor", [])]:
            raise ValueError("single-phase role plan must contain one executor")
        for axis, root_amount in assignment_budget.items():
            if allocated[axis] + reserve_budget[axis] != root_amount:
                raise ValueError("role budgets and reserve do not partition the root")
        if selected_slot is None:
            raise ValueError("runtime evaluation binds an unknown role slot")
        selected_prerequisites = selected_slot["prerequisite_slot_ids"]
        input_spec = attempt_role.get("input_spec")
        if (
            selected_slot["role"] != self.role
            or selected_slot["ordinal"] != self.ordinal
            or selected_slot["preallocated_attempt_id"] != self.attempt_id
            or selected_slot["preallocation_digest"]
            != self.preallocation_digest
            or selected_slot["role_policy_digest"] != self.policy_digest
            or budget_map(selected_slot["child_budget"], "selected role budget")
            != dict(self.role_budget)
            or selected_prerequisites != list(self.prerequisite_slot_ids)
            or not isinstance(input_spec, Mapping)
            or input_spec.get("kind") != selected_slot["input_kind"]
        ):
            raise ValueError("runtime evaluation selected role slot is rebound")
        if (
            set(attempt_role) != _EVALUATION_ATTEMPT_ROLE_FIELDS_V2
            or attempt_role.get("schema_id")
            != _EVALUATION_ATTEMPT_ROLE_V2_SCHEMA_ID
        ):
            raise ValueError("attempt-role body shape/schema is not versioned")
        if any(
            not _is_digest_text(value)
            for name, value in assignment.items()
            if name.endswith("_digest")
        ):
            raise ValueError("assignment body contains a malformed digest")
        input_kind = selected_slot["input_kind"]
        input_shapes = {
            "observer_sealed_predecision": {
                "cutoff_head_event_digest", "kind", "predecision_input_digest",
                "sealed_prefix_digest", "target_attempt_id",
            },
            "baseline_legacy_s4": {
                "kind", "legacy_s4_context_digest", "predecision_input_digest",
                "target_attempt_id",
            },
            "candidate_context_packet": {
                "compiler_digest", "compiler_receipt_digest",
                "compiler_request_digest", "context_packet_byte_count",
                "context_packet_cas_receipt_digest", "context_packet_digest",
                "context_packet_manifest_digest", "context_policy_digest",
                "decision_need_authority_receipt_digest", "decision_need_digest",
                "decision_need_origin", "decision_need_policy_digest",
                "decision_need_proposal_digest", "kind",
                "producer_observer_attempt_id",
                "producer_observer_binding_digest",
                "producer_observer_terminal_event_digest",
                "proposal_capture_receipt_digest",
                "source_predecision_input_digest", "target_attempt_id",
            },
        }
        expected_input_kind = {
            ("current_s4_baseline", "executor"): "baseline_legacy_s4",
            ("deterministic_decision_need_c6", "executor"): (
                "candidate_context_packet"
            ),
            ("model_decision_need_r0_c6", "observer"): (
                "observer_sealed_predecision"
            ),
            ("model_decision_need_r0_c6", "executor"): (
                "candidate_context_packet"
            ),
        }.get((self.arm_id, self.role))
        if (
            expected_input_kind is None
            or input_kind != expected_input_kind
            or set(input_spec) != input_shapes[input_kind]
            or input_spec.get("target_attempt_id") != self.attempt_id
        ):
            raise ValueError("attempt input union is rebound or unversioned")
        if input_kind in {"observer_sealed_predecision", "baseline_legacy_s4"}:
            if (
                input_spec.get("predecision_input_digest")
                != assignment.get("predecision_input_digest")
            ):
                raise ValueError("attempt input is rebound from predecision state")
        else:
            if (
                input_spec.get("source_predecision_input_digest")
                != assignment.get("predecision_input_digest")
                or input_spec.get("decision_need_policy_digest")
                != assignment.get("decision_need_policy_digest")
                or input_spec.get("context_policy_digest")
                != assignment.get("context_policy_digest")
                or input_spec.get("compiler_digest")
                != assignment.get("compiler_digest")
            ):
                raise ValueError("candidate packet is rebound from assignment policy")
            if self.arm_id == "deterministic_decision_need_c6":
                if (
                    input_spec.get("decision_need_origin") != "deterministic"
                    or input_spec.get("producer_observer_attempt_id") is not None
                    or input_spec.get("producer_observer_binding_digest") is not None
                    or input_spec.get("producer_observer_terminal_event_digest")
                    is not None
                ):
                    raise ValueError("deterministic input claims observer lineage")
            elif self.role == "executor" and (
                input_spec.get("decision_need_origin") != "model_r0_observer"
            ):
                raise ValueError("model-R0 executor input origin is false")
        invocation = attempt_role.get("invocation_spec")
        if (
            type(invocation) is not dict
            or set(invocation) != _EVALUATION_INVOCATION_FIELDS_V2
            or invocation.get("schema_id") != _EVALUATION_INVOCATION_V2_SCHEMA_ID
            or invocation.get("target_attempt_id") != self.attempt_id
            or invocation.get("arm_config_digest")
            != assignment.get("arm_config_digest")
            or invocation.get("role_policy_digest") != self.policy_digest
            or invocation.get("offline_policy_digest")
            != assignment.get("offline_policy_digest")
            or invocation.get("native_web_enabled") is not False
            or invocation.get("knowledge_base_enabled") is not False
            or invocation.get("provider_egress_only") is not True
            or invocation.get("transport_mode") != "stdin_ephemeral"
            or invocation.get("prompt_in_argv") is not False
            or invocation.get("resume") is not False
            or invocation.get("session_persistence") is not False
            or invocation.get("invocation_ordinal") != 1
            or invocation.get("fallback_allowed") is not False
        ):
            raise ValueError("attempt invocation spec is rebound or unversioned")
        if any(
            not _is_digest_text(value)
            for source in (input_spec, invocation)
            for name, value in source.items()
            if name.endswith("_digest") and value is not None
        ):
            raise ValueError("attempt input/invocation contains a malformed digest")
        expected_attempt_fields = {
            "accepted_set_change": False,
            "assignment_binding_digest": self.assignment_binding_digest,
            "attempt_id": self.attempt_id,
            "attempt_identity_digest": self.attempt_identity_digest,
            "branch_id": self.branch_id,
            "execution_generation": self.execution_generation,
            "launch_ordinal": self.launch_ordinal,
            "ordinal": self.ordinal,
            "permit_digest": self.permit_digest,
            "permit_id": self.permit_id,
            "policy_digest": self.policy_digest,
            "preallocation_digest": self.preallocation_digest,
            "role": self.role,
            "role_budget_digest": self.role_budget_digest,
            "run_id": self.run_id,
            "run_fence_epoch": self.run_fence_epoch,
            "run_manifest_digest": self.run_manifest_digest,
            "scope_digest": self.scope_digest,
            "slot_id": self.slot_id,
        }
        if any(attempt_role.get(name) != value for name, value in expected_attempt_fields.items()):
            raise ValueError("attempt-role body is rebound to runtime identity")
        if attempt_role.get("prerequisite_attempt_binding_digests") != list(
            self.prerequisite_attempt_binding_digests
        ) or attempt_role.get("prerequisite_terminal_event_digests") != list(
            self.prerequisite_terminal_event_digests
        ):
            raise ValueError("attempt-role prerequisite inventory is rebound")
        expected_scope = ExecutionScope(
            self.run_id, self.run_fence_epoch, self.execution_generation
        )
        if expected_scope.digest != self.scope_digest:
            raise ValueError("runtime evaluation scope digest is false")
        expected_attempt = AttemptIdentity(
            expected_scope,
            self.branch_id,
            self.attempt_id,
            self.launch_ordinal,
        )
        if expected_attempt.digest != self.attempt_identity_digest:
            raise ValueError("runtime evaluation attempt digest is false")
        prerequisite_slot_ids = tuple(
            _text(item, "prerequisite_slot_id")
            for item in self.prerequisite_slot_ids
        )
        prerequisite_attempt_ids = tuple(
            _text(item, "prerequisite_attempt_id")
            for item in self.prerequisite_attempt_ids
        )
        binding_digests = tuple(
            _digest(item, "prerequisite_attempt_binding_digest")
            for item in self.prerequisite_attempt_binding_digests
        )
        terminal_digests = tuple(
            _digest(item, "prerequisite_terminal_event_digest")
            for item in self.prerequisite_terminal_event_digests
        )
        if (
            len(binding_digests) != len(set(binding_digests))
            or len(terminal_digests) != len(set(terminal_digests))
            or len(prerequisite_attempt_ids) != len(set(prerequisite_attempt_ids))
            or len(prerequisite_slot_ids) != len(set(prerequisite_slot_ids))
            or len(binding_digests) != len(terminal_digests)
            or len(binding_digests) != len(prerequisite_attempt_ids)
            or len(binding_digests) != len(prerequisite_slot_ids)
        ):
            raise ValueError("runtime evaluation prerequisites are not one-to-one")
        if self.arm_id == "model_decision_need_r0_c6":
            expected_count = 0 if self.role == "observer" else 1
            if len(binding_digests) != expected_count:
                raise ValueError(
                    "model-R0 runtime roles require observer-to-executor lineage"
                )
            input_spec = attempt_role.get("input_spec")
            if self.role == "executor" and (
                not isinstance(input_spec, Mapping)
                or input_spec.get("producer_observer_attempt_id")
                != prerequisite_attempt_ids[0]
                or input_spec.get("producer_observer_binding_digest")
                != binding_digests[0]
                or input_spec.get("producer_observer_terminal_event_digest")
                != terminal_digests[0]
            ):
                raise ValueError(
                    "model-R0 packet is rebound from its observer prerequisite"
                )
        elif self.role != "executor" or binding_digests:
            raise ValueError(
                "single-phase runtime evaluation arms have one unblocked executor"
            )
        object.__setattr__(
            self, "prerequisite_slot_ids", prerequisite_slot_ids
        )
        object.__setattr__(
            self, "prerequisite_attempt_ids", prerequisite_attempt_ids
        )
        object.__setattr__(
            self, "prerequisite_attempt_binding_digests", binding_digests
        )
        object.__setattr__(
            self, "prerequisite_terminal_event_digests", terminal_digests
        )
        budget: dict[str, int] = {}
        for axis, amount in dict(self.role_budget).items():
            if (
                type(axis) is not str
                or not axis
                or axis != axis.strip()
                or type(amount) is not int
                or amount < 0
            ):
                raise ValueError("role budget must contain canonical non-negative axes")
            budget[axis] = amount
        if not budget or budget.get("attempts") != 1:
            raise ValueError("one runtime evaluation role consumes one attempt")
        if canonical_digest({"budget": budget}) != self.role_budget_digest:
            raise ValueError("role budget does not match its digest")
        object.__setattr__(self, "role_budget", freeze_json(budget))
        assignment_budget = root_budget.get("assignment_budget")
        if not isinstance(assignment_budget, Mapping) or any(
            axis not in assignment_budget
            or type(assignment_budget[axis]) is not int
            or amount > assignment_budget[axis]
            for axis, amount in budget.items()
        ):
            raise ValueError("role budget exceeds or diverges from the root budget")
        flags = dict(self.authority_flags)
        if set(flags) != RUNTIME_EVALUATION_V2_AUTHORITY_FIELDS or any(
            type(value) is not bool or value for value in flags.values()
        ):
            raise ValueError("runtime evaluation v2 authority flags must all be false")
        object.__setattr__(self, "authority_flags", freeze_json(flags))
        if self.schema_id != RUNTIME_EVALUATION_BINDING_V2_SCHEMA_ID:
            raise ValueError("unsupported runtime evaluation binding schema")

    def canonical_body(self) -> dict[str, Any]:
        return {
            "arm_id": self.arm_id,
            "assignment_binding_digest": self.assignment_binding_digest,
            "assignment_body": self.assignment_body,
            "attempt_id": self.attempt_id,
            "attempt_identity_digest": self.attempt_identity_digest,
            "attempt_role_binding_digest": self.attempt_role_binding_digest,
            "attempt_role_body": self.attempt_role_body,
            "authority_flags": self.authority_flags,
            "branch_id": self.branch_id,
            "execution_generation": self.execution_generation,
            "expected_run_id": self.expected_run_id,
            "launch_ordinal": self.launch_ordinal,
            "ordinal": self.ordinal,
            "permit_digest": self.permit_digest,
            "permit_id": self.permit_id,
            "policy_digest": self.policy_digest,
            "preallocation_digest": self.preallocation_digest,
            "prerequisite_slot_ids": list(self.prerequisite_slot_ids),
            "prerequisite_attempt_ids": list(self.prerequisite_attempt_ids),
            "prerequisite_attempt_binding_digests": list(
                self.prerequisite_attempt_binding_digests
            ),
            "prerequisite_terminal_event_digests": list(
                self.prerequisite_terminal_event_digests
            ),
            "role": self.role,
            "role_budget": self.role_budget,
            "role_budget_digest": self.role_budget_digest,
            "root_budget_body": self.root_budget_body,
            "root_budget_digest": self.root_budget_digest,
            "run_fence_epoch": self.run_fence_epoch,
            "run_id": self.run_id,
            "run_manifest_digest": self.run_manifest_digest,
            "schema_id": self.schema_id,
            "scope_digest": self.scope_digest,
            "split": self.split,
            "slot_id": self.slot_id,
        }

    @property
    def digest(self) -> str:
        return canonical_digest(self.canonical_body())

    @classmethod
    def from_canonical(cls, value: object) -> "RuntimeEvaluationBindingV2":
        if type(value) is not dict:
            raise ValueError("runtime evaluation binding must be an exact object")
        expected = {
            "arm_id", "assignment_binding_digest", "assignment_body",
            "attempt_id", "attempt_identity_digest",
            "attempt_role_binding_digest", "attempt_role_body", "authority_flags",
            "branch_id", "execution_generation", "expected_run_id",
            "launch_ordinal", "ordinal", "permit_digest", "permit_id",
            "policy_digest", "preallocation_digest",
            "prerequisite_slot_ids",
            "prerequisite_attempt_ids",
            "prerequisite_attempt_binding_digests",
            "prerequisite_terminal_event_digests", "role", "role_budget",
            "role_budget_digest", "root_budget_body", "root_budget_digest",
            "run_fence_epoch", "run_id", "run_manifest_digest", "schema_id",
            "scope_digest", "split", "slot_id",
        }
        if set(value) != expected:
            raise ValueError("runtime evaluation binding shape is not versioned")
        binding_prereqs = value["prerequisite_attempt_binding_digests"]
        terminal_prereqs = value["prerequisite_terminal_event_digests"]
        attempt_prereqs = value["prerequisite_attempt_ids"]
        slot_prereqs = value["prerequisite_slot_ids"]
        if (
            type(slot_prereqs) is not list
            or type(attempt_prereqs) is not list
            or type(binding_prereqs) is not list
            or type(terminal_prereqs) is not list
        ):
            raise ValueError("runtime evaluation prerequisites must be JSON lists")
        return cls(
            **{
                **value,
                "prerequisite_slot_ids": tuple(slot_prereqs),
                "prerequisite_attempt_ids": tuple(attempt_prereqs),
                "prerequisite_attempt_binding_digests": tuple(binding_prereqs),
                "prerequisite_terminal_event_digests": tuple(terminal_prereqs),
            }
        )


@dataclass(frozen=True, slots=True)
class AttemptPermit:
    permit_id: str
    lease: LeaseIdentity
    policy_digest: str
    reservation_ids: tuple[str, ...]
    effect_class: EffectClass
    expires_at_ns: int
    constraints: Mapping[str, Any] = field(default_factory=dict)

    def __post_init__(self) -> None:
        object.__setattr__(self, "permit_id", _text(self.permit_id, "permit_id"))
        if type(self.lease) is not LeaseIdentity:
            raise TypeError("lease must be LeaseIdentity")
        object.__setattr__(
            self, "policy_digest", _digest(self.policy_digest, "policy_digest")
        )
        if type(self.expires_at_ns) is not int or self.expires_at_ns < 0:
            raise ValueError("expires_at_ns must be non-negative")
        if type(self.effect_class) is not EffectClass:
            raise TypeError("effect_class must be EffectClass")
        reservations = tuple(
            _text(item, "reservation_id") for item in self.reservation_ids
        )
        if not reservations or len(set(reservations)) != len(reservations):
            raise ValueError("reservation_ids must be non-empty and unique")
        object.__setattr__(self, "reservation_ids", reservations)
        object.__setattr__(self, "constraints", freeze_json(self.constraints))

    def canonical_body(self) -> dict[str, Any]:
        return {
            "constraints": self.constraints,
            "effect_class": self.effect_class.value,
            "expires_at_ns": self.expires_at_ns,
            "lease_digest": self.lease.digest,
            "permit_id": self.permit_id,
            "policy_digest": self.policy_digest,
            "reservation_ids": self.reservation_ids,
        }

    @property
    def digest(self) -> str:
        return canonical_digest(self.canonical_body())
