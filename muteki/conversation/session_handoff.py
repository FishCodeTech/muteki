"""C38: structured session handoff when Runtime session is rebuilt.

When ``ensure_session`` closes the live AgentSession and starts a new one
(Provider switch, permission/access rebuild, credential change, or
force-new retry without native resume), Muteki must not rely on Thread UI
history alone. This module builds a branch-scoped handoff bundle that can
be injected into ``SessionStart.options.resume_prompt`` and/or the first
``AgentInput`` for an ordinary message turn.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any, Sequence

DEFAULT_MAX_MESSAGES = 40
DEFAULT_MAX_CHARS = 24_000

RECOVERY_REUSE_LIVE = "reuse_live"
RECOVERY_NATIVE_RESUME = "native_resume"
RECOVERY_STRUCTURED_HANDOFF = "structured_handoff"
RECOVERY_EMPTY = "empty"

REASON_RUNTIME_SWITCH = "runtime_switch"
REASON_PERMISSION = "permission"
REASON_CREDENTIAL = "credential"
REASON_RESTART = "restart"
REASON_RETRY = "retry"
REASON_FORK = "fork"
REASON_UNKNOWN = "unknown"


@dataclass
class HandoffMessage:
    role: str
    text: str
    turn_id: str = ""
    message_id: str = ""
    attachment_ids: list[str] = field(default_factory=list)
    capability_refs: list[dict[str, Any]] = field(default_factory=list)

    def to_dict(self) -> dict[str, Any]:
        return {
            "role": self.role,
            "text": self.text,
            "turn_id": self.turn_id,
            "message_id": self.message_id,
            "attachment_ids": list(self.attachment_ids),
            "capability_refs": list(self.capability_refs),
        }


@dataclass
class SessionHandoffBundle:
    """Portable recovery payload for a newly created AgentSession."""

    kind: str
    reason: str
    thread_id: str
    generation: int = 1
    source_adapter_id: str = ""
    source_agent_session_id: str = ""
    branch_message_count: int = 0
    included: list[HandoffMessage] = field(default_factory=list)
    omitted_count: int = 0
    omitted_reason: str = ""
    boundary_label: str = ""

    def to_event_payload(self) -> dict[str, Any]:
        return {
            "recovery_kind": self.kind,
            "recovery_reason": self.reason,
            "handoff_included_count": len(self.included),
            "handoff_omitted_count": self.omitted_count,
            "handoff_omitted_reason": self.omitted_reason,
            "handoff_branch_message_count": self.branch_message_count,
            "handoff_boundary_label": self.boundary_label,
            "handoff_attachment_ids": sorted({
                aid for item in self.included for aid in item.attachment_ids
            }),
            "source_adapter_id": self.source_adapter_id,
            "source_agent_session_id": self.source_agent_session_id,
        }

    def to_dict(self) -> dict[str, Any]:
        payload = self.to_event_payload()
        payload["included"] = [item.to_dict() for item in self.included]
        payload["thread_id"] = self.thread_id
        payload["generation"] = self.generation
        return payload


_ROLE_LABELS = {"user": "用户", "assistant": "助手", "system": "系统"}


def classify_rebuild_reason(
    *,
    switched: bool,
    force_new: bool,
    turn_kind: str,
    previous_session_key: str,
    new_session_key: str,
    previous_runtime_key: str,
    new_runtime_key: str,
) -> str:
    """Name why a new session is being created (for UI + events)."""
    if force_new or turn_kind == "retry":
        return REASON_RETRY
    if turn_kind == "resume" and not switched:
        return REASON_RESTART
    if not switched and previous_session_key == new_session_key:
        return REASON_RESTART
    if previous_runtime_key and previous_runtime_key != new_runtime_key:
        return REASON_RUNTIME_SWITCH
    prev = _split_session_key(previous_session_key)
    new = _split_session_key(new_session_key)
    if prev.get("credential_id") != new.get("credential_id"):
        return REASON_CREDENTIAL
    if (
        prev.get("access_mode") != new.get("access_mode")
        or prev.get("permission_mode") != new.get("permission_mode")
        or prev.get("sandbox_mode") != new.get("sandbox_mode")
    ):
        return REASON_PERMISSION
    if switched:
        return REASON_RUNTIME_SWITCH
    return REASON_UNKNOWN


def _split_session_key(session_key: str) -> dict[str, str]:
    text = str(session_key or "")
    parts = text.split("|")
    out: dict[str, str] = {
        "runtime_key": parts[0] if parts else "",
        "credential_id": parts[1] if len(parts) > 1 else "",
        "access_mode": "",
        "permission_mode": "",
        "sandbox_mode": "",
    }
    for part in parts[2:]:
        if "=" in part:
            key, _, value = part.partition("=")
            if key in out:
                out[key] = value
    return out


def build_session_handoff(
    *,
    thread_id: str,
    messages: Sequence[Any],
    turns: Sequence[Any] | None = None,
    exclude_turn_id: str = "",
    kind: str = RECOVERY_STRUCTURED_HANDOFF,
    reason: str = REASON_RUNTIME_SWITCH,
    generation: int = 1,
    source_adapter_id: str = "",
    source_agent_session_id: str = "",
    max_messages: int = DEFAULT_MAX_MESSAGES,
    max_chars: int = DEFAULT_MAX_CHARS,
) -> SessionHandoffBundle:
    """Build a handoff from current-branch messages, enriching from turns."""
    turn_by_id: dict[str, Any] = {}
    for turn in turns or ():
        turn_id = str(getattr(turn, "turn_id", "") or "")
        if turn_id:
            turn_by_id[turn_id] = turn

    history = [
        item for item in messages
        if str(getattr(item, "turn_id", "") or "") != exclude_turn_id
    ]
    branch_count = len(history)
    if not history:
        empty_kind = kind if kind == RECOVERY_NATIVE_RESUME else RECOVERY_EMPTY
        return SessionHandoffBundle(
            kind=empty_kind,
            reason=reason,
            thread_id=thread_id,
            generation=generation,
            source_adapter_id=source_adapter_id,
            source_agent_session_id=source_agent_session_id,
            branch_message_count=0,
            boundary_label=_boundary_label(empty_kind, reason, 0, 0),
        )

    selected: list[Any] = []
    char_total = 0
    omitted = 0
    omitted_reason = ""
    for item in reversed(list(history)):
        text = str(getattr(item, "text", "") or "")
        if not text.strip() and not _turn_attachments(turn_by_id, item):
            continue
        if len(selected) >= max_messages or (
            selected and char_total + len(text) > max_chars
        ):
            omitted += 1
            omitted_reason = "window"
            continue
        selected.append(item)
        char_total += len(text)
    selected.reverse()

    included: list[HandoffMessage] = []
    for item in selected:
        turn_id = str(getattr(item, "turn_id", "") or "")
        turn = turn_by_id.get(turn_id)
        attachment_ids = _turn_attachments(turn_by_id, item)
        refs: list[dict[str, Any]] = []
        if turn is not None:
            raw_refs = getattr(turn, "capability_refs", None) or []
            if isinstance(raw_refs, list):
                refs = [dict(ref) for ref in raw_refs if isinstance(ref, dict)]
        included.append(HandoffMessage(
            role=str(getattr(item, "role", "") or "user"),
            text=str(getattr(item, "text", "") or ""),
            turn_id=turn_id,
            message_id=str(getattr(item, "message_id", "") or ""),
            attachment_ids=attachment_ids,
            capability_refs=refs,
        ))

    if kind != RECOVERY_NATIVE_RESUME:
        kind = RECOVERY_STRUCTURED_HANDOFF
    label = _boundary_label(kind, reason, len(included), omitted)
    return SessionHandoffBundle(
        kind=kind,
        reason=reason,
        thread_id=thread_id,
        generation=generation,
        source_adapter_id=source_adapter_id,
        source_agent_session_id=source_agent_session_id,
        branch_message_count=branch_count,
        included=included,
        omitted_count=omitted,
        omitted_reason=omitted_reason,
        boundary_label=label,
    )


def _turn_attachments(turn_by_id: dict[str, Any], message: Any) -> list[str]:
    turn_id = str(getattr(message, "turn_id", "") or "")
    turn = turn_by_id.get(turn_id)
    if turn is None:
        return []
    raw = getattr(turn, "attachments", None) or []
    return [str(item) for item in raw if str(item)]


def _boundary_label(kind: str, reason: str, included: int, omitted: int) -> str:
    reason_zh = {
        REASON_RUNTIME_SWITCH: "Runtime 切换",
        REASON_PERMISSION: "权限/访问模式变更",
        REASON_CREDENTIAL: "凭据切换",
        REASON_RESTART: "进程重启接管",
        REASON_RETRY: "重试重建",
        REASON_FORK: "分叉继承",
        REASON_UNKNOWN: "会话重建",
    }.get(reason, "会话重建")
    if kind == RECOVERY_NATIVE_RESUME:
        return f"{reason_zh}：已尝试原生 resume 续接会话"
    if kind == RECOVERY_EMPTY:
        return f"{reason_zh}：无当前分支历史可交接"
    base = (
        f"{reason_zh}：Muteki 已注入整理后的当前分支历史"
        f"（{included} 条"
    )
    if omitted:
        base += f"，另有 {omitted} 条因窗口限制未纳入"
    return base + "）"


def render_transcript(bundle: SessionHandoffBundle) -> str:
    if not bundle.included:
        return ""
    lines: list[str] = []
    for item in bundle.included:
        label = _ROLE_LABELS.get(item.role, item.role)
        body = item.text.strip()
        extras: list[str] = []
        if item.attachment_ids:
            extras.append("附件:" + ",".join(item.attachment_ids))
        if item.capability_refs:
            names = [
                str(ref.get("name") or ref.get("id") or ref.get("kind") or "ref")
                for ref in item.capability_refs
            ]
            extras.append("引用:" + ",".join(names))
        if extras:
            body = (body + "\n[" + "; ".join(extras) + "]").strip()
        if body:
            lines.append(f"{label}：{body}")
    return "\n".join(lines)


def render_resume_prompt(
    bundle: SessionHandoffBundle,
    turn_text: str,
    *,
    action: str = "继续",
) -> str:
    transcript = render_transcript(bundle)
    current = (turn_text or "").strip() or "请继续此前未完成的工作。"
    if not transcript:
        return current
    omitted = ""
    if bundle.omitted_count:
        omitted = (
            f"\n（注：另有 {bundle.omitted_count} 条更早消息因上下文窗口"
            "限制未纳入，请勿臆造未给出的内容。）"
        )
    return (
        f"以下是本次{action}前由 Muteki 整理的当前分支对话历史，"
        "请只基于这些历史和当前用户消息继续："
        f"{omitted}\n{transcript}\n\n当前用户消息：{current}"
    )


def render_agent_text(
    bundle: SessionHandoffBundle,
    turn_text: str,
) -> str:
    return render_resume_prompt(bundle, turn_text, action="切换/重建后继续")


def continuation_prompt_from_messages(
    turn: Any,
    messages: Sequence[Any],
    turns: Sequence[Any] | None = None,
) -> str:
    action = "继续" if getattr(turn, "kind", "") == "resume" else "重试"
    bundle = build_session_handoff(
        thread_id=str(getattr(turn, "thread_id", "") or ""),
        messages=messages,
        turns=turns,
        exclude_turn_id=str(getattr(turn, "turn_id", "") or ""),
        kind=RECOVERY_STRUCTURED_HANDOFF,
        reason=REASON_RETRY if action == "重试" else REASON_RESTART,
    )
    return render_resume_prompt(
        bundle, str(getattr(turn, "text", "") or ""), action=action,
    )


__all__ = [
    "DEFAULT_MAX_CHARS",
    "DEFAULT_MAX_MESSAGES",
    "HandoffMessage",
    "REASON_CREDENTIAL",
    "REASON_FORK",
    "REASON_PERMISSION",
    "REASON_RESTART",
    "REASON_RETRY",
    "REASON_RUNTIME_SWITCH",
    "REASON_UNKNOWN",
    "RECOVERY_EMPTY",
    "RECOVERY_NATIVE_RESUME",
    "RECOVERY_REUSE_LIVE",
    "RECOVERY_STRUCTURED_HANDOFF",
    "SessionHandoffBundle",
    "build_session_handoff",
    "classify_rebuild_reason",
    "continuation_prompt_from_messages",
    "render_agent_text",
    "render_resume_prompt",
    "render_transcript",
]
