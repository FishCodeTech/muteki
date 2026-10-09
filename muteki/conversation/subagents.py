"""Muteki-owned subagents: child conversation Threads spawned by a parent Agent.

A parent conversation Agent reaches these commands through the capability
gateway (``muteki_spawn_subagent`` / ``muteki_subagent_status`` /
``muteki_subagent_cancel``). The gateway sets the command aggregate to the
Thread that owns the calling Binding, so an Agent can only act on its own
Thread and its descendants.

Each child is an ordinary conversation Thread:

- ``conversation.subagent.spawn`` commits the child ``core.thread.created``,
  its ``core.thread.lineage_set`` and the parent's ``core.subagent.spawned``
  in one transaction, then (side effect) activates the Thread, which issues
  the child's own CapabilityBinding, and sends the prompt through
  ``conversation.turn.send``. Archiving the child revokes that Binding like
  any other Thread.
- Child status is derived from the child's Turns and queue; changes are
  mirrored onto the parent stream as ``core.subagent.updated`` by the
  append-events observer installed by ``ConversationService``.
- Cancelling cascades to descendants; interrupting a parent Turn or
  archiving a parent Thread cancels its still-active children.
"""

from __future__ import annotations

import asyncio
import logging
import os
import time
from dataclasses import dataclass, replace
from typing import Any, Awaitable, Callable, Optional

from muteki.external_agents.descriptors import UnknownProviderError, get_descriptor
from muteki.external_agents.factory import engine_for_adapter
from muteki.platform.command_handlers.base import (
    OPERATOR_ACTOR_KINDS,
    CommandAPIError,
    CommandFailed,
    CommandPlan,
    HandlerContext,
    SideEffectResult,
    correlation_id_of,
    make_error,
)
from muteki.platform.contracts.base import new_id
from muteki.platform.contracts.commands import ActorRef, CommandEnvelope, QueryResult
from muteki.platform.contracts.errors import ErrorCategory, ErrorEnvelope
from muteki.platform.contracts.events import EventEnvelope
from muteki.platform.contracts.external_agents import ACCESS_MODE_VALUES, AccessMode
from muteki.platform.contracts.objects import Thread, Workspace
from muteki.platform.contracts.receipts import CommandReceipt, ReceiptState
from muteki.platform.store import PlatformStore

from . import events as ev
from .commands import _failed, _receipt, _reject_paused_runtime
from .manager import WORKSPACE_GIT, WS_MODE_NEW, ConversationError, ConversationManager
from .models import (
    SUBAGENT_ORIGIN_APP,
    TURN_COMPLETED,
    TURN_FAILED,
    TURN_INTERRUPTED,
    TURN_CANCELLED,
    TURN_QUEUED,
    TURN_RUNNING,
    ConversationAgentNode,
    ThreadLineage,
    ThreadRuntimeSelection,
)
from .projections import _assistant_message_id
from .store import ConversationStore

LOG = logging.getLogger(__name__)

SPAWN_COMMAND = "conversation.subagent.spawn"
CANCEL_COMMAND = "conversation.subagent.cancel"
STATUS_QUERY = "conversation.subagent.status"

ERR_DEPTH_EXCEEDED = "conversation.subagent.depth_exceeded"
ERR_CONCURRENCY_EXCEEDED = "conversation.subagent.concurrency_exceeded"
ERR_NOT_FOUND = "conversation.subagent.not_found"
ERR_ACCESS_EXCEEDS_PARENT = "conversation.subagent.access_exceeds_parent"
ERR_CALLER_UNKNOWN = "conversation.subagent.caller_unknown"
ERR_CALLER_MISMATCH = "conversation.subagent.caller_mismatch"
ERR_MODE_INVALID = "conversation.subagent.mode_invalid"
ERR_PARENT_ARCHIVED = "conversation.subagent.parent_archived"
ERR_PARENT_CANCELLED = "conversation.subagent.parent_cancelled"
ERR_ARGUMENT_INVALID = "conversation.subagent.argument_invalid"
ERR_TIMEOUT_INVALID = "conversation.subagent.timeout_invalid"
ERR_RUNTIME_INVALID = "conversation.subagent.runtime_invalid"
ERR_WORKTREE_UNAVAILABLE = "conversation.subagent.worktree_unavailable"
ERR_WORKTREE_FAILED = "conversation.subagent.worktree_failed"
ERR_ACTIVATION_FAILED = "conversation.subagent.activation_failed"
ERR_DISPATCH_FAILED = "conversation.subagent.dispatch_failed"
ERR_CANCEL_INCOMPLETE = "conversation.subagent.cancel_incomplete"
ERR_ORCHESTRATOR_UNBOUND = "conversation.subagent.orchestrator_unbound"
ERR_SPAWN_RECORD_INVALID = "conversation.subagent.spawn_record_invalid"
ERR_RECEIPT_UNFINALIZED = "conversation.subagent.receipt_unfinalized"

STATUS_PENDING = "pending"
STATUS_RUNNING = "running"
STATUS_COMPLETED = "completed"
STATUS_FAILED = "failed"
STATUS_CANCELLED = "cancelled"
STATUS_INTERRUPTED = "interrupted"
ACTIVE_STATUSES = frozenset({STATUS_PENDING, STATUS_RUNNING})
TERMINAL_STATUSES = frozenset({
    STATUS_COMPLETED, STATUS_FAILED, STATUS_CANCELLED, STATUS_INTERRUPTED,
})

ISOLATION_SHARED = "shared"
ISOLATION_WORKTREE = "worktree"

#: Durable spawn side-effect progress recorded on the parent node (see
#: ``ConversationAgentNode.spawn_state``). Recovery only resumes nodes whose
#: spawn_state names a step before ``dispatched``.
SPAWN_STATE_PLANNED = "planned"
SPAWN_STATE_WORKSPACE_READY = "workspace_ready"
SPAWN_STATE_ACTIVATED = "activated"
SPAWN_STATE_DISPATCHED = "dispatched"
SPAWN_STATE_SKIPPED = "skipped"
SPAWN_STATE_FAILED = "failed"
_PENDING_SPAWN_STATES = frozenset({
    SPAWN_STATE_PLANNED, SPAWN_STATE_WORKSPACE_READY, SPAWN_STATE_ACTIVATED,
})
#: Error category ``run_spawn`` reports for each spawn failure code.
_SPAWN_FAILURE_CATEGORIES = {
    ERR_WORKTREE_FAILED: ErrorCategory.STATE,
    ERR_ACTIVATION_FAILED: ErrorCategory.INTERNAL,
    ERR_DISPATCH_FAILED: ErrorCategory.STATE,
}

#: AccessMode declaration order is the autonomy order (least to most).
_ACCESS_RANK = {mode.value: index for index, mode in enumerate(AccessMode)}

#: Nested commands are issued by the orchestrator itself, never by the Agent.
ORCHESTRATOR_ACTOR = ActorRef(kind="system", id="subagent-orchestrator")

#: Waiters re-read the child state at least this often even without a wake-up,
#: so an observer notification lost across threads cannot stall a bounded wait.
_WAIT_RECHECK_S = 5.0

_CHILD_EVENTS = frozenset({
    ev.EV_TURN_STARTED,
    ev.EV_TURN_COMPLETED,
    ev.EV_TURN_FAILED,
    ev.EV_TURN_INTERRUPTED,
    ev.EV_QUEUE_DISPATCH_FAILED,
    ev.EV_THREAD_ARCHIVED,
})
_PARENT_EVENTS = frozenset({ev.EV_TURN_INTERRUPTED, ev.EV_THREAD_ARCHIVED})
#: The running Turn finished between reading the state and the interrupt.
_TURN_ALREADY_ENDED_CODES = frozenset({
    "conversation.turn.not_running", "conversation.turn.expected_mismatch",
})


def _env_number(name: str, default: float) -> float:
    raw = os.environ.get(name)
    if raw is None or not raw.strip():
        return default
    try:
        return float(raw)
    except ValueError as exc:
        raise ValueError(f"{name} must be a number, got {raw!r}") from exc


@dataclass(frozen=True)
class SubagentLimits:
    """Orchestration limits (root Thread depth is 0)."""

    max_depth: int = 2
    #: pending + running descendants allowed under one root Thread.
    max_running_per_root: int = 4
    #: ``wait=true`` without ``timeout_seconds``. MCP clients enforce their
    #: own per-call timeouts (often about a minute), so the default stays
    #: below that and longer waits are an explicit caller choice.
    default_wait_seconds: float = 45.0
    max_wait_seconds: float = 1800.0

    def __post_init__(self) -> None:
        if self.max_depth < 0 or self.max_running_per_root < 1:
            raise ValueError("subagent limits must be max_depth >= 0 and max_running_per_root >= 1")
        if not 0 < self.default_wait_seconds <= self.max_wait_seconds:
            raise ValueError("subagent wait limits must satisfy 0 < default <= max")

    @classmethod
    def from_env(cls) -> "SubagentLimits":
        base = cls()
        return cls(
            max_depth=int(_env_number("MUTEKI_SUBAGENT_MAX_DEPTH", base.max_depth)),
            max_running_per_root=int(_env_number(
                "MUTEKI_SUBAGENT_MAX_RUNNING_PER_ROOT", base.max_running_per_root)),
            default_wait_seconds=_env_number(
                "MUTEKI_SUBAGENT_WAIT_SECONDS", base.default_wait_seconds),
            max_wait_seconds=_env_number(
                "MUTEKI_SUBAGENT_MAX_WAIT_SECONDS", base.max_wait_seconds),
        )

    def describe(self) -> dict[str, Any]:
        return {
            "max_depth": self.max_depth,
            "max_running_per_root": self.max_running_per_root,
            "default_wait_seconds": self.default_wait_seconds,
            "max_wait_seconds": self.max_wait_seconds,
        }


