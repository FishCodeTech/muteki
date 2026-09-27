"""C6 evaluation validators moved from sqlite_store.py."""
from __future__ import annotations

import json
from collections.abc import Mapping, Sequence
from typing import Any

from muteki.epistemic.contracts import (
    canonical_digest,
    canonical_json_bytes,
)

from muteki.epistemic.sqlite_types import (
    CommandEvent,
    IntegrityError,
    _C6_EVAL_OUTCOME_SCHEMA_ID,
    _C6_EVAL_OUTCOME_UNKNOWN_FIELDS,
    _C6_EVAL_OUTCOME_VERIFIED_FIELDS,
    _is_sha256,
)

def _validate_c6_eval_outcome_mutation(
    self, kind: str, payload: Mapping[str, Any]
) -> None:
    """Validate the checker-only, projection-free terminal declaration.

    This mutation intentionally writes no projection row.  Its purpose is to
    force the outcome event through a distinct store-local capability and to
    validate its complete transitive terminal-binding inventory atomically.
    The read-only evaluator still recomputes accounting and CAS closure later.
    """

    p = dict(payload)
    contracts = {
        "c6_eval_outcome_guard": (
            "C6_EVAL_OUTCOME_VERIFIED",
            "solved",
            _C6_EVAL_OUTCOME_VERIFIED_FIELDS,
        ),
        "c6_eval_outcome_unknown_guard": (
            "C6_EVAL_OUTCOME_UNKNOWN",
            "unknown",
            _C6_EVAL_OUTCOME_UNKNOWN_FIELDS,
        ),
    }
    if kind not in contracts:
        raise IntegrityError("unknown C6 checker outcome mutation")
    event_kind, required_result, fields = contracts[kind]
    if set(p) != fields:
        raise IntegrityError("C6 checker outcome payload shape is not versioned")
    if p.get("schema_id") != _C6_EVAL_OUTCOME_SCHEMA_ID:
        raise IntegrityError("C6 checker outcome schema diverged")
    if event_kind == "C6_EVAL_OUTCOME_VERIFIED":
        if p.get("result") not in {"solved", "clean_unsolved"}:
            raise IntegrityError("verified C6 checker outcome is not terminal")
    elif p.get("result") != required_result:
        raise IntegrityError("C6 checker UNKNOWN result diverged")
    if p.get("run_id") != self.run_id:
        raise IntegrityError("C6 checker outcome run identity diverged")
    digest_fields = {
        "assignment_digest",
        "evaluation_binding_digest",
        "scope_digest",
    }
    if event_kind == "C6_EVAL_OUTCOME_VERIFIED":
        digest_fields |= {
            "artifact_manifest_digest",
            "checker_build_digest",
            "checker_input_manifest_digest",
            "checker_output_digest",
            "checker_policy_digest",
            "complete_accounting_digest",
        }
    else:
        digest_fields.add("reason_digest")
    if any(not _is_sha256(p.get(name)) for name in digest_fields):
        raise IntegrityError("C6 checker outcome contains a malformed digest")

    terminal_digests = p.get("terminal_binding_event_digests")
    if (
        type(terminal_digests) is not list
        or not terminal_digests
        or terminal_digests != sorted(terminal_digests)
        or len(terminal_digests) != len(set(terminal_digests))
        or any(not _is_sha256(item) for item in terminal_digests)
    ):
        raise IntegrityError(
            "C6 checker outcome terminal binding inventory is not canonical"
        )

    current = self._state()
    current_scope = canonical_digest(
        {
            "execution_generation": current.execution_generation,
            "run_fence_epoch": current.run_fence_epoch,
            "run_id": current.run_id,
        }
    )
    if (
        current.run_execution.value != "running"
        or p["scope_digest"] != current_scope
    ):
        raise IntegrityError("C6 checker outcome is outside the running scope")

    payload_json = canonical_json_bytes(p).decode()
    outcome_rows = self._conn.execute(
        "SELECT command_id,event_id,actor,seq FROM events "
        "WHERE kind=? AND payload_json=?",
        (event_kind, payload_json),
    ).fetchall()
    expected_command = f"C6_EVAL_OUTCOME:{p['assignment_digest']}"
    expected_event = f"event:C6_EVAL_OUTCOME:{p['assignment_digest']}"
    if len(outcome_rows) != 1 or tuple(outcome_rows[0][:3]) != (
        expected_command,
        expected_event,
        "c6-evaluation-checker-authority",
    ):
        raise IntegrityError("C6 checker outcome command identity diverged")
    outcome_seq = int(outcome_rows[0][3])
    total_outcomes = self._conn.execute(
        "SELECT COUNT(*) FROM events WHERE kind IN "
        "('C6_EVAL_OUTCOME_VERIFIED','C6_EVAL_OUTCOME_UNKNOWN')"
    ).fetchone()
    if total_outcomes is None or int(total_outcomes[0]) != 1:
        raise IntegrityError("C6 checker outcome is not unique for the run")

    terminal_rows = self._conn.execute(
        "SELECT event_digest,payload_json,seq FROM events "
        "WHERE kind='C6_EVAL_TERMINAL_BOUND' ORDER BY event_digest"
    ).fetchall()
    if [str(row[0]) for row in terminal_rows] != terminal_digests:
        raise IntegrityError(
            "C6 checker outcome does not bind the complete terminal set"
        )
    attempt_count = self._conn.execute(
        "SELECT COUNT(*) FROM events WHERE kind='ATTEMPT_ADMITTED'"
    ).fetchone()
    if attempt_count is None or int(attempt_count[0]) != len(terminal_rows):
        raise IntegrityError("C6 checker outcome covers only part of the run")
    terminal_identity: set[tuple[str, str]] = set()
    manifest_digest = self._conn.execute(
        "SELECT manifest_digest FROM run_meta WHERE singleton=1"
    ).fetchone()
    if manifest_digest is None:
        raise IntegrityError("C6 checker outcome has no immutable run manifest")
    for terminal_digest, terminal_json, terminal_seq in terminal_rows:
        terminal = json.loads(terminal_json)
        if (
            int(terminal_seq) >= outcome_seq
            or terminal.get("evaluation_binding_digest")
            != p["evaluation_binding_digest"]
            or terminal.get("scope_digest") != p["scope_digest"]
        ):
            raise IntegrityError("C6 checker terminal lineage is rebound")
        launch_row = self._conn.execute(
            "SELECT payload_json FROM events WHERE event_digest=? "
            "AND kind='C6_EVAL_LAUNCH_BOUND'",
            (terminal.get("launch_binding_event_digest"),),
        ).fetchone()
        if launch_row is None:
            raise IntegrityError("C6 checker terminal has no launch parent")
        launch = json.loads(launch_row[0])
        attempt_row = self._conn.execute(
            "SELECT payload_json FROM events WHERE event_digest=? "
            "AND kind='C6_EVAL_ATTEMPT_BOUND'",
            (launch.get("attempt_binding_event_digest"),),
        ).fetchone()
        if attempt_row is None:
            raise IntegrityError("C6 checker launch has no attempt parent")
        attempt = json.loads(attempt_row[0])
        for name in (
            "attempt_digest",
            "attempt_id",
            "evaluation_binding_digest",
            "permit_digest",
            "permit_id",
            "scope_digest",
        ):
            if terminal.get(name) != launch.get(name) or launch.get(
                name
            ) != attempt.get(name):
                raise IntegrityError("C6 checker transitive lineage diverged")
        binding = attempt.get("evaluation_binding")
        if (
            type(binding) is not dict
            or binding.get("assignment_digest") != p["assignment_digest"]
            or binding.get("run_manifest_digest") != str(manifest_digest[0])
            or canonical_digest(binding) != p["evaluation_binding_digest"]
        ):
            raise IntegrityError("C6 checker assignment/run binding diverged")
        terminal_identity.add(
            (str(terminal.get("attempt_id")), str(terminal.get("permit_id")))
        )
    if len(terminal_identity) != len(terminal_rows):
        raise IntegrityError("C6 checker terminal identity is reused")

    forbidden = self._conn.execute(
        "SELECT kind FROM events WHERE kind IN "
        "('BUDGET_PESSIMISTICALLY_SETTLED','BUDGET_USAGE_UNKNOWN',"
        "'EFFECT_UNKNOWN','WORKER_UNKNOWN',"
        "'FLAG_ACCEPTED','GOAL_COMPLETED','EXECUTION_STOP_REQUESTED',"
        "'EXECUTION_SCOPE_DRAINED','S4E_CLOSURE_ATTESTED') LIMIT 1"
    ).fetchone()
    if forbidden is not None:
        raise IntegrityError(
            "C6 checker outcome cannot close UNKNOWN, accepted, or drained state"
        )
    for event_kind_required in (
        "BUDGET_SETTLED",
        "EFFECT_OBSERVED",
        "WORKER_TERMINAL",
    ):
        count = self._conn.execute(
            "SELECT COUNT(*) FROM events WHERE kind=? AND seq<?",
            (event_kind_required, outcome_seq),
        ).fetchone()
        if count is None or int(count[0]) != len(terminal_rows):
            raise IntegrityError(
                "C6 checker outcome precedes complete terminal accounting"
            )


