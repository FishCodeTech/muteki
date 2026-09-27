"""契约模型共享基类与工具。

所有平台契约模型继承 ``ContractModel``：禁止未知字段（extra="forbid"），
时间字段统一使用 timezone-aware ``datetime``，JSON 序列化为 ISO8601 字符串。
"""

from __future__ import annotations

import uuid
from datetime import datetime, timezone

from pydantic import BaseModel, ConfigDict

#: 平台契约的整体 schema 版本。单个模型/事件类型可以在此之上独立演进自己的
#: ``schema_version`` 字段；只有不兼容变更才递增。
CONTRACT_SCHEMA_VERSION = 1


def utcnow() -> datetime:
    """当前 UTC 时间（timezone-aware），序列化后为 ISO8601。"""
    return datetime.now(timezone.utc)


def new_id(prefix: str) -> str:
    """生成带前缀的稳定 id，风格与 ``muteki.control.models._id`` 一致。"""
    return f"{prefix}-{uuid.uuid4().hex}"


class ContractModel(BaseModel):
    """平台契约基类：拒绝未知字段，保证 schema 与 TS 类型对齐。"""

    model_config = ConfigDict(extra="forbid")

    schema_version: int = CONTRACT_SCHEMA_VERSION
