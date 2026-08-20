"""Default-off CHECKED -> RESOLVED cognitive verification authority.

The resolver consumes only a losslessly verified canonical receipt prefix.  It
does not accept caller-authored event references, dispatch work, retry UNKNOWN,
write production state, or alter the hardcoded provenance gate.
"""

from __future__ import annotations

from collections.abc import Mapping, Sequence
from dataclasses import dataclass
from typing import Any

from muteki.epistemic.cas import CASIntegrityError, ReceiptCAS
from muteki.epistemic.contracts import (
    EventEnvelopeV2,
    canonical_digest,
    canonical_json_bytes,
)
from muteki.epistemic.receipt_objects import (
    CanonicalCommandReceiptResolverV1,
    ResolvedReceiptFieldV1,
    VerifiedReceiptPrefixV1,
)
from muteki.epistemic.sqlite_store import (
    CommandEvent,
    EpistemicSQLiteStore,
    IntegrityError,
    ProjectionMutation,
)
from muteki.runtime.cognitive_verification_resolution_v1 import (
    ACCEPTED_SET_CHANGE,
    AUTOMATIC_REDISPATCH_PERMITTED,
    BOUNDED_NEGATIVE_WITNESS,
    CANONICAL_VERIFICATION_RESOLUTION_SCHEMA_ID,
    CANONICAL_VERIFICATION_RESOLVER_ACTOR,
    CANONICAL_VERIFICATION_RESOLVER_VERSION,
    COGNITIVE_VERIFICATION_RESOLVED,
    PROVENANCE_GATE_ACCEPTED_SET,
    ResolvedCognitiveFactStatusV1,
    ResolvedCognitiveFactV1,
    canonical_verification_resolution_payload_v1,
)
from muteki.runtime.cognitive_engine_registry_v1 import (
    COGNITIVE_ENGINE_REGISTRY_SCHEMA_ID,
    RunPinnedEngineIdentityV1,
)
from muteki.runtime.cognitive_reproduction_evidence_v1 import (
    COGNITIVE_REPRODUCTION_LAUNCH_WITNESSED,
    COGNITIVE_REPRODUCTION_PRELAUNCH_DECLARED,
    reconstruct_reproduction_witness,
    validate_launch_witness_payload_shape,
    validate_prelaunch_declaration_payload_shape,
)
from muteki.runtime.cognitive_reproduction_witness_v1 import (
    ReproductionWitnessStatusV1,
    assess_cognitive_reproduction_witness,
)
from muteki.runtime.cognitive_verification_authority_v1 import (
    COGNITIVE_VERIFICATION_CHECKED,
    COGNITIVE_VERIFICATION_CHECKER_ACTOR,
    COGNITIVE_VERIFICATION_CHECK_INPUT_COMMITTED,
    COGNITIVE_VERIFICATION_CHECK_OUTPUT_SEALED,
    validate_cognitive_verification_check_input_shape,
    validate_cognitive_verification_check_output_shape,
)
from muteki.runtime.cognitive_verification_certificate_v1 import (
    CanonicalCognitiveEventReferenceV1,
    CanonicalRuntimePayloadOccurrenceV1,
    CognitiveAttemptAccountingRoleV1,
    CognitiveEngineRegistryProvenanceV1,
    CognitiveReproductionWitnessProvenanceV1,
    CognitiveVerificationCertificateStatusV1,
    CognitiveVerificationCertificateV1,
    ResolvedCognitiveAttemptAccountingV1,
    reduce_cognitive_verification_certificate_v1,
)
from muteki.runtime.cognitive_verification_checker_v1 import (
    CognitiveVerificationRelationV1,
    DeterministicCognitiveVerificationCheckV1,
)
from muteki.runtime.hypothesis import (
    ActionClass,
    DiscriminatingExperiment,
    EffectClass,
    ExperimentPrediction,
    SemanticSignature,
)


COGNITIVE_VERIFICATION_RESOLVER_ACTOR = CANONICAL_VERIFICATION_RESOLVER_ACTOR
PRODUCTION_ENABLED = False
AUTHORITY_EFFECT = "RECOMMENDATION_AND_LEARNING_ELIGIBILITY_ONLY"
SYNTHETIC_CONTAINED_FIXTURE_SCOPE = (
    "offline_synthetic_contained_receipt_fixture_only"
)
SYNTHETIC_CONTAINED_FIXTURE_EVENT = (
    "COGNITIVE_VERIFICATION_SYNTHETIC_CONTAINED_FIXTURE_SEALED"
)
SYNTHETIC_CONTAINED_FIXTURE_ACTOR = (
    "cognitive-verification-synthetic-fixture-authority"
)
SYNTHETIC_CONTAINED_FIXTURE_SCHEMA_ID = (
    "muteki.cognitive-verification-synthetic-contained-fixture.v1"
)
RESOLVER_CERTIFICATE_SCHEMA_ID = (
    "muteki.cognitive-verification-resolver-certificate.v1"
)

_RESOLUTION_FIELDS = frozenset(
    {
        "accepted_set_change",
        "automatic_redispatch_permitted",
        "bounded_negative_witness_digest",
        "causal_kernel_digest",
        "certificate_digest",
        "certificate_occurrence_digest",
        "checker_checked_event_digest",
        "learning_eligible",
        "observed_partition_digest",
        "provenance_gate_accepted_set",
        "resolver_version",
        "schema_id",
        "source_assignment_event_digest",
        "source_experiment_digest",
        "source_observation_event_digest",
        "status",
        "world_epoch_digest",
    }
)


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


def _event_from_resolved_field(value: ResolvedReceiptFieldV1) -> EventEnvelopeV2:
    if type(value) is not ResolvedReceiptFieldV1:
        raise TypeError("resolved event must be ResolvedReceiptFieldV1")
    pointer = value.pointer
    if pointer.event_ordinal is None or pointer.field_path != (
        f"events[{pointer.event_ordinal}]"
    ):
        raise ValueError("resolver requires one whole canonical event")
    body = value.value
    expected = {
        "actor",
        "command_id",
        "event_id",
        "kind",
        "occurred_at_ns",
        "ordinal",
        "parent_event_digest",
        "payload",
        "run_id",
        "schema_version",
    }
    if not isinstance(body, Mapping) or set(body) != expected:
        raise ValueError("resolved event envelope shape is not canonical")
    event = EventEnvelopeV2(
        event_id=body["event_id"],
        run_id=body["run_id"],
        command_id=body["command_id"],
        ordinal=body["ordinal"],
        kind=body["kind"],
        actor=body["actor"],
        occurred_at_ns=body["occurred_at_ns"],
        payload=body["payload"],
        parent_event_digest=body["parent_event_digest"],
        schema_version=body["schema_version"],
    )
    if (
        canonical_json_bytes(event.canonical_body()) != canonical_json_bytes(body)
        or event.run_id != pointer.run_id
        or event.command_id != pointer.command_id
        or event.ordinal != pointer.event_ordinal
        or event.digest != pointer.event_digest
        or canonical_digest(body) != pointer.value_digest
    ):
        raise ValueError("resolved event was rebound from its receipt pointer")
    return event


@dataclass(frozen=True, slots=True)
class _CanonicalOccurrenceV1:
    event: EventEnvelopeV2
    resolved: ResolvedReceiptFieldV1
    seq: int

    @property
    def receipt_digest(self) -> str:
        return self.resolved.pointer.receipt_digest

    @property
    def payload_digest(self) -> str:
        return canonical_digest(self.event.payload)


