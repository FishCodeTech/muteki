"""f12 checkpointed-DAG SQLite schema + canonical store helpers.

Design baseline: docs/frameworks_2026/12_checkpointed_dag_swarm.md §5.

All F12 tables live in the SAME SQLite database as the SharedGraph (the graph's
``_conn``/``_lock`` are reused, same pattern as f11_agent_teams). Table creation
and column upgrades are idempotent: an older F12 database re-opens cleanly.

Event kinds are appended to the graph's append-only ``events`` table so the
existing event queries (``events_since`` / eval scripts) read them unchanged.
They describe ORCHESTRATION state only — they are never evidence for a flag.
"""

from __future__ import annotations

import json
import sqlite3
import time
from typing import Any

F12_FEATURE_KINDS = (
    "f12_execution_created",
    "f12_plan_revision_created",
    "f12_plan_revision_rejected",
    "f12_plan_revision_accepted",
    "f12_plan_materialized",
    "f12_source_woken",
    "f12_schedule_selected",
    "f12_task_state_changed",
    "f12_task_settled",
    "f12_checkpoint_written",
    "f12_goal_review_requested",
    "f12_goal_reviewed",
    "f12_profile_routed",
    "f12_recovery_required",
    "f12_operator_directive",
)

# Task lifecycle (design §5.3). `blocked` is a READ MODEL computed from
# dependency state, never a stored status.
TASK_OPEN_STATES = ("pending", "scheduled", "running")
TASK_TERMINAL_STATES = ("succeeded", "failed", "cancelled", "inconclusive")

EXECUTION_OPEN_STATES = ("active", "completing")
EXECUTION_TERMINAL_STATES = ("completed", "stopped", "recovery_required")

