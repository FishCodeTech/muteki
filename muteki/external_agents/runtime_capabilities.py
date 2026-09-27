"""会话级 Runtime 能力目录。

这里描述的是外部 Agent Runtime 当前真正暴露给客户端的能力，不是 Muteki
自身的工具 schema。能力项必须带调用通道、来源和验证状态，调用方不能再按
命令名称猜测应该由前端、本地服务还是外部 Runtime 处理。
"""

from __future__ import annotations

from typing import TYPE_CHECKING, Any, Literal, Optional

from pydantic import Field

from muteki.platform.contracts.base import ContractModel

if TYPE_CHECKING:
    from muteki.external_agents.interaction_matrix import InteractionCapabilityMatrix


RuntimeCapabilityKind = Literal[
    "command", "operation", "skill", "reference", "mcp_status",
]
RuntimeCapabilityChannel = Literal[
    "local_handler", "app_server_rpc", "acp_prompt", "provider_native",
    "session_config",
]
RuntimeCapabilityOrigin = Literal[
    "builtin", "dynamic", "verified_static", "static_fallback",
]
RuntimeCapabilityDelivery = Literal[
    "local", "guaranteed", "best_effort",
]
RuntimeCapabilityVerification = Literal["verified", "unverified"]


class RuntimeCapabilityItem(ContractModel):
    """一个可展示或可调用的 Runtime 能力。"""

    id: str
    kind: RuntimeCapabilityKind
    name: str
    description: str = ""
    source: str = ""
    scope: str = "session"
    engine: str = ""
    channel: RuntimeCapabilityChannel
    resolution: Literal["runtime", "client"] = "runtime"
    origin: RuntimeCapabilityOrigin
    delivery: RuntimeCapabilityDelivery
    verification: RuntimeCapabilityVerification
    action: str = ""
    argument_hint: str = ""
    invocation: dict[str, Any] = Field(default_factory=dict)
    status: str = "available"
    # C24: non-verified / limited catalog rows stay visible but not invocable.
    support_level: Literal[
        "supported", "limited", "unsupported", "unknown", "expired",
    ] = "supported"
    reason: str = ""
    alternative: str = ""
    invocable: bool = True


class RuntimeCapabilitySnapshot(ContractModel):
    """一个 AgentSession 当前的能力快照。"""

    adapter_id: str
    instance_id: str = "default"
    agent_session_id: str = ""
    external_session_id: Optional[str] = None
    revision: int = 0
    stale: bool = False
    items: list[RuntimeCapabilityItem] = Field(default_factory=list)
    diagnostics: list[str] = Field(default_factory=list)
    matrix: Optional[Any] = None

    def public_items(self) -> list[dict[str, Any]]:
        return [item.model_dump(mode="json") for item in self.items]

    def public_matrix(self) -> Optional[dict[str, Any]]:
        if self.matrix is None:
            return None
        if hasattr(self.matrix, "model_dump"):
            return self.matrix.model_dump(mode="json")
        if isinstance(self.matrix, dict):
            return dict(self.matrix)
        return None

    def command(self, name: str) -> Optional[RuntimeCapabilityItem]:
        normalized = str(name or "").strip().lstrip("/").casefold()
        return next(
            (
                item for item in self.items
                if item.kind in {"command", "skill"}
                and item.name.casefold() == normalized
            ),
            None,
        )


def dynamic_command_item(
    *,
    adapter_id: str,
    engine: str,
    name: str,
    description: str = "",
    argument_hint: str = "",
    channel: RuntimeCapabilityChannel,
    kind: Literal["command", "skill"] = "command",
    invocation: Optional[dict[str, Any]] = None,
) -> RuntimeCapabilityItem:
    """构造一个由 Runtime 实时公布的命令或 Skill。"""
    normalized = str(name or "").strip().lstrip("/")
    return RuntimeCapabilityItem(
        id=f"runtime:{adapter_id}:{kind}:{normalized}",
        kind=kind,
        name=normalized,
        description=str(description or "").strip(),
        source=adapter_id,
        scope="engine",
        engine=engine,
        channel=channel,
        resolution="runtime",
        origin="dynamic",
        delivery="guaranteed",
        verification="verified",
        action="invoke-runtime-command",
        argument_hint=str(argument_hint or "").strip(),
        invocation=dict(invocation or {"command": normalized}),
        support_level="supported",
        invocable=True,
    )


__all__ = [
    "RuntimeCapabilityItem",
    "RuntimeCapabilitySnapshot",
    "dynamic_command_item",
]
