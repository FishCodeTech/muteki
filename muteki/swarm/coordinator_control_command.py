"""Control-envelope admission and normalized command context."""

from __future__ import annotations

import asyncio
from dataclasses import dataclass
from typing import Any, Optional


@dataclass(slots=True)
class ControlCommandContext:
    cmd: dict[str, Any]
    payload: dict[str, Any]
    text: Any
    delivery_text: str
    action: str
    original_action: str
    target: str
    request_id: str
    command_id: str
    scope_kind: str
    scope_value: str
    scope_is_global: bool
    continuation_intent_id: str


@staticmethod
def _control_scope_parts(target: str) -> tuple[str, str]:
    raw = str(target or "global").strip() or "global"
    if ":" not in raw:
        return ("global", "") if raw == "global" else ("worker", raw)
    kind, value = raw.split(":", 1)
    return kind.strip().lower(), value.strip()


def _control_target_solvers(self, target: str) -> list[Any]:
    """Resolve a legacy target against the process-local live registry."""
    kind, value = self._control_scope_parts(target)
    workers = list(getattr(self, "_live_solvers", {}).values())
    if kind in {"global", "run"}:
        return workers
    if kind == "challenge":
        return workers if value == self.challenge.id else []
    if kind in {"worker", "solver"}:
        return [w for w in workers
                if str(getattr(w, "solver_id", "")) == value]
    if kind == "engine":
        return [w for w in workers
                if str(getattr(w, "engine", "")) == value]
    if kind == "intent":
        return [w for w in workers
                if str(
                    getattr(w, "intent_id_assigned", "")
                    or getattr(w, "_intent_id", "")
                    or getattr(w, "intent_id", "")
                ) == value]
    if kind == "lane":
        return [w for w in workers
                if str(getattr(w, "lane", "")) == value]
    return []


