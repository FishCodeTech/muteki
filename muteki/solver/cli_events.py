"""CliSolver event, lifecycle, and intent publication helpers."""
from __future__ import annotations

import time
from typing import Any, Optional

from muteki.control.models import stable_decision_request_id
from muteki.core.events import (
    Event,
    EventType,
    blackboard_delta_payload,
    worker_lifecycle_payload,
    worker_status_payload,
)

async def _emit(self, etype: EventType, **payload: Any) -> Optional[Event]:
    if self.bus is not None:
        return await self.bus.emit(Event(
            event_type=etype, run_id=self.run_id,
            challenge_id=self.challenge.id, solver_id=self.solver_id,
            payload=payload,
        ))
    return None


def _decision_request_identity(self, need: str, need_kind: str) -> tuple[str, dict]:
    """Return one stable id + correlation payload for this execution occurrence."""
    key = (str(need), str(need_kind))
    request_id = self._decision_request_ids.get(key)
    execution_id = str(
        self._cli_session or self.resume_session
        or f"attempt:{self._execution_occurrence}"
    )
    if request_id is None:
        request_id = stable_decision_request_id(
            run_id=self.run_id,
            worker_id=self.solver_id,
            prompt=str(need),
            kind=str(need_kind),
            correlation_key=str(
                getattr(self, "_intent_id", "") or self.intent_id_assigned or self.mode
            ),
            execution_id=execution_id,
            execution_occurrence=self._execution_occurrence,
            resolve_epoch=self._resolve_epoch,
        )
        self._decision_request_ids[key] = request_id
    return request_id, {
        "execution_id": execution_id,
        "execution_occurrence": self._execution_occurrence,
        "resolve_epoch": self._resolve_epoch,
    }


async def _emit_bb(self, kind: str, **fields: Any) -> None:
    """One blackboard.delta — the swarm's collaboration layer (intent claim
    lifecycle / facts / dead-ends / flag) that the OneNote board renders."""
    if kind == "intent_concluded":
        receipt = getattr(self, "_last_worker_result_commit", None)
        if (isinstance(receipt, dict) and receipt
                and receipt.get("concluded") is False):
            # A late Worker may still hand off useful evidence after its lease was
            # reclaimed, but the owner fence rejected its state transition. Do not
            # publish a contradictory terminal delta to the UI.
            await self._emit(
                EventType.BLACKBOARD_DELTA,
                **blackboard_delta_payload(
                    "intent_conclusion_rejected", actor=self.solver_id,
                    intent_id=str(fields.get("intent_id") or ""),
                    worker=self.solver_id,
                    reason="worker no longer owns the intent",
                ),
            )
            return
    await self._emit(
        EventType.BLACKBOARD_DELTA,
        **blackboard_delta_payload(kind, actor=self.solver_id, **fields))


def _record_intent_db(self, goal: str) -> bool:
    """Atomically start this whole-challenge Intent under this Worker.

    Bootstrap Workers create their own Intent, so proposal and ownership must
    become visible together.  Returning ``False`` prevents the CLI process from
    starting when another owner or terminal state already holds the same Intent.
    """
    if self.shared_graph is None:
        return True
    return bool(self.shared_graph.start_intent(
        actor=self.solver_id,
        worker=self.solver_id,
        intent_id=self._intent_id,
        goal=goal,
        worker_class=self.driver.name,
    ))


def _conclude_intent_db(self, *, result: str,
                        to_fact_seq: "Optional[int]" = None,
                        result_detail: str = "") -> None:
    """P1-B: conclude this worker's intent in the DB intents table (status='done'
    + result text) so the NEXT bootstrap worker's board shows this direction was
    already attempted and what came of it. Best-effort + owner-fenced inside
    conclude_intent."""
    if self.shared_graph is None:
        return
    if self._control_context_delivery_unknown and not result_detail:
        # Popen already received argv, so an unconfirmed journal commit is a
        # terminal disclosure-unknown attempt, not a safe-to-retry pre-start
        # failure. A new operator command can create fresh context without
        # replaying this one-shot material.
        result_detail = (
            "required operator context delivery became unknown after process start")
    try:
        self.shared_graph.conclude_intent(
            actor=self.solver_id, intent_id=self._intent_id,
            result=result, to_fact_seq=to_fact_seq,
            result_detail=result_detail)
    except Exception:
        pass


