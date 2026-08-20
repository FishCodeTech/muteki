"""Pure admission rules for control commands.

Admission proves only that a command is well-formed and allowed.  It never
claims that a worker consumed the command; that belongs to ``EffectReceipt``.
"""

from __future__ import annotations

import json
import time
from dataclasses import dataclass
from typing import Optional

from muteki.control.models import (
    ControlAction,
    ControlCommand,
    RunControlMode,
    RunControlState,
    ScopeKind,
)


class AdmissionError(ValueError):
    def __init__(self, code: str, detail: str) -> None:
        super().__init__(detail)
        self.code = code
        self.detail = detail


@dataclass(frozen=True)
class AdmissionDecision:
    accepted: bool
    code: str = "accepted"
    detail: str = ""
    desired_mode: Optional[RunControlMode] = None


_TEXT_ACTIONS = {
    ControlAction.HINT,
    ControlAction.FOCUS,
    ControlAction.REDIRECT,
    ControlAction.DIRECTIVE,
    ControlAction.CORRECTION,
    ControlAction.SUBMIT,
}
_RUN_SCOPED_ACTIONS = {
    ControlAction.PAUSE,
    ControlAction.RESUME,
    ControlAction.STOP,
    ControlAction.COMPLETE,
    ControlAction.GRACEFUL_DRAIN,
    ControlAction.CLEAR_STANDING,
    ControlAction.RESET_GUIDANCE,
    ControlAction.MARK_FALSE,
}
_RUN_SCOPE_KINDS = {ScopeKind.GLOBAL, ScopeKind.RUN, ScopeKind.CHALLENGE}
_TERMINATED_FOLLOWUPS = {
    ControlAction.ASK,
    ControlAction.WRITEUP,
    ControlAction.HINT,
    ControlAction.FOCUS,
    ControlAction.REDIRECT,
    ControlAction.MARK_FALSE,
    ControlAction.ANSWER_DECISION,
    ControlAction.DISMISS,
    ControlAction.DISMISS_HELP,
    ControlAction.CLEAR_STANDING,
    ControlAction.RESET_GUIDANCE,
    ControlAction.EXPIRE_CONTEXT,
    # Idempotent cleanup follow-ups remain legal after a STOP moved desired state
    # to TERMINATED but its effect receipt was only PARTIAL/UNKNOWN. Otherwise the
    # orphaned runtime could never be re-signalled through the durable control path.
    ControlAction.STOP,
    ControlAction.FORCE_CANCEL,
}


class ControlAdmission:
    def __init__(self, *, max_payload_bytes: int = 64 * 1024,
                 challenge_id: Optional[str] = None) -> None:
        self.max_payload_bytes = max(1024, int(max_payload_bytes))
        self.challenge_id = str(challenge_id).strip() if challenge_id else None

    def admit(self, command: ControlCommand, state: RunControlState, *,
              now: Optional[float] = None) -> AdmissionDecision:
        now = time.time() if now is None else now
        if command.run_id != state.run_id:
            raise AdmissionError(
                "run_mismatch",
                f"command run {command.run_id!r} does not match journal run {state.run_id!r}",
            )
        if (command.scope.kind is ScopeKind.RUN
                and command.scope.value != state.run_id):
            raise AdmissionError(
                "scope_mismatch",
                f"run scope {command.scope.value!r} does not match {state.run_id!r}",
            )
        expected_challenge = self.challenge_id or state.run_id
        if (command.scope.kind is ScopeKind.CHALLENGE
                and command.scope.value != expected_challenge):
            raise AdmissionError(
                "scope_mismatch",
                f"challenge scope {command.scope.value!r} does not match "
                f"{expected_challenge!r}",
            )
        if command.deadline_at is not None and command.deadline_at <= now:
            raise AdmissionError("deadline_expired", "command deadline has expired")
        if (state.mode is RunControlMode.TERMINATED
                and command.action not in _TERMINATED_FOLLOWUPS):
            raise AdmissionError("run_terminated", "terminated run rejects live control commands")

        try:
            payload_raw = json.dumps(command.payload, ensure_ascii=False,
                                     sort_keys=True, separators=(",", ":"))
        except (TypeError, ValueError) as exc:
            raise AdmissionError("invalid_payload", "payload must be JSON serializable") from exc
        if len(payload_raw.encode("utf-8")) > self.max_payload_bytes:
            raise AdmissionError(
                "payload_too_large",
                f"payload exceeds {self.max_payload_bytes} bytes",
            )

        if command.action in _TEXT_ACTIONS:
            text = str(command.payload.get("text") or "").strip()
            url = str(command.payload.get("url") or
                      command.payload.get("target_url") or "").strip()
            if not text and not (command.action is ControlAction.REDIRECT and url):
                raise AdmissionError(
                    "missing_content",
                    f"{command.action.value} requires payload.text"
                    + (" or payload.url" if command.action is ControlAction.REDIRECT else ""),
                )

        if command.action in _RUN_SCOPED_ACTIONS:
            if command.scope.kind not in _RUN_SCOPE_KINDS:
                raise AdmissionError(
                    "invalid_scope",
                    f"{command.action.value} is a run-scoped action",
                )

        if command.action is ControlAction.ANSWER_DECISION:
            if not str(command.payload.get("request_id") or "").strip():
                raise AdmissionError(
                    "missing_request_id", "answer_decision requires payload.request_id"
                )
        elif command.action in {ControlAction.ADD_CONTEXT, ControlAction.EXPIRE_CONTEXT}:
            key = "context" if command.action is ControlAction.ADD_CONTEXT else "context_id"
            if not command.payload.get(key):
                raise AdmissionError(
                    "missing_context", f"{command.action.value} requires payload.{key}"
                )
        elif command.action is ControlAction.CANCEL_WORKER:
            if (command.scope.kind is not ScopeKind.WORKER
                    and not command.payload.get("worker_id")):
                raise AdmissionError(
                    "missing_worker", "cancel_worker requires worker scope or payload.worker_id"
                )

        return AdmissionDecision(
            accepted=True,
            desired_mode=desired_mode_for(command, state),
        )


def desired_mode_for(command: ControlCommand,
                     state: RunControlState) -> Optional[RunControlMode]:
    """Return a desired run-state mutation, or ``None`` for imperative commands.

    Targeted worker freeze/thaw is imperative and does not mutate the whole run's
    desired mode.  Global/run/challenge freeze and resume do.
    """
    run_scoped = command.scope.kind in _RUN_SCOPE_KINDS
    if command.action is ControlAction.PAUSE:
        return RunControlMode.QUIESCED
    if command.action is ControlAction.GRACEFUL_DRAIN and run_scoped:
        return RunControlMode.QUIESCED
    if command.action is ControlAction.FREEZE and run_scoped:
        return RunControlMode.FROZEN
    if command.action in {ControlAction.RESUME, ControlAction.THAW} and run_scoped:
        return RunControlMode.ACTIVE
    if command.action in {ControlAction.STOP, ControlAction.COMPLETE}:
        return RunControlMode.TERMINATED
    return None