@dataclass
class _LiveStatus:
    status: str
    child_turn_id: Optional[str] = None
    message_id: Optional[str] = None
    error_code: Optional[str] = None
    error: Optional[str] = None


@dataclass(frozen=True)
class _SpawnRecord:
    """Everything recovery needs to finish an interrupted spawn.

    Every field is derivable from committed events alone: the parent node
    payload of ``core.subagent.spawned`` and the child stream's
    ``core.thread.created`` / ``core.thread.lineage_set`` events.
    """
    subagent_id: str
    parent_thread_id: str
    spawn_command_id: str
    correlation_id: str
    principal_id: str
    title: str
    title_source: str
    prompt: str
    spawn_state: str
    isolation: str
    workspace_id: Optional[str]
    workspace_settings: dict[str, Any]
    worktree_path: Optional[str]
    runtime: dict[str, Any]
    project_id: Optional[str]
    cancel_requested: bool
    status: str


def _error(code: str, message: str, category: ErrorCategory, *,
           correlation_id: str = "", detail: Optional[dict[str, Any]] = None,
           recovery_hint: str = "") -> ErrorEnvelope:
    error = make_error(code, message, category, correlation_id=correlation_id or None,
                       recovery_hint=recovery_hint)
    if detail:
        error = error.model_copy(update={"detail": dict(detail)})
    return error


