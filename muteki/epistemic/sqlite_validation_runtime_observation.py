"""Runtime cognitive observation validation."""

from __future__ import annotations

import json
from collections.abc import Mapping
from typing import Any

from muteki.epistemic.contracts import canonical_digest, canonical_json_bytes
from muteki.epistemic.sqlite_types import IntegrityError


def _validate_runtime_cognitive_execution_mutation(
    self, payload: Mapping[str, Any]
) -> None:
    """Semantic compare-and-append for one runtime structural observation."""

    from muteki.epistemic.cognitive_events_v1 import (
        COGNITIVE_EXECUTION_OBSERVED,
        COGNITIVE_EXPERIMENT_ASSIGNED,
        COGNITIVE_RUNTIME_EXECUTABLE_ASSIGNMENT_SCHEMA_ID,
        COGNITIVE_RUNTIME_REPRODUCTION_ASSIGNMENT_SCHEMA_ID,
    )
    from muteki.runtime.cognitive_runtime_observation_v1 import (
        validate_runtime_cognitive_observation_payload_shape,
    )
    from muteki.runtime.executable_experiment_v1 import (
        ExecutableExperimentBindingV1,
    )

    try:
        validate_runtime_cognitive_observation_payload_shape(payload)
    except (TypeError, ValueError) as exc:
        raise IntegrityError("runtime cognitive observation payload is false") from exc
    p = dict(payload)
    assignment = self._conn.execute(
        "SELECT seq,event_id,payload_json FROM events WHERE event_digest=? AND kind=?",
        (p["assignment_event_digest"], COGNITIVE_EXPERIMENT_ASSIGNED),
    ).fetchone()
    terminal = self._conn.execute(
        "SELECT seq,event_id,kind,payload_json FROM events "
        "WHERE event_digest=? AND kind=?",
        (p["terminal_event_digest"], p["terminal_event_kind"]),
    ).fetchone()
    budget = self._conn.execute(
        "SELECT seq,event_id,kind,payload_json FROM events "
        "WHERE event_digest=? AND kind=?",
        (p["budget_event_digest"], p["budget_event_kind"]),
    ).fetchone()
    own = self._conn.execute(
        "SELECT seq,command_id FROM events WHERE kind=? AND payload_json=?",
        (
            COGNITIVE_EXECUTION_OBSERVED,
            canonical_json_bytes(p).decode(),
        ),
    ).fetchone()
    if assignment is None or terminal is None or budget is None or own is None:
        raise IntegrityError("runtime cognitive observation lineage is incomplete")
    assignment_payload = json.loads(assignment[2])
    terminal_payload = json.loads(terminal[3])
    budget_payload = json.loads(budget[3])
    assignment_schema_id = assignment_payload.get("schema_id")
    if assignment_schema_id not in {
        COGNITIVE_RUNTIME_EXECUTABLE_ASSIGNMENT_SCHEMA_ID,
        COGNITIVE_RUNTIME_REPRODUCTION_ASSIGNMENT_SCHEMA_ID,
    }:
        raise IntegrityError(
            "runtime cognitive observation requires executable assignment"
        )
    try:
        executable = ExecutableExperimentBindingV1.from_canonical(
            assignment_payload["executable_experiment_binding_body"]
        )
    except (KeyError, TypeError, ValueError) as exc:
        raise IntegrityError("runtime cognitive executable binding is false") from exc
    if (
        assignment_payload.get("executable_experiment_binding_digest")
        != executable.digest
        or p["executable_experiment_binding_digest"] != executable.digest
        or p["executable_spec_digest"] != executable.spec.digest
        or canonical_digest(assignment_payload) != p["assignment_payload_digest"]
        or assignment[1] != f"event:{COGNITIVE_EXPERIMENT_ASSIGNED}:{p['attempt_id']}"
        or terminal[1] != p["terminal_event_id"]
        or budget[1] != p["budget_event_id"]
    ):
        raise IntegrityError(
            "runtime cognitive assignment or event identity is rebound"
        )
    if (
        canonical_digest(terminal_payload) != p["terminal_payload_digest"]
        or canonical_digest(budget_payload) != p["budget_payload_digest"]
    ):
        raise IntegrityError("runtime cognitive terminal/budget binding is false")
    for name in (
        "attempt_digest",
        "attempt_id",
        "permit_digest",
        "permit_id",
        "scope_digest",
    ):
        if p[name] != assignment_payload.get(name) or p[name] != terminal_payload.get(
            name
        ):
            raise IntegrityError("runtime cognitive identity lineage diverged")
    if (
        budget_payload.get("attempt_id") != p["attempt_id"]
        or terminal_payload.get("outcome") != p["terminal_outcome"]
        or assignment_payload.get("experiment_digest") != p["experiment_digest"]
        or assignment_payload.get("world_epoch_digest") != p["world_epoch_digest"]
        or assignment_payload.get("context_packet_binding_body", {}).get(
            "packet_digest"
        )
        != p["context_packet_digest"]
        or not (assignment[0] < terminal[0] < budget[0] < own[0])
    ):
        raise IntegrityError("runtime cognitive semantic lineage diverged")
    for event_digest, expected_receipt in (
        (p["assignment_event_digest"], p["assignment_event_receipt_digest"]),
        (p["terminal_event_digest"], p["terminal_event_receipt_digest"]),
        (p["budget_event_digest"], p["budget_event_receipt_digest"]),
    ):
        if self.receipt_digest_for_event(event_digest) != expected_receipt:
            raise IntegrityError("runtime cognitive receipt lineage diverged")

    _validate_runtime_cognitive_captures(
        self,
        p=p,
        executable=executable,
        assignment_schema_id=assignment_schema_id,
        assignment_payload=assignment_payload,
        terminal=terminal,
    )
    _validate_runtime_cognitive_prefix(
        self,
        p=p,
        observation_kind=COGNITIVE_EXECUTION_OBSERVED,
    )


