"""CompetitionStore：competition.db 的实体持久化、命令回执、事件日志与水位
（任务书 10.2，设计 7.1/9/10，COMP-01）。

- 单个 SQLite 文件（默认 ``control_root/competition.db``），WAL 模式，
  一条长连接 + busy_timeout，风格与 ``muteki.platform.store`` 一致；
  competition.db 是独立数据库，不与 platform.db 共享表，也不向任何 Run 的
  SharedGraph 写比赛状态。
- 领域实体独立表：关键字段独立列（可查询、可加唯一约束），完整负载为
  payload JSON 列；读取时以 payload 为准重建 ``models`` 中的契约模型。
- ``competition_events`` 是 append-only 事件日志，全局 seq 自增，
  (aggregate_type, aggregate_id, stream_seq) 唯一且单调；事件 envelope
  统一使用 ``contracts.EventEnvelope``（命名空间 competition.*，提交事件
  competition.platform_submission.*），支持 expected_version 乐观并发。
- ``competition_commands`` / ``competition_receipts`` 以 command_id /
  idempotency_key 唯一约束实现幂等：重复命令返回同一 receipt
  （deduplicated=True），内容不一致抛 ``IdempotencyConflictError``。
- 远端删除形成 tombstone（实体行的 tombstoned 标记），不物理删除历史
  revision 和 RunBinding（任务书 10.4）。
"""

from __future__ import annotations

import hashlib
import json
import os
import sqlite3
import threading
from contextlib import contextmanager
from dataclasses import dataclass
from datetime import datetime, timedelta, timezone
from enum import Enum
from pathlib import Path
from typing import Any, Optional

from pydantic import Field

from muteki.platform.contracts.base import ContractModel, utcnow
from muteki.platform.contracts.commands import CommandEnvelope
from muteki.platform.contracts.events import EventEnvelope
from muteki.platform.contracts.receipts import (
    CommandReceipt,
    OutboxRecord,
    OutboxStatus,
    ReceiptState,
)
from muteki.platform.migrations import current_version
from muteki.platform.store import (
    IdempotencyConflictError as _PlatformIdempotencyConflictError,
    NotFoundError as _PlatformNotFoundError,
    OptimisticConcurrencyError as _PlatformOptimisticConcurrencyError,
    StateConflictError as _PlatformStateConflictError,
    StoreError as _PlatformStoreError,
)
from muteki.competition.migrations import apply_competition_migrations
from muteki.competition.models import (
    BINDING_ACTIVE_STATES,
    LEASE_ACTIVE_STATES,
    ArtifactObject,
    ChallengeArtifact,
    ChallengeRevision,
    ChallengeState,
    Competition,
    CompetitionChallenge,
    CompetitionPolicy,
    InstanceLease,
    PlatformConnection,
    PlatformSubmission,
    ReconcileCheckpoint,
    ResourceBudget,
    RunBinding,
    SchedulerQueueEntry,
    SubmissionCandidate,
    SubmissionState,
    SyncCursor,
)


class StoreError(_PlatformStoreError):
    """CompetitionStore 基础错误。

    继承 platform 层对应错误，使共享 ``MutekiCommandApiImpl.dispatch``
    的既有异常处理（幂等冲突 / 乐观并发 / not_found）对本存储同样生效。
    """


class NotFoundError(StoreError, _PlatformNotFoundError):
    """对象或回执不存在。"""


class IdempotencyConflictError(StoreError, _PlatformIdempotencyConflictError):
    """相同 command_id / idempotency_key 提交了不同内容。"""


class OptimisticConcurrencyError(StoreError, _PlatformOptimisticConcurrencyError):
    """expected_version 与当前流版本不一致，或 stream_seq 不连续。"""


class UniqueConflictError(StoreError):
    """唯一约束冲突（实体唯一身份 / 部分唯一索引），message 含表与列。"""


class StateConflictError(StoreError, _PlatformStateConflictError):
    """状态字段不允许当前写入（如 fencing token 回退、非法状态转移落库）。"""


# ---------------------------------------------------------------------------
# 默认路径
# ---------------------------------------------------------------------------


def default_db_path(control_root: str | Path | None = None) -> Path:
    """competition.db 默认路径：``control_root/competition.db``。

    control_root 未给出时依次看 ``MUTEKI_COMPETITION_DB``（完整文件路径）、
    ``MUTEKI_COORDINATOR_CONTROL_ROOT`` 和仓库默认 ``state/control``。
    """
    if control_root is not None:
        return Path(control_root) / "competition.db"
    env_db = os.environ.get("MUTEKI_COMPETITION_DB", "").strip()
    if env_db:
        return Path(env_db)
    env_root = os.environ.get("MUTEKI_COORDINATOR_CONTROL_ROOT", "").strip()
    if env_root:
        return Path(env_root) / "competition.db"
    return Path("state") / "control" / "competition.db"


# ---------------------------------------------------------------------------
# 实体表规格：模型 → 表 / 主键 / 独立列
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
    #: 这些列把空串归一为 NULL：唯一约束里 NULL 可重复（如租约尚未分配
    #: 平台实例 id 时，(connection_id, NULL, generation) 互不冲突）。
    null_if_empty: tuple[str, ...] = ()


