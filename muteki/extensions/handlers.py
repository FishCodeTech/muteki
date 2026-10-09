"""扩展生命周期与业务命令的 MutekiCommandAPI Handler（任务书 6.1、11.2/11.3，EXT-01）。

命令命名空间 ``extension.*``：

- 生命周期：extension.install / enable / disable / upgrade / rollback /
  uninstall —— plan 只做参数校验并产生 accepted 事件，真实安装与子进程操作
  放在提交后的副作用闭包里（模式与 run_handlers 一致），结果事件写
  core.extension.<op>.completed / .failed；
- 业务调用：extension.command —— 经 ExtensionService.invoke 转发到扩展
  子进程的 command/handle；
- 事件提案：extension.propose_event —— 扩展经 event/propose 回到宿主后由
  宿主以 system 身份 dispatch；命名空间权限（ext.<id>.* 且落在 events_write
  声明内）与扩展声明的 event schema 在 plan 阶段校验，不通过即 FAILED。
  这是扩展写 Domain Event 的唯一入口：扩展不能直接写核心投影。

注册入口：``register_extension_handlers(api, service)``，同时把
``service.proposal_handler`` 接到 Command API。
"""

from __future__ import annotations

from typing import Any

from muteki.extensions.host import ExtensionProcessError
from muteki.extensions.installer import Source
from muteki.extensions.manifest import (
    SchemaValidationError,
    validate_against_schema,
)
from muteki.extensions.permissions import PermissionChecker, PermissionDenied
from muteki.extensions.protocol import (
    ExtensionRpcError,
    ExtensionUnavailable,
    ProtocolError,
)
from muteki.extensions.registry import (
    ExtensionError,
    ExtensionService,
    ExtensionState,
)
from muteki.platform.command_handlers.base import (
    CommandFailed,
    CommandPlan,
    HandlerContext,
    SideEffectResult,
    correlation_id_of,
    make_error,
)
from muteki.platform.contracts.commands import ActorRef
from muteki.platform.contracts.errors import ErrorCategory
from muteki.platform.contracts.events import EventEnvelope
from muteki.platform.contracts.receipts import (
    AggregateRef,
    CommandReceipt,
    ReceiptState,
)

#: 查询路径上 ExtensionError.code → 错误分类（未列出的按状态错误处理）。
_QUERY_ERROR_CATEGORIES = {
    "extension.not_found": ErrorCategory.NOT_FOUND,
    "extension.not_running": ErrorCategory.STATE,
}

#: JSON-RPC 标准错误码中属于请求本身有误的部分（method / params）。
_RPC_REQUEST_ERROR_CODES = frozenset({-32601, -32602})

#: 生命周期事件生产者（内置领域模块命名风格）。
PRODUCER = "builtin.extensions"

#: 安装命令的聚合流（安装前还不知道扩展 id）。
INSTALL_AGGREGATE_ID = "registry"


def _event(
    command: Any,
    event_type: str,
    aggregate_id: str,
    payload: dict[str, Any],
) -> EventEnvelope:
    """构造 core.extension.* 生命周期事件（correlation/causation 贯穿）。"""
    return EventEnvelope(
        aggregate_type="extension",
        aggregate_id=aggregate_id,
        event_type=event_type,
        producer=PRODUCER,
        actor_id=command.actor.id or "system",
        command_id=command.command_id,
        causation_id=str(command.payload.get("causation_id") or "")
        or command.command_id,
        correlation_id=correlation_id_of(command),
        idempotency_key=command.idempotency_key,
        payload=payload,
    )


def _source_of(payload: dict[str, Any]) -> Source:
    raw = payload.get("source")
    if not isinstance(raw, dict):
        raise CommandFailed(make_error(
            "extension.source_required",
            "payload.source must be an object describing the package source",
            ErrorCategory.VALIDATION))
    return Source(
        kind=str(raw.get("kind") or "").strip(),
        path=str(raw.get("path") or ""),
        url=str(raw.get("url") or ""),
        ref=str(raw.get("ref") or ""),
        sha256=str(raw.get("sha256") or ""),
        catalog_root=str(raw.get("catalog_root") or ""),
        extension_id=str(raw.get("extension_id") or ""),
        version=str(raw.get("version") or ""),
    )


