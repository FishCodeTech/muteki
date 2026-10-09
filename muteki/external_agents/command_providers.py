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
from .runtime_capabilities import RuntimeCapabilityItem


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


def operation_item(adapter_id: str, engine: str, name: str, method: str, description: str,
                   argument_hint: str = "") -> RuntimeCapabilityItem:
    return RuntimeCapabilityItem(
        id=f"runtime:{adapter_id}:operation:{name}", kind="operation", name=name,
        description=description, source=adapter_id, scope="session", engine=engine,
        channel="provider_native", resolution="client", origin="verified_static",
        delivery="guaranteed", verification="verified", action="invoke-runtime-operation",
        invocation={"method": method}, argument_hint=argument_hint,
    )


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
