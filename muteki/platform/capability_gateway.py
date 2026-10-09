"""AgentCapabilityGateway 实现（任务书 6.6、设计 9.4/17.3，CAP-01）。

``AgentCapabilityGatewayImpl`` 实现 ``contracts.protocols.AgentCapabilityGateway``
Protocol，是外部主对话 Agent 调用 Muteki 获准能力的唯一协议中立入口：

- ``describe`` 只返回该 CapabilityBinding tool_set 允许的工具 schema；
- ``invoke`` 先验证 Grant（binding token 引用）、Binding 版本 / Thread /
  AgentSession / principal / audience / 有效期 / 撤销状态，再检查工具是否
  属于该 Binding，最后归一化到 MutekiCommandAPI 的 dispatch / query /
  read_events / wait / get_receipt；
- 统一 correlation id 与错误 envelope；查询结果完整返回。

Gateway 不保存任何 Run / Competition / SharedGraph / 提交队列 / 事件缓存 /
AgentSession 业务状态：Binding 与 Grant 读取自 PlatformStore，业务权限
（command/query/resource scope）由 Command API 的 CommandPolicy 统一判定，
本类只做协议入口侧的绑定上下文校验，不复制业务权限规则。
"""

from __future__ import annotations

import logging
import json
from time import perf_counter
from typing import Any, Callable, Optional

from muteki.platform.capability_bindings import CapabilityBindingService
from muteki.platform.capability_catalog import (
    DEFAULT_CATALOG,
    CapabilityCatalog,
    CapabilityToolSpec,
    ToolTargetKind,
    effective_tool_set,
)
from muteki.platform.command_handlers.base import (
    CommandAPIError,
    CommandFailed,
    make_error,
)
from muteki.platform.contracts.capabilities import (
    BindingContext,
    CapabilityBinding,
    CapabilityDescriptor,
    CapabilityGrant,
    CapabilityInvocation,
    CapabilityResult,
)
from muteki.platform.contracts.commands import (
    ActorRef,
    CommandEnvelope,
    EventReadRequest,
    QueryEnvelope,
    QueryResult,
    WaitRequest,
)
from muteki.platform.contracts.errors import ErrorCategory, ErrorEnvelope
from muteki.platform.contracts.graphs import GraphScope
from muteki.platform.contracts.events import EventEnvelope
from muteki.platform.contracts.objects import (
    AgentSession,
    Project,
    Thread,
)
from muteki.platform.contracts.protocols import MutekiCommandAPI
from muteki.platform.store import PlatformStore

LOG = logging.getLogger(__name__)