class SubagentService:
    """Lineage queries, live status, waiting, cancellation and cascades.

    Holds no business state of its own: lineage and nodes are projected from
    events; only asyncio primitives (waiters, background tasks) live here.
    """

    def __init__(
        self,
        store: PlatformStore,
        conv: ConversationStore,
        manager: ConversationManager,
        *,
        limits: Optional[SubagentLimits] = None,
    ) -> None:
        self._store = store
        self._conv = conv
        self._manager = manager
        self.limits = limits or SubagentLimits.from_env()
        self._api: Any = None
        self._loop: Optional[asyncio.AbstractEventLoop] = None
        self._waiters: dict[str, asyncio.Event] = {}
        self._tasks: set[asyncio.Task[Any]] = set()
        # Subagent ids whose spawn side effects are running on this loop.
        self._spawning: set[str] = set()
        # One pending cascade per (parent, kind) so re-triggers coalesce.
        self._cascade_tasks: dict[tuple[str, str], asyncio.Task[Any]] = {}

    # -- wiring ------------------------------------------------------------------

    def bind_command_api(self, api: Any) -> None:
        self._api = api

    def _require_api(self, correlation_id: str = "") -> Any:
        if self._api is None:
            raise CommandFailed(_error(
                ERR_ORCHESTRATOR_UNBOUND,
                "subagent orchestrator has no Command API; ConversationService.register was not called",
                ErrorCategory.INTERNAL, correlation_id=correlation_id))
        return self._api

    def _remember_loop(self) -> None:
        try:
            self._loop = asyncio.get_running_loop()
        except RuntimeError:
            pass

    def _call_soon(self, callback: Callable[[], None]) -> bool:
        try:
            loop = asyncio.get_running_loop()
        except RuntimeError:
            loop = None
        if loop is not None:
            self._loop = loop
            loop.call_soon(callback)
            return True
        if self._loop is not None and not self._loop.is_closed():
            self._loop.call_soon_threadsafe(callback)
            return True
        return False

    def _spawn_task(self, factory: Callable[[], Awaitable[Any]], label: str) -> None:
        def start() -> None:
            task = asyncio.ensure_future(factory())
            self._tasks.add(task)
            task.add_done_callback(self._task_done)

        if not self._call_soon(start):
            LOG.warning("subagent observer has no event loop; %s runs at next reconcile", label)

    def _task_done(self, task: asyncio.Task[Any]) -> None:
        self._tasks.discard(task)
        if task.cancelled():
            return
        exc = task.exception()
        if exc is not None:
            LOG.error("subagent background task failed", exc_info=exc)

    async def shutdown(self) -> None:
        tasks = list(self._tasks)
        for task in tasks:
            task.cancel()
        if tasks:
            await asyncio.gather(*tasks, return_exceptions=True)

    # -- lineage -----------------------------------------------------------------

    def lineage(self, thread_id: str) -> Optional[ThreadLineage]:
        return self._conv.get_state(thread_id).lineage if thread_id else None

    def node(self, subagent_id: str) -> Optional[ConversationAgentNode]:
        lineage = self.lineage(subagent_id)
        if lineage is None:
            return None
        parent = self._conv.get_state(lineage.parent_thread_id)
        return next((n for n in parent.subagents if n.agent_id == subagent_id), None)

    def is_descendant(self, thread_id: str, ancestor_id: str) -> bool:
        seen: set[str] = set()
        current = self.lineage(thread_id)
        while current is not None and current.parent_thread_id not in seen:
            if current.parent_thread_id == ancestor_id:
                return True
            seen.add(current.parent_thread_id)
            current = self.lineage(current.parent_thread_id)
        return False

    def descendants(self, thread_id: str) -> list[str]:
        """Breadth-first descendant ids (children before grandchildren)."""
        by_parent: dict[str, list[str]] = {}
        for state in self._conv.list_states():
            if state.lineage is not None:
                by_parent.setdefault(state.lineage.parent_thread_id, []).append(state.thread_id)
        ordered: list[str] = []
        frontier = [thread_id]
        seen = {thread_id}
        while frontier:
            next_frontier: list[str] = []
            for parent in frontier:
                for child in by_parent.get(parent, []):
                    if child not in seen:
                        seen.add(child)
                        ordered.append(child)
                        next_frontier.append(child)
            frontier = next_frontier
        return ordered

    def active_in_root(self, root_thread_id: str) -> list[str]:
        active: list[str] = []
        for state in self._conv.list_states():
            lineage = state.lineage
            if lineage is None or lineage.root_thread_id != root_thread_id:
                continue
            if self.live_status(state.thread_id).status in ACTIVE_STATUSES:
                active.append(state.thread_id)
        return active

    # -- status ------------------------------------------------------------------

    def live_status(
        self, subagent_id: str, node: Optional[ConversationAgentNode] = None,
    ) -> _LiveStatus:
        """Status from the child's own Turns and queue (the source of truth)."""
        node = node or self.node(subagent_id)
        cancel_requested = bool(node and node.cancel_requested)
        turns = self._conv.list_current_turns(subagent_id)
        last = turns[-1] if turns else None
        if last is None:
            queue = self._conv.list_queue(subagent_id)
            failed_item = next((item for item in queue if item.status == "failed"), None)
            if failed_item is not None:
                return _LiveStatus(
                    status=STATUS_FAILED,
                    error_code=str(failed_item.error.get("code") or "conversation.queue.dispatch_failed"),
                    error=str(failed_item.error.get("message") or ""),
                )
            if node is not None and node.status in TERMINAL_STATUSES:
                return _LiveStatus(status=node.status, error_code=node.error_code, error=node.error)
            if cancel_requested and not any(item.status == "dispatching" for item in queue):
                return _LiveStatus(status=STATUS_CANCELLED)
            return _LiveStatus(status=STATUS_PENDING)
        message = self._conv.get_message(_assistant_message_id(last.turn_id))
        message_id = message.message_id if message is not None else None
        if last.status == TURN_QUEUED:
            return _LiveStatus(status=STATUS_PENDING, child_turn_id=last.turn_id)
        if last.status == TURN_RUNNING:
            return _LiveStatus(status=STATUS_RUNNING, child_turn_id=last.turn_id)
        if last.status == TURN_COMPLETED:
            return _LiveStatus(status=STATUS_COMPLETED, child_turn_id=last.turn_id,
                               message_id=message_id)
        if last.status == TURN_FAILED:
            error = dict(last.error or {})
            return _LiveStatus(
                status=STATUS_FAILED, child_turn_id=last.turn_id, message_id=message_id,
                error_code=str(error.get("code") or "conversation.turn.failed"),
                error=str(error.get("message") or error.get("detail") or ""),
            )
        if last.status in {TURN_INTERRUPTED, TURN_CANCELLED}:
            return _LiveStatus(
                status=STATUS_CANCELLED if cancel_requested or last.status == TURN_CANCELLED else STATUS_INTERRUPTED,
                child_turn_id=last.turn_id, message_id=message_id,
            )
        raise ValueError(f"unexpected current turn status {last.status!r} on {subagent_id}")

    def snapshot(self, subagent_id: str, *, include_text: bool) -> dict[str, Any]:
        lineage = self.lineage(subagent_id)
        if lineage is None:
            raise LookupError(subagent_id)
        node = self.node(subagent_id)
        live = self.live_status(subagent_id, node)
        thread = self._manager.get_thread(subagent_id)
        selection = self._conv.get_runtime_selection(subagent_id)
        final: Optional[dict[str, Any]] = None
        if live.status in TERMINAL_STATUSES and live.message_id:
            message = self._conv.get_message(live.message_id)
            if message is not None:
                final = {
                    "message_id": message.message_id,
                    "thread_id": subagent_id,
                    "turn_id": message.turn_id,
                    "chars": len(message.text),
                    # failed / interrupted Turns keep whatever text was produced.
                    "complete": live.status == STATUS_COMPLETED,
                }
                if include_text:
                    final["text"] = message.text
        return {
            "subagent_id": subagent_id,
            "thread_id": subagent_id,
            "origin": SUBAGENT_ORIGIN_APP,
            "title": thread.title if thread is not None else (node.title if node else ""),
            "status": live.status,
            "child_turn_id": live.child_turn_id,
            "parent_thread_id": lineage.parent_thread_id,
            "parent_turn_id": lineage.parent_turn_id,
            "root_thread_id": lineage.root_thread_id,
            "depth": lineage.depth,
            "runtime": _runtime_view(selection),
            "isolation": lineage.isolation,
            "workspace_id": lineage.workspace_id,
            "worktree_path": lineage.worktree_path,
            "worktree_state": node.worktree_state if node is not None else None,
            "cancel_requested": bool(node and node.cancel_requested),
            "final_message": final,
            "error": ({"code": live.error_code, "message": live.error or ""}
                      if live.error_code else None),
            "child_subagent_ids": [
                item.agent_id for item in self._conv.get_state(subagent_id).subagents
            ],
        }

    def sync_node(self, subagent_id: str) -> Optional[EventEnvelope]:
        """Mirror the child's live status onto the parent node when it changed."""
        lineage = self.lineage(subagent_id)
        node = self.node(subagent_id)
        if lineage is None or node is None:
            return None
        live = self.live_status(subagent_id, node)
        patch: dict[str, Any] = {}
        for key in ("status", "child_turn_id", "message_id", "error_code", "error"):
            value = getattr(live, key)
            if key in {"error_code", "error"} and value is None and live.status in TERMINAL_STATUSES:
                continue
            if getattr(node, key) != value:
                patch[key] = value
        if not patch:
            return None
        if live.status in TERMINAL_STATUSES and node.completed_at is None:
            patch["completed_at"] = time.time()
        stored = self._store.append_events([ev.thread_event(
            lineage.parent_thread_id, ev.EV_SUBAGENT_UPDATED,
            {"subagent_id": subagent_id, **_json_safe_times(patch)},
        )])
        return stored[0]

    def reconcile(self) -> int:
        """Re-derive every app_owned node (startup, or after lost wake-ups)."""
        changed = 0
        for state in self._conv.list_states():
            if state.lineage is not None and self.sync_node(state.thread_id) is not None:
                changed += 1
        return changed

    # -- crash recovery ----------------------------------------------------------

    async def recover(self) -> dict[str, Any]:
        """Finish spawn side effects, cascades and worktree cleanups lost to a
        crash between the spawn commit and their completion.

        Every step re-derives from committed events and reuses the live path's
        idempotency keys, so work that did complete before the crash is
        recognized (deduplicated), never repeated. Per-child failures are
        recorded on the node and in the report; one bad child does not stop
        the others.
        """
        self._remember_loop()
        report: dict[str, Any] = {
            "spawns_resumed": [],
            "spawns_failed": [],
            "invalid_spawn_records": [],
            "cascades": [],
            "worktree_cleanups": [],
            "reconciled": 0,
        }
        report["receipts_finalized"] = []
        report["receipt_errors"] = []
        records, invalid = self._spawn_records()
        report["invalid_spawn_records"] = invalid
        for record in records:
            if record.subagent_id in self._spawning:
                continue
            if record.spawn_state not in _PENDING_SPAWN_STATES:
                # Progress reached a terminal mark but the process died before
                # the Command API finalized the spawn receipt.
                receipt = self._store.get_receipt(record.spawn_command_id)
                if receipt is not None and receipt.state is ReceiptState.ACCEPTED:
                    self._finalize_spawn_receipt(record, None, report)
                continue
            self._spawning.add(record.subagent_id)
            try:
                result = await self.run_spawn(record)
            except Exception as exc:  # noqa: BLE001 - typed failures return via SideEffectResult
                LOG.exception("subagent spawn recovery crashed: %s", record.subagent_id)
                report["spawns_failed"].append({
                    "subagent_id": record.subagent_id,
                    "code": "conversation.subagent.recovery_crashed",
                    "message": f"{type(exc).__name__}: {exc}",
                })
                continue
            finally:
                self._spawning.discard(record.subagent_id)
            node = self.node(record.subagent_id)
            entry = {
                "subagent_id": record.subagent_id,
                "spawn_state": node.spawn_state if node is not None else None,
                "status": node.status if node is not None else None,
                "error_code": node.error_code if node is not None else None,
            }
            if node is not None and node.spawn_state == SPAWN_STATE_FAILED:
                report["spawns_failed"].append(entry)
            else:
                report["spawns_resumed"].append(entry)
            self._finalize_spawn_receipt(record, result, report)
        report["reconciled"] = self.reconcile()

        async def guarded(step: str, coro: Awaitable[Any]) -> Any:
            # One failing cascade/cleanup must not abort the remaining recovery.
            try:
                return await coro
            except Exception as exc:  # noqa: BLE001 - recorded in the report
                LOG.exception("subagent recovery step failed: %s", step)
                report["step_errors"].append(
                    {"step": step, "message": f"{type(exc).__name__}: {exc}"})
                return None

        report["step_errors"] = []
        for state in self._conv.list_states():
            if state.subagents:
                if state.status == "archived":
                    cascade = await guarded(
                        f"cascade:{state.thread_id}",
                        self._cascade_once(state.thread_id, ev.EV_THREAD_ARCHIVED, ""))
                    if cascade is not None:
                        report["cascades"].append(cascade)
                else:
                    for turn in self._conv.list_current_turns(state.thread_id):
                        if turn.status not in {TURN_INTERRUPTED, TURN_CANCELLED}:
                            continue
                        if any(n.turn_id == turn.turn_id for n in state.subagents):
                            cascade = await guarded(
                                f"cascade:{state.thread_id}:{turn.turn_id}",
                                self._cascade_once(
                                    state.thread_id, ev.EV_TURN_INTERRUPTED, turn.turn_id))
                            if cascade is not None:
                                report["cascades"].append(cascade)
            lineage = state.lineage
            if lineage is not None and state.status == "archived" and (
                    lineage.isolation == ISOLATION_WORKTREE):
                cleanup = await guarded(
                    f"worktree-cleanup:{state.thread_id}",
                    self._cleanup_worktree(state.thread_id, f"recover:{state.thread_id}"))
                if cleanup is not None:
                    report["worktree_cleanups"].append(
                        {"subagent_id": state.thread_id, **cleanup})
        return report

    def _settled_spawn_result(self, record: _SpawnRecord) -> SideEffectResult:
        """The result ``run_spawn`` returned for a spawn whose progress is
        already terminal, rebuilt from the node."""
        node = self.node(record.subagent_id)
        if node is None:
            raise ValueError(f"subagent node {record.subagent_id} is missing")
        snapshot = self.snapshot(record.subagent_id, include_text=False)
        if node.spawn_state == SPAWN_STATE_FAILED:
            code = str(node.error_code or "")
            category = _SPAWN_FAILURE_CATEGORIES.get(code)
            if category is None:
                raise ValueError(
                    f"failed spawn {record.subagent_id} carries unknown error_code {code!r}")
            return SideEffectResult(
                error=_error(code, str(node.error or code), category,
                             correlation_id=record.correlation_id),
                state=ReceiptState.FAILED, output=snapshot)
        if node.spawn_state == SPAWN_STATE_SKIPPED:
            return SideEffectResult(output=snapshot)
        return SideEffectResult(output={**snapshot, "wait": {"requested": False}})

    def _finalize_spawn_receipt(
        self, record: _SpawnRecord, result: Optional[SideEffectResult],
        report: dict[str, Any],
    ) -> None:
        """Settle the spawn command receipt left ACCEPTED by a crashed process.

        ``result`` is what recovery's ``run_spawn`` returned; None rebuilds it
        from a node whose progress was already terminal. The original caller's
        wait cannot be honoured after a restart, so the output is marked
        ``recovered``.
        """
        try:
            if result is None:
                result = self._settled_spawn_result(record)
            result = replace(result, output={**result.output, "recovered": True})
            receipt = self._require_api(record.correlation_id).finalize_accepted(
                record.spawn_command_id, result)
        except Exception as exc:  # noqa: BLE001 - recorded in the report; receipt stays ACCEPTED
            LOG.exception("subagent spawn receipt finalization failed: %s", record.subagent_id)
            code = exc.error.code if isinstance(exc, CommandAPIError) else ERR_RECEIPT_UNFINALIZED
            report["receipt_errors"].append({
                "subagent_id": record.subagent_id,
                "command_id": record.spawn_command_id,
                "code": code,
                "message": f"{type(exc).__name__}: {exc}",
            })
            return
        report["receipts_finalized"].append({
            "subagent_id": record.subagent_id,
            "command_id": record.spawn_command_id,
            "state": receipt.state.value,
            "error_code": receipt.error.code if receipt.error is not None else None,
            "deduplicated": receipt.deduplicated,
        })

    def _spawn_records(self) -> tuple[list[_SpawnRecord], list[dict[str, Any]]]:
        """Rebuild one resumable record per app_owned node from events."""
        records: list[_SpawnRecord] = []
        invalid: list[dict[str, Any]] = []
        for state in self._conv.list_states():
            for node in state.subagents:
                if node.origin != SUBAGENT_ORIGIN_APP:
                    continue
                try:
                    records.append(self._spawn_record(state.thread_id, node))
                except ValueError as exc:
                    invalid.append({
                        "subagent_id": node.agent_id,
                        "code": ERR_SPAWN_RECORD_INVALID,
                        "message": str(exc),
                    })
        return records, invalid

    def _spawn_record(self, parent_thread_id: str, node: ConversationAgentNode) -> _SpawnRecord:
        subagent_id = node.agent_id
        lineage = self.lineage(subagent_id)
        child_stream = self._store.read_events(ev.AGGREGATE_THREAD, subagent_id, limit=50)
        created = next(
            (e for e in child_stream if e.event_type == ev.EV_THREAD_CREATED), None)
        if lineage is None or created is None:
            raise ValueError(f"child stream of {subagent_id} lacks created/lineage events")
        created_payload = dict(created.payload or {})
        spawn_command_id = str(created.command_id or "")
        prompt = str(node.request or "")
        principal_id = str(node.principal_id or "")
        if not spawn_command_id or not prompt or not principal_id:
            raise ValueError(
                f"node {subagent_id} misses spawn command id, prompt or principal_id")
        workspace_settings: dict[str, Any] = {}
        if lineage.isolation == ISOLATION_WORKTREE and lineage.workspace_id:
            bound = next(
                (e for e in self._store.read_events(
                    ev.AGGREGATE_WORKSPACE, lineage.workspace_id, limit=10)
                 if e.event_type == ev.EV_WORKSPACE_BOUND), None)
            if bound is None:
                raise ValueError(f"workspace {lineage.workspace_id} has no bound event")
            workspace_settings = dict(dict(bound.payload or {}).get("settings") or {})
        # Nodes recorded before progress tracking carry no spawn_state; their
        # side effects belong to a lost process and are never resumed.
        spawn_state = node.spawn_state or SPAWN_STATE_DISPATCHED
        return _SpawnRecord(
            subagent_id=subagent_id,
            parent_thread_id=parent_thread_id,
            spawn_command_id=spawn_command_id,
            correlation_id=str(created.correlation_id or spawn_command_id),
            principal_id=principal_id,
            title=str(created_payload.get("title") or node.title or ""),
            title_source=str(created_payload.get("title_source") or "user"),
            prompt=prompt,
            spawn_state=spawn_state,
            isolation=lineage.isolation,
            workspace_id=lineage.workspace_id,
            workspace_settings=workspace_settings,
            worktree_path=lineage.worktree_path,
            runtime=dict(created_payload.get("runtime") or {}),
            project_id=created_payload.get("project_id") or None,
            cancel_requested=node.cancel_requested,
            status=node.status,
        )

    async def run_spawn(
        self, record: _SpawnRecord, *, wait: bool = False, timeout: float = 0.0,
    ) -> SideEffectResult:
        """Spawn side effects, resumable from the recorded spawn_state.

        The live dispatch and startup recovery both run this with the same
        record content; confirmed progress is appended to the parent node
        before the next step starts, so a crash at any point resumes the
        remaining steps exactly once.
        """
        svc = self

        def fail(code: str, message: str, category: ErrorCategory, **detail: Any) -> SideEffectResult:
            patch: dict[str, Any] = {
                "status": STATUS_FAILED, "spawn_state": SPAWN_STATE_FAILED,
                "error_code": code, "error": message,
            }
            if code == ERR_WORKTREE_FAILED:
                patch["worktree_state"] = "removed"
            svc._append_node_patch(record.parent_thread_id, record.subagent_id, patch,
                                   command_id=record.spawn_command_id,
                                   correlation_id=record.correlation_id)
            if self._manager.get_thread(record.subagent_id) is None:
                # No Thread row or Binding exists for the child; archive its
                # event stream directly so it cannot linger as active.
                svc._archive_child_stream(
                    record.subagent_id, reason="spawn_failed", error_code=code,
                    command_id=record.spawn_command_id,
                    correlation_id=record.correlation_id)
            return SideEffectResult(
                error=_error(code, message, category,
                             correlation_id=record.correlation_id, detail=detail),
                state=ReceiptState.FAILED,
                output=svc.snapshot(record.subagent_id, include_text=False),
            )

        node = self.node(record.subagent_id)
        state = record.spawn_state
        if state not in _PENDING_SPAWN_STATES and state != SPAWN_STATE_DISPATCHED:
            return SideEffectResult(
                output=self.snapshot(record.subagent_id, include_text=False))
        if state in _PENDING_SPAWN_STATES and (
                self._conv.list_queue(record.subagent_id)
                or self._conv.list_current_turns(record.subagent_id)):
            # The first turn.send committed before the crash; only the
            # progress mark was lost.
            state = SPAWN_STATE_DISPATCHED
            self._append_node_patch(record.parent_thread_id, record.subagent_id,
                                    {"spawn_state": SPAWN_STATE_DISPATCHED},
                                    command_id=record.spawn_command_id,
                                    correlation_id=record.correlation_id)
        cancel_requested = bool(node and node.cancel_requested)
        if state in _PENDING_SPAWN_STATES and cancel_requested:
            self._append_node_patch(
                record.parent_thread_id, record.subagent_id,
                {"spawn_state": SPAWN_STATE_SKIPPED, "status": STATUS_CANCELLED},
                command_id=record.spawn_command_id,
                correlation_id=record.correlation_id)
            return SideEffectResult(
                output=self.snapshot(record.subagent_id, include_text=False))
        if state == SPAWN_STATE_PLANNED and record.isolation == ISOLATION_WORKTREE:
            try:
                await asyncio.to_thread(self._adopt_or_create_worktree, record)
            except ConversationError as exc:
                return fail(ERR_WORKTREE_FAILED, str(exc), ErrorCategory.STATE)
            self._append_node_patch(
                record.parent_thread_id, record.subagent_id,
                {"spawn_state": SPAWN_STATE_WORKSPACE_READY, "worktree_state": "active"},
                command_id=record.spawn_command_id,
                correlation_id=record.correlation_id)
            state = SPAWN_STATE_WORKSPACE_READY
        if state in (SPAWN_STATE_PLANNED, SPAWN_STATE_WORKSPACE_READY):
            try:
                _activate_spawn(self._manager, record)
            except Exception as exc:  # noqa: BLE001 - recorded on the node and returned typed
                LOG.exception("subagent thread activation failed: %s", record.subagent_id)
                return fail(ERR_ACTIVATION_FAILED, f"{type(exc).__name__}: {exc}",
                            ErrorCategory.INTERNAL)
            self._append_node_patch(record.parent_thread_id, record.subagent_id,
                                    {"spawn_state": SPAWN_STATE_ACTIVATED},
                                    command_id=record.spawn_command_id,
                                    correlation_id=record.correlation_id)
            state = SPAWN_STATE_ACTIVATED
        if state != SPAWN_STATE_DISPATCHED:
            receipt = await self._dispatch(
                "conversation.turn.send", record.subagent_id,
                {"text": record.prompt,
                 "client_message_id": f"subagent:{record.subagent_id}"},
                f"subagent-send:{record.spawn_command_id}",
                correlation_id=record.correlation_id)
            if receipt.error is not None:
                return fail(ERR_DISPATCH_FAILED, receipt.error.message, ErrorCategory.STATE,
                            cause=receipt.error.model_dump(mode="json"))
            self._append_node_patch(record.parent_thread_id, record.subagent_id,
                                    {"spawn_state": SPAWN_STATE_DISPATCHED},
                                    command_id=record.spawn_command_id,
                                    correlation_id=record.correlation_id)
        if not wait:
            return SideEffectResult(output={
                **self.snapshot(record.subagent_id, include_text=False),
                "wait": {"requested": False},
            })
        snapshot, finished, waited = await self.wait_for_terminal(record.subagent_id, timeout)
        return SideEffectResult(output={
            **snapshot,
            "wait": {
                "requested": True,
                "outcome": "finished" if finished else "pending",
                "timeout_seconds": timeout,
                "waited_seconds": waited,
            },
        })

    def _adopt_or_create_worktree(self, record: _SpawnRecord) -> None:
        """Create the child worktree, adopting a half-finished one after a crash."""
        from .git_workspace import inspect_git_workspace, resolve_git_root

        if self._manager.get_workspace(record.workspace_id or "") is not None:
            return
        settings = dict(record.workspace_settings)
        branch = str(settings.get("branch") or "")
        path = str(record.worktree_path or "")
        if path and resolve_git_root(path) is not None:
            status = inspect_git_workspace(path)
            if (status.get("current_branch") == branch
                    and (not settings.get("git_common_dir")
                         or status.get("git_common_dir") == settings.get("git_common_dir"))):
                self._store.save(Workspace(
                    workspace_id=record.workspace_id or "",
                    project_id=record.project_id,
                    kind=WORKSPACE_GIT,
                    root_path=str(status.get("root_path") or path),
                    settings={**settings, "pending_create": False},
                ))
                return
            raise ConversationError(
                f"worktree 路径已被其他检出占用：{path}")
        managed = {"mode", "branch", "base_ref", "worktree_of", "git_common_dir",
                   "created_via", "pending_create"}
        planned = self._manager.plan_workspace(
            project_id=record.project_id,
            mode=WS_MODE_NEW,
            branch=branch,
            base_ref=str(settings.get("base_ref") or "HEAD"),
            parent_root=str(settings.get("worktree_of") or ""),
            workspace_id=record.workspace_id or "",
            settings={key: value for key, value in settings.items() if key not in managed},
        )
        self._manager.save_workspace(planned)

    def _append_node_patch(
        self, parent_thread_id: str, subagent_id: str, patch: dict[str, Any],
        *, command_id: str = "", correlation_id: str = "",
    ) -> EventEnvelope:
        stored = self._store.append_events([ev.thread_event(
            parent_thread_id, ev.EV_SUBAGENT_UPDATED,
            {"subagent_id": subagent_id, **_json_safe_times(patch)},
            command_id=command_id or None,
            correlation_id=correlation_id or None,
        )])
        return stored[0]

    def _archive_child_stream(
        self, subagent_id: str, *, reason: str, error_code: str = "",
        command_id: str = "", correlation_id: str = "",
    ) -> bool:
        state = self._conv.get_state(subagent_id)
        if state.status == "archived":
            return False
        self._store.append_events([ev.thread_event(
            subagent_id, ev.EV_THREAD_ARCHIVED,
            {"thread_id": subagent_id, "reason": reason,
             **({"error_code": error_code} if error_code else {})},
            command_id=command_id or None,
            correlation_id=correlation_id or None,
        )])
        return True

    async def wait_for_terminal(
        self, subagent_id: str, timeout_seconds: float,
    ) -> tuple[dict[str, Any], bool, float]:
        """Wait (bounded) for a terminal status; returns (snapshot, finished, waited)."""
        self._remember_loop()
        started = time.monotonic()
        deadline = started + timeout_seconds
        while True:
            waiter = self._waiters.setdefault(subagent_id, asyncio.Event())
            snapshot = self.snapshot(subagent_id, include_text=True)
            waited = round(time.monotonic() - started, 3)
            if snapshot["status"] in TERMINAL_STATUSES:
                # The waiter can wake before the observer task mirrors the
                # terminal status, so the caller would see a stale parent node.
                self.sync_node(subagent_id)
                return snapshot, True, waited
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                return snapshot, False, waited
            try:
                await asyncio.wait_for(waiter.wait(), timeout=min(remaining, _WAIT_RECHECK_S))
            except asyncio.TimeoutError:
                continue

    def _wake(self, subagent_id: str) -> None:
        waiter = self._waiters.pop(subagent_id, None)
        if waiter is not None:
            waiter.set()

    # -- nested commands ---------------------------------------------------------

    async def _dispatch(
        self, command_type: str, thread_id: str, payload: dict[str, Any],
        idempotency_key: str, *, correlation_id: str = "",
        aggregate_type: str = ev.AGGREGATE_THREAD,
    ) -> CommandReceipt:
        api = self._require_api(correlation_id)
        return await api.dispatch(CommandEnvelope(
            command_type=command_type,
            aggregate_type=aggregate_type,
            aggregate_id=thread_id,
            actor=ORCHESTRATOR_ACTOR,
            payload={**payload, **({"correlation_id": correlation_id} if correlation_id else {})},
            idempotency_key=idempotency_key,
        ))

    async def stop_child(self, subagent_id: str, key: str, *, correlation_id: str = "") -> dict[str, Any]:
        """Stop one child's queued and running work; returns a typed outcome."""
        state = self._conv.get_state(subagent_id)
        outcome: dict[str, Any] = {"subagent_id": subagent_id, "actions": []}
        errors: list[dict[str, Any]] = []
        if state.queue_count and not state.queue_paused:
            receipt = await self._dispatch(
                "conversation.queue.pause", subagent_id, {"reason": "subagent_cancelled"},
                f"{key}:{subagent_id}:queue-pause", correlation_id=correlation_id)
            outcome["actions"].append("queue_paused")
            if receipt.error is not None:
                errors.append(receipt.error.model_dump(mode="json"))
        running = self._conv.get_state(subagent_id).running_turn_id
        if running:
            receipt = await self._dispatch(
                "conversation.turn.interrupt", subagent_id, {"expected_turn_id": running},
                f"{key}:{subagent_id}:interrupt:{running}", correlation_id=correlation_id)
            outcome["turn_id"] = running
            if receipt.error is None:
                outcome["actions"].append("turn_interrupted")
            elif receipt.error.code in _TURN_ALREADY_ENDED_CODES:
                outcome["actions"].append("turn_already_ended")
            else:
                errors.append(receipt.error.model_dump(mode="json"))
        if errors:
            outcome["errors"] = errors
        self.sync_node(subagent_id)
        outcome["status"] = self.live_status(subagent_id).status
        return outcome

    # -- observer ----------------------------------------------------------------

    def observe(self, event: EventEnvelope) -> None:
        """Append-events hook (runs after the projection hook)."""
        if event.aggregate_type != ev.AGGREGATE_THREAD:
            return
        etype = event.event_type
        if etype not in _CHILD_EVENTS and etype not in _PARENT_EVENTS:
            return
        thread_id = event.aggregate_id
        state = self._conv.get_state(thread_id)
        payload = dict(event.payload or {})
        if state.lineage is not None and etype in _CHILD_EVENTS:
            self._call_soon(lambda: self._wake(thread_id))
            if etype == ev.EV_THREAD_ARCHIVED:
                self._spawn_task(
                    lambda: self._on_child_archived(thread_id, event.event_id),
                    f"child {thread_id} archived")
            else:
                self._spawn_task(
                    lambda: self._on_child_turn_event(thread_id, etype, event.event_id),
                    f"child {thread_id} {etype}")
        if state.subagents and etype in _PARENT_EVENTS:
            self._schedule_cascade(thread_id, etype, payload)

    async def _on_child_turn_event(self, subagent_id: str, etype: str, event_id: str) -> None:
        node = self.node(subagent_id)
        if (etype == ev.EV_TURN_STARTED and node is not None and node.cancel_requested
                and node.status not in TERMINAL_STATUSES):
            # Cancellation arrived before this Turn could be interrupted.
            await self.stop_child(subagent_id, f"subagent-cancel-on-start:{event_id}")
        self.sync_node(subagent_id)

    async def _on_child_archived(self, subagent_id: str, event_id: str) -> None:
        self.sync_node(subagent_id)
        await self._cleanup_worktree(subagent_id, event_id)

    # -- cascades -----------------------------------------------------------------

    def _schedule_cascade(self, parent_id: str, etype: str, payload: dict[str, Any]) -> None:
        """Queue one cascade pass per (parent, kind); repeated events coalesce.

        A pass re-derives its targets from current state and reuses stable
        idempotency keys, so a lost wake-up loses nothing and startup recovery
        re-runs the same derivation.
        """
        turn_id = str(payload.get("turn_id") or "")

        def start() -> None:
            self._start_cascade(parent_id, etype, turn_id)

        if not self._call_soon(start):
            LOG.warning("subagent observer has no event loop; cascade %s runs at next recover",
                        parent_id)

    def _cascade_key(self, parent_id: str, etype: str, turn_id: str) -> tuple[str, str]:
        kind = "archive" if etype == ev.EV_THREAD_ARCHIVED else f"turn:{turn_id}"
        return (parent_id, kind)

    def _start_cascade(self, parent_id: str, etype: str, turn_id: str) -> Optional[asyncio.Task[Any]]:
        key = self._cascade_key(parent_id, etype, turn_id)
        existing = self._cascade_tasks.get(key)
        if existing is not None and not existing.done():
            return None

        async def run() -> dict[str, Any]:
            try:
                return await self._run_cascade(parent_id, etype, turn_id)
            finally:
                if self._cascade_tasks.get(key) is asyncio.current_task():
                    self._cascade_tasks.pop(key, None)

        task = asyncio.ensure_future(run())
        self._cascade_tasks[key] = task
        self._tasks.add(task)
        task.add_done_callback(self._task_done)
        return task

    async def _cascade_once(self, parent_id: str, etype: str, turn_id: str) -> dict[str, Any]:
        """Run one cascade pass and await it (startup recovery)."""
        key = self._cascade_key(parent_id, etype, turn_id)
        existing = self._cascade_tasks.get(key)
        if existing is not None and not existing.done():
            return await existing
        return await self._run_cascade(parent_id, etype, turn_id)

    async def _run_cascade(self, parent_id: str, etype: str, turn_id: str) -> dict[str, Any]:
        state = self._conv.get_state(parent_id)
        report: dict[str, Any] = {"parent_thread_id": parent_id, "cancelled": [],
                                  "archived": [], "errors": []}
        if etype == ev.EV_TURN_INTERRUPTED:
            turn = self._conv.get_turn(turn_id) if turn_id else None
            if turn is None or turn.status not in {TURN_INTERRUPTED, TURN_CANCELLED}:
                report["skipped"] = "turn_not_interrupted"
                return report
            targets = [n for n in state.subagents if n.turn_id == turn_id]
            reason = "parent_turn_cancelled" if turn.status == TURN_CANCELLED else "parent_turn_interrupted"
            key_fragment = f"subagent-cascade:turn:{parent_id}:{turn_id}"
            report["kind"] = reason
        else:
            if state.status != "archived":
                report["skipped"] = "parent_not_archived"
                return report
            targets = list(state.subagents)
            reason = "parent_thread_archived"
            ended = (state.archived_at or state.updated_at).isoformat()
            key_fragment = f"subagent-cascade:archive:{parent_id}:{ended}"
            report["kind"] = "parent_thread_archived"
        for node in targets:
            if self.lineage(node.agent_id) is None:
                # Not a real child Thread (e.g. an invalid spawn record, which
                # recovery reports separately); nothing can be dispatched for it.
                report.setdefault("skipped_invalid", []).append(node.agent_id)
                continue
            if self.live_status(node.agent_id, node).status in ACTIVE_STATUSES:
                receipt = await self._dispatch(
                    CANCEL_COMMAND, parent_id,
                    {"subagent_id": node.agent_id, "reason": reason},
                    f"{key_fragment}:{node.agent_id}")
                if receipt.error is not None:
                    LOG.warning("subagent cascade cancel %s failed: %s %s",
                                node.agent_id, receipt.error.code, receipt.error.message)
                    report["errors"].append({
                        "subagent_id": node.agent_id, "action": "cancel",
                        "code": receipt.error.code, "message": receipt.error.message})
                else:
                    report["cancelled"].append(node.agent_id)
            if etype != ev.EV_THREAD_ARCHIVED:
                continue
            child_state = self._conv.get_state(node.agent_id)
            if child_state.status == "archived":
                await self._cleanup_worktree(node.agent_id, f"{key_fragment}:{node.agent_id}")
                continue
            if self._manager.get_thread(node.agent_id) is None:
                # The spawn side effects never activated this child, so there
                # is no Thread row or Binding for conversation.thread.archive;
                # archive the event stream directly.
                if self._archive_child_stream(node.agent_id, reason=reason):
                    report["archived"].append(node.agent_id)
                await self._cleanup_worktree(node.agent_id, f"{key_fragment}:{node.agent_id}")
                continue
            # Archiving the child revokes its Binding and closes its session;
            # its own archive event cascades further down and cleans its worktree.
            receipt = await self._dispatch(
                "conversation.thread.archive", node.agent_id,
                {"thread_id": node.agent_id},
                f"{key_fragment}:archive:{node.agent_id}")
            if receipt.error is not None:
                LOG.warning("subagent cascade archive %s failed: %s %s",
                            node.agent_id, receipt.error.code, receipt.error.message)
                report["errors"].append({
                    "subagent_id": node.agent_id, "action": "archive",
                    "code": receipt.error.code, "message": receipt.error.message})
            else:
                report["archived"].append(node.agent_id)
        return report

    async def _cleanup_worktree(self, subagent_id: str, cause: str) -> Optional[dict[str, Any]]:
        """Remove a subagent-owned worktree once its Thread is archived.

        Uncommitted changes are never discarded: a dirty worktree is retained
        and reported on the node with ``conversation.git.worktree_dirty``.
        Returns the applied node patch, or None when nothing needed doing.
        """
        from .git_workspace import inspect_git_workspace

        lineage = self.lineage(subagent_id)
        node = self.node(subagent_id)
        if (lineage is None or node is None or lineage.isolation != ISOLATION_WORKTREE
                or not lineage.workspace_id or node.worktree_state == "removed"):
            return None
        workspace = self._manager.get_workspace(lineage.workspace_id)
        patch: dict[str, Any]
        if workspace is None or not workspace.root_path:
            patch = {"worktree_state": "removed"}
        else:
            status = await asyncio.to_thread(inspect_git_workspace, workspace.root_path)
            if status.get("dirty"):
                patch = {"worktree_state": "retained", "error_code": "conversation.git.worktree_dirty",
                         "error": f"worktree {workspace.root_path} has uncommitted changes; kept"}
            else:
                receipt = await self._dispatch(
                    "conversation.workspace.delete_worktree", lineage.workspace_id,
                    {"workspace_id": lineage.workspace_id},
                    f"subagent-worktree-cleanup:{subagent_id}",
                    aggregate_type=ev.AGGREGATE_WORKSPACE)
                if receipt.error is None:
                    patch = {"worktree_state": "removed"}
                else:
                    patch = {"worktree_state": "retained", "error_code": receipt.error.code,
                             "error": receipt.error.message}
        self._store.append_events([ev.thread_event(
            lineage.parent_thread_id, ev.EV_SUBAGENT_UPDATED,
            {"subagent_id": subagent_id, **patch, "cause_event_id": cause},
        )])
        return patch


