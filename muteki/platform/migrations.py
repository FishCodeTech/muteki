"""platform.db 的 schema 与 migration 框架（任务书 5.3、5.4，CORE-02）。

约定：
- migration 版本号单调递增，逐个在独立事务中执行。
- ``schema_migrations`` 历史表记录每次迁移的 receipt（版本、名称、
  应用时间、DDL 校验和），作为恢复记录；重启时已应用的版本不会重复执行。
- 数据迁移不改写历史事件正文；旧事件由 upcaster 在读取/投影时转换
  （见 ``store.UpcasterRegistry``）。
"""

from __future__ import annotations

import hashlib
import json
import sqlite3
from dataclasses import dataclass
from datetime import datetime, timezone
from typing import Optional


class MigrationError(RuntimeError):
    """migration 历史不一致或版本回退。"""


def _utcnow_iso() -> str:
    return datetime.now(timezone.utc).isoformat()


#: v1 全量 schema。每张表都有稳定主键、schema_version、created_at/updated_at
#: 和完整负载 JSON 列（payload）；关键字段独立成列以便查询和加唯一约束。
_SCHEMA_V1 = """
CREATE TABLE IF NOT EXISTS projects (
    project_id     TEXT PRIMARY KEY,
    name           TEXT NOT NULL DEFAULT '',
    schema_version INTEGER NOT NULL,
    created_at     TEXT NOT NULL,
    updated_at     TEXT NOT NULL,
    payload        TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS workspaces (
    workspace_id   TEXT PRIMARY KEY,
    project_id     TEXT,
    kind           TEXT NOT NULL DEFAULT '',
    root_path      TEXT NOT NULL DEFAULT '',
    schema_version INTEGER NOT NULL,
    created_at     TEXT NOT NULL,
    updated_at     TEXT NOT NULL,
    payload        TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_workspaces_project ON workspaces(project_id);

CREATE TABLE IF NOT EXISTS threads (
    thread_id      TEXT PRIMARY KEY,
    project_id     TEXT,
    workspace_id   TEXT,
    mode           TEXT NOT NULL DEFAULT '',
    title          TEXT NOT NULL DEFAULT '',
    schema_version INTEGER NOT NULL,
    created_at     TEXT NOT NULL,
    updated_at     TEXT NOT NULL,
    payload        TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_threads_project ON threads(project_id);

CREATE TABLE IF NOT EXISTS tasks (
    task_id        TEXT PRIMARY KEY,
    thread_id      TEXT,
    project_id     TEXT,
    kind           TEXT NOT NULL DEFAULT '',
    title          TEXT NOT NULL DEFAULT '',
    revision       INTEGER NOT NULL DEFAULT 1,
    schema_version INTEGER NOT NULL,
    created_at     TEXT NOT NULL,
    updated_at     TEXT NOT NULL,
    payload        TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_tasks_thread ON tasks(thread_id);
CREATE INDEX IF NOT EXISTS idx_tasks_project ON tasks(project_id);

CREATE TABLE IF NOT EXISTS agent_sessions (
    agent_session_id     TEXT PRIMARY KEY,
    external_session_id  TEXT,
    adapter_id           TEXT NOT NULL DEFAULT '',
    runtime_instance_id  TEXT,
    thread_id            TEXT,
    run_id               TEXT,
    execution_generation INTEGER,
    closed_at            TEXT,
    schema_version       INTEGER NOT NULL,
    created_at           TEXT NOT NULL,
    updated_at           TEXT NOT NULL,
    payload              TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_agent_sessions_thread ON agent_sessions(thread_id);
CREATE INDEX IF NOT EXISTS idx_agent_sessions_run ON agent_sessions(run_id);

CREATE TABLE IF NOT EXISTS execution_bindings (
    execution_binding_id TEXT PRIMARY KEY,
    run_id               TEXT,
    task_id              TEXT,
    executor_id          TEXT NOT NULL DEFAULT '',
    adapter_id           TEXT,
    runtime_instance_id  TEXT,
    profile_id           TEXT,
    policy_version       INTEGER NOT NULL DEFAULT 1,
    revoked_at           TEXT,
    schema_version       INTEGER NOT NULL,
    created_at           TEXT NOT NULL,
    updated_at           TEXT NOT NULL,
    payload              TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_execution_bindings_run ON execution_bindings(run_id);

CREATE TABLE IF NOT EXISTS capability_bindings (
    binding_id      TEXT NOT NULL,
    binding_version INTEGER NOT NULL,
    thread_id       TEXT NOT NULL DEFAULT '',
    principal_id    TEXT NOT NULL DEFAULT '',
    mode            TEXT NOT NULL DEFAULT '',
    policy_version  INTEGER NOT NULL DEFAULT 1,
    revoked_at      TEXT,
    schema_version  INTEGER NOT NULL,
    created_at      TEXT NOT NULL,
    updated_at      TEXT NOT NULL,
    payload         TEXT NOT NULL,
    PRIMARY KEY (binding_id, binding_version)
);
CREATE INDEX IF NOT EXISTS idx_capability_bindings_thread ON capability_bindings(thread_id);

CREATE TABLE IF NOT EXISTS capability_grants (
    grant_id            TEXT PRIMARY KEY,
    binding_id          TEXT NOT NULL DEFAULT '',
    agent_session_id    TEXT NOT NULL DEFAULT '',
    runtime_instance_id TEXT,
    injection_kind      TEXT NOT NULL DEFAULT '',
    audience            TEXT NOT NULL DEFAULT '',
    credential_ref      TEXT,
    issued_at           TEXT,
    expires_at          TEXT,
    revoked_at          TEXT,
    last_touched_at     TEXT,
    schema_version      INTEGER NOT NULL,
    created_at          TEXT NOT NULL,
    updated_at          TEXT NOT NULL,
    payload             TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_capability_grants_binding ON capability_grants(binding_id);
CREATE INDEX IF NOT EXISTS idx_capability_grants_session ON capability_grants(agent_session_id);

CREATE TABLE IF NOT EXISTS artifacts (
    sha256         TEXT PRIMARY KEY,
    name           TEXT NOT NULL DEFAULT '',
    kind           TEXT NOT NULL DEFAULT '',
    media_type     TEXT NOT NULL DEFAULT '',
    size           INTEGER NOT NULL DEFAULT 0,
    run_id         TEXT,
    schema_version INTEGER NOT NULL,
    created_at     TEXT NOT NULL,
    updated_at     TEXT NOT NULL,
    payload        TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_artifacts_run ON artifacts(run_id);

CREATE TABLE IF NOT EXISTS resource_leases (
    resource_kind  TEXT NOT NULL,
    resource_key   TEXT NOT NULL,
    owner          TEXT NOT NULL DEFAULT '',
    scope          TEXT NOT NULL DEFAULT '',
    ttl_seconds    INTEGER NOT NULL DEFAULT 0,
    fencing_token  INTEGER NOT NULL DEFAULT 0,
    acquired_at    TEXT,
    expires_at     TEXT,
    released_at    TEXT,
    schema_version INTEGER NOT NULL,
    created_at     TEXT NOT NULL,
    updated_at     TEXT NOT NULL,
    payload        TEXT NOT NULL,
    PRIMARY KEY (resource_kind, resource_key)
);

CREATE TABLE IF NOT EXISTS domain_modules (
    module_id      TEXT PRIMARY KEY,
    version        TEXT NOT NULL DEFAULT '',
    workspace_kind TEXT NOT NULL DEFAULT '',
    enabled        INTEGER NOT NULL DEFAULT 1,
    schema_version INTEGER NOT NULL,
    created_at     TEXT NOT NULL,
    updated_at     TEXT NOT NULL,
    payload        TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS extensions (
    extension_id   TEXT PRIMARY KEY,
    origin         TEXT NOT NULL DEFAULT 'installed',
    enabled        INTEGER NOT NULL DEFAULT 1,
    schema_version INTEGER NOT NULL,
    created_at     TEXT NOT NULL,
    updated_at     TEXT NOT NULL,
    payload        TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS extension_versions (
    extension_id   TEXT NOT NULL,
    version        TEXT NOT NULL,
    schema_version INTEGER NOT NULL,
    created_at     TEXT NOT NULL,
    updated_at     TEXT NOT NULL,
    payload        TEXT NOT NULL,
    PRIMARY KEY (extension_id, version)
);

CREATE TABLE IF NOT EXISTS command_receipts (
    command_id      TEXT PRIMARY KEY,
    receipt_id      TEXT NOT NULL UNIQUE,
    idempotency_key TEXT UNIQUE,
    command_type    TEXT NOT NULL DEFAULT '',
    aggregate_type  TEXT NOT NULL DEFAULT '',
    aggregate_id    TEXT NOT NULL DEFAULT '',
    run_id          TEXT,
    state           TEXT NOT NULL DEFAULT 'accepted',
    payload_hash    TEXT NOT NULL DEFAULT '',
    schema_version  INTEGER NOT NULL,
    created_at      TEXT NOT NULL,
    updated_at      TEXT NOT NULL,
    payload         TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_command_receipts_state ON command_receipts(state);
CREATE INDEX IF NOT EXISTS idx_command_receipts_aggregate
    ON command_receipts(aggregate_type, aggregate_id);

CREATE TABLE IF NOT EXISTS domain_events (
    seq                  INTEGER PRIMARY KEY AUTOINCREMENT,
    event_id             TEXT NOT NULL UNIQUE,
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
    updated_at           TEXT NOT NULL,
    payload              TEXT NOT NULL,
    UNIQUE (aggregate_type, aggregate_id, stream_seq)
);
CREATE INDEX IF NOT EXISTS idx_domain_events_type ON domain_events(event_type);
CREATE INDEX IF NOT EXISTS idx_domain_events_command ON domain_events(command_id);

CREATE TABLE IF NOT EXISTS projection_watermarks (
    projection_name TEXT PRIMARY KEY,
    last_event_seq  INTEGER NOT NULL DEFAULT 0,
    schema_version  INTEGER NOT NULL,
    created_at      TEXT NOT NULL,
    updated_at      TEXT NOT NULL,
    payload         TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS outbox (
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
CREATE INDEX IF NOT EXISTS idx_outbox_status ON outbox(status, next_attempt_at);
"""

