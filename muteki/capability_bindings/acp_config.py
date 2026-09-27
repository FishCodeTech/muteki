"""ACP MCP Config builder（CAP-02，设计 17.5/17.6）。

按核验结论（docs/research/third_party_verification.md §ACP，ACP 稳定协议
版本 = 1）生成 ``session/new`` / ``session/load`` / ``session/resume`` 的
``mcpServers`` 注入配置：

- HTTP transport 元素形态（已确认）：
  ``{"type": "http", "name", "url", "headers": [{"name", "value"}]}``；
- ``session/new`` params 为 ``{cwd, mcpServers}``；``session/load`` 与
  ``session/resume`` 额外带 ``sessionId``；
- 使用前必须经 ``initialize`` 确认 ``agentCapabilities.mcpCapabilities.http``
  为 true（能力门控是硬要求，本模块只生成配置、不做探测）；
- SSE transport 已被 MCP 弃用，本 builder 不生成 SSE 形态。

bearer token 由调用方（Adapter / 平台接线）用 ``auth.issue_bearer_token``
现场签发；未提供时 Authorization 头使用占位符，Adapter 必须在写入真实
Session 参数前替换。本模块无业务状态。
"""

from __future__ import annotations

from typing import Any, Optional, Sequence

from muteki.platform.contracts.capabilities import (
    CapabilityDescriptor,
    CapabilityInjectionPlan,
    InjectionKind,
)

#: 未提供 bearer token 时的占位符；Adapter 必须在 session 建立前替换。
TOKEN_PLACEHOLDER = "${MUTEKI_CAPABILITY_BEARER_TOKEN}"

DEFAULT_SERVER_NAME = "muteki-control"


def build_http_mcp_server_entry(
    url: str,
    *,
    name: str = DEFAULT_SERVER_NAME,
    bearer_token: Optional[str] = None,
) -> dict[str, Any]:
    """ACP v1 HTTP transport 的 mcpServers 元素（已核验字段形态）。"""
    token = bearer_token if bearer_token is not None else TOKEN_PLACEHOLDER
    return {
        "type": "http",
        "name": name,
        "url": url,
        "headers": [{"name": "Authorization", "value": f"Bearer {token}"}],
    }


def build_session_new_params(
    cwd: str, mcp_servers: Sequence[dict[str, Any]]
) -> dict[str, Any]:
    """``session/new`` 的 params（cwd 必填且必须是绝对路径，由调用方保证）。"""
    return {"cwd": cwd, "mcpServers": list(mcp_servers)}


def build_session_load_params(
    session_id: str, cwd: str, mcp_servers: Sequence[dict[str, Any]]
) -> dict[str, Any]:
    """``session/load`` 的 params（Agent 会以 session/update 重放历史）。

    仅在 ``agentCapabilities.loadSession`` 为 true 时可发送。
    """
    return {"sessionId": session_id, "cwd": cwd, "mcpServers": list(mcp_servers)}


def build_session_resume_params(
    session_id: str, cwd: str, mcp_servers: Sequence[dict[str, Any]]
) -> dict[str, Any]:
    """``session/resume`` 的 params（不重放历史；2026-04-22 已稳定）。

    仅在 ``agentCapabilities.sessionCapabilities.resume`` 存在时可发送；
    按核验建议优先 resume、load 仅作降级。
    """
    return {"sessionId": session_id, "cwd": cwd, "mcpServers": list(mcp_servers)}


def build_injection_plan(
    descriptor: CapabilityDescriptor,
    *,
    gateway_endpoint: str,
    bearer_token: Optional[str] = None,
    binding_id: Optional[str] = None,
    grant_id: Optional[str] = None,
    credential_ref: Optional[str] = None,
    audience: str = "",
    server_name: str = DEFAULT_SERVER_NAME,
) -> CapabilityInjectionPlan:
    """ACP 注入计划：runtime_config 携带可直接展开的 mcpServers 配置。

    RUNTIME-02+ 的 Adapter 在 ``session/new`` / ``session/load`` /
    ``session/resume`` 时把 ``runtime_config["mcpServers"]`` 填入 params；
    bearer_token 为空时须先以真实 token 替换占位符。
    """
    entry = build_http_mcp_server_entry(
        gateway_endpoint, name=server_name, bearer_token=bearer_token)
    return CapabilityInjectionPlan(
        injection_kind=InjectionKind.ACP_MCP_CONFIG,
        gateway_endpoint=gateway_endpoint,
        tool_descriptions=list(descriptor.tools),
        credential_ref=credential_ref,
        audience=audience,
        binding_id=binding_id,
        grant_id=grant_id,
        runtime_config={
            "mcpServers": [entry],
            "session_methods": {
                "new": "session/new",
                "load": "session/load",
                "resume": "session/resume",
            },
            # 能力门控提示：HTTP transport 需 mcpCapabilities.http=true。
            "requires_capability": "mcpCapabilities.http",
        },
    )


__all__ = [
    "DEFAULT_SERVER_NAME",
    "TOKEN_PLACEHOLDER",
    "build_http_mcp_server_entry",
    "build_injection_plan",
    "build_session_load_params",
    "build_session_new_params",
    "build_session_resume_params",
]
