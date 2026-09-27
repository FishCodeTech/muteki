"""ExternalAgentSessionExecutor：通用对话 / 单 Agent 任务的执行器（CONV-01，任务书 8.1）。

职责：

- 创建或恢复 AgentSession：经 ``AdapterRegistry`` 找到 Runtime instance 的
  Adapter，走 ``BaseExternalAgentAdapter`` 的七步 Binding 交付（内部
  AgentSession → CapabilityBinding → probe 选注入计划 → 签发短期 grant 并
  注入 → external session id 回填 / 失败撤 grant → lease 续期 → 关闭撤
  grant）；
- Thread 消息转 ``AgentInput``，消费 Adapter 的统一 ``AgentEvent`` 流并
  翻译为 Thread 聚合流上的 Public Event（core.* 命名空间，完整内容）；
- streaming / approval / user input / steer / interrupt / resume 的处理；
- 保存 Artifact、usage、Session resume handle（由 Adapter 落到
  AgentSession 记录）；
- 每个 Thread 一个执行代（generation）：切换 Runtime 时关闭旧 Session
  （联动撤销 grant）、generation+1、重新生成注入计划并创建新 Session；
- 不创建 SharedGraph；长期记忆由 Manager 经用户允许后写 memory.timeline.v1。

Turn 执行是异步后台任务：``conversation.turn.send`` 的副作用只负责启动
任务并立即返回 receipt（异步命令模式）；Turn 进度经事件流与水位追踪。
"""

from __future__ import annotations

import asyncio
import json
import logging
import time
from pathlib import Path
from typing import Any, Optional

from muteki.external_agents.registry import AdapterRegistry
from muteki.external_agents.interaction_matrix import (
    attach_matrix_to_snapshot,
    build_matrix_from_probe,
)
from muteki.external_agents.runtime_capabilities import RuntimeCapabilitySnapshot
from muteki.external_agents.sessions import EXIT_INTERRUPTED
from muteki.external_agents.user_input_schema import normalize_pending_user_input
from muteki.platform.contracts.errors import ErrorCategory, ErrorEnvelope
from muteki.platform.contracts.external_agents import (
    AgentEvent,
    AgentEventType,
    AgentInput,
    AgentSessionRef,
    SessionStart,
)
from muteki.platform.contracts.objects import AgentSession, Artifact, Task, Thread
from muteki.platform.contracts.receipts import (
    AggregateRef,
    CommandReceipt,
    ReceiptState,
)
from muteki.platform.store import PlatformStore
from muteki.external_agents.factory import engine_for_adapter
from muteki.external_agents.approvals import ApprovalDecision, ApprovalRequest
from muteki.solver.credential_accounts import resolve_credential_env

from . import events as ev
from muteki.conversation.approval_queue import (
    has_actionable_approvals,
    normalize_approval_payload,
)
from .attachment_delivery import (
    AttachmentDeliveryError,
    attachments_payload,
    merge_attachment_context,
    resolve_attachments,
    thread_authorized_sha256s,
)
from .composer_capabilities import resolve_capability_refs
from .manager import ConversationManager
from .models import (
    TURN_COMPLETED,
    TURN_FAILED,
    TURN_INTERRUPTED,
    TURN_KIND_EDIT_RESEND,
    TURN_KIND_RETRY,
    TURN_QUEUED,
    TURN_RUNNING,
    ThreadState,
    TurnRecord,
)
from .session_handoff import (
    REASON_RESTART,
    REASON_RETRY,
    RECOVERY_NATIVE_RESUME,
    RECOVERY_STRUCTURED_HANDOFF,
    SessionHandoffBundle,
    build_session_handoff,
    classify_rebuild_reason,
    continuation_prompt_from_messages,
    render_agent_text,
    render_resume_prompt,
)
from .store import ConversationStore

LOG = logging.getLogger(__name__)


class ControlDeliveryError(RuntimeError):
    """A control response failed; retain its pending public request."""

    def __init__(self, runtime_code: str):
        super().__init__("操作未送达原会话，待处理请求已保留。请刷新后重试，或取消本轮。")
        self.runtime_code = runtime_code

# #188: capability refresh failure backoff (seconds).
_CAPABILITY_REFRESH_BACKOFF_BASE_S = 2.0
_CAPABILITY_REFRESH_BACKOFF_MAX_S = 60.0

#: 这些失败表示 Runtime 已正常结束本轮，Session 可以留给下一轮。
#: 其余失败（超时、传输断开、执行器异常）必须关掉 Session，否则
#: 八个引擎都会在 Muteki 停听后继续调 Capability。
_KEEP_SESSION_TURN_REASONS = frozenset({
    "empty_assistant",
    "refusal",
    EXIT_INTERRUPTED,
    "interrupted",
})


def _match_runtime_capability(
    snapshot: RuntimeCapabilitySnapshot,
    invocation: dict[str, Any],
) -> Any:
    """按稳定身份重新解析一次 Runtime 调用。

    已经保存 capability id 的选择只能命中同一个 id，禁止在 id 失效后
    悄悄按 wire text 绑定到另一项。用户手输命令没有 id，只有 wire text
    在当前目录唯一时才允许解析。
    """
    eligible = [
        item for item in snapshot.items
        if item.kind in {"command", "operation", "skill"}
        and item.verification == "verified"
        and item.delivery == "guaranteed"
        and bool(getattr(item, "invocable", True))
        and str(getattr(item, "support_level", "supported") or "supported")
        in {"supported", "limited"}
    ]
    capability_id = str(invocation.get("id") or "")
    if capability_id:
        return next(
            (item for item in eligible if item.id == capability_id), None,
        )
    wire_text = str(invocation.get("wire_text") or "").casefold()
    if not wire_text:
        return None
    matches = [
        item for item in eligible
        if str(
            item.invocation.get("wire_text") or f"/{item.name}"
        ).casefold() == wire_text
    ]
    return matches[0] if len(matches) == 1 else None


def _continuation_prompt(
    turn: TurnRecord, messages: list[Any], turns: list[Any] | None = None,
) -> str:
    """把已保存的对话内容还原成可独立执行的继续/重试指令。"""
    return continuation_prompt_from_messages(turn, messages, turns)



