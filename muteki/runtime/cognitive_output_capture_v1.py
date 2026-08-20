"""Audited-runner binding for executable cognitive stdout/stderr.

The ordinary capture API accepts host-supplied bytes and is therefore useful for
tool telemetry but insufficient as experiment evidence.  This module is called by
the exact C6 host broker from ``run_cli_streaming``'s post-drain callback.  It maps
the two text streams to prospectively declared observation ids and seals them under
a separate store capability/actor.  It owns no classification, verification,
learning, retry, dispatch, or gate authority.
"""

from __future__ import annotations

from muteki.epistemic.broker import (
    CaptureSession,
    canonical_cognitive_observation_capture_id,
)
from muteki.epistemic.cas import ReceiptCAS
from muteki.epistemic.cognitive_events_v1 import (
    COGNITIVE_EXPERIMENT_ASSIGNED,
    COGNITIVE_RUNTIME_EXECUTABLE_ASSIGNMENT_SCHEMA_ID,
    COGNITIVE_RUNTIME_REPRODUCTION_ASSIGNMENT_SCHEMA_ID,
    validate_runtime_context_executable_assignment_payload_shape,
    validate_runtime_reproduction_assignment_payload_shape,
)
from muteki.epistemic.sqlite_store import EpistemicSQLiteStore, IntegrityError
from muteki.runtime.cognition import DeliveredContextPacketV1
from muteki.runtime.contracts import AttemptPermit
from muteki.runtime.executable_experiment_v1 import (
    ExecutableExperimentBindingV1,
    ObservationSource,
)
from muteki.runtime.ports import CaptureChunk


COGNITIVE_RUNTIME_OUTPUT_CAPTURE_VERSION = (
    "muteki.cognitive-runtime-output-capture.v1"
)
PRODUCTION_ENABLED = False
ACCEPTED_SET_CHANGE = False


class CognitiveRuntimeOutputCaptureV1:
    """Resolve one assignment and seal only bytes observed by the C6 reader."""

    def __init__(
        self,
        *,
        store: EpistemicSQLiteStore,
        cas: ReceiptCAS,
        delivered: DeliveredContextPacketV1,
        permit: AttemptPermit,
    ) -> None:
        if type(store) is not EpistemicSQLiteStore:
            raise TypeError("store must be exactly EpistemicSQLiteStore")
        if not isinstance(cas, ReceiptCAS):
            raise TypeError("cas must be ReceiptCAS")
        if type(delivered) is not DeliveredContextPacketV1:
            raise TypeError("delivered must be DeliveredContextPacketV1")
        if type(permit) is not AttemptPermit:
            raise TypeError("permit must be AttemptPermit")
        if permit.constraints.get("context_packet") != delivered.binding.canonical_body():
            raise IntegrityError("runtime output permit is rebound from its ContextPacket")
        self._store = store
        self._cas = cas
        self._delivered = delivered
        self._permit = permit

    def _binding(self) -> ExecutableExperimentBindingV1 | None:
        rows = tuple(
            row
            for row in self._store.event_rows(kind=COGNITIVE_EXPERIMENT_ASSIGNED)
            if row["payload"].get("permit_digest") == self._permit.digest
        )
        if not rows:
            return None
        if len(rows) != 1:
            raise IntegrityError("runtime cognitive output assignment is ambiguous")
        payload = rows[0]["payload"]
        schema_id = payload.get("schema_id")
        if schema_id not in {
            COGNITIVE_RUNTIME_EXECUTABLE_ASSIGNMENT_SCHEMA_ID,
            COGNITIVE_RUNTIME_REPRODUCTION_ASSIGNMENT_SCHEMA_ID,
        }:
            return None
        try:
            if schema_id == COGNITIVE_RUNTIME_REPRODUCTION_ASSIGNMENT_SCHEMA_ID:
                validate_runtime_reproduction_assignment_payload_shape(payload)
            else:
                validate_runtime_context_executable_assignment_payload_shape(payload)
            binding = ExecutableExperimentBindingV1.from_canonical(
                payload["executable_experiment_binding_body"]
            )
            binding.resolve(self._cas)
        except (KeyError, TypeError, ValueError) as exc:
            raise IntegrityError(
                "runtime cognitive output executable binding failed"
            ) from exc
        if (
            payload["attempt_digest"] != self._permit.lease.attempt.digest
            or payload["permit_id"] != self._permit.permit_id
            or payload["scope_digest"] != self._permit.lease.attempt.scope.digest
            or payload["context_packet_binding_body"]["packet_digest"]
            != self._delivered.binding.packet_digest
        ):
            raise IntegrityError("runtime cognitive output assignment lineage diverged")
        return binding

    def seal_from_audited_runner(
        self,
        *,
        stdout: str,
        stderr: str,
        occurred_at_ns: int,
    ) -> tuple[CaptureChunk, ...]:
        """Seal the exact post-drain text before driver parsing normalizes it."""

        if type(stdout) is not str or type(stderr) is not str:
            raise TypeError("audited runtime streams must be exact text")
        if type(occurred_at_ns) is not int or occurred_at_ns < 0:
            raise ValueError("occurred_at_ns must be a non-negative exact integer")
        binding = self._binding()
        if binding is None:
            return ()
        by_source = {
            ObservationSource.STDOUT: stdout.encode("utf-8"),
            ObservationSource.STDERR: stderr.encode("utf-8"),
        }
        declared = tuple(
            sorted(
                (
                    observation
                    for observation in binding.spec.observations
                    if observation.source in by_source
                ),
                key=lambda item: item.observation_id,
            )
        )
        if not declared:
            return ()
        for observation in declared:
            if len(by_source[observation.source]) > observation.maximum_bytes:
                raise IntegrityError(
                    "audited cognitive output exceeds its prospective byte ceiling"
                )
        session = CaptureSession(
            store=self._store,
            cas=self._cas,
            permit=self._permit,
        )
        captured: list[CaptureChunk] = []
        for ordinal, observation in enumerate(declared):
            captured.append(
                session._capture_cognitive_output(
                    capture_id=canonical_cognitive_observation_capture_id(
                        permit_digest=self._permit.digest,
                        spec_digest=binding.spec.digest,
                        observation_id=observation.observation_id,
                    ),
                    stream=observation.source.value,
                    data=by_source[observation.source],
                    occurred_at_ns=occurred_at_ns,
                    terminal=ordinal == len(declared) - 1,
                )
            )
        return tuple(captured)


__all__ = [
    "ACCEPTED_SET_CHANGE",
    "COGNITIVE_RUNTIME_OUTPUT_CAPTURE_VERSION",
    "CognitiveRuntimeOutputCaptureV1",
    "PRODUCTION_ENABLED",
]