_DDL = """
CREATE TABLE IF NOT EXISTS f12_execution (
    execution_id         TEXT PRIMARY KEY,
    run_id               TEXT NOT NULL,
    challenge_id         TEXT NOT NULL,
    execution_generation INTEGER NOT NULL,
    active_revision_id   TEXT,
    source_turn          INTEGER NOT NULL DEFAULT 0,
    checkpoint_no        INTEGER NOT NULL DEFAULT 0,
    source_ack_checkpoint_no INTEGER NOT NULL DEFAULT 0,
    effect               INTEGER NOT NULL DEFAULT 60,
    speed                INTEGER NOT NULL DEFAULT 25,
    approval_mode        TEXT NOT NULL DEFAULT 'auto_eval',
    goal_status          TEXT NOT NULL DEFAULT 'open',
    status               TEXT NOT NULL DEFAULT 'active',
    degraded_mode        TEXT NOT NULL DEFAULT '',
    created_at           REAL NOT NULL,
    updated_at           REAL NOT NULL
);
CREATE TABLE IF NOT EXISTS f12_plan_revision (
    revision_id          TEXT PRIMARY KEY,
    execution_id         TEXT NOT NULL,
    revision_no          INTEGER NOT NULL,
    parent_revision_id   TEXT,
    plan_json            TEXT NOT NULL,
    plan_sha256          TEXT NOT NULL,
    source_turn          INTEGER NOT NULL,
    status               TEXT NOT NULL DEFAULT 'draft',
    reject_reason        TEXT,
    schedule_json        TEXT NOT NULL DEFAULT '[]',
    created_at           REAL NOT NULL,
    accepted_at          REAL,
    UNIQUE (execution_id, revision_no, plan_sha256)
);
CREATE TABLE IF NOT EXISTS f12_plan_task (
    execution_id         TEXT NOT NULL,
    task_id              TEXT NOT NULL,
    revision_id          TEXT NOT NULL,
    intent_id            TEXT NOT NULL,
    goal                 TEXT NOT NULL,
    worker_class         TEXT NOT NULL DEFAULT 'code',
    depends_on           TEXT NOT NULL DEFAULT '[]',
    success_criteria     TEXT NOT NULL DEFAULT '[]',
    required_tier        TEXT NOT NULL DEFAULT 'C1',
    profile_hint         TEXT NOT NULL DEFAULT '',
    execution_directory  TEXT NOT NULL DEFAULT '',
    route_hash           TEXT NOT NULL DEFAULT '',
    lane_key             TEXT NOT NULL DEFAULT '',
    risk_class           TEXT NOT NULL DEFAULT '',
    resource_key         TEXT NOT NULL DEFAULT '',
    priority             INTEGER NOT NULL DEFAULT 0,
    status               TEXT NOT NULL DEFAULT 'pending',
    attempt              INTEGER NOT NULL DEFAULT 0,
    result_code          TEXT NOT NULL DEFAULT '',
    result_ref           TEXT NOT NULL DEFAULT '',
    result_checkpoint_no INTEGER,
    generation           INTEGER NOT NULL DEFAULT 1,
    created_at           REAL NOT NULL,
    updated_at           REAL NOT NULL,
    PRIMARY KEY (execution_id, task_id)
);
CREATE TABLE IF NOT EXISTS f12_checkpoint (
    execution_id         TEXT NOT NULL,
    checkpoint_no        INTEGER NOT NULL,
    revision_id          TEXT,
    source_turn          INTEGER NOT NULL,
    kind                 TEXT NOT NULL,
    trigger_task_id      TEXT NOT NULL DEFAULT '',
    schedule_json        TEXT NOT NULL DEFAULT '[]',
    task_summary_json    TEXT NOT NULL DEFAULT '{}',
    fact_highwater       INTEGER NOT NULL DEFAULT 0,
    artifact_highwater   INTEGER NOT NULL DEFAULT 0,
    budget_highwater     REAL NOT NULL DEFAULT 0.0,
    state_sha256         TEXT NOT NULL,
    wake_source          INTEGER NOT NULL DEFAULT 0,
    wake_ack             INTEGER NOT NULL DEFAULT 0,
    not_before           REAL NOT NULL DEFAULT 0.0,
    detail               TEXT NOT NULL DEFAULT '',
    created_at           REAL NOT NULL,
    PRIMARY KEY (execution_id, checkpoint_no)
);
CREATE TABLE IF NOT EXISTS f12_source_turn (
    execution_id         TEXT NOT NULL,
    turn_no              INTEGER NOT NULL,
    wake_checkpoint_no   INTEGER NOT NULL,
    status               TEXT NOT NULL DEFAULT 'called',
    prompt_sha256        TEXT NOT NULL DEFAULT '',
    reply_text           TEXT NOT NULL DEFAULT '',
    reply_sha256         TEXT NOT NULL DEFAULT '',
    plan_sha256          TEXT NOT NULL DEFAULT '',
    revision_id          TEXT NOT NULL DEFAULT '',
    error                TEXT NOT NULL DEFAULT '',
    created_at           REAL NOT NULL,
    updated_at           REAL NOT NULL,
    PRIMARY KEY (execution_id, turn_no)
);
CREATE TABLE IF NOT EXISTS f12_goal_review (
    review_id            TEXT PRIMARY KEY,
    execution_id         TEXT NOT NULL,
    revision_id          TEXT NOT NULL DEFAULT '',
    mode                 TEXT NOT NULL DEFAULT 'self',
    status               TEXT NOT NULL DEFAULT 'pending',
    verdict              TEXT NOT NULL DEFAULT '',
    detail               TEXT NOT NULL DEFAULT '',
    reviewer_intent_id   TEXT NOT NULL DEFAULT '',
    proposal_seq         INTEGER,
    disposition          TEXT NOT NULL DEFAULT '',
    created_at           REAL NOT NULL,
    decided_at           REAL
);
CREATE INDEX IF NOT EXISTS idx_f12_task_intent
    ON f12_plan_task(execution_id, intent_id);
CREATE INDEX IF NOT EXISTS idx_f12_checkpoint_wake
    ON f12_checkpoint(execution_id, wake_source, wake_ack, checkpoint_no);
"""


def ensure_f12_schema(conn: sqlite3.Connection) -> None:
    """Idempotent create/upgrade. Safe to run on every graph open."""
    conn.executescript(_DDL)
    # Column-level upgrades for DBs created by an older F12 build. Each ALTER
    # raises OperationalError when the column already exists — ignored on
    # purpose (same migration style as SQLiteSharedGraph.__init__).
    for ddl in (
        "ALTER TABLE f12_checkpoint ADD COLUMN not_before REAL NOT NULL DEFAULT 0.0",
        "ALTER TABLE f12_checkpoint ADD COLUMN detail TEXT NOT NULL DEFAULT ''",
        "ALTER TABLE f12_goal_review ADD COLUMN disposition TEXT NOT NULL DEFAULT ''",
        "ALTER TABLE f12_execution ADD COLUMN degraded_mode TEXT NOT NULL DEFAULT ''",
    ):
        try:
            conn.execute(ddl)
            conn.commit()
        except sqlite3.OperationalError:
            pass
    conn.commit()


def ensure_f12_schema_on_graph(graph: Any) -> bool:
    conn = getattr(graph, "_conn", None)
    lock = getattr(graph, "_lock", None)
    if conn is None:
        return False
    try:
        if lock is None:
            ensure_f12_schema(conn)
        else:
            with lock:
                ensure_f12_schema(conn)
        return True
    except Exception:
        return False


# ---------------------------------------------------------------------------
# small serialisation helpers
# ---------------------------------------------------------------------------


