"""competition.db 的 schema 与 migration（任务书 10.2，设计 7.1，COMP-01）。

复用 ``muteki.platform.migrations`` 的 migration 框架（版本单调递增、
schema_migrations 历史 receipt、逐语句事务执行）；competition.db 是独立
数据库，不与 platform.db 共享任何表。

每张实体表的关键字段独立成列（可查询、可加唯一约束），完整负载为
payload JSON 列；读取以 payload 为准重建契约模型。部分唯一索引
（活动 RunBinding / 活动 InstanceLease）依赖 SQLite 的部分索引能力，
状态集合与 ``models.BINDING_ACTIVE_STATES`` / ``models.LEASE_ACTIVE_STATES``
保持一致。
"""

from __future__ import annotations

from muteki.platform.migrations import (
    Migration,
    applied_versions,
    apply_migrations,
    current_version,
    migration_receipts,
)

#: v1 全量 schema（任务书 10.2 列出的全部实体 + 命令/回执/事件/水位/outbox）。
_SCHEMA_V1 = """
CREATE TABLE IF NOT EXISTS platform_connections (
    connection_id       TEXT PRIMARY KEY,
    platform_kind       TEXT NOT NULL DEFAULT '',
    canonical_base_url  TEXT NOT NULL DEFAULT '',
    account_key         TEXT NOT NULL DEFAULT '',
    credential_ref      TEXT NOT NULL DEFAULT '',
    status              TEXT NOT NULL DEFAULT 'active',
    schema_version      INTEGER NOT NULL,
    created_at          TEXT NOT NULL,
    updated_at          TEXT NOT NULL,
    payload             TEXT NOT NULL,
    UNIQUE (platform_kind, canonical_base_url, account_key)
);

CREATE TABLE IF NOT EXISTS competitions (
    competition_id          TEXT PRIMARY KEY,
    connection_id           TEXT NOT NULL DEFAULT '',
    external_competition_id TEXT NOT NULL DEFAULT '',
    title                   TEXT NOT NULL DEFAULT '',
    scheduler_state         TEXT NOT NULL DEFAULT 'stopped',
    tombstoned              INTEGER NOT NULL DEFAULT 0,
    schema_version          INTEGER NOT NULL,
    created_at              TEXT NOT NULL,
    updated_at              TEXT NOT NULL,
    payload                 TEXT NOT NULL,
    UNIQUE (connection_id, external_competition_id)
);
CREATE INDEX IF NOT EXISTS idx_competitions_connection ON competitions(connection_id);

CREATE TABLE IF NOT EXISTS competition_policies (
    competition_id  TEXT PRIMARY KEY,
    automation_mode TEXT NOT NULL DEFAULT 'assisted',
    schema_version  INTEGER NOT NULL,
    created_at      TEXT NOT NULL,
    updated_at      TEXT NOT NULL,
    payload         TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS competition_challenges (
    challenge_id          TEXT PRIMARY KEY,
    competition_id        TEXT NOT NULL,
    external_challenge_id TEXT NOT NULL DEFAULT '',
    name                  TEXT NOT NULL DEFAULT '',
    category              TEXT NOT NULL DEFAULT '',
    current_revision_id   TEXT,
    remote_state          TEXT NOT NULL DEFAULT 'open',
    state                 TEXT NOT NULL DEFAULT 'discovered',
    paused_from           TEXT,
    tombstoned            INTEGER NOT NULL DEFAULT 0,
    schema_version        INTEGER NOT NULL,
    created_at            TEXT NOT NULL,
    updated_at            TEXT NOT NULL,
    payload               TEXT NOT NULL,
    UNIQUE (competition_id, external_challenge_id)
);
CREATE INDEX IF NOT EXISTS idx_comp_challenges_state
    ON competition_challenges(competition_id, state);

CREATE TABLE IF NOT EXISTS challenge_revisions (
    revision_id              TEXT PRIMARY KEY,
    competition_challenge_id TEXT NOT NULL,
    content_hash             TEXT NOT NULL,
    revision_seq             INTEGER NOT NULL DEFAULT 1,
    name                     TEXT NOT NULL DEFAULT '',
    category                 TEXT NOT NULL DEFAULT '',
    points                   REAL NOT NULL DEFAULT 0,
    schema_version           INTEGER NOT NULL,
    created_at               TEXT NOT NULL,
    updated_at               TEXT NOT NULL,
    payload                  TEXT NOT NULL,
    UNIQUE (competition_challenge_id, content_hash)
);
CREATE INDEX IF NOT EXISTS idx_challenge_revisions_challenge
    ON challenge_revisions(competition_challenge_id);

CREATE TABLE IF NOT EXISTS artifact_objects (
    sha256         TEXT PRIMARY KEY,
    size           INTEGER NOT NULL DEFAULT 0,
    media_type     TEXT NOT NULL DEFAULT '',
    origin         TEXT NOT NULL DEFAULT '',
    local_path     TEXT NOT NULL DEFAULT '',
    schema_version INTEGER NOT NULL,
    created_at     TEXT NOT NULL,
    updated_at     TEXT NOT NULL,
    payload        TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS challenge_artifacts (
    revision_id    TEXT NOT NULL,
    sha256         TEXT NOT NULL,
    name           TEXT NOT NULL DEFAULT '',
    position       INTEGER NOT NULL DEFAULT 0,
    schema_version INTEGER NOT NULL,
    created_at     TEXT NOT NULL,
    updated_at     TEXT NOT NULL,
    payload        TEXT NOT NULL,
    PRIMARY KEY (revision_id, sha256)
);

CREATE TABLE IF NOT EXISTS sync_cursors (
    competition_id TEXT NOT NULL,
    kind           TEXT NOT NULL,
    cursor         TEXT NOT NULL DEFAULT '',
    etag           TEXT NOT NULL DEFAULT '',
    schema_version INTEGER NOT NULL,
    created_at     TEXT NOT NULL,
    updated_at     TEXT NOT NULL,
    payload        TEXT NOT NULL,
    PRIMARY KEY (competition_id, kind)
);

CREATE TABLE IF NOT EXISTS run_bindings (
    binding_id                TEXT PRIMARY KEY,
    competition_id            TEXT NOT NULL,
    competition_challenge_id  TEXT NOT NULL,
    revision_id               TEXT NOT NULL DEFAULT '',
    run_id                    TEXT NOT NULL DEFAULT '',
    execution_generation      INTEGER NOT NULL DEFAULT 1,
    lease_id                  TEXT,
    state                     TEXT NOT NULL DEFAULT 'planned',
    schema_version            INTEGER NOT NULL,
    created_at                TEXT NOT NULL,
    updated_at                TEXT NOT NULL,
    payload                   TEXT NOT NULL,
    UNIQUE (run_id, execution_generation)
);
-- 一题最多一个活动 binding（设计 9.3 规则 5）；活动状态集合与
-- models.BINDING_ACTIVE_STATES 保持一致。
CREATE UNIQUE INDEX IF NOT EXISTS uq_run_bindings_active_challenge
    ON run_bindings(competition_challenge_id)
    WHERE state IN ('planned', 'creating', 'starting', 'active',
                    'local_finished', 'remote_pending', 'rejected',
                    'resolving', 'paused');
CREATE INDEX IF NOT EXISTS idx_run_bindings_competition
    ON run_bindings(competition_id, state);

CREATE TABLE IF NOT EXISTS instance_leases (
    lease_id                  TEXT PRIMARY KEY,
    connection_id             TEXT NOT NULL DEFAULT '',
    competition_id            TEXT NOT NULL DEFAULT '',
    competition_challenge_id  TEXT NOT NULL DEFAULT '',
    platform_instance_id      TEXT,          -- 平台尚未分配时为 NULL（可重复）
    generation                INTEGER NOT NULL DEFAULT 1,
    fencing_token             INTEGER NOT NULL DEFAULT 0,
    owner                     TEXT NOT NULL DEFAULT '',
    address                   TEXT NOT NULL DEFAULT '',
    state                     TEXT NOT NULL DEFAULT 'requested',
    expires_at                TEXT,
    schema_version            INTEGER NOT NULL,
    created_at                TEXT NOT NULL,
    updated_at                TEXT NOT NULL,
    payload                   TEXT NOT NULL,
    UNIQUE (connection_id, platform_instance_id, generation)
);
-- 同一道题最多一个活动租约（设计 7.1）；活动状态集合与
-- models.LEASE_ACTIVE_STATES 保持一致。
CREATE UNIQUE INDEX IF NOT EXISTS uq_instance_leases_active_challenge
    ON instance_leases(competition_challenge_id)
    WHERE state IN ('requested', 'provisioning', 'active', 'renewing',
                    'releasing');
CREATE INDEX IF NOT EXISTS idx_instance_leases_competition
    ON instance_leases(competition_id, state);

CREATE TABLE IF NOT EXISTS scheduler_queue (
    competition_id           TEXT NOT NULL,
    competition_challenge_id TEXT NOT NULL,
    state                    TEXT NOT NULL DEFAULT 'queued',
    priority                 REAL NOT NULL DEFAULT 0,
    score                    REAL NOT NULL DEFAULT 0,
    not_before               TEXT,
    schema_version           INTEGER NOT NULL,
    created_at               TEXT NOT NULL,
    updated_at               TEXT NOT NULL,
    payload                  TEXT NOT NULL,
    PRIMARY KEY (competition_id, competition_challenge_id)
);

CREATE TABLE IF NOT EXISTS resource_budgets (
    competition_id TEXT NOT NULL,
    kind           TEXT NOT NULL,
    "limit"        REAL NOT NULL DEFAULT 0,
    used           REAL NOT NULL DEFAULT 0,
    window         TEXT NOT NULL DEFAULT 'run',
    resets_at      TEXT,
    schema_version INTEGER NOT NULL,
    created_at     TEXT NOT NULL,
    updated_at     TEXT NOT NULL,
    payload        TEXT NOT NULL,
    PRIMARY KEY (competition_id, kind)
);

CREATE TABLE IF NOT EXISTS submission_candidates (
    candidate_id              TEXT PRIMARY KEY,
    competition_id            TEXT NOT NULL DEFAULT '',
    competition_challenge_id  TEXT NOT NULL DEFAULT '',
    answer_slot               INTEGER NOT NULL DEFAULT 1,
    digest                    TEXT NOT NULL DEFAULT '',
    source_run_id             TEXT NOT NULL DEFAULT '',
    state                     TEXT NOT NULL DEFAULT 'candidate',
    schema_version            INTEGER NOT NULL,
    created_at                TEXT NOT NULL,
    updated_at                TEXT NOT NULL,
    payload                   TEXT NOT NULL,
    UNIQUE (competition_challenge_id, answer_slot, digest)
);
CREATE INDEX IF NOT EXISTS idx_submission_candidates_challenge
    ON submission_candidates(competition_challenge_id, state);

CREATE TABLE IF NOT EXISTS platform_submissions (
    submission_id             TEXT PRIMARY KEY,
    competition_id            TEXT NOT NULL DEFAULT '',
    competition_challenge_id  TEXT NOT NULL DEFAULT '',
    candidate_id              TEXT NOT NULL DEFAULT '',
    answer_slot               INTEGER NOT NULL DEFAULT 1,
    digest                    TEXT NOT NULL DEFAULT '',
    attempt                   INTEGER NOT NULL DEFAULT 1,
    state                     TEXT NOT NULL DEFAULT 'queued',
    retry_after_at            TEXT,
    schema_version            INTEGER NOT NULL,
    created_at                TEXT NOT NULL,
    updated_at                TEXT NOT NULL,
    payload                   TEXT NOT NULL,
    -- 平台（经 competition_id）、题目、候选值和 attempt 的幂等键。
    UNIQUE (competition_id, competition_challenge_id, answer_slot, digest, attempt)
);
CREATE INDEX IF NOT EXISTS idx_platform_submissions_challenge
    ON platform_submissions(competition_challenge_id, state);

CREATE TABLE IF NOT EXISTS competition_commands (
    command_id      TEXT PRIMARY KEY,
    idempotency_key TEXT UNIQUE,
    command_type    TEXT NOT NULL DEFAULT '',
    aggregate_type  TEXT NOT NULL DEFAULT '',
    aggregate_id    TEXT NOT NULL DEFAULT '',
    actor_json      TEXT NOT NULL DEFAULT '{}',
    payload_hash    TEXT NOT NULL DEFAULT '',
    schema_version  INTEGER NOT NULL,
    received_at     TEXT NOT NULL,
    payload         TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_competition_commands_type
    ON competition_commands(command_type);

CREATE TABLE IF NOT EXISTS competition_receipts (
    command_id     TEXT PRIMARY KEY,
    receipt_id     TEXT NOT NULL UNIQUE,
    state          TEXT NOT NULL DEFAULT 'accepted',
    run_id         TEXT,
    event_cursor   TEXT,
    error_json     TEXT,
    schema_version INTEGER NOT NULL,
    created_at     TEXT NOT NULL,
    updated_at     TEXT NOT NULL,
    payload        TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_competition_receipts_state
    ON competition_receipts(state);

CREATE TABLE IF NOT EXISTS competition_events (
    seq                  INTEGER PRIMARY KEY AUTOINCREMENT,
    event_id             TEXT NOT NULL UNIQUE,
    competition_id       TEXT NOT NULL DEFAULT '',
    aggregate_type       TEXT NOT NULL,
    aggregate_id         TEXT NOT NULL,
    stream_seq           INTEGER NOT NULL,
    event_type           TEXT NOT NULL,
    event_schema_version INTEGER NOT NULL,
    occurred_at          TEXT NOT NULL,
    producer             TEXT NOT NULL DEFAULT '',
    actor_id             TEXT NOT NULL DEFAULT 'system',
    command_id           TEXT,
    causation_id         TEXT,
    correlation_id       TEXT,
    idempotency_key      TEXT,
    schema_version       INTEGER NOT NULL,
    created_at           TEXT NOT NULL,
    payload              TEXT NOT NULL,
    UNIQUE (aggregate_type, aggregate_id, stream_seq)
);
CREATE INDEX IF NOT EXISTS idx_competition_events_comp
    ON competition_events(competition_id, seq);
CREATE INDEX IF NOT EXISTS idx_competition_events_type
    ON competition_events(event_type);
CREATE INDEX IF NOT EXISTS idx_competition_events_command
    ON competition_events(command_id);

CREATE TABLE IF NOT EXISTS competition_projection_watermarks (
    projection_name TEXT PRIMARY KEY,
    last_event_seq  INTEGER NOT NULL DEFAULT 0,
    schema_version  INTEGER NOT NULL,
    created_at      TEXT NOT NULL,
    updated_at      TEXT NOT NULL,
    payload         TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS competition_outbox (
    outbox_id       TEXT PRIMARY KEY,
    idempotency_key TEXT NOT NULL UNIQUE,
    command_id      TEXT,
    event_id        TEXT,
    aggregate_type  TEXT NOT NULL DEFAULT '',
    aggregate_id    TEXT NOT NULL DEFAULT '',
    event_type      TEXT NOT NULL DEFAULT '',
    destination     TEXT NOT NULL DEFAULT '',
    status          TEXT NOT NULL DEFAULT 'pending',
    attempts        INTEGER NOT NULL DEFAULT 0,
    next_attempt_at TEXT,
    last_error      TEXT,
    delivered_at    TEXT,
    schema_version  INTEGER NOT NULL,
    created_at      TEXT NOT NULL,
    updated_at      TEXT NOT NULL,
    payload         TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_competition_outbox_status
    ON competition_outbox(status, next_attempt_at);

CREATE TABLE IF NOT EXISTS reconcile_checkpoints (
    checkpoint_id  TEXT PRIMARY KEY,
    competition_id TEXT NOT NULL DEFAULT '',
    step           TEXT NOT NULL DEFAULT '',
    status         TEXT NOT NULL DEFAULT 'done',
    event_seq      INTEGER NOT NULL DEFAULT 0,
    schema_version INTEGER NOT NULL,
    created_at     TEXT NOT NULL,
    updated_at     TEXT NOT NULL,
    payload        TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_reconcile_checkpoints_comp
    ON reconcile_checkpoints(competition_id, step);
"""

#: competition.db 全部已发布 migration（单调递增，只能追加更高版本）。
COMPETITION_MIGRATIONS: tuple[Migration, ...] = (
    Migration(version=1, name="competition_core_schema", ddl=_SCHEMA_V1),
)


def apply_competition_migrations(conn) -> list[dict]:
    """应用 competition.db 的未执行 migration，返回本次新应用的 receipt。"""
    return apply_migrations(conn, COMPETITION_MIGRATIONS)


__all__ = [
    "COMPETITION_MIGRATIONS",
    "apply_competition_migrations",
    "applied_versions",
    "current_version",
    "migration_receipts",
]