async def _emit_finished(self, *, flag: Optional[str], solved: bool,
                         flags: Optional[list[str]] = None) -> None:
    """Emit this solver's terminal lifecycle event, scoped correctly.

    scope="run"    → RUN_FINISHED (this solver IS the run: mock / race / standby).
    scope="worker" → WORKER_FINISHED (a swarm sub-worker; the coordinator owns the
                     single run-level RUN_FINISHED emitted when its loop exits).
    Payload carries `flag` (first, back-compat) + `flags` (all, multi-flag) +
    `solved`. Worker-scoped events also carry `reason` (timeout / oom /
    cancelled / steered / ...). Run-scoped RUN_FINISHED keeps its own reason
    vocabulary and is not overwritten here."""
    partial_flag_progress = (
        flags is not None and len(flags) > 0
        and not self._flags_complete_for_worker()
    )
    etype = (
        EventType.RUN_FINISHED
        if self.lifecycle_scope == "run" and not partial_flag_progress
        else EventType.WORKER_FINISHED
    )
    stop_reason = (
        self._worker_stop_reason
        or ("solved" if solved else "finished")
    ).strip()
    if stop_reason:
        self._note_worker_stop(stop_reason)
    # I: granular lifecycle — the worker has exited (with its final token total).
    await self._emit_lifecycle("exited", solved=solved)
    finished_payload: dict[str, Any] = {
        "flag": flag,
        "flags": list(flags if flags is not None else (
            [flag] if flag is not None else [])),
        "solved": solved,
    }
    if etype is EventType.WORKER_FINISHED:
        finished_payload["reason"] = stop_reason
    await self._emit(etype, **finished_payload)


async def _emit_worker_status(
    self, *, online: bool, reason: str, status: Optional[str] = None,
) -> None:
    await self._emit(
        EventType.WORKER_STATUS,
        **worker_status_payload(
            online,
            status=status or ("online" if online else "offline"),
            reason=reason,
            engine=self.driver.name,
            session=self._cli_session or "",
            runtime=self._last_runtime_status if not online else None,
            worker_role=self.mode,
            identity=self.identity or None,
        ))


def _tokens_spent(self) -> int:
    """Best-effort running token total for THIS worker (in+out), for the
    lifecycle telemetry. Reads the cost ledger's per-solver snapshot if present."""
    try:
        if self.cost is not None and hasattr(self.cost, "snapshot"):
            snap = self.cost.snapshot() or {}
            by = snap.get("by_solver") or snap.get("bySolver") or {}
            row = by.get(self.solver_id) or {}
            return int(row.get("tokens_in", 0) or 0) + int(row.get("tokens_out", 0) or 0)
    except Exception:
        pass
    return 0


async def _emit_lifecycle(self, phase: str, *, paused: bool = False,
                          **extra: Any) -> None:
    """I: emit a granular worker-lifecycle event (spawned/phase_changed/
    stalled/exited). Best-effort; never disturbs the solve."""
    if phase == "stalled" and getattr(self, "_stalled_at", None) is None:
        self._stalled_at = time.monotonic()
    try:
        await self._emit(
            EventType.WORKER_LIFECYCLE,
            **worker_lifecycle_payload(
                phase,
                intent_id=getattr(self, "intent_id_assigned", "") or getattr(self, "_intent_id", "") or "",
                tokens_spent=self._tokens_spent(),
                paused=paused,
                engine=self.driver.name, worker_role=self.mode,
                **{**self.identity, **extra}))
    except Exception:
        pass