def _json(value: Any) -> str:
    return json.dumps(value, ensure_ascii=False, sort_keys=True, default=str)


def _loads(text: Any, default: Any) -> Any:
    if not text:
        return default
    try:
        return json.loads(text)
    except (TypeError, json.JSONDecodeError):
        return default


def append_event(graph: Any, kind: str, payload: dict, *, actor: str = "f12") -> int:
    """Append an F12 orchestration event to the shared append-only log."""
    append = getattr(graph, "_append", None)
    if callable(append):
        try:
            return int(append(kind, actor, dict(payload)))
        except Exception:
            return -1
    return -1


# ---------------------------------------------------------------------------
# execution row
# ---------------------------------------------------------------------------


def create_execution(
    graph: Any,
    *,
    execution_id: str,
    run_id: str,
    challenge_id: str,
    execution_generation: int,
    effect: int,
    speed: int,
    approval_mode: str,
    degraded_mode: str = "",
) -> dict[str, Any]:
    now = time.time()
    conn, lock = graph._conn, getattr(graph, "_lock", None)

    def _op() -> None:
        conn.execute(
            "INSERT OR IGNORE INTO f12_execution "
            "(execution_id, run_id, challenge_id, execution_generation, effect, "
            " speed, approval_mode, degraded_mode, created_at, updated_at) "
            "VALUES (?,?,?,?,?,?,?,?,?,?)",
            (
                execution_id,
                run_id,
                challenge_id,
                int(execution_generation),
                int(effect),
                int(speed),
                approval_mode,
                degraded_mode,
                now,
                now,
            ),
        )
        conn.commit()

    if lock is None:
        _op()
    else:
        with lock:
            _op()
    row = get_execution(graph, execution_id)
    if row is not None and int(row.get("checkpoint_no") or 0) == 0:
        append_event(
            graph,
            "f12_execution_created",
            {
                "execution_id": execution_id,
                "run_id": run_id,
                "challenge_id": challenge_id,
                "execution_generation": int(execution_generation),
                "effect": int(effect),
                "speed": int(speed),
                "approval_mode": approval_mode,
                "degraded_mode": degraded_mode,
            },
        )
    return row or {}


def get_execution(graph: Any, execution_id: str) -> dict[str, Any] | None:
    conn, lock = graph._conn, getattr(graph, "_lock", None)

    def _q() -> Any:
        return conn.execute(
            "SELECT execution_id, run_id, challenge_id, execution_generation, "
            "active_revision_id, source_turn, checkpoint_no, "
            "source_ack_checkpoint_no, effect, speed, approval_mode, goal_status, "
            "status, degraded_mode, created_at, updated_at "
            "FROM f12_execution WHERE execution_id=?",
            (execution_id,),
        ).fetchone()

    if lock is None:
        row = _q()
    else:
        with lock:
            row = _q()
    if row is None:
        return None
    return {
        "execution_id": row[0],
        "run_id": row[1],
        "challenge_id": row[2],
        "execution_generation": int(row[3]),
        "active_revision_id": row[4] or "",
        "source_turn": int(row[5]),
        "checkpoint_no": int(row[6]),
        "source_ack_checkpoint_no": int(row[7]),
        "effect": int(row[8]),
        "speed": int(row[9]),
        "approval_mode": row[10],
        "goal_status": row[11],
        "status": row[12],
        "degraded_mode": row[13] or "",
        "created_at": float(row[14]),
        "updated_at": float(row[15]),
    }


def update_execution(graph: Any, execution_id: str, **fields: Any) -> None:
    if not fields:
        return
    allowed = {
        "active_revision_id",
        "source_turn",
        "checkpoint_no",
        "source_ack_checkpoint_no",
        "goal_status",
        "status",
        "degraded_mode",
        "effect",
        "speed",
        "approval_mode",
    }
    sets: list[str] = []
    params: list[Any] = []
    for key, value in fields.items():
        if key not in allowed:
            continue
        sets.append(f"{key}=?")
        params.append(value)
    if not sets:
        return
    sets.append("updated_at=?")
    params.append(time.time())
    params.append(execution_id)
    conn, lock = graph._conn, getattr(graph, "_lock", None)
    sql = f"UPDATE f12_execution SET {', '.join(sets)} WHERE execution_id=?"

    def _op() -> None:
        conn.execute(sql, tuple(params))
        conn.commit()

    if lock is None:
        _op()
    else:
        with lock:
            _op()


# ---------------------------------------------------------------------------
# plan revisions
# ---------------------------------------------------------------------------


