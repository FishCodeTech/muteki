"""Shared canonical contract for resolver-owned cognitive facts.

Runtime owns the event schema and receipt-bound fact DTO.  The runtime canonical
cycle consumes these definitions directly, while compatibility research paths
re-export the same class objects so both sides consume byte-identical facts.
"""

from __future__ import annotations

from collections.abc import Mapping
from dataclasses import dataclass, field
from enum import Enum

from muteki.epistemic.contracts import (
    EventEnvelopeV2,
    canonical_digest,
    canonical_json_bytes,
)
from muteki.epistemic.receipt_objects import (
    ResolvedReceiptFieldV1,
    VerifiedReceiptPrefixV1,
)
from muteki.runtime.hypothesis import DiscriminatingExperiment


CANONICAL_VERIFICATION_RESOLUTION_SCHEMA_ID = (
    "muteki.cognitive-verification-resolution.v1"
)
CANONICAL_VERIFICATION_RESOLVER_VERSION = "muteki.cognitive-verification-resolver.v1"
CANONICAL_VERIFICATION_RESOLVER_ACTOR = "cognitive-verification-resolver-authority"
COGNITIVE_VERIFICATION_RESOLVED = "COGNITIVE_VERIFICATION_RESOLVED"
PRODUCTION_ENABLED = False
PROVENANCE_GATE_ACCEPTED_SET = "UNCHANGED"
ACCEPTED_SET_CHANGE = False
AUTOMATIC_REDISPATCH_PERMITTED = False
BOUNDED_NEGATIVE_WITNESS = None


def _digest(value: object, name: str) -> str:
    if (
        type(value) is not str
        or len(value) != 64
        or any(character not in "0123456789abcdef" for character in value)
    ):
        raise ValueError(f"{name} must be a lowercase sha256 digest")
    return value


class ResolvedCognitiveFactStatusV1(str, Enum):
    VERIFIED_SUPPORTED = "VERIFIED_SUPPORTED"
    VERIFIED_DISAGREEMENT = "VERIFIED_DISAGREEMENT"
    HELD_UNKNOWN = "HELD_UNKNOWN"
    INELIGIBLE = "INELIGIBLE"
    INVALID = "INVALID"


