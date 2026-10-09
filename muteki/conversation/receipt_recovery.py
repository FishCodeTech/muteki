"""Startup settlement of conversation command receipts left ACCEPTED by a crash.

``MutekiCommandApiImpl.dispatch`` commits the request events and an ACCEPTED
receipt, then runs the side effect, journals its result and finalizes the
receipt. A process that dies inside that window leaves the receipt ACCEPTED
forever: callers polling it never see a terminal state.

``ConversationReceiptRecovery.settle`` finalizes every such conversation
receipt exactly once through ``finalize_accepted``:

1. A journaled effect result wins: it is what the side effect produced.
2. Otherwise the per-command-type settler in ``SETTLERS`` derives the outcome
   from committed state (Thread/Workspace rows, Turn status, queue rows,
   memory graph, event log). A settler only reports success that committed
   state proves; when it cannot, the receipt fails with
   ``conversation.command.interrupted_by_restart`` and
   ``delivery_unknown`` / ``outcome_unknown`` say whether the effect may
   have reached the Runtime or been partially applied.

Every conversation command type must have a settler (checked at import), so
a new command type cannot silently stay ACCEPTED after a restart.
"""

from __future__ import annotations

import logging
from collections import Counter
from dataclasses import dataclass, replace
from pathlib import Path
from typing import Any, Awaitable, Callable, Optional

from muteki.platform.command_handlers.base import (
    CommandAPIError,
    SideEffectResult,
    make_error,
)
from muteki.platform.contracts.capabilities import CapabilityBinding
from muteki.platform.contracts.errors import ErrorCategory
from muteki.platform.contracts.events import EventEnvelope
from muteki.platform.contracts.objects import Task
from muteki.platform.contracts.receipts import CommandReceipt, ReceiptState
from muteki.platform.store import PlatformStore

from . import events as ev
from .commands import COMMAND_TYPES
from .manager import WS_MODE_NEW, ConversationManager
from .models import (
    TURN_COMPLETED,
    TURN_FAILED,
    TURN_INTERRUPTED,
    TURN_CANCELLED,
    TURN_SUPERSEDED,
)
from .store import ConversationStore
from .subagents import (
    ACTIVE_STATUSES,
    CANCEL_COMMAND,
    ERR_RECEIPT_UNFINALIZED,
    ERR_SPAWN_RECORD_INVALID,
    SPAWN_COMMAND,
    SubagentService,
)

LOG = logging.getLogger(__name__)

ERR_INTERRUPTED_BY_RESTART = "conversation.command.interrupted_by_restart"
ERR_RECOVERY_UNSUPPORTED = "conversation.command.recovery_unsupported"
ERR_RECEIPT_STATE_INVALID = "conversation.command.receipt_state_invalid"
ERR_SETTLE_FAILED = "conversation.command.settle_failed"
ERR_API_UNBOUND = "conversation.command.api_unbound"

CONVERSATION_PREFIX = "conversation."
_PENDING_STATES = (
    ReceiptState.ACCEPTED.value, ReceiptState.RUNNING.value, ReceiptState.WAITING.value,
)
_TERMINAL_TURN_STATUSES = frozenset({
    TURN_COMPLETED, TURN_FAILED, TURN_INTERRUPTED, TURN_CANCELLED, TURN_SUPERSEDED,
})
_MSG_NOT_APPLIED = "服务重启时该命令尚未完成，没有产生效果，可以重新发起"
_MSG_DELIVERY_UNKNOWN = "服务重启时该命令正在投递给 Runtime，结果未确认，请先核对当前对话状态"
_MSG_OUTCOME_UNKNOWN = "服务重启时该命令正在执行，结果未确认，请先核对当前状态"


@dataclass(frozen=True)
class PendingCommand:
    """One ACCEPTED receipt plus every committed event stamped with its command_id."""

    command_id: str
    command_type: str
    aggregate_type: str
    aggregate_id: str
    idempotency_key: Optional[str]
    receipt: CommandReceipt
    #: (global seq, event) in commit order: request events, and any events
    #: later work stamped with the same command_id (e.g. queue promotion).
    events: tuple[tuple[int, EventEnvelope], ...]

    def first(self, event_type: str) -> Optional[tuple[int, EventEnvelope]]:
        return next((item for item in self.events if item[1].event_type == event_type), None)

    def all(self, event_type: str) -> list[EventEnvelope]:
        return [event for _seq, event in self.events if event.event_type == event_type]

    @property
    def correlation_id(self) -> str:
        for _seq, event in self.events:
            if event.correlation_id:
                return event.correlation_id
        return self.command_id

    @property
    def actor_id(self) -> str:
        return self.events[0][1].actor_id if self.events else "system"


