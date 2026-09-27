"""Worker lifecycle and decision-dismiss control actions."""

from __future__ import annotations

import asyncio
import os

from muteki.swarm.coordinator_control_command import ControlCommandContext


async def _handle_worker_control(self, context: ControlCommandContext) -> bool:
    cmd = context.cmd
    payload = context.payload
    action = context.action
    original_action = context.original_action
    target = context.target
    command_id = context.command_id
    if action in ("cancel_worker", "force_cancel"):
        cancel_target = target
        worker_id = str(cmd.get("worker_id") or payload.get("worker_id") or "")
        if worker_id and target == "global":
            cancel_target = f"worker:{worker_id}"
        selected = self._control_target_solvers(cancel_target)
        requested: list[str] = []
        failures: list[str] = []
        for worker in selected:
            sid = str(getattr(worker, "solver_id", "") or "")
            if self._cancel_solver(worker):
                requested.append(sid)
                self._update_control_worker_status(
                    sid, "cancel_requested")
            else:
                failures.append(sid)
        state = (
            "effect_observed" if requested and not failures else
            "partial" if requested else "unknown"
        )
        self._ack_control(
            cmd, state=state,
            detail=(
                f"cancellation requested for {len(requested)} live worker(s)"
                if requested else
                "matching worker cancellation could not be delivered"
                if selected else "no matching live worker to cancel"
            ),
            target_ids=requested,
            metadata={
                "effect": "worker_cancel_requested",
                "cancel_failures": failures,
                "process_exit_confirmed": False,
            })
        return True
    if original_action == "expire_context":
        self._ack_control(
            cmd, state="effect_observed",
            detail="typed operator context expired in the durable journal",
            metadata={"effect": "context_expired"})
        return True
    if action == "graceful_drain":
        self._operator_draining = True
        if self._operator_event is not None:
            self._operator_event.set()
        self._ack_control(
            cmd, state="effect_observed",
            detail="new dispatch quiesced; in-flight workers are draining",
            metadata={"effect": "graceful_drain"})
        return True
    if action == "spawn_worker":
        if self.worker_cmds is None:
            self._ack_control(
                cmd, state="unknown",
                detail="worker dispatcher is unavailable",
                metadata={"code": "worker_dispatcher_unavailable"})
            return True
        loop = asyncio.get_running_loop()
        worker_ack = loop.create_future()
        worker_envelope = {
            "action": "spawn",
            "engine": str(
                cmd.get("engine") or payload.get("engine")
                or cmd.get("profile") or payload.get("profile") or ""),
            "_control_ack": worker_ack,
            "command_id": command_id,
            "_control_started": False,
            "_control_cancel_requested": False,
        }
        await self.worker_cmds.put(worker_envelope)
        if self._operator_event is not None:
            self._operator_event.set()
        try:
            try:
                spawn_timeout = float(os.environ.get(
                    "MUTEKI_WORKER_CONTROL_ACK_TIMEOUT", "10"))
            except (TypeError, ValueError):
                spawn_timeout = 10.0
            worker_result = await asyncio.wait_for(
                asyncio.shield(worker_ack),
                timeout=max(0.1, spawn_timeout),
            )
        except asyncio.TimeoutError:
            worker_envelope["_control_cancel_requested"] = True
            removed = False
            try:
                pending = getattr(self.worker_cmds, "_queue")
                pending.remove(worker_envelope)
                self.worker_cmds.task_done()
                removed = True
            except (AttributeError, ValueError):
                pass
            if removed:
                worker_result = {
                    "state": "unknown",
                    "detail": (
                        "worker spawn retired before dispatcher claim"),
                    "target_ids": [],
                    "metadata": {
                        "code": "worker_spawn_timeout_unclaimed",
                        "late_effect_fenced": True,
                    },
                }
                if not worker_ack.done():
                    worker_ack.set_result(worker_result)
            else:
                # Claimed commands are no longer timeout-cancellable.
                # Await the dispatcher's real terminal proof so a spawn
                # can never occur after the durable command says UNKNOWN.
                worker_result = await asyncio.shield(worker_ack)
        self._ack_control(
            cmd,
            state=str(worker_result.get("state") or "unknown"),
            detail=str(worker_result.get("detail") or ""),
            target_ids=list(worker_result.get("target_ids") or []),
            metadata=dict(worker_result.get("metadata") or {}),
        )
        return True
    if original_action == "writeup":
        self._ack_control(
            cmd, state="unknown",
            detail="writeup requires a finished-run standby worker",
            metadata={"code": "writeup_requires_standby"})
        return True
    return False


async def _handle_dismiss_control(self, context: ControlCommandContext) -> bool:
    cmd = context.cmd
    action = context.action
    target = context.target
    request_id = context.request_id
    # DISMISS a worker's hand-raise (NEED_INPUT) WITHOUT supplying the
    # resource: the operator judges the ask a false alarm / not worth
    # answering. The swarm must NOT stay frozen waiting on a blocker the
    # operator won't clear. Clear the pending ask (scoped to target),
    # record a dead-end so a re-spawned worker doesn't immediately re-raise
    # the same thing, unfreeze the workers, and wake the coordinator. No
    # resource is injected (distinct from a hint/redirect that answers it).
    if action in ("dismiss", "dismiss_help"):
        if request_id:
            dismissed = [h for h in self._pending_help
                         if str(h.get("request_id") or h.get("id") or "")
                         == request_id]
        elif target == "global":
            dismissed = list(self._pending_help)
        else:
            scoped = target.split(":", 1)[-1] if ":" in target else target
            dismissed = [h for h in self._pending_help
                         if str(h.get("worker", "")) == scoped]
        if not dismissed:
            self._ack_control(
                cmd, state="unknown",
                detail="no matching decision request to dismiss",
                metadata={"effect": "no_effect",
                          "code": "decision_not_found"})
            return True
        dismissed_objects = {id(h) for h in dismissed}
        self._pending_help = [
            h for h in self._pending_help
            if id(h) not in dismissed_objects
        ]
        for h in dismissed:
            need = str(h.get("need", "")).strip()
            if need:
                try:
                    await self.insight.dead_end(
                        "coordinator",
                        f"operator dismissed the ask «{need[:160]}» — "
                        f"not supplying it; do not re-raise")
                except Exception:
                    pass
        if not self._pending_help:
            if not self._control_frozen:
                self._operator_paused = False
            if self._operator_event is not None:
                self._operator_event.set()
        # SIGCONT the workers we froze on the hand-raise so the swarm
        # resumes instead of sitting paused on a dismissed blocker.
        if ("__help__" not in self._freeze_suspensions
                and not self._control_frozen):
            try:
                await self.insight.guidance(
                    "", action="resume", target=target, standing=False)
            except Exception:
                pass
        try:
            await self._emit_coord_bb(
                "help_dismissed",
                reason=f"operator dismissed {len(dismissed)} hand-raise(s)"
                       f"{'' if target == 'global' else ' for ' + target}",
                count=len(dismissed))
        except Exception:
            pass
        self._ack_control(
            cmd, state="effect_observed",
            detail=f"dismissed {len(dismissed)} matching decision request(s)",
            target_ids=[str(h.get("worker") or "") for h in dismissed
                        if h.get("worker")],
            metadata={"effect": "decision_dismissed"})
        return True
    return False