class AgentCapabilityGatewayImpl:
    """``AgentCapabilityGateway`` Protocol 的默认实现（无业务状态）。"""

    def __init__(
        self,
        store: PlatformStore,
        command_api: MutekiCommandAPI,
        *,
        catalog: CapabilityCatalog = DEFAULT_CATALOG,
        binding_service: Optional[CapabilityBindingService] = None,
    ) -> None:
        self._store = store
        self._api = command_api
        self._catalog = catalog
        self._bindings = binding_service or CapabilityBindingService(
            store, catalog=catalog)

    # -- describe --------------------------------------------------------------

    async def describe(self, binding_id: str) -> CapabilityDescriptor:
        """该 Binding 可见的能力描述；只包含 tool_set 允许的工具。"""
        binding = self._bindings.get_binding(binding_id)
        if binding is None:
            raise CommandAPIError(make_error(
                "capability.binding.not_found",
                f"unknown capability binding {binding_id!r}",
                ErrorCategory.NOT_FOUND,
                correlation_id=binding_id))
        if binding.revoked_at is not None:
            raise CommandAPIError(make_error(
                "capability.binding.revoked",
                "capability binding has been revoked",
                ErrorCategory.PERMISSION,
                correlation_id=binding_id))
        tool_set = effective_tool_set(binding.mode, binding.tool_set)
        return CapabilityDescriptor(
            binding_id=binding.binding_id,
            binding_version=binding.binding_version,
            mode=binding.mode,
            tools=[spec.describe() for spec in self._catalog.filter(tool_set)],
            allowed_commands=self._catalog.command_types_of(tool_set),
            allowed_queries=self._catalog.query_types_of(tool_set),
            resource_scopes=list(binding.resource_scopes),
        )

    # -- invoke ------------------------------------------------------------------

    async def invoke(
        self, context: BindingContext, invocation: CapabilityInvocation
    ) -> CapabilityResult:
        started = perf_counter()
        result = await self._invoke(context, invocation)
        duration_ms = (perf_counter() - started) * 1000
        try:
            binding = self._bindings.get_binding(context.binding_id)
            if binding is not None:
                from muteki.capability_bindings.http_jsonrpc import capability_result_payload
                self._store.append_events(EventEnvelope(
                    aggregate_type="capability", aggregate_id=binding.binding_id,
                    event_type="core.capability.invoked", producer="builtin.capability",
                    actor_id=context.principal_id,
                    correlation_id=invocation.invocation_id,
                    payload={"tool": invocation.tool_name, "invocation_id": invocation.invocation_id,
                             "thread_id": binding.thread_id, "agent_session_id": context.agent_session_id,
                             "binding_version": context.binding_version, "ok": result.ok,
                             "receipt_state": result.receipt.state.value if result.receipt is not None else None,
                             "error_code": result.error.code if result.error is not None else None,
                             "duration_ms": round(duration_ms, 3),
                             "input_bytes": len(json.dumps(invocation.arguments, ensure_ascii=False,
                                                          separators=(",", ":")).encode()),
                             "result_bytes": len(json.dumps(capability_result_payload(result), ensure_ascii=False,
                                                           separators=(",", ":")).encode())},
                ))
        except Exception:
            # A completed action must not be retried because telemetry failed.
            # Keep the business result intact and make the recording failure visible.
            LOG.exception("capability telemetry persistence failed: invocation=%s", invocation.invocation_id)
        return result

    async def _invoke(
        self, context: BindingContext, invocation: CapabilityInvocation
    ) -> CapabilityResult:
        correlation_id = (
            str(invocation.correlation_id or "").strip() or invocation.invocation_id
        )
        try:
            binding, _grant = self._authorize(context, correlation_id)
            spec = self._require_tool(binding, invocation.tool_name, correlation_id)
            errors = self._catalog.validation_errors(spec.name, invocation.arguments)
            if errors:
                required = spec.input_schema.get("required", [])
                if spec.aggregate_arg in required and spec.aggregate_arg not in invocation.arguments:
                    self._require_aggregate_arg(spec, invocation.arguments, correlation_id)
                raise CommandAPIError(ErrorEnvelope(
                    code="capability.arguments_invalid",
                    message=f"{spec.name} 参数不符合工具 Schema：" + "; ".join(
                        ".".join(str(part) for part in error["path"]) + " (" + error["rule"] + ")"
                        for error in errors),
                    category=ErrorCategory.VALIDATION,
                    correlation_id=correlation_id,
                    recovery_hint="根据 detail.validation_errors 修正参数后重试同一工具；不要猜测 ID 或把校验失败当作空结果。",
                    detail={"validation_errors": errors},
                ))
            if spec.target_kind is ToolTargetKind.QUERY:
                defaults = {name: schema["default"] for name, schema in spec.input_schema.get("properties", {}).items()
                            if "default" in schema}
                if defaults:
                    invocation = invocation.model_copy(update={"arguments": {**defaults, **invocation.arguments}})
            return await self._dispatch_to_command_api(
                binding, spec, invocation, correlation_id)
        except CommandAPIError as exc:
            return CapabilityResult(
                ok=False,
                invocation_id=invocation.invocation_id,
                error=self._with_correlation(exc.error, correlation_id),
            )
        except Exception as exc:  # 保持边界可见，不向外泄漏内部细节
            LOG.exception("capability invoke failed: %s", invocation.tool_name)
            return CapabilityResult(
                ok=False,
                invocation_id=invocation.invocation_id,
                error=make_error(
                    "capability.internal",
                    f"{type(exc).__name__}: {exc}",
                    ErrorCategory.INTERNAL,
                    correlation_id=correlation_id),
            )

    # -- 绑定上下文校验（协议入口侧；业务权限仍由 Command API 判定） -------------

    def _authorize(
        self, context: BindingContext, correlation_id: str
    ) -> tuple[CapabilityBinding, CapabilityGrant]:
        """验证 binding token（Grant 引用）、Thread、AgentSession、principal、
        audience、有效期和撤销状态；通过后置 Grant 活跃心跳（touch）。"""

        def denied(code: str, message: str) -> CommandAPIError:
            return CommandAPIError(make_error(
                code, message, ErrorCategory.PERMISSION,
                correlation_id=correlation_id))

        # Binding：存在、版本匹配、未撤销、Thread 与 principal 匹配。
        binding = self._bindings.get_binding(context.binding_id)
        if binding is None:
            raise denied(
                "capability.binding.not_found",
                f"unknown capability binding {context.binding_id!r}")
        if context.binding_version and context.binding_version != binding.binding_version:
            raise denied(
                "capability.binding.stale",
                f"binding {context.binding_id!r} is now at version "
                f"{binding.binding_version}; reload the session and retry")
        if binding.revoked_at is not None:
            raise denied(
                "capability.binding.revoked",
                "capability binding has been revoked")
        if binding.thread_id and binding.thread_id != context.thread_id:
            raise denied(
                "capability.binding.thread_mismatch",
                "binding thread does not match invocation context")
        if binding.principal_id and binding.principal_id != context.principal_id:
            raise denied(
                "capability.binding.principal_mismatch",
                "binding principal does not match invocation context")

        # Grant（binding token）：存在、属于该 Binding 与 Session、audience
        # 匹配、未撤销、未过期。
        if not context.grant_id:
            raise denied(
                "capability.grant.required",
                "invocation context carries no grant credential")
        grant = self._bindings.get_grant(context.grant_id)
        if grant is None:
            raise denied(
                "capability.grant.not_found",
                "unknown grant credential")
        if grant.binding_id != binding.binding_id:
            raise denied(
                "capability.grant.binding_mismatch",
                "grant was not issued for this binding")
        if context.agent_session_id and grant.agent_session_id != context.agent_session_id:
            raise denied(
                "capability.grant.session_mismatch",
                "grant was not issued for this agent session")
        if grant.audience and grant.audience != context.audience:
            raise denied(
                "capability.grant.audience_mismatch",
                "grant audience does not match invocation context")
        if grant.revoked_at is not None:
            raise denied(
                "capability.grant.revoked",
                "grant has been revoked")
        if self._bindings.grant_expired(grant):
            raise denied(
                "capability.grant.expired",
                "grant has expired; rotate or re-issue the grant")

        # AgentSession：存在、未关闭、归属该 Thread。
        if context.agent_session_id:
            session = self._store.get(AgentSession, context.agent_session_id)
            if session is None:
                raise denied(
                    "capability.session.not_found",
                    f"unknown agent session {context.agent_session_id!r}")
            if session.closed_at is not None:
                raise denied(
                    "capability.session.closed",
                    "agent session is closed; grant must be re-issued")
            if (session.thread_id and binding.thread_id
                    and session.thread_id != binding.thread_id):
                raise denied(
                    "capability.session.thread_mismatch",
                    "agent session does not belong to the binding thread")

        self._bindings.touch_grant(grant.grant_id)
        return binding, grant

    def _require_tool(
        self, binding: CapabilityBinding, tool_name: str, correlation_id: str
    ) -> CapabilityToolSpec:
        spec = self._catalog.get(tool_name)
        if spec is None:
            raise CommandAPIError(make_error(
                "capability.tool_unknown",
                f"unknown capability tool {tool_name!r}",
                ErrorCategory.VALIDATION,
                correlation_id=correlation_id,
                recovery_hint="刷新当前会话的工具目录，使用目录中的准确工具名。"))
        if spec.name not in effective_tool_set(binding.mode, binding.tool_set):
            raise CommandAPIError(make_error(
                "capability.tool_not_allowed",
                f"tool {tool_name!r} is not permitted for mode "
                f"{binding.mode.value}",
                ErrorCategory.PERMISSION,
                correlation_id=correlation_id,
                recovery_hint="request a binding for a thread mode that grants this tool"))
        return spec

    # -- 归一化到 MutekiCommandAPI ---------------------------------------------

    async def _dispatch_to_command_api(
        self,
        binding: CapabilityBinding,
        spec: CapabilityToolSpec,
        invocation: CapabilityInvocation,
        correlation_id: str,
    ) -> CapabilityResult:
        args = dict(invocation.arguments)
        actor = ActorRef(
            kind="agent",
            id=binding.principal_id,
            binding_id=binding.binding_id,
            binding_version=binding.binding_version,
            thread_id=binding.thread_id,
        )

        if spec.target_kind is ToolTargetKind.COMMAND:
            receipt = await self._api.dispatch(self._build_command(spec, args, actor))
            return CapabilityResult(
                ok=receipt.error is None,
                invocation_id=invocation.invocation_id,
                receipt=receipt,
                error=receipt.error,
            )
        if spec.target_kind is ToolTargetKind.QUERY:
            if spec.aggregate_from_caller_thread:
                aggregate_id = binding.thread_id
            elif spec.aggregate_arg:
                aggregate_id = self._require_aggregate_arg(spec, args, correlation_id)
            else:
                aggregate_id = ""
            result = await self._api.query(QueryEnvelope(
                query_type=str(spec.query_type or ""),
                aggregate_type=spec.aggregate_type or None,
                aggregate_id=aggregate_id,
                actor=actor,
                params=args,
            ))
            return CapabilityResult(
                ok=True,
                invocation_id=invocation.invocation_id,
                result=result.result,
            )
        if spec.target_kind is ToolTargetKind.READ_EVENTS:
            aggregate_id = self._require_aggregate_arg(
                spec, args, correlation_id)
            page = await self._api.read_events(EventReadRequest(
                aggregate_type=spec.aggregate_type,
                aggregate_id=aggregate_id,
                after_cursor=(str(args["after_cursor"]).strip() or None)
                if args.get("after_cursor") is not None else None,
                limit=int(args.get("limit") or 100),
            ), principal=actor)
            return CapabilityResult(
                ok=True,
                invocation_id=invocation.invocation_id,
                result=page.model_dump(mode="json"),
            )
        if spec.target_kind is ToolTargetKind.WAIT:
            aggregate_id = self._require_aggregate_arg(
                spec, args, correlation_id)
            raw_timeout = args.get("timeout_seconds")
            outcome = await self._api.wait(WaitRequest(
                aggregate_type=spec.aggregate_type,
                aggregate_id=aggregate_id,
                after_cursor=(str(args["after_cursor"]).strip() or None)
                if args.get("after_cursor") is not None else None,
                timeout_seconds=(
                    30.0 if raw_timeout is None else float(raw_timeout)
                ),
                limit=int(args.get("limit") or 100),
            ), principal=actor)
            return CapabilityResult(
                ok=True,
                invocation_id=invocation.invocation_id,
                result=outcome.model_dump(mode="json"),
            )
        # RECEIPT：与 Web 相同的 CommandReceipt。
        command_id = str(args.get("command_id") or "").strip()
        if not command_id:
            raise CommandAPIError(make_error(
                "capability.argument_required",
                f"{spec.name} requires arguments.command_id",
                ErrorCategory.VALIDATION,
                correlation_id=correlation_id))
        receipt = await self._api.get_receipt(command_id)
        return CapabilityResult(
            ok=True,
            invocation_id=invocation.invocation_id,
            receipt=receipt,
        )

    def _build_command(
        self, spec: CapabilityToolSpec, args: dict[str, Any], actor: ActorRef
    ) -> CommandEnvelope:
        """工具入参 → typed command 信封。

        不向前透传 correlation_id 等 Gateway 私有键：payload 与 Web 入口保持
        逐字节一致，同一 command_id 的重放才会命中 Command API 的幂等去重
        （与 Web 返回同一 receipt，而不是内容冲突）。
        """
        envelope_keys = {"command_id", "idempotency_key", "expected_version"}
        payload = {k: v for k, v in args.items() if k not in envelope_keys}
        # 工具声明的默认 payload 键（如 dispatch_challenge 的 kind）垫底，
        # 调用方显式传值优先。
        if spec.payload_defaults:
            payload = {**spec.payload_defaults, **payload}
        fields: dict[str, Any] = {
            "command_type": str(spec.command_type or ""),
            "aggregate_type": spec.aggregate_type,
            "aggregate_id": (
                str(actor.thread_id or "") if spec.aggregate_from_caller_thread
                else self._aggregate_arg(spec, args)
            ),
            "actor": actor,
            "payload": payload,
        }
        if args.get("command_id"):
            fields["command_id"] = str(args["command_id"]).strip()
        if args.get("idempotency_key"):
            fields["idempotency_key"] = str(args["idempotency_key"]).strip()
        if args.get("expected_version") is not None:
            fields["expected_version"] = int(args["expected_version"])
        return CommandEnvelope(**fields)

    @staticmethod
    def _aggregate_arg(spec: CapabilityToolSpec, args: dict[str, Any]) -> str:
        if not spec.aggregate_arg:
            return ""
        return str(args.get(spec.aggregate_arg) or "").strip()

    def _require_aggregate_arg(
        self, spec: CapabilityToolSpec, args: dict[str, Any], correlation_id: str
    ) -> str:
        value = self._aggregate_arg(spec, args)
        if not value:
            raise CommandAPIError(ErrorEnvelope(
                code="capability.argument_required",
                message=f"{spec.name} requires arguments.{spec.aggregate_arg}",
                category=ErrorCategory.VALIDATION,
                correlation_id=correlation_id,
                recovery_hint=f"从列表查询或创建回执取得真实资源 ID，填入 arguments.{spec.aggregate_arg} 后重试。",
                detail={"validation_errors": [{"path": ["arguments", spec.aggregate_arg],
                                               "rule": "nonempty_reference", "expected": "existing resource ID"}]},
            ))
        return value

    @staticmethod
    def _with_correlation(error: ErrorEnvelope, correlation_id: str) -> ErrorEnvelope:
        if error.correlation_id:
            return error
        return error.model_copy(update={"correlation_id": correlation_id})