def insert_revision(
    graph: Any,
    *,
    revision_id: str,
    execution_id: str,
    revision_no: int,
    parent_revision_id: str,
    plan_json: str,
    plan_sha256: str,
    source_turn: int,
    status: str,
    schedule: list[str],
    reject_reason: str = "",
) -> bool:
    """Insert an immutable revision row. Returns False when it already exists
    (same execution_id, revision_no, plan_sha256 → idempotent replay)."""
    now = time.time()
    conn, lock = graph._conn, getattr(graph, "_lock", None)

    def _op() -> bool:
        cur = conn.execute(
            "INSERT OR IGNORE INTO f12_plan_revision "
            "(revision_id, execution_id, revision_no, parent_revision_id, "
            " plan_json, plan_sha256, source_turn, status, reject_reason, "
            " schedule_json, created_at, accepted_at) "
            "VALUES (?,?,?,?,?,?,?,?,?,?,?,?)",
            (
                revision_id,
                execution_id,
                int(revision_no),
                parent_revision_id or None,
                plan_json,
                plan_sha256,
                int(source_turn),
                status,
                reject_reason or None,
                _json(schedule),
                now,
                now if status == "accepted" else None,
            ),
        )
        conn.commit()
        return cur.rowcount > 0

    if lock is None:
        return _op()
    with lock:
        return _op()


def set_revision_status(
    graph: Any,
    revision_id: str,
    status: str,
    *,
    reject_reason: str = "",
) -> None:
    conn, lock = graph._conn, getattr(graph, "_lock", None)
    now = time.time()

    def _op() -> None:
        if status == "accepted":
            conn.execute(
                "UPDATE f12_plan_revision SET status=?, accepted_at=? "
                "WHERE revision_id=?",
                (status, now, revision_id),
            )
        elif status == "rejected":
            conn.execute(
                "UPDATE f12_plan_revision SET status=?, reject_reason=? "
                "WHERE revision_id=?",
                (status, reject_reason[:1000], revision_id),
            )
        else:
            conn.execute(
                "UPDATE f12_plan_revision SET status=? WHERE revision_id=?",
                (status, revision_id),
            )
        conn.commit()

    if lock is None:
        _op()
    else:
        with lock:
            _op()


def get_revision(graph: Any, revision_id: str) -> dict[str, Any] | None:
    conn, lock = graph._conn, getattr(graph, "_lock", None)

    def _q() -> Any:
        return conn.execute(
            "SELECT revision_id, execution_id, revision_no, parent_revision_id, "
            "plan_json, plan_sha256, source_turn, status, reject_reason, "
            "schedule_json, created_at, accepted_at "
            "FROM f12_plan_revision WHERE revision_id=?",
            (revision_id,),
        ).fetchone()

    if lock is None:
        row = _q()
    else:
        with lock:
            row = _q()
    if row is None:
        return None
    return {
        "revision_id": row[0],
        "execution_id": row[1],
        "revision_no": int(row[2]),
        "parent_revision_id": row[3] or "",
        "plan_json": row[4],
        "plan_sha256": row[5],
        "source_turn": int(row[6]),
        "status": row[7],
        "reject_reason": row[8] or "",
        "schedule": _loads(row[9], []),
        "created_at": float(row[10]),
        "accepted_at": float(row[11]) if row[11] is not None else None,
    }


def find_revision_by_sha(
    graph: Any, execution_id: str, plan_sha256: str
) -> dict[str, Any] | None:
    conn, lock = graph._conn, getattr(graph, "_lock", None)

    def _q() -> Any:
        return conn.execute(
            "SELECT revision_id FROM f12_plan_revision "
            "WHERE execution_id=? AND plan_sha256=? "
            "ORDER BY revision_no DESC LIMIT 1",
            (execution_id, plan_sha256),
        ).fetchone()

    if lock is None:
        row = _q()
    else:
        with lock:
            row = _q()
    if row is None:
        return None
    return get_revision(graph, str(row[0]))


def next_revision_no(graph: Any, execution_id: str) -> int:
    conn, lock = graph._conn, getattr(graph, "_lock", None)

    def _q() -> Any:
        return conn.execute(
            "SELECT COALESCE(MAX(revision_no), 0) FROM f12_plan_revision "
            "WHERE execution_id=?",
            (execution_id,),
        ).fetchone()

    if lock is None:
        row = _q()
    else:
        with lock:
            row = _q()
    return int(row[0] if row else 0) + 1


# ---------------------------------------------------------------------------
# plan tasks
# ---------------------------------------------------------------------------


