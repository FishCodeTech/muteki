"""Transactional search admission and multi-dimensional budget reservations."""

from __future__ import annotations

from collections.abc import Mapping, Sequence
from dataclasses import dataclass
from enum import Enum
from typing import Any

from muteki.epistemic.sqlite_store import (
    CommandEvent,
    EpistemicSQLiteStore,
    IntegrityError,
    ProjectionMutation,
)
from muteki.epistemic.cas import ReceiptCAS
from muteki.epistemic.contracts import (
    EventEnvelopeV2,
    canonical_digest,
    canonical_json_bytes,
    freeze_json,
)
from muteki.epistemic.cognitive_events_v1 import (
    COGNITIVE_BINDING_ACTOR,
    COGNITIVE_EXPERIMENT_ASSIGNED,
    CognitiveExperimentBindingV1,
    cognitive_assignment_payload,
    cognitive_runtime_context_assignment_payload,
    cognitive_runtime_context_executable_assignment_payload,
    cognitive_runtime_reproduction_assignment_payload,
)
from muteki.epistemic.receipt_objects import VerifiedReceiptPrefixV1
from muteki.runtime.contracts import (
    AttemptIdentity,
    AttemptPermit,
    ContextPacketAdmissionBindingV1,
    EffectClass,
    EvaluationExecutionBindingV1,
    LeaseIdentity,
    RuntimeEvaluationBindingV2,
    RUNTIME_EVALUATION_EXECUTABLE_SPLITS_V2,
)
from muteki.runtime.controller import CommandClass, LiveHealthGuard
from muteki.runtime.canonical_cognitive_selection_v1 import (
    COGNITIVE_CANONICAL_SELECTION_ACTOR,
    COGNITIVE_CANONICAL_SELECTION_BOUND,
    canonical_cycle_request_binding_body_v1,
    canonical_selection_sidecar_payload_v1,
    reconstruct_resolved_cognitive_facts_v1,
    validate_canonical_selection_against_store,
)
from muteki.runtime.canonical_cognitive_continuation_v2 import (
    COGNITIVE_CANONICAL_CONTINUATION_ACTOR_V2,
    COGNITIVE_CANONICAL_CONTINUATION_BOUND_V2,
    COGNITIVE_CANONICAL_CONTINUATION_MUTATION_V2,
    canonical_continuation_sidecar_payload_v2,
    validate_canonical_continuation_against_store_v2,
    validate_canonical_continuation_sidecar_shape_v2,
)
from muteki.runtime.executable_experiment_v1 import ExecutableExperimentBindingV1
from muteki.runtime.canonical_cognitive_cycle_v1 import (
    CanonicalCognitiveCycleModeV1,
    CanonicalCognitiveCyclePlanV1,
    CanonicalCognitiveCycleRequestV1,
    plan_canonical_cognitive_cycle_v1,
)
from muteki.runtime.hypothesis import HypothesisSelector
from muteki.runtime.usage import UsageNotEstimable, UsageReport


class BudgetDimension(str, Enum):
    WALL_MS = "wall_ms"
    TOKENS = "tokens"
    COST_MICRO_USD = "cost_micro_usd"
    TOOL_CALLS = "tool_calls"
    WORKER_MS = "worker_ms"
    ATTEMPTS = "attempts"


@dataclass(frozen=True, slots=True)
class AdmissionRequest:
    attempt: AttemptIdentity
    lease: LeaseIdentity
    permit_id: str
    account_id: str
    requested_budget: Mapping[str, int]
    conflict_keys: tuple[str, ...]
    effect_class: EffectClass
    fingerprint: str
    policy_digest: str
    expires_at_ns: int
    context_packet: ContextPacketAdmissionBindingV1 | None = None

    def __post_init__(self) -> None:
        if type(self.attempt) is not AttemptIdentity:
            raise TypeError("attempt must be AttemptIdentity")
        if type(self.lease) is not LeaseIdentity or self.lease.attempt != self.attempt:
            raise ValueError("lease does not belong to the requested attempt")
        for name in ("permit_id", "account_id", "fingerprint"):
            value = getattr(self, name)
            if type(value) is not str or not value or value != value.strip():
                raise ValueError(f"{name} must be a non-empty canonical string")
        policy = self.policy_digest
        if type(policy) is not str:
            raise ValueError("policy_digest must be a lowercase sha256 digest")
        if len(policy) != 64 or any(ch not in "0123456789abcdef" for ch in policy):
            raise ValueError("policy_digest must be a lowercase sha256 digest")
        object.__setattr__(self, "policy_digest", policy)
        if not isinstance(self.requested_budget, Mapping):
            raise TypeError("requested_budget must be a mapping")
        requested = {}
        for key, value in self.requested_budget.items():
            if (
                type(key) is not str
                or not key
                or key != key.strip()
                or type(value) is not int
                or value < 0
            ):
                raise ValueError(
                    "requested budget must use named non-negative integers"
                )
            requested[key] = value
        if not requested:
            raise ValueError("requested budget is required")
        object.__setattr__(self, "requested_budget", freeze_json(requested))
        if type(self.conflict_keys) is not tuple:
            raise TypeError("conflict_keys must be a built-in tuple")
        conflicts = self.conflict_keys
        if any(
            type(item) is not str or not item or item != item.strip()
            for item in conflicts
        ) or len(set(conflicts)) != len(conflicts):
            raise ValueError("conflict_keys must be non-empty and unique")
        object.__setattr__(self, "conflict_keys", conflicts)
        if type(self.effect_class) is not EffectClass:
            raise TypeError("effect_class must be EffectClass")
        if type(self.expires_at_ns) is not int or self.expires_at_ns < 0:
            raise ValueError("expires_at_ns must be a non-negative integer")
        if self.context_packet is not None:
            if type(self.context_packet) is not ContextPacketAdmissionBindingV1:
                raise TypeError(
                    "context_packet must be ContextPacketAdmissionBindingV1 or None"
                )
            if self.context_packet.target_attempt_id != self.attempt.attempt_id:
                raise ValueError("context packet belongs to another attempt")


