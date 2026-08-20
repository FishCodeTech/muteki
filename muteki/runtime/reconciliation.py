"""Fail-closed startup classification and reconciliation of worker owners.

The event log is the authority.  Runtime projections are deliberately not used to
decide whether a worker may be redispatched: a launch marker without one unique
terminal marker is conservatively converted to UNKNOWN, together with an UNKNOWN
budget hold, in one canonical command.
"""

from __future__ import annotations

import json
from collections.abc import Mapping
from dataclasses import dataclass, replace
from enum import Enum
from typing import Any

from muteki.epistemic.contracts import (
    canonical_digest,
    canonical_json_bytes,
)
from muteki.epistemic.cognitive_events_v1 import (
    COGNITIVE_ASSIGNMENT_SCHEMA_ID,
    COGNITIVE_BINDING_ACTOR,
    COGNITIVE_EXECUTION_OBSERVED,
    COGNITIVE_EXPERIMENT_ASSIGNED,
    COGNITIVE_RUNTIME_CONTEXT_ASSIGNMENT_SCHEMA_ID,
    COGNITIVE_RUNTIME_EXECUTABLE_ASSIGNMENT_SCHEMA_ID,
    cognitive_execution_payload,
)
from muteki.epistemic.sqlite_store import (
    CommandEvent,
    EpistemicSQLiteStore,
    ProjectionMutation,
)
from muteki.runtime.controller import AuthorityDenied, CommandClass, LiveHealthGuard
from muteki.runtime.usage import UsageReport


class WorkerLifecycleState(str, Enum):
    NOT_LAUNCHED = "not_launched"
    IN_FLIGHT_ORPHAN = "in_flight_orphan"
    TERMINAL = "terminal"
    AMBIGUOUS = "ambiguous"


class ReconciliationDisposition(str, Enum):
    HOLD_NOT_LAUNCHED = "hold_not_launched"
    MARK_UNKNOWN = "mark_unknown"
    ALREADY_TERMINAL = "already_terminal"
    HOLD_INCOMPLETE_TERMINAL = "hold_incomplete_terminal"
    HOLD_AMBIGUOUS = "hold_ambiguous"


@dataclass(frozen=True, slots=True)
class PermitLifecycle:
    permit_id: str
    attempt_id: str | None
    state: WorkerLifecycleState
    admission_event_digest: str | None
    launch_event_digest: str | None
    terminal_event_digest: str | None
    terminal_kind: str | None
    accounting_complete: bool
    reasons: tuple[str, ...]


@dataclass(frozen=True, slots=True)
class LifecycleInventory:
    permits: tuple[PermitLifecycle, ...]
    unbound_event_digests: tuple[str, ...]

    @property
    def is_unambiguous(self) -> bool:
        return not self.unbound_event_digests and all(
            item.state is not WorkerLifecycleState.AMBIGUOUS
            and (
                item.state is not WorkerLifecycleState.TERMINAL
                or item.accounting_complete
            )
            for item in self.permits
        )


@dataclass(frozen=True, slots=True)
class ReconciliationPlan:
    lifecycle: PermitLifecycle
    disposition: ReconciliationDisposition
    occurrence_digest: str | None = None
    command_id: str | None = None
    idempotency_key: str | None = None


@dataclass(frozen=True, slots=True)
class ReconciliationOutcome:
    plan: ReconciliationPlan
    lifecycle_after: PermitLifecycle
    receipt_digest: str | None


_LIFECYCLE_KINDS = frozenset(
    {"ATTEMPT_ADMITTED", "WORKER_LAUNCH_PREPARED", "WORKER_TERMINAL", "WORKER_UNKNOWN"}
)
_BUDGET_KINDS = frozenset(
    {
        "BUDGET_PESSIMISTICALLY_SETTLED",
        "BUDGET_SETTLED",
        "BUDGET_USAGE_UNKNOWN",
    }
)


def _canonical_text(value: object) -> str | None:
    if type(value) is str and value and value == value.strip():
        return value
    return None


def _is_digest(value: object) -> bool:
    return bool(
        type(value) is str
        and len(value) == 64
        and all(character in "0123456789abcdef" for character in value)
    )


def _same_json(left: object, right: object) -> bool:
    try:
        return canonical_json_bytes(left) == canonical_json_bytes(right)
    except (TypeError, ValueError):
        return False


