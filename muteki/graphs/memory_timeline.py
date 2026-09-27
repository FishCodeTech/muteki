"""memory.timeline.v1：通用对话长期记忆图（GRAPH-01，任务书 8.3）。

通用对话（Thread）的长期记忆：

- 只记录用户允许的记忆事件（``memory.recorded`` 事件类型本身即授权标记；
  payload 显式带 ``consent`` 时必须为真），每条记忆必须带来源 ``source``
  （来源记录：谁、在哪条 Thread、基于什么内容写入）。
- 删除以 ``memory.deleted`` tombstone 事件表示：投影行打 ``deleted_seq``
  标记，事件日志与投影行都不物理删除，可审计、可回放。
- 独立 SQLite 库；事件类型使用 ``memory.`` 前缀，与其他图不混用。
"""

from __future__ import annotations

import json
from datetime import datetime, timezone
from typing import Any

from muteki.platform.contracts.graphs import GraphEvent, GraphScope, GraphSnapshot

from muteki.graphs.base import GraphError, SQLiteGraphBase

# ---------------------------------------------------------------------------
# 领域事件词汇（"memory." 前缀）
# ---------------------------------------------------------------------------

EV_MEMORY_RECORDED = "memory.recorded"
EV_MEMORY_DELETED = "memory.deleted"


class MemoryTimelineGraph(SQLiteGraphBase):
    """通用对话长期记忆的 SQLite 实现。"""

    GRAPH_ID = "memory.timeline.v1"
    # 记忆时间线没有可 claim 的工作项；base 的原子 claim 对空 intents 投影
    # 自然返回 granted=False，lease 仍可用于 Thread 级写锁等资源。

    EXTRA_SCHEMA = """
    CREATE TABLE IF NOT EXISTS memory_items (
        memory_id TEXT PRIMARY KEY,
        thread_id TEXT NOT NULL DEFAULT '',
        kind TEXT NOT NULL DEFAULT 'note',
        content TEXT NOT NULL DEFAULT '',
        source_json TEXT NOT NULL DEFAULT '{}',
        created_seq INTEGER NOT NULL,
        updated_seq INTEGER NOT NULL,
        deleted_seq INTEGER,
        delete_reason TEXT
    );
    """

    # -- 领域校验与投影 ---------------------------------------------------------

    def _validate_event(self, event: GraphEvent) -> None:
        p = event.payload
        if event.event_type == EV_MEMORY_RECORDED:
            if not p.get("memory_id") or not p.get("content"):
                raise GraphError("memory.recorded requires memory_id and content")
            source = p.get("source")
            if not isinstance(source, dict) or not source:
                raise GraphError(
                    "memory.recorded requires a non-empty source record"
                )
            # 显式 consent 字段存在时必须为真（用户允许的记忆才写入）
            if "consent" in p and not p.get("consent"):
                raise GraphError("memory.recorded without user consent")
        elif event.event_type == EV_MEMORY_DELETED:
            if not p.get("memory_id"):
                raise GraphError("memory.deleted requires memory_id")

    def _apply_event(self, event: GraphEvent, seq: int) -> None:
        p = event.payload
        if event.event_type == EV_MEMORY_RECORDED:
            # 同一 memory_id 重新记录视为更新；若曾被删除则随之复活
            self._conn.execute(
                "INSERT INTO memory_items (memory_id, thread_id, kind, content, "
                "source_json, created_seq, updated_seq) "
                "VALUES (?,?,?,?,?,?,?) "
                "ON CONFLICT(memory_id) DO UPDATE SET "
                "thread_id=excluded.thread_id, kind=excluded.kind, "
                "content=excluded.content, source_json=excluded.source_json, "
                "updated_seq=excluded.updated_seq, deleted_seq=NULL, "
                "delete_reason=NULL",
                (
                    str(p["memory_id"]),
                    str(p.get("thread_id") or ""),
                    str(p.get("kind") or "note"),
                    str(p["content"]),
                    json.dumps(p.get("source"), ensure_ascii=False, default=str),
                    seq,
                    seq,
                ),
            )
        elif event.event_type == EV_MEMORY_DELETED:
            # tombstone：只打删除标记，不物理删除投影行，更不删除事件
            self._conn.execute(
                "UPDATE memory_items SET deleted_seq=?, delete_reason=?, "
                "updated_seq=? WHERE memory_id=? AND deleted_seq IS NULL",
                (
                    seq,
                    str(p.get("reason") or ""),
                    seq,
                    str(p["memory_id"]),
                ),
            )

    # -- GraphService.snapshot ---------------------------------------------------

    async def snapshot(self, scope: GraphScope) -> GraphSnapshot:
        """记忆读取模型；scope 支持 thread_id 过滤与 include_deleted 开关。"""
        self._check_graph_id(scope.graph_id)
        thread_id = scope.scope.get("thread_id")
        include_deleted = bool(scope.scope.get("include_deleted"))
        with self._lock:
            memories, tombstones = self._load_memories(thread_id=thread_id)
            watermark = self.watermark()
        state: dict[str, Any] = {
            "memories": memories,
            "deleted_count": len(tombstones),
        }
        if include_deleted:
            state["tombstones"] = tombstones
        return GraphSnapshot(
            graph_id=self.GRAPH_ID, watermark=watermark, state=state
        )

    # -- 投影读取辅助 ------------------------------------------------------------

    def _load_memories(
        self, *, thread_id: Any = None
    ) -> tuple[list[dict[str, Any]], list[dict[str, Any]]]:
        sql = (
            "SELECT m.memory_id, m.thread_id, m.kind, m.content, "
            "m.source_json, m.created_seq, m.updated_seq, m.deleted_seq, "
            "m.delete_reason, created.ts, updated.ts "
            "FROM memory_items AS m "
            "LEFT JOIN graph_events AS created ON created.seq=m.created_seq "
            "LEFT JOIN graph_events AS updated ON updated.seq=m.updated_seq"
        )
        params: tuple[Any, ...] = ()
        if thread_id:
            sql += " WHERE m.thread_id=?"
            params = (str(thread_id),)
        sql += " ORDER BY m.created_seq"
        rows = self._conn.execute(sql, params).fetchall()
        memories: list[dict[str, Any]] = []
        tombstones: list[dict[str, Any]] = []
        for r in rows:
            item = {
                "memory_id": r[0],
                "thread_id": r[1],
                "kind": r[2],
                "content": r[3],
                "source": json.loads(r[4] or "{}"),
                "created_seq": int(r[5]),
                "updated_seq": int(r[6]),
                "created_at": self._iso_timestamp(r[9]),
                "updated_at": self._iso_timestamp(r[10]),
            }
            if r[7] is None:
                memories.append(item)
            else:
                tombstones.append(
                    {
                        "memory_id": r[0],
                        "deleted_seq": int(r[7]),
                        "deleted_at": self._iso_timestamp(r[10]),
                        "reason": r[8] or "",
                    }
                )
        return memories, tombstones

    @staticmethod
    def _iso_timestamp(value: Any) -> str:
        if value is None:
            return ""
        return datetime.fromtimestamp(float(value), tz=timezone.utc).isoformat()
