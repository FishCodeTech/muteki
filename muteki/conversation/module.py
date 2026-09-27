"""ConversationService：CONV-01 的装配入口（任务书 9.1）。

把 ConversationStore / ConversationProjection / ConversationManager /
ExternalAgentSessionExecutor 组装为一个服务束，并把 conversation.* 的
Command/Query Handler 注册到共享 ``MutekiCommandAPI``（幂等）。

INTEG-01 挂载时::

    service = ConversationService(
        store, registry=adapter_registry, binding_service=binding_service)
    service.register(command_api)
    app.include_router(create_conversation_router(
        command_api=command_api, service=service, runtime_service=...))

重启恢复：同一 platform.db 新建 ConversationService 后调用
``projection.rebuild_all()`` 追赶读模型水位；AgentSession 记录与 resume
handle 都在 PlatformStore 中，Executor 在下一个 Turn 时用 resume_handle
接管同一 external session。
"""

from __future__ import annotations

import asyncio
from pathlib import Path
from typing import Any, Optional

from muteki.external_agents.registry import AdapterRegistry
from muteki.platform.capability_bindings import CapabilityBindingService
from muteki.platform.contracts.objects import AgentSession
from muteki.platform.store import PlatformStore

from .commands import register_conversation_handlers
from . import events as conversation_events
from .executor import ExternalAgentSessionExecutor
from .manager import ConversationManager
from .projections import ConversationProjection
from .store import ConversationStore


