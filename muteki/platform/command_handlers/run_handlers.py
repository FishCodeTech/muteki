"""Run 业务命令 Handler（任务书 6.1/6.2，COMMAND-01）。

拥有 ``run.*`` 命令命名空间，把每个命令映射到现有 Run 控制路径：
RunGateway（CORE-04）→ RunManager.post_control → ``muteki/control`` 持久控制
journal。这里不重建第二套暂停/恢复/Operator 指令状态机：command_id 幂等、
acceptance/effect receipt、执行代（generation）语义全部沿用控制层。

长任务（run.start 启动 Swarm、run.resolve 续做执行代）在接收事件与回执
持久化后立即执行副作用并返回；RunManager.start 本身只启动执行代协程即返回，
调用方经 get_receipt / snapshot / read_events / 有界 wait 继续追踪。

命令清单：
- ``run.create``：幂等绑定 Run（ensure_bound_run；binding key 冲突 → CONFLICT）
- ``run.start`` / ``run.resolve``：启动 / 续做执行代
- ``run.pause`` / ``run.resume`` / ``run.stop`` / ``run.freeze`` / ``run.thaw``
  / ``run.complete``：Run 控制
- ``run.operator_directive`` / ``run.hint`` / ``run.answer_decision``
  / ``run.add_context``：Operator 指令与人机协作
- ``run.spawn_worker`` / ``run.cancel_worker``：Worker 管理
"""

from __future__ import annotations

from typing import Any, Optional

from muteki.control import ControlAction

from muteki.platform.command_handlers.base import (
    CommandFailed,
    CommandPlan,
    HandlerContext,
    SideEffectResult,
    correlation_id_of,
    make_error,
)
from muteki.platform.contracts.errors import ErrorCategory, ErrorEnvelope
from muteki.platform.contracts.events import EventEnvelope
from muteki.platform.contracts.receipts import AggregateRef, CommandReceipt, ReceiptState
from muteki.platform.contracts.runs import BoundRunRequest, RunCommand
from muteki.solver.engine_registry import (
    DSH_DISABLED_REASON,
    ENGINE_TEMPORARILY_UNSUPPORTED_CODE,
    temporarily_disabled_engine_in_payload,
)

#: 事件生产者标识（builtin 领域模块命名风格）
PRODUCER = "builtin.single-security-task"

#: 对外稳定的 run.* 控制命令。Competition 的 redirect 属于
#: ``run_binding.redirect``，不能被通用 ``run.redirect`` 抢占命名空间。
_CONTROL_ACTIONS = {
    "run.pause": "pause",
    "run.resume": "resume",
    "run.stop": "stop",
    "run.freeze": "freeze",
    "run.thaw": "thaw",
    "run.complete": "complete",
    "run.hint": "hint",
    "run.answer_decision": "answer_decision",
    "run.add_context": "add_context",
    "run.spawn_worker": "spawn_worker",
    "run.cancel_worker": "cancel_worker",
    "run.operator_directive": "directive",
}

_MAINTENANCE_ACTIONS = {
    "run.rail.update": "rail.update",
    "run.rail.delete": "rail.delete",
    "run.folder.create": "folder.create",
    "run.folder.update": "folder.update",
    "run.folder.delete": "folder.delete",
    "run.archive": "archive",
    "run.purge": "purge",
    "run.open": "open",
    "run.artifact.attach": "artifact.attach",
}

#: 信封 payload 里属于控制通道本身的保留键，不向下传递给 driver/控制负载。
_RESERVED_PAYLOAD_KEYS = frozenset({
    "run_id", "binding_key", "task_id", "task_kind", "task_revision",
    "executor_id", "expected_generation", "correlation_id", "causation_id",
})


def _event(
    command_envelope: Any,
    event_type: str,
    aggregate_id: str,
    payload: dict[str, Any],
) -> EventEnvelope:
    """构造统一 envelope 的领域事件（correlation/causation 贯穿）。"""
    return EventEnvelope(
        aggregate_type="run",
        aggregate_id=aggregate_id,
        event_type=event_type,
        producer=PRODUCER,
        actor_id=command_envelope.actor.id or "system",
        command_id=command_envelope.command_id,
        causation_id=str(command_envelope.payload.get("causation_id") or "")
        or command_envelope.command_id,
        correlation_id=correlation_id_of(command_envelope),
        idempotency_key=command_envelope.idempotency_key,
        payload=payload,
    )


def _run_id_of(command_envelope: Any) -> str:
    run_id = str(
        command_envelope.payload.get("run_id") or command_envelope.aggregate_id or ""
    ).strip()
    return run_id


def _forward_payload(command_envelope: Any) -> dict[str, Any]:
    """剥掉保留键后透传给 driver / 控制负载。"""
    return {
        k: v for k, v in dict(command_envelope.payload).items()
        if k not in _RESERVED_PAYLOAD_KEYS
    }


