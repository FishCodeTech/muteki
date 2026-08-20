"""Fail-closed resolution and one-shot claiming of canonical attempt permits."""

from __future__ import annotations

import json
from dataclasses import dataclass
from typing import Any, Mapping

from muteki.epistemic.contracts import (
    EventEnvelopeV2,
    canonical_digest,
    canonical_json_bytes,
)
from muteki.epistemic.sqlite_store import (
    CommandEvent,
    EpistemicSQLiteStore,
    IdempotencyConflict,
    IntegrityError,
    ProjectionMutation,
)
from muteki.runtime.contracts import (
    AttemptPermit,
    ExecutionScope,
    RUNTIME_EVALUATION_EXECUTABLE_SPLITS_V2,
    RuntimeEvaluationBindingV2,
)


class PermitResolutionError(RuntimeError):
    """The supplied permit cannot be proven launchable from canonical state."""


@dataclass(frozen=True, slots=True)
class ResolvedPermit:
    admission_event_digest: str
    attempt_id: str
    permit_digest: str


def _same_json(left: Any, right: Any) -> bool:
    return canonical_json_bytes(left) == canonical_json_bytes(right)


def _exact_int(value: Any, expected: int) -> bool:
    return type(value) is int and type(expected) is int and value == expected


class CanonicalPermitResolver:
    """Resolve a permit from immutable admission evidence and claim one launch.

    Resolution and the launch command run under the store's single-writer lock.
    This deliberately uses the store's narrow internal projection connection until
    permit lifecycle queries become a first-class epistemic-store port.
    """

    def __init__(self, *, store: EpistemicSQLiteStore, scope: ExecutionScope) -> None:
        self._store = store
        self._scope = scope

    def claim_launch(self, permit: AttemptPermit, *, now_ns: int) -> ResolvedPermit:
        return self._claim_launch(permit, now_ns=now_ns, shadow=False, shadow_v2=False)

    def claim_shadow_launch(
        self, permit: AttemptPermit, *, now_ns: int
    ) -> ResolvedPermit:
        return self._claim_launch(permit, now_ns=now_ns, shadow=True, shadow_v2=False)

    def claim_shadow_launch_v2(
        self,
        permit: AttemptPermit,
        *,
        runtime_binding: RuntimeEvaluationBindingV2,
        now_ns: int,
    ) -> ResolvedPermit:
        """Claim one launch for an exact C6 v2 role-bound permit."""

        if type(runtime_binding) is not RuntimeEvaluationBindingV2:
            raise PermitResolutionError(
                "v2 launch requires an exact RuntimeEvaluationBindingV2"
            )
        if runtime_binding.split not in RUNTIME_EVALUATION_EXECUTABLE_SPLITS_V2:
            raise PermitResolutionError(
                "sealed_final requires separate evaluator opening authority"
            )
        return self._claim_launch(
            permit,
            now_ns=now_ns,
            shadow=False,
            shadow_v2=True,
            runtime_binding_v2=runtime_binding,
        )

    def _claim_launch(
        self,
        permit: AttemptPermit,
        *,
        now_ns: int,
        shadow: bool,
        shadow_v2: bool,
        runtime_binding_v2: RuntimeEvaluationBindingV2 | None = None,
    ) -> ResolvedPermit:
        if type(now_ns) is not int or now_ns < 0:
            raise PermitResolutionError("launch time must be a non-negative integer")
        if type(permit) is not AttemptPermit:
            raise PermitResolutionError("launch requires an exact AttemptPermit")
        if shadow and shadow_v2:
            raise PermitResolutionError(
                "shadow launch boundaries cannot be combined"
            )

        # commit_command uses the same RLock, so validation cannot race a budget
        # settlement, UNKNOWN transition, or competing launch claim.
        with self._store._lock:
            resolved = self._resolve_locked(permit, now_ns=now_ns)
            attempt_sidecar: dict[str, Any] | None = None
            attempt_sidecar_digest = ""
            v2_attempt_sidecar: dict[str, Any] | None = None
            v2_attempt_sidecar_digest = ""
            shadow_admissions = [
                row
                for row in self._store.event_rows(kind="C6_EVAL_ATTEMPT_BOUND")
                if row["payload"].get("permit_id") == permit.permit_id
            ]
            v2_admissions = [
                row
                for row in self._store.event_rows(kind="C6_EVAL_V2_ATTEMPT_BOUND")
                if row["payload"].get("permit_id") == permit.permit_id
            ]
            if not shadow and not shadow_v2 and (shadow_admissions or v2_admissions):
                raise PermitResolutionError(
                    "shadow admission requires the shadow launch boundary"
                )
            if shadow and v2_admissions:
                raise PermitResolutionError(
                    "v2 shadow admission requires the v2 shadow launch boundary"
                )
            if shadow_v2 and shadow_admissions:
                raise PermitResolutionError(
                    "v1 shadow admission requires the v1 shadow launch boundary"
                )
            if shadow:
                sidecar_rows = self._store._conn.execute(
                    "SELECT event_digest,payload_json FROM events "
                    "WHERE kind='C6_EVAL_ATTEMPT_BOUND'"
                ).fetchall()
                matching = [
                    (str(row[0]), json.loads(row[1]))
                    for row in sidecar_rows
                    if json.loads(row[1]).get("permit_id") == permit.permit_id
                ]
                if len(matching) != 1:
                    raise PermitResolutionError(
                        "shadow launch has no unique evaluation attempt binding"
                    )
                attempt_sidecar_digest, attempt_sidecar = matching[0]
            if shadow_v2:
                sidecar_rows = self._store._conn.execute(
                    "SELECT event_digest,payload_json FROM events "
                    "WHERE kind='C6_EVAL_V2_ATTEMPT_BOUND'"
                ).fetchall()
                matching = [
                    (str(row[0]), json.loads(row[1]))
                    for row in sidecar_rows
                    if json.loads(row[1]).get("permit_id") == permit.permit_id
                ]
                if len(matching) != 1:
                    raise PermitResolutionError(
                        "v2 shadow launch has no unique evaluation attempt binding"
                    )
                v2_attempt_sidecar_digest, v2_attempt_sidecar = matching[0]
                runtime_binding_body = v2_attempt_sidecar.get("runtime_binding")
                if type(runtime_binding_body) is not dict:
                    raise PermitResolutionError(
                        "v2 shadow launch runtime binding is absent"
                    )
                assert runtime_binding_v2 is not None
                try:
                    stored_runtime_binding = (
                        RuntimeEvaluationBindingV2.from_canonical(
                            runtime_binding_body
                        )
                    )
                except (TypeError, ValueError) as exc:
                    raise PermitResolutionError(
                        "v2 shadow launch runtime binding is false"
                    ) from exc
                if (
                    stored_runtime_binding.split
                    not in RUNTIME_EVALUATION_EXECUTABLE_SPLITS_V2
                ):
                    raise PermitResolutionError(
                        "sealed_final requires separate evaluator opening authority"
                    )
                if (
                    v2_attempt_sidecar.get("assignment_binding_digest")
                    != runtime_binding_v2.assignment_binding_digest
                    or v2_attempt_sidecar.get("attempt_role_binding_digest")
                    != runtime_binding_v2.attempt_role_binding_digest
                    or v2_attempt_sidecar.get("runtime_binding_digest")
                    != runtime_binding_v2.digest
                    or stored_runtime_binding != runtime_binding_v2
                    or runtime_binding_v2.permit_id != permit.permit_id
                    or runtime_binding_v2.permit_digest != permit.digest
                    or runtime_binding_v2.attempt_identity_digest
                    != permit.lease.attempt.digest
                    or runtime_binding_v2.scope_digest
                    != permit.lease.attempt.scope.digest
                ):
                    raise PermitResolutionError(
                        "v2 launch role binding differs from canonical admission"
                    )
                expected_budget = dict(runtime_binding_v2.role_budget)
                if not _same_json(
                    permit.constraints.get("requested_budget"), expected_budget
                ):
                    raise PermitResolutionError(
                        "v2 launch budget differs from the exact role ceiling"
                    )
                try:
                    self._store.validate_runtime_evaluation_v2_prerequisite_lineage(
                        runtime_binding_v2
                    )
                except IntegrityError as exc:
                    raise PermitResolutionError(str(exc)) from exc
            payload = {
                "admission_event_digest": resolved.admission_event_digest,
                "attempt_digest": permit.lease.attempt.digest,
                "attempt_id": permit.lease.attempt.attempt_id,
                "lease_digest": permit.lease.digest,
                "lease_id": permit.lease.lease_id,
                "launch_ordinal": permit.lease.attempt.launch_ordinal,
                "permit_digest": permit.digest,
                "permit_id": permit.permit_id,
                "reservation_ids": permit.reservation_ids,
                "scope_digest": permit.lease.attempt.scope.digest,
            }
            events = [
                CommandEvent(
                    f"event:launch:{permit.permit_id}",
                    "WORKER_LAUNCH_PREPARED",
                    "run-supervisor",
                    now_ns,
                    payload,
                )
            ]
            mutations = [ProjectionMutation("attempt_launch", payload)]
            if attempt_sidecar is not None:
                sidecar = {
                    "attempt_binding_event_digest": attempt_sidecar_digest,
                    "attempt_digest": permit.lease.attempt.digest,
                    "attempt_id": permit.lease.attempt.attempt_id,
                    "base_event_id": events[0].event_id,
                    "base_payload_digest": canonical_digest(payload),
                    "evaluation_binding_digest": attempt_sidecar[
                        "evaluation_binding_digest"
                    ],
                    "permit_digest": permit.digest,
                    "permit_id": permit.permit_id,
                    "phase": "launch",
                    "schema_id": "muteki.c6-eval-binding-sidecar.v1",
                    "scope_digest": permit.lease.attempt.scope.digest,
                }
                events.append(
                    CommandEvent(
                        f"event:C6_EVAL_LAUNCH_BOUND:{permit.permit_id}",
                        "C6_EVAL_LAUNCH_BOUND",
                        "c6-evaluation-binding-authority",
                        now_ns,
                        sidecar,
                    )
                )
                mutations.append(
                    ProjectionMutation("c6_eval_launch_bind_guard", sidecar)
                )
            if v2_attempt_sidecar is not None:
                assert runtime_binding_v2 is not None
                sidecar = {
                    "assignment_binding_digest": v2_attempt_sidecar[
                        "assignment_binding_digest"
                    ],
                    "attempt_role_binding_digest": v2_attempt_sidecar[
                        "attempt_role_binding_digest"
                    ],
                    "attempt_binding_event_digest": v2_attempt_sidecar_digest,
                    "attempt_digest": permit.lease.attempt.digest,
                    "attempt_id": permit.lease.attempt.attempt_id,
                    "base_event_id": events[0].event_id,
                    "base_payload_digest": canonical_digest(payload),
                    "permit_digest": permit.digest,
                    "permit_id": permit.permit_id,
                    "phase": "launch",
                    "prerequisite_terminal_event_digests": list(
                        runtime_binding_v2.prerequisite_terminal_event_digests
                    ),
                    "role": v2_attempt_sidecar["role"],
                    "runtime_binding_digest": v2_attempt_sidecar[
                        "runtime_binding_digest"
                    ],
                    "schema_id": "muteki.c6-eval-binding-sidecar.v2",
                    "scope_digest": permit.lease.attempt.scope.digest,
                    "slot_id": v2_attempt_sidecar["slot_id"],
                }
                events.append(
                    CommandEvent(
                        f"event:C6_EVAL_V2_LAUNCH_BOUND:{permit.permit_id}",
                        "C6_EVAL_V2_LAUNCH_BOUND",
                        "c6-evaluation-binding-v2-authority",
                        now_ns,
                        sidecar,
                    )
                )
                mutations.append(
                    ProjectionMutation("c6_eval_v2_launch_bind_guard", sidecar)
                )
            try:
                authority = None
                if shadow:
                    authority = self._store._evaluation_commit_capability
                elif shadow_v2:
                    authority = self._store._evaluation_v2_commit_capability
                result = self._store.commit_command(
                    command_id=f"launch:{permit.permit_id}",
                    idempotency_key=f"launch:{permit.permit_id}",
                    command_payload=payload,
                    events=events,
                    projection_mutations=mutations,
                    authority_capability=authority,
                    committed_at_ns=now_ns,
                )
            except (IdempotencyConflict, IntegrityError) as exc:
                raise PermitResolutionError(
                    "permit launch identity conflicts with canonical history"
                ) from exc
            if result.idempotent:
                raise PermitResolutionError("permit was already launched")
            return resolved

    def _resolve_locked(self, permit: AttemptPermit, *, now_ns: int) -> ResolvedPermit:
        attempt = permit.lease.attempt
        scope = attempt.scope
        if scope != self._scope or scope.run_id != self._store.run_id:
            raise PermitResolutionError("permit scope is stale")
        current = self._store._state()
        if (
            current.run_id,
            current.run_fence_epoch,
            current.execution_generation,
        ) != (scope.run_id, scope.run_fence_epoch, scope.execution_generation):
            raise PermitResolutionError("permit scope is no longer current")
        if (
            current.kernel_health.value != "ready"
            or current.run_execution.value != "running"
            or current.search_mode.value != "active"
        ):
            raise PermitResolutionError("canonical run state forbids launch")
        if now_ns >= permit.expires_at_ns:
            raise PermitResolutionError("permit is expired")

        lifecycle_rows = self._store._conn.execute(
            "SELECT kind,payload_json FROM events WHERE kind IN "
            "('WORKER_LAUNCH_PREPARED','WORKER_TERMINAL','WORKER_UNKNOWN')"
        ).fetchall()
        for kind, raw_payload in lifecycle_rows:
            candidate = json.loads(raw_payload)
            if candidate.get("permit_id") == permit.permit_id:
                if kind == "WORKER_LAUNCH_PREPARED":
                    raise PermitResolutionError("permit was already launched")
                raise PermitResolutionError(
                    "permit already has a terminal lifecycle event"
                )

        rows = self._store._conn.execute(
            "SELECT event_id,run_id,command_id,ordinal,kind,actor,occurred_at_ns,"
            "payload_json,parent_event_digest,event_digest FROM events "
            "WHERE kind='ATTEMPT_ADMITTED' ORDER BY seq"
        ).fetchall()
        matches: list[tuple[Mapping[str, Any], str]] = []
        for row in rows:
            candidate = json.loads(row[7])
            if candidate.get("permit_id") == permit.permit_id:
                try:
                    envelope = EventEnvelopeV2(
                        event_id=row[0],
                        run_id=row[1],
                        command_id=row[2],
                        ordinal=row[3],
                        kind=row[4],
                        actor=row[5],
                        occurred_at_ns=row[6],
                        payload=candidate,
                        parent_event_digest=row[8],
                    )
                except (TypeError, ValueError) as exc:
                    raise PermitResolutionError(
                        "admission event is not canonical"
                    ) from exc
                if envelope.digest != row[9] or envelope.run_id != self._store.run_id:
                    raise PermitResolutionError("admission event digest is invalid")
                matches.append((candidate, str(row[9])))
        if len(matches) != 1:
            raise PermitResolutionError("permit has no unique canonical admission")
        payload, event_digest = matches[0]

        body = payload.get("permit")
        declared_digest = payload.get("permit_digest")
        if not isinstance(body, dict) or not isinstance(declared_digest, str):
            raise PermitResolutionError("canonical admission lacks a resolvable permit")
        if canonical_digest(body) != declared_digest:
            raise PermitResolutionError(
                "canonical permit body does not match its digest"
            )
        if declared_digest != permit.digest or not _same_json(
            body, permit.canonical_body()
        ):
            raise PermitResolutionError(
                "supplied permit differs from canonical admission"
            )

        identity_checks = (
            payload.get("attempt_id") == attempt.attempt_id,
            payload.get("branch_id") == attempt.branch_id,
            _exact_int(payload.get("launch_ordinal"), attempt.launch_ordinal),
            payload.get("attempt_digest") == attempt.digest,
            payload.get("scope_digest") == scope.digest,
            payload.get("lease_id") == permit.lease.lease_id,
            _exact_int(payload.get("lease_epoch"), permit.lease.lease_epoch),
            _exact_int(
                payload.get("worker_generation"), permit.lease.worker_generation
            ),
            payload.get("lease_digest") == permit.lease.digest,
            payload.get("policy_digest") == permit.policy_digest,
            payload.get("effect_class") == permit.effect_class.value,
            _exact_int(payload.get("expires_at_ns"), permit.expires_at_ns),
            _same_json(payload.get("reservation_ids"), permit.reservation_ids),
        )
        if not all(identity_checks):
            raise PermitResolutionError("canonical admission identity is inconsistent")

        expected_constraints = {
            "account_id": payload.get("account_id"),
            "conflict_keys": payload.get("conflict_keys"),
            "fingerprint": payload.get("fingerprint"),
            "requested_budget": payload.get("requested_budget"),
        }
        # A production C6 context packet is part of the exact admission
        # contract, not auxiliary metadata.  Keep the legacy permit body
        # byte-for-byte unchanged when the feature is off, but require the
        # canonical payload and permit constraint to agree when it is on.
        if payload.get("context_packet") is not None:
            expected_constraints["context_packet"] = payload["context_packet"]
        if not _same_json(body.get("constraints"), expected_constraints):
            raise PermitResolutionError("canonical permit constraints are inconsistent")

        projection = self._store._conn.execute(
            "SELECT branch_id,permit_id,scope_digest,lease_id,lease_epoch,"
            "worker_generation,fingerprint,effect_class,state "
            "FROM runtime_attempts WHERE attempt_id=?",
            (attempt.attempt_id,),
        ).fetchone()
        if projection is None:
            raise PermitResolutionError("admitted attempt projection is missing")
        expected_projection = (
            attempt.branch_id,
            permit.permit_id,
            scope.digest,
            permit.lease.lease_id,
            permit.lease.lease_epoch,
            permit.lease.worker_generation,
            permit.constraints.get("fingerprint"),
            permit.effect_class.value,
            "reserved",
        )
        if tuple(projection) != expected_projection:
            raise PermitResolutionError(
                "attempt is stale, completed, UNKNOWN, or projection-inconsistent"
            )

        reservations = self._store._conn.execute(
            "SELECT reservation_id,state FROM budget_reservations "
            "WHERE attempt_id=? ORDER BY reservation_id",
            (attempt.attempt_id,),
        ).fetchall()
        projected_ids = tuple(str(row[0]) for row in reservations)
        if (
            len(projected_ids) != len(permit.reservation_ids)
            or set(projected_ids) != set(permit.reservation_ids)
            or any(row[1] != "active" for row in reservations)
        ):
            raise PermitResolutionError("permit reservations are not exactly active")

        return ResolvedPermit(
            admission_event_digest=event_digest,
            attempt_id=attempt.attempt_id,
            permit_digest=declared_digest,
        )