class ExternalAgentSessionExecutor:
    """Thread ↔ 外部 Agent Runtime Session 的执行桥。"""

    def __init__(
        self,
        store: PlatformStore,
        conv: ConversationStore,
        manager: ConversationManager,
        registry: AdapterRegistry,
        *,
        sessions_root: str | Path,
    ) -> None:
        self._store = store
        self._conv = conv
        self._manager = manager
        self._registry = registry
        self._sessions_root = Path(sessions_root)
        # thread_id → 正在执行的 Turn 任务 / 顺序锁
        self._tasks: dict[str, asyncio.Task] = {}
        self._stop_fences: dict[str, asyncio.Event] = {}
        self._locks: dict[str, asyncio.Lock] = {}
        self._queue_locks: dict[str, asyncio.Lock] = {}
        # 本进程内已 start 的 AgentSession（重启后需经 resume_handle 接管）
        self._live: set[str] = set()
        # Codex 等 Runtime 会在同一轮的多个 item 完成节点重复上报同一份
        # workspace diff。Artifact 本体虽然按摘要去重，事件仍会重复进入界面。
        self._emitted_workspace_artifacts: set[tuple[str, str]] = set()
        self._capability_cache: dict[str, RuntimeCapabilitySnapshot] = {}
        self._capability_tasks: dict[
            str, asyncio.Task[RuntimeCapabilitySnapshot]
        ] = {}
        # thread_id → last capability refresh failure (error / backoff / attempts)
        self._capability_failures: dict[str, dict[str, Any]] = {}
        # thread_id → pending structured handoff for next AgentInput
        self._pending_handoff: dict[str, SessionHandoffBundle] = {}

    @staticmethod
    def _selection_snapshot(selection: Any) -> dict[str, str]:
        """Return the non-secret Runtime identity attached to a public turn event."""
        return {
            "adapter_id": str(getattr(selection, "adapter_id", "") or ""),
            "instance_id": str(getattr(selection, "instance_id", "") or "default"),
            "credential_id": str(getattr(selection, "credential_id", "") or ""),
            "model": str(getattr(selection, "model", "") or ""),
            "effort": str(getattr(selection, "effort", "") or ""),
            "access_mode": str(
                getattr(selection, "access_mode", "") or ""
            ),
            "permission_mode": str(
                getattr(selection, "permission_mode", "") or ""
            ),
            "sandbox_mode": str(getattr(selection, "sandbox_mode", "") or ""),
        }

    def _runtime_snapshot(self, thread_id: str) -> dict[str, str]:
        return self._selection_snapshot(
            self._manager.runtime_selection(thread_id)
        )

    # -- 事件出口 -----------------------------------------------------------------

    def _emit(
        self,
        thread_id: str,
        event_type: str,
        payload: dict[str, Any],
        *,
        command_id: Optional[str] = None,
    ) -> None:
        """向 Thread 聚合流追加事件；投影由 ConversationService 挂钩应用。"""
        event = ev.thread_event(
            thread_id, event_type, payload, command_id=command_id)
        self._store.append_events(event)

    # -- Session 生命周期 ----------------------------------------------------------

    def _bind_runtime(self, adapter: Any) -> None:
        """使用 Adapter 前补齐 PlatformStore 与 BindingService。

        Adapter 可能先构造再注册，SessionTracker 若没有 store，
        AgentSession 不会写入 platform.db，审批、恢复和重启都找不到会话。
        """
        tracker = getattr(adapter, "_tracker", None)
        if tracker is not None and getattr(tracker, "_store", None) is None:
            tracker._store = self._store
        if getattr(adapter, "_binding_service", None) is None:
            adapter._binding_service = self._manager.bindings

    def _adapter_for(self, adapter_id: str, instance_id: str) -> Any:
        adapter = self._registry.get(adapter_id, instance_id)
        if adapter is None:
            raise LookupError(
                f"runtime instance 未注册：{adapter_id}:{instance_id}")
        self._bind_runtime(adapter)
        return adapter

    async def _resolve_turn_attachments(
        self,
        thread: Thread,
        turn: TurnRecord,
        adapter: Any,
        *,
        workspace_root: str,
    ) -> list[Any]:
        """Resolve Turn attachment digests into staged Adapter payload entries."""
        caps = await adapter.capabilities()
        image_input = bool(getattr(caps, "image_input", False))
        authorized = thread_authorized_sha256s(
            self._store.read_events("thread", thread.thread_id, limit=10_000)
        )

        def get_artifact(sha256: str) -> Optional[Artifact]:
            return self._store.get(Artifact, sha256)

        stage_root = ""
        if not str(workspace_root or "").strip():
            stage_root = str(
                Path(self._sessions_root)
                / "_conversation_attachments"
                / thread.thread_id
            )

        return resolve_attachments(
            list(turn.attachments),
            read_content=self._conv.read_artifact_content,
            get_artifact=get_artifact,
            authorized_sha256s=authorized,
            workspace_root=workspace_root,
            stage_root=stage_root,
            image_input=image_input,
            stage=True,
        )

    def _open_session_for_thread(self, thread_id: str) -> Optional[AgentSession]:
        """Recover an unclosed AgentSession when ThreadState lost its pointer (#122)."""
        selection = self._manager.runtime_selection(thread_id)
        open_rows = [
            item for item in self._store.list(AgentSession, thread_id=thread_id)
            if item.closed_at is None
        ]
        if not open_rows:
            return None
        matching = [
            row for row in open_rows
            if row.adapter_id == selection.adapter_id
            and (row.runtime_instance_id or "default")
            == (selection.instance_id or "default")
        ]
        candidates = matching or open_rows
        state = self._conv.get_state(thread_id)
        if state.running_turn_id:
            turn = self._conv.get_turn(state.running_turn_id)
            wanted = str(getattr(turn, "agent_session_id", "") or "")
            if wanted:
                for row in candidates:
                    if row.agent_session_id == wanted:
                        return row
        # Newest open session for this thread.
        candidates.sort(
            key=lambda row: row.created_at or row.agent_session_id,
            reverse=True,
        )
        return candidates[0]

    def _session_record(self, thread_id: str) -> Optional[AgentSession]:
        state = self._conv.get_state(thread_id)
        record: Optional[AgentSession] = None
        if state.agent_session_id:
            record = self._store.get(AgentSession, state.agent_session_id)
            if record is None:
                # Adapter 未接线 PlatformStore 时，Session 只在进程内 tracker。
                selection = self._manager.runtime_selection(thread_id)
                adapter = self._registry.get(
                    selection.adapter_id, selection.instance_id or "default")
                tracker = (
                    getattr(adapter, "_tracker", None)
                    if adapter is not None else None
                )
                getter = getattr(tracker, "get", None)
                if getter is not None:
                    record = getter(state.agent_session_id)
            if record is not None and record.closed_at is None:
                return record
        # #122: ThreadState may have lost agent_session_id while the Runtime
        # session (and turn claim) are still live — rebind from store.
        recovered = self._open_session_for_thread(thread_id)
        if recovered is not None:
            if state.agent_session_id != recovered.agent_session_id:
                self._conv.save_state(state.model_copy(update={
                    "agent_session_id": recovered.agent_session_id,
                }))
            return recovered
        return record

    @staticmethod
    def _ref_from_record(record: AgentSession) -> AgentSessionRef:
        return AgentSessionRef(
            agent_session_id=record.agent_session_id,
            adapter_id=record.adapter_id,
            external_session_id=record.external_session_id,
            runtime_instance_id=record.runtime_instance_id,
            resume_handle=record.resume_handle,
        )

    async def ensure_capability_session(
        self, thread_id: str
    ) -> tuple[Any, AgentSessionRef]:
        """为能力目录建立真实 Runtime session，但不产生模型 Turn。"""
        thread = self._manager.get_thread(thread_id)
        if thread is None:
            raise LookupError(f"unknown thread: {thread_id}")
        selection = self._manager.runtime_selection(thread_id)
        if not selection.adapter_id:
            raise LookupError(f"thread {thread_id} 未选择 Runtime instance")
        state = self._conv.get_state(thread_id)
        # #122: never close/replace the live turn's session for a capability probe.
        turn_busy = bool(
            state.running_turn_id or self._conv.active_turn_id(thread_id)
        )
        record = self._session_record(thread_id)
        if turn_busy:
            if record is None or record.closed_at is not None:
                raise LookupError(
                    f"thread {thread_id} 有执行中的 Turn，但没有可复用的 AgentSession"
                )
            adapter = self._adapter_for(
                selection.adapter_id, selection.instance_id)
            if record.agent_session_id not in self._live:
                self._live.add(record.agent_session_id)
            return adapter, self._ref_from_record(record)
        same_selection = False
        if record is not None:
            old_key = (
                f"{record.adapter_id}:"
                f"{record.runtime_instance_id or 'default'}"
            )
            same_selection = old_key == selection.runtime_key and (
                state.session_runtime_key == selection.session_key
                or (
                    not selection.credential_id
                    and state.session_runtime_key in {"", old_key}
                )
            )
            if record.closed_at is None and same_selection:
                adapter = self._adapter_for(
                    selection.adapter_id, selection.instance_id)
                if record.agent_session_id in self._live:
                    return adapter, self._ref_from_record(record)
            elif record.closed_at is None:
                await self._close_record(record, reason="runtime_switch")
                state = state.model_copy(update={
                    "agent_session_id": None,
                    "session_runtime_key": "",
                    "current_generation": state.current_generation + 1,
                })
                self._conv.save_state(state)
                record = None

        adapter = self._adapter_for(selection.adapter_id, selection.instance_id)
        workspace = (
            self._manager.get_workspace(str(thread.workspace_id or ""))
            if thread.workspace_id else None
        )
        engine = engine_for_adapter(selection.adapter_id)
        credential_env: dict[str, str] = {}
        if selection.credential_id:
            credential_env = resolve_credential_env(
                selection.credential_id,
                engine=engine,
                sessions_root=self._sessions_root,
                container=False,
                model=selection.model,
                agent_state_dir=(
                    self._sessions_root / "_conversation_agent_state" / thread_id
                ),
            ).env
        options: dict[str, Any] = {
            "principal_id": "local-user",
            "thread_mode": thread.mode,
            "cwd": (
                str(Path(workspace.root_path).expanduser().resolve())
                if workspace and workspace.root_path else ""
            ),
            "credential_id": selection.credential_id,
            "capability_discovery": True,
        }
        if credential_env:
            options["env"] = credential_env
        resume = record if record is not None and same_selection else None
        ref = await adapter.start(SessionStart(
            **({"agent_session_id": resume.agent_session_id}
               if resume is not None else {}),
            thread_id=thread_id,
            execution_generation=state.current_generation,
            workspace_id=thread.workspace_id,
            resume_handle=resume.resume_handle if resume is not None else None,
            model=selection.model or None,
            effort=selection.effort or None,
            access_mode=selection.access_mode or None,
            permission_mode=selection.permission_mode or None,
            sandbox_mode=selection.sandbox_mode or None,
            options=options,
        ))
        self._live.add(ref.agent_session_id)
        self._conv.save_state(state.model_copy(update={
            "agent_session_id": ref.agent_session_id,
            "session_runtime_key": selection.session_key,
        }))
        return adapter, ref

    async def runtime_capabilities(
        self, thread_id: str
    ) -> RuntimeCapabilitySnapshot:
        adapter, ref = await self.ensure_capability_session(thread_id)
        snapshot = await adapter.runtime_capability_snapshot(ref)
        snapshot = self._enrich_capability_snapshot(
            thread_id, snapshot, adapter_id=adapter.id,
            instance_id=getattr(adapter.identity, "instance_id", "default"),
        )
        self._capability_cache[thread_id] = snapshot
        self._capability_failures.pop(thread_id, None)
        return snapshot

    def cached_runtime_capabilities(
        self, thread_id: str
    ) -> Optional[RuntimeCapabilitySnapshot]:
        return self._capability_cache.get(thread_id)

    def _capability_runtime_key(self, thread_id: str) -> str:
        selection = self._manager.runtime_selection(thread_id)
        return str(getattr(selection, "session_key", "") or (
            f"{selection.adapter_id}:{selection.instance_id or 'default'}"
        ))

    def capability_refresh_failure(
        self, thread_id: str,
    ) -> Optional[dict[str, Any]]:
        """Return a public copy of the last refresh failure, if any (#188)."""
        row = self._capability_failures.get(thread_id)
        if row and row.get("runtime_key") != self._capability_runtime_key(thread_id):
            self._capability_failures.pop(thread_id, None)
            row = None
        if not row:
            return None
        now = time.monotonic()
        next_retry = float(row.get("next_retry_at") or 0.0)
        return {
            "error": str(row.get("error") or ""),
            "failed_at": float(row.get("failed_at") or 0.0),
            "attempts": int(row.get("attempts") or 0),
            "backoff_seconds": float(row.get("backoff_seconds") or 0.0),
            "retry_after_seconds": max(0.0, next_retry - now),
        }

    def capability_refresh_in_flight(self, thread_id: str) -> bool:
        current = self._capability_tasks.get(thread_id)
        return current is not None and not current.done()

    def _capability_refresh_allowed(self, thread_id: str) -> bool:
        if self.capability_refresh_in_flight(thread_id):
            return False
        failure = self.capability_refresh_failure(thread_id)
        return failure is None or failure["retry_after_seconds"] <= 0

    def interaction_matrix_for(
        self, thread_id: str, *, adapter_id: str = "", instance_id: str = "default",
    ) -> dict[str, Any]:
        """Thread 视图与 composer 共用的矩阵公开载荷。

        Cache hit requires full Runtime identity match (adapter_id +
        instance_id). A thread-scoped snapshot for another Provider must
        not be returned as the requested Provider's confirmed matrix.
        """
        snapshot = self._capability_cache.get(thread_id)
        selection = self._manager.runtime_selection(thread_id)
        aid = adapter_id or selection.adapter_id
        iid = instance_id or selection.instance_id or "default"
        iid = iid or "default"
        record = self._registry.record(aid, iid)
        report = record.last_probe if record is not None else None

        def _identity_matches(snap: RuntimeCapabilitySnapshot | None) -> bool:
            if snap is None:
                return False
            return (
                snap.adapter_id == aid
                and (snap.instance_id or "default") == iid
            )

        if (
            snapshot is not None
            and snapshot.matrix is not None
            and _identity_matches(snapshot)
        ):
            return snapshot.public_matrix() or {}

        matching = snapshot if _identity_matches(snapshot) else None
        # Stamp requested identity when probe/snapshot are absent so the
        # conservative/unknown matrix does not inherit another Provider.
        identity_carrier = matching
        if identity_carrier is None and aid:
            identity_carrier = RuntimeCapabilitySnapshot(
                adapter_id=aid,
                instance_id=iid,
                revision=0,
                stale=True,
                diagnostics=["尚未取得会话级能力目录"],
            )
        matrix = build_matrix_from_probe(
            report,
            revision=matching.revision if matching is not None else 0,
            stale=True if matching is None else matching.stale,
            snapshot=identity_carrier,
            diagnostics=(
                list(matching.diagnostics)
                if matching is not None
                else ["尚未取得会话级能力目录"]
            ),
        )
        return matrix.model_dump(mode="json")

    def _enrich_capability_snapshot(
        self,
        thread_id: str,
        snapshot: RuntimeCapabilitySnapshot,
        *,
        adapter_id: str,
        instance_id: str,
    ) -> RuntimeCapabilitySnapshot:
        record = self._registry.record(adapter_id, instance_id)
        report = record.last_probe if record is not None else None
        return attach_matrix_to_snapshot(snapshot, report)

    def _emit_capabilities_updated(
        self, thread_id: str, snapshot: RuntimeCapabilitySnapshot,
    ) -> None:
        """Push capability revision to the thread SSE so the UI can rehydrate."""
        matrix = snapshot.public_matrix() or {}
        self._emit(thread_id, ev.EV_RUNTIME_CAPABILITIES_UPDATED, {
            "revision": int(snapshot.revision),
            "stale": bool(snapshot.stale),
            "adapter_id": snapshot.adapter_id,
            "diagnostics": list(snapshot.diagnostics),
            "matrix": matrix,
        })

    def _record_capability_refresh_failure(
        self, thread_id: str, exc: BaseException,
    ) -> RuntimeCapabilitySnapshot:
        """Persist failure snapshot + backoff so detail reads stop spinning (#188)."""
        now_mono = time.monotonic()
        previous = self.capability_refresh_failure(thread_id) or {}
        attempts = int(previous.get("attempts") or 0) + 1
        delay = min(
            _CAPABILITY_REFRESH_BACKOFF_MAX_S,
            _CAPABILITY_REFRESH_BACKOFF_BASE_S * (2 ** max(0, attempts - 1)),
        )
        error = f"{type(exc).__name__}: {str(exc)[:300]}"
        self._capability_failures[thread_id] = {
            "runtime_key": self._capability_runtime_key(thread_id),
            "error": error,
            "failed_at": time.time(),
            "attempts": attempts,
            "backoff_seconds": delay,
            "next_retry_at": now_mono + delay,
        }
        selection = self._manager.runtime_selection(thread_id)
        previous_snap = self._capability_cache.get(thread_id)
        adapter_id = str(
            getattr(selection, "adapter_id", "")
            or (previous_snap.adapter_id if previous_snap is not None else "")
            or ""
        )
        instance_id = str(
            getattr(selection, "instance_id", "")
            or (previous_snap.instance_id if previous_snap is not None else "")
            or "default"
        )
        if previous_snap is not None and (
            previous_snap.adapter_id != adapter_id
            or (previous_snap.instance_id or "default") != instance_id
        ):
            previous_snap = None
        diagnostics = [
            f"Runtime 能力刷新失败：{error}",
            f"已尝试 {attempts} 次；约 {int(delay)}s 后可重试",
        ]
        snapshot = RuntimeCapabilitySnapshot(
            adapter_id=adapter_id,
            instance_id=instance_id,
            agent_session_id=(
                previous_snap.agent_session_id if previous_snap is not None else ""
            ),
            external_session_id=(
                previous_snap.external_session_id
                if previous_snap is not None else None
            ),
            revision=int(previous_snap.revision) if previous_snap is not None else 0,
            stale=True,
            items=list(previous_snap.items) if previous_snap is not None else [],
            diagnostics=diagnostics,
            matrix=(
                previous_snap.matrix if previous_snap is not None else None
            ),
        )
        try:
            snapshot = self._enrich_capability_snapshot(
                thread_id,
                snapshot,
                adapter_id=adapter_id,
                instance_id=instance_id,
            )
        except Exception:  # noqa: BLE001 — 失败快照仍要落缓存
            LOG.exception(
                "capability failure matrix enrich failed: %s", thread_id,
            )
        self._capability_cache[thread_id] = snapshot
        return snapshot

    def refresh_runtime_capabilities(self, thread_id: str) -> bool:
        """后台刷新目录；同一 Thread 同时只允许一个探测任务。

        刷新成功后发出 ``core.runtime.capabilities_updated``，让已连接的
        SSE 客户端 ``scheduleRefresh`` 拉到新的 matrix revision（#119）。

        失败时写入诊断快照、失败事件与退避状态，避免详情重复读取无限重试
        （#188）。返回是否真正启动了新的后台任务。
        """
        if not self._capability_refresh_allowed(thread_id):
            return False
        runtime_key = self._capability_runtime_key(thread_id)
        task = asyncio.ensure_future(self.runtime_capabilities(thread_id))
        self._capability_tasks[thread_id] = task

        def _finished(done: asyncio.Task[RuntimeCapabilitySnapshot]) -> None:
            self._capability_tasks.pop(thread_id, None)
            try:
                snapshot = done.result()
            except asyncio.CancelledError:
                return
            except Exception as exc:  # noqa: BLE001 — 失败快照 + 事件可见
                if self._capability_runtime_key(thread_id) != runtime_key:
                    return
                LOG.exception("runtime capability refresh failed: %s", thread_id)
                try:
                    snapshot = self._record_capability_refresh_failure(
                        thread_id, exc,
                    )
                    failure = self._capability_failures.get(thread_id) or {}
                    self._emit(thread_id, ev.EV_RUNTIME_ERROR, {
                        "code": "conversation.runtime.capability_refresh_failed",
                        "detail": str(failure.get("error") or exc)[:320],
                        "attempts": int(failure.get("attempts") or 0),
                        "backoff_seconds": float(
                            failure.get("backoff_seconds") or 0.0
                        ),
                        "recoverable": True,
                    })
                    self._emit_capabilities_updated(thread_id, snapshot)
                except Exception:  # noqa: BLE001 — 失败路径仍不抛回事件环
                    LOG.exception(
                        "runtime capability failure event failed: %s",
                        thread_id,
                    )
                return
            if self._capability_runtime_key(thread_id) != runtime_key:
                return
            self._capability_cache[thread_id] = snapshot
            self._capability_failures.pop(thread_id, None)
            try:
                self._emit_capabilities_updated(thread_id, snapshot)
            except Exception:  # noqa: BLE001 — 事件失败不影响缓存
                LOG.exception(
                    "runtime capability update event failed: %s", thread_id,
                )

        task.add_done_callback(_finished)
        return True

    async def runtime_operation(
        self, thread_id: str, name: str
    ) -> dict[str, Any]:
        adapter, ref = await self.ensure_capability_session(thread_id)
        snapshot = await adapter.runtime_capability_snapshot(ref)
        snapshot = self._enrich_capability_snapshot(
            thread_id, snapshot, adapter_id=adapter.id,
            instance_id=getattr(adapter.identity, "instance_id", "default"),
        )
        self._capability_cache[thread_id] = snapshot
        verified = next((
            item for item in snapshot.items
            if item.kind == "operation"
            and item.name.casefold() == name.casefold()
            and item.verification == "verified"
            and item.resolution == "client"
            and bool(getattr(item, "invocable", True))
        ), None)
        if verified is None:
            raise RuntimeError(f"Runtime operation 已失效：{name}")
        operation = getattr(adapter, "runtime_operation", None)
        if not callable(operation):
            raise RuntimeError(
                f"{adapter.id} 没有结构化 Runtime operation 通道")
        result = await operation(ref, name)
        return result if isinstance(result, dict) else {"result": result}

    async def ensure_session(
        self,
        thread: Thread,
        turn: TurnRecord,
        *,
        force_new: bool = False,
    ) -> tuple[Any, AgentSessionRef]:
        """取得该 Thread 当前可用的 (adapter, session)；必要时创建 / 恢复 / 切换。

        - 无活跃 Session：创建新 Session（七步 Binding 交付在 Adapter 内）；
        - 进程重启后：用保存的 resume_handle 接管同一 external session，
          保持同一 agent_session_id；
        - Runtime 选择变化：关闭旧 Session（撤销 grant），generation+1，
          重新生成注入计划并启动新 Session；普通 message 也会做结构化交接。
        """
        selection = self._manager.runtime_selection(thread.thread_id)
        if not selection.adapter_id:
            raise LookupError(
                f"thread {thread.thread_id} 未选择 Runtime instance")
        record = self._session_record(thread.thread_id)
        state = self._conv.get_state(thread.thread_id)
        previous_session_key = str(state.session_runtime_key or "")
        previous_runtime_key = ""
        previous_adapter_id = ""
        previous_agent_session_id = ""
        if record is not None:
            previous_runtime_key = (
                f"{record.adapter_id}:{record.runtime_instance_id or 'default'}"
            )
            previous_adapter_id = str(record.adapter_id or "")
            previous_agent_session_id = str(record.agent_session_id or "")

        if force_new:
            if record is not None and record.closed_at is None:
                await self._close_record(record, reason="turn_retry")
            state = self._conv.save_state(state.model_copy(update={
                "agent_session_id": None,
                "session_runtime_key": "",
            }))
            adapter = self._adapter_for(
                selection.adapter_id, selection.instance_id,
            )
            return await self._start(
                adapter, thread, turn, state,
                force_new=True,
                previous_session_key=previous_session_key,
                previous_runtime_key=previous_runtime_key,
                previous_adapter_id=previous_adapter_id,
                previous_agent_session_id=previous_agent_session_id,
            )

        switched = False
        if record is not None:
            old_key = f"{record.adapter_id}:{record.runtime_instance_id or 'default'}"
            same_selection = old_key == selection.runtime_key and (
                state.session_runtime_key == selection.session_key
                or (
                    not selection.credential_id
                    and state.session_runtime_key in {"", old_key}
                )
            )
            if same_selection and (record.closed_at is None or record.resume_handle):
                adapter = self._adapter_for(selection.adapter_id, selection.instance_id)
                if record.closed_at is None and record.agent_session_id in self._live:
                    # 本进程内活跃：直接复用。
                    ref = self._ref_from_record(record)
                    self._pending_handoff.pop(thread.thread_id, None)
                    self._conv.save_turn(turn.model_copy(update={
                        "agent_session_id": ref.agent_session_id,
                        "execution_generation": state.current_generation,
                    }))
                    return adapter, ref
                # 重启后接管：同一 agent_session_id + resume_handle。
                return await self._start(
                    adapter, thread, turn, state,
                    agent_session_id=record.agent_session_id,
                    resume_handle=record.resume_handle,
                    previous_session_key=previous_session_key,
                    previous_runtime_key=previous_runtime_key,
                    previous_adapter_id=previous_adapter_id,
                    previous_agent_session_id=previous_agent_session_id,
                )
            # 切换 Runtime 或旧 Session 已关闭：撤销旧 grant 并抬升执行代。
            if record.closed_at is None:
                await self._close_record(record, reason="runtime_switch")
                switched = True

        adapter = self._adapter_for(selection.adapter_id, selection.instance_id)
        if switched:
            state = state.model_copy(
                update={"current_generation": state.current_generation + 1})
        return await self._start(
            adapter, thread, turn, state, switched=switched,
            previous_session_key=previous_session_key,
            previous_runtime_key=previous_runtime_key,
            previous_adapter_id=previous_adapter_id,
            previous_agent_session_id=previous_agent_session_id,
        )

    async def _start(
        self,
        adapter: Any,
        thread: Thread,
        turn: TurnRecord,
        state: ThreadState,
        *,
        agent_session_id: str = "",
        resume_handle: Optional[str] = None,
        switched: bool = False,
        force_new: bool = False,
        previous_session_key: str = "",
        previous_runtime_key: str = "",
        previous_adapter_id: str = "",
        previous_agent_session_id: str = "",
    ) -> tuple[Any, AgentSessionRef]:
        selection = self._manager.runtime_selection(thread.thread_id)
        workspace = (
            self._manager.get_workspace(thread.workspace_id)
            if thread.workspace_id else None)
        engine = engine_for_adapter(selection.adapter_id)
        credential_env: dict[str, str] = {}
        if selection.credential_id:
            credential_env = resolve_credential_env(
                selection.credential_id,
                engine=engine,
                sessions_root=self._sessions_root,
                container=False,
                model=selection.model,
                agent_state_dir=(
                    self._sessions_root / "_conversation_agent_state"
                    / thread.thread_id
                ),
            ).env

        reason = classify_rebuild_reason(
            switched=switched,
            force_new=force_new,
            turn_kind=str(turn.kind or ""),
            previous_session_key=previous_session_key,
            new_session_key=selection.session_key,
            previous_runtime_key=previous_runtime_key,
            new_runtime_key=selection.runtime_key,
        )
        messages = self._conv.list_current_messages(thread.thread_id)
        turns = self._conv.list_turns(thread.thread_id)
        native_resume = bool(resume_handle) and not switched and not force_new
        if native_resume:
            recovery_kind = RECOVERY_NATIVE_RESUME
            handoff = build_session_handoff(
                thread_id=thread.thread_id,
                messages=messages,
                turns=turns,
                exclude_turn_id=turn.turn_id,
                kind=RECOVERY_NATIVE_RESUME,
                reason=reason or REASON_RESTART,
                generation=state.current_generation,
                source_adapter_id=previous_adapter_id,
                source_agent_session_id=previous_agent_session_id,
            )
            resume_prompt = turn.text or "Continue from where you left off."
            self._pending_handoff.pop(thread.thread_id, None)
        else:
            handoff = build_session_handoff(
                thread_id=thread.thread_id,
                messages=messages,
                turns=turns,
                exclude_turn_id=turn.turn_id,
                kind=RECOVERY_STRUCTURED_HANDOFF,
                reason=reason,
                generation=state.current_generation,
                source_adapter_id=previous_adapter_id,
                source_agent_session_id=previous_agent_session_id,
            )
            recovery_kind = handoff.kind
            if handoff.included:
                if turn.kind == "resume":
                    action = "继续"
                elif turn.kind in {TURN_KIND_RETRY, TURN_KIND_EDIT_RESEND}:
                    action = "重试"
                else:
                    action = "切换/重建后继续"
                resume_prompt = render_resume_prompt(
                    handoff, turn.text or "Continue from where you left off.",
                    action=action,
                )
                self._pending_handoff[thread.thread_id] = handoff
            else:
                resume_prompt = turn.text or "Continue from where you left off."
                self._pending_handoff.pop(thread.thread_id, None)

        options: dict[str, Any] = {
            "principal_id": "local-user",
            "thread_mode": thread.mode,
            "cwd": (
                str(Path(workspace.root_path).expanduser().resolve())
                if workspace and workspace.root_path else ""
            ),
            "resume_prompt": resume_prompt,
            "credential_id": selection.credential_id,
            "handoff": handoff.to_dict() if handoff is not None else {},
            "recovery_kind": recovery_kind,
            "recovery_reason": reason,
        }
        if credential_env:
            options["env"] = credential_env
        request = SessionStart(
            **({"agent_session_id": agent_session_id} if agent_session_id else {}),
            thread_id=thread.thread_id,
            run_id=turn.run_id,
            execution_generation=state.current_generation,
            workspace_id=thread.workspace_id,
            resume_handle=resume_handle,
            model=selection.model or None,
            effort=selection.effort or None,
            access_mode=selection.access_mode or None,
            permission_mode=selection.permission_mode or None,
            sandbox_mode=selection.sandbox_mode or None,
            options=options,
        )
        ref = await adapter.start(request)
        self._live.add(ref.agent_session_id)
        self._conv.save_turn(turn.model_copy(update={
            "agent_session_id": ref.agent_session_id,
            "execution_generation": state.current_generation,
        }))
        run = self._conv.get_run(turn.run_id or "")
        if run is not None:
            self._conv.save_run(run.model_copy(update={
                "agent_session_id": ref.agent_session_id,
                "adapter_id": ref.adapter_id,
                "runtime_instance_id": ref.runtime_instance_id or "default",
                "generation": state.current_generation,
            }))
        event_payload: dict[str, Any] = {
            "generation": state.current_generation,
            "runtime_key": selection.session_key,
            "adapter_runtime_key": selection.runtime_key,
            "credential_id": selection.credential_id,
            "agent_session_id": ref.agent_session_id,
            "adapter_id": ref.adapter_id,
            "instance_id": ref.runtime_instance_id or "default",
            "model": selection.model,
            "effort": selection.effort,
            "access_mode": selection.access_mode,
            "permission_mode": selection.permission_mode,
            "runtime": self._selection_snapshot(selection),
            "recovery_kind": recovery_kind,
            "recovery_reason": reason,
        }
        if handoff is not None:
            event_payload.update(handoff.to_event_payload())
        # Always persist state for the new session.
        self._conv.save_state(state.model_copy(update={
            "agent_session_id": ref.agent_session_id,
            "session_runtime_key": selection.session_key,
            "current_generation": state.current_generation,
        }))
        if switched or force_new or (
            handoff is not None and handoff.included and not native_resume
        ):
            self._emit(thread.thread_id, ev.EV_RUNTIME_SWITCHED, event_payload)
        return adapter, ref

    async def _close_record(
        self, record: AgentSession, *, reason: str, strict: bool = False,
    ) -> None:
        """关闭一个活跃 Session：Adapter close（联动撤 grant）+ 事件。"""
        adapter = self._registry.get(
            record.adapter_id, record.runtime_instance_id or "default")
        if adapter is not None:
            try:
                await adapter.close(AgentSessionRef(
                    agent_session_id=record.agent_session_id,
                    adapter_id=record.adapter_id,
                    external_session_id=record.external_session_id,
                    runtime_instance_id=record.runtime_instance_id,
                    resume_handle=record.resume_handle,
                ))
            except Exception:  # noqa: BLE001 — 关闭失败不阻断切换
                if strict:
                    raise
                LOG.exception("close agent session failed: %s",
                              record.agent_session_id)
        elif strict:
            raise RuntimeError("Runtime 已确认中断，但无法找到接入点以关闭其资源")
        self._live.discard(record.agent_session_id)
        if record.thread_id:
            self._capability_cache.pop(record.thread_id, None)
            self._capability_failures.pop(record.thread_id, None)
        if record.thread_id:
            self._emit(record.thread_id, ev.EV_SESSION_CLOSED, {
                "agent_session_id": record.agent_session_id,
                "reason": reason,
                "resume_available": reason == "operator_stopped" and bool(record.resume_handle),
            })

    async def close_thread(
        self, thread_id: str, *, reason: str = "thread_archived"
    ) -> bool:
        """关闭 Thread 的活跃 Session，并返回是否实际关闭。"""
        record = self._session_record(thread_id)
        if record is not None and record.closed_at is None:
            await self._close_record(record, reason=reason)
            return True
        return False

    async def _stop_detached_runtime(
        self, thread_id: str, *, reason: str
    ) -> None:
        """对话侧已不再消费 Runtime 时关掉 Session。

        八个引擎共用：close → teardown 进程 + 撤销 grant。
        否则引擎会继续下发或停止任务，对话时间线却不再记录。
        """
        try:
            await self.close_thread(thread_id, reason=reason)
        except Exception:  # noqa: BLE001
            LOG.exception(
                "stop detached conversation runtime failed: %s", thread_id)

    @staticmethod
    def _turn_failure_keeps_session(payload: dict[str, Any]) -> bool:
        reason = str(
            payload.get("reason") or payload.get("stop_reason") or "")
        return reason in _KEEP_SESSION_TURN_REASONS

    async def reload_thread_capabilities(self, thread_id: str) -> dict[str, bool]:
        """Binding 变化后终止旧 Turn/Session，让下一轮签发新版 Grant。"""
        task = self._tasks.get(thread_id)
        interrupted = bool(task is not None and not task.done())
        if interrupted:
            try:
                await self.interrupt(thread_id)
            except Exception:  # noqa: BLE001 — 仍需关闭携带旧 Grant 的会话
                LOG.exception(
                    "interrupt before capability reload failed: %s", thread_id)
        restarted = await self.close_thread(
            thread_id, reason="capability_binding_changed")
        return {"interrupted": interrupted, "session_closed": restarted}

    async def close_all(self, *, reason: str) -> int:
        """关闭全部活动 Runtime Session，供 Secret/权限轮换使用。"""
        records = [
            item for item in self._store.list(AgentSession)
            if item.closed_at is None
        ]
        for record in records:
            await self._close_record(record, reason=reason)
        return len(records)

    # -- Turn 执行 ------------------------------------------------------------------

    def start_turn(self, turn_id: str) -> None:
        """启动一个 Turn 的后台执行任务（异步命令模式的副作用）。"""
        turn = self._conv.get_turn(turn_id)
        if turn is None:
            raise LookupError(f"unknown turn: {turn_id}")
        current = self._tasks.get(turn.thread_id)
        if current is not None and not current.done():
            raise RuntimeError(f"thread {turn.thread_id} already has a running task")
        task = asyncio.ensure_future(self._run_guarded(turn))
        self._tasks[turn.thread_id] = task

    async def start_next_queued(self, thread_id: str) -> bool:
        """线程空闲时原子提升队首消息，并启动对应 Turn。"""
        lock = self._queue_locks.setdefault(thread_id, asyncio.Lock())
        async with lock:
            task = self._tasks.get(thread_id)
            state = self._conv.get_state(thread_id)
            if (
                (task is not None and not task.done())
                or state.status != "active"
                or state.running_turn_id
                or state.queue_paused
                or self._conv.active_turn_id(thread_id)
            ):
                return False
            item = self._conv.next_queue_item(thread_id)
            if item is None:
                return False
            if item.status == "failed":
                return False

            try:
                # Consume the atomically claimed, latest payload. An edit may
                # have committed after next_queue_item returned its snapshot.
                item = self._conv.mark_queue_dispatching(item.queue_id)
            except ValueError:
                return False
            idem = f"queue:{item.queue_id}"
            try:
                if item.runtime:
                    self._manager.save_runtime_selection(thread_id, item.runtime)
                prior = self._conv.find_turn_by_idempotency(thread_id, idem)
                if prior is not None:
                    turn = prior
                else:
                    turn, _run, _task, _created = self._manager.request_turn(
                        thread_id,
                        text=item.text,
                        kind="message",
                        command_id=item.command_id,
                        idempotency_key=idem,
                        attachments=list(item.attachments),
                        capability_refs=list(item.capability_refs),
                        runtime_invocation=dict(item.runtime_invocation),
                    )
                self._conv.mark_queue_consumed(item.queue_id, turn.turn_id)
                self._store.append_events([
                    ev.thread_event(
                        thread_id,
                        ev.EV_QUEUE_PROMOTED,
                        {
                            "queue_id": item.queue_id,
                            "turn_id": turn.turn_id,
                            "queue_revision": self._conv.get_state(thread_id).queue_revision,
                        },
                        actor_id=item.actor_id,
                        command_id=item.command_id or None,
                        correlation_id=item.correlation_id or item.command_id or None,
                    ),
                    ev.thread_event(
                        thread_id,
                        ev.EV_TURN_REQUESTED,
                        {
                            "queue_id": item.queue_id,
                            "turn_id": turn.turn_id,
                            "run_id": turn.run_id,
                            "task_id": turn.task_id,
                            "seq": turn.seq,
                            "kind": turn.kind,
                            "text": turn.text,
                            "attachments": list(turn.attachments),
                            "capability_refs": list(turn.capability_refs),
                            "runtime_invocation": dict(turn.runtime_invocation),
                        },
                        actor_id=item.actor_id,
                        command_id=item.command_id or None,
                        correlation_id=item.correlation_id or item.command_id or None,
                    ),
                ])
                self.start_turn(turn.turn_id)
                return True
            except Exception as exc:  # noqa: BLE001 — 队列发送失败必须持久化
                LOG.exception("queue promotion failed: %s", item.queue_id)
                error = {
                    "code": "conversation.queue.dispatch_failed",
                    "message": f"{type(exc).__name__}: {str(exc)[:300]}",
                }
                self._conv.mark_queue_failed(item.queue_id, error)
                self._emit(thread_id, ev.EV_QUEUE_DISPATCH_FAILED, {
                    "queue_id": item.queue_id,
                    "error": error,
                }, command_id=item.command_id or None)
                self._emit(thread_id, ev.EV_QUEUE_PAUSED, {
                    "reason": "dispatch_failed",
                    "queue_id": item.queue_id,
                }, command_id=item.command_id or None)
                return False

    async def _run_guarded(self, turn: TurnRecord) -> None:
        # A second client may continue after the terminal event but before the
        # stop request has finished releasing the previous native session.
        fence = self._stop_fences.get(turn.thread_id)
        if fence is not None:
            await fence.wait()
        lock = self._locks.setdefault(turn.thread_id, asyncio.Lock())
        async with lock:  # 同一 Thread 串行执行 Turn
            try:
                await self._run_turn(turn)
            except asyncio.CancelledError:
                await self._stop_detached_runtime(
                    turn.thread_id, reason="turn_cancelled")
                raise
            except Exception as exc:  # noqa: BLE001 — 边界可见，不静默
                LOG.exception("turn failed: %s", turn.turn_id)
                self._emit(turn.thread_id, ev.EV_RUNTIME_ERROR, {
                    "turn_id": turn.turn_id,
                    "code": "conversation.turn.executor_error",
                    "detail": f"{type(exc).__name__}: {str(exc)[:300]}",
                })
                self._emit(turn.thread_id, ev.EV_TURN_FAILED, {
                    "turn_id": turn.turn_id,
                    "error": {
                        "code": "conversation.turn.executor_error",
                        "message": f"{type(exc).__name__}: {str(exc)[:300]}",
                    },
                }, command_id=turn.command_id or None)
                await self._stop_detached_runtime(
                    turn.thread_id, reason="turn_executor_error")
            finally:
                self._tasks.pop(turn.thread_id, None)
                current = self._conv.get_turn(turn.turn_id)
                if current is not None and current.status == TURN_COMPLETED:
                    await self.start_next_queued(turn.thread_id)
                elif (
                    current is not None
                    and current.status in {TURN_FAILED, TURN_INTERRUPTED}
                    and self._conv.list_queue(turn.thread_id)
                ):
                    reason = (
                        "turn_interrupted"
                        if current.status == TURN_INTERRUPTED
                        else "turn_failed"
                    )
                    state = self._conv.pause_queue(turn.thread_id, reason)
                    self._emit(turn.thread_id, ev.EV_QUEUE_PAUSED, {
                        "reason": reason,
                        "turn_id": turn.turn_id,
                        "queue_revision": state.queue_revision,
                    }, command_id=turn.command_id or None)

    async def _run_turn(self, turn: TurnRecord) -> None:
        thread = self._manager.get_thread(turn.thread_id)
        if thread is None:
            raise LookupError(f"unknown thread: {turn.thread_id}")
        resume_supported = True
        if turn.kind == "resume":
            selection = self._manager.runtime_selection(thread.thread_id)
            candidate = self._adapter_for(
                selection.adapter_id, selection.instance_id,
            )
            resume_supported = bool(
                (await candidate.capabilities()).resume
            )
        adapter, ref = await self.ensure_session(
            thread,
            turn,
            force_new=(
                turn.kind in {TURN_KIND_RETRY, TURN_KIND_EDIT_RESEND}
                or (turn.kind == "resume" and not resume_supported)
            ),
        )
        # #119: 会话已连接后主动刷新能力目录，勿依赖用户打开 / $ @ 命令菜单。
        cached_caps = self._capability_cache.get(thread.thread_id)
        if cached_caps is None or cached_caps.stale:
            self.refresh_runtime_capabilities(thread.thread_id)
        # Devin session/load restores history during ensure_session; it does
        # not run another turn. Send the current continuation prompt below.
        if (turn.kind == "resume" and resume_supported
                and adapter.id != "devin.acp"):
            # Native resume path: do not also dump structured handoff into send.
            self._pending_handoff.pop(thread.thread_id, None)
            stream = adapter.resume(ref)
        else:
            pending = self._pending_handoff.pop(thread.thread_id, None)
            text = turn.text
            if pending is not None and pending.included:
                # C38: ordinary message (and retry/resume/edit-resend without native resume)
                # after session rebuild — inject structured current-branch history.
                text = render_agent_text(pending, turn.text)
            elif turn.kind in {TURN_KIND_RETRY, TURN_KIND_EDIT_RESEND, "resume"}:
                text = _continuation_prompt(
                    turn,
                    self._conv.list_current_messages(thread.thread_id),
                    self._conv.list_turns(thread.thread_id),
                )
            elif turn.seq == 1:
                fork_tasks = [
                    item for item in self._store.list(Task, thread_id=thread.thread_id)
                    if item.kind == "conversation.fork"
                ]
                if fork_tasks:
                    history = self._conv.list_current_messages(thread.thread_id)
                    if history:
                        transcript = "\n".join(
                            f"{'用户' if item.role == 'user' else '助手'}：{item.text}"
                            for item in history
                        )
                        text = (
                            "以下是该分叉继承的对话历史，请在此上下文上继续：\n"
                            f"{transcript}\n\n当前用户消息：{turn.text}"
                        )
            selection = self._manager.runtime_selection(thread.thread_id)
            workspace = (
                self._manager.get_workspace(str(thread.workspace_id or ""))
                if thread.workspace_id else None
            )
            _, capability_context = resolve_capability_refs(
                turn.capability_refs,
                engine=engine_for_adapter(selection.adapter_id),
                workspace_root=str(workspace.root_path if workspace is not None else ""),
                extension_service=self._manager.extension_service,
                threads=self._manager.list_threads(),
                message_loader=self._conv.list_current_messages,
                message_lookup=self._conv.get_message,
            )
            runtime_invocation = dict(turn.runtime_invocation or {})
            if runtime_invocation:
                snapshot = await adapter.runtime_capability_snapshot(ref)
                snapshot = self._enrich_capability_snapshot(
                    thread.thread_id, snapshot, adapter_id=adapter.id,
                    instance_id=getattr(
                        adapter.identity, "instance_id", "default"),
                )
                self._capability_cache[thread.thread_id] = snapshot
                client_revision = runtime_invocation.get("revision")
                if (
                    client_revision is not None
                    and int(client_revision) != int(snapshot.revision)
                ):
                    raise RuntimeError(
                        "能力目录 revision 已变化，请重新打开命令目录选择")
                if snapshot.stale:
                    raise RuntimeError(
                        "Runtime 能力目录已经过期，请重新打开命令目录选择")
                matched = _match_runtime_capability(
                    snapshot, runtime_invocation,
                )
                if matched is None:
                    raise RuntimeError(
                        "所选 Runtime 命令已失效，请重新打开命令目录选择")
                runtime_invocation = matched.model_dump(mode="json")
                runtime_invocation["arguments"] = str(
                    turn.runtime_invocation.get("arguments") or "")
                runtime_invocation["revision"] = snapshot.revision
                if matched.kind == "operation" and matched.resolution == "client":
                    result = await adapter.runtime_operation(ref, matched.name)
                    result_text = json.dumps(
                        result, ensure_ascii=False, indent=2, default=str)
                    self._emit(thread.thread_id, ev.EV_MESSAGE_COMPLETED, {
                        "turn_id": turn.turn_id,
                        "role": "assistant",
                        "text": result_text,
                        "runtime_operation": matched.name,
                    }, command_id=turn.command_id or None)
                    self._emit(thread.thread_id, ev.EV_TURN_COMPLETED, {
                        "turn_id": turn.turn_id,
                        "runtime_operation": matched.name,
                    }, command_id=turn.command_id or None)
                    return
            if capability_context:
                text = (
                    "[用户显式添加的参考上下文]\n"
                    f"{capability_context}\n\n[当前用户请求]\n{text}"
                )
            attachment_payload: list[dict[str, Any]] = []
            if turn.attachments:
                try:
                    resolved = await self._resolve_turn_attachments(
                        thread, turn, adapter,
                        workspace_root=str(
                            workspace.root_path if workspace is not None else ""
                        ),
                    )
                except AttachmentDeliveryError as exc:
                    self._emit(thread.thread_id, ev.EV_TURN_FAILED, {
                        "turn_id": turn.turn_id,
                        "error": {
                            "code": exc.code,
                            "message": exc.message,
                        },
                    }, command_id=turn.command_id or None)
                    return
                attachment_payload = attachments_payload(resolved)
                text = merge_attachment_context(text, resolved)
            stream = adapter.send(ref, AgentInput(
                kind="message",
                text=text,
                payload={
                    "attachments": attachment_payload,
                    "capability_context": "",
                    "runtime_capability": runtime_invocation,
                    "runtime_command_arguments": str(
                        runtime_invocation.get("arguments") or ""
                    ),
                },
            ))
        await self._consume(thread.thread_id, turn, stream)

    async def _consume(
        self, thread_id: str, turn: Optional[TurnRecord], stream: Any,
        *, require_control_delivery: bool = False,
    ) -> None:
        """消费 Adapter 事件流并翻译为 Thread Public Event。"""
        interrupted = False
        runtime_error: Optional[dict[str, Any]] = None
        saw_completed_assistant = False
        detach_reason: Optional[str] = None
        try:
            async for event in stream:
                if event.event_type is AgentEventType.RUNTIME_ERROR:
                    runtime_error = dict(event.payload)
                if event.event_type is AgentEventType.RUNTIME_EXITED:
                    detach_reason = "runtime_exited"
                if (
                    event.event_type is AgentEventType.TURN_FAILED
                    and not self._turn_failure_keeps_session(
                        dict(event.payload or {}))
                ):
                    detach_reason = "turn_failed"
                if require_control_delivery and event.event_type in {
                    AgentEventType.RUNTIME_ERROR,
                    AgentEventType.TURN_FAILED,
                    AgentEventType.RUNTIME_EXITED,
                }:
                    # Control streams may report rejection as an event and end
                    # normally. Let the command handler retain the pending item;
                    # publishing a successful resolved event would lose it.
                    payload = dict(event.payload or {})
                    raise ControlDeliveryError(str(
                        payload.get("code") or event.event_type.value))
                if (require_control_delivery
                        and event.event_type is AgentEventType.APPROVAL_RESOLVED):
                    # The command publishes resolution after the complete
                    # response stream succeeds. Other runtime streams still
                    # translate autonomous resolution normally.
                    continue
                if event.event_type is AgentEventType.MESSAGE_COMPLETED:
                    payload = dict(event.payload or {})
                    role = str(payload.get("role") or "assistant").lower()
                    saw_completed_assistant = (
                        role in {"assistant", "agent"}
                        and bool(str(payload.get("text") or "").strip())
                    ) or saw_completed_assistant
                if (event.event_type is AgentEventType.TURN_COMPLETED
                        and not saw_completed_assistant):
                    self._emit(thread_id, ev.EV_TURN_FAILED, {
                        "turn_id": turn.turn_id if turn else event.turn_id,
                        "error": {
                            "code": "conversation.empty_assistant",
                            "message": "Runtime completed without assistant text",
                        },
                    }, command_id=(turn.command_id or None) if turn else None)
                    continue
                interrupted = self._translate(thread_id, turn, event) or interrupted
            if interrupted and turn is not None:
                current = self._conv.get_turn(turn.turn_id)
                if current is not None and current.status == TURN_RUNNING:
                    self._emit(thread_id, ev.EV_TURN_INTERRUPTED, {
                        "turn_id": turn.turn_id,
                        "phase": "runtime_exited",
                    }, command_id=turn.command_id or None)
                return
            if turn is None:
                return
            current = self._conv.get_turn(turn.turn_id)
            state = self._conv.get_state(thread_id)
            if (
                current is not None
                and current.status in (TURN_QUEUED, TURN_RUNNING)
                and not has_actionable_approvals(
                    state.pending_approvals, state.pending_approval
                )
                and state.pending_user_input is None
            ):
                error = runtime_error or {
                    "code": "conversation.turn.stream_ended",
                    "detail": "runtime event stream ended without a terminal turn event",
                }
                self._emit(thread_id, ev.EV_TURN_FAILED, {
                    "turn_id": turn.turn_id,
                    "error": error,
                }, command_id=turn.command_id or None)
                detach_reason = "stream_ended"
        except asyncio.CancelledError:
            detach_reason = "consume_cancelled"
            raise
        finally:
            if detach_reason:
                await self._stop_detached_runtime(
                    thread_id, reason=f"conversation_runtime_detached:{detach_reason}")

    # -- AgentEvent → Thread 事件 ------------------------------------------------

    def _translate(
        self, thread_id: str, turn: Optional[TurnRecord], event: AgentEvent
    ) -> bool:
        """翻译一条统一 AgentEvent。返回本次是否观察到 interrupt 分类。"""
        turn_id = (turn.turn_id if turn is not None else None) or event.turn_id
        base: dict[str, Any] = {
            "agent_session_id": event.agent_session_id,
        }
        if turn_id:
            base["turn_id"] = turn_id
        if event.turn_id and event.turn_id != turn_id:
            # Runtime 内部的 turn id 仅作排障信息保留，不驱动 TurnRecord。
            base["runtime_turn_id"] = event.turn_id
        etype = event.event_type
        p = event.payload
        out_type: Optional[str] = None
        payload: dict[str, Any] = {}

        if etype is AgentEventType.SESSION_STARTED:
            out_type, payload = ev.EV_SESSION_STARTED, {
                **base, **p, "runtime": self._runtime_snapshot(thread_id),
            }
        elif etype is AgentEventType.SESSION_RESUMED:
            out_type, payload = ev.EV_SESSION_RESUMED, {
                **base, **p, "runtime": self._runtime_snapshot(thread_id),
            }
        elif etype is AgentEventType.SESSION_CLOSED:
            out_type, payload = ev.EV_SESSION_CLOSED, {**base, **p}
        elif etype is AgentEventType.TURN_STARTED:
            # A session can serve multiple turns.  Snapshot the selection on
            # every turn so the UI can label historical replies after a later
            # endpoint or model switch.
            out_type, payload = ev.EV_TURN_STARTED, {
                **base, **p, "runtime": self._runtime_snapshot(thread_id),
            }
        elif etype is AgentEventType.MESSAGE_DELTA:
            role = str(p.get("role") or "assistant").lower()
            if role not in {"assistant", "agent"}:
                return False
            if p.get("thinking") is True:
                text = str(p.get("reasoning_summary") or p.get("text") or "")
                if not text:
                    return False
                out_type, payload = ev.EV_REASONING_SUMMARY, {
                    **base,
                    "reasoning_summary": text,
                    "partial": True,
                }
            else:
                out_type, payload = ev.EV_MESSAGE_DELTA, {
                    **base, **p, "role": "assistant"}
        elif etype is AgentEventType.MESSAGE_COMPLETED:
            role = str(p.get("role") or "assistant").lower()
            text = str(p.get("text") or "")
            if role not in {"assistant", "agent"} or not text.strip():
                return False
            out_type, payload = ev.EV_MESSAGE_COMPLETED, {
                **base, **p, "text": text, "role": "assistant"}
        elif etype is AgentEventType.REASONING_SUMMARY:
            # Runtime-provided reasoning is a dedicated public stream so the UI
            # can place it inside the turn's expandable work process.
            out_type, payload = ev.EV_REASONING_SUMMARY, {**base, **p}
        elif etype is AgentEventType.TOOL_STARTED:
            out_type, payload = ev.EV_TOOL_STARTED, {**base, **p}
        elif etype is AgentEventType.TOOL_PROGRESS:
            # Progress/output deltas are not lifecycle starts (#218).
            out_type, payload = ev.EV_TOOL_PROGRESS, {**base, **p}
        elif etype is AgentEventType.TOOL_COMPLETED:
            out_type, payload = ev.EV_TOOL_COMPLETED, {**base, **p}
        elif etype is AgentEventType.APPROVAL_REQUESTED:
            request = ApprovalRequest.from_payload(p)
            normalized = normalize_approval_payload(request.to_payload())
            out_type, payload = ev.EV_APPROVAL_REQUESTED, {
                **base, **normalized}
        elif etype is AgentEventType.USER_INPUT_REQUESTED:
            out_type, payload = ev.EV_USER_INPUT_REQUESTED, {
                **base, **normalize_pending_user_input({**p}),
            }
        elif etype is AgentEventType.USAGE_UPDATED:
            out_type, payload = ev.EV_USAGE_UPDATED, {**base, **p, "usage_native_type": event.native_type, "runtime": self._runtime_snapshot(thread_id)}
        elif etype is AgentEventType.PLAN_UPDATED:
            out_type, payload = ev.EV_PLAN_UPDATED, {
                **base,
                **p,
                "source": str(p.get("source") or "adapter"),
                "native_type": event.native_type,
            }
        elif etype is AgentEventType.AGENT_UPDATED:
            agents = [{**node, "turn_id": turn_id} for node in p.get("agents", [])
                      if isinstance(node, dict)]
            out_type, payload = ev.EV_AGENT_UPDATED, {
                **base, **p, "agents": agents,
                "source": "adapter", "native_type": event.native_type,
            }
        elif etype is AgentEventType.RUNTIME_CAPABILITIES_UPDATED:
            out_type, payload = ev.EV_RUNTIME_CAPABILITIES_UPDATED, {
                **base, **p,
            }
            # Runtime 已报告目录变化：同步拉会话级快照，避免 matrix 仍停在 revision 0。
            self.refresh_runtime_capabilities(thread_id)
        elif etype is AgentEventType.RUNTIME_WARNING:
            out_type, payload = ev.EV_RUNTIME_WARNING, {
                **base, "severity": "warning", **p}
        elif etype is AgentEventType.RUNTIME_ERROR:
            out_type, payload = ev.EV_RUNTIME_ERROR, {**base, **p}
        elif etype is AgentEventType.ARTIFACT_CREATED:
            saved = self._save_agent_artifact(thread_id, turn, p)
            out_type, payload = ev.EV_ARTIFACT_CREATED, {
                **base, **p, **saved,
            }
        elif etype is AgentEventType.WORKSPACE_CHANGED:
            diff = str(
                p.get("diff")
                or (p.get("native") or {}).get("diff")
                or (p.get("native") or {}).get("unifiedDiff")
                or (p.get("native") or {}).get("patch")
                or ""
            )
            if diff:
                artifact = self._manager.attach_artifact(
                    thread_id,
                    name=f"{turn.turn_id if turn else 'workspace'}.diff",
                    content=diff.encode("utf-8"),
                    kind="conversation.diff",
                    media_type="text/x-diff",
                    run_id=turn.run_id if turn else None,
                )
                event_key = (
                    turn.turn_id if turn else thread_id,
                    artifact.sha256,
                )
                if event_key not in self._emitted_workspace_artifacts:
                    self._emitted_workspace_artifacts.add(event_key)
                    self._emit(thread_id, ev.EV_ARTIFACT_CREATED, {
                        **base,
                        "sha256": artifact.sha256,
                        "name": artifact.name,
                        "kind": artifact.kind,
                        "media_type": artifact.media_type,
                        "size": artifact.size,
                        "turn_id": turn.turn_id if turn else None,
                        "created_at": (
                            artifact.created_at.isoformat()
                            if getattr(artifact, "created_at", None) is not None
                            else None
                        ),
                    }, command_id=(turn.command_id or None) if turn else None)
            out_type, payload = ev.EV_WORKSPACE_CHANGED, {
                **base,
                "diff": diff,
                "has_diff": bool(diff),
            }
        elif etype is AgentEventType.TURN_COMPLETED:
            out_type, payload = ev.EV_TURN_COMPLETED, {**base, **p}
        elif etype is AgentEventType.TURN_FAILED:
            reason = str(p.get("reason") or "")
            if reason == EXIT_INTERRUPTED:
                out_type = ev.EV_TURN_INTERRUPTED
            else:
                out_type = ev.EV_TURN_FAILED
            payload = {**base, **p}
        elif etype is AgentEventType.RUNTIME_EXITED:
            self._emit(
                thread_id, ev.EV_RUNTIME_EXITED, {**base, **p},
                command_id=(turn.command_id or None) if turn else None)
            return str(p.get("classification") or "") == EXIT_INTERRUPTED
        elif etype is AgentEventType.APPROVAL_RESOLVED:
            # ACP 等 Runtime 可以按会话策略自动完成审批；这类结果没有经过
            # Web 的 approval.resolve 命令，也必须进入投影以清除待处理状态。
            out_type, payload = ev.EV_APPROVAL_RESOLVED, {**base, **p}
        elif etype is AgentEventType.USER_INPUT_RESOLVED:
            # 用户输入结果由 user_input.resolve 命令落事件。
            return False
        else:
            # 未知 / 私有事件进入诊断事件流，不改变错误状态。
            out_type, payload = ev.EV_RUNTIME_EVENT, {
                **base, "severity": "info",
                "native_type": event.native_type or str(etype),
                "native": p,
            }

        if out_type is not None:
            self._emit(thread_id, out_type, payload,
                       command_id=(turn.command_id or None) if turn else None)
        return False

    def _save_agent_artifact(
        self, thread_id: str, turn: Optional[TurnRecord], payload: dict[str, Any]
    ) -> dict[str, Any]:
        """Agent 产出的 Artifact 落库（有内容本体时）。"""
        content_b64 = str(payload.get("content_base64") or "")
        path = str(payload.get("path") or "")
        if not content_b64 and not path:
            return {}
        artifact = self._manager.attach_artifact(
            thread_id,
            name=str(payload.get("name") or "artifact"),
            content_base64=content_b64,
            path=path,
            kind=str(payload.get("kind") or "conversation.artifact"),
            media_type=str(payload.get("media_type") or ""),
            run_id=turn.run_id if turn else None,
        )
        return {
            "sha256": artifact.sha256,
            "name": artifact.name,
            "kind": artifact.kind,
            "media_type": artifact.media_type,
            "size": artifact.size,
        }

    # -- 控制面（steer / interrupt / approval / user input） ----------------------

    def _ref_for(self, thread_id: str) -> tuple[Any, AgentSessionRef]:
        record = self._session_record(thread_id)
        if record is None:
            raise LookupError(f"thread {thread_id} 没有活跃 AgentSession")
        adapter = self._adapter_for(
            record.adapter_id, record.runtime_instance_id or "default")
        return adapter, AgentSessionRef(
            agent_session_id=record.agent_session_id,
            adapter_id=record.adapter_id,
            external_session_id=record.external_session_id,
            runtime_instance_id=record.runtime_instance_id,
            resume_handle=record.resume_handle,
        )

    async def steer(
        self,
        thread_id: str,
        text: str,
        *,
        expected_turn_id: str,
        client_message_id: str = "",
        capability_revision: int | None = None,
    ) -> CommandReceipt:
        """处理中 steer：转发给 Adapter（无带内通道的 Adapter 回 typed receipt）。"""
        state = self._conv.get_state(thread_id)
        if not state.running_turn_id or state.running_turn_id != expected_turn_id:
            raise RuntimeError("执行中的 Turn 已变化，请重新确认后再引导")
        adapter, ref = self._ref_for(thread_id)
        snapshot = self._capability_cache.get(thread_id)
        matrix_payload = self.interaction_matrix_for(
            thread_id,
            adapter_id=adapter.id,
            instance_id=getattr(adapter.identity, "instance_id", "default"),
        )
        rows = {
            str(item.get("key")): item
            for item in (matrix_payload.get("rows") or [])
            if isinstance(item, dict)
        }
        steer_row = rows.get("steer") or {}
        steer_level = str(steer_row.get("level") or "unknown")
        current_revision = int(
            matrix_payload.get("revision")
            or (snapshot.revision if snapshot is not None else 0)
        )
        if capability_revision is not None and int(capability_revision) != current_revision:
            interrupt_row = rows.get("interrupt") or {}
            return CommandReceipt(
                command_id=f"steer-rev-{thread_id}",
                state=ReceiptState.FAILED,
                aggregate=AggregateRef(type="thread", id=thread_id),
                error=ErrorEnvelope(
                    code="external_agent.capability.expired",
                    message=(
                        "客户端能力 revision 与服务端不一致，"
                        "请刷新后再引导"
                    ),
                    category=ErrorCategory.VALIDATION,
                    recovery_hint=str(
                        interrupt_row.get("alternative")
                        or "请等待能力刷新完成后重试，或使用停止执行"
                    ),
                    retryable=True,
                    detail={
                        "capability": "steer",
                        "client_revision": int(capability_revision),
                        "server_revision": current_revision,
                        "level": "expired",
                        "reason": "capability revision mismatch",
                        "alternative": interrupt_row.get("alternative") or "",
                        "matrix": matrix_payload,
                    },
                ),
            )
        if steer_level != "supported" or bool(matrix_payload.get("stale")):
            interrupt_row = rows.get("interrupt") or {}
            alternative = str(
                steer_row.get("alternative")
                or interrupt_row.get("alternative")
                or "可使用停止执行中断当前回合，再发送新消息"
            )
            return CommandReceipt(
                command_id=f"steer-cap-{thread_id}",
                state=ReceiptState.FAILED,
                aggregate=AggregateRef(type="thread", id=thread_id),
                error=ErrorEnvelope(
                    code="external_agent.capability.unsupported",
                    message=str(
                        steer_row.get("reason")
                        or "当前 Runtime 不支持引导正在执行的回答"
                    ),
                    category=ErrorCategory.STATE,
                    recovery_hint=alternative,
                    retryable=False,
                    detail={
                        "capability": "steer",
                        "level": steer_level,
                        "reason": steer_row.get("reason") or "",
                        "alternative": alternative,
                        "revision": current_revision,
                        "matrix": matrix_payload,
                    },
                ),
            )
        return await adapter.steer(ref, AgentInput(
            kind="steer",
            text=text,
            payload={
                "expected_turn_id": expected_turn_id,
                "client_user_message_id": client_message_id,
                "capability_revision": current_revision,
            },
        ))

    async def interrupt(self, thread_id: str) -> CommandReceipt:
        """中断当前 Turn：转发给 Adapter，并等到 Turn 任务收尾。"""
        if thread_id in self._stop_fences:
            raise RuntimeError("当前对话正在停止，请等待资源释放后重试")
        fence = asyncio.Event()
        self._stop_fences[thread_id] = fence
        try:
            return await self._interrupt_and_release(thread_id)
        finally:
            self._stop_fences.pop(thread_id, None)
            fence.set()

    async def _interrupt_and_release(self, thread_id: str) -> CommandReceipt:
        adapter, ref = self._ref_for(thread_id)
        task = self._tasks.get(thread_id)
        receipt = await adapter.interrupt(ref)
        if receipt.error is not None or receipt.state is not ReceiptState.COMPLETED:
            return receipt
        if task is not None and not task.done():
            try:
                await asyncio.wait_for(asyncio.shield(task), timeout=15)
            except (asyncio.TimeoutError, asyncio.CancelledError):
                LOG.warning("interrupt wait timed out: %s", thread_id)
                # Runtime 已确认中断后，后台消费任务仍可能卡在第三方的审批
                # 回调或未结束的流上。此时必须清理本地任务，否则投影已经显示
                # interrupted，start_turn 却仍会以 already running 拒绝后续操作。
                if not task.done():
                    task.cancel()
                    try:
                        await task
                    except asyncio.CancelledError:
                        pass
        # A turn-level acknowledgement does not stop long-lived native tool
        # processes. Release this thread's Runtime after its event stream drains;
        # the persisted native resume handle is retained for the next continuation.
        # Unlike best-effort switching cleanup, an operator stop must report a
        # failed teardown rather than claiming all owned work has stopped.
        record = self._session_record(thread_id)
        if record is not None and record.closed_at is None:
            await self._close_record(record, reason="operator_stopped", strict=True)
        return receipt

    async def resolve_approval(
        self, thread_id: str, approval_id: str, decision: str, note: str = "",
        scope: str = "once",
    ) -> None:
        """approval 答复送回 Runtime，并翻译其后续事件。"""
        adapter, ref = self._ref_for(thread_id)
        approval = ApprovalDecision.from_payload({
            "approval_id": approval_id,
            "decision": decision,
            "scope": scope,
            "note": note,
        })
        stream = adapter.send(ref, AgentInput(
            kind="approval_response",
            payload=approval.to_payload(),
        ))
        await self._consume(thread_id, None, stream, require_control_delivery=True)

    async def resolve_user_input(
        self,
        thread_id: str,
        request_id: str,
        text: str = "",
        *,
        answers: Optional[dict[str, Any]] = None,
        decision: str = "submit",
    ) -> None:
        """user input 答复送回 Runtime（或写入 C22 fixture capture）。"""
        structured = dict(answers or {})
        decision_norm = (decision or "submit").strip().lower() or "submit"
        if self._manager.capture_user_input_fixture(
            thread_id,
            request_id,
            decision=decision_norm,
            answers=structured,
            text=text,
        ):
            return
        adapter, ref = self._ref_for(thread_id)
        stream = adapter.send(ref, AgentInput(
            kind="user_input_response",
            payload={
                "request_id": request_id,
                "decision": decision_norm,
                "answers": structured,
            },
            text=text,
        ))
        await self._consume(thread_id, None, stream, require_control_delivery=True)

    # -- 关停 ------------------------------------------------------------------------

    async def shutdown(self) -> None:
        """进程退出前：先 interrupt 在途 Turn，再取消任务（Session 记录保留）。"""
        running = list(self._tasks)
        for thread_id in running:
            try:
                await self.interrupt(thread_id)
            except Exception:  # noqa: BLE001 — 关停路径不阻断
                LOG.exception("shutdown interrupt failed: %s", thread_id)
        tasks = [
            *self._tasks.values(),
            *self._capability_tasks.values(),
        ]
        for task in tasks:
            task.cancel()
        for task in tasks:
            try:
                await task
            except (asyncio.CancelledError, Exception):  # noqa: BLE001
                pass
        self._tasks.clear()
        self._capability_tasks.clear()


__all__ = ["ExternalAgentSessionExecutor"]
