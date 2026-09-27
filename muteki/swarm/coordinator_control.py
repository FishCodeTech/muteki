"""Operator control, HITL drain, and standing guidance. Moved from coordinator_flags.py."""

from __future__ import annotations

import asyncio
import hashlib
import time
from typing import TYPE_CHECKING, Any, Optional

if TYPE_CHECKING:
    pass

from muteki.swarm.coordinator_control_command import (
    _control_scope_parts as _control_scope_parts,
    _control_target_solvers as _control_target_solvers,
    prepare_control_command,
)
from muteki.swarm.coordinator_control_guidance import (
    _handle_clear_guidance_control,
    _handle_mark_false_control,
    apply_control_context,
)
from muteki.swarm.coordinator_control_runtime import (
    _handle_freeze_control,
    _handle_run_lifecycle_control,
    _handle_thaw_control,
)
from muteki.swarm.coordinator_control_workers import (
    _handle_dismiss_control,
    _handle_worker_control,
)
from muteki.swarm.swarm_support import (
    _STANDING_MAX,
    ControlShutdownIncomplete,
)

def _resolve_control_text(self, text: Any) -> str:
    """Materialise secret references transiently, at worker delivery only."""
    value = str(text or "")
    if not value.startswith("secret://"):
        return value
    resolver = getattr(self, "_secret_resolver", None)
    if not callable(resolver):
        return value
    try:
        return str(resolver(value))
    except Exception:
        # Never leak resolver details (which may contain the secret/path) into
        # a command receipt.  The unresolved opaque ref is safe to retain.
        return value


def _materialize_reserved_control_text(self, text: Any) -> str:
    """Resolve a reserved prompt secret, failing closed on any ambiguity."""
    value = str(text or "")
    if not value.startswith("secret://"):
        return value
    resolver = getattr(self, "_secret_resolver", None)
    if not callable(resolver):
        raise RuntimeError("reserved secret material is unavailable")
    try:
        resolved = str(resolver(value) or "")
    except Exception:
        raise RuntimeError("reserved secret material is unavailable") from None
    if not resolved or resolved.startswith("secret://"):
        raise RuntimeError("reserved secret material is unavailable")
    return resolved


@staticmethod
def _ack_control(
    cmd: dict[str, Any],
    *,
    state: str,
    detail: str,
    target_ids: Optional[list[str]] = None,
    metadata: Optional[dict[str, Any]] = None,
) -> None:
    future = cmd.get("_control_ack")
    if future is None:
        return
    try:
        if future.done() or future.cancelled():
            return
        future.set_result({
            "state": state,
            "detail": detail,
            "target_ids": list(target_ids or []),
            "metadata": dict(metadata or {}),
        })
    except Exception:
        pass


def _set_control_frozen(self, target: str, frozen: bool) -> tuple[list[str], int]:
    selected = self._control_target_solvers(target)
    confirmed: list[str] = []
    failures = 0
    for worker in selected:
        sid = str(getattr(worker, "solver_id", "") or "")
        setter = getattr(worker, "_set_paused", None)
        if not callable(setter):
            failures += 1
            continue
        try:
            signalled = setter(frozen)
            if (signalled is not False
                    and bool(getattr(worker, "_paused", False)) is frozen):
                confirmed.append(sid)
                self._update_control_worker_status(
                    sid, "frozen" if frozen else "running")
            else:
                failures += 1
        except Exception:
            failures += 1
    return confirmed, failures


def _control_paused_ids(self, target: str) -> list[str]:
    """Observed process-level freeze projection for truthful partial receipts."""
    return [
        str(getattr(worker, "solver_id", "") or "")
        for worker in self._control_target_solvers(target)
        if bool(getattr(worker, "_paused", False))
    ]


