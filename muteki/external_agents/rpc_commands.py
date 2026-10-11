"""Native non-prompt commands and context signals shared by the Pi and OMP RPC protocols."""
from __future__ import annotations

from typing import Any, Awaitable, Callable

from muteki.platform.contracts.agent_events import CompactionPayload, UsagePayload, dump_payload
from muteki.platform.contracts.external_agents import AgentEventType

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


async def rpc_operation(
    command: Callable[..., Awaitable[dict[str, Any]]],
    peer: Any,
    *,
    adapter_id: str,
    engine: str,
    name: str,
    arguments: str = "",
) -> dict[str, Any]:
    """Run one verified RPC operation through the adapter's own command channel.

    The caller owns session lookup and the busy-turn check; native errors
    (including Pi's "Nothing to compact") propagate unchanged.
    """
    operations = {item.name: item for item in rpc_operation_items(adapter_id, engine)}
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
    result = await command(peer, method, params, timeout=180 if method == "compact" else 30)
    outcome: dict[str, Any] = {
        "status": "completed",
        "message": "上下文已由引擎原生压缩" if method == "compact" else f"已完成 /{name}",
        "result": result,
    }
    data = result if isinstance(result, dict) else {}
    if method == "compact":
        # OMP's preserveData holds base64 snapshot frames the engine already
        # stored in its own session file; the receipt keeps everything else.
        if "preserveData" in data:
            outcome["result"] = {key: value for key, value in data.items() if key != "preserveData"}
        # Pi reports an estimate after compaction; OMP reports only the size before.
        after = _tokens(data.get("estimatedTokensAfter"))
        outcome["compaction"] = {
            "tokens_before": _tokens(data.get("tokensBefore")),
            "tokens_after": after,
            "summary": str(data.get("shortSummary") or "") or None,
        }
        if after is not None:
            outcome["context"] = {"used": after}
    elif method == "get_session_stats":
        usage = data.get("contextUsage") if isinstance(data.get("contextUsage"), dict) else {}
        outcome["context"] = {
            "used": _tokens(usage.get("tokens")),
            "limit": _tokens(usage.get("contextWindow")),
        }
    elif method == "get_state":
        model = data.get("model") if isinstance(data.get("model"), dict) else {}
        outcome["context"] = {"limit": _tokens(model.get("contextWindow"))}
    return outcome


def _tokens(value: Any) -> int | None:
    return value if isinstance(value, int) and not isinstance(value, bool) and value >= 0 else None


def rpc_context_window(state: Any) -> int | None:
    """``get_state`` → the active model's ``contextWindow``."""
    model = state.get("model") if isinstance(state, dict) else None
    value = _tokens(model.get("contextWindow")) if isinstance(model, dict) else None
    return value or None


def rpc_context_usage_event(
    usage: dict[str, Any], handle: dict[str, Any], *, label: str,
) -> tuple[Any, str, dict[str, Any]] | None:
    """Occupancy after one assistant message.

    The message's prompt (input plus both cache buckets) and its output are
    what the next request carries; ``get_session_stats.contextUsage.tokens``
    reports the same sum.
    """
    buckets = [_tokens(usage.get(key)) for key in ("input", "cacheRead", "cacheWrite", "output")]
    if all(value is None for value in buckets):
        return None
    return (AgentEventType.USAGE_UPDATED, f"{label}.context", dump_payload(UsagePayload(
        scope="context_only",
        context_used_tokens=sum(value or 0 for value in buckets),
        context_window=handle.get("context_window"),
    )))


def rpc_compaction_events(
    msg: dict[str, Any], handle: dict[str, Any], succeeded: bool, *, label: str,
) -> list[tuple[Any, str, dict[str, Any]]]:
    """``compaction_start``/``compaction_end`` (and ``auto_*``) → contract events."""
    etype = str(msg.get("type") or "")
    trigger = "manual" if str(msg.get("reason") or "") == "manual" else "auto"
    if etype.endswith("_start"):
        return [(AgentEventType.COMPACTION_UPDATED, f"{label}.{etype}",
                 dump_payload(CompactionPayload(status="started", trigger=trigger)))]
    result = msg.get("result") if isinstance(msg.get("result"), dict) else {}
    after = _tokens(result.get("estimatedTokensAfter"))
    if succeeded:
        payload = CompactionPayload(
            status="completed", trigger=trigger,
            tokens_before=_tokens(result.get("tokensBefore")), tokens_after=after,
            summary=str(result.get("shortSummary") or "") or None)
    else:
        reason = msg.get("errorMessage") or (
            "skipped" if msg.get("skipped") else "aborted" if msg.get("aborted") else "")
        payload = CompactionPayload(
            status="failed", trigger=trigger, message=str(reason) or None)
    events = [(AgentEventType.COMPACTION_UPDATED, f"{label}.{etype}", dump_payload(payload))]
    if succeeded and after is not None:
        events.append((AgentEventType.USAGE_UPDATED, f"{label}.context", dump_payload(UsagePayload(
            scope="context_only", context_used_tokens=after,
            context_window=handle.get("context_window")))))
    return events