def _is_c6_v2_observer_attempt_locked(self, attempt_id: object) -> bool:
    if type(attempt_id) is not str or not attempt_id:
        return False
    rows = self._conn.execute(
        "SELECT payload_json FROM events WHERE kind='C6_EVAL_V2_ATTEMPT_BOUND'"
    ).fetchall()
    matches = [
        json.loads(row[0])
        for row in rows
        if json.loads(row[0]).get("attempt_id") == attempt_id
    ]
    if len(matches) > 1:
        raise IntegrityError("C6 evaluation v2 observer identity is ambiguous")
    return bool(matches and matches[0].get("role") == "observer")


def _reject_c6_v2_observer_events_locked(
    self, events: Sequence[CommandEvent]
) -> None:
    for event in events:
        if event.kind not in {"PROGRESS_RECORDED", "ATTEMPT_BARREN"}:
            continue
        if self._is_c6_v2_observer_attempt_locked(event.payload.get("attempt_id")):
            raise IntegrityError("C6 evaluation v2 observer cannot update progress")


def _require_c6_v2_terminal_accounting_locked(
    self, events: Sequence[CommandEvent]
) -> None:
    terminal_events = tuple(
        event
        for event in events
        if event.kind in {"WORKER_TERMINAL", "WORKER_UNKNOWN"}
    )
    if not terminal_events:
        return
    rows = self._conn.execute(
        "SELECT payload_json FROM events "
        "WHERE kind='C6_EVAL_V2_LAUNCH_BOUND' ORDER BY seq"
    ).fetchall()
    launch_payloads = tuple(json.loads(row[0]) for row in rows)
    cognitive_assignment_rows = self._conn.execute(
        "SELECT payload_json FROM events "
        "WHERE kind='COGNITIVE_EXPERIMENT_ASSIGNED' ORDER BY seq"
    ).fetchall()
    cognitive_assignments = tuple(
        json.loads(row[0]) for row in cognitive_assignment_rows
    )
    event_kinds = {event.kind for event in events}
    for terminal in terminal_events:
        permit_id = terminal.payload.get("permit_id")
        matching = tuple(
            payload
            for payload in launch_payloads
            if payload.get("permit_id") == permit_id
        )
        if len(matching) > 1:
            raise IntegrityError("evaluation v2 launch identity is ambiguous")
        if not matching:
            continue
        bound_cognitive = tuple(
            payload
            for payload in cognitive_assignments
            if payload.get("permit_id") == permit_id
        )
        if len(bound_cognitive) > 1:
            raise IntegrityError(
                "v2 terminal has ambiguous cognitive assignment lineage"
            )
        if bound_cognitive and "COGNITIVE_EXECUTION_OBSERVED" not in event_kinds:
            raise IntegrityError(
                "cognitive-bound v2 terminal requires its atomic observation"
            )
        if not bound_cognitive and "COGNITIVE_EXECUTION_OBSERVED" in event_kinds:
            raise IntegrityError(
                "v2 terminal cannot mint an unassigned cognitive observation"
            )
        if "C6_EVAL_V2_TERMINAL_BOUND" not in event_kinds or not event_kinds & {
            "BUDGET_PESSIMISTICALLY_SETTLED",
            "BUDGET_SETTLED",
            "BUDGET_USAGE_UNKNOWN",
        }:
            raise IntegrityError(
                "evaluation v2 terminal requires atomic budget and sidecar closure"
            )


