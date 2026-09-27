"""比赛领域实体与状态机（任务书 10.2，设计文档第 7、9 章，COMP-01）。

实体模型统一继承 ``ContractModel``（extra="forbid"，schema_version 字段），
风格与 ``muteki.platform.contracts`` 一致。比赛状态只落在 competition.db，
不写入任何 Run 的 SharedGraph；平台凭据只保存 ``secret://`` 引用
（真实 secret 存储属 COMP-02）。

状态机与设计文档第 9 章逐字对齐；InstanceLease 同时覆盖设计 9.2
（none→acquiring→active→renewing→active，分支 released/lost/error）与
任务书 10.6（requested→provisioning→active→renewing→releasing→released
+ failed/expired）两套命名，取并集，对应关系：

- ``none``（设计）≈ 租约行尚未创建；``requested`` 是已落库的初始状态。
- ``acquiring``（设计）= ``provisioning``（任务书）。
- ``error``（设计）= ``failed``（任务书）。
- ``lost``（设计，平台侧实例丢失）与 ``expired``（任务书，TTL 到期）并列保留。
- ``releasing``（任务书）是设计 9.2 中 ``active → released`` 的显式中间态。
"""

from __future__ import annotations

import hashlib
import json
from datetime import datetime
from enum import Enum
from typing import Any, Optional

from pydantic import Field

from muteki.platform.contracts.base import ContractModel, new_id, utcnow


class IllegalTransitionError(RuntimeError):
    """状态机不允许的转移；携带 from/to 供上层生成准确错误。"""

    def __init__(self, machine: str, current: str, target: str) -> None:
        super().__init__(
            f"{machine}: illegal transition {current!r} -> {target!r}"
        )
        self.machine = machine
        self.current = current
        self.target = target


# ---------------------------------------------------------------------------
# 题目状态机（设计 9.1）
# ---------------------------------------------------------------------------


class ChallengeState(str, Enum):
    DISCOVERED = "discovered"
    SELECTED = "selected"
    QUEUED = "queued"
    PROVISIONING = "provisioning"
    DISPATCHING = "dispatching"
    RUNNING = "running"
    CANDIDATE_FOUND = "candidate_found"
    SUBMITTING = "submitting"
    SOLVED = "solved"
    PAUSED = "paused"
    SKIPPED = "skipped"
    EXHAUSTED = "exhausted"
    FAILED = "failed"
    RETIRED = "retired"


#: 设计 9.1 转移表。``paused`` 的恢复目标不固定在表里：进入 paused 时把
#: 原状态记入 ``paused_from``，恢复时只允许回到该状态（见
#: ``ensure_challenge_transition``）。``retired`` 允许从任意非 solved 状态
#: 进入（远端删除 tombstone / Operator 退役路径）；``solved`` 是终态。
_CHALLENGE_TABLE: dict[ChallengeState, set[ChallengeState]] = {
    ChallengeState.DISCOVERED: {
        ChallengeState.SELECTED, ChallengeState.SKIPPED, ChallengeState.PAUSED,
        ChallengeState.RETIRED,
    },
    ChallengeState.SELECTED: {
        ChallengeState.QUEUED, ChallengeState.SKIPPED, ChallengeState.PAUSED,
        ChallengeState.RETIRED,
    },
    ChallengeState.QUEUED: {
        ChallengeState.PROVISIONING, ChallengeState.PAUSED,
        ChallengeState.SKIPPED, ChallengeState.RETIRED,
    },
    ChallengeState.PROVISIONING: {
        ChallengeState.DISPATCHING, ChallengeState.FAILED,
        ChallengeState.PAUSED, ChallengeState.RETIRED,
    },
    ChallengeState.DISPATCHING: {
        ChallengeState.RUNNING, ChallengeState.FAILED, ChallengeState.PAUSED,
        ChallengeState.RETIRED,
    },
    ChallengeState.RUNNING: {
        ChallengeState.CANDIDATE_FOUND, ChallengeState.PAUSED,
        ChallengeState.EXHAUSTED, ChallengeState.FAILED, ChallengeState.RETIRED,
        # 多 Flag 部分判对会退回 running；凑满 expected 后允许直接收口。
        ChallengeState.SOLVED,
    },
    ChallengeState.CANDIDATE_FOUND: {
        # submitting（批准并生成提交 outbox）；远端判错 / 候选被否后随
        # RunBinding resolve 回到 running（设计 9.1 注释）。
        ChallengeState.SUBMITTING, ChallengeState.RUNNING,
        ChallengeState.PAUSED, ChallengeState.RETIRED,
        ChallengeState.SOLVED,
    },
    ChallengeState.SUBMITTING: {
        # submitting→submitting：rate_limited 后等待 retry_after 的自循环。
        ChallengeState.SOLVED, ChallengeState.RUNNING,
        ChallengeState.SUBMITTING, ChallengeState.PAUSED,
        ChallengeState.RETIRED,
    },
    ChallengeState.PAUSED: {
        ChallengeState.SKIPPED, ChallengeState.RETIRED,
        # resume 目标由 paused_from 动态校验，见 ensure_challenge_transition。
    },
    ChallengeState.FAILED: {ChallengeState.RETIRED},
    ChallengeState.SKIPPED: {ChallengeState.RETIRED},
    ChallengeState.EXHAUSTED: {ChallengeState.RETIRED},
    ChallengeState.SOLVED: set(),  # 终态
}

