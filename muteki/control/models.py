"""Typed contracts for Muteki's operator control plane.

These models deliberately live outside ``muteki.swarm``.  Operator input is a
command/context stream, not evidence, and must never acquire evidence authority
merely because it was persisted or delivered to a worker.
"""

from __future__ import annotations

import hashlib
import json
import time
import uuid
from enum import Enum
from typing import Any, Optional

from pydantic import BaseModel, ConfigDict, Field, field_validator, model_validator


def _id(prefix: str) -> str:
    return f"{prefix}-{uuid.uuid4().hex}"


def _canonical_hash(value: Any) -> str:
    raw = json.dumps(value, ensure_ascii=False, sort_keys=True,
                     separators=(",", ":"), default=str)
    return hashlib.sha256(raw.encode("utf-8")).hexdigest()


def context_resource_id_for_command(command_id: str) -> str:
    """Stable ContextResource id shared by actor and runtime binding receipts."""
    value = str(command_id or "").strip()
    if not value:
        raise ValueError("command_id cannot be empty")
    return f"CTX-{hashlib.sha256(value.encode('utf-8')).hexdigest()[:24]}"


def continuation_intent_id_for_command(command_id: str) -> str:
    """Stable graph intent used to deliver worker-scoped prompt context.

    Worker ids identify one ephemeral process, not a resumable mailbox.  A typed
    operator command therefore gets its own exact continuation intent instead of
    silently widening to every worker of the same engine.
    """
    value = str(command_id or "").strip()
    if not value:
        raise ValueError("command_id cannot be empty")
    return f"I-control-{hashlib.sha256(value.encode('utf-8')).hexdigest()[:24]}"


class ScopeKind(str, Enum):
    GLOBAL = "global"
    RUN = "run"
    CHALLENGE = "challenge"
    WORKER = "worker"
    INTENT = "intent"
    ENGINE = "engine"
    LANE = "lane"


class ControlScope(BaseModel):
    """A typed selector.  ``solver:<id>`` is accepted as a legacy worker alias."""

    model_config = ConfigDict(frozen=True, extra="forbid")

    kind: ScopeKind = ScopeKind.GLOBAL
    value: str = ""

    @model_validator(mode="after")
    def _validate_value(self) -> "ControlScope":
        value = self.value.strip()
        if self.kind is ScopeKind.GLOBAL and value:
            raise ValueError("global scope cannot carry a value")
        if self.kind is not ScopeKind.GLOBAL and not value:
            raise ValueError(f"{self.kind.value} scope requires a value")
        object.__setattr__(self, "value", value)
        return self

    @classmethod
    def parse(cls, raw: "ControlScope | str | None") -> "ControlScope":
        if isinstance(raw, cls):
            return raw
        if isinstance(raw, dict):
            return cls(kind=raw.get("kind", ScopeKind.GLOBAL),
                       value=str(raw.get("value") or ""))
        text = str(raw or "global").strip()
        if text == "global":
            return cls()
        if ":" not in text:
            raise ValueError(f"invalid control scope: {text!r}")
        prefix, value = text.split(":", 1)
        prefix = prefix.strip().lower()
        if prefix == "solver":
            prefix = ScopeKind.WORKER.value
        try:
            kind = ScopeKind(prefix)
        except ValueError as exc:
            raise ValueError(f"unknown control scope kind: {prefix!r}") from exc
        return cls(kind=kind, value=value)

    def __str__(self) -> str:
        return "global" if self.kind is ScopeKind.GLOBAL else f"{self.kind.value}:{self.value}"

    def as_legacy_target(self) -> str:
        if self.kind is ScopeKind.WORKER:
            return f"solver:{self.value}"
        return str(self)


class ControlAction(str, Enum):
    ASK = "ask"
    HINT = "hint"
    FOCUS = "focus"
    REDIRECT = "redirect"
    DIRECTIVE = "directive"
    CORRECTION = "correction"
    PAUSE = "pause"
    FREEZE = "freeze"
    RESUME = "resume"
    THAW = "thaw"
    STOP = "stop"
    COMPLETE = "complete"
    GRACEFUL_DRAIN = "graceful_drain"
    FORCE_CANCEL = "force_cancel"
    DISMISS = "dismiss"
    DISMISS_HELP = "dismiss_help"
    CLEAR_STANDING = "clear_standing"
    RESET_GUIDANCE = "reset_guidance"
    MARK_FALSE = "mark_false"
    SUBMIT = "submit"
    WRITEUP = "writeup"
    ANSWER_DECISION = "answer_decision"
    ADD_CONTEXT = "add_context"
    EXPIRE_CONTEXT = "expire_context"
    SPAWN_WORKER = "spawn_worker"
    CANCEL_WORKER = "cancel_worker"


