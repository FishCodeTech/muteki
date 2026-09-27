"""Muteki Extension Host（EXT-01）。

安装包以 Agent Plugins 1.0.0 根 ``plugin.json`` 为唯一清单，Muteki 专用
运行声明位于 ``extensions.io.github.fishcodetech.muteki``。第三方扩展默认
运行在独立子进程，经版本化 JSON-RPC over stdio 与宿主
通信；宿主负责发现、manifest 校验、依赖与 core version 检查、启动 /
握手 / 健康检查 / 重启 / 停止、权限执行、状态目录与迁移、版本回滚和
日志按原文归档。扩展不能直接写核心投影，只能经 MutekiCommandAPI 提交
命令或事件提案。

入口：

- 生命周期编排：``ExtensionService``（registry.py）
- 安装来源：``ExtensionInstaller`` / ``Source``（installer.py）、
  ``ExtensionCatalog``（catalog.py）
- 子进程宿主：``ExtensionProcess``（host.py）
- 协议：``PROTOCOL_VERSION`` / ``JsonRpcPeer`` / ``serve_stdio``（protocol.py）
- 权限：``PermissionChecker``（permissions.py）
- 命令接入：``register_extension_handlers``（handlers.py）
- Web 路由：``create_extension_router``（api.py，供 EXT-02/INTEG-01 挂载）
"""

from muteki.extensions.catalog import ExtensionCatalog
from muteki.extensions.handlers import register_extension_handlers
from muteki.extensions.host import ExtensionProcess
from muteki.extensions.installer import ExtensionInstaller, Source
from muteki.extensions.manifest import (
    CORE_VERSION,
    ManifestError,
    load_manifest,
    validate_manifest,
)
from muteki.extensions.permissions import (
    EnvironmentSecretResolver,
    PermissionChecker,
    PermissionDenied,
)
from muteki.extensions.protocol import PROTOCOL_VERSION
from muteki.extensions.registry import (
    ExtensionError,
    ExtensionRecord,
    ExtensionService,
    ExtensionState,
)

__all__ = [
    "CORE_VERSION",
    "PROTOCOL_VERSION",
    "EnvironmentSecretResolver",
    "ExtensionCatalog",
    "ExtensionError",
    "ExtensionInstaller",
    "ExtensionProcess",
    "ExtensionRecord",
    "ExtensionService",
    "ExtensionState",
    "ManifestError",
    "PermissionChecker",
    "PermissionDenied",
    "Source",
    "load_manifest",
    "register_extension_handlers",
    "validate_manifest",
]
