"""Reproduction validators moved from sqlite_store.py."""
from __future__ import annotations

import json
from collections.abc import Mapping
from typing import Any

from muteki.epistemic.contracts import (
    canonical_digest,
    canonical_json_bytes,
)

from muteki.epistemic.sqlite_types import (
    IntegrityError,
)

def _validate_cognitive_reproduction_source_locked(
    self,
    payload: Mapping[str, Any],
) -> None:
    """Recompute one pre-outcome reproduction binding from canonical history."""

    from muteki.epistemic.cas import ReceiptCAS
    from muteki.epistemic.cognitive_events_v1 import (
        COGNITIVE_EXPERIMENT_ASSIGNED,
        COGNITIVE_RUNTIME_EXECUTABLE_ASSIGNMENT_SCHEMA_ID,
        CognitiveExperimentBindingV1,
        cognitive_runtime_reproduction_assignment_payload,
    )
    from muteki.runtime.executable_experiment_v1 import (
        ExecutableExperimentBindingV1,
    )

    p = dict(payload)
    source_assignment = self._conn.execute(
        "SELECT seq,event_digest,payload_json FROM events "
        "WHERE event_digest=? AND kind=?",
        (
            p["source_assignment_event_digest"],
            COGNITIVE_EXPERIMENT_ASSIGNED,
        ),
    ).fetchone()
    source_observation = self._conn.execute(
        "SELECT seq,event_digest,payload_json FROM events "
        "WHERE event_digest=? AND kind='COGNITIVE_EXECUTION_OBSERVED'",
        (p["source_observation_event_digest"],),
    ).fetchone()
    if source_assignment is None or source_observation is None:
        raise IntegrityError("cognitive reproduction source lineage is absent")
    source_assignment_payload = json.loads(source_assignment[2])
    source_observation_payload = json.loads(source_observation[2])
    if (
        source_assignment_payload.get("schema_id")
        != COGNITIVE_RUNTIME_EXECUTABLE_ASSIGNMENT_SCHEMA_ID
        or source_observation_payload.get("assignment_event_digest")
        != source_assignment[1]
        or source_assignment[0] >= source_observation[0]
        or source_observation[0] > self._state().head_seq
        or self.receipt_digest_for_event(source_assignment[1])
        != p["source_assignment_event_receipt_digest"]
        or self.receipt_digest_for_event(source_observation[1])
        != p["source_observation_event_receipt_digest"]
    ):
        raise IntegrityError("cognitive reproduction source lineage diverged")
    prior_reproductions = [
        item
        for item in self.event_rows(kind=COGNITIVE_EXPERIMENT_ASSIGNED)
        if item["payload"].get("source_observation_event_digest")
        == p["source_observation_event_digest"]
    ]
    if len(prior_reproductions) != 1:
        raise IntegrityError("one observation may have exactly one reproducer")

    reproduction = ExecutableExperimentBindingV1.from_canonical(
        p["executable_experiment_binding_body"]
    )
    binding = CognitiveExperimentBindingV1(
        assignment_body=p["assignment_body"],
        experiment_body=p["experiment_body"],
        h5_request_body=p["h5_request_body"],
        h5_selection_plan_body=p["h5_selection_plan_body"],
        decision_prefix_digest=p["decision_prefix_digest"],
        decision_cutoff_seq=p["decision_cutoff_seq"],
        decision_head_event_digest=p["decision_head_event_digest"],
    )
    base = self._conn.execute(
        "SELECT payload_json FROM events WHERE event_id=?",
        (p["base_event_id"],),
    ).fetchone()
    assert base is not None
    expected = cognitive_runtime_reproduction_assignment_payload(
        binding=binding,
        admission_payload=json.loads(base[0]),
        executable_experiment=reproduction,
        source_assignment_event_digest=source_assignment[1],
        source_assignment_event_receipt_digest=(
            p["source_assignment_event_receipt_digest"]
        ),
        source_assignment_payload=source_assignment_payload,
        source_observation_event_digest=source_observation[1],
        source_observation_event_receipt_digest=(
            p["source_observation_event_receipt_digest"]
        ),
        source_observation_payload=source_observation_payload,
        required_reproducer_profile_digest=(
            p["required_reproducer_profile_digest"]
        ),
    )
    if canonical_json_bytes(expected) != canonical_json_bytes(p):
        raise IntegrityError("cognitive reproduction assignment is not derived")

    proof = source_observation_payload.get("host_launch_proof_body")
    claim_digest = (
        proof.get("prompt_launch_claim_event_digest")
        if isinstance(proof, dict)
        else None
    )
    source_claim = self._conn.execute(
        "SELECT payload_json FROM events WHERE event_digest=? "
        "AND kind='CONTEXT_PROMPT_LAUNCH_CLAIMED'",
        (claim_digest,),
    ).fetchone()
    if source_claim is None:
        raise IntegrityError("cognitive reproduction source launch is absent")
    source_profile_digest = json.loads(source_claim[0]).get("profile_digest")
    if source_profile_digest == p["required_reproducer_profile_digest"]:
        raise IntegrityError("cognitive reproducer profile is not distinct")

    # Outcome blindness starts with a structural rule: O2 receives the exact
    # pre-O1 decision context, not caller-authored post-O1 prose.  Attempt and
    # packet identities are intentionally fresh, but every semantic context
    # field must replay byte-equivalent to the source decision occurrence.
    source_packet = source_assignment_payload["context_packet_binding_body"]
    reproduction_packet = p["context_packet_binding_body"]
    decision_rows = self._conn.execute(
        "SELECT seq,payload_json FROM events "
        "WHERE kind='RUNTIME_CONTEXT_DECISION_REGISTERED'"
    ).fetchall()

    def decision_for(decision_id: str) -> tuple[int, dict[str, Any]]:
        matches = [
            (int(row[0]), json.loads(row[1]))
            for row in decision_rows
            if json.loads(row[1]).get("decision_id") == decision_id
        ]
        if len(matches) != 1:
            raise IntegrityError(
                "cognitive reproduction decision lineage is absent or ambiguous"
            )
        return matches[0]

    source_decision_seq, source_context = decision_for(source_packet["decision_id"])
    reproduction_decision_seq, reproduction_context = decision_for(
        reproduction_packet["decision_id"]
    )
    inherited_fields = (
        "acceptance_boundary",
        "decision_need",
        "effect_ambiguity",
        "feature_state_digest",
        "non_negotiable_policy",
        "objective",
        "remaining_budget",
        "scope_digest",
    )
    reproduction_assignment_seq = prior_reproductions[0]["seq"]
    if (
        source_decision_seq >= source_assignment[0]
        or reproduction_decision_seq >= reproduction_assignment_seq
        or any(
            source_context.get(name) != reproduction_context.get(name)
            for name in inherited_fields
        )
        or source_context.get("context_digest")
        != reproduction_context.get("context_digest")
    ):
        raise IntegrityError(
            "cognitive reproduction context is not inherited pre-outcome"
        )

    cas = ReceiptCAS(self.path.parent / "receipt-cas")
    packet_bytes = cas.read_verified(
        p["context_packet_binding_body"]["packet_digest"]
    )
    worker_view_bytes = reproduction.spec.worker_view_bytes
    for withheld in p["withheld_source_digest_set"]:
        marker = withheld.encode("ascii")
        if marker in packet_bytes or marker in worker_view_bytes:
            raise IntegrityError(
                "cognitive reproduction leaked source outcome data"
            )


