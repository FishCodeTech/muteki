"""旧版第三方命令目录兼容入口。

第三方命令由 Runtime session 的协议接口提供：ACP 的
``available_commands_update``、Claude SDK 的 ``system/init``、OpenCode
Server 的 ``GET /command``，以及 Codex App Server 的结构化管理方法。

本模块保留函数名，避免旧调用方导入失败；它不再返回任何未经当前 Session
确认的命令。Muteki 自有的 ``/new``、``/clear`` 由
``composer_capabilities`` 单独维护。
"""

from __future__ import annotations


ENGINE_DISPLAY_NAMES: dict[str, str] = {
    "claude": "Claude Code",
    "codex": "Codex",
    "cursor": "Cursor",
    "pi": "Pi",
    "omp": "OMP",
    "kimi": "Kimi Code",
    "grok": "Grok",
    "opencode": "OpenCode",
    "devin": "Devin CLI",
    "droid": "Droid",
}


def engine_command_items(engine: str) -> list[dict[str, str]]:
    """返回空目录；第三方命令必须从当前 Runtime session 读取。"""
    del engine
    return []


__all__ = ["ENGINE_DISPLAY_NAMES", "engine_command_items"]
