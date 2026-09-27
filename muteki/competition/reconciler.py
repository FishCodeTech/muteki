"""CompetitionReconciler：启动恢复的固定十步顺序与 wrong 否决的水位投影
（任务书 10.8 / 设计 9.5、10 末段，COMP-08）。

启动恢复顺序固定（任务书 10.8 逐字）：

1.  加载 schema 和 migration；
2.  回放未完成 command receipt；
3.  处理 competition outbox；
4.  对齐 PlatformConnection 和同步游标；
5.  reconcile InstanceLease（经 COMP-07 ``InstanceLeaseManager.reconcile``）；
6.  对齐 RunBinding 和 RunGateway snapshot；
7.  补齐 wrong/correct 到子 Run 的投影；
8.  恢复 Scheduler 队列、预算和冷却；
9.  恢复 submitting/unknown 状态（submitting 重启统一归 unknown，先查远端
    状态，禁止直接重发）；
10. 发布 snapshot watermark 后开放写命令。

每步写一条 ``ReconcileCheckpoint``（done/failed + details + 执行时的
事件水位）；单步失败记录后继续后续步骤，报告 ``ok=False``（与
``muteki.platform.reconciler.Reconciler`` 的 issue 收集风格一致）。

跨库投影（设计 10 末段：比赛事实先写 competition.db，随后投影到对应
Run，不能跨库原子事务）：

- ``RunFlagInvalidationProjection`` 是 ``CompetitionProjection`` 协议
  实现：折叠 ``platform_submission.state_changed``（to=wrong）事件，
  把候选值经 ``reopen_after_false_positive`` 写进来源 Run 的
  SharedGraph flag invalidation 通道。
- 幂等双保险：水位（``competition_projection_watermarks``）保证每条
  事件只消费一次；图内 ``flaginvalid::<flag>`` dedupe_key 保证水位
  重建 / 重复执行也不重复写否决。
- 事件与图都不保存之外的第二套否决存储（设计 12.2 末段）。
"""

from __future__ import annotations

import sqlite3
from dataclasses import dataclass, field
from datetime import datetime
from typing import Any, Callable, Optional

from muteki.competition import events as ev
from muteki.competition.models import (
    BINDING_ACTIVE_STATES,
    BindingState,
    ConnectionStatus,
    PlatformConnection,
    PlatformSubmission,
    ReconcileCheckpoint,
    ResourceBudget,
    RunBinding,
    SchedulerQueueEntry,
    SubmissionCandidate,
    SubmissionState,
    SyncCursor,
)
from muteki.competition.projections import CompetitionProjectionManager
from muteki.competition.store import CompetitionStore
from muteki.competition.submission import SubmissionService
from muteki.platform.contracts.base import utcnow
from muteki.platform.contracts.events import EventEnvelope

#: 启动恢复的固定步骤名（任务书 10.8 逐字顺序，检查点与报告共用）。
RECOVERY_STEPS: tuple[str, ...] = (
    "schema_migrations",          # 1. 加载 schema 和 migration
    "replay_receipts",            # 2. 回放未完成 command receipt
    "drain_outbox",               # 3. 处理 competition outbox
    "align_connections_cursors",  # 4. 对齐 PlatformConnection 和同步游标
    "reconcile_leases",           # 5. reconcile InstanceLease（COMP-07）
    "align_bindings_gateway",     # 6. 对齐 RunBinding 和 RunGateway snapshot
    "project_run_verdicts",       # 7. 补齐 wrong/correct 到子 Run 的投影
    "restore_scheduler",          # 8. 恢复 Scheduler 队列、预算和冷却
    "restore_submissions",        # 9. 恢复 submitting/unknown 状态
    "publish_watermark",          # 10. 发布 snapshot watermark 后开放写命令
)

#: wrong 否决投影器在 competition_projection_watermarks 里的名字。
RUN_FLAG_INVALIDATION_PROJECTION = "run_flag_invalidation"


@dataclass
class CompetitionReconcileReport:
    """一次启动恢复的结果（每步 details 与失败清单）。"""

    ok: bool = True
    steps: dict[str, dict[str, Any]] = field(default_factory=dict)
    failed_steps: list[str] = field(default_factory=list)
    watermark: int = 0