def _validate_reproduction_launch_lineage_locked(
    self,
    *,
    payload: Mapping[str, Any],
    launch: Mapping[str, Any],
    own_seq: int,
) -> None:
    """Rebind a declared/actual snapshot to the existing C6 claim chain."""

    from muteki.epistemic.cas import ReceiptCAS

    lineage = launch["canonical_lineage"]
    stage = self._conn.execute(
        "SELECT seq,payload_json FROM events WHERE event_digest=? "
        "AND kind='CONTEXT_PROMPT_STAGED'",
        (lineage["stage_event_digest"],),
    ).fetchone()
    invocation = self._conn.execute(
        "SELECT seq,payload_json FROM events WHERE event_digest=? "
        "AND kind='CONTEXT_PROMPT_INVOCATION_BOUND'",
        (lineage["invocation_event_digest"],),
    ).fetchone()
    claim = self._conn.execute(
        "SELECT seq,payload_json FROM events WHERE event_digest=? "
        "AND kind='CONTEXT_PROMPT_LAUNCH_CLAIMED'",
        (lineage["claim_event_digest"],),
    ).fetchone()
    if stage is None or invocation is None or claim is None:
        raise IntegrityError("reproduction launch canonical lineage is absent")
    stage_payload = json.loads(stage[1])
    invocation_payload = json.loads(invocation[1])
    claim_payload = json.loads(claim[1])
    if not (
        stage[0]
        == lineage["stage_seq"]
        < invocation[0]
        == lineage["invocation_seq"]
        < claim[0]
        == lineage["claim_seq"]
        < own_seq
    ):
        raise IntegrityError("reproduction launch evidence ordering is false")
    for event_digest, expected_receipt in (
        (lineage["stage_event_digest"], lineage["stage_event_receipt_digest"]),
        (
            lineage["invocation_event_digest"],
            lineage["invocation_event_receipt_digest"],
        ),
        (lineage["claim_event_digest"], lineage["claim_event_receipt_digest"]),
    ):
        if self.receipt_digest_for_event(event_digest) != expected_receipt:
            raise IntegrityError("reproduction launch receipt lineage diverged")
    if (
        stage_payload.get("permit_digest") != payload["permit_digest"]
        or invocation_payload.get("permit_digest") != payload["permit_digest"]
        or claim_payload.get("permit_digest") != payload["permit_digest"]
        or claim_payload.get("permit_id") != payload["permit_id"]
        or stage_payload.get("stage_id") != launch["stage_id"]
        or invocation_payload.get("stage_id") != launch["stage_id"]
        or claim_payload.get("stage_id") != launch["stage_id"]
        or stage_payload.get("prompt_artifact_digest")
        != launch["prompt_artifact_digest"]
        or stage_payload.get("prompt_byte_count")
        != launch["full_prompt_byte_count"]
        or invocation_payload.get("invocation_id") != launch["invocation_id"]
        or claim_payload.get("invocation_id") != launch["invocation_id"]
        or invocation_payload.get("argv_artifact_digest")
        != launch["argv_artifact_digest"]
        or invocation_payload.get("argv_byte_count") != launch["argv_byte_count"]
        or claim_payload.get("claim_id") != launch["claim_id"]
        or claim_payload.get("launch_material_digest")
        != launch["launch_material_digest"]
        or claim_payload.get("profile_digest") != launch["launch_profile_digest"]
    ):
        raise IntegrityError("reproduction launch objects were rebound")
    cas = ReceiptCAS(self.path.parent / "receipt-cas")
    try:
        raw = cas.read_verified(launch["launch_material_digest"])
        material = json.loads(raw)
    except (TypeError, ValueError, json.JSONDecodeError) as exc:
        raise IntegrityError("reproduction launch material cannot replay") from exc
    if canonical_json_bytes(material) != raw:
        raise IntegrityError("reproduction launch material is non-canonical")
    if (
        material.get("argv_artifact_digest") != launch["argv_artifact_digest"]
        or material.get("cwd_digest") != launch["cwd_digest"]
        or tuple(material.get("environment") or ()) != tuple(launch["environment"])
        or material.get("profile_digest") != launch["launch_profile_digest"]
    ):
        raise IntegrityError(
            "reproduction launch snapshot is not the claim material"
        )
    prior_terminals = self._conn.execute(
        "SELECT seq FROM events WHERE kind IN "
        "('CONTEXT_PROMPT_RELEASED','CONTEXT_PROMPT_PRELAUNCH_ABORTED',"
        "'CONTEXT_PROMPT_UNKNOWN') AND json_extract(payload_json,'$.permit_digest')=? "
        "AND seq<?",
        (payload["permit_digest"], own_seq),
    ).fetchall()
    if prior_terminals:
        raise IntegrityError(
            "reproduction evidence was backfilled after launch terminal"
        )


