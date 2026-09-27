"""命令回执与 outbox 契约（任务书 6.2）。

异步命令模式：命令持久化接收后立即返回 CommandReceipt，调用方通过
get_receipt、snapshot、after_cursor 事件读取或有界 wait 继续跟踪。
"""

from __future__ import annotations

from datetime import datetime
from enum import Enum
from typing import Any, Optional

from pydantic import Field

from .base import ContractModel, new_id, utcnow
from .errors import ErrorEnvelope


class ReceiptState(str, Enum):
    """命令回执状态。"""

    ACCEPTED = "accepted"      # 已持久化并进入处理流程
    RUNNING = "running"        # 执行者已经领取并开始处理
    WAITING = "waiting"        # 等待外部系统、审批或重试窗口
    COMPLETED = "completed"    # 领域事件与投影已提交
    FAILED = "failed"          # 处理失败（error 带分类与恢复建议）
    CONFLICT = "conflict"      # expected_version / 幂等内容冲突
    CANCELLED = "cancelled"    # 在允许取消的阶段终止


class AggregateRef(ContractModel):
    """命令作用的聚合引用，例如 {"type": "run", "id": "run_..."}。"""

    type: str = ""
    id: str = ""


class CommandReceipt(ContractModel):
    """命令接收回执（任务书 6.2 JSON）。

    ``accepted`` 只表示命令已持久化；``next`` 列出调用方可以继续使用的
    跟踪方式（receipt / snapshot / events / wait）。
    """

    command_id: str
    receipt_id: str = Field(default_factory=lambda: new_id("rcpt"))
    state: ReceiptState = ReceiptState.ACCEPTED
    run_id: Optional[str] = None
    aggregate: Optional[AggregateRef] = None
    # 绑定 aggregate/stream 与授权范围的不透明事件游标
    event_cursor: Optional[str] = None
    deduplicated: bool = False
    submitted_at: datetime = Field(default_factory=utcnow)
    next: list[str] = Field(
        default_factory=lambda: ["receipt", "snapshot", "events", "wait"]
    )
    # 命令产生的结构化小结果，例如 folder、archive operation 或更新后的摘要。
    # 长任务主体仍通过 snapshot/events/wait 跟踪。
    output: dict[str, Any] = Field(default_factory=dict)
    # 长操作的实际效果另有 EffectReceipt。命令回执只记录接受与关联关系，
    # 不能在 effect 仍为 pending/running 时伪造 completed。
    effect_ids: list[str] = Field(default_factory=list)
    error: Optional[ErrorEnvelope] = None


class EffectState(str, Enum):
    """外部效果执行状态；与命令接受状态分开持久化。"""

    ACCEPTED = "accepted"
    RUNNING = "running"
    WAITING = "waiting"
    COMPLETED = "completed"
    FAILED = "failed"
    CANCELLED = "cancelled"
    DEAD_LETTER = "dead_letter"


class EffectReceipt(ContractModel):
    """命令产生的一项可重试外部效果的持久回执。"""

    effect_id: str = Field(default_factory=lambda: new_id("effect"))
    command_id: str
    acceptance_receipt_id: str = ""
    domain: str = ""
    state: EffectState = EffectState.ACCEPTED
    aggregate: Optional[AggregateRef] = None
    correlation_id: str = ""
    outbox_id: str = ""
    destination: str = ""
    idempotency_key: str = ""
    attempts: int = 0
    max_attempts: int = 8
    retry_after: Optional[datetime] = None
    object_links: dict[str, str] = Field(default_factory=dict)
    output: dict[str, Any] = Field(default_factory=dict)
    error: Optional[ErrorEnvelope] = None
    created_at: datetime = Field(default_factory=utcnow)
    updated_at: datetime = Field(default_factory=utcnow)
    completed_at: Optional[datetime] = None


class OutboxStatus(str, Enum):
    PENDING = "pending"
    PROCESSING = "processing"
    DELIVERED = "delivered"
    FAILED = "failed"
    PAUSED = "paused"
    DEAD_LETTER = "dead_letter"
    CANCELLED = "cancelled"


class OutboxRecord(ContractModel):
    """持久 outbox 记录。

    跨 platform.db / competition.db / Run 存储 / SharedGraph 边界的外部
    副作用，通过 outbox 在事务提交后投递；重启后未完成的记录必须恢复。
    """

    outbox_id: str = Field(default_factory=lambda: new_id("outbox"))
    command_id: Optional[str] = None
    effect_id: Optional[str] = None
    correlation_id: str = ""
    idempotency_key: str = ""
    event_id: Optional[str] = None
    aggregate_type: str = ""
    aggregate_id: str = ""
    event_type: str = ""
    # 投递目标，例如 platform.ctfd / webhook / mcp.notify
    destination: str = ""
    status: OutboxStatus = OutboxStatus.PENDING
    attempts: int = 0
    payload: dict[str, Any] = Field(default_factory=dict)
    created_at: datetime = Field(default_factory=utcnow)
    delivered_at: Optional[datetime] = None
    last_error: Optional[str] = None
