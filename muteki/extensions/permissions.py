"""扩展权限执行点（任务书 11.2、12.1，EXT-01）。

执行模型（核验文档总览原则：第三方默认子进程，进程内仅限可信内置代码）：

- 子进程级强隔离（网络命名空间、seccomp 等）属于容器 / Worker execution 层；
  本层的真实执行点是「不发放」：宿主只把已授权的路径、域名与 secret 值
  经 initialize 参数和受控环境变量交给扩展，未声明的一律不给。
- 扩展回到宿主的每个入口都重新校验：event/propose 的事件命名空间必须落在
  manifest 声明的 ``events_write`` 内（且必须属于 ``ext.<id>.*``），
  command/handle 的命令类型必须带 ``ext.<id>.`` 前缀，提案 payload 经
  扩展声明的 schema 子集校验（``manifest.validate_against_schema``）。
- secret 只按 ``permissions.secrets`` 声明解引用，值只在启动时注入扩展
  进程环境；宿主归档扩展日志时保留完整内容。

扩展不能直接写核心投影：只能经 host → MutekiCommandAPI 提交命令或事件提案
（见 ``muteki/extensions/handlers.py`` 的 extension.propose_event）。
"""

from __future__ import annotations

import os
import re
from pathlib import Path
from typing import Any, Optional, Protocol

from muteki.platform.contracts.extensions import ExtensionManifest
from muteki.platform.contracts.events import NS_EXT_PREFIX


class PermissionDenied(PermissionError):
    """扩展触发了 manifest 未声明的权限；code 供统一错误 envelope 使用。"""

    def __init__(self, code: str, message: str) -> None:
        super().__init__(f"{code}: {message}")
        self.code = code
        self.message = message


class SecretResolver(Protocol):
    """secret:// 引用解引用器（宿主侧 SecretStore 的最小接口）。

    任务书 12.1：持久对象只保存 ``secret://scope/id``；Adapter / 扩展进程
    启动时由宿主按权限解引用。Secret 不进入事件、回执、Prompt 或错误堆栈。
    """

    def resolve(self, ref: str) -> Optional[str]:
        """解析 ``secret://scope/id``（或 ``scope/id``）；不存在返回 None。"""
        ...


class EnvironmentSecretResolver:
    """以环境变量为后端的 resolver：``secret://env/NAME`` → ``os.environ[NAME]``。

    这是本地开发与测试用的最小实现；产品 SecretStore 落地后替换注入即可，
    权限判定逻辑不变。
    """

    def resolve(self, ref: str) -> Optional[str]:
        scope, _, ident = normalize_secret_ref(ref).partition("/")
        if scope != "env" or not ident:
            return None
        return os.environ.get(ident)


class ProductExtensionSecretResolver:
    """产品扩展 resolver：读取协调器私有平台 SecretStore，可选开发环境引用。"""

    def __init__(self, platform_store: Any, *, allow_environment: bool = False) -> None:
        self._platform_store = platform_store
        self._allow_environment = bool(allow_environment)

    def resolve(self, ref: str) -> Optional[str]:
        text = str(ref or "").strip()
        if text.startswith("secret://platform/"):
            try:
                return self._platform_store.resolve(text)
            except Exception:
                return None
        if self._allow_environment:
            return EnvironmentSecretResolver().resolve(text)
        return None


def normalize_secret_ref(ref: str) -> str:
    """``secret://scope/id`` 与 ``scope/id`` 归一化为 ``scope/id``。"""
    text = str(ref or "").strip()
    if text.startswith("secret://"):
        text = text[len("secret://"):]
    return text


def secret_env_name(ref: str) -> str:
    """secret 引用注入扩展进程时使用的环境变量名（确定性、可审计）。"""
    slug = re.sub(r"[^A-Za-z0-9]+", "_", normalize_secret_ref(ref)).strip("_")
    return f"MUTEKI_SECRET_{slug.upper()}"


