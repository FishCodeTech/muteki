"""DomainModule 启停命令 Handler。"""

from __future__ import annotations

from typing import Any

from muteki.platform.command_handlers.base import (
    CommandFailed,
    CommandPlan,
    HandlerContext,
    SideEffectResult,
    correlation_id_of,
    make_error,
)
from muteki.platform.contracts.errors import ErrorCategory
from muteki.platform.contracts.events import EventEnvelope
from muteki.platform.contracts.receipts import AggregateRef, CommandReceipt, ReceiptState


class DomainModuleCommandHandler:
    command_types = {"module.enable", "module.disable"}

    def __init__(self, registry: Any) -> None:
        self._registry = registry

    async def plan(self, command: Any, ctx: HandlerContext) -> CommandPlan:
        module_id = str(
            command.payload.get("module_id") or command.aggregate_id or ""
        ).strip()
        record = self._registry.get(module_id)
        if record is None:
            raise CommandFailed(make_error(
                "module.not_found",
                f"unknown module {module_id!r}",
                ErrorCategory.NOT_FOUND,
                correlation_id=correlation_id_of(command),
            ))
        enabled = command.command_type == "module.enable"

        async def _apply() -> SideEffectResult:
            ctx.store.set_domain_module_enabled(module_id, enabled)
            if enabled:
                self._registry.enable(module_id)
            else:
                self._registry.disable(module_id)
            return SideEffectResult()

        return CommandPlan(
            events=[EventEnvelope(
                aggregate_type="module",
                aggregate_id=module_id,
                event_type=f"core.module.{'enabled' if enabled else 'disabled'}",
                producer="platform.registry",
                actor_id=command.actor.id or "system",
                command_id=command.command_id,
                causation_id=command.command_id,
                correlation_id=correlation_id_of(command),
                idempotency_key=command.idempotency_key,
                payload={"module_id": module_id, "enabled": enabled},
            )],
            receipt=CommandReceipt(
                command_id=command.command_id,
                state=ReceiptState.ACCEPTED,
                aggregate=AggregateRef(type="module", id=module_id),
            ),
            side_effect=_apply,
        )


def register_domain_module_handlers(api: Any, registry: Any) -> None:
    if "module.enable" not in api.handlers.known_command_types():
        api.register_command(DomainModuleCommandHandler(registry))


__all__ = ["DomainModuleCommandHandler", "register_domain_module_handlers"]