class ControlCommand(BaseModel):
    """An immutable, idempotent operator command.

    ``expected_generation`` is an optimistic CAS against the durable desired
    ``RunControlState``.  It is intentionally optional for legacy callers.
    """

    model_config = ConfigDict(frozen=True, extra="forbid")

    command_id: str = Field(default_factory=lambda: _id("C"), min_length=3,
                            max_length=128)
    run_id: str = Field(min_length=1, max_length=256)
    actor: str = Field(default="operator", min_length=1, max_length=256)
    action: ControlAction
    scope: ControlScope = Field(default_factory=ControlScope)
    payload: dict[str, Any] = Field(default_factory=dict)
    expected_generation: Optional[int] = Field(default=None, ge=0)
    deadline_at: Optional[float] = None
    created_at: float = Field(default_factory=time.time)

    @field_validator("command_id", "run_id", "actor")
    @classmethod
    def _strip_ids(cls, value: str) -> str:
        value = value.strip()
        if not value:
            raise ValueError("identifier cannot be empty")
        return value

    @field_validator("scope", mode="before")
    @classmethod
    def _parse_scope(cls, value: Any) -> ControlScope:
        return ControlScope.parse(value)

    def semantic_hash(self) -> str:
        """Hash the semantic body used with ``command_id`` for idempotency.

        Creation time is transport metadata and therefore excluded.  Reusing an
        id for a different action, scope, actor, payload, CAS, or deadline is a
        hard conflict rather than a retry.
        """
        body = self.model_dump(mode="json", exclude={"command_id", "created_at"})
        return _canonical_hash(body)


class ContextKind(str, Enum):
    CLUE = "clue"
    CONSTRAINT = "constraint"
    ENDPOINT = "endpoint"
    OBJECTIVE = "objective"
    SECRET_REF = "secret_ref"
    OPERATOR_NOTE = "operator_note"


class ContextTaint(str, Enum):
    OPERATOR_UNVERIFIED = "operator_unverified"
    SECRET_REFERENCE = "secret_reference"
    TRUSTED_SYSTEM = "trusted_system"


class ContextResource(BaseModel):
    """Durable operator context, explicitly separate from evidence."""

    model_config = ConfigDict(frozen=True, extra="forbid")

    context_id: str = Field(default_factory=lambda: _id("CTX"), min_length=3,
                            max_length=128)
    run_id: str = Field(min_length=1, max_length=256)
    kind: ContextKind = ContextKind.CLUE
    content: str = Field(min_length=1, max_length=32768)
    scope: ControlScope = Field(default_factory=ControlScope)
    taint: ContextTaint = ContextTaint.OPERATOR_UNVERIFIED
    standing: bool = False
    max_bindings: Optional[int] = Field(default=None, ge=1)
    created_at: float = Field(default_factory=time.time)
    expires_at: Optional[float] = None
    metadata: dict[str, Any] = Field(default_factory=dict)

    @field_validator("scope", mode="before")
    @classmethod
    def _parse_scope(cls, value: Any) -> ControlScope:
        return ControlScope.parse(value)

    @model_validator(mode="after")
    def _secret_is_reference_only(self) -> "ContextResource":
        if self.kind is ContextKind.SECRET_REF:
            if not self.content.startswith("secret://"):
                raise ValueError("secret_ref context must contain a secret:// reference")
            if self.taint is not ContextTaint.SECRET_REFERENCE:
                object.__setattr__(self, "taint", ContextTaint.SECRET_REFERENCE)
        return self

    def semantic_hash(self) -> str:
        return _canonical_hash(
            self.model_dump(mode="json", exclude={"context_id", "created_at"})
        )

    def is_expired(self, now: Optional[float] = None) -> bool:
        return self.expires_at is not None and self.expires_at <= (
            time.time() if now is None else now)