def upsert_task(graph: Any, task: dict[str, Any]) -> None:
    """Insert a materialised task. Existing rows keep their status/attempt
    (identity fields are immutable once materialised — the validator enforces
    that before this is called)."""
    now = time.time()
    conn, lock = graph._conn, getattr(graph, "_lock", None)

    def _op() -> None:
        conn.execute(
            "INSERT OR IGNORE INTO f12_plan_task "
            "(execution_id, task_id, revision_id, intent_id, goal, worker_class, "
            " depends_on, success_criteria, required_tier, profile_hint, "
            " execution_directory, route_hash, lane_key, risk_class, resource_key, "
            " priority, status, attempt, generation, created_at, updated_at) "
            "VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,'pending',0,?,?,?)",
            (
                task["execution_id"],
                task["task_id"],
                task["revision_id"],
                task["intent_id"],
                task.get("goal", ""),
                task.get("worker_class", "code"),
                _json(task.get("depends_on") or []),
                _json(task.get("success_criteria") or []),
                task.get("required_tier", "C1"),
                task.get("profile_hint", ""),
                task.get("execution_directory", ""),
                task.get("route_hash", ""),
                task.get("lane_key", ""),
                task.get("risk_class", ""),
                task.get("resource_key", ""),
                int(task.get("priority") or 0),
                int(task.get("generation") or 1),
                now,
                now,
            ),
        )
        conn.commit()

    if lock is None:
        _op()
    else:
        with lock:
            _op()


def get_task(graph: Any, execution_id: str, task_id: str) -> dict[str, Any] | None:
    conn, lock = graph._conn, getattr(graph, "_lock", None)

    def _q() -> Any:
        return conn.execute(
            "SELECT execution_id, task_id, revision_id, intent_id, goal, "
            "worker_class, depends_on, success_criteria, required_tier, "
            "profile_hint, execution_directory, route_hash, lane_key, risk_class, "
            "resource_key, priority, status, attempt, result_code, result_ref, "
            "result_checkpoint_no, generation, created_at, updated_at "
            "FROM f12_plan_task WHERE execution_id=? AND task_id=?",
            (execution_id, task_id),
        ).fetchone()

    if lock is None:
        row = _q()
    else:
        with lock:
            row = _q()
    return _task_row(row) if row else None


def _task_row(row: Any) -> dict[str, Any]:
    return {
        "execution_id": row[0],
        "task_id": row[1],
        "revision_id": row[2],
        "intent_id": row[3],
        "goal": row[4],
        "worker_class": row[5],
        "depends_on": _loads(row[6], []),
        "success_criteria": _loads(row[7], []),
        "required_tier": row[8],
        "profile_hint": row[9] or "",
        "execution_directory": row[10] or "",
        "route_hash": row[11] or "",
        "lane_key": row[12] or "",
        "risk_class": row[13] or "",
        "resource_key": row[14] or "",
        "priority": int(row[15]),
        "status": row[16],
        "attempt": int(row[17]),
        "result_code": row[18] or "",
        "result_ref": row[19] or "",
        "result_checkpoint_no": row[20],
        "generation": int(row[21]),
        "created_at": float(row[22]),
        "updated_at": float(row[23]),
    }


def list_tasks(graph: Any, execution_id: str) -> list[dict[str, Any]]:
    conn, lock = graph._conn, getattr(graph, "_lock", None)

    def _q() -> Any:
        return conn.execute(
            "SELECT execution_id, task_id, revision_id, intent_id, goal, "
            "worker_class, depends_on, success_criteria, required_tier, "
            "profile_hint, execution_directory, route_hash, lane_key, risk_class, "
            "resource_key, priority, status, attempt, result_code, result_ref, "
            "result_checkpoint_no, generation, created_at, updated_at "
            "FROM f12_plan_task WHERE execution_id=? ORDER BY created_at, task_id",
            (execution_id,),
        ).fetchall()

    if lock is None:
        rows = _q()
    else:
        with lock:
            rows = _q()
    return [_task_row(r) for r in rows]


def get_task_by_intent(
    graph: Any, execution_id: str, intent_id: str
) -> dict[str, Any] | None:
    conn, lock = graph._conn, getattr(graph, "_lock", None)

    def _q() -> Any:
        return conn.execute(
            "SELECT task_id FROM f12_plan_task "
            "WHERE execution_id=? AND intent_id=?",
            (execution_id, intent_id),
        ).fetchone()

    if lock is None:
        row = _q()
    else:
        with lock:
            row = _q()
    if row is None:
        return None
    return get_task(graph, execution_id, str(row[0]))


