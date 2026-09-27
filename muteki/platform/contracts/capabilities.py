"""能力绑定契约（任务书 5.3、6.5、6.6）。

ExecutionBinding 描述 Run 的执行选择；CapabilityBinding 描述 Thread 授予
外部主对话 Agent 的工具、命令、查询和资源范围。两者分表保存、语义互不复用。
"""

from __future__ import annotations

from datetime import datetime
from enum import Enum
from typing import Any, Optional

from pydantic import Field, field_validator, model_validator

from .base import ContractModel, new_id, utcnow
from .errors import ErrorEnvelope
from .receipts import CommandReceipt


class ThreadMode(str, Enum):
    """Thread 对话模式，决定 CapabilityBinding 的默认能力集合。"""

    CONVERSATION = "conversation"
    SINGLE_TASK = "single_task"
    COMPETITION = "competition"
    MANAGEMENT = "management"


class InjectionKind(str, Enum):
    """``prepare_capability_injection`` 可选的能力注入方式（MCP 优先）。"""

    MCP = "mcp"
    NATIVE_TOOL = "native_tool"
    ACP_MCP_CONFIG = "acp_mcp_config"
    AGENT_PLUGIN = "agent_plugin"
    # 只用于读取旧 CapabilityGrant；新会话不再产生两种私有包装。
    RUNTIME_PLUGIN = "runtime_plugin"
    HTTP_JSONRPC = "http_jsonrpc"
    CLI_SKILL = "cli_skill"


class ExecutionBinding(ContractModel):
    """Run 使用的执行器、Adapter instance、Profile、Policy 和领域能力。"""

    execution_binding_id: str = Field(default_factory=lambda: new_id("ebind"))
    run_id: Optional[str] = None
    task_id: Optional[str] = None
    executor_id: str = ""
    adapter_id: Optional[str] = None
    runtime_instance_id: Optional[str] = None
    profile_id: Optional[str] = None
    policy_version: int = 1
    domain_capabilities: list[str] = Field(default_factory=list)
    created_at: datetime = Field(default_factory=utcnow)
    revoked_at: Optional[datetime] = None


class CapabilityBinding(ContractModel):
    """Thread 级能力授权。mode 与 tool_set 在一个 version 内不可变。"""

    binding_id: str = Field(default_factory=lambda: new_id("cbind"))
    binding_version: int = 1
    thread_id: str = ""
    principal_id: str = ""
    mode: ThreadMode = ThreadMode.CONVERSATION
    tool_set: list[str] = Field(default_factory=list)
    allowed_commands: list[str] = Field(default_factory=list)
    allowed_queries: list[str] = Field(default_factory=list)
    resource_scopes: list[str] = Field(default_factory=list)
    policy_version: int = 1
    created_at: datetime = Field(default_factory=utcnow)
    revoked_at: Optional[datetime] = None


class CapabilityGrant(ContractModel):
    """CapabilityBinding 面向具体 AgentSession 的短期授权和注入实例。"""

    grant_id: str = Field(default_factory=lambda: new_id("cgrant"))
    binding_id: str = ""
    agent_session_id: str = ""
    runtime_instance_id: Optional[str] = None
    injection_kind: InjectionKind = InjectionKind.MCP
    audience: str = ""
    # 短期、可撤销、绑定 audience 的凭据引用，不是凭据本体
    credential_ref: Optional[str] = None
    issued_at: datetime = Field(default_factory=utcnow)
    expires_at: Optional[datetime] = None
    revoked_at: Optional[datetime] = None
    last_touched_at: Optional[datetime] = None


class ToolDescription(ContractModel):
    """注入给 Runtime 的工具描述（名称、说明与 JSON Schema 入参）。"""

    name: str
    description: str = ""
    input_schema: dict[str, Any] = Field(default_factory=dict)


#: 注入计划中禁止出现的配置键（小写、精确匹配）。这些键一旦出现，
#: 说明计划携带了平台 Token、SharedGraph 数据库路径或其他 secret 材料。
_FORBIDDEN_PLAN_KEYS = frozenset({
    "token", "access_token", "refresh_token", "api_token",
    "api_key", "apikey", "secret", "secret_key", "client_secret",
    "password", "passwd", "private_key", "credential", "credentials",
    "db_path", "database_path", "database_url", "dsn", "connection_string",
    "sharedgraph_db", "shared_graph_path",
})

