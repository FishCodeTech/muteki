"""ConversationService：CONV-01 的装配入口（任务书 9.1）。

把 ConversationStore / ConversationProjection / ConversationManager /
ExternalAgentSessionExecutor 组装为一个服务束，并把 conversation.* 的
Command/Query Handler 注册到共享 ``MutekiCommandAPI``（幂等）。

INTEG-01 挂载时::

    service = ConversationService(
        store, registry=adapter_registry, binding_service=binding_service)
    service.register(command_api)
    app.include_router(create_conversation_router(
        command_api=command_api, service=service))

重启恢复：同一 platform.db 新建 ConversationService 后调用
``projection.rebuild_all()`` 追赶读模型水位；AgentSession 记录与 resume
handle 都在 PlatformStore 中，Executor 在下一个 Turn 时用 resume_handle
接管同一 external session。
"""

from __future__ import annotations

import asyncio
import logging
import time
from pathlib import Path
from typing import Any, Optional

from muteki.external_agents.registry import AdapterRegistry
from muteki.platform.capability_bindings import CapabilityBindingService
from muteki.platform.contracts.commands import ActorRef, CommandEnvelope
from muteki.platform.contracts.objects import AgentSession
from muteki.platform.contracts.receipts import ReceiptState
from muteki.platform.store import PlatformStore
from muteki.platform.contracts.agent_events import redact_secrets

from .commands import register_conversation_handlers
from . import events as conversation_events
from .executor import ExternalAgentSessionExecutor
from .manager import ConversationManager
from .projections import ConversationProjection
from .receipt_recovery import ConversationReceiptRecovery
from .store import ConversationStore
from .subagents import SubagentService, register_subagent_handlers

