"""PlatformStore：platform.db 的对象持久化、命令回执、事件日志与水位（CORE-02）。

对应任务书 5.2/5.3/6.1：
- 单个 SQLite 文件（默认 ``control_root/platform.db``），WAL 模式，
  一条长连接（``check_same_thread=False``）+ ``threading.RLock`` 覆盖
  每一次 execute→fetch；``busy_timeout`` 风格与 ``muteki.swarm.shared_graph``
  一致。未持锁的并发取行会在 pysqlite fetchall / GC 路径上变成原生崩溃（#216）。
- 领域对象独立表：关键字段独立列（可查询、可加唯一约束），完整负载
  为 payload JSON 列；读取时以 payload 为准重建契约模型。
- ``domain_events`` 是 append-only 事件日志，全局 seq 自增，
  (aggregate_type, aggregate_id, stream_seq) 唯一且单调；支持
  expected_version 乐观并发校验。
- ``command_receipts`` 以 command_id / idempotency_key 唯一约束实现
  幂等：重复提交返回已有 receipt（deduplicated=True），内容不一致抛冲突。
- upcaster 只在读取和重建投影时转换旧事件，不改写历史事件正文。
"""

from __future__ import annotations

from contextlib import contextmanager
import hashlib
import json
import os
import sqlite3
import threading
from dataclasses import dataclass
from datetime import datetime
from enum import Enum
from pathlib import Path
from typing import Any, Callable, Optional

from pydantic import Field

from muteki.platform.contracts import (
    AggregateRef,
    AgentSession,
    Artifact,
    CapabilityBinding,
    CapabilityGrant,
    CommandEnvelope,
    CommandReceipt,
    ContractModel,
    DomainModuleDescriptor,
    EventEnvelope,
    EffectReceipt,
    EffectState,
    ExecutionBinding,
    ExtensionManifest,
    Project,
    PublicEvent,
    ReceiptState,
    ResourceLease,
    Task,
    Thread,
    Workspace,
    utcnow,
)
from muteki.platform.migrations import apply_migrations, current_version


class StoreError(RuntimeError):
    """PlatformStore 基础错误。"""


class NotFoundError(StoreError):
    """对象或回执不存在。"""


class IdempotencyConflictError(StoreError):
    """相同 command_id / idempotency_key 提交了不同内容。"""


class OptimisticConcurrencyError(StoreError):
    """expected_version 与当前流版本不一致，或 stream_seq 不连续。"""


class StateConflictError(StoreError):
    """状态字段不允许当前写入（如 fencing token 回退）。"""


# ---------------------------------------------------------------------------
# 默认路径
# ---------------------------------------------------------------------------

#: 未显式给 db_path / control_root 时的默认控制根目录。
DEFAULT_CONTROL_ROOT = Path("state") / "control"


def default_db_path(control_root: str | Path | None = None) -> Path:
    """platform.db 默认路径：``control_root/platform.db``。

    control_root 未给出时依次看 ``MUTEKI_PLATFORM_DB``（完整文件路径）、
    ``MUTEKI_COORDINATOR_CONTROL_ROOT`` 和仓库默认 ``state/control``。
    """
    if control_root is not None:
        return Path(control_root) / "platform.db"
    env_db = os.environ.get("MUTEKI_PLATFORM_DB", "").strip()
    if env_db:
        return Path(env_db)
    env_root = os.environ.get("MUTEKI_COORDINATOR_CONTROL_ROOT", "").strip()
    if env_root:
        return Path(env_root) / "platform.db"
    return DEFAULT_CONTROL_ROOT / "platform.db"


# ---------------------------------------------------------------------------
# 对象表规格：契约模型 → 表 / 主键 / 独立列
# ---------------------------------------------------------------------------


def _column_value(value: Any) -> Any:
    """模型字段值 → SQLite 列值（datetime→ISO8601，Enum→value）。"""
    if value is None:
        return None
    if isinstance(value, datetime):
        return value.isoformat()
    if isinstance(value, Enum):
        return value.value
    if isinstance(value, bool):
        return int(value)
    if isinstance(value, (list, dict)):
        return json.dumps(value, ensure_ascii=False, sort_keys=True)
    return value


@dataclass(frozen=True)
class _ObjectSpec:
    table: str
    pk: tuple[str, ...]            # 主键对应的模型字段
    columns: tuple[str, ...]       # 独立列对应的模型字段（不含 pk）
    # 模型字段名与列名不同时的重命名，例如 DomainModuleDescriptor.id → module_id
    renames: tuple[tuple[str, str], ...] = ()

    def column_of(self, field: str) -> str:
        return dict(self.renames).get(field, field)

    def field_of(self, column: str) -> str:
        reverse = {col: fld for fld, col in self.renames}
        return reverse.get(column, column)


