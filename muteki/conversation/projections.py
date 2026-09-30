"""Conversation 投影：Thread 事件流 → 读模型（CONV-01，任务书 9.1）。

``ConversationProjection`` 消费 ``(thread, thread_id)`` 聚合流事件，维护
``ConversationStore`` 中的消息、Turn 状态与 Thread 读模型（unread /
running / pending approval / usage / last error）。重启后可用
``rebuild_thread`` 从事件流重放恢复读模型；事件是唯一事实来源。
"""

from __future__ import annotations

from typing import Any

from muteki.platform.contracts.base import utcnow
from muteki.platform.contracts.events import EventEnvelope
from muteki.platform.store import PlatformStore

from . import events as ev
from .approval_queue import (
    clear_approvals,
    expire_approvals,
    hydrate_approvals,
    remove_approval,
    upsert_approval,
)
from .models import (
    TURN_COMPLETED,
    TURN_FAILED,
    TURN_INTERRUPTED,
    TURN_QUEUED,
    TURN_RUNNING,
    TURN_SUPERSEDED,
    ConversationMessage,
    ThreadAgentTreeSnapshot,
    ThreadPlanSnapshot,
    ThreadState,
)
from .agents import unsupported_agent_tree, upsert_agent_tree
from .plan import unsupported_snapshot, upsert_plan
from .store import ConversationStore


def _queue_update(
    state: ThreadState,
    *,
    payload: dict[str, Any] | None = None,
    resolve_id: str | None = None,
    expire_reason: str | None = None,
    clear: bool = False,
) -> dict[str, Any]:
    """Sync pending_approvals map + compat pending_approval singleton."""
    queue = hydrate_approvals(state.pending_approvals, state.pending_approval)
    if clear:
        queue, primary = clear_approvals()
    elif expire_reason is not None:
        queue, primary = expire_approvals(queue, reason=expire_reason)
    elif resolve_id is not None:
        queue, primary = remove_approval(queue, resolve_id)
    elif payload is not None:
        queue, primary = upsert_approval(queue, payload)
    else:
        from .approval_queue import primary_approval
        primary = primary_approval(queue)
    return {
        "pending_approvals": queue,
        "pending_approval": primary,
    }


def _user_message_id(turn_id: str) -> str:
    return f"msg-user-{turn_id}"


def _assistant_message_id(turn_id: str) -> str:
    return f"msg-asst-{turn_id}"


def _error_payload(value: Any) -> dict[str, Any]:
    """Normalize Runtime error payloads for the Conversation read model."""
    if isinstance(value, dict):
        return dict(value)
    if value in (None, ""):
        return {}
    return {"message": str(value)}


def _pos_int(v: Any) -> int | None:
    """Return a positive int or None."""
    try:
        n = int(v)
        return n if n > 0 else None
    except (TypeError, ValueError):
        return None


def _extract_context_window(
    payload: dict[str, Any],
    native_type: str,
    occurred_at: str,
    current: dict[str, Any] | None,
) -> dict[str, Any] | None:
    """C27: Parse context-window fuel-gauge fields from a usage-updated payload.

    Returns an updated ``context_window`` dict or ``None`` when the payload
    contains no relevant context-window information.  Never derives occupancy
    from cumulative token sums — ``limit=None`` stays ``None`` when unknown.
    """
    usage = payload.get("usage") or {}
    # Codex reports modelContextWindow in the structured usage event.
    limit = _pos_int(usage.get("model_context_window") or usage.get("context_window_tokens"))
    # Bug-fix (High): only accept explicit window-fill fields — never cumulative
    # input_tokens, which is a Codex session total, not current window occupancy.
    total = _pos_int(usage.get("tokens_used") or usage.get("context_used"))

    # ACP capacity/occupancy events carry context-only data.
    # Bug-fix (High): ACP nests used/size under the `usage` sub-dict, not top-level.
    if native_type == "acp.usage_update":
        cap = _pos_int(usage.get("size") or usage.get("capacity") or payload.get("capacity"))
        occ = _pos_int(usage.get("used") or usage.get("occupancy") or payload.get("occupancy") or payload.get("used"))
        if cap is not None:
            limit = cap
        if occ is not None:
            total = occ

    if limit is None and total is None:
        return None

    prev = current or {}
    return {
        "total": total if total is not None else prev.get("total"),
        "limit": limit if limit is not None else prev.get("limit"),
        "zones": prev.get("zones", []),
        "compacted": prev.get("compacted", []),
        "compact_status": prev.get("compact_status", "idle"),
        "updated_at": occurred_at,
        "source": "usage_event",
    }