def _activate_spawn(manager: ConversationManager, record: _SpawnRecord) -> None:
    """Idempotent ``activate_thread`` for a (possibly half-finished) spawn.

    A Thread row means the first attempt crashed after ``store.save``; only
    the missing Binding / Runtime selection steps are re-run then.
    """
    existing = manager.get_thread(record.subagent_id)
    if existing is None:
        manager.activate_thread(
            Thread(
                thread_id=record.subagent_id,
                project_id=record.project_id,
                workspace_id=record.workspace_id,
                title=record.title,
                title_source=record.title_source,
                mode="conversation",
            ),
            record.principal_id,
            runtime=dict(record.runtime),
        )
        return
    manager._bindings.issue_binding(existing.thread_id, record.principal_id, existing.mode)
    if record.runtime:
        manager.save_runtime_selection(existing.thread_id, dict(record.runtime))


def _runtime_view(selection: Optional[ThreadRuntimeSelection]) -> Optional[dict[str, Any]]:
    if selection is None:
        return None
    return {
        "adapter_id": selection.adapter_id,
        "instance_id": selection.instance_id,
        "credential_id": selection.credential_id,
        "model": selection.model,
        "effort": selection.effort,
        "service_tier": selection.service_tier,
        "access_mode": selection.access_mode,
    }