class DecisionKind(str, Enum):
    EXTERNAL_INPUT = "external_input"
    APPROVAL = "approval"
    CONFLICT = "conflict"
    RISK = "risk"
    UNCERTAINTY = "uncertainty"


class DecisionStatus(str, Enum):
    OPEN = "open"
    ANSWERED = "answered"
    DISMISSED = "dismissed"
    EXPIRED = "expired"


class DecisionRequest(BaseModel):
    """A scoped request for operator judgment.

    Only ``blocking_scope`` is held; unrelated workers remain dispatchable.
    ``request_id`` is present on the first emitted representation and is the key
    used by answers, so the UI never has to match requests by text.
    """

    model_config = ConfigDict(frozen=True, extra="forbid")

    request_id: str = Field(default_factory=lambda: _id("DR"), min_length=3,
                            max_length=128)
    run_id: str = Field(min_length=1, max_length=256)
    worker_id: str = Field(default="", max_length=256)
    prompt: str = Field(min_length=1, max_length=32768)
    kind: DecisionKind = DecisionKind.EXTERNAL_INPUT
    blocking_scope: ControlScope = Field(default_factory=ControlScope)
    choices: list[str] = Field(default_factory=list, max_length=32)
    default_action: str = Field(default="", max_length=256)
    # Correlation is deliberately part of the request identity.  A worker may
    # encounter the same blocker again after a fresh execution/resolve; that is a
    # new operator decision, while replaying the already-persisted event must retain
    # the original id.
    execution_id: str = Field(default="", max_length=256)
    execution_occurrence: str = Field(default="", max_length=256)
    resolve_epoch: str = Field(default="", max_length=128)
    deadline_at: Optional[float] = None
    created_at: float = Field(default_factory=time.time)
    metadata: dict[str, Any] = Field(default_factory=dict)

    @field_validator("blocking_scope", mode="before")
    @classmethod
    def _parse_scope(cls, value: Any) -> ControlScope:
        return ControlScope.parse(value)

    @field_validator("execution_id", "execution_occurrence", "resolve_epoch",
                     mode="before")
    @classmethod
    def _stringify_correlation(cls, value: Any) -> str:
        return "" if value is None else str(value)

    def semantic_hash(self) -> str:
        return _canonical_hash(
            self.model_dump(mode="json", exclude={"request_id", "created_at"})
        )


def stable_decision_request_id(*, run_id: str, worker_id: str, prompt: str,
                               kind: DecisionKind | str,
                               correlation_key: str = "",
                               execution_id: str = "",
                               execution_occurrence: str = "",
                               resolve_epoch: str | int = "") -> str:
    """Build a replay-stable id for one concrete decision occurrence.

    ``execution_occurrence`` (or ``resolve_epoch``) must change when a fresh
    execution encounters the same semantic blocker.  Replaying the persisted
    event reuses the id already carried by that event and therefore stays stable.
    """
    kind_value = kind.value if isinstance(kind, DecisionKind) else str(kind)
    digest = _canonical_hash({
        "run_id": run_id,
        "worker_id": worker_id,
        "prompt": prompt,
        "kind": kind_value,
        "correlation_key": correlation_key,
        "execution_id": str(execution_id or ""),
        "execution_occurrence": str(execution_occurrence or ""),
        "resolve_epoch": str(resolve_epoch if resolve_epoch is not None else ""),
    })[:20]
    return f"DR-{digest}"


class DecisionAnswer(BaseModel):
    model_config = ConfigDict(frozen=True, extra="forbid")

    answer_id: str = Field(default_factory=lambda: _id("DA"), min_length=3,
                           max_length=128)
    request_id: str = Field(min_length=3, max_length=128)
    run_id: str = Field(min_length=1, max_length=256)
    actor: str = Field(default="operator", min_length=1, max_length=256)
    status: DecisionStatus = DecisionStatus.ANSWERED
    answer: str = Field(default="", max_length=32768)
    created_at: float = Field(default_factory=time.time)
    metadata: dict[str, Any] = Field(default_factory=dict)

    @model_validator(mode="after")
    def _terminal_only(self) -> "DecisionAnswer":
        if self.status is DecisionStatus.OPEN:
            raise ValueError("a decision answer must be terminal")
        return self


