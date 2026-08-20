"""Host-owned decision context for the real Protocol 2 run plane.

The C6 packet is a sealed, lossy *view* over lossless canonical command receipt
objects.  It is compiled after an attempt identity is preallocated and before that
attempt is admitted.  The packet can guide a worker, but it cannot create facts,
progress, effects, budget, dispatch, verification, or gate decisions.

This module intentionally reuses the audited epistemic ``ContextPacketV1`` compiler
instead of the evaluator-specific C6 authority or the isolated research shadow.
"""

from __future__ import annotations

import hashlib
import json
import threading
from contextlib import contextmanager
from collections.abc import Iterator, Mapping
from dataclasses import dataclass
from typing import Any

from muteki.epistemic.cas import ReceiptCAS
from muteki.epistemic.context_packet_v1 import (
    ACCEPTED_SET_CHANGE,
    CONTEXT_PACKET_COMPILER_VERSION,
    NOT_AVAILABLE_H5,
    ContextPacketBuildRequestV1,
    ContextPacketCompilerV1,
    ContextSection,
    LossyViewKind,
    OmissionDeclarationV1,
    OmissionReason,
    PacketFieldBindingV1,
    SealedContextPacketV1,
    SourceTrustLabel,
)
from muteki.epistemic.contracts import (
    canonical_digest,
    canonical_json_bytes,
    freeze_json,
)
from muteki.epistemic.sqlite_store import (
    CommandEvent,
    EpistemicSQLiteStore,
    IntegrityError,
    ProjectionMutation,
)
from muteki.runtime.contracts import (
    AttemptIdentity,
    AttemptPermit,
    ContextPacketAdmissionBindingV1,
    EffectClass,
    ExecutionScope,
    LeaseIdentity,
)
from muteki.runtime.controller import CommandClass, LiveHealthGuard
from muteki.runtime.prompt_stage import (
    PROMPT_STAGE_VERSION,
    PromptAssemblyV1,
    PromptInvocationBindingV1,
    StagedPromptV1,
)


COGNITIVE_CONTEXT_VERSION = "muteki.runtime-context.c6.v2"
COGNITIVE_CONTEXT_ACTOR = "cognitive-context-authority-v1"
_DELIVERY_TRANSPORTS = frozenset({"argv", "stdin"})
_C6_LAUNCH_MATERIAL_SCHEMA_ID = "muteki.runtime-c6-launch-material.v1"
_C6_HOST_PROFILE_SCHEMA_ID = "muteki.runtime-c6-host-launch.v2"


@dataclass(frozen=True, slots=True)
class CognitiveFeatureGateV1:
    """Frozen C6 feature shape; default-off runs carry no such object."""

    context_version: str = COGNITIVE_CONTEXT_VERSION
    prompt_stage_version: str = PROMPT_STAGE_VERSION
    transport: str = "argv"

    def __post_init__(self) -> None:
        if self.context_version != COGNITIVE_CONTEXT_VERSION:
            raise ValueError("unsupported cognitive context version")
        if self.prompt_stage_version != PROMPT_STAGE_VERSION:
            raise ValueError("unsupported cognitive prompt-stage version")
        if self.transport != "argv":
            raise ValueError("strict C6 currently permits argv transport only")

    def canonical_body(self) -> dict[str, str]:
        return {
            "context_version": self.context_version,
            "prompt_stage_version": self.prompt_stage_version,
            "schema_id": "muteki.runtime-cognitive-feature-gate.v1",
            "transport": self.transport,
        }

    @property
    def digest(self) -> str:
        return canonical_digest(self.canonical_body())


def _text(value: object, name: str) -> str:
    if type(value) is not str or not value or value != value.strip():
        raise ValueError(f"{name} must be an exact non-empty string")
    return value


def _digest(value: object, name: str) -> str:
    text = _text(value, name)
    if len(text) != 64 or any(character not in "0123456789abcdef" for character in text):
        raise ValueError(f"{name} must be a lowercase sha256 digest")
    return text


def _transport(value: object) -> str:
    transport = _text(value, "transport")
    if transport not in _DELIVERY_TRANSPORTS:
        raise ValueError("transport must be argv or stdin")
    return transport


def _string_tuple(value: object, name: str, *, required: bool = False) -> tuple[str, ...]:
    if type(value) is not tuple:
        raise TypeError(f"{name} must be a built-in tuple")
    result = tuple(_text(item, f"{name} item") for item in value)
    if required and not result:
        raise ValueError(f"{name} must not be empty")
    if len(set(result)) != len(result):
        raise ValueError(f"{name} must not contain duplicates")
    return result


@dataclass(frozen=True, slots=True)
class DecisionContextInputV1:
    """Host-frozen decision need; never authored by the executing worker."""

    objective: str
    decision_need: str
    acceptance_boundary: str
    non_negotiable_policy: tuple[str, ...]
    remaining_budget: Mapping[str, int]
    effect_ambiguity: tuple[str, ...] = ()

    def __post_init__(self) -> None:
        for name in ("objective", "decision_need", "acceptance_boundary"):
            object.__setattr__(self, name, _text(getattr(self, name), name))
        object.__setattr__(
            self,
            "non_negotiable_policy",
            _string_tuple(
                self.non_negotiable_policy,
                "non_negotiable_policy",
                required=True,
            ),
        )
        object.__setattr__(
            self,
            "effect_ambiguity",
            _string_tuple(self.effect_ambiguity, "effect_ambiguity"),
        )
        if not isinstance(self.remaining_budget, Mapping):
            raise TypeError("remaining_budget must be a mapping")
        budget: dict[str, int] = {}
        for axis, amount in self.remaining_budget.items():
            if (
                type(axis) is not str
                or not axis
                or axis != axis.strip()
                or type(amount) is not int
                or amount < 0
            ):
                raise ValueError(
                    "remaining_budget requires canonical axes and non-negative integers"
                )
            budget[axis] = amount
        if not budget:
            raise ValueError("remaining_budget must not be empty")
        object.__setattr__(
            self,
            "remaining_budget",
            freeze_json(budget, path="$.decision_context.remaining_budget"),
        )

    def canonical_body(self) -> dict[str, Any]:
        return {
            "acceptance_boundary": self.acceptance_boundary,
            "decision_need": self.decision_need,
            "effect_ambiguity": self.effect_ambiguity,
            "non_negotiable_policy": self.non_negotiable_policy,
            "objective": self.objective,
            "remaining_budget": self.remaining_budget,
        }

    @property
    def digest(self) -> str:
        return canonical_digest(self.canonical_body())


@dataclass(frozen=True, slots=True)
class DeliveredContextPacketV1:
    """Immutable delivery object passed to a worker after exact admission."""

    binding: ContextPacketAdmissionBindingV1
    packet: SealedContextPacketV1

    def __post_init__(self) -> None:
        if type(self.binding) is not ContextPacketAdmissionBindingV1:
            raise TypeError("binding must be ContextPacketAdmissionBindingV1")
        if type(self.packet) is not SealedContextPacketV1:
            raise TypeError("packet must be SealedContextPacketV1")
        if (
            self.binding.packet_digest != self.packet.digest
            or self.binding.manifest_digest != self.packet.manifest.digest
            or self.binding.target_attempt_id != self.packet.manifest.target_attempt_id
        ):
            raise ValueError("delivered packet differs from its admission binding")

    @property
    def prompt_digest(self) -> str:
        return canonical_digest({"prompt": self.render_for_prompt()})

    def render_for_prompt(self) -> str:
        """Render the bounded sealed view; lossless receipts stay outside the prompt."""

        lines = [
            "\n## Sealed decision context (C6; attempt-bound)",
            f"Packet digest: {self.binding.packet_digest}",
            f"Manifest digest: {self.binding.manifest_digest}",
            f"Target attempt: {self.binding.target_attempt_id}",
            (
                "Authority boundary: this is a lossy view of canonical receipts. "
                "Proposal text is not verified evidence, and the hard acceptance "
                "gate is unchanged."
            ),
        ]
        for section in self.packet.view.sections:
            visible = section.items
            omissions = section.omissions
            if not visible and not omissions and not section.stage_marker:
                continue
            lines.append(f"### {section.section.value}")
            if section.stage_marker:
                lines.append(f"- stage: {section.stage_marker}")
            for atom in visible:
                rendered = canonical_json_bytes(atom.value).decode("utf-8")
                lines.append(
                    f"- {atom.field_id} [{atom.trust.value}]: {rendered}"
                )
            for omission in omissions:
                lines.append(
                    f"- omitted {omission.omission_id}: {omission.reason.value}"
                    + (f" ({omission.stage_marker})" if omission.stage_marker else "")
                )
        return "\n".join(lines) + "\n"


class PromptInvocationAlreadyBound(IntegrityError):
    """A host launch attempted to reuse an irrevocable C6 invocation boundary.

    ``bind_prompt_invocation`` remains idempotent for replay/reconstruction, but a
    live host broker must never interpret that idempotence as permission to run the
    sealed argv a second time.  The broker handles this condition by terminalizing
    the unresolved boundary as UNKNOWN before it can reach Popen.
    """


class PromptLaunchAlreadyClaimed(IntegrityError):
    """A durable C6 pre-Popen claim cannot be reused for another process."""


@dataclass(frozen=True, slots=True)
class PromptLaunchClaimV1:
    """One immutable, durable authority boundary immediately before local Popen."""

    staged: StagedPromptV1
    invocation: PromptInvocationBindingV1
    claim_id: str
    launch_material_digest: str
    profile_digest: str

    def __post_init__(self) -> None:
        if type(self.staged) is not StagedPromptV1:
            raise TypeError("staged must be StagedPromptV1")
        if type(self.invocation) is not PromptInvocationBindingV1:
            raise TypeError("invocation must be PromptInvocationBindingV1")
        if self.invocation.staged != self.staged:
            raise ValueError("claim invocation must bind the exact staged prompt")
        object.__setattr__(self, "claim_id", _text(self.claim_id, "claim_id"))
        if not self.claim_id.startswith("claim-"):
            raise ValueError("claim_id must have the canonical claim prefix")
        object.__setattr__(
            self,
            "launch_material_digest",
            _digest(self.launch_material_digest, "launch_material_digest"),
        )
        object.__setattr__(self, "profile_digest", _digest(self.profile_digest, "profile_digest"))


