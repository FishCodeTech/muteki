"""Extension capability 与产品 Registry 的动态接线。

扩展的 ``provides`` 只有在子进程 ``capabilities/list`` 同时报告后才进入
产品注册表。Domain Module 使用现有 ``DomainModuleRegistry``；
External Agent Adapter 使用现有 ``AdapterRegistry``；其余控制层能力进入
``ExtensionCapabilityRegistry``，通过同一 Extension RPC 进程调用。

停用会保留可观测的 disabled 记录，同时撤销所有可执行句柄；卸载会彻底
注销。这样设置页显示的状态与产品当前真正可调用的能力一致。
"""

from __future__ import annotations

import re
from dataclasses import dataclass
from typing import Any, AsyncIterator, Optional

from muteki.external_agents.base import BaseExternalAgentAdapter
from muteki.external_agents.capabilities import (
    BOOL_CAPABILITY_FIELDS,
    CapabilityProbeReport,
    SOURCE_REPORTED,
)
from muteki.platform.contracts.external_agents import (
    AgentCapabilities,
    AgentEvent,
    AgentInput,
    AgentSessionRef,
    AgentSessionSnapshot,
    ProbeRequest,
)
from muteki.platform.contracts.extensions import ExtensionManifest
from muteki.platform.contracts.modules import (
    ArtifactObject,
    DomainModuleDescriptor,
    InstanceLeaseRef,
    InstanceResult,
    PlatformCapabilities,
    PlatformChallengeRef,
    PlatformConnectionRef,
    RemoteArtifactRef,
    SubmissionRequest,
    SubmissionResult,
    SyncRequest,
    SyncResult,
)
from muteki.platform.contracts.receipts import CommandReceipt
from muteki.extensions.protocol import ExtensionUnavailable


_TOKEN_RE = re.compile(r"[^a-zA-Z0-9_.-]+")
_COMMAND_SEGMENTS = {
    "external-agent-adapter": "external_agent",
    "platform-adapter": "platform_adapter",
    "agent-capability-binding": "capability_binding",
    "tool-integration": "tool_integration",
    "protocol-binding": "protocol_binding",
}


def _rpc_token(value: str) -> str:
    """把 manifest 标识转成稳定的扩展命令片段。"""
    return _TOKEN_RE.sub("_", str(value or "").strip()).strip("._-")


@dataclass
class ExtensionProvidedCapability:
    """一个运行中扩展提供的可调用能力句柄。"""

    type: str
    id: str
    api_version: int
    extension_id: str
    extension_version: str
    process: Any
    state: str = "ready"
    detail: str = ""

    @property
    def origin(self) -> str:
        return f"extension:{self.extension_id}"

    def public_view(self) -> dict[str, Any]:
        return {
            "type": self.type,
            "id": self.id,
            "api_version": self.api_version,
            "state": self.state,
            "origin": self.origin,
            "extension_id": self.extension_id,
            "extension_version": self.extension_version,
            "detail": self.detail,
        }

    async def invoke(
        self, action: str, payload: Optional[dict[str, Any]] = None
    ) -> dict[str, Any]:
        """经 Extension Host 的受控 RPC 调用该能力。"""
        if self.state != "ready" or self.process is None:
            # The request has not crossed the extension process boundary.  A
            # caller may safely retry after the extension becomes ready.
            raise ExtensionUnavailable(
                f"extension capability is {self.state}: {self.type}:{self.id}"
            )
        verb = _rpc_token(action)
        if not verb:
            raise ValueError("extension capability action is required")
        command_type = (
            f"ext.{self.extension_id}."
            f"{_COMMAND_SEGMENTS.get(self.type, _rpc_token(self.type).replace('-', '_'))}."
            f"{_rpc_token(self.id)}.{verb}"
        )
        return await self.process.handle_command(command_type, payload)


