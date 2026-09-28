"""完整 Web 进程中的 Platform/Conversation/Competition/Extension 装配。"""

from __future__ import annotations

import asyncio
import os
import json
import logging
from pathlib import Path
from typing import Any

from fastapi import APIRouter, Request, Response

from apps.web.agent_runtime_api import (
    AgentRuntimeService,
    RuntimeInstanceConfigStore,
    create_agent_runtime_router,
)
from apps.web.capability_management_api import create_capability_management_router
from apps.web.competition_api import create_competition_router
from apps.web.conversation_api import create_conversation_router
from apps.web.conversation_shares import create_conversation_shares_router
from apps.web.conversation_metadata import ConversationMetadataService
from apps.web.extension_api import create_extension_router
from apps.web.platform_api import create_platform_router
from apps.web.operations_api import (
    ProductObservability,
    create_operations_router,
)
from apps.web.run_gateway import RunGateway
from apps.web.worker_config import WorkerConfigStore
from muteki.capability_bindings.http_jsonrpc import MutekiHttpJsonRpcBridge
from muteki.capability_bindings.mcp_server import MutekiControlMcpServer
from muteki.competition import events as competition_events
from muteki.competition.gate_bridge import (
    CompetitionGateBridge,
    CompetitionRunWitnessResolver,
)
from muteki.competition.commands import (
    COMPETITION_COMMAND_TYPES,
    CompetitionCommandApi,
)
from muteki.competition.effects import register_effect_control_handlers
from muteki.competition.services import CompetitionServiceFactory
from muteki.competition.secrets import PlatformSecretStore
from muteki.competition.submission import open_run_shared_graph
from muteki.competition.store import CompetitionStore
from muteki.conversation.module import ConversationService
from muteki.graphs.memory_timeline import MemoryTimelineGraph
from muteki.extensions.handlers import register_extension_handlers
from muteki.extensions.integrations import ExtensionRegistryBridge
from muteki.extensions.permissions import ProductExtensionSecretResolver
from muteki.extensions.registry import ExtensionService
from muteki.external_agents.factory import (
    RuntimeAdapterFactory,
)
from muteki.external_agents.registry import AdapterRegistry
from muteki.platform.command_api import MutekiCommandApiImpl
from muteki.platform.command_handlers.base import CommandAPIError
from muteki.platform.command_handlers.base import correlation_id_of, make_error
from muteki.platform.command_handlers.modules import register_domain_module_handlers
from muteki.platform.capability_gateway import (
    AgentCapabilityGatewayImpl,
)
from muteki.platform.contracts.errors import ErrorCategory
from muteki.platform.contracts.receipts import (
    AggregateRef,
    CommandReceipt,
    ReceiptState,
)
from muteki.platform.reconciler import Reconciler
from muteki.platform.startup import StartupRecoveryReport
from muteki.platform.builtin_modules import register_builtin_modules
from muteki.platform.registry_api import create_registry_router
from muteki.platform.registry import default_capability_catalog
from muteki.platform.store import IdempotencyConflictError, PlatformStore
from muteki.single_task import register_single_task_handlers
from muteki.solver.cli_driver import cli_adapter_for

LOG = logging.getLogger(__name__)


def _build_cli_compat_adapter(config: Any, common: dict[str, Any]) -> Any:
    """Web 产品层为 Runtime Factory 注入显式 CLI compatibility 构造器。"""
    profile: dict[str, Any] = {
        "engine": str(config.adapter_id).removeprefix("cli."),
    }
    return cli_adapter_for(profile, **common)


def _runtime_env_refs(
    platform_secrets: PlatformSecretStore,
    refs: Any,
) -> dict[str, str]:
    """解析显式环境引用；值只交给 Adapter 的子进程环境。"""
    resolved: dict[str, str] = {}
    for name, reference in dict(refs or {}).items():
        text = str(reference or "")
        if text.startswith("env:"):
            value = os.environ.get(text.removeprefix("env:"))
        elif text.startswith("secret://platform/"):
            try:
                value = platform_secrets.resolve(text)
            except Exception:
                value = None
        else:
            value = None
        if value is None:
            raise ValueError(f"Runtime 环境引用无法解析：{name}")
        resolved[str(name)] = value
    return resolved