def _require_gateway(ctx: HandlerContext) -> Any:
    gateway = ctx.run_gateway
    if gateway is None:
        raise CommandFailed(make_error(
            "run.gateway_unavailable",
            "run gateway is not configured for this Command API",
            ErrorCategory.INTERNAL))
    return gateway


def _require_run_id(command_envelope: Any, command_type: str) -> str:
    run_id = _run_id_of(command_envelope)
    if not run_id:
        raise CommandFailed(make_error(
            "run.id_required",
            f"{command_type} requires aggregate_id or payload.run_id",
            ErrorCategory.VALIDATION,
            correlation_id=correlation_id_of(command_envelope)))
    return run_id


class RunCommandHandler:
    """run.* 命令命名空间的属主。"""

    command_types = {
        "run.create",
        "run.start",
        "run.resolve",
        "run.control",
        *_CONTROL_ACTIONS.keys(),
        *_MAINTENANCE_ACTIONS.keys(),
    }

    async def plan(self, command: Any, ctx: HandlerContext) -> CommandPlan:
        if temporarily_disabled_engine_in_payload(command.payload):
            raise CommandFailed(make_error(
                ENGINE_TEMPORARILY_UNSUPPORTED_CODE,
                DSH_DISABLED_REASON,
                ErrorCategory.STATE,
                correlation_id=correlation_id_of(command),
            ))
        gateway = _require_gateway(ctx)
        ct = command.command_type
        if ct == "run.create":
            return await self._plan_create(gateway, command)
        if ct in ("run.start", "run.resolve"):
            return await self._plan_lifecycle(gateway, command, ct)
        if ct in _MAINTENANCE_ACTIONS:
            return await self._plan_maintenance(gateway, command, ct)
        return await self._plan_control(gateway, command, ct)

    @staticmethod
    async def _ensure_run_handle(gateway: Any, command: Any, run_id: str) -> None:
        """校验 Run 存在并打开句柄（重启后 bound Run 不在内存 rail 里，
        snapshot 会从持久绑定索引恢复；未知 Run 翻译为 not_found）。"""
        try:
            await gateway.snapshot(run_id)
        except LookupError as exc:
            raise CommandFailed(make_error(
                "run.not_found", str(exc), ErrorCategory.NOT_FOUND,
                correlation_id=correlation_id_of(command))) from exc

    # -- run.create ----------------------------------------------------------

    async def _plan_create(self, gateway: Any, command: Any) -> CommandPlan:
        payload = dict(command.payload)
        if bool(payload.get("legacy")):
            ref = await gateway.ensure_legacy_run(
                str(payload.get("run_id") or command.aggregate_id or "").strip())
            return CommandPlan(
                events=[_event(command, "run.created", ref.run_id, {
                    "run_id": ref.run_id,
                    "legacy": True,
                })],
                receipt=CommandReceipt(
                    command_id=command.command_id,
                    state=ReceiptState.ACCEPTED,
                    run_id=ref.run_id,
                    aggregate=AggregateRef(type="run", id=ref.run_id),
                ),
            )
        binding_key = str(payload.get("binding_key") or command.aggregate_id or "").strip()
        if not binding_key:
            raise CommandFailed(make_error(
                "run.binding_key_required",
                "run.create requires payload.binding_key",
                ErrorCategory.VALIDATION,
                correlation_id=correlation_id_of(command)))
        request = BoundRunRequest(
            binding_key=binding_key,
            task_id=(str(payload["task_id"]).strip() or None)
            if payload.get("task_id") is not None else None,
            task_kind=str(payload.get("task_kind") or ""),
            task_revision=int(payload.get("task_revision") or 1),
            run_id=(str(payload["run_id"]).strip() or None)
            if payload.get("run_id") is not None else None,
            executor_id=(str(payload["executor_id"]).strip() or None)
            if payload.get("executor_id") is not None else None,
        )
        try:
            ref = await gateway.ensure_bound_run(request)
        except Exception as exc:
            # CORE-04 的 BoundRunConflictError 携带 typed ErrorEnvelope；
            # 其余异常按内部错误归类。懒导入避免 platform → apps.web 模块环。
            error = getattr(exc, "error", None)
            if isinstance(error, ErrorEnvelope):
                raise CommandFailed(
                    error.model_copy(update={
                        "correlation_id": correlation_id_of(command)}),
                    state=ReceiptState.CONFLICT
                    if error.category is ErrorCategory.CONFLICT
                    else ReceiptState.FAILED) from exc
            raise
        receipt = CommandReceipt(
            command_id=command.command_id,
            state=ReceiptState.ACCEPTED,
            run_id=ref.run_id,
            aggregate=AggregateRef(type="run", id=ref.run_id),
        )
        # ensure_bound_run 本身幂等（相同 binding key 永远返回同一 Run），
        # 计划阶段调用安全；不再有其他外部副作用。
        return CommandPlan(
            events=[_event(command, "run.created", ref.run_id, {
                "run_id": ref.run_id,
                "binding_key": ref.binding_key,
                "task_id": ref.task_id,
                "task_kind": request.task_kind,
                "task_revision": request.task_revision,
                "executor_id": ref.executor_id,
            })],
            receipt=receipt,
        )

    # -- run.start / run.resolve ----------------------------------------------

    async def _plan_lifecycle(self, gateway: Any, command: Any, ct: str) -> CommandPlan:
        run_id = _require_run_id(command, ct)
        await self._ensure_run_handle(gateway, command, run_id)
        action = ct.rsplit(".", 1)[1]  # start | resolve
        forward = _forward_payload(command)
        receipt = CommandReceipt(
            command_id=command.command_id,
            state=ReceiptState.ACCEPTED,
            run_id=run_id,
            aggregate=AggregateRef(type="run", id=run_id),
        )

        async def _deliver() -> SideEffectResult:
            run_command = RunCommand(
                command_type=action,
                command_id=command.command_id,
                actor_id=command.actor.id,
                payload=forward,
            )
            gw_receipt = await gateway.command(run_id, run_command)
            return _side_effect_result(command, ct, run_id, gw_receipt)

        return CommandPlan(
            events=[_event(command, "run.command.accepted", run_id, {
                "command_type": ct, "run_id": run_id})],
            receipt=receipt,
            side_effect=_deliver,
            outbox_destination="run.gateway",
        )

    # -- 控制命令（pause/resume/stop/directive/worker 等） ----------------------

    async def _plan_control(self, gateway: Any, command: Any, ct: str) -> CommandPlan:
        run_id = _require_run_id(command, ct)
        await self._ensure_run_handle(gateway, command, run_id)
        if ct == "run.control":
            action = str(command.payload.get("control_action") or "").strip()
            if action not in {item.value for item in ControlAction}:
                raise CommandFailed(make_error(
                    "run.control.action_invalid",
                    f"unsupported control action {action!r}",
                    ErrorCategory.VALIDATION,
                    correlation_id=correlation_id_of(command)))
        else:
            action = _CONTROL_ACTIONS[ct]
        forward = _forward_payload(command)
        forward.pop("control_action", None)
        expected_generation = command.payload.get("expected_generation")
        receipt = CommandReceipt(
            command_id=command.command_id,
            state=ReceiptState.ACCEPTED,
            run_id=run_id,
            aggregate=AggregateRef(type="run", id=run_id),
        )

        async def _deliver() -> SideEffectResult:
            run_command = RunCommand(
                command_type=action,
                command_id=command.command_id,
                actor_id=command.actor.id,
                expected_generation=(
                    int(expected_generation)
                    if expected_generation is not None else None),
                payload=forward,
            )
            gw_receipt = await gateway.command(run_id, run_command)
            return _side_effect_result(command, ct, run_id, gw_receipt)

        return CommandPlan(
            events=[_event(command, "run.command.accepted", run_id, {
                "command_type": ct, "action": action, "run_id": run_id})],
            receipt=receipt,
            side_effect=_deliver,
            outbox_destination="run.gateway",
        )

    async def _plan_maintenance(
        self, gateway: Any, command: Any, ct: str
    ) -> CommandPlan:
        """把旧版 rail/folder/归档操作纳入统一命令、回执和 outbox。"""
        aggregate_id = _require_run_id(command, ct)
        operation = _MAINTENANCE_ACTIONS[ct]
        forward = _forward_payload(command)
        receipt = CommandReceipt(
            command_id=command.command_id,
            state=ReceiptState.ACCEPTED,
            run_id=(aggregate_id if not operation.startswith("folder.") else None),
            aggregate=AggregateRef(type="run", id=aggregate_id),
        )

        async def _deliver() -> SideEffectResult:
            try:
                output = await gateway.mutate_legacy(
                    operation, aggregate_id, forward)
            except Exception as exc:
                error = getattr(exc, "error", None)
                if not isinstance(error, ErrorEnvelope):
                    raise
                error = error.model_copy(update={
                    "correlation_id": error.correlation_id
                    or correlation_id_of(command)})
                return SideEffectResult(
                    events=[_event(command, "run.operation.failed", aggregate_id, {
                        "command_type": ct,
                        "operation": operation,
                        "error": error.model_dump(mode="json"),
                    })],
                    error=error,
                    state=(ReceiptState.CONFLICT
                           if error.category in {
                               ErrorCategory.CONFLICT, ErrorCategory.STATE}
                           else ReceiptState.FAILED),
                )
            return SideEffectResult(
                events=[_event(command, "run.operation.completed", aggregate_id, {
                    "command_type": ct,
                    "operation": operation,
                })],
                state=ReceiptState.COMPLETED,
                output=output,
            )

        return CommandPlan(
            events=[_event(command, "run.operation.accepted", aggregate_id, {
                "command_type": ct,
                "operation": operation,
            })],
            receipt=receipt,
            side_effect=_deliver,
            outbox_destination="run.gateway",
        )