_OBJECT_SPECS: dict[type[ContractModel], _ObjectSpec] = {
    Project: _ObjectSpec("projects", ("project_id",), ("name",)),
    Workspace: _ObjectSpec(
        "workspaces", ("workspace_id",), ("project_id", "kind", "root_path")
    ),
    Thread: _ObjectSpec(
        "threads",
        ("thread_id",),
        ("project_id", "workspace_id", "mode", "title"),
    ),
    Task: _ObjectSpec(
        "tasks",
        ("task_id",),
        ("thread_id", "project_id", "kind", "title", "revision"),
    ),
    AgentSession: _ObjectSpec(
        "agent_sessions",
        ("agent_session_id",),
        (
            "external_session_id",
            "adapter_id",
            "runtime_instance_id",
            "thread_id",
            "run_id",
            "execution_generation",
            "closed_at",
        ),
    ),
    ExecutionBinding: _ObjectSpec(
        "execution_bindings",
        ("execution_binding_id",),
        (
            "run_id",
            "task_id",
            "executor_id",
            "adapter_id",
            "runtime_instance_id",
            "profile_id",
            "policy_version",
            "revoked_at",
        ),
    ),
    CapabilityBinding: _ObjectSpec(
        "capability_bindings",
        ("binding_id", "binding_version"),
        ("thread_id", "principal_id", "mode", "policy_version", "revoked_at"),
    ),
    CapabilityGrant: _ObjectSpec(
        "capability_grants",
        ("grant_id",),
        (
            "binding_id",
            "agent_session_id",
            "runtime_instance_id",
            "injection_kind",
            "audience",
            "credential_ref",
            "issued_at",
            "expires_at",
            "revoked_at",
            "last_touched_at",
        ),
    ),
    Artifact: _ObjectSpec(
        "artifacts", ("sha256",), ("name", "kind", "media_type", "size", "run_id")
    ),
    ResourceLease: _ObjectSpec(
        "resource_leases",
        ("resource_kind", "resource_key"),
        (
            "owner",
            "scope",
            "ttl_seconds",
            "fencing_token",
            "acquired_at",
            "expires_at",
            "released_at",
        ),
    ),
    DomainModuleDescriptor: _ObjectSpec(
        "domain_modules",
        ("id",),
        ("version", "workspace_kind"),
        renames=(("id", "module_id"),),
    ),
}


def _hash_command(command: CommandEnvelope) -> str:
    """命令幂等比较用的内容摘要（类型 + 聚合 + payload）。"""
    raw = json.dumps(
        {
            "command_type": command.command_type,
            "aggregate_type": command.aggregate_type,
            "aggregate_id": command.aggregate_id,
            "payload": command.payload,
        },
        ensure_ascii=False,
        sort_keys=True,
        separators=(",", ":"),
        default=str,
    )
    return hashlib.sha256(raw.encode("utf-8")).hexdigest()


# ---------------------------------------------------------------------------
# Upcaster 注册表
# ---------------------------------------------------------------------------

UpcasterFunc = Callable[[EventEnvelope], EventEnvelope]


class UpcasterRegistry:
    """按 (event_type, schema_version) 注册的事件升级链。

    读取事件和重建投影时，把旧版本事件逐级升到当前版本；历史事件正文
    在数据库里保持不变。注册 ``from_version`` 的 upcaster 表示把该版本
    升级到 ``from_version + 1``，链式执行直到没有更高版本的 upcaster。
    """

    def __init__(self) -> None:
        self._upcasters: dict[tuple[str, int], UpcasterFunc] = {}

    def register(
        self, event_type: str, from_version: int, func: UpcasterFunc
    ) -> None:
        key = (event_type, int(from_version))
        if key in self._upcasters:
            raise ValueError(f"upcaster already registered: {key}")
        self._upcasters[key] = func

    def upcast(self, event: EventEnvelope) -> EventEnvelope:
        current = event
        while True:
            func = self._upcasters.get((current.event_type, current.schema_version))
            if func is None:
                return current
            upgraded = func(current)
            if upgraded.schema_version <= current.schema_version:
                raise StoreError(
                    f"upcaster for {current.event_type} v{current.schema_version} "
                    "did not increase schema_version"
                )
            current = upgraded


# ---------------------------------------------------------------------------
# Snapshot 读模型（供 SSE 恢复与状态查询）
# ---------------------------------------------------------------------------


class StreamHead(ContractModel):
    """一条聚合流的当前水位。"""

    aggregate_type: str = ""
    aggregate_id: str = ""
    head_seq: int = 0


class PlatformSnapshot(ContractModel):
    """Platform 级快照：对象计数、事件水位、投影水位与待处理工作。"""

    generated_at: datetime = Field(default_factory=utcnow)
    schema_version_db: Optional[int] = None
    object_counts: dict[str, int] = Field(default_factory=dict)
    event_watermark: int = 0
    streams: list[StreamHead] = Field(default_factory=list)
    projection_watermarks: dict[str, int] = Field(default_factory=dict)
    pending_receipts: int = 0
    pending_outbox: int = 0


# ---------------------------------------------------------------------------
# PlatformStore
# ---------------------------------------------------------------------------