class RunFlagInvalidationProjection:
    """competition.db → 每 Run SharedGraph 的 wrong 否决投影（带水位）。

    实现 ``CompetitionProjection`` 协议：``ensure``/``reset`` 无自有状态表
    （否决只写在各 Run 的 SharedGraph；reset 后重放由图内 dedupe_key
    保证幂等）。``apply`` 失败时抛出让水位停在原地，下次运行重试。
    """

    name = RUN_FLAG_INVALIDATION_PROJECTION

    def __init__(
        self,
        store: CompetitionStore,
        shared_graph_for: Callable[[str], Optional[Any]],
    ) -> None:
        self._store = store
        self._shared_graph_for = shared_graph_for

    def ensure(self, conn: sqlite3.Connection) -> None:
        return None  # 无自有状态表

    def reset(self, conn: sqlite3.Connection) -> None:
        return None  # 图内 dedupe_key 保证重放幂等，无需清理

    def apply(self, conn: sqlite3.Connection, event: EventEnvelope) -> None:
        if event.event_type != ev.SUBMISSION_STATE_CHANGED:
            return
        if event.payload.get("to") != SubmissionState.WRONG.value:
            return
        submission = self._store.get(
            PlatformSubmission, str(event.payload.get("submission_id") or ""))
        if submission is None:
            return
        candidate = self._store.get(
            SubmissionCandidate, submission.candidate_id)
        if candidate is None or not candidate.source_run_id:
            return
        graph = self._shared_graph_for(candidate.source_run_id)
        if graph is None:
            return  # Run 的图已不可得（已清理）：否决无处投影，跳过
        graph.reopen_after_false_positive(
            actor="competition",
            flag=candidate.value,
            reason=(
                f"remote verdict wrong (submission {submission.submission_id})"
            ),
        )


