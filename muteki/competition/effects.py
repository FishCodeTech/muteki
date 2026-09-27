"""比赛外部效果的人工重试与取消命令。"""

from __future__ import annotations

from typing import Any

from muteki.competition.models import PlatformSubmission, SubmissionState
from muteki.competition.store import CompetitionStore
from muteki.platform.command_handlers.base import (
    CommandFailed,
    CommandPlan,
    HandlerContext,
    SideEffectResult,
    correlation_id_of,
    make_error,
)
from muteki.platform.contracts.commands import CommandEnvelope
from muteki.platform.contracts.errors import ErrorCategory
from muteki.platform.contracts.events import EventEnvelope
from muteki.platform.contracts.receipts import (
    AggregateRef,
    CommandReceipt,
    EffectState,
    OutboxStatus,
    ReceiptState,
)


COMMAND_TYPES = {"effect.retry", "effect.cancel"}


class CompetitionEffectControlHandler:
    """对 competition outbox 的可恢复阶段执行人工操作。"""

    command_types = COMMAND_TYPES

    def __init__(
        self,
        platform_store: Any,
        competition_store: CompetitionStore,
        *,
        submission_service: Any = None,
    ) -> None:
        self.platform_store = platform_store
        self.competition_store = competition_store
        self.submissions = submission_service

    async def plan(
        self, command: CommandEnvelope, ctx: HandlerContext
    ) -> CommandPlan:
        effect_id = str(
            command.payload.get("effect_id") or command.aggregate_id or ""
        ).strip()
        effect = self.platform_store.get_effect_receipt(effect_id)
        if effect is None or effect.domain != "competition" or not effect.outbox_id:
            raise CommandFailed(make_error(
                "effect.not_found",
                f"competition effect not found: {effect_id}",
                ErrorCategory.NOT_FOUND,
                correlation_id=correlation_id_of(command),
            ))
        record = self.competition_store.outbox.get(effect.outbox_id)
        if record is None:
            raise CommandFailed(make_error(
                "effect.outbox_missing",
                f"outbox record missing: {effect.outbox_id}",
                ErrorCategory.STATE,
                correlation_id=correlation_id_of(command),
            ))

        if command.command_type == "effect.retry":
            if record.status in {
                OutboxStatus.PROCESSING,
                OutboxStatus.DELIVERED,
                OutboxStatus.CANCELLED,
            }:
                raise self._conflict(
                    command, "effect.retry_not_allowed",
                    f"effect is {record.status.value}; remote delivery may have started",
                    "inspect the remote result before creating another effect",
                )
            target_state = EffectState.WAITING
            event_type = "competition.effect.manual_retry_requested"
        else:
            if not bool(command.payload.get("confirm")):
                raise CommandFailed(make_error(
                    "effect.cancel_confirmation_required",
                    "effect.cancel requires payload.confirm=true",
                    ErrorCategory.VALIDATION,
                    correlation_id=correlation_id_of(command),
                ))
            if record.status in {
                OutboxStatus.PROCESSING, OutboxStatus.DELIVERED,
            }:
                raise self._conflict(
                    command, "effect.irreversible",
                    f"effect is {record.status.value}; cancellation is no longer safe",
                    "inspect or reconcile the remote result",
                )
            target_state = EffectState.CANCELLED
            event_type = "competition.effect.cancel_requested"

        async def _apply() -> SideEffectResult:
            if command.command_type == "effect.retry":
                updated_outbox = self.competition_store.outbox.manual_retry(
                    record.outbox_id)
                updated_effect = self.platform_store.update_effect_receipt(
                    effect.effect_id,
                    EffectState.WAITING,
                    attempts=updated_outbox.attempts,
                    retry_after=None,
                    error=None,
                )
                original_state = ReceiptState.WAITING
            else:
                if (
                    str(record.payload.get("op") or "") == "submit"
                    and self.submissions is not None
                ):
                    submission_id = str(
                        record.payload.get("submission_id") or "")
                    if submission_id:
                        submission = self.competition_store.get(
                            PlatformSubmission, submission_id,
                        )
                        if (
                            submission is not None
                            and submission.submission_state()
                            is not SubmissionState.CANCELLED
                        ):
                            self.submissions.cancel_submission(
                                submission_id, reason="effect_cancelled")
                updated_outbox = self.competition_store.outbox.cancel(
                    record.outbox_id)
                updated_effect = self.platform_store.update_effect_receipt(
                    effect.effect_id,
                    EffectState.CANCELLED,
                    attempts=updated_outbox.attempts,
                    retry_after=None,
                    error=None,
                )
                original_state = ReceiptState.CANCELLED
            original = self.competition_store.get_receipt(effect.command_id)
            if original is not None:
                self.competition_store.update_receipt_state(
                    effect.command_id,
                    original_state,
                    error=None,
                    effect_ids=[updated_effect.effect_id],
                    output={
                        **dict(original.output or {}),
                        "effect_state": updated_effect.state.value,
                        "effect_ids": [updated_effect.effect_id],
                        "outbox_ids": [updated_outbox.outbox_id],
                    },
                )
            return SideEffectResult(output={
                "effect": updated_effect.model_dump(mode="json"),
                "outbox_status": updated_outbox.status.value,
                "original_command_id": effect.command_id,
            })

        return CommandPlan(
            events=[EventEnvelope(
                aggregate_type="effect",
                aggregate_id=effect.effect_id,
                event_type=event_type,
                producer="builtin.competition",
                actor_id=command.actor.id or "system",
                command_id=command.command_id,
                causation_id=command.command_id,
                correlation_id=correlation_id_of(command),
                idempotency_key=command.idempotency_key,
                payload={
                    "effect_id": effect.effect_id,
                    "original_command_id": effect.command_id,
                    "outbox_id": effect.outbox_id,
                    "from": effect.state.value,
                    "to": target_state.value,
                },
            )],
            receipt=CommandReceipt(
                command_id=command.command_id,
                state=ReceiptState.ACCEPTED,
                aggregate=AggregateRef(type="effect", id=effect.effect_id),
            ),
            side_effect=_apply,
        )

    @staticmethod
    def _conflict(
        command: CommandEnvelope,
        code: str,
        message: str,
        recovery_hint: str,
    ) -> CommandFailed:
        return CommandFailed(
            make_error(
                code,
                message,
                ErrorCategory.STATE,
                correlation_id=correlation_id_of(command),
                recovery_hint=recovery_hint,
            ),
            state=ReceiptState.CONFLICT,
        )


def register_effect_control_handlers(
    api: Any,
    platform_store: Any,
    competition_store: CompetitionStore,
    *,
    submission_service: Any = None,
) -> None:
    if "effect.retry" in api.handlers.known_command_types():
        return
    api.register_command(CompetitionEffectControlHandler(
        platform_store,
        competition_store,
        submission_service=submission_service,
    ))


__all__ = [
    "COMMAND_TYPES",
    "CompetitionEffectControlHandler",
    "register_effect_control_handlers",
]
