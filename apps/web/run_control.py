"""Run start, control, HITL, and resolve. Moved from run_manager.py."""

from __future__ import annotations

import asyncio
import os
from typing import Any, Optional

from apps.web.control_adapter import (
    ControlPayloadError,
    QueueControlPort,
    compile_control_command,
    control_paths,
    effect_event_payload,
    safe_receipt_detail,
    safe_hitl_echo,
)
from muteki.control import (
    ApplyResult,
    ControlAction,
    ControlActor,
    ControlAdmission,
    ControlScope,
    DecisionKind,
    DecisionRequest,
    DecisionStatus,
    EffectState,
    RunControlMode,
    SQLiteControlJournal,
    StateConflict,
)
from muteki.control.secrets import SecretStore
from muteki.core.events import Event, EventType, hitl_response_payload

from apps.web.run_standby_control import apply_standby_control

from apps.web.run_state import (  # noqa: F401
    LOG, Run, BoundRunConflictError, BoundRunStore, Driver,
    BOUND_RUN_ID_PREFIX, _safe_exception_detail, _runtime_error_id,
    _apply_blackboard_meta, _apply_operator_meta,
)

def has_control_command(self, run_id: str, command_id: str) -> bool:
    """持久控制 journal 是否已知该 command_id（RunGateway 幂等去重用）。"""
    run = self.runs.get(run_id)
    if run is None:
        return False
    _actor, journal, _secrets = self._ensure_control(run)
    return journal.get_command(str(command_id or "")) is not None


