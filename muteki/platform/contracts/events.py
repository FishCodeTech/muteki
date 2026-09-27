"""平台事件契约（任务书 6.1、设计文档 8.3/8.5）。

事件命名空间约定：``core.*`` / ``run.*`` / ``competition.*`` / ``ctf.*`` /
``pentest.*`` / ``ext.<id>.*``。内部 Domain Event、查询 Projection 和前端
Public Event 使用同一份完整事件内容。
"""

from __future__ import annotations

from datetime import datetime
from typing import Any, Optional

from pydantic import Field, field_validator

from .base import ContractModel, new_id, utcnow

#: 事件命名空间前缀
NS_CORE = "core."
NS_RUN = "run."
NS_COMPETITION = "competition."
NS_CTF = "ctf."
NS_PENTEST = "pentest."
NS_EXT_PREFIX = "ext."

#: 全部内置命名空间前缀（ext.<id>.* 单独判断）
BUILTIN_NAMESPACES = (NS_CORE, NS_RUN, NS_COMPETITION, NS_CTF, NS_PENTEST)


def ext_namespace(extension_id: str) -> str:
    """扩展事件命名空间，例如 ext.org.example.*。"""
    ext_id = str(extension_id or "").strip().strip(".")
    if not ext_id:
        raise ValueError("extension_id cannot be empty")
    return f"{NS_EXT_PREFIX}{ext_id}."


def is_known_namespace(event_type: str) -> bool:
    """判断事件类型是否落在已知命名空间内。"""
    text = str(event_type or "")
    if text.startswith(NS_EXT_PREFIX):
        return True
    return any(text.startswith(prefix) for prefix in BUILTIN_NAMESPACES)


class EventEnvelope(ContractModel):
    """新增领域事件的统一 envelope（任务书 6.1 JSON）。"""

    event_id: str = Field(default_factory=lambda: new_id("evt"))
    aggregate_type: str = ""
    aggregate_id: str = ""
    # 在 (aggregate_type, aggregate_id) 流内的单调序号
    stream_seq: int = 0
    event_type: str = ""
    occurred_at: datetime = Field(default_factory=utcnow)
    # 产生方，例如 builtin.competition / ext.<id>
    producer: str = ""
    actor_id: str = "system"
    command_id: Optional[str] = None
    causation_id: Optional[str] = None
    correlation_id: Optional[str] = None
    idempotency_key: Optional[str] = None
    payload: dict[str, Any] = Field(default_factory=dict)

    @field_validator("event_type")
    @classmethod
    def _event_type_namespaced(cls, value: str) -> str:
        text = value.strip()
        if not text:
            raise ValueError("event_type cannot be empty")
        if not is_known_namespace(text):
            raise ValueError(
                f"event_type must use a known namespace "
                f"(core.*/run.*/competition.*/ctf.*/pentest.*/ext.<id>.*): {text!r}"
            )
        return text


class PublicEvent(ContractModel):
    """面向 SSE/WebSocket/AG-UI 的完整公开事件。"""

    event_id: str = ""
    aggregate_type: str = ""
    aggregate_id: str = ""
    stream_seq: int = 0
    event_type: str = ""
    occurred_at: datetime = Field(default_factory=utcnow)
    producer: str = ""
    actor_id: str = "system"
    command_id: Optional[str] = None
    causation_id: Optional[str] = None
    correlation_id: Optional[str] = None
    idempotency_key: Optional[str] = None
    payload: dict[str, Any] = Field(default_factory=dict)