def _side_effect_result(
    command: Any, ct: str, run_id: str, gw_receipt: CommandReceipt
) -> SideEffectResult:
    """把 RunGateway 的 acceptance receipt 归一化为副作用结果。

    acceptance 语义：控制 journal 的持久接收即接受；终态 effect 由调用方经
    control_receipt / Run 事件流继续追踪（CommandReceipt.next 已声明）。
    """
    if gw_receipt.state is ReceiptState.ACCEPTED:
        return SideEffectResult(
            events=[_event(command, "run.command.completed", run_id, {
                "command_type": ct,
                "run_id": run_id,
                "acceptance_state": "accepted",
                "control_deduplicated": gw_receipt.deduplicated,
            })],
            state=ReceiptState.COMPLETED,
            deduplicated=gw_receipt.deduplicated,
            output=dict(gw_receipt.output),
        )
    error = gw_receipt.error or make_error(
        "run.command.rejected", "run command rejected", ErrorCategory.STATE)
    error = error.model_copy(update={
        "correlation_id": error.correlation_id or correlation_id_of(command)})
    state = (ReceiptState.CONFLICT
             if gw_receipt.state is ReceiptState.CONFLICT else ReceiptState.FAILED)
    return SideEffectResult(
        events=[_event(command, "run.command.failed", run_id, {
            "command_type": ct,
            "run_id": run_id,
            "error": {
                "code": error.code,
                "message": error.message,
                "category": error.category.value,
            },
        })],
        error=error,
        state=state,
        deduplicated=gw_receipt.deduplicated,
    )