def _ensure_control(self, run: Run) -> tuple[ControlActor, SQLiteControlJournal,
                                              SecretStore]:
    """Create the one actor/journal/secret boundary owned by this run."""
    if (run.control_actor is not None and run.control_journal is not None
            and run.control_secrets is not None):
        return run.control_actor, run.control_journal, run.control_secrets

    db_path, secrets_path = control_paths(
        self.coordinator_control_dir(run.run_id))
    journal = SQLiteControlJournal.open(db_path=db_path, run_id=run.run_id)
    # SessionStore and the control journal are separate durable sinks. A crash
    # can persist HITL_REQUEST to JSONL before the live metadata sink appends
    # its DecisionRequest. Rebuild that idempotent edge before validating any
    # answer so a replayed card can never become permanently unanswerable.
    self._reconcile_decision_requests(run, journal)
    # This is the sole owner boundary for a fresh web runtime generation. Any
    # pre-Popen reservation left by the prior process has an unknowable delivery
    # outcome and must be terminalised append-only, never silently replayed.
    journal.recover_context_reservations(actor="web-runtime-recovery")
    secrets = SecretStore(secrets_path)

    def _live() -> bool:
        return run.task is not None and not run.task.done()

    try:
        ack_timeout = float(os.environ.get("MUTEKI_CONTROL_ACK_TIMEOUT", "2"))
    except (TypeError, ValueError):
        ack_timeout = 2.0
    try:
        claim_timeout = float(os.environ.get(
            "MUTEKI_CONTROL_CLAIM_TIMEOUT", "30"))
    except (TypeError, ValueError):
        claim_timeout = 30.0
    async def _standby_control(wire: dict[str, Any]) -> Any:
        return await apply_standby_control(
            self,
            run=run,
            journal=journal,
            claim_timeout=claim_timeout,
            wire=wire,
        )

    port = QueueControlPort(
        inbox=run.hitl,
        is_live=_live,
        ready=run.control_ready,
        worker_ready=run.worker_control_ready,
        ack_timeout=ack_timeout,
        claim_timeout=claim_timeout,
        standby_actions=tuple(
            self._STANDBY_ACTIONS | self._OFFLINE_CONTROL_ACTIONS),
        on_standby=_standby_control,
    )

    async def _cancel_main_runtime(command, targets, desired) -> ApplyResult:
        run_wide = command.scope.kind.value in {
            "global", "run", "challenge",
        }
        allows_run_termination_fallback = (
            command.action in {ControlAction.STOP, ControlAction.COMPLETE}
            or (
                command.action is ControlAction.FORCE_CANCEL
                and desired.mode is RunControlMode.TERMINATED
            )
        )
        if allows_run_termination_fallback and run_wide:
            run.termination_reasons[run.execution_generation] = "operator_stop"
        result = await port.apply(command, targets, desired)
        if not allows_run_termination_fallback or not run_wide:
            return result
        # QueueControlPort already owns the stronger standby process/runtime
        # fence. Never launder its PARTIAL/UNKNOWN into a clean main-task exit
        # merely because run.task is absent.
        if self._standby_busy(run):
            return result

        task = run.task
        cancel_requested = bool(task is not None and not task.done())
        if cancel_requested:
            run.termination_reasons[run.execution_generation] = "operator_stop"
            task.cancel()
            try:
                await asyncio.wait_for(
                    asyncio.shield(task),
                    timeout=self._main_runtime_cancel_timeout(),
                )
            except (asyncio.CancelledError, asyncio.TimeoutError):
                pass
            except Exception:
                # The wrapper's failure is reflected by task.done/runtime owner
                # below; exception text is never needed for the cancellation proof.
                pass
        task_done = task is None or task.done()
        exit_confirmed = task_done and not run.runtime_incomplete
        metadata = {
            "effect": "run_terminated" if exit_confirmed else "run_cancel_requested",
            "coordinator_effect": result.state.value,
            "cancel_requested": cancel_requested,
            "task_done": task_done,
            "runtime_exit_confirmed": exit_confirmed,
        }
        if exit_confirmed:
            run.paused = False
            if not run.finished:
                detail = "Operator requested run termination"
                await run.bus.emit(Event(
                    event_type=EventType.RUN_FINISHED,
                    run_id=run.run_id,
                    payload={
                        "solved": bool(run.solved),
                        "flag": run.flag,
                        "flags": list(run.flags),
                        "reason": "operator_stop",
                        "failure_code": "operator_stop",
                        "failure_phase": "runtime",
                        "error_id": _runtime_error_id(
                            run.run_id, run.execution_generation, detail),
                        "detail": detail,
                    },
                ))
            return ApplyResult(
                state=EffectState.EFFECT_OBSERVED,
                detail="run task cancellation and runtime exit confirmed",
                target_ids=[], metadata=metadata,
            )
        return ApplyResult(
            state=EffectState.PARTIAL if cancel_requested else EffectState.UNKNOWN,
            detail="run cancellation requested but runtime exit is unconfirmed",
            target_ids=[], metadata=metadata,
        )

    class _RuntimeFencedPort:
        async def apply(self, command, targets, desired):
            return await _cancel_main_runtime(command, targets, desired)

    async def _effect_sink(receipt) -> None:
        command = journal.get_command(receipt.command_id)
        if command is None:
            return
        run.control_generation = journal.current_state().generation
        await run.bus.emit(Event(
            event_type=EventType.CONTROL_COMMAND,
            run_id=run.run_id,
            payload=effect_event_payload(command, receipt),
        ))

    actor = ControlActor(
        run_id=run.run_id,
        journal=journal,
        port=_RuntimeFencedPort(),
        registry=run.worker_registry,
        admission=ControlAdmission(challenge_id=run.run_id),
        effect_sink=_effect_sink,
        secret_resolver=secrets.resolve,
    )
    run.control_actor = actor
    run.control_journal = journal
    run.control_secrets = secrets
    run.control_generation = journal.current_state().generation
    return actor, journal, secrets


