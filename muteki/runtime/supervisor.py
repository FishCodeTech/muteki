"""Single owner for every admitted run-plane coroutine and its terminal receipt."""

from __future__ import annotations

import asyncio
import time
from collections.abc import Awaitable, Callable, Mapping
from typing import Any

from muteki.epistemic.contracts import canonical_digest
from muteki.epistemic.cognitive_events_v1 import (
    COGNITIVE_ASSIGNMENT_SCHEMA_ID,
    COGNITIVE_BINDING_ACTOR,
    COGNITIVE_EXECUTION_OBSERVED,
    COGNITIVE_EXPERIMENT_ASSIGNED,
    COGNITIVE_RUNTIME_CONTEXT_ASSIGNMENT_SCHEMA_ID,
    COGNITIVE_RUNTIME_EXECUTABLE_ASSIGNMENT_SCHEMA_ID,
    COGNITIVE_RUNTIME_REPRODUCTION_ASSIGNMENT_SCHEMA_ID,
    cognitive_execution_payload,
)
from muteki.epistemic.sqlite_store import (
    CommandEvent,
    EpistemicSQLiteStore,
    ProjectionMutation,
)
from muteki.runtime.contracts import (
    AttemptPermit,
    ExecutionScope,
    RuntimeEvaluationBindingV2,
)
from muteki.runtime.c6_transport import C6HostLaunchInterlock
from muteki.runtime.permit_resolver import (
    CanonicalPermitResolver,
    PermitResolutionError,
)
from muteki.runtime.usage import UsageReport


class LaunchRejected(RuntimeError):
    pass