_COMPETITION_AGGREGATE_TYPES = frozenset({
    competition_events.AGG_COMPETITION,
    competition_events.AGG_CONNECTION,
    competition_events.AGG_CHALLENGE,
    competition_events.AGG_BINDING,
    competition_events.AGG_LEASE,
    competition_events.AGG_SUBMISSION,
})


class WebCommandApiRouter:
    """把统一能力入口分派到平台或比赛 Command API。"""

    def __init__(
        self,
        platform_api: MutekiCommandApiImpl,
        competition_api: CompetitionCommandApi,
        *,
        global_store: PlatformStore,
    ) -> None:
        self.platform_api = platform_api
        self.competition_api = competition_api
        self.global_store = global_store

    @property
    def handlers(self) -> Any:
        """注册产品级非比赛 Handler 时使用平台 Registry。"""
        return self.platform_api.handlers

    @property
    def competition_store(self) -> CompetitionStore:
        return self.competition_api.competition_store

    def register_command(self, handler: Any) -> None:
        self.platform_api.register_command(handler)

    def register_query(self, handler: Any) -> None:
        self.platform_api.register_query(handler)

    async def dispatch(self, command: Any) -> Any:
        domain = (
            "competition"
            if command.command_type in COMPETITION_COMMAND_TYPES
            else "platform"
        )
        api = (
            self.competition_api
            if domain == "competition"
            else self.platform_api
        )
        try:
            self.global_store.claim_command_domain(command, domain)
        except IdempotencyConflictError as exc:
            receipt = CommandReceipt(
                command_id=command.command_id,
                state=ReceiptState.CONFLICT,
                aggregate=AggregateRef(
                    type=command.aggregate_type, id=command.aggregate_id),
                error=make_error(
                    "command.global_identity_conflict",
                    str(exc),
                    ErrorCategory.CONFLICT,
                    correlation_id=correlation_id_of(command),
                    recovery_hint="use a new command_id for the other domain",
                ),
                next=["receipt"],
            )
            self._log_command(command, domain, receipt)
            return receipt
        receipt = await api.dispatch(command)
        self._log_command(command, domain, receipt)
        return receipt

    @staticmethod
    def _log_command(command: Any, domain: str, receipt: Any) -> None:
        """记录可关联的命令元数据；不记录 payload、凭据或答案。"""
        LOG.info(json.dumps({
            "event": "product.command.receipt",
            "command_id": command.command_id,
            "command_type": command.command_type,
            "domain": domain,
            "aggregate_type": command.aggregate_type,
            "aggregate_id": command.aggregate_id,
            "receipt_id": receipt.receipt_id,
            "receipt_state": receipt.state.value,
            "correlation_id": (
                getattr(getattr(receipt, "error", None), "correlation_id", None)
                or correlation_id_of(command)
            ),
            "run_id": receipt.run_id,
        }, ensure_ascii=False))

    async def query(self, query: Any) -> Any:
        api = (
            self.competition_api
            if query.query_type
            in self.competition_api.handlers.known_query_types()
            else self.platform_api
        )
        return await api.query(query)

    async def read_events(self, request: Any, *, principal: Any = None) -> Any:
        api = self._api_for_aggregate(request.aggregate_type)
        return await api.read_events(request, principal=principal)

    async def wait(self, request: Any, *, principal: Any = None) -> Any:
        api = self._api_for_aggregate(request.aggregate_type)
        return await api.wait(request, principal=principal)

    async def get_receipt(self, command_id: str) -> Any:
        domain = self.global_store.command_domain(command_id)
        if domain == "platform":
            return await self.platform_api.get_receipt(command_id)
        if domain == "competition":
            return await self.competition_api.get_receipt(command_id)
        raise CommandAPIError(make_error(
            "command.receipt_not_found",
            f"no globally indexed receipt for command {command_id!r}",
            ErrorCategory.NOT_FOUND,
            correlation_id=command_id,
        ))

    def _api_for_aggregate(self, aggregate_type: str) -> MutekiCommandApiImpl:
        if aggregate_type in _COMPETITION_AGGREGATE_TYPES:
            return self.competition_api
        return self.platform_api