def _assert_c6_claims_closed_before_worker_terminal_locked(
    self, *, permit_id: object, worker_terminal_event_id: object
) -> None:
    """Prevent every worker-terminal path from overtaking a host launch claim."""

    if (
        type(permit_id) is not str
        or not permit_id
        or type(worker_terminal_event_id) is not str
        or not worker_terminal_event_id
    ):
        raise IntegrityError("C6 terminal guard has no exact permit identity")
    worker_terminal = self._conn.execute(
        "SELECT seq FROM events WHERE event_id=? AND kind IN "
        "('WORKER_TERMINAL','WORKER_UNKNOWN')",
        (worker_terminal_event_id,),
    ).fetchone()
    if worker_terminal is None:
        raise IntegrityError("C6 terminal guard has no canonical worker terminal")
    worker_terminal_seq = int(worker_terminal[0])
    claims = self._conn.execute(
        "SELECT payload_json FROM events WHERE kind='CONTEXT_PROMPT_LAUNCH_CLAIMED'"
    ).fetchall()
    terminals = self._conn.execute(
        "SELECT kind,seq,payload_json FROM events WHERE kind IN "
        "('CONTEXT_PROMPT_RELEASED','CONTEXT_PROMPT_UNKNOWN',"
        "'CONTEXT_PROMPT_PRELAUNCH_ABORTED')"
    ).fetchall()
    decoded_terminals = [
        (str(kind), int(seq), json.loads(raw_payload))
        for kind, seq, raw_payload in terminals
    ]
    for raw_claim in claims:
        claim = json.loads(raw_claim[0])
        if claim.get("permit_id") != permit_id:
            continue
        stage_id = claim.get("stage_id")
        if type(stage_id) is not str or not stage_id:
            raise IntegrityError("C6 host launch claim has no stage identity")
        matching = [
            (kind, seq, payload)
            for kind, seq, payload in decoded_terminals
            if payload.get("stage_id") == stage_id and seq < worker_terminal_seq
        ]
        if len(matching) != 1:
            raise IntegrityError(
                "worker terminal cannot overtake an unresolved C6 host claim"
            )