class CognitiveContextAuthority:
    """Narrow host authority: canonical decision event -> sealed packet event."""

    def __init__(self, *, store: EpistemicSQLiteStore, cas: ReceiptCAS) -> None:
        if not isinstance(store, EpistemicSQLiteStore):
            raise TypeError("store must be EpistemicSQLiteStore")
        if not isinstance(cas, ReceiptCAS):
            raise TypeError("cas must be ReceiptCAS")
        self._store = store
        self._cas = cas
        self._lock = threading.RLock()

    @contextmanager
    def _fence_final_host_launch(
        self,
        *,
        delivered: DeliveredContextPacketV1,
        permit: AttemptPermit,
        claim: PromptLaunchClaimV1,
    ) -> Iterator[None]:
        """Hold the exact C6 validation/Popen/terminal SQLite fence.

        This helper deliberately performs no live-state validation before the
        transaction begins.  The caller's final durable validation must run *in*
        the store fence so a second cooperative host cannot insert a terminal,
        budget closure, or BOOT transition between that check and local Popen.
        """

        self._assert_delivered_binding(delivered=delivered, permit=permit)
        if type(claim) is not PromptLaunchClaimV1:
            raise TypeError("claim must be PromptLaunchClaimV1")
        if claim.staged.permit_digest != permit.digest:
            raise IntegrityError("C6 host launch fence claim belongs to another permit")
        with self._store.c6_host_launch_fence(
            claim_id=claim.claim_id,
            stage_id=claim.staged.stage_id,
        ):
            yield

    def _assert_fenced_host_launch_terminal(
        self,
        *,
        delivered: DeliveredContextPacketV1,
        permit: AttemptPermit,
        claim: PromptLaunchClaimV1,
        expected_kind: str,
    ) -> None:
        """Require the terminal written inside one live C6 launch fence.

        The interlock invokes this before the fence commits.  It prevents a
        callback from returning after Popen without the exact durable terminal
        that makes the next writer's ordering replayable.
        """

        if expected_kind not in {
            "CONTEXT_PROMPT_RELEASED",
            "CONTEXT_PROMPT_PRELAUNCH_ABORTED",
            "CONTEXT_PROMPT_UNKNOWN",
        }:
            raise ValueError("unsupported C6 fenced terminal kind")
        self._claim_row(delivered=delivered, permit=permit, claim=claim)
        terminals = self._terminal_rows_for_stage(stage_id=claim.staged.stage_id)
        if len(terminals) != 1 or terminals[0]["kind"] != expected_kind:
            raise IntegrityError("C6 host launch fence terminal is absent or divergent")
        payload = terminals[0]["payload"]
        if (
            payload.get("stage_id") != claim.staged.stage_id
            or payload.get("permit_digest") != permit.digest
        ):
            raise IntegrityError("C6 host launch fence terminal identity diverged")
        if (
            expected_kind != "CONTEXT_PROMPT_UNKNOWN"
            and payload.get("claim_id") != claim.claim_id
        ):
            raise IntegrityError("C6 host launch fence terminal claim diverged")

    def _seal_host_launch_material(self, *, body: Mapping[str, Any]) -> str:
        """Seal a non-secret canonical launch-material manifest for the host broker."""

        if not isinstance(body, Mapping):
            raise TypeError("C6 launch material must be a mapping")
        raw = canonical_json_bytes(dict(body))
        sealed = self._cas.seal_bytes(raw)
        if (
            sealed.digest != hashlib.sha256(raw).hexdigest()
            or self._cas.read_verified(sealed.digest) != raw
        ):
            raise IntegrityError("C6 launch material CAS object diverged")
        return sealed.digest

    def compile_reproduction_for_attempt(
        self,
        *,
        source_observation_event_digest: str,
        attempt: AttemptIdentity,
        feature_gate: CognitiveFeatureGateV1,
        occurred_at_ns: int,
    ) -> DeliveredContextPacketV1:
        """Compile O2 only from O1's canonical pre-outcome decision context.

        The caller supplies no reproduction prose.  The later reproduction
        admission/store guard independently compares these inherited fields, so
        this convenience entry point is not itself the blindness authority.
        """

        from muteki.epistemic.cognitive_events_v1 import (
            COGNITIVE_EXECUTION_OBSERVED,
            COGNITIVE_EXPERIMENT_ASSIGNED,
            COGNITIVE_RUNTIME_EXECUTABLE_ASSIGNMENT_SCHEMA_ID,
            COGNITIVE_RUNTIME_EXECUTION_SCHEMA_ID,
        )

        source_observation_event_digest = _digest(
            source_observation_event_digest,
            "source_observation_event_digest",
        )
        observations = tuple(
            row
            for row in self._store.event_rows(kind=COGNITIVE_EXECUTION_OBSERVED)
            if row["event_digest"] == source_observation_event_digest
        )
        if len(observations) != 1:
            raise IntegrityError(
                "reproduction context source observation is absent or ambiguous"
            )
        observation = observations[0]
        if observation["payload"].get("schema_id") != COGNITIVE_RUNTIME_EXECUTION_SCHEMA_ID:
            raise IntegrityError("reproduction context source is not a runtime observation")
        assignments = tuple(
            row
            for row in self._store.event_rows(kind=COGNITIVE_EXPERIMENT_ASSIGNED)
            if row["event_digest"]
            == observation["payload"].get("assignment_event_digest")
        )
        if (
            len(assignments) != 1
            or assignments[0]["payload"].get("schema_id")
            != COGNITIVE_RUNTIME_EXECUTABLE_ASSIGNMENT_SCHEMA_ID
        ):
            raise IntegrityError(
                "reproduction context source assignment is absent or unsupported"
            )
        source_packet = assignments[0]["payload"].get(
            "context_packet_binding_body"
        )
        if not isinstance(source_packet, Mapping):
            raise IntegrityError("reproduction source packet binding is malformed")
        decisions = tuple(
            row
            for row in self._store.event_rows(
                kind="RUNTIME_CONTEXT_DECISION_REGISTERED"
            )
            if row["payload"].get("decision_id") == source_packet.get("decision_id")
        )
        if len(decisions) != 1 or decisions[0]["seq"] >= assignments[0]["seq"]:
            raise IntegrityError(
                "reproduction source has no unique pre-outcome decision"
            )
        body = decisions[0]["payload"]
        context = DecisionContextInputV1(
            objective=body["objective"],
            decision_need=body["decision_need"],
            acceptance_boundary=body["acceptance_boundary"],
            non_negotiable_policy=tuple(body["non_negotiable_policy"]),
            remaining_budget=dict(body["remaining_budget"]),
            effect_ambiguity=tuple(body["effect_ambiguity"]),
        )
        return self.compile_for_attempt(
            attempt=attempt,
            context=context,
            feature_gate=feature_gate,
            occurred_at_ns=occurred_at_ns,
        )

    def compile_for_attempt(
        self,
        *,
        attempt: AttemptIdentity,
        context: DecisionContextInputV1,
        feature_gate: CognitiveFeatureGateV1,
        occurred_at_ns: int,
    ) -> DeliveredContextPacketV1:
        if type(attempt) is not AttemptIdentity:
            raise TypeError("attempt must be AttemptIdentity")
        if type(context) is not DecisionContextInputV1:
            raise TypeError("context must be DecisionContextInputV1")
        if type(feature_gate) is not CognitiveFeatureGateV1:
            raise TypeError("feature_gate must be CognitiveFeatureGateV1")
        if type(occurred_at_ns) is not int or occurred_at_ns < 0:
            raise ValueError("occurred_at_ns must be a non-negative exact integer")
        state = self._store.state()
        if (
            attempt.scope.run_id != state.run_id
            or attempt.scope.run_fence_epoch != state.run_fence_epoch
            or attempt.scope.execution_generation != state.execution_generation
        ):
            raise IntegrityError("context attempt is outside the current execution scope")

        # There is exactly one pre-decision slot for a preallocated attempt.  The
        # context body deliberately does *not* participate in this identity: a
        # changed body must conflict with the same idempotency key rather than leave
        # a second, orphaned canonical decision before admission.
        identity_digest = canonical_digest(
            {
                "attempt_digest": attempt.digest,
                "version": COGNITIVE_CONTEXT_VERSION,
            }
        )
        decision_id = f"decision-{identity_digest[:32]}"
        decision_epoch_id = f"epoch-{attempt.scope.digest[:32]}"
        decision_payload = {
            **context.canonical_body(),
            "accepted_set_change": ACCEPTED_SET_CHANGE,
            "attempt_digest": attempt.digest,
            "context_digest": context.digest,
            "decision_epoch_id": decision_epoch_id,
            "decision_id": decision_id,
            "feature_state_digest": feature_gate.digest,
            "preallocated_attempt_id": attempt.attempt_id,
            "scope_digest": attempt.scope.digest,
            "schema_id": COGNITIVE_CONTEXT_VERSION,
        }
        decision_result = self._store.commit_command(
            command_id=f"context:decision:{decision_id}",
            idempotency_key=f"context:decision:{decision_id}",
            command_payload=decision_payload,
            events=(
                CommandEvent(
                    f"event:context-decision:{decision_id}",
                    "RUNTIME_CONTEXT_DECISION_REGISTERED",
                    COGNITIVE_CONTEXT_ACTOR,
                    occurred_at_ns,
                    decision_payload,
                ),
            ),
            authority_capability=self._store._cognitive_context_commit_capability,
            forbid_attempt_admission_id=attempt.attempt_id,
            forbid_prior_events=(
                (
                    "RUNTIME_CONTEXT_DECISION_REGISTERED",
                    {
                        "preallocated_attempt_id": attempt.attempt_id,
                        "scope_digest": attempt.scope.digest,
                    },
                ),
            ),
            committed_at_ns=occurred_at_ns,
        )
        cutoff_seq = decision_result.last_seq
        resolver = self._store.receipt_field_resolver(cutoff_seq=cutoff_seq)
        prefix = resolver.verify_complete_through(cutoff_seq)

        def pointer(field: str):
            return resolver.pointer_for(
                decision_result.receipt_digest,
                f"events[0].payload.{field}",
                cutoff_seq=cutoff_seq,
            )

        contract = SourceTrustLabel.CANONICAL_CONTRACT
        bindings = (
            PacketFieldBindingV1(
                "objective",
                ContextSection.OBJECTIVE_POLICY,
                contract,
                LossyViewKind.TEXT,
                pointer("objective"),
                critical=True,
            ),
            PacketFieldBindingV1(
                "non_negotiable_policy",
                ContextSection.OBJECTIVE_POLICY,
                contract,
                LossyViewKind.STRING_LIST,
                pointer("non_negotiable_policy"),
                critical=True,
            ),
            PacketFieldBindingV1(
                "acceptance_boundary",
                ContextSection.OBJECTIVE_POLICY,
                contract,
                LossyViewKind.TEXT,
                pointer("acceptance_boundary"),
                critical=True,
            ),
            PacketFieldBindingV1(
                "preallocated_attempt_id",
                ContextSection.EPOCH_CAPABILITIES,
                contract,
                LossyViewKind.TEXT,
                pointer("preallocated_attempt_id"),
                critical=True,
            ),
            PacketFieldBindingV1(
                "decision_need",
                ContextSection.HYPOTHESIS_BOUNDARY,
                contract,
                LossyViewKind.TEXT,
                pointer("decision_need"),
                critical=True,
            ),
            PacketFieldBindingV1(
                "remaining_budget",
                ContextSection.EFFECT_BUDGET,
                contract,
                LossyViewKind.NONNEGATIVE_INT_MAP,
                pointer("remaining_budget"),
                critical=True,
            ),
            PacketFieldBindingV1(
                "effect_ambiguity",
                ContextSection.EFFECT_BUDGET,
                contract,
                LossyViewKind.STRING_LIST,
                pointer("effect_ambiguity"),
                critical=True,
            ),
        )
        request = ContextPacketBuildRequestV1(
            run_id=attempt.scope.run_id,
            decision_id=decision_id,
            decision_epoch_id=decision_epoch_id,
            target_attempt_id=attempt.attempt_id,
            cutoff_seq=cutoff_seq,
            cutoff_head_event_digest=prefix.head_event_digest,
            expected_index_digest=prefix.index_digest,
            expected_prefix_digest=prefix.digest,
            bindings=bindings,
            omissions=(
                OmissionDeclarationV1(
                    "h5-hypothesis-boundary",
                    OmissionReason.STAGE_UNAVAILABLE,
                    stage_marker=NOT_AVAILABLE_H5,
                ),
            ),
        )
        packet = ContextPacketCompilerV1().compile(
            request,
            resolver=resolver,
            cas=self._cas,
        )
        compiler_receipt_body = {
            "accepted_set_change": ACCEPTED_SET_CHANGE,
            "build_request_digest": request.digest,
            "compiler_version": CONTEXT_PACKET_COMPILER_VERSION,
            "cutoff_seq": cutoff_seq,
            "decision_receipt_digest": decision_result.receipt_digest,
            "feature_state_digest": feature_gate.digest,
            "manifest_digest": packet.manifest.digest,
            "packet_byte_count": packet.sealed.byte_count,
            "packet_digest": packet.digest,
            "scope_digest": attempt.scope.digest,
            "target_attempt_id": attempt.attempt_id,
        }
        compiler_receipt = self._cas.seal_bytes(
            canonical_json_bytes(compiler_receipt_body)
        )
        compile_payload = {
            **compiler_receipt_body,
            "compiler_receipt_digest": compiler_receipt.digest,
            "decision_id": decision_id,
            "schema_id": COGNITIVE_CONTEXT_VERSION,
        }
        compilation_result = self._store.commit_command(
            command_id=f"context:packet:{decision_id}",
            idempotency_key=f"context:packet:{decision_id}",
            command_payload=compile_payload,
            events=(
                CommandEvent(
                    f"event:context-packet:{decision_id}",
                    "CONTEXT_PACKET_COMPILED",
                    COGNITIVE_CONTEXT_ACTOR,
                    occurred_at_ns + 1,
                    compile_payload,
                ),
            ),
            authority_capability=self._store._cognitive_context_commit_capability,
            forbid_attempt_admission_id=attempt.attempt_id,
            required_prior_event=(
                "RUNTIME_CONTEXT_DECISION_REGISTERED",
                {
                    "decision_id": decision_id,
                    "preallocated_attempt_id": attempt.attempt_id,
                },
            ),
            committed_at_ns=occurred_at_ns + 1,
        )
        binding = ContextPacketAdmissionBindingV1(
            target_attempt_id=attempt.attempt_id,
            decision_id=decision_id,
            decision_receipt_digest=decision_result.receipt_digest,
            compiler_receipt_digest=compiler_receipt.digest,
            compilation_event_receipt_digest=compilation_result.receipt_digest,
            packet_digest=packet.digest,
            manifest_digest=packet.manifest.digest,
            cutoff_seq=cutoff_seq,
            compiler_version=CONTEXT_PACKET_COMPILER_VERSION,
            feature_state_digest=feature_gate.digest,
        )
        return DeliveredContextPacketV1(binding=binding, packet=packet)

    def record_packet_unadmitted(
        self,
        *,
        delivered: DeliveredContextPacketV1,
        reason: str,
        occurred_at_ns: int,
    ) -> str:
        """Close a compiled packet that never obtained an attempt permit.

        A failed worker preflight or admission decision must not leave a compiled
        packet looking like an eligible work item on restart.  This is a terminal
        *pre-admission* classification only: it creates no effect, retry, budget,
        progress, or worker authority.
        """

        if type(delivered) is not DeliveredContextPacketV1:
            raise TypeError("delivered must be DeliveredContextPacketV1")
        if type(occurred_at_ns) is not int or occurred_at_ns < 0:
            raise ValueError("occurred_at_ns must be a non-negative exact integer")
        binding = delivered.binding
        payload = {
            "accepted_set_change": ACCEPTED_SET_CHANGE,
            "compilation_event_receipt_digest": binding.compilation_event_receipt_digest,
            "compiler_receipt_digest": binding.compiler_receipt_digest,
            "decision_id": binding.decision_id,
            "feature_state_digest": binding.feature_state_digest,
            "manifest_digest": binding.manifest_digest,
            "packet_digest": binding.packet_digest,
            "reason_digest": canonical_digest({"reason": _text(reason, "reason")}),
            "schema_id": COGNITIVE_CONTEXT_VERSION,
            "scope_digest": self._scope_digest_for_binding(binding=binding),
            "target_attempt_id": binding.target_attempt_id,
        }
        result = self._store.commit_command(
            command_id=f"context:unadmitted:{binding.decision_id}",
            idempotency_key=f"context:unadmitted:{binding.decision_id}",
            command_payload=payload,
            events=(
                CommandEvent(
                    f"event:context-packet-unadmitted:{binding.decision_id}",
                    "CONTEXT_PACKET_UNADMITTED",
                    COGNITIVE_CONTEXT_ACTOR,
                    occurred_at_ns,
                    payload,
                ),
            ),
            authority_capability=self._store._cognitive_context_commit_capability,
            required_prior_event=(
                "CONTEXT_PACKET_COMPILED",
                {
                    "compiler_receipt_digest": binding.compiler_receipt_digest,
                    "feature_state_digest": binding.feature_state_digest,
                    "packet_digest": binding.packet_digest,
                    "target_attempt_id": binding.target_attempt_id,
                },
            ),
            forbid_attempt_admission_id=binding.target_attempt_id,
            forbid_prior_events=(
                (
                    "CONTEXT_PACKET_UNADMITTED",
                    {"target_attempt_id": binding.target_attempt_id},
                ),
            ),
            committed_at_ns=occurred_at_ns,
        )
        return result.receipt_digest

    def _scope_digest_for_binding(
        self, *, binding: ContextPacketAdmissionBindingV1
    ) -> str:
        rows = [
            row
            for row in self._store.event_rows(kind="CONTEXT_PACKET_COMPILED")
            if row["payload"].get("compiler_receipt_digest")
            == binding.compiler_receipt_digest
        ]
        if len(rows) != 1:
            raise IntegrityError("compiled ContextPacket does not resolve uniquely")
        scope_digest = rows[0]["payload"].get("scope_digest")
        return _digest(scope_digest, "scope_digest")

    @staticmethod
    def _assert_delivered_binding(
        *, delivered: DeliveredContextPacketV1, permit: AttemptPermit
    ) -> None:
        if type(delivered) is not DeliveredContextPacketV1:
            raise TypeError("delivered must be DeliveredContextPacketV1")
        if type(permit) is not AttemptPermit:
            raise TypeError("permit must be AttemptPermit")
        binding = delivered.binding
        if (
            permit.lease.attempt.attempt_id != binding.target_attempt_id
            or permit.constraints.get("context_packet") != binding.canonical_body()
        ):
            raise IntegrityError("permit is not bound to the ContextPacket")

    def _admission_row(
        self, *, delivered: DeliveredContextPacketV1, permit: AttemptPermit
    ) -> dict[str, Any]:
        self._assert_delivered_binding(delivered=delivered, permit=permit)
        binding = delivered.binding
        rows = [
            row
            for row in self._store.event_rows(kind="ATTEMPT_ADMITTED")
            if row["payload"].get("attempt_id") == binding.target_attempt_id
            and row["payload"].get("permit_digest") == permit.digest
        ]
        if (
            len(rows) != 1
            or rows[0]["payload"].get("context_packet")
            != binding.canonical_body()
        ):
            raise IntegrityError("ContextPacket has no exact canonical admission")
        return rows[0]

    def _rebuild_admitted_context(
        self, *, admission: Mapping[str, Any], occurred_at_ns: int
    ) -> tuple[DeliveredContextPacketV1, AttemptPermit]:
        """Reconstruct one C6 admission solely from canonical receipts.

        This is boot-recovery code, not a general retry API.  Recompiling the
        deterministic packet must resolve existing command receipts; any change in
        the decision, feature gate, permit, packet, or CAS object fails closed.
        """

        payload = admission
        context_body = payload.get("context_packet")
        permit_body = payload.get("permit")
        if not isinstance(context_body, Mapping) or not isinstance(permit_body, Mapping):
            raise IntegrityError("C6 admission has no reconstructible packet or permit")
        try:
            binding = ContextPacketAdmissionBindingV1(**dict(context_body))
            state = self._store.state()
            scope = ExecutionScope(
                state.run_id, state.run_fence_epoch, state.execution_generation
            )
            attempt = AttemptIdentity(
                scope=scope,
                branch_id=payload.get("branch_id"),
                attempt_id=payload.get("attempt_id"),
                launch_ordinal=payload.get("launch_ordinal"),
            )
            lease = LeaseIdentity(
                attempt=attempt,
                lease_id=payload.get("lease_id"),
                lease_epoch=payload.get("lease_epoch"),
                worker_generation=payload.get("worker_generation"),
            )
            permit = AttemptPermit(
                permit_id=permit_body.get("permit_id"),
                lease=lease,
                policy_digest=permit_body.get("policy_digest"),
                reservation_ids=tuple(permit_body.get("reservation_ids", ())),
                effect_class=EffectClass(permit_body.get("effect_class")),
                expires_at_ns=permit_body.get("expires_at_ns"),
                constraints=permit_body.get("constraints"),
            )
        except (TypeError, ValueError) as exc:
            raise IntegrityError("C6 admission identity cannot be reconstructed") from exc
        expected_admission = {
            "attempt_digest": attempt.digest,
            "attempt_id": attempt.attempt_id,
            "lease_digest": lease.digest,
            "lease_id": lease.lease_id,
            "permit_digest": permit.digest,
            "permit_id": permit.permit_id,
            "scope_digest": scope.digest,
        }
        if (
            any(payload.get(name) != value for name, value in expected_admission.items())
            or binding.target_attempt_id != attempt.attempt_id
            or permit.constraints.get("context_packet") != binding.canonical_body()
        ):
            raise IntegrityError("C6 admission identity diverged during recovery")
        gate = CognitiveFeatureGateV1()
        if binding.feature_state_digest != gate.digest:
            raise IntegrityError("C6 recovery does not recognize the frozen feature gate")
        decisions = [
            row
            for row in self._store.event_rows(kind="RUNTIME_CONTEXT_DECISION_REGISTERED")
            if row["payload"].get("decision_id") == binding.decision_id
        ]
        if len(decisions) != 1:
            raise IntegrityError("C6 admission has no unique canonical decision")
        decision = decisions[0]["payload"]
        try:
            context = DecisionContextInputV1(
                objective=decision.get("objective"),
                decision_need=decision.get("decision_need"),
                acceptance_boundary=decision.get("acceptance_boundary"),
                non_negotiable_policy=tuple(decision.get("non_negotiable_policy", ())),
                remaining_budget=decision.get("remaining_budget"),
                effect_ambiguity=tuple(decision.get("effect_ambiguity", ())),
            )
        except (TypeError, ValueError) as exc:
            raise IntegrityError("C6 decision context cannot be reconstructed") from exc
        if (
            decision.get("attempt_digest") != attempt.digest
            or decision.get("context_digest") != context.digest
            or decision.get("feature_state_digest") != gate.digest
            or decision.get("preallocated_attempt_id") != attempt.attempt_id
            or decision.get("scope_digest") != scope.digest
        ):
            raise IntegrityError("C6 decision lineage diverged during recovery")
        delivered = self.compile_for_attempt(
            attempt=attempt,
            context=context,
            feature_gate=gate,
            occurred_at_ns=occurred_at_ns,
        )
        if delivered.binding != binding:
            raise IntegrityError("C6 compiled packet diverged during recovery")
        # This also re-checks the exact admission event rather than trusting the
        # reconstruction path alone.
        self._admission_row(delivered=delivered, permit=permit)
        return delivered, permit

    def _rebuild_staged_prompt(
        self,
        *,
        delivered: DeliveredContextPacketV1,
        permit: AttemptPermit,
        row: Mapping[str, Any],
    ) -> StagedPromptV1:
        payload = row.get("payload")
        if not isinstance(payload, Mapping):
            raise IntegrityError("C6 prompt stage payload is malformed")
        try:
            assembly = PromptAssemblyV1(
                packet_digest=delivered.binding.packet_digest,
                context_block_digest=_digest(
                    payload.get("context_block_digest"), "context_block_digest"
                ),
                full_prompt_digest=_digest(
                    payload.get("prompt_artifact_digest"), "prompt_artifact_digest"
                ),
                full_prompt_byte_count=payload.get("prompt_byte_count"),
                transport=payload.get("transport"),
            )
            staged = StagedPromptV1(
                attempt_digest=permit.lease.attempt.digest,
                permit_digest=permit.digest,
                assembly=assembly,
                stage_id=payload.get("stage_id"),
            )
            self._stage_row(delivered=delivered, permit=permit, staged=staged)
        except (TypeError, ValueError) as exc:
            raise IntegrityError("C6 prompt stage cannot be reconstructed") from exc
        return staged

    def _rebuild_prompt_invocation(
        self,
        *,
        delivered: DeliveredContextPacketV1,
        permit: AttemptPermit,
        staged: StagedPromptV1,
        row: Mapping[str, Any],
    ) -> PromptInvocationBindingV1:
        payload = row.get("payload")
        if not isinstance(payload, Mapping):
            raise IntegrityError("C6 prompt invocation payload is malformed")
        try:
            invocation = PromptInvocationBindingV1(
                staged=staged,
                argv_artifact_digest=_digest(
                    payload.get("argv_artifact_digest"), "argv_artifact_digest"
                ),
                argv_byte_count=payload.get("argv_byte_count"),
                prompt_argument_count=payload.get("prompt_argument_count"),
                invocation_id=payload.get("invocation_id"),
            )
            self._invocation_row(
                delivered=delivered,
                permit=permit,
                staged=staged,
                invocation=invocation,
            )
        except (TypeError, ValueError) as exc:
            raise IntegrityError("C6 prompt invocation cannot be reconstructed") from exc
        return invocation

    def _terminal_rows_for_stage(self, *, stage_id: str) -> tuple[dict[str, Any], ...]:
        rows = [
            row
            for kind in (
                "CONTEXT_PROMPT_RELEASED",
                "CONTEXT_PROMPT_UNKNOWN",
                "CONTEXT_PROMPT_PRELAUNCH_ABORTED",
            )
            for row in self._store.event_rows(kind=kind)
            if row["payload"].get("stage_id") == stage_id
        ]
        if len(rows) > 1:
            raise IntegrityError("C6 prompt stage has ambiguous terminal records")
        return tuple(rows)

    def _close_existing_prompt_invocation_for_host(
        self,
        *,
        delivered: DeliveredContextPacketV1,
        permit: AttemptPermit,
        reason: str,
        occurred_at_ns: int,
    ) -> str:
        """Atomically close a persisted broker boundary before a new broker runs.

        This is deliberately host-internal.  A durable invocation has already
        crossed the irreversible argv construction boundary, so a restarted or
        competing broker must never treat its idempotent identity as permission to
        start another process.  Under the store writer lock, a dangling boundary is
        converted to UNKNOWN; a completed boundary is simply reported as terminal.
        """

        if type(occurred_at_ns) is not int or occurred_at_ns < 0:
            raise ValueError("occurred_at_ns must be a non-negative exact integer")
        _text(reason, "reason")
        with self._store._lock:
            self._admission_row(delivered=delivered, permit=permit)
            invocations = [
                row
                for row in self._store.event_rows(
                    kind="CONTEXT_PROMPT_INVOCATION_BOUND"
                )
                if row["payload"].get("permit_digest") == permit.digest
            ]
            if not invocations:
                return "none"
            if len(invocations) != 1:
                raise IntegrityError("C6 permit has multiple invocation boundaries")
            invocation_row = invocations[0]
            stage_id = invocation_row["payload"].get("stage_id")
            if type(stage_id) is not str or not stage_id:
                raise IntegrityError("C6 invocation has no exact staged prompt")
            terminal = self._terminal_rows_for_stage(stage_id=stage_id)
            if terminal:
                return {
                    "CONTEXT_PROMPT_RELEASED": "released",
                    "CONTEXT_PROMPT_UNKNOWN": "unknown",
                    "CONTEXT_PROMPT_PRELAUNCH_ABORTED": "aborted",
                }[terminal[0]["kind"]]
            stages = [
                row
                for row in self._store.event_rows(kind="CONTEXT_PROMPT_STAGED")
                if row["payload"].get("permit_digest") == permit.digest
                and row["payload"].get("stage_id") == stage_id
            ]
            if len(stages) != 1:
                raise IntegrityError("C6 invocation has no unique staged prompt")
            staged = self._rebuild_staged_prompt(
                delivered=delivered, permit=permit, row=stages[0]
            )
            invocation = self._rebuild_prompt_invocation(
                delivered=delivered,
                permit=permit,
                staged=staged,
                row=invocation_row,
            )
            try:
                self.record_prompt_unknown(
                    delivered=delivered,
                    permit=permit,
                    staged=staged,
                    invocation=invocation,
                    reason=reason,
                    occurred_at_ns=occurred_at_ns,
                )
            except IntegrityError:
                terminal = self._terminal_rows_for_stage(stage_id=stage_id)
                if terminal:
                    return {
                        "CONTEXT_PROMPT_RELEASED": "released",
                        "CONTEXT_PROMPT_UNKNOWN": "unknown",
                        "CONTEXT_PROMPT_PRELAUNCH_ABORTED": "aborted",
                    }[terminal[0]["kind"]]
                raise
            return "unknown"

    def _assert_host_launch_live(
        self,
        *,
        delivered: DeliveredContextPacketV1,
        permit: AttemptPermit,
        occurred_at_ns: int,
    ) -> str:
        """Resolve the active worker owner immediately before a host launch claim."""

        if type(occurred_at_ns) is not int or occurred_at_ns < 0:
            raise ValueError("occurred_at_ns must be a non-negative exact integer")
        with self._store._lock:
            self._admission_row(delivered=delivered, permit=permit)
            state = self._store._state()
            scope = permit.lease.attempt.scope
            if (
                state.run_id != scope.run_id
                or state.run_fence_epoch != scope.run_fence_epoch
                or state.execution_generation != scope.execution_generation
                or state.kernel_health.value != "ready"
                or state.run_execution.value != "running"
                or state.search_mode.value != "active"
            ):
                raise IntegrityError("C6 host launch is outside the active scope")
            if occurred_at_ns >= permit.expires_at_ns:
                raise IntegrityError("C6 host launch permit is expired")
            attempt = self._store._conn.execute(
                "SELECT permit_id,scope_digest,lease_id,state "
                "FROM runtime_attempts WHERE attempt_id=?",
                (permit.lease.attempt.attempt_id,),
            ).fetchone()
            if attempt != (
                permit.permit_id,
                scope.digest,
                permit.lease.lease_id,
                "running",
            ):
                raise IntegrityError("C6 host launch attempt owner is not active")
            reservations = self._store._conn.execute(
                "SELECT state FROM budget_reservations WHERE attempt_id=?",
                (permit.lease.attempt.attempt_id,),
            ).fetchall()
            if not reservations or any(row[0] != "active" for row in reservations):
                raise IntegrityError("C6 host launch reservations are not active")
            launch = self._worker_launch_row(permit=permit)
            terminal_rows = [
                row
                for kind in ("WORKER_TERMINAL", "WORKER_UNKNOWN")
                for row in self._store.event_rows(kind=kind)
                if row["payload"].get("permit_id") == permit.permit_id
            ]
            if terminal_rows:
                raise IntegrityError("C6 host launch follows a worker terminal")
            return str(launch["event_digest"])

    def _assert_durable_host_launch_claim_live(
        self,
        *,
        delivered: DeliveredContextPacketV1,
        permit: AttemptPermit,
        claim: PromptLaunchClaimV1,
        occurred_at_ns: int,
    ) -> str:
        """Revalidate the exact durable claim at the actual local Popen fence.

        ``CONTEXT_PROMPT_LAUNCH_CLAIMED`` is deliberately not a bearer token.  The
        host adapter calls this while it holds the supervisor-owned interlock,
        immediately before its synchronous ``subprocess.Popen`` callback.  This
        checks the persisted claim/material/profile lineage *and* the same live
        attempt/scope conditions used when the claim was committed.  A shaped
        in-memory claim can therefore never authorize a child by itself.
        """

        if type(claim) is not PromptLaunchClaimV1:
            raise TypeError("claim must be PromptLaunchClaimV1")
        if type(occurred_at_ns) is not int or occurred_at_ns < 0:
            raise ValueError("occurred_at_ns must be a non-negative exact integer")
        with self._store._lock:
            claim_row, receipt = self._claim_row(
                delivered=delivered,
                permit=permit,
                claim=claim,
            )
            if self._terminal_rows_for_stage(stage_id=claim.staged.stage_id):
                raise IntegrityError("C6 host launch claim is already terminal")
            launch_event_digest = self._assert_host_launch_live(
                delivered=delivered,
                permit=permit,
                occurred_at_ns=occurred_at_ns,
            )
            if claim_row["payload"].get("worker_launch_event_digest") != launch_event_digest:
                raise IntegrityError("C6 durable claim launch lineage diverged")
            return receipt

    def recover_dangling_prompt_invocations(
        self,
        *,
        guard: LiveHealthGuard,
        occurred_at_ns: int,
    ) -> tuple[str, ...]:
        """Boot-only global sweep for invocation-bound prompts without a terminal.

        A release may have crossed the process/provider boundary before a crash, so
        a persisted ``INVOCATION_BOUND`` with no terminal record is always turned
        into canonical UNKNOWN.  Stage-only records are intentionally not relabelled
        UNKNOWN: they never crossed the argv/Popen boundary and require a separate
        prelaunch-abort design if they must become closable later.
        """

        if type(guard) is not LiveHealthGuard:
            raise TypeError("guard must be LiveHealthGuard")
        if type(occurred_at_ns) is not int or occurred_at_ns < 0:
            raise ValueError("occurred_at_ns must be a non-negative exact integer")
        guard.authorize(CommandClass.RECOVERY, self._store.state())
        receipts: list[str] = []
        invocation_rows = self._store.event_rows(
            kind="CONTEXT_PROMPT_INVOCATION_BOUND"
        )
        dangling_by_permit: dict[tuple[str, str], dict[str, Any]] = {}
        for invocation_row in invocation_rows:
            invocation_payload = invocation_row["payload"]
            stage_id = invocation_payload.get("stage_id")
            attempt_id = invocation_payload.get("target_attempt_id")
            permit_digest = invocation_payload.get("permit_digest")
            if (
                type(stage_id) is not str
                or not stage_id
                or type(attempt_id) is not str
                or not attempt_id
                or type(permit_digest) is not str
                or not permit_digest
            ):
                raise IntegrityError("C6 recovery found a malformed invocation identity")
            terminals = self._terminal_rows_for_stage(stage_id=stage_id)
            if terminals:
                continue
            key = (attempt_id, permit_digest)
            if key in dangling_by_permit:
                raise IntegrityError("C6 permit has multiple dangling invocation boundaries")
            dangling_by_permit[key] = invocation_row
        if not dangling_by_permit:
            return ()
        admissions = [
            row
            for row in self._store.event_rows(kind="ATTEMPT_ADMITTED")
            if isinstance(row["payload"].get("context_packet"), dict)
        ]
        for (attempt_id, permit_digest), invocation_row in sorted(
            dangling_by_permit.items()
        ):
            matching_admissions = [
                row
                for row in admissions
                if row["payload"].get("attempt_id") == attempt_id
                and row["payload"].get("permit_digest") == permit_digest
            ]
            if len(matching_admissions) != 1:
                raise IntegrityError("dangling C6 invocation has no exact admission")
            admission = matching_admissions[0]
            delivered, permit = self._rebuild_admitted_context(
                admission=admission["payload"], occurred_at_ns=occurred_at_ns
            )
            invocations_for_permit = [
                row
                for row in invocation_rows
                if row["payload"].get("permit_digest") == permit.digest
            ]
            if len(invocations_for_permit) != 1:
                raise IntegrityError("C6 permit has multiple invocation boundaries")
            stage_id = invocation_row["payload"]["stage_id"]
            stages = [
                row
                for row in self._store.event_rows(kind="CONTEXT_PROMPT_STAGED")
                if row["payload"].get("target_attempt_id")
                == delivered.binding.target_attempt_id
                and row["payload"].get("permit_digest") == permit.digest
                and row["payload"].get("stage_id") == stage_id
            ]
            if len(stages) != 1:
                raise IntegrityError("dangling C6 invocation has no exact staged prompt")
            staged = self._rebuild_staged_prompt(
                delivered=delivered, permit=permit, row=stages[0]
            )
            invocation = self._rebuild_prompt_invocation(
                delivered=delivered,
                permit=permit,
                staged=staged,
                row=invocation_row,
            )
            try:
                receipts.append(
                    self.record_prompt_unknown(
                        delivered=delivered,
                        permit=permit,
                        staged=staged,
                        invocation=invocation,
                        reason="boot_recovery_dangling_prompt_invocation",
                        occurred_at_ns=occurred_at_ns,
                    )
                )
            except IntegrityError:
                # A fenced live host can commit RELEASED between this recovery
                # sweep's initial terminal scan and the blocked SQLite write.
                # Re-read under the current canonical prefix: an exact terminal
                # wins the race and needs no UNKNOWN; anything else remains an
                # integrity failure rather than being silently retried.
                if self._terminal_rows_for_stage(stage_id=stage_id):
                    continue
                raise
        return tuple(sorted(receipts))

    def stage_prompt(
        self,
        *,
        delivered: DeliveredContextPacketV1,
        permit: AttemptPermit,
        prompt: str,
        transport: str,
        occurred_at_ns: int,
    ) -> StagedPromptV1:
        """Seal one exact packet-containing prompt before a CLI may be released.

        This is intentionally *not* called a pre-effect delivery receipt.  It
        proves a host-owned prompt assembly was staged before release; only a later
        release observation says a CLI process was started with that assembly.
        """

        if type(occurred_at_ns) is not int or occurred_at_ns < 0:
            raise ValueError("occurred_at_ns must be a non-negative exact integer")
        # Current direct CLI execution cannot make stdin availability a pre-effect
        # fence.  Keep the pure stage contract general for a future broker, but fail
        # closed in the real C6 authority until such a broker exists.
        if _transport(transport) != "argv":
            raise IntegrityError("strict C6 requires a staged argv transport")
        self._admission_row(delivered=delivered, permit=permit)
        binding = delivered.binding
        assembly = PromptAssemblyV1.materialize(
            packet_digest=binding.packet_digest,
            context_block=delivered.render_for_prompt(),
            full_prompt=prompt,
            transport=transport,
        )
        prompt_bytes = prompt.encode("utf-8")
        sealed_prompt = self._cas.seal_bytes(prompt_bytes)
        if (
            sealed_prompt.digest != assembly.full_prompt_digest
            or sealed_prompt.byte_count != assembly.full_prompt_byte_count
            or self._cas.read_verified(sealed_prompt.digest) != prompt_bytes
        ):
            raise IntegrityError("staged prompt CAS object diverged")
        staged = StagedPromptV1.create(
            attempt_digest=permit.lease.attempt.digest,
            permit_digest=permit.digest,
            assembly=assembly,
        )
        terminal_rows = [
            row
            for kind in (
                "CONTEXT_PROMPT_RELEASED",
                "CONTEXT_PROMPT_UNKNOWN",
                "CONTEXT_PROMPT_PRELAUNCH_ABORTED",
            )
            for row in self._store.event_rows(kind=kind)
            if row["payload"].get("stage_id") == staged.stage_id
        ]
        if terminal_rows:
            raise IntegrityError(
                "an identical staged prompt already reached a terminal state; "
                "automatic redispatch is forbidden"
            )
        payload = {
            "accepted_set_change": ACCEPTED_SET_CHANGE,
            "assembly_digest": assembly.digest,
            "compilation_event_receipt_digest": binding.compilation_event_receipt_digest,
            "compiler_receipt_digest": binding.compiler_receipt_digest,
            "context_block_digest": assembly.context_block_digest,
            "feature_state_digest": binding.feature_state_digest,
            "manifest_digest": binding.manifest_digest,
            "packet_digest": binding.packet_digest,
            "permit_digest": permit.digest,
            "permit_id": permit.permit_id,
            "prompt_artifact_digest": sealed_prompt.digest,
            "prompt_byte_count": sealed_prompt.byte_count,
            "schema_id": COGNITIVE_CONTEXT_VERSION,
            "scope_digest": permit.lease.attempt.scope.digest,
            "stage_id": staged.stage_id,
            "target_attempt_id": binding.target_attempt_id,
            "transport": assembly.transport,
        }
        result = self._store.commit_command(
            command_id=f"context:stage:{staged.stage_id}",
            idempotency_key=f"context:stage:{staged.stage_id}",
            command_payload=payload,
            events=(
                CommandEvent(
                    f"event:context-stage:{staged.stage_id}",
                    "CONTEXT_PROMPT_STAGED",
                    COGNITIVE_CONTEXT_ACTOR,
                    occurred_at_ns,
                    payload,
                ),
            ),
            authority_capability=self._store._cognitive_context_commit_capability,
            required_prior_event=(
                "ATTEMPT_ADMITTED",
                {
                    "attempt_id": binding.target_attempt_id,
                    "permit_digest": permit.digest,
                },
            ),
            committed_at_ns=occurred_at_ns,
        )
        # The receipt is intentionally not embedded in StagedPromptV1: its identity
        # is derived from immutable prompt inputs and can be reconstructed after a
        # restart through the exact stage event.
        if result.idempotent:
            persisted = [
                row for row in self._store.event_rows(kind="CONTEXT_PROMPT_STAGED")
                if row["payload"].get("stage_id") == staged.stage_id
            ]
            if len(persisted) != 1 or persisted[0]["payload"] != payload:
                raise IntegrityError("idempotent prompt stage diverged")
        return staged

    def _stage_row(
        self,
        *,
        delivered: DeliveredContextPacketV1,
        permit: AttemptPermit,
        staged: StagedPromptV1,
    ) -> tuple[dict[str, Any], str]:
        self._admission_row(delivered=delivered, permit=permit)
        if (
            staged.attempt_digest != permit.lease.attempt.digest
            or staged.permit_digest != permit.digest
            or staged.assembly.packet_digest != delivered.binding.packet_digest
        ):
            raise IntegrityError("staged prompt is rebound to another attempt")
        rows = [
            row for row in self._store.event_rows(kind="CONTEXT_PROMPT_STAGED")
            if row["payload"].get("stage_id") == staged.stage_id
        ]
        if len(rows) != 1:
            raise IntegrityError("staged prompt does not resolve uniquely")
        row = rows[0]
        payload = row["payload"]
        expected = {
            "assembly_digest": staged.assembly.digest,
            "context_block_digest": staged.assembly.context_block_digest,
            "feature_state_digest": delivered.binding.feature_state_digest,
            "packet_digest": delivered.binding.packet_digest,
            "permit_digest": permit.digest,
            "prompt_artifact_digest": staged.assembly.full_prompt_digest,
            "prompt_byte_count": staged.assembly.full_prompt_byte_count,
            "stage_id": staged.stage_id,
            "target_attempt_id": delivered.binding.target_attempt_id,
            "transport": staged.assembly.transport,
        }
        if any(payload.get(name) != value for name, value in expected.items()):
            raise IntegrityError("staged prompt event diverged")
        raw = self._cas.read_verified(staged.assembly.full_prompt_digest)
        if (
            len(raw) != staged.assembly.full_prompt_byte_count
            or hashlib.sha256(raw).hexdigest() != staged.assembly.full_prompt_digest
            or raw.decode("utf-8").count(delivered.render_for_prompt()) != 1
        ):
            raise IntegrityError("staged prompt no longer contains the exact ContextPacket")
        return row, self._store.resolve_receipt_for_event(row["event_digest"]).digest

    @staticmethod
    def _assert_worker_launch_matches_permit(
        *, launch: Mapping[str, Any], permit: AttemptPermit
    ) -> None:
        expected = {
            "attempt_digest": permit.lease.attempt.digest,
            "attempt_id": permit.lease.attempt.attempt_id,
            "lease_digest": permit.lease.digest,
            "lease_id": permit.lease.lease_id,
            "permit_digest": permit.digest,
            "permit_id": permit.permit_id,
            "scope_digest": permit.lease.attempt.scope.digest,
        }
        if any(launch.get(name) != value for name, value in expected.items()):
            raise IntegrityError("worker launch does not bind the exact C6 permit")

    def _worker_launch_row(self, *, permit: AttemptPermit) -> dict[str, Any]:
        launches = [
            row
            for row in self._store.event_rows(kind="WORKER_LAUNCH_PREPARED")
            if row["payload"].get("permit_id") == permit.permit_id
        ]
        if len(launches) != 1:
            raise IntegrityError("prompt invocation requires one canonical worker launch")
        launch = launches[0]
        self._assert_worker_launch_matches_permit(
            launch=launch["payload"], permit=permit
        )
        return launch

    def bind_prompt_invocation(
        self,
        *,
        delivered: DeliveredContextPacketV1,
        permit: AttemptPermit,
        staged: StagedPromptV1,
        argv: object,
        occurred_at_ns: int,
        require_fresh: bool = False,
    ) -> PromptInvocationBindingV1:
        """Bind a staged prompt to the exact argv before process creation.

        This is a host-side invocation construction receipt.  It does not claim a
        child parsed the prompt or a provider received it; ``record_prompt_release``
        is only a later local process-start observation.
        """

        if type(occurred_at_ns) is not int or occurred_at_ns < 0:
            raise ValueError("occurred_at_ns must be a non-negative exact integer")
        if type(require_fresh) is not bool:
            raise TypeError("require_fresh must be a built-in bool")
        stage_row, stage_receipt = self._stage_row(
            delivered=delivered, permit=permit, staged=staged
        )
        launch = self._worker_launch_row(permit=permit)
        invocation, argv_bytes = PromptInvocationBindingV1.bind_argv(
            staged=staged, argv=argv
        )
        sealed_argv = self._cas.seal_bytes(argv_bytes)
        if (
            sealed_argv.digest != invocation.argv_artifact_digest
            or sealed_argv.byte_count != invocation.argv_byte_count
            or self._cas.read_verified(sealed_argv.digest) != argv_bytes
        ):
            raise IntegrityError("prompt invocation argv CAS object diverged")
        payload = {
            "accepted_set_change": ACCEPTED_SET_CHANGE,
            "argv_artifact_digest": invocation.argv_artifact_digest,
            "argv_byte_count": invocation.argv_byte_count,
            "assembly_digest": staged.assembly.digest,
            "feature_state_digest": delivered.binding.feature_state_digest,
            "invocation_id": invocation.invocation_id,
            "packet_digest": delivered.binding.packet_digest,
            "permit_digest": permit.digest,
            "prompt_argument_count": invocation.prompt_argument_count,
            "prompt_stage_event_digest": stage_row["event_digest"],
            "prompt_stage_receipt_digest": stage_receipt,
            "schema_id": COGNITIVE_CONTEXT_VERSION,
            "scope_digest": permit.lease.attempt.scope.digest,
            "stage_id": staged.stage_id,
            "target_attempt_id": delivered.binding.target_attempt_id,
            "transport": staged.assembly.transport,
            "worker_launch_event_digest": launch["event_digest"],
        }
        result = self._store.commit_command(
            command_id=f"context:invocation:{invocation.invocation_id}",
            idempotency_key=f"context:invocation:{invocation.invocation_id}",
            command_payload=payload,
            events=(
                CommandEvent(
                    f"event:context-invocation:{invocation.invocation_id}",
                    "CONTEXT_PROMPT_INVOCATION_BOUND",
                    COGNITIVE_CONTEXT_ACTOR,
                    occurred_at_ns,
                    payload,
                ),
            ),
            authority_capability=self._store._cognitive_context_commit_capability,
            required_prior_event=(
                "CONTEXT_PROMPT_STAGED", {"stage_id": staged.stage_id}
            ),
            forbid_prior_events=(
                # A C6 permit has one irrevocable host argv launch boundary.  A
                # later timeout/resume prompt must obtain a new admitted attempt,
                # never create a second hidden provider/CLI effect in this one.
                ("CONTEXT_PROMPT_INVOCATION_BOUND", {"permit_digest": permit.digest}),
                ("CONTEXT_PROMPT_INVOCATION_BOUND", {"stage_id": staged.stage_id}),
                ("CONTEXT_PROMPT_RELEASED", {"stage_id": staged.stage_id}),
                ("CONTEXT_PROMPT_UNKNOWN", {"stage_id": staged.stage_id}),
            ),
            committed_at_ns=occurred_at_ns,
        )
        if result.idempotent:
            persisted = [
                row
                for row in self._store.event_rows(
                    kind="CONTEXT_PROMPT_INVOCATION_BOUND"
                )
                if row["payload"].get("invocation_id") == invocation.invocation_id
            ]
            if len(persisted) != 1 or persisted[0]["payload"] != payload:
                raise IntegrityError("idempotent prompt invocation diverged")
            if require_fresh:
                # Idempotence is valid for reconstruction, not for a fresh host
                # execution request.  A second broker must terminalize the prior
                # boundary as UNKNOWN rather than reuse its argv/Popen effect.
                raise PromptInvocationAlreadyBound(
                    "C6 invocation boundary was already bound"
                )
        return invocation

    def claim_prompt_launch(
        self,
        *,
        delivered: DeliveredContextPacketV1,
        permit: AttemptPermit,
        staged: StagedPromptV1,
        invocation: PromptInvocationBindingV1,
        profile_digest: str,
        launch_material_digest: str,
        occurred_at_ns: int,
    ) -> PromptLaunchClaimV1:
        """Atomically claim the one host-Popen boundary for a live C6 permit.

        The semantic I/O guard rechecks active scope, reservations, exact worker
        launch, terminal absence, and the running attempt projection in the same
        store transaction that appends this event.  A claim is not a release: a
        crash after it remains UNKNOWN until reconciliation proves otherwise.
        """

        if type(occurred_at_ns) is not int or occurred_at_ns < 0:
            raise ValueError("occurred_at_ns must be a non-negative exact integer")
        profile_digest = _digest(profile_digest, "profile_digest")
        launch_material_digest = _digest(
            launch_material_digest, "launch_material_digest"
        )
        launch_event_digest = self._assert_host_launch_live(
            delivered=delivered,
            permit=permit,
            occurred_at_ns=occurred_at_ns,
        )
        stage_row, stage_receipt = self._stage_row(
            delivered=delivered, permit=permit, staged=staged
        )
        invocation_row, invocation_receipt = self._invocation_row(
            delivered=delivered,
            permit=permit,
            staged=staged,
            invocation=invocation,
        )
        claim_id = "claim-" + canonical_digest(
            {
                "invocation_id": invocation.invocation_id,
                "launch_material_digest": launch_material_digest,
                "profile_digest": profile_digest,
                "stage_id": staged.stage_id,
            }
        )[:32]
        claim = PromptLaunchClaimV1(
            staged=staged,
            invocation=invocation,
            claim_id=claim_id,
            launch_material_digest=launch_material_digest,
            profile_digest=profile_digest,
        )
        self._validate_claim_launch_material(claim=claim)
        payload = {
            "accepted_set_change": ACCEPTED_SET_CHANGE,
            "attempt_digest": permit.lease.attempt.digest,
            "attempt_id": permit.lease.attempt.attempt_id,
            "claim_id": claim.claim_id,
            "expires_at_ns": permit.expires_at_ns,
            "feature_state_digest": delivered.binding.feature_state_digest,
            "invocation_id": invocation.invocation_id,
            "launch_material_digest": claim.launch_material_digest,
            "lease_digest": permit.lease.digest,
            "lease_id": permit.lease.lease_id,
            "packet_digest": delivered.binding.packet_digest,
            "permit_digest": permit.digest,
            "permit_id": permit.permit_id,
            "profile_digest": claim.profile_digest,
            "prompt_invocation_event_digest": invocation_row["event_digest"],
            "prompt_invocation_receipt_digest": invocation_receipt,
            "prompt_stage_event_digest": stage_row["event_digest"],
            "prompt_stage_receipt_digest": stage_receipt,
            "schema_id": COGNITIVE_CONTEXT_VERSION,
            "scope_digest": permit.lease.attempt.scope.digest,
            "stage_id": staged.stage_id,
            "target_attempt_id": delivered.binding.target_attempt_id,
            "transport": staged.assembly.transport,
            "worker_launch_event_digest": launch_event_digest,
        }
        guard = {
            "action": "c6_launch",
            "attempt_digest": permit.lease.attempt.digest,
            "attempt_id": permit.lease.attempt.attempt_id,
            "expires_at_ns": permit.expires_at_ns,
            "lease_digest": permit.lease.digest,
            "lease_id": permit.lease.lease_id,
            "permit_digest": permit.digest,
            "permit_id": permit.permit_id,
            "scope_digest": permit.lease.attempt.scope.digest,
            "worker_launch_event_digest": launch_event_digest,
        }
        result = self._store.commit_command(
            command_id=f"context:launch-claim:{claim.claim_id}",
            idempotency_key=f"context:launch-claim:{claim.claim_id}",
            command_payload={"claim": payload, "guard": guard},
            events=(
                CommandEvent(
                    f"event:context-launch-claim:{claim.claim_id}",
                    "CONTEXT_PROMPT_LAUNCH_CLAIMED",
                    COGNITIVE_CONTEXT_ACTOR,
                    occurred_at_ns,
                    payload,
                ),
            ),
            projection_mutations=(ProjectionMutation("attempt_io_guard", guard),),
            authority_capability=self._store._cognitive_context_commit_capability,
            required_prior_event=(
                "CONTEXT_PROMPT_INVOCATION_BOUND",
                {"invocation_id": invocation.invocation_id},
            ),
            forbid_prior_events=(
                ("CONTEXT_PROMPT_LAUNCH_CLAIMED", {"permit_digest": permit.digest}),
                ("CONTEXT_PROMPT_LAUNCH_CLAIMED", {"stage_id": staged.stage_id}),
                ("CONTEXT_PROMPT_RELEASED", {"stage_id": staged.stage_id}),
                ("CONTEXT_PROMPT_UNKNOWN", {"stage_id": staged.stage_id}),
                (
                    "CONTEXT_PROMPT_PRELAUNCH_ABORTED",
                    {"stage_id": staged.stage_id},
                ),
            ),
            committed_at_ns=occurred_at_ns,
        )
        if result.idempotent:
            raise PromptLaunchAlreadyClaimed(
                "C6 host launch claim was already bound"
            )
        return claim

    def _validate_claim_launch_material(
        self, *, claim: PromptLaunchClaimV1
    ) -> dict[str, Any]:
        """Validate the sealed, non-secret Phase-A launch material by receipt.

        The C6 broker seals this body before claiming the irreversible Popen
        boundary.  Re-check it here and during closure so a claim cannot merely
        point at an arbitrary CAS object or silently drift from its argv/profile.
        """

        if type(claim) is not PromptLaunchClaimV1:
            raise TypeError("claim must be PromptLaunchClaimV1")
        raw = self._cas.read_verified(claim.launch_material_digest)
        try:
            body = json.loads(raw)
        except (TypeError, ValueError, json.JSONDecodeError) as exc:
            raise IntegrityError("C6 launch material is not canonical JSON") from exc
        if (
            not isinstance(body, dict)
            or canonical_json_bytes(body) != raw
            or hashlib.sha256(raw).hexdigest() != claim.launch_material_digest
            or set(body)
            != {
                "argv_artifact_digest",
                "cwd_digest",
                "environment",
                "executable_token_digest",
                "profile",
                "profile_digest",
                "schema_id",
            }
            or body.get("schema_id") != _C6_LAUNCH_MATERIAL_SCHEMA_ID
            or body.get("argv_artifact_digest")
            != claim.invocation.argv_artifact_digest
            or body.get("profile_digest") != claim.profile_digest
        ):
            raise IntegrityError("C6 launch material diverges from its claim")
        for name in (
            "argv_artifact_digest",
            "cwd_digest",
            "executable_token_digest",
            "profile_digest",
        ):
            _digest(body.get(name), name)
        profile = body.get("profile")
        if (
            not isinstance(profile, dict)
            or set(profile) != {"backend", "driver_name", "schema_id"}
            or profile.get("backend") != "host_popen"
            or profile.get("schema_id") != _C6_HOST_PROFILE_SCHEMA_ID
            or canonical_digest(profile) != claim.profile_digest
        ):
            raise IntegrityError("C6 host launch profile diverges from its claim")
        _text(profile.get("driver_name"), "profile.driver_name")
        environment = body.get("environment")
        if type(environment) is not list:
            raise IntegrityError("C6 launch material environment is malformed")
        names: list[str] = []
        for item in environment:
            if not isinstance(item, dict) or set(item) != {"name", "value_digest"}:
                raise IntegrityError("C6 launch material environment entry is malformed")
            name = _text(item.get("name"), "environment name")
            if any(
                token in name.upper()
                for token in ("TOKEN", "SECRET", "PASSWORD", "CREDENTIAL", "API_KEY")
            ):
                raise IntegrityError("C6 launch material contains a secret-bearing override")
            _digest(item.get("value_digest"), "environment value_digest")
            names.append(name)
        if names != sorted(names) or len(names) != len(set(names)):
            raise IntegrityError("C6 launch material environment is not canonical")
        return body

    def _claim_row(
        self,
        *,
        delivered: DeliveredContextPacketV1,
        permit: AttemptPermit,
        claim: PromptLaunchClaimV1,
    ) -> tuple[dict[str, Any], str]:
        invocation_row, _invocation_receipt = self._invocation_row(
            delivered=delivered,
            permit=permit,
            staged=claim.staged,
            invocation=claim.invocation,
        )
        rows = [
            row
            for row in self._store.event_rows(kind="CONTEXT_PROMPT_LAUNCH_CLAIMED")
            if row["payload"].get("claim_id") == claim.claim_id
        ]
        if len(rows) != 1:
            raise IntegrityError("C6 host launch claim does not resolve uniquely")
        row = rows[0]
        payload = row["payload"]
        expected = {
            "claim_id": claim.claim_id,
            "expires_at_ns": permit.expires_at_ns,
            "feature_state_digest": delivered.binding.feature_state_digest,
            "invocation_id": claim.invocation.invocation_id,
            "launch_material_digest": claim.launch_material_digest,
            "packet_digest": delivered.binding.packet_digest,
            "permit_digest": permit.digest,
            "permit_id": permit.permit_id,
            "profile_digest": claim.profile_digest,
            "prompt_invocation_event_digest": invocation_row["event_digest"],
            "scope_digest": permit.lease.attempt.scope.digest,
            "stage_id": claim.staged.stage_id,
            "target_attempt_id": delivered.binding.target_attempt_id,
            "transport": "argv",
        }
        if any(payload.get(name) != value for name, value in expected.items()):
            raise IntegrityError("C6 host launch claim lineage diverged")
        if invocation_row["seq"] >= row["seq"]:
            raise IntegrityError("C6 host launch claim ordering diverged")
        self._validate_claim_launch_material(claim=claim)
        return row, self._store.resolve_receipt_for_event(row["event_digest"]).digest

    def _claim_for_stage(
        self,
        *,
        delivered: DeliveredContextPacketV1,
        permit: AttemptPermit,
        staged: StagedPromptV1,
        invocation: PromptInvocationBindingV1,
    ) -> tuple[PromptLaunchClaimV1, dict[str, Any], str]:
        rows = [
            row
            for row in self._store.event_rows(kind="CONTEXT_PROMPT_LAUNCH_CLAIMED")
            if row["payload"].get("stage_id") == staged.stage_id
        ]
        if len(rows) != 1:
            raise IntegrityError("prompt invocation has no unique C6 host launch claim")
        payload = rows[0]["payload"]
        try:
            claim = PromptLaunchClaimV1(
                staged=staged,
                invocation=invocation,
                claim_id=payload.get("claim_id"),
                launch_material_digest=payload.get("launch_material_digest"),
                profile_digest=payload.get("profile_digest"),
            )
        except (TypeError, ValueError) as exc:
            raise IntegrityError("C6 host launch claim cannot be reconstructed") from exc
        row, receipt = self._claim_row(
            delivered=delivered, permit=permit, claim=claim
        )
        return claim, row, receipt

    def _release_receipt_for_stage(
        self,
        *,
        delivered: DeliveredContextPacketV1,
        permit: AttemptPermit,
        staged: StagedPromptV1,
        invocation: PromptInvocationBindingV1,
        release: Mapping[str, Any],
    ) -> str:
        """Verify that one release is exactly downstream of one durable claim."""

        stage_row, stage_receipt = self._stage_row(
            delivered=delivered, permit=permit, staged=staged
        )
        invocation_row, invocation_receipt = self._invocation_row(
            delivered=delivered,
            permit=permit,
            staged=staged,
            invocation=invocation,
        )
        claim, claim_row, claim_receipt = self._claim_for_stage(
            delivered=delivered,
            permit=permit,
            staged=staged,
            invocation=invocation,
        )
        launch = self._worker_launch_row(permit=permit)
        payload = release.get("payload")
        if not isinstance(payload, Mapping):
            raise IntegrityError("C6 prompt release payload is malformed")
        release_seq = release.get("seq")
        if type(release_seq) is not int:
            raise IntegrityError("C6 prompt release sequence is malformed")
        process_id = payload.get("process_id")
        expected_start_observation = canonical_digest(
            {
                "backend": "host_popen",
                "claim_id": claim.claim_id,
                "invocation_id": invocation.invocation_id,
                "launch_material_digest": claim.launch_material_digest,
                "process_id": process_id,
                "stage_id": staged.stage_id,
            }
        )
        expected = {
            "claim_id": claim.claim_id,
            "expires_at_ns": permit.expires_at_ns,
            "feature_state_digest": delivered.binding.feature_state_digest,
            "invocation_id": invocation.invocation_id,
            "launch_material_digest": claim.launch_material_digest,
            "packet_digest": delivered.binding.packet_digest,
            "permit_digest": permit.digest,
            "profile_digest": claim.profile_digest,
            "prompt_launch_claim_event_digest": claim_row["event_digest"],
            "prompt_launch_claim_receipt_digest": claim_receipt,
            "prompt_invocation_event_digest": invocation_row["event_digest"],
            "prompt_invocation_receipt_digest": invocation_receipt,
            "prompt_stage_event_digest": stage_row["event_digest"],
            "prompt_stage_receipt_digest": stage_receipt,
            "scope_digest": permit.lease.attempt.scope.digest,
            "stage_id": staged.stage_id,
            "start_observation_digest": expected_start_observation,
            "target_attempt_id": delivered.binding.target_attempt_id,
            "transport": "argv",
            "transport_backend": "host_popen",
            "worker_launch_event_digest": launch["event_digest"],
        }
        if (
            type(process_id) is not int
            or process_id <= 0
            or any(payload.get(name) != value for name, value in expected.items())
            or launch["seq"] >= stage_row["seq"]
            or stage_row["seq"] >= invocation_row["seq"]
            or invocation_row["seq"] >= claim_row["seq"]
            or claim_row["seq"] >= release_seq
        ):
            raise IntegrityError("C6 prompt release lineage diverged")
        return self._store.resolve_receipt_for_event(release["event_digest"]).digest

    def _prelaunch_abort_receipt_for_stage(
        self,
        *,
        delivered: DeliveredContextPacketV1,
        permit: AttemptPermit,
        staged: StagedPromptV1,
        invocation: PromptInvocationBindingV1,
        aborted: Mapping[str, Any],
    ) -> str:
        """Verify a known-not-started terminal without treating it as release."""

        claim, claim_row, claim_receipt = self._claim_for_stage(
            delivered=delivered,
            permit=permit,
            staged=staged,
            invocation=invocation,
        )
        payload = aborted.get("payload")
        if not isinstance(payload, Mapping):
            raise IntegrityError("C6 prelaunch abort payload is malformed")
        abort_seq = aborted.get("seq")
        if type(abort_seq) is not int:
            raise IntegrityError("C6 prelaunch abort sequence is malformed")
        try:
            _digest(payload.get("reason_digest"), "reason_digest")
        except (TypeError, ValueError) as exc:
            raise IntegrityError("C6 prelaunch abort reason digest is malformed") from exc
        expected = {
            "claim_id": claim.claim_id,
            "expires_at_ns": permit.expires_at_ns,
            "feature_state_digest": delivered.binding.feature_state_digest,
            "invocation_id": invocation.invocation_id,
            "launch_material_digest": claim.launch_material_digest,
            "packet_digest": delivered.binding.packet_digest,
            "permit_digest": permit.digest,
            "profile_digest": claim.profile_digest,
            "prompt_launch_claim_event_digest": claim_row["event_digest"],
            "prompt_launch_claim_receipt_digest": claim_receipt,
            "scope_digest": permit.lease.attempt.scope.digest,
            "stage_id": staged.stage_id,
            "target_attempt_id": delivered.binding.target_attempt_id,
            "transport": "argv",
            "worker_launch_event_digest": claim_row["payload"]["worker_launch_event_digest"],
        }
        if (
            any(payload.get(name) != value for name, value in expected.items())
            or claim_row["seq"] >= abort_seq
        ):
            raise IntegrityError("C6 prelaunch abort lineage diverged")
        return self._store.resolve_receipt_for_event(aborted["event_digest"]).digest

    def _invocation_row(
        self,
        *,
        delivered: DeliveredContextPacketV1,
        permit: AttemptPermit,
        staged: StagedPromptV1,
        invocation: PromptInvocationBindingV1,
    ) -> tuple[dict[str, Any], str]:
        if type(invocation) is not PromptInvocationBindingV1:
            raise TypeError("invocation must be PromptInvocationBindingV1")
        stage_row, _stage_receipt = self._stage_row(
            delivered=delivered, permit=permit, staged=staged
        )
        launch = self._worker_launch_row(permit=permit)
        if invocation.staged != staged:
            raise IntegrityError("prompt invocation belongs to another staged prompt")
        rows = [
            row
            for row in self._store.event_rows(kind="CONTEXT_PROMPT_INVOCATION_BOUND")
            if row["payload"].get("invocation_id") == invocation.invocation_id
        ]
        if len(rows) != 1:
            raise IntegrityError("prompt invocation does not resolve uniquely")
        row = rows[0]
        payload = row["payload"]
        expected = {
            "argv_artifact_digest": invocation.argv_artifact_digest,
            "argv_byte_count": invocation.argv_byte_count,
            "assembly_digest": staged.assembly.digest,
            "feature_state_digest": delivered.binding.feature_state_digest,
            "invocation_id": invocation.invocation_id,
            "packet_digest": delivered.binding.packet_digest,
            "permit_digest": permit.digest,
            "prompt_argument_count": 1,
            "prompt_stage_event_digest": stage_row["event_digest"],
            "stage_id": staged.stage_id,
            "target_attempt_id": delivered.binding.target_attempt_id,
            "transport": "argv",
            "worker_launch_event_digest": launch["event_digest"],
        }
        if any(payload.get(name) != value for name, value in expected.items()):
            raise IntegrityError("prompt invocation event diverged")
        if launch["seq"] >= stage_row["seq"] or stage_row["seq"] >= row["seq"]:
            raise IntegrityError("prompt invocation launch/stage ordering diverged")
        raw = self._cas.read_verified(invocation.argv_artifact_digest)
        if len(raw) != invocation.argv_byte_count:
            raise IntegrityError("prompt invocation argv length diverged")
        try:
            argv = json.loads(raw)
            reconstructed, reconstructed_raw = PromptInvocationBindingV1.bind_argv(
                staged=staged, argv=argv
            )
        except (TypeError, ValueError, json.JSONDecodeError) as exc:
            raise IntegrityError("prompt invocation argv artifact is malformed") from exc
        if reconstructed != invocation or reconstructed_raw != raw:
            raise IntegrityError("prompt invocation argv artifact diverged")
        return row, self._store.resolve_receipt_for_event(row["event_digest"]).digest

    def _record_prompt_release_from_host(
        self,
        *,
        delivered: DeliveredContextPacketV1,
        permit: AttemptPermit,
        staged: StagedPromptV1,
        invocation: PromptInvocationBindingV1,
        claim: PromptLaunchClaimV1,
        process_id: int,
        occurred_at_ns: int,
    ) -> str:
        """Host-only record of a local Popen start after C6 staging.

        This deliberately has a private name.  Generic workers and the retired
        direct stage port cannot manufacture a positive PID into a release receipt;
        the sole production caller is the C6 host broker after its audited Popen
        adapter observes an actual ``subprocess.Popen`` object.
        """

        if type(occurred_at_ns) is not int or occurred_at_ns < 0:
            raise ValueError("occurred_at_ns must be a non-negative exact integer")
        if type(process_id) is not int or process_id <= 0:
            raise ValueError("process_id must be a positive exact integer")
        stage_row, stage_receipt = self._stage_row(
            delivered=delivered, permit=permit, staged=staged
        )
        invocation_row, invocation_receipt = self._invocation_row(
            delivered=delivered,
            permit=permit,
            staged=staged,
            invocation=invocation,
        )
        claim_row, claim_receipt = self._claim_row(
            delivered=delivered, permit=permit, claim=claim
        )
        payload = {
            "accepted_set_change": ACCEPTED_SET_CHANGE,
            "claim_id": claim.claim_id,
            "expires_at_ns": permit.expires_at_ns,
            "feature_state_digest": delivered.binding.feature_state_digest,
            "invocation_id": invocation.invocation_id,
            "launch_material_digest": claim.launch_material_digest,
            "packet_digest": delivered.binding.packet_digest,
            "permit_digest": permit.digest,
            "profile_digest": claim.profile_digest,
            "prompt_launch_claim_event_digest": claim_row["event_digest"],
            "prompt_launch_claim_receipt_digest": claim_receipt,
            "prompt_invocation_event_digest": invocation_row["event_digest"],
            "prompt_invocation_receipt_digest": invocation_receipt,
            "prompt_stage_event_digest": stage_row["event_digest"],
            "prompt_stage_receipt_digest": stage_receipt,
            "process_id": process_id,
            "schema_id": COGNITIVE_CONTEXT_VERSION,
            "start_observation_digest": canonical_digest(
                {
                    "backend": "host_popen",
                    "claim_id": claim.claim_id,
                    "invocation_id": invocation.invocation_id,
                    "launch_material_digest": claim.launch_material_digest,
                    "process_id": process_id,
                    "stage_id": staged.stage_id,
                }
            ),
            "scope_digest": permit.lease.attempt.scope.digest,
            "stage_id": staged.stage_id,
            "target_attempt_id": delivered.binding.target_attempt_id,
            "transport_backend": "host_popen",
            "transport": staged.assembly.transport,
            "worker_launch_event_digest": invocation_row["payload"][
                "worker_launch_event_digest"
            ],
        }
        result = self._store.commit_command(
            command_id=f"context:release:{staged.stage_id}",
            idempotency_key=f"context:release:{staged.stage_id}",
            command_payload=payload,
            events=(
                CommandEvent(
                    f"event:context-release:{staged.stage_id}",
                    "CONTEXT_PROMPT_RELEASED",
                    COGNITIVE_CONTEXT_ACTOR,
                    occurred_at_ns,
                    payload,
                ),
            ),
            authority_capability=self._store._cognitive_context_commit_capability,
            required_prior_event=(
                "CONTEXT_PROMPT_LAUNCH_CLAIMED",
                {"claim_id": claim.claim_id},
            ),
            forbid_prior_events=(
                ("CONTEXT_PROMPT_RELEASED", {"stage_id": staged.stage_id}),
                ("CONTEXT_PROMPT_UNKNOWN", {"stage_id": staged.stage_id}),
                (
                    "CONTEXT_PROMPT_PRELAUNCH_ABORTED",
                    {"stage_id": staged.stage_id},
                ),
            ),
            committed_at_ns=occurred_at_ns,
        )
        return result.receipt_digest

    def record_prompt_prelaunch_aborted(
        self,
        *,
        delivered: DeliveredContextPacketV1,
        permit: AttemptPermit,
        claim: PromptLaunchClaimV1,
        reason: str,
        occurred_at_ns: int,
    ) -> str:
        """Close a claimed boundary proven not to have reached local Popen.

        This is intentionally not a retry token.  The associated permit remains a
        completed/failed attempt; any retry must receive a new admission, budget,
        and claim rather than revive this argv boundary.
        """

        if type(occurred_at_ns) is not int or occurred_at_ns < 0:
            raise ValueError("occurred_at_ns must be a non-negative exact integer")
        claim_row, claim_receipt = self._claim_row(
            delivered=delivered, permit=permit, claim=claim
        )
        payload = {
            "accepted_set_change": ACCEPTED_SET_CHANGE,
            "claim_id": claim.claim_id,
            "expires_at_ns": permit.expires_at_ns,
            "feature_state_digest": delivered.binding.feature_state_digest,
            "invocation_id": claim.invocation.invocation_id,
            "launch_material_digest": claim.launch_material_digest,
            "packet_digest": delivered.binding.packet_digest,
            "permit_digest": permit.digest,
            "profile_digest": claim.profile_digest,
            "prompt_launch_claim_event_digest": claim_row["event_digest"],
            "prompt_launch_claim_receipt_digest": claim_receipt,
            "reason_digest": canonical_digest({"reason": _text(reason, "reason")}),
            "schema_id": COGNITIVE_CONTEXT_VERSION,
            "scope_digest": permit.lease.attempt.scope.digest,
            "stage_id": claim.staged.stage_id,
            "target_attempt_id": delivered.binding.target_attempt_id,
            "transport": claim.staged.assembly.transport,
            "worker_launch_event_digest": claim_row["payload"][
                "worker_launch_event_digest"
            ],
        }
        result = self._store.commit_command(
            command_id=f"context:prelaunch-aborted:{claim.staged.stage_id}",
            idempotency_key=f"context:prelaunch-aborted:{claim.staged.stage_id}",
            command_payload=payload,
            events=(
                CommandEvent(
                    f"event:context-prelaunch-aborted:{claim.staged.stage_id}",
                    "CONTEXT_PROMPT_PRELAUNCH_ABORTED",
                    COGNITIVE_CONTEXT_ACTOR,
                    occurred_at_ns,
                    payload,
                ),
            ),
            authority_capability=self._store._cognitive_context_commit_capability,
            required_prior_event=(
                "CONTEXT_PROMPT_LAUNCH_CLAIMED",
                {"claim_id": claim.claim_id},
            ),
            forbid_prior_events=(
                ("CONTEXT_PROMPT_RELEASED", {"stage_id": claim.staged.stage_id}),
                ("CONTEXT_PROMPT_UNKNOWN", {"stage_id": claim.staged.stage_id}),
                (
                    "CONTEXT_PROMPT_PRELAUNCH_ABORTED",
                    {"stage_id": claim.staged.stage_id},
                ),
            ),
            committed_at_ns=occurred_at_ns,
        )
        return result.receipt_digest

    def record_prompt_unknown(
        self,
        *,
        delivered: DeliveredContextPacketV1,
        permit: AttemptPermit,
        staged: StagedPromptV1,
        invocation: PromptInvocationBindingV1,
        reason: str,
        occurred_at_ns: int,
    ) -> str:
        """Hold a staged prompt whose release state is ambiguous; never retry it."""

        if type(occurred_at_ns) is not int or occurred_at_ns < 0:
            raise ValueError("occurred_at_ns must be a non-negative exact integer")
        _stage_row, stage_receipt = self._stage_row(
            delivered=delivered, permit=permit, staged=staged
        )
        invocation_row, invocation_receipt = self._invocation_row(
            delivered=delivered,
            permit=permit,
            staged=staged,
            invocation=invocation,
        )
        payload = {
            "accepted_set_change": ACCEPTED_SET_CHANGE,
            "feature_state_digest": delivered.binding.feature_state_digest,
            "invocation_id": invocation.invocation_id,
            "packet_digest": delivered.binding.packet_digest,
            "permit_digest": permit.digest,
            "prompt_invocation_event_digest": invocation_row["event_digest"],
            "prompt_invocation_receipt_digest": invocation_receipt,
            "prompt_stage_receipt_digest": stage_receipt,
            "reason_digest": canonical_digest({"reason": _text(reason, "reason")}),
            "schema_id": COGNITIVE_CONTEXT_VERSION,
            "scope_digest": permit.lease.attempt.scope.digest,
            "stage_id": staged.stage_id,
            "target_attempt_id": delivered.binding.target_attempt_id,
            "transport": staged.assembly.transport,
        }
        result = self._store.commit_command(
            command_id=f"context:unknown:{staged.stage_id}",
            idempotency_key=f"context:unknown:{staged.stage_id}",
            command_payload=payload,
            events=(
                CommandEvent(
                    f"event:context-unknown:{staged.stage_id}",
                    "CONTEXT_PROMPT_UNKNOWN",
                    COGNITIVE_CONTEXT_ACTOR,
                    occurred_at_ns,
                    payload,
                ),
            ),
            authority_capability=self._store._cognitive_context_commit_capability,
            required_prior_event=(
                "CONTEXT_PROMPT_INVOCATION_BOUND",
                {"invocation_id": invocation.invocation_id},
            ),
            forbid_prior_events=(
                ("CONTEXT_PROMPT_RELEASED", {"stage_id": staged.stage_id}),
                ("CONTEXT_PROMPT_UNKNOWN", {"stage_id": staged.stage_id}),
                (
                    "CONTEXT_PROMPT_PRELAUNCH_ABORTED",
                    {"stage_id": staged.stage_id},
                ),
            ),
            committed_at_ns=occurred_at_ns,
        )
        return result.receipt_digest

    def require_verified_prompt_stages(
        self,
        *,
        delivered: DeliveredContextPacketV1,
        permit: AttemptPermit,
    ) -> tuple[str, ...]:
        """Independently re-check all prompt stage terminal states from the log."""

        self._admission_row(delivered=delivered, permit=permit)
        stages = [
            row
            for row in self._store.event_rows(kind="CONTEXT_PROMPT_STAGED")
            if row["payload"].get("target_attempt_id")
            == delivered.binding.target_attempt_id
            and row["payload"].get("permit_digest") == permit.digest
        ]
        if not stages:
            raise IntegrityError("context-bound attempt has no staged prompt")
        release_receipts: list[str] = []
        for stage in stages:
            stage_id = stage["payload"].get("stage_id")
            if type(stage_id) is not str or not stage_id:
                raise IntegrityError("staged prompt identity is malformed")
            stage_payload = stage["payload"]
            try:
                assembly = PromptAssemblyV1(
                    packet_digest=delivered.binding.packet_digest,
                    context_block_digest=_digest(
                        stage_payload.get("context_block_digest"),
                        "context_block_digest",
                    ),
                    full_prompt_digest=_digest(
                        stage_payload.get("prompt_artifact_digest"),
                        "prompt_artifact_digest",
                    ),
                    full_prompt_byte_count=stage_payload.get("prompt_byte_count"),
                    transport=stage_payload.get("transport"),
                )
                staged = StagedPromptV1(
                    attempt_digest=permit.lease.attempt.digest,
                    permit_digest=permit.digest,
                    assembly=assembly,
                    stage_id=stage_id,
                )
                _checked_stage, _stage_receipt = self._stage_row(
                    delivered=delivered, permit=permit, staged=staged
                )
            except (TypeError, ValueError) as exc:
                raise IntegrityError("staged prompt cannot be reconstructed") from exc
            invocation_rows = [
                row
                for row in self._store.event_rows(
                    kind="CONTEXT_PROMPT_INVOCATION_BOUND"
                )
                if row["payload"].get("stage_id") == stage_id
            ]
            if len(invocation_rows) != 1:
                raise IntegrityError("staged prompt has no unique argv binding")
            invocation_payload = invocation_rows[0]["payload"]
            try:
                invocation = PromptInvocationBindingV1(
                    staged=staged,
                    argv_artifact_digest=_digest(
                        invocation_payload.get("argv_artifact_digest"),
                        "argv_artifact_digest",
                    ),
                    argv_byte_count=invocation_payload.get("argv_byte_count"),
                    prompt_argument_count=invocation_payload.get(
                        "prompt_argument_count"
                    ),
                    invocation_id=invocation_payload.get("invocation_id"),
                )
                invocation_row, invocation_receipt = self._invocation_row(
                    delivered=delivered,
                    permit=permit,
                    staged=staged,
                    invocation=invocation,
                )
            except (TypeError, ValueError) as exc:
                raise IntegrityError("prompt invocation cannot be reconstructed") from exc
            # Re-read the prompt artifact rather than trusting a worker-side status
            # flag.  ``_stage_row`` already checked it contains the exact packet.
            prompt_digest = staged.assembly.full_prompt_digest
            raw = self._cas.read_verified(prompt_digest)
            block = delivered.render_for_prompt()
            if raw.decode("utf-8").count(block) != 1:
                raise IntegrityError("stage artifact no longer binds the ContextPacket")
            released = [
                row
                for row in self._store.event_rows(kind="CONTEXT_PROMPT_RELEASED")
                if row["payload"].get("stage_id") == stage_id
            ]
            unknown = [
                row
                for row in self._store.event_rows(kind="CONTEXT_PROMPT_UNKNOWN")
                if row["payload"].get("stage_id") == stage_id
            ]
            aborted = [
                row
                for row in self._store.event_rows(
                    kind="CONTEXT_PROMPT_PRELAUNCH_ABORTED"
                )
                if row["payload"].get("stage_id") == stage_id
            ]
            if unknown:
                raise IntegrityError("staged prompt reached UNKNOWN")
            if aborted:
                if len(aborted) != 1 or released:
                    raise IntegrityError("staged prompt has ambiguous terminal records")
                self._prelaunch_abort_receipt_for_stage(
                    delivered=delivered,
                    permit=permit,
                    staged=staged,
                    invocation=invocation,
                    aborted=aborted[0],
                )
                raise IntegrityError("staged prompt was known prelaunch-aborted")
            if len(released) != 1:
                raise IntegrityError("staged prompt is not uniquely released")
            release_receipts.append(
                self._release_receipt_for_stage(
                    delivered=delivered,
                    permit=permit,
                    staged=staged,
                    invocation=invocation,
                    release=released[0],
                )
            )
        return tuple(sorted(release_receipts))

    def verify_scope_prompt_stage_closure(
        self, *, scope_digest: str
    ) -> tuple[str, ...]:
        """Re-check every C6-bound admission before the run can close.

        This resolver intentionally reads canonical events and sealed prompt bytes
        instead of any live worker field.  ``UNKNOWN`` remains fail-closed.  A
        verified ``PRELAUNCH_ABORTED`` is a known-not-started closure receipt, not a
        release and not a promotion signal.
        """

        scope = _digest(scope_digest, "scope_digest")
        admissions = [
            row
            for row in self._store.event_rows(kind="ATTEMPT_ADMITTED")
            if row["payload"].get("scope_digest") == scope
            and isinstance(row["payload"].get("context_packet"), dict)
        ]
        receipts: list[str] = []
        for admission in admissions:
            delivered, permit = self._rebuild_admitted_context(
                admission=admission["payload"], occurred_at_ns=0
            )
            if permit.lease.attempt.scope.digest != scope:
                raise IntegrityError("context-bound admission scope diverged")
            stages = [
                row
                for row in self._store.event_rows(kind="CONTEXT_PROMPT_STAGED")
                if row["payload"].get("target_attempt_id")
                == delivered.binding.target_attempt_id
                and row["payload"].get("permit_digest") == permit.digest
            ]
            if not stages:
                raise IntegrityError("context-bound admission has no staged prompt")
            for stage in stages:
                stage_payload = stage["payload"]
                stage_id = stage_payload.get("stage_id")
                if (
                    type(stage_id) is not str
                    or not stage_id
                    or stage_payload.get("packet_digest")
                    != delivered.binding.packet_digest
                    or stage_payload.get("feature_state_digest")
                    != delivered.binding.feature_state_digest
                    or stage_payload.get("scope_digest") != scope
                ):
                    raise IntegrityError("prompt stage diverged from its admission")
                prompt_digest = _digest(
                    stage_payload.get("prompt_artifact_digest"),
                    "prompt_artifact_digest",
                )
                raw = self._cas.read_verified(prompt_digest)
                if (
                    len(raw) != stage_payload.get("prompt_byte_count")
                    or hashlib.sha256(raw).hexdigest() != prompt_digest
                    or (
                        f"Packet digest: {delivered.binding.packet_digest}"
                    ).encode("utf-8")
                    not in raw
                ):
                    raise IntegrityError("prompt stage artifact is not replayable")
                try:
                    assembly = PromptAssemblyV1(
                        packet_digest=delivered.binding.packet_digest,
                        context_block_digest=_digest(
                            stage_payload.get("context_block_digest"),
                            "context_block_digest",
                        ),
                        full_prompt_digest=prompt_digest,
                        full_prompt_byte_count=stage_payload.get("prompt_byte_count"),
                        transport=stage_payload.get("transport"),
                    )
                    staged = StagedPromptV1(
                        attempt_digest=permit.lease.attempt.digest,
                        permit_digest=permit.digest,
                        assembly=assembly,
                        stage_id=stage_id,
                    )
                except (TypeError, ValueError) as exc:
                    raise IntegrityError("prompt stage identity is not replayable") from exc
                invocations = [
                    row
                    for row in self._store.event_rows(
                        kind="CONTEXT_PROMPT_INVOCATION_BOUND"
                    )
                    if row["payload"].get("stage_id") == stage_id
                ]
                if len(invocations) != 1:
                    raise IntegrityError("prompt stage has no unique argv binding")
                invocation_row = invocations[0]
                invocation_payload = invocation_row["payload"]
                try:
                    invocation = PromptInvocationBindingV1(
                        staged=staged,
                        argv_artifact_digest=_digest(
                            invocation_payload.get("argv_artifact_digest"),
                            "argv_artifact_digest",
                        ),
                        argv_byte_count=invocation_payload.get("argv_byte_count"),
                        prompt_argument_count=invocation_payload.get(
                            "prompt_argument_count"
                        ),
                        invocation_id=invocation_payload.get("invocation_id"),
                    )
                    self._invocation_row(
                        delivered=delivered,
                        permit=permit,
                        staged=staged,
                        invocation=invocation,
                    )
                except (TypeError, ValueError) as exc:
                    raise IntegrityError("prompt invocation is not replayable") from exc
                released = [
                    row
                    for row in self._store.event_rows(kind="CONTEXT_PROMPT_RELEASED")
                    if row["payload"].get("stage_id") == stage_id
                ]
                unknown = [
                    row
                    for row in self._store.event_rows(kind="CONTEXT_PROMPT_UNKNOWN")
                    if row["payload"].get("stage_id") == stage_id
                ]
                aborted = [
                    row
                    for row in self._store.event_rows(
                        kind="CONTEXT_PROMPT_PRELAUNCH_ABORTED"
                    )
                    if row["payload"].get("stage_id") == stage_id
                ]
                if unknown:
                    raise IntegrityError("prompt stage closure is UNKNOWN")
                if len(released) + len(aborted) != 1:
                    raise IntegrityError("prompt stage closure is missing or ambiguous")
                if released:
                    receipts.append(
                        self._release_receipt_for_stage(
                            delivered=delivered,
                            permit=permit,
                            staged=staged,
                            invocation=invocation,
                            release=released[0],
                        )
                    )
                else:
                    receipts.append(
                        self._prelaunch_abort_receipt_for_stage(
                            delivered=delivered,
                            permit=permit,
                            staged=staged,
                            invocation=invocation,
                            aborted=aborted[0],
                        )
                    )
        return tuple(sorted(receipts))