#: paused 允许恢复到的状态（必须与 paused_from 一致）。
_CHALLENGE_RESUMABLE: set[ChallengeState] = {
    ChallengeState.SELECTED, ChallengeState.QUEUED, ChallengeState.PROVISIONING,
    ChallengeState.DISPATCHING, ChallengeState.RUNNING,
    ChallengeState.CANDIDATE_FOUND, ChallengeState.SUBMITTING,
}


def ensure_challenge_transition(
    current: ChallengeState,
    target: ChallengeState,
    *,
    paused_from: Optional[ChallengeState] = None,
) -> None:
    """校验题目状态转移，非法时抛 ``IllegalTransitionError``。

    进入 ``paused`` 时调用方负责把当前状态写入 ``paused_from``；从
    ``paused`` 恢复时 target 必须等于 ``paused_from``。
    """
    if current is ChallengeState.PAUSED and target in _CHALLENGE_RESUMABLE:
        if paused_from is not None and target is paused_from:
            return
        raise IllegalTransitionError(
            "challenge", current.value,
            f"{target.value}(paused_from={paused_from.value if paused_from else None})",
        )
    if target in _CHALLENGE_TABLE.get(current, set()):
        return
    raise IllegalTransitionError("challenge", current.value, target.value)


# ---------------------------------------------------------------------------
# 实例租约状态机（设计 9.2 ∪ 任务书 10.6，对应关系见模块 docstring）
# ---------------------------------------------------------------------------


class LeaseState(str, Enum):
    REQUESTED = "requested"        # 设计 9.2 的 none→acquiring 之间的已落库初始态
    PROVISIONING = "provisioning"  # = 设计 9.2 的 acquiring
    ACTIVE = "active"
    RENEWING = "renewing"
    RELEASING = "releasing"
    RELEASED = "released"
    FAILED = "failed"              # = 设计 9.2 的 error
    EXPIRED = "expired"
    LOST = "lost"


#: 同一道题视为“活动租约”的状态集合（部分唯一索引与调度判定共用）。
LEASE_ACTIVE_STATES: frozenset[LeaseState] = frozenset({
    LeaseState.REQUESTED, LeaseState.PROVISIONING, LeaseState.ACTIVE,
    LeaseState.RENEWING, LeaseState.RELEASING,
})

_LEASE_TABLE: dict[LeaseState, set[LeaseState]] = {
    # requested → released：平台尚未交付实例前取消，无需外部释放调用。
    LeaseState.REQUESTED: {
        LeaseState.PROVISIONING, LeaseState.FAILED, LeaseState.RELEASED,
    },
    LeaseState.PROVISIONING: {
        LeaseState.ACTIVE, LeaseState.FAILED, LeaseState.EXPIRED,
    },
    LeaseState.ACTIVE: {
        LeaseState.RENEWING, LeaseState.RELEASING, LeaseState.RELEASED,
        LeaseState.LOST, LeaseState.EXPIRED, LeaseState.FAILED,
    },
    LeaseState.RENEWING: {
        LeaseState.ACTIVE, LeaseState.FAILED, LeaseState.LOST,
        LeaseState.EXPIRED,
    },
    LeaseState.RELEASING: {LeaseState.RELEASED, LeaseState.FAILED},
    # released / failed / expired / lost 为终态；重启后由 reconciler 探测
    # 活动租约（设计 9.5），不在这里隐式复活。
    LeaseState.RELEASED: set(),
    LeaseState.FAILED: set(),
    LeaseState.EXPIRED: set(),
    LeaseState.LOST: set(),
}