_SCHEMA_V2 = """
CREATE TABLE IF NOT EXISTS command_ledger_index (
    command_id      TEXT PRIMARY KEY,
    domain          TEXT NOT NULL,
    command_type    TEXT NOT NULL DEFAULT '',
    aggregate_type  TEXT NOT NULL DEFAULT '',
    aggregate_id    TEXT NOT NULL DEFAULT '',
    payload_hash    TEXT NOT NULL,
    created_at      TEXT NOT NULL,
    updated_at      TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_command_ledger_index_domain
    ON command_ledger_index(domain, created_at);

CREATE TABLE IF NOT EXISTS effect_receipts (
    effect_id       TEXT PRIMARY KEY,
    command_id      TEXT NOT NULL,
    receipt_id      TEXT NOT NULL DEFAULT '',
    domain          TEXT NOT NULL DEFAULT '',
    outbox_id       TEXT NOT NULL UNIQUE,
    destination     TEXT NOT NULL DEFAULT '',
    aggregate_type  TEXT NOT NULL DEFAULT '',
    aggregate_id    TEXT NOT NULL DEFAULT '',
    state           TEXT NOT NULL DEFAULT 'accepted',
    attempts        INTEGER NOT NULL DEFAULT 0,
    retry_after     TEXT,
    correlation_id  TEXT NOT NULL DEFAULT '',
    schema_version  INTEGER NOT NULL,
    created_at      TEXT NOT NULL,
    updated_at      TEXT NOT NULL,
    payload         TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_effect_receipts_command
    ON effect_receipts(command_id, created_at);
CREATE INDEX IF NOT EXISTS idx_effect_receipts_state
    ON effect_receipts(state, retry_after);
"""