def _decision_request_from_event(
    self, run: Run, ev: Event,
) -> Optional[DecisionRequest]:
    payload = ev.payload or {}
    request_id = str(
        payload.get("request_id") or payload.get("id") or "").strip()
    prompt = str(
        payload.get("need") or payload.get("prompt") or "").strip()
    if not request_id or not prompt:
        return None
    worker = str(payload.get("worker") or ev.solver_id or "").strip()
    raw_scope = payload.get("blocking_scope")
    try:
        scope = ControlScope.parse(
            raw_scope if raw_scope is not None
            else f"worker:{worker}" if worker else "global")
    except (TypeError, ValueError):
        scope = ControlScope.parse(
            f"worker:{worker}" if worker else "global")
    worker_ref = next(
        (ref for ref in run.worker_registry.snapshot()
         if ref.worker_id == worker), None)
    intent_id = str(
        payload.get("intent_id")
        or getattr(worker_ref, "intent_id", "") or "")
    lane = str(
        payload.get("lane")
        or getattr(worker_ref, "lane", "") or "")
    delivery_scope = str(payload.get("delivery_scope") or "")
    if not delivery_scope:
        if intent_id:
            delivery_scope = f"intent:{intent_id}"
        elif lane:
            delivery_scope = f"lane:{lane}"
    # Every HITL_REQUEST originates from an explicit NEED_INPUT hand-raise and
    # therefore requires operator action. Historical need_kind values remain in
    # the journal as metadata but no longer change the decision semantics.
    kind = DecisionKind.EXTERNAL_INPUT
    return DecisionRequest(
        request_id=request_id,
        run_id=run.run_id,
        worker_id=worker,
        prompt=prompt,
        kind=kind,
        blocking_scope=scope,
        choices=[str(v) for v in (payload.get("options") or [])][:32],
        default_action=str(payload.get("default_action") or ""),
        execution_id=str(payload.get("execution_id") or ""),
        execution_occurrence=str(
            payload.get("execution_occurrence") or ""),
        resolve_epoch=str(payload.get("resolve_epoch") or ""),
        deadline_at=(float(payload["deadline_at"])
                     if payload.get("deadline_at") is not None else None),
        created_at=float(ev.ts),
        metadata={
            "delivery_scope": delivery_scope,
            "engine": str(
                payload.get("engine")
                or getattr(worker_ref, "engine", "") or ""),
            "intent_id": intent_id,
            "lane": lane,
            "reconciled_from_session": True,
        },
    )


def _reconcile_decision_requests(
    self, run: Run, journal: SQLiteControlJournal,
) -> int:
    appended = 0
    for raw in run.store.load_all(run.run_id):
        if str(raw.get("event_type") or "") != EventType.HITL_REQUEST.value:
            continue
        try:
            request = self._decision_request_from_event(
                run, Event.model_validate(raw))
            if request is None:
                continue
            if journal.get_decision_request(request.request_id) is None:
                journal.append_decision_request(request)
                appended += 1
        except Exception:
            # Preserve other valid cards if one historical row is malformed or
            # reuses an id inconsistently. That row remains fail-closed/unknown.
            LOG.error(
                "failed to reconcile durable decision request for run %s",
                run.run_id,
            )
    return appended


def _record_decision_request(self, run: Run, ev: Event) -> None:
    request = self._decision_request_from_event(run, ev)
    if request is None:
        return
    try:
        _actor, journal, _secrets = self._ensure_control(run)
        journal.append_decision_request(request)
    except Exception:
        LOG.exception(
            "failed to journal decision request %s", request.request_id)


async def post_control(
    self,
    run_id: str,
    body: dict[str, Any],
    *,
    wait_for_effect: bool = False,
) -> dict[str, Any]:
    """Durably accept one command; never confuse HTTP acceptance with effect."""
    run = self.runs.get(run_id)
    if run is None:
        return {"ok": False, "status": "unknown_run"}
    if (self._shutting_down or run_id in self._closing_runs
            or run_id in self._launching_runs):
        return {
            "ok": False,
            "status": "unknown",
            "detail": "run lifecycle transition is in progress",
            "code": "run_lifecycle_unavailable",
        }
    lock = self._control_submit_locks.setdefault(run_id, asyncio.Lock())
    async with lock:
        if (self.runs.get(run_id) is not run or self._shutting_down
                or run_id in self._closing_runs
                or run_id in self._launching_runs):
            return {
                "ok": False,
                "status": "unknown",
                "detail": "run lifecycle transition is in progress",
                "code": "run_lifecycle_unavailable",
            }
        return await self._post_control_serialized(
            run_id, run, body, wait_for_effect=wait_for_effect)


