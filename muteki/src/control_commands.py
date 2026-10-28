from __future__ import annotations

from typing import Any

from muteki.platform.command_handlers.base import (
    CommandFailed, CommandPlan, HandlerContext, SideEffectResult,
    correlation_id_of,
)
from muteki.platform.contracts.commands import QueryResult
from muteki.platform.contracts.events import EventEnvelope
from muteki.platform.contracts.receipts import ReceiptState

from .contracts import ExecutionRequest
from .policy import TOOL_CAPABILITIES, evaluate
from .store import SrcStoreError

SRC_CONTROL_COMMAND_TYPES = {
    "src.policy.update", "src.approval.decide", "src.grant.revoke",
}
SRC_CONTROL_QUERY_TYPES = {
    "src.policy.current", "src.policy.preview", "src.execution.receipt",
    "src.execution.capabilities",
}


class SrcControlCommandHandler:
    command_types = SRC_CONTROL_COMMAND_TYPES

    def __init__(self, service: Any) -> None:
        self.service = service

    def _task(self, command: Any, ctx: HandlerContext) -> str:
        try:
            if ctx.principal.kind not in {"operator", "system"}:
                raise SrcStoreError("policy_denied", "Agent cannot modify the SRC control plane")
            task_id = "" if command.aggregate_type == "thread" else command.aggregate_id
            return self.service.resolve_actor(ctx, task_id).task_id
        except SrcStoreError as exc:
            from .commands import _error
            raise _error(exc, correlation_id_of(command)) from exc

    def _capabilities(self, task_id: str, *, activation: bool) -> dict[str, Any]:
        import inspect
        method = self.service.execution.environment_capabilities
        parameters = inspect.signature(method).parameters
        kwargs: dict[str, Any] = {}
        if "task_id" in parameters:
            kwargs["task_id"] = task_id
        if "for_activation" in parameters:
            kwargs["for_activation"] = activation
        return method(**kwargs)

    async def plan(self, command: Any, ctx: HandlerContext) -> CommandPlan:
        task_id = self._task(command, ctx)
        if command.command_type == "src.policy.update":
            self.service.require_active(task_id)
        event_type = f"{command.command_type}.requested"

        async def apply() -> SideEffectResult:
            try:
                if command.command_type == "src.policy.update":
                    result = self.service.execution.revise_policy(
                        task_id, command.payload, actor_id=command.actor.id,
                        environment_capabilities=self._capabilities(task_id, activation=True),
                    ).model_dump(mode="json")
                elif command.command_type == "src.approval.decide":
                    result = self.service.execution.decide_approval(
                        task_id, str(command.payload.get("approval_id") or ""),
                        str(command.payload.get("decision") or ""), actor_id=command.actor.id,
                        base_revision=int(command.payload.get("base_revision") or 0),
                        scope=command.payload.get("scope"),
                    ).model_dump(mode="json")
                elif command.command_type == "src.grant.revoke":
                    grant_id = str(command.payload.get("grant_id") or "")
                    self.service.execution.revoke_grant(
                        task_id, grant_id, base_revision=int(command.payload.get("base_revision") or 0))
                    result = {"grant_id": grant_id, "revoked": True}
                else:
                    raise SrcStoreError("command.unsupported", command.command_type)
                return SideEffectResult(output=result)
            except SrcStoreError as exc:
                from .commands import _error
                failure = _error(exc, correlation_id_of(command))
                return SideEffectResult(error=failure.error, state=failure.state)

        from .commands import PRODUCER, _receipt
        return CommandPlan(
            events=[EventEnvelope(
                aggregate_type="task", aggregate_id=task_id, event_type=event_type,
                producer=PRODUCER, actor_id=command.actor.id,
                command_id=command.command_id, correlation_id=correlation_id_of(command),
                idempotency_key=command.idempotency_key, payload=dict(command.payload),
            )],
            receipt=_receipt(command, task_id), side_effect=apply,
            outbox_destination="src.control",
        )


class SrcControlQueryHandler:
    query_types = SRC_CONTROL_QUERY_TYPES

    def __init__(self, service: Any) -> None:
        self.service = service

    async def handle(self, query: Any, ctx: HandlerContext) -> QueryResult:
        try:
            task_id = str(query.params.get("task_id") or "")
            if not task_id and query.aggregate_type != "thread":
                task_id = str(query.aggregate_id or "")
            task_id = self.service.resolve_actor(ctx, task_id).task_id
            if query.query_type == "src.policy.current":
                result = self.service.execution.current_policy(task_id).model_dump(mode="json")
            elif query.query_type == "src.execution.capabilities":
                import inspect
                method = self.service.execution.environment_capabilities
                parameters = inspect.signature(method).parameters
                kwargs = {"task_id": task_id} if "task_id" in parameters else {}
                result = method(**kwargs)
            elif query.query_type == "src.execution.receipt":
                invocation_id = str(query.params.get("invocation_id") or "")
                result = self.service.store.require(
                    task_id, "execution_receipt", invocation_id).model_dump(mode="json")
            elif query.query_type == "src.policy.preview":
                policy = self.service.execution.current_policy(task_id)
                raw_request = dict(query.params.get("request") or {})
                request = ExecutionRequest.model_validate({
                    **raw_request, "task_id": task_id, "thread_id": self.service.binding(task_id).thread_id,
                    "policy_version": policy.version,
                    "arguments_hash": raw_request.get("arguments_hash") or self.service.store.digest(raw_request.get("arguments") or {}),
                })
                result = evaluate(policy, request)
            else:
                raise SrcStoreError("query.unsupported", query.query_type)
            return QueryResult(query_id=query.query_id, query_type=query.query_type, result=result)
        except SrcStoreError as exc:
            from .commands import _error
            raise _error(exc, query.query_id) from exc


__all__ = ["SrcControlCommandHandler", "SrcControlQueryHandler", "SRC_CONTROL_COMMAND_TYPES", "SRC_CONTROL_QUERY_TYPES"]