def _contain_unfrozen_control_workers(self, target: str) -> list[str]:
    """Fail-closed fallback when signal compensation cannot be proven.

    A worker that cannot be returned to the canonical frozen state is asked to
    terminate through the normal runtime cancellation path.  The receipt still
    remains PARTIAL—cancellation request is not process-exit proof—but no such
    worker is knowingly allowed to continue consuming budget outside control.
    """
    requested: list[str] = []
    for worker in self._control_target_solvers(target):
        if bool(getattr(worker, "_paused", False)):
            continue
        sid = str(getattr(worker, "solver_id", "") or "")
        if self._cancel_solver(worker):
            requested.append(sid)
            # A successful cancel() call is a request, not process-exit proof.
            self._update_control_worker_status(sid, "cancel_requested")
    return requested


def _lease_scope_for_control(self, target: str) -> tuple[str, str]:
    kind, value = self._control_scope_parts(target)
    if kind == "solver":
        kind = "worker"
    if kind == "global":
        return "challenge", self.challenge.id
    if kind == "run":
        return "run", value or self.run_id
    return kind, value


def _begin_operator_help_freeze(self) -> bool:
    """Transactionally freeze a NEED_INPUT wait across process/lease/budget.

    Returns True only when this helper acquired the suspension. An existing
    explicit operator FREEZE remains owned by its original command and must not
    be thawed by the help-wait bracket.
    """
    if self._control_frozen:
        return False
    freeze_key = "__help__"
    if self._freeze_suspensions:
        raise RuntimeError("another freeze scope is already active")
    confirmed, failures = self._set_control_frozen("global", True)
    if failures:
        _rolled_back, rollback_failures = self._set_control_frozen(
            "global", False)
        if rollback_failures:
            self._contain_unfrozen_control_workers("global")
        raise RuntimeError("operator-help worker freeze was not confirmed")
    help_ids = [
        str(h.get("request_id") or h.get("id") or "")
        for h in self._pending_help
    ]
    help_digest = hashlib.sha256(str(help_ids).encode()).hexdigest()[:16]
    suspension_id = f"help:{self.run_id}:{help_digest}"
    if self.shared_graph is None:
        self._set_control_frozen("global", False)
        raise RuntimeError("operator-help lease graph is unavailable")
    try:
        self.shared_graph.suspend_active_leases(
            actor="control", suspension_id=suspension_id,
            scope_kind="challenge", scope_id=self.challenge.id,
            # Operational infinity: the append-only suspension remains the
            # owner until an explicit resume event, without a 7-day reclaim
            # cliff during an unattended operator wait.
            guard_s=1_000_000_000_000.0, reason="operator help wait")
    except Exception as exc:
        _rolled_back, rollback_failures = self._set_control_frozen(
            "global", False)
        if rollback_failures:
            self._contain_unfrozen_control_workers("global")
        raise RuntimeError("operator-help lease guard failed") from exc
    started = time.monotonic()
    self._freeze_suspensions[freeze_key] = suspension_id
    self._freeze_started_at[freeze_key] = started
    self._control_frozen = True
    self._operator_paused = True
    if self._budget_suspend_started is None:
        self._budget_suspend_started = started
    return True


def _end_operator_help_freeze(self, *, reason: str) -> None:
    """Restore a help-owned suspension; retain it on any failed fence."""
    freeze_key = "__help__"
    suspension_id = self._freeze_suspensions.get(freeze_key, "")
    if not suspension_id:
        return
    confirmed, failures = self._set_control_frozen("global", False)
    if failures:
        _refrozen, refreeze_failures = self._set_control_frozen(
            "global", True)
        if refreeze_failures:
            self._contain_unfrozen_control_workers("global")
        raise RuntimeError("operator-help worker thaw was not confirmed")
    started = self._freeze_started_at.get(freeze_key)
    duration = max(0.0, time.monotonic() - started) if started else 0.0
    try:
        if self.shared_graph is None:
            raise RuntimeError("operator-help lease graph is unavailable")
        self.shared_graph.resume_suspended_leases(
            actor="control", suspension_id=suspension_id,
            duration_s=duration, reason=reason)
    except Exception as exc:
        _refrozen, refreeze_failures = self._set_control_frozen(
            "global", True)
        if refreeze_failures:
            self._contain_unfrozen_control_workers("global")
        raise RuntimeError("operator-help lease restoration failed") from exc
    self._freeze_suspensions.pop(freeze_key, None)
    self._freeze_started_at.pop(freeze_key, None)
    self._control_frozen = False
    self._operator_paused = False
    if self._budget_suspend_started is not None:
        self._budget_suspended_total += max(
            0.0, time.monotonic() - self._budget_suspend_started)
        self._budget_suspend_started = None