class _LeaveAccepted(Exception):
    """The receipt has a dedicated owner that will settle it; report, do not finalize."""

    def __init__(self, code: str, message: str) -> None:
        super().__init__(message)
        self.code = code


class _MissingRequestEvent(LookupError):
    pass


Settler = Callable[["ConversationReceiptRecovery", PendingCommand], Awaitable[SideEffectResult]]


class ConversationReceiptRecovery:
    """Settles ACCEPTED conversation receipts from committed state (startup only)."""

    def __init__(
        self,
        store: PlatformStore,
        conv: ConversationStore,
        manager: ConversationManager,
        subagents: SubagentService,
    ) -> None:
        self._store = store
        self._conv = conv
        self._manager = manager
        self._subagents = subagents

    # -- entry -------------------------------------------------------------------

    async def settle(self, api: Any) -> dict[str, Any]:
        report: dict[str, Any] = {
            "settled": {},
            "receipts": [],
            "errors": [],
            "unknown_types": [],
            "in_flight": [],
            "other_domains_pending": {},
        }
        settled: dict[str, Counter[str]] = {}
        other: Counter[str] = Counter()
        for row in self._pending_rows():
            command_type = str(row["command_type"])
            command_id = str(row["command_id"])
            if not command_type.startswith(CONVERSATION_PREFIX):
                other[command_type] += 1
                continue
            if row["state"] != ReceiptState.ACCEPTED.value:
                # No conversation handler produces RUNNING / WAITING receipts;
                # finalize_accepted cannot settle them and guessing would lie.
                report["errors"].append(self._error_entry(
                    command_id, command_type, ERR_RECEIPT_STATE_INVALID,
                    f"conversation receipt is {row['state']}, only accepted receipts are settled"))
                continue
            if api is None:
                report["errors"].append(self._error_entry(
                    command_id, command_type, ERR_API_UNBOUND,
                    "ConversationService.register was not called; receipts cannot be finalized"))
                continue
            if api.effect_in_flight(command_id):
                report["in_flight"].append({"command_id": command_id, "command_type": command_type})
                continue
            try:
                receipt, source = await self._settle_one(api, row, report)
            except _LeaveAccepted as exc:
                report["errors"].append(self._error_entry(command_id, command_type, exc.code, str(exc)))
                continue
            except Exception as exc:  # noqa: BLE001 - recorded per receipt; one bad row must not stop the rest
                LOG.exception("conversation receipt settlement failed: %s %s", command_type, command_id)
                code = exc.error.code if isinstance(exc, CommandAPIError) else ERR_SETTLE_FAILED
                report["errors"].append(self._error_entry(
                    command_id, command_type, code, f"{type(exc).__name__}: {exc}"))
                continue
            if receipt.state is ReceiptState.ACCEPTED:
                report["errors"].append(self._error_entry(
                    command_id, command_type, ERR_SETTLE_FAILED,
                    "receipt is still accepted after settlement"))
                continue
            settled.setdefault(command_type, Counter())[receipt.state.value] += 1
            detail = dict(receipt.error.detail) if receipt.error is not None else {}
            report["receipts"].append({
                "command_id": command_id,
                "command_type": command_type,
                "state": receipt.state.value,
                "source": source,
                "error_code": receipt.error.code if receipt.error is not None else None,
                "delivery_unknown": bool(detail.get("delivery_unknown")),
                "outcome_unknown": bool(detail.get("outcome_unknown")),
                "deduplicated": receipt.deduplicated,
            })
        report["settled"] = {key: dict(value) for key, value in sorted(settled.items())}
        report["other_domains_pending"] = dict(sorted(other.items()))
        return report

    async def _settle_one(
        self, api: Any, row: Any, report: dict[str, Any],
    ) -> tuple[CommandReceipt, str]:
        command_id = str(row["command_id"])
        if self._store.effect_result(command_id) is not None:
            # get_receipt replays the journaled result exactly like dispatch.
            return await api.get_receipt(command_id), "journal"
        receipt = CommandReceipt.model_validate_json(row["payload"])
        # The receipt aggregate is what the plan created or targeted (a new
        # Project / Thread / fork id); the row keeps the request's aggregate_id,
        # which is empty for create commands and the source Thread for fork.
        aggregate = receipt.aggregate
        pending = PendingCommand(
            command_id=command_id,
            command_type=str(row["command_type"]),
            aggregate_type=aggregate.type if aggregate else str(row["aggregate_type"]),
            aggregate_id=aggregate.id if aggregate else str(row["aggregate_id"]),
            idempotency_key=row["idempotency_key"],
            receipt=receipt,
            events=tuple(self._command_events(command_id)),
        )
        settler = SETTLERS.get(pending.command_type)
        if settler is None:
            report["unknown_types"].append(
                {"command_id": command_id, "command_type": pending.command_type})
            result = self._failure(
                pending, code=ERR_RECOVERY_UNSUPPORTED, outcome_unknown=True,
                message=f"没有 {pending.command_type} 的重启恢复规则，结果未确认",
                category=ErrorCategory.INTERNAL)
        else:
            try:
                result = await settler(self, pending)
            except _MissingRequestEvent as exc:
                result = self._failure(
                    pending, code=ERR_RECEIPT_STATE_INVALID, outcome_unknown=True,
                    message=f"回执缺少已提交的请求事件 {exc}，结果未确认",
                    category=ErrorCategory.INTERNAL)
        result = replace(result, output={**result.output, "recovered": True})
        return api.finalize_accepted(command_id, result), "derived"

    # -- results -------------------------------------------------------------------

    @staticmethod
    def _error_entry(command_id: str, command_type: str, code: str, message: str) -> dict[str, Any]:
        return {"command_id": command_id, "command_type": command_type,
                "code": code, "message": message}

    @staticmethod
    def _completed(output: Optional[dict[str, Any]] = None,
                   events: Optional[list[EventEnvelope]] = None) -> SideEffectResult:
        return SideEffectResult(output=dict(output or {}), events=list(events or []))

    @staticmethod
    def _failure(
        pending: PendingCommand,
        *,
        message: str,
        delivery_unknown: bool = False,
        outcome_unknown: bool = False,
        code: str = ERR_INTERRUPTED_BY_RESTART,
        category: ErrorCategory = ErrorCategory.INTERNAL,
        detail: Optional[dict[str, Any]] = None,
        output: Optional[dict[str, Any]] = None,
    ) -> SideEffectResult:
        unknown = delivery_unknown or outcome_unknown
        error = make_error(
            code, message, category,
            correlation_id=pending.correlation_id,
            # A blind retry is only safe when the effect is known not to have
            # happened; otherwise the caller must reconcile first.
            retryable=not unknown,
            recovery_hint=("核对当前状态后再决定是否重新发起，不要直接重复提交"
                           if unknown else "服务已重启，可以重新发起该操作"),
        ).model_copy(update={"detail": {
            "command_id": pending.command_id,
            "command_type": pending.command_type,
            "phase": "process_restart",
            "delivery_unknown": delivery_unknown,
            "outcome_unknown": outcome_unknown,
            **(detail or {}),
        }})
        return SideEffectResult(error=error, state=ReceiptState.FAILED, output=dict(output or {}))

    def _not_applied(self, pending: PendingCommand, **detail: Any) -> SideEffectResult:
        return self._failure(pending, message=_MSG_NOT_APPLIED, detail=detail)

    def _delivery_unknown(self, pending: PendingCommand, **detail: Any) -> SideEffectResult:
        return self._failure(pending, message=_MSG_DELIVERY_UNKNOWN,
                             delivery_unknown=True, detail=detail)

    def _outcome_unknown(self, pending: PendingCommand, **detail: Any) -> SideEffectResult:
        return self._failure(pending, message=_MSG_OUTCOME_UNKNOWN,
                             outcome_unknown=True, detail=detail)

    @staticmethod
    def _result_event(pending: PendingCommand, thread_id: str, event_type: str,
                      payload: dict[str, Any], *, idempotency_key: Optional[str] = None,
                      ) -> EventEnvelope:
        return ev.thread_event(
            thread_id, event_type, payload, actor_id=pending.actor_id,
            command_id=pending.command_id, correlation_id=pending.correlation_id,
            idempotency_key=idempotency_key)

    @staticmethod
    def _request(pending: PendingCommand, event_type: str) -> tuple[int, EventEnvelope]:
        found = pending.first(event_type)
        if found is None:
            raise _MissingRequestEvent(event_type)
        return found

    # -- committed-state reads -----------------------------------------------------

    def _pending_rows(self) -> list[Any]:
        placeholders = ", ".join("?" for _ in _PENDING_STATES)
        with self._store.lock:
            return list(self._store.conn.execute(
                "SELECT command_id, command_type, aggregate_type, aggregate_id, "
                "idempotency_key, state, payload FROM command_receipts "
                f"WHERE state IN ({placeholders}) ORDER BY created_at, rowid",  # noqa: S608
                _PENDING_STATES,
            ).fetchall())

    def _decode(self, payload: str) -> EventEnvelope:
        return self._store.upcasters.upcast(EventEnvelope.model_validate_json(payload))

    def _command_events(self, command_id: str) -> list[tuple[int, EventEnvelope]]:
        with self._store.lock:
            rows = list(self._store.conn.execute(
                "SELECT seq, payload FROM domain_events WHERE command_id = ? ORDER BY seq",
                (command_id,),
            ).fetchall())
        return [(int(row["seq"]), self._decode(row["payload"])) for row in rows]

    def _thread_event(
        self, thread_id: str, event_type: str, key: str, value: str, *, after_seq: int = 0,
        exclude_command_id: str = "",
    ) -> Optional[EventEnvelope]:
        with self._store.lock:
            rows = list(self._store.conn.execute(
                "SELECT payload FROM domain_events WHERE aggregate_type = ? "
                "AND aggregate_id = ? AND event_type = ? AND seq > ? "
                "AND json_extract(payload, '$.payload.' || ?) = ? ORDER BY seq",
                (ev.AGGREGATE_THREAD, thread_id, event_type, int(after_seq), key, value),
            ).fetchall())
        for row in rows:
            event = self._decode(row["payload"])
            if exclude_command_id and event.command_id == exclude_command_id:
                continue
            return event
        return None

    def _turn_started(self, thread_id: str, turn_id: str) -> bool:
        return self._thread_event(thread_id, ev.EV_TURN_STARTED, "turn_id", turn_id) is not None

    def _running_turn_before(self, thread_id: str, seq: int) -> Optional[str]:
        """``running_turn_id`` as the projection held it just before global ``seq``."""
        ending = (ev.EV_TURN_COMPLETED, ev.EV_TURN_FAILED, ev.EV_TURN_INTERRUPTED)
        clearing = (ev.EV_THREAD_ARCHIVED, ev.EV_TURN_RETRIED, ev.EV_TURN_EDIT_RESENT,
                    ev.EV_TURN_REWOUND)
        types = (ev.EV_TURN_STARTED, *ending, *clearing)
        placeholders = ", ".join("?" for _ in types)
        with self._store.lock:
            rows = list(self._store.conn.execute(
                "SELECT payload FROM domain_events WHERE aggregate_type = ? AND aggregate_id = ? "
                f"AND seq < ? AND event_type IN ({placeholders}) ORDER BY seq",  # noqa: S608
                (ev.AGGREGATE_THREAD, thread_id, int(seq), *types),
            ).fetchall())
        running: Optional[str] = None
        for row in rows:
            event = self._decode(row["payload"])
            turn_id = str(event.payload.get("turn_id") or "")
            if event.event_type == ev.EV_TURN_STARTED:
                running = turn_id or None
            elif event.event_type in clearing:
                running = None
            elif not turn_id or running is None or running == turn_id:
                running = None
        return running

    # -- settlers: Project / Workspace ---------------------------------------------

    async def _project_create(self, pending: PendingCommand) -> SideEffectResult:
        self._request(pending, ev.EV_PROJECT_CREATED)
        bound = pending.first(ev.EV_WORKSPACE_BOUND)
        project = self._manager.get_project(pending.aggregate_id)
        workspace_id = str(bound[1].payload.get("workspace_id") or "") if bound else ""
        workspace = self._manager.get_workspace(workspace_id) if workspace_id else None
        if project is not None and (not workspace_id or workspace is not None):
            return self._completed({"workspace_id": workspace.workspace_id,
                                    "root_path": workspace.root_path} if workspace else {})
        if project is None and workspace is None:
            return self._not_applied(pending, project_saved=False)
        return self._outcome_unknown(pending, project_saved=project is not None,
                                     workspace_saved=workspace is not None)

    async def _project_update(self, pending: PendingCommand) -> SideEffectResult:
        self._request(pending, ev.EV_PROJECT_UPDATED)
        # The settings patch is not in any committed event, so whether the
        # saved Project already carries it cannot be told apart.
        return self._outcome_unknown(pending, project_id=pending.aggregate_id)

    async def _workspace_bind(self, pending: PendingCommand) -> SideEffectResult:
        _seq, bound = self._request(pending, ev.EV_WORKSPACE_BOUND)
        payload = bound.payload
        workspace = self._manager.get_workspace(str(payload.get("workspace_id") or ""))
        if workspace is not None:
            return self._completed({
                "workspace_id": workspace.workspace_id,
                "root_path": workspace.root_path,
                "kind": workspace.kind,
                "settings": dict(workspace.settings or {}),
            })
        root = str(payload.get("root_path") or "")
        if str(payload.get("mode") or "") == WS_MODE_NEW and root and Path(root).exists():
            # ``git worktree add`` ran but the Workspace row was never saved.
            return self._outcome_unknown(pending, workspace_saved=False,
                                         worktree_on_disk=True, root_path=root)
        return self._not_applied(pending, workspace_saved=False)

    async def _workspace_delete_worktree(self, pending: PendingCommand) -> SideEffectResult:
        _seq, changed = self._request(pending, ev.EV_WORKSPACE_CHANGED)
        root = str(changed.payload.get("root_path") or "")
        if not root:
            return self._outcome_unknown(pending, reason="request event has no root_path")
        if not Path(root).exists():
            return self._completed({"workspace_id": pending.aggregate_id, "removed": True,
                                    "root_path": root})
        return self._not_applied(pending, worktree_on_disk=True, root_path=root)

    # -- settlers: Thread ------------------------------------------------------------

    async def _thread_create(self, pending: PendingCommand) -> SideEffectResult:
        _seq, created = self._request(pending, ev.EV_THREAD_CREATED)
        thread_id = pending.aggregate_id
        thread = self._manager.get_thread(thread_id)
        bindings = self._store.list(CapabilityBinding, thread_id=thread_id)
        selection_needed = bool(created.payload.get("runtime"))
        selection = self._conv.get_runtime_selection(thread_id)
        if thread is not None and bindings and (not selection_needed or selection is not None):
            return self._completed()
        if thread is None:
            return self._not_applied(pending, thread_saved=False)
        return self._outcome_unknown(pending, thread_saved=True, binding_issued=bool(bindings),
                                     runtime_selection_saved=selection is not None)

    async def _thread_fork(self, pending: PendingCommand) -> SideEffectResult:
        _seq, forked = self._request(pending, ev.EV_THREAD_FORKED)
        thread_id = pending.aggregate_id
        if self._manager.get_thread(thread_id) is None:
            return self._not_applied(pending, forked_thread_saved=False)
        if forked.payload.get("from_turn_id") and any(
                task.kind == "conversation.fork"
                for task in self._store.list(Task, thread_id=thread_id)):
            # The fork Task is the last write of complete_fork.
            return self._completed()
        return self._outcome_unknown(pending, forked_thread_saved=True,
                                     forked_thread_id=thread_id)

    async def _thread_resume(self, pending: PendingCommand) -> SideEffectResult:
        _seq, resumed = self._request(pending, ev.EV_THREAD_RESUMED)
        return self._turn_start_outcome(pending, str(resumed.payload.get("turn_id") or ""), {})

    async def _turn_retry(self, pending: PendingCommand) -> SideEffectResult:
        found = pending.first(ev.EV_TURN_RETRIED) or pending.first(ev.EV_TURN_EDIT_RESENT)
        if found is None:
            raise _MissingRequestEvent(f"{ev.EV_TURN_RETRIED}|{ev.EV_TURN_EDIT_RESENT}")
        payload = found[1].payload
        output = {"impact": {
            "mode": "edit_resend" if payload.get("edited") else "retry",
            "superseded_turn_ids": list(payload.get("superseded_turn_ids") or []),
            "workspace_policy": payload.get("workspace_policy"),
            "external_side_effects": payload.get("external_side_effects"),
        }}
        return self._turn_start_outcome(pending, str(payload.get("turn_id") or ""), output)

    def _turn_start_outcome(self, pending: PendingCommand, turn_id: str,
                            output: dict[str, Any]) -> SideEffectResult:
        """Side effects whose only remote step is ``executor.start_turn``."""
        thread_id = pending.aggregate_id
        if not turn_id:
            return self._outcome_unknown(pending, reason="request event has no turn_id")
        if self._turn_started(thread_id, turn_id):
            return self._completed(output)
        turn = self._conv.get_turn(turn_id)
        # start_turn may have handed the prompt to the Runtime before the
        # Runtime reported ``turn.started``.
        return self._failure(
            pending, message=_MSG_DELIVERY_UNKNOWN, delivery_unknown=True, output=output,
            detail={"turn_id": turn_id, "turn_status": turn.status if turn else None})

    async def _turn_native_rewind(self, pending: PendingCommand) -> SideEffectResult:
        _seq, requested = self._request(pending, ev.EV_RUNTIME_OPERATION_REQUESTED)
        thread_id = pending.aggregate_id
        turn_id = str(requested.payload.get("turn_id") or "")
        target = self._conv.get_turn(turn_id) if turn_id else None
        if target is None:
            return self._outcome_unknown(pending, turn_id=turn_id, reason="rewind target is missing")
        if target.status == TURN_SUPERSEDED:
            # Commands are refused while a history mutation runs, so the target
            # can only have been superseded by this rewind's local commit.
            superseded = [turn.turn_id for turn in self._conv.list_turns(thread_id)
                          if turn.seq >= target.seq and turn.status == TURN_SUPERSEDED]
            payload = {"turn_id": turn_id, "superseded_turn_ids": superseded, "applied": True,
                       "external_side_effects": "cannot_undo"}
            # The event is native_rewind_turn's idempotency record and lets a
            # projection rebuild reproduce the supersede.
            return self._completed(
                {"impact": {"mode": "native_rewind", **payload}},
                [self._result_event(pending, thread_id, ev.EV_TURN_REWOUND, payload,
                                    idempotency_key=pending.idempotency_key or pending.command_id)])
        state = self._conv.get_state(thread_id)
        if state.history_recovery_required:
            return self._delivery_unknown(pending, turn_id=turn_id, history_recovery_required=True)
        return self._not_applied(pending, turn_id=turn_id)

    async def _thread_archive(self, pending: PendingCommand) -> SideEffectResult:
        _seq, requested = self._request(pending, "core.thread.archive_requested")
        thread_id = pending.aggregate_id
        bindings = self._store.list(CapabilityBinding, thread_id=thread_id)
        if bindings and all(binding.revoked_at is not None for binding in bindings):
            # archive_thread (binding revocation) is the last local step; the
            # session close and queue pause ran before it.
            return self._completed(events=[self._result_event(
                pending, thread_id, ev.EV_THREAD_ARCHIVED,
                {"thread_id": thread_id, "noop": bool(requested.payload.get("noop"))})])
        return self._failure(
            pending, message="服务重启时归档尚未完成，对话仍未归档，可以重新归档",
            detail={"bindings_revoked": False})

    async def _thread_rename(self, pending: PendingCommand) -> SideEffectResult:
        self._request(pending, ev.EV_THREAD_RENAME_REQUESTED)
        # The requested title is not in any committed event.
        return self._outcome_unknown(pending, thread_id=pending.aggregate_id)

    # -- settlers: Turn / queue ------------------------------------------------------

    async def _turn_send(self, pending: PendingCommand) -> SideEffectResult:
        if pending.first(ev.EV_RUNTIME_OPERATION_REQUESTED) is not None:
            return self._delivery_unknown(pending, runtime_operation=True)
        _seq, added = self._request(pending, ev.EV_QUEUE_ADDED)
        queue_id = str(added.payload.get("queue_id") or "")
        item = self._conv.get_queue_item(queue_id) if queue_id else None
        if item is None:
            return self._outcome_unknown(pending, queue_id=queue_id, reason="queue item is missing")
        # The message was durably queued in the accept transaction; startup
        # recovery re-schedules queue dispatch, so the command's effect holds.
        return self._completed({"queue_id": queue_id, "queued": True,
                                "queue_status": item.status})

    async def _turn_steer(self, pending: PendingCommand) -> SideEffectResult:
        _seq, requested = self._request(pending, "core.turn.steer_requested")
        return self._delivery_unknown(pending, turn_id=requested.payload.get("turn_id"))

    async def _queue_steer(self, pending: PendingCommand) -> SideEffectResult:
        seq, requested = self._request(pending, "core.queue.steer_requested")
        thread_id = pending.aggregate_id
        queue_id = str(requested.payload.get("queue_id") or "")
        turn_id = str(requested.payload.get("turn_id") or "")
        item = self._conv.get_queue_item(queue_id) if queue_id else None
        deleted_elsewhere = self._thread_event(
            thread_id, ev.EV_QUEUE_DELETED, "queue_id", queue_id, after_seq=seq,
            exclude_command_id=pending.command_id) is not None
        if item is not None and item.status == "cancelled" and not deleted_elsewhere:
            # The item is cancelled only after the Runtime acknowledged the steer.
            client_message_id = item.client_message_id or item.queue_id
            return self._completed(events=[
                self._result_event(pending, thread_id, ev.EV_TURN_STEERED, {
                    "turn_id": turn_id, "text": item.text,
                    "client_message_id": client_message_id, "queue_id": queue_id}),
                self._result_event(pending, thread_id, ev.EV_QUEUE_DELETED, {
                    "queue_id": queue_id, "reason": "steered",
                    "queue_revision": self._conv.get_state(thread_id).queue_revision}),
            ])
        return self._delivery_unknown(pending, queue_id=queue_id, turn_id=turn_id,
                                      queue_status=item.status if item else None)

    async def _turn_interrupt(self, pending: PendingCommand) -> SideEffectResult:
        _seq, requested = self._request(pending, "core.turn.interrupt_requested")
        turn_id = str(requested.payload.get("turn_id") or "")
        turn = self._conv.get_turn(turn_id) if turn_id else None
        if turn is not None and turn.status in _TERMINAL_TURN_STATUSES:
            # The command's goal is a stopped Turn; startup recovery marks every
            # orphan Turn interrupted before receipts are settled.
            return self._completed({"turn_id": turn_id, "turn_status": turn.status})
        return self._delivery_unknown(pending, turn_id=turn_id,
                                      turn_status=turn.status if turn else None)

    async def _queue_resume(self, pending: PendingCommand) -> SideEffectResult:
        self._request(pending, ev.EV_QUEUE_RESUMED)
        # resume_queue committed with the accept; the side effect only kicks
        # dispatch, which startup recovery schedules again.
        state = self._conv.get_state(pending.aggregate_id)
        return self._completed({"queue_paused": state.queue_paused})

    # -- settlers: artifacts / interactions -------------------------------------------

    async def _artifact_attach(self, pending: PendingCommand) -> SideEffectResult:
        self._request(pending, "core.artifact.attach_requested")
        # The content digest is not in any committed event.
        return self._outcome_unknown(pending, thread_id=pending.aggregate_id)

    async def _approval_resolve(self, pending: PendingCommand) -> SideEffectResult:
        seq, requested = self._request(pending, "core.approval.resolve_requested")
        approval_id = str(requested.payload.get("approval_id") or "")
        resolved = self._thread_event(
            pending.aggregate_id, ev.EV_APPROVAL_RESOLVED, "approval_id", approval_id,
            after_seq=seq)
        if resolved is not None:
            return self._completed({"approval_id": approval_id,
                                    "resolution_event_id": resolved.event_id})
        return self._delivery_unknown(pending, approval_id=approval_id)

    async def _user_input_resolve(self, pending: PendingCommand) -> SideEffectResult:
        seq, requested = self._request(pending, "core.user_input.responding")
        request_id = str(requested.payload.get("request_id") or "")
        resolved = self._thread_event(
            pending.aggregate_id, ev.EV_USER_INPUT_RESOLVED, "request_id", request_id,
            after_seq=seq)
        if resolved is not None:
            return self._completed({"request_id": request_id,
                                    "resolution_event_id": resolved.event_id})
        return self._delivery_unknown(pending, request_id=request_id)

    async def _user_input_inject(self, pending: PendingCommand) -> SideEffectResult:
        self._request(pending, ev.EV_USER_INPUT_REQUESTED)
        # The fixture registration lives only in the exited process's memory.
        return self._failure(
            pending, message="服务重启后用户输入夹具已失效，需要重新注入",
            detail={"fixture_registered": False})

    async def _memory_record(self, pending: PendingCommand) -> SideEffectResult:
        _seq, requested = self._request(pending, "core.memory.record_requested")
        memory_id = str(requested.payload.get("memory_id") or "")
        snapshot = await self._manager.memory_snapshot(pending.aggregate_id, include_deleted=True)
        item = next((row for row in snapshot.get("memories", [])
                     if row.get("memory_id") == memory_id), None)
        tombstone = next((row for row in snapshot.get("tombstones", [])
                          if row.get("memory_id") == memory_id), None)
        if item is None and tombstone is None:
            return self._not_applied(pending, memory_id=memory_id)
        # A tombstone alone means the memory was recorded and deleted since.
        recorded = {"memory_id": memory_id, **({"kind": item["kind"]} if item else {})}
        return self._completed(
            {"memory_id": memory_id,
             "graph_stream_seq": item["updated_seq"] if item else None},
            [self._result_event(pending, pending.aggregate_id, "core.memory.recorded", recorded)])

    async def _memory_delete(self, pending: PendingCommand) -> SideEffectResult:
        _seq, requested = self._request(pending, "core.memory.delete_requested")
        memory_id = str(requested.payload.get("memory_id") or "")
        snapshot = await self._manager.memory_snapshot(pending.aggregate_id, include_deleted=True)
        tombstone = next((row for row in snapshot.get("tombstones", [])
                          if row.get("memory_id") == memory_id), None)
        if tombstone is None:
            return self._not_applied(pending, memory_id=memory_id)
        return self._completed(
            {"memory_id": memory_id, "graph_stream_seq": tombstone["deleted_seq"]},
            [self._result_event(pending, pending.aggregate_id, "core.memory.deleted",
                                {"memory_id": memory_id})])

    async def _plan_amend(self, pending: PendingCommand) -> SideEffectResult:
        seq, updated = self._request(pending, ev.EV_PLAN_UPDATED)
        overlay = dict(updated.payload.get("pending_amendment") or {})
        running = self._running_turn_before(pending.aggregate_id, seq)
        if running is None:
            # Without a running Turn the side effect only returns the overlay.
            return self._completed({"pending_amendment": overlay, "steered": False})
        return self._failure(
            pending, message=_MSG_DELIVERY_UNKNOWN, delivery_unknown=True,
            detail={"turn_id": running}, output={"pending_amendment": overlay})

    # -- settlers: subagents ------------------------------------------------------------

    async def _subagent_spawn(self, pending: PendingCommand) -> SideEffectResult:
        _seq, spawned = self._request(pending, ev.EV_SUBAGENT_SPAWNED)
        subagent_id = str(spawned.payload.get("subagent_id") or "")
        _records, invalid = self._subagents._spawn_records()
        broken = next((row for row in invalid if row.get("subagent_id") == subagent_id), None)
        if broken is not None:
            return self._failure(
                pending, code=ERR_SPAWN_RECORD_INVALID, outcome_unknown=True,
                message=f"子 Agent {subagent_id} 的派生记录无效，结果未确认",
                detail={"subagent_id": subagent_id, "reason": broken.get("message")})
        # SubagentService.recover owns spawn receipts; it retries at next start.
        raise _LeaveAccepted(
            ERR_RECEIPT_UNFINALIZED,
            f"subagent recovery did not finalize the spawn receipt of {subagent_id}")

    async def _subagent_cancel(self, pending: PendingCommand) -> SideEffectResult:
        requests = pending.all(ev.EV_SUBAGENT_CANCEL_REQUESTED)
        if not requests:
            raise _MissingRequestEvent(ev.EV_SUBAGENT_CANCEL_REQUESTED)
        target = next((str(item.payload.get("subagent_id") or "") for item in requests
                       if item.payload.get("cascade_from") is None), "")
        reason = str(requests[0].payload.get("reason") or "")
        outcomes = []
        for item in requests:
            subagent_id = str(item.payload.get("subagent_id") or "")
            outcomes.append({"subagent_id": subagent_id,
                             "status": self._subagents.live_status(subagent_id).status})
        active = [row["subagent_id"] for row in outcomes if row["status"] in ACTIVE_STATUSES]
        output = {"subagent_id": target, "reason": reason, "outcomes": outcomes}
        if not active:
            return self._completed(output)
        return self._failure(
            pending, message="服务重启时取消尚未完成，仍有子 Agent 在运行，可以重新取消",
            detail={"active_subagent_ids": active}, output=output)

    # -- settlers: commands without a side effect --------------------------------------

    async def _no_side_effect(self, pending: PendingCommand) -> SideEffectResult:
        # These commands finalize inside the accept transaction, so an
        # ACCEPTED receipt means the receipt row itself is inconsistent.
        return self._failure(
            pending, code=ERR_RECEIPT_STATE_INVALID, outcome_unknown=True,
            message=f"{pending.command_type} 不应停留在 accepted，结果未确认",
            detail={"reason": "command type finalizes inside the accept transaction"})


