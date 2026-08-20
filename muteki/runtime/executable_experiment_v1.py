"""Executable, deterministic boundary for one H5 discriminating experiment.

H5's existing ``DiscriminatingExperiment`` deliberately contains semantic and
predicate *digests*, not executable instructions.  This module materializes those
digests into an experiment-specific spec that can be sealed in the existing CAS,
placed byte-for-byte in a worker prompt, and replayed by the host over canonical
capture bytes.

The spec is not an admission permit and the classifier is not a verifier.  A worker
never selects its own outcome partition: the host evaluates prospectively declared
predicates.  Zero matches, missing/invalid observations, and multiple distinct
partition matches remain inconclusive.  The module owns no store, dispatch, budget,
effect, progress, learning, production, or provenance-gate authority.
"""

from __future__ import annotations

import hashlib
import json
from collections.abc import Mapping
from dataclasses import dataclass
from enum import Enum
from typing import Any

from muteki.epistemic.cas import ReceiptCAS
from muteki.epistemic.contracts import (
    FrozenJSON,
    canonical_digest,
    canonical_json_bytes,
    freeze_json,
)
from muteki.runtime.hypothesis import DiscriminatingExperiment


EXECUTABLE_EXPERIMENT_SCHEMA_ID = "muteki.executable-experiment.v1"
EXECUTABLE_EXPERIMENT_BINDING_SCHEMA_ID = (
    "muteki.executable-experiment-binding.v1"
)
EXECUTABLE_EXPERIMENT_WORKER_VIEW_SCHEMA_ID = (
    "muteki.executable-experiment-worker-view.v1"
)
EXECUTABLE_EXPERIMENT_REPRODUCTION_KERNEL_SCHEMA_ID = (
    "muteki.executable-experiment-reproduction-kernel.v1"
)
DETERMINISTIC_CLASSIFIER_VERSION = (
    "muteki.executable-experiment-classifier.v1"
)
PRODUCTION_ENABLED = False
ACCEPTED_SET_CHANGE = False

MAX_SPEC_CANONICAL_BYTES = 131_072
MAX_PROCEDURE_STEPS = 32
MAX_OBSERVATIONS = 32
MAX_PREDICATES = 64
MAX_STOP_CONDITIONS = 32
MAX_INSTRUCTION_CHARS = 8_192
MAX_OBSERVATION_BYTES = 1_048_576


def _text(value: object, name: str, *, maximum: int | None = None) -> str:
    if type(value) is not str or not value or value != value.strip():
        raise ValueError(f"{name} must be exact non-empty text")
    if maximum is not None and len(value) > maximum:
        raise ValueError(f"{name} exceeds its bounded length")
    return value


def _identifier(value: object, name: str) -> str:
    result = _text(value, name, maximum=160)
    if not result[0].isalnum() or any(
        character
        not in "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789_.:-"
        for character in result
    ):
        raise ValueError(f"{name} must be a bounded canonical identifier")
    return result


def _digest(value: object, name: str) -> str:
    result = _text(value, name)
    if len(result) != 64 or any(
        character not in "0123456789abcdef" for character in result
    ):
        raise ValueError(f"{name} must be a lowercase sha256 digest")
    return result


def _positive_int(value: object, name: str, *, maximum: int | None = None) -> int:
    if type(value) is not int or value <= 0:
        raise ValueError(f"{name} must be a positive exact integer")
    if maximum is not None and value > maximum:
        raise ValueError(f"{name} exceeds its hard ceiling")
    return value


def _canonical_tuple(
    value: object,
    name: str,
    *,
    item_type: type,
    maximum: int,
    required: bool = True,
) -> tuple[Any, ...]:
    if type(value) is not tuple or any(type(item) is not item_type for item in value):
        raise TypeError(f"{name} must be a tuple of exact {item_type.__name__}")
    if required and not value:
        raise ValueError(f"{name} must not be empty")
    if len(value) > maximum:
        raise ValueError(f"{name} exceeds its hard count ceiling")
    return value


class ObservationSource(str, Enum):
    STDOUT = "stdout"
    STDERR = "stderr"
    TOOL_RESULT = "tool_result"


class PredicateKind(str, Enum):
    UTF8_CONTAINS = "utf8_contains"
    UTF8_EQUALS = "utf8_equals"
    JSON_POINTER_EQUALS = "json_pointer_equals"
    RAW_SHA256_EQUALS = "raw_sha256_equals"


class ClassificationStatus(str, Enum):
    OBSERVED = "observed"
    INCONCLUSIVE = "inconclusive"
    AMBIGUOUS = "ambiguous"