def _control_continuation_id(
    self, command_id: str, *, required_when_unavailable: bool = False,
) -> str:
    """Return the exact context edge for a command without mutating the graph.

    ``required_when_unavailable`` is used only when command semantics already
    prove that this is an exact replacement edge (for example a decision
    answer or worker-scoped context).  A missing provider must then fail closed
    onto the deterministic id; it must never turn exact context into a global
    InsightBus broadcast.
    """
    if not command_id:
        return ""
    from muteki.control import (
        context_resource_id_for_command,
        continuation_intent_id_for_command,
    )
    context_id = context_resource_id_for_command(command_id)
    intent_id = continuation_intent_id_for_command(command_id)
    provider = getattr(self, "_context_provider", None)
    if not callable(provider):
        return intent_id if required_when_unavailable else ""
    try:
        try:
            resources = provider(active_only=False)
        except TypeError:
            resources = provider()
        resource = next(
            (row for row in resources
             if str(getattr(row, "context_id", "")) == context_id),
            None,
        )
        if resource is None or str(
                getattr(resource, "scope", "") or "") != f"intent:{intent_id}":
            return intent_id if required_when_unavailable else ""
        return intent_id
    except Exception:
        # A configured durable provider that is transiently unreadable cannot
        # prove the command is global. Fail closed onto its deterministic edge;
        # proposal/status checks below will return UNKNOWN, never broadcast.
        return intent_id


def _propose_control_continuation(
    self, *, command_id: str, action: str, target: str,
) -> str:
    """Materialize the exact graph edge backing worker-scoped context.

    The context row is already durable when the runtime port is called.  This
    method deliberately inspects only its id/scope (never secret content) and
    creates the deterministic open intent that a replacement worker can claim.
    """
    intent_id = self._control_continuation_id(command_id)
    if not intent_id:
        return ""
    if self.shared_graph is None:
        return ""
    try:
        from muteki.control import context_resource_id_for_command
        context_id = context_resource_id_for_command(command_id)
        status_provider = getattr(self, "_context_status_provider", None)
        if callable(status_provider):
            if status_provider(context_id) != "active":
                return ""
        else:
            provider = getattr(self, "_context_provider", None)
            if not callable(provider) or not any(
                str(getattr(row, "context_id", "")) == context_id
                for row in provider()
            ):
                return ""
        self.shared_graph.propose_intent(
            actor="operator",
            intent_id=intent_id,
            goal=("Continue the blocked execution using the operator-provided "
                  f"context for control command {command_id}."),
            payload={
                "source": "operator_continuation",
                "source_command_id": command_id,
                "action": action,
                "requested_scope": target,
                "priority": "operator",
                "worker_class": "shell_agent",
            },
        )
        return intent_id
    except Exception:
        return ""