def _extension_id_of(command: Any) -> str:
    ext_id = str(
        command.payload.get("extension_id") or command.aggregate_id or ""
    ).strip()
    if not ext_id:
        raise CommandFailed(make_error(
            "extension.id_required",
            f"{command.command_type} requires payload.extension_id",
            ErrorCategory.VALIDATION,
            correlation_id=correlation_id_of(command)))
    return ext_id


def _failure(command: Any, op: str, ext_id: str, exc: Exception) -> SideEffectResult:
    """把服务层异常归一化为 failed 副作用结果（含结果事件）。"""
    code = getattr(exc, "code", None)
    # ExtensionRpcError 的 code 是 JSON-RPC 整数错误码；ErrorEnvelope.code
    # 只接受字符串，统一归一到 extension.op_failed，原始码留在 message 里。
    if not isinstance(code, str) or not code:
        code = "extension.op_failed"
    message = getattr(exc, "message", None) or str(exc)
    category = (ErrorCategory.PERMISSION if isinstance(exc, PermissionDenied)
                else ErrorCategory.INTERNAL)
    error = make_error(code, message, category,
                       correlation_id=correlation_id_of(command))
    return SideEffectResult(
        events=[_event(command, f"core.extension.{op}.failed", ext_id, {
            "error": {"code": code, "message": message},
        })],
        error=error,
        state=ReceiptState.FAILED,
    )