def ensure_lease_transition(current: LeaseState, target: LeaseState) -> None:
    if target not in _LEASE_TABLE.get(current, set()):
        raise IllegalTransitionError("lease", current.value, target.value)


# ---------------------------------------------------------------------------
# RunBinding 状态机（设计 9.3）
# ---------------------------------------------------------------------------


class BindingState(str, Enum):
    PLANNED = "planned"
    CREATING = "creating"
    STARTING = "starting"
    ACTIVE = "active"
    LOCAL_FINISHED = "local_finished"
    REMOTE_PENDING = "remote_pending"
    REJECTED = "rejected"
    RESOLVING = "resolving"
    SOLVED = "solved"
    PAUSED = "paused"
    FAILED = "failed"
    STOPPED = "stopped"


#: “活动 binding”状态集合：同一道题最多一个活动 binding（部分唯一索引）。
BINDING_ACTIVE_STATES: frozenset[BindingState] = frozenset({
    BindingState.PLANNED, BindingState.CREATING, BindingState.STARTING,
    BindingState.ACTIVE, BindingState.LOCAL_FINISHED,
    BindingState.REMOTE_PENDING, BindingState.REJECTED,
    BindingState.RESOLVING, BindingState.PAUSED,
})

_BINDING_TABLE: dict[BindingState, set[BindingState]] = {
    BindingState.PLANNED: {
        BindingState.CREATING, BindingState.FAILED, BindingState.STOPPED,
    },
    BindingState.CREATING: {
        BindingState.STARTING, BindingState.FAILED, BindingState.STOPPED,
    },
    BindingState.STARTING: {
        BindingState.ACTIVE, BindingState.FAILED, BindingState.STOPPED,
    },
    BindingState.ACTIVE: {
        BindingState.LOCAL_FINISHED, BindingState.RESOLVING,
        BindingState.PAUSED, BindingState.FAILED, BindingState.STOPPED,
    },
    BindingState.LOCAL_FINISHED: {
        BindingState.REMOTE_PENDING, BindingState.FAILED, BindingState.STOPPED,
    },
    BindingState.REMOTE_PENDING: {
        BindingState.SOLVED, BindingState.REJECTED, BindingState.FAILED,
    },
    BindingState.REJECTED: {BindingState.RESOLVING, BindingState.STOPPED},
    # resolving → active：实例硬变化或 Operator resolve 的新执行代（设计 9.3
    # 规则 7：每次经过 resolving 启动新执行时递增 execution_generation）。
    BindingState.RESOLVING: {
        BindingState.ACTIVE, BindingState.FAILED, BindingState.STOPPED,
    },
    BindingState.PAUSED: {
        BindingState.ACTIVE, BindingState.RESOLVING, BindingState.STOPPED,
        BindingState.FAILED,
    },
    BindingState.SOLVED: set(),    # 终态
    BindingState.FAILED: set(),    # 终态
    BindingState.STOPPED: set(),   # 终态
}


def ensure_binding_transition(current: BindingState, target: BindingState) -> None:
    if target not in _BINDING_TABLE.get(current, set()):
        raise IllegalTransitionError("run_binding", current.value, target.value)


# ---------------------------------------------------------------------------
# 远端提交状态机（设计 9.4）
# ---------------------------------------------------------------------------


class SubmissionState(str, Enum):
    QUEUED = "queued"
    SUBMITTING = "submitting"
    CORRECT = "correct"
    WRONG = "wrong"
    DUPLICATE_OR_SOLVED = "duplicate_or_solved"
    RATE_LIMITED = "rate_limited"
    TRANSIENT_FAILURE = "transient_failure"
    AUTH_REQUIRED = "auth_required"
    UNKNOWN = "unknown"
    CANCELLED = "cancelled"


