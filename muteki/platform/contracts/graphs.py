"""GraphService 契约（任务书 8.3）。

三类正式实现（ctf.shared_graph.v1 / collaboration.graph.v1 /
memory.timeline.v1）共享事件、租约和投影基础语义，但不共享数据库、
不混用领域事件。这里只定义接口两侧的传输模型。
"""

from __future__ import annotations

from datetime import datetime
from typing import Any, Optional

from pydantic import Field

from .base import ContractModel, new_id, utcnow


class GraphEvent(ContractModel):
    """写入图服务的一条事件。"""

    graph_id: str = ""
    event_type: str = ""
    actor_id: str = "system"
    occurred_at: datetime = Field(default_factory=utcnow)
    idempotency_key: Optional[str] = None
    payload: dict[str, Any] = Field(default_factory=dict)


class GraphReceipt(ContractModel):
    """append 的接收回执。"""

    graph_id: str = ""
    event_id: str = Field(default_factory=lambda: new_id("gevt"))
    stream_seq: int = 0
    deduplicated: bool = False


class GraphScope(ContractModel):
    """snapshot 的读取范围，例如某条 branch、某类 fact。"""

    graph_id: str
    scope: dict[str, Any] = Field(default_factory=dict)


class GraphSnapshot(ContractModel):
    """图状态快照（已投影的读取模型）。"""

    graph_id: str
    watermark: int = 0
    state: dict[str, Any] = Field(default_factory=dict)


class ClaimRequest(ContractModel):
    """原子领取一个开放 Intent（或等价工作项）。"""

    graph_id: str
    intent_id: str
    claimant: str = ""
    ttl_seconds: int = 0


class ClaimReceipt(ContractModel):
    granted: bool = False
    graph_id: str = ""
    intent_id: str = ""
    claimant: str = ""
    fencing_token: int = 0
    expires_at: Optional[datetime] = None


class LeaseRequest(ContractModel):
    """申请一个图内独占资源（端口、监听器、目标会话等）的租约。"""

    graph_id: str
    resource_kind: str
    resource_key: str
    owner: str = ""
    ttl_seconds: int = 0


class LeaseReceipt(ContractModel):
    granted: bool = False
    graph_id: str = ""
    resource_kind: str = ""
    resource_key: str = ""
    owner: str = ""
    fencing_token: int = 0
    expires_at: Optional[datetime] = None