async def _reconcile_standing_guidance(self) -> list[str]:
    """Drain durable clear/reset commands into the shared graph exactly once.

    Typed context and the evidence graph intentionally use separate SQLite
    stores. The control command is therefore an outbox record; the graph's
    ``apply_standing_clear`` transaction stores directive tombstones and its
    command-id marker together. Any crash point is repaired on the next Swarm
    start without replaying later operator guidance.
    """
    operation_provider = getattr(self, "_standing_clear_provider", None)
    if not callable(operation_provider):
        return []
    operations = list(operation_provider())
    if not operations:
        return []
    if self.shared_graph is None:
        raise RuntimeError("standing-clear reconciliation graph is unavailable")
    context_provider = getattr(self, "_context_provider", None)
    context_expirer = getattr(self, "_context_expirer", None)
    applied_commands: list[str] = []

    for operation in operations:
        def _value(name: str, default: Any = "") -> Any:
            if isinstance(operation, dict):
                return operation.get(name, default)
            return getattr(operation, name, default)

        command_id = str(_value("command_id") or "").strip()
        if not command_id:
            raise RuntimeError("standing-clear outbox record has no command_id")
        action = str(_value("action", "clear_standing") or "clear_standing")
        actor = str(_value("actor", "operator") or "operator")
        exact_text = str(_value("text") or "").strip()
        cutoff_raw = _value("cutoff_before", None)
        cutoff_before = (
            float(cutoff_raw) if cutoff_raw is not None else None
        )
        eligible_ids = {
            str(value or "").strip()
            for value in (_value("eligible_standing_command_ids", ()) or ())
            if str(value or "").strip()
        }

        def _matching_context(resource: Any) -> bool:
            if not bool(getattr(resource, "standing", False)):
                return False
            if exact_text and str(
                    getattr(resource, "content", "") or "") != exact_text:
                return False
            metadata = dict(getattr(resource, "metadata", {}) or {})
            source_id = str(
                metadata.get("source_command_id") or "").strip()
            if source_id:
                # Closed-set eligibility is the concurrent recovery fence.
                # Unknown ids may have committed after the outbox snapshot.
                return source_id in eligible_ids
            return cutoff_before is None or float(
                getattr(resource, "created_at", 0.0) or 0.0
            ) < cutoff_before

        # Context revocation is the first half of the absence guarantee. A
        # crash directly after PERSISTED (before ControlActor's companion
        # pass) is repaired here as well. The next-command cutoff protects
        # standing context added after this clear.
        if callable(context_provider):
            active_resources = list(context_provider())
            matching = [
                resource for resource in active_resources
                if _matching_context(resource)
            ]
            if matching and not callable(context_expirer):
                raise RuntimeError(
                    "standing-clear context expirer is unavailable")
            for resource in matching:
                context_expirer(
                    str(getattr(resource, "context_id", "") or ""),
                    actor=actor, reason=f"{action}:startup-reconcile")
            remaining = [
                resource for resource in context_provider()
                if _matching_context(resource)
            ]
            if remaining:
                raise RuntimeError(
                    "standing-clear context expiration was not confirmed")

        result = self.shared_graph.apply_standing_clear(
            command_id=command_id,
            actor=actor,
            text="" if exact_text.startswith("secret://") else exact_text,
            cutoff_before=cutoff_before,
            eligible_command_ids=sorted(eligible_ids),
            match_by_source_ids=exact_text.startswith("secret://"),
        )
        if not bool(result.get("already_applied", False)):
            applied_commands.append(command_id)
            try:
                await self._emit_coord_bb(
                    "operator_directive_changed",
                    action=action,
                    command_id=command_id,
                    recovered=True,
                    expired_directives=list(
                        result.get("expired_directives") or []),
                )
            except Exception:
                # Graph state is canonical and replayable; telemetry is only a
                # projection and must never roll back or mask reconciliation.
                pass

    # Rebuild the volatile prompt projection from the now-canonical graph so
    # a bounded replay retains standing guidance added by later commands.
    try:
        self._standing_guidance = [
            str(row.get("text") or "")
            for row in self.shared_graph.operator_directives(active_only=True)
            if row.get("standing") and row.get("text")
        ][-_STANDING_MAX:]
    except Exception:
        # Failure to rebuild a volatile projection is safe: new workers also
        # read the canonical graph, and startup teardown remains fenced by run().
        pass
    return applied_commands