_SUBMISSION_TABLE: dict[SubmissionState, set[SubmissionState]] = {
    SubmissionState.QUEUED: {SubmissionState.SUBMITTING, SubmissionState.CANCELLED},
    SubmissionState.SUBMITTING: {
        SubmissionState.CORRECT, SubmissionState.WRONG,
        SubmissionState.DUPLICATE_OR_SOLVED, SubmissionState.RATE_LIMITED,
        SubmissionState.TRANSIENT_FAILURE, SubmissionState.AUTH_REQUIRED,
        SubmissionState.UNKNOWN,
    },
    # rate_limited 到 retry_after 后、transient_failure 有界重试、auth_required
    # 连接重新授权后，统一回到 queued（设计 9.4）。
    SubmissionState.RATE_LIMITED: {SubmissionState.QUEUED, SubmissionState.CANCELLED},
    SubmissionState.TRANSIENT_FAILURE: {
        SubmissionState.QUEUED, SubmissionState.CANCELLED,
    },
    SubmissionState.AUTH_REQUIRED: {
        SubmissionState.QUEUED, SubmissionState.CANCELLED,
    },
    # unknown 禁止直接重复提交：由 reconciler 核对远端状态后落终态，或由
    # Operator 取消（设计 9.4 / 任务书 10.7）。唯一例外是宿主已有证据证明
    # 请求在平台边界之前失败，此时先归 transient_failure 再有界重试。
    SubmissionState.UNKNOWN: {
        SubmissionState.CORRECT, SubmissionState.WRONG,
        SubmissionState.DUPLICATE_OR_SOLVED, SubmissionState.TRANSIENT_FAILURE,
        SubmissionState.CANCELLED,
    },
    SubmissionState.CORRECT: set(),
    SubmissionState.WRONG: set(),
    SubmissionState.DUPLICATE_OR_SOLVED: set(),
    SubmissionState.CANCELLED: set(),
}


def ensure_submission_transition(
    current: SubmissionState, target: SubmissionState
) -> None:
    if target not in _SUBMISSION_TABLE.get(current, set()):
        raise IllegalTransitionError("platform_submission", current.value, target.value)


# ---------------------------------------------------------------------------
# 提交候选状态（任务书 10.7：observe 不自动提交；assisted 等待 Operator 确认）
# ---------------------------------------------------------------------------


class CandidateState(str, Enum):
    CANDIDATE = "candidate"
    AWAITING_APPROVAL = "awaiting_approval"
    APPROVED = "approved"
    SUBMITTED = "submitted"
    REJECTED = "rejected"
    SUPERSEDED = "superseded"
    CANCELLED = "cancelled"


_CANDIDATE_TABLE: dict[CandidateState, set[CandidateState]] = {
    CandidateState.CANDIDATE: {
        CandidateState.AWAITING_APPROVAL, CandidateState.APPROVED,
        CandidateState.REJECTED, CandidateState.SUPERSEDED,
        CandidateState.CANCELLED,
    },
    CandidateState.AWAITING_APPROVAL: {
        CandidateState.APPROVED, CandidateState.REJECTED,
        CandidateState.CANCELLED,
    },
    CandidateState.APPROVED: {CandidateState.SUBMITTED, CandidateState.CANCELLED},
    CandidateState.REJECTED: set(),
    CandidateState.SUPERSEDED: set(),
    CandidateState.CANCELLED: set(),
    CandidateState.SUBMITTED: set(),
}


def ensure_candidate_transition(
    current: CandidateState, target: CandidateState
) -> None:
    if target not in _CANDIDATE_TABLE.get(current, set()):
        raise IllegalTransitionError("submission_candidate", current.value, target.value)


# ---------------------------------------------------------------------------
# 实体模型（任务书 10.2 / 设计 7.1）
# ---------------------------------------------------------------------------


class PlatformKind(str, Enum):
    MOCK = "mock"
    CTFD = "ctfd"
    RCTF = "rctf"
    GZCTF = "gzctf"
    GENERIC_BROWSER = "generic_browser"