class RunSupervisor:
    def __init__(self, *, store: EpistemicSQLiteStore, scope: ExecutionScope) -> None:
        self._store = store
        self._scope = scope
        self._resolver = CanonicalPermitResolver(store=store, scope=scope)
        self._c6_interlock = C6HostLaunchInterlock()
        self._tasks: dict[str, asyncio.Task[Any]] = {}
        self._permits: dict[str, AttemptPermit] = {}
        self._terminal_failures: dict[str, BaseException] = {}
        self._cancel_requested: set[str] = set()
        self._entered: set[str] = set()
        self._preentry_cancel_callbacks: dict[str, Callable[[], None]] = {}
        self._accepting = True

    @property
    def c6_interlock(self) -> C6HostLaunchInterlock:
        """Supervisor-owned fence for the actual C6 host-Popen boundary."""

        return self._c6_interlock

    def cancellation_requested(self, permit_id: str) -> bool:
        """Return whether cancellation has fenced this owned permit/task."""

        current = asyncio.current_task()
        return bool(
            permit_id in self._cancel_requested
            or (current is not None and current.cancelling())
        )

    def require_not_cancelled(self, permit_id: str) -> None:
        """Reject post-cancel outcomes before they can create live-session effects."""

        if self.cancellation_requested(permit_id):
            raise asyncio.CancelledError

    def spawn_owned(
        self,
        permit: AttemptPermit,
        coroutine_factory: Callable[[], Awaitable[Any]],
        *,
        now_ns: int,
        on_preentry_cancel: Callable[[], None] | None = None,
    ) -> asyncio.Task[Any]:
        return self._spawn_owned(
            permit,
            coroutine_factory,
            now_ns=now_ns,
            shadow=False,
            shadow_v2=False,
            on_preentry_cancel=on_preentry_cancel,
        )

    def spawn_owned_shadow(
        self,
        permit: AttemptPermit,
        coroutine_factory: Callable[[], Awaitable[Any]],
        *,
        now_ns: int,
    ) -> asyncio.Task[Any]:
        return self._spawn_owned(
            permit, coroutine_factory, now_ns=now_ns, shadow=True, shadow_v2=False
        )

    def spawn_owned_shadow_v2(
        self,
        permit: AttemptPermit,
        coroutine_factory: Callable[[], Awaitable[Any]],
        *,
        runtime_binding: RuntimeEvaluationBindingV2,
        now_ns: int,
    ) -> asyncio.Task[Any]:
        """Launch a production-disabled C6 v2 role-bound attempt under the supervisor."""

        return self._spawn_owned(
            permit,
            coroutine_factory,
            now_ns=now_ns,
            shadow=False,
            shadow_v2=True,
            runtime_binding_v2=runtime_binding,
        )

    def _spawn_owned(
        self,
        permit: AttemptPermit,
        coroutine_factory: Callable[[], Awaitable[Any]],
        *,
        now_ns: int,
        shadow: bool,
        shadow_v2: bool,
        runtime_binding_v2: RuntimeEvaluationBindingV2 | None = None,
        on_preentry_cancel: Callable[[], None] | None = None,
    ) -> asyncio.Task[Any]:
        if not self._accepting:
            raise LaunchRejected("supervisor admission is quiescing")
        try:
            if shadow_v2:
                if type(runtime_binding_v2) is not RuntimeEvaluationBindingV2:
                    raise PermitResolutionError(
                        "v2 launch requires an exact RuntimeEvaluationBindingV2"
                    )
                self._resolver.claim_shadow_launch_v2(
                    permit,
                    runtime_binding=runtime_binding_v2,
                    now_ns=now_ns,
                )
            elif shadow:
                self._resolver.claim_shadow_launch(permit, now_ns=now_ns)
            else:
                self._resolver.claim_launch(permit, now_ns=now_ns)
        except PermitResolutionError as exc:
            raise LaunchRejected(str(exc)) from exc
        if on_preentry_cancel is not None and not callable(on_preentry_cancel):
            raise TypeError("on_preentry_cancel must be callable")
        # The resolver has appended WORKER_LAUNCH_PREPARED at this point.  Register
        # its in-memory counterpart before a task can see a C6 runner; terminal,
        # quiesce, and emergency paths all revoke this same fence.
        self._c6_interlock.register(permit=permit)
        coroutine = self._run_owned(
            permit, coroutine_factory, shadow=shadow, shadow_v2=shadow_v2
        )
        try:
            task = asyncio.create_task(
                coroutine,
                name=f"attempt-{permit.lease.attempt.attempt_id}",
            )
            task_cancel = task.cancel

            def cancel_owned(msg: object | None = None) -> bool:
                # Task.cancel() is a synchronous cancellation boundary.  Latch the
                # request before asyncio can inject CancelledError so a child cannot
                # erase the authority fence with Task.uncancel() and then publish an
                # apparently successful result during unwind.  Keep the concrete
                # event-loop Task (and any configured task-factory behavior) intact.
                if not task.done():
                    self._cancel_requested.add(permit.permit_id)
                return task_cancel(msg)

            task.cancel = cancel_owned
        except BaseException:
            coroutine.close()
            self._c6_interlock.revoke(
                permit=permit, reason="owned task creation failed"
            )
            raise
        self._tasks[permit.permit_id] = task
        self._permits[permit.permit_id] = permit
        if on_preentry_cancel is not None:
            self._preentry_cancel_callbacks[permit.permit_id] = on_preentry_cancel
        task.add_done_callback(
            lambda completed, owned_permit=permit, owned_shadow=shadow,
            owned_shadow_v2=shadow_v2: self._owned_task_done(
                owned_permit,
                completed,
                shadow=owned_shadow,
                shadow_v2=owned_shadow_v2,
            )
        )
        return task

    def _owned_task_done(
        self,
        permit: AttemptPermit,
        task: asyncio.Task[Any],
        *,
        shadow: bool,
        shadow_v2: bool,
    ) -> None:
        """Close the canonical owner when cancellation prevents first entry."""

        permit_id = permit.permit_id
        if permit_id in self._entered:
            self._entered.discard(permit_id)
            self._tasks.pop(permit_id, None)
            self._permits.pop(permit_id, None)
            self._preentry_cancel_callbacks.pop(permit_id, None)
            self._cancel_requested.discard(permit_id)
            return
        # A completed task that never entered can only have been cancelled before
        # its coroutine's first bytecode. Keep this explicit so future task-factory
        # changes cannot turn a non-cancelled anomaly into an UNKNOWN receipt.
        if not task.cancelled():
            exc = LaunchRejected("owned task completed without entering supervisor")
            self._terminal_failures[permit_id] = exc
            self._tasks.pop(permit_id, None)
            self._permits.pop(permit_id, None)
            self._preentry_cancel_callbacks.pop(permit_id, None)
            self._cancel_requested.discard(permit_id)
            return
        self._cancel_requested.add(permit_id)
        try:
            callback = self._preentry_cancel_callbacks.get(permit_id)
            if callback is not None:
                callback()
            self._terminal_receipt(
                permit, "unknown", shadow=shadow, shadow_v2=shadow_v2
            )
        except BaseException as exc:
            self._terminal_failures[permit_id] = exc
        finally:
            self._tasks.pop(permit_id, None)
            self._permits.pop(permit_id, None)
            self._preentry_cancel_callbacks.pop(permit_id, None)
            self._cancel_requested.discard(permit_id)

    async def _run_owned(
        self,
        permit: AttemptPermit,
        coroutine_factory: Callable[[], Awaitable[Any]],
        *,
        shadow: bool,
        shadow_v2: bool,
    ) -> Any:
        permit_id = permit.permit_id
        self._entered.add(permit_id)
        try:
            result = await coroutine_factory()
        except asyncio.CancelledError:
            self._cancel_requested.add(permit_id)
            self._terminal_receipt(
                permit, "unknown", shadow=shadow, shadow_v2=shadow_v2
            )
            raise
        except BaseException:
            self._terminal_receipt(
                permit, "unknown", shadow=shadow, shadow_v2=shadow_v2
            )
            raise
        else:
            if self.cancellation_requested(permit.permit_id):
                self._terminal_receipt(
                    permit, "unknown", shadow=shadow, shadow_v2=shadow_v2
                )
                raise asyncio.CancelledError
            success_outcome = "observed"
            if shadow_v2:
                role = self._v2_role_for_permit(permit.permit_id)
                success_outcome = "proposal" if role == "observer" else "observed"
            self._terminal_receipt(
                permit, success_outcome, shadow=shadow, shadow_v2=shadow_v2
            )
            return result
        finally:
            # ``_owned_task_done`` removes the entered marker after Task completion;
            # retaining it through this frame lets that callback distinguish an
            # ordinary completed coroutine from cancellation before first entry.
            # The sticky cancellation latch is likewise cleared there so retained
            # synchronous callbacks remain fenced throughout task unwinding.
            self._tasks.pop(permit_id, None)
            self._permits.pop(permit_id, None)
            self._preentry_cancel_callbacks.pop(permit_id, None)

    def _v2_role_for_permit(self, permit_id: str) -> str:
        rows = [
            row
            for row in self._store.event_rows(kind="C6_EVAL_V2_ATTEMPT_BOUND")
            if row["payload"].get("permit_id") == permit_id
        ]
        if len(rows) != 1:
            raise LaunchRejected("v2 terminal has no unique evaluation attempt binding")
        role = rows[0]["payload"].get("role")
        if type(role) is not str or not role:
            raise LaunchRejected("v2 evaluation attempt role is malformed")
        return role

    @staticmethod
    def _v2_budget_terminal(
        admission: Mapping[str, Any], *, unknown: bool
    ) -> tuple[str, str, dict[str, Any]]:
        raw_reserved = admission.get("requested_budget")
        raw_reservation_ids = admission.get("reservation_ids")
        if not isinstance(raw_reserved, Mapping) or type(raw_reservation_ids) not in {
            list,
            tuple,
        }:
            raise LaunchRejected("v2 terminal lost its admission budget contract")
        reserved = dict(raw_reserved)
        reservation_ids = tuple(raw_reservation_ids)
        try:
            report = UsageReport.from_observed_and_reservation(
                reserved=reserved,
                observed={},
                complete_axes=frozenset(),
            )
        except (TypeError, ValueError) as exc:
            raise LaunchRejected("v2 terminal budget contract is malformed") from exc
        common = {
            "attempt_id": admission.get("attempt_id"),
            "reservation_ids": reservation_ids,
            "usage_report": report.canonical_body(),
            "usage_report_digest": report.digest,
        }
        if unknown:
            return (
                "BUDGET_USAGE_UNKNOWN",
                "budget_unknown",
                {
                    **common,
                    "held_usage": dict(report.pessimistic_usage()),
                    "revision": 1,
                },
            )
        # Worker success and usage observability are independent.  V2 has no
        # measured-usage port, so close the owner by charging the full ceiling
        # under an explicitly unobserved pessimistic schema.  Never relabel that
        # charge as actual or OBSERVED usage.
        return (
            "BUDGET_PESSIMISTICALLY_SETTLED",
            "budget_pessimistic_settle",
            {
                **common,
                "charge_basis": "unobserved_reservation_ceiling",
                "charged_usage": dict(report.pessimistic_usage()),
                "settlement_revision": 1,
            },
        )

    def _terminal_receipt(
        self,
        permit: AttemptPermit,
        outcome: str,
        *,
        shadow: bool,
        shadow_v2: bool = False,
    ) -> None:
        # The occurrence identity is stable; retry returns the original terminal
        # receipt rather than inventing a second terminalization.
        if shadow and shadow_v2:
            exc = LaunchRejected("shadow terminal boundaries cannot be combined")
            self._terminal_failures[permit.permit_id] = exc
            raise exc
        self._c6_interlock.revoke(
            permit=permit,
            reason="supervisor is terminalizing the admitted worker",
        )
        occurred_at_ns = time.time_ns()
        admissions = [
            row
            for row in self._store.event_rows(kind="ATTEMPT_ADMITTED")
            if row["payload"].get("permit_id") == permit.permit_id
        ]
        launches = [
            row
            for row in self._store.event_rows(kind="WORKER_LAUNCH_PREPARED")
            if row["payload"].get("permit_id") == permit.permit_id
        ]
        if len(admissions) != 1 or len(launches) != 1:
            exc = LaunchRejected("worker terminal lineage is not uniquely resolvable")
            self._terminal_failures[permit.permit_id] = exc
            raise exc
        cognitive_assignments = [
            row
            for row in self._store.event_rows(kind=COGNITIVE_EXPERIMENT_ASSIGNED)
            if row["payload"].get("permit_id") == permit.permit_id
        ]
        if len(cognitive_assignments) > 1:
            exc = LaunchRejected("worker has multiple canonical cognitive assignments")
            self._terminal_failures[permit.permit_id] = exc
            raise exc
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
                COGNITIVE_RUNTIME_REPRODUCTION_ASSIGNMENT_SCHEMA_ID,
            }
        ]
        if cognitive_assignments and not (
            eval_cognitive_assignments or runtime_context_assignments
        ):
            exc = LaunchRejected("cognitive assignment schema is unrecognized")
            self._terminal_failures[permit.permit_id] = exc
            raise exc
        if eval_cognitive_assignments and not shadow_v2:
            exc = LaunchRejected(
                "eval-v2 cognitive assignment requires the v2 terminal boundary"
            )
            self._terminal_failures[permit.permit_id] = exc
            raise exc
        if runtime_context_assignments and (shadow or shadow_v2):
            exc = LaunchRejected(
                "runtime-context cognitive assignment requires the ordinary terminal boundary"
            )
            self._terminal_failures[permit.permit_id] = exc
            raise exc
        if shadow_v2:
            role = self._v2_role_for_permit(permit.permit_id)
            if role == "observer" and outcome not in {"proposal", "unknown"}:
                exc = LaunchRejected(
                    "observer v2 can terminate only as proposal or UNKNOWN"
                )
                self._terminal_failures[permit.permit_id] = exc
                raise exc
            if role == "executor" and outcome not in {"observed", "unknown"}:
                exc = LaunchRejected(
                    "executor v2 can terminate only as observed or UNKNOWN"
                )
                self._terminal_failures[permit.permit_id] = exc
                raise exc
        terminal_event_id = f"event:launch-terminal:{permit.permit_id}"
        payload = {
            "admission_event_digest": admissions[0]["event_digest"],
            "attempt_digest": permit.lease.attempt.digest,
            "attempt_id": permit.lease.attempt.attempt_id,
            "launch_event_digest": launches[0]["event_digest"],
            "lease_digest": permit.lease.digest,
            "lease_id": permit.lease.lease_id,
            "outcome": outcome,
            "permit_digest": permit.digest,
            "permit_id": permit.permit_id,
            "scope_digest": permit.lease.attempt.scope.digest,
        }
        terminal_kind = "WORKER_TERMINAL" if outcome != "unknown" else "WORKER_UNKNOWN"
        command_payload: Mapping[str, Any] = payload
        events = [
            CommandEvent(
                f"event:launch-terminal:{permit.permit_id}",
                terminal_kind,
                "run-supervisor",
                occurred_at_ns,
                payload,
            )
        ]
        mutations = [
            ProjectionMutation(
                "worker_terminal_guard",
                {**payload, "terminal_event_id": terminal_event_id},
            )
        ]
        if shadow:
            launch_bindings = [
                row
                for row in self._store.event_rows(kind="C6_EVAL_LAUNCH_BOUND")
                if row["payload"].get("permit_id") == permit.permit_id
            ]
            if len(launch_bindings) != 1:
                exc = LaunchRejected(
                    "shadow terminal has no unique evaluation launch binding"
                )
                self._terminal_failures[permit.permit_id] = exc
                raise exc
            launch_binding = launch_bindings[0]
            sidecar = {
                "attempt_digest": permit.lease.attempt.digest,
                "attempt_id": permit.lease.attempt.attempt_id,
                "base_event_id": events[0].event_id,
                "base_payload_digest": canonical_digest(payload),
                "evaluation_binding_digest": launch_binding["payload"][
                    "evaluation_binding_digest"
                ],
                "launch_binding_event_digest": launch_binding["event_digest"],
                "permit_digest": permit.digest,
                "permit_id": permit.permit_id,
                "phase": "terminal",
                "schema_id": "muteki.c6-eval-binding-sidecar.v1",
                "scope_digest": permit.lease.attempt.scope.digest,
            }
            events.append(
                CommandEvent(
                    f"event:C6_EVAL_TERMINAL_BOUND:{permit.permit_id}",
                    "C6_EVAL_TERMINAL_BOUND",
                    "c6-evaluation-binding-authority",
                    occurred_at_ns,
                    sidecar,
                )
            )
            mutations.append(ProjectionMutation("c6_eval_terminal_bind_guard", sidecar))
        if shadow_v2:
            launch_bindings = [
                row
                for row in self._store.event_rows(kind="C6_EVAL_V2_LAUNCH_BOUND")
                if row["payload"].get("permit_id") == permit.permit_id
            ]
            if len(launch_bindings) != 1:
                exc = LaunchRejected(
                    "v2 shadow terminal has no unique evaluation launch binding"
                )
                self._terminal_failures[permit.permit_id] = exc
                raise exc
            launch_binding = launch_bindings[0]
            budget_kind, budget_mutation, budget_payload = self._v2_budget_terminal(
                admissions[0]["payload"], unknown=outcome == "unknown"
            )
            budget_event_id = (
                f"event:launch-budget-unknown:{permit.permit_id}"
                if budget_kind == "BUDGET_USAGE_UNKNOWN"
                else f"event:launch-budget-pessimistic:{permit.permit_id}"
            )
            events.append(
                CommandEvent(
                    budget_event_id,
                    budget_kind,
                    "run-supervisor",
                    occurred_at_ns,
                    budget_payload,
                )
            )
            mutations.append(ProjectionMutation(budget_mutation, budget_payload))
            sidecar = {
                "assignment_binding_digest": launch_binding["payload"][
                    "assignment_binding_digest"
                ],
                "attempt_digest": permit.lease.attempt.digest,
                "attempt_id": permit.lease.attempt.attempt_id,
                "base_event_id": events[0].event_id,
                "base_payload_digest": canonical_digest(payload),
                "budget_event_id": budget_event_id,
                "budget_event_kind": budget_kind,
                "budget_payload_digest": canonical_digest(budget_payload),
                "launch_binding_event_digest": launch_binding["event_digest"],
                "permit_digest": permit.digest,
                "permit_id": permit.permit_id,
                "phase": "terminal",
                "role": launch_binding["payload"]["role"],
                "attempt_role_binding_digest": launch_binding["payload"][
                    "attempt_role_binding_digest"
                ],
                "runtime_binding_digest": launch_binding["payload"][
                    "runtime_binding_digest"
                ],
                "schema_id": "muteki.c6-eval-binding-sidecar.v2",
                "scope_digest": permit.lease.attempt.scope.digest,
                "slot_id": launch_binding["payload"]["slot_id"],
                "terminal_outcome": outcome,
            }
            events.append(
                CommandEvent(
                    f"event:C6_EVAL_V2_TERMINAL_BOUND:{permit.permit_id}",
                    "C6_EVAL_V2_TERMINAL_BOUND",
                    "c6-evaluation-binding-v2-authority",
                    occurred_at_ns,
                    sidecar,
                )
            )
            mutations.append(
                ProjectionMutation("c6_eval_v2_terminal_bind_guard", sidecar)
            )
            command_payload_body: dict[str, Any] = {
                "budget_terminal": budget_payload,
                "evaluation_terminal_binding": sidecar,
                "worker_terminal": payload,
            }
            if eval_cognitive_assignments:
                assignment = eval_cognitive_assignments[0]
                cognitive_payload = cognitive_execution_payload(
                    assignment_event_digest=assignment["event_digest"],
                    assignment_payload=assignment["payload"],
                    terminal_payload=payload,
                    budget_event_id=budget_event_id,
                    budget_kind=budget_kind,
                    budget_payload=budget_payload,
                    evaluation_terminal_event_id=(
                        f"event:C6_EVAL_V2_TERMINAL_BOUND:{permit.permit_id}"
                    ),
                    evaluation_terminal_sidecar=sidecar,
                )
                events.append(
                    CommandEvent(
                        (f"event:{COGNITIVE_EXECUTION_OBSERVED}:{permit.permit_id}"),
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
                command_payload_body["cognitive_execution"] = cognitive_payload
            command_payload = command_payload_body
        try:
            authority = None
            if shadow:
                authority = self._store._evaluation_commit_capability
            elif shadow_v2:
                authority = (
                    self._store._evaluation_v2_cognitive_commit_capability
                    if eval_cognitive_assignments
                    else self._store._evaluation_v2_commit_capability
                )
            self._store.commit_command(
                command_id=f"launch-terminal:{permit.permit_id}",
                idempotency_key=f"launch-terminal:{permit.permit_id}",
                command_payload=command_payload,
                events=events,
                projection_mutations=mutations,
                authority_capability=authority,
                committed_at_ns=occurred_at_ns,
            )
        except BaseException as exc:
            self._terminal_failures[permit.permit_id] = exc
            raise
        if eval_cognitive_assignments:
            observed = [
                row
                for row in self._store.event_rows(kind=COGNITIVE_EXECUTION_OBSERVED)
                if row["payload"].get("permit_id") == permit.permit_id
            ]
            if len(observed) != 1:
                exc = LaunchRejected(
                    "cognitive terminal did not resolve its exact observation"
                )
                self._terminal_failures[permit.permit_id] = exc
                raise exc

    async def emergency_stop(self) -> None:
        self._accepting = False
        self._c6_interlock.revoke_all(reason="supervisor emergency stop")
        owned = list(self._tasks.items())
        self._cancel_requested.update(permit_id for permit_id, _task in owned)
        tasks = [task for _permit_id, task in owned]
        for task in tasks:
            task.cancel()
        if tasks:
            await asyncio.gather(*tasks, return_exceptions=True)

    async def drain(self) -> None:
        tasks = list(self._tasks.values())
        if tasks:
            await asyncio.gather(*tasks, return_exceptions=True)
        if self._terminal_failures:
            permit_ids = ",".join(sorted(self._terminal_failures))
            raise LaunchRejected(
                f"terminal receipt is unresolved for permits: {permit_ids}"
            )

    def quiesce(self) -> None:
        """Synchronously close launch admission before a drain can yield."""
        self._accepting = False
        self._c6_interlock.revoke_all(reason="supervisor quiesce")

    @property
    def active_count(self) -> int:
        return len(self._tasks)