def _thread_turn_still_active(
    conv: ConversationStore,
    thread_id: str,
    state: ThreadState,
) -> bool:
    """True when claim or ThreadState still points at a non-terminal turn."""
    claim_id = conv.active_turn_id(thread_id)
    for turn_id in (claim_id, state.running_turn_id):
        if not turn_id:
            continue
        turn = conv.get_turn(str(turn_id))
        if turn is None:
            # Claim without a row still blocks control-plane wipe (#122).
            if claim_id and str(turn_id) == str(claim_id):
                return True
            continue
        if turn.status in (TURN_QUEUED, TURN_RUNNING):
            return True
    return False


class ConversationProjection:
    """Thread 事件流到 Conversation 读模型的投影器。"""

    def __init__(self, store: PlatformStore, conv: ConversationStore, *, sessions_root=None) -> None:
        self._store = store
        self._conv = conv
        from muteki.core.usage import UsageStore
        from pathlib import Path
        self.usage = UsageStore(sessions_root or Path(store.db_path).parent.parent)

    # -- 单事件应用 -------------------------------------------------------------

    def apply(self, event: EventEnvelope) -> None:
        """把一条 Thread 流事件应用到读模型（幂等：重放不产生重复消息）。"""
        if event.aggregate_type != ev.AGGREGATE_THREAD:
            return
        thread_id = event.aggregate_id
        state = self._conv.get_state(thread_id)
        update: dict[str, Any] = {
            "head_stream_seq": max(state.head_stream_seq, event.stream_seq),
            "last_active_at": utcnow(),
        }
        p = event.payload
        etype = event.event_type

        # 标题/摘要属于导航元数据：自动生成或操作者手动重命名都不应
        # 仅因元数据变化把已读 Thread 重新标为未读。
        if etype in (
            ev.EV_THREAD_METADATA_UPDATED,
            ev.EV_THREAD_RENAME_REQUESTED,
            ev.EV_THREAD_RENAMED,
        ):
            update["head_stream_seq"] = state.head_stream_seq

        if etype == ev.EV_THREAD_CREATED:
            update["status"] = "active"
        elif etype == ev.EV_THREAD_ARCHIVED:
            update["status"] = "archived"
            update["archived_at"] = event.occurred_at
            update["running_turn_id"] = None
        elif etype == ev.EV_THREAD_UNARCHIVED:
            # Visibility restore only - do not resume queue or open a session.
            update["status"] = "active"
            update["archived_at"] = None
        elif etype == ev.EV_TURN_REQUESTED:
            turn_id = str(p.get("turn_id") or "")
            text = str(p.get("text") or "")
            update["last_turn_seq"] = max(
                state.last_turn_seq, int(p.get("seq") or 0))
            if turn_id and (text or p.get("attachments") or p.get("capability_refs")):
                msg_id = _user_message_id(turn_id)
                existed = self._conv.get_message(msg_id) is not None
                self._conv.save_message(ConversationMessage(
                    message_id=msg_id,
                    thread_id=thread_id,
                    turn_id=turn_id,
                    role="user",
                    text=text,
                    stream_seq=event.stream_seq,
                ))
                if not existed:
                    update["message_count"] = state.message_count + 1
                update["last_message_preview"] = text[:200]
        elif etype == ev.EV_TURN_STEERED:
            turn_id = str(p.get("turn_id") or "")
            text = str(p.get("text") or "")
            client_message_id = str(p.get("client_message_id") or event.event_id)
            if turn_id and text:
                msg_id = f"msg-steer-{client_message_id}"
                existed = self._conv.get_message(msg_id) is not None
                self._conv.save_message(ConversationMessage(
                    message_id=msg_id,
                    thread_id=thread_id,
                    turn_id=turn_id,
                    role="user",
                    kind="steer",
                    text=text,
                    stream_seq=event.stream_seq,
                ))
                if not existed:
                    update["message_count"] = state.message_count + 1
                update["last_message_preview"] = text[:200]
        elif etype in {
            ev.EV_TURN_RETRIED,
            ev.EV_TURN_EDIT_RESENT,
            ev.EV_TURN_REWOUND,
        }:
            superseded_ids = {
                str(value) for value in p.get("superseded_turn_ids") or []
                if str(value)
            }
            for superseded_id in superseded_ids:
                old_turn = self._conv.get_turn(superseded_id)
                if old_turn is None or old_turn.thread_id != thread_id:
                    continue
                self._conv.save_turn(old_turn.model_copy(update={
                    "status": TURN_SUPERSEDED,
                    "completed_at": old_turn.completed_at or event.occurred_at,
                }))
                if old_turn.run_id:
                    old_run = self._conv.get_run(old_turn.run_id)
                    if old_run is not None:
                        self._conv.save_run(old_run.model_copy(update={
                            "status": TURN_SUPERSEDED,
                            "ended_at": old_run.ended_at or event.occurred_at,
                        }))
                self._conv.release_active_turn(thread_id, superseded_id)
            retained = self._conv.list_current_messages(thread_id)
            update.update({
                "running_turn_id": None,
                "current_generation": int(
                    p.get("generation") or state.current_generation
                ),
                **_queue_update(state, expire_reason="回合已切换，待审请求已过期"),
                "pending_user_input": None,
                "last_error": {},
                "usage": {},
                "message_count": len(retained),
                "last_message_preview": retained[-1].text[:200] if retained else "",
            })
        elif etype == ev.EV_TURN_STARTED:
            update["running_turn_id"] = str(p.get("turn_id") or "") or None
            update["last_error"] = {}
            turn = self._conv.get_turn(str(p.get("turn_id") or ""))
            if turn is not None and isinstance(p.get("runtime"), dict):
                runtime = {key: str(value) for key, value in p["runtime"].items() if key in
                           {"adapter_id", "instance_id", "credential_id", "model", "effort", "access_mode"} and value is not None}
                self._conv.save_turn(turn.model_copy(update={"runtime_snapshot": runtime}))
        elif etype in (ev.EV_TURN_COMPLETED, ev.EV_TURN_FAILED,
                       ev.EV_TURN_INTERRUPTED):
            incoming_turn = str(p.get("turn_id") or "")
            if (
                not incoming_turn
                or not state.running_turn_id
                or state.running_turn_id == incoming_turn
            ):
                update["running_turn_id"] = None
            # Turn 结束后把未决审批标为 expired，避免误点旧请求。
            update.update(_queue_update(
                state, expire_reason="回合已结束，待审请求已过期"))
            update["pending_user_input"] = None
            if etype == ev.EV_TURN_FAILED:
                update["last_error"] = _error_payload(p.get("error") or p)
            elif etype == ev.EV_TURN_COMPLETED:
                update["last_error"] = {}
        elif etype == ev.EV_MESSAGE_COMPLETED:
            turn_id = str(p.get("turn_id") or "")
            text = str(p.get("text") or "")
            role = str(p.get("role") or "assistant").lower()
            if role not in {"assistant", "agent"} or not text.strip():
                self._conv.save_state(state.model_copy(update=update))
                return
            self._save_assistant_message(
                thread_id, turn_id, text, event.stream_seq, state, update)
        elif etype == ev.EV_MESSAGE_DELTA:
            role = str(p.get("role") or "assistant").lower()
            if role in {"assistant", "agent"} and not p.get("thinking"):
                self._save_assistant_message(
                    thread_id,
                    str(p.get("turn_id") or ""),
                    str(p.get("text") or ""),
                    event.stream_seq,
                    state,
                    update,
                    append=True,
                )
        elif etype == ev.EV_APPROVAL_REQUESTED:
            # C23 queue upsert; soft-link Plan dock awaiting_decision (#23) without
            # owning Plan tab identity.
            queue_patch = _queue_update(state, payload=dict(p))
            update.update(queue_patch)
            plan = state.plan
            if plan is not None and plan.phase not in {"cleared", "unsupported"}:
                approval_id = str(
                    (queue_patch.get("pending_approval") or {}).get("approval_id")
                    or p.get("approval_id")
                    or p.get("request_id")
                    or ""
                )
                update["plan"] = plan.model_copy(update={
                    "phase": "awaiting_decision",
                    "awaiting": {
                        "kind": "approval",
                        "request_id": approval_id,
                        "summary": str(
                            p.get("summary")
                            or p.get("title")
                            or p.get("tool")
                            or p.get("reason")
                            or "等待审批"
                        ),
                    },
                    "last_change_summary": "进入等待决策（审批）",
                })
        elif etype in {"core.approval.resolve_requested", "core.approval.delivery_failed"}:
            resolve_id = str(p.get("approval_id") or "")
            queue = hydrate_approvals(state.pending_approvals, state.pending_approval)
            row = queue.get(resolve_id)
            if row is not None and (etype == "core.approval.resolve_requested" or row.get("command_id") == event.command_id):
                row = {**row, "status": "resolving" if etype == "core.approval.resolve_requested" else "pending",
                       "command_id": event.command_id}
                update.update(_queue_update(state, payload=row))
        elif etype == ev.EV_APPROVAL_RESOLVED:
            resolve_id = str(
                p.get("approval_id") or p.get("request_id") or ""
            ).strip()
            queue_patch = _queue_update(state, resolve_id=resolve_id)
            update.update(queue_patch)
            plan = state.plan
            # Only leave awaiting_decision when no actionable approvals remain.
            remaining = queue_patch.get("pending_approvals") or {}
            still_pending = any(
                str((row or {}).get("status") or "pending") == "pending"
                for row in remaining.values()
            )
            if (
                plan is not None
                and plan.phase == "awaiting_decision"
                and not still_pending
            ):
                next_phase = "executing"
                if plan.tasks and all(
                    task.status in {"completed", "cancelled"} for task in plan.tasks
                ):
                    next_phase = "completed"
                update["plan"] = plan.model_copy(update={
                    "phase": next_phase,
                    "awaiting": None,
                    "last_change_summary": "审批已解决",
                })
        elif etype == ev.EV_USER_INPUT_REQUESTED:
            update["pending_user_input"] = dict(p)
            plan = state.plan
            if plan is not None and plan.phase not in {"cleared", "unsupported"}:
                update["plan"] = plan.model_copy(update={
                    "phase": "awaiting_decision",
                    "awaiting": {
                        "kind": "user_input",
                        "request_id": str(p.get("request_id") or ""),
                        "summary": str(
                            p.get("prompt")
                            or p.get("question")
                            or p.get("summary")
                            or "等待用户输入"
                        ),
                    },
                    "last_change_summary": "进入等待决策（用户输入）",
                })
        elif etype in {"core.user_input.responding", "core.user_input.delivery_failed"}:
            pending = dict(state.pending_user_input or {})
            if pending.get("request_id") == p.get("request_id") and (
                    etype == "core.user_input.responding" or pending.get("command_id") == event.command_id):
                update["pending_user_input"] = {**pending,
                    "status": "resolving" if etype == "core.user_input.responding" else "pending",
                    "command_id": event.command_id}
        elif etype == ev.EV_USER_INPUT_RESOLVED:
            pending_id = str((state.pending_user_input or {}).get("request_id") or "")
            if pending_id and pending_id != str(p.get("request_id") or ""):
                # Late delivery acknowledgements never clear a newer question.
                self._conv.save_state(state.model_copy(update=update))
                return
            update["pending_user_input"] = None
            plan = state.plan
            if plan is not None and plan.phase == "awaiting_decision":
                next_phase = "executing"
                if plan.tasks and all(
                    task.status in {"completed", "cancelled"} for task in plan.tasks
                ):
                    next_phase = "completed"
                update["plan"] = plan.model_copy(update={
                    "phase": next_phase,
                    "awaiting": None,
                    "last_change_summary": "用户输入已解决",
                })
        elif etype == ev.EV_PLAN_UPDATED:
            patch = bool(p.get("patch") or p.get("delta"))
            next_plan = upsert_plan(state.plan, dict(p), patch=patch)
            if next_plan is not None:
                update["plan"] = next_plan
        elif etype == ev.EV_PLAN_CLEARED:
            reason = str(p.get("unsupported_reason") or p.get("reason") or "")
            if p.get("unsupported") or str(p.get("phase") or "") == "unsupported":
                update["plan"] = unsupported_snapshot(
                    reason or "当前 Runtime 不支持结构化计划事件",
                    previous=state.plan,
                )
            else:
                update["plan"] = ThreadPlanSnapshot(
                    revision=max(1, int(state.plan.revision if state.plan else 0) + 1),
                    phase="cleared",
                    source="none",
                    tasks=[],
                    last_change_summary=reason or "计划已清除",
                )
        elif etype == ev.EV_AGENT_UPDATED:
            patch = bool(p.get("patch") or p.get("delta"))
            update["agents"] = upsert_agent_tree(state.agents, dict(p), patch=patch)
        elif etype == ev.EV_AGENT_CLEARED:
            reason = str(p.get("unsupported_reason") or p.get("reason") or "")
            if p.get("unsupported") or str(p.get("phase") or "") == "unsupported":
                update["agents"] = unsupported_agent_tree(
                    reason or "当前 Runtime 未上报委派 Agent 事件",
                    previous=state.agents,
                    activity=str(p.get("tool_activity_summary") or "") or None,
                )
            else:
                update["agents"] = ThreadAgentTreeSnapshot(
                    revision=max(1, int(state.agents.revision if state.agents else 0) + 1),
                    source="none",
                    agents=[],
                    tool_activity_summary=str(
                        p.get("tool_activity_summary") or ""
                    ).strip() or None,
                )
        elif etype == ev.EV_USAGE_UPDATED:
            raw = dict(p.get("usage") or {})
            runtime = p.get("runtime") or {}
            native = str(p.get("usage_native_type") or "")
            session = str(p.get("agent_session_id") or thread_id)
            # Codex structured events carry session cumulative totals. CLI events
            # carry invocation totals. Other adapters identify messages explicitly.
            key = (session if native == "thread/tokenUsage/updated" else
                   str(p.get("usage_id") or (f"{session}:message:{p['usage_message_id']}" if "usage_message_id" in p else None) or p.get("turn_id") or event.event_id))
            if native == "acp.usage_update":
                # Capacity/occupancy is not consumption; keep token fields missing
                # so aggregate/UI can show 未上报 instead of a fake 0.
                raw = {"source": "context-only"}
            context = dict(thread_id=thread_id, turn_id=p.get("turn_id"),
                           workspace_kind="conversation", actor_kind="worker", role="assistant",
                           model=runtime.get("model"), engine=runtime.get("adapter_id"))
            if native == "thread/tokenUsage/updated":
                self.usage.cumulative(raw, series=f"conversation:{thread_id}:{session}",
                                      seq=event.stream_seq, at=event.occurred_at.timestamp(), **context)
            else:
                self.usage.record(raw, identity=f"conversation:{thread_id}:{key}",
                                  seq=event.stream_seq, at=event.occurred_at.timestamp(), **context)
            update["usage"] = self.usage.query(thread_id=thread_id)["totals"]
            # C27: Extract context-window fuel gauge from usage events.
            # Adapters may report model_context_window (Codex) or ACP capacity
            # fields; we never derive occupancy from cumulative token sums.
            cw_updated = _extract_context_window(p, native, event.occurred_at.isoformat(), state.context_window)
            if cw_updated is not None:
                update["context_window"] = cw_updated
        elif etype == ev.EV_CONTEXT_WINDOW:
            # C27: explicit context-window event (adapter or manual compaction).
            prev = state.context_window or {}
            update["context_window"] = {
                "total": _pos_int(p.get("total")) if p.get("total") is not None else prev.get("total"),
                "limit": _pos_int(p.get("limit")) if p.get("limit") is not None else prev.get("limit"),
                "zones": p.get("zones") if isinstance(p.get("zones"), list) else prev.get("zones", []),
                "compacted": p.get("compacted") if isinstance(p.get("compacted"), list) else prev.get("compacted", []),
                "compact_status": str(p.get("compact_status") or prev.get("compact_status") or "idle"),
                "updated_at": event.occurred_at.isoformat(),
                "source": str(p.get("source") or "event"),
            }
        elif etype in (ev.EV_RUNTIME_ERROR, ev.EV_SESSION_ERROR):
            update["last_error"] = dict(p)
        elif etype in (ev.EV_SESSION_STARTED, ev.EV_SESSION_RESUMED):
            update["agent_session_id"] = str(p.get("agent_session_id") or "") or None
        elif etype == ev.EV_SESSION_CLOSED:
            # #122: do not wipe running turn / HITL while a turn claim is still
            # live. Session close may race capability probes or cross-runtime
            # handoff; clearing running_turn_id here made stop/steer/approval
            # fail while the Runtime kept executing.
            closed_id = str(p.get("agent_session_id") or "").strip()
            still_active = _thread_turn_still_active(self._conv, thread_id, state)
            matches_pointer = (
                not closed_id
                or not state.agent_session_id
                or state.agent_session_id == closed_id
            )
            if still_active:
                if matches_pointer:
                    update["agent_session_id"] = None
                # Keep running_turn_id, session_runtime_key, pending approvals
                # and pending_user_input so the user retains control.
            else:
                update["running_turn_id"] = None
                keep_resume = bool(p.get("resume_available")) and p.get("reason") == "operator_stopped"
                if matches_pointer and not keep_resume:
                    update["agent_session_id"] = None
                if not keep_resume:
                    update["session_runtime_key"] = ""
                update.update(_queue_update(
                    state, expire_reason="Runtime 会话已关闭，待审请求已过期"))
                update["pending_user_input"] = None
        elif etype == ev.EV_RUNTIME_SWITCHED:
            update["current_generation"] = int(
                p.get("generation") or state.current_generation)
            update["session_runtime_key"] = str(p.get("runtime_key") or "")
            update["agent_session_id"] = str(p.get("agent_session_id") or "") or None
            if state.plan is not None and state.plan.phase not in {
                "cleared", "unsupported",
            }:
                # Keep last snapshot visible but mark continuity risk until the
                # new Runtime emits its own plan events (coordinate with #16).
                update["plan"] = state.plan.model_copy(update={
                    "last_change_summary": "Runtime 已切换；等待新的计划事件",
                })
        elif etype == ev.EV_RUNTIME_CAPABILITIES_UPDATED:
            caps = p.get("capabilities") if isinstance(p.get("capabilities"), dict) else {}
            supports_plan = caps.get("plan")
            if supports_plan is False and (
                state.plan is None
                or state.plan.phase in {"cleared", "unsupported"}
                or state.plan.source in {"none", ""}
            ):
                update["plan"] = unsupported_snapshot(
                    str(
                        p.get("unsupported_reason")
                        or "当前 Runtime 未声明结构化计划能力"
                    ),
                    previous=state.plan,
                )

        self._conv.save_state(state.model_copy(update=update))

        # Turn 状态同步（事件是唯一事实来源；TurnRecord 跟随事件推进）。
        turn_id = str(p.get("turn_id") or "")
        if turn_id and etype == ev.EV_USAGE_UPDATED:
            turn = self._conv.get_turn(turn_id)
            if turn is not None:
                self._conv.save_turn(turn.model_copy(
                    update={"usage": self.usage.query(thread_id=thread_id, turn_id=turn_id)["totals"]}))
        if turn_id and etype in (
            ev.EV_TURN_STARTED, ev.EV_TURN_COMPLETED,
            ev.EV_TURN_FAILED, ev.EV_TURN_INTERRUPTED,
        ):
            turn = self._conv.get_turn(turn_id)
            if turn is not None:
                status = {
                    ev.EV_TURN_STARTED: TURN_RUNNING,
                    ev.EV_TURN_COMPLETED: TURN_COMPLETED,
                    ev.EV_TURN_FAILED: TURN_FAILED,
                    ev.EV_TURN_INTERRUPTED: TURN_INTERRUPTED,
                }[etype]
                turn_update: dict[str, Any] = {"status": status}
                if status in (TURN_COMPLETED, TURN_FAILED, TURN_INTERRUPTED):
                    turn_update["completed_at"] = event.occurred_at
                if etype == ev.EV_TURN_FAILED:
                    turn_update["error"] = _error_payload(p.get("error") or p)
                self._conv.save_turn(turn.model_copy(update=turn_update))
                if turn.run_id:
                    run = self._conv.get_run(turn.run_id)
                    if run is not None:
                        run_update: dict[str, Any] = {"status": status}
                        if status in (
                            TURN_COMPLETED, TURN_FAILED, TURN_INTERRUPTED,
                        ):
                            run_update["ended_at"] = event.occurred_at
                        self._conv.save_run(run.model_copy(update=run_update))
                if status in (TURN_COMPLETED, TURN_FAILED, TURN_INTERRUPTED):
                    self._conv.release_active_turn(thread_id, turn_id)
                if status in (TURN_FAILED, TURN_INTERRUPTED):
                    self.ensure_assistant_from_deltas(thread_id, turn_id)

    def _save_assistant_message(
        self,
        thread_id: str,
        turn_id: str,
        text: str,
        stream_seq: int,
        state: ThreadState,
        update: dict[str, Any],
        *,
        append: bool = False,
    ) -> None:
        """写入或追加该 Turn 的助手消息（中断时也要保留已产生的正文）。"""
        if not turn_id:
            return
        text = str(text or "")
        msg_id = _assistant_message_id(turn_id)
        existing = self._conv.get_message(msg_id)
        if append:
            if not text:
                return
            if existing is not None:
                text = f"{existing.text}{text}"
        if not text.strip():
            return
        existed = existing is not None
        self._conv.save_message(ConversationMessage(
            message_id=msg_id,
            thread_id=thread_id,
            turn_id=turn_id,
            role="assistant",
            text=text,
            stream_seq=stream_seq,
        ))
        if not existed:
            update["message_count"] = state.message_count + 1
        update["last_message_preview"] = text[:200]

    def _collect_assistant_delta_text(self, thread_id: str, turn_id: str) -> str:
        if not thread_id or not turn_id:
            return ""
        parts: list[str] = []
        for event in self._store.read_events(
            ev.AGGREGATE_THREAD, thread_id, limit=10000,
        ):
            if event.event_type != ev.EV_MESSAGE_DELTA:
                continue
            payload = event.payload or {}
            if str(payload.get("turn_id") or "") != turn_id:
                continue
            if payload.get("thinking"):
                continue
            role = str(payload.get("role") or "assistant").lower()
            if role not in {"assistant", "agent"}:
                continue
            chunk = str(payload.get("text") or "")
            if chunk:
                parts.append(chunk)
        return "".join(parts)

    def ensure_assistant_from_deltas(self, thread_id: str, turn_id: str) -> None:
        """中断/失败后：若还没有助手消息，用已落库的 delta 拼出一条。"""
        if not thread_id or not turn_id:
            return
        if self._conv.get_message(_assistant_message_id(turn_id)) is not None:
            return
        text = self._collect_assistant_delta_text(thread_id, turn_id)
        if not text.strip():
            return
        state = self._conv.get_state(thread_id)
        update: dict[str, Any] = {}
        self._save_assistant_message(
            thread_id, turn_id, text, state.head_stream_seq, state, update)
        if update:
            self._conv.save_state(state.model_copy(update=update))

    # -- 重建 / 已读 -----------------------------------------------------------

    def rebuild_thread(self, thread_id: str) -> int:
        """从事件流重放重建该 Thread 的读模型，返回应用的条数。"""
        state = self._conv.get_state(thread_id)
        events = self._store.read_events(
            ev.AGGREGATE_THREAD, thread_id,
            after_seq=state.head_stream_seq, limit=10000)
        for event in events:
            self.apply(event)
        return len(events)

    def rebuild_all(self) -> int:
        """重启后重建所有 Thread 读模型（只追落后水位的事件）。"""
        total = 0
        for state in self._conv.list_states():
            total += self.rebuild_thread(state.thread_id)
        # 新建后尚未产生状态的 Thread 也覆盖到。
        from muteki.platform.contracts.objects import Thread

        for thread in self._store.list(Thread):
            if self._conv.get_state(thread.thread_id).head_stream_seq == 0:
                total += self.rebuild_thread(thread.thread_id)
        return total

    def mark_read(self, thread_id: str) -> ThreadState:
        """用户查看 Thread：已读水位推进到当前流头（清除 unread）。"""
        state = self._conv.get_state(thread_id)
        return self._conv.save_state(state.model_copy(
            update={"read_stream_seq": state.head_stream_seq}))


__all__ = ["ConversationProjection"]
