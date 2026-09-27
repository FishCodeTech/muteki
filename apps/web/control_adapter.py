"""Web boundary for the durable operator control plane.

The FastAPI/RunManager layer owns admission and audit, while the existing swarm
continues to consume plain dictionaries from ``run.hitl``.  ``QueueControlPort``
is the deliberately small bridge between those worlds.  A queue write is only
routing; the port reports an observed effect only after the coordinator resolves
the per-command acknowledgement future.
"""

from __future__ import annotations

import asyncio
import inspect
from collections.abc import Callable, Mapping, Sequence
from pathlib import Path
from typing import Any, Optional

from muteki.control import (
    ApplyResult,
    ControlAction,
    ControlCommand,
    ControlScope,
    EffectReceipt,
    EffectState,
    RunControlState,
    WorkerRef,
)
from muteki.control.secrets import SecretStore
from muteki.core.events import control_command_payload


_RESERVED_BODY_KEYS = {
    "action", "target", "scope", "payload", "command_id",
    "expected_generation", "deadline_at",
}
class ControlPayloadError(ValueError):
    """A clean client error while compiling a wire request."""


def compile_control_command(
    run_id: str,
    body: Mapping[str, Any],
    *,
    secrets: SecretStore,
    existing_command: Optional[ControlCommand] = None,
) -> ControlCommand:
    """Compile the typed endpoint and legacy flat HITL shape into one command."""
    raw_payload = body.get("payload") or {}
    if not isinstance(raw_payload, Mapping):
        raise ControlPayloadError("payload must be a JSON object")
    payload = dict(raw_payload)
    # Legacy /hitl callers put text/url/request_id/standing/etc. at the top level.
    for key, value in body.items():
        if key not in _RESERVED_BODY_KEYS:
            payload.setdefault(str(key), value)

    raw_action = str(body.get("action") or "hint").strip().lower()
    request_id = str(payload.get("request_id") or "").strip()
    if raw_action in {"answer", "submit"} and request_id:
        raw_action = ControlAction.ANSWER_DECISION.value
    elif raw_action == "reject":
        raw_action = ControlAction.DISMISS.value
    try:
        action = ControlAction(raw_action)
    except ValueError as exc:
        raise ControlPayloadError(f"unsupported control action: {raw_action}") from exc

    try:
        scope = ControlScope.parse(body.get("scope", body.get("target", "global")))
    except (TypeError, ValueError) as exc:
        raise ControlPayloadError(str(exc)) from exc

    values: dict[str, Any] = {
        "run_id": run_id,
        "action": action,
        "scope": scope,
        "payload": payload,
    }
    if body.get("command_id") is not None:
        values["command_id"] = body.get("command_id")
    if body.get("expected_generation") is not None:
        values["expected_generation"] = body.get("expected_generation")
    if body.get("deadline_at") is not None:
        values["deadline_at"] = body.get("deadline_at")
    return ControlCommand.model_validate(values)


def safe_hitl_echo(command: ControlCommand, *, status: str) -> dict[str, Any]:
    """Return the complete operator command to the conversation event stream."""
    payload = command.payload
    result: dict[str, Any] = {
        "target": command.scope.as_legacy_target(),
        "action": command.action.value,
        "command_id": command.command_id,
        "status": status,
    }
    request_id = payload.get("request_id")
    if request_id:
        result["request_id"] = str(request_id)
    text = payload.get("text") or payload.get("hint") or payload.get("answer")
    if text:
        result["text"] = str(text)
    url = payload.get("url") or payload.get("target_url")
    if url:
        result["url"] = str(url)
    return result


def _effect_kind(command: ControlCommand, receipt: EffectReceipt) -> str:
    runtime_effect = str(receipt.metadata.get("effect") or "").lower()
    aliases = {
        "graceful_drain": "run_quiesced",
        "termination_requested": "run_terminated",
        "standby_cancelled": "run_terminated",
    }
    runtime_effect = aliases.get(runtime_effect, runtime_effect)
    authoritative = {
        "run_quiesced", "run_resumed", "run_frozen", "run_thawed",
        "workers_frozen", "workers_thawed", "run_terminated",
    }
    if runtime_effect in authoritative:
        return runtime_effect
    run_wide = command.scope.kind.value in {"global", "run", "challenge"}
    return {
        ControlAction.PAUSE: "run_quiesced",
        ControlAction.FREEZE: "run_frozen" if run_wide else "workers_frozen",
        ControlAction.RESUME: "run_resumed",
        ControlAction.THAW: "run_thawed" if run_wide else "workers_thawed",
        ControlAction.GRACEFUL_DRAIN: "run_quiesced",
        ControlAction.STOP: "run_terminated",
        ControlAction.COMPLETE: "run_terminated",
    }.get(command.action, "command_applied")


