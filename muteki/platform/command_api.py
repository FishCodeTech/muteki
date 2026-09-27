"""Muteki Command API 实现（任务书 6.1/6.2，COMMAND-01）。

``MutekiCommandApiImpl`` 实现 ``contracts.protocols.MutekiCommandAPI``，是
Web、AgentCapabilityGateway、领域模块和兼容 CLI 的统一应用入口。权限
（Principal / CapabilityBinding / resource scope）、幂等、回执、事件、
外部副作用投递与恢复只在这里和 ``command_handlers`` 实现一次。

dispatch 固定顺序（任务书 6.1）：

1. 权限判定 → 2. 幂等预检（command_id / idempotency_key 去重返回同一
   receipt，内容不一致回 CONFLICT）→ 3. Handler plan 产生领域事件与
   accepted 回执 → 4. 持久化回执 → 5. 带 expected_version 追加事件 →
   6. 执行外部副作用（写 outbox，经 RunGateway 进入 muteki/control）→
   7. 写副作用结果事件并落终态回执。

崩溃恢复：步骤 4–6 之间崩溃会留下 accepted 回执，由 CORE-02 的
``Reconciler``（pending_receipts / outbox.pending）列出给调用方回放；
Run 侧副作用自身经控制 journal 的 command_id 幂等去重。
"""

from __future__ import annotations

import asyncio
import logging
import time
from typing import Any, Optional

from muteki.platform.command_handlers.base import (
    CommandAPIError,
    CommandFailed,
    CommandHandler,
    CommandPolicy,
    HandlerContext,
    HandlerRegistry,
    PolicyDenied,
    QueryHandler,
    correlation_id_of,
    make_error,
)
from muteki.platform.command_handlers.cursor import (
    CursorError,
    assert_cursor_scope,
    encode_cursor,
    load_cursor_key,
)
from muteki.platform.contracts.capabilities import CapabilityBinding
from muteki.platform.contracts.commands import (
    ActorRef,
    CommandEnvelope,
    EventPage,
    EventReadRequest,
    QueryEnvelope,
    QueryResult,
    WaitRequest,
    WaitResult,
)
from muteki.platform.contracts.errors import ErrorCategory
from muteki.platform.contracts.receipts import (
    AggregateRef,
    CommandReceipt,
    OutboxRecord,
    ReceiptState,
)
from muteki.platform.outbox import OutboxManager
from muteki.platform.store import (
    IdempotencyConflictError,
    OptimisticConcurrencyError,
    PlatformStore,
)

LOG = logging.getLogger(__name__)

#: 有界 wait 的服务端上限：到达上限必须返回 timeout + 最新 cursor，
#: 不能持续等待整个 Swarm / 同步 / 提交链结束（任务书 6.2）。
DEFAULT_MAX_WAIT_SECONDS = 30.0
WAIT_POLL_INTERVAL_SECONDS = 0.1
#: read_events / wait 单页事件数上限。
MAX_PAGE_LIMIT = 500


