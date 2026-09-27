"""Command API 的内置命令 / 查询 Handler（COMMAND-01）。

``register_builtin_handlers`` 把产品内置 Handler 挂到 Command API：
- ``run.*``：经 RunGateway 复用 muteki/control 控制路径（见 run_handlers）；
- ``task.*``：platform.db 事件溯源聚合（见 task_handlers）。

领域模块 / 扩展不得在这里之外复制权限、幂等、回执与恢复逻辑。
"""

from __future__ import annotations

from muteki.platform.command_handlers.base import (
    CommandAPIError,
    CommandFailed,
    CommandHandler,
    CommandPlan,
    CommandPolicy,
    HandlerContext,
    HandlerRegistry,
    PolicyDenied,
    QueryHandler,
    SideEffectResult,
)
from muteki.platform.command_handlers.cursor import (
    CursorError,
    assert_cursor_scope,
    decode_cursor,
    encode_cursor,
    load_cursor_key,
    scope_digest,
)
from muteki.platform.command_handlers.run_handlers import (
    RunCommandHandler,
    RunQueryHandler,
)
from muteki.platform.command_handlers.task_handlers import (
    TaskCommandHandler,
    TaskQueryHandler,
)


def register_builtin_handlers(api: object) -> None:
    """把内置 Handler 注册到 Command API（幂等性由 HandlerRegistry 去重保证）。"""
    api.register_command(RunCommandHandler())
    api.register_command(TaskCommandHandler())
    api.register_query(RunQueryHandler())
    api.register_query(TaskQueryHandler())


__all__ = [
    "CommandAPIError",
    "CommandFailed",
    "CommandHandler",
    "CommandPlan",
    "CommandPolicy",
    "CursorError",
    "HandlerContext",
    "HandlerRegistry",
    "PolicyDenied",
    "QueryHandler",
    "RunCommandHandler",
    "RunQueryHandler",
    "SideEffectResult",
    "TaskCommandHandler",
    "TaskQueryHandler",
    "assert_cursor_scope",
    "decode_cursor",
    "encode_cursor",
    "load_cursor_key",
    "register_builtin_handlers",
    "scope_digest",
]