async def _post_control_serialized(
    self,
    run_id: str,
    run: Run,
    body: dict[str, Any],
    *,
    wait_for_effect: bool = False,
) -> dict[str, Any]:
    """Compile and submit under the per-run idempotency/secret boundary."""
    if run.runtime_incomplete:
        return {
            "ok": False,
            "status": "unknown",
            "detail": "runtime shutdown is incomplete",
            "code": "runtime_shutdown_incomplete",
        }
    actor, journal, secrets = self._ensure_control(run)
    requested_id = str(body.get("command_id") or "").strip()
    existing = journal.get_command(requested_id) if requested_id else None

    def _decision_status_with_reconcile(
            request_id: str) -> Optional[DecisionStatus]:
        status = journal.decision_status(request_id)
        if status is None and request_id:
            # The JSONL sink may have committed while the live metadata sink
            # transiently failed. Repair in the same process as well as after
            # restart, then re-read before rejecting the operator's answer.
            self._reconcile_decision_requests(run, journal)
            status = journal.decision_status(request_id)
        return status

    # Validate decision correlation before any plaintext is staged into the
    # SecretStore. This closes the larger boundary around compile validation:
    # an unknown/already-answered request must not leave an unreachable file.
    raw_payload = body.get("payload")
    decision_payload = raw_payload if isinstance(raw_payload, dict) else {}
    raw_request_id = str(
        decision_payload.get("request_id")
        or body.get("request_id")
        or ""
    ).strip()
    raw_action = str(body.get("action") or "hint").strip().lower()
    is_decision_answer = (
        raw_action == ControlAction.ANSWER_DECISION.value
        or (raw_action in {"answer", "submit"} and bool(raw_request_id))
    )
    if is_decision_answer:
        decision_status = _decision_status_with_reconcile(raw_request_id)
        if decision_status is None:
            raise ControlPayloadError(
                f"unknown decision request: {raw_request_id}")
        if decision_status is not DecisionStatus.OPEN and existing is None:
            raise StateConflict(
                f"decision {raw_request_id!r} is already {decision_status.value}")

    command = compile_control_command(
        run_id, body, secrets=secrets, existing_command=existing)
    if existing is None:
        existing = journal.get_command(command.command_id)

    request_id = str(command.payload.get("request_id") or "").strip()
    if command.action is ControlAction.ANSWER_DECISION:
        decision_status = _decision_status_with_reconcile(request_id)
        if decision_status is None:
            raise ControlPayloadError(f"unknown decision request: {request_id}")
        if decision_status is not DecisionStatus.OPEN and existing is None:
            raise StateConflict(
                f"decision {request_id!r} is already {decision_status.value}")

    live = run.task is not None and not run.task.done()
    schedule_standby = not live and command.action.value in self._STANDBY_ACTIONS
    schedule_offline = (
        not live
        and command.action.value in self._OFFLINE_CONTROL_ACTIONS)
    if schedule_standby:
        self._register_standby_winner(run)
    if ((schedule_standby or schedule_offline)
            and bool(getattr(run.bus, "_closed", False))):
        # Finished buses are closed; make receipt events visible before the actor
        # emits them and before the standby worker starts.
        self._fresh_bus(run)

    receipt = await (
        actor.submit_and_wait(command)
        if wait_for_effect
        else actor.submit(command)
    )
    if existing is None:
        await run.bus.emit(Event(
            event_type=EventType.HITL_RESPONSE,
            run_id=run_id,
            payload=safe_hitl_echo(command, status=receipt.state.value),
        ))
    ok = receipt.state not in {EffectState.REJECTED, EffectState.FAILED}
    result = {
        "ok": ok,
        "command_id": command.command_id,
        "status": receipt.state.value,
        "generation": receipt.observed_generation,
        "detail": safe_receipt_detail(command, receipt.detail),
        "code": receipt.metadata.get("code", ""),
    }
    if schedule_standby and existing is None:
        # Give the actor one scheduling turn so legacy callers can immediately
        # observe standby_task without claiming the terminal effect in HTTP.
        await asyncio.sleep(0)
    return result