_OBJECT_SPECS: dict[type[ContractModel], _ObjectSpec] = {
    PlatformConnection: _ObjectSpec(
        "platform_connections",
        ("connection_id",),
        ("platform_kind", "canonical_base_url", "account_key",
         "credential_ref", "status"),
    ),
    Competition: _ObjectSpec(
        "competitions",
        ("competition_id",),
        ("connection_id", "external_competition_id", "title",
         "scheduler_state", "tombstoned"),
    ),
    CompetitionPolicy: _ObjectSpec(
        "competition_policies", ("competition_id",), ("automation_mode",),
    ),
    CompetitionChallenge: _ObjectSpec(
        "competition_challenges",
        ("challenge_id",),
        ("competition_id", "external_challenge_id", "name", "category",
         "current_revision_id", "remote_state", "state", "paused_from",
         "tombstoned"),
    ),
    ChallengeRevision: _ObjectSpec(
        "challenge_revisions",
        ("revision_id",),
        ("competition_challenge_id", "content_hash", "revision_seq",
         "name", "category", "points"),
    ),
    ArtifactObject: _ObjectSpec(
        "artifact_objects",
        ("sha256",),
        ("size", "media_type", "origin", "local_path"),
    ),
    ChallengeArtifact: _ObjectSpec(
        "challenge_artifacts",
        ("revision_id", "sha256"),
        ("name", "position"),
    ),
    SyncCursor: _ObjectSpec(
        "sync_cursors",
        ("competition_id", "kind"),
        ("cursor", "etag"),
    ),
    RunBinding: _ObjectSpec(
        "run_bindings",
        ("binding_id",),
        ("competition_id", "competition_challenge_id", "revision_id",
         "run_id", "execution_generation", "lease_id", "state"),
    ),
    InstanceLease: _ObjectSpec(
        "instance_leases",
        ("lease_id",),
        ("connection_id", "competition_id", "competition_challenge_id",
         "platform_instance_id", "generation", "fencing_token", "owner",
         "address", "state", "expires_at"),
        null_if_empty=("platform_instance_id",),
    ),
    SchedulerQueueEntry: _ObjectSpec(
        "scheduler_queue",
        ("competition_id", "competition_challenge_id"),
        ("state", "priority", "score", "not_before"),
    ),
    ResourceBudget: _ObjectSpec(
        "resource_budgets",
        ("competition_id", "kind"),
        ("limit", "used", "window", "resets_at"),
    ),
    SubmissionCandidate: _ObjectSpec(
        "submission_candidates",
        ("candidate_id",),
        ("competition_id", "competition_challenge_id", "answer_slot",
         "digest", "source_run_id", "state"),
    ),
    PlatformSubmission: _ObjectSpec(
        "platform_submissions",
        ("submission_id",),
        ("competition_id", "competition_challenge_id", "candidate_id",
         "answer_slot", "digest", "attempt", "state", "retry_after_at"),
    ),
    ReconcileCheckpoint: _ObjectSpec(
        "reconcile_checkpoints",
        ("checkpoint_id",),
        ("competition_id", "step", "status", "event_seq"),
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
# Snapshot 读模型（任务书 COMP-01：题目、队列、租约、提交、水位的聚合读模型）
# ---------------------------------------------------------------------------


class ChallengeSnapshot(ContractModel):
    """单题的聚合视图：题目 + 当前 revision + 活动 binding / lease + 提交。"""

    challenge: CompetitionChallenge
    current_revision: Optional[ChallengeRevision] = None
    active_binding: Optional[RunBinding] = None
    active_lease: Optional[InstanceLease] = None
    queue_entry: Optional[SchedulerQueueEntry] = None
    candidates: list[SubmissionCandidate] = Field(default_factory=list)
    submissions: list[PlatformSubmission] = Field(default_factory=list)


class CompetitionSnapshot(ContractModel):
    """比赛级快照：题目、队列、租约、提交、预算与水位的聚合读模型。"""

    generated_at: datetime = Field(default_factory=utcnow)
    schema_version_db: Optional[int] = None
    connection: Optional[PlatformConnection] = None
    competition: Optional[Competition] = None
    policy: Optional[CompetitionPolicy] = None
    challenges: list[ChallengeSnapshot] = Field(default_factory=list)
    queue: list[SchedulerQueueEntry] = Field(default_factory=list)
    budgets: list[ResourceBudget] = Field(default_factory=list)
    event_watermark: int = 0
    projection_watermarks: dict[str, int] = Field(default_factory=dict)
    pending_receipts: int = 0
    pending_outbox: int = 0


# ---------------------------------------------------------------------------
# CompetitionStore
# ---------------------------------------------------------------------------


class CompetitionStore:
    """competition.db 的唯一写入口。

    写操作线程安全（内部锁 + 单连接）；WAL 允许其他进程只读观察。
    提供给 Command API 的鸭子类型接口（record_command / append_events /
    read_events / update_receipt_state …）与 ``PlatformStore`` 同名同语义，
    使共享 ``MutekiCommandApiImpl`` 可以直接绑定到本存储。
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
        # 新建库从一开始启用增量回收；已有库会在运维 VACUUM 后采用该模式。
        # 这样后续清理历史事件时可以归还空页，不再永久保留峰值文件体积。
        cur.execute("PRAGMA auto_vacuum=INCREMENTAL")
        cur.execute("PRAGMA journal_mode=WAL")
        cur.execute("PRAGMA busy_timeout=5000")
        cur.execute("PRAGMA synchronous=NORMAL")
        cur.execute("PRAGMA foreign_keys=ON")
        self._conn.commit()
        apply_competition_migrations(self._conn)
        with self.transaction():
            self._conn.execute("CREATE TABLE IF NOT EXISTS command_effect_results (command_id TEXT PRIMARY KEY, payload TEXT NOT NULL)")
        self.outbox = CompetitionOutboxManager(self)

    @contextmanager
    def transaction(self):
        """Serialize a synchronous local commit; nested stores share savepoints."""
        with self._lock:
            depth = self._transaction_depth
            savepoint = f"competition_nested_{depth}"
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

    def save_effect_result(self, command_id: str, payload: dict[str, Any]) -> None:
        with self.transaction():
            self._conn.execute("INSERT OR REPLACE INTO command_effect_results VALUES (?, ?)",
                               (command_id, json.dumps(payload, ensure_ascii=False)))

    def effect_result(self, command_id: str) -> Optional[dict[str, Any]]:
        with self._lock:
            row = self._conn.execute("SELECT payload FROM command_effect_results WHERE command_id=?", (command_id,)).fetchone()
        return json.loads(row[0]) if row else None

    # -- 生命周期 ---------------------------------------------------------

    @property
    def conn(self) -> sqlite3.Connection:
        """底层连接，供 projections / reconciler 同事务使用。"""
        return self._conn

    @property
    def lock(self) -> threading.RLock:
        return self._lock

    def close(self) -> None:
        with self._lock:
            self._conn.close()

    def __enter__(self) -> "CompetitionStore":
        return self

    def __exit__(self, *exc: Any) -> None:
        self.close()

    def schema_version(self) -> Optional[int]:
        """当前数据库 schema（migration）版本。"""
        return current_version(self._conn)

    # -- 实体 CRUD ---------------------------------------------------------

    @staticmethod
    def _qident(name: str) -> str:
        """SQL 标识符加引号：``resource_budgets.limit`` 等列名是 SQL 关键字，
        不加引号会触发 syntax error（COMP-06 最小扩展：只加引号，不改语义）。"""
        return '"' + name.replace('"', '""') + '"'

    def _spec_for(self, model_cls: type[ContractModel]) -> _ObjectSpec:
        spec = _OBJECT_SPECS.get(model_cls)
        if spec is None:
            raise StoreError(f"no object table registered for {model_cls.__name__}")
        return spec

    @staticmethod
    def _unique_error(spec: _ObjectSpec, exc: sqlite3.IntegrityError) -> UniqueConflictError:
        return UniqueConflictError(
            f"unique constraint violation on {spec.table}: {exc}"
        )

    def save(self, obj: ContractModel) -> ContractModel:
        """upsert 一个实体；唯一约束冲突抛 ``UniqueConflictError``（准确表/列）。"""
        spec = self._spec_for(type(obj))
        if "updated_at" in type(obj).model_fields:
            obj = obj.model_copy(update={"updated_at": utcnow()})
        now = utcnow().isoformat()
        fields = list(spec.pk) + list(spec.columns)
        null_if_empty = set(spec.null_if_empty)
        values = [
            None if (f in null_if_empty and getattr(obj, f) == "")
            else _column_value(getattr(obj, f))
            for f in fields
        ]
        created = getattr(obj, "created_at", None)
        created_iso = created.isoformat() if isinstance(created, datetime) else now
        all_columns = fields + ["schema_version", "created_at", "updated_at", "payload"]
        all_values = values + [obj.schema_version, created_iso, now, obj.model_dump_json()]
        quoted_columns = ", ".join(self._qident(c) for c in all_columns)
        placeholders = ", ".join("?" for _ in all_columns)
        pk_set = set(spec.pk)
        update_cols = [c for c in all_columns if c not in pk_set and c != "created_at"]
        update_clause = ", ".join(
            f"{self._qident(c)}=excluded.{self._qident(c)}" for c in update_cols)
        sql = (
            f"INSERT INTO {spec.table} ({quoted_columns}) "
            f"VALUES ({placeholders}) "
            f"ON CONFLICT({', '.join(self._qident(c) for c in spec.pk)}) "
            f"DO UPDATE SET {update_clause}"
        )
        try:
            with self.transaction():
                self._conn.execute(sql, all_values)
        except sqlite3.IntegrityError as exc:
            raise self._unique_error(spec, exc) from exc
        return obj

    def get(self, model_cls: type[ContractModel], *pk: Any) -> Optional[Any]:
        """按主键读取实体；不存在返回 None。"""
        spec = self._spec_for(model_cls)
        if len(pk) != len(spec.pk):
            raise ValueError(f"{spec.table} primary key has {len(spec.pk)} parts")
        where = " AND ".join(f"{self._qident(f)} = ?" for f in spec.pk)
        row = self._conn.execute(
            f"SELECT payload FROM {spec.table} WHERE {where}",
            tuple(_column_value(v) for v in pk),
        ).fetchone()
        if row is None:
            return None
        return model_cls.model_validate_json(row["payload"])

    def list(self, model_cls: type[ContractModel], **filters: Any) -> list[Any]:
        """按独立列等值过滤列出实体，按 created_at 升序。"""
        spec = self._spec_for(model_cls)
        allowed = set(spec.pk) | set(spec.columns)
        unknown = set(filters) - allowed
        if unknown:
            raise ValueError(
                f"filters must reference dedicated columns of {spec.table}: "
                f"{sorted(unknown)}"
            )
        clauses = [f"{self._qident(f)} = ?" for f in filters]
        values = [_column_value(v) for v in filters.values()]
        sql = f"SELECT payload FROM {spec.table}"
        if clauses:
            sql += " WHERE " + " AND ".join(clauses)
        sql += " ORDER BY created_at, rowid"
        rows = self._conn.execute(sql, values).fetchall()
        return [model_cls.model_validate_json(row["payload"]) for row in rows]

    # -- 领域查询助手（Command Handler / Reconciler 使用） ---------------------

    def connection_by_identity(
        self, platform_kind: str, canonical_base_url: str, account_key: str
    ) -> Optional[PlatformConnection]:
        rows = self.list(
            PlatformConnection,
            platform_kind=platform_kind,
            canonical_base_url=canonical_base_url,
            account_key=account_key,
        )
        return rows[0] if rows else None

    def competition_by_external(
        self, connection_id: str, external_competition_id: str
    ) -> Optional[Competition]:
        rows = self.list(
            Competition,
            connection_id=connection_id,
            external_competition_id=external_competition_id,
        )
        return rows[0] if rows else None

    def challenge_by_external(
        self, competition_id: str, external_challenge_id: str
    ) -> Optional[CompetitionChallenge]:
        rows = self.list(
            CompetitionChallenge,
            competition_id=competition_id,
            external_challenge_id=external_challenge_id,
        )
        return rows[0] if rows else None

    def revision_by_hash(
        self, competition_challenge_id: str, content_hash: str
    ) -> Optional[ChallengeRevision]:
        rows = self.list(
            ChallengeRevision,
            competition_challenge_id=competition_challenge_id,
            content_hash=content_hash,
        )
        return rows[0] if rows else None

    def next_revision_seq(self, competition_challenge_id: str) -> int:
        row = self._conn.execute(
            "SELECT MAX(revision_seq) FROM challenge_revisions "
            "WHERE competition_challenge_id = ?",
            (competition_challenge_id,),
        ).fetchone()
        return (int(row[0]) if row and row[0] is not None else 0) + 1

    def active_binding_for_challenge(
        self, competition_challenge_id: str
    ) -> Optional[RunBinding]:
        """该题当前的活动 binding（设计 9.3 规则 5：最多一个）。"""
        states = sorted(s.value for s in BINDING_ACTIVE_STATES)
        placeholders = ", ".join("?" for _ in states)
        row = self._conn.execute(
            f"SELECT payload FROM run_bindings "
            f"WHERE competition_challenge_id = ? AND state IN ({placeholders}) "
            f"ORDER BY created_at, rowid LIMIT 1",  # noqa: S608 占位符参数化
            (competition_challenge_id, *states),
        ).fetchone()
        return RunBinding.model_validate_json(row["payload"]) if row else None

    def binding_by_run(
        self, run_id: str, execution_generation: int
    ) -> Optional[RunBinding]:
        rows = self.list(
            RunBinding, run_id=run_id, execution_generation=execution_generation
        )
        return rows[0] if rows else None

    def active_lease_for_challenge(
        self, competition_challenge_id: str
    ) -> Optional[InstanceLease]:
        """该题当前的活动租约（设计 7.1：最多一个）。"""
        states = sorted(s.value for s in LEASE_ACTIVE_STATES)
        placeholders = ", ".join("?" for _ in states)
        row = self._conn.execute(
            f"SELECT payload FROM instance_leases "
            f"WHERE competition_challenge_id = ? AND state IN ({placeholders}) "
            f"ORDER BY created_at, rowid LIMIT 1",  # noqa: S608 占位符参数化
            (competition_challenge_id, *states),
        ).fetchone()
        return InstanceLease.model_validate_json(row["payload"]) if row else None

    def next_fencing_token(self, connection_id: str, platform_instance_id: str) -> int:
        """同一 (connection_id, platform_instance_id) 的下一个 fencing token。"""
        row = self._conn.execute(
            "SELECT MAX(fencing_token) FROM instance_leases "
            "WHERE connection_id = ? AND platform_instance_id = ?",
            (connection_id, platform_instance_id),
        ).fetchone()
        return (int(row[0]) if row and row[0] is not None else 0) + 1

    def next_instance_generation(
        self,
        connection_id: str,
        platform_instance_id: str,
        *,
        exclude_lease_id: str = "",
    ) -> int:
        """同一平台实例的下一个 generation，更新当前租约时排除自身。"""
        row = self._conn.execute(
            "SELECT MAX(generation) FROM instance_leases "
            "WHERE connection_id = ? AND platform_instance_id = ? "
            "AND lease_id != ?",
            (connection_id, platform_instance_id, exclude_lease_id),
        ).fetchone()
        return (int(row[0]) if row and row[0] is not None else 0) + 1

    def next_submission_attempt(
        self, competition_challenge_id: str, answer_slot: int, digest: str
    ) -> int:
        """同一候选值的下一个 attempt 序号（幂等键的末段）。"""
        row = self._conn.execute(
            "SELECT MAX(attempt) FROM platform_submissions "
            "WHERE competition_challenge_id = ? AND answer_slot = ? AND digest = ?",
            (competition_challenge_id, int(answer_slot), digest),
        ).fetchone()
        return (int(row[0]) if row and row[0] is not None else 0) + 1

    def save_lease(self, lease: InstanceLease) -> InstanceLease:
        """保存租约并强制 fencing token 单调（同平台实例不回退）。

        同一 (connection_id, platform_instance_id) 上：新租约行的
        fencing_token 必须严格大于既有最大值；更新已有行时不允许回退。
        """
        # fencing 单调约束用于拒绝旧执行者继续写活动实例状态。把已失效的
        # 旧租约保存为 lost/released 等终态时不再代表远端控制权，应允许
        # 完成恢复收尾。
        if (
            lease.platform_instance_id
            and lease.lease_state() in LEASE_ACTIVE_STATES
        ):
            head = self._conn.execute(
                "SELECT MAX(fencing_token) FROM instance_leases "
                "WHERE connection_id = ? AND platform_instance_id = ? "
                "AND lease_id != ?",
                (lease.connection_id, lease.platform_instance_id,
                 lease.lease_id),
            ).fetchone()
            best = int(head[0]) if head and head[0] is not None else None
            if best is not None:
                current = self.get(InstanceLease, lease.lease_id)
                if current is None and lease.fencing_token <= best:
                    raise StateConflictError(
                        "fencing_token must increase for a new lease on the "
                        f"same platform instance: {lease.fencing_token} <= "
                        f"{best} for "
                        f"{lease.connection_id}/{lease.platform_instance_id}"
                    )
                if current is not None and lease.fencing_token < best:
                    raise StateConflictError(
                        "fencing_token must not regress: "
                        f"{lease.fencing_token} < {best} for "
                        f"{lease.connection_id}/{lease.platform_instance_id}"
                    )
        return self.save(lease)

    # -- tombstone（远端删除；任务书 10.4） -----------------------------------

    def tombstone_challenge(
        self, challenge_id: str, *, at: Optional[datetime] = None
    ) -> CompetitionChallenge:
        """题目 tombstone：标记删除并退役，不物理删除 revision / RunBinding。"""
        challenge = self.get(CompetitionChallenge, challenge_id)
        if challenge is None:
            raise NotFoundError(f"challenge not found: {challenge_id}")
        moment = at or utcnow()
        challenge = challenge.model_copy(update={
            "tombstoned": True,
            "tombstoned_at": moment,
            "remote_state": "hidden",
            "state": ChallengeState.RETIRED.value,
            "paused_from": None,
        })
        return self.save(challenge)

    def tombstone_competition(
        self, competition_id: str, *, at: Optional[datetime] = None
    ) -> Competition:
        competition = self.get(Competition, competition_id)
        if competition is None:
            raise NotFoundError(f"competition not found: {competition_id}")
        return self.save(competition.model_copy(update={"tombstoned": True}))

    # -- 远端提交重启归位（设计 9.4） ------------------------------------------

    def reset_submitting_to_unknown(self) -> list[PlatformSubmission]:
        """服务重启后遗留 ``submitting`` 统一归 ``unknown``（禁止直接重提）。"""
        rows = self._conn.execute(
            "SELECT payload FROM platform_submissions WHERE state = ?",
            (SubmissionState.SUBMITTING.value,),
        ).fetchall()
        changed: list[PlatformSubmission] = []
        for row in rows:
            submission = PlatformSubmission.model_validate_json(row["payload"])
            submission = submission.model_copy(
                update={"state": SubmissionState.UNKNOWN.value}
            )
            changed.append(self.save(submission))
        return changed

    # -- 命令与回执（幂等；设计 10 第 1/5 步） ----------------------------------

    def record_command(
        self, command: CommandEnvelope, receipt: CommandReceipt
    ) -> CommandReceipt:
        """持久化命令与回执（同一事务）。

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
                    "INSERT INTO competition_commands (command_id, "
                    "idempotency_key, command_type, aggregate_type, "
                    "aggregate_id, actor_json, payload_hash, schema_version, "
                    "received_at, payload) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                    (
                        command.command_id,
                        command.idempotency_key,
                        command.command_type,
                        command.aggregate_type,
                        command.aggregate_id,
                        command.actor.model_dump_json(),
                        payload_hash,
                        command.schema_version,
                        now,
                        command.model_dump_json(),
                    ),
                )
                self._conn.execute(
                    "INSERT INTO competition_receipts (command_id, receipt_id, "
                    "state, run_id, event_cursor, error_json, schema_version, "
                    "created_at, updated_at, payload) "
                    "VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                    (
                        command.command_id,
                        receipt.receipt_id,
                        receipt.state.value,
                        receipt.run_id,
                        receipt.event_cursor,
                        receipt.error.model_dump_json() if receipt.error else None,
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

    def _find_command_row(
        self, command_id: str, idempotency_key: Optional[str]
    ) -> Optional[sqlite3.Row]:
        row = self._conn.execute(
            "SELECT * FROM competition_commands WHERE command_id = ?",
            (command_id,),
        ).fetchone()
        if row is None and idempotency_key is not None:
            row = self._conn.execute(
                "SELECT * FROM competition_commands WHERE idempotency_key = ?",
                (idempotency_key,),
            ).fetchone()
        return row

    def check_idempotency(self, command: CommandEnvelope) -> Optional[CommandReceipt]:
        """幂等预检：无记录返回 None；内容不一致抛 ``IdempotencyConflictError``；
        内容一致返回已有 receipt（``deduplicated=True``）。"""
        row = self._find_command_row(command.command_id, command.idempotency_key)
        if row is None:
            return None
        if row["payload_hash"] != _hash_command(command):
            raise IdempotencyConflictError(
                f"command {command.command_id} / idempotency_key "
                f"{command.idempotency_key!r} replayed with different content"
            )
        receipt_row = self._conn.execute(
            "SELECT payload FROM competition_receipts WHERE command_id = ?",
            (row["command_id"],),
        ).fetchone()
        if receipt_row is None:
            return None
        return CommandReceipt.model_validate_json(receipt_row["payload"]).model_copy(
            update={"deduplicated": True}
        )

    def get_command(self, command_id: str) -> Optional[CommandEnvelope]:
        """按 command_id 读取已持久化的命令（不存在返回 None）。

        COMP-08 最小扩展：reconciler 回放未完成 receipt 时需要按回执
        找回原命令信封；只读，不改任何既有写入行为。
        """
        row = self._conn.execute(
            "SELECT payload FROM competition_commands WHERE command_id = ?",
            (command_id,),
        ).fetchone()
        return CommandEnvelope.model_validate_json(row["payload"]) if row else None

    def get_receipt(self, command_id: str) -> Optional[CommandReceipt]:
        row = self._conn.execute(
            "SELECT payload FROM competition_receipts WHERE command_id = ?",
            (command_id,),
        ).fetchone()
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
        """推进回执状态；event_cursor / run_id 非空时一并回写。"""
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
                "UPDATE competition_receipts SET state = ?, run_id = ?, "
                "event_cursor = ?, error_json = ?, updated_at = ?, payload = ? "
                "WHERE command_id = ?",
                (
                    state.value,
                    receipt.run_id,
                    receipt.event_cursor,
                    receipt.error.model_dump_json() if receipt.error else None,
                    utcnow().isoformat(),
                    receipt.model_dump_json(),
                    command_id,
                ),
            )
        return receipt

    def pending_receipts(self) -> list[CommandReceipt]:
        """仍处于 accepted 的回执（重启后由 reconciler 列出回放，任务书 10.8）。"""
        rows = self._conn.execute(
            "SELECT payload FROM competition_receipts "
            "WHERE state IN (?, ?, ?) ORDER BY created_at, rowid",
            (
                ReceiptState.ACCEPTED.value, ReceiptState.RUNNING.value,
                ReceiptState.WAITING.value,
            ),
        ).fetchall()
        return [CommandReceipt.model_validate_json(row["payload"]) for row in rows]

    # -- 领域事件（append-only + expected_version；设计 10 第 3 步） ------------

    def _stream_head(self, aggregate_type: str, aggregate_id: str) -> int:
        row = self._conn.execute(
            "SELECT MAX(stream_seq) FROM competition_events "
            "WHERE aggregate_type = ? AND aggregate_id = ?",
            (aggregate_type, aggregate_id),
        ).fetchone()
        return int(row[0]) if row and row[0] is not None else 0

    def stream_head(self, aggregate_type: str, aggregate_id: str) -> int:
        """一条聚合流当前的最大 stream_seq（空流为 0）。"""
        return self._stream_head(aggregate_type, aggregate_id)

    def append_events(
        self,
        events: list[EventEnvelope] | EventEnvelope,
        *,
        expected_version: Optional[int] = None,
    ) -> list[EventEnvelope]:
        """追加领域事件，返回带最终 stream_seq 的 envelope 列表。

        语义与 ``PlatformStore.append_events`` 一致：expected_version 非空时
        要求目标流 head 恰好相等，否则抛 ``OptimisticConcurrencyError``；
        stream_seq 为 0 时按流内 head + 1 自动编号。事件的 competition_id
        取 payload["competition_id"]（无则空串），写入独立列供按比赛分页。
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
                    "INSERT INTO competition_events (event_id, competition_id, "
                    "aggregate_type, aggregate_id, stream_seq, event_type, "
                    "event_schema_version, occurred_at, producer, actor_id, "
                    "command_id, causation_id, correlation_id, "
                    "idempotency_key, schema_version, created_at, payload) "
                    "VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                    (
                        event.event_id,
                        str(event.payload.get("competition_id") or ""),
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
                        event.model_dump_json(),
                    ),
                )
                heads[key] = next_seq
                stored.append(event)
        return stored

    def read_events(
        self,
        aggregate_type: str,
        aggregate_id: str,
        *,
        after_seq: int = 0,
        limit: int = 100,
    ) -> list[EventEnvelope]:
        """按流读取事件（stream_seq 升序）。"""
        rows = self._conn.execute(
            "SELECT payload FROM competition_events "
            "WHERE aggregate_type = ? AND aggregate_id = ? AND stream_seq > ? "
            "ORDER BY stream_seq LIMIT ?",
            (aggregate_type, aggregate_id, int(after_seq), int(limit)),
        ).fetchall()
        return [EventEnvelope.model_validate_json(row["payload"]) for row in rows]

    def read_all_events(
        self, *, after_seq: int = 0, limit: int = 1000
    ) -> list[tuple[int, EventEnvelope]]:
        """按全局 seq 读取全部事件（投影消费用）。"""
        rows = self._conn.execute(
            "SELECT seq, payload FROM competition_events WHERE seq > ? "
            "ORDER BY seq LIMIT ?",
            (int(after_seq), int(limit)),
        ).fetchall()
        return [
            (int(row["seq"]), EventEnvelope.model_validate_json(row["payload"]))
            for row in rows
        ]

    def read_competition_events(
        self, competition_id: str, *, after_seq: int = 0, limit: int = 500
    ) -> list[tuple[int, EventEnvelope]]:
        """按比赛读取事件页（GET /api/competitions/{id}/events 的存储侧）。"""
        rows = self._conn.execute(
            "SELECT seq, payload FROM competition_events "
            "WHERE competition_id = ? AND seq > ? ORDER BY seq LIMIT ?",
            (competition_id, int(after_seq), int(limit)),
        ).fetchall()
        return [
            (int(row["seq"]), EventEnvelope.model_validate_json(row["payload"]))
            for row in rows
        ]

    def event_watermark(self) -> int:
        """事件日志全局水位（最大 seq；空日志为 0）。SSE 恢复游标基于此值。"""
        row = self._conn.execute(
            "SELECT MAX(seq) FROM competition_events"
        ).fetchone()
        return int(row[0]) if row and row[0] is not None else 0

    def read_competition_history(
        self, competition_id: str, *, before_seq: int, limit: int,
    ) -> list[tuple[int, EventEnvelope]]:
        """按倒序游标读取有限历史页；实时看板不走历史回放。"""
        rows = self._conn.execute(
            "SELECT seq, payload FROM competition_events "
            "WHERE competition_id = ? AND seq < ? ORDER BY seq DESC LIMIT ?",
            (competition_id, before_seq, limit),
        ).fetchall()
        return [
            (int(row["seq"]), EventEnvelope.model_validate_json(row["payload"]))
            for row in rows
        ]

    # -- 快照 ------------------------------------------------------------------

    def snapshot(self, competition_id: str) -> CompetitionSnapshot:
        """比赛聚合读模型：题目、队列、租约、提交、预算与水位。"""
        competition = self.get(Competition, competition_id)
        connection = (
            self.get(PlatformConnection, competition.connection_id)
            if competition is not None else None
        )
        policy = self.get(CompetitionPolicy, competition_id)
        challenges: list[ChallengeSnapshot] = []
        for challenge in self.list(
            CompetitionChallenge, competition_id=competition_id
        ):
            revision = (
                self.get(ChallengeRevision, challenge.current_revision_id)
                if challenge.current_revision_id else None
            )
            challenges.append(ChallengeSnapshot(
                challenge=challenge,
                current_revision=revision,
                active_binding=self.active_binding_for_challenge(
                    challenge.challenge_id),
                active_lease=self.active_lease_for_challenge(
                    challenge.challenge_id),
                queue_entry=self.get(
                    SchedulerQueueEntry, competition_id, challenge.challenge_id),
                candidates=self.list(
                    SubmissionCandidate,
                    competition_challenge_id=challenge.challenge_id),
                submissions=self.list(
                    PlatformSubmission,
                    competition_challenge_id=challenge.challenge_id),
            ))
        watermark_rows = self._conn.execute(
            "SELECT projection_name, last_event_seq "
            "FROM competition_projection_watermarks"
        ).fetchall()
        watermarks = {
            row["projection_name"]: int(row["last_event_seq"])
            for row in watermark_rows
        }
        pending_receipts = self._conn.execute(
            "SELECT COUNT(*) FROM competition_receipts WHERE state IN (?, ?, ?)",
            (
                ReceiptState.ACCEPTED.value, ReceiptState.RUNNING.value,
                ReceiptState.WAITING.value,
            ),
        ).fetchone()[0]
        pending_outbox = self._conn.execute(
            "SELECT COUNT(*) FROM competition_outbox WHERE status IN (?, ?, ?, ?)",
            (
                OutboxStatus.PENDING.value, OutboxStatus.PROCESSING.value,
                OutboxStatus.FAILED.value, OutboxStatus.PAUSED.value,
            ),
        ).fetchone()[0]
        return CompetitionSnapshot(
            generated_at=utcnow(),
            schema_version_db=self.schema_version(),
            connection=connection,
            competition=competition,
            policy=policy,
            challenges=challenges,
            queue=self.list(SchedulerQueueEntry, competition_id=competition_id),
            budgets=self.list(ResourceBudget, competition_id=competition_id),
            event_watermark=self.event_watermark(),
            projection_watermarks=watermarks,
            pending_receipts=int(pending_receipts),
            pending_outbox=int(pending_outbox),
        )


# ---------------------------------------------------------------------------
# competition_outbox（设计 10 第 6 步；外部副作用供 COMP-03+ 消费）
# ---------------------------------------------------------------------------


class CompetitionOutboxManager:
    """competition_outbox 的入队、待投递查询与投递结果回写。

    语义与 ``muteki.platform.outbox.OutboxManager`` 一致，但落在
    competition.db：同一副作用的幂等键唯一，重复入队返回已有记录。
    """

    def __init__(self, store: CompetitionStore) -> None:
        self._store = store

    def enqueue(
        self, record: OutboxRecord, *, idempotency_key: Optional[str] = None
    ) -> tuple[OutboxRecord, bool]:
        """入队一条副作用。返回 (记录, 是否新插入)；幂等键重复时返回已有记录。"""
        key = (idempotency_key or "").strip() or (
            record.command_id or record.event_id or record.outbox_id
        )
        now = utcnow().isoformat()
        with self._store.transaction():
            existing = self._store.conn.execute(
                "SELECT payload FROM competition_outbox WHERE idempotency_key = ?",
                (key,),
            ).fetchone()
            if existing is not None:
                return OutboxRecord.model_validate_json(existing["payload"]), False
            self._store.conn.execute(
                "INSERT INTO competition_outbox (outbox_id, idempotency_key, "
                "command_id, event_id, aggregate_type, aggregate_id, event_type, "
                "destination, status, attempts, next_attempt_at, last_error, "
                "delivered_at, schema_version, created_at, updated_at, payload) "
                "VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                (
                    record.outbox_id,
                    key,
                    record.command_id,
                    record.event_id,
                    record.aggregate_type,
                    record.aggregate_id,
                    record.event_type,
                    record.destination,
                    record.status.value,
                    record.attempts,
                    now,
                    record.last_error,
                    None,
                    record.schema_version,
                    record.created_at.isoformat(),
                    now,
                    record.model_dump_json(),
                ),
            )
        return record, True

    def get(self, outbox_id: str) -> Optional[OutboxRecord]:
        row = self._store.conn.execute(
            "SELECT payload FROM competition_outbox WHERE outbox_id = ?",
            (outbox_id,),
        ).fetchone()
        return OutboxRecord.model_validate_json(row["payload"]) if row else None

    def for_command(self, command_id: str) -> list[OutboxRecord]:
        rows = self._store.conn.execute(
            "SELECT payload FROM competition_outbox WHERE command_id = ? "
            "ORDER BY created_at, rowid",
            (str(command_id),),
        ).fetchall()
        return [OutboxRecord.model_validate_json(row["payload"]) for row in rows]

    def idempotency_key(self, outbox_id: str) -> str:
        row = self._store.conn.execute(
            "SELECT idempotency_key FROM competition_outbox WHERE outbox_id = ?",
            (str(outbox_id),),
        ).fetchone()
        return str(row["idempotency_key"] or "") if row else ""

    def mark_processing(self, outbox_id: str) -> OutboxRecord:
        """原子领取 pending/failed 记录并递增实际投递尝试次数。"""
        with self._store.transaction():
            row = self._store.conn.execute(
                "SELECT payload FROM competition_outbox WHERE outbox_id = ?",
                (outbox_id,),
            ).fetchone()
            if row is None:
                raise NotFoundError(f"outbox record not found: {outbox_id}")
            record = OutboxRecord.model_validate_json(row["payload"])
            if record.status not in {OutboxStatus.PENDING, OutboxStatus.FAILED}:
                return record
            record = record.model_copy(update={
                "status": OutboxStatus.PROCESSING,
                "attempts": record.attempts + 1,
            })
            self._store.conn.execute(
                "UPDATE competition_outbox SET status = ?, attempts = ?, "
                "updated_at = ?, payload = ? WHERE outbox_id = ?",
                (
                    OutboxStatus.PROCESSING.value, record.attempts,
                    utcnow().isoformat(), record.model_dump_json(), outbox_id,
                ),
            )
        return record

    def pending(
        self, *, now: Optional[datetime] = None, limit: int = 100
    ) -> list[OutboxRecord]:
        """待投递记录：pending，或 failed 且已到 next_attempt_at。"""
        moment = (now or utcnow()).isoformat()
        rows = self._store.conn.execute(
            "SELECT payload FROM competition_outbox "
            "WHERE status = ? OR (status = ? AND next_attempt_at <= ?) "
            "ORDER BY created_at, rowid LIMIT ?",
            (OutboxStatus.PENDING.value, OutboxStatus.FAILED.value, moment,
             int(limit)),
        ).fetchall()
        return [OutboxRecord.model_validate_json(row["payload"]) for row in rows]

    def mark_delivered(
        self, outbox_id: str, *, at: Optional[datetime] = None
    ) -> OutboxRecord:
        record = self.get(outbox_id)
        if record is None:
            raise NotFoundError(f"outbox record not found: {outbox_id}")
        record = record.model_copy(update={
            "status": OutboxStatus.DELIVERED,
            "delivered_at": at or utcnow(),
            "last_error": None,
        })
        with self._store.transaction():
            self._store.conn.execute(
                "UPDATE competition_outbox SET status = ?, delivered_at = ?, "
                "last_error = NULL, updated_at = ?, payload = ? "
                "WHERE outbox_id = ?",
                (
                    OutboxStatus.DELIVERED.value,
                    record.delivered_at.isoformat(),
                    utcnow().isoformat(),
                    record.model_dump_json(),
                    outbox_id,
                ),
            )
        return record

    def mark_failed(
        self, outbox_id: str, error: str, *, retry_delay_seconds: float = 60.0,
        increment_attempt: bool = True,
    ) -> OutboxRecord:
        record = self.get(outbox_id)
        if record is None:
            raise NotFoundError(f"outbox record not found: {outbox_id}")
        record = record.model_copy(update={
            "status": OutboxStatus.FAILED,
            "attempts": record.attempts + (1 if increment_attempt else 0),
            "last_error": str(error),
        })
        next_attempt = (
            datetime.now(timezone.utc) + timedelta(seconds=retry_delay_seconds)
        ).isoformat()
        with self._store.transaction():
            self._store.conn.execute(
                "UPDATE competition_outbox SET status = ?, attempts = ?, "
                "next_attempt_at = ?, last_error = ?, updated_at = ?, "
                "payload = ? WHERE outbox_id = ?",
                (
                    OutboxStatus.FAILED.value,
                    record.attempts,
                    next_attempt,
                    record.last_error,
                    utcnow().isoformat(),
                    record.model_dump_json(),
                    outbox_id,
                ),
            )
        return record

    def mark_dead_letter(self, outbox_id: str, error: str) -> OutboxRecord:
        record = self.get(outbox_id)
        if record is None:
            raise NotFoundError(f"outbox record not found: {outbox_id}")
        record = record.model_copy(update={
            "status": OutboxStatus.DEAD_LETTER,
            "last_error": str(error),
        })
        with self._store.transaction():
            self._store.conn.execute(
                "UPDATE competition_outbox SET status = ?, last_error = ?, "
                "updated_at = ?, payload = ? WHERE outbox_id = ?",
                (
                    OutboxStatus.DEAD_LETTER.value, record.last_error,
                    utcnow().isoformat(), record.model_dump_json(), outbox_id,
                ),
            )
        return record

    def mark_paused(self, outbox_id: str, error: str) -> OutboxRecord:
        """暂停自动投递，等待凭据更新或人工重试。"""
        record = self.get(outbox_id)
        if record is None:
            raise NotFoundError(f"outbox record not found: {outbox_id}")
        record = record.model_copy(update={
            "status": OutboxStatus.PAUSED,
            "last_error": str(error),
        })
        with self._store.transaction():
            self._store.conn.execute(
                "UPDATE competition_outbox SET status = ?, last_error = ?, "
                "updated_at = ?, payload = ? WHERE outbox_id = ?",
                (
                    OutboxStatus.PAUSED.value, record.last_error,
                    utcnow().isoformat(), record.model_dump_json(), outbox_id,
                ),
            )
        return record

    def manual_retry(self, outbox_id: str) -> OutboxRecord:
        """把可恢复终态重新放回 pending；已投递或处理中拒绝重试。"""
        record = self.get(outbox_id)
        if record is None:
            raise NotFoundError(f"outbox record not found: {outbox_id}")
        if record.status in {
            OutboxStatus.DELIVERED,
            OutboxStatus.PROCESSING,
            OutboxStatus.CANCELLED,
        }:
            raise StateConflictError(
                f"outbox {outbox_id} in {record.status.value} cannot be retried")
        if record.status is OutboxStatus.PENDING:
            return record
        record = record.model_copy(update={
            "status": OutboxStatus.PENDING,
            "last_error": None,
            "delivered_at": None,
        })
        now = utcnow().isoformat()
        with self._store.transaction():
            self._store.conn.execute(
                "UPDATE competition_outbox SET status = ?, next_attempt_at = ?, "
                "last_error = NULL, delivered_at = NULL, updated_at = ?, "
                "payload = ? WHERE outbox_id = ?",
                (
                    OutboxStatus.PENDING.value, now, now,
                    record.model_dump_json(), outbox_id,
                ),
            )
        return record

    def cancel(self, outbox_id: str) -> OutboxRecord:
        """在远端效果尚未开始时取消 outbox。"""
        record = self.get(outbox_id)
        if record is None:
            raise NotFoundError(f"outbox record not found: {outbox_id}")
        if record.status is OutboxStatus.CANCELLED:
            return record
        if record.status in {OutboxStatus.DELIVERED, OutboxStatus.PROCESSING}:
            raise StateConflictError(
                f"outbox {outbox_id} in {record.status.value} cannot be cancelled")
        record = record.model_copy(update={
            "status": OutboxStatus.CANCELLED,
            "last_error": "cancelled by operator before delivery",
        })
        with self._store.transaction():
            self._store.conn.execute(
                "UPDATE competition_outbox SET status = ?, last_error = ?, "
                "updated_at = ?, payload = ? WHERE outbox_id = ?",
                (
                    OutboxStatus.CANCELLED.value, record.last_error,
                    utcnow().isoformat(), record.model_dump_json(), outbox_id,
                ),
            )
        return record

    def recover_processing(self) -> int:
        """重启时把进程终止留下的 processing 记录恢复为可重试状态。"""
        rows = self._store.conn.execute(
            "SELECT payload FROM competition_outbox WHERE status = ?",
            (OutboxStatus.PROCESSING.value,),
        ).fetchall()
        count = 0
        for row in rows:
            record = OutboxRecord.model_validate_json(row["payload"])
            self.mark_failed(
                record.outbox_id, "consumer interrupted before acknowledgement",
                retry_delay_seconds=0, increment_attempt=False)
            count += 1
        return count


__all__ = [
    "ChallengeSnapshot",
    "CompetitionOutboxManager",
    "CompetitionSnapshot",
    "CompetitionStore",
    "IdempotencyConflictError",
    "NotFoundError",
    "OptimisticConcurrencyError",
    "StateConflictError",
    "StoreError",
    "UniqueConflictError",
    "default_db_path",
]