class ConnectionStatus(str, Enum):
    ACTIVE = "active"
    DISABLED = "disabled"
    AUTH_REQUIRED = "auth_required"  # 平台侧认证失效：暂停外部动作（任务书 10.7）


class PlatformConnection(ContractModel):
    """平台连接。凭据只保存 ``secret://`` 引用，不落真实 secret。"""

    connection_id: str = Field(default_factory=lambda: new_id("pconn"))
    platform_kind: str = ""           # PlatformKind.value
    canonical_base_url: str = ""      # 规范化后的 base url（小写 host、去尾斜杠）
    account_key: str = ""             # 账户标识（用户名 / token 指纹等）
    credential_ref: str = ""          # secret:// 引用（COMP-02 的真实存储键）
    capabilities: dict[str, Any] = Field(default_factory=dict)
    status: str = ConnectionStatus.ACTIVE.value
    last_error: str = ""
    archived: bool = False            # 本地从清单移除，历史与事件保留
    created_at: datetime = Field(default_factory=utcnow)
    updated_at: datetime = Field(default_factory=utcnow)


class SchedulerState(str, Enum):
    STOPPED = "stopped"
    RUNNING = "running"
    PAUSED = "paused"


class Competition(ContractModel):
    """一场比赛在某个平台连接下的本地投影。"""

    competition_id: str = Field(default_factory=lambda: new_id("comp"))
    connection_id: str = ""
    external_competition_id: str = ""
    title: str = ""
    description: str = ""
    starts_at: Optional[datetime] = None
    ends_at: Optional[datetime] = None
    # Adapter 提供的稳定运行状态，例如远端批次与插件自管 VPN 的状态。
    # 这里只允许安全元数据；token、cookie 与 VPN 配置始终留在 SecretStore。
    platform_status: dict[str, Any] = Field(default_factory=dict)
    scheduler_state: str = SchedulerState.STOPPED.value
    tombstoned: bool = False          # 远端删除：tombstone，不物理删除
    archived: bool = False            # 本地从清单移除，历史与事件保留
    created_at: datetime = Field(default_factory=utcnow)
    updated_at: datetime = Field(default_factory=utcnow)


class AutomationMode(str, Enum):
    OBSERVE = "observe"          # 只观察，不自动创建提交动作
    ASSISTED = "assisted"        # 候选等待 Operator 确认
    AUTONOMOUS = "autonomous"    # 来源/预算/冷却/策略满足后自动提交


class CompetitionPolicy(ContractModel):
    """每场一份当前策略（任务书 10.5 / 设计 15）。

    ``policy_profile`` 选用通用策略档（如 ``tsec_eval``），不绑定 platform_kind。
    轮次时间盒 / working set / visit floor / keepalive 等字段供调度器消费。
    """

    competition_id: str = ""
    automation_mode: str = AutomationMode.ASSISTED.value
    max_concurrent_runs: int = 2
    max_instances: int = 1
    submission_cooldown_seconds: float = 30.0
    category_allow: list[str] = Field(default_factory=list)
    category_deny: list[str] = Field(default_factory=list)
    budget_limits: dict[str, float] = Field(default_factory=dict)  # kind → 上限
    # ---- 策略档与测评调度（通用；tsec_eval 等 profile 写入）----
    policy_profile: str = "default"
    round_timeboxes_s: list[int] = Field(default_factory=list)
    visit_floor_s: float = 0.0
    # 当首轮题目已全部派发且仍有空闲槽位时，允许复访题提前补位。
    # 默认关闭；需要该行为的平台由 Adapter policy_hints 显式声明。
    fill_idle_revisits: bool = False
    # 最终阶段题在普通题全部至少运行一轮后，可按最低排序填补空槽。
    terminal_phase_fill_idle: bool = False
    overdue_mult: float = 1.25
    working_set: int = 0              # 0 = 不额外限制（仅用 max_concurrent_runs）
    keepalive_max: int = 0
    keepalive_tail_s: float = 0.0
    total_budget_s: float = 0.0
    dry_defer_waves: int = 0
    deepchain_slots: int = 0
    per_challenge_seconds: float = 0.0
    stuck_waves_cap: int = 0
    # 平台可声明确定的题目准入顺序；未列出的题目排在列表之后。
    challenge_order: list[str] = Field(default_factory=list)
    # 平台可声明一组最后阶段题目。默认需其余题目都进入终态；平台也可启用
    # terminal_phase_fill_idle，在普通题完成首轮后用最终阶段题填补空槽。
    # persistent_challenge_ids 中的题目不使用 visit 时间盒或租约 TTL。
    # 两组值均使用平台 external_challenge_id，平台特例由 Adapter 提供。
    terminal_phase_challenge_ids: list[str] = Field(default_factory=list)
    persistent_challenge_ids: list[str] = Field(default_factory=list)
    updated_at: datetime = Field(default_factory=utcnow)


