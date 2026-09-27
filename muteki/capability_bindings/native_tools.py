"""Agent SDK Native Tool Bridge（CAP-02，设计 17.5）。

按核验结论（docs/research/third_party_verification.md §CLAUDE-10）：
Claude Agent SDK **没有**独立于 MCP 的原生工具注册 API，自定义工具的官方
机制是进程内 SDK MCP server（``tool()`` 装饰器 + ``create_sdk_mcp_server``，
生成 ``McpSdkServerConfig(type="sdk")`` 放进 ``mcp_servers``）。因此本
Bridge 是 MCP 绑定的进程内形态，复用同一 Gateway 管线，不存在独立的
命令目录或权限逻辑：

- ``build_native_tools`` 从 Gateway descriptor 动态生成工具清单
  （name / description / input_schema + 调用 handler）；
- ``build_sdk_mcp_server`` 在安装了 ``claude_agent_sdk`` 时把同一组工具
  包成真实 Runtime 可直接消费的进程内 MCP server 配置；未安装时抛出带
  修复提示的 RuntimeError（Binding 包不强制依赖 SDK）。

handler 返回 SDK 工具结果形态 ``{"content": [{"type": "text", ...}]}``；
业务结果与错误 envelope 与其他入口逐字节一致。无业务状态。
"""

from __future__ import annotations

import json
from dataclasses import dataclass
from typing import Any, Awaitable, Callable, Optional

from muteki.platform.contracts.capabilities import (
    BindingContext,
    CapabilityDescriptor,
    CapabilityInjectionPlan,
    CapabilityInvocation,
    InjectionKind,
)
from muteki.platform.contracts.protocols import AgentCapabilityGateway

from .http_jsonrpc import capability_result_payload

#: 进程内 server 在 mcp_servers 字典中的默认键名。
DEFAULT_SERVER_NAME = "muteki-control"

#: 工具 handler 的签名：arguments → SDK 工具结果 dict。
NativeToolHandler = Callable[[dict[str, Any]], Awaitable[dict[str, Any]]]


@dataclass
class NativeToolSpec:
    """一个进程内工具的描述与调用桥（从 Gateway descriptor 生成）。"""

    name: str
    description: str
    input_schema: dict[str, Any]
    handler: NativeToolHandler


def build_native_tools(
    descriptor: CapabilityDescriptor,
    context: BindingContext,
    gateway: AgentCapabilityGateway,
) -> list[NativeToolSpec]:
    """按 descriptor 的工具清单生成 Native Tool 桥（不维护第二份目录）。

    ``context`` 在 Session 启动注入时固定；Grant 之后被撤销 / 过期时，
    Gateway 仍会在每次调用统一拒绝（Binding 不缓存授权状态）。
    """
    specs: list[NativeToolSpec] = []
    for tool in descriptor.tools:
        specs.append(NativeToolSpec(
            name=tool.name,
            description=tool.description,
            input_schema=dict(tool.input_schema or {"type": "object"}),
            handler=_make_handler(gateway, context, tool.name),
        ))
    return specs


def _make_handler(
    gateway: AgentCapabilityGateway, context: BindingContext, tool_name: str
) -> NativeToolHandler:
    async def handler(arguments: dict[str, Any]) -> dict[str, Any]:
        result = await gateway.invoke(context, CapabilityInvocation(
            tool_name=tool_name, arguments=dict(arguments or {})))
        payload = capability_result_payload(result)
        return {
            "content": [{
                "type": "text",
                "text": json.dumps(payload, ensure_ascii=False),
            }],
            "isError": not result.ok,
        }

    return handler


def build_sdk_mcp_server(
    descriptor: CapabilityDescriptor,
    context: BindingContext,
    gateway: AgentCapabilityGateway,
    *,
    name: str = DEFAULT_SERVER_NAME,
    version: str = "1.0.0",
) -> Any:
    """构造 Claude Agent SDK 的进程内 MCP server（McpSdkServerConfig）。

    返回对象可直接放入 ``ClaudeAgentOptions.mcp_servers`` 字典
    （``{name: config}``）。未安装 ``claude_agent_sdk`` 时抛 RuntimeError。
    """
    try:
        from claude_agent_sdk import create_sdk_mcp_server, tool
    except ImportError as exc:
        raise RuntimeError(
            "claude_agent_sdk 未安装：进程内 Native Tool 需要 "
            "claude-agent-sdk（pip install claude-agent-sdk）；"
            "或改用 MCP / HTTP-JSONRPC Binding"
        ) from exc
    sdk_tools = [
        tool(spec.name, spec.description, spec.input_schema)(spec.handler)
        for spec in build_native_tools(descriptor, context, gateway)
    ]
    return create_sdk_mcp_server(name=name, version=version, tools=sdk_tools)


def build_injection_plan(
    descriptor: CapabilityDescriptor,
    *,
    binding_id: Optional[str] = None,
    grant_id: Optional[str] = None,
    credential_ref: Optional[str] = None,
    audience: str = "",
    server_name: str = DEFAULT_SERVER_NAME,
) -> CapabilityInjectionPlan:
    """Native Tool（in-process MCP）注入计划。

    进程内形态没有网络 endpoint；RUNTIME-02+ 的 Adapter 拿到计划后用
    ``build_sdk_mcp_server`` 现场构造 SDK server（工具 handler 闭包持有
    BindingContext，进程内不需要 bearer token）。
    """
    return CapabilityInjectionPlan(
        injection_kind=InjectionKind.NATIVE_TOOL,
        gateway_endpoint="",
        tool_descriptions=list(descriptor.tools),
        credential_ref=credential_ref,
        audience=audience,
        binding_id=binding_id,
        grant_id=grant_id,
        runtime_config={
            "in_process_mcp": True,
            "server_name": server_name,
            # §CLAUDE-10：Claude 的 Native Tool 即 in-process MCP server，
            # 不存在独立原生工具注册 API。
            "sdk_mechanism": "claude_agent_sdk.create_sdk_mcp_server",
        },
    )


__all__ = [
    "DEFAULT_SERVER_NAME",
    "NativeToolHandler",
    "NativeToolSpec",
    "build_injection_plan",
    "build_native_tools",
    "build_sdk_mcp_server",
]