class ExtensionCommandHandler:
    """extension.* 命令命名空间的属主。"""

    command_types = {
        "extension.install",
        "extension.enable",
        "extension.disable",
        "extension.upgrade",
        "extension.rollback",
        "extension.uninstall",
        "extension.command",
        "extension.propose_event",
    }

    def __init__(self, service: ExtensionService) -> None:
        self._service = service

    async def plan(self, command: Any, ctx: HandlerContext) -> CommandPlan:
        ct = command.command_type
        if ct == "extension.propose_event":
            return self._plan_propose_event(command)
        if ct == "extension.install":
            return self._plan_install(command)
        ext_id = _extension_id_of(command)
        op = ct.rsplit(".", 1)[1]  # enable | disable | upgrade | rollback | uninstall | command
        receipt = CommandReceipt(
            command_id=command.command_id,
            state=ReceiptState.ACCEPTED,
            aggregate=AggregateRef(type="extension", id=ext_id),
        )
        accepted = _event(command, f"core.extension.{op}.accepted", ext_id, {
            "extension_id": ext_id,
            "command_type": ct,
        })

        async def _apply() -> SideEffectResult:
            try:
                detail = await self._apply_op(op, ext_id, command)
            except (ExtensionError, PermissionDenied, SchemaValidationError,
                    ValueError) as exc:
                return _failure(command, op, ext_id, exc)
            except Exception as exc:  # 子进程层的意外错误，保持边界可见
                return _failure(command, op, ext_id, exc)
            return SideEffectResult(
                events=[_event(command, f"core.extension.{op}.completed",
                               ext_id, detail)],
                state=ReceiptState.COMPLETED,
            )

        return CommandPlan(events=[accepted], receipt=receipt,
                           side_effect=_apply)

    # -- 生命周期副作用 ---------------------------------------------------------

    async def _apply_op(
        self, op: str, ext_id: str, command: Any
    ) -> dict[str, Any]:
        service = self._service
        payload = dict(command.payload)
        if op == "enable":
            record = await service.enable(
                ext_id,
                version=(str(payload["version"]) if payload.get("version")
                         else None),
                config=(dict(payload["config"]) if payload.get("config")
                        is not None else None),
            )
            return {"extension_id": ext_id, "active_version": record.active_version,
                    "state": record.state.value}
        if op == "disable":
            record = await service.disable(ext_id)
            return {"extension_id": ext_id, "state": record.state.value}
        if op == "upgrade":
            record = await service.upgrade(
                ext_id, _source_of(payload),
                config=(dict(payload["config"]) if payload.get("config")
                        is not None else None),
                preview_id=str(payload.get("preview_id") or ""),
                confirmations=list(payload.get("confirmations") or []),
            )
            return {"extension_id": ext_id, "active_version": record.active_version,
                    "state": record.state.value}
        if op == "rollback":
            record = await service.rollback(
                ext_id,
                version=(str(payload["version"]) if payload.get("version")
                         else None),
            )
            return {"extension_id": ext_id, "active_version": record.active_version,
                    "state": record.state.value}
        if op == "uninstall":
            await service.uninstall(
                ext_id,
                preserve_state=bool(payload.get("preserve_state", True)),
                preserve_logs=bool(payload.get("preserve_logs", True)),
                preserve_artifacts=bool(payload.get("preserve_artifacts", True)),
            )
            receipts = service.uninstall_receipts(ext_id)
            return {"extension_id": ext_id, "state": "uninstalled",
                    "retention": receipts[-1] if receipts else {}}
        if op == "command":
            result = await service.invoke(
                ext_id,
                str(payload.get("command_type") or ""),
                payload.get("params") if isinstance(payload.get("params"), dict)
                else {},
            )
            return {"extension_id": ext_id, "result": result}
        raise ExtensionError("extension.unsupported_op", f"unsupported op: {op}")

    # -- install ----------------------------------------------------------------

    def _plan_install(self, command: Any) -> CommandPlan:
        source = _source_of(dict(command.payload))
        if not source.kind:
            raise CommandFailed(make_error(
                "extension.source_required",
                "payload.source.kind is required "
                "(local-dir | archive | git | http | catalog)",
                ErrorCategory.VALIDATION,
                correlation_id=correlation_id_of(command)))
        receipt = CommandReceipt(
            command_id=command.command_id,
            state=ReceiptState.ACCEPTED,
            aggregate=AggregateRef(type="extension", id=INSTALL_AGGREGATE_ID),
        )
        accepted = _event(command, "core.extension.install.accepted",
                          INSTALL_AGGREGATE_ID, {
                              "source_kind": source.kind,
                              "catalog_id": source.extension_id,
                          })

        async def _apply() -> SideEffectResult:
            try:
                record = self._service.install(
                    source,
                    preview_id=str(command.payload.get("preview_id") or ""),
                    confirmations=list(command.payload.get("confirmations") or []),
                )
            except Exception as exc:
                return _failure(command, "install", INSTALL_AGGREGATE_ID, exc)
            return SideEffectResult(
                events=[_event(command, "core.extension.install.completed",
                               record.extension_id, {
                                   "extension_id": record.extension_id,
                                   "versions": record.installed_versions,
                               })],
                state=ReceiptState.COMPLETED,
            )

        return CommandPlan(events=[accepted], receipt=receipt,
                           side_effect=_apply)

    # -- 事件提案（扩展写事件的唯一入口） -------------------------------------------

    def _plan_propose_event(self, command: Any) -> CommandPlan:
        ext_id = _extension_id_of(command)
        event_type = str(command.payload.get("event_type") or "").strip()
        payload = command.payload.get("payload")
        payload = payload if isinstance(payload, dict) else {}
        aggregate_type = str(
            command.payload.get("aggregate_type") or "extension").strip()
        aggregate_id = str(
            command.payload.get("aggregate_id") or ext_id).strip()
        try:
            record = self._service._require_record(ext_id)
            manifest = self._service.manifest_of(ext_id)
        except ExtensionError as exc:
            raise CommandFailed(make_error(
                exc.code, exc.message, ErrorCategory.NOT_FOUND,
                correlation_id=correlation_id_of(command))) from exc
        if record.state is ExtensionState.DISABLED:
            raise CommandFailed(make_error(
                "extension.disabled",
                f"{ext_id} is disabled; event proposals are rejected",
                ErrorCategory.STATE,
                correlation_id=correlation_id_of(command)))
        checker = PermissionChecker(manifest)
        try:
            checker.check_event_type(event_type)
        except PermissionDenied as exc:
            raise CommandFailed(make_error(
                exc.code, exc.message, ErrorCategory.PERMISSION,
                correlation_id=correlation_id_of(command))) from exc
        schema = (record.capabilities.get("event_schemas") or {}).get(event_type)
        if isinstance(schema, dict):
            try:
                validate_against_schema(
                    payload, schema, field=f"event:{event_type}")
            except SchemaValidationError as exc:
                raise CommandFailed(make_error(
                    "extension.event_schema_invalid", str(exc),
                    ErrorCategory.VALIDATION,
                    correlation_id=correlation_id_of(command))) from exc
        receipt = CommandReceipt(
            command_id=command.command_id,
            state=ReceiptState.ACCEPTED,
            aggregate=AggregateRef(type=aggregate_type, id=aggregate_id),
        )
        event = EventEnvelope(
            aggregate_type=aggregate_type,
            aggregate_id=aggregate_id,
            event_type=event_type,
            producer=f"ext.{ext_id}",
            actor_id=command.actor.id or f"ext-host:{ext_id}",
            command_id=command.command_id,
            causation_id=str(command.payload.get("causation_id") or "")
            or command.command_id,
            correlation_id=correlation_id_of(command),
            payload=payload,
        )
        return CommandPlan(events=[event], receipt=receipt)