class _VerifiedEventInventoryV1:
    def __init__(
        self,
        *,
        prefix: VerifiedReceiptPrefixV1,
        resolver: CanonicalCommandReceiptResolverV1,
    ) -> None:
        if type(prefix) is not VerifiedReceiptPrefixV1:
            raise TypeError("prefix must be VerifiedReceiptPrefixV1")
        if type(resolver) is not CanonicalCommandReceiptResolverV1:
            raise TypeError("resolver must be CanonicalCommandReceiptResolverV1")
        if (
            prefix.run_id != resolver.index.run_id
            or prefix.cutoff_seq != resolver.index.complete_through_seq
        ):
            raise ValueError("resolver and verified prefix diverged")
        self.prefix = prefix
        self.resolver = resolver
        self._cache: dict[str, _CanonicalOccurrenceV1] = {}
        self._references = {item.event_digest: item for item in prefix.events}
        self._entries = {
            item.receipt_digest: item for item in resolver.index.entries
        }

    def occurrence(self, event_digest: str) -> _CanonicalOccurrenceV1:
        digest = _digest(event_digest, "event_digest")
        cached = self._cache.get(digest)
        if cached is not None:
            return cached
        reference = self._references.get(digest)
        if reference is None:
            raise IntegrityError("event is outside the verified receipt prefix")
        entry = self._entries.get(reference.receipt_digest)
        if entry is None:
            raise IntegrityError("event receipt is outside the complete index")
        ordinal = reference.seq - entry.first_seq
        pointer = self.resolver.pointer_for(
            reference.receipt_digest,
            f"events[{ordinal}]",
            cutoff_seq=self.prefix.cutoff_seq,
        )
        resolved = self.resolver.resolve(
            pointer,
            cutoff_seq=self.prefix.cutoff_seq,
        )
        event = _event_from_resolved_field(resolved)
        if (
            resolved.event_global_seq != reference.seq
            or event.kind != reference.kind
            or event.digest != reference.event_digest
            or canonical_digest(event.payload) != reference.payload_digest
            or pointer.receipt_digest != reference.receipt_digest
        ):
            raise IntegrityError("event differs from its verified prefix inventory")
        result = _CanonicalOccurrenceV1(
            event=event,
            resolved=resolved,
            seq=reference.seq,
        )
        self._cache[digest] = result
        return result

    def by_kind(self, kind: str) -> tuple[_CanonicalOccurrenceV1, ...]:
        return tuple(
            self.occurrence(item.event_digest)
            for item in self.prefix.events
            if item.kind == kind
        )

    def one_by_payload_digest(
        self, *, kind: str, payload_digest: str
    ) -> _CanonicalOccurrenceV1:
        digest = _digest(payload_digest, "payload_digest")
        matches = tuple(
            self.occurrence(item.event_digest)
            for item in self.prefix.events
            if item.kind == kind and item.payload_digest == digest
        )
        if len(matches) != 1:
            raise IntegrityError(f"{kind} payload occurrence is absent or ambiguous")
        return matches[0]


def _parse_experiment(body: Mapping[str, Any]) -> DiscriminatingExperiment:
    if not isinstance(body, Mapping):
        raise TypeError("source experiment body must be a mapping")
    signature_body = body.get("semantic_signature")
    if not isinstance(signature_body, Mapping):
        raise TypeError("source experiment semantic signature is absent")
    signature = SemanticSignature(
        action_class=ActionClass(signature_body["action_class"]),
        tool_capability_digest=signature_body["tool_capability_digest"],
        resource_digest=signature_body["resource_digest"],
        parameter_region_digest=signature_body["parameter_region_digest"],
        precondition_set_digest=signature_body["precondition_set_digest"],
        read_set_digest=signature_body["read_set_digest"],
        world_epoch_digest=signature_body["world_epoch_digest"],
        tool_policy_digest=signature_body["tool_policy_digest"],
        model_policy_digest=signature_body["model_policy_digest"],
        prediction_partition_digests=tuple(
            signature_body["prediction_partition_digests"]
        ),
        stop_condition_digests=tuple(signature_body["stop_condition_digests"]),
        effect_class=EffectClass(signature_body["effect_class"]),
        canonicalizer_version=signature_body["canonicalizer_version"],
    )
    predictions = tuple(
        ExperimentPrediction(
            hypothesis_digest=item["hypothesis_digest"],
            predicate_digest=item["predicate_digest"],
            outcome_partition_digest=item["outcome_partition_digest"],
        )
        for item in body["predictions"]
    )
    result = DiscriminatingExperiment(
        experiment_id=body["experiment_id"],
        version=body["version"],
        context_packet_digest=body["context_packet_digest"],
        scope_digest=body["scope_digest"],
        semantic_signature=signature,
        hypothesis_digests=tuple(body["hypothesis_digests"]),
        predictions=predictions,
        estimated_cost_units=body["estimated_cost_units"],
    )
    if canonical_json_bytes(result.canonical_body()) != canonical_json_bytes(body):
        raise ValueError("source experiment is not a canonical typed replay")
    return result


def _parse_engine_identity(body: Mapping[str, Any]) -> RunPinnedEngineIdentityV1:
    expected = {
        "actual_launch_profile_digest",
        "driver_build_digest",
        "driver_executable_token_digest",
        "driver_implementation_digest",
        "engine_identity_digest",
        "launch_material_digest",
        "model_identity_digest",
        "provider_attestation_receipt_digest",
        "provider_identity_digest",
        "run_scope_digest",
        "schema_id",
        "session_digest",
        "source_group_digest",
        "source_identity_fingerprint_digest",
        "tool_policy_digest",
        "workspace_digest",
    }
    if not isinstance(body, Mapping) or set(body) != expected:
        raise ValueError("engine registry entry is not versioned")
    if body["schema_id"] != COGNITIVE_ENGINE_REGISTRY_SCHEMA_ID:
        raise ValueError("engine registry entry schema is unsupported")
    result = RunPinnedEngineIdentityV1(
        run_scope_digest=body["run_scope_digest"],
        launch_material_digest=body["launch_material_digest"],
        actual_launch_profile_digest=body["actual_launch_profile_digest"],
        driver_implementation_digest=body["driver_implementation_digest"],
        driver_build_digest=body["driver_build_digest"],
        driver_executable_token_digest=body["driver_executable_token_digest"],
        engine_identity_digest=body["engine_identity_digest"],
        provider_identity_digest=body["provider_identity_digest"],
        model_identity_digest=body["model_identity_digest"],
        tool_policy_digest=body["tool_policy_digest"],
        session_digest=body["session_digest"],
        workspace_digest=body["workspace_digest"],
        source_group_digest=body["source_group_digest"],
        provider_attestation_receipt_digest=body[
            "provider_attestation_receipt_digest"
        ],
    )
    if canonical_json_bytes(result.canonical_body()) != canonical_json_bytes(body):
        raise ValueError("engine registry entry is not a canonical replay")
    return result


def _event_reference(
    occurrence: _CanonicalOccurrenceV1,
    *,
    run_scope_digest: str,
    attempt_id: str,
) -> CanonicalCognitiveEventReferenceV1:
    return CanonicalCognitiveEventReferenceV1(
        run_id=occurrence.event.run_id,
        run_scope_digest=run_scope_digest,
        seq=occurrence.seq,
        event_kind=occurrence.event.kind,
        actor=occurrence.event.actor,
        event_digest=occurrence.event.digest,
        command_receipt_digest=occurrence.receipt_digest,
        payload_digest=occurrence.payload_digest,
        attempt_id=attempt_id,
    )


def _runtime_occurrence(
    occurrence: _CanonicalOccurrenceV1,
    *,
    run_scope_digest: str,
    attempt_id: str,
) -> CanonicalRuntimePayloadOccurrenceV1:
    return CanonicalRuntimePayloadOccurrenceV1(
        reference=_event_reference(
            occurrence,
            run_scope_digest=run_scope_digest,
            attempt_id=attempt_id,
        ),
        payload=occurrence.event.payload,
    )