class MutekiCommandApiImpl:
    """``MutekiCommandAPI`` Protocol 的默认实现。"""

    def __init__(
        self,
        store: PlatformStore,
        *,
        run_gateway: Any = None,
        registry: Optional[HandlerRegistry] = None,
        policy: Optional[CommandPolicy] = None,
        cursor_key: Optional[bytes] = None,
        max_wait_seconds: float = DEFAULT_MAX_WAIT_SECONDS,
        services: Optional[dict[str, Any]] = None,
    ) -> None:
        self._store = store
        self._run_gateway = run_gateway
        self._registry = registry or HandlerRegistry()
        self._policy = policy or CommandPolicy()
        # 游标签名密钥：默认持久化在 platform.db 旁，重启后历史游标仍有效。
        self._cursor_key = cursor_key or load_cursor_key(store.db_path)
        self._max_wait_seconds = float(max_wait_seconds)
        self._services = dict(services or {})
        self._outbox = OutboxManager(store)

    @classmethod
    def with_builtin_handlers(
        cls, store: PlatformStore, *, run_gateway: Any = None, **kwargs: Any
    ) -> "MutekiCommandApiImpl":
        """构造并注册内置 run.*/task.* Handler 的 Command API。"""
        from muteki.platform.command_handlers import register_builtin_handlers

        api = cls(store, run_gateway=run_gateway, **kwargs)
        register_builtin_handlers(api)
        return api

    # -- 注册 -----------------------------------------------------------------

    def register_command(self, handler: CommandHandler) -> None:
        self._registry.register_command(handler)

    def register_query(self, handler: QueryHandler) -> None:
        self._registry.register_query(handler)

    @property
    def handlers(self) -> HandlerRegistry:
        return self._registry

    # -- 内部工具 ---------------------------------------------------------------

    def _ctx(
        self,
        actor: ActorRef,
        binding: Optional[CapabilityBinding],
        correlation_id: str,
    ) -> HandlerContext:
        return HandlerContext(
            store=self._store,
            run_gateway=self._run_gateway,
            principal=actor,
            binding=binding,
            correlation_id=correlation_id,
            services=self._services,
        )

    @staticmethod
    def _scope_of(
        actor: ActorRef, binding: Optional[CapabilityBinding]
    ) -> str:
        """游标绑定的授权范围标识：binding 优先，其次 principal。"""
        if binding is not None:
            return f"binding:{binding.binding_id}"
        return f"{actor.kind}:{actor.id}"

    def _cursor(self, aggregate_type: str, aggregate_id: str, seq: int,
                scope: str) -> str:
        return encode_cursor(
            self._cursor_key, aggregate_type, aggregate_id, seq, scope=scope)

    def _transient_receipt(
        self,
        command: CommandEnvelope,
        state: ReceiptState,
        error: Any,
    ) -> CommandReceipt:
        """未持久化的即时回执（权限拒绝 / 幂等内容冲突：命令未被接受）。"""
        return CommandReceipt(
            command_id=command.command_id,
            state=state,
            aggregate=AggregateRef(
                type=command.aggregate_type, id=command.aggregate_id),
            error=error,
            next=["receipt"],
        )

    # -- dispatch ---------------------------------------------------------------

    async def dispatch(self, command: CommandEnvelope) -> CommandReceipt:
        correlation_id = correlation_id_of(command)

        # 1. 权限判定（唯一实现处：CommandPolicy）
        try:
            binding = self._policy.binding_for_command(self._store, command)
        except PolicyDenied as exc:
            return self._transient_receipt(command, ReceiptState.FAILED, exc.error)

        # 2. 幂等预检：同一 command_id / idempotency_key 返回同一 receipt
        try:
            prior = self._store.check_idempotency(command)
        except IdempotencyConflictError as exc:
            return self._transient_receipt(
                command, ReceiptState.CONFLICT,
                make_error("command.idempotency_conflict", str(exc),
                           ErrorCategory.CONFLICT, correlation_id=correlation_id,
                           recovery_hint="use a new command_id for changed content"))
        if prior is not None:
            return prior

        # 3. Handler plan（无副作用地产生事件与 accepted 回执）
        try:
            handler = self._registry.command_handler(command.command_type)
        except CommandAPIError as exc:
            return self._transient_receipt(command, ReceiptState.FAILED, exc.error)
        try:
            plan = await handler.plan(
                command, self._ctx(command.actor, binding, correlation_id))
        except CommandFailed as exc:
            # 业务终态（校验 / not_found / 冲突）：持久化，重试返回同一回执。
            return self._persist_terminal(command, exc.state, exc.error)
        except Exception as exc:  # Handler 内部错误，保持边界可见
            LOG.exception("command plan failed: %s", command.command_type)
            return self._persist_terminal(
                command, ReceiptState.FAILED,
                make_error("command.plan_failed", f"{type(exc).__name__}: {exc}",
                           ErrorCategory.INTERNAL, correlation_id=correlation_id))

        # 4. 持久化 accepted 回执（并发竞态下的第二次去重）
        stored = self._store.record_command(command, plan.receipt)
        if stored.deduplicated:
            return stored

        # 5. 带 expected_version 追加领域事件（乐观并发统一判定）
        try:
            events = self._store.append_events(
                plan.events, expected_version=command.expected_version)
        except OptimisticConcurrencyError as exc:
            error = make_error(
                "command.version_conflict", str(exc), ErrorCategory.CONFLICT,
                correlation_id=correlation_id,
                recovery_hint="refresh snapshot and retry with the current version")
            return self._store.update_receipt_state(
                command.command_id, ReceiptState.CONFLICT, error=error)

        scope = self._scope_of(command.actor, binding)
        last = events[-1]
        cursor = self._cursor(
            last.aggregate_type, last.aggregate_id, last.stream_seq, scope)
        run_id = plan.receipt.run_id

        # 6. 事务提交后的外部副作用（可写 outbox 供重启恢复）
        if plan.side_effect is None:
            return self._store.update_receipt_state(
                command.command_id, ReceiptState.COMPLETED,
                event_cursor=cursor, run_id=run_id)
        outbox_record: Optional[OutboxRecord] = None
        if plan.outbox_destination:
            outbox_record, _ = self._outbox.enqueue(OutboxRecord(
                command_id=command.command_id,
                event_id=last.event_id,
                aggregate_type=last.aggregate_type,
                aggregate_id=last.aggregate_id,
                event_type=last.event_type,
                destination=plan.outbox_destination,
            ))
        try:
            result = await plan.side_effect()
        except Exception as exc:
            LOG.exception("command side effect failed: %s", command.command_type)
            if outbox_record is not None:
                self._outbox.mark_failed(outbox_record.outbox_id, str(exc))
            error = make_error(
                "command.side_effect_failed", f"{type(exc).__name__}: {exc}",
                ErrorCategory.INTERNAL, correlation_id=correlation_id,
                retryable=True)
            return self._store.update_receipt_state(
                command.command_id, ReceiptState.FAILED, error=error,
                event_cursor=cursor, run_id=run_id)

        # 7. 副作用结果事件 + 终态回执
        if result.events:
            self._store.append_events(result.events)
        if outbox_record is not None:
            if result.error is None:
                self._outbox.mark_delivered(outbox_record.outbox_id)
            else:
                self._outbox.mark_failed(
                    outbox_record.outbox_id, result.error.message)
        return self._store.update_receipt_state(
            command.command_id, result.state, error=result.error,
            event_cursor=cursor, run_id=run_id, output=result.output)

    def _persist_terminal(
        self, command: CommandEnvelope, state: ReceiptState, error: Any
    ) -> CommandReceipt:
        """持久化业务终态回执；内容一致的重试将经幂等预检返回同一回执。"""
        receipt = CommandReceipt(
            command_id=command.command_id,
            state=state,
            run_id=str(command.payload.get("run_id") or "").strip() or None,
            aggregate=AggregateRef(
                type=command.aggregate_type, id=command.aggregate_id),
            error=error,
        )
        try:
            return self._store.record_command(command, receipt)
        except IdempotencyConflictError:
            return self._transient_receipt(
                command, ReceiptState.CONFLICT,
                make_error("command.idempotency_conflict",
                           "command replayed with different content",
                           ErrorCategory.CONFLICT,
                           correlation_id=correlation_id_of(command)))

    # -- get_receipt --------------------------------------------------------------

    async def get_receipt(self, command_id: str) -> CommandReceipt:
        receipt = self._store.get_receipt(command_id)
        if receipt is None:
            raise CommandAPIError(make_error(
                "command.receipt_not_found",
                f"no receipt for command {command_id!r}",
                ErrorCategory.NOT_FOUND,
                correlation_id=command_id))
        return receipt

    # -- query --------------------------------------------------------------------

    async def query(self, query: QueryEnvelope) -> QueryResult:
        binding = self._policy.binding_for_query(self._store, query)
        handler = self._registry.query_handler(query.query_type)
        return await handler.handle(
            query, self._ctx(query.actor, binding, query.query_id))

    # -- read_events / wait ---------------------------------------------------------

    def _authorize_stream(
        self,
        aggregate_type: str,
        aggregate_id: str,
        after_cursor: Optional[str],
        principal: Optional[ActorRef],
    ) -> tuple[int, str]:
        """事件流读取的统一判定：权限 + 游标 scope 绑定。返回 (after_seq, scope)。"""
        binding = self._policy.authorize_read(
            self._store, aggregate_type, aggregate_id, principal=principal)
        scope = (self._scope_of(principal, binding)
                 if principal is not None else "")
        try:
            after = assert_cursor_scope(
                self._cursor_key, after_cursor, aggregate_type, aggregate_id,
                scope=scope)
        except CursorError as exc:
            message = str(exc)
            if "bound to" in message:
                raise CommandAPIError(make_error(
                    "event.cursor_scope", message, ErrorCategory.PERMISSION,
                    recovery_hint="read each aggregate with its own cursor")) from exc
            raise CommandAPIError(make_error(
                "event.cursor_invalid", message, ErrorCategory.VALIDATION)) from exc
        return after, scope

    @staticmethod
    def _clamp_limit(limit: int) -> int:
        return max(1, min(int(limit), MAX_PAGE_LIMIT))

    async def _require_stream_aggregate(
        self, aggregate_type: str, aggregate_id: str
    ) -> None:
        """确认事件流对应的领域对象真实存在。

        Run 不是 ``PlatformStore`` 的普通对象表；它由 ``RunGateway`` 持有，
        因此必须通过同一个运行入口确认。没有 RunGateway 的纯事件存储场景
        则只接受已经写入过事件的聚合。
        """
        if aggregate_type != "run":
            return
        if self._run_gateway is not None:
            try:
                await self._run_gateway.snapshot(aggregate_id)
            except LookupError as exc:
                raise CommandAPIError(make_error(
                    "run.not_found",
                    str(exc),
                    ErrorCategory.NOT_FOUND,
                )) from exc
            return
        if self._store.stream_head(aggregate_type, aggregate_id) <= 0:
            raise CommandAPIError(make_error(
                "run.not_found",
                f"unknown run {aggregate_id!r}",
                ErrorCategory.NOT_FOUND,
            ))

    async def read_events(
        self, request: EventReadRequest, *, principal: Optional[ActorRef] = None
    ) -> EventPage:
        after, scope = self._authorize_stream(
            request.aggregate_type, request.aggregate_id,
            request.after_cursor, principal)
        await self._require_stream_aggregate(
            request.aggregate_type, request.aggregate_id)
        limit = self._clamp_limit(request.limit)
        # 多读一条判断是否还有后续；next_cursor 仅在还有后续事件时返回。
        rows = self._store.read_events(
            request.aggregate_type, request.aggregate_id,
            after_seq=after, limit=limit + 1)
        has_more = len(rows) > limit
        page = rows[:limit]
        next_cursor = None
        if has_more and page:
            last = page[-1]
            next_cursor = self._cursor(
                last.aggregate_type, last.aggregate_id, last.stream_seq, scope)
        return EventPage(events=page, next_cursor=next_cursor)

    async def wait(
        self, request: WaitRequest, *, principal: Optional[ActorRef] = None
    ) -> WaitResult:
        after, scope = self._authorize_stream(
            request.aggregate_type, request.aggregate_id,
            request.after_cursor, principal)
        await self._require_stream_aggregate(
            request.aggregate_type, request.aggregate_id)
        limit = self._clamp_limit(request.limit)
        timeout = max(0.0, min(float(request.timeout_seconds),
                               self._max_wait_seconds))
        deadline = time.monotonic() + timeout
        while True:
            rows = self._store.read_events(
                request.aggregate_type, request.aggregate_id,
                after_seq=after, limit=limit)
            if rows:
                last = rows[-1]
                return WaitResult(
                    state="events",
                    events=rows,
                    cursor=self._cursor(
                        last.aggregate_type, last.aggregate_id,
                        last.stream_seq, scope),
                )
            if time.monotonic() >= deadline:
                # 有界超时：返回最新 cursor，不持续等待整个任务结束。
                head = self._store.stream_head(
                    request.aggregate_type, request.aggregate_id)
                cursor = (
                    self._cursor(request.aggregate_type, request.aggregate_id,
                                 head, scope)
                    if head > 0 else request.after_cursor)
                return WaitResult(state="timeout", cursor=cursor)
            await asyncio.sleep(WAIT_POLL_INTERVAL_SECONDS)


__all__ = [
    "DEFAULT_MAX_WAIT_SECONDS",
    "MAX_PAGE_LIMIT",
    "MutekiCommandApiImpl",
]
