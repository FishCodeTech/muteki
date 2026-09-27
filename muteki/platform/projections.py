"""查询投影与水位持久化（任务书 6.1，CORE-02）。

- Projection 是查询侧物化视图：从 ``domain_events`` 全局 seq 顺序折叠而来，
  可随时丢弃重建。投影自己的状态表由 Projection 实现负责（``ensure``/``reset``）。
- 每个 projection 的消费水位持久化在 ``projection_watermarks``
  （``last_event_seq`` = 已消费的最大全局事件 seq），重启后从水位继续。
- 消费事件时经 ``store.upcasters`` 把旧版本事件升到当前版本；
  历史事件正文不改写。
"""

from __future__ import annotations

import sqlite3
from typing import Protocol, runtime_checkable

from muteki.platform.contracts import EventEnvelope, utcnow
from muteki.platform.store import PlatformStore, StoreError


@runtime_checkable
class Projection(Protocol):
    """一个可重建的查询投影。

    实现约定：
    - ``ensure(conn)`` 创建投影自己的状态表（幂等）。
    - ``reset(conn)`` 清空投影状态（重建前调用）。
    - ``apply(conn, event)`` 折叠一条（已 upcast 的）事件；与水位更新
      在同一事务里提交。
    """

    name: str

    def ensure(self, conn: sqlite3.Connection) -> None: ...

    def reset(self, conn: sqlite3.Connection) -> None: ...

    def apply(self, conn: sqlite3.Connection, event: EventEnvelope) -> None: ...


class ProjectionManager:
    """投影注册、水位持久化、增量消费与重建。"""

    def __init__(
        self, store: PlatformStore, projections: tuple[Projection, ...] = ()
    ) -> None:
        self._store = store
        self._projections: dict[str, Projection] = {}
        for projection in projections:
            self.register(projection)

    def register(self, projection: Projection) -> None:
        name = str(projection.name).strip()
        if not name:
            raise ValueError("projection name cannot be empty")
        if name in self._projections:
            raise StoreError(f"projection already registered: {name}")
        with self._store.lock, self._store.conn:
            projection.ensure(self._store.conn)
            self._store.conn.execute(
                "INSERT INTO projection_watermarks (projection_name, "
                "last_event_seq, schema_version, created_at, updated_at, payload) "
                "VALUES (?, 0, 1, ?, ?, '{}') "
                "ON CONFLICT(projection_name) DO NOTHING",
                (name, utcnow().isoformat(), utcnow().isoformat()),
            )
        self._projections[name] = projection

    def names(self) -> list[str]:
        return sorted(self._projections)

    def get(self, name: str) -> Projection:
        projection = self._projections.get(name)
        if projection is None:
            raise StoreError(f"unknown projection: {name}")
        return projection

    # -- 水位 ---------------------------------------------------------------

    def watermark(self, name: str) -> int:
        """projection 已消费的全局事件 seq（未消费过为 0）。"""
        with self._store.lock:
            row = self._store.conn.execute(
                "SELECT last_event_seq FROM projection_watermarks "
                "WHERE projection_name = ?",
                (name,),
            ).fetchone()
        return int(row["last_event_seq"]) if row else 0

    def watermarks(self) -> dict[str, int]:
        with self._store.lock:
            rows = list(self._store.conn.execute(
                "SELECT projection_name, last_event_seq FROM projection_watermarks"
            ).fetchall())
        return {row["projection_name"]: int(row["last_event_seq"]) for row in rows}

    def _set_watermark(self, conn: sqlite3.Connection, name: str, seq: int) -> None:
        conn.execute(
            "UPDATE projection_watermarks SET last_event_seq = ?, updated_at = ? "
            "WHERE projection_name = ?",
            (int(seq), utcnow().isoformat(), name),
        )

    # -- 消费与重建 -----------------------------------------------------------

    def lagging(self) -> list[str]:
        """水位落后于事件日志全局水位的 projection。"""
        head = self._store.event_watermark()
        return [name for name in self.names() if self.watermark(name) < head]

    def run(self, name: str, *, limit: int = 1000) -> int:
        """增量消费：从水位之后按全局 seq 折叠事件，返回本次消费条数。"""
        projection = self.get(name)
        applied = 0
        while True:
            after = self.watermark(name)
            batch = self._store.read_all_events(after_seq=after, limit=limit)
            if not batch:
                return applied
            with self._store.lock, self._store.conn:
                for seq, event in batch:
                    projection.apply(self._store.conn, event)
                    applied += 1
                self._set_watermark(self._store.conn, name, batch[-1][0])
            if len(batch) < limit:
                return applied

    def run_all(self, *, limit: int = 1000) -> dict[str, int]:
        """对所有已注册 projection 做增量消费。"""
        return {name: self.run(name, limit=limit) for name in self.names()}

    def rebuild(self, name: str, *, limit: int = 1000) -> int:
        """重建投影：清空状态、水位归零、从头回放（事件经 upcaster 升级）。"""
        projection = self.get(name)
        with self._store.lock, self._store.conn:
            projection.reset(self._store.conn)
            self._set_watermark(self._store.conn, name, 0)
        return self.run(name, limit=limit)

    def rebuild_all(self, *, limit: int = 1000) -> dict[str, int]:
        return {name: self.rebuild(name, limit=limit) for name in self.names()}