@dataclass(frozen=True, slots=True)
class ProcedureStepV1:
    step_id: str
    instruction: str
    required_capability_digest: str
    input_artifact_digests: tuple[str, ...]
    observation_ids: tuple[str, ...]

    def __post_init__(self) -> None:
        object.__setattr__(self, "step_id", _identifier(self.step_id, "step_id"))
        object.__setattr__(
            self,
            "instruction",
            _text(
                self.instruction,
                "instruction",
                maximum=MAX_INSTRUCTION_CHARS,
            ),
        )
        object.__setattr__(
            self,
            "required_capability_digest",
            _digest(self.required_capability_digest, "required_capability_digest"),
        )
        for name in ("input_artifact_digests", "observation_ids"):
            value = getattr(self, name)
            if type(value) is not tuple:
                raise TypeError(f"{name} must be a built-in tuple")
            normalized = tuple(
                _digest(item, f"{name}[{index}]")
                if name == "input_artifact_digests"
                else _identifier(item, f"{name}[{index}]")
                for index, item in enumerate(value)
            )
            if len(normalized) != len(set(normalized)) or normalized != tuple(
                sorted(normalized)
            ):
                raise ValueError(f"{name} must be unique and canonicalized")
            object.__setattr__(self, name, normalized)

    def canonical_body(self) -> dict[str, Any]:
        return {
            "input_artifact_digests": self.input_artifact_digests,
            "instruction": self.instruction,
            "observation_ids": self.observation_ids,
            "required_capability_digest": self.required_capability_digest,
            "step_id": self.step_id,
        }

    @property
    def digest(self) -> str:
        return canonical_digest(self.canonical_body())

@dataclass(frozen=True, slots=True)
class ObservationSpecV1:
    observation_id: str
    source: ObservationSource
    media_type: str
    maximum_bytes: int

    def __post_init__(self) -> None:
        object.__setattr__(
            self,
            "observation_id",
            _identifier(self.observation_id, "observation_id"),
        )
        if type(self.source) is not ObservationSource:
            raise TypeError("source must be ObservationSource")
        if self.media_type not in {"text/plain; charset=utf-8", "application/json"}:
            raise ValueError("observation media_type is unsupported")
        object.__setattr__(
            self,
            "maximum_bytes",
            _positive_int(
                self.maximum_bytes,
                "maximum_bytes",
                maximum=MAX_OBSERVATION_BYTES,
            ),
        )

    def canonical_body(self) -> dict[str, Any]:
        return {
            "maximum_bytes": self.maximum_bytes,
            "media_type": self.media_type,
            "observation_id": self.observation_id,
            "source": self.source.value,
        }

    @property
    def digest(self) -> str:
        return canonical_digest(self.canonical_body())

@dataclass(frozen=True, slots=True)
class PredicateSpecV1:
    predicate_id: str
    observation_id: str
    kind: PredicateKind
    operand: FrozenJSON
    outcome_partition_digest: str

    def __post_init__(self) -> None:
        object.__setattr__(
            self,
            "predicate_id",
            _identifier(self.predicate_id, "predicate_id"),
        )
        object.__setattr__(
            self,
            "observation_id",
            _identifier(self.observation_id, "observation_id"),
        )
        if type(self.kind) is not PredicateKind:
            raise TypeError("kind must be PredicateKind")
        operand = freeze_json(self.operand, path="$.predicate.operand")
        if self.kind in {PredicateKind.UTF8_CONTAINS, PredicateKind.UTF8_EQUALS}:
            _text(operand, "predicate operand", maximum=8_192)
        elif self.kind is PredicateKind.RAW_SHA256_EQUALS:
            _digest(operand, "predicate operand")
        elif self.kind is PredicateKind.JSON_POINTER_EQUALS:
            if not isinstance(operand, Mapping) or set(operand) != {
                "pointer",
                "value",
            }:
                raise ValueError(
                    "json_pointer_equals operand must contain pointer and value"
                )
            pointer = operand["pointer"]
            if type(pointer) is not str or not pointer.startswith("/"):
                raise ValueError("JSON pointer must be an absolute object path")
            if any(part == "" for part in pointer.split("/")[1:]):
                raise ValueError("JSON pointer contains an empty segment")
            for segment in pointer.split("/")[1:]:
                index = 0
                while index < len(segment):
                    if segment[index] != "~":
                        index += 1
                        continue
                    if index + 1 >= len(segment) or segment[index + 1] not in {
                        "0",
                        "1",
                    }:
                        raise ValueError("JSON pointer contains an invalid escape")
                    index += 2
        object.__setattr__(self, "operand", operand)
        object.__setattr__(
            self,
            "outcome_partition_digest",
            _digest(self.outcome_partition_digest, "outcome_partition_digest"),
        )

    def predicate_body(self) -> dict[str, Any]:
        return {
            "kind": self.kind.value,
            "observation_id": self.observation_id,
            "operand": self.operand,
            "predicate_id": self.predicate_id,
        }

    @property
    def predicate_digest(self) -> str:
        return canonical_digest(self.predicate_body())

    def canonical_body(self) -> dict[str, Any]:
        return {
            **self.predicate_body(),
            "outcome_partition_digest": self.outcome_partition_digest,
            "predicate_digest": self.predicate_digest,
        }