async def _reconcile_control_continuations(self) -> list[str]:
    """Repair the durable context -> graph outbox edge after crash/offline.

    Context lives in the control journal while intents live in the evidence
    graph, so they cannot share one SQLite transaction.  On every Swarm start we
    deterministically replay only the selector metadata (never context/secret
    content); graph dedupe makes this idempotent.
    """
    provider = getattr(self, "_context_provider", None)
    if not callable(provider) or self.shared_graph is None:
        return []
    repaired: list[str] = []
    retired: list[tuple[str, str]] = []
    try:
        active_resources = list(provider())
    except Exception:
        return []
    try:
        resources = list(provider(active_only=False))
    except TypeError:
        resources = active_resources
    except Exception:
        resources = active_resources
    active_ids = {
        str(getattr(resource, "context_id", "") or "")
        for resource in active_resources
    }
    status_provider = getattr(self, "_context_status_provider", None)
    try:
        open_ids = {
            str(row.get("intent_id") or "") for row in self._open_intents()
        }
    except Exception:
        open_ids = set()
    state_reader = getattr(self.shared_graph, "intent_claim_state", None)
    from muteki.control import continuation_intent_id_for_command
    for resource in resources:
        try:
            metadata = dict(getattr(resource, "metadata", {}) or {})
            command_id = str(metadata.get("source_command_id") or "")
            if not command_id:
                continue
            intent_id = continuation_intent_id_for_command(command_id)
            if str(getattr(resource, "scope", "") or "") != f"intent:{intent_id}":
                continue
            context_id = str(getattr(resource, "context_id", "") or "")
            if context_id not in active_ids:
                status = "inactive"
                if callable(status_provider):
                    try:
                        status = str(status_provider(context_id) or status)
                    except Exception:
                        pass
                if intent_id in open_ids:
                    # A finite/expired/unknown edge can no longer be delivered.
                    # Close it append-only instead of leaving an operator-priority
                    # intent to churn through RequiredContextUnavailable forever.
                    self.shared_graph.conclude_intent(
                        actor="coordinator", intent_id=intent_id,
                        result=f"context_{status}",
                        result_detail=(
                            "operator continuation retired during recovery: "
                            f"context delivery status={status}"),
                    )
                    retired.append((intent_id, status))
                    open_ids.discard(intent_id)
                continue
            # Reconciliation is a live outbox poll as well as a startup pass.
            # Do not emit a synthetic "recovered" event on every coordinator
            # tick for an edge that is already materialized (open, claimed, or
            # terminal). A missing row is the only repairable postcondition.
            if callable(state_reader):
                try:
                    if dict(state_reader(intent_id) or {}):
                        continue
                except Exception:
                    # An unreadable postcondition is not permission to claim a
                    # recovery. Let the next live tick retry the read.
                    continue
            elif intent_id in open_ids:
                continue
            self.shared_graph.propose_intent(
                actor="control-recovery",
                intent_id=intent_id,
                goal=("Continue the blocked execution using the operator-provided "
                      f"context for control command {command_id}."),
                payload={
                    "source": "operator_continuation_recovery",
                    "source_command_id": command_id,
                    "action": str(metadata.get("action") or "context"),
                    "requested_scope": str(metadata.get("command_scope") or ""),
                    "priority": "operator",
                    "worker_class": "shell_agent",
                },
            )
            if callable(state_reader):
                try:
                    if not dict(state_reader(intent_id) or {}):
                        continue
                except Exception:
                    continue
            repaired.append(intent_id)
            open_ids.add(intent_id)
        except Exception:
            continue
    for intent_id in repaired:
        try:
            await self._emit_coord_bb(
                "intent_proposed", intent_id=intent_id,
                goal="recovered operator-scoped continuation",
                recovered=True)
        except Exception:
            # Graph writes above are canonical. Blackboard/UI projection is
            # replayable telemetry and cannot invalidate recovery.
            pass
    for intent_id, status in retired:
        try:
            await self._emit_coord_bb(
                "intent_state_changed", intent_id=intent_id,
                dispatch_state="closed", recovered=True,
                reason=f"context_{status}")
        except Exception:
            pass
    return repaired