def context_input_from_runtime(
    *,
    remaining_budget: Mapping[str, int],
    policy_digest: str,
) -> DecisionContextInputV1:
    """Derive a minimal contract from runtime-owned state only.

    A worker's task prose, intent labels, or mutable challenge object can be useful
    *proposal* material, but it is not canonical contract evidence.  C6 therefore
    starts with intentionally generic guidance until H5 supplies receipt-bound
    hypotheses and verified observations.
    """

    return DecisionContextInputV1(
        objective="Execute the frozen run policy within this admitted attempt.",
        decision_need=(
            "Choose one admissible next observation or experiment; do not represent "
            "proposal text as verified evidence."
        ),
        acceptance_boundary=(
            "Only the unchanged hardcoded provenance gate may accept a goal unit; "
            "worker claims and ContextPacket text are never sufficient."
        ),
        non_negotiable_policy=(
            f"policy_digest={_text(policy_digest, 'policy_digest')}",
            "stay within the admitted attempt and reserved budget",
            "treat UNKNOWN as reconciliation hold, never automatic redispatch",
            "do not treat proposal prose as verified evidence",
        ),
        remaining_budget=remaining_budget,
        effect_ambiguity=(
            "CLI/provider effects are separately accounted; a prompt-stage or "
            "transport ambiguity is an UNKNOWN hold, not a successful delivery.",
        ),
    )


__all__ = [
    "COGNITIVE_CONTEXT_ACTOR",
    "COGNITIVE_CONTEXT_VERSION",
    "CognitiveFeatureGateV1",
    "CognitiveContextAuthority",
    "DecisionContextInputV1",
    "DeliveredContextPacketV1",
    "PromptInvocationAlreadyBound",
    "PromptLaunchAlreadyClaimed",
    "PromptLaunchClaimV1",
    "context_input_from_runtime",
]