@dataclass(frozen=True, slots=True)
class StopConditionSpecV1:
    condition_id: str
    description: str

    def __post_init__(self) -> None:
        object.__setattr__(
            self,
            "condition_id",
            _identifier(self.condition_id, "condition_id"),
        )
        object.__setattr__(
            self,
            "description",
            _text(self.description, "description", maximum=2_048),
        )

    def canonical_body(self) -> dict[str, str]:
        return {
            "condition_id": self.condition_id,
            "description": self.description,
        }

    @property
    def digest(self) -> str:
        return canonical_digest(self.canonical_body())


@dataclass(frozen=True, slots=True)
class ExecutableExperimentSpecV1:
    experiment_digest: str
    context_packet_digest: str
    scope_digest: str
    semantic_signature_digest: str
    procedure_steps: tuple[ProcedureStepV1, ...]
    observations: tuple[ObservationSpecV1, ...]
    predicates: tuple[PredicateSpecV1, ...]
    stop_conditions: tuple[StopConditionSpecV1, ...]
    schema_id: str = EXECUTABLE_EXPERIMENT_SCHEMA_ID
    accepted_set_change: bool = ACCEPTED_SET_CHANGE

    def __post_init__(self) -> None:
        for name in (
            "experiment_digest",
            "context_packet_digest",
            "scope_digest",
            "semantic_signature_digest",
        ):
            object.__setattr__(self, name, _digest(getattr(self, name), name))
        steps = _canonical_tuple(
            self.procedure_steps,
            "procedure_steps",
            item_type=ProcedureStepV1,
            maximum=MAX_PROCEDURE_STEPS,
        )
        observations = _canonical_tuple(
            self.observations,
            "observations",
            item_type=ObservationSpecV1,
            maximum=MAX_OBSERVATIONS,
        )
        predicates = _canonical_tuple(
            self.predicates,
            "predicates",
            item_type=PredicateSpecV1,
            maximum=MAX_PREDICATES,
        )
        stops = _canonical_tuple(
            self.stop_conditions,
            "stop_conditions",
            item_type=StopConditionSpecV1,
            maximum=MAX_STOP_CONDITIONS,
        )
        for name, values, key in (
            ("procedure_steps", steps, lambda item: item.step_id),
            ("observations", observations, lambda item: item.observation_id),
            ("predicates", predicates, lambda item: item.predicate_id),
            ("stop_conditions", stops, lambda item: item.condition_id),
        ):
            identifiers = tuple(key(item) for item in values)
            if identifiers != tuple(sorted(identifiers)) or len(identifiers) != len(
                set(identifiers)
            ):
                raise ValueError(f"{name} must be unique and canonicalized")
        observation_ids = {item.observation_id for item in observations}
        observation_sources = tuple(item.source for item in observations)
        if len(observation_sources) != len(set(observation_sources)):
            raise ValueError(
                "v1 executable experiments permit one observation per source"
            )
        if any(step.input_artifact_digests for step in steps):
            raise ValueError(
                "v1 executable experiments do not yet materialize input artifacts"
            )
        if any(not step.observation_ids for step in steps):
            raise ValueError("every procedure step must declare an observation")
        referenced_observation_ids = {
            observation_id
            for step in steps
            for observation_id in step.observation_ids
        }
        if referenced_observation_ids != observation_ids:
            raise ValueError("procedure must bind every declared observation exactly")
        if any(
            not set(step.observation_ids).issubset(observation_ids) for step in steps
        ):
            raise ValueError("procedure references an unknown observation")
        if any(
            predicate.observation_id not in observation_ids
            for predicate in predicates
        ):
            raise ValueError("predicate references an unknown observation")
        if any(predicate.kind is PredicateKind.RAW_SHA256_EQUALS for predicate in predicates):
            raise ValueError(
                "v1 host text capture cannot materialize raw byte hash predicates"
            )
        if self.schema_id != EXECUTABLE_EXPERIMENT_SCHEMA_ID:
            raise ValueError("executable experiment schema is unsupported")
        if self.accepted_set_change is not False:
            raise ValueError("executable experiment cannot change the gate accepted set")
        body_size = len(canonical_json_bytes(self.canonical_body()))
        if body_size > MAX_SPEC_CANONICAL_BYTES:
            raise ValueError("executable experiment exceeds its canonical byte ceiling")

    def validate_against(self, experiment: DiscriminatingExperiment) -> None:
        if type(experiment) is not DiscriminatingExperiment:
            raise TypeError("experiment must be DiscriminatingExperiment")
        self.validate_against_body(experiment.canonical_body())

    def validate_against_body(self, experiment: Mapping[str, Any]) -> None:
        """Replay the binding from the canonical experiment body in an event."""

        if not isinstance(experiment, Mapping):
            raise TypeError("experiment must be a canonical mapping")
        predictions = experiment.get("predictions")
        signature = experiment.get("semantic_signature")
        if type(predictions) not in {tuple, list} or not isinstance(
            signature, Mapping
        ):
            raise ValueError("canonical H5 experiment body is malformed")
        expected_predicates = {
            (item.get("predicate_digest"), item.get("outcome_partition_digest"))
            for item in predictions
            if isinstance(item, Mapping)
        }
        materialized_predicates = {
            (item.predicate_digest, item.outcome_partition_digest)
            for item in self.predicates
        }
        if (
            len(expected_predicates) != len(predictions)
            or self.experiment_digest != canonical_digest(experiment)
            or self.context_packet_digest != experiment.get("context_packet_digest")
            or self.scope_digest != experiment.get("scope_digest")
            or self.semantic_signature_digest != canonical_digest(signature)
            or any(
                item.required_capability_digest
                != signature.get("tool_capability_digest")
                for item in self.procedure_steps
            )
            or materialized_predicates != expected_predicates
            or {item.outcome_partition_digest for item in self.predicates}
            != set(signature.get("prediction_partition_digests", ()))
            or {item.digest for item in self.stop_conditions}
            != set(signature.get("stop_condition_digests", ()))
        ):
            raise ValueError(
                "executable spec does not materialize the exact H5 experiment"
            )

    @classmethod
    def from_canonical(cls, body: Mapping[str, Any]) -> "ExecutableExperimentSpecV1":
        if not isinstance(body, Mapping) or set(body) != {
            "accepted_set_change",
            "context_packet_digest",
            "experiment_digest",
            "observations",
            "predicates",
            "procedure_steps",
            "schema_id",
            "scope_digest",
            "semantic_signature_digest",
            "stop_conditions",
        }:
            raise ValueError("executable experiment canonical shape diverged")
        return cls(
            experiment_digest=body["experiment_digest"],
            context_packet_digest=body["context_packet_digest"],
            scope_digest=body["scope_digest"],
            semantic_signature_digest=body["semantic_signature_digest"],
            procedure_steps=tuple(
                ProcedureStepV1(
                    step_id=item["step_id"],
                    instruction=item["instruction"],
                    required_capability_digest=item[
                        "required_capability_digest"
                    ],
                    input_artifact_digests=tuple(
                        item["input_artifact_digests"]
                    ),
                    observation_ids=tuple(item["observation_ids"]),
                )
                for item in body["procedure_steps"]
            ),
            observations=tuple(
                ObservationSpecV1(
                    observation_id=item["observation_id"],
                    source=ObservationSource(item["source"]),
                    media_type=item["media_type"],
                    maximum_bytes=item["maximum_bytes"],
                )
                for item in body["observations"]
            ),
            predicates=tuple(
                PredicateSpecV1(
                    predicate_id=item["predicate_id"],
                    observation_id=item["observation_id"],
                    kind=PredicateKind(item["kind"]),
                    operand=item["operand"],
                    outcome_partition_digest=item["outcome_partition_digest"],
                )
                for item in body["predicates"]
            ),
            stop_conditions=tuple(
                StopConditionSpecV1(
                    condition_id=item["condition_id"],
                    description=item["description"],
                )
                for item in body["stop_conditions"]
            ),
            schema_id=body["schema_id"],
            accepted_set_change=body["accepted_set_change"],
        )

    def canonical_body(self) -> dict[str, Any]:
        return {
            "accepted_set_change": self.accepted_set_change,
            "context_packet_digest": self.context_packet_digest,
            "experiment_digest": self.experiment_digest,
            "observations": tuple(item.canonical_body() for item in self.observations),
            "predicates": tuple(item.canonical_body() for item in self.predicates),
            "procedure_steps": tuple(
                item.canonical_body() for item in self.procedure_steps
            ),
            "schema_id": self.schema_id,
            "scope_digest": self.scope_digest,
            "semantic_signature_digest": self.semantic_signature_digest,
            "stop_conditions": tuple(
                item.canonical_body() for item in self.stop_conditions
            ),
        }

    @property
    def bytes(self) -> bytes:
        return canonical_json_bytes(self.canonical_body())

    def worker_view_body(self) -> dict[str, Any]:
        """Execution-only view; prospective predicates stay host-sealed."""

        return {
            "accepted_set_change": False,
            "context_packet_digest": self.context_packet_digest,
            "experiment_digest": self.experiment_digest,
            "observations": tuple(
                item.canonical_body() for item in self.observations
            ),
            "predicates_withheld_from_worker": True,
            "procedure_steps": tuple(
                item.canonical_body() for item in self.procedure_steps
            ),
            "schema_id": EXECUTABLE_EXPERIMENT_WORKER_VIEW_SCHEMA_ID,
            "scope_digest": self.scope_digest,
            "stop_conditions": tuple(
                item.canonical_body() for item in self.stop_conditions
            ),
        }

    def reproduction_kernel_body(self) -> dict[str, Any]:
        """Attempt-independent procedure identity for a preregistered replay.

        A fresh reproduction necessarily has a different attempt, ContextPacket,
        scope binding, and H5 experiment digest.  Those identities must therefore
        not be used to decide whether two executions exercised the same causal
        channel.  The procedure, observation contract, hidden predicates, semantic
        capability, and stop conditions do remain exact.  This kernel is only an
        equivalence witness; it grants no retry, verification, or learning authority.
        """

        return {
            "accepted_set_change": False,
            "observations": tuple(
                item.canonical_body() for item in self.observations
            ),
            "predicates": tuple(item.canonical_body() for item in self.predicates),
            "procedure_steps": tuple(
                item.canonical_body() for item in self.procedure_steps
            ),
            "schema_id": EXECUTABLE_EXPERIMENT_REPRODUCTION_KERNEL_SCHEMA_ID,
            "semantic_signature_digest": self.semantic_signature_digest,
            "stop_conditions": tuple(
                item.canonical_body() for item in self.stop_conditions
            ),
        }

    @property
    def reproduction_kernel_digest(self) -> str:
        return canonical_digest(self.reproduction_kernel_body())

    def validate_reproduction_kernel(
        self,
        reproduction: "ExecutableExperimentSpecV1",
    ) -> None:
        if type(reproduction) is not ExecutableExperimentSpecV1:
            raise TypeError("reproduction must be ExecutableExperimentSpecV1")
        if reproduction.reproduction_kernel_digest != self.reproduction_kernel_digest:
            raise ValueError("reproduction changed the preregistered causal kernel")

    @property
    def worker_view_bytes(self) -> bytes:
        return canonical_json_bytes(self.worker_view_body())

    @property
    def worker_view_digest(self) -> str:
        return canonical_digest(self.worker_view_body())

    @property
    def digest(self) -> str:
        return canonical_digest(self.canonical_body())

    def validate_classification(
        self,
        classification: "DeterministicExperimentClassificationV1",
    ) -> None:
        """Bind a structural verdict back to this exact predicate partition map."""

        if type(classification) is not DeterministicExperimentClassificationV1:
            raise TypeError(
                "classification must be DeterministicExperimentClassificationV1"
            )
        predicate_partition = {
            item.predicate_digest: item.outcome_partition_digest
            for item in self.predicates
        }
        expected_predicates = tuple(sorted(predicate_partition))
        expected_partitions = tuple(sorted(set(predicate_partition.values())))
        if (
            classification.spec_digest != self.digest
            or classification.prospective_predicate_digests != expected_predicates
            or classification.prospective_partition_digests != expected_partitions
        ):
            raise ValueError("classification is rebound from its executable spec")
        matched_partitions = {
            predicate_partition[digest]
            for digest in classification.matched_predicate_digests
        }
        if classification.status is ClassificationStatus.OBSERVED and (
            matched_partitions != {classification.observed_partition_digest}
        ):
            raise ValueError(
                "matched predicates do not prove the observed partition"
            )
        if classification.status is ClassificationStatus.AMBIGUOUS and len(
            matched_partitions
        ) < 2:
            raise ValueError("ambiguous classification lacks distinct partitions")