# ---------------------------------------------------------------------------
# 能力工具的只读查询 Handler（经 Command API 统一授权边界）
# ---------------------------------------------------------------------------


class ProjectListQueryHandler:
    """project.list：列出完整 Project 投影数据。"""

    query_types = {"project.list"}

    async def handle(self, query: QueryEnvelope, ctx: Any) -> QueryResult:
        projects = ctx.store.list(Project)
        return QueryResult(
            query_id=query.query_id,
            query_type=query.query_type,
            result=[p.model_dump(mode="json") for p in projects],
        )


class ThreadListQueryHandler:
    """thread.list：筛选并分页列出完整 Thread 投影数据。"""

    query_types = {"thread.list"}

    async def handle(self, query: QueryEnvelope, ctx: Any) -> QueryResult:
        params = dict(query.params or {})
        project_id = str(params.get("project_id") or "").strip()
        mode = str(params.get("mode") or "").strip()
        needle = str(params.get("query") or "").strip().casefold()
        try:
            limit = int(params.get("limit") or 50)
        except (TypeError, ValueError) as exc:
            raise CommandFailed(make_error(
                "thread.list.bad_limit",
                "thread.list limit 必须是 1 到 200 的整数",
                ErrorCategory.VALIDATION,
                correlation_id=query.query_id,
            )) from exc
        if not 1 <= limit <= 200:
            raise CommandFailed(make_error(
                "thread.list.bad_limit",
                "thread.list limit 必须是 1 到 200 的整数",
                ErrorCategory.VALIDATION,
                correlation_id=query.query_id,
            ))

        threads = ctx.store.list(Thread)
        if project_id:
            threads = [item for item in threads if item.project_id == project_id]
        if mode:
            threads = [item for item in threads if item.mode == mode]
        if needle:
            threads = [
                item for item in threads
                if needle in str(item.thread_id).casefold()
                or needle in str(item.title or "").casefold()
            ]
        threads.sort(
            key=lambda item: str(item.updated_at or item.created_at or ""),
            reverse=True,
        )
        total = len(threads)
        from .command_handlers.pagination import page_items
        selected, continuation = page_items(threads, params, limit, ctx, query.query_type,
                                             lambda thread: thread.thread_id)
        return QueryResult(
            query_id=query.query_id,
            query_type=query.query_type,
            result={
                "threads": [t.model_dump(mode="json") for t in selected],
                "total": total,
                "returned": len(selected),
                **continuation,
            },
        )


