"""Native non-prompt commands shared by the Pi and OMP RPC protocols."""
from __future__ import annotations

from typing import Any

from .command_providers import operation_item


RPC_OPERATIONS = {
    "compact": ("compact", "原生压缩上下文", "[保留内容的说明]"),
    "session": ("get_session_stats", "查看原生会话统计", ""),
    "stats": ("get_session_stats", "查看原生会话统计", ""),
    "context": ("get_state", "查看原生上下文与会话状态", ""),
    "autocompact": ("set_auto_compaction", "设置当前会话自动压缩", "[on|off]"),
    "autoretry": ("set_auto_retry", "设置当前会话自动重试", "on|off"),
}


def rpc_operation_items(adapter_id: str, engine: str):
    operations = dict(RPC_OPERATIONS)
    if engine == "pi":
        operations["tree"] = ("get_tree", "查看原生会话分支树", "")
    return [operation_item(adapter_id, engine, name, method, description, hint)
            for name, (method, description, hint) in operations.items()]


async def rpc_operation(adapter, session, name: str, arguments: str = "") -> dict[str, Any]:
    handle = adapter._rpc.get(session.agent_session_id)
    if not handle or handle.get("current_turn_id"):
        raise RuntimeError("请等待当前回复结束后执行命令")
    operations = {item.name: item for item in rpc_operation_items(adapter.id, adapter.id.split(".")[0])}
    item = operations.get(name.strip().lstrip("/"))
    if item is None:
        raise RuntimeError("当前引擎未提供此原生操作")
    method = item.invocation["method"]
    params: dict[str, Any] = {}
    if method == "compact" and arguments:
        params["customInstructions"] = arguments
    elif method in {"set_auto_compaction", "set_auto_retry"}:
        if not arguments and method == "set_auto_compaction":
            method = "get_state"
        elif arguments.casefold() in {"on", "off"}:
            params["enabled"] = arguments.casefold() == "on"
        else:
            raise ValueError(f"/{name} 需要 on 或 off")
    elif arguments:
        raise ValueError(f"/{name} 不接受参数")
    try:
        result = await adapter._cmd(handle["peer"], method, params, timeout=180 if method == "compact" else 30)
    except RuntimeError as exc:
        if method == "compact" and "nothing to compact" in str(exc).casefold():
            return {"status": "not_needed", "message": "当前上下文较短，无需压缩"}
        raise
    return {"status": "completed", "message": "上下文已由引擎原生压缩" if method == "compact" else f"已完成 /{name}", "result": result}