class ExtensionCapabilityRegistry:
    """非 DomainModule 扩展能力的运行期注册表。"""

    def __init__(self) -> None:
        self._entries: dict[tuple[str, str], ExtensionProvidedCapability] = {}

    def get(
        self, capability_type: str, capability_id: str
    ) -> Optional[ExtensionProvidedCapability]:
        return self._entries.get((capability_type, capability_id))

    def register(
        self, capability: ExtensionProvidedCapability
    ) -> ExtensionProvidedCapability:
        key = (capability.type, capability.id)
        existing = self._entries.get(key)
        if existing is not None and existing.extension_id != capability.extension_id:
            raise ValueError(
                "extension capability already registered: "
                f"{capability.type}:{capability.id}"
            )
        self._entries[key] = capability
        return capability

    def disable_owner(self, extension_id: str) -> list[ExtensionProvidedCapability]:
        changed: list[ExtensionProvidedCapability] = []
        for capability in self._entries.values():
            if capability.extension_id != extension_id:
                continue
            capability.state = "disabled"
            capability.detail = "extension is disabled"
            capability.process = None
            changed.append(capability)
        return changed

    def unregister(
        self, capability_type: str, capability_id: str
    ) -> Optional[ExtensionProvidedCapability]:
        return self._entries.pop((capability_type, capability_id), None)

    def unregister_owner(self, extension_id: str) -> list[ExtensionProvidedCapability]:
        removed: list[ExtensionProvidedCapability] = []
        for key, capability in list(self._entries.items()):
            if capability.extension_id != extension_id:
                continue
            removed.append(self._entries.pop(key))
        return removed

    def list_entries(
        self, *, include_disabled: bool = True
    ) -> list[ExtensionProvidedCapability]:
        entries = list(self._entries.values())
        if not include_disabled:
            entries = [item for item in entries if item.state == "ready"]
        return sorted(entries, key=lambda item: (item.type, item.id))

    async def invoke(
        self,
        capability_type: str,
        capability_id: str,
        action: str,
        payload: Optional[dict[str, Any]] = None,
    ) -> dict[str, Any]:
        capability = self.get(capability_type, capability_id)
        if capability is None:
            raise KeyError(
                f"unknown extension capability: {capability_type}:{capability_id}"
            )
        return await capability.invoke(action, payload)