@dataclass(frozen=True, slots=True)
class ExecutableExperimentBindingV1:
    spec: ExecutableExperimentSpecV1
    artifact_digest: str
    byte_count: int
    worker_view_artifact_digest: str
    worker_view_byte_count: int
    schema_id: str = EXECUTABLE_EXPERIMENT_BINDING_SCHEMA_ID

    def __post_init__(self) -> None:
        if type(self.spec) is not ExecutableExperimentSpecV1:
            raise TypeError("spec must be ExecutableExperimentSpecV1")
        object.__setattr__(
            self,
            "artifact_digest",
            _digest(self.artifact_digest, "artifact_digest"),
        )
        object.__setattr__(
            self,
            "byte_count",
            _positive_int(
                self.byte_count,
                "byte_count",
                maximum=MAX_SPEC_CANONICAL_BYTES,
            ),
        )
        object.__setattr__(
            self,
            "worker_view_artifact_digest",
            _digest(
                self.worker_view_artifact_digest,
                "worker_view_artifact_digest",
            ),
        )
        object.__setattr__(
            self,
            "worker_view_byte_count",
            _positive_int(
                self.worker_view_byte_count,
                "worker_view_byte_count",
                maximum=MAX_SPEC_CANONICAL_BYTES,
            ),
        )
        if (
            self.schema_id != EXECUTABLE_EXPERIMENT_BINDING_SCHEMA_ID
            or self.artifact_digest != self.spec.digest
            or self.byte_count != len(self.spec.bytes)
            or self.worker_view_artifact_digest != self.spec.worker_view_digest
            or self.worker_view_byte_count != len(self.spec.worker_view_bytes)
        ):
            raise ValueError("executable experiment artifact binding diverged")

    @classmethod
    def seal(
        cls,
        *,
        cas: ReceiptCAS,
        spec: ExecutableExperimentSpecV1,
        experiment: DiscriminatingExperiment,
    ) -> "ExecutableExperimentBindingV1":
        if not isinstance(cas, ReceiptCAS):
            raise TypeError("cas must be ReceiptCAS")
        if type(spec) is not ExecutableExperimentSpecV1:
            raise TypeError("spec must be ExecutableExperimentSpecV1")
        spec.validate_against(experiment)
        sealed = cas.seal_bytes(spec.bytes)
        sealed_worker_view = cas.seal_bytes(spec.worker_view_bytes)
        return cls(
            spec=spec,
            artifact_digest=sealed.digest,
            byte_count=sealed.byte_count,
            worker_view_artifact_digest=sealed_worker_view.digest,
            worker_view_byte_count=sealed_worker_view.byte_count,
        )

    def resolve(self, cas: ReceiptCAS) -> ExecutableExperimentSpecV1:
        if not isinstance(cas, ReceiptCAS):
            raise TypeError("cas must be ReceiptCAS")
        raw = cas.read_verified(self.artifact_digest)
        if raw != self.spec.bytes or len(raw) != self.byte_count:
            raise ValueError("executable experiment CAS artifact diverged")
        worker_view = cas.read_verified(self.worker_view_artifact_digest)
        if (
            worker_view != self.spec.worker_view_bytes
            or len(worker_view) != self.worker_view_byte_count
        ):
            raise ValueError("executable experiment worker view CAS artifact diverged")
        return self.spec

    @classmethod
    def from_canonical(
        cls, body: Mapping[str, Any]
    ) -> "ExecutableExperimentBindingV1":
        if not isinstance(body, Mapping) or set(body) != {
            "artifact_digest",
            "byte_count",
            "schema_id",
            "spec",
            "spec_digest",
            "worker_view_artifact_digest",
            "worker_view_byte_count",
            "worker_view_digest",
        }:
            raise ValueError("executable experiment binding shape diverged")
        spec = ExecutableExperimentSpecV1.from_canonical(body["spec"])
        if body["spec_digest"] != spec.digest:
            raise ValueError("executable experiment spec digest is false")
        if body["worker_view_digest"] != spec.worker_view_digest:
            raise ValueError("executable experiment worker view digest is false")
        return cls(
            spec=spec,
            artifact_digest=body["artifact_digest"],
            byte_count=body["byte_count"],
            worker_view_artifact_digest=body["worker_view_artifact_digest"],
            worker_view_byte_count=body["worker_view_byte_count"],
            schema_id=body["schema_id"],
        )

    def canonical_body(self) -> dict[str, Any]:
        return {
            "artifact_digest": self.artifact_digest,
            "byte_count": self.byte_count,
            "schema_id": self.schema_id,
            "spec": self.spec.canonical_body(),
            "spec_digest": self.spec.digest,
            "worker_view_artifact_digest": self.worker_view_artifact_digest,
            "worker_view_byte_count": self.worker_view_byte_count,
            "worker_view_digest": self.spec.worker_view_digest,
        }

    @property
    def digest(self) -> str:
        return canonical_digest(self.canonical_body())


