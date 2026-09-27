"""Muteki Command API 的命令、查询与事件读取信封（任务书 6.1、6.2）。

所有会改变持久状态的产品操作必须进入 typed command；查询、事件读取和
有界 wait 复用同一 Principal、CapabilityBinding、资源范围和审计边界。
"""

from __future__ import annotations

from typing import Any, Optional

from pydantic import Field

from .base import ContractModel, new_id
from .events import EventEnvelope


class ActorRef(ContractModel):
    """命令发起方，例如 {"kind": "operator", "id": "local-user"}。"""

    # operator | system | worker | agent | extension
    kind: str = "system"
    id: str = ""
    # Agent 调用必须携带 Gateway 已核验的确切 CapabilityBinding。仅用
    # principal_id 会在同一用户拥有多个 Thread Binding 时发生串线。
    binding_id: str = ""
    binding_version: Optional[int] = None
    thread_id: str = ""


class CommandEnvelope(ContractModel):
    """typed command 信封（任务书 6.1 JSON）。

    处理顺序固定：读取已有 receipt → 校验 expected_version → 产生领域事件
    → 同一事务写事件、投影、receipt 和 outbox → 提交 → 执行外部副作用。
    """

    command_id: str = Field(default_factory=lambda: new_id("cmd"))
    command_type: str = ""
    aggregate_type: str = ""
    aggregate_id: str = ""
    expected_version: Optional[int] = None
    actor: ActorRef = Field(default_factory=ActorRef)
    idempotency_key: Optional[str] = None
    payload: dict[str, Any] = Field(default_factory=dict)


class QueryEnvelope(ContractModel):
    """只读查询信封。查询不产生事件，但走同一授权边界。"""

    query_id: str = Field(default_factory=lambda: new_id("qry"))
    query_type: str = ""
    aggregate_type: Optional[str] = None
    aggregate_id: Optional[str] = None
    actor: ActorRef = Field(default_factory=ActorRef)
    params: dict[str, Any] = Field(default_factory=dict)


class EventReadRequest(ContractModel):
    """按 aggregate 流读取事件。cursor 是绑定流与授权范围的不透明值。"""

    aggregate_type: str
    aggregate_id: str
    after_cursor: Optional[str] = None
    limit: int = 100


class WaitRequest(ContractModel):
    """有界等待：到达服务端上限时返回 timeout 和最新 cursor。"""

    aggregate_type: str
    aggregate_id: str
    after_cursor: Optional[str] = None
    timeout_seconds: float = 30.0
    limit: int = 100


class QueryResult(ContractModel):
    """查询结果。result 为完整投影数据。"""

    query_id: Optional[str] = None
    query_type: str = ""
    result: Any = None
    cursor: Optional[str] = None


class EventPage(ContractModel):
    """一页事件。next_cursor 仅在还有后续事件时返回。"""

    events: list[EventEnvelope] = Field(default_factory=list)
    next_cursor: Optional[str] = None


class WaitResult(ContractModel):
    """有界 wait 的结果：新事件或 timeout（附带最新 cursor）。"""

    # events | timeout
    state: str = "timeout"
    events: list[EventEnvelope] = Field(default_factory=list)
    cursor: Optional[str] = None