def _json_safe_times(patch: dict[str, Any]) -> dict[str, Any]:
    from datetime import datetime, timezone

    result = dict(patch)
    if isinstance(result.get("completed_at"), float):
        result["completed_at"] = datetime.fromtimestamp(
            result["completed_at"], tz=timezone.utc).isoformat()
    return result


def _caller_thread(command_or_query: Any, *, aggregate_id: str, payload: dict[str, Any]) -> str:
    """The Thread the request acts for; Agents may only act for their own."""
    actor = command_or_query.actor
    correlation = getattr(command_or_query, "command_id", "") or getattr(command_or_query, "query_id", "")
    if actor.kind in OPERATOR_ACTOR_KINDS:
        caller = aggregate_id or str(payload.get("thread_id") or "").strip()
    else:
        caller = str(actor.thread_id or "").strip()
        if aggregate_id and caller and aggregate_id != caller:
            raise CommandFailed(_error(
                ERR_CALLER_MISMATCH, "subagent tools act only on the calling Thread",
                ErrorCategory.PERMISSION, correlation_id=correlation))
    if not caller:
        raise CommandFailed(_error(
            ERR_CALLER_UNKNOWN, "the calling conversation Thread could not be determined",
            ErrorCategory.PERMISSION, correlation_id=correlation))
    return caller