@dataclass(frozen=True, slots=True)
class CapturedObservationV1:
    observation_id: str
    source: ObservationSource
    raw: bytes
    capture_event_digest: str
    manifest_digest: str

    def __post_init__(self) -> None:
        object.__setattr__(
            self,
            "observation_id",
            _identifier(self.observation_id, "observation_id"),
        )
        if type(self.source) is not ObservationSource:
            raise TypeError("source must be ObservationSource")
        if type(self.raw) is not bytes:
            raise TypeError("raw must be exact bytes")
        if len(self.raw) > MAX_OBSERVATION_BYTES:
            raise ValueError("captured observation exceeds the global byte ceiling")
        for name in ("capture_event_digest", "manifest_digest"):
            object.__setattr__(self, name, _digest(getattr(self, name), name))

    @property
    def raw_digest(self) -> str:
        return hashlib.sha256(self.raw).hexdigest()

    def canonical_body(self) -> dict[str, Any]:
        return {
            "byte_count": len(self.raw),
            "capture_event_digest": self.capture_event_digest,
            "manifest_digest": self.manifest_digest,
            "observation_id": self.observation_id,
            "raw_digest": self.raw_digest,
            "source": self.source.value,
        }


@dataclass(frozen=True, slots=True)
class DeterministicExperimentClassificationV1:
    spec_digest: str
    status: ClassificationStatus
    observed_partition_digest: str | None
    prospective_partition_digests: tuple[str, ...]
    prospective_predicate_digests: tuple[str, ...]
    matched_predicate_digests: tuple[str, ...]
    observation_bindings: tuple[Mapping[str, Any], ...]
    reason_codes: tuple[str, ...]
    classifier_version: str = DETERMINISTIC_CLASSIFIER_VERSION
    learning_eligible: bool = False
    accepted_set_change: bool = ACCEPTED_SET_CHANGE

    def __post_init__(self) -> None:
        object.__setattr__(self, "spec_digest", _digest(self.spec_digest, "spec_digest"))
        if type(self.status) is not ClassificationStatus:
            raise TypeError("status must be ClassificationStatus")
        if self.observed_partition_digest is not None:
            object.__setattr__(
                self,
                "observed_partition_digest",
                _digest(
                    self.observed_partition_digest,
                    "observed_partition_digest",
                ),
            )
        if (self.status is ClassificationStatus.OBSERVED) != (
            self.observed_partition_digest is not None
        ):
            raise ValueError("classification status/partition diverged")
        for name in (
            "prospective_partition_digests",
            "prospective_predicate_digests",
        ):
            values = getattr(self, name)
            if type(values) is not tuple or not values:
                raise TypeError(f"{name} must be a non-empty built-in tuple")
            for digest in values:
                _digest(digest, name)
            if values != tuple(sorted(set(values))):
                raise ValueError(f"{name} must be canonicalized")
        for digest in self.matched_predicate_digests:
            _digest(digest, "matched_predicate_digest")
        if self.matched_predicate_digests != tuple(
            sorted(set(self.matched_predicate_digests))
        ):
            raise ValueError("matched predicate digests are not canonicalized")
        if not set(self.matched_predicate_digests).issubset(
            self.prospective_predicate_digests
        ):
            raise ValueError("classification matched an undeclared predicate")
        if self.observed_partition_digest is not None and (
            self.observed_partition_digest not in self.prospective_partition_digests
            or not self.matched_predicate_digests
        ):
            raise ValueError("classification observed an undeclared partition")
        if type(self.observation_bindings) is not tuple or any(
            not isinstance(item, Mapping) for item in self.observation_bindings
        ):
            raise TypeError("observation_bindings must be a built-in tuple")
        object.__setattr__(
            self,
            "observation_bindings",
            tuple(
                freeze_json(item, path=f"$.observation_bindings[{index}]")
                for index, item in enumerate(self.observation_bindings)
            ),
        )
        if type(self.reason_codes) is not tuple or not self.reason_codes:
            raise ValueError("classification requires explicit reason codes")
        if self.classifier_version != DETERMINISTIC_CLASSIFIER_VERSION:
            raise ValueError("classifier version diverged")
        if self.learning_eligible is not False or self.accepted_set_change is not False:
            raise ValueError("structural classification overclaims authority")

    def canonical_body(self) -> dict[str, Any]:
        return {
            "accepted_set_change": self.accepted_set_change,
            "classifier_version": self.classifier_version,
            "learning_eligible": self.learning_eligible,
            "matched_predicate_digests": self.matched_predicate_digests,
            "observation_bindings": self.observation_bindings,
            "observed_partition_digest": self.observed_partition_digest,
            "prospective_partition_digests": self.prospective_partition_digests,
            "prospective_predicate_digests": self.prospective_predicate_digests,
            "reason_codes": self.reason_codes,
            "spec_digest": self.spec_digest,
            "status": self.status.value,
        }

    @property
    def digest(self) -> str:
        return canonical_digest(self.canonical_body())


