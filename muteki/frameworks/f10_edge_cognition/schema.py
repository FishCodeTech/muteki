"""f10 edge cognition strategic ledger tables."""

from __future__ import annotations

import sqlite3
from typing import Any

F10_FEATURE_KINDS = (
    "edge_shell_started",
    "edge_shell_checkpoint",
    "edge_shell_stuck",
    "edge_shell_killed",
    "edge_intent_spawned",
    "edge_sub_intent",
    "edge_budget_trip",
    "edge_meta_explore",
)

_DDL = """
CREATE TABLE IF NOT EXISTS edge_run_budget (
    run_id          TEXT PRIMARY KEY,
    token_budget    INTEGER NOT NULL,
    tokens_spent    INTEGER NOT NULL DEFAULT 0,
    status          TEXT NOT NULL DEFAULT 'open'
);
CREATE TABLE IF NOT EXISTS edge_worker_budget (
    shell_id        TEXT PRIMARY KEY,
    intent_id       TEXT,
    token_limit     INTEGER NOT NULL,
    tokens_used     INTEGER NOT NULL DEFAULT 0,
    turn_limit      INTEGER NOT NULL DEFAULT 10,
    turns_used      INTEGER NOT NULL DEFAULT 0,
    killed          INTEGER NOT NULL DEFAULT 0,
    updated_at      REAL NOT NULL
);
CREATE TABLE IF NOT EXISTS edge_intent_queue (
    intent_id       TEXT PRIMARY KEY,
    goal            TEXT NOT NULL,
    predicted_effects TEXT NOT NULL DEFAULT '[]',
    priority        REAL NOT NULL DEFAULT 0.5,
    source          TEXT NOT NULL DEFAULT 'prepare',
    created_at      REAL NOT NULL
);
CREATE TABLE IF NOT EXISTS edge_worker_lifecycle (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    shell_id        TEXT NOT NULL,
    event           TEXT NOT NULL,
    ts              REAL NOT NULL,
    intent_id       TEXT,
    payload         TEXT NOT NULL DEFAULT '{}'
);
CREATE TABLE IF NOT EXISTS edge_capability_profile (
    engine          TEXT NOT NULL,
    category        TEXT NOT NULL,
    win_rate        REAL NOT NULL DEFAULT 0.5,
    tokens_per_flag REAL,
    sample_count    INTEGER NOT NULL DEFAULT 0,
    updated_at      REAL NOT NULL,
    PRIMARY KEY (engine, category)
);
"""


def ensure_f10_schema(conn: sqlite3.Connection) -> None:
    conn.executescript(_DDL)
    conn.commit()


def ensure_f10_schema_on_graph(graph: Any) -> bool:
    conn = getattr(graph, "_conn", None)
    lock = getattr(graph, "_lock", None)
    if conn is None:
        return False
    try:
        if lock is None:
            ensure_f10_schema(conn)
        else:
            with lock:
                ensure_f10_schema(conn)
        return True
    except Exception:
        return False
