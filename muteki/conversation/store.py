"""ConversationStore：Conversation 私有读模型与 Turn/Run 记录的持久化（CONV-01）。

通用对象（Project / Workspace / Thread / Task / AgentSession / Artifact）
继续由 CORE-02 ``PlatformStore`` 持久化；本 Store 只负责 Conversation
私有的表，与 platform.db 同库同事务连接（``conv_`` 前缀）：

- ``conv_turns``：Turn 记录；``(thread_id, idempotency_key)`` 部分唯一索引
  实现 Turn 级幂等（命令层幂等之外的崩溃恢复兜底）；
- ``conv_runs``：Turn 的 Task / Run / 执行代记录（RunRef 不在 CORE-02
  对象表内，故由 Conversation 自管）；
- ``conv_messages``：投影出的对话消息；
- ``conv_thread_runtime``：Thread 的 Runtime 选择；
- ``conv_thread_state``：Thread 读模型（unread / running / pending …）。

Artifact 内容本体写在 ``<db 目录>/conversation_artifacts/<sha256>``，
platform.db 只存内容寻址的 Artifact 行。
"""

from __future__ import annotations

import sqlite3
import threading
from pathlib import Path
from typing import Any, Optional

from muteki.platform.store import PlatformStore
from muteki.platform.contracts.base import utcnow

from .models import (
    TURN_SUPERSEDED,
    ConversationMessage,
    QueuedTurnRequest,
    ThreadRuntimeSelection,
    ThreadState,
    TurnRecord,
    TurnRunRef,
)

_SCHEMA = """
CREATE TABLE IF NOT EXISTS conv_turns (
    turn_id TEXT PRIMARY KEY,
    thread_id TEXT NOT NULL,
    seq INTEGER NOT NULL,
    status TEXT NOT NULL,
    command_id TEXT NOT NULL DEFAULT '',
    idempotency_key TEXT,
    payload TEXT NOT NULL
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_conv_turns_idem
    ON conv_turns (thread_id, idempotency_key)
    WHERE idempotency_key IS NOT NULL AND idempotency_key != '';
CREATE INDEX IF NOT EXISTS idx_conv_turns_thread ON conv_turns (thread_id, seq);

CREATE TABLE IF NOT EXISTS conv_active_turn_claims (
    thread_id TEXT PRIMARY KEY,
    turn_id TEXT NOT NULL UNIQUE,
    claimed_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS conv_runs (
    run_id TEXT PRIMARY KEY,
    turn_id TEXT NOT NULL,
    thread_id TEXT NOT NULL,
    generation INTEGER NOT NULL,
    status TEXT NOT NULL,
    payload TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_conv_runs_thread ON conv_runs (thread_id);

CREATE TABLE IF NOT EXISTS conv_messages (
    message_id TEXT PRIMARY KEY,
    thread_id TEXT NOT NULL,
    turn_id TEXT,
    role TEXT NOT NULL,
    stream_seq INTEGER NOT NULL DEFAULT 0,
    payload TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_conv_messages_thread
    ON conv_messages (thread_id, stream_seq);

CREATE TABLE IF NOT EXISTS conv_thread_runtime (
    thread_id TEXT PRIMARY KEY,
    payload TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS conv_thread_state (
    thread_id TEXT PRIMARY KEY,
    payload TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS conv_turn_queue (
    queue_id TEXT PRIMARY KEY,
    thread_id TEXT NOT NULL,
    position INTEGER NOT NULL,
    status TEXT NOT NULL,
    idempotency_key TEXT NOT NULL DEFAULT '',
    payload TEXT NOT NULL
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_conv_turn_queue_idem
    ON conv_turn_queue (thread_id, idempotency_key)
    WHERE idempotency_key != '';
CREATE INDEX IF NOT EXISTS idx_conv_turn_queue_active
    ON conv_turn_queue (thread_id, status, position);

CREATE TABLE IF NOT EXISTS conv_sidebar_prefs (
    pref_key TEXT PRIMARY KEY,
    payload  TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    version INTEGER NOT NULL DEFAULT 0
);
"""


class ActiveTurnConflict(RuntimeError):
    """同一 Thread 已有 queued/running Turn。"""