class ExtensionExternalAgentAdapter(BaseExternalAgentAdapter):
    """把扩展子进程声明的 ExternalAgentAdapter 接入 AdapterRegistry。

    扩展命令前缀为
    ``ext.<extension-id>.external_agent.<provide-id>.<operation>``。响应使用
    平台现有 ExternalAgent 契约；形状不正确时如实失败，不伪造能力或事件。
    """

    def __init__(
        self,
        capability: ExtensionProvidedCapability,
        *,
        store: Any = None,
        binding_service: Any = None,
        gateway_endpoint: str = "",
    ) -> None:
        super().__init__(
            capability.id,
            instance_id="default",
            store=store,
            binding_service=binding_service,
            gateway_endpoint=gateway_endpoint,
        )
        self._capability = capability

    async def _invoke(
        self, operation: str, payload: Optional[dict[str, Any]] = None
    ) -> dict[str, Any]:
        return await self._capability.invoke(operation, payload)

    async def probe(self, request: ProbeRequest) -> AgentCapabilities:
        response = await self._invoke(
            "probe", {"request": request.model_dump(mode="json")}
        )
        raw = response.get("capabilities", response)
        capabilities = AgentCapabilities.model_validate(raw)
        self._probe_cache = CapabilityProbeReport(
            adapter_id=self.identity.adapter_id,
            instance_id=self.identity.instance_id,
            capabilities=capabilities,
            binary_path=f"extension://{self._capability.extension_id}",
            field_sources={
                field: SOURCE_REPORTED for field in BOOL_CAPABILITY_FIELDS
            },
            detail=(
                f"provided by {self._capability.extension_id}"
                f"@{self._capability.extension_version}"
            ),
        )
        return capabilities

    async def _launch(self, request, plan, bearer_token):
        payload: dict[str, Any] = {
            "request": request.model_dump(mode="json"),
            "injection_plan": (
                plan.model_dump(mode="json") if plan is not None else None
            ),
        }
        if bearer_token is not None:
            # Token 只经运行中子进程 RPC 交付，不进入 registry metadata/record。
            payload["bearer_token"] = bearer_token
        return await self._invoke("start", payload)

    async def _event_stream(
        self, operation: str, session: AgentSessionRef, payload: dict[str, Any]
    ) -> AsyncIterator[AgentEvent]:
        response = await self._invoke(operation, payload)
        raw_events = response.get("events")
        if not isinstance(raw_events, list):
            raise ValueError(
                f"extension adapter {operation} response must contain events[]"
            )
        for raw in raw_events:
            event = AgentEvent.model_validate(raw)
            if not event.agent_session_id:
                event = event.model_copy(update={
                    "agent_session_id": session.agent_session_id,
                    "external_session_id": session.external_session_id,
                })
            yield self.emit(event)

    def send(
        self, session: AgentSessionRef, input: AgentInput
    ) -> AsyncIterator[AgentEvent]:
        return self._event_stream("send", session, {
            "session": session.model_dump(mode="json"),
            "input": input.model_dump(mode="json"),
        })

    def resume(self, session: AgentSessionRef) -> AsyncIterator[AgentEvent]:
        return self._event_stream("resume", session, {
            "session": session.model_dump(mode="json"),
        })

    async def steer(
        self, session: AgentSessionRef, input: AgentInput
    ) -> CommandReceipt:
        response = await self._invoke("steer", {
            "session": session.model_dump(mode="json"),
            "input": input.model_dump(mode="json"),
        })
        return CommandReceipt.model_validate(response.get("receipt", response))

    async def interrupt(self, session: AgentSessionRef) -> CommandReceipt:
        response = await self._invoke("interrupt", {
            "session": session.model_dump(mode="json"),
        })
        return CommandReceipt.model_validate(response.get("receipt", response))

    async def snapshot(self, session: AgentSessionRef) -> AgentSessionSnapshot:
        response = await self._invoke("snapshot", {
            "session": session.model_dump(mode="json"),
        })
        return AgentSessionSnapshot.model_validate(
            response.get("snapshot", response)
        )

    async def _teardown(self, session: AgentSessionRef) -> str:
        response = await self._invoke("close", {
            "session": session.model_dump(mode="json"),
        })
        return str(response.get("classification") or "closed")