def set_task_state(
    graph: Any,
    execution_id: str,
    task_id: str,
    status: str,
    *,
    attempt: int | None = None,
    result_code: str = "",
    result_ref: str = "",
    result_checkpoint_no: int | None = None,
    expected: tuple[str, ...] | None = None,
) -> bool:
    """Idempotent, optionally fenced task state update.

    ``expected`` restricts the states this transition may leave (owner/state
    fence): a late writer moving an already-terminal task is a no-op.
    """
    conn, lock = graph._conn, getattr(graph, "_lock", None)
    now = time.time()

    def _op() -> bool:
        where = "execution_id=? AND task_id=?"
        params: list[Any] = [status, now]
        sql = (
            "UPDATE f12_plan_task SET status=?, updated_at=?"
        )
        if attempt is not None:
            sql += ", attempt=?"
            params.append(int(attempt))
        if result_code:
            sql += ", result_code=?"
            params.append(result_code)
        if result_ref:
            sql += ", result_ref=?"
            params.append(result_ref)
        if result_checkpoint_no is not None:
            sql += ", result_checkpoint_no=?"
            params.append(int(result_checkpoint_no))
        sql += f" WHERE {where}"
        params.extend([execution_id, task_id])
        if expected:
            sql += " AND status IN (" + ",".join("?" for _ in expected) + ")"
            params.extend(expected)
        cur = conn.execute(sql, tuple(params))
        conn.commit()
        return cur.rowcount > 0

    if lock is None:
        return _op()
    with lock:
        return _op()


# ---------------------------------------------------------------------------
# checkpoints
# ---------------------------------------------------------------------------


def write_checkpoint(
    graph: Any,
    *,
    execution_id: str,
    revision_id: str,
    source_turn: int,
    kind: str,
    trigger_task_id: str = "",
    schedule: list[str] | None = None,
    task_summary: dict[str, Any] | None = None,
    fact_highwater: int = 0,
    artifact_highwater: int = 0,
    budget_highwater: float = 0.0,
    state_sha256: str,
    wake_source: bool = False,
    not_before: float = 0.0,
    detail: str = "",
) -> int:
    """Append a monotonic checkpoint. Returns the new checkpoint_no.

    checkpoint_no is allocated inside the same lock/transaction as the INSERT
    so a crash can never produce duplicate numbers or a gap-less reuse.
    """
    conn, lock = graph._conn, getattr(graph, "_lock", None)
    now = time.time()

    def _op() -> int:
        conn.execute("BEGIN IMMEDIATE")
        try:
            row = conn.execute(
                "SELECT COALESCE(MAX(checkpoint_no), 0) FROM f12_checkpoint "
                "WHERE execution_id=?",
                (execution_id,),
            ).fetchone()
            no = int(row[0] if row else 0) + 1
            conn.execute(
                "INSERT INTO f12_checkpoint "
                "(execution_id, checkpoint_no, revision_id, source_turn, kind, "
                " trigger_task_id, schedule_json, task_summary_json, "
                " fact_highwater, artifact_highwater, budget_highwater, "
                " state_sha256, wake_source, wake_ack, not_before, detail, "
                " created_at) "
                "VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,0,?,?,?)",
                (
                    execution_id,
                    no,
                    revision_id or None,
                    int(source_turn),
                    kind,
                    trigger_task_id,
                    _json(schedule or []),
                    _json(task_summary or {}),
                    int(fact_highwater),
                    int(artifact_highwater),
                    float(budget_highwater),
                    state_sha256,
                    1 if wake_source else 0,
                    float(not_before or 0.0),
                    detail[:1000],
                    now,
                ),
            )
            conn.execute(
                "UPDATE f12_execution SET checkpoint_no=?, updated_at=? "
                "WHERE execution_id=?",
                (no, now, execution_id),
            )
            conn.commit()
            return no
        except Exception:
            conn.rollback()
            raise

    if lock is None:
        return _op()
    with lock:
        return _op()


def get_checkpoint(
    graph: Any, execution_id: str, checkpoint_no: int
) -> dict[str, Any] | None:
    conn, lock = graph._conn, getattr(graph, "_lock", None)

    def _q() -> Any:
        return conn.execute(
            "SELECT checkpoint_no, revision_id, source_turn, kind, "
            "trigger_task_id, schedule_json, task_summary_json, fact_highwater, "
            "artifact_highwater, budget_highwater, state_sha256, wake_source, "
            "wake_ack, not_before, detail, created_at "
            "FROM f12_checkpoint WHERE execution_id=? AND checkpoint_no=?",
            (execution_id, int(checkpoint_no)),
        ).fetchone()

    if lock is None:
        row = _q()
    else:
        with lock:
            row = _q()
    return _checkpoint_row(row) if row else None


def _checkpoint_row(row: Any) -> dict[str, Any]:
    return {
        "checkpoint_no": int(row[0]),
        "revision_id": row[1] or "",
        "source_turn": int(row[2]),
        "kind": row[3],
        "trigger_task_id": row[4] or "",
        "schedule": _loads(row[5], []),
        "task_summary": _loads(row[6], {}),
        "fact_highwater": int(row[7]),
        "artifact_highwater": int(row[8]),
        "budget_highwater": float(row[9]),
        "state_sha256": row[10],
        "wake_source": bool(row[11]),
        "wake_ack": bool(row[12]),
        "not_before": float(row[13] or 0.0),
        "detail": row[14] or "",
        "created_at": float(row[15]),
    }