R = ConversationReceiptRecovery
SETTLERS: dict[str, Settler] = {
    "conversation.project.create": R._project_create,
    "conversation.project.update": R._project_update,
    "conversation.workspace.bind": R._workspace_bind,
    "conversation.workspace.delete_worktree": R._workspace_delete_worktree,
    "conversation.thread.create": R._thread_create,
    "conversation.thread.rename": R._thread_rename,
    "conversation.thread.resume": R._thread_resume,
    "conversation.turn.resume": R._thread_resume,
    "conversation.thread.quota_resume": R._no_side_effect,
    "conversation.thread.fork": R._thread_fork,
    "conversation.thread.archive": R._thread_archive,
    "conversation.thread.unarchive": R._no_side_effect,
    "conversation.turn.send": R._turn_send,
    "conversation.turn.retry": R._turn_retry,
    "conversation.turn.edit_resend": R._turn_retry,
    "conversation.turn.native_rewind": R._turn_native_rewind,
    "conversation.turn.steer": R._turn_steer,
    "conversation.turn.interrupt": R._turn_interrupt,
    "conversation.queue.update": R._no_side_effect,
    "conversation.queue.delete": R._no_side_effect,
    "conversation.queue.reorder": R._no_side_effect,
    "conversation.queue.pause": R._no_side_effect,
    "conversation.queue.resume": R._queue_resume,
    "conversation.queue.steer": R._queue_steer,
    "conversation.artifact.attach": R._artifact_attach,
    "conversation.approval.resolve": R._approval_resolve,
    "conversation.approval.inject": R._no_side_effect,
    "conversation.user_input.resolve": R._user_input_resolve,
    "conversation.user_input.inject": R._user_input_inject,
    "conversation.memory.record": R._memory_record,
    "conversation.memory.delete": R._memory_delete,
    "conversation.plan.inject": R._no_side_effect,
    "conversation.plan.amend": R._plan_amend,
    "conversation.agents.inject": R._no_side_effect,
    SPAWN_COMMAND: R._subagent_spawn,
    CANCEL_COMMAND: R._subagent_cancel,
}
del R

_MISSING = (set(COMMAND_TYPES) | {SPAWN_COMMAND, CANCEL_COMMAND}) - set(SETTLERS)
if _MISSING:
    raise RuntimeError(f"conversation command types without a restart settler: {sorted(_MISSING)}")


__all__ = [
    "ERR_API_UNBOUND",
    "ERR_INTERRUPTED_BY_RESTART",
    "ERR_RECEIPT_STATE_INVALID",
    "ERR_RECOVERY_UNSUPPORTED",
    "ERR_SETTLE_FAILED",
    "ConversationReceiptRecovery",
    "SETTLERS",
]
