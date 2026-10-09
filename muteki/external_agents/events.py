"""统一 AgentEvent 事件层（RUNTIME-01，任务书 6.7）。

职责：

- 会话内单调序号分配（``EventSequencer``）；
- 标准 AgentEvent 事件（``AgentEventType``）的构造辅助，统一携带
  ``agent_session_id``、external session id、``run_id``、
- native event 保留：原生事件类型名进 ``native_type``，原始负载
  进 ``payload["native"]``，未知原生事件不修改核心状态机；
- payload 原样保留，供运行记录、黑板和报告完整展示。
"""

from __future__ import annotations

import threading
from typing import Any, Optional, Union

from pydantic import BaseModel

from muteki.platform.contracts.agent_events import dump_payload
from muteki.platform.contracts.external_agents import (
    AgentEvent,
    AgentEventType,
)

class EventSequencer:
    """一个 Agent Session 内的单调事件序号分配器（线程安全）。

    序号从 1 开始严格递增；Adapter 每条出站事件必须经同一个 sequencer
    分配，保证投影侧可以检洞与去重。
    """

    def __init__(self) -> None:
        self._lock = threading.Lock()
        self._seq = 0

    def next(self) -> int:
        with self._lock:
            self._seq += 1
            return self._seq

    @property
    def current(self) -> int:
        with self._lock:
            return self._seq


def build_event(
    event_type: AgentEventType,
    sequencer: EventSequencer,
    *,
    agent_session_id: str,
    external_session_id: Optional[str] = None,
    run_id: Optional[str] = None,
    execution_generation: Optional[int] = None,
    turn_id: Optional[str] = None,
    native_type: Optional[str] = None,
    payload: Union[BaseModel, dict[str, Any], None] = None,
) -> AgentEvent:
    """构造一条统一 AgentEvent：分配序号并保留完整 payload。

    ``payload`` 是 ``muteki.platform.contracts.agent_events`` 中该事件类型的
    payload 模型（或其 dump）；``native_type`` 保留原生事件类型名，引擎
    私有字段只放 ``payload["native"]``。执行器在边界按契约统一校验。
    """
    if isinstance(payload, BaseModel):
        payload = dump_payload(payload)
    return AgentEvent(
        event_type=event_type,
        agent_session_id=agent_session_id,
        external_session_id=external_session_id,
        run_id=run_id,
        execution_generation=execution_generation,
        turn_id=turn_id,
        seq=sequencer.next(),
        native_type=native_type,
        payload=payload or {},
    )


__all__ = [
    "EventSequencer",
    "build_event",
]