def _strict_budget(value: object) -> dict[str, int] | None:
    if not isinstance(value, Mapping) or not value:
        return None
    budget: dict[str, int] = {}
    for axis, amount in value.items():
        if (
            type(axis) is not str
            or not axis
            or axis != axis.strip()
            or type(amount) is not int
            or amount < 0
        ):
            return None
        budget[axis] = amount
    return budget


def _reservation_ids(value: object) -> tuple[str, ...] | None:
    if type(value) not in {list, tuple}:
        return None
    identities = tuple(value)
    if (
        not identities
        or any(_canonical_text(item) is None for item in identities)
        or len(set(identities)) != len(identities)
    ):
        return None
    return identities


class OrphanReconciler:
    """Classify canonical worker lifecycles and terminalize safe orphans.

    Callers must run this while ordinary admission is closed.  The store lock makes
    each classification-and-commit decision atomic with respect to other writers.
    This component never emits a launch, retry, release, or settlement command.
    """

    def __init__(
        self,
        *,
        store: EpistemicSQLiteStore,
        guard: LiveHealthGuard | None = None,
    ) -> None:
        self._store = store
        self._guard = guard

    def inventory(self) -> LifecycleInventory:
        with self._store._lock:
            return self._inventory_locked()

    def classify(self, permit_id: str) -> PermitLifecycle:
        permit_id = self._permit_id(permit_id)
        inventory = self.inventory()
        for item in inventory.permits:
            if item.permit_id == permit_id:
                return item
        raise KeyError(permit_id)

    def plan(self, permit_id: str) -> ReconciliationPlan:
        permit_id = self._permit_id(permit_id)
        with self._store._lock:
            return self._plan_locked(permit_id)

    def reconcile(
        self, permit_id: str, *, occurred_at_ns: int
    ) -> ReconciliationOutcome:
        permit_id = self._permit_id(permit_id)
        if type(occurred_at_ns) is not int or occurred_at_ns < 0:
            raise ValueError("occurred_at_ns must be a non-negative integer")

        with self._store._lock:
            if self._guard is None:
                raise AuthorityDenied(
                    "orphan reconciliation requires a boot-scoped recovery guard"
                )
            self._guard.authorize(CommandClass.RECOVERY, self._store._state())
            plan = self._plan_locked(permit_id)
            if plan.disposition is not ReconciliationDisposition.MARK_UNKNOWN:
                return ReconciliationOutcome(
                    plan=plan,
                    lifecycle_after=plan.lifecycle,
                    receipt_digest=None,
                )

            lifecycle = plan.lifecycle
            if (
                lifecycle.attempt_id is None
                or lifecycle.admission_event_digest is None
                or lifecycle.launch_event_digest is None
                or plan.occurrence_digest is None
                or plan.command_id is None
                or plan.idempotency_key is None
            ):
                raise RuntimeError("safe orphan plan is missing canonical identity")

            admission = self._unique_admission_payload_locked(permit_id)
            reserved = _strict_budget(admission.get("requested_budget"))
            reservation_ids = _reservation_ids(admission.get("reservation_ids"))
            if reserved is None or reservation_ids is None:
                raise RuntimeError(
                    "safe orphan plan lost its admission budget contract"
                )
            report = UsageReport.from_observed_and_reservation(
                reserved=reserved,
                observed={},
                complete_axes=frozenset(),
            )
            budget_payload = {
                "attempt_id": lifecycle.attempt_id,
                "held_usage": dict(report.pessimistic_usage()),
                "reservation_ids": reservation_ids,
                "revision": 1,
                "usage_report": report.canonical_body(),
                "usage_report_digest": report.digest,
            }
            worker_payload = {
                "admission_event_digest": lifecycle.admission_event_digest,
                "attempt_digest": admission["attempt_digest"],
                "attempt_id": lifecycle.attempt_id,
                "launch_event_digest": lifecycle.launch_event_digest,
                "lease_digest": admission["lease_digest"],
                "lease_id": admission["lease_id"],
                "outcome": "unknown",
                "permit_digest": admission["permit_digest"],
                "permit_id": permit_id,
                "reason": "startup_in_flight_orphan",
                "scope_digest": admission["scope_digest"],
            }
            worker_event_id = f"event:worker-unknown:{plan.occurrence_digest}"
            budget_event_id = f"event:budget-unknown:{plan.occurrence_digest}"
            command_payload = {
                "action": "mark_worker_and_usage_unknown",
                "admission_event_digest": lifecycle.admission_event_digest,
                "attempt_id": lifecycle.attempt_id,
                "budget_unknown": budget_payload,
                "launch_event_digest": lifecycle.launch_event_digest,
                "occurrence_digest": plan.occurrence_digest,
                "permit_id": permit_id,
                "worker_unknown": worker_payload,
            }
            events = [
                CommandEvent(
                    worker_event_id,
                    "WORKER_UNKNOWN",
                    "orphan-reconciler",
                    occurred_at_ns,
                    worker_payload,
                ),
                CommandEvent(
                    budget_event_id,
                    "BUDGET_USAGE_UNKNOWN",
                    "orphan-reconciler",
                    occurred_at_ns,
                    budget_payload,
                ),
            ]
            mutations = [
                ProjectionMutation(
                    "orphan_reconcile_guard",
                    {
                        "attempt_digest": admission["attempt_digest"],
                        "attempt_id": lifecycle.attempt_id,
                        "budget_unknown_event_id": budget_event_id,
                        "launch_event_digest": lifecycle.launch_event_digest,
                        "lease_digest": admission["lease_digest"],
                        "lease_id": admission["lease_id"],
                        "permit_digest": admission["permit_digest"],
                        "permit_id": permit_id,
                        "scope_digest": admission["scope_digest"],
                        "worker_unknown_event_id": worker_event_id,
                    },
                ),
                ProjectionMutation("budget_unknown", budget_payload),
            ]
            authority = None
            v2_launch = self._v2_launch_binding_locked(permit_id)
            cognitive_assignments = [
                row
                for row in self._store.event_rows(kind=COGNITIVE_EXPERIMENT_ASSIGNED)
                if row["payload"].get("permit_id") == permit_id
            ]
            if len(cognitive_assignments) > 1:
                raise RuntimeError(
                    "orphan has multiple canonical cognitive assignments"
                )
            eval_cognitive_assignments = [
                row
                for row in cognitive_assignments
                if row["payload"].get("schema_id") == COGNITIVE_ASSIGNMENT_SCHEMA_ID
            ]
            runtime_context_assignments = [
                row
                for row in cognitive_assignments
                if row["payload"].get("schema_id")
                in {
                    COGNITIVE_RUNTIME_CONTEXT_ASSIGNMENT_SCHEMA_ID,
                    COGNITIVE_RUNTIME_EXECUTABLE_ASSIGNMENT_SCHEMA_ID,
                }
            ]
            if cognitive_assignments and not (
                eval_cognitive_assignments or runtime_context_assignments
            ):
                raise RuntimeError("orphan cognitive assignment schema is unrecognized")
            if v2_launch is not None:
                if runtime_context_assignments:
                    raise RuntimeError(
                        "runtime-context orphan cannot carry a v2 launch binding"
                    )
                launch_binding_digest, launch_binding = v2_launch
                sidecar = {
                    "assignment_binding_digest": launch_binding[
                        "assignment_binding_digest"
                    ],
                    "attempt_digest": admission["attempt_digest"],
                    "attempt_id": lifecycle.attempt_id,
                    "attempt_role_binding_digest": launch_binding[
                        "attempt_role_binding_digest"
                    ],
                    "base_event_id": worker_event_id,
                    "base_payload_digest": canonical_digest(worker_payload),
                    "budget_event_id": budget_event_id,
                    "budget_event_kind": "BUDGET_USAGE_UNKNOWN",
                    "budget_payload_digest": canonical_digest(budget_payload),
                    "launch_binding_event_digest": launch_binding_digest,
                    "permit_digest": admission["permit_digest"],
                    "permit_id": permit_id,
                    "phase": "terminal",
                    "role": launch_binding["role"],
                    "runtime_binding_digest": launch_binding["runtime_binding_digest"],
                    "schema_id": "muteki.c6-eval-binding-sidecar.v2",
                    "scope_digest": admission["scope_digest"],
                    "slot_id": launch_binding["slot_id"],
                    "terminal_outcome": "unknown",
                }
                events.append(
                    CommandEvent(
                        f"event:C6_EVAL_V2_TERMINAL_BOUND:{permit_id}",
                        "C6_EVAL_V2_TERMINAL_BOUND",
                        "c6-evaluation-binding-v2-authority",
                        occurred_at_ns,
                        sidecar,
                    )
                )
                mutations.append(
                    ProjectionMutation("c6_eval_v2_terminal_bind_guard", sidecar)
                )
                command_payload = {
                    **command_payload,
                    "evaluation_terminal_binding": sidecar,
                }
                if eval_cognitive_assignments:
                    assignment = eval_cognitive_assignments[0]
                    if launch_binding["role"] != "executor":
                        raise RuntimeError(
                            "cognitive orphan assignment is not an executor"
                        )
                    cognitive_payload = cognitive_execution_payload(
                        assignment_event_digest=assignment["event_digest"],
                        assignment_payload=assignment["payload"],
                        terminal_payload=worker_payload,
                        budget_event_id=budget_event_id,
                        budget_kind="BUDGET_USAGE_UNKNOWN",
                        budget_payload=budget_payload,
                        evaluation_terminal_event_id=(
                            f"event:C6_EVAL_V2_TERMINAL_BOUND:{permit_id}"
                        ),
                        evaluation_terminal_sidecar=sidecar,
                    )
                    events.append(
                        CommandEvent(
                            (f"event:{COGNITIVE_EXECUTION_OBSERVED}:{permit_id}"),
                            COGNITIVE_EXECUTION_OBSERVED,
                            COGNITIVE_BINDING_ACTOR,
                            occurred_at_ns,
                            cognitive_payload,
                        )
                    )
                    mutations.append(
                        ProjectionMutation(
                            "cognitive_execution_observe_guard",
                            cognitive_payload,
                        )
                    )
                    command_payload = {
                        **command_payload,
                        "cognitive_execution": cognitive_payload,
                    }
                    authority = self._store._evaluation_v2_cognitive_commit_capability
                else:
                    authority = self._store._evaluation_v2_commit_capability
            elif eval_cognitive_assignments:
                raise RuntimeError(
                    "cognitive orphan has no canonical v2 launch binding"
                )
            result = self._store.commit_command(
                command_id=plan.command_id,
                idempotency_key=plan.idempotency_key,
                command_payload=command_payload,
                events=events,
                projection_mutations=mutations,
                authority_capability=authority,
                committed_at_ns=occurred_at_ns,
            )
            after = self._lifecycle_for_locked(permit_id)
            if (
                after.state is not WorkerLifecycleState.TERMINAL
                or after.terminal_kind != "WORKER_UNKNOWN"
                or not after.accounting_complete
            ):
                raise RuntimeError(
                    "orphan reconciliation did not reach UNKNOWN terminal"
                )
            if eval_cognitive_assignments:
                observed = [
                    row
                    for row in self._store.event_rows(kind=COGNITIVE_EXECUTION_OBSERVED)
                    if row["payload"].get("permit_id") == permit_id
                ]
                if (
                    len(observed) != 1
                    or observed[0]["payload"].get("usage_status") != "unknown"
                ):
                    raise RuntimeError(
                        "cognitive orphan did not reach UNKNOWN observation"
                    )
            return ReconciliationOutcome(
                plan=plan,
                lifecycle_after=after,
                receipt_digest=result.receipt_digest,
            )

    @staticmethod
    def _permit_id(value: object) -> str:
        permit_id = _canonical_text(value)
        if permit_id is None:
            raise ValueError("permit_id must be a non-empty canonical string")
        return permit_id

    def _plan_locked(self, permit_id: str) -> ReconciliationPlan:
        inventory = self._inventory_locked()
        try:
            lifecycle = next(
                item for item in inventory.permits if item.permit_id == permit_id
            )
        except StopIteration as exc:
            raise KeyError(permit_id) from exc

        # An unbound marker could belong to this permit but cannot be proven to do
        # so.  No canonical mutation is safe until the log is repaired/audited.
        if inventory.unbound_event_digests:
            lifecycle = replace(
                lifecycle,
                state=WorkerLifecycleState.AMBIGUOUS,
                reasons=lifecycle.reasons + ("canonical log contains unbound markers",),
            )
            return ReconciliationPlan(
                lifecycle=lifecycle,
                disposition=ReconciliationDisposition.HOLD_AMBIGUOUS,
            )

        dispositions = {
            WorkerLifecycleState.NOT_LAUNCHED: ReconciliationDisposition.HOLD_NOT_LAUNCHED,
            WorkerLifecycleState.TERMINAL: ReconciliationDisposition.ALREADY_TERMINAL,
            WorkerLifecycleState.AMBIGUOUS: ReconciliationDisposition.HOLD_AMBIGUOUS,
        }
        if lifecycle.state is not WorkerLifecycleState.IN_FLIGHT_ORPHAN:
            disposition = dispositions[lifecycle.state]
            if (
                lifecycle.state is WorkerLifecycleState.TERMINAL
                and not lifecycle.accounting_complete
            ):
                disposition = ReconciliationDisposition.HOLD_INCOMPLETE_TERMINAL
            return ReconciliationPlan(
                lifecycle=lifecycle,
                disposition=disposition,
            )

        occurrence_digest = canonical_digest(
            {
                "action": "mark_worker_and_usage_unknown",
                "admission_event_digest": lifecycle.admission_event_digest,
                "attempt_id": lifecycle.attempt_id,
                "launch_event_digest": lifecycle.launch_event_digest,
                "permit_id": lifecycle.permit_id,
                "version": 1,
            }
        )
        command_id = f"orphan-reconcile:{occurrence_digest}"
        return ReconciliationPlan(
            lifecycle=lifecycle,
            disposition=ReconciliationDisposition.MARK_UNKNOWN,
            occurrence_digest=occurrence_digest,
            command_id=command_id,
            idempotency_key=command_id,
        )

    def _inventory_locked(self) -> LifecycleInventory:
        rows = self._store.event_rows()
        admissions: dict[str, list[dict[str, Any]]] = {}
        markers: dict[str, list[dict[str, Any]]] = {}
        unbound: list[str] = []
        for row in rows:
            kind = row["kind"]
            if kind not in _LIFECYCLE_KINDS:
                continue
            permit_id = _canonical_text(row["payload"].get("permit_id"))
            if permit_id is None:
                unbound.append(str(row["event_digest"]))
                continue
            target = admissions if kind == "ATTEMPT_ADMITTED" else markers
            target.setdefault(permit_id, []).append(row)

        admitted_attempt_ids = {
            attempt_id
            for admission_rows in admissions.values()
            for row in admission_rows
            if (attempt_id := _canonical_text(row["payload"].get("attempt_id")))
            is not None
        }
        for row in rows:
            if row["kind"] not in _BUDGET_KINDS:
                continue
            attempt_id = _canonical_text(row["payload"].get("attempt_id"))
            if attempt_id is None or attempt_id not in admitted_attempt_ids:
                unbound.append(str(row["event_digest"]))

        permit_ids = sorted(set(admissions) | set(markers))
        lifecycles = tuple(
            self._classify_rows(
                permit_id,
                tuple(admissions.get(permit_id, ())),
                tuple(markers.get(permit_id, ())),
                rows,
            )
            for permit_id in permit_ids
        )
        return LifecycleInventory(
            permits=lifecycles,
            unbound_event_digests=tuple(unbound),
        )

    def _lifecycle_for_locked(self, permit_id: str) -> PermitLifecycle:
        inventory = self._inventory_locked()
        for item in inventory.permits:
            if item.permit_id == permit_id:
                return item
        raise KeyError(permit_id)

    def _unique_admission_payload_locked(self, permit_id: str) -> Mapping[str, Any]:
        rows = tuple(
            row
            for row in self._store.event_rows(kind="ATTEMPT_ADMITTED")
            if row["payload"].get("permit_id") == permit_id
        )
        if len(rows) != 1:
            raise RuntimeError("permit lost its unique canonical admission")
        return rows[0]["payload"]

    def _v2_launch_binding_locked(
        self, permit_id: str
    ) -> tuple[str, dict[str, Any]] | None:
        rows = self._store._conn.execute(
            "SELECT event_digest,payload_json FROM events "
            "WHERE kind='C6_EVAL_V2_LAUNCH_BOUND' ORDER BY seq"
        ).fetchall()
        matching = [
            (str(row[0]), json.loads(row[1]))
            for row in rows
            if json.loads(row[1]).get("permit_id") == permit_id
        ]
        if len(matching) > 1:
            raise RuntimeError("v2 orphan launch binding is ambiguous")
        return matching[0] if matching else None

    def _classify_rows(
        self,
        permit_id: str,
        admissions: tuple[dict[str, Any], ...],
        markers: tuple[dict[str, Any], ...],
        all_rows: tuple[dict[str, Any], ...],
    ) -> PermitLifecycle:
        reasons: list[str] = []
        if len(admissions) != 1:
            reasons.append("permit must have exactly one admission marker")
        admission = admissions[0] if admissions else None
        attempt_id = None
        if admission is not None:
            attempt_id = _canonical_text(admission["payload"].get("attempt_id"))
            reasons.extend(self._validate_admission(permit_id, admission))

        launches = tuple(
            row for row in markers if row["kind"] == "WORKER_LAUNCH_PREPARED"
        )
        terminals = tuple(
            row
            for row in markers
            if row["kind"] in {"WORKER_TERMINAL", "WORKER_UNKNOWN"}
        )
        if len(launches) > 1:
            reasons.append("duplicate launch markers")
        if len(terminals) > 1:
            reasons.append("duplicate or contradictory terminal markers")

        launch = launches[0] if launches else None
        terminal = terminals[0] if terminals else None
        if admission is not None and launch is not None:
            reasons.extend(self._validate_launch(admission, launch))
            if int(launch["seq"]) <= int(admission["seq"]):
                reasons.append("launch marker precedes admission")
        if terminal is not None:
            if admission is None or launch is None:
                reasons.append("terminal marker has no admitted launch")
            else:
                reasons.extend(self._validate_terminal(admission, launch, terminal))
                if int(terminal["seq"]) <= int(launch["seq"]):
                    reasons.append("terminal marker precedes launch")

        budget_rows: tuple[dict[str, Any], ...] = ()
        if attempt_id is not None:
            budget_rows = tuple(
                row
                for row in all_rows
                if row["kind"] in _BUDGET_KINDS
                and row["payload"].get("attempt_id") == attempt_id
            )
        if len(budget_rows) > 1:
            reasons.append("duplicate or contradictory budget terminal markers")
        if budget_rows and terminal is None:
            reasons.append("budget terminal marker has no worker terminal")
        if (
            budget_rows
            and launch is not None
            and int(budget_rows[0]["seq"]) <= int(launch["seq"])
        ):
            reasons.append("budget terminal marker precedes worker launch")
        if budget_rows and terminal is not None:
            # Worker outcome/effect ambiguity and usage observability are
            # orthogonal. A failed worker may still have a complete, exact usage
            # stream; only an observed terminal requires settled accounting.
            if terminal["kind"] == "WORKER_TERMINAL" and budget_rows[0]["kind"] not in {
                "BUDGET_PESSIMISTICALLY_SETTLED",
                "BUDGET_SETTLED",
            }:
                reasons.append("worker and budget terminal outcomes contradict")

        if reasons:
            state = WorkerLifecycleState.AMBIGUOUS
        elif launch is None:
            state = WorkerLifecycleState.NOT_LAUNCHED
        elif terminal is None:
            state = WorkerLifecycleState.IN_FLIGHT_ORPHAN
        else:
            state = WorkerLifecycleState.TERMINAL

        return PermitLifecycle(
            permit_id=permit_id,
            attempt_id=attempt_id,
            state=state,
            admission_event_digest=(
                str(admission["event_digest"]) if admission is not None else None
            ),
            launch_event_digest=(
                str(launch["event_digest"]) if launch is not None else None
            ),
            terminal_event_digest=(
                str(terminal["event_digest"]) if terminal is not None else None
            ),
            terminal_kind=(str(terminal["kind"]) if terminal is not None else None),
            accounting_complete=len(budget_rows) == 1,
            reasons=tuple(reasons),
        )

    @staticmethod
    def _validate_admission(permit_id: str, row: Mapping[str, Any]) -> tuple[str, ...]:
        payload = row["payload"]
        reasons: list[str] = []
        attempt_id = _canonical_text(payload.get("attempt_id"))
        if attempt_id is None:
            reasons.append("admission attempt identity is malformed")
        for digest_name in (
            "attempt_digest",
            "lease_digest",
            "policy_digest",
            "scope_digest",
        ):
            if not _is_digest(payload.get(digest_name)):
                reasons.append(f"admission {digest_name} is malformed")
        for ordinal_name in ("launch_ordinal", "lease_epoch", "worker_generation"):
            ordinal = payload.get(ordinal_name)
            if type(ordinal) is not int or ordinal < 1:
                reasons.append(f"admission {ordinal_name} is malformed")
        expires_at_ns = payload.get("expires_at_ns")
        if type(expires_at_ns) is not int or expires_at_ns < 0:
            reasons.append("admission expiry is malformed")
        declared = payload.get("permit_digest")
        body = payload.get("permit")
        if not _is_digest(declared) or not isinstance(body, Mapping):
            reasons.append("admission permit body or digest is malformed")
        elif canonical_digest(body) != declared:
            reasons.append("admission permit body digest mismatch")
        elif body.get("permit_id") != permit_id:
            reasons.append("admission permit body identity mismatch")

        budget = _strict_budget(payload.get("requested_budget"))
        identities = _reservation_ids(payload.get("reservation_ids"))
        if budget is None:
            reasons.append("admission budget contract is malformed")
        if identities is None:
            reasons.append("admission reservation identities are malformed")
        if isinstance(body, Mapping):
            body_bindings = {
                "lease_digest": payload.get("lease_digest"),
                "policy_digest": payload.get("policy_digest"),
                "permit_id": permit_id,
                "effect_class": payload.get("effect_class"),
                "expires_at_ns": payload.get("expires_at_ns"),
            }
            for key, value in body_bindings.items():
                if not _same_json(body.get(key), value):
                    reasons.append(f"permit {key} diverges from admission")
            body_expiry = body.get("expires_at_ns")
            if type(body_expiry) is not int or body_expiry < 0:
                reasons.append("permit expiry is malformed")
            if not _same_json(
                body.get("reservation_ids"), payload.get("reservation_ids")
            ):
                reasons.append("permit reservation identities diverge from admission")
            constraints = body.get("constraints")
            expected_constraints = {
                "account_id": payload.get("account_id"),
                "conflict_keys": payload.get("conflict_keys"),
                "fingerprint": payload.get("fingerprint"),
                "requested_budget": payload.get("requested_budget"),
            }
            if "context_packet" in payload:
                expected_constraints["context_packet"] = payload.get("context_packet")
            if not isinstance(constraints, Mapping) or not _same_json(
                constraints, expected_constraints
            ):
                reasons.append("permit constraints diverge from admission")
        if not _is_digest(row.get("event_digest")):
            reasons.append("admission event digest is malformed")
        return tuple(reasons)

    @staticmethod
    def _validate_launch(
        admission: Mapping[str, Any], launch: Mapping[str, Any]
    ) -> tuple[str, ...]:
        admitted = admission["payload"]
        payload = launch["payload"]
        expected = {
            "admission_event_digest": admission["event_digest"],
            "attempt_digest": admitted.get("attempt_digest"),
            "lease_digest": admitted.get("lease_digest"),
            "launch_ordinal": admitted.get("launch_ordinal"),
            "permit_digest": admitted.get("permit_digest"),
            "permit_id": admitted.get("permit_id"),
            "reservation_ids": admitted.get("reservation_ids"),
            "scope_digest": admitted.get("scope_digest"),
        }
        reasons = []
        for key, value in expected.items():
            if not _same_json(payload.get(key), value):
                reasons.append(f"launch {key} does not bind canonical admission")
        if not _is_digest(launch.get("event_digest")):
            reasons.append("launch event digest is malformed")
        return tuple(reasons)

    @staticmethod
    def _validate_terminal(
        admission: Mapping[str, Any],
        launch: Mapping[str, Any],
        terminal: Mapping[str, Any],
    ) -> tuple[str, ...]:
        payload = terminal["payload"]
        kind = terminal["kind"]
        outcome = payload.get("outcome")
        reasons = []
        if kind == "WORKER_UNKNOWN":
            if outcome != "unknown":
                reasons.append("WORKER_UNKNOWN has a non-UNKNOWN outcome")
        elif _canonical_text(outcome) is None or outcome == "unknown":
            reasons.append("WORKER_TERMINAL outcome is malformed")

        optional_bindings = {
            "admission_event_digest": admission["event_digest"],
            "attempt_id": admission["payload"].get("attempt_id"),
            "launch_event_digest": launch["event_digest"],
            "permit_digest": admission["payload"].get("permit_digest"),
            "permit_id": admission["payload"].get("permit_id"),
        }
        for key, value in optional_bindings.items():
            if key in payload and not _same_json(payload.get(key), value):
                reasons.append(f"terminal {key} does not bind canonical launch")
        if not _is_digest(terminal.get("event_digest")):
            reasons.append("terminal event digest is malformed")
        return tuple(reasons)