def _validate_cognitive_reproduction_prelaunch_mutation(
    self, payload: Mapping[str, Any]
) -> None:
    from muteki.epistemic.cas import ReceiptCAS
    from muteki.runtime.cognitive_reproduction_evidence_v1 import (
        COGNITIVE_REPRODUCTION_PRELAUNCH_DECLARED,
        validate_prelaunch_declaration_payload_shape,
    )

    try:
        validate_prelaunch_declaration_payload_shape(payload)
    except (TypeError, ValueError) as exc:
        raise IntegrityError("reproduction prelaunch declaration is false") from exc
    p = dict(payload)
    own = self._conn.execute(
        "SELECT seq,event_id FROM events WHERE kind=? AND payload_json=?",
        (
            COGNITIVE_REPRODUCTION_PRELAUNCH_DECLARED,
            canonical_json_bytes(p).decode(),
        ),
    ).fetchone()
    if own is None:
        raise IntegrityError("reproduction declaration event is absent")
    if p["run_id"] != self.run_id:
        raise IntegrityError("reproduction declaration crossed runs")
    reproduction = p["reproduction_assignment"]
    assignment = self._conn.execute(
        "SELECT seq,payload_json FROM events WHERE event_digest=? "
        "AND kind='COGNITIVE_EXPERIMENT_ASSIGNED'",
        (reproduction.get("assignment_event_digest"),),
    ).fetchone()
    source = p["source"]
    source_assignment = self._conn.execute(
        "SELECT seq,payload_json FROM events WHERE event_digest=? "
        "AND kind='COGNITIVE_EXPERIMENT_ASSIGNED'",
        (source.get("assignment_event_digest"),),
    ).fetchone()
    source_observation = self._conn.execute(
        "SELECT seq,payload_json FROM events WHERE event_digest=? "
        "AND kind='COGNITIVE_EXECUTION_OBSERVED'",
        (source.get("observation_event_digest"),),
    ).fetchone()
    source_claim = self._conn.execute(
        "SELECT seq,payload_json FROM events WHERE event_digest=? "
        "AND kind='CONTEXT_PROMPT_LAUNCH_CLAIMED'",
        (source.get("launch_claim_event_digest"),),
    ).fetchone()
    if (
        assignment is None
        or source_assignment is None
        or source_observation is None
        or source_claim is None
    ):
        raise IntegrityError("reproduction declaration source lineage is absent")
    assignment_payload = json.loads(assignment[1])
    source_assignment_payload = json.loads(source_assignment[1])
    source_observation_payload = json.loads(source_observation[1])
    source_claim_payload = json.loads(source_claim[1])
    if (
        assignment_payload.get("schema_id")
        != "muteki.cognitive-experiment-assigned.runtime-context-reproduction.v1"
        or assignment_payload.get("permit_digest") != p["permit_digest"]
        or assignment_payload.get("permit_id") != p["permit_id"]
        or assignment_payload.get("scope_digest") != p["scope_digest"]
        or assignment_payload.get("world_epoch_digest") != p["world_epoch_digest"]
        or assignment_payload.get("source_assignment_event_digest")
        != source["assignment_event_digest"]
        or assignment_payload.get("source_observation_event_digest")
        != source["observation_event_digest"]
        or reproduction.get("assignment_seq") != assignment[0]
        or reproduction.get("experiment_digest")
        != assignment_payload.get("experiment_digest")
        or reproduction.get("reproduction_kernel_digest")
        != assignment_payload.get("reproduction_kernel_digest")
        or self.receipt_digest_for_event(reproduction["assignment_event_digest"])
        != reproduction["assignment_event_receipt_digest"]
        or source.get("assignment_seq") != source_assignment[0]
        or source.get("observation_seq") != source_observation[0]
        or source_observation_payload.get("assignment_event_digest")
        != source["assignment_event_digest"]
        or source_claim_payload.get("launch_material_digest")
        != source.get("launch_material_digest")
        or source_claim_payload.get("profile_digest")
        != source.get("launch_profile_digest")
    ):
        raise IntegrityError("reproduction declaration semantic lineage diverged")
    for event_digest, expected_receipt in (
        (
            source["assignment_event_digest"],
            source["assignment_event_receipt_digest"],
        ),
        (
            source["observation_event_digest"],
            source["observation_event_receipt_digest"],
        ),
        (
            source["launch_claim_event_digest"],
            source["launch_claim_event_receipt_digest"],
        ),
    ):
        if self.receipt_digest_for_event(event_digest) != expected_receipt:
            raise IntegrityError("reproduction source receipt lineage diverged")
    fence = p["source_fence"]
    if (
        fence.get("source_assignment_event_digest")
        != source["assignment_event_digest"]
        or fence.get("cutoff_seq")
        != source_assignment_payload.get("decision_cutoff_seq")
        or fence.get("prefix_digest")
        != source_assignment_payload.get("decision_prefix_digest")
        or fence.get("prefix_head_event_digest")
        != source_assignment_payload.get("decision_head_event_digest")
        or fence.get("source_observation_seq") != source_observation[0]
    ):
        raise IntegrityError("reproduction source pre-outcome fence is false")
    try:
        source_material_raw = ReceiptCAS(
            self.path.parent / "receipt-cas"
        ).read_verified(source["launch_material_digest"])
        source_material = json.loads(source_material_raw)
    except (TypeError, ValueError, json.JSONDecodeError) as exc:
        raise IntegrityError("source launch material cannot replay") from exc
    if canonical_json_bytes(source_material) != source_material_raw or fence.get(
        "source_workspace_identity_digest"
    ) != source_material.get("cwd_digest"):
        raise IntegrityError("source workspace identity is false")
    source_names = {
        item.get("name")
        for item in source_material.get("environment", ())
        if isinstance(item, Mapping)
    }
    if source.get("home_identity_known") is (
        "HOME" not in source_names
    ) or source.get("session_identity_known") is (
        "MUTEKI_COGNITIVE_SESSION_ID" not in source_names
    ):
        raise IntegrityError("source session identity completeness is false")
    if not (
        source_assignment[0]
        < source_observation[0]
        < assignment[0]
        < p["declared_launch"]["canonical_lineage"]["stage_seq"]
        < own[0]
    ):
        raise IntegrityError("reproduction declaration event ordering is false")
    self._validate_reproduction_launch_lineage_locked(
        payload=p,
        launch=p["declared_launch"],
        own_seq=own[0],
    )
    duplicates = self._conn.execute(
        "SELECT COUNT(*) FROM events WHERE kind=? "
        "AND json_extract(payload_json,'$.permit_digest')=?",
        (COGNITIVE_REPRODUCTION_PRELAUNCH_DECLARED, p["permit_digest"]),
    ).fetchone()
    if duplicates is None or duplicates[0] != 1:
        raise IntegrityError("reproduction declaration is not unique")