#: v3 数据修复：旧版 Run / 平台设置 HTTP 入口绕过全局命令路由直接
#: dispatch，回执写进了 platform.db 却没有登记 command_ledger_index，按
#: command_id 查回执时找不到领域。platform.db 的 command_receipts 只存
#: platform 领域的回执，因此按原内容摘要补登记为 platform；已登记的
#: command_id（含 competition）保持不变。
_SCHEMA_V3 = """
INSERT OR IGNORE INTO command_ledger_index
    (command_id, domain, command_type, aggregate_type, aggregate_id,
     payload_hash, created_at, updated_at)
SELECT command_id, 'platform', command_type, aggregate_type, aggregate_id,
       payload_hash, created_at, updated_at
FROM command_receipts
"""


@dataclass(frozen=True)
class Migration:
    """一次单调递增的 schema 迁移。"""

    version: int
    name: str
    ddl: str

    @property
    def checksum(self) -> str:
        return hashlib.sha256(self.ddl.encode("utf-8")).hexdigest()


#: 全部已发布 migration，按版本升序。新增 schema 变更只能追加更高版本。
MIGRATIONS: tuple[Migration, ...] = (
    Migration(version=1, name="core_schema", ddl=_SCHEMA_V1),
    Migration(version=2, name="global_command_and_effect_receipts", ddl=_SCHEMA_V2),
    Migration(version=3, name="backfill_platform_command_index", ddl=_SCHEMA_V3),
)