class PlatformStore:
    """platform.db 的唯一写入口。

    写操作线程安全（内部锁 + 单连接）；WAL 允许其他进程只读观察。
    """

    def __init__(
        self,
        db_path: str | Path | None = None,
        *,
        control_root: str | Path | None = None,
    ) -> None:
        if db_path is None:
            db_path = default_db_path(control_root)
        self.db_path = str(db_path)
        Path(self.db_path).parent.mkdir(parents=True, exist_ok=True)
        self._conn = sqlite3.connect(self.db_path, check_same_thread=False)
        self._conn.row_factory = sqlite3.Row
        self._lock = threading.RLock()
        self._transaction_depth = 0
        cur = self._conn.cursor()
        cur.execute("PRAGMA journal_mode=WAL")
        cur.execute("PRAGMA busy_timeout=5000")
        cur.execute("PRAGMA synchronous=NORMAL")
        cur.execute("PRAGMA foreign_keys=ON")
        self._conn.commit()
        #: 事件 upcaster 注册表；读取与投影重建时应用，不改写历史正文。
        self.upcasters = UpcasterRegistry()
        apply_migrations(self._conn)
        with self.transaction():
            self._conn.execute("CREATE TABLE IF NOT EXISTS command_effect_results (command_id TEXT PRIMARY KEY, payload TEXT NOT NULL)")

    @contextmanager
    def transaction(self):
        """Serialize a synchronous local commit; nested stores share savepoints."""
        with self._lock:
            depth = self._transaction_depth
            savepoint = f"platform_nested_{depth}"
            if depth:
                self._conn.execute(f"SAVEPOINT {savepoint}")
            else:
                self._conn.execute("BEGIN IMMEDIATE")
            self._transaction_depth += 1
            try:
                yield
            except BaseException:
                if depth:
                    self._conn.execute(f"ROLLBACK TO {savepoint}")
                    self._conn.execute(f"RELEASE {savepoint}")
                else:
                    self._conn.rollback()
                raise
            else:
                try:
                    if depth:
                        self._conn.execute(f"RELEASE {savepoint}")
                    else:
                        self._conn.commit()
                except BaseException:
                    if depth:
                        self._conn.execute(f"ROLLBACK TO {savepoint}")
                        self._conn.execute(f"RELEASE {savepoint}")
                    else:
                        self._conn.rollback()
                    raise
            finally:
                self._transaction_depth -= 1

    @property
    def installation_id(self) -> str:
        """An opaque identity persisted in this installation's actual database."""
        from uuid import uuid4
        with self.transaction():
            self._conn.execute("CREATE TABLE IF NOT EXISTS installation_metadata (key TEXT PRIMARY KEY, value TEXT NOT NULL)")
            self._conn.execute("INSERT OR IGNORE INTO installation_metadata VALUES ('installation_id', ?)", (str(uuid4()),))
            return str(self._conn.execute("SELECT value FROM installation_metadata WHERE key='installation_id'").fetchone()[0])

    def save_effect_result(self, command_id: str, payload: dict[str, Any]) -> None:
        """Journal acknowledged effects before projection/receipt finalization."""
        with self.transaction():
            self._conn.execute("INSERT OR REPLACE INTO command_effect_results VALUES (?, ?)",
                               (command_id, json.dumps(payload, ensure_ascii=False)))

    def effect_result(self, command_id: str) -> Optional[dict[str, Any]]:
        row = self._fetchone("SELECT payload FROM command_effect_results WHERE command_id=?", (command_id,))
        return json.loads(row[0]) if row else None

    def artifact_metadata(self, thread_id: str, digest: str) -> Optional[dict[str, Any]]:
        row = self._fetchone(
            "SELECT payload FROM domain_events WHERE aggregate_type='thread' "
            "AND aggregate_id=? AND event_type IN ('core.artifact.attached','core.artifact.created') "
            "AND json_extract(payload,'$.payload.sha256')=? ORDER BY stream_seq DESC LIMIT 1",
            (thread_id, digest))
        return dict(json.loads(row[0])["payload"]) if row else None

    def artifact_digests(self, thread_id: str) -> set[str]:
        rows = self._fetchall(
            "SELECT DISTINCT json_extract(payload,'$.payload.sha256') FROM domain_events "
            "WHERE aggregate_type='thread' AND aggregate_id=? "
            "AND event_type IN ('core.artifact.attached','core.artifact.created')",
            (thread_id,))
        return {str(row[0]) for row in rows if row[0]}

    # -- 生命周期 ---------------------------------------------------------

    @property
    def conn(self) -> sqlite3.Connection:
        """底层连接，供 projections / outbox / reconciler 同事务使用。

        调用方必须持有 ``lock`` 覆盖整个 execute→fetch 区间；
        ``check_same_thread=False`` 不能代替串行化（#216）。
        """
        return self._conn

    @property
    def lock(self) -> threading.RLock:
        """共享连接的互斥锁；ConversationStore 也复用此锁（#138/#216）。"""
        return self._lock

    def close(self) -> None:
        with self._lock:
            self._conn.close()

    def __enter__(self) -> "PlatformStore":
        return self

    def __exit__(self, *exc: Any) -> None:
        self.close()

    def _fetchone(self, sql: str, params: tuple | list = ()) -> Optional[sqlite3.Row]:
        """Serialized execute→fetchone on the shared connection (#216)."""
        with self._lock:
            return self._conn.execute(sql, params).fetchone()

    def _fetchall(self, sql: str, params: tuple | list = ()) -> list[sqlite3.Row]:
        """Serialized execute→fetchall on the shared connection (#216)."""
        with self._lock:
            return list(self._conn.execute(sql, params).fetchall())

    def schema_version(self) -> Optional[int]:
        """当前数据库 schema（migration）版本。"""
        with self._lock:
            return current_version(self._conn)

    # -- 全局命令身份与效果回执 -------------------------------------------

    def claim_command_domain(
        self, command: CommandEnvelope, domain: str
    ) -> dict[str, str]:
        """原子登记 command_id 的唯一业务领域。

        platform 与 competition 使用独立业务数据库，因此由 platform.db
        保存轻量全局索引。相同 ID 只能在同一领域以完全相同内容重放；
        查询回执随后按此索引精确定位，不能依赖数据库查询顺序。
        """
        domain = str(domain or "").strip()
        if not domain:
            raise ValueError("command domain is required")
        digest = _hash_command(command)
        now = utcnow().isoformat()
        with self.transaction():
            self._conn.execute(
                "INSERT OR IGNORE INTO command_ledger_index "
                "(command_id, domain, command_type, aggregate_type, aggregate_id, "
                "payload_hash, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
                (
                    command.command_id, domain, command.command_type,
                    command.aggregate_type, command.aggregate_id, digest, now, now,
                ),
            )
            row = self._conn.execute(
                "SELECT * FROM command_ledger_index WHERE command_id = ?",
                (command.command_id,),
            ).fetchone()
        assert row is not None
        if row["domain"] != domain or row["payload_hash"] != digest:
            raise IdempotencyConflictError(
                f"command {command.command_id} already belongs to domain "
                f"{row['domain']!r} with different content")
        return {str(key): str(row[key] or "") for key in row.keys()}

    def command_domain(self, command_id: str) -> Optional[str]:
        row = self._fetchone(
            "SELECT domain FROM command_ledger_index WHERE command_id = ?",
            (str(command_id),),
        )
        return str(row["domain"]) if row else None

    def save_effect_receipt(self, effect: EffectReceipt) -> EffectReceipt:
        """按 outbox_id 幂等保存一项外部效果回执。"""
        if not effect.outbox_id:
            raise ValueError("effect receipt requires outbox_id")
        aggregate = effect.aggregate or AggregateRef()
        now = utcnow().isoformat()
        with self.transaction():
            existing = self._conn.execute(
                "SELECT payload FROM effect_receipts WHERE outbox_id = ?",
                (effect.outbox_id,),
            ).fetchone()
            if existing is not None:
                prior = EffectReceipt.model_validate_json(existing["payload"])
                if prior.command_id != effect.command_id:
                    raise IdempotencyConflictError(
                        f"outbox {effect.outbox_id} is linked to another command")
                return prior
            self._conn.execute(
                "INSERT INTO effect_receipts (effect_id, command_id, receipt_id, "
                "domain, outbox_id, destination, aggregate_type, aggregate_id, "
                "state, attempts, retry_after, correlation_id, schema_version, "
                "created_at, updated_at, payload) "
                "VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                (
                    effect.effect_id, effect.command_id,
                    effect.acceptance_receipt_id, effect.domain,
                    effect.outbox_id, effect.destination, aggregate.type,
                    aggregate.id, effect.state.value, effect.attempts,
                    effect.retry_after.isoformat() if effect.retry_after else None,
                    effect.correlation_id, effect.schema_version,
                    effect.created_at.isoformat(), now, effect.model_dump_json(),
                ),
            )
        return effect

    def get_effect_receipt(self, effect_id: str) -> Optional[EffectReceipt]:
        row = self._fetchone(
            "SELECT payload FROM effect_receipts WHERE effect_id = ?",
            (str(effect_id),),
        )
        return EffectReceipt.model_validate_json(row["payload"]) if row else None

    def effect_for_outbox(self, outbox_id: str) -> Optional[EffectReceipt]:
        row = self._fetchone(
            "SELECT payload FROM effect_receipts WHERE outbox_id = ?",
            (str(outbox_id),),
        )
        return EffectReceipt.model_validate_json(row["payload"]) if row else None

    def effects_for_command(self, command_id: str) -> list[EffectReceipt]:
        rows = self._fetchall(
            "SELECT payload FROM effect_receipts WHERE command_id = ? "
            "ORDER BY created_at, effect_id",
            (str(command_id),),
        )
        return [EffectReceipt.model_validate_json(row["payload"]) for row in rows]

    def list_effect_receipts(
        self, *, states: Optional[set[EffectState]] = None, limit: int = 200
    ) -> list[EffectReceipt]:
        params: list[Any] = []
        where = ""
        if states:
            values = sorted(state.value for state in states)
            where = "WHERE state IN (" + ",".join("?" for _ in values) + ")"
            params.extend(values)
        params.append(max(1, min(int(limit), 1000)))
        rows = self._fetchall(
            f"SELECT payload FROM effect_receipts {where} "
            "ORDER BY updated_at DESC LIMIT ?",
            tuple(params),
        )
        return [EffectReceipt.model_validate_json(row["payload"]) for row in rows]

    def update_effect_receipt(
        self,
        effect_id: str,
        state: EffectState,
        *,
        attempts: Optional[int] = None,
        retry_after: Optional[datetime] = None,
        error: Any = None,
        output: Optional[dict[str, Any]] = None,
    ) -> EffectReceipt:
        effect = self.get_effect_receipt(effect_id)
        if effect is None:
            raise NotFoundError(f"effect receipt not found: {effect_id}")
        now = utcnow()
        changes: dict[str, Any] = {
            "state": state,
            "updated_at": now,
            "error": error,
        }
        if attempts is not None:
            changes["attempts"] = int(attempts)
        changes["retry_after"] = retry_after
        if output is not None:
            changes["output"] = dict(output)
        if state in {
            EffectState.COMPLETED, EffectState.CANCELLED,
            EffectState.DEAD_LETTER,
        }:
            changes["completed_at"] = now
        else:
            changes["completed_at"] = None
        effect = effect.model_copy(update=changes)
        aggregate = effect.aggregate or AggregateRef()
        with self.transaction():
            self._conn.execute(
                "UPDATE effect_receipts SET state = ?, attempts = ?, "
                "retry_after = ?, aggregate_type = ?, aggregate_id = ?, "
                "updated_at = ?, payload = ? WHERE effect_id = ?",
                (
                    effect.state.value, effect.attempts,
                    effect.retry_after.isoformat() if effect.retry_after else None,
                    aggregate.type, aggregate.id, now.isoformat(),
                    effect.model_dump_json(), effect.effect_id,
                ),
            )
        return effect

    # -- 对象 CRUD ---------------------------------------------------------

    def _spec_for(self, model_cls: type[ContractModel]) -> _ObjectSpec:
        spec = _OBJECT_SPECS.get(model_cls)
        if spec is None:
            raise StoreError(f"no object table registered for {model_cls.__name__}")
        return spec

    def save(self, obj: ContractModel) -> ContractModel:
        """upsert 一个领域对象；有 updated_at 字段的模型会被刷新为当前时间。"""
        spec = self._spec_for(type(obj))
        if "updated_at" in type(obj).model_fields:
            obj = obj.model_copy(update={"updated_at": utcnow()})
        now = utcnow().isoformat()
        fields = list(spec.pk) + list(spec.columns)
        columns = [spec.column_of(f) for f in fields]
        values = [_column_value(getattr(obj, f)) for f in fields]
        created = getattr(obj, "created_at", None)
        created_iso = created.isoformat() if isinstance(created, datetime) else now
        all_columns = columns + ["schema_version", "created_at", "updated_at", "payload"]
        all_values = values + [obj.schema_version, created_iso, now, obj.model_dump_json()]
        placeholders = ", ".join("?" for _ in all_columns)
        update_cols = [c for c in all_columns if c not in {spec.column_of(f) for f in spec.pk} and c != "created_at"]
        update_clause = ", ".join(f"{c}=excluded.{c}" for c in update_cols)
        sql = (
            f"INSERT INTO {spec.table} ({', '.join(all_columns)}) "
            f"VALUES ({placeholders}) "
            f"ON CONFLICT({', '.join(spec.column_of(f) for f in spec.pk)}) "
            f"DO UPDATE SET {update_clause}"
        )
        with self.transaction():
            self._conn.execute(sql, all_values)
        return obj

    def get(self, model_cls: type[ContractModel], *pk: Any) -> Optional[Any]:
        """按主键读取对象；不存在返回 None。"""
        spec = self._spec_for(model_cls)
        if len(pk) != len(spec.pk):
            raise ValueError(f"{spec.table} primary key has {len(spec.pk)} parts")
        where = " AND ".join(f"{spec.column_of(f)} = ?" for f in spec.pk)
        with self._lock:
            row = self._conn.execute(
                f"SELECT payload FROM {spec.table} WHERE {where}",
                tuple(_column_value(v) for v in pk),
            ).fetchone()
        if row is None:
            return None
        return model_cls.model_validate_json(row["payload"])

    def list(self, model_cls: type[ContractModel], **filters: Any) -> list[Any]:
        """按独立列等值过滤列出对象，按 created_at 升序。"""
        spec = self._spec_for(model_cls)
        allowed = set(spec.pk) | set(spec.columns)
        unknown = set(filters) - allowed
        if unknown:
            raise ValueError(
                f"filters must reference dedicated columns of {spec.table}: "
                f"{sorted(unknown)}"
            )
        clauses = [f"{spec.column_of(f)} = ?" for f in filters]
        values = [_column_value(v) for v in filters.values()]
        sql = f"SELECT payload FROM {spec.table}"
        if clauses:
            sql += " WHERE " + " AND ".join(clauses)
        sql += " ORDER BY created_at, rowid"
        rows = self._fetchall(sql, values)
        return [model_cls.model_validate_json(row["payload"]) for row in rows]

    def delete(self, model_cls: type[ContractModel], *pk: Any) -> bool:
        """按主键删除对象，返回是否有行被删除。"""
        spec = self._spec_for(model_cls)
        if len(pk) != len(spec.pk):
            raise ValueError(f"{spec.table} primary key has {len(spec.pk)} parts")
        where = " AND ".join(f"{spec.column_of(f)} = ?" for f in spec.pk)
        with self.transaction():
            cur = self._conn.execute(
                f"DELETE FROM {spec.table} WHERE {where}",
                tuple(_column_value(v) for v in pk),
            )
        return cur.rowcount > 0

    def domain_module_enabled(self, module_id: str) -> Optional[bool]:
        """读取内置/扩展 DomainModule 的持久启用状态。"""
        row = self._fetchone(
            "SELECT enabled FROM domain_modules WHERE module_id = ?",
            (str(module_id),),
        )
        return bool(row["enabled"]) if row is not None else None

    def set_domain_module_enabled(self, module_id: str, enabled: bool) -> None:
        """持久化模块启停；描述必须先通过 ``save`` 登记。"""
        with self.transaction():
            cur = self._conn.execute(
                "UPDATE domain_modules SET enabled = ?, updated_at = ? "
                "WHERE module_id = ?",
                (int(bool(enabled)), utcnow().isoformat(), str(module_id)),
            )
        if cur.rowcount == 0:
            raise NotFoundError(f"DomainModule not found: {module_id}")

    # -- 对象状态字段（revoked_at / expires_at / closed_at 等） --------------

    def _update_state(self, model_cls: type[ContractModel], pk: tuple[Any, ...],
                      **changes: Any) -> Any:
        obj = self.get(model_cls, *pk)
        if obj is None:
            raise NotFoundError(f"{model_cls.__name__} not found: {pk}")
        obj = obj.model_copy(update=changes)
        return self.save(obj)

    def revoke_execution_binding(
        self, execution_binding_id: str, *, at: Optional[datetime] = None
    ) -> ExecutionBinding:
        return self._update_state(
            ExecutionBinding, (execution_binding_id,), revoked_at=at or utcnow()
        )

    def revoke_capability_binding(
        self, binding_id: str, version: int, *, at: Optional[datetime] = None
    ) -> CapabilityBinding:
        return self._update_state(
            CapabilityBinding, (binding_id, version), revoked_at=at or utcnow()
        )

    def revoke_capability_grant(
        self, grant_id: str, *, at: Optional[datetime] = None
    ) -> CapabilityGrant:
        return self._update_state(
            CapabilityGrant, (grant_id,), revoked_at=at or utcnow()
        )

    def touch_capability_grant(self, grant_id: str) -> CapabilityGrant:
        """刷新 grant 的 last_touched_at（活跃心跳）。"""
        return self._update_state(
            CapabilityGrant, (grant_id,), last_touched_at=utcnow()
        )

    def renew_capability_grant(
        self,
        grant_id: str,
        *,
        expires_at: datetime,
        at: Optional[datetime] = None,
    ) -> CapabilityGrant:
        """原子延长活动 grant 的租期，同时刷新活跃时间。

        续期只改变 ``expires_at`` / ``last_touched_at``；grant 身份、凭据引用
        与撤销状态保持不变。已撤销 grant 是终态，不能通过续期恢复。
        """
        touched_at = at or utcnow()
        with self._lock:
            grant = self.get(CapabilityGrant, grant_id)
            if grant is None:
                raise NotFoundError(f"CapabilityGrant not found: {grant_id}")
            if grant.revoked_at is not None:
                raise StateConflictError(
                    f"capability grant revoked: {grant_id}")
            effective_expiry = expires_at
            if (
                grant.expires_at is not None
                and grant.expires_at > effective_expiry
            ):
                effective_expiry = grant.expires_at
            renewed = grant.model_copy(update={
                "expires_at": effective_expiry,
                "last_touched_at": touched_at,
            })
            return self.save(renewed)

    def close_agent_session(
        self, agent_session_id: str, *, at: Optional[datetime] = None
    ) -> AgentSession:
        return self._update_state(
            AgentSession, (agent_session_id,), closed_at=at or utcnow()
        )

    def acquire_lease(self, lease: ResourceLease) -> ResourceLease:
        """占用资源：fencing token 必须严格大于现有持有者，且现有 lease 已释放或过期。"""
        existing = self.get(ResourceLease, lease.resource_kind, lease.resource_key)
        if existing is not None:
            still_held = existing.released_at is None
            if still_held and existing.expires_at is not None:
                still_held = existing.expires_at > utcnow()
            if still_held and existing.owner != lease.owner:
                raise StateConflictError(
                    f"resource lease held by {existing.owner!r}: "
                    f"{lease.resource_kind}/{lease.resource_key}"
                )
            if lease.fencing_token <= existing.fencing_token:
                raise StateConflictError(
                    "fencing_token must increase monotonically: "
                    f"{lease.fencing_token} <= {existing.fencing_token}"
                )
        return self.save(lease)

    def release_lease(
        self, resource_kind: str, resource_key: str, *, at: Optional[datetime] = None
    ) -> ResourceLease:
        return self._update_state(
            ResourceLease, (resource_kind, resource_key), released_at=at or utcnow()
        )

    # -- 扩展（extensions / extension_versions 两表） ------------------------

    def save_extension(self, manifest: ExtensionManifest) -> ExtensionManifest:
        """登记 Agent Plugin 的 Muteki 解析视图，并保留每版历史。"""
        now = utcnow().isoformat()
        payload = manifest.model_dump_json()
        with self.transaction():
            self._conn.execute(
                "INSERT INTO extensions (extension_id, origin, enabled, "
                "schema_version, created_at, updated_at, payload) "
                "VALUES (?, ?, 1, ?, ?, ?, ?) "
                "ON CONFLICT(extension_id) DO UPDATE SET "
                "origin=excluded.origin, schema_version=excluded.schema_version, "
                "updated_at=excluded.updated_at, payload=excluded.payload",
                (
                    manifest.id,
                    manifest.origin,
                    manifest.schema_version,
                    now,
                    now,
                    payload,
                ),
            )
            self._conn.execute(
                "INSERT INTO extension_versions (extension_id, version, "
                "schema_version, created_at, updated_at, payload) "
                "VALUES (?, ?, ?, ?, ?, ?) "
                "ON CONFLICT(extension_id, version) DO UPDATE SET "
                "schema_version=excluded.schema_version, "
                "updated_at=excluded.updated_at, payload=excluded.payload",
                (
                    manifest.id,
                    manifest.version,
                    manifest.schema_version,
                    now,
                    now,
                    payload,
                ),
            )
        return manifest

    def get_extension(self, extension_id: str) -> Optional[ExtensionManifest]:
        row = self._fetchone(
            "SELECT payload FROM extensions WHERE extension_id = ?", (extension_id,)
        )
        return ExtensionManifest.model_validate_json(row["payload"]) if row else None

    def list_extension_versions(self, extension_id: str) -> list[ExtensionManifest]:
        rows = self._fetchall(
            "SELECT payload FROM extension_versions WHERE extension_id = ? "
            "ORDER BY created_at, rowid",
            (extension_id,),
        )
        return [ExtensionManifest.model_validate_json(row["payload"]) for row in rows]

    # -- 命令回执（幂等） ----------------------------------------------------

    def record_command(
        self, command: CommandEnvelope, receipt: CommandReceipt
    ) -> CommandReceipt:
        """持久化命令与回执。

        command_id 或 idempotency_key 已存在时：内容一致返回已有 receipt
        （``deduplicated=True``），内容不一致抛 ``IdempotencyConflictError``。
        """
        if receipt.command_id != command.command_id:
            raise ValueError("receipt.command_id must match command.command_id")
        now = utcnow().isoformat()
        payload_hash = _hash_command(command)
        try:
            with self.transaction():
                self._conn.execute(
                    "INSERT INTO command_receipts (command_id, receipt_id, "
                    "idempotency_key, command_type, aggregate_type, aggregate_id, "
                    "run_id, state, payload_hash, schema_version, created_at, "
                    "updated_at, payload) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                    (
                        command.command_id,
                        receipt.receipt_id,
                        command.idempotency_key,
                        command.command_type,
                        command.aggregate_type,
                        command.aggregate_id,
                        receipt.run_id,
                        receipt.state.value,
                        payload_hash,
                        receipt.schema_version,
                        receipt.submitted_at.isoformat(),
                        now,
                        receipt.model_dump_json(),
                    ),
                )
            return receipt
        except sqlite3.IntegrityError:
            existing = self.check_idempotency(command)
            if existing is None:
                raise
            return existing

    def _find_receipt_row(
        self, command_id: str, idempotency_key: Optional[str]
    ) -> Optional[sqlite3.Row]:
        row = self._fetchone(
            "SELECT * FROM command_receipts WHERE command_id = ?", (command_id,)
        )
        if row is None and idempotency_key is not None:
            row = self._fetchone(
                "SELECT * FROM command_receipts WHERE idempotency_key = ?",
                (idempotency_key,),
            )
        return row

    def check_idempotency(self, command: CommandEnvelope) -> Optional[CommandReceipt]:
        """幂等预检（COMMAND-01 dispatch 的快速去重路径）。

        无记录返回 None；相同 command_id / idempotency_key 但内容不一致抛
        ``IdempotencyConflictError``；内容一致返回已有 receipt
        （``deduplicated=True``）。与 ``record_command`` 的冲突判定共用同一
        内容摘要，保证两条路径语义一致。
        """
        row = self._find_receipt_row(command.command_id, command.idempotency_key)
        if row is None:
            return None
        if row["payload_hash"] != _hash_command(command):
            raise IdempotencyConflictError(
                f"command {command.command_id} / idempotency_key "
                f"{command.idempotency_key!r} replayed with different content"
            )
        return CommandReceipt.model_validate_json(row["payload"]).model_copy(
            update={"deduplicated": True}
        )

    def stream_head(self, aggregate_type: str, aggregate_id: str) -> int:
        """一条聚合流当前的最大 stream_seq（空流为 0）。供 wait/cursor 使用。"""
        return self._stream_head(aggregate_type, aggregate_id)

    def get_receipt(self, command_id: str) -> Optional[CommandReceipt]:
        row = self._fetchone(
            "SELECT payload FROM command_receipts WHERE command_id = ?", (command_id,)
        )
        return CommandReceipt.model_validate_json(row["payload"]) if row else None

    def find_receipt_by_idempotency_key(self, key: str) -> Optional[CommandReceipt]:
        row = self._fetchone(
            "SELECT payload FROM command_receipts WHERE idempotency_key = ?", (key,)
        )
        return CommandReceipt.model_validate_json(row["payload"]) if row else None

    def update_receipt_state(
        self,
        command_id: str,
        state: ReceiptState,
        *,
        error: Any = None,
        event_cursor: Optional[str] = None,
        run_id: Optional[str] = None,
        output: Optional[dict[str, Any]] = None,
        effect_ids: Optional[list[str]] = None,
    ) -> CommandReceipt:
        """推进回执状态（accepted → completed / failed / conflict）。

        ``event_cursor`` / ``run_id`` 非空时一并回写（dispatch 在事件追加后
        才能确定游标）；None 表示保持原值。
        """
        receipt = self.get_receipt(command_id)
        if receipt is None:
            raise NotFoundError(f"receipt not found: {command_id}")
        changes: dict[str, Any] = {"state": state, "error": error}
        if event_cursor is not None:
            changes["event_cursor"] = event_cursor
        if run_id is not None:
            changes["run_id"] = run_id
        if output is not None:
            changes["output"] = dict(output)
        if effect_ids is not None:
            changes["effect_ids"] = list(dict.fromkeys(effect_ids))
        receipt = receipt.model_copy(update=changes)
        with self.transaction():
            self._conn.execute(
                "UPDATE command_receipts SET state = ?, run_id = ?, "
                "updated_at = ?, payload = ? WHERE command_id = ?",
                (
                    state.value,
                    receipt.run_id,
                    utcnow().isoformat(),
                    receipt.model_dump_json(),
                    command_id,
                ),
            )
        return receipt

    def pending_receipts(self) -> list[CommandReceipt]:
        """仍处于 accepted 的回执（重启后需要调用方回放）。"""
        rows = self._fetchall(
            "SELECT payload FROM command_receipts "
            "WHERE state IN (?, ?, ?) ORDER BY created_at, rowid",
            (
                ReceiptState.ACCEPTED.value, ReceiptState.RUNNING.value,
                ReceiptState.WAITING.value,
            ),
        )
        return [CommandReceipt.model_validate_json(row["payload"]) for row in rows]

    # -- 领域事件（append-only + expected_version） ---------------------------

    def _stream_head(self, aggregate_type: str, aggregate_id: str) -> int:
        row = self._fetchone(
            "SELECT MAX(stream_seq) FROM domain_events "
            "WHERE aggregate_type = ? AND aggregate_id = ?",
            (aggregate_type, aggregate_id),
        )
        return int(row[0]) if row and row[0] is not None else 0

    def append_events(
        self,
        events: list[EventEnvelope] | EventEnvelope,
        *,
        expected_version: Optional[int] = None,
    ) -> list[EventEnvelope]:
        """追加领域事件，返回带最终 stream_seq 的 envelope 列表。

        - ``expected_version`` 非空时要求目标流当前 head 恰好等于它，
          否则抛 ``OptimisticConcurrencyError``；给出 expected_version 时
          所有事件必须属于同一条流。
        - envelope.stream_seq 为 0 时由存储按流内 head + 1 自动编号；
          显式给出的 stream_seq 必须恰好是下一个序号。
        """
        if isinstance(events, EventEnvelope):
            events = [events]
        if not events:
            return []
        streams = {(e.aggregate_type, e.aggregate_id) for e in events}
        if expected_version is not None and len(streams) != 1:
            raise ValueError(
                "expected_version requires all events to target one stream"
            )
        now = utcnow().isoformat()
        stored: list[EventEnvelope] = []
        with self.transaction():
            heads: dict[tuple[str, str], int] = {}
            checked: set[tuple[str, str]] = set()
            for event in events:
                key = (event.aggregate_type, event.aggregate_id)
                if key not in heads:
                    heads[key] = self._stream_head(*key)
                if expected_version is not None and key not in checked:
                    if heads[key] != expected_version:
                        raise OptimisticConcurrencyError(
                            f"expected_version {expected_version} != current "
                            f"stream head {heads[key]} for "
                            f"{event.aggregate_type}/{event.aggregate_id}"
                        )
                    checked.add(key)
                next_seq = heads[key] + 1
                if event.stream_seq and event.stream_seq != next_seq:
                    raise OptimisticConcurrencyError(
                        f"stream_seq {event.stream_seq} is not the next "
                        f"sequence {next_seq} for "
                        f"{event.aggregate_type}/{event.aggregate_id}"
                    )
                event = event.model_copy(update={"stream_seq": next_seq})
                self._conn.execute(
                    "INSERT INTO domain_events (event_id, aggregate_type, "
                    "aggregate_id, stream_seq, event_type, event_schema_version, "
                    "occurred_at, producer, actor_id, command_id, causation_id, "
                    "correlation_id, idempotency_key, schema_version, created_at, "
                    "updated_at, payload) "
                    "VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                    (
                        event.event_id,
                        event.aggregate_type,
                        event.aggregate_id,
                        event.stream_seq,
                        event.event_type,
                        event.schema_version,
                        event.occurred_at.isoformat(),
                        event.producer,
                        event.actor_id,
                        event.command_id,
                        event.causation_id,
                        event.correlation_id,
                        event.idempotency_key,
                        event.schema_version,
                        now,
                        now,
                        event.model_dump_json(),
                    ),
                )
                heads[key] = next_seq
                stored.append(event)
        return stored

    def _row_to_event(self, row: sqlite3.Row, *, upcast: bool) -> EventEnvelope:
        event = EventEnvelope.model_validate_json(row["payload"])
        return self.upcasters.upcast(event) if upcast else event

    def read_events(
        self,
        aggregate_type: str,
        aggregate_id: str,
        *,
        after_seq: int = 0,
        limit: int = 100,
        upcast: bool = True,
    ) -> list[EventEnvelope]:
        """按流读取事件（stream_seq 升序）。upcast 时旧事件在读取侧升级。"""
        with self._lock:
            rows = self._conn.execute(
                "SELECT payload FROM domain_events "
                "WHERE aggregate_type = ? AND aggregate_id = ? AND stream_seq > ? "
                "ORDER BY stream_seq LIMIT ?",
                (aggregate_type, aggregate_id, int(after_seq), int(limit)),
            ).fetchall()
        return [self._row_to_event(row, upcast=upcast) for row in rows]

    def read_all_events(
        self,
        *,
        after_seq: int = 0,
        limit: int = 1000,
        upcast: bool = True,
    ) -> list[tuple[int, EventEnvelope]]:
        """按全局 seq 读取全部事件（投影消费与 Public Event 用）。"""
        with self._lock:
            rows = self._conn.execute(
                "SELECT seq, payload FROM domain_events WHERE seq > ? "
                "ORDER BY seq LIMIT ?",
                (int(after_seq), int(limit)),
            ).fetchall()
        return [
            (int(row["seq"]), self._row_to_event(row, upcast=upcast)) for row in rows
        ]

    def event_watermark(self) -> int:
        """事件日志全局水位（最大 seq；空日志为 0）。SSE 恢复游标基于此值。"""
        with self._lock:
            row = self._conn.execute("SELECT MAX(seq) FROM domain_events").fetchone()
        return int(row[0]) if row and row[0] is not None else 0

    def public_events(
        self, *, after_seq: int = 0, limit: int = 100
    ) -> list[tuple[int, PublicEvent]]:
        """完整 Public Event 页；返回 (全局 seq, PublicEvent)。"""
        page: list[tuple[int, PublicEvent]] = []
        for seq, event in self.read_all_events(after_seq=after_seq, limit=limit):
            data = event.model_dump(mode="python")
            page.append((seq, PublicEvent(**data)))
        return page

    def public_events_for(
        self,
        aggregate_type: str,
        aggregate_id: str,
        *,
        after_seq: int = 0,
        limit: int = 100,
    ) -> list[tuple[int, PublicEvent]]:
        """按聚合过滤的 Public Event 页；游标仍是全局 seq。"""
        with self._lock:
            rows = self._conn.execute(
                "SELECT seq, payload FROM domain_events "
                "WHERE aggregate_type = ? AND aggregate_id = ? AND seq > ? "
                "ORDER BY seq LIMIT ?",
                (str(aggregate_type), str(aggregate_id), int(after_seq), int(limit)),
            ).fetchall()
        page: list[tuple[int, PublicEvent]] = []
        for row in rows:
            event = self._row_to_event(row, upcast=True)
            data = event.model_dump(mode="python")
            page.append((int(row["seq"]), PublicEvent(**data)))
        return page

    # -- 快照与水位 ----------------------------------------------------------

    def snapshot(self) -> PlatformSnapshot:
        """Platform 级快照：对象计数、事件水位、流 head、投影与待处理工作。"""
        with self._lock:
            counts: dict[str, int] = {}
            for spec in {id(s): s for s in _OBJECT_SPECS.values()}.values():
                row = self._conn.execute(
                    f"SELECT COUNT(*) FROM {spec.table}"  # noqa: S608 表名来自代码常量
                ).fetchone()
                counts[spec.table] = int(row[0])
            stream_rows = self._conn.execute(
                "SELECT aggregate_type, aggregate_id, MAX(stream_seq) AS head "
                "FROM domain_events GROUP BY aggregate_type, aggregate_id "
                "ORDER BY aggregate_type, aggregate_id"
            ).fetchall()
            streams = [
                StreamHead(
                    aggregate_type=row["aggregate_type"],
                    aggregate_id=row["aggregate_id"],
                    head_seq=int(row["head"]),
                )
                for row in stream_rows
            ]
            watermark_rows = self._conn.execute(
                "SELECT projection_name, last_event_seq FROM projection_watermarks"
            ).fetchall()
            watermarks = {
                row["projection_name"]: int(row["last_event_seq"])
                for row in watermark_rows
            }
            pending_receipts = self._conn.execute(
                "SELECT COUNT(*) FROM command_receipts WHERE state = ?",
                (ReceiptState.ACCEPTED.value,),
            ).fetchone()[0]
            pending_outbox = self._conn.execute(
                "SELECT COUNT(*) FROM outbox WHERE status != 'delivered'"
            ).fetchone()[0]
            schema_version_db = current_version(self._conn)
            event_row = self._conn.execute(
                "SELECT MAX(seq) FROM domain_events"
            ).fetchone()
            event_wm = int(event_row[0]) if event_row and event_row[0] is not None else 0
        return PlatformSnapshot(
            generated_at=utcnow(),
            schema_version_db=schema_version_db,
            object_counts=counts,
            event_watermark=event_wm,
            streams=streams,
            projection_watermarks=watermarks,
            pending_receipts=int(pending_receipts),
            pending_outbox=int(pending_outbox),
        )
