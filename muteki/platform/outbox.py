"""持久 outbox：跨存储边界外部副作用的可靠投递记录（任务书 5.2、6.2）。

跨 platform.db / competition.db / Run 存储 / SharedGraph 边界的外部副作用
先在同库事务里写 outbox，事务提交后再投递；重启后未完成记录由
reconciler 列出并继续投递。幂等键唯一：同一副作用重复入队返回已有记录。
"""

from __future__ import annotations

from datetime import datetime, timedelta, timezone
from typing import Optional

from muteki.platform.contracts import OutboxRecord, OutboxStatus, utcnow
from muteki.platform.store import NotFoundError, PlatformStore

#: 状态以 contracts.OutboxStatus 为准（pending / delivered / failed）。


class OutboxManager:
    """outbox 的入队、待投递查询与投递结果回写。"""

    def __init__(self, store: PlatformStore) -> None:
        self._store = store

    @staticmethod
    def _key_for(record: OutboxRecord, idempotency_key: Optional[str]) -> str:
        """副作用幂等键：显式给出优先，否则取 command_id / event_id / outbox_id。"""
        key = (idempotency_key or "").strip() or (
            record.command_id or record.event_id or record.outbox_id
        )
        return key

    def enqueue(
        self, record: OutboxRecord, *, idempotency_key: Optional[str] = None
    ) -> tuple[OutboxRecord, bool]:
        """入队一条副作用。返回 (记录, 是否新插入)；幂等键重复时返回已有记录。"""
        key = self._key_for(record, idempotency_key)
        now = utcnow().isoformat()
        with self._store.transaction():
            existing = self._store.conn.execute(
                "SELECT payload FROM outbox WHERE idempotency_key = ?", (key,)
            ).fetchone()
            if existing is not None:
                return OutboxRecord.model_validate_json(existing["payload"]), False
            self._store.conn.execute(
                "INSERT INTO outbox (outbox_id, idempotency_key, command_id, "
                "event_id, aggregate_type, aggregate_id, event_type, destination, "
                "status, attempts, next_attempt_at, last_error, delivered_at, "
                "schema_version, created_at, updated_at, payload) "
                "VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                (
                    record.outbox_id,
                    key,
                    record.command_id,
                    record.event_id,
                    record.aggregate_type,
                    record.aggregate_id,
                    record.event_type,
                    record.destination,
                    record.status.value,
                    record.attempts,
                    now,  # 立即可投递
                    record.last_error,
                    None,
                    record.schema_version,
                    record.created_at.isoformat(),
                    now,
                    record.model_dump_json(),
                ),
            )
        return record, True

    def get(self, outbox_id: str) -> Optional[OutboxRecord]:
        with self._store.lock:
            row = self._store.conn.execute(
                "SELECT payload FROM outbox WHERE outbox_id = ?", (outbox_id,)
            ).fetchone()
        return OutboxRecord.model_validate_json(row["payload"]) if row else None

    def for_command(self, command_id: str) -> list[OutboxRecord]:
        with self._store.lock:
            rows = self._store.conn.execute(
                "SELECT payload FROM outbox WHERE command_id = ? ORDER BY created_at, rowid",
                (command_id,),
            ).fetchall()
        return [OutboxRecord.model_validate_json(row["payload"]) for row in rows]

    def pending(
        self, *, now: Optional[datetime] = None, limit: int = 100
    ) -> list[OutboxRecord]:
        """待投递记录：pending，或 failed 且已到 next_attempt_at。"""
        moment = (now or utcnow()).isoformat()
        with self._store.lock:
            rows = list(self._store.conn.execute(
                "SELECT payload FROM outbox "
                "WHERE status = ? OR (status = ? AND next_attempt_at <= ?) "
                "ORDER BY created_at, rowid LIMIT ?",
                (OutboxStatus.PENDING.value, OutboxStatus.FAILED.value, moment, int(limit)),
            ).fetchall())
        return [OutboxRecord.model_validate_json(row["payload"]) for row in rows]

    def mark_delivered(
        self, outbox_id: str, *, at: Optional[datetime] = None
    ) -> OutboxRecord:
        """投递成功：状态 delivered，记录 delivered_at。"""
        record = self.get(outbox_id)
        if record is None:
            raise NotFoundError(f"outbox record not found: {outbox_id}")
        record = record.model_copy(
            update={
                "status": OutboxStatus.DELIVERED,
                "delivered_at": at or utcnow(),
                "last_error": None,
            }
        )
        with self._store.transaction():
            self._store.conn.execute(
                "UPDATE outbox SET status = ?, delivered_at = ?, last_error = NULL, "
                "updated_at = ?, payload = ? WHERE outbox_id = ?",
                (
                    OutboxStatus.DELIVERED.value,
                    record.delivered_at.isoformat(),
                    utcnow().isoformat(),
                    record.model_dump_json(),
                    outbox_id,
                ),
            )
        return record

    def mark_failed(
        self,
        outbox_id: str,
        error: str,
        *,
        retry_delay_seconds: float = 60.0,
    ) -> OutboxRecord:
        """投递失败：attempts + 1，按退避设置 next_attempt_at，记录 last_error。"""
        record = self.get(outbox_id)
        if record is None:
            raise NotFoundError(f"outbox record not found: {outbox_id}")
        record = record.model_copy(
            update={
                "status": OutboxStatus.FAILED,
                "attempts": record.attempts + 1,
                "last_error": str(error),
            }
        )
        next_attempt = (
            datetime.now(timezone.utc) + timedelta(seconds=retry_delay_seconds)
        ).isoformat()
        with self._store.transaction():
            self._store.conn.execute(
                "UPDATE outbox SET status = ?, attempts = ?, next_attempt_at = ?, "
                "last_error = ?, updated_at = ?, payload = ? WHERE outbox_id = ?",
                (
                    OutboxStatus.FAILED.value,
                    record.attempts,
                    next_attempt,
                    record.last_error,
                    utcnow().isoformat(),
                    record.model_dump_json(),
                    outbox_id,
                ),
            )
        return record