class ExtensionPlatformAdapter:
    """把扩展子进程声明的 PlatformAdapter 接入比赛工厂。

    命令前缀为
    ``ext.<extension-id>.platform_adapter.<provide-id>.<operation>``。
    响应使用平台现有 PlatformAdapter 契约；形状不正确时如实失败。
    凭据经 RPC payload 注入运行中子进程，不进入 registry metadata。
    """

    def __init__(
        self,
        capability: ExtensionProvidedCapability,
        *,
        connection: Any = None,
        secrets: Any = None,
        label: str = "",
    ) -> None:
        self.id = capability.id
        self._capability = capability
        self.connection = connection
        self._secrets = secrets
        self.label = label or capability.id
        self.extension_id = capability.extension_id
        self.extension_version = capability.extension_version

    def bind_connection(self, connection: Any, secrets: Any = None) -> "ExtensionPlatformAdapter":
        """按连接返回绑定副本（工厂缓存用）。"""
        return ExtensionPlatformAdapter(
            self._capability,
            connection=connection,
            secrets=secrets if secrets is not None else self._secrets,
            label=self.label,
        )

    async def _invoke(
        self, operation: str, payload: Optional[dict[str, Any]] = None
    ) -> dict[str, Any]:
        body = dict(payload or {})
        if self.connection is not None:
            body.setdefault(
                "connection",
                {
                    "connection_id": getattr(self.connection, "connection_id", ""),
                    "platform_kind": getattr(self.connection, "platform_kind", ""),
                    "endpoint": (
                        getattr(self.connection, "canonical_base_url", None)
                        or getattr(self.connection, "endpoint", "")
                        or ""
                    ),
                    "account_key": getattr(self.connection, "account_key", ""),
                    "credential_ref": getattr(self.connection, "credential_ref", ""),
                },
            )
            if self._secrets is not None:
                ref = str(getattr(self.connection, "credential_ref", "") or "")
                if ref:
                    try:
                        secret = self._secrets.resolve(ref)
                    except Exception:
                        secret = None
                    if secret:
                        # Token 只经运行中子进程 RPC 交付。
                        body["credential"] = secret
        try:
            return await self._capability.invoke(operation, body)
        except ExtensionUnavailable as exc:
            # Capability disable/reload happens before the platform request is
            # dispatched, so classify it as retryable rather than ambiguous.
            from muteki.competition.platforms.base import PlatformTransientError
            raise PlatformTransientError(str(exc)) from exc

    def _raise_platform_error(self, response: dict[str, Any]) -> None:
        err = response.get("error")
        if not isinstance(err, dict):
            return
        code = str(err.get("code") or err.get("category") or "")
        message = str(err.get("message") or code or "extension platform error")
        from muteki.competition.platforms.base import (
            PlatformAuthRequiredError,
            PlatformNotFoundError,
            PlatformPermissionError,
            PlatformRateLimitedError,
            PlatformTimeoutError,
            PlatformTransportError,
            PlatformUnknownResultError,
        )
        mapping = {
            "auth_required": PlatformAuthRequiredError,
            "authentication_required": PlatformAuthRequiredError,
            "rate_limited": PlatformRateLimitedError,
            "timeout": PlatformTimeoutError,
            "transport": PlatformTransportError,
            "not_found": PlatformNotFoundError,
            "permission": PlatformPermissionError,
            "unknown": PlatformUnknownResultError,
            "invalid_state": PlatformTransportError,
            "infra": PlatformTransportError,
        }
        exc_cls = mapping.get(code, PlatformTransportError)
        retry_after = err.get("retry_after_seconds")
        if exc_cls is PlatformRateLimitedError and retry_after is not None:
            raise PlatformRateLimitedError(
                message, retry_after_seconds=float(retry_after)
            )
        raise exc_cls(message)

    async def probe(self, connection: PlatformConnectionRef) -> PlatformCapabilities:
        response = await self._invoke(
            "probe", {"request": connection.model_dump(mode="json")}
        )
        self._raise_platform_error(response)
        raw = response.get("capabilities", response)
        caps = PlatformCapabilities.model_validate(raw)
        if not caps.platform_kind:
            caps = caps.model_copy(update={"platform_kind": self.id})
        return caps

    async def sync_competition(self, request: SyncRequest) -> SyncResult:
        response = await self._invoke(
            "sync_competition", {"request": request.model_dump(mode="json")}
        )
        self._raise_platform_error(response)
        return SyncResult.model_validate(response.get("result", response))

    async def fetch_artifact(self, artifact: RemoteArtifactRef) -> ArtifactObject:
        response = await self._invoke(
            "fetch_artifact", {"artifact": artifact.model_dump(mode="json")}
        )
        self._raise_platform_error(response)
        raw = response.get("artifact", response)
        if isinstance(raw.get("content"), str):
            import base64
            raw = dict(raw)
            raw["content"] = base64.b64decode(raw["content"])
        return ArtifactObject.model_validate(raw)

    async def acquire_instance(self, challenge: PlatformChallengeRef) -> InstanceResult:
        response = await self._invoke(
            "acquire_instance", {"challenge": challenge.model_dump(mode="json")}
        )
        self._raise_platform_error(response)
        return InstanceResult.model_validate(response.get("result", response))

    async def renew_instance(self, lease: InstanceLeaseRef) -> InstanceResult:
        response = await self._invoke(
            "renew_instance", {"lease": lease.model_dump(mode="json")}
        )
        self._raise_platform_error(response)
        return InstanceResult.model_validate(response.get("result", response))

    async def release_instance(self, lease: InstanceLeaseRef) -> None:
        response = await self._invoke(
            "release_instance", {"lease": lease.model_dump(mode="json")}
        )
        self._raise_platform_error(response)

    async def submit(self, request: SubmissionRequest) -> SubmissionResult:
        response = await self._invoke(
            "submit", {"request": request.model_dump(mode="json")}
        )
        self._raise_platform_error(response)
        return SubmissionResult.model_validate(response.get("result", response))

    async def reconcile_submission(self, request: dict[str, Any]) -> SubmissionResult:
        """Ask an extension adapter to reconcile an ambiguous submission."""
        response = await self._invoke(
            "reconcile_submission", {"request": dict(request)}
        )
        self._raise_platform_error(response)
        return SubmissionResult.model_validate(response.get("result", response))


