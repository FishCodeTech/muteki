"""Command/Query Handler 框架与统一判定层（任务书 6.1，COMMAND-01）。

权限判定只在这里实现一次，供 Command API 的 dispatch / query / read_events /
wait 共用；Web、MCP、CLI 等传输入口只做身份认证和格式转换，不得复制这里的
Principal / CapabilityBinding / resource scope / command policy 判定。

- ``CommandPolicy``：Principal（ActorRef）→ operator 全权；非 operator 必须
  持有 PlatformStore 持久化的有效 CapabilityBinding（未撤销、principal 匹配、
  command/query 在允许清单、resource scope 覆盖目标聚合）。
- ``CommandHandler`` 两阶段：``plan`` 无副作用地产出领域事件 + accepted
  回执 + 可选外部副作用闭包；持久化顺序由 Command API 统一编排。
- expected_version 乐观并发在 Command API 调 ``PlatformStore.append_events``
  时统一校验，不经各 Handler 重复实现。
"""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import TYPE_CHECKING, Any, Awaitable, Callable, Optional, Protocol

from muteki.platform.contracts.capabilities import CapabilityBinding
from muteki.platform.contracts.commands import (
    ActorRef,
    CommandEnvelope,
    QueryEnvelope,
)
from muteki.platform.contracts.errors import ErrorCategory, ErrorEnvelope
from muteki.platform.contracts.receipts import CommandReceipt, ReceiptState

if TYPE_CHECKING:  # 避免与 store / gateway 的模块环
    from muteki.platform.contracts.events import EventEnvelope
    from muteki.platform.store import PlatformStore


#: operator / system 身份的 actor 拥有本地全权（单操作员产品默认）。
OPERATOR_ACTOR_KINDS = frozenset({"operator", "system"})


class CommandAPIError(RuntimeError):
    """Command API 的统一错误载体：附带 ErrorEnvelope，供各传输入口转换。"""

    def __init__(self, error: ErrorEnvelope) -> None:
        super().__init__(f"{error.code}: {error.message}")
        self.error = error


class PolicyDenied(CommandAPIError):
    """权限 / 资源范围判定拒绝（category=permission）。"""


class CommandFailed(CommandAPIError):
    """Handler 在 plan 阶段的业务失败（校验、not_found、状态机冲突等）。

    ``state`` 决定回执落为 FAILED 还是 CONFLICT。
    """

    def __init__(
        self, error: ErrorEnvelope, *, state: ReceiptState = ReceiptState.FAILED
    ) -> None:
        super().__init__(error)
        self.state = state


def make_error(
    code: str,
    message: str,
    category: ErrorCategory,
    *,
    correlation_id: Optional[str] = None,
    recovery_hint: str = "",
    retryable: bool = False,
) -> ErrorEnvelope:
    """构造统一错误 envelope（correlation id 贯穿 receipt/event/错误）。"""
    return ErrorEnvelope(
        code=code,
        message=message,
        category=category,
        recovery_hint=recovery_hint,
        correlation_id=correlation_id,
        retryable=retryable,
    )


def correlation_id_of(command: CommandEnvelope) -> str:
    """命令的 correlation id：payload 显式给出优先，否则用 command_id。"""
    text = str(command.payload.get("correlation_id") or "").strip()
    return text or command.command_id


class HandlerContext:
    """Handler 运行所需的共享依赖（判定已完成，Handler 不再重复授权）。"""

    def __init__(
        self,
        *,
        store: "PlatformStore",
        run_gateway: Any = None,
        principal: Optional[ActorRef] = None,
        binding: Optional[CapabilityBinding] = None,
        correlation_id: str = "",
        services: Optional[dict[str, Any]] = None,
    ) -> None:
        self.store = store
        self.run_gateway = run_gateway
        self.principal = principal or ActorRef()
        self.binding = binding
        self.correlation_id = correlation_id
        self.services = services or {}


@dataclass
class SideEffectResult:
    """外部副作用（提交后执行）的结果。"""

    #: 副作用结果事件（与接收事件同一聚合流）
    events: list["EventEnvelope"] = field(default_factory=list)
    #: 失败时的统一错误 envelope；非空则回执落为 ``state``
    error: Optional[ErrorEnvelope] = None
    state: ReceiptState = ReceiptState.COMPLETED
    #: 下游幂等命中（如控制 journal 的 command_id 去重）
    deduplicated: bool = False
    #: 需要随终态回执返回的结构化小结果。
    output: dict[str, Any] = field(default_factory=dict)