_HISTORY_DDL = """
CREATE TABLE IF NOT EXISTS schema_migrations (
    version      INTEGER PRIMARY KEY,
    name         TEXT NOT NULL,
    applied_at   TEXT NOT NULL,
    receipt_json TEXT NOT NULL
);
"""


def _applied_versions(conn: sqlite3.Connection) -> dict[int, str]:
    rows = conn.execute(
        "SELECT version, receipt_json FROM schema_migrations ORDER BY version"
    ).fetchall()
    return {int(version): receipt for version, receipt in rows}


def applied_versions(conn: sqlite3.Connection) -> list[int]:
    """已应用的 migration 版本（升序）。"""
    return sorted(_applied_versions(conn))


def migration_receipts(conn: sqlite3.Connection) -> list[dict]:
    """migration 历史 receipt（恢复记录），按版本升序。"""
    receipts: list[dict] = []
    for _version, raw in sorted(_applied_versions(conn).items()):
        receipts.append(json.loads(raw))
    return receipts


def apply_migrations(
    conn: sqlite3.Connection,
    migrations: tuple[Migration, ...] = MIGRATIONS,
) -> list[dict]:
    """应用所有未执行的 migration，返回本次新应用的 receipt 列表。

    每个 migration 在独立事务中执行并写入历史 receipt。版本必须单调：
    数据库里出现高于代码已知版本、或历史中间缺版本时抛 ``MigrationError``。
    """
    versions = [m.version for m in migrations]
    if versions != sorted(versions) or len(set(versions)) != len(versions):
        raise MigrationError("migrations must have strictly increasing versions")

    conn.execute(_HISTORY_DDL)
    conn.commit()

    applied = _applied_versions(conn)
    known = {m.version for m in migrations}
    unknown = set(applied) - known
    if unknown:
        raise MigrationError(
            f"database has migrations unknown to this build: {sorted(unknown)}"
        )
    max_applied = max(applied, default=0)
    missing = [v for v in known if v < max_applied and v not in applied]
    if missing:
        raise MigrationError(
            f"migration history gap below current version {max_applied}: {missing}"
        )

    new_receipts: list[dict] = []
    for migration in migrations:
        if migration.version in applied:
            continue
        receipt = {
            "receipt_id": f"mig-{migration.version:04d}",
            "version": migration.version,
            "name": migration.name,
            "applied_at": _utcnow_iso(),
            "checksum": migration.checksum,
        }
        try:
            with conn:
                # executescript 会先隐式 COMMIT，破坏事务；逐条执行保持原子性。
                # 当前 DDL 只含普通 CREATE 语句，不含语句内分号。
                for statement in migration.ddl.split(";"):
                    if statement.strip():
                        conn.execute(statement)
                conn.execute(
                    "INSERT INTO schema_migrations "
                    "(version, name, applied_at, receipt_json) VALUES (?, ?, ?, ?)",
                    (
                        migration.version,
                        migration.name,
                        receipt["applied_at"],
                        json.dumps(receipt, ensure_ascii=False, sort_keys=True),
                    ),
                )
        except sqlite3.Error as exc:  # 事务已由 with 回滚
            raise MigrationError(
                f"migration {migration.version} ({migration.name}) failed: {exc}"
            ) from exc
        new_receipts.append(receipt)
    return new_receipts


def current_version(conn: sqlite3.Connection) -> Optional[int]:
    """数据库当前 schema 版本；未初始化时返回 None。"""
    try:
        row = conn.execute(
            "SELECT MAX(version) FROM schema_migrations"
        ).fetchone()
    except sqlite3.Error:
        return None
    return int(row[0]) if row and row[0] is not None else None