class SubagentCommandHandler:
    """``conversation.subagent.spawn`` / ``conversation.subagent.cancel``."""

    command_types = {SPAWN_COMMAND, CANCEL_COMMAND}

    def __init__(self, service: SubagentService) -> None:
        self._svc = service
        self._manager = service._manager
        self._conv = service._conv

    async def plan(self, command: Any, ctx: HandlerContext) -> CommandPlan:
        self._svc._remember_loop()
        self._svc._require_api(correlation_id_of(command))
        if command.command_type == SPAWN_COMMAND:
            return self._plan_spawn(command, ctx)
        return self._plan_cancel(command)

    # -- spawn -------------------------------------------------------------------

    def _child_runtime(
        self, command: Any, parent: ThreadRuntimeSelection, payload: dict[str, Any],
    ) -> dict[str, Any]:
        def text(key: str) -> str:
            value = payload.get(key)
            if value is not None and not isinstance(value, str):
                raise _failed(command, ERR_ARGUMENT_INVALID, f"{key} must be a string",
                              ErrorCategory.VALIDATION)
            return str(value or "").strip()

        engine = text("engine").lower()
        adapter_id = text("adapter_id")
        if engine:
            try:
                default_adapter = get_descriptor(engine).identity.default_adapter_id
            except UnknownProviderError as exc:
                raise _failed(command, ERR_RUNTIME_INVALID, f"unknown engine {engine!r}",
                              ErrorCategory.VALIDATION) from exc
            if adapter_id and engine_for_adapter(adapter_id) != engine:
                raise _failed(command, ERR_RUNTIME_INVALID,
                              f"adapter_id {adapter_id!r} does not belong to engine {engine!r}",
                              ErrorCategory.VALIDATION)
            adapter_id = adapter_id or default_adapter
        adapter_id = adapter_id or parent.adapter_id
        if not adapter_id:
            raise _failed(command, ERR_RUNTIME_INVALID,
                          "the calling Thread has no Runtime selection to inherit; pass engine and model",
                          ErrorCategory.VALIDATION)
        same_engine = bool(parent.adapter_id) and (
            engine_for_adapter(adapter_id) == engine_for_adapter(parent.adapter_id))
        same_adapter = adapter_id == parent.adapter_id
        model = text("model") or (parent.model if same_engine else "")
        same_model = same_engine and model == parent.model
        access_mode = text("access_mode") or parent.access_mode or AccessMode.SUPERVISED.value
        if access_mode not in ACCESS_MODE_VALUES:
            raise _failed(command, ERR_ARGUMENT_INVALID, f"unknown access_mode {access_mode!r}",
                          ErrorCategory.VALIDATION)
        parent_access = parent.access_mode or AccessMode.SUPERVISED.value
        if _ACCESS_RANK[access_mode] > _ACCESS_RANK.get(parent_access, 0):
            raise _failed(command, ERR_ACCESS_EXCEEDS_PARENT,
                          f"access_mode {access_mode!r} exceeds the parent's {parent_access!r}",
                          ErrorCategory.PERMISSION,
                          detail={"requested": access_mode, "parent": parent_access})
        return {
            "adapter_id": adapter_id,
            "instance_id": text("instance_id") or (parent.instance_id if same_adapter else "default"),
            "credential_id": text("credential_id") or (parent.credential_id if same_engine else ""),
            "model": model,
            "effort": text("effort") or (parent.effort if same_model else ""),
            "service_tier": parent.service_tier if same_adapter and same_model else "",
            "access_mode": access_mode,
        }

    def _plan_spawn(self, command: Any, ctx: HandlerContext) -> CommandPlan:
        svc = self._svc
        manager = self._manager
        payload = dict(command.payload or {})
        caller = _caller_thread(command, aggregate_id=command.aggregate_id, payload=payload)
        parent = manager.get_thread(caller)
        if parent is None:
            raise _failed(command, "conversation.thread.not_found", f"unknown thread: {caller}",
                          ErrorCategory.NOT_FOUND)
        if parent.mode != "conversation":
            raise _failed(command, ERR_MODE_INVALID,
                          "subagents can only be spawned from conversation Threads",
                          ErrorCategory.PERMISSION)
        parent_state = self._conv.get_state(caller)
        if parent_state.status == "archived":
            raise _failed(command, ERR_PARENT_ARCHIVED, "the calling Thread is archived",
                          ErrorCategory.STATE)
        parent_lineage = parent_state.lineage
        own_node = svc.node(caller) if parent_lineage is not None else None
        if own_node is not None and own_node.cancel_requested:
            raise _failed(command, ERR_PARENT_CANCELLED,
                          "the calling subagent has been cancelled and cannot spawn more",
                          ErrorCategory.STATE)

        prompt = payload.get("prompt")
        if not isinstance(prompt, str) or not prompt.strip():
            raise _failed(command, ERR_ARGUMENT_INVALID, "prompt must be a non-empty string",
                          ErrorCategory.VALIDATION)
        title = payload.get("title")
        if title is not None and not isinstance(title, str):
            raise _failed(command, ERR_ARGUMENT_INVALID, "title must be a string",
                          ErrorCategory.VALIDATION)
        title = str(title or "").strip()
        isolation = payload.get("isolation", ISOLATION_SHARED)
        if isolation not in (ISOLATION_SHARED, ISOLATION_WORKTREE):
            raise _failed(command, ERR_ARGUMENT_INVALID, "isolation must be 'shared' or 'worktree'",
                          ErrorCategory.VALIDATION)
        wait = payload.get("wait", False)
        if not isinstance(wait, bool):
            raise _failed(command, ERR_ARGUMENT_INVALID, "wait must be a boolean",
                          ErrorCategory.VALIDATION)
        timeout = payload.get("timeout_seconds")
        if timeout is None:
            timeout = svc.limits.default_wait_seconds
        elif isinstance(timeout, bool) or not isinstance(timeout, (int, float)) or not (
                0 < float(timeout) <= svc.limits.max_wait_seconds):
            raise _failed(command, ERR_TIMEOUT_INVALID,
                          f"timeout_seconds must be within (0, {svc.limits.max_wait_seconds}]",
                          ErrorCategory.VALIDATION,
                          detail={"max_wait_seconds": svc.limits.max_wait_seconds})
        timeout = float(timeout)

        depth = (parent_lineage.depth if parent_lineage is not None else 0) + 1
        if depth > svc.limits.max_depth:
            raise _failed(command, ERR_DEPTH_EXCEEDED,
                          f"subagent depth {depth} exceeds the limit {svc.limits.max_depth}",
                          ErrorCategory.PERMISSION,
                          detail={"depth": depth, "max_depth": svc.limits.max_depth})
        root_id = parent_lineage.root_thread_id if parent_lineage is not None else caller
        # Plan runs without awaiting until the Command API commits its events,
        # so this count and the new child's lineage are atomic per event loop.
        active = svc.active_in_root(root_id)
        if len(active) >= svc.limits.max_running_per_root:
            raise _failed(command, ERR_CONCURRENCY_EXCEEDED,
                          f"root thread {root_id} already has {len(active)} active subagents "
                          f"(limit {svc.limits.max_running_per_root})",
                          ErrorCategory.STATE,
                          detail={"root_thread_id": root_id, "active_subagent_ids": active,
                                  "max_running_per_root": svc.limits.max_running_per_root})

        parent_selection = manager.runtime_selection(caller)
        runtime = self._child_runtime(command, parent_selection, payload)
        _reject_paused_runtime(command, runtime)

        child = Thread(
            project_id=parent.project_id,
            workspace_id=parent.workspace_id,
            title=title,
            title_source="user" if title else "fallback",
            mode="conversation",
        )
        try:
            selection = manager._build_runtime_selection(child.thread_id, runtime)
        except ConversationError as exc:
            raise _failed(command, ERR_RUNTIME_INVALID, str(exc), ErrorCategory.VALIDATION,
                          detail={"runtime": runtime}) from exc

        planned_workspace = None
        if isolation == ISOLATION_WORKTREE:
            parent_workspace = (manager.get_workspace(str(parent.workspace_id))
                                if parent.workspace_id else None)
            if parent_workspace is None or not parent_workspace.root_path:
                raise _failed(command, ERR_WORKTREE_UNAVAILABLE,
                              "the calling Thread has no workspace to derive a worktree from",
                              ErrorCategory.STATE)
            if not parent.project_id:
                raise _failed(command, ERR_WORKTREE_UNAVAILABLE,
                              "worktree isolation requires the calling Thread to belong to a project",
                              ErrorCategory.STATE)
            try:
                planned_workspace = manager.plan_workspace(
                    project_id=parent.project_id,
                    mode=WS_MODE_NEW,
                    branch=f"muteki/subagent/{child.thread_id}",
                    parent_root=parent_workspace.root_path,
                    settings={"created_by": "subagent", "subagent_id": child.thread_id},
                )
            except ConversationError as exc:
                raise _failed(command, ERR_WORKTREE_UNAVAILABLE, str(exc), ErrorCategory.STATE,
                              detail={"parent_root": parent_workspace.root_path}) from exc
            child = child.model_copy(update={"workspace_id": planned_workspace.workspace_id})

        principal = (ctx.binding.principal_id if ctx.binding is not None and ctx.binding.principal_id
                     else str(payload.get("principal_id") or "local-user"))
        parent_turn_id = parent_state.running_turn_id or None
        lineage = {
            "origin": SUBAGENT_ORIGIN_APP,
            "subagent_id": child.thread_id,
            "parent_thread_id": caller,
            "parent_turn_id": parent_turn_id,
            "root_thread_id": root_id,
            "depth": depth,
            "isolation": isolation,
            "workspace_id": child.workspace_id,
            "worktree_path": planned_workspace.root_path if planned_workspace is not None else None,
        }
        node = {
            "title": title or prompt.strip().splitlines()[0],
            "turn_id": parent_turn_id,
            "model": selection.model or None,
            "session_ref": child.thread_id,
            "status": STATUS_PENDING,
            "request": prompt,
            "thread_id": child.thread_id,
            "depth": depth,
            "adapter_id": selection.adapter_id,
            "access_mode": selection.access_mode,
            "isolation": isolation,
            "workspace_id": child.workspace_id,
            "worktree_path": lineage["worktree_path"],
            "worktree_state": None,
            "spawn_state": SPAWN_STATE_PLANNED,
            "principal_id": principal,
        }
        meta = dict(actor_id=command.actor.id, command_id=command.command_id,
                    correlation_id=correlation_id_of(command))
        events: list[EventEnvelope] = []
        if planned_workspace is not None:
            settings = dict(planned_workspace.settings or {})
            events.append(ev.conversation_event(
                ev.AGGREGATE_WORKSPACE, planned_workspace.workspace_id, ev.EV_WORKSPACE_BOUND, {
                    "workspace_id": planned_workspace.workspace_id,
                    "project_id": planned_workspace.project_id,
                    "kind": planned_workspace.kind,
                    "root_path": planned_workspace.root_path,
                    "mode": str(settings.get("mode") or ""),
                    "branch": str(settings.get("branch") or ""),
                    # Full plan so recovery can re-create or adopt the worktree.
                    "settings": settings,
                }, **meta))
        events.extend([
            ev.thread_event(child.thread_id, ev.EV_THREAD_CREATED, {
                "thread_id": child.thread_id,
                "project_id": child.project_id,
                "workspace_id": child.workspace_id,
                "title": child.title,
                "title_source": child.title_source,
                "mode": child.mode,
                "runtime": runtime,
            }, **meta),
            ev.thread_event(child.thread_id, ev.EV_THREAD_LINEAGE_SET, {
                "thread_id": child.thread_id, "lineage": lineage,
            }, **meta),
            ev.thread_event(caller, ev.EV_SUBAGENT_SPAWNED, {
                "subagent_id": child.thread_id, "node": node,
            }, **meta, idempotency_key=command.idempotency_key),
        ])

        correlation = correlation_id_of(command)
        record = _SpawnRecord(
            subagent_id=child.thread_id,
            parent_thread_id=caller,
            spawn_command_id=command.command_id,
            correlation_id=correlation,
            principal_id=principal,
            title=child.title,
            title_source=child.title_source,
            prompt=prompt,
            spawn_state=SPAWN_STATE_PLANNED,
            isolation=isolation,
            workspace_id=str(child.workspace_id or "") or None,
            workspace_settings=(dict(planned_workspace.settings)
                                if planned_workspace is not None else {}),
            worktree_path=lineage["worktree_path"],
            runtime=runtime,
            project_id=child.project_id,
            cancel_requested=False,
            status=STATUS_PENDING,
        )

        async def _spawn() -> SideEffectResult:
            svc._spawning.add(child.thread_id)
            try:
                return await svc.run_spawn(record, wait=wait, timeout=timeout)
            finally:
                svc._spawning.discard(child.thread_id)

        return CommandPlan(
            events=events,
            receipt=_receipt(command, ev.AGGREGATE_THREAD, caller),
            side_effect=_spawn,
        )

    # -- cancel ------------------------------------------------------------------

    def _plan_cancel(self, command: Any) -> CommandPlan:
        svc = self._svc
        payload = dict(command.payload or {})
        caller = _caller_thread(command, aggregate_id=command.aggregate_id, payload=payload)
        target = payload.get("subagent_id")
        if not isinstance(target, str) or not target.strip():
            raise _failed(command, ERR_ARGUMENT_INVALID, "subagent_id is required",
                          ErrorCategory.VALIDATION)
        target = target.strip()
        if not svc.is_descendant(target, caller):
            raise _failed(command, ERR_NOT_FOUND,
                          f"{target} is not a subagent of thread {caller}",
                          ErrorCategory.NOT_FOUND)
        reason = str(payload.get("reason") or "requested")
        targets = [target, *svc.descendants(target)]
        meta = dict(actor_id=command.actor.id, command_id=command.command_id,
                    correlation_id=correlation_id_of(command))
        events = []
        for subagent_id in targets:
            lineage = svc.lineage(subagent_id)
            events.append(ev.thread_event(lineage.parent_thread_id, ev.EV_SUBAGENT_CANCEL_REQUESTED, {
                "subagent_id": subagent_id,
                "reason": reason,
                "requested_by_thread_id": caller,
                "cascade_from": None if subagent_id == target else target,
            }, **meta))
        correlation = correlation_id_of(command)

        async def _cancel() -> SideEffectResult:
            outcomes = [
                await svc.stop_child(subagent_id, f"subagent-cancel:{command.command_id}",
                                     correlation_id=correlation)
                for subagent_id in targets
            ]
            output = {"subagent_id": target, "reason": reason, "outcomes": outcomes}
            failed = [item for item in outcomes if item.get("errors")]
            if failed:
                return SideEffectResult(
                    error=_error(ERR_CANCEL_INCOMPLETE,
                                 f"{len(failed)} of {len(outcomes)} subagents could not be stopped",
                                 ErrorCategory.STATE, correlation_id=correlation,
                                 detail={"outcomes": outcomes}),
                    state=ReceiptState.FAILED, output=output)
            return SideEffectResult(output=output)

        return CommandPlan(
            events=events,
            receipt=_receipt(command, ev.AGGREGATE_THREAD, caller),
            side_effect=_cancel,
        )