@dataclass
class CommandPlan:
    """Handler plan 阶段的产出：事件 + accepted 回执 + 可选副作用。"""

    #: 领域事件（至少一条；由 Command API 带 expected_version 追加）
    events: list["EventEnvelope"]
    #: accepted 状态的回执；event_cursor 由 Command API 在事件追加后盖章
    receipt: CommandReceipt
    #: 事务提交后执行的外部副作用（如经 RunGateway 进入 muteki/control）
    side_effect: Optional[Callable[[], Awaitable[SideEffectResult]]] = None
    #: 非空则为该副作用写 outbox 记录（幂等键 = command_id），投递后回写状态
    outbox_destination: str = ""


class CommandHandler(Protocol):
    """一个 command_type 命名空间的属主。"""

    command_types: set[str]

    async def plan(self, command: CommandEnvelope, ctx: HandlerContext) -> CommandPlan: ...


class QueryHandler(Protocol):
    query_types: set[str]

    async def handle(self, query: QueryEnvelope, ctx: HandlerContext) -> Any: ...


class HandlerRegistry:
    """command_type / query_type → handler 映射；重复注册命名空间即报错。"""

    def __init__(self) -> None:
        self._commands: dict[str, CommandHandler] = {}
        self._queries: dict[str, QueryHandler] = {}

    def register_command(self, handler: CommandHandler) -> None:
        for ct in handler.command_types:
            if ct in self._commands:
                raise CommandAPIError(make_error(
                    "command.handler.duplicate",
                    f"command_type {ct!r} already registered",
                    ErrorCategory.INTERNAL))
            self._commands[ct] = handler

    def register_query(self, handler: QueryHandler) -> None:
        for qt in handler.query_types:
            if qt in self._queries:
                raise CommandAPIError(make_error(
                    "query.handler.duplicate",
                    f"query_type {qt!r} already registered",
                    ErrorCategory.INTERNAL))
            self._queries[qt] = handler

    def command_handler(self, command_type: str) -> CommandHandler:
        handler = self._commands.get(command_type)
        if handler is None:
            raise CommandAPIError(make_error(
                "command.unsupported",
                f"no handler for command_type {command_type!r}",
                ErrorCategory.VALIDATION))
        return handler

    def query_handler(self, query_type: str) -> QueryHandler:
        handler = self._queries.get(query_type)
        if handler is None:
            raise CommandAPIError(make_error(
                "query.unsupported",
                f"no handler for query_type {query_type!r}",
                ErrorCategory.VALIDATION))
        return handler

    def known_command_types(self) -> set[str]:
        return set(self._commands)

    def known_query_types(self) -> set[str]:
        return set(self._queries)


def scope_covers(resource_scopes: list[str], aggregate_type: str, aggregate_id: str) -> bool:
    """resource scope 判定：条目格式 ``<type>:<id>``，``*`` 为通配。

    例如 ``run:*`` 覆盖所有 Run，``run:crun_x`` 只覆盖单个 Run，
    ``*:*`` 覆盖全部聚合。空清单不视为通配（由调用方决定默认语义）。
    """
    for entry in resource_scopes:
        text = str(entry or "").strip()
        if not text:
            continue
        if ":" in text:
            kind, _, selector = text.partition(":")
        else:
            kind, selector = text, "*"
        if kind not in (aggregate_type, "*"):
            continue
        if selector in ("", "*", aggregate_id):
            return True
    return False