class RunQueryHandler:
    """Run 只读查询：历史清单与单个快照。"""

    query_types = {"run.list", "run.snapshot"}

    async def handle(self, query: Any, ctx: HandlerContext) -> Any:
        from muteki.platform.contracts.commands import QueryResult

        gateway = _require_gateway(ctx)
        if query.query_type == "run.list":
            params = dict(query.params or {})
            include_archived = params.get("include_archived", True)
            if isinstance(include_archived, str):
                include_archived = include_archived.strip().lower() not in {
                    "0", "false", "no", "off",
                }
            rows = await gateway.list_runs(
                include_archived=bool(include_archived),
            )
            category = str(params.get("category") or "").strip()
            status = str(params.get("status") or "").strip()
            search = str(params.get("query") or "").strip().casefold()
            solved = params.get("solved")
            if isinstance(solved, str):
                normalized = solved.strip().lower()
                solved = normalized in {"1", "true", "yes", "on"}
            if category:
                rows = [row for row in rows if str(row.get("category")) == category]
            if status:
                rows = [row for row in rows if str(row.get("status")) == status]
            if isinstance(solved, bool):
                rows = [row for row in rows if bool(row.get("solved")) is solved]
            if search:
                rows = [
                    row for row in rows
                    if search in " ".join((
                        str(row.get("run_id") or ""),
                        str(row.get("name") or ""),
                        str(row.get("category") or ""),
                    )).casefold()
                ]
            try:
                limit = min(2000, max(1, int(params.get("limit") or 500)))
            except (TypeError, ValueError):
                limit = 500
            total = len(rows)
            from .pagination import page_items
            rows, continuation = page_items(rows, params, limit, ctx, query.query_type,
                                             lambda row: str(row.get("run_id") or ""))
            return QueryResult(
                query_id=query.query_id,
                query_type=query.query_type,
                result={
                    "runs": rows,
                    "total": total,
                    "returned": len(rows),
                    **continuation,
                },
            )

        run_id = str(
            query.params.get("run_id") or query.aggregate_id or "").strip()
        if not run_id:
            raise CommandFailed(make_error(
                "run.id_required",
                "run.snapshot requires params.run_id",
                ErrorCategory.VALIDATION,
                correlation_id=query.query_id))
        try:
            snapshot = await gateway.snapshot(run_id)
        except LookupError as exc:
            raise CommandFailed(make_error(
                "run.not_found", str(exc), ErrorCategory.NOT_FOUND,
                correlation_id=query.query_id)) from exc
        return QueryResult(
            query_id=query.query_id,
            query_type=query.query_type,
            result=snapshot.model_dump(mode="json"),
        )


__all__ = ["PRODUCER", "RunCommandHandler", "RunQueryHandler"]