def _json_pointer(value: Any, pointer: str) -> tuple[bool, Any]:
    current = value
    for raw_segment in pointer.split("/")[1:]:
        segment = raw_segment.replace("~1", "/").replace("~0", "~")
        if not isinstance(current, Mapping) or segment not in current:
            return False, None
        current = current[segment]
    return True, current


def _predicate_matches(predicate: PredicateSpecV1, raw: bytes) -> bool:
    if predicate.kind is PredicateKind.RAW_SHA256_EQUALS:
        return hashlib.sha256(raw).hexdigest() == predicate.operand
    try:
        text = raw.decode("utf-8")
    except UnicodeDecodeError:
        return False
    if predicate.kind is PredicateKind.UTF8_CONTAINS:
        return str(predicate.operand) in text
    if predicate.kind is PredicateKind.UTF8_EQUALS:
        return text == predicate.operand
    assert predicate.kind is PredicateKind.JSON_POINTER_EQUALS
    def reject_duplicate_keys(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
        result: dict[str, Any] = {}
        for key, value in pairs:
            if key in result:
                raise ValueError("duplicate JSON object key")
            result[key] = value
        return result

    try:
        value = json.loads(text, object_pairs_hook=reject_duplicate_keys)
    except (json.JSONDecodeError, ValueError):
        return False
    assert isinstance(predicate.operand, Mapping)
    found, observed = _json_pointer(value, str(predicate.operand["pointer"]))
    if not found:
        return False
    try:
        return canonical_json_bytes(freeze_json(observed)) == canonical_json_bytes(
            predicate.operand["value"]
        )
    except (TypeError, ValueError):
        return False


def classify_executable_experiment(
    spec: ExecutableExperimentSpecV1,
    captures: tuple[CapturedObservationV1, ...],
) -> DeterministicExperimentClassificationV1:
    """Classify host-captured bytes; never trust a worker-authored partition."""

    if type(spec) is not ExecutableExperimentSpecV1:
        raise TypeError("spec must be ExecutableExperimentSpecV1")
    if type(captures) is not tuple or any(
        type(item) is not CapturedObservationV1 for item in captures
    ):
        raise TypeError("captures must be a tuple of CapturedObservationV1")
    by_id: dict[str, CapturedObservationV1] = {}
    duplicate_ids: set[str] = set()
    for item in captures:
        if item.observation_id in by_id:
            duplicate_ids.add(item.observation_id)
        else:
            by_id[item.observation_id] = item
    spec_by_id = {item.observation_id: item for item in spec.observations}
    reasons: list[str] = []
    invalid_ids: set[str] = set(duplicate_ids)
    if duplicate_ids:
        reasons.append("duplicate_observation_binding")
    for observation_id, expected in spec_by_id.items():
        captured = by_id.get(observation_id)
        if captured is None:
            reasons.append(f"missing_observation:{observation_id}")
            invalid_ids.add(observation_id)
        elif captured.source is not expected.source:
            reasons.append(f"observation_source_mismatch:{observation_id}")
            invalid_ids.add(observation_id)
        elif len(captured.raw) > expected.maximum_bytes:
            reasons.append(f"observation_size_exceeded:{observation_id}")
            invalid_ids.add(observation_id)
    extra = sorted(set(by_id) - set(spec_by_id))
    if extra:
        reasons.append("undeclared_observation_binding")
        invalid_ids.update(extra)

    matched: list[PredicateSpecV1] = []
    for predicate in spec.predicates:
        capture = by_id.get(predicate.observation_id)
        if capture is None or predicate.observation_id in invalid_ids:
            continue
        if _predicate_matches(predicate, capture.raw):
            matched.append(predicate)
    partitions = {item.outcome_partition_digest for item in matched}
    if len(partitions) == 1 and not invalid_ids:
        status = ClassificationStatus.OBSERVED
        observed_partition = next(iter(partitions))
        reasons.append("one_prospective_partition_matched")
    elif len(partitions) > 1:
        status = ClassificationStatus.AMBIGUOUS
        observed_partition = None
        reasons.append("multiple_distinct_partitions_matched")
    else:
        status = ClassificationStatus.INCONCLUSIVE
        observed_partition = None
        reasons.append("no_unique_prospective_partition_matched")
    return DeterministicExperimentClassificationV1(
        spec_digest=spec.digest,
        status=status,
        observed_partition_digest=observed_partition,
        prospective_partition_digests=tuple(
            sorted({item.outcome_partition_digest for item in spec.predicates})
        ),
        prospective_predicate_digests=tuple(
            sorted({item.predicate_digest for item in spec.predicates})
        ),
        matched_predicate_digests=tuple(
            sorted({item.predicate_digest for item in matched})
        ),
        observation_bindings=tuple(
            item.canonical_body() for item in sorted(captures, key=lambda row: row.observation_id)
        ),
        reason_codes=tuple(dict.fromkeys(reasons)),
    )


__all__ = [
    "ACCEPTED_SET_CHANGE",
    "DETERMINISTIC_CLASSIFIER_VERSION",
    "EXECUTABLE_EXPERIMENT_BINDING_SCHEMA_ID",
    "EXECUTABLE_EXPERIMENT_REPRODUCTION_KERNEL_SCHEMA_ID",
    "EXECUTABLE_EXPERIMENT_SCHEMA_ID",
    "EXECUTABLE_EXPERIMENT_WORKER_VIEW_SCHEMA_ID",
    "PRODUCTION_ENABLED",
    "CapturedObservationV1",
    "ClassificationStatus",
    "DeterministicExperimentClassificationV1",
    "ExecutableExperimentBindingV1",
    "ExecutableExperimentSpecV1",
    "ObservationSource",
    "ObservationSpecV1",
    "PredicateKind",
    "PredicateSpecV1",
    "ProcedureStepV1",
    "StopConditionSpecV1",
    "classify_executable_experiment",
]