class CompetitionReconciler:
    """competition.db 的启动恢复（任务书 10.8 固定十步）。

    - ``command_replay``：可选回放钩子 ``async (command, receipt)``，由
      COMMAND-01 的命令 API 注入；缺省时未完成 receipt 只列入报告
      （与 platform 层 Reconciler 的清单语义一致）。
    - ``submission_service`` / ``lease_manager`` / ``gateway`` /
      ``projection_manager``：各步骤的执行体；缺省时对应步骤只做
      观测记录（details 注明 skipped），不伪造恢复动作。
    """

    def __init__(
        self,
        store: CompetitionStore,
        *,
        command_replay: Optional[Callable[..., Any]] = None,
        submission_service: Optional[SubmissionService] = None,
        lease_manager: Any = None,
        binding_service: Any = None,
        gateway: Any = None,
        projection_manager: Optional[CompetitionProjectionManager] = None,
        now: Optional[Callable[[], datetime]] = None,
    ) -> None:
        self._store = store
        self._command_replay = command_replay
        self._submissions = submission_service
        self._leases = lease_manager
        self._bindings = binding_service
        self._gateway = gateway
        self._projections = projection_manager
        self._now = now or utcnow
        self._writes_open = False

    @property
    def writes_open(self) -> bool:
        """步骤 10（发布 snapshot watermark）完成后才开放写命令。"""
        return self._writes_open

    async def recover(self) -> CompetitionReconcileReport:
        """按固定顺序执行十步恢复；每步写 reconcile_checkpoints。"""
        self._writes_open = False
        report = CompetitionReconcileReport()
        handlers: dict[str, Callable[[], Any]] = {
            "schema_migrations": self._step_schema,
            "replay_receipts": self._step_replay_receipts,
            "drain_outbox": self._step_drain_outbox,
            "align_connections_cursors": self._step_align_connections,
            "reconcile_leases": self._step_reconcile_leases,
            "align_bindings_gateway": self._step_align_bindings,
            "project_run_verdicts": self._step_project_verdicts,
            "restore_scheduler": self._step_restore_scheduler,
            "restore_submissions": self._step_restore_submissions,
            "publish_watermark": self._step_publish_watermark,
        }
        for step in RECOVERY_STEPS:
            try:
                details = await handlers[step]()
            except Exception as exc:
                report.ok = False
                report.failed_steps.append(step)
                details = {"error": f"{type(exc).__name__}: {exc}"}
                self._checkpoint(step, status="failed", details=details)
            else:
                self._checkpoint(step, status="done", details=details)
            report.steps[step] = details
        report.watermark = self._store.event_watermark()
        return report

    def _checkpoint(
        self, step: str, *, status: str, details: dict[str, Any]
    ) -> None:
        self._store.save(ReconcileCheckpoint(
            step=step,
            status=status,
            details=details,
            event_seq=self._store.event_watermark(),
        ))

    # ------------------------------------------------------------------
    # 1. 加载 schema 和 migration
    # ------------------------------------------------------------------

    async def _step_schema(self) -> dict[str, Any]:
        # migration 在 CompetitionStore 构造时已应用；这里校验并记录版本。
        version = self._store.schema_version()
        if version is None or version < 1:
            raise RuntimeError("competition.db schema migrations not applied")
        return {"schema_version": version}

    # ------------------------------------------------------------------
    # 2. 回放未完成 command receipt
    # ------------------------------------------------------------------

    async def _step_replay_receipts(self) -> dict[str, Any]:
        pending = self._store.pending_receipts()
        replayed: list[str] = []
        missing_command: list[str] = []
        if self._command_replay is not None:
            for receipt in pending:
                command = self._store.get_command(receipt.command_id)
                if command is None:
                    missing_command.append(receipt.command_id)
                    continue
                await self._command_replay(command, receipt)
                replayed.append(receipt.command_id)
        return {
            "pending": len(pending),
            "replayed": replayed,
            "missing_command": missing_command,
            "mode": "replay" if self._command_replay is not None else "listed",
        }

    # ------------------------------------------------------------------
    # 3. 处理 competition outbox
    # ------------------------------------------------------------------

    async def _step_drain_outbox(self) -> dict[str, Any]:
        executed: list[str] = []
        if self._submissions is not None:
            executed = [
                s.submission_id for s in await self._submissions.pump()
            ]
        remaining = len(self._store.outbox.pending(now=self._now(), limit=1000))
        return {"submitted": executed, "pending_outbox": remaining}

    # ------------------------------------------------------------------
    # 4. 对齐 PlatformConnection 和同步游标
    # ------------------------------------------------------------------

    async def _step_align_connections(self) -> dict[str, Any]:
        connections = self._store.list(PlatformConnection)
        cursors = self._store.list(SyncCursor)
        # auth_required 的连接不在启动时自动复活：等重新授权后由命令显式
        # 恢复（任务书 10.7：auth_failed 暂停该连接的外部动作）。
        return {
            "connections": len(connections),
            "auth_paused": [
                c.connection_id for c in connections
                if c.status != ConnectionStatus.ACTIVE.value
            ],
            "sync_cursors": len(cursors),
        }

    # ------------------------------------------------------------------
    # 5. reconcile InstanceLease（经 COMP-07）
    # ------------------------------------------------------------------

    async def _step_reconcile_leases(self) -> dict[str, Any]:
        if self._leases is None:
            return {"skipped": "no lease manager"}
        report = await self._leases.reconcile()
        return {
            "confirmed": list(report.confirmed),
            "adopted": list(report.adopted),
            "lost": list(report.lost),
            "released": list(report.released),
            "failed": list(report.failed),
            "degraded": list(report.degraded),
            "paused": list(report.paused),
        }

    # ------------------------------------------------------------------
    # 6. 对齐 RunBinding 和 RunGateway snapshot
    # ------------------------------------------------------------------

    async def _step_align_bindings(self) -> dict[str, Any]:
        active = [
            b for b in self._store.list(RunBinding)
            if b.state in BINDING_ACTIVE_STATES
        ]
        aligned: list[str] = []
        paused: list[str] = []
        unreachable: list[str] = []
        if self._gateway is not None:
            for binding in active:
                try:
                    snapshot = await self._gateway.snapshot(binding.run_id)
                except Exception:
                    unreachable.append(binding.binding_id)
                    continue
                # gateway 是 Run 状态权威：执行代落后于 snapshot 时对齐。
                if int(snapshot.generation or 0) > binding.execution_generation:
                    binding = self._store.save(binding.model_copy(update={
                        "execution_generation": int(snapshot.generation),
                    }))
                    aligned.append(binding.binding_id)
                # RunManager 重启后不会伪装恢复旧进程；非 running 快照说明
                # 该 ACTIVE binding 已没有运行时所有者，必须释放调度席位。
                if (
                    self._bindings is not None
                    and binding.binding_state() is BindingState.ACTIVE
                    and snapshot.state != "running"
                ):
                    updated = await self._bindings.pause(
                        binding.competition_challenge_id,
                        reason=f"restart_snapshot_{snapshot.state or 'unknown'}",
                    )
                    if updated is not None:
                        paused.append(updated.binding_id)
        return {
            "active_bindings": len(active),
            "generation_aligned": aligned,
            "paused_without_runtime": paused,
            "gateway_unreachable": unreachable,
            "mode": "gateway" if self._gateway is not None else "observed",
        }

    # ------------------------------------------------------------------
    # 7. 补齐 wrong/correct 到子 Run 的投影
    # ------------------------------------------------------------------

    async def _step_project_verdicts(self) -> dict[str, Any]:
        details: dict[str, Any] = {}
        if self._projections is not None and (
                RUN_FLAG_INVALIDATION_PROJECTION in self._projections.names()):
            details["invalidation_events_applied"] = self._projections.run(
                RUN_FLAG_INVALIDATION_PROJECTION)
            details["invalidation_watermark"] = self._projections.watermark(
                RUN_FLAG_INVALIDATION_PROJECTION)
        else:
            details["invalidation_projection"] = "skipped"
        if self._submissions is not None:
            details["aligned"] = await self._submissions.align_verdicts()
        else:
            details["aligned"] = "skipped"
        return details

    # ------------------------------------------------------------------
    # 8. 恢复 Scheduler 队列、预算和冷却
    # ------------------------------------------------------------------

    async def _step_restore_scheduler(self) -> dict[str, Any]:
        # Scheduler 无内存态（每 tick 从 competition.db 重建输入）；队列、
        # 预算、冷却（policy.submission_cooldown_seconds、连接 capabilities
        # 的 submission_cooldown_until、submission.retry_after_at）都是持久
        # 行，这里做可读性校验与汇总即完成恢复。
        queue: dict[str, int] = {}
        for entry in self._store.list(SchedulerQueueEntry):
            queue[entry.state] = queue.get(entry.state, 0) + 1
        budgets = {
            f"{b.competition_id}/{b.kind}": {"limit": b.limit, "used": b.used}
            for b in self._store.list(ResourceBudget)
        }
        cooldowns = [
            c.connection_id for c in self._store.list(PlatformConnection)
            if (c.capabilities or {}).get("submission_cooldown_until")
        ]
        return {
            "queue_by_state": queue,
            "budgets": budgets,
            "connection_cooldowns": cooldowns,
        }

    # ------------------------------------------------------------------
    # 9. 恢复 submitting/unknown 状态
    # ------------------------------------------------------------------

    async def _step_restore_submissions(self) -> dict[str, Any]:
        # submitting 重启统一归 unknown（设计 9.4），禁止直接重发。
        reset = self._store.reset_submitting_to_unknown()
        details: dict[str, Any] = {
            "submitting_to_unknown": [s.submission_id for s in reset],
        }
        if self._submissions is not None:
            # 先查远端状态（poll_submission / reconcile_submission），
            # 查不了的保持 unknown 等 Operator。
            report = await self._submissions.reconcile_unknown()
            details["reconciled"] = {k: list(v) for k, v in report.items()}
        return details

    # ------------------------------------------------------------------
    # 10. 发布 snapshot watermark 后开放写命令
    # ------------------------------------------------------------------

    async def _step_publish_watermark(self) -> dict[str, Any]:
        watermark = self._store.event_watermark()
        self._writes_open = True
        return {"watermark": watermark, "writes_open": True}


__all__ = [
    "CompetitionReconciler",
    "CompetitionReconcileReport",
    "RECOVERY_STEPS",
    "RUN_FLAG_INVALIDATION_PROJECTION",
    "RunFlagInvalidationProjection",
]