def list_checkpoints(graph: Any, execution_id: str) -> list[dict[str, Any]]:
    conn, lock = graph._conn, getattr(graph, "_lock", None)

    def _q() -> Any:
        return conn.execute(
            "SELECT checkpoint_no FROM f12_checkpoint WHERE execution_id=? "
            "ORDER BY checkpoint_no",
            (execution_id,),
        ).fetchall()

    if lock is None:
        rows = _q()
    else:
        with lock:
            rows = _q()
    out = []
    for (no,) in rows:
        cp = get_checkpoint(graph, execution_id, int(no))
        if cp is not None:
            out.append(cp)
    return out


def oldest_unacked_wake(graph: Any, execution_id: str) -> dict[str, Any] | None:
    """The oldest wake_source=1 checkpoint not yet acknowledged by the source
    coordinator agent (and not scheduled for a future retry)."""
    conn, lock = graph._conn, getattr(graph, "_lock", None)
    now = time.time()

    def _q() -> Any:
        return conn.execute(
            "SELECT checkpoint_no FROM f12_checkpoint "
            "WHERE execution_id=? AND wake_source=1 AND wake_ack=0 "
            "AND not_before<=? "
            "ORDER BY checkpoint_no LIMIT 1",
            (execution_id, now),
        ).fetchone()

    if lock is None:
        row = _q()
    else:
        with lock:
            row = _q()
    if row is None:
        return None
    return get_checkpoint(graph, execution_id, int(row[0]))


def pending_wake_exists(graph: Any, execution_id: str) -> bool:
    """Any unacknowledged wake (including future-scheduled retries)?"""
    conn, lock = graph._conn, getattr(graph, "_lock", None)

    def _q() -> Any:
        return conn.execute(
            "SELECT 1 FROM f12_checkpoint "
            "WHERE execution_id=? AND wake_source=1 AND wake_ack=0 LIMIT 1",
            (execution_id,),
        ).fetchone()

    if lock is None:
        row = _q()
    else:
        with lock:
            row = _q()
    return row is not None


def ack_wake(graph: Any, execution_id: str, checkpoint_no: int) -> None:
    conn, lock = graph._conn, getattr(graph, "_lock", None)

    def _op() -> None:
        conn.execute(
            "UPDATE f12_checkpoint SET wake_ack=1 "
            "WHERE execution_id=? AND checkpoint_no=?",
            (execution_id, int(checkpoint_no)),
        )
        conn.execute(
            "UPDATE f12_execution SET source_ack_checkpoint_no="
            "MAX(source_ack_checkpoint_no, ?), updated_at=? "
            "WHERE execution_id=?",
            (int(checkpoint_no), time.time(), execution_id),
        )
        conn.commit()

    if lock is None:
        _op()
    else:
        with lock:
            _op()


# ---------------------------------------------------------------------------
# source turns (audit + idempotent reply application)
# ---------------------------------------------------------------------------


def begin_source_turn(
    graph: Any,
    *,
    execution_id: str,
    turn_no: int,
    wake_checkpoint_no: int,
    prompt_sha256: str,
) -> bool:
    """Insert the 'called' row for a source turn. False → this turn row already
    exists (restart replay: read it back instead of re-calling the model)."""
    conn, lock = graph._conn, getattr(graph, "_lock", None)
    now = time.time()

    def _op() -> bool:
        cur = conn.execute(
            "INSERT OR IGNORE INTO f12_source_turn "
            "(execution_id, turn_no, wake_checkpoint_no, status, prompt_sha256, "
            " created_at, updated_at) VALUES (?,?,?,'called',?,?,?)",
            (execution_id, int(turn_no), int(wake_checkpoint_no), prompt_sha256, now, now),
        )
        conn.commit()
        return cur.rowcount > 0

    if lock is None:
        return _op()
    with lock:
        return _op()