def _assert_c6_claims_closed_before_attempt_state_change_locked(
    self, *, attempt_id: object
) -> None:
    """Keep budget owner closure behind every claimed host launch.

    The local C6 interlock holds its process-local lock across the final
    durable validation and ``Popen``.  Budget settlement/UNKNOWN commands use
    the canonical store instead, so they must not be allowed to turn the same
    attempt or its reservations terminal in the small interval after that
    validation.  A prompt terminal is the durable hand-off: only RELEASED,
    PRELAUNCH_ABORTED, or UNKNOWN may precede the attempt-state transition.
    """

    if type(attempt_id) is not str or not attempt_id:
        raise IntegrityError("C6 budget guard has no exact attempt identity")
    attempt = self._conn.execute(
        "SELECT permit_id,scope_digest FROM runtime_attempts WHERE attempt_id=?",
        (attempt_id,),
    ).fetchone()
    if attempt is None:
        raise IntegrityError("C6 budget guard has no canonical attempt owner")
    permit_id, scope_digest = str(attempt[0]), str(attempt[1])
    claims = self._conn.execute(
        "SELECT payload_json FROM events WHERE kind='CONTEXT_PROMPT_LAUNCH_CLAIMED'"
    ).fetchall()
    terminals = self._conn.execute(
        "SELECT kind,payload_json FROM events WHERE kind IN "
        "('CONTEXT_PROMPT_RELEASED','CONTEXT_PROMPT_UNKNOWN',"
        "'CONTEXT_PROMPT_PRELAUNCH_ABORTED')"
    ).fetchall()
    decoded_terminals = [
        (str(kind), json.loads(raw_payload)) for kind, raw_payload in terminals
    ]
    for raw_claim in claims:
        claim = json.loads(raw_claim[0])
        if claim.get("permit_id") != permit_id:
            continue
        if (
            claim.get("attempt_id") != attempt_id
            or claim.get("scope_digest") != scope_digest
        ):
            raise IntegrityError("C6 host launch claim owner identity diverged")
        stage_id = claim.get("stage_id")
        permit_digest = claim.get("permit_digest")
        if (
            type(stage_id) is not str
            or not stage_id
            or type(permit_digest) is not str
            or not permit_digest
        ):
            raise IntegrityError("C6 host launch claim has no budget identity")
        matching = [
            (kind, payload)
            for kind, payload in decoded_terminals
            if payload.get("stage_id") == stage_id
            and payload.get("permit_digest") == permit_digest
        ]
        if len(matching) != 1:
            raise IntegrityError(
                "attempt state change cannot overtake an unresolved C6 host claim"
            )