class PermissionChecker:
    """单个扩展（单个版本）声明权限的执行点。

    构造后宿主把 ``initialize_params()`` 的授权视图交给扩展进程，
    并用 ``check_*`` 系列校验扩展回到宿主的每个请求。
    """

    def __init__(
        self,
        manifest: ExtensionManifest,
        *,
        workspace_root: str | Path | None = None,
        state_dir: str | Path | None = None,
        secret_resolver: Optional[SecretResolver] = None,
    ) -> None:
        self.manifest = manifest
        self._workspace_root = (
            Path(workspace_root).resolve() if workspace_root else None
        )
        self._state_dir = Path(state_dir).resolve() if state_dir else None
        self._resolver = secret_resolver
        self._grants = manifest.permissions

    # -- 文件系统 -------------------------------------------------------------

    def check_filesystem(self, path: str | Path, *, write: bool = False) -> Path:
        """校验扩展声明内允许访问的路径，返回解析后的绝对路径。

        - ``workspace-read`` / ``workspace-write`` 覆盖 workspace 根目录内；
        - ``state-read`` / ``state-write`` 覆盖扩展私有状态目录内；
        其余路径一律拒绝。
        """
        target = Path(path).resolve()
        for base, read_token, write_token in (
            (self._workspace_root, "workspace-read", "workspace-write"),
            (self._state_dir, "state-read", "state-write"),
        ):
            if base is None:
                continue
            if target != base and base not in target.parents:
                continue
            token = write_token if write else read_token
            if token not in self._grants.filesystem:
                raise PermissionDenied(
                    "extension.filesystem_denied",
                    f"{token} not declared in manifest permissions",
                )
            return target
        raise PermissionDenied(
            "extension.filesystem_denied",
            f"path is outside every granted root: {path}",
        )

    # -- 网络 -----------------------------------------------------------------

    def check_network(self, host: str) -> None:
        """域名白名单判定：精确匹配或 manifest 声明的 ``*.suffix`` 通配。"""
        text = str(host or "").strip().lower()
        if not text:
            raise PermissionDenied(
                "extension.network_denied", "empty host is not allowed"
            )
        for entry in self._grants.network:
            pattern = entry.strip().lower()
            if pattern.startswith("*."):
                suffix = pattern[1:]  # ".example.com"
                if text.endswith(suffix) and text != pattern[2:]:
                    return
            elif text == pattern:
                return
        raise PermissionDenied(
            "extension.network_denied",
            f"host {host!r} is not in the manifest network whitelist",
        )

    # -- Secret ---------------------------------------------------------------

    def resolve_secrets(self) -> dict[str, str]:
        """按声明解引用 secret，返回 ``{注入环境变量名: 值}``。

        未声明的引用不会被解引用；声明了但 resolver 无法解析时报错
        （宁可启动失败，不静默降级为无凭据运行）。
        """
        resolved: dict[str, str] = {}
        for ref in self._grants.secrets:
            value = self._resolver.resolve(ref) if self._resolver else None
            if value is None:
                raise PermissionDenied(
                    "extension.secret_unresolvable",
                    f"declared secret ref cannot be resolved: {ref!r}",
                )
            resolved[secret_env_name(ref)] = value
        return resolved

    # -- 事件命名空间 / 命令前缀 ------------------------------------------------

    def check_event_type(self, event_type: str) -> None:
        """扩展只能写 ``ext.<id>.*`` 内、且在 events_write 声明内的事件。"""
        text = str(event_type or "").strip()
        own_prefix = f"{NS_EXT_PREFIX}{self.manifest.id}."
        if not text.startswith(own_prefix) or text == own_prefix:
            raise PermissionDenied(
                "extension.event_namespace_denied",
                f"extensions may only write inside {own_prefix}*: {event_type!r}",
            )
        for pattern in self._grants.events_write:
            if _namespace_pattern_matches(pattern.strip(), text):
                return
        raise PermissionDenied(
            "extension.event_namespace_denied",
            f"event type {event_type!r} is not covered by events_write "
            f"{sorted(self._grants.events_write)}",
        )

    def check_command_type(self, command_type: str) -> None:
        """扩展业务命令必须带 ``ext.<id>.`` 前缀（与事件命名空间一致）。"""
        text = str(command_type or "").strip()
        prefix = f"{NS_EXT_PREFIX}{self.manifest.id}."
        if not text.startswith(prefix) or not text[len(prefix):]:
            raise PermissionDenied(
                "extension.command_namespace_denied",
                f"extension command types must look like {prefix}<verb>: "
                f"{command_type!r}",
            )

    # -- 授权视图 ---------------------------------------------------------------

    def initialize_params(self) -> dict[str, Any]:
        """交给扩展进程的授权视图：只含已授予的根路径与声明清单本身。

        未授予 workspace-read 时 workspace_root 不下发；secret 值不在这里
        （经环境变量注入，见 ``resolve_secrets``）。
        """
        return {
            "workspace_root": (
                str(self._workspace_root)
                if self._workspace_root is not None
                and "workspace-read" in self._grants.filesystem
                else None
            ),
            "state_dir": str(self._state_dir) if self._state_dir else None,
            "permissions": {
                "filesystem": list(self._grants.filesystem),
                "network": list(self._grants.network),
                "secrets": list(self._grants.secrets),
                "events_write": list(self._grants.events_write),
            },
            "secret_injection": {
                "mode": "minimal_environment",
                "scopes": list(self._grants.secrets),
                "lifetime": "extension_process",
            },
        }

def _namespace_pattern_matches(pattern: str, event_type: str) -> bool:
    """events_write 模式匹配：``ext.<id>.*`` 或 ``ext.<id>.foo.*`` 后缀通配。"""
    if pattern.endswith(".*"):
        prefix = pattern[:-1]  # 保留结尾点
        return event_type.startswith(prefix)
    return event_type == pattern


__all__ = [
    "EnvironmentSecretResolver",
    "PermissionChecker",
    "PermissionDenied",
    "SecretResolver",
    "ProductExtensionSecretResolver",
    "normalize_secret_ref",
    "secret_env_name",
]