class CompetitionChallenge(ContractModel):
    """比赛题目：远端身份 + 当前 revision + 调度状态机。"""

    challenge_id: str = Field(default_factory=lambda: new_id("cch"))
    competition_id: str = ""
    external_challenge_id: str = ""
    name: str = ""
    category: str = ""
    current_revision_id: Optional[str] = None
    remote_state: str = "open"        # open / closed / solved_remote / hidden
    state: str = ChallengeState.DISCOVERED.value
    # 进入 paused 前的状态；resume 只允许回到这里（见状态机注释）。
    paused_from: Optional[str] = None
    tombstoned: bool = False          # 远端删除：tombstone（任务书 10.4）
    tombstoned_at: Optional[datetime] = None
    created_at: datetime = Field(default_factory=utcnow)
    updated_at: datetime = Field(default_factory=utcnow)

    def challenge_state(self) -> ChallengeState:
        return ChallengeState(self.state)


class RevisionArtifact(ContractModel):
    """revision 附件清单条目（内容寻址，不含签名下载地址 / Cookie）。"""

    sha256: str = ""
    name: str = ""
    size: int = 0
    media_type: str = ""


def revision_content_hash(
    *,
    name: str,
    category: str,
    points: float,
    description: str,
    target: str,
    flag_format: str,
    hints: list[str],
    prerequisites: list[str],
    multi_flag: bool,
    expected_flags: int,
    artifacts: list[RevisionArtifact],
) -> str:
    """ChallengeRevision 内容寻址 hash（设计 7.2）。

    包含名称、类别、分值、描述、稳定目标描述、Flag 格式、前置关系、提示
    摘要、多答案结构与附件（sha256/文件名/大小/MIME）；不包含短期签名
    下载地址、Cookie、访问令牌和实例续租时间。
    """
    raw = json.dumps(
        {
            "name": name,
            "category": category,
            "points": points,
            "description": description,
            "target": target,
            "flag_format": flag_format,
            "hints": sorted(str(h) for h in hints),
            "prerequisites": sorted(str(p) for p in prerequisites),
            "multi_flag": bool(multi_flag),
            "expected_flags": int(expected_flags),
            "artifacts": [
                {
                    "sha256": a.sha256,
                    "name": a.name,
                    "size": a.size,
                    "media_type": a.media_type,
                }
                for a in sorted(artifacts, key=lambda x: (x.sha256, x.name))
            ],
        },
        ensure_ascii=False,
        sort_keys=True,
        separators=(",", ":"),
    )
    return hashlib.sha256(raw.encode("utf-8")).hexdigest()


class ChallengeRevision(ContractModel):
    """不可变题目内容 revision，同时承担派发快照（设计 7.2）。"""

    revision_id: str = Field(default_factory=lambda: new_id("rev"))
    competition_challenge_id: str = ""
    content_hash: str = ""
    revision_seq: int = 1             # 该题第几个 revision（单调递增）
    name: str = ""
    category: str = ""
    points: float = 0.0
    description: str = ""
    target: str = ""                  # 稳定目标描述（不含动态实例地址）
    flag_format: str = ""
    hints: list[str] = Field(default_factory=list)
    prerequisites: list[str] = Field(default_factory=list)
    multi_flag: bool = False
    expected_flags: int = 1
    artifacts: list[RevisionArtifact] = Field(default_factory=list)
    created_at: datetime = Field(default_factory=utcnow)


