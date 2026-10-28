"""Command policy for the chat transports.

Native catalogs remain authoritative for executable prompts. Client commands
reuse Muteki's existing controls; terminal commands never become model prompts.
Protocol operations are supplied by each adapter after opening its session.
The per-engine catalog comes from the provider descriptors; engines without a
declared catalog get no client or terminal-only rows.
"""
from __future__ import annotations

from dataclasses import dataclass
from typing import Any

from .descriptors import all_descriptors
from .runtime_capabilities import RuntimeCapabilityItem, RuntimeCapabilitySnapshot


@dataclass(frozen=True)
class CommandProvider:
    engine: str
    aliases: dict[str, str]
    terminal_only: tuple[str, ...]


PROVIDERS: dict[str, CommandProvider] = {
    descriptor.engine: CommandProvider(
        descriptor.engine,
        dict(descriptor.commands.client_aliases),
        tuple(descriptor.commands.terminal_only or ()),
    )
    for descriptor in all_descriptors()
    if descriptor.commands.defined
}
_DESCRIPTIONS = {
    "skills": "查看当前引擎的 Skills", "plugins": "管理 Muteki 聊天扩展", "mcp": "查看当前引擎的 MCP",
    "help": "查看当前引擎的全部聊天命令", "model": "选择当前聊天模型",
    "effort": "调整当前聊天的思考强度", "permissions": "调整当前聊天的权限模式",
    "rename": "重命名当前聊天", "sessions": "查找并切换 Muteki 中的聊天",
    "fork": "从当前聊天分叉，保留原记录", "export": "导出当前聊天",
    "status": "查看当前聊天状态与用量", "copy": "复制最后一条助手回复",
    "rewind": "选择需要回退的轮次", "diff": "查看当前聊天的文件变更",
    "details": "查看当前聊天的执行详情", "new": "开始空白聊天，保留历史记录",
}


def client_commands(engine: str, native_names: set[str]) -> list[dict[str, Any]]:
    provider = PROVIDERS.get(engine)
    if provider is None:
        return []
    rows = []
    for name, action in provider.aliases.items():
        # State-changing UI controls stay in the same persisted Muteki runtime
        # selection. Read-only native operations retain their richer output.
        if name in native_names and action in {"help", "status", "skills", "plugins", "mcp"}:
            continue
        rows.append({
            "id": f"client:{engine}:{name}", "kind": "command", "name": name,
            "description": _DESCRIPTIONS[action], "source": "Muteki 聊天",
            "scope": "conversation", "engine": engine, "channel": "local_handler",
            "action": f"ui:{action}", "invocable": True, "support_level": "supported",
            "argument_hint": {"model": "[模型]", "rename": "[名称]", "effort": "[强度]"}.get(action, ""),
        })
    return rows


def unavailable_commands(engine: str, native_names: set[str]) -> list[dict[str, Any]]:
    provider = PROVIDERS.get(engine)
    if provider is None:
        return []
    return [{
        "id": f"unavailable:{engine}:{name}", "kind": "command", "name": name,
        "description": "当前聊天协议未提供此操作", "source": engine,
        "scope": "engine", "engine": engine, "action": "inspect-runtime-capability",
        "channel": "local_handler", "invocable": False, "support_level": "unsupported",
        "reason": "仅终端或原客户端可用，当前协议未公布可执行接口",
        "alternative": "请在原客户端使用；不会把该命令当作普通消息发送给模型",
    } for name in provider.terminal_only if name not in native_names]


def unavailable_compact(engine: str, adapter_id: str) -> dict[str, Any]:
    """Greyed /compact for a live session whose runtime publishes no compaction."""
    return {
        "id": f"unavailable:{engine}:compact", "kind": "command", "name": "compact",
        "description": "压缩当前会话上下文", "source": adapter_id,
        "scope": "engine", "engine": engine, "action": "inspect-runtime-capability",
        "channel": "local_handler", "invocable": False, "support_level": "unsupported",
        "reason": f"{adapter_id} 当前会话没有公布压缩命令或接口，平台无法触发上下文压缩",
        "alternative": "用 /new 开始新聊天，或切换到支持压缩的接入；不会把 /compact 当作普通消息发送给模型",
    }


def operation_item(adapter_id: str, engine: str, name: str, method: str, description: str,
                   argument_hint: str = "") -> RuntimeCapabilityItem:
    return RuntimeCapabilityItem(
        id=f"runtime:{adapter_id}:operation:{name}", kind="operation", name=name,
        description=description, source=adapter_id, scope="session", engine=engine,
        channel="provider_native", resolution="client", origin="verified_static",
        delivery="guaranteed", verification="verified", action="invoke-runtime-operation",
        invocation={"method": method}, argument_hint=argument_hint,
    )


def detached_compaction_snapshot(
    snapshot: RuntimeCapabilitySnapshot, compact: RuntimeCapabilityItem,
) -> RuntimeCapabilitySnapshot:
    """Keep an explicit native compact contract usable before session resume.

    Dynamic session commands remain stale. The real send must resume the
    runtime and resolve this command again against its live capabilities.
    """
    return snapshot.model_copy(update={
        "stale": True,
        "items": [*snapshot.items, compact.model_copy(update={
            "origin": "verified_static", "scope": "engine",
        })],
    })


def native_prompt(capability: dict[str, Any], arguments: str) -> str:
    """Preserve the command at byte zero; prose before / breaks native parsers."""
    invocation = capability.get("invocation") or {}
    wire = str(invocation.get("native_wire_text") or invocation.get("wire_text") or f"/{capability['name']}")
    return wire + (" " + arguments if arguments else "")


def codex_review_target(arguments: str) -> dict[str, str]:
    if not arguments:
        return {"type": "uncommittedChanges"}
    for flag, kind, key in (("--base", "baseBranch", "branch"), ("--commit", "commit", "sha")):
        if arguments == flag or arguments.startswith(flag + " "):
            value = arguments[len(flag):].strip()
            if not value:
                raise ValueError(f"{flag} 需要参数")
            return {"type": kind, key: value}
    return {"type": "custom", "instructions": arguments}


def operation_result_text(name: str, result: Any) -> str:
    """Readable reply for an operation turn, persisted as the turn's message.

    Engine data (``/mcp``, ``/status``...) is appended whole as a JSON block so
    the reply stays complete after reload; compaction details have their own
    timeline entry and are not repeated.
    """
    message = str(result.get("message") or "").strip() if isinstance(result, dict) else ""
    text = message or f"/{name} 已完成"
    data = result.get("result") if isinstance(result, dict) else result
    if data in (None, "", [], {}) or name == "compact":
        return text
    import json
    return f"{text}\n\n```json\n{json.dumps(data, ensure_ascii=False, indent=2, default=str)}\n```"


def public_operation_result(value: Any) -> Any:
    """Do not persist native MCP credentials in command receipts or history."""
    if isinstance(value, dict):
        private = {"env", "headers", "args", "apikey", "accesstoken", "refreshtoken",
                   "authorization", "bearertoken", "password", "secret", "credentials", "token"}
        return {key: "[redacted]" if str(key).replace("_", "").replace("-", "").lower() in private
                else public_operation_result(item) for key, item in value.items()}
    if isinstance(value, list):
        return [public_operation_result(item) for item in value]
    if isinstance(value, str) and value.startswith(("https://", "http://")):
        from urllib.parse import urlsplit, urlunsplit
        url = urlsplit(value)
        if url.username or url.password or url.query:
            host = url.netloc.rsplit("@", 1)[-1]
            return urlunsplit((url.scheme, host, url.path, "", ""))
    return value
