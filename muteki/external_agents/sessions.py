"""Session/Turn identity、generation fencing 与退出分类（RUNTIME-01，任务书 7.4）。

- ``SessionTracker``：维护 Muteki 内部 ``AgentSession`` 记录（状态机
  ``preparing → active → closed|error``）；有 ``PlatformStore`` 时落库，
  否则进程内字典（测试与轻量场景）。
- ``EventProjector``：把统一 AgentEvent 投影到 ``AgentSessionSnapshot``；
  generation fencing——``execution_generation`` 低于当前 generation 的
  事件一律拒绝写入新投影；序号回退（重复/乱序）同样拒绝。
- ``classify_exit``：Runtime 进程退出分类为
  ``interrupted`` / ``failed`` / ``resumable`` / ``closed``。退出分类只
  决定 Session 收尾状态，**不产生** ``turn.completed``——turn 的完成只能
  来自 Runtime 的真实输出事件，Runtime 退出不能伪造完成事件。
"""

from __future__ import annotations

import threading
from typing import Any, Optional

from muteki.platform.contracts.base import utcnow
from muteki.platform.contracts.external_agents import (
    AgentEvent,
    AgentEventType,
    AgentSessionSnapshot,
    SessionStart,
)
from muteki.platform.contracts.objects import AgentSession

#: 退出分类。
EXIT_INTERRUPTED = "interrupted"   # 操作者取消 / 超时 / steer 切断
EXIT_FAILED = "failed"             # 非零退出或 Runtime 错误
EXIT_RESUMABLE = "resumable"       # 留有可恢复句柄，可用 resume 继续
EXIT_CLOSED = "closed"             # 干净退出且无可恢复句柄

#: AgentSession 内部状态。
STATE_PREPARING = "preparing"
STATE_ACTIVE = "active"
STATE_CLOSED = "closed"
STATE_ERROR = "error"


def classify_exit(
    *,
    returncode: Optional[int] = None,
    cancelled: bool = False,
    steered: bool = False,
    timed_out: bool = False,
    resume_handle: Optional[str] = None,
    error: str = "",
) -> str:
    """把 Runtime 进程退出归类为四种收尾状态之一。

    优先级：取消/超时 > 错误 > 可恢复 > 干净关闭。返回值只是 Session
    收尾语义，绝不作为 turn 完成的依据。
    """
    if cancelled or steered or timed_out:
        return EXIT_INTERRUPTED
    if error or (returncode is not None and returncode != 0):
        return EXIT_FAILED
    if resume_handle:
        return EXIT_RESUMABLE
    return EXIT_CLOSED


class SessionTracker:
    """内部 AgentSession 记录的状态机与存取。

    状态机：``preparing``（已建记录、未拿到 external session id）→
    ``active``（external id 回填）→ ``closed`` / ``error``。启动失败必须
    走 ``mark_error`` 并由调用方撤销 grant，不能停在 ``preparing``。
    """

    def __init__(self, store: Any = None) -> None:
        self._store = store
        self._lock = threading.RLock()
        self._memory: dict[str, AgentSession] = {}
        self._states: dict[str, str] = {}
        self._generations: dict[str, int] = {}
        self._exit_classifications: dict[str, str] = {}

    # -- 生命周期 ---------------------------------------------------------

    def create(
        self,
        request: SessionStart,
        *,
        adapter_id: str,
        runtime_instance_id: Optional[str] = None,
    ) -> AgentSession:
        """步骤 1：先建内部 AgentSession 记录（状态 preparing）。"""
        record = AgentSession(
            agent_session_id=request.agent_session_id,
            adapter_id=adapter_id,
            runtime_instance_id=runtime_instance_id,
            thread_id=request.thread_id,
            run_id=request.run_id,
            execution_generation=request.execution_generation,
            resume_handle=request.resume_handle,
        )
        with self._lock:
            if self._store is not None:
                self._store.save(record)
            self._memory[record.agent_session_id] = record
            self._states[record.agent_session_id] = STATE_PREPARING
            if request.execution_generation is not None:
                self._generations[record.agent_session_id] = int(
                    request.execution_generation)
        return record

    def activate(
        self,
        agent_session_id: str,
        *,
        external_session_id: Optional[str],
        resume_handle: Optional[str] = None,
    ) -> AgentSession:
        """步骤 5：external session id 回填，状态转 active。"""
        with self._lock:
            record = self._require(agent_session_id)
            record = record.model_copy(update={
                "external_session_id": external_session_id,
                "resume_handle": resume_handle or record.resume_handle,
            })
            self._save(record)
            self._states[agent_session_id] = STATE_ACTIVE
            return record

    def mark_error(self, agent_session_id: str, reason: str = "") -> None:
        """启动/运行失败：状态转 error（调用方负责撤销 grant）。"""
        with self._lock:
            self._states[agent_session_id] = STATE_ERROR

    def close(self, agent_session_id: str, classification: str = EXIT_CLOSED) -> None:
        """关闭：状态转 closed（退出分类记入 ``_exit_classifications``）。"""
        with self._lock:
            self._states[agent_session_id] = STATE_CLOSED
            record = self._memory.get(agent_session_id)
            if record is not None and record.closed_at is None:
                record = record.model_copy(update={"closed_at": utcnow()})
                self._save(record)
        self._exit_classifications[agent_session_id] = classification

    def exit_classification(self, agent_session_id: str) -> str:
        return self._exit_classifications.get(agent_session_id, "")

    # -- generation fencing ------------------------------------------------

    def set_generation(self, agent_session_id: str, generation: int) -> None:
        """resolve 产生新 execution generation 时抬升 fencing 水位。"""
        with self._lock:
            current = self._generations.get(agent_session_id)
            if current is None or generation > current:
                self._generations[agent_session_id] = int(generation)

    def current_generation(self, agent_session_id: str) -> Optional[int]:
        with self._lock:
            return self._generations.get(agent_session_id)

    # -- 查询 ---------------------------------------------------------------

    def state(self, agent_session_id: str) -> str:
        with self._lock:
            return self._states.get(agent_session_id, "")

    def get(self, agent_session_id: str) -> Optional[AgentSession]:
        with self._lock:
            if self._store is not None:
                loaded = self._store.get(AgentSession, agent_session_id)
                if loaded is not None:
                    self._memory[agent_session_id] = loaded
                    return loaded
            return self._memory.get(agent_session_id)

    # -- 内部 ---------------------------------------------------------------

    def _require(self, agent_session_id: str) -> AgentSession:
        record = self.get(agent_session_id)
        if record is None:
            raise KeyError(f"unknown agent session: {agent_session_id}")
        return record

    def _save(self, record: AgentSession) -> None:
        if self._store is not None:
            self._store.save(record)
        self._memory[record.agent_session_id] = record