class ArtifactObject(ContractModel):
    """比赛级 CAS 对象：按 sha256 去重，重复附件不重复下载（任务书 10.4）。"""

    sha256: str = ""
    size: int = 0
    media_type: str = ""
    origin: str = ""                  # 来源（connection_id / url 摘要）
    local_path: str = ""              # 本地对象路径
    created_at: datetime = Field(default_factory=utcnow)


class ChallengeArtifact(ContractModel):
    """revision ↔ CAS 对象关联。"""

    revision_id: str = ""
    sha256: str = ""
    name: str = ""
    position: int = 0


class SyncCursor(ContractModel):
    """同步游标 / etag / 平台增量标识（任务书 10.4）。"""

    competition_id: str = ""
    kind: str = ""                    # challenges / scoreboard / submissions …
    cursor: str = ""
    etag: str = ""
    updated_at: datetime = Field(default_factory=utcnow)


class RunBinding(ContractModel):
    """题目 ↔ Run ↔ 执行代 ↔ 实例租约的绑定（设计 7.1 / 9.3）。

    身份为 (run_id, execution_generation)；同一道题最多一个活动 binding
    （部分唯一索引，活动状态见 ``BINDING_ACTIVE_STATES``）。
    """

    binding_id: str = Field(default_factory=lambda: new_id("bind"))
    competition_id: str = ""
    competition_challenge_id: str = ""
    revision_id: str = ""
    run_id: str = ""
    execution_generation: int = 1
    lease_id: Optional[str] = None
    state: str = BindingState.PLANNED.value
    created_at: datetime = Field(default_factory=utcnow)
    updated_at: datetime = Field(default_factory=utcnow)

    def binding_state(self) -> BindingState:
        return BindingState(self.state)


class InstanceLease(ContractModel):
    """动态实例租约：owner、平台 generation、TTL、renew deadline、fencing token。"""

    lease_id: str = Field(default_factory=lambda: new_id("lease"))
    connection_id: str = ""
    competition_id: str = ""
    competition_challenge_id: str = ""
    platform_instance_id: str = ""    # 平台实例 id
    generation: int = 1               # 平台侧 generation
    fencing_token: int = 0            # 同 (connection_id, platform_instance_id) 单调递增
    owner: str = ""                   # 持有方（run_id / binding_id）
    address: str = ""                 # 实例地址（host:port / url）
    credential_ref: str = ""          # 实例凭据的 secret:// 引用
    ttl_seconds: int = 0
    renew_deadline_at: Optional[datetime] = None
    expires_at: Optional[datetime] = None
    state: str = LeaseState.REQUESTED.value
    last_error: str = ""
    created_at: datetime = Field(default_factory=utcnow)
    updated_at: datetime = Field(default_factory=utcnow)

    def lease_state(self) -> LeaseState:
        return LeaseState(self.state)


class QueueEntryState(str, Enum):
    QUEUED = "queued"
    DISPATCHING = "dispatching"
    HELD = "held"          # Operator / 预算 / 冷却挂起
    DONE = "done"
    DROPPED = "dropped"


class SchedulerQueueEntry(ContractModel):
    """Scheduler 队列条目：一题一条，含最近一次可解释 admission_decision。"""

    competition_id: str = ""
    competition_challenge_id: str = ""
    state: str = QueueEntryState.QUEUED.value
    priority: float = 0.0             # Operator 固定优先级
    score: float = 0.0
    not_before: Optional[datetime] = None
    admission_decision: dict[str, Any] = Field(default_factory=dict)
    enqueued_at: datetime = Field(default_factory=utcnow)
    updated_at: datetime = Field(default_factory=utcnow)


class ResourceBudget(ContractModel):
    """预算桶：token / 费用 / 墙钟 / 平台提交 / 动态实例配额（任务书 10.5）。"""

    competition_id: str = ""
    kind: str = ""                    # tokens / cost / wallclock / submissions / instances
    limit: float = 0.0
    used: float = 0.0
    window: str = "run"               # run / day / competition
    resets_at: Optional[datetime] = None
    updated_at: datetime = Field(default_factory=utcnow)


def flag_digest(value: str) -> str:
    """候选 Flag 值的稳定摘要（唯一键与判重用，不明文做索引）。"""
    return hashlib.sha256(value.encode("utf-8")).hexdigest()