class ConversationStore:
    """Conversation 私有表的读写入口（线程安全，复用 PlatformStore 连接与锁）。"""

    def __init__(self, store: PlatformStore) -> None:
        self._store = store
        with store.lock, store.conn:
            store.conn.executescript(_SCHEMA)
            self._ensure_message_fts()

    # -- 内部 ---------------------------------------------------------------

    @property
    def _conn(self) -> sqlite3.Connection:
        return self._store.conn

    @property
    def _lock(self) -> threading.RLock:
        """Share PlatformStore's lock for the shared sqlite connection.

        A second RLock raced with ``append_events`` / SSE reads on the same
        handle and surfaced as ``InterfaceError: bad parameter or other API
        misuse``, aborting long streaming turns (#138).
        """
        return self._store.lock

    def _execute(self, sql: str, params: tuple | list = ()) -> sqlite3.Cursor:
        with self._lock:
            return self._conn.execute(sql, params)

    def _fetchone(self, sql: str, params: tuple | list = ()):
        with self._lock:
            return self._conn.execute(sql, params).fetchone()

    def _fetchall(self, sql: str, params: tuple | list = ()):
        with self._lock:
            return self._conn.execute(sql, params).fetchall()

    @staticmethod
    def _dump(model: Any) -> str:
        return model.model_dump_json()

    # -- Turn ---------------------------------------------------------------

    def save_turn(self, turn: TurnRecord) -> TurnRecord:
        with self._store.transaction():
            self._execute(
                "INSERT INTO conv_turns (turn_id, thread_id, seq, status, "
                "command_id, idempotency_key, payload) VALUES (?,?,?,?,?,?,?) "
                "ON CONFLICT(turn_id) DO UPDATE SET status=excluded.status, "
                "payload=excluded.payload",
                (turn.turn_id, turn.thread_id, turn.seq, turn.status,
                 turn.command_id, turn.idempotency_key, self._dump(turn)),
            )
        return turn

    def commit_history_rewind(self, turns: list[TurnRecord], runs: list[TurnRunRef], state: ThreadState) -> None:
        """Commit branch visibility, run states and thread state in one transaction."""
        with self._store.transaction():
            for turn in turns:
                self._execute("UPDATE conv_turns SET status=?, payload=? WHERE turn_id=?",
                              (turn.status, self._dump(turn), turn.turn_id))
                self._execute("DELETE FROM conv_active_turn_claims WHERE thread_id=? AND turn_id=?",
                              (turn.thread_id, turn.turn_id))
            for run in runs:
                self._execute("UPDATE conv_runs SET status=?, payload=? WHERE run_id=?",
                              (run.status, self._dump(run), run.run_id))
            self._execute("INSERT OR REPLACE INTO conv_thread_state (thread_id, payload) VALUES (?,?)",
                          (state.thread_id, self._dump(state)))

    def get_turn(self, turn_id: str) -> Optional[TurnRecord]:
        row = self._fetchone(
            "SELECT payload FROM conv_turns WHERE turn_id = ?",
            (turn_id,))
        return TurnRecord.model_validate_json(row["payload"]) if row else None

    def find_turn_by_idempotency(
        self, thread_id: str, idempotency_key: str
    ) -> Optional[TurnRecord]:
        """Turn 级幂等查找：同 (thread, key) 返回已存在的 Turn。"""
        if not idempotency_key:
            return None
        row = self._fetchone(
            "SELECT payload FROM conv_turns WHERE thread_id = ? AND "
            "idempotency_key = ?", (thread_id, idempotency_key))
        return TurnRecord.model_validate_json(row["payload"]) if row else None

    def list_turns(self, thread_id: str) -> list[TurnRecord]:
        rows = self._fetchall(
            "SELECT payload FROM conv_turns WHERE thread_id = ? ORDER BY seq",
            (thread_id,))
        return [TurnRecord.model_validate_json(r["payload"]) for r in rows]

    def list_current_turns(self, thread_id: str) -> list[TurnRecord]:
        """Return the active conversation branch, excluding replaced attempts."""
        return [
            turn for turn in self.list_turns(thread_id)
            if turn.status != TURN_SUPERSEDED
        ]

    def list_active_turns(self) -> list[TurnRecord]:
        rows = self._fetchall(
            "SELECT payload FROM conv_turns WHERE status IN ('queued','running') "
            "ORDER BY thread_id, seq"
        )
        return [TurnRecord.model_validate_json(r["payload"]) for r in rows]

    def claim_active_turn(self, thread_id: str, turn_id: str) -> None:
        """原子领取 Thread 执行权；终态遗留 claim 会在本次领取前清理。"""
        from muteki.platform.contracts.base import utcnow

        with self._store.transaction():
            row = self._fetchone(
                "SELECT turn_id FROM conv_active_turn_claims WHERE thread_id = ?",
                (thread_id,),
            )
            if row is not None:
                active = self.get_turn(str(row["turn_id"]))
                if active is not None and active.status in {"queued", "running"}:
                    raise ActiveTurnConflict(
                        f"thread {thread_id} already has active turn "
                        f"{active.turn_id}"
                    )
                self._execute(
                    "DELETE FROM conv_active_turn_claims WHERE thread_id = ?",
                    (thread_id,),
                )
            try:
                self._execute(
                    "INSERT INTO conv_active_turn_claims "
                    "(thread_id, turn_id, claimed_at) VALUES (?,?,?)",
                    (thread_id, turn_id, utcnow().isoformat()),
                )
            except sqlite3.IntegrityError as exc:
                raise ActiveTurnConflict(
                    f"thread {thread_id} already has an active turn"
                ) from exc

    def release_active_turn(self, thread_id: str, turn_id: str = "") -> bool:
        with self._store.transaction():
            if turn_id:
                cursor = self._execute(
                    "DELETE FROM conv_active_turn_claims "
                    "WHERE thread_id = ? AND turn_id = ?",
                    (thread_id, turn_id),
                )
            else:
                cursor = self._execute(
                    "DELETE FROM conv_active_turn_claims WHERE thread_id = ?",
                    (thread_id,),
                )
        return bool(cursor.rowcount)

    def next_turn_seq(self, thread_id: str) -> int:
        row = self._fetchone(
            "SELECT MAX(seq) FROM conv_turns WHERE thread_id = ?",
            (thread_id,))
        return (int(row[0]) if row and row[0] is not None else 0) + 1

    def active_turn_id(self, thread_id: str) -> Optional[str]:
        row = self._fetchone(
            "SELECT turn_id FROM conv_active_turn_claims WHERE thread_id = ?",
            (thread_id,),
        )
        return str(row["turn_id"]) if row else None

    # -- 后续消息队列 ---------------------------------------------------------

    @staticmethod
    def _queue_statuses() -> tuple[str, str, str]:
        return ("queued", "dispatching", "failed")

    def _active_queue_rows(self, thread_id: str) -> list[sqlite3.Row]:
        return self._fetchall(
            "SELECT payload FROM conv_turn_queue WHERE thread_id = ? "
            "AND status IN ('queued','dispatching','failed') "
            "ORDER BY position, rowid",
            (thread_id,),
        )

    def _sync_queue_summary(
        self,
        thread_id: str,
        *,
        paused: Optional[bool] = None,
        pause_reason: Optional[str] = None,
        failed_item_id: Any = ...,
        bump_revision: bool = True,
    ) -> ThreadState:
        state = self.get_state(thread_id)
        updates: dict[str, Any] = {
            "queue_count": len(self._active_queue_rows(thread_id)),
            "queue_revision": state.queue_revision + (1 if bump_revision else 0),
        }
        if paused is not None:
            updates["queue_paused"] = paused
        if pause_reason is not None:
            updates["queue_pause_reason"] = pause_reason
        if failed_item_id is not ...:
            updates["queue_failed_item_id"] = failed_item_id
        return self.save_state(state.model_copy(update=updates))

    def enqueue_turn(self, item: QueuedTurnRequest) -> tuple[QueuedTurnRequest, bool]:
        """幂等入队；返回（队列项，是否新建）。"""
        with self._store.transaction():
            if item.idempotency_key:
                row = self._fetchone(
                    "SELECT payload FROM conv_turn_queue WHERE thread_id = ? "
                    "AND idempotency_key = ?",
                    (item.thread_id, item.idempotency_key),
                )
                if row:
                    return QueuedTurnRequest.model_validate_json(row["payload"]), False
            row = self._fetchone(
                "SELECT MAX(position) FROM conv_turn_queue WHERE thread_id = ? "
                "AND status IN ('queued','dispatching','failed')",
                (item.thread_id,),
            )
            position = (int(row[0]) if row and row[0] is not None else 0) + 1
            item = item.model_copy(update={"position": position})
            self._execute(
                "INSERT INTO conv_turn_queue "
                "(queue_id, thread_id, position, status, idempotency_key, payload) "
                "VALUES (?,?,?,?,?,?)",
                (item.queue_id, item.thread_id, item.position, item.status,
                 item.idempotency_key, self._dump(item)),
            )
            self._sync_queue_summary(item.thread_id)
        return item, True

    def get_queue_item(self, queue_id: str) -> Optional[QueuedTurnRequest]:
        row = self._fetchone(
            "SELECT payload FROM conv_turn_queue WHERE queue_id = ?", (queue_id,)
        )
        return QueuedTurnRequest.model_validate_json(row["payload"]) if row else None

    def find_queue_by_idempotency(
        self, thread_id: str, idempotency_key: str
    ) -> Optional[QueuedTurnRequest]:
        if not idempotency_key:
            return None
        row = self._fetchone(
            "SELECT payload FROM conv_turn_queue WHERE thread_id = ? "
            "AND idempotency_key = ?",
            (thread_id, idempotency_key),
        )
        return QueuedTurnRequest.model_validate_json(row["payload"]) if row else None

    def list_queue(self, thread_id: str) -> list[QueuedTurnRequest]:
        return [
            QueuedTurnRequest.model_validate_json(row["payload"])
            for row in self._active_queue_rows(thread_id)
        ]

    def next_queue_item(self, thread_id: str) -> Optional[QueuedTurnRequest]:
        rows = self.list_queue(thread_id)
        return rows[0] if rows else None

    def save_queue_item(
        self, item: QueuedTurnRequest, *, bump_revision: bool = True
    ) -> QueuedTurnRequest:
        with self._store.transaction():
            self._execute(
                "UPDATE conv_turn_queue SET position = ?, status = ?, payload = ? "
                "WHERE queue_id = ?",
                (item.position, item.status, self._dump(item), item.queue_id),
            )
            self._sync_queue_summary(item.thread_id, bump_revision=bump_revision)
        return item

    def update_queue_item(
        self, thread_id: str, queue_id: str, *, text: str,
        runtime: Optional[dict[str, Any]] = None,
        capability_refs: Optional[list[dict[str, Any]]] = None,
        runtime_invocation: Optional[dict[str, Any]] = None,
        expected_revision: Optional[int] = None,
    ) -> QueuedTurnRequest:
        # Do not let a concurrent promotion/cancellation slip between the state
        # check and save and resurrect a consumed item as a queued draft.
        with self._store.transaction():
            if expected_revision is not None and self.get_state(thread_id).queue_revision != expected_revision:
                raise ValueError("队列版本已改变，请刷新后比较当前正文与编辑草稿")
            item = self.get_queue_item(queue_id)
            if item is None or item.thread_id != thread_id:
                raise LookupError(f"unknown queue item: {queue_id}")
            if item.status not in {"queued", "failed"}:
                raise ValueError("只有等待中或发送失败的队列消息可以编辑")
            return self.save_queue_item(item.model_copy(update={
                "text": text,
                **({"runtime": runtime} if runtime is not None else {}),
                **({"capability_refs": capability_refs} if capability_refs is not None else {}),
                **({"runtime_invocation": runtime_invocation} if runtime_invocation is not None else {}),
                "status": "queued",
                "error": {},
                "updated_at": utcnow(),
            }))

    def cancel_queue_item(self, thread_id: str, queue_id: str) -> QueuedTurnRequest:
        with self._lock:
            item = self.get_queue_item(queue_id)
            if item is None or item.thread_id != thread_id:
                raise LookupError(f"unknown queue item: {queue_id}")
            if item.status == "dispatching":
                raise ValueError("队列消息正在发送，不能删除")
            updated = item.model_copy(update={"status": "cancelled", "updated_at": utcnow()})
            return self.save_queue_item(updated)

    def reorder_queue(
        self, thread_id: str, queue_ids: list[str], *, expected_revision: Optional[int] = None
    ) -> list[QueuedTurnRequest]:
        with self._store.transaction():
            state = self.get_state(thread_id)
            if expected_revision is not None and expected_revision != state.queue_revision:
                raise ValueError("队列已变化，请刷新后重试")
            current = self.list_queue(thread_id)
            current_ids = [item.queue_id for item in current]
            if set(queue_ids) != set(current_ids) or len(queue_ids) != len(current_ids):
                raise ValueError("queue_ids 必须完整包含当前队列项")
            by_id = {item.queue_id: item for item in current}
            reordered: list[QueuedTurnRequest] = []
            for position, queue_id in enumerate(queue_ids, 1):
                item = by_id[queue_id].model_copy(update={
                    "position": position,
                    "updated_at": utcnow(),
                })
                self._execute(
                    "UPDATE conv_turn_queue SET position = ?, payload = ? WHERE queue_id = ?",
                    (position, self._dump(item), queue_id),
                )
                reordered.append(item)
            self._sync_queue_summary(thread_id)
            return reordered

    def mark_queue_dispatching(self, queue_id: str) -> QueuedTurnRequest:
        with self._lock:
            item = self.get_queue_item(queue_id)
            if item is None:
                raise LookupError(f"unknown queue item: {queue_id}")
            if item.status != "queued":
                raise ValueError("队列消息已被修改、取消或发送，请刷新队列")
            return self.save_queue_item(item.model_copy(update={
                "status": "dispatching", "error": {}, "updated_at": utcnow(),
            }))

    def mark_queue_consumed(self, queue_id: str, turn_id: str) -> QueuedTurnRequest:
        item = self.get_queue_item(queue_id)
        if item is None:
            raise LookupError(f"unknown queue item: {queue_id}")
        return self.save_queue_item(item.model_copy(update={
            "status": "consumed",
            "promoted_turn_id": turn_id,
            "updated_at": utcnow(),
        }))

    def mark_queue_failed(self, queue_id: str, error: dict[str, Any]) -> QueuedTurnRequest:
        item = self.get_queue_item(queue_id)
        if item is None:
            raise LookupError(f"unknown queue item: {queue_id}")
        updated = item.model_copy(update={
            "status": "failed", "error": error, "updated_at": utcnow(),
        })
        with self._store.transaction():
            self._execute(
                "UPDATE conv_turn_queue SET status = ?, payload = ? WHERE queue_id = ?",
                (updated.status, self._dump(updated), queue_id),
            )
            self._sync_queue_summary(
                item.thread_id,
                paused=True,
                pause_reason="dispatch_failed",
                failed_item_id=queue_id,
            )
        return updated

    def pause_queue(self, thread_id: str, reason: str) -> ThreadState:
        return self._sync_queue_summary(
            thread_id, paused=True, pause_reason=reason, bump_revision=True,
        )

    def resume_queue(self, thread_id: str) -> ThreadState:
        with self._store.transaction():
            for item in self.list_queue(thread_id):
                if item.status != "failed":
                    continue
                updated = item.model_copy(update={
                    "status": "queued", "error": {}, "updated_at": utcnow(),
                })
                self._execute(
                    "UPDATE conv_turn_queue SET status = ?, payload = ? WHERE queue_id = ?",
                    (updated.status, self._dump(updated), updated.queue_id),
                )
            return self._sync_queue_summary(
                thread_id,
                paused=False,
                pause_reason="",
                failed_item_id=None,
                bump_revision=True,
            )

    # -- Run -----------------------------------------------------------------

    def save_run(self, run: TurnRunRef) -> TurnRunRef:
        with self._store.transaction():
            self._execute(
                "INSERT INTO conv_runs (run_id, turn_id, thread_id, generation, "
                "status, payload) VALUES (?,?,?,?,?,?) "
                "ON CONFLICT(run_id) DO UPDATE SET status=excluded.status, "
                "payload=excluded.payload",
                (run.run_id, run.turn_id, run.thread_id, run.generation,
                 run.status, self._dump(run)),
            )
        return run

    def get_run(self, run_id: str) -> Optional[TurnRunRef]:
        row = self._fetchone(
            "SELECT payload FROM conv_runs WHERE run_id = ?", (run_id,))
        return TurnRunRef.model_validate_json(row["payload"]) if row else None

    def list_runs(self, thread_id: str) -> list[TurnRunRef]:
        rows = self._fetchall(
            "SELECT payload FROM conv_runs WHERE thread_id = ? "
            "ORDER BY rowid", (thread_id,))
        return [TurnRunRef.model_validate_json(r["payload"]) for r in rows]

    def list_current_runs(self, thread_id: str) -> list[TurnRunRef]:
        """Return runs that belong to the current conversation branch."""
        current_turn_ids = {
            turn.turn_id for turn in self.list_current_turns(thread_id)
        }
        return [
            run for run in self.list_runs(thread_id)
            if run.turn_id in current_turn_ids
        ]

    # -- 消息 -------------------------------------------------------------------

    def save_message(self, message: ConversationMessage) -> ConversationMessage:
        with self._store.transaction():
            self._execute(
                "INSERT OR REPLACE INTO conv_messages (message_id, thread_id, "
                "turn_id, role, stream_seq, payload) VALUES (?,?,?,?,?,?)",
                (message.message_id, message.thread_id, message.turn_id,
                 message.role, message.stream_seq, self._dump(message)),
            )
            self._upsert_message_fts(message)
        return message

    def get_message(self, message_id: str) -> Optional[ConversationMessage]:
        row = self._fetchone(
            "SELECT payload FROM conv_messages WHERE message_id = ?",
            (message_id,))
        return (ConversationMessage.model_validate_json(row["payload"])
                if row else None)

    def list_messages(self, thread_id: str) -> list[ConversationMessage]:
        rows = self._fetchall(
            "SELECT payload FROM conv_messages WHERE thread_id = ? "
            "ORDER BY stream_seq, rowid", (thread_id,))
        return [ConversationMessage.model_validate_json(r["payload"]) for r in rows]

    def list_current_messages(self, thread_id: str) -> list[ConversationMessage]:
        """Return messages that belong to the current, non-superseded branch."""
        superseded = {
            turn.turn_id for turn in self.list_turns(thread_id)
            if turn.status == TURN_SUPERSEDED
        }
        return [
            message for message in self.list_messages(thread_id)
            if not message.turn_id or message.turn_id not in superseded
        ]

    def list_messages_page(
        self,
        thread_id: str,
        *,
        limit: int = 50,
        before_stream_seq: Optional[int] = None,
        after_stream_seq: Optional[int] = None,
        current_branch_only: bool = True,
    ) -> dict[str, Any]:
        """Return a chronological page of messages with stable stream_seq cursors.

        - No cursor → newest ``limit`` messages (ascending within the page).
        - ``before_stream_seq`` → older page (strictly less than cursor).
        - ``after_stream_seq`` → newer page (strictly greater than cursor).
        Superseded-turn messages are excluded when ``current_branch_only``.
        """
        page_limit = max(1, int(limit))
        # Fetch one extra row to detect has_more without a second full scan.
        fetch_limit = page_limit + 1
        branch_clause = ""
        params: list[Any] = [thread_id]
        if current_branch_only:
            branch_clause = (
                " AND (m.turn_id IS NULL OR m.turn_id = '' OR m.turn_id NOT IN ("
                " SELECT turn_id FROM conv_turns"
                " WHERE thread_id = ? AND status = ?"
                " ))"
            )
            params.extend([thread_id, TURN_SUPERSEDED])

        cursor_clause = ""
        if before_stream_seq is not None and after_stream_seq is not None:
            raise ValueError("before_stream_seq and after_stream_seq are mutually exclusive")
        if before_stream_seq is not None:
            cursor_clause = " AND m.stream_seq < ?"
            params.append(int(before_stream_seq))
            order = "m.stream_seq DESC, m.rowid DESC"
        elif after_stream_seq is not None:
            cursor_clause = " AND m.stream_seq > ?"
            params.append(int(after_stream_seq))
            order = "m.stream_seq ASC, m.rowid ASC"
        else:
            # Newest page: pull descending then reverse to chronological.
            order = "m.stream_seq DESC, m.rowid DESC"

        params.append(fetch_limit)
        rows = self._fetchall(
            "SELECT m.payload FROM conv_messages m"
            " WHERE m.thread_id = ?"
            f"{branch_clause}{cursor_clause}"
            f" ORDER BY {order} LIMIT ?",
            params,
        )
        messages = [
            ConversationMessage.model_validate_json(row["payload"]) for row in rows
        ]
        has_extra = len(messages) > page_limit
        if has_extra:
            messages = messages[:page_limit]
        # Descending fetches (newest page / before cursor) → chronological.
        if after_stream_seq is None:
            messages = list(reversed(messages))

        oldest = messages[0] if messages else None
        newest = messages[-1] if messages else None
        oldest_seq = oldest.stream_seq if oldest else None
        newest_seq = newest.stream_seq if newest else None

        def _exists_beyond(cmp: str, seq: Optional[int]) -> bool:
            if seq is None:
                return False
            check_params: list[Any] = [thread_id]
            check_branch = ""
            if current_branch_only:
                check_branch = (
                    " AND (m.turn_id IS NULL OR m.turn_id = '' OR m.turn_id NOT IN ("
                    " SELECT turn_id FROM conv_turns"
                    " WHERE thread_id = ? AND status = ?"
                    " ))"
                )
                check_params.extend([thread_id, TURN_SUPERSEDED])
            check_params.append(int(seq))
            row = self._fetchone(
                "SELECT 1 FROM conv_messages m"
                " WHERE m.thread_id = ?"
                f"{check_branch} AND m.stream_seq {cmp} ? LIMIT 1",
                check_params,
            )
            return row is not None

        if before_stream_seq is not None:
            has_more_before = has_extra or _exists_beyond("<", oldest_seq)
            has_more_after = _exists_beyond(">", newest_seq) if newest_seq is not None else True
        elif after_stream_seq is not None:
            has_more_after = has_extra or _exists_beyond(">", newest_seq)
            has_more_before = _exists_beyond("<", oldest_seq) if oldest_seq is not None else True
        else:
            has_more_before = has_extra or _exists_beyond("<", oldest_seq)
            has_more_after = False

        return {
            "messages": messages,
            "messages_page": {
                "limit": page_limit,
                "oldest_stream_seq": oldest_seq,
                "newest_stream_seq": newest_seq,
                "oldest_message_id": oldest.message_id if oldest else None,
                "newest_message_id": newest.message_id if newest else None,
                "has_more_before": bool(has_more_before),
                "has_more_after": bool(has_more_after),
            },
        }

    # -- Runtime 选择 -------------------------------------------------------------


    # -- Message FTS (C07) -------------------------------------------------------

    def _ensure_message_fts(self) -> None:
        """Create FTS5 trigram index and backfill once if empty."""
        self._execute(
            "CREATE VIRTUAL TABLE IF NOT EXISTS conv_messages_fts USING fts5("
            " message_id UNINDEXED,"
            " thread_id UNINDEXED,"
            " turn_id UNINDEXED,"
            " role UNINDEXED,"
            " stream_seq UNINDEXED,"
            " text,"
            " tokenize='trigram'"
            ")"
        )
        row = self._fetchone(
            "SELECT COUNT(*) AS n FROM conv_messages_fts"
        )
        indexed = int(row["n"] if row and row["n"] is not None else 0)
        if indexed:
            return
        total = self._fetchone(
            "SELECT COUNT(*) AS n FROM conv_messages"
        )
        if not total or int(total["n"] or 0) == 0:
            return
        self.backfill_message_fts()

    def _upsert_message_fts(self, message: ConversationMessage) -> None:
        text_value = str(message.text or "")
        self._execute(
            "DELETE FROM conv_messages_fts WHERE message_id = ?",
            (message.message_id,),
        )
        if not text_value.strip():
            return
        self._execute(
            "INSERT INTO conv_messages_fts "
            "(message_id, thread_id, turn_id, role, stream_seq, text) "
            "VALUES (?,?,?,?,?,?)",
            (
                message.message_id,
                message.thread_id,
                message.turn_id or "",
                message.role,
                int(message.stream_seq or 0),
                text_value,
            ),
        )

    def backfill_message_fts(self) -> int:
        """Rebuild FTS from conv_messages payloads. Returns indexed row count."""
        with self._store.transaction():
            self._execute(
                "CREATE VIRTUAL TABLE IF NOT EXISTS conv_messages_fts USING fts5("
                " message_id UNINDEXED,"
                " thread_id UNINDEXED,"
                " turn_id UNINDEXED,"
                " role UNINDEXED,"
                " stream_seq UNINDEXED,"
                " text,"
                " tokenize='trigram'"
                ")"
            )
            self._execute("DELETE FROM conv_messages_fts")
            rows = self._fetchall(
                "SELECT payload FROM conv_messages"
            )
            count = 0
            for row in rows:
                message = ConversationMessage.model_validate_json(row["payload"])
                if not str(message.text or "").strip():
                    continue
                self._execute(
                    "INSERT INTO conv_messages_fts "
                    "(message_id, thread_id, turn_id, role, stream_seq, text) "
                    "VALUES (?,?,?,?,?,?)",
                    (
                        message.message_id,
                        message.thread_id,
                        message.turn_id or "",
                        message.role,
                        int(message.stream_seq or 0),
                        message.text,
                    ),
                )
                count += 1
        return count

    @staticmethod
    def _snippet_for(text: str, query: str, *, radius: int = 42) -> str:
        body = str(text or "")
        needle = str(query or "")
        if not body:
            return ""
        if not needle:
            return body[: radius * 2]
        lower_body = body.casefold()
        lower_needle = needle.casefold()
        pos = lower_body.find(lower_needle)
        if pos < 0:
            pos = body.find(needle)
        if pos < 0:
            return body[: radius * 2]
        start = max(0, pos - radius)
        end = min(len(body), pos + len(needle) + radius)
        chunk = body[start:end]
        rel = pos - start
        return (
            ("…" if start > 0 else "")
            + chunk[:rel]
            + "«"
            + chunk[rel : rel + len(needle)]
            + "»"
            + chunk[rel + len(needle) :]
            + ("…" if end < len(body) else "")
        )

    def search_messages(
        self,
        query: str,
        *,
        limit: int = 30,
        include_superseded: bool = False,
        thread_ids: Optional[list[str]] = None,
        offset: int = 0,
    ) -> list[dict[str, Any]]:
        """Full-text search over projected message bodies (C07).

        FTS5 trigram MATCH for queries ≥3 chars; LIKE fallback for short
        CJK/Latin tokens so 1–2 character Chinese queries still hit.
        """
        q = str(query or "").strip()
        if not q:
            return []
        page_limit = max(1, min(int(limit), 101))
        self._ensure_message_fts()

        clauses: list[str] = []
        params: list[Any] = []
        use_fts = len(q) >= 3
        if use_fts:
            safe = q.replace('"', '""')
            clauses.append("conv_messages_fts MATCH ?")
            params.append(f'"{safe}"')
        else:
            escaped = (
                q.replace("\\", "\\\\")
                .replace("%", "\\%")
                .replace("_", "\\_")
            )
            clauses.append("text LIKE ? ESCAPE '\\'")
            params.append(f"%{escaped}%")

        if not include_superseded:
            clauses.append(
                "(turn_id = '' OR turn_id NOT IN ("
                " SELECT turn_id FROM conv_turns WHERE status = ?"
                "))"
            )
            params.append(TURN_SUPERSEDED)

        if thread_ids is not None:
            if not thread_ids:
                return []
            placeholders = ",".join("?" for _ in thread_ids)
            clauses.append(f"thread_id IN ({placeholders})")
            params.extend(list(thread_ids))

        params.extend([page_limit, max(0, int(offset))])
        where = " AND ".join(clauses)
        rows = self._fetchall(
            "SELECT message_id, thread_id, turn_id, role, stream_seq, text "
            "FROM conv_messages_fts "
            f"WHERE {where} "
            "ORDER BY (SELECT json_extract(m.payload,'$.created_at') FROM conv_messages m "
            "WHERE m.message_id=conv_messages_fts.message_id) DESC, "
            "stream_seq DESC, message_id DESC LIMIT ? OFFSET ?",
            params,
        )

        turn_cache: dict[str, set[str]] = {}
        hits: list[dict[str, Any]] = []
        for row in rows:
            body = str(row["text"] or "")
            thread_id = str(row["thread_id"])
            turn_id = str(row["turn_id"] or "") or None
            superseded = False
            if include_superseded and turn_id:
                if thread_id not in turn_cache:
                    turn_cache[thread_id] = {
                        turn.turn_id for turn in self.list_turns(thread_id)
                        if turn.status == TURN_SUPERSEDED
                    }
                superseded = turn_id in turn_cache[thread_id]
            hits.append({
                "message_id": row["message_id"],
                "thread_id": thread_id,
                "turn_id": turn_id,
                "role": row["role"],
                "stream_seq": int(row["stream_seq"] or 0),
                "text": body,
                "snippet": self._snippet_for(body, q),
                "superseded": superseded,
            })
        return hits

    def get_message_stream_seq(
        self, thread_id: str, message_id: str,
    ) -> Optional[int]:
        row = self._fetchone(
            "SELECT stream_seq FROM conv_messages "
            "WHERE thread_id = ? AND message_id = ?",
            (thread_id, message_id),
        )
        return int(row["stream_seq"]) if row else None

    def save_runtime_selection(
        self, selection: ThreadRuntimeSelection
    ) -> ThreadRuntimeSelection:
        with self._store.transaction():
            self._execute(
                "INSERT OR REPLACE INTO conv_thread_runtime (thread_id, payload) "
                "VALUES (?,?)", (selection.thread_id, self._dump(selection)))
        return selection

    def get_runtime_selection(
        self, thread_id: str
    ) -> Optional[ThreadRuntimeSelection]:
        row = self._fetchone(
            "SELECT payload FROM conv_thread_runtime WHERE thread_id = ?",
            (thread_id,))
        if not row:
            return None
        return ThreadRuntimeSelection.model_validate_json(row["payload"])

    def list_runtime_selections(self) -> list[ThreadRuntimeSelection]:
        """Return all Thread credential/runtime references for catalog usage."""
        rows = self._fetchall(
            "SELECT payload FROM conv_thread_runtime ORDER BY thread_id"
        )
        return [
            ThreadRuntimeSelection.model_validate_json(row["payload"])
            for row in rows
        ]

    def delete_runtime_selection(self, thread_id: str) -> bool:
        with self._store.transaction():
            cursor = self._execute(
                "DELETE FROM conv_thread_runtime WHERE thread_id = ?",
                (thread_id,),
            )
        return bool(cursor.rowcount)

    # -- Thread 读模型 -------------------------------------------------------------

    def save_state(self, state: ThreadState) -> ThreadState:
        with self._store.transaction():
            self._execute(
                "INSERT OR REPLACE INTO conv_thread_state (thread_id, payload) "
                "VALUES (?,?)", (state.thread_id, self._dump(state)))
        return state

    def get_state(self, thread_id: str) -> ThreadState:
        row = self._fetchone(
            "SELECT payload FROM conv_thread_state WHERE thread_id = ?",
            (thread_id,))
        if not row:
            return ThreadState(thread_id=thread_id)
        return ThreadState.model_validate_json(row["payload"])

    def list_states(self) -> list[ThreadState]:
        rows = self._fetchall(
            "SELECT payload FROM conv_thread_state")
        return [ThreadState.model_validate_json(r["payload"]) for r in rows]

    # -- Artifact 内容 ------------------------------------------------------------

    @property
    def artifacts_dir(self) -> Path:
        path = Path(self._store.db_path).parent / "conversation_artifacts"
        path.mkdir(parents=True, exist_ok=True)
        return path

    def write_artifact_content(self, sha256: str, content: bytes) -> Path:
        """写入 Artifact 内容本体（内容寻址，重复写同内容幂等）。"""
        path = self.artifacts_dir / sha256
        if not path.exists():
            tmp = path.with_suffix(".tmp")
            tmp.write_bytes(content)
            tmp.replace(path)
        return path

    def read_artifact_content(self, sha256: str) -> Optional[bytes]:
        path = self.artifacts_dir / sha256
        return path.read_bytes() if path.exists() else None

    # -- Sidebar 偏好（C30）------------------------------------------------------

    def get_sidebar_prefs(self, key: str = "sidebar") -> Optional[dict[str, Any]]:
        """返回 sidebar 偏好 JSON dict（含 version 字段），不存在时返回 None。"""
        import json as _json
        row = self._fetchone(
            "SELECT payload, version FROM conv_sidebar_prefs WHERE pref_key = ?",
            (key,),
        )
        if not row:
            return None
        data = _json.loads(row["payload"])
        data["version"] = row["version"]
        return data

    def save_sidebar_prefs(
        self,
        payload: dict[str, Any],
        key: str = "sidebar",
        expected_version: Optional[int] = None,
    ) -> dict[str, Any]:
        """保存 sidebar 偏好；expected_version 不匹配时抛 OptimisticConcurrencyError。

        返回写入后的完整 dict（含新 version）。
        """
        import json as _json
        from muteki.platform.store import OptimisticConcurrencyError

        with self._store.transaction():
            row = self._fetchone(
                "SELECT version FROM conv_sidebar_prefs WHERE pref_key = ?",
                (key,),
            )
            current_version: int = row["version"] if row else 0
            if expected_version is not None and current_version != expected_version:
                raise OptimisticConcurrencyError(
                    f"sidebar_prefs version conflict: expected {expected_version}, got {current_version}"
                )
            new_version = current_version + 1
            clean_payload = {k: v for k, v in payload.items() if k != "version"}
            serialised = _json.dumps(clean_payload, ensure_ascii=False)
            self._execute(
                "INSERT OR REPLACE INTO conv_sidebar_prefs (pref_key, payload, updated_at, version) "
                "VALUES (?, ?, ?, ?)",
                (key, serialised, utcnow(), new_version),
            )
        result = dict(clean_payload)
        result["version"] = new_version
        return result


__all__ = ["ActiveTurnConflict", "ConversationStore"]
