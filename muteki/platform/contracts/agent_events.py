"""Typed payload contract for ``AgentEvent`` (adapter -> executor boundary).

Every ``AgentEventType`` has exactly one payload model.  Adapters translate
native engine frames into these models; whatever the engine reports beyond
the normalized fields goes into ``native`` and is never read for control
flow.  The executor validates each event once with :func:`parse_payload`
before translating it into Thread public events.

Reasoning representation
    Reasoning is only ever carried by ``REASONING_SUMMARY`` with an explicit
    ``channel``: ``summary`` for engine-written summaries (Codex reasoning
    summary items) and ``thinking`` for raw thinking text (Claude thinking
    blocks, ACP ``agent_thought_chunk``, Kimi ``thinking.delta``).
    ``MESSAGE_DELTA`` / ``MESSAGE_COMPLETED`` only carry visible assistant
    (or replayed user) text.

Usage representation
    ``input_tokens`` always includes cached input; ``cached_input_tokens`` and
    ``cache_write_tokens`` are the cached subsets.  ``scope`` states how the
    numbers aggregate (per message, per turn, per invocation, session
    cumulative, or context occupancy only).

Failures
    ``TURN_FAILED`` and ``RUNTIME_ERROR`` carry ``error: AgentFailure``.  The
    stable code is ``external_agent.<category>``; the engine-specific suffix
    that older events used as the code (``empty_assistant``,
    ``native_command.unconfirmed``...) is ``reason``.  Consumers compare
    ``category``/``reason``, never message text.
"""

from __future__ import annotations

import re
from enum import Enum
from typing import Any, Literal, Optional, Union

from pydantic import (
    BaseModel,
    ConfigDict,
    Field,
    ValidationError,
    field_validator,
    model_validator,
)

from .external_agents import AgentEventType

AGENT_EVENT_CONTRACT_CODE = "external_agent.contract.invalid_payload"
AGENT_EVENT_RESERVED_KEY_CODE = "external_agent.contract.reserved_key"

#: Keys owned by the executor / Thread event envelope.  An adapter payload
#: that carries one of them would silently override core fields.
RESERVED_PAYLOAD_KEYS = frozenset({
    "agent_session_id",
    "command_id",
    "correlation_id",
    "event_id",
    "native_type",
    "occurred_at",
    "runtime",
    "runtime_turn_id",
    "schema_version",
    "seq",
    "stream_seq",
    "thread_id",
    "turn_id",
    "usage_native_type",
})


class AgentEventContractError(ValueError):
    """An adapter emitted a payload that does not match the event contract."""

    def __init__(self, code: str, event_type: str, message: str,
                 *, errors: Optional[list[dict[str, Any]]] = None) -> None:
        super().__init__(f"{code}: {event_type}: {message}")
        self.code = code
        self.event_type = event_type
        self.message = message
        self.errors = list(errors or [])


# -- redaction -----------------------------------------------------------------

_REDACTED = "[REDACTED]"
_SECRET_PATTERNS: tuple[tuple[re.Pattern[str], str], ...] = (
    (re.compile(r"(?i)\b(bearer|basic)\s+[A-Za-z0-9._~+/=-]{8,}"), rf"\1 {_REDACTED}"),
    (re.compile(r"\b(sk|rk|pk)-[A-Za-z0-9_-]{8,}"), rf"\1-{_REDACTED}"),
    (re.compile(r"\bcrsr_[A-Za-z0-9_-]{8,}"), _REDACTED),
    (re.compile(r"\b(gh[pousr]_|github_pat_|xox[abposr]-|AKIA)[A-Za-z0-9_-]{8,}"), rf"\1{_REDACTED}"),
    # Match after the fixed URL delimiter instead of repeatedly scanning a
    # long scheme-like word. Keep masking even when a log prefixes the URL.
    (re.compile(r"(?<=://)[^/\s:@]+:[^/\s@]+@"), f"{_REDACTED}@"),
    (re.compile(
        r"(?i)([?&](?:access_token|refresh_token|id_token|token|api[_-]?key|key|"
        r"secret|password|passwd|auth|signature|sig|code)=)[^&\s#\"']+"),
     rf"\1{_REDACTED}"),
    (re.compile(
        r"(?i)([\"']?(?:authorization|x-api-key|api[_-]?key|access[_-]?token|"
        r"refresh[_-]?token|client[_-]?secret|password)[\"']?\s*[:=]\s*[\"']?)"
        r"(?!\[REDACTED\])(?!(?:bearer|basic)\s)[^\s\"',}]+"),
     rf"\1{_REDACTED}"),
)