def effect_event_payload(command: ControlCommand,
                         receipt: EffectReceipt) -> dict[str, Any]:
    effect: Optional[dict[str, Any]] = None
    if receipt.state is EffectState.EFFECT_OBSERVED:
        effect = {
            "kind": _effect_kind(command, receipt),
            "targets": list(receipt.target_ids),
        }
    request_id = (receipt.metadata.get("request_id")
                  or command.payload.get("request_id"))
    detail = str(receipt.detail or "")
    return control_command_payload(
        command.command_id,
        command.action.value,
        target=command.scope.as_legacy_target(),
        status=receipt.state.value,
        request_id=str(request_id) if request_id else None,
        effect=effect,
        detail=detail,
        generation=receipt.observed_generation,
        target_ids=list(receipt.target_ids),
        code=str(receipt.metadata.get("code") or ""),
        receipt_id=receipt.receipt_id,
        decision_closed=bool(receipt.metadata.get("decision_closed", False)),
        decision_status=str(receipt.metadata.get("decision_status") or ""),
    )


def safe_receipt_detail(command: ControlCommand, detail: Any) -> str:
    return str(detail or "")


def materialize_runtime_secrets(value: Any, *, secrets: SecretStore) -> Any:
    """Resolve opaque references for an ephemeral runtime envelope only."""
    if isinstance(value, Mapping):
        return {str(k): materialize_runtime_secrets(v, secrets=secrets)
                for k, v in value.items()}
    if isinstance(value, list):
        return [materialize_runtime_secrets(v, secrets=secrets) for v in value]
    if isinstance(value, str) and value.startswith("secret://"):
        return secrets.resolve(value)
    return value


def control_paths(coordinator_root: str | Path) -> tuple[Path, Path]:
    """Return journal/SecretStore paths below a coordinator-private run root.

    The caller owns the trust boundary: this root must not be the worker workspace
    (or any of its descendants). ``RunManager.coordinator_control_dir`` enforces
    that invariant before this pure path helper is used.
    """
    root = Path(coordinator_root)
    return root / "control.db", root / "secrets"


def _coerce_apply_result(value: Any, *, targets: Sequence[WorkerRef]) -> ApplyResult:
    target_ids = [target.worker_id for target in targets]
    if isinstance(value, ApplyResult):
        return value
    if isinstance(value, Mapping):
        data = dict(value)
        data.setdefault("target_ids", target_ids)
        return ApplyResult.model_validate(data)
    if value is True:
        return ApplyResult(
            state=EffectState.EFFECT_OBSERVED,
            detail="coordinator acknowledged command effect",
            target_ids=target_ids,
        )
    return ApplyResult(
        state=EffectState.UNKNOWN,
        detail="coordinator acknowledgement did not prove an effect",
        target_ids=target_ids,
    )