def _attempt_events(
    inventory: _VerifiedEventInventoryV1,
    *,
    attempt_id: str,
    kinds: tuple[str, ...],
) -> tuple[_CanonicalOccurrenceV1, ...]:
    return tuple(
        item
        for kind in kinds
        for item in inventory.by_kind(kind)
        if item.event.payload.get("attempt_id") == attempt_id
    )


def _accounting(
    inventory: _VerifiedEventInventoryV1,
    *,
    role: CognitiveAttemptAccountingRoleV1,
    attempt_id: str,
    run_scope_digest: str,
) -> ResolvedCognitiveAttemptAccountingV1:
    admissions = _attempt_events(
        inventory,
        attempt_id=attempt_id,
        kinds=("ATTEMPT_ADMITTED",),
    )
    terminals = _attempt_events(
        inventory,
        attempt_id=attempt_id,
        kinds=("WORKER_TERMINAL", "WORKER_UNKNOWN"),
    )
    budgets = _attempt_events(
        inventory,
        attempt_id=attempt_id,
        kinds=("BUDGET_SETTLED", "BUDGET_USAGE_UNKNOWN"),
    )
    if len(admissions) != 1 or len(terminals) != 1 or len(budgets) > 1:
        raise IntegrityError(
            f"{role.value} accounting occurrence is absent or ambiguous"
        )
    return ResolvedCognitiveAttemptAccountingV1(
        role=role,
        admission=_runtime_occurrence(
            admissions[0],
            run_scope_digest=run_scope_digest,
            attempt_id=attempt_id,
        ),
        terminal=_runtime_occurrence(
            terminals[0],
            run_scope_digest=run_scope_digest,
            attempt_id=attempt_id,
        ),
        budget_occurrences=tuple(
            _runtime_occurrence(
                item,
                run_scope_digest=run_scope_digest,
                attempt_id=attempt_id,
            )
            for item in budgets
        ),
    )


@dataclass(frozen=True, slots=True)
class CognitiveVerificationResolutionAssessmentV1:
    """Replay result over a complete prefix; only the authority may append it."""

    prefix: VerifiedReceiptPrefixV1
    source_experiment: DiscriminatingExperiment
    status: ResolvedCognitiveFactStatusV1
    reason_codes: tuple[str, ...]
    certificate_digest: str
    certificate_occurrence_digest: str
    source_assignment_event_digest: str
    source_observation_event_digest: str
    checker_checked_event_digest: str
    causal_kernel_digest: str
    observed_partition_digest: str | None
    certificate: CognitiveVerificationCertificateV1 | None = None
    synthetic_fixture_only: bool = False

    def __post_init__(self) -> None:
        if type(self.prefix) is not VerifiedReceiptPrefixV1:
            raise TypeError("prefix must be VerifiedReceiptPrefixV1")
        if type(self.source_experiment) is not DiscriminatingExperiment:
            raise TypeError("source_experiment must be DiscriminatingExperiment")
        if type(self.status) is not ResolvedCognitiveFactStatusV1:
            raise TypeError("status must be ResolvedCognitiveFactStatusV1")
        if (
            type(self.reason_codes) is not tuple
            or not self.reason_codes
            or self.reason_codes != tuple(sorted(set(self.reason_codes)))
        ):
            raise ValueError("reason_codes must be a canonical non-empty tuple")
        for name in (
            "certificate_digest",
            "certificate_occurrence_digest",
            "source_assignment_event_digest",
            "source_observation_event_digest",
            "checker_checked_event_digest",
            "causal_kernel_digest",
        ):
            _digest(getattr(self, name), name)
        if self.observed_partition_digest is not None:
            _digest(
                self.observed_partition_digest,
                "observed_partition_digest",
            )
        if (
            self.status is ResolvedCognitiveFactStatusV1.VERIFIED_SUPPORTED
            and self.observed_partition_digest is None
        ):
            raise ValueError("VERIFIED_SUPPORTED requires one observed partition")
        if self.certificate is not None and (
            type(self.certificate) is not CognitiveVerificationCertificateV1
            or self.certificate.digest != self.certificate_digest
            or self.certificate.occurrence_digest
            != self.certificate_occurrence_digest
        ):
            raise ValueError("certificate digests diverged from the replay")
        if type(self.synthetic_fixture_only) is not bool:
            raise TypeError("synthetic_fixture_only must be exact bool")

    @property
    def learning_eligible(self) -> bool:
        return self.status is ResolvedCognitiveFactStatusV1.VERIFIED_SUPPORTED

    def resolution_payload(self) -> dict[str, object]:
        payload = canonical_verification_resolution_payload_v1(
            status=self.status,
            certificate_digest=self.certificate_digest,
            certificate_occurrence_digest=self.certificate_occurrence_digest,
            source_assignment_event_digest=(
                self.source_assignment_event_digest
            ),
            source_observation_event_digest=(
                self.source_observation_event_digest
            ),
            checker_checked_event_digest=self.checker_checked_event_digest,
            source_experiment=self.source_experiment,
            causal_kernel_digest=self.causal_kernel_digest,
            observed_partition_digest=self.observed_partition_digest,
        )
        validate_cognitive_verification_resolution_payload_shape(payload)
        return payload


@dataclass(frozen=True, slots=True)
class CognitiveVerificationResolutionRecordV1:
    prefix: VerifiedReceiptPrefixV1
    resolved_event: ResolvedReceiptFieldV1
    source_experiment: DiscriminatingExperiment
    assessment: CognitiveVerificationResolutionAssessmentV1

    def __post_init__(self) -> None:
        if type(self.prefix) is not VerifiedReceiptPrefixV1:
            raise TypeError("prefix must be VerifiedReceiptPrefixV1")
        if type(self.resolved_event) is not ResolvedReceiptFieldV1:
            raise TypeError("resolved_event must be ResolvedReceiptFieldV1")
        if type(self.source_experiment) is not DiscriminatingExperiment:
            raise TypeError("source_experiment must be DiscriminatingExperiment")
        if (
            type(self.assessment)
            is not CognitiveVerificationResolutionAssessmentV1
        ):
            raise TypeError("assessment has the wrong type")
        fact = ResolvedCognitiveFactV1(
            prefix=self.prefix,
            resolved_event=self.resolved_event,
            source_experiment=self.source_experiment,
        )
        if fact.status is not self.assessment.status:
            raise ValueError("resolution record status diverged from its fact")

    @property
    def fact(self) -> ResolvedCognitiveFactV1:
        return ResolvedCognitiveFactV1(
            prefix=self.prefix,
            resolved_event=self.resolved_event,
            source_experiment=self.source_experiment,
        )


def _one(
    values: Sequence[_CanonicalOccurrenceV1], name: str
) -> _CanonicalOccurrenceV1:
    if len(values) != 1:
        raise IntegrityError(f"{name} occurrence is absent or ambiguous")
    return values[0]


def _synthetic_marker(
    inventory: _VerifiedEventInventoryV1,
    *,
    checked_event_digest: str,
    witness_event_digest: str,
) -> _CanonicalOccurrenceV1:
    marker = _one(
        inventory.by_kind(SYNTHETIC_CONTAINED_FIXTURE_EVENT),
        "synthetic containment marker",
    )
    payload = marker.event.payload
    expected = {
        "accepted_set_change",
        "checker_checked_event_digest",
        "fixture_scope",
        "host_popen_containment_claimed",
        "production_enabled",
        "run_id",
        "schema_id",
        "witness_event_digest",
    }
    if (
        marker.event.actor != SYNTHETIC_CONTAINED_FIXTURE_ACTOR
        or not isinstance(payload, Mapping)
        or set(payload) != expected
        or payload["schema_id"] != SYNTHETIC_CONTAINED_FIXTURE_SCHEMA_ID
        or payload["fixture_scope"] != SYNTHETIC_CONTAINED_FIXTURE_SCOPE
        or payload["production_enabled"] is not False
        or payload["accepted_set_change"] is not False
        or payload["host_popen_containment_claimed"] is not False
        or payload["run_id"] != inventory.prefix.run_id
        or payload["checker_checked_event_digest"] != checked_event_digest
        or payload["witness_event_digest"] != witness_event_digest
    ):
        raise IntegrityError("synthetic containment marker is not test-only canonical data")
    if marker.seq <= inventory.occurrence(checked_event_digest).seq:
        raise IntegrityError("synthetic containment marker must follow CHECKED")
    return marker