class SubmissionCandidate(ContractModel):
    """经现有 Gate 确认、带真实来源的候选（任务书 10.7）。

    唯一键 (competition_challenge_id, answer_slot, flag_digest)（设计 7.1）。
    """

    candidate_id: str = Field(default_factory=lambda: new_id("cand"))
    competition_id: str = ""
    competition_challenge_id: str = ""
    answer_slot: int = 1
    value: str = ""                   # 候选原文（提交时使用；不落日志）
    digest: str = ""                  # flag_digest(value)
    source_run_id: str = ""           # 来源 Run
    source_ref: str = ""              # 来源事件 / 产物引用（gate witness）
    gate_verdict: str = ""            # 现有 Gate 的判定摘要
    source_execution_generation: int = 0
    source_worker_id: str = ""
    source_session_id: str = ""
    shared_graph_fact_id: str = ""
    witness_digest: str = ""
    witness_artifact_path: str = ""
    state: str = CandidateState.CANDIDATE.value
    created_at: datetime = Field(default_factory=utcnow)
    updated_at: datetime = Field(default_factory=utcnow)

    def candidate_state(self) -> CandidateState:
        return CandidateState(self.state)


class PlatformSubmission(ContractModel):
    """一次远端提交 attempt（设计 7.1 / 9.4）。

    幂等键 (competition_id, competition_challenge_id, answer_slot,
    flag_digest, attempt)：同一平台、同一题、同一候选值的第 N 次尝试唯一。
    ``submitting`` 在服务重启后统一归 ``unknown``（设计 9.4）。
    """

    submission_id: str = Field(default_factory=lambda: new_id("psub"))
    competition_id: str = ""
    competition_challenge_id: str = ""
    candidate_id: str = ""
    answer_slot: int = 1
    digest: str = ""
    attempt: int = 1
    state: str = SubmissionState.QUEUED.value
    retry_after_at: Optional[datetime] = None
    remote_receipt: str = ""          # 平台回执摘要（判对/判错原文）
    last_error: str = ""
    created_at: datetime = Field(default_factory=utcnow)
    updated_at: datetime = Field(default_factory=utcnow)

    def submission_state(self) -> SubmissionState:
        return SubmissionState(self.state)


class ReconcileCheckpoint(ContractModel):
    """Reconciler 启动恢复步骤的检查点（任务书 10.8 / 设计 9.5）。"""

    # COMP-08 最小扩展：COMP-01 的 store 表规格以 checkpoint_id 为主键，
    # 模型漏了该字段导致 save() 直接 AttributeError；这里补齐（只加字段，
    # 不改任何既有行为）。
    checkpoint_id: str = Field(default_factory=lambda: new_id("rcp"))
    competition_id: str = ""          # 空串表示全局步骤
    step: str = ""                    # 恢复顺序中的步骤名
    status: str = "done"              # done / failed
    details: dict[str, Any] = Field(default_factory=dict)
    event_seq: int = 0                # 执行时的事件水位
    created_at: datetime = Field(default_factory=utcnow)


__all__ = [
    "ArtifactObject",
    "AutomationMode",
    "BINDING_ACTIVE_STATES",
    "BindingState",
    "CandidateState",
    "ChallengeArtifact",
    "ChallengeRevision",
    "ChallengeState",
    "Competition",
    "CompetitionChallenge",
    "CompetitionPolicy",
    "ConnectionStatus",
    "IllegalTransitionError",
    "InstanceLease",
    "LEASE_ACTIVE_STATES",
    "LeaseState",
    "PlatformConnection",
    "PlatformKind",
    "PlatformSubmission",
    "QueueEntryState",
    "ReconcileCheckpoint",
    "ResourceBudget",
    "RevisionArtifact",
    "RunBinding",
    "SchedulerQueueEntry",
    "SchedulerState",
    "SubmissionCandidate",
    "SubmissionState",
    "SyncCursor",
    "ensure_binding_transition",
    "ensure_candidate_transition",
    "ensure_challenge_transition",
    "ensure_lease_transition",
    "ensure_submission_transition",
    "flag_digest",
    "revision_content_hash",
]