class EventProjector:
    """统一 AgentEvent → AgentSessionSnapshot 的投影（含 fencing）。

    拒绝规则（返回 ``(False, snapshot, reason)``，不写入投影）：

    - ``stale_generation``：事件 generation 低于该 Session 当前
      generation（旧 generation 的事件不能写入新 generation 投影）；
    - ``duplicate_seq``：事件序号不大于已投影的最大序号（重放/乱序）。
    """

    def __init__(self, tracker: SessionTracker) -> None:
        self._tracker = tracker
        self._lock = threading.RLock()
        self._snapshots: dict[str, AgentSessionSnapshot] = {}

    def apply(
        self, event: AgentEvent
    ) -> "tuple[bool, AgentSessionSnapshot, str]":
        with self._lock:
            snapshot = self._snapshots.get(event.agent_session_id)
            if snapshot is None:
                snapshot = AgentSessionSnapshot(
                    agent_session_id=event.agent_session_id)
                self._snapshots[event.agent_session_id] = snapshot

            current_gen = self._tracker.current_generation(
                event.agent_session_id)
            if (event.execution_generation is not None
                    and current_gen is not None
                    and event.execution_generation < current_gen):
                return False, snapshot, "stale_generation"
            if event.seq and event.seq <= snapshot.last_event_seq:
                return False, snapshot, "duplicate_seq"

            snapshot = self._project(snapshot, event)
            self._snapshots[event.agent_session_id] = snapshot
            return True, snapshot, ""

    def snapshot(self, agent_session_id: str) -> AgentSessionSnapshot:
        with self._lock:
            return self._snapshots.get(
                agent_session_id,
                AgentSessionSnapshot(agent_session_id=agent_session_id),
            )

    # -- 内部 ---------------------------------------------------------------

    def _project(
        self, snapshot: AgentSessionSnapshot, event: AgentEvent
    ) -> AgentSessionSnapshot:
        update: dict[str, Any] = {"last_event_seq": max(
            snapshot.last_event_seq, event.seq)}
        if event.external_session_id and not snapshot.external_session_id:
            update["external_session_id"] = event.external_session_id

        etype = event.event_type
        if etype in (AgentEventType.SESSION_STARTED, AgentEventType.SESSION_RESUMED):
            update["state"] = "active"
        elif etype is AgentEventType.SESSION_CLOSED:
            update["state"] = "closed"
        elif etype is AgentEventType.TURN_STARTED:
            update["current_turn_id"] = event.turn_id
        elif etype in (AgentEventType.TURN_COMPLETED, AgentEventType.TURN_FAILED):
            if snapshot.current_turn_id == event.turn_id:
                update["current_turn_id"] = None
        elif etype is AgentEventType.APPROVAL_REQUESTED:
            update["pending_approval"] = event.payload
        elif etype is AgentEventType.APPROVAL_RESOLVED:
            update["pending_approval"] = None
        elif etype is AgentEventType.USER_INPUT_REQUESTED:
            update["pending_user_input"] = event.payload
        elif etype is AgentEventType.USER_INPUT_RESOLVED:
            update["pending_user_input"] = None
        elif etype is AgentEventType.USAGE_UPDATED:
            update["usage"] = dict(event.payload.get("usage") or event.payload)
        elif etype is AgentEventType.RUNTIME_ERROR:
            update["state"] = "error"
        return snapshot.model_copy(update=update)


__all__ = [
    "EXIT_CLOSED",
    "EXIT_FAILED",
    "EXIT_INTERRUPTED",
    "EXIT_RESUMABLE",
    "EventProjector",
    "SessionTracker",
    "classify_exit",
]