def redact_secrets(text: str) -> str:
    """Mask credentials (bearer/basic tokens, API keys, URL credentials and
    secret query/header values) in diagnostic text before it is persisted."""
    result = str(text)
    for pattern, replacement in _SECRET_PATTERNS:
        result = pattern.sub(replacement, result)
    return result


# -- shared building blocks ----------------------------------------------------


class ContractModel(BaseModel):
    """Strict payload base.  Unlike ``base.ContractModel`` it has no
    ``schema_version`` field: payloads are versioned by their event type and
    a per-payload version would leak into every nested public event."""

    model_config = ConfigDict(extra="forbid")


class _Payload(ContractModel):
    """Base of every event payload: engine leftovers only live in ``native``."""

    native: dict[str, Any] = Field(default_factory=dict)


class FailureCategory(str, Enum):
    USAGE_LIMIT = "usage_limit"
    AUTH = "auth"
    TRANSPORT = "transport"
    PERMISSION = "permission"
    VALIDATION = "validation"
    PROVIDER = "provider"
    RUNTIME_EXITED = "runtime_exited"
    TIMEOUT = "timeout"
    CANCELLED = "cancelled"
    UNSUPPORTED = "unsupported"
    UNKNOWN = "unknown"


def failure_code(category: FailureCategory | str) -> str:
    return f"external_agent.{FailureCategory(category).value}"


class AgentFailure(ContractModel):
    """Typed failure carried by ``TURN_FAILED`` and ``RUNTIME_ERROR``.

    ``message`` is a one-line human summary; ``detail`` is the complete
    (redacted) diagnostic text.  Neither is truncated.
    """

    code: str = ""
    category: FailureCategory
    # Stable engine-specific suffix (``empty_assistant``, ``approval.stale``).
    reason: str = Field(min_length=1)
    engine: str = Field(min_length=1)
    message: str = Field(min_length=1)
    detail: str = ""
    retryable: bool = False
    # The engine's own error code / exception type, when it reports one.
    native_code: Optional[str] = None
    # The command may have reached the engine; resending could duplicate it.
    delivery_unknown: bool = False

    @field_validator("message", "detail")
    @classmethod
    def _redact(cls, value: str) -> str:
        return redact_secrets(value)

    @model_validator(mode="after")
    def _stable_code(self) -> "AgentFailure":
        expected = failure_code(self.category)
        if self.code and self.code != expected:
            raise ValueError(
                f"failure code {self.code!r} must be {expected!r} for "
                f"category {self.category.value!r}")
        self.code = expected
        return self


def agent_failure(
    category: FailureCategory | str,
    reason: str,
    *,
    engine: str,
    message: str,
    detail: Any = "",
    retryable: bool = False,
    native_code: Optional[str] = None,
    delivery_unknown: bool = False,
) -> AgentFailure:
    """Build an :class:`AgentFailure`; ``detail`` keeps the full text."""
    text = str(detail) if detail not in (None, "") else ""
    return AgentFailure(
        category=FailureCategory(category),
        reason=reason,
        engine=engine,
        message=str(message).strip() or reason,
        detail=text,
        retryable=retryable,
        native_code=native_code,
        delivery_unknown=delivery_unknown,
    )


def exception_failure(
    exc: BaseException,
    category: FailureCategory | str,
    reason: str,
    *,
    engine: str,
    message: str,
    retryable: bool = False,
    delivery_unknown: bool = False,
) -> AgentFailure:
    """Failure from an exception: summary message plus full exception text."""
    return agent_failure(
        category, reason, engine=engine, message=message,
        detail=f"{type(exc).__name__}: {exc}", retryable=retryable,
        native_code=type(exc).__name__, delivery_unknown=delivery_unknown,
    )


ToolStatus = Literal["pending", "running", "completed", "failed", "cancelled"]
FileChangeKind = Literal["add", "modify", "delete", "rename"]


class WorkspaceFileChange(ContractModel):
    path: str = Field(min_length=1)
    change: Optional[FileChangeKind] = None
    old_path: Optional[str] = None
    unified_diff: Optional[str] = None