def _validate_cognitive_reproduction_launch_witness_mutation(
    self, payload: Mapping[str, Any]
) -> None:
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

    try:
        validate_launch_witness_payload_shape(payload)
    except (TypeError, ValueError) as exc:
        raise IntegrityError("reproduction launch witness is false") from exc
    p = dict(payload)
    own = self._conn.execute(
        "SELECT seq FROM events WHERE kind=? AND payload_json=?",
        (
            COGNITIVE_REPRODUCTION_LAUNCH_WITNESSED,
            canonical_json_bytes(p).decode(),
        ),
    ).fetchone()
    declaration = self._conn.execute(
        "SELECT seq,payload_json FROM events WHERE event_digest=? AND kind=?",
        (
            p["declaration_event_digest"],
            COGNITIVE_REPRODUCTION_PRELAUNCH_DECLARED,
        ),
    ).fetchone()
    if own is None or declaration is None:
        raise IntegrityError(
            "reproduction declaration/witness occurrence is absent"
        )
    declared = json.loads(declaration[1])
    try:
        validate_prelaunch_declaration_payload_shape(declared)
    except (TypeError, ValueError) as exc:
        raise IntegrityError("witness predecessor declaration is false") from exc
    if (
        p["run_id"] != self.run_id
        or p["run_id"] != declared["run_id"]
        or p["permit_digest"] != declared["permit_digest"]
        or p["permit_id"] != declared["permit_id"]
        or p["scope_digest"] != declared["scope_digest"]
        or p["world_epoch_digest"] != declared["world_epoch_digest"]
        or canonical_digest(declared) != p["declaration_payload_digest"]
        or self.receipt_digest_for_event(p["declaration_event_digest"])
        != p["declaration_event_receipt_digest"]
        or declaration[0] >= own[0]
    ):
        raise IntegrityError("reproduction witness predecessor was rebound")
    self._validate_reproduction_launch_lineage_locked(
        payload=p,
        launch=p["actual_launch"],
        own_seq=own[0],
    )
    try:
        witness = reconstruct_reproduction_witness(
            source_fence_body=declared["source_fence"],
            declared_launch=declared["declared_launch"],
            actual_launch=p["actual_launch"],
        )
        assessment = assess_cognitive_reproduction_witness(witness)
    except (KeyError, TypeError, ValueError) as exc:
        raise IntegrityError(
            "canonical reproduction witness cannot replay"
        ) from exc
    reasons = [f"witness:{reason.value}" for reason in assessment.reason_codes]
    source = declared["source"]
    if source["home_identity_known"] is not True:
        reasons.append("source_home_identity_unknown")
    if source["session_identity_known"] is not True:
        reasons.append("source_session_identity_unknown")
    if p["actual_launch"]["home_identity_present"] is not True:
        reasons.append("reproduction_home_identity_missing")
    if p["actual_launch"]["session_identity_present"] is not True:
        reasons.append("reproduction_session_identity_missing")
    if p["actual_launch"]["input_snapshot"]["complete"] is not True:
        reasons.append("workspace_snapshot_incomplete")
    if p["actual_launch"]["input_channel_containment"] != "sealed_containment":
        reasons.append("external_input_channel_containment_unproven")
    if canonical_digest(p["actual_launch"]) != declared["declared_launch_digest"]:
        reasons.append("declared_actual_launch_material_changed")
    reason_codes = tuple(dict.fromkeys(reasons))
    expected_status = (
        "preregistered_exact_shadow"
        if not reason_codes
        and assessment.status is ReproductionWitnessStatusV1.OUTCOME_BLIND
        else "held_unknown"
    )
    if (
        witness.canonical_body() != p["witness_body"]
        or witness.digest != p["witness_digest"]
        or assessment.canonical_body() != p["witness_assessment"]
        or assessment.digest != p["witness_assessment_digest"]
        or reason_codes != tuple(p["policy_reason_codes"])
        or expected_status != p["evidence_status"]
    ):
        raise IntegrityError("reproduction witness or assessment was fabricated")
    duplicates = self._conn.execute(
        "SELECT COUNT(*) FROM events WHERE kind=? "
        "AND json_extract(payload_json,'$.permit_digest')=?",
        (COGNITIVE_REPRODUCTION_LAUNCH_WITNESSED, p["permit_digest"]),
    ).fetchone()
    if duplicates is None or duplicates[0] != 1:
        raise IntegrityError("reproduction launch witness is not unique")