class ExtensionQueryHandler:
    """扩展只读查询：list / get / health / projection / logs。"""

    query_types = {
        "extension.list",
        "extension.get",
        "extension.health",
        "extension.projection",
        "extension.logs",
    }

    def __init__(self, service: ExtensionService) -> None:
        self._service = service

    async def handle(self, query: Any, ctx: HandlerContext) -> Any:
        from muteki.platform.contracts.commands import QueryResult

        result: Any
        qt = query.query_type
        params = dict(query.params)
        ext_id = str(params.get("extension_id") or query.aggregate_id or "").strip()
        if qt == "extension.list":
            result = [r.model_dump(mode="json") for r in self._service.list_records()]
        elif not ext_id:
            raise CommandFailed(make_error(
                "extension.id_required",
                f"{qt} requires params.extension_id",
                ErrorCategory.VALIDATION, correlation_id=query.query_id))
        else:
            try:
                result = await self._read(qt, ext_id, params)
            except ExtensionError as exc:
                raise CommandFailed(make_error(
                    exc.code, exc.message, _QUERY_ERROR_CATEGORIES.get(
                        exc.code, ErrorCategory.STATE),
                    correlation_id=query.query_id)) from exc
            except ExtensionRpcError as exc:
                # The extension rejected or failed the read; report its own
                # message instead of surfacing an unhandled 500.
                raise CommandFailed(make_error(
                    "extension.rpc_error",
                    f"{ext_id} {qt}: {exc.message}",
                    ErrorCategory.VALIDATION
                    if exc.code in _RPC_REQUEST_ERROR_CODES
                    else ErrorCategory.RUNTIME,
                    correlation_id=query.query_id)) from exc
            except (ExtensionProcessError, ExtensionUnavailable, ProtocolError) as exc:
                raise CommandFailed(make_error(
                    "extension.process_unavailable", f"{ext_id} {qt}: {exc}",
                    ErrorCategory.RUNTIME, correlation_id=query.query_id,
                    retryable=True)) from exc
        return QueryResult(query_id=query.query_id, query_type=qt, result=result)

    async def _read(self, qt: str, ext_id: str, params: dict[str, Any]) -> Any:
        if qt == "extension.get":
            record = self._service.get_record(ext_id)
            return record.model_dump(mode="json") if record is not None else None
        if qt == "extension.health":
            return await self._service.health(ext_id)
        if qt == "extension.projection":
            return await self._service.read_projection(
                ext_id, str(params.get("name") or ""))
        return {"lines": self._service.logs(
            ext_id, limit=int(params.get("limit") or 200))}


def register_extension_handlers(api: Any, service: ExtensionService) -> None:
    """把扩展 Handler 注册进 MutekiCommandAPI，并接好 event/propose 回调。

    扩展子进程的事件提案由宿主以 system 身份重新 dispatch
    ``extension.propose_event``：权限与 schema 校验在 Handler plan 阶段
    再执行一次（宿主不信任子进程自称已校验）。
    """
    api.register_command(ExtensionCommandHandler(service))
    api.register_query(ExtensionQueryHandler(service))

    async def _proposal(extension_id: str, event_type: str,
                        payload: dict[str, Any]) -> dict[str, Any]:
        from muteki.platform.contracts.commands import CommandEnvelope

        receipt = await api.dispatch(CommandEnvelope(
            command_type="extension.propose_event",
            aggregate_type="extension",
            aggregate_id=extension_id,
            actor=ActorRef(kind="system", id=f"ext-host:{extension_id}"),
            payload={
                "extension_id": extension_id,
                "event_type": event_type,
                "payload": payload,
            },
        ))
        if receipt.state is not ReceiptState.COMPLETED:
            error = receipt.error
            raise ExtensionError(
                (error.code if error else "extension.proposal_rejected"),
                (error.message if error else "event proposal rejected"),
            )
        return {
            "accepted": True,
            "command_id": receipt.command_id,
            "event_cursor": receipt.event_cursor,
        }

    service.proposal_handler = _proposal


__all__ = [
    "PRODUCER",
    "ExtensionCommandHandler",
    "ExtensionQueryHandler",
    "register_extension_handlers",
]