class RateLimitState(ContractModel):
    limited: bool = False
    warning: bool = False
    # Unix seconds of the latest reset among exhausted windows.
    resets_at: Optional[float] = None
    kind: Optional[str] = None
    utilization: Optional[float] = None


# -- per-event payloads --------------------------------------------------------


class SessionPayload(_Payload):
    """SESSION_STARTED / SESSION_RESUMED."""

    transport: Optional[str] = None
    adapter_id: Optional[str] = None
    instance_id: Optional[str] = None
    cwd: Optional[str] = None
    model: Optional[str] = None


class SessionClosedPayload(_Payload):
    reason: Optional[str] = None
    resume_available: bool = False


class TurnStartedPayload(_Payload):
    kind: Optional[str] = None


class TurnCompletedPayload(_Payload):
    stop_reason: Optional[str] = None
    duration_ms: Optional[int] = None


class TurnFailedPayload(_Payload):
    error: AgentFailure


MessageRole = Literal["assistant", "user"]


class MessageDeltaPayload(_Payload):
    text: str
    role: MessageRole = "assistant"
    phase: Optional[str] = None
    message_id: Optional[str] = None


class MessageCompletedPayload(_Payload):
    text: str
    role: MessageRole = "assistant"
    phase: Optional[str] = None
    message_id: Optional[str] = None


ReasoningChannel = Literal["summary", "thinking"]


class ReasoningPayload(_Payload):
    """REASONING_SUMMARY: the only reasoning carrier (see module docstring).

    ``partial=True`` frames are deltas appended to the same ``item_id``;
    ``partial=False`` is a complete block.
    """

    text: str = Field(min_length=1)
    channel: ReasoningChannel
    partial: bool = False
    item_id: Optional[str] = None
    duration_ms: Optional[int] = None


class ToolPayload(_Payload):
    """TOOL_STARTED / TOOL_PROGRESS / TOOL_COMPLETED."""

    tool_call_id: str = Field(min_length=1)
    name: Optional[str] = None
    # Engine tool family: command | file_change | mcp | web | agent | other.
    kind: Optional[str] = None
    title: Optional[str] = None
    input: Any = None
    output: Any = None
    # Incremental output text (TOOL_PROGRESS only).
    chunk: Optional[str] = None
    status: Optional[ToolStatus] = None
    error: Optional[str] = None
    exit_code: Optional[int] = None
    duration_ms: Optional[int] = None
    parent_tool_call_id: Optional[str] = None
    agent_id: Optional[str] = None
    model: Optional[str] = None
    message_id: Optional[str] = None


class ApprovalOption(ContractModel):
    option_id: str = Field(min_length=1)
    label: str = ""
    # allow_once | allow_always | reject_once | reject_always | engine value
    kind: Optional[str] = None


ApprovalKind = Literal[
    "command_execution", "file_change", "permissions", "mcp_tool_call", "tool",
    "plan_exit",
]


class ApprovalRequestedPayload(_Payload):
    approval_id: str = Field(min_length=1)
    # Delegated subagent that owns the request, when the engine attributes it.
    agent_id: Optional[str] = None
    approval_kind: Optional[ApprovalKind] = None
    title: Optional[str] = None
    reason: Optional[str] = None
    tool_call_id: Optional[str] = None
    tool_name: Optional[str] = None
    command: Optional[str] = None
    cwd: Optional[str] = None
    path: Optional[str] = None
    paths: list[str] = Field(default_factory=list)
    unified_diff: Optional[str] = None
    files: list[WorkspaceFileChange] = Field(default_factory=list)
    input: Any = None
    options: list[ApprovalOption] = Field(default_factory=list)
    # Approval scopes the engine can honor (once | session).
    scopes: list[str] = Field(default_factory=list)
    expires_at: Optional[str] = None
    # Full proposed plan when ``approval_kind == "plan_exit"``. Never truncated.
    plan_markdown: Optional[str] = None


ApprovalOutcome = Literal["allow", "deny", "cancelled", "expired"]


class ApprovalResolvedPayload(_Payload):
    approval_id: str = Field(min_length=1)
    agent_id: Optional[str] = None
    decision: Optional[ApprovalOutcome] = None
    scope: Optional[str] = None
    option_id: Optional[str] = None
    note: Optional[str] = None
    # True when the engine resolved it by its own policy (no operator).
    automatic: bool = False


