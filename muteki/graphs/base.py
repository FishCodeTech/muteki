"""GraphService 三类实现共享的 SQLite 事件 / 租约 / 投影基础（GRAPH-01）。

对应任务书 8.3 与设计文档 11.3：三类图实现可以共享事件、租约和投影基础
代码，但不共享同一个数据库、不混用同一套领域事件。本模块只提供机制：

- ``graph_events``：append-only 事件日志（INSERT only），``event_id`` 唯一、
  ``idempotency_key`` 部分唯一索引实现 append 幂等；事件与领域投影在同一
  事务内落库（子类钩子 ``_apply_event``）。
- ``graph_intents`` / ``graph_leases``：通用工作项与独占资源投影；
  原子 claim 是单事务内的条件 UPDATE，fencing token 来自 ``graph_counters``
  的单调计数器，与租约在同一事务内分配。
- 领域词汇（事件类型常量、投影表、snapshot 读模型）全部由子类定义；
  基类不携带任何领域语义。

数据库风格与 ``muteki.platform.store`` / ``muteki.swarm.shared_graph``
一致：单长连接 + WAL + busy_timeout + 写锁。
"""

from __future__ import annotations

import json
import sqlite3
import threading
import time
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Optional

from muteki.platform.contracts import new_id
from muteki.platform.contracts.graphs import (
    ClaimReceipt,
    ClaimRequest,
    GraphEvent,
    GraphReceipt,
    LeaseReceipt,
    LeaseRequest,
)


class GraphError(RuntimeError):
    """GraphService 基础错误（参数校验、graph_id 不匹配等）。"""


# ---------------------------------------------------------------------------
# 基础 schema：事件日志 + fencing 计数器 + 通用 intent / lease 投影
# ---------------------------------------------------------------------------

BASE_SCHEMA = """
CREATE TABLE IF NOT EXISTS graph_events (
    seq INTEGER PRIMARY KEY AUTOINCREMENT,
    ts REAL NOT NULL,
    event_id TEXT NOT NULL UNIQUE,
    event_type TEXT NOT NULL,
    actor_id TEXT NOT NULL DEFAULT 'system',
    idempotency_key TEXT,
    payload TEXT NOT NULL DEFAULT '{}'
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_graph_events_idem
    ON graph_events(idempotency_key) WHERE idempotency_key IS NOT NULL;
CREATE TABLE IF NOT EXISTS graph_counters (
    name TEXT PRIMARY KEY,
    value INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS graph_intents (
    intent_id TEXT PRIMARY KEY,
    goal TEXT NOT NULL DEFAULT '',
    status TEXT NOT NULL DEFAULT 'open',
    claimed_by TEXT,
    fencing_token INTEGER NOT NULL DEFAULT 0,
    lease_until REAL,
    result TEXT,
    payload TEXT NOT NULL DEFAULT '{}',
    created_seq INTEGER NOT NULL,
    updated_seq INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS graph_leases (
    resource_kind TEXT NOT NULL,
    resource_key TEXT NOT NULL,
    owner TEXT NOT NULL DEFAULT '',
    fencing_token INTEGER NOT NULL DEFAULT 0,
    acquired_at REAL NOT NULL,
    expires_at REAL,
    released_at REAL,
    PRIMARY KEY (resource_kind, resource_key)
);
"""


def _ts_to_dt(ts: Optional[float]) -> Optional[datetime]:
    """epoch 秒 → timezone-aware datetime；None 原样返回。"""
    if ts is None:
        return None
    return datetime.fromtimestamp(float(ts), tz=timezone.utc)