class QueueControlPort:
    """Deliver a command to the existing coordinator queue with a real ACK fence."""

    def __init__(
        self,
        *,
        inbox: "asyncio.Queue[dict[str, Any]]",
        is_live: Callable[[], bool],
        ready: Optional[asyncio.Event] = None,
        worker_ready: Optional[asyncio.Event] = None,
        ack_timeout: float = 2.0,
        claim_timeout: Optional[float] = None,
        standby_actions: Sequence[str] = (),
        on_standby: Optional[Callable[[dict[str, Any]], Any]] = None,
    ) -> None:
        self.inbox = inbox
        self.is_live = is_live
        self.ready = ready
        self.worker_ready = worker_ready
        self.ack_timeout = max(0.01, float(ack_timeout))
        self.claim_timeout = max(
            self.ack_timeout,
            float(claim_timeout) if claim_timeout is not None
            else max(30.0, self.ack_timeout * 5.0),
        )
        self.standby_actions = frozenset(standby_actions)
        self.on_standby = on_standby

    @staticmethod
    def wire_command(command: ControlCommand) -> dict[str, Any]:
        return {
            "target": command.scope.as_legacy_target(),
            "action": command.action.value,
            **dict(command.payload),
            "command_id": command.command_id,
        }

    async def apply(
        self,
        command: ControlCommand,
        targets: Sequence[WorkerRef],
        desired: RunControlState,
    ) -> ApplyResult:
        del desired  # the queue consumer applies the desired transition
        wire = self.wire_command(command)
        if not self.is_live():
            if self.on_standby is not None:
                standby_result = self.on_standby(wire)
                if inspect.isawaitable(standby_result):
                    standby_result = await standby_result
                if standby_result is not None:
                    return _coerce_apply_result(standby_result, targets=targets)
            return ApplyResult(
                state=EffectState.UNKNOWN,
                detail="no live coordinator accepted the command",
                target_ids=[],
            )

        # RunManager publishes the Python driver task before the Swarm has started
        # its queue consumers.  Waiting on explicit readiness keeps startup-time
        # commands ordered and prevents the ordinary two-second ACK budget from
        # expiring while no consumer can possibly claim them.  Run termination is
        # deliberately exempt so STOP can still cancel a driver stuck in setup.
        termination_actions = {
            ControlAction.STOP,
            ControlAction.COMPLETE,
            ControlAction.FORCE_CANCEL,
        }
        readiness = self.ready
        if command.action in {
            ControlAction.SPAWN_WORKER,
            ControlAction.CANCEL_WORKER,
        }:
            readiness = self.worker_ready or readiness
        if (readiness is not None and not readiness.is_set()
                and command.action not in termination_actions):
            await readiness.wait()
            if not self.is_live():
                return ApplyResult(
                    state=EffectState.UNKNOWN,
                    detail="run ended before the coordinator control consumer became ready",
                    target_ids=[],
                    metadata={"code": "control_consumer_unavailable"},
                )

        loop = asyncio.get_running_loop()
        acknowledgement: "asyncio.Future[Any]" = loop.create_future()
        wire["_control_ack"] = acknowledgement
        wire["_control_deadline"] = loop.time() + self.ack_timeout
        wire["_control_started"] = False
        await self.inbox.put(wire)
        try:
            value = await asyncio.wait_for(
                asyncio.shield(acknowledgement), timeout=self.ack_timeout)
        except asyncio.TimeoutError:
            # Request cancellation before declaring UNKNOWN. If the envelope is
            # still queued, remove it and balance Queue.join bookkeeping. Once the
            # coordinator has dequeued/claimed it we must wait for its terminal ACK:
            # returning UNKNOWN on a wall-clock timeout would let the real effect
            # execute later and permanently fork journal state from runtime state.
            wire["_control_cancel_requested"] = True
            removed = False
            try:
                pending = getattr(self.inbox, "_queue")
                pending.remove(wire)
                self.inbox.task_done()
                removed = True
            except (AttributeError, ValueError):
                pass
            if not removed:
                try:
                    value = await asyncio.wait_for(
                        asyncio.shield(acknowledgement), timeout=self.claim_timeout)
                    return _coerce_apply_result(value, targets=targets)
                except asyncio.TimeoutError:
                    # The official consumer publishes its per-envelope task. Cancel
                    # that task and advance the consumer generation.  The generation
                    # fence matters because user/runtime callbacks can suppress
                    # ``CancelledError``: waiting for that stale child to exit would
                    # otherwise strand every later control command behind it.
                    consumer = wire.get("_control_consumer_task")
                    cancel_sent = isinstance(consumer, asyncio.Task)
                    if cancel_sent:
                        consumer.cancel()
                    restart_event = wire.get("_control_restart_event")
                    restart_requested = isinstance(restart_event, asyncio.Event)
                    if restart_requested:
                        restart_event.set()
                    try:
                        # Cooperative consumers resolve the ACK from their ``finally``
                        # immediately.  Keep this grace period bounded by the configured
                        # ACK policy so a cancellation-suppressing consumer cannot hold
                        # the actor for an additional hard-coded two seconds.
                        value = await asyncio.wait_for(
                            asyncio.shield(acknowledgement),
                            timeout=min(2.0, max(0.05, self.ack_timeout)),
                        )
                        return _coerce_apply_result(value, targets=targets)
                    except asyncio.TimeoutError:
                        return ApplyResult(
                            state=EffectState.UNKNOWN,
                            detail="claimed control consumer did not acknowledge cancellation",
                            target_ids=[target.worker_id for target in targets],
                            metadata={
                                "code": "claim_timeout",
                                "consumer_cancel_sent": cancel_sent,
                                "consumer_restart_requested": restart_requested,
                            },
                        )
            return ApplyResult(
                state=EffectState.UNKNOWN,
                detail=("command cancelled before coordinator routing" if removed
                        else "coordinator cancellation acknowledgement timed out"),
                target_ids=[target.worker_id for target in targets],
                metadata={"code": "ack_timeout", "cancelled_before_route": removed},
            )
        return _coerce_apply_result(value, targets=targets)