async def _supervise_control_drain(self) -> None:
    """Keep the control inbox available after a command-level watchdog abort.

    A claimed command callback is allowed to be arbitrary runtime code and can
    suppress ``CancelledError``.  A watchdog therefore fences the old epoch,
    but deliberately does *not* run a replacement mutator concurrently with a
    still-live stale handler. Availability degrades to UNKNOWN while that owner
    drains; linearizability never degrades. Cancelling the supervisor retains
    ownership until all such coroutines actually exit.
    """
    restart_event = asyncio.Event()
    if isinstance(getattr(self, "control_ready", None), asyncio.Event):
        self.control_ready.set()
    self._control_restart_event = restart_event
    self._control_consumer_epoch = int(
        getattr(self, "_control_consumer_epoch", 0) or 0)
    orphans: set[asyncio.Task[Any]] = set()
    self._control_orphan_tasks = orphans
    child: Optional[asyncio.Task[Any]] = None
    restart_wait: Optional[asyncio.Task[Any]] = None

    def _retire(task: asyncio.Task[Any]) -> None:
        orphans.discard(task)
        try:
            task.result()
        except BaseException:
            pass
        if not orphans:
            self._clear_shutdown_incomplete("hitl_orphan")

    try:
        while True:
            restart_event.clear()
            epoch = int(self._control_consumer_epoch)
            child = asyncio.create_task(
                self._drain_hitl(consumer_epoch=epoch),
                name=f"hitl-drain-envelope-{epoch}",
            )
            restart_wait = asyncio.create_task(
                restart_event.wait(),
                name=f"hitl-drain-restart-{epoch}",
            )
            done, _pending = await asyncio.wait(
                {child, restart_wait},
                return_when=asyncio.FIRST_COMPLETED,
            )
            if restart_wait in done:
                # Publish the dequeue fence before cancelling the old child.
                # Do not start a replacement until this handler exits: epoch
                # checks cannot roll back mutations it performs after an await,
                # so concurrent generations would allow stale PAUSE to land
                # after a newer RESUME while both receipts looked successful.
                self._control_consumer_epoch = epoch + 1
                restart_event.clear()
                # QueueControlPort normally issued the first cancellation before
                # setting this event.  Do not issue a second one while the child
                # is handling/suppressing that exception: a duplicate cancel
                # would accidentally make the adversarial callback cooperative
                # and hide the very failure mode this generation fence handles.
                if not child.done() and child.cancelling() == 0:
                    child.cancel()
                orphans.add(child)
                child.add_done_callback(_retire)
                restart_wait = None
                try:
                    await asyncio.shield(child)
                except asyncio.CancelledError:
                    current = asyncio.current_task()
                    if current is not None and current.cancelling():
                        raise
                    # Cooperative child cancellation; ownership is released.
                except Exception:
                    pass
                child = None
                continue

            restart_wait.cancel()
            await asyncio.gather(restart_wait, return_exceptions=True)
            restart_wait = None
            try:
                child.result()
            except asyncio.CancelledError:
                # A child-only cancel from a legacy caller is treated like a
                # restart.  Supervisor cancellation is handled by the outer
                # cancellation branch below.
                self._control_consumer_epoch = epoch + 1
                child = None
                await asyncio.sleep(0)
                continue
            except Exception:
                self._control_consumer_epoch = epoch + 1
                child = None
                await asyncio.sleep(0)
                continue
            child = None
            return
    except asyncio.CancelledError:
        # Fence every consumer owned by this supervisor before asking it to
        # stop.  A shutdown-suppressing child can finish its current envelope,
        # but can never claim another one if this Swarm is resumed/restarted.
        self._control_consumer_epoch = int(
            getattr(self, "_control_consumer_epoch", 0) or 0) + 1
        if restart_wait is not None:
            restart_wait.cancel()
        if child is not None and not child.done():
            if child not in orphans:
                orphans.add(child)
                child.add_done_callback(_retire)
            child = None
        owned_orphans = tuple(orphans)
        for orphan in owned_orphans:
            if not orphan.done():
                orphan.cancel()
        if restart_wait is not None:
            await asyncio.gather(restart_wait, return_exceptions=True)
            restart_wait = None
        # Epoch fencing restores availability, but it is not an exit proof. Wait
        # only a bounded interval so server shutdown cannot hang forever. If an
        # adversarial callback still suppresses cancellation, retain ownership
        # and surface an explicit incomplete state; the coordinator then refuses
        # to finalize the graph underneath it.
        pending: set[asyncio.Task[Any]] = set()
        if owned_orphans:
            _done, pending = await asyncio.wait(
                owned_orphans,
                timeout=max(0.0, float(getattr(
                    self, "control_shutdown_timeout", 2.0))),
            )
        if pending:
            self._mark_shutdown_incomplete("hitl_orphan")
            raise ControlShutdownIncomplete(
                f"{len(pending)} control handler(s) still own runtime state")
        raise
    finally:
        if getattr(self, "_control_restart_event", None) is restart_event:
            self._control_restart_event = None