#: 常见凭据本体的值前缀；credential_ref 只能是引用，不能是本体。
_SECRET_VALUE_PREFIXES = ("sk-", "ghp_", "gho_", "xox", "-----BEGIN")


def _scan_forbidden(value: Any, path: str) -> list[str]:
    """递归扫描配置中的禁用键，返回违规路径列表。"""
    hits: list[str] = []
    if isinstance(value, dict):
        for key, item in value.items():
            key_path = f"{path}.{key}" if path else str(key)
            if str(key).strip().lower() in _FORBIDDEN_PLAN_KEYS:
                hits.append(key_path)
            hits.extend(_scan_forbidden(item, key_path))
    elif isinstance(value, list):
        for index, item in enumerate(value):
            hits.extend(_scan_forbidden(item, f"{path}[{index}]"))
    return hits


class CapabilityInjectionPlan(ContractModel):
    """Adapter 为一次 Session 启动生成的能力注入计划。

    只包含 Gateway endpoint、工具描述、短期 credential reference、audience
    和 Runtime 所需配置；禁止包含平台 Token、SharedGraph 数据库路径或业务
    状态（构造时校验，见 ``_reject_secret_material``）。
    """

    injection_kind: InjectionKind
    # AgentCapabilityGateway 的协议入口地址（MCP/HTTP 等）
    gateway_endpoint: str = ""
    tool_descriptions: list[ToolDescription] = Field(default_factory=list)
    credential_ref: Optional[str] = None
    audience: str = ""
    # Runtime 侧所需的注入配置（如 ACP 的 mcpServers、CLI skill 路径）
    runtime_config: dict[str, Any] = Field(default_factory=dict)
    binding_id: Optional[str] = None
    grant_id: Optional[str] = None

    @field_validator("credential_ref")
    @classmethod
    def _credential_ref_is_reference(cls, value: Optional[str]) -> Optional[str]:
        if value is None:
            return value
        text = value.strip()
        if not text:
            raise ValueError("credential_ref cannot be empty")
        if any(text.startswith(prefix) for prefix in _SECRET_VALUE_PREFIXES) or " " in text:
            raise ValueError("credential_ref must be a reference, not raw credential material")
        return text

    @model_validator(mode="after")
    def _reject_secret_material(self) -> "CapabilityInjectionPlan":
        hits = _scan_forbidden(self.runtime_config, "runtime_config")
        if hits:
            raise ValueError(
                "injection plan must not carry secrets or database paths: "
                + ", ".join(sorted(hits))
            )
        return self


class BindingContext(ContractModel):
    """一次能力调用经协议入口解析后的绑定上下文。"""

    binding_id: str
    binding_version: int = 1
    grant_id: Optional[str] = None
    thread_id: str = ""
    agent_session_id: Optional[str] = None
    principal_id: str = ""
    audience: str = ""
    mode: ThreadMode = ThreadMode.CONVERSATION


class CapabilityDescriptor(ContractModel):
    """``AgentCapabilityGateway.describe`` 返回的可见能力描述。"""

    binding_id: str
    binding_version: int = 1
    mode: ThreadMode = ThreadMode.CONVERSATION
    tools: list[ToolDescription] = Field(default_factory=list)
    allowed_commands: list[str] = Field(default_factory=list)
    allowed_queries: list[str] = Field(default_factory=list)
    resource_scopes: list[str] = Field(default_factory=list)


class CapabilityInvocation(ContractModel):
    """一次能力调用请求，最终归一化为 MutekiCommandAPI 的 command/query。"""

    tool_name: str
    invocation_id: str = Field(default_factory=lambda: new_id("inv"))
    arguments: dict[str, Any] = Field(default_factory=dict)
    correlation_id: Optional[str] = None


class CapabilityResult(ContractModel):
    """一次能力调用的统一结果。

    改变状态的调用返回与 Web 相同的 CommandReceipt；只读调用把完整的
    数据放在 ``result``；失败时 ``error`` 为统一错误 envelope。
    """

    ok: bool = True
    invocation_id: Optional[str] = None
    receipt: Optional[CommandReceipt] = None
    result: Any = None
    error: Optional[ErrorEnvelope] = None
