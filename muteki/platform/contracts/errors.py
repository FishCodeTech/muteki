"""统一错误 envelope（任务书 6.1、6.6，完成条件 17.9）。

所有入口（Web、MCP、Native Tool、Agent Plugin、HTTP/JSON-RPC）
返回相同结构的错误：code、message、错误分类、恢复建议和 correlation id。
"""

from __future__ import annotations

from enum import Enum
from typing import Any, Optional

from pydantic import Field

from .base import ContractModel


class ErrorCategory(str, Enum):
    """错误分类，调用方据此决定重试、升级或放弃。"""

    VALIDATION = "validation"      # 请求格式或字段校验失败
    PERMISSION = "permission"      # Principal/Binding/资源范围判定拒绝
    NOT_FOUND = "not_found"        # 对象或流不存在
    CONFLICT = "conflict"          # expected_version / binding key 冲突、幂等重放内容不一致
    STATE = "state"                # 状态机不允许当前操作
    RATE_LIMIT = "rate_limit"      # 限速
    TIMEOUT = "timeout"            # 有界 wait 或外部调用超时
    RUNTIME = "runtime"            # 外部 Agent Runtime 侧错误
    PLATFORM = "platform"          # 比赛平台侧错误
    INTERNAL = "internal"          # 平台内部错误


class ErrorEnvelope(ContractModel):
    """统一错误 envelope。"""

    # 稳定机器码，例如 capability.binding.revoked / run.conflict
    code: str
    message: str
    category: ErrorCategory = ErrorCategory.INTERNAL
    # 面向调用方的恢复建议，例如 "refresh snapshot and retry"
    recovery_hint: str = ""
    correlation_id: Optional[str] = None
    retryable: bool = False
    detail: dict[str, Any] = Field(default_factory=dict)