class UserInputRequestedPayload(_Payload):
    request_id: str = Field(min_length=1)
    # Engine interaction flavor (``acp.elicitation``, ``codex.mcp_elicitation``...).
    user_input_kind: Optional[str] = None
    agent_id: Optional[str] = None
    title: Optional[str] = None
    message: Optional[str] = None
    # Normalized questions (``user_input_schema.normalize_question`` shape).
    questions: list[dict[str, Any]] = Field(default_factory=list)
    # JSON-schema style form (MCP/ACP elicitation ``requestedSchema``).
    requested_schema: Optional[dict[str, Any]] = None
    options: list[dict[str, Any]] = Field(default_factory=list)
    # Decisions the engine accepts (``submit`` / ``cancel`` / ``decline``).
    response_actions: list[str] = Field(default_factory=list)
    tool_call_id: Optional[str] = None
    expires_at: Optional[str] = None


UserInputOutcome = Literal["answered", "cancelled", "declined", "timeout", "expired"]


class UserInputResolvedPayload(_Payload):
    request_id: str = Field(min_length=1)
    outcome: Optional[UserInputOutcome] = None
    answers: Optional[dict[str, Any]] = None


class ArtifactPayload(_Payload):
    name: str = Field(min_length=1)
    kind: Optional[str] = None
    media_type: Optional[str] = None
    # Exactly one of ``path`` (engine-written file) or base64 content.
    path: Optional[str] = None
    content_base64: Optional[str] = None

    @model_validator(mode="after")
    def _one_source(self) -> "ArtifactPayload":
        if (self.path is None) == (self.content_base64 is None):
            raise ValueError("artifact payload needs exactly one of path or content_base64")
        return self


class WorkspaceChangedPayload(_Payload):
    unified_diff: Optional[str] = None
    files: list[WorkspaceFileChange] = Field(default_factory=list)


UsageScope = Literal[
    # One model message; ``usage_id`` identifies it (re-reports overwrite).
    "message",
    # The whole turn.
    "turn",
    # One engine invocation (CLI process run).
    "invocation",
    # Session-wide running totals; consumers diff successive reports.
    "session_cumulative",
    # Context-window occupancy only; token fields are not consumption.
    "context_only",
]


class UsagePayload(_Payload):
    scope: UsageScope
    usage_id: Optional[str] = None
    input_tokens: Optional[int] = Field(default=None, ge=0)
    output_tokens: Optional[int] = Field(default=None, ge=0)
    cached_input_tokens: Optional[int] = Field(default=None, ge=0)
    cache_write_tokens: Optional[int] = Field(default=None, ge=0)
    reasoning_tokens: Optional[int] = Field(default=None, ge=0)
    total_tokens: Optional[int] = Field(default=None, ge=0)
    context_window: Optional[int] = Field(default=None, ge=0)
    context_used_tokens: Optional[int] = Field(default=None, ge=0)
    cost_usd: Optional[float] = Field(default=None, ge=0)
    step_count: Optional[int] = Field(default=None, ge=0)
    llm_duration_ms: Optional[int] = Field(default=None, ge=0)

    @model_validator(mode="after")
    def _message_identity(self) -> "UsagePayload":
        if self.scope in {"message", "invocation"} and not self.usage_id:
            raise ValueError(f"usage scope {self.scope!r} requires usage_id")
        return self


PlanTaskStatus = Literal["pending", "in_progress", "completed", "blocked", "cancelled"]


class PlanTaskPayload(ContractModel):
    task_id: str = Field(min_length=1)
    title: str = Field(min_length=1)
    status: PlanTaskStatus = "pending"
    blocked_reason: Optional[str] = None


class PlanPayload(_Payload):
    tasks: list[PlanTaskPayload] = Field(default_factory=list)
    title: Optional[str] = None
    explanation: Optional[str] = None
    # Plan lifecycle phase the engine reports (``plan.py`` re-derives it from
    # task states when omitted).
    phase: Optional[str] = None
    # True: ``tasks`` upsert into the current plan; False: full replacement.
    patch: bool = False
    revision: Optional[int] = Field(default=None, ge=0)


AgentStatus = Literal["pending", "running", "completed", "failed", "cancelled"]