async def _drain_hitl(self, *, consumer_epoch: Optional[int] = None) -> None:
    """Background: pull human commands off hitl_inbox and broadcast them to
    every solver via the InsightBus. Runs until cancelled. Each item is a
    dict {target, action, text} (the shape RunManager.post_hitl enqueues)."""
    if self.hitl_inbox is None:
        return
    if consumer_epoch is None:
        consumer_epoch = int(
            getattr(self, "_control_consumer_epoch", 0) or 0)
    while True:
        if consumer_epoch != int(
                getattr(self, "_control_consumer_epoch", 0) or 0):
            return
        cmd = await self.hitl_inbox.get()
        if consumer_epoch != int(
                getattr(self, "_control_consumer_epoch", 0) or 0):
            # Epoch changed while this consumer was blocked in Queue.get().
            # Return it before balancing the old claim so Queue.join never
            # observes a transient zero while the command is still pending;
            # never let a stale generation apply it.
            await self.hitl_inbox.put(cmd)
            try:
                self.hitl_inbox.task_done()
            except Exception:
                pass
            return
        try:
            context = await prepare_control_command(
                self, cmd, consumer_epoch=consumer_epoch)
            if context is None:
                continue
            if await _handle_run_lifecycle_control(self, context):
                continue
            if await _handle_freeze_control(self, context):
                continue
            if await _handle_thaw_control(self, context):
                continue
            if await _handle_worker_control(self, context):
                continue
            if await _handle_dismiss_control(self, context):
                continue
            if await _handle_clear_guidance_control(self, context):
                continue
            if await _handle_mark_false_control(self, context):
                continue
            await apply_control_context(self, context)
        except Exception:
            # a malformed command must never kill the drain loop
            if isinstance(cmd, dict):
                self._ack_control(
                    cmd, state="failed",
                    detail="coordinator rejected malformed control command",
                    metadata={"code": "coordinator_apply_failure"})
            continue
        finally:
            if isinstance(cmd, dict):
                self._ack_control(
                    cmd, state="unknown",
                    detail="control consumer exited before a terminal effect was confirmed",
                    metadata={"code": "consumer_interrupted"})
                cmd.pop("_control_consumer_task", None)
                cmd.pop("_control_restart_event", None)
                cmd.pop("_control_consumer_epoch", None)
            try:
                self.hitl_inbox.task_done()
            except Exception:
                pass
