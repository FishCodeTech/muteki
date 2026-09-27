"""能力协议 Binding 包（CAP-02，设计 17.5，任务书 6.6 / 10.11）。

同一 AgentCapabilityGateway 的五种协议入口：

- ``mcp_server.MutekiControlMcpServer``：MCP 2026-07-28 Streamable HTTP
  （默认入口）；
- ``native_tools``：Agent SDK 进程内 MCP server 形态的工具桥（Claude）；
- ``acp_config``：ACP ``session/new|load|resume`` 的 mcpServers 配置生成；
- ``agent_plugin``：Agent Plugins 1.0.0 标准包与客户端安装映射；
- ``http_jsonrpc``：结构化 HTTP/JSON-RPC Bridge；

所有入口的工具清单由 Gateway describe 动态生成，结果与错误 envelope 完全
一致；任何入口都不保存业务状态、平台 Token、事件缓存、限速队列或 Agent
Session 业务状态，也不 import Manager / Store。
"""

from .acp_config import (
    build_http_mcp_server_entry,
    build_session_load_params,
    build_session_new_params,
    build_session_resume_params,
)
from .agent_plugin import (
    AgentPluginComponent,
    AgentPluginDiagnostic,
    AgentPluginLoadError,
    AgentPluginPackage,
    AgentPluginRuntimeMaterialization,
    AgentPluginSkill,
    builtin_agent_plugin,
    discover_agent_plugin,
    install_plugin,
    install_skill,
    load_agent_plugin,
    load_manifest,
    materialize_runtime,
    package_root,
    require_verified_agent_plugin,
    resolve_client_extension_entry,
    write_connection_config,
)
from .auth import (
    BearerTokenError,
    issue_bearer_token,
    parse_bearer_token,
)
from .http_jsonrpc import MutekiHttpJsonRpcBridge
from .mcp_server import PROTOCOL_VERSION, MutekiControlMcpServer, asgi_app
from .native_tools import (
    NativeToolSpec,
    build_native_tools,
    build_sdk_mcp_server,
)

__all__ = [
    "AgentPluginComponent",
    "AgentPluginDiagnostic",
    "AgentPluginLoadError",
    "AgentPluginPackage",
    "AgentPluginRuntimeMaterialization",
    "AgentPluginSkill",
    "BearerTokenError",
    "MutekiControlMcpServer",
    "MutekiHttpJsonRpcBridge",
    "NativeToolSpec",
    "PROTOCOL_VERSION",
    "asgi_app",
    "build_http_mcp_server_entry",
    "build_native_tools",
    "build_sdk_mcp_server",
    "build_session_load_params",
    "build_session_new_params",
    "build_session_resume_params",
    "builtin_agent_plugin",
    "discover_agent_plugin",
    "install_plugin",
    "install_skill",
    "issue_bearer_token",
    "parse_bearer_token",
    "load_manifest",
    "load_agent_plugin",
    "materialize_runtime",
    "package_root",
    "write_connection_config",
    "require_verified_agent_plugin",
    "resolve_client_extension_entry",
]