def _fallback_certificate_digests(
    *,
    prefix: VerifiedReceiptPrefixV1,
    check: DeterministicCognitiveVerificationCheckV1,
    status: ResolvedCognitiveFactStatusV1,
    reason_codes: tuple[str, ...],
    occurrences: Sequence[_CanonicalOccurrenceV1],
    synthetic_fixture_only: bool,
) -> tuple[str, str]:
    ordered = tuple(
        {
            "event_digest": item.event.digest,
            "event_kind": item.event.kind,
            "payload_digest": item.payload_digest,
            "receipt_digest": item.receipt_digest,
            "seq": item.seq,
        }
        for item in sorted(occurrences, key=lambda value: value.seq)
    )
    occurrence_digest = canonical_digest(
        {
            "ordered_occurrences": ordered,
            "prefix_digest": prefix.digest,
            "schema_id": "muteki.cognitive-verification-resolver-occurrences.v1",
        }
    )
    certificate_digest = canonical_digest(
        {
            "accepted_set_change": False,
            "automatic_redispatch_permitted": False,
            "deterministic_check_digest": check.digest,
            "learning_eligible": (
                status is ResolvedCognitiveFactStatusV1.VERIFIED_SUPPORTED
            ),
            "occurrence_digest": occurrence_digest,
            "production_enabled": False,
            "reason_codes": reason_codes,
            "schema_id": RESOLVER_CERTIFICATE_SCHEMA_ID,
            "status": status.value,
            "synthetic_fixture_only": synthetic_fixture_only,
        }
    )
    return certificate_digest, occurrence_digest


def _checker_chain(
    inventory: _VerifiedEventInventoryV1,
    *,
    checked: _CanonicalOccurrenceV1,
    check: DeterministicCognitiveVerificationCheckV1,
    cas: ReceiptCAS,
) -> tuple[
    _CanonicalOccurrenceV1,
    _CanonicalOccurrenceV1,
    str,
    str,
    str,
]:
    outputs = tuple(
        item
        for item in inventory.by_kind(
            COGNITIVE_VERIFICATION_CHECK_OUTPUT_SEALED
        )
        if item.event.payload.get("check_digest") == check.digest
    )
    output = _one(outputs, "sealed checker output")
    output_payload = output.event.payload
    validate_cognitive_verification_check_output_shape(output_payload)
    if (
        output.event.actor != COGNITIVE_VERIFICATION_CHECKER_ACTOR
        or canonical_json_bytes(output_payload["check_body"])
        != canonical_json_bytes(check.canonical_body())
    ):
        raise IntegrityError("CHECKED differs from its sealed checker output")
    try:
        raw = cas.read_verified(output_payload["raw_digest"])
    except (CASIntegrityError, ValueError) as exc:
        raise IntegrityError("sealed checker output CAS is unavailable") from exc
    if (
        len(raw) != output_payload["byte_count"]
        or raw != canonical_json_bytes(check.canonical_body())
    ):
        raise IntegrityError("sealed checker output CAS replay diverged")
    input_event = inventory.occurrence(output_payload["input_event_digest"])
    if input_event.event.kind != COGNITIVE_VERIFICATION_CHECK_INPUT_COMMITTED:
        raise IntegrityError("checker output input event kind diverged")
    input_payload = input_event.event.payload
    validate_cognitive_verification_check_input_shape(input_payload)
    if (
        input_event.event.actor != COGNITIVE_VERIFICATION_CHECKER_ACTOR
        or input_event.receipt_digest
        != output_payload["input_event_receipt_digest"]
        or input_payload["attempt_id"] != output_payload["attempt_id"]
        or input_payload["attempt_digest"] != output_payload["attempt_digest"]
        or input_payload["permit_digest"] != output_payload["permit_digest"]
        or input_payload["scope_digest"] != output_payload["scope_digest"]
    ):
        raise IntegrityError("checker input/output lineage diverged")
    attempt_id = output_payload["attempt_id"]
    launches = _attempt_events(
        inventory,
        attempt_id=attempt_id,
        kinds=("WORKER_LAUNCH_PREPARED",),
    )
    terminals = _attempt_events(
        inventory,
        attempt_id=attempt_id,
        kinds=("WORKER_TERMINAL", "WORKER_UNKNOWN"),
    )
    budgets = _attempt_events(
        inventory,
        attempt_id=attempt_id,
        kinds=("BUDGET_SETTLED", "BUDGET_USAGE_UNKNOWN"),
    )
    launch = _one(launches, "checker launch")
    terminal = _one(terminals, "checker terminal")
    budget = _one(budgets, "checker budget")
    if not (
        input_event.seq
        < launch.seq
        < output.seq
        < terminal.seq
        < budget.seq
        < checked.seq
    ):
        raise IntegrityError("checker lifecycle ordering is not canonical")
    return (
        input_event,
        output,
        attempt_id,
        input_payload["checker_implementation_digest"],
        input_payload["checker_build_digest"],
    )


