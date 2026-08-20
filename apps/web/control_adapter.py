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
import re
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
    IdempotencyConflict,
    RunControlState,
    WorkerRef,
)
from muteki.control.secrets import SecretStore, SecretStoreError
from muteki.core.events import control_command_payload


_RESERVED_BODY_KEYS = {
    "action", "target", "scope", "payload", "command_id",
    "expected_generation", "deadline_at",
}
_SENSITIVE_KEY = re.compile(
    r"(?:password|passwd|token|secret|credential|api[_-]?key|private[_-]?key|"
    r"密码|凭证|令牌|密钥)",
    re.IGNORECASE,
)
_SENSITIVE_TEXT = re.compile(
    r"(?:\b(?:password|passwd|token|secret|credential|api[ _-]?key|private[ _-]?key)"
    r"\b|密码|凭证|令牌|密钥)\s*(?::|=|is\s+|是\s*)?\S+",
    re.IGNORECASE,
)
_URL_USERINFO = re.compile(r"^[a-z][a-z0-9+.-]*://[^/@\s]+:[^/@\s]+@", re.IGNORECASE)


class ControlPayloadError(ValueError):
    """A clean client error while compiling a wire request."""


class _RetrySecretStore:
    """Reuse refs by canonical payload path, independent of JSON key order."""

    def __init__(self, base: SecretStore, command: ControlCommand) -> None:
        self.base = base
        self.command_id = command.command_id
        self.refs_by_path: dict[tuple[str, ...], str] = {}

        def _walk(value: Any, path: tuple[str, ...] = ()) -> None:
            if isinstance(value, Mapping):
                for key, child in value.items():
                    skey = str(key)
                    if not path and skey in {"secret_refs", "redacted"}:
                        continue
                    _walk(child, (*path, skey))
            elif isinstance(value, list):
                for index, child in enumerate(value):
                    _walk(child, (*path, str(index)))
            elif isinstance(value, str) and value.startswith("secret://"):
                self.refs_by_path[path] = value

        _walk(command.payload)

    def put(self, value: str) -> str:
        # Compatibility for callers without a path. A retry with multiple secret
        # fields is intentionally rejected rather than positionally guessing.
        if len(self.refs_by_path) != 1:
            raise IdempotencyConflict(
                f"command_id {self.command_id!r} was reused with different secret fields")
        return self._reuse(value, next(iter(self.refs_by_path.values())))

    def put_for_path(self, value: str, path: tuple[str, ...]) -> str:
        reference = self.refs_by_path.get(tuple(path))
        if not reference:
            raise IdempotencyConflict(
                f"command_id {self.command_id!r} was reused with different secret fields")
        return self._reuse(value, reference)

    def _reuse(self, value: str, reference: str) -> str:
        try:
            prior = self.base.resolve(reference)
        except Exception as exc:
            raise IdempotencyConflict(
                f"command_id {self.command_id!r} references unavailable secret material") from exc
        if prior != value:
            raise IdempotencyConflict(
                f"command_id {self.command_id!r} was reused with different content")
        return reference

    def get(self, reference: str) -> Any:
        return self.base.get(reference)


class _StagedSecretStore:
    """Roll back newly-created secret files if command validation fails.

    SecretStore publication is intentionally atomic, but compiling a command is a
    larger transaction: Pydantic/CAS fields are validated after payload traversal.
    Without this small staging owner, an invalid command could leave unreachable
    secret files behind even though no journal command existed.
    """

    def __init__(self, base: SecretStore) -> None:
        self.base = base
        self.created: list[str] = []

    def put(self, value: str) -> str:
        reference = self.base.put(value)
        self.created.append(reference)
        return reference

    def put_for_path(self, value: str, _path: tuple[str, ...]) -> str:
        return self.put(value)

    def get(self, reference: str) -> Any:
        return self.base.get(reference)

    def rollback(self) -> None:
        for reference in reversed(self.created):
            try:
                self.base.delete(reference)
            except SecretStoreError:
                pass
        self.created.clear()


def _looks_sensitive_text(value: str) -> bool:
    return bool(_SENSITIVE_TEXT.search(value))


def _put_secret(secrets: Any, value: str, path: tuple[str, ...]) -> str:
    by_path = getattr(secrets, "put_for_path", None)
    if callable(by_path):
        return str(by_path(value, path))
    return str(secrets.put(value))


def _validate_secret_reference(secrets: Any, reference: str) -> str:
    getter = getattr(secrets, "get", None)
    if not callable(getter):
        base = getattr(secrets, "base", None)
        getter = getattr(base, "get", None)
    if not callable(getter):
        raise ControlPayloadError("secret reference store is unavailable")
    try:
        metadata = getter(reference)
    except SecretStoreError as exc:
        raise ControlPayloadError("unknown or invalid secret reference") from exc
    canonical = str(getattr(metadata, "reference", reference) or reference)
    if canonical != reference:
        raise ControlPayloadError("non-canonical secret reference")
    return canonical


