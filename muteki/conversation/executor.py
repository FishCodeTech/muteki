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
from dataclasses import dataclass, field
from hashlib import sha256
from pathlib import Path
from typing import Any, Callable, Optional

from muteki.external_agents.base import BaseExternalAgentAdapter
from muteki.external_agents.registry import AdapterRegistry
from muteki.external_agents.interaction_matrix import (
    HISTORY_REBUILD_METHOD,
    attach_matrix_to_snapshot,
    build_matrix_from_probe,
)
from muteki.external_agents.runtime_capabilities import RuntimeCapabilitySnapshot
from muteki.external_agents.sessions import EXIT_INTERRUPTED
from muteki.external_agents.user_input_schema import normalize_pending_user_input
from muteki.platform.contracts.agent_events import (
    AgentEventContractError,
    AgentFailure,
    FailureCategory,
    agent_failure,
    dump_payload,
    parse_payload,
    redact_secrets,
)
from muteki.platform.contracts.capabilities import ToolDescription
from muteki.platform.contracts.errors import ErrorCategory, ErrorEnvelope
from muteki.platform.contracts.external_agents import (
    AgentEvent,
    AgentEventType,
    AgentSessionRef,
    ApprovalResponseInput,
    MessageInput,
    SessionStart,
    SteerInput,
    UserInputResponseInput,
)
from muteki.platform.contracts.protocols import (
    BackgroundUpdateAdapter,
    NativeContinuationAdapter,
    NativeRewindAdapter,
    RuntimeOperationAdapter,
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
    approval_response_capability,
    has_actionable_approvals,
    normalize_approval_payload,
    stamp_response_capability,
)
from .attachment_delivery import (
    AttachmentDeliveryError,
    attachments_payload,
    merge_attachment_context,
    resolve_attachments,
    thread_authorized_sha256s,
)
from .checkpoints import CheckpointError, capture_checkpoint
from .composer_capabilities import ComposerCapabilityError, resolve_capability_refs
from .manager import ConversationManager, ConversationError
from .models import (
    ThreadRuntimeSelection,
    CAPABILITY_REFRESH_FAILED_CODE,
    TURN_COMPLETED,
    TURN_FAILED,
    TURN_INTERRUPTED,
    TURN_CANCELLED,
    TURN_KIND_EDIT_RESEND,
    TURN_KIND_RETRY,
    TURN_QUEUED,
    TURN_RUNNING,
    ThreadState,
    TurnRecord,
)
from .session_handoff import (
    REASON_RESTART,
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


def _chat_tool_descriptions(tools: list[dict[str, Any]]) -> list[ToolDescription]:
    """Launch-time view of chat plugin tools; routing keys stay with the plugin service."""
    return [
        ToolDescription(name=tool["name"], description=tool["description"],
                        input_schema=tool["input_schema"])
        for tool in tools
    ]


class ControlDeliveryError(RuntimeError):
    """A control response failed; retain its pending public request."""

    def __init__(self, runtime_code: str, *, delivery_unknown: bool = False,
                 reason: str = ""):
        super().__init__(
            "操作的投递结果无法确认，原决定已保留。请恢复原回执，或取消本轮。"
            if delivery_unknown else
            "操作未送达原会话，待处理请求已保留。请刷新后重试，或取消本轮。")
        self.runtime_code = runtime_code
        # AgentFailure.reason (engine-specific suffix) of the rejection.
        self.runtime_reason = reason
        self.delivery_unknown = delivery_unknown

# #188: capability refresh failure backoff (seconds).
_CAPABILITY_REFRESH_BACKOFF_BASE_S = 2.0
_CAPABILITY_REFRESH_BACKOFF_MAX_S = 60.0

#: 这些失败表示 Runtime 已正常结束本轮，Session 可以留给下一轮。
#: 其余失败（超时、传输断开、执行器异常）必须关掉 Session，否则
#: 八个引擎都会在 Muteki 停听后继续调 Capability。取消（operator
#: interrupt）同样保留 Session。
_KEEP_SESSION_FAILURE_REASONS = frozenset({
    "empty_assistant",
    "refusal",
    # pi/omp native-command turns ended unconfirmed without assistant text;
    # the session itself is healthy (旧事件以 reason=empty_assistant 保留会话)。
    "native_command.unconfirmed",
})

#: AgentFailure.reason of an executor-side contract violation.
_CONTRACT_VIOLATION_REASON = "event_contract"


def _contract_failure(exc: AgentEventContractError, engine: str) -> AgentFailure:
    return agent_failure(
        FailureCategory.VALIDATION, _CONTRACT_VIOLATION_REASON,
        engine=engine or "unknown",
        message=f"Runtime adapter emitted an invalid {exc.event_type} event",
        detail=exc.message, native_code=exc.code,
    )


def _public_failure(failure: AgentFailure) -> dict[str, Any]:
    return dump_payload(failure)


def _summary_line(text: str) -> str:
    """First non-empty line as the display summary; callers keep the full
    text in ``detail`` next to it."""
    for line in text.splitlines():
        if line.strip():
            return line.strip()
    return text.strip()


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



_TERMINAL_TURN_EVENTS = frozenset({
    ev.EV_TURN_COMPLETED, ev.EV_TURN_FAILED, ev.EV_TURN_INTERRUPTED,
})

#: What a terminal turn event says about the thread's native session.
DISPOSITION_REUSABLE = "reusable"
DISPOSITION_NEEDS_RESTART = "needs_restart"
DISPOSITION_CLOSED = "closed"

FENCED_EVENT_CODE = "conversation.event.fenced"


@dataclass
class _StreamFence:
    """Identity an event stream is allowed to speak for."""

    session_id: str
    generation: Optional[int]
    run_id: str = ""
    reported: set = field(default_factory=set)
    dropped: int = 0


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
        self._continuation_sources: dict[str, tuple[Any, AgentSessionRef]] = {}
        self._continuation_tasks: dict[str, asyncio.Task] = {}
        self._native_wake_turns: dict[str, tuple[str, str]] = {}
        self._shutting_down = False
        self._stop_fences: dict[str, asyncio.Event] = {}
        self._locks: dict[str, asyncio.Lock] = {}
        self._session_locks: dict[str, asyncio.Lock] = {}
        self._runtime_operations: set[str] = set()
        self._history_mutations: set[str] = set()
        self._queue_locks: dict[str, asyncio.Lock] = {}
        # 本进程内已 start 的 AgentSession（重启后需经 resume_handle 接管）
        self._live: set[str] = set()
        self._chat_revisions: dict[str, str] = {}
        self._chat_contexts: dict[str, str] = {}
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
        self._model_success_recorder: Optional[Callable[[dict[str, str]], None]] = None
        # agent_session_id → interaction mode the native session was launched in
        self._session_modes: dict[str, str] = {}
        # Sessions closed or replaced by this process; late events from them
        # must not drive the thread's current turn (execution-generation fence).
        self._closed_sessions: set[str] = set()
        # thread_id → what the next terminal turn event tells clients about
        # the session ("reusable" unless a detach/close was decided).
        self._disposition_hints: dict[str, str] = {}
        manager.bind_session_liveness(self.session_is_live)

    def session_is_live(self, agent_session_id: str) -> bool:
        """True when this process still has the native session attached."""
        return bool(agent_session_id) and agent_session_id in self._live

    def bind_model_success_recorder(
        self, recorder: Callable[[dict[str, str]], None],
    ) -> None:
        """Let the host persist model availability proven by a real chat turn."""
        self._model_success_recorder = recorder

    @staticmethod
    def _selection_snapshot(selection: Any) -> dict[str, str]:
        """Return the non-secret Runtime identity attached to a public turn event."""
        return {
            "adapter_id": str(getattr(selection, "adapter_id", "") or ""),
            "instance_id": str(getattr(selection, "instance_id", "") or "default"),
            "credential_id": str(getattr(selection, "credential_id", "") or ""),
            "model": str(getattr(selection, "model", "") or ""),
            "effort": str(getattr(selection, "effort", "") or ""),
            "service_tier": str(getattr(selection, "service_tier", "") or ""),
            "access_mode": str(
                getattr(selection, "access_mode", "") or ""
            ),
            "permission_mode": str(
                getattr(selection, "permission_mode", "") or ""
            ),
            "sandbox_mode": str(getattr(selection, "sandbox_mode", "") or ""),
            "interaction_mode": str(
                getattr(selection, "interaction_mode", "") or "default"
            ),
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
        if event_type in _TERMINAL_TURN_EVENTS and "thread_disposition" not in payload:
            payload = {
                **payload,
                "thread_disposition": (
                    self._disposition_hints.get(thread_id) or DISPOSITION_REUSABLE),
            }
        event = ev.thread_event(
            thread_id, event_type, payload, command_id=command_id)
        self._store.append_events(event)

    # -- Session 生命周期 ----------------------------------------------------------

    def _bind_runtime(self, adapter: BaseExternalAgentAdapter) -> None:
        """使用 Adapter 前补齐 PlatformStore 与 BindingService。

        Adapter 可能先构造再注册，SessionTracker 若没有 store，
        AgentSession 不会写入 platform.db，审批、恢复和重启都找不到会话。
        """
        adapter.attach_platform(store=self._store, binding_service=self._manager.bindings)

    def _adapter_for(self, adapter_id: str, instance_id: str) -> BaseExternalAgentAdapter:
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
        authorized = self._store.artifact_digests(thread.thread_id)

        def get_artifact(sha256: str) -> Optional[Artifact]:
            artifact = self._store.get(Artifact, sha256)
            metadata = self._store.artifact_metadata(thread.thread_id, sha256)
            if artifact is not None and metadata is not None:
                artifact = artifact.model_copy(update={key: metadata[key] for key in
                    ("name", "media_type", "kind") if key in metadata})
            return artifact

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
        """Recover an unclosed AgentSession when ThreadState lost its pointer (#122).

        Only sessions of the currently selected adapter instance are eligible;
        a session owned by another runtime is never rebound to this thread.
        """
        selection = self._manager.runtime_selection(thread_id)
        selected_instance = selection.instance_id or "default"
        candidates = [
            row for row in self._store.list(AgentSession, thread_id=thread_id)
            if row.closed_at is None
            and row.adapter_id == selection.adapter_id
            and (row.runtime_instance_id or "default") == selected_instance
        ]
        if not candidates:
            return None
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
                if adapter is not None:
                    record = adapter.session_record(state.agent_session_id)
            if record is not None and (record.closed_at is None or record.resume_handle):
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
        async with self._session_locks.setdefault(thread_id, asyncio.Lock()):
            return await self._ensure_capability_session(thread_id)

    async def _ensure_capability_session(
        self, thread_id: str, *, fresh: bool = False
    ) -> tuple[Any, AgentSessionRef]:
        """为能力目录建立真实 Runtime session，但不产生模型 Turn。"""
        thread = self._manager.get_thread(thread_id)
        if thread is None:
            raise LookupError(f"unknown thread: {thread_id}")
        await self._recover_history_mutation(thread_id)
        selection = self._manager.runtime_selection(thread_id)
        if not selection.adapter_id:
            raise LookupError(f"thread {thread_id} 未选择 Runtime instance")
        state = self._conv.get_state(thread_id)
        # #122: never close/replace the live turn's session for a capability probe.
        turn_busy = bool(
            state.running_turn_id or self._conv.active_turn_id(thread_id) or thread_id in self._runtime_operations
        )
        record = self._session_record(thread_id)
        previous_record = record
        # A catalog refresh must not replace the history that the user is
        # about to resume after a crash. Return the adapter's disconnected
        # snapshot; the explicit continuation owns reopening this handle.
        current_turns = self._conv.list_current_turns(thread_id)
        last_turn = current_turns[-1] if current_turns else None
        if (
            not turn_busy and record is not None and record.resume_handle
            and record.agent_session_id not in self._live
            and state.session_runtime_key == selection.session_key
            and last_turn is not None
            and last_turn.agent_session_id == record.agent_session_id
            and last_turn.status in {TURN_INTERRUPTED, TURN_FAILED, TURN_CANCELLED}
        ):
            adapter = self._adapter_for(selection.adapter_id, selection.instance_id)
            return adapter, self._ref_from_record(record)
        if fresh:
            record = None
        if turn_busy:
            if record is None or record.closed_at is not None:
                raise LookupError(
                    f"thread {thread_id} 有执行中的 Turn，但没有可复用的 AgentSession"
                )
            if record.agent_session_id not in self._live:
                raise LookupError("会话正在建立，请等待能力目录刷新")
            adapter = self._adapter_for(record.adapter_id, record.runtime_instance_id or "default")
            return adapter, self._ref_from_record(record)
        same_selection = False
        restart_reason = ""
        mode_from = ""
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
            same_selection = same_selection and self._chat_revision_matches(thread, record)
            if record.closed_at is None and same_selection:
                adapter = self._adapter_for(
                    selection.adapter_id, selection.instance_id)
                if record.agent_session_id in self._live:
                    launched = self._launched_interaction_mode(
                        record, selection.interaction_mode)
                    if (
                        launched != selection.interaction_mode
                        and not await self._plan_mode_per_turn(adapter)
                    ):
                        await self._close_record(
                            record, reason="interaction_mode_change")
                        state = state.model_copy(update={
                            "agent_session_id": None,
                            "session_runtime_key": "",
                            "current_generation": state.current_generation + 1,
                        })
                        self._conv.save_state(state)
                        restart_reason = "interaction_mode_change"
                        mode_from = launched
                        record = None
                        same_selection = False
                    else:
                        return adapter, self._ref_from_record(record)
            elif record.closed_at is None:
                await self._close_record(record, reason="runtime_switch")
                state = state.model_copy(update={
                    "agent_session_id": None,
                    "session_runtime_key": "",
                    "current_generation": state.current_generation + 1,
                })
                self._conv.save_state(state)
                restart_reason = "settings_mismatch"
                record = None

        adapter = self._adapter_for(selection.adapter_id, selection.instance_id)
        workspace = (
            self._manager.get_workspace(str(thread.workspace_id or ""))
            if thread.workspace_id else None
        )
        engine = engine_for_adapter(selection.adapter_id)
        credential_env: dict[str, str] = {}
        if selection.credential_id:
            from apps.web.worker_models import conversation_model_input
            credential_env = resolve_credential_env(
                selection.credential_id,
                engine=engine,
                sessions_root=self._sessions_root,
                container=False,
                model=selection.model,
                model_input=conversation_model_input(self._sessions_root, selection),
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
        }
        plugins = getattr(self, "chat_plugins", None)
        if plugins is not None and thread.mode == "conversation":
            if not options["cwd"]:
                options["cwd"] = str(plugins.visualization_root(thread.thread_id).parent)
            options["chat_tools"] = _chat_tool_descriptions(await plugins.prepare_tools(engine))
            options["chat_control_enabled"] = plugins.control_enabled(engine)
            credential_env = await asyncio.to_thread(
                plugins.prepare_environment, engine,
                thread_id + ":" + selection.credential_id,
                credential_env,
                previous_revision=(plugins.session_revision(previous_record.agent_session_id)
                                   if previous_record is not None and previous_record.adapter_id == selection.adapter_id else None),
            )
        if plugins is not None and thread.mode == "conversation":
            options.update(await asyncio.to_thread(plugins.native_launch_options, engine, credential_env))
        await self._prepare_native_fork(state, selection, options, credential_env)
        if credential_env:
            options["env"] = credential_env
        resume = record if record is not None and same_selection else None
        started_generation = state.current_generation + int(fresh)
        ref = await adapter.start(SessionStart(
            **({"agent_session_id": resume.agent_session_id}
               if resume is not None else {}),
            thread_id=thread_id,
            execution_generation=started_generation,
            workspace_id=thread.workspace_id,
            resume_handle=resume.resume_handle if resume is not None else None,
            model=selection.model or None,
            effort=selection.effort or None,
            service_tier=getattr(selection, "service_tier", "") or None,
            access_mode=selection.access_mode or None,
            permission_mode=selection.permission_mode or None,
            sandbox_mode=selection.sandbox_mode or None,
            interaction_mode=selection.interaction_mode,
            options=options,
        ))
        self._live.add(ref.agent_session_id)
        self._session_modes[ref.agent_session_id] = selection.interaction_mode
        self._closed_sessions.discard(ref.agent_session_id)
        self._remember_chat_revision(thread, ref)
        if previous_record is not None and resume is None:
            self._pending_handoff[thread_id] = build_session_handoff(
                thread_id=thread_id, messages=self._conv.list_current_messages(thread_id),
                turns=self._conv.list_turns(thread_id), exclude_turn_id="",
                kind=RECOVERY_STRUCTURED_HANDOFF, reason="capability_reload",
                generation=state.current_generation,
                source_adapter_id=previous_record.adapter_id,
                source_agent_session_id=previous_record.agent_session_id,
            )
        # adapter.start awaits a subprocess; archive or projections may have
        # changed ThreadState meanwhile, so never write back the stale copy.
        current = self._conv.get_state(thread_id)
        if current.status == "archived":
            started = self._store.get(AgentSession, ref.agent_session_id)
            if started is not None:
                if started.closed_at is None:
                    await self._close_record(started, reason="thread_archived")
            else:
                await adapter.close(ref)
            self._live.discard(ref.agent_session_id)
            self._chat_contexts.pop(ref.agent_session_id, None)
            raise LookupError(f"thread {thread_id} 已归档，能力会话已关闭")
        self._conv.save_state(current.model_copy(update={
            "agent_session_id": ref.agent_session_id,
            "session_runtime_key": selection.session_key,
            "native_fork": None,
        }))
        if restart_reason:
            switched: dict[str, Any] = {
                "generation": started_generation,
                "reason": restart_reason,
                "recovery_reason": restart_reason,
                "runtime_key": selection.session_key,
                "adapter_runtime_key": selection.runtime_key,
                "agent_session_id": ref.agent_session_id,
                "adapter_id": ref.adapter_id,
                "instance_id": ref.runtime_instance_id or "default",
                "runtime": self._selection_snapshot(selection),
            }
            if mode_from:
                switched["interaction_mode_from"] = mode_from
                switched["interaction_mode_to"] = selection.interaction_mode
            self._emit(thread_id, ev.EV_RUNTIME_SWITCHED, switched)
        return adapter, ref

    async def runtime_capabilities(
        self, thread_id: str
    ) -> RuntimeCapabilitySnapshot:
        adapter, ref = await self.ensure_capability_session(thread_id)
        snapshot = await adapter.runtime_capability_snapshot(ref)
        snapshot = self._enrich_capability_snapshot(
            thread_id, snapshot, adapter_id=adapter.id,
            instance_id=adapter.identity.instance_id,
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
        from muteki.external_agents.command_providers import operation_item
        if snapshot.agent_session_id and not snapshot.stale and not any(
            item.name == "rewind" and item.kind == "operation" for item in snapshot.items
        ):
            snapshot.items.append(operation_item(adapter_id, engine_for_adapter(adapter_id),
                "rewind", HISTORY_REBUILD_METHOD, "回退聊天历史并重建引擎上下文；保留工作区文件"))
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
        error = redact_secrets(f"{type(exc).__name__}: {exc}")
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
        state = self._conv.get_state(thread_id)
        if state.status == "archived":
            return False
        record = self._session_record(thread_id)
        if state.running_turn_id and (record is None or record.agent_session_id not in self._live):
            return False
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
                if self._conv.get_state(thread_id).status == "archived":
                    return
                LOG.exception("runtime capability refresh failed: %s", thread_id)
                try:
                    snapshot = self._record_capability_refresh_failure(
                        thread_id, exc,
                    )
                    failure = self._capability_failures.get(thread_id) or {}
                    self._emit(thread_id, ev.EV_RUNTIME_ERROR, {
                        "code": CAPABILITY_REFRESH_FAILED_CODE,
                        "detail": str(failure.get("error") or redact_secrets(str(exc))),
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

    async def _recover_history_mutation(self, thread_id: str) -> None:
        state = self._conv.get_state(thread_id)
        if not state.history_recovery_required or thread_id in self._history_mutations:
            return
        # An interrupted provider call may have succeeded remotely. Never resume
        # that uncertain native history; rebuild the last committed branch.
        records = {r.agent_session_id: r for r in self._store.list(AgentSession, thread_id=thread_id)
                   if r.closed_at is None}
        current = self._session_record(thread_id)
        if current is not None and current.closed_at is None:
            records[current.agent_session_id] = current
        for record in records.values():
            await self._close_record(record, reason="history_recovery")
        self._conv.save_state(state.model_copy(update={
            "agent_session_id": None, "session_runtime_key": "",
            "history_recovery_required": False, "history_rebuild_pending": True,
            "current_generation": state.current_generation + 1,
        }))
        self._pending_handoff.pop(thread_id, None)

    async def rewind_turn(self, thread_id: str, turn_id: str, *, command_id: str,
                          idempotency_key: str, file_mode: str) -> dict[str, Any]:
        lock = self._locks.setdefault(thread_id, asyncio.Lock())
        if lock.locked():
            raise RuntimeError("请等待当前回复结束后回退")
        async with lock, self._session_locks.setdefault(thread_id, asyncio.Lock()):
            await self._recover_history_mutation(thread_id)
            options = dict(command_id=command_id, idempotency_key=idempotency_key,
                           file_mode=file_mode, capability_override={"invocable": True})
            ids, apply = self._manager.native_rewind_turn(thread_id, turn_id, dry_run=True, **options)
            if not apply:
                return {"superseded_turn_ids": ids, "applied": False}
            before = self._conv.get_state(thread_id)
            old_record = self._session_record(thread_id)
            target = self._conv.get_turn(turn_id)
            native_id = target.native_turn_id
            if not native_id:
                for event in self._store.read_events("thread", thread_id, limit=10000):
                    if event.event_type == ev.EV_TURN_STARTED and event.payload.get("turn_id") == turn_id:
                        native_id = str(event.payload.get("runtime_turn_id") or "")
            fresh_ref = None
            strategy = "rebuild"
            committed = False
            self._history_mutations.add(thread_id)
            try:
                adapter, ref = await self._ensure_capability_session(thread_id)
                before = self._conv.get_state(thread_id)
                old_record = self._session_record(thread_id)
                self._conv.save_state(before.model_copy(update={"history_recovery_required": True}))
                if (isinstance(adapter, NativeRewindAdapter) and native_id
                    and target.agent_session_id == ref.agent_session_id
                    and adapter.supports_native_rewind(ref)):
                    await adapter.rewind_session(ref, native_id)
                    strategy = "native"
                else:
                    adapter, fresh_ref = await self._ensure_capability_session(thread_id, fresh=True)
                ids, applied = self._manager.native_rewind_turn(
                    thread_id, turn_id, provider_rewind=lambda **_: True,
                    rebuild_history=strategy == "rebuild", **options)
                committed = True
                if strategy == "rebuild":
                    state = self._conv.get_state(thread_id).model_copy(update={"history_rebuild_pending": True})
                    self._conv.save_state(state)
                    self._pending_handoff[thread_id] = build_session_handoff(
                        thread_id=thread_id, messages=self._conv.list_current_messages(thread_id),
                        turns=self._conv.list_turns(thread_id), exclude_turn_id="",
                        kind=RECOVERY_STRUCTURED_HANDOFF, reason="history_rewind",
                        generation=state.current_generation,
                        source_adapter_id=old_record.adapter_id if old_record else "",
                        source_agent_session_id=old_record.agent_session_id if old_record else "")
                    if old_record and old_record.agent_session_id != fresh_ref.agent_session_id:
                        await self._close_record(old_record, reason="history_rewind")
                        self._conv.save_state(state)
                else:
                    self._pending_handoff.pop(thread_id, None)
                self._capability_cache.pop(thread_id, None)
                return {"superseded_turn_ids": ids, "applied": applied, "strategy": strategy}
            except BaseException:
                # Fresh-session bootstrap failure must leave the old branch usable.
                if fresh_ref is not None and not committed:
                    await adapter.close(fresh_ref)
                    self._live.discard(fresh_ref.agent_session_id)
                    self._chat_contexts.pop(fresh_ref.agent_session_id, None)
                if not committed:
                    self._conv.save_state(before.model_copy(update={"history_recovery_required": True}))
                raise
            finally:
                self._history_mutations.discard(thread_id)

    async def runtime_operation(self, thread_id: str, name: str, arguments: str = "") -> dict[str, Any]:
        lock = self._locks.setdefault(thread_id, asyncio.Lock())
        if lock.locked():
            raise RuntimeError("请等待当前回复结束后执行原生操作")
        async with lock:
            return await self._runtime_operation(thread_id, name, arguments)

    async def _runtime_operation(
        self, thread_id: str, name: str, arguments: str = ""
    ) -> dict[str, Any]:
        state = self._conv.get_state(thread_id)
        if state.running_turn_id or self._conv.active_turn_id(thread_id):
            raise RuntimeError("请等待当前回复结束后执行原生操作")
        adapter, ref = await self.ensure_capability_session(thread_id)
        snapshot = await adapter.runtime_capability_snapshot(ref)
        snapshot = self._enrich_capability_snapshot(
            thread_id, snapshot, adapter_id=adapter.id,
            instance_id=adapter.identity.instance_id,
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
        if not isinstance(adapter, RuntimeOperationAdapter):
            raise RuntimeError(
                f"{adapter.id} 没有结构化 Runtime operation 通道")
        self._runtime_operations.add(thread_id)
        try:
            if verified.name == "compact":
                self._chat_contexts.pop(ref.agent_session_id, None)
            result = await adapter.runtime_operation(ref, name, arguments)
        finally:
            self._runtime_operations.discard(thread_id)
        from muteki.external_agents.command_providers import public_operation_result
        return public_operation_result(result)

    def _chat_revision_matches(self, thread: Thread, record: AgentSession) -> bool:
        plugins = getattr(self, "chat_plugins", None)
        if plugins is None or thread.mode != "conversation":
            return True
        if record.resume_handle and not self._resume_handle_usable(record):
            return False
        if record.agent_session_id not in self._live and not any(
            turn.agent_session_id == record.agent_session_id and turn.status in {TURN_COMPLETED, TURN_INTERRUPTED, TURN_CANCELLED}
            for turn in self._conv.list_turns(thread.thread_id)
        ):
            # Discovery-only Codex/Cursor sessions may not persist any native
            # history. After restart, start fresh instead of resuming a phantom.
            return False
        previous = self._chat_revisions.get(record.agent_session_id) or plugins.session_revision(record.agent_session_id)
        return previous == plugins.revision(engine_for_adapter(record.adapter_id))

    def _resume_handle_usable(self, record: AgentSession) -> bool:
        """Ask the owning adapter whether the stored handle can still resume.

        Adapters without ``resume_handle_usable`` accept any stored handle.
        """
        adapter = self._registry.get(
            record.adapter_id, record.runtime_instance_id or "default")
        check = getattr(adapter, "resume_handle_usable", None)
        return bool(check(record.resume_handle)) if callable(check) else True

    def _remember_chat_revision(self, thread: Thread, ref: AgentSessionRef) -> None:
        plugins = getattr(self, "chat_plugins", None)
        if plugins is not None and thread.mode == "conversation":
            self._chat_revisions[ref.agent_session_id] = plugins.revision(engine_for_adapter(ref.adapter_id))
            plugins.remember_session(ref.agent_session_id, self._chat_revisions[ref.agent_session_id])

    @staticmethod
    async def _resume_continues_turn(adapter: Any) -> bool:
        """Whether ``adapter.resume`` itself continues the interrupted turn."""
        caps = await adapter.capabilities()
        return bool(getattr(caps, "resume_continues_turn", True))

    def _launched_interaction_mode(self, record: AgentSession, fallback: str) -> str:
        """Mode the native session was actually started in.

        The in-process map is authoritative. After it is missing, the last
        turn bound to the session is the persisted record; otherwise ``fallback``
        (the mode about to be sent) so a session with no history is not restarted.
        """
        remembered = self._session_modes.get(record.agent_session_id)
        if remembered:
            return remembered
        thread_id = str(record.thread_id or "")
        if thread_id:
            prior = [
                item for item in self._conv.list_turns(thread_id)
                if item.agent_session_id == record.agent_session_id
                and item.interaction_mode
            ]
            if prior:
                prior.sort(key=lambda item: (item.seq, item.turn_id))
                return prior[-1].interaction_mode
        return fallback or "default"

    @staticmethod
    async def _plan_mode_per_turn(adapter: Any) -> bool:
        """Whether the adapter honours ``MessagePayload.interaction_mode`` on a live session."""
        caps = await adapter.capabilities()
        return bool(caps.plan_mode_per_turn)

    async def ensure_session(
        self,
        thread: Thread,
        turn: TurnRecord,
        *,
        force_new: bool = False,
    ) -> tuple[Any, AgentSessionRef]:
        async with self._session_locks.setdefault(thread.thread_id, asyncio.Lock()):
            return await self._ensure_session(thread, turn, force_new=force_new)

    async def _ensure_session(
        self, thread: Thread, turn: TurnRecord, *, force_new: bool = False,
    ) -> tuple[Any, AgentSessionRef]:
        """取得该 Thread 当前可用的 (adapter, session)；必要时创建 / 恢复 / 切换。

        - 无活跃 Session：创建新 Session（七步 Binding 交付在 Adapter 内）；
        - 进程重启后：用保存的 resume_handle 接管同一 external session，
          保持同一 agent_session_id；
        - Runtime 选择变化：关闭旧 Session（撤销 grant），generation+1，
          重新生成注入计划并启动新 Session；普通 message 也会做结构化交接。
        """
        await self._recover_history_mutation(thread.thread_id)
        selection = self._manager.runtime_selection(thread.thread_id)
        if not selection.adapter_id:
            raise LookupError(
                f"thread {thread.thread_id} 未选择 Runtime instance")
        record = self._session_record(thread.thread_id)
        state = self._conv.get_state(thread.thread_id)
        if turn.kind == "resume":
            # Continue the session that executed the last turn, rather than a
            # newer discovery-only session. This also repairs a stale pointer
            # left by a capability refresh from an earlier server version.
            prior = next((item for item in reversed(self._conv.list_current_turns(thread.thread_id))
                          if item.turn_id != turn.turn_id), None)
            if prior is not None and prior.agent_session_id and prior.runtime_snapshot:
                prior_selection = ThreadRuntimeSelection.model_validate({
                    key: value for key, value in prior.runtime_snapshot.items()
                    if key in ThreadRuntimeSelection.model_fields})
                prior_record = self._store.get(AgentSession, prior.agent_session_id)
                if (prior_selection.session_key == selection.session_key
                        and prior_record is not None and prior_record.resume_handle):
                    record = prior_record
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
            # Reopen an explicitly resumed native conversation with the
            # current launch options. An extension/cache revision change must
            # not silently turn "continue" into a new native conversation.
            if turn.kind != "resume" or not record.resume_handle:
                same_selection = same_selection and self._chat_revision_matches(thread, record)
            if same_selection and (record.closed_at is None or record.resume_handle):
                adapter = self._adapter_for(selection.adapter_id, selection.instance_id)
                launched_mode = self._launched_interaction_mode(
                    record, turn.interaction_mode)
                if (
                    record.closed_at is None
                    and record.agent_session_id in self._live
                    and launched_mode != turn.interaction_mode
                    and not await self._plan_mode_per_turn(adapter)
                ):
                    # The adapter fixes plan/default at launch, so a mode
                    # change needs a new native session; keep the native
                    # conversation when a resume handle exists.
                    await self._close_record(
                        record, reason="interaction_mode_change")
                    mode_change = (launched_mode, turn.interaction_mode)
                    if record.resume_handle:
                        return await self._start(
                            adapter, thread, turn, state,
                            agent_session_id=record.agent_session_id,
                            resume_handle=record.resume_handle,
                            previous_session_key=previous_session_key,
                            previous_runtime_key=previous_runtime_key,
                            previous_adapter_id=previous_adapter_id,
                            previous_agent_session_id=previous_agent_session_id,
                            mode_change=mode_change,
                        )
                    state = state.model_copy(update={
                        "current_generation": state.current_generation + 1})
                    return await self._start(
                        adapter, thread, turn, state, switched=True,
                        previous_session_key=previous_session_key,
                        previous_runtime_key=previous_runtime_key,
                        previous_adapter_id=previous_adapter_id,
                        previous_agent_session_id=previous_agent_session_id,
                        mode_change=mode_change,
                    )
                if record.closed_at is None and record.agent_session_id in self._live:
                    # 本进程内活跃：直接复用。
                    ref = self._ref_from_record(record)
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
        mode_change: Optional[tuple[str, str]] = None,
    ) -> tuple[Any, AgentSessionRef]:
        selection = self._manager.runtime_selection(thread.thread_id)
        workspace = (
            self._manager.get_workspace(thread.workspace_id)
            if thread.workspace_id else None)
        engine = engine_for_adapter(selection.adapter_id)
        credential_env: dict[str, str] = {}
        if selection.credential_id:
            from apps.web.worker_models import conversation_model_input
            credential_env = resolve_credential_env(
                selection.credential_id,
                engine=engine,
                sessions_root=self._sessions_root,
                container=False,
                model=selection.model,
                model_input=conversation_model_input(self._sessions_root, selection),
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
            interaction_mode_changed=mode_change is not None,
        )
        messages = self._conv.list_current_messages(thread.thread_id)
        turns = self._conv.list_turns(thread.thread_id)
        native_fork = bool(state.native_fork and state.native_fork.get("adapter_id") == selection.adapter_id)
        native_resume = bool(resume_handle) and not switched and not force_new
        if native_resume or native_fork:
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
        }
        plugins = getattr(self, "chat_plugins", None)
        if plugins is not None and thread.mode == "conversation":
            if not options["cwd"]:
                options["cwd"] = str(plugins.visualization_root(thread.thread_id).parent)
            options["chat_tools"] = _chat_tool_descriptions(await plugins.prepare_tools(engine))
            options["chat_control_enabled"] = plugins.control_enabled(engine)
            credential_env = await asyncio.to_thread(
                plugins.prepare_environment, engine,
                thread.thread_id + ":" + selection.credential_id,
                credential_env,
                previous_revision=(plugins.session_revision(previous_agent_session_id)
                                   if previous_agent_session_id and previous_adapter_id == selection.adapter_id else None),
            )
        if plugins is not None and thread.mode == "conversation":
            options.update(await asyncio.to_thread(plugins.native_launch_options, engine, credential_env))
        await self._prepare_native_fork(state, selection, options, credential_env)
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
            service_tier=getattr(selection, "service_tier", "") or None,
            access_mode=selection.access_mode or None,
            permission_mode=selection.permission_mode or None,
            sandbox_mode=selection.sandbox_mode or None,
            interaction_mode=turn.interaction_mode,
            options=options,
        )
        ref = await adapter.start(request)
        self._live.add(ref.agent_session_id)
        self._session_modes[ref.agent_session_id] = turn.interaction_mode
        self._closed_sessions.discard(ref.agent_session_id)
        self._remember_chat_revision(thread, ref)
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
            "service_tier": selection.service_tier,
            "access_mode": selection.access_mode,
            "permission_mode": selection.permission_mode,
            "runtime": self._selection_snapshot(selection),
            "recovery_kind": recovery_kind,
            "recovery_reason": reason,
        }
        if mode_change is not None:
            event_payload["interaction_mode_from"] = mode_change[0]
            event_payload["interaction_mode_to"] = mode_change[1]
        if handoff is not None:
            event_payload.update(handoff.to_event_payload())
        # Always persist state for the new session.
        self._conv.save_state(self._conv.get_state(thread.thread_id).model_copy(update={
            "agent_session_id": ref.agent_session_id,
            "session_runtime_key": selection.session_key,
            "current_generation": state.current_generation,
            "native_fork": None,
        }))
        if switched or force_new or mode_change is not None or (
            handoff is not None and handoff.included and not native_resume
        ):
            self._emit(thread.thread_id, ev.EV_RUNTIME_SWITCHED, event_payload)
        return adapter, ref

    async def _prepare_native_fork(
        self, state: ThreadState, selection: ThreadRuntimeSelection,
        options: dict[str, Any], credential_env: dict[str, str],
    ) -> None:
        source = state.native_fork
        if not source or source["adapter_id"] != selection.adapter_id:
            return
        plugins = getattr(self, "chat_plugins", None)
        if plugins is not None:
            await asyncio.to_thread(
                plugins.prepare_codex_fork_history,
                source["source_thread_id"] + ":" + source["source_credential_id"], credential_env,
                previous_revision=plugins.session_revision(source["source_agent_session_id"]),
            )
        options.update(fork_from=source["native_thread_id"], fork_last_turn_id=source["native_turn_id"])
        self._pending_handoff.pop(state.thread_id, None)

    async def _close_record(
        self, record: AgentSession, *, reason: str, strict: bool = False,
    ) -> None:
        """关闭一个活跃 Session：Adapter close（联动撤 grant）+ 事件。"""
        self._continuation_sources.pop(record.thread_id or "", None)
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
        self._chat_contexts.pop(record.agent_session_id, None)
        self._session_modes.pop(record.agent_session_id, None)
        self._closed_sessions.add(record.agent_session_id)
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
        closed = False
        if record is not None and record.closed_at is None:
            await self._close_record(record, reason=reason, strict=reason == "thread_archived")
            closed = True
        plugins = getattr(self, "chat_plugins", None)
        if plugins is not None and reason == "thread_archived":
            await asyncio.to_thread(plugins.release_thread_assets, thread_id)
        return closed

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
    def _turn_failure_keeps_session(failure: AgentFailure) -> bool:
        return (
            failure.category is FailureCategory.CANCELLED
            or failure.reason in _KEEP_SESSION_FAILURE_REASONS
        )

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

    def _offer_continuation(self, thread_id: str) -> None:
        """T3 offerWake: dispatch only while the native wake is still current."""
        if self._shutting_down:
            return
        task = self._continuation_tasks.get(thread_id)
        if task is not None and not task.done():
            return
        task = asyncio.create_task(self._start_continuation(thread_id))
        self._continuation_tasks[thread_id] = task
        task.add_done_callback(lambda _task: self._continuation_tasks.pop(thread_id, None))

    async def _start_continuation(self, thread_id: str) -> bool:
        lock = self._queue_locks.setdefault(thread_id, asyncio.Lock())
        async with lock:
            source = self._continuation_sources.get(thread_id)
            if source is None or self._shutting_down:
                return False
            adapter, ref = source
            record = self._session_record(thread_id)
            state = self._conv.get_state(thread_id)
            task = self._tasks.get(thread_id)
            if (record is None or record.closed_at is not None
                    or record.agent_session_id != ref.agent_session_id
                    or ref.agent_session_id in self._closed_sessions
                    or thread_id in self._stop_fences or state.status != "active"
                    or (task is not None and not task.done())
                    or self._conv.active_turn_id(thread_id)):
                return False
            pending = adapter.pending_continuation(ref)
            if pending is None:
                return False
            wake_id, detail = pending
            try:
                turn, _run, _task, _created = self._manager.request_turn(
                    thread_id, kind="continuation", text="",
                    idempotency_key=f"native-wake:{ref.agent_session_id}:{wake_id}")
                self._native_wake_turns[turn.turn_id] = (ref.agent_session_id, wake_id)
                self._emit(thread_id, ev.EV_TURN_REQUESTED, {
                    "turn_id": turn.turn_id, "run_id": turn.run_id, "task_id": turn.task_id,
                    "seq": turn.seq, "kind": "continuation", "text": "",
                    "detail": detail, "interaction_mode": turn.interaction_mode,
                })
                self.start_turn(turn.turn_id)
                return True
            except Exception as exc:
                LOG.exception("native continuation dispatch failed: %s", thread_id)
                self._emit(thread_id, ev.EV_RUNTIME_ERROR, {
                    "code": "conversation.continuation.dispatch_failed",
                    "message": "后台续接回合启动失败", "detail": redact_secrets(str(exc)),
                })
                return False

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
                            "interaction_mode": turn.interaction_mode,
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
                full = redact_secrets(f"{type(exc).__name__}: {exc}")
                detail = getattr(exc, "detail", "")
                if detail:
                    full += "\n" + redact_secrets(str(detail))
                error = {
                    "code": "conversation.queue.dispatch_failed",
                    "message": _summary_line(full),
                    "detail": full,
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
                full = redact_secrets(f"{type(exc).__name__}: {exc}")
                detail = getattr(exc, "detail", "")
                if detail:
                    full += "\n" + redact_secrets(str(detail))
                native_code = getattr(exc, "code", None)
                code = native_code if isinstance(native_code, str) and native_code else "conversation.turn.executor_error"
                self._emit(turn.thread_id, ev.EV_RUNTIME_ERROR, {
                    "turn_id": turn.turn_id,
                    "code": code,
                    "message": _summary_line(full),
                    "detail": full,
                })
                self._emit(turn.thread_id, ev.EV_TURN_FAILED, {
                    "turn_id": turn.turn_id,
                    "error": {
                        "code": code,
                        "message": _summary_line(full),
                        "detail": full,
                    },
                }, command_id=turn.command_id or None)
                if not isinstance(exc, ComposerCapabilityError):
                    await self._stop_detached_runtime(
                        turn.thread_id, reason="turn_executor_error")
            finally:
                self._tasks.pop(turn.thread_id, None)
                current = self._conv.get_turn(turn.turn_id)
                computer = getattr(self, "computer_control", None)
                if computer is not None and (
                    asyncio.current_task().cancelling()
                    or (current is not None and current.status in {
                        TURN_COMPLETED, TURN_FAILED, TURN_INTERRUPTED, TURN_CANCELLED,
                    })
                ):
                    await computer.finish_turn(turn.thread_id, turn.turn_id)
                if current is not None and current.status == TURN_COMPLETED:
                    if not await self.start_next_queued(turn.thread_id):
                        await self._start_continuation(turn.thread_id)
                elif (
                    current is not None
                    and current.status in {TURN_FAILED, TURN_INTERRUPTED, TURN_CANCELLED}
                    and self._conv.list_queue(turn.thread_id)
                ):
                    reason = (
                        "turn_interrupted"
                        if current.status == TURN_INTERRUPTED
                        else "turn_cancelled" if current.status == TURN_CANCELLED
                        else "turn_failed"
                    )
                    state = self._conv.pause_queue(turn.thread_id, reason)
                    self._emit(turn.thread_id, ev.EV_QUEUE_PAUSED, {
                        "reason": reason,
                        "turn_id": turn.turn_id,
                        "queue_revision": state.queue_revision,
                    }, command_id=turn.command_id or None)

    async def _capture_turn_checkpoint(self, thread: Thread, turn: TurnRecord) -> None:
        workspace = (
            self._manager.get_workspace(str(thread.workspace_id or ""))
            if thread.workspace_id else None
        )
        root = str(workspace.root_path or "") if workspace is not None else ""
        if not root:
            return
        try:
            await asyncio.to_thread(capture_checkpoint, root, thread.thread_id, turn.turn_id)
        except CheckpointError as exc:
            # The turn still runs; only "restore files" for this turn is lost.
            LOG.warning("turn checkpoint failed thread=%s turn=%s: %s", thread.thread_id, turn.turn_id, exc)
            self._emit(thread.thread_id, ev.EV_RUNTIME_WARNING, {
                "turn_id": turn.turn_id,
                "severity": "warning",
                "code": exc.code,
                "message": f"未能保存本轮开始前的文件检查点，之后无法回退这一轮的文件：{exc}",
            }, command_id=turn.command_id or None)

    async def _run_turn(self, turn: TurnRecord) -> None:
        thread = self._manager.get_thread(turn.thread_id)
        if thread is None:
            raise LookupError(f"unknown thread: {turn.thread_id}")
        self._disposition_hints.pop(turn.thread_id, None)
        await self._capture_turn_checkpoint(thread, turn)
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
        if isinstance(adapter, NativeContinuationAdapter):
            self._continuation_sources[thread.thread_id] = (adapter, ref)

            def native_background(origin_turn_id, updates):
                if ref.agent_session_id in self._closed_sessions:
                    return
                record = self._session_record(thread.thread_id)
                if record is None or record.agent_session_id != ref.agent_session_id or record.closed_at is not None:
                    return
                origin = self._conv.get_turn(origin_turn_id)
                if origin is None or origin.status == "superseded":
                    return
                for kind, native, payload in updates:
                    self._translate(thread.thread_id, origin, AgentEvent(
                        event_type=kind, agent_session_id=ref.agent_session_id,
                        execution_generation=record.execution_generation,
                        payload=payload, native_type=native))

            adapter.bind_continuations(ref, turn.turn_id, native_background,
                                       lambda: self._offer_continuation(thread.thread_id))
        if turn.runtime_invocation and isinstance(adapter, BackgroundUpdateAdapter):
            # A runtime command can keep producing output after the turn
            # stream ends; any adapter with a background channel gets a sink.
            background_started = False
            def background(updates):
                nonlocal background_started
                current = self._conv.get_turn(turn.turn_id)
                if current is None or current.status == "superseded":
                    return
                for kind, native, payload in updates:
                    try:
                        model = parse_payload(kind, payload)
                    except AgentEventContractError as exc:
                        LOG.error("background runtime event rejected: %s", exc)
                        failure = _contract_failure(exc, engine_for_adapter(adapter.id))
                        self._emit(thread.thread_id, ev.EV_RUNTIME_ERROR, {
                            "turn_id": turn.turn_id, "agent_session_id": ref.agent_session_id,
                            "code": failure.code, "category": failure.category.value,
                            "reason": failure.reason, "message": failure.message,
                            "detail": failure.detail, "error": _public_failure(failure),
                            "background": True,
                        })
                        continue
                    if kind in {AgentEventType.MESSAGE_DELTA, AgentEventType.MESSAGE_COMPLETED}:
                        text = model.text
                        if not text or model.role != "assistant":
                            continue
                        if not background_started:
                            text = "\n\n" + text
                            background_started = True
                        self._emit(thread.thread_id, ev.EV_MESSAGE_DELTA, {
                            "turn_id": turn.turn_id, "agent_session_id": ref.agent_session_id,
                            "text": text, "role": "assistant", "background": True,
                        })
                    elif kind is AgentEventType.RUNTIME_CAPABILITIES_UPDATED:
                        self._capability_cache.pop(thread.thread_id, None)
                    else:
                        self._translate(thread.thread_id, turn, AgentEvent(
                            event_type=kind, agent_session_id=ref.agent_session_id,
                            payload=payload, native_type=native), model)
            adapter.bind_background_handler(ref, background)
        # #119: 会话已连接后主动刷新能力目录，勿依赖用户打开 / $ @ 命令菜单。
        cached_caps = self._capability_cache.get(thread.thread_id)
        if cached_caps is None or cached_caps.stale:
            self.refresh_runtime_capabilities(thread.thread_id)
        application_context_delivery = None
        # Adapters whose session/load restores history during ensure_session
        # do not run another turn on resume; send the continuation prompt.
        if turn.kind == "continuation":
            pending = self._native_wake_turns.pop(turn.turn_id, None)
            if (not isinstance(adapter, NativeContinuationAdapter) or pending is None
                    or pending[0] != ref.agent_session_id):
                raise ConversationError("后台续接所属会话已失效", code="conversation.continuation.stale")
            stream = adapter.run_continuation(ref, pending[1])
        elif (turn.kind == "resume" and resume_supported
                and await self._resume_continues_turn(adapter)):
            # Native resume path: do not also dump structured handoff into send.
            self._pending_handoff.pop(thread.thread_id, None)
            stream = adapter.resume(ref)
        else:
            pending = self._pending_handoff.pop(thread.thread_id, None)
            state = self._conv.get_state(thread.thread_id)
            if state.history_rebuild_pending:
                pending = build_session_handoff(
                    thread_id=thread.thread_id, messages=self._conv.list_current_messages(thread.thread_id),
                    turns=self._conv.list_turns(thread.thread_id), exclude_turn_id=turn.turn_id,
                    kind=RECOVERY_STRUCTURED_HANDOFF, reason="history_rewind", generation=state.current_generation)
                self._conv.save_state(state.model_copy(update={"history_rebuild_pending": False}))
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
                plugin_service=getattr(self, "chat_plugins", None) if thread.mode == "conversation" else None,
                threads=self._manager.list_threads(),
                message_loader=self._conv.list_current_messages,
                message_lookup=self._conv.get_message,
            )
            runtime_invocation = dict(turn.runtime_invocation or {})
            if runtime_invocation:
                snapshot = await adapter.runtime_capability_snapshot(ref)
                snapshot = self._enrich_capability_snapshot(
                    thread.thread_id, snapshot, adapter_id=adapter.id,
                    instance_id=adapter.identity.instance_id,
                )
                self._capability_cache[thread.thread_id] = snapshot
                client_revision = runtime_invocation.get("revision")
                if (
                    client_revision is not None
                    and int(client_revision) != int(snapshot.revision)
                ):
                    raise ComposerCapabilityError(
                        "能力目录 revision 已变化，请重新打开命令目录选择")
                if snapshot.stale:
                    raise ComposerCapabilityError(
                        "Runtime 能力目录已经过期，请重新打开命令目录选择")
                matched = _match_runtime_capability(
                    snapshot, runtime_invocation,
                )
                if matched is None:
                    raise ComposerCapabilityError(
                        "所选 Runtime 命令已失效，请重新打开命令目录选择")
                runtime_invocation = matched.model_dump(mode="json")
                runtime_invocation["arguments"] = str(
                    turn.runtime_invocation.get("arguments") or "")
                runtime_invocation["revision"] = snapshot.revision
                if matched.name == "compact":
                    self._chat_contexts.pop(ref.agent_session_id, None)
                if matched.kind == "operation" and matched.resolution == "client":
                    arguments = str(runtime_invocation.get("arguments") or "")
                    result = await adapter.runtime_operation(ref, matched.name, arguments) if arguments else await adapter.runtime_operation(ref, matched.name)
                    from muteki.external_agents.command_providers import public_operation_result
                    result = public_operation_result(result)
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
            plugins = getattr(self, "chat_plugins", None)
            if plugins is not None and thread.mode == "conversation":
                from .computer_control import chat_context as computer_chat_context
                skill_context = plugins.skill_catalog_context(engine_for_adapter(selection.adapter_id))
                context = "\n\n".join(part for part in (
                    computer_chat_context(selection.access_mode, gateway_available=(
                        adapter.injection_plan(ref.agent_session_id) is not None)), skill_context,
                    plugins.visualization_context(thread.thread_id)) if part)
                revision = f"{adapter.context_revision(ref)}:" + sha256(context.encode()).hexdigest()
                cacheable = adapter.context_compaction_events
                if not cacheable or self._chat_contexts.get(ref.agent_session_id) != revision:
                    text = context + "\n\n" + text
                    if cacheable:
                        application_context_delivery = (ref.agent_session_id, revision)
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
            if runtime_invocation and runtime_invocation.get("kind") != "skill":
                from muteki.external_agents.command_providers import native_prompt
                text = native_prompt(runtime_invocation, str(runtime_invocation.get("arguments") or ""))
                application_context_delivery = None
            elif runtime_invocation and (runtime_invocation.get("invocation") or {}).get("protocol") != "codex.turn/start":
                from muteki.external_agents.command_providers import native_prompt
                text = native_prompt(runtime_invocation, str(runtime_invocation.get("arguments") or ""))
                application_context_delivery = None
            stream = adapter.send(ref, MessageInput(
                text=text,
                payload={
                    "attachments": attachment_payload,
                    "capability_context": "",
                    "runtime_capability": runtime_invocation,
                    "runtime_command_arguments": str(
                        runtime_invocation.get("arguments") or ""
                    ),
                    "interaction_mode": turn.interaction_mode,
                },
            ))
        await self._consume(thread.thread_id, turn, stream,
                            application_context_delivery=application_context_delivery)

    async def _consume(
        self, thread_id: str, turn: Optional[TurnRecord], stream: Any,
        *, require_control_delivery: bool = False,
        application_context_delivery: Optional[tuple[str, str]] = None,
    ) -> None:
        """消费 Adapter 事件流并翻译为 Thread Public Event。"""
        interrupted = False
        runtime_error: Optional[dict[str, Any]] = None
        saw_completed_assistant = False
        detach_reason: Optional[str] = None
        # Freeze the launch selection before consuming events. The picker or a
        # queued turn can change the thread's current selection during a reply.
        verification_runtime = (
            self._runtime_snapshot(thread_id)
            if turn is not None and self._model_success_recorder is not None
            and (not turn.runtime_invocation or turn.runtime_invocation.get("kind") == "skill")
            else None
        )
        saw_failed_turn = False
        model_recorded = False
        fence = self._open_fence(thread_id, turn)
        try:
            async for event in stream:
                if fence is not None and self._fence_violation(thread_id, turn, fence, event):
                    continue
                try:
                    model = parse_payload(event.event_type, event.payload)
                except AgentEventContractError as exc:
                    # A malformed adapter frame is a Runtime bug: surface it
                    # as a typed failure and end the turn instead of guessing
                    # what the engine meant.
                    LOG.error("runtime event rejected thread=%s: %s", thread_id, exc)
                    failure = _contract_failure(exc, engine_for_adapter(
                        self._manager.runtime_selection(thread_id).adapter_id))
                    turn_ref = turn.turn_id if turn else event.turn_id
                    if require_control_delivery:
                        raise ControlDeliveryError(failure.code) from exc
                    self._disposition_hints[thread_id] = DISPOSITION_NEEDS_RESTART
                    self._emit(thread_id, ev.EV_RUNTIME_ERROR, {
                        "agent_session_id": event.agent_session_id,
                        **({"turn_id": turn_ref} if turn_ref else {}),
                        "code": failure.code, "category": failure.category.value,
                        "reason": failure.reason, "message": failure.message,
                        "detail": failure.detail, "error": _public_failure(failure),
                    })
                    if turn_ref:
                        self._emit(thread_id, ev.EV_TURN_FAILED, {
                            "agent_session_id": event.agent_session_id,
                            "turn_id": turn_ref,
                            "reason": failure.reason,
                            "error": _public_failure(failure),
                        }, command_id=(turn.command_id or None) if turn else None)
                    saw_failed_turn = True
                    detach_reason = "event_contract"
                    return
                if application_context_delivery is not None and event.event_type is AgentEventType.TURN_STARTED:
                    session_id, revision = application_context_delivery
                    self._chat_contexts[session_id] = revision
                if event.event_type is AgentEventType.TURN_FAILED:
                    saw_failed_turn = True
                    if not self._turn_failure_keeps_session(model.error):
                        detach_reason = "turn_failed"
                        self._disposition_hints[thread_id] = DISPOSITION_NEEDS_RESTART
                if event.event_type is AgentEventType.RUNTIME_ERROR:
                    runtime_error = _public_failure(model.error)
                if event.event_type is AgentEventType.RUNTIME_EXITED:
                    detach_reason = "runtime_exited"
                    self._disposition_hints[thread_id] = DISPOSITION_NEEDS_RESTART
                if require_control_delivery and event.event_type in {
                    AgentEventType.RUNTIME_ERROR,
                    AgentEventType.TURN_FAILED,
                    AgentEventType.RUNTIME_EXITED,
                }:
                    # Control streams may report rejection as an event and end
                    # normally. Let the command handler retain the pending item;
                    # publishing a successful resolved event would lose it.
                    failure = getattr(model, "error", None)
                    raise ControlDeliveryError(
                        failure.code if failure is not None else event.event_type.value,
                        delivery_unknown=bool(failure is not None and failure.delivery_unknown),
                        reason=failure.reason if failure is not None else "",
                    )
                if (require_control_delivery
                        and event.event_type in {AgentEventType.APPROVAL_RESOLVED,
                                                AgentEventType.USER_INPUT_RESOLVED}):
                    # The command publishes resolution after the complete
                    # response stream succeeds. Other runtime streams still
                    # translate autonomous resolution normally.
                    continue
                if event.event_type is AgentEventType.MESSAGE_COMPLETED:
                    saw_completed_assistant = (
                        model.role == "assistant" and bool(model.text.strip())
                    ) or saw_completed_assistant
                if (
                    event.event_type is AgentEventType.TURN_COMPLETED
                    and verification_runtime is not None
                    and saw_completed_assistant
                    and not model_recorded and not saw_failed_turn
                    and not interrupted and runtime_error is None
                ):
                    try:
                        # Persist before publishing completion so the UI's
                        # subsequent credential fetch already sees verification.
                        await asyncio.to_thread(
                            self._model_success_recorder, verification_runtime,
                        )
                        model_recorded = True
                    except Exception:  # noqa: BLE001 — metadata must not fail a successful reply
                        LOG.warning("could not record model success for turn %s", turn.turn_id, exc_info=True)
                if event.event_type is AgentEventType.TURN_COMPLETED:
                    plugins = getattr(self, "chat_plugins", None)
                    thread = self._manager.get_thread(thread_id) if plugins is not None else None
                    if thread is not None and thread.mode == "conversation":
                        messages = [message for message in self._conv.list_current_messages(thread_id)
                                    if message.turn_id == (turn.turn_id if turn else event.turn_id)]
                        try:
                            await asyncio.to_thread(
                                plugins.publish_visualizations, thread_id, messages,
                                engine_for_adapter(self._manager.runtime_selection(thread_id).adapter_id),
                            )
                        except Exception:  # A failed visual stays a visible local error, never discards the reply.
                            LOG.warning("could not publish visual replies for thread %s", thread_id, exc_info=True)
                interrupted = self._translate(thread_id, turn, event, model) or interrupted
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
                self._disposition_hints[thread_id] = DISPOSITION_NEEDS_RESTART
                self._emit(thread_id, ev.EV_TURN_FAILED, {
                    "turn_id": turn.turn_id,
                    "error": error,
                }, command_id=turn.command_id or None)
                detach_reason = "stream_ended"
        except asyncio.CancelledError:
            detach_reason = "consume_cancelled"
            raise
        finally:
            if application_context_delivery is not None and (saw_failed_turn or runtime_error or interrupted or detach_reason):
                self._chat_contexts.pop(application_context_delivery[0], None)
            if detach_reason:
                await self._stop_detached_runtime(
                    thread_id, reason=f"conversation_runtime_detached:{detach_reason}")
            computer = getattr(self, "computer_control", None)
            current = self._conv.get_turn(turn.turn_id) if turn is not None else None
            if computer is not None and current is not None and current.status in {
                TURN_COMPLETED, TURN_FAILED, TURN_INTERRUPTED, TURN_CANCELLED,
            }:
                await computer.finish_turn(thread_id, turn.turn_id)

    def _open_fence(
        self, thread_id: str, turn: Optional[TurnRecord],
    ) -> Optional[_StreamFence]:
        """Bind a stream to the session its turn (or the thread) is running on."""
        session_id = ""
        if turn is not None:
            stored = self._conv.get_turn(turn.turn_id)
            session_id = str(
                (stored.agent_session_id if stored is not None else "")
                or turn.agent_session_id or "")
        if not session_id:
            session_id = str(
                getattr(self._conv.get_state(thread_id), "agent_session_id", "") or "")
        if not session_id:
            return None
        record = self._store.get(AgentSession, session_id)
        run_id = ""
        if turn is not None and turn.run_id:
            run_id = str(turn.run_id)
        elif record is not None and record.run_id:
            run_id = str(record.run_id)
        return _StreamFence(
            session_id=session_id,
            generation=record.execution_generation if record is not None else None,
            run_id=run_id,
        )

    def _fence_violation(
        self, thread_id: str, turn: Optional[TurnRecord],
        fence: _StreamFence, event: AgentEvent,
    ) -> str:
        """Drop events that belong to another session, run, or generation.

        Returns the reason ("" when the event is the stream's own); one typed
        warning per reason is published so the drop stays observable.
        """
        reason = ""
        if event.agent_session_id and event.agent_session_id != fence.session_id:
            reason = "foreign_session"
        elif fence.session_id in self._closed_sessions:
            reason = "session_replaced"
        elif (event.execution_generation is not None
              and fence.generation is not None
              and event.execution_generation != fence.generation):
            reason = "stale_generation"
        elif event.run_id:
            event_run = str(event.run_id)
            session_run = ""
            bound = self._store.get(AgentSession, fence.session_id)
            if bound is not None and bound.run_id:
                session_run = str(bound.run_id)
            if event_run not in {fence.run_id, session_run}:
                owned = self._conv.get_run(event_run)
                if owned is None or owned.thread_id != thread_id:
                    reason = "foreign_run"
        if not reason:
            return ""
        fence.dropped += 1
        if reason not in fence.reported:
            fence.reported.add(reason)
            LOG.warning(
                "fenced runtime event thread=%s reason=%s session=%s expected=%s",
                thread_id, reason, event.agent_session_id, fence.session_id)
            self._emit(thread_id, ev.EV_RUNTIME_WARNING, {
                "code": FENCED_EVENT_CODE,
                "reason": reason,
                "severity": "warning",
                "message": "dropped a runtime event that does not belong to the current session",
                "agent_session_id": event.agent_session_id or fence.session_id,
                "expected_agent_session_id": fence.session_id,
                "event_execution_generation": event.execution_generation,
                "expected_execution_generation": fence.generation,
                "event_run_id": event.run_id,
                "expected_run_id": fence.run_id,
                "event_type": event.event_type.value,
                **({"turn_id": turn.turn_id} if turn is not None else {}),
            }, command_id=(turn.command_id or None) if turn else None)
        return reason

    # -- AgentEvent → Thread 事件 ------------------------------------------------

    def _translate(
        self, thread_id: str, turn: Optional[TurnRecord], event: AgentEvent,
        model: Any = None,
    ) -> bool:
        """翻译一条统一 AgentEvent。返回本次是否观察到 interrupt 分类。

        ``model`` is the payload already validated by ``parse_payload``; the
        Thread event is built only from its normalized fields, so adapter
        payloads can never override executor-owned keys.
        """
        if model is None:
            model = parse_payload(event.event_type, event.payload)
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
        fields = dump_payload(model)
        native = fields.pop("native", None)
        if native:
            base["native"] = native
        out_type: Optional[str] = None
        payload: dict[str, Any] = {}

        def public(values: dict[str, Any], **extra: Any) -> dict[str, Any]:
            return {
                **{key: value for key, value in values.items() if value is not None},
                **extra,
                **base,
            }

        if etype in (AgentEventType.SESSION_STARTED, AgentEventType.SESSION_RESUMED):
            out_type = (ev.EV_SESSION_STARTED if etype is AgentEventType.SESSION_STARTED
                        else ev.EV_SESSION_RESUMED)
            payload = public(fields, runtime=self._runtime_snapshot(thread_id))
        elif etype is AgentEventType.SESSION_CLOSED:
            out_type, payload = ev.EV_SESSION_CLOSED, public(fields)
        elif etype is AgentEventType.TURN_STARTED:
            if turn is not None and event.turn_id:
                stored = self._conv.get_turn(turn.turn_id)
                if stored is not None:
                    self._conv.save_turn(stored.model_copy(update={"native_turn_id": event.turn_id}))
            # A session can serve multiple turns.  Snapshot the selection on
            # every turn so the UI can label historical replies after a later
            # endpoint or model switch.
            out_type, payload = ev.EV_TURN_STARTED, public(
                fields, runtime=self._runtime_snapshot(thread_id))
        elif etype is AgentEventType.MESSAGE_DELTA:
            if model.role != "assistant" or not model.text:
                return False
            out_type, payload = ev.EV_MESSAGE_DELTA, public(fields, role="assistant")
        elif etype is AgentEventType.MESSAGE_COMPLETED:
            if model.role != "assistant" or not model.text.strip():
                return False
            out_type, payload = ev.EV_MESSAGE_COMPLETED, public(fields, role="assistant")
        elif etype is AgentEventType.REASONING_SUMMARY:
            # Runtime-provided reasoning is a dedicated public stream so the UI
            # can place it inside the turn's expandable work process.
            out_type, payload = ev.EV_REASONING_SUMMARY, public(
                fields, reasoning_summary=model.text, partial=model.partial)
        elif etype in (AgentEventType.TOOL_STARTED, AgentEventType.TOOL_PROGRESS,
                       AgentEventType.TOOL_COMPLETED):
            out_type = {
                AgentEventType.TOOL_STARTED: ev.EV_TOOL_STARTED,
                # Progress/output deltas are not lifecycle starts (#218).
                AgentEventType.TOOL_PROGRESS: ev.EV_TOOL_PROGRESS,
                AgentEventType.TOOL_COMPLETED: ev.EV_TOOL_COMPLETED,
            }[etype]
            parent = fields.pop("parent_tool_call_id", None)
            extra: dict[str, Any] = {"call_id": model.tool_call_id}
            if model.name:
                extra["tool"] = model.name
            if parent:
                extra["parent_tool_use_id"] = parent
            if model.kind == "agent":
                extra["is_agent"] = True
            if model.status == "failed" or model.error:
                extra["is_error"] = True
            payload = public(fields, **extra)
        elif etype is AgentEventType.APPROVAL_REQUESTED:
            approval = dict(fields)
            unified_diff = approval.pop("unified_diff", None)
            if unified_diff:
                approval["diff"] = unified_diff
            if model.files:
                approval["files"] = [
                    {key: value for key, value in {
                        "path": item.path, "kind": item.change,
                        "old_path": item.old_path, "diff": item.unified_diff,
                    }.items() if value is not None}
                    for item in model.files
                ]
            if native:
                approval["native"] = native
            request = ApprovalRequest.from_payload(approval)
            normalized = normalize_approval_payload(request.to_payload())
            normalized.update(base)
            normalized = stamp_response_capability(
                normalized,
                agent_session_id=event.agent_session_id,
                generation=event.execution_generation)
            out_type, payload = ev.EV_APPROVAL_REQUESTED, normalized
        elif etype is AgentEventType.APPROVAL_RESOLVED:
            # ACP 等 Runtime 可以按会话策略自动完成审批；这类结果没有经过
            # Web 的 approval.resolve 命令，也必须进入投影以清除待处理状态。
            out_type, payload = ev.EV_APPROVAL_RESOLVED, public(fields)
        elif etype is AgentEventType.USER_INPUT_REQUESTED:
            pending = dict(fields)
            schema = pending.pop("requested_schema", None)
            if schema is not None:
                pending["schema"] = schema
            if native:
                pending["native"] = native
            normalized = normalize_pending_user_input(pending)
            normalized.update(base)
            out_type, payload = ev.EV_USER_INPUT_REQUESTED, normalized
        elif etype is AgentEventType.USER_INPUT_RESOLVED:
            # Native timeout/cancellation is authoritative too. The matching
            # request ID protects a newer question from a late acknowledgement.
            out_type, payload = ev.EV_USER_INPUT_RESOLVED, public(fields)
        elif etype is AgentEventType.USAGE_UPDATED:
            out_type, payload = ev.EV_USAGE_UPDATED, self._usage_payload(
                thread_id, model, base, event.native_type)
        elif etype is AgentEventType.PLAN_UPDATED:
            out_type, payload = ev.EV_PLAN_UPDATED, public(
                fields, source="adapter", native_type=event.native_type)
        elif etype is AgentEventType.AGENT_UPDATED:
            agents = [{**node, "turn_id": turn_id} for node in fields.pop("agents")]
            out_type, payload = ev.EV_AGENT_UPDATED, public(
                fields, agents=agents, source="adapter",
                native_type=event.native_type)
        elif etype is AgentEventType.RUNTIME_CAPABILITIES_UPDATED:
            out_type, payload = ev.EV_RUNTIME_CAPABILITIES_UPDATED, public(fields)
            # Runtime 已报告目录变化：同步拉会话级快照，避免 matrix 仍停在 revision 0。
            self.refresh_runtime_capabilities(thread_id)
        elif etype is AgentEventType.RUNTIME_WARNING:
            out_type, payload = ev.EV_RUNTIME_WARNING, public(
                fields, severity="warning")
        elif etype is AgentEventType.RUNTIME_ERROR:
            failure: AgentFailure = model.error
            out_type, payload = ev.EV_RUNTIME_ERROR, public(
                {}, code=failure.code, category=failure.category.value,
                reason=failure.reason, message=failure.message,
                detail=failure.detail, retryable=failure.retryable,
                delivery_unknown=failure.delivery_unknown,
                error=_public_failure(failure))
        elif etype is AgentEventType.ARTIFACT_CREATED:
            saved = self._save_agent_artifact(thread_id, turn, fields)
            out_type, payload = ev.EV_ARTIFACT_CREATED, public(
                {key: value for key, value in fields.items()
                 if key != "content_base64"}, **saved)
        elif etype is AgentEventType.WORKSPACE_CHANGED:
            diff = model.unified_diff or "\n".join(
                item.unified_diff for item in model.files if item.unified_diff)
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
            out_type, payload = ev.EV_WORKSPACE_CHANGED, public(
                {"files": fields.get("files")}, diff=diff, has_diff=bool(diff))
        elif etype is AgentEventType.TURN_COMPLETED:
            out_type, payload = ev.EV_TURN_COMPLETED, public(fields)
        elif etype is AgentEventType.TURN_FAILED:
            failure = model.error
            out_type = (ev.EV_TURN_INTERRUPTED
                        if failure.category is FailureCategory.CANCELLED
                        else ev.EV_TURN_FAILED)
            payload = public({}, reason=failure.reason,
                             error=_public_failure(failure))
            if failure.category is FailureCategory.CANCELLED:
                # Retain the terminal event family for existing SSE clients;
                # its typed status distinguishes refusal from our interrupt.
                payload["status"] = (
                    TURN_INTERRUPTED if failure.reason == "interrupted"
                    else TURN_CANCELLED)
        elif etype is AgentEventType.RUNTIME_EXITED:
            self._emit(
                thread_id, ev.EV_RUNTIME_EXITED, public(fields),
                command_id=(turn.command_id or None) if turn else None)
            return model.classification == EXIT_INTERRUPTED

        if out_type is not None:
            self._emit(thread_id, out_type, payload,
                       command_id=(turn.command_id or None) if turn else None)
        return False

    def _usage_payload(
        self, thread_id: str, model: Any, base: dict[str, Any],
        native_type: Optional[str],
    ) -> dict[str, Any]:
        """Normalized usage -> ``core.usage.updated``.

        ``usage`` uses the ledger's canonical names (``core.usage.normalize``)
        with ``input_includes_cache`` set, so no consumer guesses aliases.
        ``usage_native_type`` keeps the two values projections branch on
        (session cumulative / context-only) until they read ``usage_scope``.
        """
        usage = {
            key: value for key, value in {
                "input_tokens": model.input_tokens,
                "output_tokens": model.output_tokens,
                "cache_read_tokens": model.cached_input_tokens,
                "cache_write_tokens": model.cache_write_tokens,
                "reasoning_tokens": model.reasoning_tokens,
                "total_tokens": model.total_tokens,
                "model_context_window": model.context_window,
                "context_used": model.context_used_tokens,
                "reported_cost": model.cost_usd,
                "step_count": model.step_count,
                "llm_duration_ms": model.llm_duration_ms,
            }.items() if value is not None
        }
        usage["input_includes_cache"] = True
        # Version the canonical inclusive buckets so replay of pre-normalization
        # events can be repaired without adding cache/reasoning twice.
        usage["token_schema_version"] = 2
        compat_native = {
            "session_cumulative": "thread/tokenUsage/updated",
            "context_only": "acp.usage_update",
        }.get(model.scope, native_type)
        payload: dict[str, Any] = {
            "usage": usage,
            "usage_scope": model.scope,
            "usage_native_type": compat_native,
            "runtime": self._runtime_snapshot(thread_id),
        }
        if model.usage_id:
            # Ledger identity is thread-scoped; adapter ids are session-scoped.
            payload["usage_id"] = f"{base['agent_session_id']}:{model.usage_id}"
        payload.update(base)
        return payload

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
            instance_id=adapter.identity.instance_id,
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
        return await adapter.steer(ref, SteerInput(
            text=text,
            payload={
                "expected_turn_id": expected_turn_id,
                "client_user_message_id": client_message_id,
                "capability_revision": current_revision,
            },
        ))

    def background_turn_id(self, thread_id: str) -> str | None:
        source = self._continuation_sources.get(thread_id)
        if source is None or source[1].agent_session_id in self._closed_sessions:
            return None
        return source[0].background_turn_id(source[1])

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
        computer = getattr(self, "computer_control", None)
        turn_id = self._conv.get_state(thread_id).running_turn_id
        # The runtime is released after the stream drains, so the interrupted
        # event already reports the closed session.
        self._disposition_hints[thread_id] = DISPOSITION_CLOSED
        # Deliver cancellation to both owned runtimes. Starting the adapter's
        # request first lets its native events classify aborted tools correctly.
        interrupt_task = asyncio.create_task(adapter.interrupt(ref))
        try:
            if computer is not None and turn_id:
                await computer.abort_turn(thread_id, turn_id)
        finally:
            receipt = await interrupt_task
        if receipt.error is not None or receipt.state is not ReceiptState.COMPLETED:
            self._disposition_hints.pop(thread_id, None)
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

    def approval_response_capability(
        self, thread_id: str, row: dict[str, Any],
    ) -> dict[str, Any]:
        """Whether the runtime that issued ``row`` can still receive the answer."""
        state = self._conv.get_state(thread_id)
        record = self._session_record(thread_id)
        is_open = record is not None and record.closed_at is None
        session_id = record.agent_session_id if record is not None else ""
        generation = state.current_generation
        if record is not None and record.execution_generation is not None:
            generation = record.execution_generation
        return approval_response_capability(
            row,
            current_session_id=session_id,
            session_open=is_open,
            current_generation=generation,
            session_live=(session_id in self._live) if is_open else None,
        )

    async def resolve_approval(
        self, thread_id: str, approval_id: str, decision: str, note: str = "",
        scope: str = "once", option_id: str = "",
    ) -> None:
        """approval 答复送回 Runtime，并翻译其后续事件。"""
        adapter, ref = self._ref_for(thread_id)
        approval = ApprovalDecision.from_payload({
            "approval_id": approval_id,
            "decision": decision,
            "scope": scope,
            "note": note,
            "option_id": option_id,
        })
        stream = adapter.send(ref, ApprovalResponseInput(
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
        stream = adapter.send(ref, UserInputResponseInput(
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
        self._shutting_down = True
        running = set(self._tasks) | {thread_id for thread_id in self._continuation_sources
                                      if self.background_turn_id(thread_id)}
        for thread_id in running:
            try:
                await self.interrupt(thread_id)
            except Exception:  # noqa: BLE001 — 关停路径不阻断
                LOG.exception("shutdown interrupt failed: %s", thread_id)
        tasks = [
            *self._tasks.values(),
            *self._capability_tasks.values(),
            *self._continuation_tasks.values(),
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
        self._continuation_tasks.clear()
        self._continuation_sources.clear()
        self._native_wake_turns.clear()


__all__ = ["ExternalAgentSessionExecutor"]