_RESOLUTION_PAYLOAD_FIELDS = frozenset(
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


def canonical_verification_resolution_payload_v1(
    *,
    status: ResolvedCognitiveFactStatusV1,
    certificate_digest: str,
    certificate_occurrence_digest: str,
    source_assignment_event_digest: str,
    source_observation_event_digest: str,
    checker_checked_event_digest: str,
    source_experiment: DiscriminatingExperiment,
    causal_kernel_digest: str,
    observed_partition_digest: str | None,
) -> dict[str, object]:
    """Build the exact resolver output consumed by the read-only cycle."""

    if type(status) is not ResolvedCognitiveFactStatusV1:
        raise TypeError("status must be ResolvedCognitiveFactStatusV1")
    if type(source_experiment) is not DiscriminatingExperiment:
        raise TypeError("source_experiment must be DiscriminatingExperiment")
    for name, value in (
        ("certificate_digest", certificate_digest),
        ("certificate_occurrence_digest", certificate_occurrence_digest),
        ("source_assignment_event_digest", source_assignment_event_digest),
        ("source_observation_event_digest", source_observation_event_digest),
        ("checker_checked_event_digest", checker_checked_event_digest),
        ("causal_kernel_digest", causal_kernel_digest),
    ):
        _digest(value, name)
    if observed_partition_digest is not None:
        _digest(observed_partition_digest, "observed_partition_digest")
    learning_eligible = status is ResolvedCognitiveFactStatusV1.VERIFIED_SUPPORTED
    if learning_eligible and observed_partition_digest is None:
        raise ValueError("VERIFIED_SUPPORTED requires an observed partition")
    return {
        "accepted_set_change": ACCEPTED_SET_CHANGE,
        "automatic_redispatch_permitted": AUTOMATIC_REDISPATCH_PERMITTED,
        "bounded_negative_witness_digest": BOUNDED_NEGATIVE_WITNESS,
        "causal_kernel_digest": causal_kernel_digest,
        "certificate_digest": certificate_digest,
        "certificate_occurrence_digest": certificate_occurrence_digest,
        "checker_checked_event_digest": checker_checked_event_digest,
        "learning_eligible": learning_eligible,
        "observed_partition_digest": observed_partition_digest,
        "provenance_gate_accepted_set": PROVENANCE_GATE_ACCEPTED_SET,
        "resolver_version": CANONICAL_VERIFICATION_RESOLVER_VERSION,
        "schema_id": CANONICAL_VERIFICATION_RESOLUTION_SCHEMA_ID,
        "source_assignment_event_digest": source_assignment_event_digest,
        "source_experiment_digest": source_experiment.digest,
        "source_observation_event_digest": source_observation_event_digest,
        "status": status.value,
        "world_epoch_digest": (source_experiment.semantic_signature.world_epoch_digest),
    }


def _event_from_resolved_field(value: ResolvedReceiptFieldV1) -> EventEnvelopeV2:
    if type(value) is not ResolvedReceiptFieldV1:
        raise TypeError("resolved_event must be ResolvedReceiptFieldV1")
    pointer = value.pointer
    if pointer.event_ordinal is None or pointer.field_path != (
        f"events[{pointer.event_ordinal}]"
    ):
        raise ValueError("resolution must bind one whole canonical event envelope")
    body = value.value
    if not isinstance(body, Mapping):
        raise TypeError("resolved event must be a canonical mapping")
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
    if set(body) != expected:
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
    if canonical_json_bytes(event.canonical_body()) != canonical_json_bytes(body):
        raise ValueError("resolved event does not replay to its canonical envelope")
    if (
        event.ordinal != pointer.event_ordinal
        or event.command_id != pointer.command_id
        or event.run_id != pointer.run_id
        or event.digest != pointer.event_digest
        or canonical_digest(body) != pointer.value_digest
    ):
        raise ValueError("resolved event is rebound from its receipt pointer")
    return event


@dataclass(frozen=True, slots=True)
class ResolvedCognitiveFactV1:
    """One resolver-owned fact at a losslessly verified receipt prefix."""

    prefix: VerifiedReceiptPrefixV1
    resolved_event: ResolvedReceiptFieldV1
    source_experiment: DiscriminatingExperiment
    status: ResolvedCognitiveFactStatusV1 = field(init=False)
    causal_kernel_digest: str = field(init=False)
    observed_partition_digest: str | None = field(init=False)
    certificate_digest: str = field(init=False)
    certificate_occurrence_digest: str = field(init=False)
    source_assignment_event_digest: str = field(init=False)

    def __post_init__(self) -> None:
        if type(self.prefix) is not VerifiedReceiptPrefixV1:
            raise TypeError("prefix must be VerifiedReceiptPrefixV1")
        if type(self.source_experiment) is not DiscriminatingExperiment:
            raise TypeError("source_experiment must be DiscriminatingExperiment")
        event = _event_from_resolved_field(self.resolved_event)
        pointer = self.resolved_event.pointer
        if event.kind != COGNITIVE_VERIFICATION_RESOLVED:
            raise ValueError("resolved event is not COGNITIVE_VERIFICATION_RESOLVED")
        if event.actor != CANONICAL_VERIFICATION_RESOLVER_ACTOR:
            raise ValueError("resolved event is not resolver-owned")
        if (
            self.prefix.run_id != event.run_id
            or pointer.run_id != self.prefix.run_id
            or self.resolved_event.event_kind != event.kind
            or self.resolved_event.event_global_seq is None
            or self.resolved_event.event_global_seq > self.prefix.cutoff_seq
            or not (
                self.resolved_event.command_first_seq
                <= self.resolved_event.event_global_seq
                <= self.resolved_event.command_last_seq
            )
        ):
            raise ValueError("resolved event is outside its verified prefix")
        matches = tuple(
            item
            for item in self.prefix.events
            if item.seq == self.resolved_event.event_global_seq
        )
        if len(matches) != 1:
            raise ValueError("verified prefix does not contain one resolution event")
        reference = matches[0]
        if (
            reference.event_digest != event.digest
            or reference.receipt_digest != pointer.receipt_digest
            or reference.kind != event.kind
            or reference.payload_digest != canonical_digest(event.payload)
        ):
            raise ValueError("resolution event differs from the verified prefix")

        payload = event.payload
        if (
            not isinstance(payload, Mapping)
            or set(payload) != _RESOLUTION_PAYLOAD_FIELDS
        ):
            raise ValueError("resolution payload shape is not versioned")
        try:
            status = ResolvedCognitiveFactStatusV1(payload["status"])
        except (TypeError, ValueError) as error:
            raise ValueError("resolution status is unknown") from error
        expected_payload = canonical_verification_resolution_payload_v1(
            status=status,
            certificate_digest=payload["certificate_digest"],
            certificate_occurrence_digest=payload["certificate_occurrence_digest"],
            source_assignment_event_digest=payload["source_assignment_event_digest"],
            source_observation_event_digest=payload["source_observation_event_digest"],
            checker_checked_event_digest=payload["checker_checked_event_digest"],
            source_experiment=self.source_experiment,
            causal_kernel_digest=payload["causal_kernel_digest"],
            observed_partition_digest=payload["observed_partition_digest"],
        )
        if canonical_json_bytes(expected_payload) != canonical_json_bytes(payload):
            raise ValueError("resolution payload is not a replay of typed facts")
        object.__setattr__(self, "status", status)
        object.__setattr__(
            self,
            "causal_kernel_digest",
            _digest(payload["causal_kernel_digest"], "causal_kernel_digest"),
        )
        object.__setattr__(
            self,
            "observed_partition_digest",
            payload["observed_partition_digest"],
        )
        for name in (
            "certificate_digest",
            "certificate_occurrence_digest",
            "source_assignment_event_digest",
        ):
            object.__setattr__(self, name, _digest(payload[name], name))

    @property
    def event(self) -> EventEnvelopeV2:
        return _event_from_resolved_field(self.resolved_event)

    @property
    def seq(self) -> int:
        assert self.resolved_event.event_global_seq is not None
        return self.resolved_event.event_global_seq

    @property
    def verification_occurrence_digest(self) -> str:
        return canonical_digest(
            {
                "certificate_occurrence_digest": self.certificate_occurrence_digest,
                "resolution_event_digest": self.event.digest,
                "resolution_receipt_digest": (
                    self.resolved_event.pointer.receipt_digest
                ),
                "schema_id": "muteki.cognitive-resolved-fact-occurrence.v1",
            }
        )

    @property
    def learning_eligible(self) -> bool:
        return self.status is ResolvedCognitiveFactStatusV1.VERIFIED_SUPPORTED

    def canonical_body(self) -> dict[str, object]:
        return {
            "certificate_digest": self.certificate_digest,
            "prefix_digest": self.prefix.digest,
            "resolution_event_digest": self.event.digest,
            "resolution_receipt_digest": self.resolved_event.pointer.receipt_digest,
            "source_experiment_digest": self.source_experiment.digest,
            "status": self.status.value,
            "verification_occurrence_digest": self.verification_occurrence_digest,
        }

    @property
    def digest(self) -> str:
        return canonical_digest(self.canonical_body())


__all__ = [
    "ACCEPTED_SET_CHANGE",
    "AUTOMATIC_REDISPATCH_PERMITTED",
    "BOUNDED_NEGATIVE_WITNESS",
    "CANONICAL_VERIFICATION_RESOLUTION_SCHEMA_ID",
    "CANONICAL_VERIFICATION_RESOLVER_ACTOR",
    "CANONICAL_VERIFICATION_RESOLVER_VERSION",
    "COGNITIVE_VERIFICATION_RESOLVED",
    "PRODUCTION_ENABLED",
    "PROVENANCE_GATE_ACCEPTED_SET",
    "ResolvedCognitiveFactStatusV1",
    "ResolvedCognitiveFactV1",
    "canonical_verification_resolution_payload_v1",
]