def _witness_and_provenance(
    inventory: _VerifiedEventInventoryV1,
    *,
    reproduction_assignment: _CanonicalOccurrenceV1,
    reproduction_observation: _CanonicalOccurrenceV1,
    source_assignment: _CanonicalOccurrenceV1,
    source_observation: _CanonicalOccurrenceV1,
    run_scope_digest: str,
    allow_synthetic_contained_fixture: bool,
    checked_event_digest: str,
) -> tuple[
    Any,
    CognitiveReproductionWitnessProvenanceV1,
    _CanonicalOccurrenceV1,
    _CanonicalOccurrenceV1,
    tuple[str, ...],
    bool,
]:
    permit_digest = reproduction_assignment.event.payload.get("permit_digest")
    witnesses = tuple(
        item
        for item in inventory.by_kind(
            COGNITIVE_REPRODUCTION_LAUNCH_WITNESSED
        )
        if item.event.payload.get("permit_digest") == permit_digest
    )
    witness_event = _one(witnesses, "reproduction launch witness")
    payload = witness_event.event.payload
    actual_launch = payload.get("actual_launch")
    if not isinstance(actual_launch, Mapping):
        raise IntegrityError("reproduction launch witness has no actual launch")
    containment = actual_launch.get("input_channel_containment")
    synthetic = containment == "sealed_containment"
    if synthetic:
        if not allow_synthetic_contained_fixture:
            raise IntegrityError(
                "synthetic containment is never accepted by the store resolver"
            )
        _synthetic_marker(
            inventory,
            checked_event_digest=checked_event_digest,
            witness_event_digest=witness_event.event.digest,
        )
    else:
        validate_launch_witness_payload_shape(payload)
    declaration_event = inventory.occurrence(
        payload["declaration_event_digest"]
    )
    if declaration_event.event.kind != COGNITIVE_REPRODUCTION_PRELAUNCH_DECLARED:
        raise IntegrityError("reproduction witness declaration kind diverged")
    declaration_payload = declaration_event.event.payload
    if synthetic:
        if (
            declaration_payload.get("fixture_scope")
            != SYNTHETIC_CONTAINED_FIXTURE_SCOPE
            or payload.get("fixture_scope")
            != SYNTHETIC_CONTAINED_FIXTURE_SCOPE
        ):
            raise IntegrityError("contained witness is not an explicit test fixture")
    else:
        validate_prelaunch_declaration_payload_shape(declaration_payload)
    if (
        witness_event.event.actor != "cognitive-launch-witness-authority"
        or declaration_event.event.actor
        != "cognitive-reproduction-declaration-authority"
        or witness_event.receipt_digest
        != inventory.prefix.events[witness_event.seq - 1].receipt_digest
        or declaration_event.receipt_digest
        != payload["declaration_event_receipt_digest"]
        or canonical_digest(declaration_payload)
        != payload["declaration_payload_digest"]
        or not (
            reproduction_assignment.seq
            < declaration_event.seq
            < witness_event.seq
            < reproduction_observation.seq
        )
    ):
        raise IntegrityError("reproduction witness occurrence lineage diverged")
    try:
        witness = reconstruct_reproduction_witness(
            source_fence_body=declaration_payload["source_fence"],
            declared_launch=declaration_payload["declared_launch"],
            actual_launch=actual_launch,
        )
        assessment = assess_cognitive_reproduction_witness(witness)
    except (KeyError, TypeError, ValueError) as exc:
        raise IntegrityError("reproduction witness does not replay") from exc
    if (
        canonical_json_bytes(witness.canonical_body())
        != canonical_json_bytes(payload["witness_body"])
        or witness.digest != payload["witness_digest"]
        or witness.source_fence.source_assignment_event_digest
        != source_assignment.event.digest
        or witness.source_fence.source_observation_seq != source_observation.seq
    ):
        raise IntegrityError("reproduction witness body or source fence diverged")
    reasons = set(payload.get("policy_reason_codes", ()))
    if containment == "host_popen_uncontained":
        reasons.add("host_popen_witness_not_contained")
    if "external_input_channel_containment_unproven" in reasons:
        reasons.add("external_input_channel_containment_unproven")
    if payload.get("evidence_status") == "held_unknown":
        reasons.add("reproduction_witness_held_unknown")
    if assessment.status is not ReproductionWitnessStatusV1.OUTCOME_BLIND:
        reasons.add("reproduction_witness_not_outcome_blind")
    if synthetic and (
        payload.get("evidence_status") != "preregistered_exact_shadow"
        or tuple(payload.get("policy_reason_codes", ()))
    ):
        raise IntegrityError("synthetic contained witness is not cleanly preregistered")
    provenance = CognitiveReproductionWitnessProvenanceV1(
        prelaunch_declaration=_event_reference(
            declaration_event,
            run_scope_digest=run_scope_digest,
            attempt_id=reproduction_assignment.event.payload["attempt_id"],
        ),
        launcher_actual_witness=_event_reference(
            witness_event,
            run_scope_digest=run_scope_digest,
            attempt_id=reproduction_assignment.event.payload["attempt_id"],
        ),
        witness_digest=witness.digest,
    )
    return (
        witness,
        provenance,
        declaration_event,
        witness_event,
        tuple(sorted(reasons)),
        synthetic,
    )


def _engine_provenance(
    inventory: _VerifiedEventInventoryV1,
    *,
    source_assignment: _CanonicalOccurrenceV1,
    source_observation: _CanonicalOccurrenceV1,
    reproduction_assignment: _CanonicalOccurrenceV1,
    reproduction_observation: _CanonicalOccurrenceV1,
    witness: Any,
    run_scope_digest: str,
) -> tuple[
    RunPinnedEngineIdentityV1,
    RunPinnedEngineIdentityV1,
    CognitiveEngineRegistryProvenanceV1,
    tuple[_CanonicalOccurrenceV1, ...],
]:
    registrations = inventory.by_kind("COGNITIVE_ENGINE_IDENTITY_REGISTERED")
    parsed: list[tuple[_CanonicalOccurrenceV1, RunPinnedEngineIdentityV1]] = []
    for occurrence in registrations:
        if occurrence.event.actor != "cognitive-engine-registry-authority":
            continue
        try:
            parsed.append(
                (occurrence, _parse_engine_identity(occurrence.event.payload))
            )
        except (TypeError, ValueError):
            continue
    source_matches = tuple(
        item
        for item in parsed
        if item[1].run_scope_digest == run_scope_digest
        and item[1].workspace_digest
        == witness.source_fence.source_workspace_identity_digest
        and item[1].session_digest
        == witness.source_fence.source_session_identity_digest
    )
    reproduction_matches = tuple(
        item
        for item in parsed
        if item[1].run_scope_digest == run_scope_digest
        and item[1].workspace_digest == witness.actual_workspace_identity_digest
        and item[1].session_digest == witness.actual_session_identity_digest
        and item[1].launch_material_digest
        == witness.actual_launch_material_digest
        and item[1].actual_launch_profile_digest
        == witness.actual_launch_profile_digest
    )
    if len(source_matches) != 1 or len(reproduction_matches) != 1:
        raise IntegrityError("engine registry occurrences are absent or ambiguous")
    source_event, source_engine = source_matches[0]
    reproduction_event, reproduction_engine = reproduction_matches[0]
    manifests = tuple(
        item
        for item in inventory.by_kind("COGNITIVE_ENGINE_REGISTRY_RUN_FROZEN")
        if item.event.actor == "cognitive-engine-registry-authority"
        and item.event.payload.get("run_scope_digest") == run_scope_digest
    )
    manifest = _one(manifests, "engine registry run manifest")
    if not (
        manifest.seq < source_assignment.seq
        and source_assignment.seq < source_event.seq < source_observation.seq
        and reproduction_assignment.seq
        < reproduction_event.seq
        < reproduction_observation.seq
    ):
        raise IntegrityError("engine registry temporal binding is not canonical")
    provenance = CognitiveEngineRegistryProvenanceV1(
        source_registration=_event_reference(
            source_event,
            run_scope_digest=run_scope_digest,
            attempt_id=source_assignment.event.payload["attempt_id"],
        ),
        reproducer_registration=_event_reference(
            reproduction_event,
            run_scope_digest=run_scope_digest,
            attempt_id=reproduction_assignment.event.payload["attempt_id"],
        ),
        run_manifest_frozen=_event_reference(
            manifest,
            run_scope_digest=run_scope_digest,
            attempt_id="run-manifest",
        ),
    )
    return (
        source_engine,
        reproduction_engine,
        provenance,
        (manifest, source_event, reproduction_event),
    )


