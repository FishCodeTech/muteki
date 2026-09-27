"""Muteki Agent Plugin 客户端扩展契约（任务书 6.9）。

Extension Host 只承载产品控制层扩展、Adapter、PlatformAdapter 和数据连接；
包级清单遵循 Agent Plugins 1.0.0，Muteki 专用字段位于
``plugin.json.extensions.io.github.fishcodetech.muteki``。Agent 内部插件继续
由外部 Runtime 管理。
"""

from __future__ import annotations

from typing import Optional

from pydantic import Field

from .base import ContractModel


class ExtensionEntrypoint(ContractModel):
    """扩展宿主入口，例如 subprocess + serve 命令。"""

    # subprocess | in_process
    host: str = "subprocess"
    command: list[str] = Field(default_factory=list)


class ExtensionProvide(ContractModel):
    """扩展提供的能力，例如 external-agent-adapter / platform-adapter。"""

    type: str = ""
    id: str = ""
    api_version: int = 1


class ExtensionRequire(ContractModel):
    """扩展依赖的平台能力。"""

    capability: str = ""
    version: int = 1


class ExtensionPermissions(ContractModel):
    """扩展声明式权限：文件系统、网络、secret 引用与可写事件命名空间。"""

    filesystem: list[str] = Field(default_factory=list)
    network: list[str] = Field(default_factory=list)
    secrets: list[str] = Field(default_factory=list)
    # 例如 ["ext.org.example.*"]
    events_write: list[str] = Field(default_factory=list)


class ExtensionManifest(ContractModel):
    """从 Agent Plugins 根清单解析出的 Muteki 客户端扩展视图。"""

    plugin_schema: str = ""
    client_namespace: str = ""
    manifest_version: int = 1
    id: str = ""
    plugin_version: str = ""
    version: str = ""
    description: str = ""
    author: dict[str, str] = Field(default_factory=dict)
    homepage: str = ""
    repository: str = ""
    license: str = ""
    keywords: list[str] = Field(default_factory=list)
    requires_core: str = ""
    # builtin | installed
    origin: str = "installed"
    # False 表示升级成功或失败后只保留目标版本，并禁止自动回滚。
    retain_previous_versions: bool = True
    entrypoints: ExtensionEntrypoint = Field(default_factory=ExtensionEntrypoint)
    provides: list[ExtensionProvide] = Field(default_factory=list)
    requires: list[ExtensionRequire] = Field(default_factory=list)
    permissions: ExtensionPermissions = Field(default_factory=ExtensionPermissions)
    config_schema: Optional[str] = None
    state_schema: Optional[str] = None
    ui: Optional[str] = None