async def prepare_control_command(
    self,
    cmd: Any,
    *,
    consumer_epoch: int,
) -> Optional[ControlCommandContext]:
    if not isinstance(cmd, dict):
        return None
    deadline = cmd.get("_control_deadline")
    expired = False
    try:
        expired = deadline is not None and asyncio.get_running_loop().time() >= float(deadline)
    except (TypeError, ValueError):
        expired = True
    if cmd.get("_control_cancel_requested") or expired:
        self._ack_control(
            cmd, state="unknown",
            detail="control envelope expired before runtime application",
            metadata={"code": "cancelled_before_apply"})
        return None
    # Synchronous claim fence paired with QueueControlPort: once this
    # flips, the producer may no longer conclude UNKNOWN merely from a
    # timeout. It waits for the real terminal ACK, preventing a command
    # from executing after its journal was already closed unknown.
    cmd["_control_started"] = True
    cmd["_control_consumer_task"] = asyncio.current_task()
    restart = getattr(self, "_control_restart_event", None)
    if isinstance(restart, asyncio.Event):
        cmd["_control_restart_event"] = restart
    cmd["_control_consumer_epoch"] = consumer_epoch
    payload = cmd.get("payload") if isinstance(cmd.get("payload"), dict) else {}
    # ``text`` remains the durable/safe representation (possibly a
    # secret:// reference). ``delivery_text`` exists transiently and is
    # used only for in-memory worker injection.
    text = (cmd.get("text") or cmd.get("hint")
            or payload.get("text") or payload.get("hint") or "")
    # Keep secret:// opaque throughout the coordinator/bus. Plaintext is
    # materialised only after a context reservation, while constructing
    # the one worker prompt that is allowed to receive it.
    delivery_text = str(text or "")
    action = str(cmd.get("action") or "hint").strip().lower()
    original_action = action
    # A decision answer is ordinary operator guidance at the existing
    # single-shot runtime boundary, but remains typed as
    # answer_decision in the durable command journal.
    if action in ("answer_decision", "submit"):
        action = "hint"
    elif action == "add_context":
        raw_context = cmd.get("context")
        if isinstance(raw_context, dict):
            text = str(raw_context.get("content") or text)
            cmd.setdefault("standing", bool(raw_context.get("standing", False)))
        elif isinstance(raw_context, str):
            text = raw_context
        delivery_text = str(text or "")
        action = "hint"
    elif action == "resume" and self._control_frozen:
        # Back-compatible resume after an emergency freeze performs a
        # real thaw; otherwise desired state would say ACTIVE while
        # subprocess groups and guarded leases remained frozen.
        action = "thaw"
    target = str(cmd.get("target") or "global").strip() or "global"
    request_id = str(cmd.get("request_id") or payload.get("request_id") or "")
    runtime_pending: Optional[dict[str, Any]] = None
    if original_action in (
        "answer_decision", "submit", "dismiss", "dismiss_help"
    ) and request_id:
        for pending in self._pending_help:
            if str(pending.get("request_id") or pending.get("id") or "") != request_id:
                continue
            if pending.get("runtime_interaction"):
                runtime_pending = pending
            pending_worker = str(pending.get("worker") or "").strip()
            if pending_worker:
                target = f"solver:{pending_worker}"
            break
    command_id = str(cmd.get("command_id") or "")
    # This Swarm instance owns exactly one challenge.  A challenge-scoped
    # command is therefore run-wide; solver/engine/intent scopes are not.
    scope_kind, scope_value = self._control_scope_parts(target)
    # A control envelope is already tied to this Swarm instance.  Do
    # not let a syntactically valid selector for another run/challenge
    # degrade into a run-wide command merely because its *kind* is
    # broad.  This check belongs at the final application boundary as
    # well as admission: legacy callers can bypass the typed HTTP API.
    if ((scope_kind == "run" and scope_value != self.run_id)
            or (scope_kind == "challenge"
                and scope_value != self.challenge.id)):
        self._ack_control(
            cmd, state="failed",
            detail="control scope does not belong to this runtime",
            metadata={"code": "scope_mismatch"})
        return None
    if (
        runtime_pending is not None
        and original_action in {"answer_decision", "submit"}
    ):
        matched = self._control_target_solvers(target)
        delivered_to: list[str] = []
        response_text = delivery_text
        if response_text.startswith("secret://"):
            response_text = self._materialize_reserved_control_text(
                response_text)
        decision = str(
            cmd.get("decision")
            or payload.get("decision")
            or response_text
        )
        for worker in matched:
            responder = getattr(
                worker, "respond_runtime_interaction", None)
            if not callable(responder):
                continue
            try:
                observed = responder(
                    request_id=request_id,
                    text=response_text,
                    decision=decision,
                )
                if hasattr(observed, "__await__"):
                    observed = await observed
            except Exception:
                continue
            if observed:
                delivered_to.append(str(
                    getattr(worker, "solver_id", "") or ""))
        if delivered_to:
            self._pending_help = [
                item for item in self._pending_help
                if str(item.get("request_id") or item.get("id") or "")
                != request_id
            ]
            if self._operator_event is not None:
                self._operator_event.set()
            self._ack_control(
                cmd,
                state="effect_observed",
                detail="response delivered to the active structured Runtime",
                target_ids=delivered_to,
                metadata={
                    "effect": "runtime_interaction_answered",
                    "request_id": request_id,
                    "runtime_interaction": runtime_pending.get(
                        "runtime_interaction"),
                },
            )
        else:
            self._ack_control(
                cmd,
                state="unknown",
                detail="the structured Runtime interaction is no longer active",
                metadata={
                    "code": "runtime_interaction_unavailable",
                    "request_id": request_id,
                },
            )
        return None
    context_actions = {
        "ask", "hint", "focus", "redirect", "directive",
        "correction", "add_context", "answer_decision", "submit",
    }
    live_context_target = bool(
        scope_kind in {"worker", "solver"}
        and original_action in context_actions
        and original_action not in {"answer_decision", "submit"}
        and any(
            not self._worker_runtime_exit_confirmed(worker)
            for worker in self._control_target_solvers(target)
        )
    )
    continuation_intent_id = ""
    if not live_context_target:
        semantic_exact_context = bool(
            command_id and delivery_text
            and original_action in context_actions
            and (
                original_action in {"answer_decision", "submit"}
                or scope_kind in {"worker", "solver"}
            )
        )
        required_continuation_id = self._control_continuation_id(
            command_id,
            required_when_unavailable=semantic_exact_context,
        )
        if required_continuation_id and not self.coordinator:
            # A fixed non-coordinator race has no dispatcher after its
            # initial worker batch. Persisting an exact intent there would
            # strand the context while falsely reporting success. Keep the
            # resource durable for a later coordinator resolve, but make the
            # current effect explicitly unsupported/unknown.
            self._ack_control(
                cmd, state="unknown",
                detail=(
                    "exact worker continuation requires coordinator mode"),
                metadata={
                    "code": "exact_continuation_requires_coordinator",
                    "continuation_intent_id": required_continuation_id,
                },
            )
            return None
        continuation_intent_id = self._propose_control_continuation(
            command_id=command_id,
            action=original_action,
            target=target,
        )
        if required_continuation_id and not continuation_intent_id:
            self._ack_control(
                cmd, state="unknown",
                detail="exact continuation could not be materialized",
                metadata={
                    "code": "exact_continuation_unavailable",
                    "continuation_intent_id": required_continuation_id,
                },
            )
            return None
        if continuation_intent_id:
            # The durable ContextResource is authoritative.  A decision
            # answer may arrive after restart with a globally-scoped wire
            # command and no volatile _pending_help row; force every legacy
            # path onto the exact continuation so it can never fall into
            # _next_worker_guidance or an engine/global bus broadcast.
            target = f"intent:{continuation_intent_id}"
            scope_kind, scope_value = "intent", continuation_intent_id
            try:
                await self._emit_coord_bb(
                    "intent_proposed",
                    intent_id=continuation_intent_id,
                    goal="operator-scoped continuation",
                    source_command_id=command_id,
                )
            except Exception:
                pass
    scope_is_global = (
        target in {"global", self.challenge.id,
                   f"challenge:{self.challenge.id}"}
        or (scope_kind == "run" and scope_value == self.run_id)
        or (scope_kind == "challenge"
            and scope_value == self.challenge.id)
    )
    if (action in {
            "pause", "resume", "stop", "complete",
            "graceful_drain", "clear_standing", "reset_guidance",
            "mark_false",
    } and not scope_is_global):
        self._ack_control(
            cmd, state="failed",
            detail=f"{action} requires a run-wide scope",
            metadata={"code": "invalid_scope"})
        return None
    return ControlCommandContext(
        cmd=cmd,
        payload=payload,
        text=text,
        delivery_text=delivery_text,
        action=action,
        original_action=original_action,
        target=target,
        request_id=request_id,
        command_id=command_id,
        scope_kind=scope_kind,
        scope_value=scope_value,
        scope_is_global=scope_is_global,
        continuation_intent_id=continuation_intent_id,
    )