class CommandPolicy:
    """Principal / CapabilityBinding / resource scope / command policy 的统一判定。

    CAP-01 的 AgentCapabilityGateway 复用本类；Binding 实体由 PlatformStore
    持久化，本类只做判定，不持有业务状态。
    """

    def is_operator(self, actor: ActorRef) -> bool:
        return actor.kind in OPERATOR_ACTOR_KINDS

    def resolve_binding(
        self, store: "PlatformStore", principal_id: str
    ) -> Optional[CapabilityBinding]:
        """取该 principal 最新版本的 CapabilityBinding（含已撤销，供判定拒绝）。"""
        bindings = store.list(CapabilityBinding, principal_id=principal_id)
        if not bindings:
            return None
        return max(bindings, key=lambda b: b.binding_version)

    # -- 命令 / 查询 / 事件读取 ----------------------------------------------

    def binding_for_command(
        self, store: "PlatformStore", command: CommandEnvelope
    ) -> Optional[CapabilityBinding]:
        """命令判定：返回通过校验的 binding（operator 为 None）。"""
        actor = command.actor
        if self.is_operator(actor):
            return None
        binding = self._require_binding(store, actor, command.command_id)
        from muteki.platform.capability_catalog import (
            DEFAULT_CATALOG,
            effective_tool_set,
        )
        effective_commands = DEFAULT_CATALOG.command_types_of(
            effective_tool_set(binding.mode, binding.tool_set)
        )
        if effective_commands and command.command_type not in effective_commands:
            raise PolicyDenied(make_error(
                "capability.command_not_allowed",
                f"command_type {command.command_type!r} not permitted for this binding",
                ErrorCategory.PERMISSION,
                correlation_id=correlation_id_of(command)))
        self._check_scope(
            binding, command.aggregate_type, command.aggregate_id,
            correlation_id=correlation_id_of(command))
        return binding

    def binding_for_query(
        self, store: "PlatformStore", query: QueryEnvelope
    ) -> Optional[CapabilityBinding]:
        actor = query.actor
        if self.is_operator(actor):
            return None
        binding = self._require_binding(store, actor, query.query_id)
        from muteki.platform.capability_catalog import (
            DEFAULT_CATALOG,
            effective_tool_set,
        )
        effective_queries = DEFAULT_CATALOG.query_types_of(
            effective_tool_set(binding.mode, binding.tool_set)
        )
        if effective_queries and query.query_type not in effective_queries:
            raise PolicyDenied(make_error(
                "capability.query_not_allowed",
                f"query_type {query.query_type!r} not permitted for this binding",
                ErrorCategory.PERMISSION,
                correlation_id=query.query_id))
        return binding

    def authorize_read(
        self,
        store: "PlatformStore",
        aggregate_type: str,
        aggregate_id: str,
        *,
        principal: Optional[ActorRef] = None,
    ) -> Optional[CapabilityBinding]:
        """事件读取 / wait 的判定。principal 为空视为内部/本地 operator 调用。"""
        if principal is None or self.is_operator(principal):
            return None
        binding = self._require_binding(store, principal, principal.id)
        self._check_scope(binding, aggregate_type, aggregate_id,
                          correlation_id=principal.id)
        return binding

    # -- 内部 -----------------------------------------------------------------

    def _require_binding(
        self, store: "PlatformStore", actor: ActorRef, correlation_id: str
    ) -> CapabilityBinding:
        if actor.binding_id:
            if actor.binding_version is not None:
                binding = store.get(
                    CapabilityBinding, actor.binding_id, actor.binding_version)
            else:
                versions = store.list(
                    CapabilityBinding, binding_id=actor.binding_id)
                binding = (
                    max(versions, key=lambda item: item.binding_version)
                    if versions else None
                )
        else:
            # 兼容内部旧调用；AgentCapabilityGateway 始终走上面的精确引用。
            binding = self.resolve_binding(store, actor.id)
        if binding is None:
            raise PolicyDenied(make_error(
                "capability.binding.required",
                (
                    f"capability binding {actor.binding_id!r} was not found"
                    if actor.binding_id else
                    f"principal {actor.kind}:{actor.id} has no capability binding"
                ),
                ErrorCategory.PERMISSION,
                correlation_id=correlation_id))
        if binding.revoked_at is not None:
            raise PolicyDenied(make_error(
                "capability.binding.revoked",
                "capability binding has been revoked",
                ErrorCategory.PERMISSION,
                correlation_id=correlation_id))
        if binding.principal_id and binding.principal_id != actor.id:
            raise PolicyDenied(make_error(
                "capability.binding.principal_mismatch",
                "binding principal does not match actor",
                ErrorCategory.PERMISSION,
                correlation_id=correlation_id))
        if actor.thread_id and binding.thread_id != actor.thread_id:
            raise PolicyDenied(make_error(
                "capability.binding.thread_mismatch",
                "binding thread does not match actor context",
                ErrorCategory.PERMISSION,
                correlation_id=correlation_id))
        return binding

    def _check_scope(
        self,
        binding: CapabilityBinding,
        aggregate_type: str,
        aggregate_id: str,
        *,
        correlation_id: str,
    ) -> None:
        # 空 scope 清单不视为通配：绑定仅覆盖自身 Thread 的判定留给 CAP-01 的
        # Binding 模板；这里放行，由 Handler 的资源归属校验兜底。
        if not binding.resource_scopes:
            return
        if scope_covers(binding.resource_scopes, aggregate_type, aggregate_id):
            return
        raise PolicyDenied(make_error(
            "capability.scope_denied",
            f"binding does not cover {aggregate_type}:{aggregate_id}",
            ErrorCategory.PERMISSION,
            correlation_id=correlation_id,
            recovery_hint="request a binding with a matching resource scope"))


__all__ = [
    "CommandAPIError",
    "CommandFailed",
    "CommandHandler",
    "CommandPlan",
    "CommandPolicy",
    "HandlerContext",
    "HandlerRegistry",
    "OPERATOR_ACTOR_KINDS",
    "PolicyDenied",
    "QueryHandler",
    "SideEffectResult",
    "correlation_id_of",
    "make_error",
    "scope_covers",
]