def _assess_verified_prefix(
    *,
    prefix: VerifiedReceiptPrefixV1,
    resolver: CanonicalCommandReceiptResolverV1,
    cas: ReceiptCAS,
    checker_checked_event_digest: str,
    allow_synthetic_contained_fixture: bool,
) -> CognitiveVerificationResolutionAssessmentV1:
    inventory = _VerifiedEventInventoryV1(prefix=prefix, resolver=resolver)
    checked = inventory.occurrence(checker_checked_event_digest)
    if (
        checked.event.kind != COGNITIVE_VERIFICATION_CHECKED
        or checked.event.actor != COGNITIVE_VERIFICATION_CHECKER_ACTOR
    ):
        raise IntegrityError("resolver target is not checker-owned CHECKED")
    try:
        check = DeterministicCognitiveVerificationCheckV1.from_canonical(
            checked.event.payload
        )
    except (TypeError, ValueError) as exc:
        raise IntegrityError("CHECKED payload does not replay") from exc

    source_assignment = inventory.one_by_payload_digest(
        kind="COGNITIVE_EXPERIMENT_ASSIGNED",
        payload_digest=check.source_assignment_payload_digest,
    )
    source_observation = inventory.one_by_payload_digest(
        kind="COGNITIVE_EXECUTION_OBSERVED",
        payload_digest=check.source_observation_payload_digest,
    )
    reproduction_assignment = inventory.one_by_payload_digest(
        kind="COGNITIVE_EXPERIMENT_ASSIGNED",
        payload_digest=check.reproduction_assignment_payload_digest,
    )
    reproduction_observation = inventory.one_by_payload_digest(
        kind="COGNITIVE_EXECUTION_OBSERVED",
        payload_digest=check.reproduction_observation_payload_digest,
    )
    source_payload = source_assignment.event.payload
    source_observation_payload = source_observation.event.payload
    reproduction_payload = reproduction_assignment.event.payload
    reproduction_observation_payload = reproduction_observation.event.payload
    source_experiment = _parse_experiment(source_payload["experiment_body"])
    if (
        source_payload.get("experiment_digest") != source_experiment.digest
        or source_observation_payload.get("assignment_event_digest")
        != source_assignment.event.digest
        or reproduction_observation_payload.get("assignment_event_digest")
        != reproduction_assignment.event.digest
        or reproduction_payload.get("source_assignment_event_digest")
        != source_assignment.event.digest
        or reproduction_payload.get("source_observation_event_digest")
        != source_observation.event.digest
    ):
        raise IntegrityError("verification assignment/observation lineage diverged")
    run_scope_digest = _digest(source_payload.get("scope_digest"), "scope_digest")
    if any(
        payload.get("scope_digest") != run_scope_digest
        for payload in (
            source_observation_payload,
            reproduction_payload,
            reproduction_observation_payload,
        )
    ):
        raise IntegrityError("verification occurrences cross run scopes")
    if not (
        source_assignment.seq
        < source_observation.seq
        < reproduction_assignment.seq
        < reproduction_observation.seq
        < checked.seq
    ):
        raise IntegrityError("verification event ordering is not canonical")

    (
        checker_input,
        checker_output,
        checker_attempt_id,
        checker_implementation_digest,
        checker_build_digest,
    ) = _checker_chain(
        inventory,
        checked=checked,
        check=check,
        cas=cas,
    )
    input_payload = checker_input.event.payload
    expected_input_bindings = {
        "source_assignment_event_digest": source_assignment.event.digest,
        "source_assignment_payload_digest": source_assignment.payload_digest,
        "source_observation_event_digest": source_observation.event.digest,
        "source_observation_payload_digest": source_observation.payload_digest,
        "reproduction_assignment_event_digest": (
            reproduction_assignment.event.digest
        ),
        "reproduction_assignment_payload_digest": (
            reproduction_assignment.payload_digest
        ),
        "reproduction_observation_event_digest": (
            reproduction_observation.event.digest
        ),
        "reproduction_observation_payload_digest": (
            reproduction_observation.payload_digest
        ),
    }
    for name, expected in expected_input_bindings.items():
        if input_payload.get(name) != expected:
            raise IntegrityError("checker input canonical lineage diverged")
    for event_name, receipt_name in (
        ("source_assignment", "source_assignment_event_receipt_digest"),
        ("source_observation", "source_observation_event_receipt_digest"),
        (
            "reproduction_assignment",
            "reproduction_assignment_event_receipt_digest",
        ),
        (
            "reproduction_observation",
            "reproduction_observation_event_receipt_digest",
        ),
    ):
        occurrence = locals()[event_name]
        if input_payload[receipt_name] != occurrence.receipt_digest:
            raise IntegrityError("checker input receipt lineage diverged")

    selected: list[_CanonicalOccurrenceV1] = [
        source_assignment,
        source_observation,
        reproduction_assignment,
        reproduction_observation,
        checker_input,
        checker_output,
        checked,
    ]
    held: set[str] = set()
    invalid: set[str] = set()
    ineligible: set[str] = set()
    if check.relation is CognitiveVerificationRelationV1.UNKNOWN:
        held.add("deterministic_checker_unknown")
    elif check.relation is CognitiveVerificationRelationV1.INVALID_SOURCE:
        invalid.add("deterministic_checker_invalid_source")

    witness = None
    witness_provenance = None
    synthetic_fixture_only = False
    try:
        (
            witness,
            witness_provenance,
            declaration_event,
            witness_event,
            witness_reasons,
            synthetic_fixture_only,
        ) = _witness_and_provenance(
            inventory,
            reproduction_assignment=reproduction_assignment,
            reproduction_observation=reproduction_observation,
            source_assignment=source_assignment,
            source_observation=source_observation,
            run_scope_digest=run_scope_digest,
            allow_synthetic_contained_fixture=(
                allow_synthetic_contained_fixture
            ),
            checked_event_digest=checked.event.digest,
        )
        selected.extend((declaration_event, witness_event))
        held.update(witness_reasons)
    except IntegrityError as exc:
        message = str(exc)
        if "synthetic containment is never accepted" in message:
            ineligible.add("synthetic_containment_not_store_authorized")
        elif "absent or ambiguous" in message:
            ineligible.add("reproduction_witness_canonical_provenance_missing")
        else:
            invalid.add("reproduction_witness_canonical_replay_invalid")

    source_accounting = reproduction_accounting = checker_accounting = None
    try:
        source_accounting = _accounting(
            inventory,
            role=CognitiveAttemptAccountingRoleV1.SOURCE,
            attempt_id=source_payload["attempt_id"],
            run_scope_digest=run_scope_digest,
        )
        reproduction_accounting = _accounting(
            inventory,
            role=CognitiveAttemptAccountingRoleV1.REPRODUCER,
            attempt_id=reproduction_payload["attempt_id"],
            run_scope_digest=run_scope_digest,
        )
        checker_accounting = _accounting(
            inventory,
            role=CognitiveAttemptAccountingRoleV1.CHECKER,
            attempt_id=checker_attempt_id,
            run_scope_digest=run_scope_digest,
        )
        for accounting in (
            source_accounting,
            reproduction_accounting,
            checker_accounting,
        ):
            selected.extend(
                (
                    inventory.occurrence(
                        accounting.admission.reference.event_digest
                    ),
                    inventory.occurrence(
                        accounting.terminal.reference.event_digest
                    ),
                    *(
                        inventory.occurrence(item.reference.event_digest)
                        for item in accounting.budget_occurrences
                    ),
                )
            )
    except (IntegrityError, TypeError, ValueError):
        ineligible.add("complete_attempt_accounting_missing")

    certificate: CognitiveVerificationCertificateV1 | None = None
    if (
        witness is not None
        and witness_provenance is not None
        and source_accounting is not None
        and reproduction_accounting is not None
        and checker_accounting is not None
    ):
        try:
            (
                source_engine,
                reproduction_engine,
                engine_provenance,
                engine_occurrences,
            ) = _engine_provenance(
                inventory,
                source_assignment=source_assignment,
                source_observation=source_observation,
                reproduction_assignment=reproduction_assignment,
                reproduction_observation=reproduction_observation,
                witness=witness,
                run_scope_digest=run_scope_digest,
            )
            selected.extend(engine_occurrences)
            certificate = reduce_cognitive_verification_certificate_v1(
                source_assignment=_event_reference(
                    source_assignment,
                    run_scope_digest=run_scope_digest,
                    attempt_id=source_payload["attempt_id"],
                ),
                source_observation=_event_reference(
                    source_observation,
                    run_scope_digest=run_scope_digest,
                    attempt_id=source_payload["attempt_id"],
                ),
                reproduction_assignment=_event_reference(
                    reproduction_assignment,
                    run_scope_digest=run_scope_digest,
                    attempt_id=reproduction_payload["attempt_id"],
                ),
                reproduction_observation=_event_reference(
                    reproduction_observation,
                    run_scope_digest=run_scope_digest,
                    attempt_id=reproduction_payload["attempt_id"],
                ),
                checker_checked=_event_reference(
                    checked,
                    run_scope_digest=run_scope_digest,
                    attempt_id=checker_attempt_id,
                ),
                source_accounting=source_accounting,
                reproduction_accounting=reproduction_accounting,
                checker_accounting=checker_accounting,
                deterministic_check=check,
                reproduction_witness=witness,
                reproduction_witness_provenance=witness_provenance,
                source_engine_identity=source_engine,
                reproducer_engine_identity=reproduction_engine,
                engine_registry_provenance=engine_provenance,
                checker_implementation_digest=checker_implementation_digest,
                checker_build_digest=checker_build_digest,
            )
        except (IntegrityError, TypeError, ValueError):
            ineligible.add("engine_registry_canonical_provenance_missing")
    elif not held and not invalid:
        ineligible.add("verification_certificate_components_incomplete")

    if certificate is not None:
        if certificate.status is CognitiveVerificationCertificateStatusV1.INVALID:
            invalid.update(certificate.reason_codes)
        elif (
            certificate.status
            is CognitiveVerificationCertificateStatusV1.HELD_UNKNOWN
        ):
            held.update(certificate.reason_codes)
        elif (
            certificate.status
            is CognitiveVerificationCertificateStatusV1.DISAGREEMENT
            and not held
            and not invalid
            and not ineligible
        ):
            pass
        elif (
            certificate.status is CognitiveVerificationCertificateStatusV1.SUPPORTED
            and not synthetic_fixture_only
        ):
            # No current production witness schema proves sealed containment.
            ineligible.add("contained_runtime_witness_authority_unavailable")

    if invalid:
        status = ResolvedCognitiveFactStatusV1.INVALID
        reasons = tuple(sorted(invalid | held | ineligible))
    elif held:
        status = ResolvedCognitiveFactStatusV1.HELD_UNKNOWN
        reasons = tuple(sorted(held | ineligible))
    elif ineligible or certificate is None:
        status = ResolvedCognitiveFactStatusV1.INELIGIBLE
        reasons = tuple(sorted(ineligible or {"verification_ineligible"}))
    elif (
        certificate.status
        is CognitiveVerificationCertificateStatusV1.DISAGREEMENT
    ):
        status = ResolvedCognitiveFactStatusV1.VERIFIED_DISAGREEMENT
        reasons = certificate.reason_codes
    elif (
        certificate.status is CognitiveVerificationCertificateStatusV1.SUPPORTED
        and synthetic_fixture_only
    ):
        status = ResolvedCognitiveFactStatusV1.VERIFIED_SUPPORTED
        reasons = certificate.reason_codes
    else:
        status = ResolvedCognitiveFactStatusV1.INELIGIBLE
        reasons = ("verification_certificate_not_learning_eligible",)

    causal_kernel_digest = (
        check.source_reproduction_kernel_digest
        if check.source_reproduction_kernel_digest is not None
        else canonical_digest(
            {
                "schema_id": "muteki.cognitive-unresolved-causal-kernel.v1",
                "source_experiment_digest": source_experiment.digest,
            }
        )
    )
    observed_partition_digest = (
        certificate.source_partition_digest
        if certificate is not None
        and status
        in {
            ResolvedCognitiveFactStatusV1.VERIFIED_SUPPORTED,
            ResolvedCognitiveFactStatusV1.VERIFIED_DISAGREEMENT,
        }
        else None
    )
    if certificate is None:
        certificate_digest, occurrence_digest = _fallback_certificate_digests(
            prefix=prefix,
            check=check,
            status=status,
            reason_codes=reasons,
            occurrences=selected,
            synthetic_fixture_only=synthetic_fixture_only,
        )
    else:
        certificate_digest = certificate.digest
        occurrence_digest = certificate.occurrence_digest
    return CognitiveVerificationResolutionAssessmentV1(
        prefix=prefix,
        source_experiment=source_experiment,
        status=status,
        reason_codes=reasons,
        certificate_digest=certificate_digest,
        certificate_occurrence_digest=occurrence_digest,
        source_assignment_event_digest=source_assignment.event.digest,
        source_observation_event_digest=source_observation.event.digest,
        checker_checked_event_digest=checked.event.digest,
        causal_kernel_digest=causal_kernel_digest,
        observed_partition_digest=observed_partition_digest,
        certificate=certificate,
        synthetic_fixture_only=synthetic_fixture_only,
    )