def _validate_runtime_cognitive_captures(
    self,
    *,
    p: dict[str, Any],
    executable: Any,
    assignment_schema_id: object,
    assignment_payload: dict[str, Any],
    terminal: tuple[Any, ...],
) -> None:
    from muteki.epistemic.cognitive_events_v1 import (
        COGNITIVE_RUNTIME_REPRODUCTION_ASSIGNMENT_SCHEMA_ID,
    )
    from muteki.runtime.cognitive_runtime_observation_v1 import (
        canonical_observation_capture_id,
    )
    from muteki.runtime.executable_experiment_v1 import (
        ClassificationStatus,
        DeterministicExperimentClassificationV1,
    )

    observation_by_id = {
        item.observation_id: item for item in executable.spec.observations
    }
    expected_capture_ids = {
        canonical_observation_capture_id(
            permit_digest=p["permit_digest"],
            spec_digest=p["executable_spec_digest"],
            observation_id=item.observation_id,
        ): item
        for item in executable.spec.observations
    }
    actual_capture_rows = self._conn.execute(
        "SELECT seq,event_id,event_digest,actor,payload_json FROM events "
        "WHERE kind='CAPTURE_CHUNK_SEALED' ORDER BY seq"
    ).fetchall()
    actual_capture_rows = tuple(
        row
        for row in actual_capture_rows
        if json.loads(row[4]).get("permit_digest") == p["permit_digest"]
    )
    bound_event_digests: set[str] = set()
    bound_capture_ids: set[str] = set()
    bound_ordinals: list[int] = []
    derived_classification_bindings: list[dict[str, Any]] = []
    for binding in p["capture_bindings"]:
        observation = observation_by_id.get(binding["observation_id"])
        if observation is None:
            raise IntegrityError(
                "runtime cognitive capture references an undeclared observation"
            )
        expected_capture_id = canonical_observation_capture_id(
            permit_digest=p["permit_digest"],
            spec_digest=p["executable_spec_digest"],
            observation_id=observation.observation_id,
        )
        capture = self._conn.execute(
            "SELECT seq,event_id,actor,payload_json FROM events "
            "WHERE event_digest=? AND kind='CAPTURE_CHUNK_SEALED'",
            (binding["capture_event_digest"],),
        ).fetchone()
        manifest = self._conn.execute(
            "SELECT seq,event_id,actor,payload_json FROM events "
            "WHERE event_digest=? AND kind='CAPTURE_MANIFEST_ADVANCED'",
            (binding["manifest_event_digest"],),
        ).fetchone()
        if capture is None or manifest is None:
            raise IntegrityError("runtime cognitive capture pointer is absent")
        capture_payload = json.loads(capture[3])
        manifest_payload = json.loads(manifest[3])
        if (
            binding["capture_id"] != expected_capture_id
            or capture[2] != "cognitive-runtime-output-port-v1"
            or manifest[2] != "cognitive-runtime-output-port-v1"
            or capture_payload != manifest_payload
            or capture_payload.get("capture_id") != expected_capture_id
            or capture_payload.get("stream") != observation.source.value
            or capture_payload.get("raw_digest") != binding["raw_digest"]
            or capture_payload.get("byte_count") != binding["byte_count"]
            or capture_payload.get("manifest_digest") != binding["manifest_digest"]
            or capture_payload.get("ordinal") != binding["ordinal"]
            or capture_payload.get("terminal") is not binding["terminal"]
            or capture[1] != binding["capture_event_id"]
            or manifest[1] != binding["manifest_event_id"]
            or manifest[0] != capture[0] + 1
            or manifest[0] >= terminal[0]
            or self.receipt_digest_for_event(binding["capture_event_digest"])
            != binding["capture_receipt_digest"]
            or self.receipt_digest_for_event(binding["manifest_event_digest"])
            != binding["manifest_receipt_digest"]
        ):
            raise IntegrityError("runtime cognitive capture binding diverged")
        bound_event_digests.add(binding["capture_event_digest"])
        bound_capture_ids.add(binding["capture_id"])
        bound_ordinals.append(binding["ordinal"])
        derived_classification_bindings.append(
            {
                "byte_count": binding["byte_count"],
                "capture_event_digest": binding["capture_event_digest"],
                "manifest_digest": binding["manifest_digest"],
                "observation_id": binding["observation_id"],
                "raw_digest": binding["raw_digest"],
                "source": binding["source"],
            }
        )
    undeclared = tuple(
        sorted(
            row[2]
            for row in actual_capture_rows
            if json.loads(row[4]).get("capture_id") not in expected_capture_ids
            or row[3] != "cognitive-runtime-output-port-v1"
        )
    )
    if tuple(p["undeclared_capture_event_digests"]) != undeclared:
        raise IntegrityError("runtime cognitive undeclared capture inventory is false")
    expected_complete = (
        not undeclared
        and len(bound_capture_ids) == len(expected_capture_ids)
        and bound_capture_ids == set(expected_capture_ids)
        and len(actual_capture_rows) == len(expected_capture_ids)
        and tuple(sorted(bound_ordinals)) == tuple(range(len(expected_capture_ids)))
        and bool(p["capture_bindings"])
        and sum(bool(item["terminal"]) for item in p["capture_bindings"]) == 1
        and max(p["capture_bindings"], key=lambda item: item["ordinal"])["terminal"]
        is True
    )
    if p["capture_inventory_complete"] is not expected_complete:
        raise IntegrityError("runtime cognitive capture completeness is false")

    classification = p["classification_body"]
    try:
        reconstructed = DeterministicExperimentClassificationV1(
            spec_digest=classification["spec_digest"],
            status=ClassificationStatus(classification["status"]),
            observed_partition_digest=classification["observed_partition_digest"],
            prospective_partition_digests=tuple(
                classification["prospective_partition_digests"]
            ),
            prospective_predicate_digests=tuple(
                classification["prospective_predicate_digests"]
            ),
            matched_predicate_digests=tuple(
                classification["matched_predicate_digests"]
            ),
            observation_bindings=tuple(classification["observation_bindings"]),
            reason_codes=tuple(classification["reason_codes"]),
            classifier_version=classification["classifier_version"],
            learning_eligible=classification["learning_eligible"],
            accepted_set_change=classification["accepted_set_change"],
        )
    except (KeyError, TypeError, ValueError) as exc:
        raise IntegrityError(
            "runtime cognitive classification cannot be reconstructed"
        ) from exc
    if (
        reconstructed.digest != p["classification_digest"]
        or tuple(reconstructed.observation_bindings)
        != tuple(
            sorted(
                derived_classification_bindings,
                key=lambda item: item["observation_id"],
            )
        )
        or reconstructed.prospective_partition_digests
        != tuple(
            sorted(
                {item.outcome_partition_digest for item in executable.spec.predicates}
            )
        )
        or reconstructed.prospective_predicate_digests
        != tuple(sorted({item.predicate_digest for item in executable.spec.predicates}))
    ):
        raise IntegrityError("runtime cognitive classification is rebound")
    try:
        executable.spec.validate_classification(reconstructed)
    except (TypeError, ValueError) as exc:
        raise IntegrityError(
            "runtime cognitive matched predicates do not prove the partition"
        ) from exc
    positive = reconstructed.status is ClassificationStatus.OBSERVED
    if positive and (
        not expected_complete
        or p["terminal_event_kind"] != "WORKER_TERMINAL"
        or p["budget_event_kind"] != "BUDGET_SETTLED"
        or p["host_launch_proof_body"] is None
        or p["epistemic_classification"] != "structurally_observed_unverified"
    ):
        raise IntegrityError(
            "runtime cognitive positive classification lacks complete runtime lineage"
        )
    proof = p["host_launch_proof_body"]
    if proof is not None and (
        proof.get("assignment_event_digest") != p["assignment_event_digest"]
        or proof.get("assignment_event_receipt_digest")
        != p["assignment_event_receipt_digest"]
        or proof.get("experiment_digest") != p["experiment_digest"]
        or proof.get("executable_spec_digest") != p["executable_spec_digest"]
        or proof.get("executable_worker_view_digest")
        != executable.spec.worker_view_digest
        or proof.get("packet_digest") != p["context_packet_digest"]
        or proof.get("attempt_digest") != p["attempt_digest"]
        or proof.get("permit_digest") != p["permit_digest"]
        or proof.get("scope_digest") != p["scope_digest"]
        or proof.get("learning_eligible") is not False
        or proof.get("verification_resolved") is not False
    ):
        raise IntegrityError("runtime cognitive host proof is rebound")
    if assignment_schema_id == COGNITIVE_RUNTIME_REPRODUCTION_ASSIGNMENT_SCHEMA_ID:
        if proof is None:
            raise IntegrityError(
                "cognitive reproduction observation requires a host launch proof"
            )
        launch_claim = self._conn.execute(
            "SELECT payload_json FROM events WHERE event_digest=? "
            "AND kind='CONTEXT_PROMPT_LAUNCH_CLAIMED'",
            (proof.get("prompt_launch_claim_event_digest"),),
        ).fetchone()
        if launch_claim is None or json.loads(launch_claim[0]).get(
            "profile_digest"
        ) != assignment_payload.get("required_reproducer_profile_digest"):
            raise IntegrityError(
                "cognitive reproduction used a non-preregistered launch profile"
            )


def _validate_runtime_cognitive_prefix(
    self,
    *,
    p: dict[str, Any],
    observation_kind: str,
) -> None:
    state = self._state()
    if (
        p["verified_prefix_cutoff_seq"] != state.head_seq
        or p["verified_prefix_head_event_digest"] != state.head_event_digest
    ):
        raise IntegrityError("runtime cognitive observation used a stale prefix")
    prefix = self.receipt_field_resolver(
        cutoff_seq=state.head_seq
    ).verify_complete_through(state.head_seq)
    if (
        prefix.digest != p["verified_prefix_digest"]
        or prefix.cutoff_seq != p["verified_prefix_cutoff_seq"]
        or prefix.head_event_digest != p["verified_prefix_head_event_digest"]
    ):
        raise IntegrityError("runtime cognitive observation prefix is not complete")
    rows = self._conn.execute(
        "SELECT payload_json FROM events WHERE kind=? ORDER BY seq",
        (observation_kind,),
    ).fetchall()
    decoded = [json.loads(row[0]) for row in rows]
    if (
        sum(
            item.get("assignment_event_digest") == p["assignment_event_digest"]
            for item in decoded
        )
        != 1
    ):
        raise IntegrityError("runtime cognitive observation is not unique")