def get_source_turn(graph: Any, execution_id: str, turn_no: int) -> dict[str, Any] | None:
    conn, lock = graph._conn, getattr(graph, "_lock", None)

    def _q() -> Any:
        return conn.execute(
            "SELECT turn_no, wake_checkpoint_no, status, prompt_sha256, "
            "reply_text, reply_sha256, plan_sha256, revision_id, error, "
            "created_at, updated_at "
            "FROM f12_source_turn WHERE execution_id=? AND turn_no=?",
            (execution_id, int(turn_no)),
        ).fetchone()

    if lock is None:
        row = _q()
    else:
        with lock:
            row = _q()
    if row is None:
        return None
    return {
        "turn_no": int(row[0]),
        "wake_checkpoint_no": int(row[1]),
        "status": row[2],
        "prompt_sha256": row[3] or "",
        "reply_text": row[4] or "",
        "reply_sha256": row[5] or "",
        "plan_sha256": row[6] or "",
        "revision_id": row[7] or "",
        "error": row[8] or "",
        "created_at": float(row[9]),
        "updated_at": float(row[10]),
    }


def update_source_turn(graph: Any, execution_id: str, turn_no: int, **fields: Any) -> None:
    allowed = {"status", "reply_text", "reply_sha256", "plan_sha256", "revision_id", "error"}
    sets: list[str] = []
    params: list[Any] = []
    for key, value in fields.items():
        if key not in allowed:
            continue
        sets.append(f"{key}=?")
        params.append(value)
    if not sets:
        return
    sets.append("updated_at=?")
    params.append(time.time())
    params.extend([execution_id, int(turn_no)])
    conn, lock = graph._conn, getattr(graph, "_lock", None)
    sql = f"UPDATE f12_source_turn SET {', '.join(sets)} WHERE execution_id=? AND turn_no=?"

    def _op() -> None:
        conn.execute(sql, tuple(params))
        conn.commit()

    if lock is None:
        _op()
    else:
        with lock:
            _op()


# ---------------------------------------------------------------------------
# goal review rows
# ---------------------------------------------------------------------------


def insert_goal_review(
    graph: Any,
    *,
    review_id: str,
    execution_id: str,
    revision_id: str,
    mode: str,
    reviewer_intent_id: str = "",
) -> bool:
    conn, lock = graph._conn, getattr(graph, "_lock", None)
    now = time.time()

    def _op() -> bool:
        cur = conn.execute(
            "INSERT OR IGNORE INTO f12_goal_review "
            "(review_id, execution_id, revision_id, mode, reviewer_intent_id, "
            " created_at) VALUES (?,?,?,?,?,?)",
            (review_id, execution_id, revision_id, mode, reviewer_intent_id, now),
        )
        conn.commit()
        return cur.rowcount > 0

    if lock is None:
        return _op()
    with lock:
        return _op()


def update_goal_review(graph: Any, review_id: str, **fields: Any) -> None:
    allowed = {"status", "verdict", "detail", "proposal_seq", "disposition", "decided_at"}
    sets: list[str] = []
    params: list[Any] = []
    for key, value in fields.items():
        if key not in allowed:
            continue
        sets.append(f"{key}=?")
        params.append(value)
    if not sets:
        return
    params.append(review_id)
    conn, lock = graph._conn, getattr(graph, "_lock", None)
    sql = f"UPDATE f12_goal_review SET {', '.join(sets)} WHERE review_id=?"

    def _op() -> None:
        conn.execute(sql, tuple(params))
        conn.commit()

    if lock is None:
        _op()
    else:
        with lock:
            _op()


def get_goal_review(graph: Any, review_id: str) -> dict[str, Any] | None:
    conn, lock = graph._conn, getattr(graph, "_lock", None)

    def _q() -> Any:
        return conn.execute(
            "SELECT review_id, execution_id, revision_id, mode, status, verdict, "
            "detail, reviewer_intent_id, proposal_seq, disposition, created_at, "
            "decided_at FROM f12_goal_review WHERE review_id=?",
            (review_id,),
        ).fetchone()

    if lock is None:
        row = _q()
    else:
        with lock:
            row = _q()
    if row is None:
        return None
    return {
        "review_id": row[0],
        "execution_id": row[1],
        "revision_id": row[2] or "",
        "mode": row[3],
        "status": row[4],
        "verdict": row[5] or "",
        "detail": row[6] or "",
        "reviewer_intent_id": row[7] or "",
        "proposal_seq": row[8],
        "disposition": row[9] or "",
        "created_at": float(row[10]),
        "decided_at": float(row[11]) if row[11] is not None else None,
    }


def list_goal_reviews(graph: Any, execution_id: str) -> list[dict[str, Any]]:
    conn, lock = graph._conn, getattr(graph, "_lock", None)

    def _q() -> Any:
        return conn.execute(
            "SELECT review_id FROM f12_goal_review WHERE execution_id=? "
            "ORDER BY created_at",
            (execution_id,),
        ).fetchall()

    if lock is None:
        rows = _q()
    else:
        with lock:
            rows = _q()
    out = []
    for (rid,) in rows:
        row = get_goal_review(graph, str(rid))
        if row is not None:
            out.append(row)
    return out