def control_receipt(self, run_id: str, command_id: str) -> Optional[dict[str, Any]]:
    """Return a safe durable receipt projection for crash/event reconciliation."""
    run = self.runs.get(run_id)
    if run is None:
        return None
    _actor, journal, _secrets = self._ensure_control(run)
    command = journal.get_command(str(command_id or ""))
    if command is None:
        return None
    receipt = journal.latest_effect(command.command_id)
    if receipt is None:
        return None
    return {
        "command_id": command.command_id,
        "receipt_id": receipt.receipt_id,
        "action": command.action.value,
        "target": command.scope.as_legacy_target(),
        "status": receipt.state.value,
        "generation": receipt.observed_generation,
        "target_ids": list(receipt.target_ids),
        "detail": safe_receipt_detail(command, receipt.detail),
        "code": str(receipt.metadata.get("code") or ""),
        "terminal": receipt.state.terminal,
    }


async def post_hitl(self, run_id: str, target: str, action: str, **fields: Any) -> bool:
    """Legacy bool facade compiled onto the same durable ControlCommand path."""
    run = self.runs.get(run_id)
    if run is None:
        return False
    fields = dict(fields)
    # Old clients have no decision picker correlation. Preserve compatibility
    # only when there is exactly one unambiguous pending request; with zero or
    # multiple requests we refuse to guess and leave request_id absent.
    if not fields.get("request_id") and len(run.pending_help) == 1:
        fields["request_id"] = next(iter(run.pending_help))
    # M2: drop an identical back-to-back resend (same target/action/text/url).
    # The UI has no client throttle, and an operator hammering the SAME hint at a
    # busy single-shot worker (run-0011: 11×) otherwise queues 11 items + 11
    # events + 11 downstream _drain_hitl sweeps. A genuinely new command (changed
    # text, or a different action) still goes through.
    sig = (target, action, str(fields.get("text") or fields.get("hint") or ""),
           str(fields.get("url") or fields.get("target_url") or ""),
           str(fields.get("flag") or ""),
           str(fields.get("request_id") or ""),
           bool(fields.get("standing", False)),
           str(fields.get("preempt_policy") or fields.get("preemption") or ""))
    # `writeup` is an idempotent-looking no-arg command from the UI, but each
    # click is a real request to run a fresh post-solve standby turn. If we
    # dedupe it here, the second "生成复盘" click only echoes a duplicate
    # HITL_RESPONSE and never starts a worker, which reads as a stuck button.
    if action != "writeup" and getattr(run, "_last_hitl_sig", None) == sig:
        await run.bus.emit(Event(
            event_type=EventType.HITL_RESPONSE, run_id=run_id,
            payload=hitl_response_payload(
                target, action, status="duplicate", text="[duplicate omitted]")))
        return True
    result = await self.post_control(
        run_id, {"target": target, "action": action, **fields})
    terminal_ok = False
    terminal_observed = False
    if result.get("command_id") and run.control_actor is not None:
        await run.control_actor.join()
        terminal = run.control_journal.latest_effect(result["command_id"])
        terminal_ok = bool(
            terminal is not None
            and terminal.state in {
                EffectState.EFFECT_OBSERVED,
                EffectState.PARTIAL,
            }
        )
        terminal_observed = bool(
            terminal is not None
            and terminal.state is EffectState.EFFECT_OBSERVED)
    if terminal_observed:
        run._last_hitl_sig = sig
    return terminal_ok


async def post_worker_cmd(self, run_id: str, action: str, *,
                          engine: Optional[str] = None,
                          solver_id: Optional[str] = None) -> bool:
    """Compile worker spawn/kill through the durable typed control plane.

    Only a terminal runtime ACK returns true. A finished/ghost run has no
    coordinator capable of proving the effect and is rejected.
    """
    run = self.runs.get(run_id)
    if run is None:
        return False
    live = run.task is not None and not run.task.done()
    if not live:
        return False
    if action == "spawn":
        result = await self.post_control(run_id, {
            "action": "spawn_worker",
            "target": "global",
            "payload": {"engine": str(engine or "")},
        })
    elif action == "kill" and solver_id:
        result = await self.post_control(run_id, {
            "action": "cancel_worker",
            "target": f"worker:{solver_id}",
            "payload": {"worker_id": solver_id},
        })
    else:
        return False
    if run.control_actor is None:
        return False
    await run.control_actor.join()
    receipt = run.control_journal.latest_effect(
        str(result.get("command_id") or ""))
    return bool(receipt is not None
                and receipt.state is EffectState.EFFECT_OBSERVED)