def validate_cognitive_verification_resolution_payload_shape(
    payload: Mapping[str, Any],
) -> None:
    """Validate the exact read-side schema without trusting its evidence."""

    if not isinstance(payload, Mapping) or set(payload) != _RESOLUTION_FIELDS:
        raise ValueError("cognitive verification resolution is not versioned")
    try:
        status = ResolvedCognitiveFactStatusV1(payload["status"])
    except (TypeError, ValueError) as exc:
        raise ValueError("cognitive verification resolution status is unknown") from exc
    if (
        payload["schema_id"] != CANONICAL_VERIFICATION_RESOLUTION_SCHEMA_ID
        or payload["resolver_version"]
        != CANONICAL_VERIFICATION_RESOLVER_VERSION
        or payload["accepted_set_change"] is not ACCEPTED_SET_CHANGE
        or payload["automatic_redispatch_permitted"]
        is not AUTOMATIC_REDISPATCH_PERMITTED
        or payload["bounded_negative_witness_digest"]
        is not BOUNDED_NEGATIVE_WITNESS
        or payload["provenance_gate_accepted_set"]
        != PROVENANCE_GATE_ACCEPTED_SET
        or payload["learning_eligible"]
        is not (status is ResolvedCognitiveFactStatusV1.VERIFIED_SUPPORTED)
    ):
        raise ValueError("cognitive verification resolution overclaims authority")
    for name in (
        "causal_kernel_digest",
        "certificate_digest",
        "certificate_occurrence_digest",
        "checker_checked_event_digest",
        "source_assignment_event_digest",
        "source_experiment_digest",
        "source_observation_event_digest",
        "world_epoch_digest",
    ):
        _digest(payload[name], name)
    partition = payload["observed_partition_digest"]
    if partition is not None:
        _digest(partition, "observed_partition_digest")
    if (
        status is ResolvedCognitiveFactStatusV1.VERIFIED_SUPPORTED
        and partition is None
    ):
        raise ValueError("VERIFIED_SUPPORTED requires an observed partition")


def validate_cognitive_verification_resolution_against_store(
    store: EpistemicSQLiteStore, payload: Mapping[str, Any]
) -> None:
    """Recompute one resolver event from its immutable predecessor prefix."""

    assess_cognitive_verification_resolution_against_store_v1(store, payload)