class WebPlatformStack:
    """共享后端入口所需的长驻服务与 router。"""

    def __init__(self, manager: Any) -> None:
        self.manager = manager
        self.root = Path(manager.state_root) / "_platform"
        self.root.mkdir(parents=True, exist_ok=True)

        self.store = PlatformStore(db_path=self.root / "platform.db")
        self.run_gateway = RunGateway(manager)
        self.command_api = MutekiCommandApiImpl.with_builtin_handlers(
            self.store,
            run_gateway=self.run_gateway,
        )
        register_single_task_handlers(self.command_api, run_manager=manager)
        self.domain_registry = register_builtin_modules()
        for registration in self.domain_registry.list_modules():
            if registration.error is not None:
                continue
            self.store.save(registration.descriptor)
            if self.store.domain_module_enabled(registration.descriptor.id) is False:
                self.domain_registry.disable(registration.descriptor.id)
        register_domain_module_handlers(self.command_api, self.domain_registry)
        self.registry = AdapterRegistry()
        self.runtime_store = RuntimeInstanceConfigStore(manager.state_root)
        self.worker_config = getattr(
            manager, "worker_config", WorkerConfigStore(manager.state_root)
        )
        self.platform_secrets = PlatformSecretStore(
            self.root / "competition" / "secrets")
        self.runtime_service = AgentRuntimeService(
            self.runtime_store,
            registry=self.registry,
            worker_config=self.worker_config,
            sessions_root=manager.state_root,
        )
        self.memory_graph = MemoryTimelineGraph(self.root / "memory.db")
        self.conversation = ConversationService(
            self.store,
            registry=self.registry,
            memory_graph=self.memory_graph,
            workspace_root=self.root / "workspaces",
            sessions_root=manager.state_root,
        )
        from apps.web.worker_models import CredentialModelCatalogStore, validate_conversation_effort
        self.conversation.executor.bind_model_success_recorder(
            CredentialModelCatalogStore(manager.state_root).record_conversation_success,
        )
        self.conversation.manager.bind_model_effort_validator(
            lambda selection: validate_conversation_effort(manager.state_root, selection)
        )
        self.conversation_metadata = ConversationMetadataService(
            self.store,
            self.conversation.manager,
            sessions_root=manager.state_root,
            worker_config=self.worker_config,
        )
        from muteki.capability_management import configure as configure_capabilities
        configure_capabilities(
            Path(self.store.db_path).with_name("capability_management.json"))
        from muteki.conversation.chat_plugins import ChatPluginService
        self.chat_plugins = ChatPluginService(self.root / "chat-plugins")
        self.conversation.manager.chat_plugins = self.chat_plugins
        self.conversation.executor.chat_plugins = self.chat_plugins
        self.conversation.register(self.command_api)
        self.runtime_factory = RuntimeAdapterFactory(
            store=self.store,
            binding_service=self.conversation.bindings,
            gateway_endpoint=os.environ.get(
                "MUTEKI_CAPABILITY_GATEWAY_ENDPOINT",
                "http://127.0.0.1:8000/api/capability",
            ),
            sessions_root=manager.state_root,
            env_ref_resolver=lambda refs: _runtime_env_refs(
                self.platform_secrets, refs),
            cli_builder=_build_cli_compat_adapter,
            probe_environment_factory=self.chat_plugins.prepare_environment,
        )
        self.runtime_service.factory = self.runtime_factory

        self.extension_registry_bridge = ExtensionRegistryBridge(
            self.domain_registry,
            self.store,
            adapter_registry=self.registry,
            binding_service=self.conversation.bindings,
            gateway_endpoint=os.environ.get(
                "MUTEKI_CAPABILITY_GATEWAY_ENDPOINT",
                "http://127.0.0.1:8000/api/capability",
            ),
        )
        self.extension = ExtensionService(
            self.store,
            install_root=self.root / "extensions" / "installed",
            state_root=self.root / "extensions" / "state",
            workspace_root=Path.cwd(),
            capabilities=default_capability_catalog(),
            secret_resolver=ProductExtensionSecretResolver(
                self.platform_secrets,
                allow_environment=(
                    os.environ.get("MUTEKI_ALLOW_ENV_EXTENSION_SECRETS") == "1"
                ),
            ),
            registration_bridge=self.extension_registry_bridge,
            permission_change_callback=self._on_extension_permission_changed,
            install_policy={
                "deny_unsigned": (
                    os.environ.get("MUTEKI_EXTENSION_DENY_UNSIGNED") == "1"),
                "deny_mutable_source": (
                    os.environ.get("MUTEKI_EXTENSION_DENY_MUTABLE_SOURCE") == "1"),
                "deny_high_permissions": (
                    os.environ.get("MUTEKI_EXTENSION_DENY_HIGH_PERMISSIONS") == "1"),
                "deny_untrusted_publisher": (
                    os.environ.get("MUTEKI_EXTENSION_DENY_UNTRUSTED") == "1"),
                "denied_publishers": [
                    item.strip() for item in os.environ.get(
                        "MUTEKI_EXTENSION_DENIED_PUBLISHERS", "").split(",")
                    if item.strip()
                ],
                "denied_extensions": [
                    item.strip() for item in os.environ.get(
                        "MUTEKI_EXTENSION_DENIED_IDS", "").split(",")
                    if item.strip()
                ],
            },
        )
        register_extension_handlers(self.command_api, self.extension)

        self.competition_store = CompetitionStore(
            db_path=self.root / "competition.db"
        )
        self._competition_graphs: dict[str, Any] = {}
        self.competition_services = CompetitionServiceFactory(
            store=self.competition_store,
            platform_store=self.store,
            run_gateway=self.run_gateway,
            root=self.root / "competition",
            shared_graph_for=self._shared_graph_for,
            workspace_root_for=manager.workspace_dir,
            source_witness_resolver=CompetitionRunWitnessResolver(
                self.competition_store, manager),
            secret_store=self.platform_secrets,
            capability_registry=self.extension_registry_bridge.capability_registry,
            extension_bridge=self.extension_registry_bridge,
        )
        self.competition_gate_bridge = CompetitionGateBridge(
            self.competition_store,
            self.competition_services.submissions,
            self.manager,
        )
        self.manager.add_product_event_sink(
            self.competition_gate_bridge.consume)
        self.competition_api = CompetitionCommandApi(
            self.competition_store,
            binding_store=self.store,
            services={
                "run_gateway": self.run_gateway,
                "effect_store": self.store,
                "competition_services": self.competition_services,
                "platform_adapter_factory": self.competition_services.adapters,
                "platform_secret_store": self.platform_secrets,
                "sync_service": self.competition_services.sync,
                "binding_service": self.competition_services.bindings,
                "lease_manager": self.competition_services.leases,
                "submission_service": self.competition_services.submissions,
            },
        )
        self.routed_command_api = WebCommandApiRouter(
            self.command_api,
            self.competition_api,
            global_store=self.store,
        )
        register_effect_control_handlers(
            self.command_api,
            self.store,
            self.competition_store,
            submission_service=self.competition_services.submissions,
        )
        from muteki.conversation.chat_plugin_gateway import ChatPluginGateway
        self.capability_gateway = ChatPluginGateway(
            self.store,
            self.routed_command_api,
            plugins=self.chat_plugins,
            selection=self.conversation.manager.runtime_selection,
            binding_service=self.conversation.bindings,
        )
        # Adapter 只在真实 Gateway 就绪后创建。Claude 的 in-process
        # Native Tool 会直接调用这个对象；若在 Gateway 创建前 populate，
        # Adapter 会永久持有 None，后续即使服务已启动也无法修复。
        self.runtime_factory.gateway = self.capability_gateway
        self.runtime_factory.populate(
            self.registry,
            [item for item in self.runtime_store.list() if item.enabled],
            include_structured_defaults=True,
            include_cli_compatibility=True,
        )
        # 同一端点同时承载两类真实协议：MCP Runtime 使用
        # initialize/tools/*，Pi/OMP/DSH 使用 muteki.* JSON-RPC。
        # Router 根据请求 method 分流，不让 MCP 客户端误入私有 Bridge。
        self.capability_mcp = MutekiControlMcpServer(self.capability_gateway)
        self.capability_bridge = MutekiHttpJsonRpcBridge(self.capability_gateway)
        self.platform_reconciler = Reconciler(self.store)
        self.competition_reconciler = self.competition_services.reconciler
        self._competition_recovery_task: asyncio.Task[Any] | None = None
        self.ready = False
        self.recovery: dict[str, Any] = {}
        self.control_receiver_status: dict[str, Any] = {
            "state": "unavailable",
            "detail": "control receiver has not started",
            "impact": "container Worker control is unavailable",
        }
        self.observability = ProductObservability(self)

    def set_control_receiver_status(self, status: dict[str, Any]) -> None:
        self.control_receiver_status = dict(status)

    def _shared_graph_for(self, run_id: str) -> Any:
        run_id = str(run_id or "")
        if not run_id:
            return None
        cached = self._competition_graphs.get(run_id)
        if cached is not None:
            return cached
        path = self.manager.graph_dir(run_id) / "shared_graph.db"
        if not path.exists():
            return None
        graph = open_run_shared_graph(str(path), challenge_id=run_id)
        self._competition_graphs[run_id] = graph
        return graph

    async def _on_product_secret_changed(
        self, old_reference: str, new_reference: str, action: str
    ) -> dict[str, Any]:
        revoked_grants = self.conversation.bindings.revoke_all_grants()
        closed_sessions = await self.conversation.executor.close_all(
            reason=f"secret_{action}")
        extensions = await self.extension.handle_secret_change(
            old_reference, new_reference)
        return {
            "action": action,
            "revoked_grants": revoked_grants,
            "closed_runtime_sessions": closed_sessions,
            "extensions": extensions,
        }

    async def _on_extension_permission_changed(
        self, extension_id: str, old: dict[str, Any], new: dict[str, Any]
    ) -> dict[str, Any]:
        revoked = self.conversation.bindings.revoke_all_grants()
        closed = await self.conversation.executor.close_all(
            reason="extension_permission_changed")
        return {
            "extension_id": extension_id,
            "revoked_grants": revoked,
            "closed_runtime_sessions": closed,
            "old": old,
            "new": new,
        }

    def routers(self) -> list[Any]:
        from apps.web.usage_api import create_usage_router
        from apps.web.chat_plugins_api import create_chat_plugins_router
        capability_router = APIRouter(tags=["capabilities"])

        async def _capability_http(request: Request) -> Response:
            body = await request.body()
            rpc_method = ""
            try:
                payload = json.loads(body.decode("utf-8"))
                if isinstance(payload, dict):
                    rpc_method = str(payload.get("method") or "")
            except (UnicodeDecodeError, json.JSONDecodeError):
                pass
            # MCP 的 initialize / notifications / tools / ping / discover 都由
            # MutekiControlMcpServer 处理；Muteki 内部 JSON-RPC 仅定义
            # muteki.describe 与 muteki.invoke。未知方法归 MCP，便于
            # Runtime 得到标准 MCP method-not-found 而不是误导性私有错误。
            bridge = (
                self.capability_bridge
                if rpc_method.startswith("muteki.")
                else self.capability_mcp
            )
            result = await bridge.handle_http(
                request.method,
                dict(request.headers),
                body,
            )
            return Response(
                content=result.body,
                status_code=result.status,
                headers=result.headers,
            )

        @capability_router.get("/api/capability")
        async def capability_http_get(request: Request) -> Response:
            return await _capability_http(request)

        @capability_router.post("/api/capability")
        async def capability_http_post(request: Request) -> Response:
            return await _capability_http(request)

        return [
            create_chat_plugins_router(self.chat_plugins, self.conversation),
            create_usage_router(self.manager, self.competition_store),
            create_registry_router(self.domain_registry, self.routed_command_api),
            create_platform_router(self.routed_command_api),
            create_operations_router(self.observability),
            capability_router,
            create_capability_management_router(
                store=self.store,
                binding_service=self.conversation.bindings,
                runtime_service=self.runtime_service,
                conversation_service=self.conversation,
                extension_service=self.extension,
            ),
            create_agent_runtime_router(
                command_api=self.routed_command_api,
                service=self.runtime_service,
            ),
            create_conversation_shares_router(service=self.conversation),
            create_conversation_router(
                command_api=self.routed_command_api,
                service=self.conversation,
                runtime_service=self.runtime_service,
                register_handlers=False,
                include_runtime_routes=False,
                sse_metrics=self.observability,
                extension_service=self.extension,
            ),
            create_competition_router(
                command_api=self.routed_command_api,
                secret_store=self.competition_services.secrets,
                platform_adapter_factory=self.competition_services.adapters,
                on_secret_changed=self._on_product_secret_changed,
                sse_metrics=self.observability,
            ),
            create_extension_router(self.extension, self.routed_command_api),
        ]

    async def recover(self) -> None:
        report = StartupRecoveryReport()
        platform = self.platform_reconciler.recover()
        report.add(
            "platform.store_and_receipts",
            "ready" if platform.ok else "unavailable",
            core=True,
            detail=("event log and receipts recovered" if platform.ok
                    else "; ".join(platform.issues)),
            impact="Command API writes are disabled when unavailable",
            evidence={
                "event_watermark": platform.event_watermark,
                "unfinished_receipts": len(platform.unfinished_receipts),
                "pending_outbox": len(platform.pending_outbox),
            },
        )
        try:
            conversation = await self.conversation.recover()
        except Exception as exc:
            conversation = {"error": f"{type(exc).__name__}: {exc}"}
            report.add(
                "conversation",
                "unavailable",
                core=True,
                detail=conversation["error"],
                impact="Conversation history remains read-only; new Turns are disabled",
            )
        else:
            report.add(
                "conversation", "ready", core=True,
                detail="projection rebuilt and orphan Turns reconciled",
                evidence=conversation,
            )
        startup_probe_enabled = os.environ.get(
            "MUTEKI_STARTUP_RUNTIME_PROBE", "0"
        ).strip().lower() not in {"0", "false", "no", "off", ""}
        if startup_probe_enabled:
            runtime_probe_summary = await self.runtime_service.probe_all()
        else:
            runtime_views = self.runtime_service.list_instances(
                include_discovered=True
            )
            runtime_probe_summary = {
                "probed_at": "",
                "total": len(runtime_views),
                "failed": 0,
                "cached": True,
                "results": [
                    {
                        "key": view["key"],
                        "adapter_id": view["adapter_id"],
                        "instance_id": view["instance_id"],
                        "engine": view["engine"],
                        "ok": True,
                        "healthy": bool((view.get("health") or {}).get("healthy")),
                        "detail": str(
                            (view.get("health") or {}).get("detail")
                            or "等待后台或手动健康检查"
                        ),
                    }
                    for view in runtime_views
                ],
            }
        runtime_probe_results = list(runtime_probe_summary["results"])
        runtime_count = len(self.registry.instances())
        report.add(
            "runtime.registry",
            "ready" if runtime_count else "unavailable",
            core=True,
            detail=f"{runtime_count} Runtime instances registered",
            impact="Conversation and Swarm cannot start Runtime sessions",
            evidence={
                "instances": runtime_count,
                "probes": runtime_probe_results,
                "healthy_defaults": sum(
                    1 for item in runtime_probe_results if item["healthy"]),
            },
        )
        try:
            extensions = await self.extension.recover_enabled()
        except Exception as exc:
            extensions = {"error": f"{type(exc).__name__}: {exc}"}
            report.add(
                "extensions", "degraded",
                detail=extensions["error"],
                impact="enabled extensions are unavailable; built-in modules continue",
            )
        else:
            ext_state = "degraded" if extensions.get("failed") else "ready"
            report.add(
                "extensions", ext_state,
                detail=("some enabled extensions failed to recover"
                        if ext_state == "degraded"
                        else "enabled extensions recovered"),
                impact=("failed extensions are isolated"
                        if ext_state == "degraded" else ""),
                evidence=extensions,
            )
        report.add(
            "competition", "degraded",
            detail="competition recovery is continuing in the background",
            impact="competition writes remain closed until recovery completes",
        )
        report.add(
            "run_gateway",
            "ready" if self.run_gateway is not None else "unavailable",
            core=True,
            detail="RunGateway is bound to RunManager",
            impact="competition Run dispatch is disabled",
        )
        control_state = str(
            self.control_receiver_status.get("state") or "unavailable")
        report.add(
            "control_receiver", control_state,
            detail=str(self.control_receiver_status.get("detail") or ""),
            impact=str(self.control_receiver_status.get("impact") or ""),
            evidence={
                key: value for key, value in self.control_receiver_status.items()
                if key not in {"state", "detail", "impact"}
            },
        )
        report.finish()
        self.recovery = report.model_dump(mode="json")
        self.ready = report.ready
        self._competition_recovery_task = asyncio.create_task(
            self._recover_competition(),
            name="muteki-competition-startup-recovery",
        )

    def _update_recovery_step(
        self, name: str, *, state: str, detail: str,
        impact: str = "", evidence: dict[str, Any] | None = None,
    ) -> None:
        steps = list(self.recovery.get("steps") or [])
        for step in steps:
            if step.get("name") == name:
                step.update({
                    "state": state,
                    "detail": detail,
                    "impact": impact,
                    "evidence": dict(evidence or {}),
                })
                break
        self.recovery["steps"] = steps
        core_ready = all(
            step.get("state") == "ready"
            for step in steps if step.get("core")
        )
        self.ready = core_ready
        self.recovery["ready"] = core_ready
        self.recovery["state"] = (
            "unavailable" if not core_ready else
            "degraded" if any(
                step.get("state") != "ready"
                for step in steps if not step.get("core")
            ) else "ready"
        )

    async def _recover_competition(self) -> None:
        try:
            gate_recovery = await self.competition_gate_bridge.recover(
                tuple(self.manager.runs.values()))
            competition = await self.competition_services.recover()
            await self.competition_services.start()
        except asyncio.CancelledError:
            raise
        except Exception as exc:
            self._update_recovery_step(
                "competition",
                state="unavailable",
                detail=f"{type(exc).__name__}: {exc}",
                impact=(
                    "competition effects, scheduling, leases and submissions "
                    "are disabled"
                ),
            )
            return
        competition_ready = bool(
            competition.get("ok", False) and self.competition_services.running)
        self._update_recovery_step(
            "competition",
            state="ready" if competition_ready else "unavailable",
            detail=(
                "competition recovery and service loops completed"
                if competition_ready
                else "competition recovery or service loop start failed"
            ),
            impact=("" if competition_ready
                    else "competition external effects are disabled"),
            evidence={
                **competition,
                "gate_bridge": gate_recovery,
                "services": self.competition_services.health()["services"],
            },
        )

    def health(self) -> dict[str, Any]:
        return {
            "status": (
                "ready" if self.ready and self.recovery.get("state") == "ready"
                else "degraded" if self.ready else "unavailable"
            ),
            "ready": self.ready,
            "recovery": dict(self.recovery),
            "competition": self.competition_services.health(),
            "competition_gate_bridge": self.competition_gate_bridge.health(),
            "runtime_instances": len(self.registry.instances()),
            "extensions": [
                {
                    "extension_id": item.extension_id,
                    "enabled": item.enabled,
                    "state": item.state.value,
                    "health": item.health,
                    "last_error": item.last_error,
                }
                for item in self.extension.list_records()
            ],
            "control_receiver": dict(self.control_receiver_status),
        }

    def startup_summary(self) -> dict[str, Any]:
        """启动完成后的安全摘要，供命令行和运维页使用。"""
        modules = []
        for registration in self.domain_registry.list_modules():
            modules.append({
                "id": registration.descriptor.id,
                "enabled": registration.state.value not in {
                    "disabled", "unavailable"},
                "state": registration.state.value,
                "detail": str(registration.error or ""),
            })
        degradations = [
            {
                "name": step.get("name"),
                "state": step.get("state"),
                "detail": step.get("detail"),
                "impact": step.get("impact"),
            }
            for step in self.recovery.get("steps", [])
            if step.get("state") != "ready"
        ]
        return {
            "status": self.health()["status"],
            "ready": self.ready,
            "backend_url": os.environ.get(
                "MUTEKI_BACKEND_URL",
                f"http://127.0.0.1:{os.environ.get('MUTEKI_WEB_PORT', '8000')}",
            ),
            "readiness_url": os.environ.get(
                "MUTEKI_BACKEND_URL",
                f"http://127.0.0.1:{os.environ.get('MUTEKI_WEB_PORT', '8000')}",
            ).rstrip("/") + "/api/readiness",
            "modules": modules,
            "degradations": degradations,
        }

    async def shutdown(self) -> None:
        self.ready = False
        self.manager.remove_product_event_sink(
            self.competition_gate_bridge.consume)
        recovery_task = self._competition_recovery_task
        self._competition_recovery_task = None
        if recovery_task is not None and not recovery_task.done():
            recovery_task.cancel()
            await asyncio.gather(recovery_task, return_exceptions=True)
        await self.competition_services.stop()
        await self.extension.shutdown()
        await self.conversation_metadata.shutdown()
        await self.conversation.shutdown()
        await self.chat_plugins.close()
        self.memory_graph.close()
        for graph in self._competition_graphs.values():
            try:
                graph.close()
            except Exception:
                pass
        self._competition_graphs.clear()
        self.competition_store.close()
        self.store.close()


__all__ = ["WebCommandApiRouter", "WebPlatformStack"]