class AgentNodePayload(ContractModel):
    """Delegated agent node patch.  Only explicitly set fields are applied;
    an explicit ``None`` clears the stored value."""

    agent_id: str = Field(min_length=1)
    parent_id: Optional[str] = None
    title: Optional[str] = None
    nickname: Optional[str] = None
    role: Optional[str] = None
    model: Optional[str] = None
    message_id: Optional[str] = None
    call_id: Optional[str] = None
    session_ref: Optional[str] = None
    status: Optional[AgentStatus] = None
    request: Optional[str] = None
    result: Optional[str] = None
    error: Optional[str] = None
    activity: Optional[str] = None
    tool_uses: Optional[int] = None
    total_tokens: Optional[int] = None
    duration_ms: Optional[int] = None


class AgentUpdatedPayload(_Payload):
    agents: list[AgentNodePayload] = Field(default_factory=list)
    patch: bool = True
    # True when the engine provably has no delegation events; consumers read
    # this pair (``conversation/agents.py``) instead of inferring absence.
    unsupported: bool = False
    unsupported_reason: Optional[str] = None

    @model_validator(mode="after")
    def _agents_or_unsupported(self) -> "AgentUpdatedPayload":
        if not self.agents and not self.unsupported:
            raise ValueError(
                "agent update needs at least one node unless unsupported=True")
        return self


class RuntimeCapabilitiesPayload(_Payload):
    revision: Optional[int] = None
    reason: Optional[str] = None
    capabilities: Optional[dict[str, Any]] = None


RuntimeWarningKind = Literal[
    "rate_limit", "notice", "degraded", "config", "protocol", "process",
]


class RuntimeWarningPayload(_Payload):
    kind: RuntimeWarningKind
    message: str = Field(min_length=1)
    # Stable machine code of the warning (engine-specific suffix allowed).
    code: Optional[str] = None
    rate_limit: Optional[RateLimitState] = None

    @field_validator("message")
    @classmethod
    def _redact(cls, value: str) -> str:
        return redact_secrets(value)

    @model_validator(mode="after")
    def _rate_limit_shape(self) -> "RuntimeWarningPayload":
        if (self.kind == "rate_limit") != (self.rate_limit is not None):
            raise ValueError("rate_limit is required exactly when kind == 'rate_limit'")
        return self


class RuntimeErrorPayload(_Payload):
    error: AgentFailure


class RuntimeExitedPayload(_Payload):
    # sessions.EXIT_* classification: interrupted | failed | resumable | closed
    classification: str = Field(min_length=1)
    exit_code: Optional[int] = None
    error: Optional[AgentFailure] = None


PayloadModel = Union[
    SessionPayload, SessionClosedPayload, TurnStartedPayload,
    TurnCompletedPayload, TurnFailedPayload, MessageDeltaPayload,
    MessageCompletedPayload, ReasoningPayload, ToolPayload,
    ApprovalRequestedPayload, ApprovalResolvedPayload,
    UserInputRequestedPayload, UserInputResolvedPayload, ArtifactPayload,
    WorkspaceChangedPayload, UsagePayload, PlanPayload, AgentUpdatedPayload,
    RuntimeCapabilitiesPayload, RuntimeWarningPayload, RuntimeErrorPayload,
    RuntimeExitedPayload,
]

PAYLOAD_MODELS: dict[AgentEventType, type[_Payload]] = {
    AgentEventType.SESSION_STARTED: SessionPayload,
    AgentEventType.SESSION_RESUMED: SessionPayload,
    AgentEventType.SESSION_CLOSED: SessionClosedPayload,
    AgentEventType.TURN_STARTED: TurnStartedPayload,
    AgentEventType.TURN_COMPLETED: TurnCompletedPayload,
    AgentEventType.TURN_FAILED: TurnFailedPayload,
    AgentEventType.MESSAGE_DELTA: MessageDeltaPayload,
    AgentEventType.MESSAGE_COMPLETED: MessageCompletedPayload,
    AgentEventType.REASONING_SUMMARY: ReasoningPayload,
    AgentEventType.TOOL_STARTED: ToolPayload,
    AgentEventType.TOOL_PROGRESS: ToolPayload,
    AgentEventType.TOOL_COMPLETED: ToolPayload,
    AgentEventType.APPROVAL_REQUESTED: ApprovalRequestedPayload,
    AgentEventType.APPROVAL_RESOLVED: ApprovalResolvedPayload,
    AgentEventType.USER_INPUT_REQUESTED: UserInputRequestedPayload,
    AgentEventType.USER_INPUT_RESOLVED: UserInputResolvedPayload,
    AgentEventType.ARTIFACT_CREATED: ArtifactPayload,
    AgentEventType.WORKSPACE_CHANGED: WorkspaceChangedPayload,
    AgentEventType.USAGE_UPDATED: UsagePayload,
    AgentEventType.PLAN_UPDATED: PlanPayload,
    AgentEventType.AGENT_UPDATED: AgentUpdatedPayload,
    AgentEventType.RUNTIME_CAPABILITIES_UPDATED: RuntimeCapabilitiesPayload,
    AgentEventType.RUNTIME_WARNING: RuntimeWarningPayload,
    AgentEventType.RUNTIME_ERROR: RuntimeErrorPayload,
    AgentEventType.RUNTIME_EXITED: RuntimeExitedPayload,
}

