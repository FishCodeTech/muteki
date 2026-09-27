"""Muteki 比赛领域（任务书第 10 章，设计 docs/design_muteki_mcp_contest_dispatch.md）。

COMP-01 落地：``CompetitionStore``（competition.db，实体 / 命令回执 /
事件日志 / 投影水位 / outbox）、状态机（``models``）、统一 envelope 领域
事件（``events``）、投影框架（``projections``）、注册进共享
MutekiCommandAPI HandlerRegistry 的比赛命令 Handler（``commands``）。

COMP-05 落地：``sync``（CompetitionSyncService 增量同步 / diff / revision /
tombstone）、``artifacts``（比赛级 sha256 CAS）、``compiler``
（ChallengeCompiler：revision + lease → 核心 Challenge）、``binding``
（RunBindingService：稳定 crun_ run_id、revision 冲突策略表）。

COMP-06 落地：``scheduler``（CompetitionScheduler 确定性调度、预算结算、
可解释 admission_decision、observe/assisted/autonomous 三档）、``advisor``
（CompetitionAdvisor：经 ExternalAgentSessionExecutor 的外部 Agent 顾问，
无平台写权限，停用时 Scheduler 独立完整工作）。

COMP-08 落地：``submission``（SubmissionService：Gate witness 准入、
queued→submitting→终态提交状态机、correct/wrong/rate_limited/auth_failed/
unknown 分支与 wrong 的 Run 持久否决投影）、``reconciler``
（CompetitionReconciler：任务书 10.8 固定十步启动恢复 + 带水位幂等的
run_flag_invalidation 投影器）。

COMP-09 落地：``commands`` 增补能力目录别名命令、
``competition.submission.submit``、``competition.message`` 与只读查询
Handler；``public_events``（CompetitionPublicEventAdapter：内部领域事件
→ 完整 Public Event）；Web API 在 ``apps/web/competition_api.py``。
"""

from muteki.competition import events, models
from muteki.competition.artifacts import (
    ArtifactDownloadError,
    CompetitionArtifactStore,
)
from muteki.competition.binding import (
    BOUND_RUN_ID_PREFIX,
    BindingConflictError,
    RevisionChangePolicy,
    RunBindingService,
    binding_key_for,
    mint_run_id,
)
from muteki.competition.commands import (
    COMPETITION_COMMAND_TYPES,
    CompetitionCommandApi,
    register_competition_handlers,
)
from muteki.competition.compiler import ChallengeCompiler, map_category
from muteki.competition.factory import PlatformAdapterFactory
from muteki.competition.outbox_consumer import CompetitionOutboxConsumer
from muteki.competition.projections import (
    CompetitionProjection,
    CompetitionProjectionManager,
)
from muteki.competition.public_events import CompetitionPublicEventAdapter
from muteki.competition.reconciler import (
    RECOVERY_STEPS,
    RUN_FLAG_INVALIDATION_PROJECTION,
    CompetitionReconciler,
    CompetitionReconcileReport,
    RunFlagInvalidationProjection,
)
from muteki.competition.scheduler import (
    ADMISSION_DECIDED,
    AdmissionDecision,
    CompetitionScheduler,
    ScheduleReport,
    SchedulerCapacity,
)
from muteki.competition.services import CompetitionServiceFactory
from muteki.competition.store import (
    ChallengeSnapshot,
    CompetitionOutboxManager,
    CompetitionSnapshot,
    CompetitionStore,
    IdempotencyConflictError,
    NotFoundError,
    OptimisticConcurrencyError,
    StateConflictError,
    StoreError,
    UniqueConflictError,
    default_db_path,
)
from muteki.competition.submission import (
    CANDIDATE_REJECTED,
    CandidateRejectedError,
    SubmissionService,
    SubmissionServiceConfig,
    open_run_shared_graph,
)
from muteki.competition.sync import (
    SYNC_APPLIED,
    CompetitionSyncService,
    SyncReport,
    normalize_snapshot,
)

__all__ = [
    "ADMISSION_DECIDED",
    "BOUND_RUN_ID_PREFIX",
    "CANDIDATE_REJECTED",
    "COMPETITION_COMMAND_TYPES",
    "AdmissionDecision",
    "ArtifactDownloadError",
    "BindingConflictError",
    "CandidateRejectedError",
    "ChallengeCompiler",
    "ChallengeSnapshot",
    "CompetitionArtifactStore",
    "CompetitionCommandApi",
    "CompetitionOutboxManager",
    "CompetitionOutboxConsumer",
    "CompetitionProjection",
    "CompetitionProjectionManager",
    "CompetitionPublicEventAdapter",
    "CompetitionReconciler",
    "CompetitionReconcileReport",
    "CompetitionScheduler",
    "CompetitionServiceFactory",
    "CompetitionSnapshot",
    "CompetitionStore",
    "CompetitionSyncService",
    "IdempotencyConflictError",
    "NotFoundError",
    "OptimisticConcurrencyError",
    "PlatformAdapterFactory",
    "RECOVERY_STEPS",
    "RUN_FLAG_INVALIDATION_PROJECTION",
    "RevisionChangePolicy",
    "RunBindingService",
    "RunFlagInvalidationProjection",
    "SYNC_APPLIED",
    "ScheduleReport",
    "SchedulerCapacity",
    "StateConflictError",
    "StoreError",
    "SubmissionService",
    "SubmissionServiceConfig",
    "SyncReport",
    "UniqueConflictError",
    "binding_key_for",
    "default_db_path",
    "events",
    "map_category",
    "mint_run_id",
    "models",
    "normalize_snapshot",
    "open_run_shared_graph",
    "register_competition_handlers",
]
