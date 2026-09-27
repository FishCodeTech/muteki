"""比赛 PlatformAdapter 与领域服务的产品装配入口。"""

from __future__ import annotations

from pathlib import Path
from typing import Any, Optional

from muteki.competition.models import (
    ConnectionStatus,
    PlatformConnection,
    PlatformKind,
)
from muteki.competition.platforms.browser import GenericBrowserAdapter
from muteki.competition.platforms.ctfd import CTFdAdapter
from muteki.competition.platforms.gzctf import GZCTFAdapter
from muteki.competition.platforms.rctf import RCtfAdapter
from muteki.competition.secrets import PlatformSecretStore
from muteki.competition.store import CompetitionStore, NotFoundError
from muteki.extensions.integrations import ExtensionPlatformAdapter
from muteki.platform.contracts.modules import PlatformConnectionRef

_BUILTIN_KIND_META: dict[str, dict[str, str]] = {
    PlatformKind.CTFD.value: {"label": "CTFd", "icon": "flag"},
    PlatformKind.RCTF.value: {"label": "rCTF", "icon": "flag"},
    PlatformKind.GZCTF.value: {"label": "GZCTF", "icon": "flag"},
    PlatformKind.GENERIC_BROWSER.value: {"label": "浏览器会话", "icon": "globe"},
}


class PlatformAdapterFactory:
    """按持久化 PlatformConnection 构造并缓存真实 Adapter。

    builtin PlatformKind 走硬编码分支；其余 kind 若对应已启用的扩展
    ``platform-adapter`` provide，则返回 ``ExtensionPlatformAdapter`` 代理。
    """

    def __init__(
        self,
        store: CompetitionStore,
        secrets: PlatformSecretStore,
        *,
        state_root: str | Path,
        transport_policy: Optional[dict[str, dict[str, Any]]] = None,
        capability_registry: Any = None,
        extension_bridge: Any = None,
    ) -> None:
        self.store = store
        self.secrets = secrets
        self.state_root = Path(state_root)
        self.state_root.mkdir(parents=True, exist_ok=True)
        self.transport_policy = dict(transport_policy or {})
        self.capability_registry = capability_registry
        self.extension_bridge = extension_bridge
        self._cache: dict[str, tuple[str, Any]] = {}

    @staticmethod
    def connection_ref(connection: PlatformConnection) -> PlatformConnectionRef:
        return PlatformConnectionRef(
            connection_id=connection.connection_id,
            platform_kind=connection.platform_kind,
            endpoint=connection.canonical_base_url,
        )

    def builtin_kinds(self) -> list[dict[str, Any]]:
        return [
            {
                "id": kind,
                "label": meta["label"],
                "icon": meta["icon"],
                "origin": "builtin",
                "source": "builtin",
                "state": "ready",
            }
            for kind, meta in _BUILTIN_KIND_META.items()
        ]

    def list_kinds(self) -> list[dict[str, Any]]:
        """大厅平台下拉：builtin ∪ 已启用扩展 platform-adapter。"""
        kinds = self.builtin_kinds()
        if self.extension_bridge is not None:
            kinds.extend(self.extension_bridge.list_platform_adapters())
        elif self.capability_registry is not None:
            for cap in self.capability_registry.list_entries(include_disabled=False):
                if cap.type != "platform-adapter" or cap.state != "ready":
                    continue
                kinds.append({
                    "id": cap.id,
                    "label": cap.id,
                    "icon": "plug",
                    "origin": "extension",
                    "source": "extension",
                    "extension_id": cap.extension_id,
                    "state": cap.state,
                })
        return kinds

    def is_supported_kind(self, kind: str) -> bool:
        if kind in {k.value for k in PlatformKind if k is not PlatformKind.MOCK}:
            return True
        if self.extension_bridge is not None:
            return bool(self.extension_bridge.platform_adapter_supported(kind))
        if self.capability_registry is None:
            return False
        cap = self.capability_registry.get("platform-adapter", kind)
        return cap is not None and cap.state == "ready"

    def for_connection(
        self, connection: PlatformConnection | str
    ) -> Any:
        if isinstance(connection, str):
            loaded = self.store.get(PlatformConnection, connection)
            if loaded is None:
                raise NotFoundError(f"platform connection not found: {connection}")
            connection = loaded
        capability_identity = ""
        if (connection.platform_kind not in {k.value for k in PlatformKind}
                and self.capability_registry is not None):
            capability = self.capability_registry.get(
                "platform-adapter", connection.platform_kind)
            capability_identity = (
                f":{id(capability)}:{getattr(capability, 'state', '')}:"
                f"{getattr(capability, 'extension_version', '')}"
            )
        fingerprint = (
            f"{connection.platform_kind}:{connection.updated_at.isoformat()}:"
            f"{connection.credential_ref}{capability_identity}"
        )
        cached = self._cache.get(connection.connection_id)
        if cached is not None and cached[0] == fingerprint:
            return cached[1]
        policy = dict(self.transport_policy.get(connection.connection_id) or {})
        kind = connection.platform_kind
        if kind == PlatformKind.CTFD.value:
            adapter = CTFdAdapter(
                connection, self.secrets,
                client_transport=policy.get("client_transport"),
            )
        elif kind == PlatformKind.RCTF.value:
            adapter = RCtfAdapter(
                connection, self.secrets,
                client_transport=policy.get("client_transport"),
            )
        elif kind == PlatformKind.GZCTF.value:
            adapter = GZCTFAdapter(
                connection, self.secrets,
                client_transport=policy.get("client_transport"),
            )
        elif kind == PlatformKind.GENERIC_BROWSER.value:
            adapter = GenericBrowserAdapter(
                self.secrets,
                self.state_root / "browser-profiles" / connection.connection_id,
                site=policy.get("site"),
                available_transports=policy.get("available_transports"),
                automation_allowed=bool(
                    policy.get("automation_allowed", True)),
                transport_factory=policy.get("transport_factory"),
            )
            if connection.status == ConnectionStatus.ACTIVE.value:
                adapter.restore_persisted_session(
                    self.connection_ref(connection))
        else:
            adapter = self._extension_adapter(connection)
            if adapter is None:
                raise ValueError(f"unsupported platform kind: {kind!r}")
        self._cache[connection.connection_id] = (fingerprint, adapter)
        return adapter

    def _extension_adapter(self, connection: PlatformConnection) -> Any:
        if self.capability_registry is None:
            return None
        cap = self.capability_registry.get(
            "platform-adapter", connection.platform_kind
        )
        if cap is None or cap.state != "ready":
            return None
        label = connection.platform_kind
        if self.extension_bridge is not None:
            meta = getattr(self.extension_bridge, "_platform_adapter_meta", {}).get(
                connection.platform_kind
            ) or {}
            label = str(meta.get("label") or label)
        return ExtensionPlatformAdapter(
            cap,
            connection=connection,
            secrets=self.secrets,
            label=label,
        )

    async def probe(self, connection: PlatformConnection | str) -> Any:
        if isinstance(connection, str):
            loaded = self.store.get(PlatformConnection, connection)
            if loaded is None:
                raise NotFoundError(f"platform connection not found: {connection}")
            connection = loaded
        adapter = self.for_connection(connection)
        return await adapter.probe(self.connection_ref(connection))

    def invalidate(self, connection_id: str) -> None:
        self._cache.pop(str(connection_id), None)

    async def close(self) -> None:
        adapters = [item[1] for item in self._cache.values()]
        self._cache.clear()
        for adapter in adapters:
            close = getattr(adapter, "aclose", None)
            if callable(close):
                await close()


__all__ = ["PlatformAdapterFactory"]