class EffectState(str, Enum):
    RECEIVED = "received"
    PERSISTED = "persisted"
    ROUTED = "routed"
    EFFECT_OBSERVED = "effect_observed"
    PARTIAL = "partial"
    FAILED = "failed"
    UNKNOWN = "unknown"
    REJECTED = "rejected"

    @property
    def terminal(self) -> bool:
        return self in {
            EffectState.EFFECT_OBSERVED,
            EffectState.PARTIAL,
            EffectState.FAILED,
            EffectState.UNKNOWN,
            EffectState.REJECTED,
        }


EFFECT_TRANSITIONS: dict[Optional[EffectState], frozenset[EffectState]] = {
    None: frozenset({EffectState.RECEIVED}),
    EffectState.RECEIVED: frozenset({
        EffectState.PERSISTED, EffectState.REJECTED, EffectState.FAILED,
        EffectState.UNKNOWN,
    }),
    EffectState.PERSISTED: frozenset({
        EffectState.ROUTED, EffectState.REJECTED, EffectState.FAILED,
        EffectState.UNKNOWN,
    }),
    EffectState.ROUTED: frozenset({
        EffectState.EFFECT_OBSERVED, EffectState.PARTIAL,
        EffectState.FAILED, EffectState.UNKNOWN,
    }),
    EffectState.EFFECT_OBSERVED: frozenset(),
    EffectState.PARTIAL: frozenset(),
    EffectState.FAILED: frozenset(),
    EffectState.UNKNOWN: frozenset(),
    EffectState.REJECTED: frozenset(),
}


class EffectReceipt(BaseModel):
    """A fact about command delivery/effect, never a prediction."""

    model_config = ConfigDict(frozen=True, extra="forbid")

    receipt_id: str = Field(default_factory=lambda: _id("E"), min_length=3,
                            max_length=128)
    command_id: str = Field(min_length=3, max_length=128)
    run_id: str = Field(min_length=1, max_length=256)
    state: EffectState
    scope: ControlScope = Field(default_factory=ControlScope)
    target_ids: list[str] = Field(default_factory=list)
    detail: str = Field(default="", max_length=32768)
    observed_generation: Optional[int] = Field(default=None, ge=0)
    created_at: float = Field(default_factory=time.time)
    metadata: dict[str, Any] = Field(default_factory=dict)

    @field_validator("scope", mode="before")
    @classmethod
    def _parse_scope(cls, value: Any) -> ControlScope:
        return ControlScope.parse(value)

    def semantic_hash(self) -> str:
        return _canonical_hash(
            self.model_dump(mode="json", exclude={"receipt_id", "created_at"})
        )


class RunControlMode(str, Enum):
    ACTIVE = "active"
    QUIESCED = "quiesced"
    FROZEN = "frozen"
    TERMINATED = "terminated"


class RunControlState(BaseModel):
    """Stable desired run state; worker effects are reported separately."""

    model_config = ConfigDict(frozen=True, extra="forbid")

    run_id: str = Field(min_length=1, max_length=256)
    generation: int = Field(default=0, ge=0)
    mode: RunControlMode = RunControlMode.ACTIVE
    updated_by_command_id: str = ""
    reason: str = Field(default="", max_length=32768)
    updated_at: float = Field(default_factory=time.time)


class WorkerRef(BaseModel):
    """Serializable worker identity exposed by a live registry."""

    model_config = ConfigDict(frozen=True, extra="forbid")

    worker_id: str = Field(min_length=1, max_length=256)
    engine: str = Field(default="", max_length=128)
    intent_id: str = Field(default="", max_length=256)
    lane: str = Field(default="", max_length=256)
    challenge_id: str = Field(default="", max_length=256)
    status: str = Field(default="running", max_length=128)
    metadata: dict[str, Any] = Field(default_factory=dict)


class ApplyResult(BaseModel):
    """Result returned by an adapter after it attempted the real effect."""

    model_config = ConfigDict(frozen=True, extra="forbid")

    state: EffectState
    detail: str = Field(default="", max_length=32768)
    target_ids: list[str] = Field(default_factory=list)
    metadata: dict[str, Any] = Field(default_factory=dict)

    @model_validator(mode="after")
    def _terminal_effect_only(self) -> "ApplyResult":
        if not self.state.terminal or self.state is EffectState.REJECTED:
            raise ValueError("adapter result must be a terminal effect state")
        return self