#: run_id → GraphService 的解析器（由平台接线提供；Gateway / Handler 自身
#: 不打开、不缓存任何 SharedGraph 句柄）。
GraphResolver = Callable[[str], Any]


class SharedGraphReadQueryHandler:
    """graph.shared.read：SharedGraph 只读状态视图（muteki_read_shared_graph）。

    经 Command API query 返回完整的 fact / intent / route / branch /
    review / directive / flag 状态视图；禁止返回数据库写入句柄。
    """

    query_types = {"graph.shared.read"}

    def __init__(self, graph_resolver: Optional[GraphResolver] = None) -> None:
        self._resolver = graph_resolver

    async def handle(self, query: QueryEnvelope, ctx: Any) -> QueryResult:
        run_id = str(
            query.params.get("run_id") or query.aggregate_id or "").strip()
        if not run_id:
            raise CommandFailed(make_error(
                "graph.run_id_required",
                "graph.shared.read requires params.run_id",
                ErrorCategory.VALIDATION,
                correlation_id=query.query_id))
        graph = self._resolver(run_id) if self._resolver is not None else None
        if graph is None:
            raise CommandFailed(make_error(
                "graph.unavailable",
                f"no shared graph available for run {run_id!r}",
                ErrorCategory.NOT_FOUND,
                correlation_id=query.query_id))
        snapshot = await graph.snapshot(GraphScope(graph_id=graph.id))
        state = dict(snapshot.state)
        available_sections = sorted(state)
        sections = query.params.get("sections")
        if sections is not None:
            if (not isinstance(sections, list) or not sections
                    or any(not isinstance(section, str) or section not in state for section in sections)):
                raise CommandFailed(ErrorEnvelope(
                    code="graph.sections_invalid", category=ErrorCategory.VALIDATION,
                    message="sections 必须是 available_sections 中的区块名称列表",
                    correlation_id=query.query_id,
                    recovery_hint="从 detail.available_sections 选择需要的区块。",
                    detail={"available_sections": available_sections},
                ))
            state = {section: state[section] for section in sections}
        return QueryResult(
            query_id=query.query_id,
            query_type=query.query_type,
            result={
                "graph_id": snapshot.graph_id,
                "watermark": snapshot.watermark,
                "state": state,
                "available_sections": available_sections,
            },
        )


def register_capability_query_handlers(
    api: Any, *, graph_resolver: Optional[GraphResolver] = None
) -> None:
    """把能力工具依赖的只读查询 Handler 注册到 Command API。

    ``graph_resolver`` 由平台接线提供（run_id → GraphService）；未提供时
    muteki_read_shared_graph 返回统一 graph.unavailable 错误。比赛类
    query（competition.*）由 COMP-09 注册，不在此处。
    """
    api.register_query(ProjectListQueryHandler())
    api.register_query(ThreadListQueryHandler())
    api.register_query(SharedGraphReadQueryHandler(graph_resolver))


__all__ = [
    "AgentCapabilityGatewayImpl",
    "GraphResolver",
    "ProjectListQueryHandler",
    "SharedGraphReadQueryHandler",
    "ThreadListQueryHandler",
    "register_capability_query_handlers",
]