class SQLiteGraphBase:
    """三类图共享的 SQLite GraphService 基类（机制层，无领域语义）。

    子类必须定义 ``GRAPH_ID``，按需定义 ``EXTRA_SCHEMA``（领域投影 DDL）、
    ``CLAIM_EVENT_TYPE``（claim 成功时追加的领域事件类型），并实现
    ``_apply_event`` / ``snapshot``。
    """

    #: GraphService 契约里的实现 id（如 "collaboration.graph.v1"）。
    GRAPH_ID = ""
    #: 子类的领域投影 DDL，随基类 schema 一起执行。
    EXTRA_SCHEMA = ""
    #: claim 成功时追加的事件类型；空串表示本图不记录 claim 事件。
    CLAIM_EVENT_TYPE = ""
    DEFAULT_CLAIM_TTL_S = 300
    DEFAULT_LEASE_TTL_S = 600

    def __init__(self, db_path: str | Path) -> None:
        if not self.GRAPH_ID:
            raise GraphError(f"{type(self).__name__} must define GRAPH_ID")
        self.db_path = str(db_path)
        Path(self.db_path).parent.mkdir(parents=True, exist_ok=True)
        self._conn = sqlite3.connect(self.db_path, check_same_thread=False)
        self._lock = threading.RLock()
        cur = self._conn.cursor()
        cur.execute("PRAGMA journal_mode=WAL")
        cur.execute("PRAGMA busy_timeout=5000")
        cur.execute("PRAGMA synchronous=NORMAL")
        cur.execute("PRAGMA foreign_keys=ON")
        self._conn.executescript(BASE_SCHEMA)
        if self.EXTRA_SCHEMA:
            self._conn.executescript(self.EXTRA_SCHEMA)
        self._conn.commit()

    # -- 生命周期 -----------------------------------------------------------

    @property
    def id(self) -> str:
        return self.GRAPH_ID

    def close(self) -> None:
        with self._lock:
            self._conn.close()

    def __enter__(self) -> "SQLiteGraphBase":
        return self

    def __exit__(self, *exc: Any) -> None:
        self.close()

    # -- 内部工具 ------------------------------------------------------------

    def _check_graph_id(self, graph_id: str) -> None:
        """请求里的 graph_id 非空时必须与本图一致，防止跨图误写。"""
        if graph_id and graph_id != self.GRAPH_ID:
            raise GraphError(
                f"graph_id mismatch: request targets {graph_id!r}, "
                f"this is {self.GRAPH_ID!r}"
            )

    def _next_counter(self, name: str) -> int:
        """单调计数器（fencing token 来源）；必须在事务内调用。"""
        self._conn.execute(
            "INSERT INTO graph_counters(name, value) VALUES(?, 1) "
            "ON CONFLICT(name) DO UPDATE SET value = value + 1",
            (name,),
        )
        row = self._conn.execute(
            "SELECT value FROM graph_counters WHERE name=?", (name,)
        ).fetchone()
        return int(row[0])

    def _insert_event(
        self,
        event_type: str,
        actor_id: str,
        payload: dict[str, Any],
        *,
        event_id: Optional[str] = None,
        idempotency_key: Optional[str] = None,
        ts: Optional[float] = None,
    ) -> int:
        """向事件日志追加一行，返回全局 seq；必须在事务内调用。"""
        cur = self._conn.execute(
            "INSERT INTO graph_events "
            "(ts, event_id, event_type, actor_id, idempotency_key, payload) "
            "VALUES (?,?,?,?,?,?)",
            (
                ts if ts is not None else time.time(),
                event_id or new_id("gevt"),
                event_type,
                actor_id or "system",
                idempotency_key,
                json.dumps(payload, ensure_ascii=False, default=str),
            ),
        )
        return int(cur.lastrowid or 0)

    def watermark(self) -> int:
        """事件日志水位（最大 seq；空日志为 0）。"""
        row = self._conn.execute("SELECT MAX(seq) FROM graph_events").fetchone()
        return int(row[0]) if row and row[0] is not None else 0

    def read_events(
        self, *, after_seq: int = 0, limit: int = 1000
    ) -> list[dict[str, Any]]:
        """按全局 seq 升序读取事件（投影核对与测试用）。"""
        rows = self._conn.execute(
            "SELECT seq, ts, event_id, event_type, actor_id, idempotency_key, "
            "payload FROM graph_events WHERE seq > ? ORDER BY seq LIMIT ?",
            (int(after_seq), int(limit)),
        ).fetchall()
        return [
            {
                "seq": int(r[0]),
                "ts": float(r[1]),
                "event_id": r[2],
                "event_type": r[3],
                "actor_id": r[4],
                "idempotency_key": r[5],
                "payload": json.loads(r[6] or "{}"),
            }
            for r in rows
        ]

    # -- 子类钩子 --------------------------------------------------------------

    def _validate_event(self, event: GraphEvent) -> None:
        """写入前的领域校验；默认不限制。校验失败抛 GraphError，事件不落库。"""

    def _apply_event(self, event: GraphEvent, seq: int) -> None:
        """把事件折叠进领域投影；与事件插入处于同一事务。默认无投影。"""

    # -- GraphService.append -------------------------------------------------

    async def append(self, event: GraphEvent) -> GraphReceipt:
        """追加一条事件并同事务更新投影；idempotency_key 去重。"""
        self._check_graph_id(event.graph_id)
        if not event.event_type:
            raise GraphError("event_type is required")
        self._validate_event(event)
        event_id = new_id("gevt")
        with self._lock:
            try:
                with self._conn:  # 事务：事件 + 投影要么都落库，要么都回滚
                    seq = self._insert_event(
                        event.event_type,
                        event.actor_id,
                        dict(event.payload),
                        event_id=event_id,
                        idempotency_key=event.idempotency_key,
                        ts=event.occurred_at.timestamp(),
                    )
                    self._apply_event(event, seq)
            except sqlite3.IntegrityError:
                # event_id 冲突几乎不可能（新生成）；主要是 idempotency_key 重放
                row = None
                if event.idempotency_key:
                    row = self._conn.execute(
                        "SELECT seq, event_id FROM graph_events "
                        "WHERE idempotency_key=?",
                        (event.idempotency_key,),
                    ).fetchone()
                if row is None:
                    raise
                return GraphReceipt(
                    graph_id=self.GRAPH_ID,
                    event_id=str(row[1]),
                    stream_seq=int(row[0]),
                    deduplicated=True,
                )
        return GraphReceipt(
            graph_id=self.GRAPH_ID, event_id=event_id, stream_seq=seq
        )

    # -- GraphService.claim --------------------------------------------------

    async def claim(self, request: ClaimRequest) -> ClaimReceipt:
        """原子领取一个开放 Intent：单事务条件 UPDATE + fencing token。

        状态为 open、或已 claim 但租约过期的工作项才可被领取；并发调用只有
        一个能成功。本图没有 intent 投影时（如 memory.timeline）自然全部失败。
        """
        self._check_graph_id(request.graph_id)
        if not request.intent_id:
            raise GraphError("intent_id is required")
        ttl = float(request.ttl_seconds or self.DEFAULT_CLAIM_TTL_S)
        now = time.time()
        expires = now + ttl
        with self._lock:
            self._conn.execute("BEGIN IMMEDIATE")
            try:
                token = self._next_counter("claim_fencing")
                cur = self._conn.execute(
                    "UPDATE graph_intents SET status='claimed', claimed_by=?, "
                    "fencing_token=?, lease_until=? "
                    "WHERE intent_id=? AND (status='open' OR "
                    "  (status='claimed' AND lease_until IS NOT NULL "
                    "   AND lease_until < ?))",
                    (request.claimant, token, expires, request.intent_id, now),
                )
                won = cur.rowcount == 1
                if won:
                    seq = 0
                    if self.CLAIM_EVENT_TYPE:
                        seq = self._insert_event(
                            self.CLAIM_EVENT_TYPE,
                            request.claimant or "system",
                            {
                                "intent_id": request.intent_id,
                                "fencing_token": token,
                                "lease_until": expires,
                            },
                        )
                        self._conn.execute(
                            "UPDATE graph_intents SET updated_seq=? "
                            "WHERE intent_id=?",
                            (seq, request.intent_id),
                        )
                    self._conn.commit()
                else:
                    # 领取失败不消耗 fencing token，随事务回滚
                    self._conn.rollback()
            except BaseException:
                self._conn.rollback()
                raise
        return ClaimReceipt(
            granted=won,
            graph_id=self.GRAPH_ID,
            intent_id=request.intent_id,
            claimant=request.claimant if won else "",
            fencing_token=token if won else 0,
            expires_at=_ts_to_dt(expires) if won else None,
        )

    # -- GraphService.lease --------------------------------------------------

    async def lease(self, request: LeaseRequest) -> LeaseReceipt:
        """申请图内独占资源租约：fencing token 单调递增，过期或被释放后可转移。"""
        self._check_graph_id(request.graph_id)
        if not request.resource_kind or not request.resource_key:
            raise GraphError("resource_kind and resource_key are required")
        ttl = float(request.ttl_seconds or self.DEFAULT_LEASE_TTL_S)
        now = time.time()
        expires = now + ttl
        owner = request.owner or "system"
        granted = False
        token = 0
        held_by = ""
        held_expires: Optional[float] = None
        with self._lock:
            self._conn.execute("BEGIN IMMEDIATE")
            try:
                row = self._conn.execute(
                    "SELECT owner, fencing_token, expires_at, released_at "
                    "FROM graph_leases WHERE resource_kind=? AND resource_key=?",
                    (request.resource_kind, request.resource_key),
                ).fetchone()
                blocked = False
                if row is not None and row[3] is None:
                    cur_exp = float(row[2]) if row[2] is not None else None
                    if (cur_exp is None or cur_exp > now) and str(row[0]) != owner:
                        # 他人持有的有效租约：不写库，回滚后直接返回失败
                        blocked = True
                        held_by = str(row[0])
                        held_expires = cur_exp
                        token = int(row[1])
                if blocked:
                    self._conn.rollback()
                else:
                    token = self._next_counter("lease_fencing")
                    self._conn.execute(
                        "INSERT INTO graph_leases (resource_kind, resource_key, "
                        "owner, fencing_token, acquired_at, expires_at, "
                        "released_at) VALUES (?,?,?,?,?,?,NULL) "
                        "ON CONFLICT(resource_kind, resource_key) DO UPDATE SET "
                        "owner=excluded.owner, fencing_token=excluded.fencing_token, "
                        "acquired_at=excluded.acquired_at, "
                        "expires_at=excluded.expires_at, released_at=NULL",
                        (
                            request.resource_kind,
                            request.resource_key,
                            owner,
                            token,
                            now,
                            expires,
                        ),
                    )
                    granted = True
                    self._conn.commit()
            except BaseException:
                self._conn.rollback()
                raise
        return LeaseReceipt(
            granted=granted,
            graph_id=self.GRAPH_ID,
            resource_kind=request.resource_kind,
            resource_key=request.resource_key,
            owner=owner if granted else held_by,
            fencing_token=token,
            expires_at=_ts_to_dt(expires if granted else held_expires),
        )

    def release_lease(self, resource_kind: str, resource_key: str) -> bool:
        """释放租约（投影层工具方法，不属于 GraphService 四个接口）。"""
        with self._lock, self._conn:
            cur = self._conn.execute(
                "UPDATE graph_leases SET released_at=? "
                "WHERE resource_kind=? AND resource_key=? AND released_at IS NULL",
                (time.time(), resource_kind, resource_key),
            )
        return cur.rowcount > 0

    # -- 投影读取辅助（供子类 snapshot 使用） -----------------------------------

    def _load_intents(
        self, *, status: Optional[str] = None
    ) -> list[dict[str, Any]]:
        sql = (
            "SELECT intent_id, goal, status, claimed_by, fencing_token, "
            "lease_until, result, created_seq, updated_seq FROM graph_intents"
        )
        params: tuple[Any, ...] = ()
        if status:
            sql += " WHERE status=?"
            params = (status,)
        sql += " ORDER BY created_seq"
        rows = self._conn.execute(sql, params).fetchall()
        return [
            {
                "intent_id": r[0],
                "goal": r[1],
                "status": r[2],
                "claimed_by": r[3] or "",
                "fencing_token": int(r[4]),
                "lease_until": r[5],
                "result": r[6] or "",
                "created_seq": int(r[7]),
                "updated_seq": int(r[8]),
            }
            for r in rows
        ]

    def _load_leases(self) -> list[dict[str, Any]]:
        rows = self._conn.execute(
            "SELECT resource_kind, resource_key, owner, fencing_token, "
            "acquired_at, expires_at, released_at FROM graph_leases "
            "ORDER BY acquired_at"
        ).fetchall()
        return [
            {
                "resource_kind": r[0],
                "resource_key": r[1],
                "owner": r[2],
                "fencing_token": int(r[3]),
                "acquired_at": r[4],
                "expires_at": r[5],
                "released_at": r[6],
            }
            for r in rows
        ]