missing = set(AgentEventType) - set(PAYLOAD_MODELS)
if missing:  # pragma: no cover - import-time contract guard
    raise RuntimeError(f"AgentEventType without payload model: {sorted(m.value for m in missing)}")
del missing


def dump_payload(model: ContractModel) -> dict[str, Any]:
    """Serialize a payload model for ``AgentEvent.payload``.

    ``exclude_unset`` keeps patch semantics: an unset agent-node field is not
    the same as an explicit ``None``.
    """
    return model.model_dump(mode="json", exclude_unset=True)


def parse_payload(event_type: AgentEventType | str, payload: dict[str, Any]) -> _Payload:
    """Validate ``payload`` against the contract of ``event_type``.

    Raises :class:`AgentEventContractError` with a stable code on reserved
    keys, unknown fields, missing fields or wrong types.
    """
    try:
        etype = AgentEventType(event_type)
    except ValueError as exc:
        raise AgentEventContractError(
            AGENT_EVENT_CONTRACT_CODE, str(event_type), "unknown event type") from exc
    if not isinstance(payload, dict):
        raise AgentEventContractError(
            AGENT_EVENT_CONTRACT_CODE, etype.value,
            f"payload must be an object, got {type(payload).__name__}")
    reserved = sorted(RESERVED_PAYLOAD_KEYS.intersection(payload))
    if reserved:
        raise AgentEventContractError(
            AGENT_EVENT_RESERVED_KEY_CODE, etype.value,
            f"payload uses executor-owned keys {reserved}")
    try:
        return PAYLOAD_MODELS[etype].model_validate(payload)
    except ValidationError as exc:
        raise AgentEventContractError(
            AGENT_EVENT_CONTRACT_CODE, etype.value, str(exc),
            errors=exc.errors(include_url=False, include_context=False)) from exc


def validate_payload(event_type: AgentEventType | str, payload: dict[str, Any]) -> dict[str, Any]:
    """Validate and return the normalized payload dict."""
    return dump_payload(parse_payload(event_type, payload))


__all__ = [
    "AGENT_EVENT_CONTRACT_CODE",
    "AGENT_EVENT_RESERVED_KEY_CODE",
    "AgentEventContractError",
    "AgentFailure",
    "AgentNodePayload",
    "AgentUpdatedPayload",
    "ApprovalOption",
    "ApprovalRequestedPayload",
    "ApprovalResolvedPayload",
    "ArtifactPayload",
    "FailureCategory",
    "MessageCompletedPayload",
    "MessageDeltaPayload",
    "PAYLOAD_MODELS",
    "PlanPayload",
    "PlanTaskPayload",
    "RESERVED_PAYLOAD_KEYS",
    "RateLimitState",
    "ReasoningPayload",
    "RuntimeCapabilitiesPayload",
    "RuntimeErrorPayload",
    "RuntimeExitedPayload",
    "RuntimeWarningPayload",
    "SessionClosedPayload",
    "SessionPayload",
    "ToolPayload",
    "TurnCompletedPayload",
    "TurnFailedPayload",
    "TurnStartedPayload",
    "UsagePayload",
    "UserInputRequestedPayload",
    "UserInputResolvedPayload",
    "WorkspaceChangedPayload",
    "WorkspaceFileChange",
    "agent_failure",
    "dump_payload",
    "exception_failure",
    "failure_code",
    "parse_payload",
    "redact_secrets",
    "validate_payload",
]