class SubagentStatusQueryHandler:
    """``conversation.subagent.status``: one subagent (full text) or all children."""

    query_types = {STATUS_QUERY}

    def __init__(self, service: SubagentService) -> None:
        self._svc = service

    async def handle(self, query: Any, ctx: HandlerContext) -> QueryResult:
        svc = self._svc
        svc._remember_loop()
        params = dict(query.params or {})
        caller = _caller_thread(query, aggregate_id=str(query.aggregate_id or ""), payload=params)
        subagent_id = params.get("subagent_id")
        wait_seconds = params.get("wait_seconds")
        if wait_seconds is not None and (
                isinstance(wait_seconds, bool) or not isinstance(wait_seconds, (int, float))
                or not 0 <= float(wait_seconds) <= svc.limits.max_wait_seconds):
            raise CommandFailed(_error(
                ERR_TIMEOUT_INVALID,
                f"wait_seconds must be within [0, {svc.limits.max_wait_seconds}]",
                ErrorCategory.VALIDATION, correlation_id=query.query_id,
                detail={"max_wait_seconds": svc.limits.max_wait_seconds}))
        if subagent_id is None:
            if wait_seconds:
                raise CommandFailed(_error(
                    ERR_ARGUMENT_INVALID, "wait_seconds requires subagent_id",
                    ErrorCategory.VALIDATION, correlation_id=query.query_id))
            children = [node.agent_id for node in svc._conv.get_state(caller).subagents]
            for child in children:
                svc.sync_node(child)
            lineage = svc.lineage(caller)
            root_id = lineage.root_thread_id if lineage is not None else caller
            return QueryResult(query_id=query.query_id, query_type=query.query_type, result={
                "thread_id": caller,
                "subagents": [svc.snapshot(child, include_text=False) for child in children],
                "count": len(children),
                "root_thread_id": root_id,
                "active_in_root": len(svc.active_in_root(root_id)),
                "limits": svc.limits.describe(),
            })
        if not isinstance(subagent_id, str) or not svc.is_descendant(subagent_id.strip(), caller):
            raise CommandFailed(_error(
                ERR_NOT_FOUND, f"{subagent_id!r} is not a subagent of thread {caller}",
                ErrorCategory.NOT_FOUND, correlation_id=query.query_id,
                recovery_hint="call muteki_subagent_status without subagent_id to list children"))
        subagent_id = subagent_id.strip()
        svc.sync_node(subagent_id)
        result: dict[str, Any]
        if wait_seconds:
            snapshot, finished, waited = await svc.wait_for_terminal(subagent_id, float(wait_seconds))
            result = {"subagent": snapshot, "wait": {
                "requested": True, "outcome": "finished" if finished else "pending",
                "timeout_seconds": float(wait_seconds), "waited_seconds": waited,
            }}
        else:
            result = {"subagent": svc.snapshot(subagent_id, include_text=True)}
        return QueryResult(query_id=query.query_id, query_type=query.query_type, result=result)


def register_subagent_handlers(api: Any, service: SubagentService) -> None:
    """Register subagent handlers on the shared Command API (idempotent)."""
    service.bind_command_api(api)
    if SPAWN_COMMAND not in api.handlers.known_command_types():
        api.register_command(SubagentCommandHandler(service))
    if STATUS_QUERY not in api.handlers.known_query_types():
        api.register_query(SubagentStatusQueryHandler(service))


__all__ = [
    "ACTIVE_STATUSES",
    "CANCEL_COMMAND",
    "SPAWN_COMMAND",
    "STATUS_QUERY",
    "TERMINAL_STATUSES",
    "SubagentCommandHandler",
    "SubagentLimits",
    "SubagentService",
    "SubagentStatusQueryHandler",
    "register_subagent_handlers",
]
