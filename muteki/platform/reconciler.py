"""启动恢复 reconciler（任务书 5.2、16 章 CORE-02）。

服务重启后的恢复流程：
1. 校验事件日志：每条聚合流的 stream_seq 必须从 1 连续、无缺口；
   投影水位不得超过事件日志全局水位。
2. 列出未完成 receipt：仍处于 accepted/running/waiting 的命令回执及其按
   command_type 的计数。领域模块各自落终态（conversation.* 由
   ``ConversationService.recover`` 结算）；这里只观察，不改写回执。
3. 列出待处理 outbox：pending / 到点 failed 的副作用记录。
4. 重建落后 projection：水位落后于事件日志的 projection 增量追平。
"""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Optional

from muteki.platform.contracts import CommandReceipt, OutboxRecord
from muteki.platform.contracts.receipts import ReceiptState
from muteki.platform.outbox import OutboxManager
from muteki.platform.projections import ProjectionManager
from muteki.platform.store import PlatformStore


def unfinished_receipts_by_command_type(store: PlatformStore) -> dict[str, int]:
    """Count of accepted/running/waiting receipts per command_type in platform.db."""
    states = (ReceiptState.ACCEPTED.value, ReceiptState.RUNNING.value,
              ReceiptState.WAITING.value)
    with store.lock:
        rows = list(store.conn.execute(
            "SELECT command_type, COUNT(*) AS n FROM command_receipts "
            "WHERE state IN (?, ?, ?) GROUP BY command_type ORDER BY command_type",
            states,
        ).fetchall())
    return {str(row["command_type"]): int(row["n"]) for row in rows}


@dataclass
class ReconcileReport:
    """一次启动恢复的结果。"""

    ok: bool = True
    #: 事件日志 / 水位校验发现的问题（非空即 ok=False）
    issues: list[str] = field(default_factory=list)
    #: 仍处于 accepted、需要调用方回放的命令回执
    unfinished_receipts: list[CommandReceipt] = field(default_factory=list)
    #: 未完成回执按 command_type 计数（run.* / conversation.* 等各领域）
    unfinished_by_command_type: dict[str, int] = field(default_factory=dict)
    #: 待投递的 outbox 记录
    pending_outbox: list[OutboxRecord] = field(default_factory=list)
    #: 本次追平 / 重建的 projection 及各自消费的事件数
    caught_up_projections: dict[str, int] = field(default_factory=dict)
    event_watermark: int = 0


class Reconciler:
    """platform.db 的启动恢复。"""

    def __init__(
        self,
        store: PlatformStore,
        *,
        projections: Optional[ProjectionManager] = None,
        outbox: Optional[OutboxManager] = None,
    ) -> None:
        self._store = store
        self._projections = projections
        self._outbox = outbox or OutboxManager(store)

    # -- 校验 -----------------------------------------------------------------

    def _validate_event_log(self) -> list[str]:
        """每条聚合流 stream_seq 必须 1..N 连续（唯一性由表约束保证）。"""
        issues: list[str] = []
        with self._store.lock:
            rows = list(self._store.conn.execute(
                "SELECT aggregate_type, aggregate_id, stream_seq FROM domain_events "
                "ORDER BY aggregate_type, aggregate_id, stream_seq"
            ).fetchall())
        current: Optional[tuple[str, str]] = None
        expected = 1
        for row in rows:
            key = (row["aggregate_type"], row["aggregate_id"])
            if key != current:
                current = key
                expected = 1
            seq = int(row["stream_seq"])
            if seq != expected:
                issues.append(
                    f"stream {key[0]}/{key[1]} gap: expected stream_seq "
                    f"{expected}, found {seq}"
                )
                expected = seq
            expected += 1
        return issues

    def _validate_watermarks(self) -> list[str]:
        """投影水位不得超过事件日志全局水位。"""
        issues: list[str] = []
        head = self._store.event_watermark()
        if self._projections is None:
            return issues
        for name, watermark in self._projections.watermarks().items():
            if watermark > head:
                issues.append(
                    f"projection {name} watermark {watermark} exceeds event "
                    f"log head {head}"
                )
        return issues

    # -- 恢复 ------------------------------------------------------------------

    def recover(self) -> ReconcileReport:
        """执行完整启动恢复，返回报告。校验发现问题时不做追平，由调用方处置。"""
        report = ReconcileReport()
        report.issues = self._validate_event_log() + self._validate_watermarks()
        report.event_watermark = self._store.event_watermark()
        report.ok = not report.issues

        # 未完成 receipt：列出给领域恢复结算
        report.unfinished_receipts = self._store.pending_receipts()
        report.unfinished_by_command_type = unfinished_receipts_by_command_type(self._store)
        # 待处理 outbox
        report.pending_outbox = self._outbox.pending(limit=1000)

        # 落后 projection 追平
        if report.ok and self._projections is not None:
            for name in self._projections.lagging():
                report.caught_up_projections[name] = self._projections.run(name)
        return report