LOG = logging.getLogger(__name__)
QUOTA_RESUME_POLL_S = 30.0
CONVERSATION_PREFS_KEY = "conversation"


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
        self.manager.bind_capability_refresh_trigger(
            self.executor.refresh_runtime_capabilities,
        )
        self.manager.bind_capability_failure_provider(
            self.executor.capability_refresh_failure,
        )
        self.subagents = SubagentService(store, self.conv, self.manager)
        self.receipt_recovery = ConversationReceiptRecovery(
            store, self.conv, self.manager, self.subagents)
        self._command_api: Any = None
        self._quota_resume_task: Optional[asyncio.Task[None]] = None
        self._quota_auto_attempted: set[str] = set()
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
        # Runs after the projection so subagent observers read updated state.
        observe = self.subagents.observe
        if observe not in hooks:
            hooks.append(observe)

    def register(self, api: Any) -> None:
        """把 conversation.* Handler 注册到共享 Command API（幂等）。"""
        register_conversation_handlers(api, self.manager, self.executor)
        register_subagent_handlers(api, self.subagents)
        self._command_api = api

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
                        resume_probe_error = redact_secrets(f"{type(exc).__name__}: {exc}")
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
        if loop is not None and self._command_api is not None and (
            self._quota_resume_task is None or self._quota_resume_task.done()
        ):
            self._quota_resume_task = loop.create_task(self._quota_resume_loop())
        subagent_recovery = await self.subagents.recover()
        # Last: orphan Turns are already interrupted and subagent recovery has
        # settled its spawn receipts, so settlers read the post-restart state.
        command_receipts = await self.receipt_recovery.settle(self._command_api)
        return {
            "rebuilt_events": rebuilt,
            "interrupted_turns": interrupted,
            "queue_restart_failed": restart_failed,
            "queue_dispatch_scheduled": scheduled,
            "quota_resume_pending": [
                state.thread_id for state in self.conv.list_states() if state.quota_resume
            ],
            "subagent_recovery": subagent_recovery,
            "command_receipts": command_receipts,
        }

    async def _quota_resume_loop(self) -> None:
        while True:
            try:
                await self.run_due_quota_resumes()
            except asyncio.CancelledError:
                raise
            except Exception:  # noqa: BLE001 - one bad pass must not stop later schedules
                LOG.exception("quota resume pass failed")
            await asyncio.sleep(QUOTA_RESUME_POLL_S)

    async def run_due_quota_resumes(self, now: Optional[float] = None) -> list[dict[str, Any]]:
        """Resume threads whose provider quota window has reset.

        Goes through ``conversation.thread.resume`` so the normal state checks
        apply; a rejected resume clears the schedule with the typed error code
        instead of retrying every pass.
        """
        api = self._command_api
        if api is None:
            return []
        now = time.time() if now is None else now
        await self.schedule_quota_holds()
        results: list[dict[str, Any]] = []
        for state in self.conv.list_states():
            schedule = state.quota_resume
            if not schedule or state.status != "active":
                continue
            if float(schedule.get("resume_at") or 0) > now:
                continue
            if state.running_turn_id or self.conv.active_turn_id(state.thread_id):
                continue
            turn_id = str(schedule.get("turn_id") or "")
            actor = ActorRef(kind="system", id="quota-resume")
            receipt = await api.dispatch(CommandEnvelope(
                command_type="conversation.thread.resume",
                aggregate_type="thread",
                aggregate_id=state.thread_id,
                actor=actor,
                payload={"thread_id": state.thread_id},
                idempotency_key=f"quota-resume:{turn_id}",
            ))
            outcome: dict[str, Any] = {
                "thread_id": state.thread_id, "turn_id": turn_id,
                "state": receipt.state.value,
            }
            if receipt.state in {ReceiptState.FAILED, ReceiptState.CONFLICT, ReceiptState.CANCELLED}:
                code = receipt.error.code if receipt.error is not None else "unknown"
                outcome["error"] = code
                await api.dispatch(CommandEnvelope(
                    command_type="conversation.thread.quota_resume",
                    aggregate_type="thread",
                    aggregate_id=state.thread_id,
                    actor=actor,
                    payload={"thread_id": state.thread_id, "enabled": False,
                             "reason": f"resume_rejected:{code}"},
                    idempotency_key=f"quota-resume-clear:{turn_id}",
                ))
            results.append(outcome)
        return results

    def auto_resume_on_quota_reset(self) -> bool:
        prefs = self.conv.get_sidebar_prefs(CONVERSATION_PREFS_KEY) or {}
        return prefs.get("auto_resume_on_quota_reset") is True

    async def schedule_quota_holds(self) -> list[dict[str, Any]]:
        """Create schedules for rate-limited failures when the server pref is on.

        Only holds no one has decided on yet are considered, so a schedule the
        user cancelled stays cancelled. Goes through the normal command so the
        same failed-turn and timestamp checks apply.
        """
        api = self._command_api
        if api is None or not self.auto_resume_on_quota_reset():
            return []
        results: list[dict[str, Any]] = []
        for state in self.conv.list_states():
            hold = state.quota_hold
            if not hold or hold.get("handled") or state.quota_resume or state.status != "active":
                continue
            turn_id = str(hold.get("turn_id") or "")
            if not turn_id or turn_id in self._quota_auto_attempted:
                continue
            if state.running_turn_id or self.conv.active_turn_id(state.thread_id):
                continue
            current = self.conv.list_current_turns(state.thread_id)
            last = current[-1] if current else None
            if last is None or last.turn_id != turn_id or last.status != "failed":
                continue
            self._quota_auto_attempted.add(turn_id)
            receipt = await api.dispatch(CommandEnvelope(
                command_type="conversation.thread.quota_resume",
                aggregate_type="thread",
                aggregate_id=state.thread_id,
                actor=ActorRef(kind="system", id="quota-resume"),
                payload={
                    "thread_id": state.thread_id,
                    "enabled": True,
                    "turn_id": turn_id,
                    "resets_at": hold.get("resets_at"),
                    "kind": str(hold.get("kind") or ""),
                    "reason": "auto_pref",
                },
                idempotency_key=f"quota-auto:{turn_id}",
            ))
            outcome: dict[str, Any] = {
                "thread_id": state.thread_id, "turn_id": turn_id,
                "state": receipt.state.value,
            }
            if receipt.error is not None:
                outcome["error"] = receipt.error.code
                LOG.info("quota auto-schedule rejected for %s: %s",
                         state.thread_id, receipt.error.code)
            results.append(outcome)
        return results

    async def shutdown(self) -> None:
        if self._quota_resume_task is not None:
            self._quota_resume_task.cancel()
        await self.subagents.shutdown()
        await self.executor.shutdown()


__all__ = ["CONVERSATION_PREFS_KEY", "ConversationService"]