def _assert_c6_scope_deactivation_is_closed_locked(
    self, *, events: Sequence[CommandEvent]
) -> None:
    """Keep a state transition from racing a claimed host Popen.

    The host interlock rechecks the active scope immediately before Popen, but
    a separate store connection could otherwise commit a pause/degrade/stop in
    the interval after that read.  Every event that makes the current scope
    non-active is therefore rejected while it has a claimed prompt without one
    prior prompt terminal.  Supervisor-driven shutdown revokes the interlock
    and appends ``PRELAUNCH_ABORTED``/``UNKNOWN`` first, so normal shutdown
    remains ordered and replayable.
    """

    deactivating_kinds = {
        "AUTHORITY_DEGRADED",
        "BOOT_VERIFYING",
        "INTEGRITY_DEGRADED",
        "SEARCH_PAUSED",
        "GOAL_COMPLETED",
        "EXECUTION_STOP_REQUESTED",
        "EXECUTION_SCOPE_DRAINED",
        "GOAL_INVALIDATED",
        "RUN_ARCHIVE_REQUESTED",
        "RUN_ARCHIVED",
        "S4E_CLOSURE_ATTESTED",
    }
    if not any(event.kind in deactivating_kinds for event in events):
        return
    state = self._state()
    scope_digest = canonical_digest(
        {
            "execution_generation": state.execution_generation,
            "run_fence_epoch": state.run_fence_epoch,
            "run_id": state.run_id,
        }
    )
    claims = self._conn.execute(
        "SELECT payload_json FROM events WHERE kind='CONTEXT_PROMPT_LAUNCH_CLAIMED'"
    ).fetchall()
    terminals = self._conn.execute(
        "SELECT kind,payload_json FROM events WHERE kind IN "
        "('CONTEXT_PROMPT_RELEASED','CONTEXT_PROMPT_UNKNOWN',"
        "'CONTEXT_PROMPT_PRELAUNCH_ABORTED')"
    ).fetchall()
    decoded_terminals = [
        (str(kind), json.loads(raw_payload)) for kind, raw_payload in terminals
    ]
    for raw_claim in claims:
        claim = json.loads(raw_claim[0])
        if claim.get("scope_digest") != scope_digest:
            continue
        stage_id = claim.get("stage_id")
        permit_digest = claim.get("permit_digest")
        if (
            type(stage_id) is not str
            or not stage_id
            or type(permit_digest) is not str
            or not permit_digest
        ):
            raise IntegrityError("C6 host launch claim has no scope identity")
        matching = [
            (kind, payload)
            for kind, payload in decoded_terminals
            if payload.get("stage_id") == stage_id
            and payload.get("permit_digest") == permit_digest
        ]
        if len(matching) != 1:
            raise IntegrityError(
                "scope transition cannot overtake an unresolved C6 host claim"
            )

    cognitive_closure_kinds = {
        "GOAL_COMPLETED",
        "EXECUTION_SCOPE_DRAINED",
        "RUN_ARCHIVE_REQUESTED",
        "RUN_ARCHIVED",
        "S4E_CLOSURE_ATTESTED",
    }
    if not any(event.kind in cognitive_closure_kinds for event in events):
        return

    # A launched default-off cognitive assignment is part of the same durable worker
    # lifecycle.  Restart must reconcile it to one structural observation
    # before any scope drain/closure can make terminalization impossible.
    assignments = self._conn.execute(
        "SELECT event_digest,payload_json FROM events "
        "WHERE kind='COGNITIVE_EXPERIMENT_ASSIGNED' ORDER BY seq"
    ).fetchall()
    observations = self._conn.execute(
        "SELECT payload_json FROM events "
        "WHERE kind='COGNITIVE_EXECUTION_OBSERVED' ORDER BY seq"
    ).fetchall()
    decoded_observations = [json.loads(row[0]) for row in observations]
    launches = self._conn.execute(
        "SELECT payload_json FROM events "
        "WHERE kind='C6_EVAL_V2_LAUNCH_BOUND' ORDER BY seq"
    ).fetchall()
    launched_permits = {json.loads(row[0]).get("permit_id") for row in launches}
    for assignment_digest, raw_assignment in assignments:
        assignment = json.loads(raw_assignment)
        if assignment.get("scope_digest") != scope_digest:
            continue
        if assignment.get("permit_id") not in launched_permits:
            # No worker/effect boundary was crossed.  The assignment stays
            # visibly unresolved for offline learning, but cannot suppress
            # a real degradation or make shutdown truth impossible.
            continue
        matching_observations = [
            item
            for item in decoded_observations
            if item.get("assignment_event_digest") == assignment_digest
        ]
        if len(matching_observations) != 1:
            raise IntegrityError(
                "scope transition cannot overtake a pending cognitive assignment"
            )