class ConversationService:
    """Conversation 后端服务束（无自有事件循环状态；执行在 Executor）。"""

    def __init__(
        self,
        store: PlatformStore,
        *,
        registry: Optional[AdapterRegistry] = None,
        binding_service: Optional[CapabilityBindingService] = None,
        memory_graph: Any = None,
        workspace_root: str | Path | None = None,
        sessions_root: str | Path | None = None,
    ) -> None:
        resolved_sessions_root = Path(
            sessions_root
            if sessions_root is not None
            else Path(store.db_path).parent.parent
        )
        self.platform = store
        self.conv = ConversationStore(store)
        self.projection = ConversationProjection(store, self.conv, sessions_root=resolved_sessions_root)
        self.bindings = binding_service or CapabilityBindingService(store)
        self.registry = registry or AdapterRegistry()
        self.manager = ConversationManager(
            store, self.conv, self.bindings, self.projection,
            memory_graph=memory_graph, workspace_root=workspace_root,
            adapter_registry=self.registry,
            sessions_root=resolved_sessions_root)
        self.executor = ExternalAgentSessionExecutor(
            store,
            self.conv,
            self.manager,
            self.registry,
            sessions_root=resolved_sessions_root,
        )
        self.manager.bind_capability_snapshot_provider(
            self.executor.cached_runtime_capabilities,
        )
        self.manager.bind_capability_cache_writer(
            lambda thread_id, snapshot: self.executor._capability_cache.__setitem__(
                thread_id, snapshot
            ),
        )
        self.manager.bind_capability_refresh_trigger(
            self.executor.refresh_runtime_capabilities,
        )
        self.manager.bind_capability_failure_provider(
            self.executor.capability_refresh_failure,
        )
        self._install_projection_hook()

    def _install_projection_hook(self) -> None:
        """Command API 与 Executor 共用 ``append_events``：追加后立刻投影。

        ``conversation.turn.send`` 等命令事件（``core.turn.requested``、
        ``core.thread.archived``）由 Command API 写入，原先只有 Executor
        自管事件才调用投影，导致 user 消息、归档状态丢失。
        """
        store = self.platform
        hooks = getattr(store, "_conversation_projection_hooks", None)
        if hooks is None:
            original = store.append_events
            hooks = []

            def append_events(events, *, expected_version=None):
                stored = original(events, expected_version=expected_version)
                for hook in list(hooks):
                    for event in stored:
                        hook(event)
                return stored

            store.append_events = append_events
            store._conversation_projection_hooks = hooks
        apply = self.projection.apply
        if apply not in hooks:
            hooks.append(apply)

    def register(self, api: Any) -> None:
        """把 conversation.* Handler 注册到共享 Command API（幂等）。"""
        register_conversation_handlers(api, self.manager, self.executor)

    def rebuild(self) -> int:
        """重启后追赶全部 Thread 读模型水位，返回应用的的事件条数。"""
        return self.projection.rebuild_all()

    async def recover(self) -> dict[str, Any]:
        """恢复投影，并为遗留 Turn 重新确认可恢复能力后标记中断。

        ``AdapterRegistry`` 的 probe 缓存只存在于当前进程。服务重启后不能
        用空缓存把仍然拥有 resume handle 的 ACP 会话降级成“只能重试”。
        因此这里只对遗留活动 Turn 对应的 Runtime 做窄范围 probe；正常没有
        活动 Turn 的启动不会等待所有引擎探测。
        """
        rebuilt = self.projection.rebuild_all()
        interrupted: list[dict[str, Any]] = []
        for turn in self.conv.list_active_turns():
            session = (
                self.platform.get(AgentSession, turn.agent_session_id)
                if turn.agent_session_id else None
            )
            resume_available = False
            adapter_id = ""
            instance_id = "default"
            resume_probe_error = ""
            if session is not None:
                adapter_id = session.adapter_id
                instance_id = session.runtime_instance_id or "default"
                record = self.registry.record(adapter_id, instance_id)
                report = record.last_probe if record is not None else None
                if report is None and record is not None and session.resume_handle:
                    try:
                        report = await self.registry.probe(
                            adapter_id, instance_id)
                    except Exception as exc:  # noqa: BLE001
                        resume_probe_error = f"{type(exc).__name__}: {exc}"[:240]
                resume_available = bool(
                    session.resume_handle
                    and report is not None
                    and report.capabilities.resume
                )
            self.platform.append_events([conversation_events.thread_event(
                turn.thread_id,
                conversation_events.EV_TURN_INTERRUPTED,
                {
                    "turn_id": turn.turn_id,
                    "phase": "process_restart",
                    "resume_available": resume_available,
                    "adapter_id": adapter_id,
                    "instance_id": instance_id,
                    "resume_probe_error": resume_probe_error,
                    "recovery_action": (
                        "resume" if resume_available else "retry"
                    ),
                },
            )])
            interrupted.append({
                "thread_id": turn.thread_id,
                "turn_id": turn.turn_id,
                "resume_available": resume_available,
                "resume_probe_error": resume_probe_error,
                "recovery_action": "resume" if resume_available else "retry",
            })
            if self.conv.list_queue(turn.thread_id):
                state = self.conv.pause_queue(
                    turn.thread_id, "process_restart_with_active_turn")
                self.platform.append_events([conversation_events.thread_event(
                    turn.thread_id,
                    conversation_events.EV_QUEUE_PAUSED,
                    {
                        "reason": "process_restart_with_active_turn",
                        "turn_id": turn.turn_id,
                        "queue_revision": state.queue_revision,
                    },
                )])

        restart_failed: list[str] = []
        for state in self.conv.list_states():
            queue = self.conv.list_queue(state.thread_id)
            dispatching = next(
                (item for item in queue if item.status == "dispatching"), None
            )
            if dispatching is not None:
                self.conv.mark_queue_failed(dispatching.queue_id, {
                    "code": "conversation.queue.process_restart",
                    "message": "服务重启时该队列消息正在发送，请确认后继续",
                })
                restart_failed.append(dispatching.queue_id)

        scheduled: list[str] = []
        try:
            loop = asyncio.get_running_loop()
        except RuntimeError:
            loop = None
        if loop is not None:
            for state in self.conv.list_states():
                if (
                    state.queue_count
                    and not state.queue_paused
                    and not state.running_turn_id
                    and not self.conv.active_turn_id(state.thread_id)
                ):
                    loop.create_task(self.executor.start_next_queued(state.thread_id))
                    scheduled.append(state.thread_id)
        return {
            "rebuilt_events": rebuilt,
            "interrupted_turns": interrupted,
            "queue_restart_failed": restart_failed,
            "queue_dispatch_scheduled": scheduled,
        }

    async def shutdown(self) -> None:
        await self.executor.shutdown()


__all__ = ["ConversationService"]