def assess_cognitive_verification_resolution_against_store_v1(
    store: EpistemicSQLiteStore, payload: Mapping[str, Any]
) -> CognitiveVerificationResolutionAssessmentV1:
    """Return the receipt-replayed assessment behind one resolver event.

    This is the read-only counterpart of the mutation guard.  It accepts no
    caller-selected occurrence, accounting vector, status, or certificate: the
    exact stored event is located by its checker occurrence, its immutable
    predecessor prefix is re-resolved, and the complete assessment is rebuilt
    from those receipt bytes.  Callers may inspect the result, but it grants no
    store-write, learning, dispatch, retry, scoring, promotion, or gate
    authority.
    """

    if type(store) is not EpistemicSQLiteStore:
        raise TypeError("store must be exactly EpistemicSQLiteStore")
    validate_cognitive_verification_resolution_payload_shape(payload)
    rows = tuple(
        row
        for row in store.event_rows(kind=COGNITIVE_VERIFICATION_RESOLVED)
        if row["payload"].get("checker_checked_event_digest")
        == payload["checker_checked_event_digest"]
    )
    if len(rows) != 1:
        raise IntegrityError("verification resolution occurrence is absent or ambiguous")
    row = rows[0]
    if canonical_json_bytes(row["payload"]) != canonical_json_bytes(payload):
        raise IntegrityError("verification resolution event/mutation payload diverged")
    cutoff_seq = row["seq"] - 1
    resolver = store.receipt_field_resolver(cutoff_seq=cutoff_seq)
    prefix = resolver.verify_complete_through(cutoff_seq)
    assessment = _assess_verified_prefix(
        prefix=prefix,
        resolver=resolver,
        cas=ReceiptCAS(store.path.parent / "receipt-cas"),
        checker_checked_event_digest=payload["checker_checked_event_digest"],
        allow_synthetic_contained_fixture=False,
    )
    if canonical_json_bytes(assessment.resolution_payload()) != canonical_json_bytes(
        payload
    ):
        raise IntegrityError("verification resolution diverged from canonical replay")
    return assessment


def _resolution_payload_from_store_prefix(
    store: EpistemicSQLiteStore, payload: Mapping[str, Any]
) -> Mapping[str, Any]:
    return assess_cognitive_verification_resolution_against_store_v1(
        store, payload
    ).resolution_payload()


def assess_synthetic_contained_receipt_fixture_v1(
    *,
    prefix: VerifiedReceiptPrefixV1,
    resolver: CanonicalCommandReceiptResolverV1,
    cas: ReceiptCAS,
    checker_checked_event_digest: str,
) -> CognitiveVerificationResolutionAssessmentV1:
    """Assess an explicit detached synthetic fixture without store write authority.

    This narrow test seam can demonstrate the positive reducer path.  The store
    mutation guard and the production resolver authority never call it and always
    reject the synthetic containment marker.
    """

    return _assess_verified_prefix(
        prefix=prefix,
        resolver=resolver,
        cas=cas,
        checker_checked_event_digest=_digest(
            checker_checked_event_digest,
            "checker_checked_event_digest",
        ),
        allow_synthetic_contained_fixture=True,
    )


class CognitiveVerificationResolverAuthorityV1:
    """Independent receipt-only resolver; it cannot CHECK, launch, or dispatch."""

    def __init__(self, *, store: EpistemicSQLiteStore) -> None:
        if type(store) is not EpistemicSQLiteStore:
            raise TypeError("store must be exactly EpistemicSQLiteStore")
        self._store = store
        self._cas = ReceiptCAS(store.path.parent / "receipt-cas")

    def _assessment(
        self,
        *,
        checker_checked_event_digest: str,
        cutoff_seq: int,
    ) -> CognitiveVerificationResolutionAssessmentV1:
        resolver = self._store.receipt_field_resolver(cutoff_seq=cutoff_seq)
        prefix = resolver.verify_complete_through(cutoff_seq)
        return _assess_verified_prefix(
            prefix=prefix,
            resolver=resolver,
            cas=self._cas,
            checker_checked_event_digest=checker_checked_event_digest,
            allow_synthetic_contained_fixture=False,
        )

    def _record(
        self,
        *,
        row: Mapping[str, Any],
        assessment: CognitiveVerificationResolutionAssessmentV1,
    ) -> CognitiveVerificationResolutionRecordV1:
        cutoff_seq = row["seq"]
        resolver = self._store.receipt_field_resolver(cutoff_seq=cutoff_seq)
        prefix = resolver.verify_complete_through(cutoff_seq)
        occurrence = _VerifiedEventInventoryV1(
            prefix=prefix,
            resolver=resolver,
        ).occurrence(row["event_digest"])
        if (
            occurrence.event.kind != COGNITIVE_VERIFICATION_RESOLVED
            or occurrence.event.actor != COGNITIVE_VERIFICATION_RESOLVER_ACTOR
            or canonical_json_bytes(occurrence.event.payload)
            != canonical_json_bytes(assessment.resolution_payload())
        ):
            raise IntegrityError("stored verification resolution does not replay")
        return CognitiveVerificationResolutionRecordV1(
            prefix=prefix,
            resolved_event=occurrence.resolved,
            source_experiment=assessment.source_experiment,
            assessment=assessment,
        )

    def resolve_checked(
        self,
        *,
        checker_checked_event_digest: str,
        occurred_at_ns: int,
    ) -> CognitiveVerificationResolutionRecordV1:
        """Resolve one canonical CHECKED occurrence exactly once."""

        checked_digest = _digest(
            checker_checked_event_digest,
            "checker_checked_event_digest",
        )
        occurred = _non_negative_int(occurred_at_ns, "occurred_at_ns")
        existing = tuple(
            row
            for row in self._store.event_rows(
                kind=COGNITIVE_VERIFICATION_RESOLVED
            )
            if row["payload"].get("checker_checked_event_digest")
            == checked_digest
        )
        if len(existing) > 1:
            raise IntegrityError("verification resolution occurrence is ambiguous")
        if existing:
            row = existing[0]
            assessment = self._assessment(
                checker_checked_event_digest=checked_digest,
                cutoff_seq=row["seq"] - 1,
            )
            if canonical_json_bytes(row["payload"]) != canonical_json_bytes(
                assessment.resolution_payload()
            ):
                raise IntegrityError(
                    "stored verification resolution diverged from replay"
                )
            return self._record(row=row, assessment=assessment)

        assessment = self._assessment(
            checker_checked_event_digest=checked_digest,
            cutoff_seq=self._store.state().head_seq,
        )
        payload = assessment.resolution_payload()
        self._store.commit_command(
            command_id=f"cognitive-verification-resolved:{checked_digest}",
            idempotency_key=f"cognitive-verification-resolved:{checked_digest}",
            command_payload=payload,
            events=(
                CommandEvent(
                    event_id=f"event:cognitive-verification-resolved:{checked_digest}",
                    kind=COGNITIVE_VERIFICATION_RESOLVED,
                    actor=COGNITIVE_VERIFICATION_RESOLVER_ACTOR,
                    occurred_at_ns=occurred,
                    payload=payload,
                ),
            ),
            projection_mutations=(
                ProjectionMutation(
                    "cognitive_verification_resolve_guard",
                    payload,
                ),
            ),
            authority_capability=(
                self._store._cognitive_verification_resolver_commit_capability
            ),
            forbid_prior_events=(
                (
                    COGNITIVE_VERIFICATION_RESOLVED,
                    {"checker_checked_event_digest": checked_digest},
                ),
            ),
            committed_at_ns=occurred,
        )
        rows = tuple(
            row
            for row in self._store.event_rows(
                kind=COGNITIVE_VERIFICATION_RESOLVED
            )
            if row["payload"].get("checker_checked_event_digest")
            == checked_digest
        )
        if len(rows) != 1:
            raise IntegrityError("verification resolution commit did not converge")
        row = rows[0]
        replay = self._assessment(
            checker_checked_event_digest=checked_digest,
            cutoff_seq=row["seq"] - 1,
        )
        return self._record(row=row, assessment=replay)


__all__ = [
    "AUTHORITY_EFFECT",
    "COGNITIVE_VERIFICATION_RESOLVED",
    "COGNITIVE_VERIFICATION_RESOLVER_ACTOR",
    "CognitiveVerificationResolutionAssessmentV1",
    "CognitiveVerificationResolutionRecordV1",
    "CognitiveVerificationResolverAuthorityV1",
    "PRODUCTION_ENABLED",
    "assess_cognitive_verification_resolution_against_store_v1",
    "assess_synthetic_contained_receipt_fixture_v1",
    "validate_cognitive_verification_resolution_against_store",
    "validate_cognitive_verification_resolution_payload_shape",
]