class ExtensionRegistryBridge:
    """把 manifest provides 接入产品的实际运行期注册表。"""

    def __init__(
        self,
        registry: Any,
        store: Any,
        *,
        adapter_registry: Any = None,
        capability_registry: Optional[ExtensionCapabilityRegistry] = None,
        binding_service: Any = None,
        gateway_endpoint: str = "",
    ) -> None:
        # registry 名称保留，兼容已有调用；实际含义是 DomainModuleRegistry。
        self.registry = registry
        self.store = store
        self.adapter_registry = adapter_registry
        self.capability_registry = (
            capability_registry or ExtensionCapabilityRegistry()
        )
        self.binding_service = binding_service
        self.gateway_endpoint = gateway_endpoint
        self._owners: dict[tuple[str, str], str] = {}
        # platform-adapter provide_id → 展示元数据（label / extension_id）
        self._platform_adapter_meta: dict[str, dict[str, Any]] = {}

    def list_platform_adapters(
        self, *, include_disabled: bool = False
    ) -> list[dict[str, Any]]:
        """供大厅 /api/platform-kinds 列举扩展平台。"""
        out: list[dict[str, Any]] = []
        for cap in self.capability_registry.list_entries(
            include_disabled=include_disabled
        ):
            if cap.type != "platform-adapter":
                continue
            if not include_disabled and cap.state != "ready":
                continue
            meta = self._platform_adapter_meta.get(cap.id) or {}
            out.append({
                "id": cap.id,
                "label": meta.get("label") or cap.id,
                "origin": "extension",
                "source": "extension",
                "extension_id": cap.extension_id,
                "extension_version": cap.extension_version,
                "api_version": cap.api_version,
                "state": cap.state,
                "icon": meta.get("icon") or "plug",
            })
        return out

    def platform_adapter_supported(self, kind: str) -> bool:
        cap = self.capability_registry.get("platform-adapter", kind)
        return cap is not None and cap.state == "ready"
    @staticmethod
    def _reported_provides(process: Any) -> dict[tuple[str, str], int]:
        if process is None:
            return {}
        raw = process.capabilities.get("provides")
        if not isinstance(raw, list):
            return {}
        reported: dict[tuple[str, str], int] = {}
        for item in raw:
            if not isinstance(item, dict):
                continue
            capability_type = str(item.get("type") or "").strip()
            capability_id = str(item.get("id") or "").strip()
            if not capability_type or not capability_id:
                continue
            try:
                version = int(item.get("api_version", 1))
            except (TypeError, ValueError):
                continue
            reported[(capability_type, capability_id)] = version
        return reported

    def enable(
        self, manifest: ExtensionManifest, process: Any = None
    ) -> list[dict[str, Any]]:
        # 升级或重新启用先撤销上一版本的全部 live handle，防止 manifest
        # 删除 provide 后旧能力继续留在产品注册表。
        self._remove_owner(manifest.id, keep_disabled=False)
        reported = self._reported_provides(process)
        entries: list[dict[str, Any]] = []
        for provide in manifest.provides:
            key = (provide.type, provide.id)
            reported_version = reported.get(key)
            if reported_version is None:
                entries.append(self._unavailable(
                    provide.type,
                    provide.id,
                    "running extension did not report this manifest provide",
                    api_version=provide.api_version,
                ))
                continue
            if reported_version != provide.api_version:
                entries.append(self._unavailable(
                    provide.type,
                    provide.id,
                    "runtime api_version does not match manifest "
                    f"({reported_version} != {provide.api_version})",
                    api_version=provide.api_version,
                ))
                continue
            owner = self._owners.get(key)
            if owner is not None and owner != manifest.id:
                entries.append(self._unavailable(
                    provide.type,
                    provide.id,
                    "capability id is already owned by another extension",
                    api_version=provide.api_version,
                ))
                continue
            if provide.type == "domain-module":
                entry = self._register_domain_module(manifest, provide)
            else:
                entry = self._register_runtime_capability(
                    manifest, provide, process
                )
            if entry.get("state") not in {"unavailable", "error"}:
                self._owners[key] = manifest.id
            entries.append(entry)
        return entries

    def _register_domain_module(
        self, manifest: ExtensionManifest, provide: Any
    ) -> dict[str, Any]:
        module_id = provide.id or manifest.id
        existing = self.registry.get(module_id)
        if existing is not None:
            return self._unavailable(
                provide.type,
                module_id,
                "module id is already owned by another provider",
                api_version=provide.api_version,
            )
        slug = manifest.id.replace(".", "-").replace("_", "-")
        descriptor = DomainModuleDescriptor(
            id=module_id,
            version="1.0.0",
            task_kinds=[f"extension.{manifest.id}"],
            default_executor="external-agent.single",
            event_namespaces=list(manifest.permissions.events_write),
            workspace_kind=f"extension.{manifest.id}",
            ui_contributions={
                "title": f"Extension · {manifest.id}",
                "description": (
                    f"{manifest.id}@{manifest.version} 提供的扩展工作区"
                ),
                "icon": "layers",
                "route": f"/settings/extensions?extension={slug}",
                "create_entry": f"/settings/extensions?extension={slug}",
                "aggregate_type": "extension",
            },
        )
        registration = self.registry.register(
            descriptor, origin=f"extension:{manifest.id}"
        )
        self.store.save(descriptor)
        self.store.set_domain_module_enabled(
            descriptor.id, registration.error is None
        )
        return {
            "type": provide.type,
            "id": module_id,
            "api_version": provide.api_version,
            "state": registration.state.value,
            "origin": registration.origin,
            "extension_id": manifest.id,
            "extension_version": manifest.version,
            "error": (
                registration.error.model_dump(mode="json")
                if registration.error else None
            ),
        }

    def _register_runtime_capability(
        self, manifest: ExtensionManifest, provide: Any, process: Any
    ) -> dict[str, Any]:
        if process is None or not process.running:
            return self._unavailable(
                provide.type,
                provide.id,
                "extension process is not running",
                api_version=provide.api_version,
            )
        capability = ExtensionProvidedCapability(
            type=provide.type,
            id=provide.id,
            api_version=provide.api_version,
            extension_id=manifest.id,
            extension_version=manifest.version,
            process=process,
        )
        try:
            self.capability_registry.register(capability)
        except ValueError as exc:
            return self._unavailable(
                provide.type,
                provide.id,
                str(exc),
                api_version=provide.api_version,
            )

        if provide.type == "external-agent-adapter":
            if self.adapter_registry is None:
                self.capability_registry.unregister(provide.type, provide.id)
                return self._unavailable(
                    provide.type,
                    provide.id,
                    "product AdapterRegistry is not connected",
                    api_version=provide.api_version,
                )
            if self.adapter_registry.get(provide.id, "default") is not None:
                self.capability_registry.unregister(provide.type, provide.id)
                return self._unavailable(
                    provide.type,
                    provide.id,
                    "adapter id/default instance is already registered",
                    api_version=provide.api_version,
                )
            adapter = ExtensionExternalAgentAdapter(
                capability,
                store=self.store,
                binding_service=self.binding_service,
                gateway_endpoint=self.gateway_endpoint,
            )
            try:
                self.adapter_registry.register(
                    adapter,
                    instance_id="default",
                    metadata={
                        "origin": capability.origin,
                        "extension_id": manifest.id,
                        "extension_version": manifest.version,
                        "provide_type": provide.type,
                        "api_version": provide.api_version,
                        "enabled": True,
                    },
                )
            except ValueError as exc:
                self.capability_registry.unregister(provide.type, provide.id)
                return self._unavailable(
                    provide.type,
                    provide.id,
                    str(exc),
                    api_version=provide.api_version,
                )
        elif provide.type == "platform-adapter":
            # 与 builtin PlatformKind 撞名时拒绝，避免覆盖 CTFd 等内建路径。
            from muteki.competition.models import PlatformKind
            if provide.id in {k.value for k in PlatformKind}:
                self.capability_registry.unregister(provide.type, provide.id)
                return self._unavailable(
                    provide.type,
                    provide.id,
                    "platform-adapter id collides with builtin PlatformKind",
                    api_version=provide.api_version,
                )
            label = (
                str(getattr(manifest, "description", "") or "").strip()
                or str(getattr(manifest, "name", "") or "").strip()
                or provide.id
            )
            self._platform_adapter_meta[provide.id] = {
                "label": label,
                "extension_id": manifest.id,
                "icon": "flag",
            }
        return capability.public_view()

    def disable(self, extension_id: str) -> list[dict[str, Any]]:
        return self._remove_owner(extension_id, keep_disabled=True)

    def uninstall(self, extension_id: str) -> list[str]:
        removed = self._remove_owner(extension_id, keep_disabled=False)
        return [f"{item['type']}:{item['id']}" for item in removed]

    def _remove_owner(
        self, extension_id: str, *, keep_disabled: bool
    ) -> list[dict[str, Any]]:
        changed: list[dict[str, Any]] = []
        owned = [
            key for key, owner in self._owners.items() if owner == extension_id
        ]
        for capability_type, capability_id in owned:
            if capability_type == "domain-module":
                registration = self.registry.get(capability_id)
                if registration is not None:
                    if keep_disabled:
                        registration = self.registry.disable(capability_id)
                        state = registration.state.value
                    else:
                        self.registry.unregister(capability_id)
                        state = "uninstalled"
                    try:
                        self.store.set_domain_module_enabled(capability_id, False)
                    except Exception:
                        pass
                    changed.append({
                        "type": capability_type,
                        "id": capability_id,
                        "state": state,
                        "origin": f"extension:{extension_id}",
                        "extension_id": extension_id,
                    })
            else:
                capability = self.capability_registry.get(
                    capability_type, capability_id
                )
                if capability_type == "external-agent-adapter":
                    if self.adapter_registry is not None:
                        self.adapter_registry.unregister(capability_id, "default")
                if capability_type == "platform-adapter":
                    self._platform_adapter_meta.pop(capability_id, None)
                if capability is not None:
                    if keep_disabled:
                        capability.state = "disabled"
                        capability.detail = "extension is disabled"
                        capability.process = None
                        changed.append(capability.public_view())
                    else:
                        changed.append({
                            **capability.public_view(),
                            "state": "uninstalled",
                            "detail": "extension is uninstalled",
                        })
        if keep_disabled:
            self.capability_registry.disable_owner(extension_id)
        else:
            self.capability_registry.unregister_owner(extension_id)
            for key in owned:
                self._owners.pop(key, None)
        return changed

    def list_entries(self, *, include_disabled: bool = True) -> list[dict[str, Any]]:
        """公开非 DomainModule 能力状态，供设置与运维页面读取。"""
        return [
            item.public_view()
            for item in self.capability_registry.list_entries(
                include_disabled=include_disabled
            )
        ]

    @staticmethod
    def _unavailable(
        capability_type: str,
        capability_id: str,
        detail: str,
        *,
        api_version: int,
    ) -> dict[str, Any]:
        return {
            "type": capability_type,
            "id": capability_id,
            "api_version": api_version,
            "state": "unavailable",
            "detail": detail,
        }


__all__ = [
    "ExtensionCapabilityRegistry",
    "ExtensionExternalAgentAdapter",
    "ExtensionPlatformAdapter",
    "ExtensionProvidedCapability",
    "ExtensionRegistryBridge",
]