def _redact_value(value: Any, *, key: str, secrets: SecretStore,
                  references: list[str], path: tuple[str, ...] = ()) -> Any:
    if isinstance(value, Mapping):
        return {
            str(k): _redact_value(v, key=str(k), secrets=secrets,
                                  references=references,
                                  path=(*path, str(k)))
            for k, v in value.items()
        }
    if isinstance(value, list):
        return [
            _redact_value(v, key=key, secrets=secrets, references=references,
                          path=(*path, str(index)))
            for index, v in enumerate(value)
        ]
    if not isinstance(value, str):
        return value
    if value.startswith("secret://"):
        reference = _validate_secret_reference(secrets, value)
        references.append(reference)
        return reference
    sensitive = (
        bool(_SENSITIVE_KEY.search(key))
        or _looks_sensitive_text(value)
        or bool(_URL_USERINFO.search(value))
    )
    if not sensitive:
        return value
    reference = _put_secret(secrets, value, path)
    references.append(reference)
    return reference


def secure_payload(payload: Mapping[str, Any], *, secrets: SecretStore,
                   force_text_secret: bool = False) -> dict[str, Any]:
    """Replace operator secrets with run-local opaque references before journaling.

    The original value exists only inside ``SecretStore``.  The returned mapping is
    safe to place in SQLite and event payloads.
    """
    references: list[str] = []
    secured: dict[str, Any] = {}
    for key, value in payload.items():
        skey = str(key)
        if skey == "context" and isinstance(value, Mapping):
            context = dict(value)
            content = context.get("content")
            if (str(context.get("kind") or "").lower() == "secret_ref"
                    and isinstance(content, str)
                    and content and not content.startswith("secret://")):
                reference = _put_secret(secrets, content, (skey, "content"))
                references.append(reference)
                context["content"] = reference
            secured[skey] = _redact_value(
                context, key=skey, secrets=secrets, references=references,
                path=(skey,))
            continue
        if (force_text_secret and skey in {"text", "hint", "answer"}
                and isinstance(value, str) and value
                and not value.startswith("secret://")):
            reference = _put_secret(secrets, value, (skey,))
            references.append(reference)
            secured[skey] = reference
        else:
            secured[skey] = _redact_value(
                value, key=skey, secrets=secrets, references=references,
                path=(skey,))
    if references:
        # Semantic payload hashes must not depend on object insertion order.
        secured["secret_refs"] = sorted(set(references))
        secured["redacted"] = True
    return secured


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

    staged: Optional[_StagedSecretStore] = None
    secret_writer: Any
    if existing_command is not None:
        secret_writer = _RetrySecretStore(secrets, existing_command)
    else:
        staged = _StagedSecretStore(secrets)
        secret_writer = staged
    try:
        secured = secure_payload(
            payload,
            secrets=secret_writer,
            force_text_secret=action is ControlAction.ANSWER_DECISION,
        )
        values: dict[str, Any] = {
            "run_id": run_id,
            "action": action,
            "scope": scope,
            "payload": secured,
        }
        if body.get("command_id") is not None:
            values["command_id"] = body.get("command_id")
        if body.get("expected_generation") is not None:
            values["expected_generation"] = body.get("expected_generation")
        if body.get("deadline_at") is not None:
            values["deadline_at"] = body.get("deadline_at")
        return ControlCommand.model_validate(values)
    except Exception:
        if staged is not None:
            staged.rollback()
        raise


def safe_hitl_echo(command: ControlCommand, *, status: str) -> dict[str, Any]:
    """Small, non-secret operator echo for the conversation event stream."""
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
    if payload.get("redacted"):
        result["text"] = "[redacted operator secret]"
        refs = payload.get("secret_refs") or []
        if refs:
            result["secret_ref"] = str(refs[0])
    else:
        text = payload.get("text") or payload.get("hint")
        if text:
            result["text"] = str(text)[:2000]
        url = payload.get("url") or payload.get("target_url")
        if url:
            result["url"] = str(url)[:2000]
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
    if command.payload.get("redacted") or _looks_sensitive_text(detail):
        detail = "[redacted control detail]"
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
    value = str(detail or "")
    if command.payload.get("redacted") or _looks_sensitive_text(value):
        return "[redacted control detail]"
    return value[:32768]


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
        ack_timeout: float = 2.0,
        claim_timeout: Optional[float] = None,
        standby_actions: Sequence[str] = (),
        on_standby: Optional[Callable[[dict[str, Any]], Any]] = None,
    ) -> None:
        self.inbox = inbox
        self.is_live = is_live
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
