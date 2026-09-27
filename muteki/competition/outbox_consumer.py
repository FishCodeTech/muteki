"""比赛外部效果的持久 outbox consumer。"""

from __future__ import annotations

import asyncio
import random
from datetime import timedelta
from typing import Any, Optional

from muteki.competition import events as ev
from muteki.competition.models import (
    BindingState,
    ConnectionStatus,
    PlatformConnection,
    RunBinding,
)
from muteki.competition.platforms.base import (
    PlatformAuthRequiredError,
    PlatformRateLimitedError,
    PlatformTransportError,
)
from muteki.competition.store import CompetitionStore
from muteki.platform.command_handlers.base import make_error
from muteki.platform.contracts.base import utcnow
from muteki.platform.contracts.errors import ErrorCategory
from muteki.platform.contracts.receipts import (
    AggregateRef,
    EffectReceipt,
    EffectState,
    OutboxRecord,
    OutboxStatus,
    ReceiptState,
)
from muteki.platform.contracts.runs import RunCommand


class CompetitionOutboxConsumer:
    """消费非提交 outbox，并同步命令回执、效果回执和领域事件。"""

    def __init__(
        self,
        store: CompetitionStore,
        *,
        adapter_factory: Any,
        sync_service: Any,
        lease_manager: Any,
        binding_service: Any,
        run_gateway: Any,
        effect_store: Any,
        max_attempts: int = 8,
        random_source: Optional[random.Random] = None,
    ) -> None:
        self.store = store
        self.adapters = adapter_factory
        self.sync_service = sync_service
        self.leases = lease_manager
        self.bindings = binding_service
        self.run_gateway = run_gateway
        self.effect_store = effect_store
        self.max_attempts = max(1, int(max_attempts))
        self.random = random_source or random.Random()

    def _effect(self, record: OutboxRecord) -> Optional[EffectReceipt]:
        if not record.command_id or self.effect_store is None:
            return None
        prior = self.effect_store.effect_for_outbox(record.outbox_id)
        if prior is not None:
            return prior
        receipt = self.store.get_receipt(record.command_id)
        if receipt is None:
            return None
        command = self.store.get_command(record.command_id)
        correlation_id = record.command_id
        if command is not None:
            correlation_id = str(
                command.payload.get("correlation_id") or command.command_id)
        effect = self.effect_store.save_effect_receipt(EffectReceipt(
            command_id=record.command_id,
            acceptance_receipt_id=receipt.receipt_id,
            domain="competition",
            state=EffectState.WAITING,
            aggregate=AggregateRef(
                type=record.aggregate_type, id=record.aggregate_id),
            correlation_id=correlation_id,
            outbox_id=record.outbox_id,
            destination=record.destination,
            idempotency_key=self.store.outbox.idempotency_key(record.outbox_id),
            attempts=record.attempts,
            max_attempts=self.max_attempts,
            object_links={
                "aggregate": f"{record.aggregate_type}:{record.aggregate_id}",
            },
        ))
        ids = [item.effect_id for item in self.effect_store.effects_for_command(
            record.command_id)]
        self.store.update_receipt_state(
            record.command_id, ReceiptState.WAITING, effect_ids=ids)
        return effect

    def _event(
        self, record: OutboxRecord, event_type: str, payload: dict[str, Any]
    ) -> None:
        command = (
            self.store.get_command(record.command_id)
            if record.command_id else None
        )
        competition_id = str(record.payload.get("competition_id") or "")
        self.store.append_events([ev.make_event(
            competition_id=competition_id,
            aggregate_type="effect",
            aggregate_id=(
                (self.effect_store.effect_for_outbox(record.outbox_id).effect_id)
                if self.effect_store is not None
                and self.effect_store.effect_for_outbox(record.outbox_id)
                else record.outbox_id
            ),
            event_type=event_type,
            command=command,
            payload={
                "outbox_id": record.outbox_id,
                "destination": record.destination,
                "aggregate_type": record.aggregate_type,
                "aggregate_id": record.aggregate_id,
                **payload,
            },
        )])

    def _sync_command(self, command_id: str, *, error: Any = None) -> None:
        records = self.store.outbox.for_command(command_id)
        if not records:
            return
        statuses = {record.status for record in records}
        current = self.store.get_receipt(command_id)
        if current is None:
            return
        output = {
            **dict(current.output or {}),
            "outbox_ids": [record.outbox_id for record in records],
            "effect_ids": [
                item.effect_id
                for item in self.effect_store.effects_for_command(command_id)
            ] if self.effect_store is not None else [],
        }
        if statuses == {OutboxStatus.DELIVERED}:
            state = ReceiptState.COMPLETED
            error = None
        elif statuses and statuses <= {
            OutboxStatus.DELIVERED, OutboxStatus.CANCELLED,
        } and OutboxStatus.CANCELLED in statuses:
            state = ReceiptState.CANCELLED
            error = None
        elif OutboxStatus.DEAD_LETTER in statuses:
            state = ReceiptState.FAILED
        elif OutboxStatus.PROCESSING in statuses:
            state = ReceiptState.RUNNING
        else:
            state = ReceiptState.WAITING
        output["effect_state"] = state.value
        self.store.update_receipt_state(
            command_id, state, error=error, output=output,
            effect_ids=output["effect_ids"],
        )

    async def run_once(self, *, limit: int = 100) -> int:
        """处理一批到期记录；submit 由 SubmissionService 的专用泵处理。"""
        handled = 0
        records = self.store.outbox.pending(limit=limit)
        for pending in records:
            if pending.payload.get("op") == "submit":
                continue
            claimed = self.store.outbox.mark_processing(pending.outbox_id)
            if claimed.status is not OutboxStatus.PROCESSING:
                continue
            handled += 1
            effect = self._effect(claimed)
            if effect is not None:
                self.effect_store.update_effect_receipt(
                    effect.effect_id, EffectState.RUNNING,
                    attempts=claimed.attempts,
                )
            if claimed.command_id:
                self._sync_command(claimed.command_id)
            self._event(
                claimed, "competition.effect.running",
                {"attempt": claimed.attempts},
            )
            try:
                output = await self._deliver(claimed)
            except Exception as exc:
                self._failed(claimed, exc, effect)
                continue
            delivered = self.store.outbox.mark_delivered(claimed.outbox_id)
            if effect is not None:
                self.effect_store.update_effect_receipt(
                    effect.effect_id, EffectState.COMPLETED,
                    attempts=delivered.attempts,
                    output=output,
                )
            self._event(
                delivered, "competition.effect.completed",
                {"attempt": delivered.attempts, "output": output},
            )
            if delivered.command_id:
                self._sync_command(delivered.command_id)
        return handled

    async def _deliver(self, record: OutboxRecord) -> dict[str, Any]:
        op = str(record.payload.get("op") or "")
        if op == "probe_connection":
            connection = self.store.get(
                PlatformConnection, str(record.payload["connection_id"]))
            if connection is None:
                raise LookupError("platform connection missing")
            caps = await self.adapters.probe(connection)
            self.store.save(connection.model_copy(update={
                "capabilities": caps.model_dump(mode="json"),
                "status": ConnectionStatus.ACTIVE.value,
                "last_error": "",
            }))
            return {"connection_id": connection.connection_id, "healthy": True}
        if op == "sync":
            competition_id = str(record.payload["competition_id"])
            connection = self.store.get(
                PlatformConnection, str(record.payload["connection_id"]))
            if connection is None:
                raise LookupError("platform connection missing")
            report = await self.sync_service.sync(
                competition_id,
                self.adapters.for_connection(connection),
                command=(
                    self.store.get_command(record.command_id)
                    if record.command_id else None
                ),
            )
            return report.model_dump(mode="json")
        if op == "ensure_instance":
            lease = await self.leases.provision_requested(
                str(record.payload["lease_id"]))
            return {
                "lease_id": lease.lease_id,
                "state": lease.state,
                "address": lease.address,
                "fencing_token": lease.fencing_token,
            }
        if op == "stop_instance":
            lease = await self.leases.release(
                str(record.payload["lease_id"]), reason="command")
            return {"lease_id": lease.lease_id, "state": lease.state}
        if op in {
            "run_binding.redirect", "run_binding.pause", "run_binding.resolve",
        }:
            binding = self.store.get(
                RunBinding, str(record.payload["binding_id"]))
            if binding is None:
                raise LookupError("run binding missing")
            command_type = "pause" if op.endswith("pause") else "resolve"
            snapshot = await self.run_gateway.snapshot(binding.run_id)
            if int(snapshot.generation) != int(binding.execution_generation):
                raise RuntimeError(
                    "stale run binding: "
                    f"binding generation {binding.execution_generation}, "
                    f"run generation {snapshot.generation}"
                )
            expected_control_generation = (
                int((snapshot.detail or {}).get("control_generation") or 0)
                if command_type == "pause"
                else None
            )
            receipt = await self.run_gateway.command(binding.run_id, RunCommand(
                command_type=command_type,
                command_id=f"{record.outbox_id}:{command_type}",
                expected_generation=expected_control_generation,
            ))
            if receipt.error is not None:
                raise RuntimeError(receipt.error.message)
            if command_type == "resolve":
                refreshed = self.store.get(RunBinding, binding.binding_id) or binding
                if refreshed.state == BindingState.RESOLVING.value:
                    self.store.save(refreshed.model_copy(update={
                        "state": BindingState.ACTIVE.value,
                        "execution_generation": (
                            refreshed.execution_generation + 1),
                    }))
            return {
                "run_id": binding.run_id,
                "run_command_id": receipt.command_id,
                "state": receipt.state.value,
            }
        raise ValueError(f"unsupported competition outbox operation: {op!r}")

    def _failed(
        self,
        record: OutboxRecord,
        exc: Exception,
        effect: Optional[EffectReceipt],
    ) -> None:
        retryable = isinstance(exc, PlatformTransportError) and exc.retryable
        auth = isinstance(exc, PlatformAuthRequiredError)
        attempts = record.attempts
        retry_after = getattr(exc, "retry_after_seconds", None)
        if retry_after is None:
            base = min(300.0, 0.5 * (2 ** max(0, attempts - 1)))
            retry_after = base + self.random.uniform(0, base * 0.2)
        terminal = (not retryable and not auth) or attempts >= self.max_attempts
        if auth and not terminal:
            failed = self.store.outbox.mark_paused(record.outbox_id, str(exc))
            state = EffectState.WAITING
            retry_after = None
        elif terminal:
            failed = self.store.outbox.mark_dead_letter(record.outbox_id, str(exc))
            state = EffectState.DEAD_LETTER
        else:
            failed = self.store.outbox.mark_failed(
                record.outbox_id, str(exc),
                retry_delay_seconds=float(retry_after),
                increment_attempt=False,
            )
            state = EffectState.WAITING
        category = (
            exc.error_category
            if isinstance(exc, PlatformTransportError)
            else ErrorCategory.INTERNAL
        )
        platform_category = getattr(exc, "category", None)
        category_code = str(
            getattr(platform_category, "value", platform_category) or "failed")
        error = make_error(
            f"competition.effect.{category_code}",
            f"{type(exc).__name__}: {exc}",
            category,
            correlation_id=record.command_id or record.outbox_id,
            retryable=not terminal,
            recovery_hint=(
                "reauthorize the platform connection"
                if auth else "retry the effect after the recorded retry window"
                if not terminal else "inspect the effect and retry manually"
            ),
        )
        if auth:
            connection_id = str(record.payload.get("connection_id") or "")
            connection = self.store.get(PlatformConnection, connection_id)
            if connection is not None:
                self.store.save(connection.model_copy(update={
                    "status": ConnectionStatus.AUTH_REQUIRED.value,
                    "last_error": str(exc),
                }))
        if effect is not None:
            self.effect_store.update_effect_receipt(
                effect.effect_id, state,
                attempts=failed.attempts,
                retry_after=(
                    utcnow() + timedelta(seconds=float(retry_after))
                    if not terminal and retry_after is not None else None
                ),
                error=error,
            )
        self._event(
            failed,
            "competition.effect.dead_letter"
            if terminal else "competition.effect.authentication_paused"
            if auth else "competition.effect.retry_scheduled",
            {
                "attempt": failed.attempts,
                "error_code": error.code,
                "retry_after_seconds": None if terminal or auth else retry_after,
            },
        )
        if failed.command_id:
            self._sync_command(failed.command_id, error=error)

    def synchronize_submission_effects(self) -> int:
        """把专用 SubmissionService 已投递的 outbox 状态同步到效果回执。"""
        count = 0
        rows = self.store.conn.execute(
            "SELECT payload FROM competition_outbox WHERE destination LIKE 'platform.%' "
            "AND command_id IS NOT NULL ORDER BY created_at"
        ).fetchall()
        for row in rows:
            record = OutboxRecord.model_validate_json(row["payload"])
            if record.payload.get("op") != "submit":
                continue
            effect = self._effect(record)
            if effect is None:
                continue
            if record.status is OutboxStatus.DELIVERED:
                if effect.state is not EffectState.COMPLETED:
                    self.effect_store.update_effect_receipt(
                        effect.effect_id, EffectState.COMPLETED,
                        attempts=record.attempts,
                    )
                    count += 1
            elif record.status in {
                OutboxStatus.PENDING, OutboxStatus.FAILED, OutboxStatus.PAUSED,
            }:
                self.effect_store.update_effect_receipt(
                    effect.effect_id, EffectState.WAITING,
                    attempts=record.attempts,
                )
            if record.command_id:
                self._sync_command(record.command_id)
        return count


__all__ = ["CompetitionOutboxConsumer"]