class SearchAdmission:
    def __init__(
        self,
        *,
        store: EpistemicSQLiteStore,
        guard: LiveHealthGuard,
        cas: ReceiptCAS | None = None,
    ) -> None:
        self._store = store
        self._guard = guard
        if cas is not None and not isinstance(cas, ReceiptCAS):
            raise TypeError("cas must be ReceiptCAS or None")
        self._cas = cas

    def remaining_budget(self, *, account_id: str) -> dict[str, int]:
        """Read one canonical pre-admission budget snapshot.

        The result is only a snapshot.  Admission remains the authority that
        reserves it atomically; callers must never use this read as a permit.
        """

        return self._store.budget_remaining(account_id)

    def create_branch(
        self,
        *,
        branch_id: str,
        depends_on: Sequence[str] = (),
        max_attempts: int,
        occurred_at_ns: int,
    ) -> str:
        result = self._store.commit_command(
            command_id=f"branch:create:{branch_id}",
            idempotency_key=f"branch:create:{branch_id}",
            command_payload={
                "branch_id": branch_id,
                "depends_on": tuple(depends_on),
                "max_attempts": max_attempts,
            },
            events=[
                CommandEvent(
                    f"event:branch:create:{branch_id}",
                    "BRANCH_CREATED",
                    "search-admission",
                    occurred_at_ns,
                    {
                        "branch_id": branch_id,
                        "depends_on": tuple(depends_on),
                        "max_attempts": max_attempts,
                    },
                )
            ],
            projection_mutations=[
                ProjectionMutation(
                    "branch_create",
                    {
                        "branch_id": branch_id,
                        "depends_on": tuple(depends_on),
                        "max_attempts": max_attempts,
                    },
                )
            ],
            committed_at_ns=occurred_at_ns,
        )
        return result.receipt_digest

    def set_branch_state(
        self,
        *,
        branch_id: str,
        expected_state: str,
        new_state: str,
        revision: int,
        occurred_at_ns: int,
    ) -> str:
        result = self._store.commit_command(
            command_id=f"branch:state:{branch_id}:{revision}",
            idempotency_key=f"branch:state:{branch_id}:{revision}",
            command_payload={
                "branch_id": branch_id,
                "expected_state": expected_state,
                "new_state": new_state,
                "revision": revision,
            },
            events=[
                CommandEvent(
                    f"event:branch:state:{branch_id}:{revision}",
                    "BRANCH_STATE_CHANGED",
                    "search-admission",
                    occurred_at_ns,
                    {
                        "branch_id": branch_id,
                        "from": expected_state,
                        "to": new_state,
                        "revision": revision,
                    },
                )
            ],
            projection_mutations=[
                ProjectionMutation(
                    "branch_state",
                    {
                        "branch_id": branch_id,
                        "expected_state": expected_state,
                        "new_state": new_state,
                    },
                )
            ],
            committed_at_ns=occurred_at_ns,
        )
        return result.receipt_digest

    def create_budget_account(
        self,
        *,
        account_id: str,
        limits: Mapping[str, int],
        occurred_at_ns: int,
        parent_id: str = "",
    ) -> str:
        result = self._store.commit_command(
            command_id=f"budget:create:{account_id}",
            idempotency_key=f"budget:create:{account_id}",
            command_payload={
                "account_id": account_id,
                "parent_id": parent_id,
                "limits": limits,
            },
            events=[
                CommandEvent(
                    f"event:budget:create:{account_id}",
                    "BUDGET_ACCOUNT_CREATED",
                    "search-admission",
                    occurred_at_ns,
                    {
                        "account_id": account_id,
                        "parent_id": parent_id,
                        "limits": limits,
                    },
                )
            ],
            projection_mutations=[
                ProjectionMutation(
                    "budget_account_create",
                    {
                        "account_id": account_id,
                        "parent_id": parent_id,
                        "limits": limits,
                    },
                )
            ],
            committed_at_ns=occurred_at_ns,
        )
        return result.receipt_digest

    def admit(self, request: AdmissionRequest, *, occurred_at_ns: int) -> AttemptPermit:
        return self._admit(
            request, occurred_at_ns=occurred_at_ns, evaluation_binding=None
        )

    def admit_cognitive_context(
        self,
        request: AdmissionRequest,
        *,
        cognitive_experiment: CognitiveExperimentBindingV1,
        occurred_at_ns: int,
    ) -> AttemptPermit:
        """Explicitly admit one default-off experiment on the ordinary C6 path.

        The legacy ordinary and evaluation admissions remain separate.  This
        seam requires the real ``ContextPacketAdmissionBindingV1`` already carried
        by ``request`` and atomically binds one H5-eligible assignment to that
        same attempt and permit without creating an evaluator sidecar.  It does
        not prove a qualified runtime planner-policy selection.
        """

        if request.context_packet is None:
            raise ValueError(
                "runtime-context cognitive admission requires a ContextPacket"
            )
        if type(cognitive_experiment) is not CognitiveExperimentBindingV1:
            raise TypeError("cognitive_experiment must be CognitiveExperimentBindingV1")
        return self._admit(
            request,
            occurred_at_ns=occurred_at_ns,
            evaluation_binding=None,
            cognitive_experiment=cognitive_experiment,
        )

    def admit_canonical_cognitive_context(
        self,
        request: AdmissionRequest,
        *,
        cognitive_experiment: CognitiveExperimentBindingV1,
        canonical_cycle_request: CanonicalCognitiveCycleRequestV1,
        canonical_cycle_plan: CanonicalCognitiveCyclePlanV1,
        occurred_at_ns: int,
    ) -> AttemptPermit:
        """Admit only the exact next assignment from the canonical v1 cycle.

        This default-off entrypoint adds an inert selection sidecar beside the
        byte-identical runtime-context assignment.  The transaction guard
        independently rebuilds resolver facts and reruns the planner.
        """

        if request.context_packet is None:
            raise ValueError("canonical cognitive admission requires a ContextPacket")
        if type(cognitive_experiment) is not CognitiveExperimentBindingV1:
            raise TypeError("cognitive_experiment must be CognitiveExperimentBindingV1")
        if type(canonical_cycle_request) is not CanonicalCognitiveCycleRequestV1:
            raise TypeError(
                "canonical_cycle_request must be CanonicalCognitiveCycleRequestV1"
            )
        if type(canonical_cycle_plan) is not CanonicalCognitiveCyclePlanV1:
            raise TypeError(
                "canonical_cycle_plan must be CanonicalCognitiveCyclePlanV1"
            )
        if canonical_cycle_plan.mode is not CanonicalCognitiveCycleModeV1.EXPERIMENT:
            raise IntegrityError(
                "canonical v1 sidecar admission accepts only EXPERIMENT mode"
            )
        existing = tuple(
            row
            for row in self._store.event_rows(kind=COGNITIVE_CANONICAL_SELECTION_BOUND)
            if row["payload"].get("attempt_id") == request.attempt.attempt_id
        )
        if len(existing) > 1:
            raise IntegrityError("canonical selection retry is ambiguous")
        if existing:
            return self._resolve_canonical_selection_retry(
                request=request,
                cognitive_experiment=cognitive_experiment,
                canonical_cycle_request=canonical_cycle_request,
                canonical_cycle_plan=canonical_cycle_plan,
                sidecar_row=existing[0],
                occurred_at_ns=occurred_at_ns,
            )
        state = self._store.state()
        resolver = self._store.receipt_field_resolver(cutoff_seq=state.head_seq)
        prefix = resolver.verify_complete_through(state.head_seq)
        facts = reconstruct_resolved_cognitive_facts_v1(
            store=self._store,
            resolver=resolver,
            prefix=prefix,
            scope_digest=request.attempt.scope.digest,
        )
        replay_request = CanonicalCognitiveCycleRequestV1(
            h5_request=canonical_cycle_request.h5_request,
            initial_masses=canonical_cycle_request.initial_masses,
            resolved_facts=facts,
            cost_estimates=canonical_cycle_request.cost_estimates,
            remaining_cost_units=canonical_cycle_request.remaining_cost_units,
        )
        supplied_request_body = canonical_cycle_request_binding_body_v1(
            request=canonical_cycle_request,
            prefix=prefix,
        )
        replay_request_body = canonical_cycle_request_binding_body_v1(
            request=replay_request,
            prefix=prefix,
        )
        if canonical_json_bytes(supplied_request_body) != canonical_json_bytes(
            replay_request_body
        ):
            raise IntegrityError(
                "canonical cycle request omitted or spliced resolver facts"
            )
        replay_plan = plan_canonical_cognitive_cycle_v1(replay_request)
        if (
            replay_plan.mode is not CanonicalCognitiveCycleModeV1.EXPERIMENT
            or replay_plan != canonical_cycle_plan
            or replay_plan.next_assignment is None
        ):
            raise IntegrityError(
                "canonical cycle plan does not replay to one next assignment"
            )
        h5_plan = HypothesisSelector.recommend(replay_request.h5_request)
        selected = next(
            (
                item
                for item in replay_request.h5_request.candidates
                if item.digest == replay_plan.next_assignment.experiment_digest
            ),
            None,
        )
        if (
            selected is None
            or cognitive_experiment.decision_prefix_digest != prefix.digest
            or cognitive_experiment.decision_cutoff_seq != prefix.cutoff_seq
            or cognitive_experiment.decision_head_event_digest
            != prefix.head_event_digest
            or canonical_json_bytes(cognitive_experiment.assignment_body)
            != canonical_json_bytes(replay_plan.next_assignment.canonical_body())
            or canonical_json_bytes(cognitive_experiment.experiment_body)
            != canonical_json_bytes(selected.canonical_body())
            or canonical_json_bytes(cognitive_experiment.h5_request_body)
            != canonical_json_bytes(replay_request.h5_request.canonical_body())
            or canonical_json_bytes(cognitive_experiment.h5_selection_plan_body)
            != canonical_json_bytes(h5_plan.canonical_body())
        ):
            raise IntegrityError(
                "runtime assignment is not the exact canonical next assignment"
            )
        return self._admit(
            request,
            occurred_at_ns=occurred_at_ns,
            evaluation_binding=None,
            cognitive_experiment=cognitive_experiment,
            canonical_selection=(replay_request, replay_plan, prefix),
        )

    def _resolve_canonical_selection_retry(
        self,
        *,
        request: AdmissionRequest,
        cognitive_experiment: CognitiveExperimentBindingV1,
        canonical_cycle_request: CanonicalCognitiveCycleRequestV1,
        canonical_cycle_plan: CanonicalCognitiveCyclePlanV1,
        sidecar_row: Mapping[str, Any],
        occurred_at_ns: int,
    ) -> AttemptPermit:
        """Resolve an already-committed exact sidecar without moving its cutoff."""

        if request.expires_at_ns <= occurred_at_ns:
            raise ValueError("permit expiry must be after admission")
        state = self._store.state()
        self._guard.authorize(CommandClass.DISPATCH, state)
        scope = request.attempt.scope
        if (
            scope.run_id != state.run_id
            or scope.run_fence_epoch != state.run_fence_epoch
            or scope.execution_generation != state.execution_generation
        ):
            raise IntegrityError("attempt scope is not the current execution scope")
        sidecar = sidecar_row["payload"]
        request_body = sidecar.get("canonical_request_body")
        prefix_claim = (
            request_body.get("pre_admission_prefix")
            if isinstance(request_body, Mapping)
            else None
        )
        if not isinstance(prefix_claim, Mapping):
            raise IntegrityError("canonical selection retry prefix is absent")
        cutoff = prefix_claim.get("cutoff_seq")
        resolver = self._store.receipt_field_resolver(cutoff_seq=cutoff)
        prefix = resolver.verify_complete_through(cutoff)
        supplied_request_body = canonical_cycle_request_binding_body_v1(
            request=canonical_cycle_request,
            prefix=prefix,
        )
        assignments = tuple(
            row
            for row in self._store.event_rows(kind=COGNITIVE_EXPERIMENT_ASSIGNED)
            if row["event_digest"]
            == sidecar.get("assignment_event", {}).get("event_digest")
        )
        admissions = tuple(
            row
            for row in self._store.event_rows(kind="ATTEMPT_ADMITTED")
            if row["event_id"] == sidecar.get("admission_event_id")
        )
        if (
            len(assignments) != 1
            or len(admissions) != 1
            or canonical_json_bytes(supplied_request_body)
            != canonical_json_bytes(request_body)
            or canonical_json_bytes(canonical_cycle_plan.canonical_body())
            != canonical_json_bytes(sidecar.get("canonical_plan_body"))
            or canonical_cycle_plan.digest != sidecar.get("canonical_plan_digest")
            or canonical_json_bytes(cognitive_experiment.assignment_body)
            != canonical_json_bytes(assignments[0]["payload"].get("assignment_body"))
            or canonical_json_bytes(cognitive_experiment.experiment_body)
            != canonical_json_bytes(assignments[0]["payload"].get("experiment_body"))
            or canonical_json_bytes(cognitive_experiment.h5_request_body)
            != canonical_json_bytes(assignments[0]["payload"].get("h5_request_body"))
            or canonical_json_bytes(cognitive_experiment.h5_selection_plan_body)
            != canonical_json_bytes(
                assignments[0]["payload"].get("h5_selection_plan_body")
            )
            or cognitive_experiment.decision_prefix_digest
            != assignments[0]["payload"].get("decision_prefix_digest")
            or cognitive_experiment.decision_cutoff_seq
            != assignments[0]["payload"].get("decision_cutoff_seq")
            or cognitive_experiment.decision_head_event_digest
            != assignments[0]["payload"].get("decision_head_event_digest")
            or cognitive_experiment.decision_prefix_digest != prefix.digest
            or cognitive_experiment.decision_cutoff_seq != prefix.cutoff_seq
            or cognitive_experiment.decision_head_event_digest
            != prefix.head_event_digest
        ):
            raise IntegrityError(
                "canonical selection retry does not match its committed sidecar"
            )
        reservation_ids = tuple(
            f"{request.permit_id}:{account_id}"
            for account_id in self._store.budget_ancestry(request.account_id)
        )
        constraints = {
            "account_id": request.account_id,
            "conflict_keys": request.conflict_keys,
            "fingerprint": request.fingerprint,
            "requested_budget": request.requested_budget,
            "context_packet": request.context_packet.canonical_body(),
        }
        permit = AttemptPermit(
            permit_id=request.permit_id,
            lease=request.lease,
            policy_digest=request.policy_digest,
            reservation_ids=reservation_ids,
            effect_class=request.effect_class,
            expires_at_ns=request.expires_at_ns,
            constraints=constraints,
        )
        if permit.digest != admissions[0]["payload"].get(
            "permit_digest"
        ) or canonical_json_bytes(permit.canonical_body()) != canonical_json_bytes(
            admissions[0]["payload"].get("permit")
        ):
            raise IntegrityError("canonical selection retry permit was rebound")
        return permit

    def admit_canonical_cognitive_continuation_v2(
        self,
        request: AdmissionRequest,
        *,
        cognitive_experiment: CognitiveExperimentBindingV1,
        canonical_cycle_request: CanonicalCognitiveCycleRequestV1,
        canonical_cycle_plan: CanonicalCognitiveCyclePlanV1,
        occurred_at_ns: int,
    ) -> AttemptPermit:
        """Admit one exact distinct experiment after store-owned HELD_UNKNOWN.

        This versioned default-off entrypoint does not reinterpret the v1
        EXPERIMENT sidecar.  It reconstructs resolver facts from the current
        receipt prefix, reruns the recommendation-only cycle, and atomically
        binds only its ``CONTINUE_DISTINCT_EXPERIMENT`` assignment.
        """

        if request.context_packet is None:
            raise ValueError(
                "canonical cognitive continuation requires a ContextPacket"
            )
        if type(cognitive_experiment) is not CognitiveExperimentBindingV1:
            raise TypeError("cognitive_experiment must be CognitiveExperimentBindingV1")
        if type(canonical_cycle_request) is not CanonicalCognitiveCycleRequestV1:
            raise TypeError(
                "canonical_cycle_request must be CanonicalCognitiveCycleRequestV1"
            )
        if type(canonical_cycle_plan) is not CanonicalCognitiveCyclePlanV1:
            raise TypeError(
                "canonical_cycle_plan must be CanonicalCognitiveCyclePlanV1"
            )
        if (
            canonical_cycle_plan.mode
            is not CanonicalCognitiveCycleModeV1.CONTINUE_DISTINCT_EXPERIMENT
        ):
            raise IntegrityError(
                "canonical v2 continuation accepts only "
                "CONTINUE_DISTINCT_EXPERIMENT mode"
            )
        v1_rows = tuple(
            row
            for row in self._store.event_rows(kind=COGNITIVE_CANONICAL_SELECTION_BOUND)
            if row["payload"].get("attempt_id") == request.attempt.attempt_id
        )
        if v1_rows:
            raise IntegrityError(
                "canonical continuation cannot reinterpret a v1 sidecar"
            )
        existing = tuple(
            row
            for row in self._store.event_rows(
                kind=COGNITIVE_CANONICAL_CONTINUATION_BOUND_V2
            )
            if row["payload"].get("attempt_id") == request.attempt.attempt_id
        )
        if len(existing) > 1:
            raise IntegrityError("canonical continuation retry is ambiguous")
        if existing:
            return self._resolve_canonical_continuation_retry_v2(
                request=request,
                cognitive_experiment=cognitive_experiment,
                canonical_cycle_request=canonical_cycle_request,
                canonical_cycle_plan=canonical_cycle_plan,
                sidecar_row=existing[0],
                occurred_at_ns=occurred_at_ns,
            )
        state = self._store.state()
        resolver = self._store.receipt_field_resolver(cutoff_seq=state.head_seq)
        prefix = resolver.verify_complete_through(state.head_seq)
        facts = reconstruct_resolved_cognitive_facts_v1(
            store=self._store,
            resolver=resolver,
            prefix=prefix,
            scope_digest=request.attempt.scope.digest,
        )
        replay_request = CanonicalCognitiveCycleRequestV1(
            h5_request=canonical_cycle_request.h5_request,
            initial_masses=canonical_cycle_request.initial_masses,
            resolved_facts=facts,
            cost_estimates=canonical_cycle_request.cost_estimates,
            remaining_cost_units=canonical_cycle_request.remaining_cost_units,
        )
        supplied_request_body = canonical_cycle_request_binding_body_v1(
            request=canonical_cycle_request,
            prefix=prefix,
        )
        replay_request_body = canonical_cycle_request_binding_body_v1(
            request=replay_request,
            prefix=prefix,
        )
        if canonical_json_bytes(supplied_request_body) != canonical_json_bytes(
            replay_request_body
        ):
            raise IntegrityError(
                "canonical continuation request omitted or spliced resolver facts"
            )
        replay_plan = plan_canonical_cognitive_cycle_v1(replay_request)
        if (
            replay_plan.mode
            is not CanonicalCognitiveCycleModeV1.CONTINUE_DISTINCT_EXPERIMENT
            or replay_plan != canonical_cycle_plan
            or replay_plan.next_assignment is None
        ):
            raise IntegrityError(
                "canonical continuation does not replay to one distinct experiment"
            )
        h5_plan = HypothesisSelector.recommend(replay_request.h5_request)
        selected = next(
            (
                item
                for item in replay_request.h5_request.candidates
                if item.digest == replay_plan.next_assignment.experiment_digest
            ),
            None,
        )
        if (
            selected is None
            or cognitive_experiment.decision_prefix_digest != prefix.digest
            or cognitive_experiment.decision_cutoff_seq != prefix.cutoff_seq
            or cognitive_experiment.decision_head_event_digest
            != prefix.head_event_digest
            or canonical_json_bytes(cognitive_experiment.assignment_body)
            != canonical_json_bytes(replay_plan.next_assignment.canonical_body())
            or canonical_json_bytes(cognitive_experiment.experiment_body)
            != canonical_json_bytes(selected.canonical_body())
            or canonical_json_bytes(cognitive_experiment.h5_request_body)
            != canonical_json_bytes(replay_request.h5_request.canonical_body())
            or canonical_json_bytes(cognitive_experiment.h5_selection_plan_body)
            != canonical_json_bytes(h5_plan.canonical_body())
        ):
            raise IntegrityError(
                "runtime assignment is not the exact canonical continuation"
            )
        return self._admit(
            request,
            occurred_at_ns=occurred_at_ns,
            evaluation_binding=None,
            cognitive_experiment=cognitive_experiment,
            canonical_continuation=(replay_request, replay_plan, prefix),
        )

    def _resolve_canonical_continuation_retry_v2(
        self,
        *,
        request: AdmissionRequest,
        cognitive_experiment: CognitiveExperimentBindingV1,
        canonical_cycle_request: CanonicalCognitiveCycleRequestV1,
        canonical_cycle_plan: CanonicalCognitiveCyclePlanV1,
        sidecar_row: Mapping[str, Any],
        occurred_at_ns: int,
    ) -> AttemptPermit:
        """Resolve an exact committed v2 companion at its original cutoff."""

        if request.expires_at_ns <= occurred_at_ns:
            raise ValueError("permit expiry must be after admission")
        state = self._store.state()
        self._guard.authorize(CommandClass.DISPATCH, state)
        scope = request.attempt.scope
        if (
            scope.run_id != state.run_id
            or scope.run_fence_epoch != state.run_fence_epoch
            or scope.execution_generation != state.execution_generation
        ):
            raise IntegrityError("attempt scope is not the current execution scope")
        sidecar = sidecar_row["payload"]
        try:
            validate_canonical_continuation_sidecar_shape_v2(sidecar)
        except (TypeError, ValueError) as exc:
            raise IntegrityError(
                "canonical continuation retry sidecar is false"
            ) from exc
        request_body = sidecar.get("canonical_request_body")
        prefix_claim = (
            request_body.get("pre_admission_prefix")
            if isinstance(request_body, Mapping)
            else None
        )
        if not isinstance(prefix_claim, Mapping):
            raise IntegrityError("canonical continuation retry prefix is absent")
        cutoff = prefix_claim.get("cutoff_seq")
        resolver = self._store.receipt_field_resolver(cutoff_seq=cutoff)
        prefix = resolver.verify_complete_through(cutoff)
        supplied_request_body = canonical_cycle_request_binding_body_v1(
            request=canonical_cycle_request,
            prefix=prefix,
        )
        assignments = tuple(
            row
            for row in self._store.event_rows(kind=COGNITIVE_EXPERIMENT_ASSIGNED)
            if row["event_digest"]
            == sidecar.get("assignment_event", {}).get("event_digest")
        )
        admissions = tuple(
            row
            for row in self._store.event_rows(kind="ATTEMPT_ADMITTED")
            if row["event_id"] == sidecar.get("admission_event_id")
        )
        if (
            len(assignments) != 1
            or len(admissions) != 1
            or canonical_json_bytes(supplied_request_body)
            != canonical_json_bytes(request_body)
            or canonical_json_bytes(canonical_cycle_plan.canonical_body())
            != canonical_json_bytes(sidecar.get("canonical_plan_body"))
            or canonical_cycle_plan.digest != sidecar.get("canonical_plan_digest")
            or canonical_json_bytes(cognitive_experiment.assignment_body)
            != canonical_json_bytes(assignments[0]["payload"].get("assignment_body"))
            or canonical_json_bytes(cognitive_experiment.experiment_body)
            != canonical_json_bytes(assignments[0]["payload"].get("experiment_body"))
            or canonical_json_bytes(cognitive_experiment.h5_request_body)
            != canonical_json_bytes(assignments[0]["payload"].get("h5_request_body"))
            or canonical_json_bytes(cognitive_experiment.h5_selection_plan_body)
            != canonical_json_bytes(
                assignments[0]["payload"].get("h5_selection_plan_body")
            )
            or cognitive_experiment.decision_prefix_digest
            != assignments[0]["payload"].get("decision_prefix_digest")
            or cognitive_experiment.decision_cutoff_seq
            != assignments[0]["payload"].get("decision_cutoff_seq")
            or cognitive_experiment.decision_head_event_digest
            != assignments[0]["payload"].get("decision_head_event_digest")
            or cognitive_experiment.decision_prefix_digest != prefix.digest
            or cognitive_experiment.decision_cutoff_seq != prefix.cutoff_seq
            or cognitive_experiment.decision_head_event_digest
            != prefix.head_event_digest
        ):
            raise IntegrityError(
                "canonical continuation retry does not match its committed sidecar"
            )
        reservation_ids = tuple(
            f"{request.permit_id}:{account_id}"
            for account_id in self._store.budget_ancestry(request.account_id)
        )
        constraints = {
            "account_id": request.account_id,
            "conflict_keys": request.conflict_keys,
            "fingerprint": request.fingerprint,
            "requested_budget": request.requested_budget,
            "context_packet": request.context_packet.canonical_body(),
        }
        permit = AttemptPermit(
            permit_id=request.permit_id,
            lease=request.lease,
            policy_digest=request.policy_digest,
            reservation_ids=reservation_ids,
            effect_class=request.effect_class,
            expires_at_ns=request.expires_at_ns,
            constraints=constraints,
        )
        if permit.digest != admissions[0]["payload"].get(
            "permit_digest"
        ) or canonical_json_bytes(permit.canonical_body()) != canonical_json_bytes(
            admissions[0]["payload"].get("permit")
        ):
            raise IntegrityError("canonical continuation retry permit was rebound")
        return permit

    def admit_executable_cognitive_context(
        self,
        request: AdmissionRequest,
        *,
        cognitive_experiment: CognitiveExperimentBindingV1,
        executable_experiment: ExecutableExperimentBindingV1,
        occurred_at_ns: int,
    ) -> AttemptPermit:
        """Admit one experiment only after its exact executable spec resolves.

        This is a separate default-off entrypoint.  Ordinary C6 and the older
        runtime-context assignment remain byte-for-byte unchanged.
        """

        if request.context_packet is None:
            raise ValueError("executable cognitive admission requires a ContextPacket")
        if type(cognitive_experiment) is not CognitiveExperimentBindingV1:
            raise TypeError("cognitive_experiment must be CognitiveExperimentBindingV1")
        if type(executable_experiment) is not ExecutableExperimentBindingV1:
            raise TypeError(
                "executable_experiment must be ExecutableExperimentBindingV1"
            )
        if self._cas is None:
            raise RuntimeError(
                "executable cognitive admission requires the run-owned receipt CAS"
            )
        executable_experiment.resolve(self._cas)
        executable_experiment.spec.validate_against_body(
            cognitive_experiment.experiment_body
        )
        return self._admit(
            request,
            occurred_at_ns=occurred_at_ns,
            evaluation_binding=None,
            cognitive_experiment=cognitive_experiment,
            executable_experiment=executable_experiment,
        )

    def admit_cognitive_reproduction(
        self,
        request: AdmissionRequest,
        *,
        cognitive_experiment: CognitiveExperimentBindingV1,
        executable_experiment: ExecutableExperimentBindingV1,
        source_observation_event_digest: str,
        required_reproducer_profile_digest: str,
        occurred_at_ns: int,
    ) -> AttemptPermit:
        """Atomically preregister one fresh attempt as a source reproduction.

        The source is resolved from canonical history here and again by the store
        semantic guard.  No caller supplies an outcome, overlay, retry decision, or
        learning label.
        """

        if request.context_packet is None:
            raise ValueError("cognitive reproduction requires a ContextPacket")
        if type(cognitive_experiment) is not CognitiveExperimentBindingV1:
            raise TypeError("cognitive_experiment must be CognitiveExperimentBindingV1")
        if type(executable_experiment) is not ExecutableExperimentBindingV1:
            raise TypeError(
                "executable_experiment must be ExecutableExperimentBindingV1"
            )
        if (
            type(source_observation_event_digest) is not str
            or len(source_observation_event_digest) != 64
            or any(
                character not in "0123456789abcdef"
                for character in source_observation_event_digest
            )
        ):
            raise ValueError("source_observation_event_digest must be a sha256 digest")
        if (
            type(required_reproducer_profile_digest) is not str
            or len(required_reproducer_profile_digest) != 64
            or any(
                character not in "0123456789abcdef"
                for character in required_reproducer_profile_digest
            )
        ):
            raise ValueError(
                "required_reproducer_profile_digest must be a sha256 digest"
            )
        if self._cas is None:
            raise RuntimeError(
                "cognitive reproduction requires the run-owned receipt CAS"
            )
        executable_experiment.resolve(self._cas)
        executable_experiment.spec.validate_against_body(
            cognitive_experiment.experiment_body
        )
        source_rows = tuple(
            row
            for row in self._store.event_rows(kind="COGNITIVE_EXECUTION_OBSERVED")
            if row["event_digest"] == source_observation_event_digest
        )
        if len(source_rows) != 1:
            raise IntegrityError("cognitive reproduction source observation is absent")
        source_observation = source_rows[0]
        source_assignment_digest = source_observation["payload"].get(
            "assignment_event_digest"
        )
        source_assignments = tuple(
            row
            for row in self._store.event_rows(kind=COGNITIVE_EXPERIMENT_ASSIGNED)
            if row["event_digest"] == source_assignment_digest
        )
        if len(source_assignments) != 1:
            raise IntegrityError("cognitive reproduction source assignment is absent")
        source_assignment = source_assignments[0]
        source = {
            "required_reproducer_profile_digest": required_reproducer_profile_digest,
            "source_assignment_event_digest": source_assignment["event_digest"],
            "source_assignment_event_receipt_digest": (
                self._store.resolve_receipt_for_event(
                    source_assignment["event_digest"]
                ).digest
            ),
            "source_assignment_payload": source_assignment["payload"],
            "source_observation_event_digest": source_observation["event_digest"],
            "source_observation_event_receipt_digest": (
                self._store.resolve_receipt_for_event(
                    source_observation["event_digest"]
                ).digest
            ),
            "source_observation_payload": source_observation["payload"],
        }
        return self._admit(
            request,
            occurred_at_ns=occurred_at_ns,
            evaluation_binding=None,
            cognitive_experiment=cognitive_experiment,
            executable_experiment=executable_experiment,
            reproduction_source=source,
        )

    def admit_shadow(
        self,
        request: AdmissionRequest,
        *,
        evaluation_binding: EvaluationExecutionBindingV1,
        occurred_at_ns: int,
    ) -> AttemptPermit:
        if request.context_packet is not None:
            raise ValueError(
                "production context binding cannot be mixed with shadow admission"
            )
        if type(evaluation_binding) is not EvaluationExecutionBindingV1:
            raise TypeError(
                "evaluation_binding must be EvaluationExecutionBindingV1; "
                "v2 role bindings require admit_shadow_v2"
            )
        if (
            self._store.run_anchor()["manifest_digest"]
            != evaluation_binding.run_manifest_digest
        ):
            raise IntegrityError(
                "evaluation binding does not match the immutable run manifest"
            )
        return self._admit(
            request,
            occurred_at_ns=occurred_at_ns,
            evaluation_binding=evaluation_binding,
        )

    def admit_shadow_v2(
        self,
        request: AdmissionRequest,
        *,
        runtime_binding: RuntimeEvaluationBindingV2,
        occurred_at_ns: int,
        cognitive_experiment: CognitiveExperimentBindingV1 | None = None,
    ) -> AttemptPermit:
        """Admit one preallocated v2 role with an atomic evaluation sidecar.

        Ordinary ``admit`` and v1 ``admit_shadow`` remain unchanged.  The role
        binding is accepted only after the current run/fence/generation scope and
        exact permit identity are recomputed and validated.
        """

        if request.context_packet is not None:
            raise ValueError(
                "production context binding cannot be mixed with shadow v2 admission"
            )

        if type(runtime_binding) is not RuntimeEvaluationBindingV2:
            raise TypeError("runtime_binding must be RuntimeEvaluationBindingV2")
        if (
            cognitive_experiment is not None
            and type(cognitive_experiment) is not CognitiveExperimentBindingV1
        ):
            raise TypeError(
                "cognitive_experiment must be CognitiveExperimentBindingV1 or None"
            )
        if cognitive_experiment is not None and runtime_binding.role != "executor":
            raise IntegrityError(
                "cognitive experiment admission requires the executor role"
            )
        if runtime_binding.split not in RUNTIME_EVALUATION_EXECUTABLE_SPLITS_V2:
            raise IntegrityError(
                "sealed_final requires separate evaluator opening authority"
            )
        if request.expires_at_ns <= occurred_at_ns:
            raise ValueError("permit expiry must be after admission")
        state = self._store.state()
        self._guard.authorize(CommandClass.DISPATCH, state)
        scope = request.attempt.scope
        if (
            scope.run_id != state.run_id
            or scope.run_fence_epoch != state.run_fence_epoch
            or scope.execution_generation != state.execution_generation
        ):
            raise IntegrityError("attempt scope is not the current execution scope")
        if (
            self._store.run_anchor()["manifest_digest"]
            != runtime_binding.run_manifest_digest
            or runtime_binding.run_id != state.run_id
            or runtime_binding.run_fence_epoch != state.run_fence_epoch
            or runtime_binding.execution_generation != state.execution_generation
        ):
            raise IntegrityError(
                "v2 role binding does not match the immutable run/fence/generation"
            )
        if (
            runtime_binding.scope_digest != scope.digest
            or runtime_binding.branch_id != request.attempt.branch_id
            or runtime_binding.attempt_id != request.attempt.attempt_id
            or runtime_binding.launch_ordinal != request.attempt.launch_ordinal
            or runtime_binding.attempt_identity_digest != request.attempt.digest
            or runtime_binding.permit_id != request.permit_id
            or runtime_binding.policy_digest != request.policy_digest
        ):
            raise IntegrityError(
                "v2 role binding does not recompute to the requested attempt/permit"
            )
        child_budget = dict(runtime_binding.role_budget)
        if dict(request.requested_budget) != child_budget:
            raise IntegrityError(
                "v2 requested budget must equal the frozen child role ceiling"
            )
        if request.requested_budget.get("attempts") != 1:
            raise IntegrityError("v2 role admission consumes exactly one attempt")
        prior_role_rows = [
            row
            for row in self._store.event_rows(kind="C6_EVAL_V2_ATTEMPT_BOUND")
            if row["payload"].get("assignment_binding_digest")
            == runtime_binding.assignment_binding_digest
        ]
        same_slot_rows = [
            row
            for row in prior_role_rows
            if row["payload"].get("slot_id") == runtime_binding.slot_id
        ]
        if same_slot_rows and not (
            len(same_slot_rows) == 1
            and same_slot_rows[0]["payload"].get("attempt_id")
            == runtime_binding.attempt_id
            and same_slot_rows[0]["payload"].get("permit_id")
            == runtime_binding.permit_id
            and same_slot_rows[0]["payload"].get("attempt_role_binding_digest")
            == runtime_binding.attempt_role_binding_digest
        ):
            raise IntegrityError(
                "v2 role slot was already admitted for this assignment"
            )
        prior_first = [
            row
            for row in prior_role_rows
            if (
                type(row["payload"].get("root_budget_reservation")) is dict
                and row["payload"]["root_budget_reservation"].get("first_reservation")
                is True
            )
        ]
        first_reservation = (
            bool(
                same_slot_rows[0]["payload"]
                .get("root_budget_reservation", {})
                .get("first_reservation")
            )
            if same_slot_rows
            else not prior_first
        )
        if prior_first:
            reserved_root = prior_first[0]["payload"]["root_budget_reservation"].get(
                "root_budget_digest"
            )
            if reserved_root != runtime_binding.root_budget_digest:
                raise IntegrityError(
                    "v2 role cannot mint a second divergent root budget"
                )
        if len(prior_first) > 1:
            raise IntegrityError("v2 assignment has multiple root budget reservations")
        self._store.validate_runtime_evaluation_v2_prerequisite_lineage(runtime_binding)
        reservation_ids = tuple(
            f"{request.permit_id}:{account_id}"
            for account_id in self._store.budget_ancestry(request.account_id)
        )
        if not reservation_ids:
            raise ValueError("budget account does not exist")
        permit = AttemptPermit(
            permit_id=request.permit_id,
            lease=request.lease,
            policy_digest=request.policy_digest,
            reservation_ids=reservation_ids,
            effect_class=request.effect_class,
            expires_at_ns=request.expires_at_ns,
            constraints={
                "account_id": request.account_id,
                "conflict_keys": request.conflict_keys,
                "fingerprint": request.fingerprint,
                "requested_budget": request.requested_budget,
            },
        )
        if runtime_binding.permit_digest != permit.digest:
            raise IntegrityError(
                "v2 role binding permit_digest does not match the recomputed permit"
            )
        if (
            runtime_binding.role == "observer"
            and request.effect_class is not EffectClass.PURE
        ):
            raise IntegrityError("v2 observer admission requires a pure effect class")
        payload = {
            "account_id": request.account_id,
            "attempt_digest": request.attempt.digest,
            "attempt_id": request.attempt.attempt_id,
            "branch_id": request.attempt.branch_id,
            "conflict_keys": request.conflict_keys,
            "effect_class": request.effect_class.value,
            "fingerprint": request.fingerprint,
            "lease_digest": request.lease.digest,
            "lease_epoch": request.lease.lease_epoch,
            "lease_id": request.lease.lease_id,
            "launch_ordinal": request.attempt.launch_ordinal,
            "expires_at_ns": request.expires_at_ns,
            "permit": permit.canonical_body(),
            "permit_digest": permit.digest,
            "permit_id": request.permit_id,
            "policy_digest": request.policy_digest,
            "reservation_ids": reservation_ids,
            "requested_budget": request.requested_budget,
            "scope_digest": request.attempt.scope.digest,
            "worker_generation": request.lease.worker_generation,
        }
        sidecar = {
            "assignment_binding_digest": runtime_binding.assignment_binding_digest,
            "attempt_role_binding_digest": (
                runtime_binding.attempt_role_binding_digest
            ),
            "attempt_digest": request.attempt.digest,
            "attempt_id": request.attempt.attempt_id,
            "base_event_id": f"event:attempt:admit:{request.attempt.attempt_id}",
            "base_payload_digest": canonical_digest(payload),
            "permit_digest": permit.digest,
            "permit_id": request.permit_id,
            "phase": "attempt",
            "role": runtime_binding.role,
            "runtime_binding": runtime_binding.canonical_body(),
            "runtime_binding_digest": runtime_binding.digest,
            "root_budget_reservation": {
                "assignment_binding_digest": (
                    runtime_binding.assignment_binding_digest
                ),
                "first_reservation": first_reservation,
                "root_budget_digest": runtime_binding.root_budget_digest,
            },
            "schema_id": "muteki.c6-eval-binding-sidecar.v2",
            "scope_digest": request.attempt.scope.digest,
            "slot_id": runtime_binding.slot_id,
        }
        events = [
            CommandEvent(
                f"event:attempt:admit:{request.attempt.attempt_id}",
                "ATTEMPT_ADMITTED",
                "search-admission",
                occurred_at_ns,
                payload,
            ),
            CommandEvent(
                f"event:C6_EVAL_V2_ATTEMPT_BOUND:{request.attempt.attempt_id}",
                "C6_EVAL_V2_ATTEMPT_BOUND",
                "c6-evaluation-binding-v2-authority",
                occurred_at_ns,
                sidecar,
            ),
        ]
        mutations = [
            ProjectionMutation("attempt_admit", payload),
            ProjectionMutation("c6_eval_v2_attempt_bind_guard", sidecar),
        ]
        command_payload: Mapping[str, Any] = payload
        cognitive_payload: Mapping[str, Any] | None = None
        if cognitive_experiment is not None:
            cognitive_payload = cognitive_assignment_payload(
                binding=cognitive_experiment,
                admission_payload=payload,
                evaluation_sidecar=sidecar,
            )
            events.append(
                CommandEvent(
                    (
                        f"event:{COGNITIVE_EXPERIMENT_ASSIGNED}:"
                        f"{request.attempt.attempt_id}"
                    ),
                    COGNITIVE_EXPERIMENT_ASSIGNED,
                    COGNITIVE_BINDING_ACTOR,
                    occurred_at_ns,
                    cognitive_payload,
                )
            )
            mutations.append(
                ProjectionMutation(
                    "cognitive_experiment_assign_guard", cognitive_payload
                )
            )
            command_payload = {
                "admission": payload,
                "cognitive_assignment": cognitive_payload,
                "evaluation_attempt_binding": sidecar,
            }
        attempt_input = runtime_binding.attempt_role_body.get("input_spec")
        required_packet_event = None
        if (
            isinstance(attempt_input, Mapping)
            and attempt_input.get("kind") == "candidate_context_packet"
        ):
            required_packet_event = (
                "C6_PACKET_COMPILED",
                {
                    "assignment_binding_digest": (
                        runtime_binding.assignment_binding_digest
                    ),
                    "compiler_receipt_digest": attempt_input.get(
                        "compiler_receipt_digest"
                    ),
                    "context_packet_digest": attempt_input.get("context_packet_digest"),
                    "context_packet_manifest_digest": attempt_input.get(
                        "context_packet_manifest_digest"
                    ),
                },
            )
        self._store.commit_command(
            command_id=f"attempt:admit:{request.attempt.attempt_id}",
            idempotency_key=f"attempt:admit:{request.permit_id}",
            command_payload=command_payload,
            events=events,
            projection_mutations=mutations,
            authority_capability=(
                self._store._evaluation_v2_cognitive_commit_capability
                if cognitive_experiment is not None
                else self._store._evaluation_v2_commit_capability
            ),
            required_prior_event=required_packet_event,
            committed_at_ns=occurred_at_ns,
        )
        rows = [
            row
            for row in self._store.event_rows(kind="C6_EVAL_V2_ATTEMPT_BOUND")
            if row["payload"].get("attempt_id") == request.attempt.attempt_id
        ]
        if (
            len(rows) != 1
            or rows[0]["payload"].get("runtime_binding_digest")
            != runtime_binding.digest
        ):
            raise IntegrityError(
                "v2 shadow admission retry did not resolve its exact sidecar"
            )
        cognitive_rows = [
            row
            for row in self._store.event_rows(kind=COGNITIVE_EXPERIMENT_ASSIGNED)
            if row["payload"].get("attempt_id") == request.attempt.attempt_id
        ]
        if cognitive_payload is None:
            if cognitive_rows:
                raise IntegrityError(
                    "ordinary v2 retry encountered a cognitive assignment"
                )
        elif len(cognitive_rows) != 1 or canonical_digest(
            cognitive_rows[0]["payload"]
        ) != canonical_digest(cognitive_payload):
            raise IntegrityError(
                "cognitive admission retry did not resolve its exact assignment"
            )
        return permit

    def _admit(
        self,
        request: AdmissionRequest,
        *,
        occurred_at_ns: int,
        evaluation_binding: EvaluationExecutionBindingV1 | None,
        cognitive_experiment: CognitiveExperimentBindingV1 | None = None,
        executable_experiment: ExecutableExperimentBindingV1 | None = None,
        reproduction_source: Mapping[str, Any] | None = None,
        canonical_selection: tuple[
            CanonicalCognitiveCycleRequestV1,
            CanonicalCognitiveCyclePlanV1,
            VerifiedReceiptPrefixV1,
        ]
        | None = None,
        canonical_continuation: tuple[
            CanonicalCognitiveCycleRequestV1,
            CanonicalCognitiveCyclePlanV1,
            VerifiedReceiptPrefixV1,
        ]
        | None = None,
    ) -> AttemptPermit:
        if executable_experiment is not None and cognitive_experiment is None:
            raise ValueError(
                "executable experiment requires a cognitive experiment binding"
            )
        if cognitive_experiment is not None and evaluation_binding is not None:
            raise ValueError(
                "runtime-context cognitive admission cannot mix evaluation bindings"
            )
        if reproduction_source is not None and executable_experiment is None:
            raise ValueError("cognitive reproduction requires an executable experiment")
        if canonical_selection is not None and (
            cognitive_experiment is None
            or executable_experiment is not None
            or reproduction_source is not None
        ):
            raise ValueError(
                "canonical selection requires one ordinary cognitive assignment"
            )
        if canonical_continuation is not None and (
            cognitive_experiment is None
            or executable_experiment is not None
            or reproduction_source is not None
            or canonical_selection is not None
        ):
            raise ValueError(
                "canonical continuation requires one ordinary cognitive assignment"
            )
        if request.expires_at_ns <= occurred_at_ns:
            raise ValueError("permit expiry must be after admission")
        state = self._store.state()
        self._guard.authorize(CommandClass.DISPATCH, state)
        scope = request.attempt.scope
        if (
            scope.run_id != state.run_id
            or scope.run_fence_epoch != state.run_fence_epoch
            or scope.execution_generation != state.execution_generation
        ):
            raise IntegrityError("attempt scope is not the current execution scope")
        reservation_ids = tuple(
            f"{request.permit_id}:{account_id}"
            for account_id in self._store.budget_ancestry(request.account_id)
        )
        if not reservation_ids:
            raise ValueError("budget account does not exist")
        if request.context_packet is not None:
            context_binding = request.context_packet
            try:
                compilation_receipt = self._store.resolve_receipt(
                    context_binding.compilation_event_receipt_digest
                )
                matching_context_events = [
                    row
                    for row in self._store.event_rows(kind="CONTEXT_PACKET_COMPILED")
                    if row["payload"].get("packet_digest")
                    == context_binding.packet_digest
                    and self._store.receipt_digest_for_event(row["event_digest"])
                    == context_binding.compilation_event_receipt_digest
                ]
                unadmitted_context_events = [
                    row
                    for row in self._store.event_rows(kind="CONTEXT_PACKET_UNADMITTED")
                    if row["payload"].get("target_attempt_id")
                    == context_binding.target_attempt_id
                ]
            except (IntegrityError, KeyError, TypeError, ValueError) as exc:
                raise IntegrityError(
                    "context packet compilation receipt did not resolve"
                ) from exc
            if (
                len(matching_context_events) != 1
                or unadmitted_context_events
                or compilation_receipt.command_id
                != f"context:packet:{context_binding.decision_id}"
                or matching_context_events[0]["payload"].get("compiler_receipt_digest")
                != context_binding.compiler_receipt_digest
                or matching_context_events[0]["payload"].get("decision_receipt_digest")
                != context_binding.decision_receipt_digest
                or matching_context_events[0]["payload"].get("feature_state_digest")
                != context_binding.feature_state_digest
                or matching_context_events[0]["payload"].get("manifest_digest")
                != context_binding.manifest_digest
                or matching_context_events[0]["payload"].get("target_attempt_id")
                != request.attempt.attempt_id
            ):
                raise IntegrityError(
                    "context packet compilation lineage is closed or diverged before admission"
                )
        constraints = {
            "account_id": request.account_id,
            "conflict_keys": request.conflict_keys,
            "fingerprint": request.fingerprint,
            "requested_budget": request.requested_budget,
        }
        if request.context_packet is not None:
            constraints["context_packet"] = request.context_packet.canonical_body()
        permit = AttemptPermit(
            permit_id=request.permit_id,
            lease=request.lease,
            policy_digest=request.policy_digest,
            reservation_ids=reservation_ids,
            effect_class=request.effect_class,
            expires_at_ns=request.expires_at_ns,
            constraints=constraints,
        )
        payload = {
            "account_id": request.account_id,
            "attempt_digest": request.attempt.digest,
            "attempt_id": request.attempt.attempt_id,
            "branch_id": request.attempt.branch_id,
            "conflict_keys": request.conflict_keys,
            "effect_class": request.effect_class.value,
            "fingerprint": request.fingerprint,
            "lease_digest": request.lease.digest,
            "lease_epoch": request.lease.lease_epoch,
            "lease_id": request.lease.lease_id,
            "launch_ordinal": request.attempt.launch_ordinal,
            "expires_at_ns": request.expires_at_ns,
            "permit": permit.canonical_body(),
            "permit_digest": permit.digest,
            "permit_id": request.permit_id,
            "policy_digest": request.policy_digest,
            "reservation_ids": reservation_ids,
            "requested_budget": request.requested_budget,
            "scope_digest": request.attempt.scope.digest,
            "worker_generation": request.lease.worker_generation,
        }
        if request.context_packet is not None:
            payload["context_packet"] = request.context_packet.canonical_body()
        events = [
            CommandEvent(
                f"event:attempt:admit:{request.attempt.attempt_id}",
                "ATTEMPT_ADMITTED",
                "search-admission",
                occurred_at_ns,
                payload,
            )
        ]
        mutations = [ProjectionMutation("attempt_admit", payload)]
        command_payload: Mapping[str, Any] = payload
        if evaluation_binding is not None:
            sidecar = {
                "attempt_digest": request.attempt.digest,
                "attempt_id": request.attempt.attempt_id,
                "base_event_id": events[0].event_id,
                "base_payload_digest": canonical_digest(payload),
                "evaluation_binding": evaluation_binding.canonical_body(),
                "evaluation_binding_digest": evaluation_binding.digest,
                "permit_digest": permit.digest,
                "permit_id": request.permit_id,
                "phase": "attempt",
                "schema_id": "muteki.c6-eval-binding-sidecar.v1",
                "scope_digest": request.attempt.scope.digest,
            }
            events.append(
                CommandEvent(
                    f"event:C6_EVAL_ATTEMPT_BOUND:{request.attempt.attempt_id}",
                    "C6_EVAL_ATTEMPT_BOUND",
                    "c6-evaluation-binding-authority",
                    occurred_at_ns,
                    sidecar,
                )
            )
            mutations.append(ProjectionMutation("c6_eval_attempt_bind_guard", sidecar))
        cognitive_payload: Mapping[str, Any] | None = None
        canonical_selection_payload: Mapping[str, Any] | None = None
        canonical_continuation_payload: Mapping[str, Any] | None = None
        if cognitive_experiment is not None:
            if reproduction_source is not None:
                cognitive_payload = cognitive_runtime_reproduction_assignment_payload(
                    binding=cognitive_experiment,
                    admission_payload=payload,
                    executable_experiment=executable_experiment,
                    **dict(reproduction_source),
                )
            elif executable_experiment is not None:
                cognitive_payload = (
                    cognitive_runtime_context_executable_assignment_payload(
                        binding=cognitive_experiment,
                        admission_payload=payload,
                        executable_experiment=executable_experiment,
                    )
                )
            else:
                cognitive_payload = cognitive_runtime_context_assignment_payload(
                    binding=cognitive_experiment,
                    admission_payload=payload,
                )
            events.append(
                CommandEvent(
                    (
                        f"event:{COGNITIVE_EXPERIMENT_ASSIGNED}:"
                        f"{request.attempt.attempt_id}"
                    ),
                    COGNITIVE_EXPERIMENT_ASSIGNED,
                    COGNITIVE_BINDING_ACTOR,
                    occurred_at_ns,
                    cognitive_payload,
                )
            )
            mutations.append(
                ProjectionMutation(
                    "cognitive_experiment_assign_guard", cognitive_payload
                )
            )
            command_payload = {
                "admission": payload,
                "cognitive_assignment": cognitive_payload,
            }
            if canonical_selection is not None:
                cycle_request, cycle_plan, decision_prefix = canonical_selection
                command_id = f"attempt:admit:{request.attempt.attempt_id}"
                base_envelope = EventEnvelopeV2(
                    event_id=events[0].event_id,
                    run_id=state.run_id,
                    command_id=command_id,
                    ordinal=0,
                    kind=events[0].kind,
                    actor=events[0].actor,
                    occurred_at_ns=events[0].occurred_at_ns,
                    payload=events[0].payload,
                    parent_event_digest=state.head_event_digest,
                )
                assignment_spec = events[1]
                assignment_envelope = EventEnvelopeV2(
                    event_id=assignment_spec.event_id,
                    run_id=state.run_id,
                    command_id=command_id,
                    ordinal=1,
                    kind=assignment_spec.kind,
                    actor=assignment_spec.actor,
                    occurred_at_ns=assignment_spec.occurred_at_ns,
                    payload=assignment_spec.payload,
                    parent_event_digest=base_envelope.digest,
                )
                canonical_selection_payload = canonical_selection_sidecar_payload_v1(
                    request=cycle_request,
                    plan=cycle_plan,
                    prefix=decision_prefix,
                    admission_payload=payload,
                    assignment_payload=cognitive_payload,
                    assignment_event_digest=assignment_envelope.digest,
                )
                events.append(
                    CommandEvent(
                        (
                            f"event:{COGNITIVE_CANONICAL_SELECTION_BOUND}:"
                            f"{request.attempt.attempt_id}"
                        ),
                        COGNITIVE_CANONICAL_SELECTION_BOUND,
                        COGNITIVE_CANONICAL_SELECTION_ACTOR,
                        occurred_at_ns,
                        canonical_selection_payload,
                    )
                )
                mutations.append(
                    ProjectionMutation(
                        "cognitive_canonical_selection_bind_guard",
                        canonical_selection_payload,
                    )
                )
                command_payload = {
                    "admission": payload,
                    "canonical_selection": canonical_selection_payload,
                    "cognitive_assignment": cognitive_payload,
                }
            elif canonical_continuation is not None:
                cycle_request, cycle_plan, decision_prefix = canonical_continuation
                command_id = f"attempt:admit:{request.attempt.attempt_id}"
                base_envelope = EventEnvelopeV2(
                    event_id=events[0].event_id,
                    run_id=state.run_id,
                    command_id=command_id,
                    ordinal=0,
                    kind=events[0].kind,
                    actor=events[0].actor,
                    occurred_at_ns=events[0].occurred_at_ns,
                    payload=events[0].payload,
                    parent_event_digest=state.head_event_digest,
                )
                assignment_spec = events[1]
                assignment_envelope = EventEnvelopeV2(
                    event_id=assignment_spec.event_id,
                    run_id=state.run_id,
                    command_id=command_id,
                    ordinal=1,
                    kind=assignment_spec.kind,
                    actor=assignment_spec.actor,
                    occurred_at_ns=assignment_spec.occurred_at_ns,
                    payload=assignment_spec.payload,
                    parent_event_digest=base_envelope.digest,
                )
                canonical_continuation_payload = (
                    canonical_continuation_sidecar_payload_v2(
                        request=cycle_request,
                        plan=cycle_plan,
                        prefix=decision_prefix,
                        admission_payload=payload,
                        assignment_payload=cognitive_payload,
                        assignment_event_digest=assignment_envelope.digest,
                    )
                )
                events.append(
                    CommandEvent(
                        (
                            f"event:{COGNITIVE_CANONICAL_CONTINUATION_BOUND_V2}:"
                            f"{request.attempt.attempt_id}"
                        ),
                        COGNITIVE_CANONICAL_CONTINUATION_BOUND_V2,
                        COGNITIVE_CANONICAL_CONTINUATION_ACTOR_V2,
                        occurred_at_ns,
                        canonical_continuation_payload,
                    )
                )
                mutations.append(
                    ProjectionMutation(
                        COGNITIVE_CANONICAL_CONTINUATION_MUTATION_V2,
                        canonical_continuation_payload,
                    )
                )
                command_payload = {
                    "admission": payload,
                    "canonical_continuation_v2": canonical_continuation_payload,
                    "cognitive_assignment": cognitive_payload,
                }
        self._store.commit_command(
            command_id=f"attempt:admit:{request.attempt.attempt_id}",
            idempotency_key=f"attempt:admit:{request.permit_id}",
            command_payload=command_payload,
            events=events,
            projection_mutations=mutations,
            authority_capability=(
                self._store._cognitive_canonical_continuation_v2_commit_capability
                if canonical_continuation is not None
                else (
                    self._store._cognitive_canonical_selection_commit_capability
                    if canonical_selection is not None
                    else (
                        self._store._cognitive_context_assignment_commit_capability
                        if cognitive_experiment is not None
                        else (
                            self._store._evaluation_commit_capability
                            if evaluation_binding is not None
                            else None
                        )
                    )
                )
            ),
            required_prior_event=(
                (
                    "CONTEXT_PACKET_COMPILED",
                    {
                        "compiler_receipt_digest": (
                            request.context_packet.compiler_receipt_digest
                        ),
                        "feature_state_digest": (
                            request.context_packet.feature_state_digest
                        ),
                        "manifest_digest": request.context_packet.manifest_digest,
                        "packet_digest": request.context_packet.packet_digest,
                        "target_attempt_id": (request.context_packet.target_attempt_id),
                    },
                )
                if request.context_packet is not None
                else None
            ),
            committed_at_ns=occurred_at_ns,
        )
        if evaluation_binding is not None:
            rows = [
                row
                for row in self._store.event_rows(kind="C6_EVAL_ATTEMPT_BOUND")
                if row["payload"].get("attempt_id") == request.attempt.attempt_id
            ]
            if (
                len(rows) != 1
                or rows[0]["payload"].get("evaluation_binding_digest")
                != evaluation_binding.digest
            ):
                raise IntegrityError(
                    "shadow admission retry did not resolve its exact sidecar"
                )
        if cognitive_payload is not None:
            rows = [
                row
                for row in self._store.event_rows(kind=COGNITIVE_EXPERIMENT_ASSIGNED)
                if row["payload"].get("attempt_id") == request.attempt.attempt_id
            ]
            if len(rows) != 1 or canonical_digest(
                rows[0]["payload"]
            ) != canonical_digest(cognitive_payload):
                raise IntegrityError(
                    "runtime-context cognitive retry did not resolve its exact assignment"
                )
        if canonical_selection_payload is not None:
            rows = [
                row
                for row in self._store.event_rows(
                    kind=COGNITIVE_CANONICAL_SELECTION_BOUND
                )
                if row["payload"].get("attempt_id") == request.attempt.attempt_id
            ]
            if len(rows) != 1 or canonical_digest(
                rows[0]["payload"]
            ) != canonical_digest(canonical_selection_payload):
                raise IntegrityError(
                    "canonical selection retry did not resolve its exact sidecar"
                )
        if canonical_continuation_payload is not None:
            rows = [
                row
                for row in self._store.event_rows(
                    kind=COGNITIVE_CANONICAL_CONTINUATION_BOUND_V2
                )
                if row["payload"].get("attempt_id") == request.attempt.attempt_id
            ]
            if len(rows) != 1 or canonical_digest(
                rows[0]["payload"]
            ) != canonical_digest(canonical_continuation_payload):
                raise IntegrityError(
                    "canonical continuation retry did not resolve its exact sidecar"
                )
        return permit

    def settle(
        self,
        *,
        attempt_id: str,
        usage_report: UsageReport | None = None,
        actual_usage: Mapping[str, int] | None = None,
        settlement_revision: int,
        occurred_at_ns: int,
    ) -> str:
        """Settle an exact reservation set from an explicit tagged usage report.

        ``actual_usage`` is a narrow compatibility path for callers that can assert
        complete observations. It is rejected unless it contains exactly every
        reserved axis with built-in, non-negative integer values.
        """
        attempt_id = self._attempt_id(attempt_id)
        self._positive_revision(settlement_revision, "settlement_revision")
        reserved, reservation_ids = self._reservation_contract(attempt_id)
        if (usage_report is None) == (actual_usage is None):
            raise TypeError("provide exactly one of usage_report or actual_usage")
        if actual_usage is not None:
            usage_report = UsageReport.from_observed_and_reservation(
                reserved=reserved,
                observed=actual_usage,
                complete_axes=frozenset(reserved),
            )
        if type(usage_report) is not UsageReport:
            raise TypeError("usage_report must be UsageReport")
        usage_report.validate_reservation(reserved)
        if usage_report.has_unknown:
            raise UsageNotEstimable(
                "UNKNOWN usage cannot settle; retain the reservation with "
                "hold_unknown_usage"
            )
        charged_usage = dict(usage_report.pessimistic_usage())
        payload = {
            "attempt_id": attempt_id,
            "actual_usage": charged_usage,
            "reservation_ids": reservation_ids,
            "settlement_revision": settlement_revision,
            "usage_report": usage_report.canonical_body(),
            "usage_report_digest": usage_report.digest,
        }
        self._require_active_or_idempotent(
            attempt_id=attempt_id,
            event_kind="BUDGET_SETTLED",
            payload=payload,
        )
        result = self._store.commit_command(
            command_id=f"budget:settle:{attempt_id}:{settlement_revision}",
            idempotency_key=f"budget:settle:{attempt_id}:{settlement_revision}",
            command_payload=payload,
            events=[
                CommandEvent(
                    f"event:budget:settle:{attempt_id}:{settlement_revision}",
                    "BUDGET_SETTLED",
                    "search-admission",
                    occurred_at_ns,
                    payload,
                )
            ],
            projection_mutations=[ProjectionMutation("budget_settle", payload)],
            committed_at_ns=occurred_at_ns,
        )
        return result.receipt_digest

    def hold_unknown_usage(
        self,
        *,
        attempt_id: str,
        revision: int,
        occurred_at_ns: int,
        usage_report: UsageReport | None = None,
    ) -> str:
        attempt_id = self._attempt_id(attempt_id)
        self._positive_revision(revision, "revision")
        reserved, reservation_ids = self._reservation_contract(attempt_id)
        if usage_report is None:
            usage_report = UsageReport.from_observed_and_reservation(
                reserved=reserved,
                observed={},
                complete_axes=frozenset(),
            )
        if type(usage_report) is not UsageReport:
            raise TypeError("usage_report must be UsageReport")
        usage_report.validate_reservation(reserved)
        if not usage_report.has_unknown:
            raise ValueError("unknown hold requires at least one UNKNOWN usage axis")
        payload = {
            "attempt_id": attempt_id,
            "held_usage": dict(usage_report.pessimistic_usage()),
            "reservation_ids": reservation_ids,
            "revision": revision,
            "usage_report": usage_report.canonical_body(),
            "usage_report_digest": usage_report.digest,
        }
        self._require_active_or_idempotent(
            attempt_id=attempt_id,
            event_kind="BUDGET_USAGE_UNKNOWN",
            payload=payload,
        )
        result = self._store.commit_command(
            command_id=f"budget:unknown:{attempt_id}:{revision}",
            idempotency_key=f"budget:unknown:{attempt_id}:{revision}",
            command_payload=payload,
            events=[
                CommandEvent(
                    f"event:budget:unknown:{attempt_id}:{revision}",
                    "BUDGET_USAGE_UNKNOWN",
                    "search-admission",
                    occurred_at_ns,
                    payload,
                )
            ],
            projection_mutations=[ProjectionMutation("budget_unknown", payload)],
            committed_at_ns=occurred_at_ns,
        )
        return result.receipt_digest

    @staticmethod
    def _attempt_id(value: object) -> str:
        if type(value) is not str or not value or value != value.strip():
            raise ValueError("attempt_id must be a non-empty canonical string")
        return value

    @staticmethod
    def _positive_revision(value: object, name: str) -> int:
        if type(value) is not int or value <= 0:
            raise ValueError(f"{name} must be a positive integer")
        return value

    def _reservation_contract(
        self, attempt_id: str
    ) -> tuple[dict[str, int], tuple[str, ...]]:
        admissions = tuple(
            row
            for row in self._store.event_rows(kind="ATTEMPT_ADMITTED")
            if row["payload"].get("attempt_id") == attempt_id
        )
        if len(admissions) != 1:
            raise IntegrityError("attempt must have exactly one canonical admission")
        payload = admissions[0]["payload"]
        raw_reserved = payload.get("requested_budget")
        if not isinstance(raw_reserved, Mapping):
            raise IntegrityError("canonical admission has no budget reservation")
        try:
            probe = UsageReport.from_observed_and_reservation(
                reserved=raw_reserved,
                observed={},
                complete_axes=frozenset(),
            )
        except (TypeError, ValueError) as exc:
            raise IntegrityError("canonical admission budget is malformed") from exc
        reserved = {
            item.axis: item.reserved_ceiling
            for item in probe.measurements
            if item.reserved_ceiling is not None
        }
        raw_ids = payload.get("reservation_ids")
        if type(raw_ids) not in {list, tuple}:
            raise IntegrityError("canonical admission has no reservation identities")
        reservation_ids = tuple(raw_ids)
        if (
            not reservation_ids
            or any(
                type(item) is not str or not item or item != item.strip()
                for item in reservation_ids
            )
            or len(set(reservation_ids)) != len(reservation_ids)
        ):
            raise IntegrityError("canonical reservation identities are malformed")
        return reserved, reservation_ids

    def _require_active_or_idempotent(
        self,
        *,
        attempt_id: str,
        event_kind: str,
        payload: Mapping[str, object],
    ) -> None:
        terminal = tuple(
            row
            for kind in ("BUDGET_SETTLED", "BUDGET_USAGE_UNKNOWN")
            for row in self._store.event_rows(kind=kind)
            if row["payload"].get("attempt_id") == attempt_id
        )
        if not terminal:
            return
        if len(terminal) == 1 and terminal[0]["kind"] == event_kind:
            prior = terminal[0]["payload"]
            if canonical_digest(prior) == canonical_digest(payload):
                return
        raise IntegrityError("attempt reservation set is no longer active")
