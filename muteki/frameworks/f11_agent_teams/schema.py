"""f11 agent-teams SQLite schema (transaction storage plane)."""

from __future__ import annotations

import sqlite3
from typing import Any

F11_FEATURE_KINDS = (
    "team_formed",
    "team_task_claimed",
    "team_msg_sent",
    "team_token_granted",
    "team_assertion_written",
    "team_channel_posted",
    "team_member_stalled",
    "team_member_dead",
    "team_member_replaced",
    "team_lead_call",
    "team_circuit_open",
)

# Teammate-mode CLI whitelist (Gate-0b): full-board reads must NOT be registered.
TEAMMATE_ALLOWED_CMDS = frozenset(
    {
        "msg-send",
        "msg-check",
        "task-list",
        "task-claim",
        "task-done",
        "assert-write",
        "artifact-put",
        "token-wait",
        "heartbeat",
        "write-fact",
        "mark-deadend",
        "claim",
        "claim-resource",
        "list-intents",
        "read-flags",
        "read-resource-locks",
        "read-deadends",
        "read-review",
    }
)

TEAMMATE_FORBIDDEN_CMDS = frozenset(
    {
        "read-facts",
        "read-routes",
        "read-branches",
        "read-archive",
        "read-all",
        "dump-board",
    }
)

_DDL = """
CREATE TABLE IF NOT EXISTS team_roster (
    team_id         TEXT PRIMARY KEY,
    challenge_id    TEXT NOT NULL,
    lead_model      TEXT NOT NULL,
    lead_calls_used INTEGER NOT NULL DEFAULT 0,
    lead_calls_cap  INTEGER NOT NULL DEFAULT 12,
    lead_cooldown_s INTEGER NOT NULL DEFAULT 45,
    last_lead_call_at REAL,
    members_json    TEXT NOT NULL DEFAULT '[]',
    budget_json     TEXT NOT NULL DEFAULT '{}',
    created_at      REAL NOT NULL,
    status          TEXT NOT NULL DEFAULT 'active'
);
CREATE TABLE IF NOT EXISTS team_task (
    task_id         TEXT PRIMARY KEY,
    team_id         TEXT NOT NULL,
    goal            TEXT NOT NULL,
    goal_id         TEXT NOT NULL,
    declared_effects TEXT NOT NULL DEFAULT '[]',
    status          TEXT NOT NULL DEFAULT 'pending',
    owner           TEXT,
    lease_json      TEXT,
    depends_on      TEXT NOT NULL DEFAULT '[]',
    evidence_refs   TEXT NOT NULL DEFAULT '[]',
    created_by      TEXT NOT NULL,
    fence           INTEGER NOT NULL DEFAULT 0,
    created_at      REAL NOT NULL,
    updated_at      REAL NOT NULL
);
CREATE TABLE IF NOT EXISTS team_message (
    msg_id          TEXT PRIMARY KEY,
    team_id         TEXT NOT NULL,
    seq             INTEGER NOT NULL,
    thread          TEXT,
    kind            TEXT NOT NULL,
    from_member     TEXT NOT NULL,
    to_json         TEXT NOT NULL,
    verbatim_json   TEXT NOT NULL DEFAULT '[]',
    body            TEXT NOT NULL DEFAULT '',
    evidence_refs   TEXT NOT NULL DEFAULT '[]',
    declared_effects TEXT NOT NULL DEFAULT '[]',
    hop             INTEGER NOT NULL DEFAULT 0,
    require_ack     INTEGER NOT NULL DEFAULT 0,
    acked_by        TEXT NOT NULL DEFAULT '[]',
    ack_of          TEXT,
    ttl_s           INTEGER NOT NULL DEFAULT 300,
    created_at      REAL NOT NULL
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_team_message_seq
    ON team_message(team_id, seq);
CREATE TABLE IF NOT EXISTS team_assertion (
    assertion_id    TEXT PRIMARY KEY,
    team_id         TEXT NOT NULL,
    text            TEXT NOT NULL,
    evidence_refs   TEXT NOT NULL DEFAULT '[]',
    confidence      REAL NOT NULL DEFAULT 0.5,
    status          TEXT NOT NULL DEFAULT 'active',
    source_seqs     TEXT NOT NULL DEFAULT '[]',
    invalidated_by  TEXT,
    ttl_s           INTEGER,
    created_at      REAL NOT NULL
);
CREATE TABLE IF NOT EXISTS team_turn_token (
    token_id        TEXT PRIMARY KEY,
    team_id         TEXT NOT NULL,
    protocol        TEXT NOT NULL,
    holder          TEXT,
    fence           INTEGER NOT NULL DEFAULT 0,
    lease_expires_at REAL,
    on_expire       TEXT NOT NULL DEFAULT 'release+notify-lead',
    status          TEXT NOT NULL DEFAULT 'active'
);
CREATE TABLE IF NOT EXISTS team_seq (
    team_id         TEXT PRIMARY KEY,
    next_seq        INTEGER NOT NULL DEFAULT 1
);
CREATE TABLE IF NOT EXISTS team_channel_digest (
    digest_id       TEXT PRIMARY KEY,
    team_id         TEXT NOT NULL,
    span_lo         INTEGER NOT NULL,
    span_hi         INTEGER NOT NULL,
    distilled       TEXT NOT NULL,
    assertion_candidates TEXT NOT NULL DEFAULT '[]',
    role_relevance  TEXT NOT NULL DEFAULT '{}',
    generated_at    REAL NOT NULL,
    by_model        TEXT NOT NULL DEFAULT 'deterministic'
);
"""


def ensure_f11_schema(conn: sqlite3.Connection) -> None:
    conn.executescript(_DDL)
    # Migration for DBs created before last_lead_call_at existed (T05 cooldown).
    cols = {row[1] for row in conn.execute("PRAGMA table_info(team_roster)").fetchall()}
    if "last_lead_call_at" not in cols:
        conn.execute("ALTER TABLE team_roster ADD COLUMN last_lead_call_at REAL")
    conn.commit()


def ensure_f11_schema_on_graph(graph: Any) -> bool:
    conn = getattr(graph, "_conn", None)
    lock = getattr(graph, "_lock", None)
    if conn is None:
        return False
    try:
        if lock is None:
            ensure_f11_schema(conn)
        else:
            with lock:
                ensure_f11_schema(conn)
        return True
    except Exception:
        return False
