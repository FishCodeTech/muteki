"""CompetitionScheduler：确定性全局调度、预算权威与可解释准入（任务书 10.5
/ 设计 11、15，COMP-06）。

调度层级（设计 11.1）：

    CompetitionScheduler：决定哪些 Run 可以启动、暂停、恢复
    Run Coordinator：决定一个 Run 内创建哪些 Worker 和 Intent
    CostController：计量并执行每 Run 内部预算终止

本模块只实现第一层：Scheduler 不直接创建 / 终止 Worker，Run 内调度归
Coordinator；准入后调用 ``RunBindingService.ensure_bound_run`` + ``start``
创建并启动子 Run（下发载荷绝不携带 ``swarm_class``，由 COMP-05 锁定）。

确定性规则（设计 11.4 优先顺序）：

1. Operator 明确优先级和指令（queue priority、skip、scheduler.pause）。
2. 已解锁、即将到期或阻塞其他题目的任务。
3. 分值、提示和前置关系。
4. 当前健康 Worker Profile 的类别能力。
5. 排队时间和类别覆盖。

每条规则按固定顺序评估并把结果写进 ``AdmissionDecision.rules``；同样输入
永远得到同样决定。默认排序键全序：operator priority → score →
enqueued_at → challenge_id。``tsec_eval`` 按 hxbai 做题顺序：
operator priority → 饥饿/过期/近期/空转 → 难度（easy 先）→ 临门 →
分值 → 综合分 → 排队时间 → challenge_id。无随机性。

预算权威（设计 11.3）：Scheduler 是 Run 准入的权威；全局消耗从每 Run 已
记录计量汇总（注入 ``usage_provider``，例如汇总各绑定 Run 的
CostController ledger），写回 ``resource_budgets.used`` 作为投影缓存 —
本模块不维护第二套独立计量计数器。

自动化三档（设计 15，逐字语义）：

- ``observe``：只同步；tick 只评估并记录 decision（action=observe），
  不执行调度和派发；
- ``assisted``：自动调度和派发，远端提交等待 Operator 确认（确认环属
  COMP-08 SubmissionService / platform_submission.approve 命令）；
- ``autonomous``：全自动。
- Operator 指令在三档中均具有最高优先级；从 ``autonomous`` 降级产生
  策略变更事件；预算耗尽 / 认证失效 / 平台策略变化时暂停相应操作并
  记录原因。

持久化：动作或原因变化时立即更新
``scheduler_queue.admission_decision``，只有时间性评分变化时按分钟刷新；
append-only 的 ``competition.scheduler.admission_decision`` 只记录动作或原因
变化以及准入，避免容量不变时每半秒重复写入相同审计事件。
日志（competition.db），含输入快照、规则结果、预算变化和最终动作。
"""

from __future__ import annotations

from datetime import datetime, timezone
from typing import Any, Callable, Optional

from pydantic import Field

from muteki.competition import events as ev
from muteki.competition.binding import RunBindingService
from muteki.competition.models import (
    LEASE_ACTIVE_STATES,
    AutomationMode,
    BindingState,
    ChallengeRevision,
    ChallengeState,
    Competition,
    CompetitionChallenge,
    CompetitionPolicy,
    ConnectionStatus,
    InstanceLease,
    PlatformConnection,
    QueueEntryState,
    ResourceBudget,
    RunBinding,
    SchedulerQueueEntry,
    SchedulerState,
    SubmissionCandidate,
    PlatformSubmission,
    SubmissionState,
    ensure_challenge_transition,
)
from muteki.competition.policy_profiles import (
    POLICY_PROFILE_TSEC_EVAL,
    difficulty_of,
    difficulty_rank,
    visit_timebox_seconds,
)
from muteki.competition.store import CompetitionStore, NotFoundError
from muteki.platform.contracts.base import ContractModel, new_id, utcnow

# ---------------------------------------------------------------------------
# 事件类型（COMP-06 新增；与 sync.py 的 SYNC_APPLIED 同样局部定义，不改
# COMP-01 的 events 模块）
# ---------------------------------------------------------------------------

#: 每次调度的可解释准入决策（append-only，payload 为 AdmissionDecision）。
ADMISSION_DECIDED = "competition.scheduler.admission_decision"
#: 预算耗尽：暂停相应操作并记录原因（设计 15）。
BUDGET_EXHAUSTED = "competition.scheduler.budget_exhausted"
#: 认证失效 / 平台策略变化导致的派发暂停。
DISPATCH_PAUSED = "competition.scheduler.dispatch_paused"
#: 自动化档位变更；从 autonomous 降级必须产生该事件（设计 15）。
AUTOMATION_CHANGED = "competition.policy.automation_changed"

#: 预算桶种类（任务书 10.5：Token、费用、墙钟、平台提交、动态实例配额）。
BUDGET_KINDS: tuple[str, ...] = (
    "tokens", "cost", "wallclock", "submissions", "instances",
)

#: 决策动作。
ACTION_ADMIT = "admit"      # 准入：ensure_bound_run + start
ACTION_HOLD = "hold"        # 挂起：规则不满足，下轮重评
ACTION_DEFER = "defer"      # 延迟：设置 not_before（近期失败退避）
ACTION_OBSERVE = "observe"  # observe 档：只记录不执行

#: 默认决策排序：operator priority → score → 排队时间 → challenge_id。
#: tsec_eval 另按饥饿档、难度、分值拆开，避免高分 hard 压过 easy。
_OPERATOR_PINNED_BONUS = 1_000_000.0  # 仅用于解释性分量，排序用 priority 列
_DECISION_PROJECTION_INTERVAL_S = 60.0

#: 真正占求解席位的 binding。paused / local_finished 不占 working set。
_RUN_SLOT_STATES: frozenset[BindingState] = frozenset({
    BindingState.PLANNED, BindingState.CREATING, BindingState.STARTING,
    BindingState.ACTIVE, BindingState.RESOLVING,
})

#: 题目已结束，不再回收 visit / 不再准入。
_TERMINAL_CHALLENGE_STATES: frozenset[ChallengeState] = frozenset({
    ChallengeState.SOLVED, ChallengeState.RETIRED, ChallengeState.SKIPPED,
    ChallengeState.EXHAUSTED,
})

#: tsec_eval 续访：题目可停在 running / candidate_found，不必回到 queued。
_TSEC_DISPATCHABLE: frozenset[ChallengeState] = frozenset({
    ChallengeState.QUEUED, ChallengeState.PROVISIONING,
    ChallengeState.RUNNING, ChallengeState.CANDIDATE_FOUND,
    ChallengeState.SUBMITTING,
})

_DEFAULT_DISPATCHABLE: frozenset[ChallengeState] = frozenset({
    ChallengeState.QUEUED, ChallengeState.PROVISIONING,
})


class SchedulerCapacity(ContractModel):
    """一次 tick 的容量输入快照（任务书 10.5 容量类输入）。

    由调用方按 tick 注入（``capacity_provider``），Scheduler 不自行探测
    运行时：0 / None 表示「该维度不限 / 由 policy 决定」。

    - ``runtime_seats_total`` / ``runtime_seats_used``：Runtime instance +
      credential seat 合并后的席位总量与占用（任务书 10.5 第 5 项）。
    - ``model_concurrency``：模型账户并发上限（0 不限）。
    - ``healthy_categories``：当前健康 Worker Profile 覆盖的类别
      （设计 11.4 第 4 条）；空列表表示「无类别能力信息」。
    - ``active_instances``：动态实例占用数；None 时从活动租约统计。
    """

    active_runs: Optional[int] = None       # None → 从活动 binding 统计
    worker_total: int = 0                   # 全部活动 Run 的 Worker 总量
    worker_limit: int = 0                   # 0 = 不限
    workers_per_run: int = 1                # 新 Run 预计占用的 Worker 数
    container_capacity: int = 0             # 0 = 不限
    runtime_seats_total: int = 0            # 0 = 不限
    runtime_seats_used: int = 0
    model_concurrency: int = 0              # 0 = 不限
    healthy_categories: list[str] = Field(default_factory=list)
    active_instances: Optional[int] = None  # None → 从活动租约统计


class RuleOutcome(ContractModel):
    """一条确定性规则的评估结果（写入 admission_decision.rules）。"""

    rule: str = ""
    passed: bool = True
    reason: str = ""          # 未通过时的稳定机器码
    detail: dict[str, Any] = Field(default_factory=dict)


class AdmissionDecision(ContractModel):
    """一次调度对一个题目的可解释准入决策（任务书 10.5）。

    记录输入快照（``inputs``）、逐条规则结果（``rules``）、预算变化
    （``budget_changes``）和最终动作（``action`` / ``reason``）。
    """

    decision_id: str = Field(default_factory=lambda: new_id("adm"))
    competition_id: str = ""
    challenge_id: str = ""
    tick: int = 0
    action: str = ACTION_HOLD
    reason: str = ""
    score: float = 0.0
    score_components: dict[str, float] = Field(default_factory=dict)
    rules: list[RuleOutcome] = Field(default_factory=list)
    inputs: dict[str, Any] = Field(default_factory=dict)
    budget_changes: dict[str, dict[str, float]] = Field(default_factory=dict)
    binding_id: Optional[str] = None
    run_id: Optional[str] = None
    advisor: dict[str, Any] = Field(default_factory=dict)
    decided_at: datetime = Field(default_factory=utcnow)


class ScheduleReport(ContractModel):
    """一轮 tick 的汇总（测试与审计断言入口）。"""

    competition_id: str = ""
    tick: int = 0
    automation_mode: str = ""
    scheduler_state: str = ""
    decided_at: datetime = Field(default_factory=utcnow)
    decisions: list[AdmissionDecision] = Field(default_factory=list)
    admitted: list[str] = Field(default_factory=list)   # challenge_id
    held: list[str] = Field(default_factory=list)
    deferred: list[str] = Field(default_factory=list)
    budgets: dict[str, dict[str, float]] = Field(default_factory=dict)
    advisor_status: str = "disabled"                    # ok/disabled/error


# ---------------------------------------------------------------------------
# 调度器
# ---------------------------------------------------------------------------


class CompetitionScheduler:
    """比赛级确定性调度器。

    构造参数：

    - ``store``：CompetitionStore（唯一持久化出口）；
    - ``binding_service``：COMP-05 RunBindingService（准入后建子 Run）；
    - ``advisor``：可选 CompetitionAdvisor；建议只记录、不改变确定规则
      的结果（设计 11.4：Scheduler 在没有 Advisor 时仍能完整运行）；
    - ``capacity_provider``：``() -> SchedulerCapacity``，按 tick 注入
      容量快照；缺省全部不限；
    - ``usage_provider``：``(competition_id) -> {kind: used}``，从每 Run
      已记录计量汇总全局消耗；缺省使用 ``resource_budgets.used`` 现值；
    - ``run_signal_provider``：可选 ``(run_id) -> dict``，汇总 Run 内的
      近期失败 / 重复路线 / Review 结果（可得范围内并入决策输入快照）；
    - ``dispatch_extra``：随 start 下发的附加载荷（如测试用 idle kind）；
      ``swarm_class`` 由 RunBindingService 强制剥离。本地 ``mock`` 平台在
      没有显式覆盖时使用固定附件命令验收 transport，其他平台使用标准
      Swarm；
    - ``failure_backoff_seconds``：近期失败后重新准入的退避窗口。
    """

    def __init__(
        self,
        store: CompetitionStore,
        binding_service: Optional[RunBindingService] = None,
        *,
        lease_manager: Any = None,
        advisor: Any = None,
        capacity_provider: Optional[Callable[[], SchedulerCapacity]] = None,
        usage_provider: Optional[Callable[[str], dict[str, float]]] = None,
        run_signal_provider: Optional[Callable[[str], dict[str, Any]]] = None,
        dispatch_extra: Optional[dict[str, Any]] = None,
        failure_backoff_seconds: float = 300.0,
    ) -> None:
        self._store = store
        self._binding = binding_service
        self._leases = lease_manager
        self._advisor = advisor
        self._capacity_provider = capacity_provider or (lambda: SchedulerCapacity())
        self._usage_provider = usage_provider
        self._run_signal_provider = run_signal_provider
        self._dispatch_extra = dict(dispatch_extra or {})
        self._failure_backoff = float(failure_backoff_seconds)
        self._tick = 0
        self._last_modes: dict[str, str] = {}

    # -- 主入口 ----------------------------------------------------------------

    async def tick(self, competition_id: str) -> ScheduleReport:
        """执行一轮确定性调度：评估 → 排序 → 准入 → 落 decision。"""
        self._tick += 1
        store = self._store
        competition = store.get(Competition, competition_id)
        if competition is None:
            raise NotFoundError(f"competition not found: {competition_id}")
        if competition.archived:
            raise NotFoundError(f"competition archived: {competition_id}")
        policy = store.get(CompetitionPolicy, competition_id) or CompetitionPolicy(
            competition_id=competition_id)
        connection = store.get(PlatformConnection, competition.connection_id)
        mode = policy.automation_mode
        report = ScheduleReport(
            competition_id=competition_id,
            tick=self._tick,
            automation_mode=mode,
            scheduler_state=competition.scheduler_state,
        )

        # 自动化档位变更检测：从 autonomous 降级必须产生策略变更事件（设计 15）。
        self._detect_mode_change(competition_id, mode)

        # Scheduler 未启动：本层不评估（Operator 命令走 COMP-01 Handler，
        # 三档均不受影响）。
        if competition.scheduler_state != SchedulerState.RUNNING.value:
            return report
        if competition.ends_at is not None and competition.ends_at <= utcnow():
            return report

        # 预算结算：从每 Run 已记录计量汇总 used，写回投影缓存并记录变化。
        budget_changes = self._settle_budgets(competition_id, policy)
        report.budgets = {
            kind: dict(change) for kind, change in budget_changes.items()
        }

        # Advisor：只读 projection，建议仅记录，不改变确定规则结果。
        advice = await self._consult_advisor(competition_id)
        report.advisor_status = (
            str(advice.get("status")) if advice is not None else "disabled"
        )

        capacity = self._capacity_provider()
        now = utcnow()

        # 自动入队（assisted / autonomous；observe 只同步不入队）。
        if mode != AutomationMode.OBSERVE.value:
            self._auto_enqueue(competition, policy, now)
            await self._enforce_terminal_phase_gate(competition, policy, now)
            # tsec_eval 中每道题持续运行到解出、人工停止或比赛结束，不再
            # 执行按 visit 时间盒暂停和复访的回收流程。
            await self._enforce_deepchain(competition, policy)

        # 逐题评估（规则管线 + 打分），产出全序。
        ranked = self._evaluate(
            competition, policy, connection, capacity, budget_changes,
            advice, now,
        )

        # 按确定优先级消费容量并准入。
        await self._admit(report, ranked, competition, policy, connection,
                          capacity, budget_changes, advice, now)
        return report

    # -- 档位变更事件 -------------------------------------------------------------

    def _detect_mode_change(self, competition_id: str, mode: str) -> None:
        prev = self._last_modes.get(competition_id)
        self._last_modes[competition_id] = mode
        if prev is None or prev == mode:
            return
        if prev == AutomationMode.AUTONOMOUS.value:
            # 从 autonomous 降级：必须产生策略变更事件（设计 15）。
            self._store.append_events([ev.make_event(
                competition_id=competition_id,
                aggregate_type=ev.AGG_COMPETITION,
                aggregate_id=competition_id,
                event_type=AUTOMATION_CHANGED,
                payload={
                    "competition_id": competition_id,
                    "from": prev,
                    "to": mode,
                    "downgrade": True,
                },
            )])

    # -- 预算结算 -----------------------------------------------------------------

    def _settle_budgets(
        self, competition_id: str, policy: CompetitionPolicy
    ) -> dict[str, dict[str, float]]:
        """刷新预算 used 并返回 {kind: {before, after, limit}}。

        used 的权威是每 Run 已记录计量（``usage_provider`` 汇总）；这里只把
        汇总结果写回 ``resource_budgets`` 作为投影缓存，不独立计量。
        policy.budget_limits 中没有预算行的 kind 也在此补建行。
        """
        store = self._store
        provided = (
            dict(self._usage_provider(competition_id))
            if self._usage_provider is not None else {}
        )
        rows = {
            row.kind: row
            for row in store.list(ResourceBudget, competition_id=competition_id)
        }
        changes: dict[str, dict[str, float]] = {}
        for kind in BUDGET_KINDS:
            row = rows.get(kind)
            limit = (
                row.limit if row is not None
                else float(policy.budget_limits.get(kind, 0.0))
            )
            before = row.used if row is not None else 0.0
            after = float(provided.get(kind, before))
            if row is None and not limit and kind not in provided:
                continue  # 无限制且无计量：不落行、不参与规则
            if row is None:
                row = ResourceBudget(
                    competition_id=competition_id, kind=kind, limit=limit,
                    used=after, window="competition",
                )
            elif after != before:
                row = row.model_copy(update={"used": after})
            store.save(row)
            changes[kind] = {"before": before, "after": after, "limit": limit}
        return changes

    # -- Advisor（可选，建议不改变确定规则） ----------------------------------------

    async def _consult_advisor(self, competition_id: str) -> Optional[dict[str, Any]]:
        advisor = self._advisor
        if advisor is None or not getattr(advisor, "enabled", False):
            return None
        try:
            advice = await advisor.advise(competition_id)
        except Exception as exc:  # Advisor 失败不影响确定调度（设计 11.4）
            return {"status": "error", "error": str(exc), "recommendations": []}
        return advice.to_dict() if hasattr(advice, "to_dict") else dict(advice)

    # -- 自动入队 -----------------------------------------------------------------

    def _auto_enqueue(
        self, competition: Competition, policy: CompetitionPolicy, now: datetime
    ) -> None:
        """assisted / autonomous 档把可做的 discovered/selected 题目自动入队。"""
        store = self._store
        for challenge in store.list(
            CompetitionChallenge, competition_id=competition.competition_id
        ):
            if challenge.tombstoned or challenge.remote_state != "open":
                continue
            if challenge.challenge_state() not in (
                ChallengeState.DISCOVERED, ChallengeState.SELECTED,
            ):
                continue
            if not challenge.current_revision_id:
                continue
            if challenge.category and challenge.category in policy.category_deny:
                continue  # 禁止策略：不自动入队（Operator 仍可手工 select）
            entry = store.get(
                SchedulerQueueEntry, competition.competition_id,
                challenge.challenge_id)
            if entry is not None and entry.state in (
                QueueEntryState.QUEUED.value, QueueEntryState.HELD.value,
                QueueEntryState.DISPATCHING.value,
            ):
                continue  # 已在队列
            # discovered → selected → queued（COMP-01 状态机转移序列）。
            current = challenge.challenge_state()
            for target in (ChallengeState.SELECTED, ChallengeState.QUEUED):
                if current is target:
                    continue
                ensure_challenge_transition(current, target)
                challenge = challenge.model_copy(
                    update={"state": target.value, "paused_from": None})
                store.save(challenge)
                store.append_events([ev.make_event(
                    competition_id=competition.competition_id,
                    aggregate_type=ev.AGG_CHALLENGE,
                    aggregate_id=challenge.challenge_id,
                    event_type=ev.CHALLENGE_STATE_CHANGED,
                    payload={
                        "challenge_id": challenge.challenge_id,
                        "from": current.value,
                        "to": target.value,
                        "reason": "scheduler.auto_enqueue",
                    },
                )])
                current = target
            store.save((entry or SchedulerQueueEntry(
                competition_id=competition.competition_id,
                competition_challenge_id=challenge.challenge_id,
            )).model_copy(update={"state": QueueEntryState.QUEUED.value}))
            store.append_events([ev.make_event(
                competition_id=competition.competition_id,
                aggregate_type=ev.AGG_CHALLENGE,
                aggregate_id=challenge.challenge_id,
                event_type=ev.QUEUE_ENQUEUED,
                payload={
                    "challenge_id": challenge.challenge_id,
                    "priority": 0.0,
                    "reason": "scheduler.auto_enqueue",
                },
            )])

    # -- 评估（规则管线 + 打分） ------------------------------------------------------

    def _evaluate(
        self,
        competition: Competition,
        policy: CompetitionPolicy,
        connection: Optional[PlatformConnection],
        capacity: SchedulerCapacity,
        budget_changes: dict[str, dict[str, float]],
        advice: Optional[dict[str, Any]],
        now: datetime,
    ) -> list[tuple[SchedulerQueueEntry, AdmissionDecision, float]]:
        """对队列中每个待调度题目跑规则管线并打分，返回全序排名。"""
        store = self._store
        exhausted = [
            kind for kind, change in budget_changes.items()
            if change["limit"] > 0 and change["after"] >= change["limit"]
        ]
        auth_failed = (
            connection is not None
            and connection.status == ConnectionStatus.AUTH_REQUIRED.value
        )
        automation_off = bool(
            connection is not None
            and str(connection.capabilities.get("automation") or "").lower()
            in ("off", "disabled", "readonly")
        )
        # 全局暂停事件：预算耗尽 / 认证失效 / 平台策略变化各记一次（设计 15）。
        for kind in exhausted:
            store.append_events([ev.make_event(
                competition_id=competition.competition_id,
                aggregate_type=ev.AGG_COMPETITION,
                aggregate_id=competition.competition_id,
                event_type=BUDGET_EXHAUSTED,
                payload={
                    "competition_id": competition.competition_id,
                    "kind": kind,
                    "limit": budget_changes[kind]["limit"],
                    "used": budget_changes[kind]["after"],
                    "operation": "dispatch",
                },
            )])
        if auth_failed:
            store.append_events([ev.make_event(
                competition_id=competition.competition_id,
                aggregate_type=ev.AGG_COMPETITION,
                aggregate_id=competition.competition_id,
                event_type=DISPATCH_PAUSED,
                payload={
                    "competition_id": competition.competition_id,
                    "reason": "auth_required",
                    "operation": "dispatch",
                },
            )])
        if automation_off:
            store.append_events([ev.make_event(
                competition_id=competition.competition_id,
                aggregate_type=ev.AGG_COMPETITION,
                aggregate_id=competition.competition_id,
                event_type=DISPATCH_PAUSED,
                payload={
                    "competition_id": competition.competition_id,
                    "reason": "platform_policy",
                    "operation": "dispatch",
                },
            )])

        ranked: list[tuple[SchedulerQueueEntry, AdmissionDecision, float]] = []
        for entry in store.list(
            SchedulerQueueEntry, competition_id=competition.competition_id
        ):
            if entry.state not in (
                QueueEntryState.QUEUED.value, QueueEntryState.HELD.value,
            ):
                continue
            challenge = store.get(
                CompetitionChallenge, entry.competition_challenge_id)
            if challenge is None:
                continue
            decision = self._decide(
                competition, policy, connection, capacity, challenge, entry,
                exhausted, auth_failed, automation_off, budget_changes,
                advice, now,
            )
            ranked.append((entry, decision, decision.score))

        configured_order = [
            str(item).strip()
            for item in (getattr(policy, "challenge_order", None) or [])
            if str(item).strip()
        ]
        if configured_order:
            order_by_external_id = {
                external_id: index
                for index, external_id in enumerate(configured_order)
            }
            order_by_challenge_id = {
                challenge.challenge_id: order_by_external_id.get(
                    challenge.external_challenge_id, len(configured_order)
                )
                for challenge in store.list(
                    CompetitionChallenge,
                    competition_id=competition.competition_id,
                )
            }
            ranked.sort(key=lambda item: (
                -item[0].priority,
                order_by_challenge_id.get(
                    item[0].competition_challenge_id, len(configured_order)
                ),
                item[0].enqueued_at.isoformat(),
                item[0].competition_challenge_id,
            ))
            return ranked

        tsec_eval = str(
            getattr(policy, "policy_profile", "") or ""
        ) == POLICY_PROFILE_TSEC_EVAL
        terminal_fill_idle = bool(
            getattr(policy, "terminal_phase_fill_idle", False)
        )
        terminal_challenge_ids = {
            challenge.challenge_id
            for challenge in store.list(
                CompetitionChallenge,
                competition_id=competition.competition_id,
            )
            if challenge.external_challenge_id in self._policy_challenge_ids(
                policy, "terminal_phase_challenge_ids"
            )
        }
        ranked.sort(key=lambda item, _tsec=tsec_eval: (
            (
                1
                if terminal_fill_idle
                and item[0].competition_challenge_id in terminal_challenge_ids
                else 0
            ),
            *self._admission_sort_key(
                item[0], item[1], item[2], tsec_eval=_tsec
            ),
        ))
        return ranked

    @staticmethod
    def _admission_sort_key(
        entry: SchedulerQueueEntry,
        decision: AdmissionDecision,
        score: float,
        *,
        tsec_eval: bool,
    ) -> tuple:
        """默认按综合分；tsec_eval 先饥饿再 easy，同档临门题优先。"""
        if tsec_eval:
            comps = decision.score_components or {}
            # easy +90 / medium +45 / hard +0 → rank 0/1/2。跨难度只看 rank。
            # hard 的 priority 是 0.0，不能用 ``or 45``。
            raw_prio = comps.get("difficulty_priority")
            prio = 45.0 if raw_prio is None else float(raw_prio)
            diff_rank = int(round((90.0 - prio) / 45.0))
            points = float(comps.get("points") or 0.0)
            near = 0 if float(comps.get("near_solve") or 0.0) > 0 else 1
            starve = int(comps.get("visit_band") or 2)
            return (
                -entry.priority,
                starve,
                diff_rank,
                near,
                -points,
                -score,
                entry.enqueued_at.isoformat(),
                entry.competition_challenge_id,
            )
        return (
            -entry.priority,
            -score,
            entry.enqueued_at.isoformat(),
            entry.competition_challenge_id,
        )

    def _decide(
        self,
        competition: Competition,
        policy: CompetitionPolicy,
        connection: Optional[PlatformConnection],
        capacity: SchedulerCapacity,
        challenge: CompetitionChallenge,
        entry: SchedulerQueueEntry,
        exhausted: list[str],
        auth_failed: bool,
        automation_off: bool,
        budget_changes: dict[str, dict[str, float]],
        advice: Optional[dict[str, Any]],
        now: datetime,
    ) -> AdmissionDecision:
        """单题规则管线 + 打分；规则按固定顺序，首个失败规则即最终原因。"""
        store = self._store
        revision = (
            store.get(ChallengeRevision, challenge.current_revision_id)
            if challenge.current_revision_id else None
        )
        signals = self._signals(challenge, now)
        inputs = self._input_snapshot(
            competition, policy, connection, capacity, challenge, revision,
            entry, signals, budget_changes, now,
        )
        mode = policy.automation_mode

        rules: list[RuleOutcome] = []

        def gate(rule: str, passed: bool, reason: str = "",
                 **detail: Any) -> bool:
            rules.append(RuleOutcome(
                rule=rule, passed=passed, reason=reason, detail=detail))
            return passed

        # 规则顺序固定（与设计 11.4 / 15 对齐）；全部为确定性判定。
        ok = True
        ok = gate("automation_mode", mode != AutomationMode.OBSERVE.value,
                  "automation_mode_observe", mode=mode) and ok
        dispatchable = (
            _TSEC_DISPATCHABLE if self._is_tsec(policy)
            else _DEFAULT_DISPATCHABLE
        )
        ok = gate("challenge_state",
                  challenge.challenge_state() in dispatchable,
                  "challenge_not_dispatchable",
                  state=challenge.state) and ok
        ok = gate("tombstone", not challenge.tombstoned,
                  "tombstoned") and ok
        ok = gate("remote_state",
                  challenge.remote_state == "open",
                  ("remote_solved" if challenge.remote_state == "solved_remote"
                   else "remote_locked"),
                  remote_state=challenge.remote_state) and ok
        ok = gate("operator_hold", entry.state != QueueEntryState.HELD.value,
                  "operator_hold") and ok
        ok = gate("category_policy",
                  not (challenge.category
                       and challenge.category in policy.category_deny),
                  "category_denied", category=challenge.category) and ok
        phase_blockers = self._terminal_phase_blockers(challenge, policy)
        ok = gate(
            "terminal_phase",
            not phase_blockers,
            "terminal_phase_wait",
            remaining=len(phase_blockers),
            blockers=phase_blockers[:10],
        ) and ok
        ok = gate("connection_auth", not auth_failed, "auth_required") and ok
        ok = gate("platform_automation", not automation_off,
                  "platform_policy") and ok
        for kind in exhausted:
            ok = gate(f"budget:{kind}", False, f"budget_exhausted:{kind}",
                      limit=budget_changes[kind]["limit"],
                      used=budget_changes[kind]["after"]) and ok
        ok = gate("revision", revision is not None, "no_revision") and ok
        # 测评策略：总预算耗尽则 hold。
        persistent = self._is_persistent_challenge(challenge, policy)
        total_budget = float(getattr(policy, "total_budget_s", 0) or 0)
        if (
            not persistent
            and total_budget > 0
            and competition.created_at is not None
        ):
            elapsed = max(0.0, (now - competition.created_at).total_seconds())
            ok = gate(
                "total_budget",
                elapsed < total_budget,
                "total_budget_exhausted",
                elapsed=elapsed,
                limit=total_budget,
            ) and ok
        # 测评策略：visit floor —— 距上次准入不足 floor 且未 overdue 则 defer。
        floor_s = float(getattr(policy, "visit_floor_s", 0) or 0)
        overdue_mult = float(getattr(policy, "overdue_mult", 1.25) or 1.25)
        last_visit = self._last_visit_at(entry)
        last_wrong = self._parse_iso_dt(signals.get("last_wrong_at"))
        # 远端在本轮准入之后明确判错，意味着当前候选已被
        # 持久否决，需要尽快续做。这一次重试跳过通用 visit floor；
        # 新一轮准入会刷新 last_visit_at，因此不会因历史 wrong
        # 永久占用调度席位。
        wrong_after_visit = (
            last_wrong is not None
            and (last_visit is None or last_wrong > last_visit)
        )
        visit_deferred = False
        fill_idle_revisit = self._fill_idle_revisit(
            competition, policy, capacity, challenge
        )
        if (
            ok
            and not persistent
            and not wrong_after_visit
            and not fill_idle_revisit
            and floor_s > 0
            and last_visit is not None
        ):
            age = (now - last_visit).total_seconds()
            overdue = age >= floor_s * overdue_mult
            under_floor = age < floor_s
            dry_waves = int((entry.admission_decision or {}).get("dry_waves") or 0)
            dry_cap = int(getattr(policy, "dry_defer_waves", 0) or 0)
            blackhole = dry_cap > 0 and dry_waves >= dry_cap
            if under_floor and not overdue and not blackhole:
                visit_deferred = True
                gate("visit_floor", False, "visit_floor",
                     age=age, floor=floor_s)
            elif blackhole and under_floor and not overdue:
                visit_deferred = True
                gate("blackhole_defer", False, "blackhole_defer",
                     dry_waves=dry_waves)
        elif ok and fill_idle_revisit and floor_s > 0 and last_visit is not None:
            gate(
                "visit_floor",
                True,
                "idle_slot_revisit",
                age=max(0.0, (now - last_visit).total_seconds()),
                floor=floor_s,
            )
        # 近期失败退避：窗口内不准入（其余容量规则在 _admit 消费容量时评估）。
        deferred = (
            ok and entry.not_before is not None and entry.not_before > now
        )
        if deferred:
            gate("failure_backoff", False, "failure_backoff",
                 not_before=entry.not_before.isoformat())
        if visit_deferred:
            deferred = True

        score, components = self._score(
            competition, capacity, challenge, revision, entry,
            signals, now, policy=policy,
        )
        if mode == AutomationMode.OBSERVE.value:
            action, reason = ACTION_OBSERVE, "automation_mode_observe"
        elif deferred and visit_deferred and not (
            entry.not_before is not None and entry.not_before > now
        ):
            action, reason = ACTION_DEFER, "visit_floor"
        elif deferred:
            action, reason = ACTION_DEFER, "failure_backoff"
        elif not ok:
            failing = next(r for r in rules if not r.passed)
            action, reason = ACTION_HOLD, failing.reason
        else:
            action, reason = ACTION_ADMIT, "admitted"

        return AdmissionDecision(
            competition_id=competition.competition_id,
            challenge_id=challenge.challenge_id,
            tick=self._tick,
            action=action,
            reason=reason,
            score=score,
            score_components=components,
            rules=rules,
            inputs=inputs,
            budget_changes={
                k: dict(v) for k, v in budget_changes.items()
            },
            advisor=self._advisor_summary(advice, challenge.challenge_id),
        )

    # -- 打分（设计 11.4 第 2~5 条） --------------------------------------------------

    def _score(
        self,
        competition: Competition,
        capacity: SchedulerCapacity,
        challenge: CompetitionChallenge,
        revision: Optional[ChallengeRevision],
        entry: SchedulerQueueEntry,
        signals: dict[str, Any],
        now: datetime,
        *,
        policy: Optional[CompetitionPolicy] = None,
    ) -> tuple[float, dict[str, float]]:
        store = self._store
        components: dict[str, float] = {}

        # 2. 阻塞其他题目：本题是其他开放题的前置 → 每个依赖者加权。
        blocking = 0.0
        if revision is not None:
            key_ids = {challenge.external_challenge_id, challenge.name} - {""}
            for other in store.list(
                CompetitionChallenge, competition_id=competition.competition_id
            ):
                if other.challenge_id == challenge.challenge_id:
                    continue
                if other.tombstoned or other.remote_state != "open":
                    continue
                other_rev = (
                    store.get(ChallengeRevision, other.current_revision_id)
                    if other.current_revision_id else None
                )
                if other_rev and key_ids & set(other_rev.prerequisites):
                    blocking += 200.0
        components["blocking_dependents"] = blocking

        # 即将到期：比赛临近结束时整体加权（均匀，不改变相对顺序，可解释）。
        remaining = self._remaining_seconds(competition, now)
        components["endgame_urgency"] = (
            25.0 if remaining is not None and remaining <= 3600 else 0.0
        )

        # 3. 分值 / 提示。
        points = float(revision.points) if revision is not None else 0.0
        components["points"] = points
        hints = float(len(revision.hints)) * 5.0 if revision is not None else 0.0
        components["hints"] = hints

        # tsec_eval：easy 先做；分值只在同难度内比。临门题（已有候选 /
        # 正在提交）在同档提前，避免续跑后被新 hard 挤掉。
        if policy is not None and str(
            getattr(policy, "policy_profile", "") or ""
        ) == POLICY_PROFILE_TSEC_EVAL:
            diff = difficulty_of(challenge.category, challenge.name)
            components["difficulty_priority"] = (
                90.0 - 45.0 * float(difficulty_rank(diff))
            )
            if challenge.state in (
                ChallengeState.CANDIDATE_FOUND.value,
                ChallengeState.SUBMITTING.value,
            ) or int(signals.get("candidates") or 0) > 0:
                components["near_solve"] = 500.0
            components["visit_band"] = float(self._visit_band(entry, policy, now))

        # 4. 当前健康 Worker Profile 的类别能力。
        healthy = set(capacity.healthy_categories)
        components["healthy_category"] = (
            50.0 if healthy and challenge.category in healthy else 0.0
        )

        # 5. 排队时间与类别覆盖。
        waiting = max(0.0, (now - entry.enqueued_at).total_seconds()) / 60.0
        components["waiting_minutes"] = waiting
        active_categories = {
            store.get(CompetitionChallenge, b.competition_challenge_id).category
            for b in store.list(RunBinding, competition_id=competition.competition_id)
            if b.binding_state() in _RUN_SLOT_STATES
            and store.get(CompetitionChallenge, b.competition_challenge_id)
            is not None
        }
        components["category_coverage"] = (
            30.0 if challenge.category not in active_categories else 0.0
        )

        # 近期失败与已有候选降权（任务书 10.5 末项，可得范围内）。
        components["recent_failure_penalty"] = (
            -50.0 * float(signals.get("recent_failures", 0))
        )
        components["existing_candidate"] = (
            40.0 if signals.get("candidates", 0) > 0 else 0.0
        )
        components["wrong_submission_penalty"] = (
            -20.0 * float(signals.get("wrong_submissions", 0))
        )
        # Operator 固定优先级作为解释性分量（真实排序键是 queue.priority）。
        components["operator_priority"] = (
            _OPERATOR_PINNED_BONUS if entry.priority > 0 else 0.0
        )
        return sum(components.values()), components

    # -- 准入 ---------------------------------------------------------------------

    async def _admit(
        self,
        report: ScheduleReport,
        ranked: list[tuple[SchedulerQueueEntry, AdmissionDecision, float]],
        competition: Competition,
        policy: CompetitionPolicy,
        connection: Optional[PlatformConnection],
        capacity: SchedulerCapacity,
        budget_changes: dict[str, dict[str, float]],
        advice: Optional[dict[str, Any]],
        now: datetime,
    ) -> None:
        """按全序消费容量并执行准入；容量耗尽时后续候选落 hold。"""
        store = self._store
        active_runs = (
            capacity.active_runs
            if capacity.active_runs is not None
            else self._active_run_count(competition.competition_id)
        )
        active_instances = (
            capacity.active_instances
            if capacity.active_instances is not None
            else self._active_instance_count(competition.competition_id)
        )
        seats_used = capacity.runtime_seats_used
        workers = capacity.worker_total

        for entry, decision, _score in ranked:
            if competition.ends_at is not None and competition.ends_at <= utcnow():
                break
            if decision.action != ACTION_ADMIT:
                self._finalize(report, entry, decision)
                continue
            challenge = store.get(
                CompetitionChallenge, decision.challenge_id)
            revision = (
                store.get(ChallengeRevision, challenge.current_revision_id)
                if challenge and challenge.current_revision_id else None
            )
            needs_instance = self._needs_instance(connection, revision)
            existing_lease = (
                store.active_lease_for_challenge(challenge.challenge_id)
                if challenge is not None else None
            )
            requires_new_instance = needs_instance and existing_lease is None
            existing_binding = (
                store.active_binding_for_challenge(challenge.challenge_id)
                if challenge is not None else None
            )
            existing_binding_state = (
                existing_binding.binding_state()
                if existing_binding is not None else None
            )
            occupying = (
                existing_binding is not None
                and existing_binding_state in _RUN_SLOT_STATES
            )
            retrying_start = existing_binding_state in {
                BindingState.PLANNED,
                BindingState.CREATING,
                BindingState.STARTING,
            }
            visit_count = int((entry.admission_decision or {}).get("visit_count") or 0)
            first_visit = visit_count == 0
            working_set = int(getattr(policy, "working_set", 0) or 0)

            # 容量规则（消费型，按全序逐个检查）。
            block = ""
            if occupying and not retrying_start:
                block = "already_running"
            elif not retrying_start:
                if (policy.max_concurrent_runs > 0
                        and active_runs >= policy.max_concurrent_runs):
                    block = "run_capacity"
                elif (
                    working_set > 0
                    and not first_visit
                    and active_runs >= working_set
                ):
                    block = "working_set"
                elif (
                    challenge is not None
                    and self._deepchain_hold(challenge, revision, policy)
                ):
                    block = "deepchain_slots"
                elif (capacity.runtime_seats_total > 0
                        and seats_used >= capacity.runtime_seats_total):
                    block = "no_runtime_seat"
                elif (capacity.worker_limit > 0
                        and workers + capacity.workers_per_run
                        > capacity.worker_limit):
                    block = "worker_capacity"
                elif (capacity.container_capacity > 0
                        and active_runs >= capacity.container_capacity):
                    block = "container_capacity"
                elif (requires_new_instance and policy.max_instances > 0
                        and active_instances >= policy.max_instances):
                    block = "instance_quota"
            if block:
                decision.rules.append(RuleOutcome(
                    rule=f"capacity:{block}", passed=False, reason=block))
                decision.action = ACTION_HOLD
                decision.reason = block
                self._finalize(report, entry, decision)
                continue

            try:
                binding = await self._dispatch(
                    challenge, needs_instance=needs_instance)
            except Exception as exc:
                decision.rules.append(RuleOutcome(
                    rule="dispatch", passed=False, reason="dispatch_failed",
                    detail={"error": str(exc)}))
                decision.action = ACTION_HOLD
                decision.reason = "dispatch_failed"
                self._finalize(report, entry, decision)
                continue

            decision.binding_id = binding.binding_id
            decision.run_id = binding.run_id
            if not occupying:
                active_runs += 1
                seats_used += 1
                workers += capacity.workers_per_run
            if requires_new_instance:
                active_instances += 1
            self._finalize(report, entry, decision, admitted=True)

    async def _dispatch(
        self,
        challenge: CompetitionChallenge,
        *,
        needs_instance: bool = False,
    ) -> RunBinding:
        """准入执行：ensure_bound_run + start，并推进题目 / 队列状态。"""
        if self._binding is None:
            raise RuntimeError(
                "CompetitionScheduler has no RunBindingService; "
                "admission requires one"
            )
        store = self._store
        lease = store.active_lease_for_challenge(challenge.challenge_id)
        acquired_here = False
        if (
            needs_instance
            and lease is None
        ):
            if self._leases is None:
                raise RuntimeError(
                    "dynamic instance is required but InstanceLeaseManager is unavailable"
                )
            lease = await self._leases.acquire(
                challenge.challenge_id, owner="competition_scheduler")
            acquired_here = True
        try:
            binding = await self._binding.ensure_bound_run(challenge.challenge_id)
            # The first lease is acquired before the binding exists so the Run
            # start payload can compile the target address. Link the two rows
            # after binding creation as well; revisit acquisition already links
            # an existing binding inside InstanceLeaseManager.
            if lease is not None and binding.lease_id != lease.lease_id:
                binding = store.save(binding.model_copy(
                    update={"lease_id": lease.lease_id}))
        except Exception:
            if acquired_here and self._leases is not None:
                await self._leases.release(
                    lease_id=lease.lease_id, reason="dispatch_failed")
            raise
        dispatch_extra = dict(self._dispatch_extra)
        competition = store.get(Competition, challenge.competition_id)
        connection = (
            store.get(PlatformConnection, competition.connection_id)
            if competition is not None else None
        )
        if connection is not None and connection.platform_kind == "mock":
            dispatch_extra.setdefault("kind", "mock_platform_acceptance")
        if connection is not None and connection.platform_kind == "tsecbench":
            dispatch_extra["allow_operator_input"] = False
        policy = (
            store.get(CompetitionPolicy, challenge.competition_id)
            if competition is not None else None
        )
        entry = store.get(
            SchedulerQueueEntry, challenge.competition_id, challenge.challenge_id
        )
        if policy is not None and (
            self._is_tsec(policy)
            or self._is_persistent_challenge(challenge, policy)
        ):
            # 0 在 RunGateway 中表示无限墙钟预算。持续执行题持有同一靶场
            # 环境，不进入 visit 的暂停/复访循环。
            dispatch_extra["visit_timebox_s"] = 0
            dispatch_extra["wall_clock_budget"] = 0
            dispatch_extra["keepalive_max"] = 0
        elif policy is not None:
            visits = int((entry.admission_decision or {}).get("visit_count") or 0) if entry else 0
            timebox = visit_timebox_seconds(policy, visits)
            dispatch_extra.setdefault("visit_timebox_s", timebox)
            dispatch_extra.setdefault("wall_clock_budget", timebox)
            if int(getattr(policy, "keepalive_max", 0) or 0) > 0:
                dispatch_extra.setdefault(
                    "keepalive_max", int(policy.keepalive_max)
                )
        try:
            state = binding.binding_state()
            if state is BindingState.PAUSED:
                resolved = await self._binding.resolve_execution(
                    challenge.challenge_id,
                    reason="scheduler.revisit",
                    extra_payload=dispatch_extra,
                )
                if resolved is None:
                    raise RuntimeError(
                        f"paused binding missing for {challenge.challenge_id}"
                    )
                binding = resolved
            elif state in {BindingState.ACTIVE, BindingState.RESOLVING}:
                return binding
            else:
                binding = await self._binding.start(
                    binding, extra_payload=dispatch_extra)
        except Exception:
            current = store.active_binding_for_challenge(challenge.challenge_id)
            if (
                acquired_here
                and self._leases is not None
                and (current is None or current.binding_state() is BindingState.PAUSED)
            ):
                await self._leases.release(
                    lease_id=lease.lease_id, reason="dispatch_failed")
            raise
        self._advance_challenge_to_running(challenge, binding)
        return binding

    def _finalize(
        self,
        report: ScheduleReport,
        entry: SchedulerQueueEntry,
        decision: AdmissionDecision,
        *,
        admitted: bool = False,
    ) -> None:
        """更新最新 decision；仅在决策转折或准入时追加审计事件。"""
        store = self._store
        body = decision.model_dump(mode="json")
        previous = dict(entry.admission_decision or {})
        decision_changed = (
            admitted
            or previous.get("action") != decision.action
            or previous.get("reason") != decision.reason
        )
        previous_decided_at = self._parse_iso_dt(previous.get("decided_at"))
        projection_due = (
            decision_changed
            or previous_decided_at is None
            or (decision.decided_at - previous_decided_at).total_seconds()
            >= _DECISION_PROJECTION_INTERVAL_S
        )
        if not projection_due:
            report.decisions.append(decision)
            return
        if admitted:
            prev = dict(entry.admission_decision or {})
            visits = int(prev.get("visit_count") or 0) + 1
            body = dict(body)
            body["visit_count"] = visits
            body["last_visit_at"] = utcnow().isoformat()
            # 保留 dry_waves 供 blackhole defer；有进展时由外部清零。
            if "dry_waves" in prev:
                body["dry_waves"] = prev.get("dry_waves")
            body["candidate_count_at_visit"] = len(store.list(
                SubmissionCandidate,
                competition_challenge_id=entry.competition_challenge_id,
            ))
            policy = store.get(CompetitionPolicy, decision.competition_id)
            challenge = store.get(
                CompetitionChallenge, entry.competition_challenge_id)
            if (
                policy is not None
                and challenge is not None
                and (
                    self._is_tsec(policy)
                    or self._is_persistent_challenge(challenge, policy)
                )
            ):
                body.pop("visit_timebox_s", None)
                body["persistent_execution"] = True
            elif policy is not None:
                body["visit_timebox_s"] = visit_timebox_seconds(policy, visits - 1)
            entry = entry.model_copy(update={
                "state": QueueEntryState.DONE.value,
                "score": decision.score,
                "admission_decision": body,
            })
            report.admitted.append(decision.challenge_id)
        else:
            # 保留 visit 游标，避免 defer/hold 冲掉 floor 计时。
            merged = dict(entry.admission_decision or {})
            for key in (
                "visit_count", "last_visit_at", "dry_waves",
                "visit_timebox_s", "candidate_count_at_visit",
                "persistent_execution",
            ):
                if key in merged and key not in body:
                    body[key] = merged[key]
            policy = store.get(CompetitionPolicy, decision.competition_id)
            challenge = store.get(
                CompetitionChallenge, entry.competition_challenge_id)
            if (
                policy is not None
                and challenge is not None
                and (
                    self._is_tsec(policy)
                    or self._is_persistent_challenge(challenge, policy)
                )
            ):
                body.pop("visit_timebox_s", None)
                body["persistent_execution"] = True
            entry = entry.model_copy(update={
                "score": decision.score,
                "admission_decision": body,
            })
            if decision.action == ACTION_DEFER:
                report.deferred.append(decision.challenge_id)
            elif decision.action == ACTION_HOLD:
                report.held.append(decision.challenge_id)
        store.save(entry)
        report.decisions.append(decision)
        if not decision_changed:
            return
        store.append_events([ev.make_event(
            competition_id=decision.competition_id,
            aggregate_type=ev.AGG_COMPETITION,
            aggregate_id=decision.competition_id,
            event_type=ADMISSION_DECIDED,
            payload=body,
        )])

    # -- 输入快照与信号 ---------------------------------------------------------------

    def _input_snapshot(
        self,
        competition: Competition,
        policy: CompetitionPolicy,
        connection: Optional[PlatformConnection],
        capacity: SchedulerCapacity,
        challenge: CompetitionChallenge,
        revision: Optional[ChallengeRevision],
        entry: SchedulerQueueEntry,
        signals: dict[str, Any],
        budget_changes: dict[str, dict[str, float]],
        now: datetime,
    ) -> dict[str, Any]:
        lease = self._store.active_lease_for_challenge(challenge.challenge_id)
        return {
            "challenge": {
                "challenge_id": challenge.challenge_id,
                "external_challenge_id": challenge.external_challenge_id,
                "state": challenge.state,
                "remote_state": challenge.remote_state,
                "unlocked": challenge.remote_state == "open",
                "category": challenge.category,
                "points": revision.points if revision else 0.0,
                "hints": len(revision.hints) if revision else 0,
                "prerequisites": (
                    list(revision.prerequisites) if revision else []),
                "revision_id": challenge.current_revision_id,
            },
            "competition": {
                "ends_at": (
                    competition.ends_at.isoformat()
                    if competition.ends_at else None),
                "remaining_seconds": self._remaining_seconds(competition, now),
                "scheduler_state": competition.scheduler_state,
                "automation_mode": policy.automation_mode,
            },
            "queue": {
                "priority": entry.priority,
                "enqueued_at": entry.enqueued_at.isoformat(),
                "waiting_seconds": max(
                    0.0, (now - entry.enqueued_at).total_seconds()),
                "not_before": (
                    entry.not_before.isoformat() if entry.not_before else None),
                "state": entry.state,
            },
            "capacity": capacity.model_dump(mode="json"),
            "policy": {
                "max_concurrent_runs": policy.max_concurrent_runs,
                "max_instances": policy.max_instances,
                "category_allow": list(policy.category_allow),
                "category_deny": list(policy.category_deny),
            },
            "connection": {
                "status": connection.status if connection else "",
                "automation": (
                    connection.capabilities.get("automation")
                    if connection else None),
            },
            "lease": (
                {
                    "lease_id": lease.lease_id,
                    "state": lease.state,
                    "expires_at": (
                        lease.expires_at.isoformat() if lease.expires_at else None),
                }
                if lease is not None else None
            ),
            "budgets": {k: dict(v) for k, v in budget_changes.items()},
            "signals": dict(signals),
        }

    def _signals(
        self, challenge: CompetitionChallenge, now: datetime
    ) -> dict[str, Any]:
        """近期失败 / 重复路线 / 已有候选 / Review 结果（可得范围内）。

        competition.db 内可得：失败 binding 数、最近失败时间、候选数、
        判错提交数。Run 内的重复路线与 Review 结果经可选
        ``run_signal_provider`` 按 run 汇总并入（不可用时缺省为空）。
        """
        store = self._store
        bindings = store.list(
            RunBinding, competition_challenge_id=challenge.challenge_id)
        failed = [b for b in bindings if b.state == BindingState.FAILED.value]
        last_failed_at = max(
            (b.updated_at for b in failed), default=None)
        candidates = store.list(
            SubmissionCandidate,
            competition_challenge_id=challenge.challenge_id)
        wrong = [
            s for s in store.list(
                PlatformSubmission,
                competition_challenge_id=challenge.challenge_id)
            if s.state == SubmissionState.WRONG.value
        ]
        last_wrong_at = max(
            (submission.updated_at for submission in wrong), default=None)
        signals: dict[str, Any] = {
            "failed_bindings": len(failed),
            "recent_failures": len(failed),
            "last_failed_at": (
                last_failed_at.isoformat() if last_failed_at else None),
            "candidates": len(candidates),
            "wrong_submissions": len(wrong),
            "last_wrong_at": (
                last_wrong_at.isoformat() if last_wrong_at else None),
        }
        if self._run_signal_provider is not None:
            per_run: dict[str, Any] = {}
            for binding in bindings:
                if not binding.run_id:
                    continue
                try:
                    per_run[binding.run_id] = dict(
                        self._run_signal_provider(binding.run_id) or {})
                except Exception as exc:  # 信号不可得时不影响确定调度
                    per_run[binding.run_id] = {"error": str(exc)}
            signals["run_signals"] = per_run
        return signals

    # -- 计数与判定助手 -----------------------------------------------------------------

    def _active_run_count(self, competition_id: str) -> int:
        return sum(
            1 for b in self._store.list(
                RunBinding, competition_id=competition_id)
            if b.binding_state() in _RUN_SLOT_STATES
        )

    def _active_instance_count(self, competition_id: str) -> int:
        return sum(
            1 for lease in self._store.list(
                InstanceLease, competition_id=competition_id)
            if lease.lease_state() in LEASE_ACTIVE_STATES
        )

    @staticmethod
    def _needs_instance(
        connection: Optional[PlatformConnection],
        revision: Optional[ChallengeRevision],
    ) -> bool:
        """题目是否需要动态实例：revision 无稳定 target 且平台声明实例能力。"""
        if revision is None or revision.target:
            return False
        if connection is None:
            return False
        caps = connection.capabilities or {}
        return bool(
            caps.get("dynamic_instances") or caps.get("instances"))

    @staticmethod
    def _remaining_seconds(
        competition: Competition, now: datetime
    ) -> Optional[float]:
        if competition.ends_at is None:
            return None
        return max(0.0, (competition.ends_at - now).total_seconds())

    def _advisor_summary(
        self, advice: Optional[dict[str, Any]], challenge_id: str
    ) -> dict[str, Any]:
        """写入 decision 的 Advisor 摘要：仅记录建议，规则结果不受其影响。"""
        if advice is None:
            return {"consulted": False}
        related = [
            rec for rec in advice.get("recommendations", [])
            if not rec.get("challenge_id")
            or rec.get("challenge_id") == challenge_id
        ]
        return {
            "consulted": True,
            "status": advice.get("status", ""),
            "recommendations": related,
        }

    # -- tsec_eval visit / deepchain ------------------------------------------------

    @staticmethod
    def _is_tsec(policy: Optional[CompetitionPolicy]) -> bool:
        return str(
            getattr(policy, "policy_profile", "") or ""
        ) == POLICY_PROFILE_TSEC_EVAL

    @staticmethod
    def _parse_iso_dt(raw: Any) -> Optional[datetime]:
        if not isinstance(raw, str) or not raw:
            return None
        try:
            value = datetime.fromisoformat(raw.replace("Z", "+00:00"))
        except ValueError:
            return None
        if value.tzinfo is None:
            return value.replace(tzinfo=timezone.utc)
        return value.astimezone(timezone.utc)

    def _last_visit_at(self, entry: SchedulerQueueEntry) -> Optional[datetime]:
        return self._parse_iso_dt(
            (entry.admission_decision or {}).get("last_visit_at")
        )

    def _is_multi_flag(
        self,
        challenge: Optional[CompetitionChallenge],
        revision: Optional[ChallengeRevision] = None,
    ) -> bool:
        if challenge is None:
            return False
        if revision is None and challenge.current_revision_id:
            revision = self._store.get(
                ChallengeRevision, challenge.current_revision_id)
        if revision is None:
            return False
        return bool(revision.multi_flag or int(revision.expected_flags or 1) > 1)

    def _visit_band(
        self,
        entry: SchedulerQueueEntry,
        policy: CompetitionPolicy,
        now: datetime,
    ) -> int:
        """0 未访/饥饿，1 过期，2 近期，3 空转黑洞。"""
        visits = int((entry.admission_decision or {}).get("visit_count") or 0)
        last_visit = self._last_visit_at(entry)
        if visits <= 0 or last_visit is None:
            return 0
        floor_s = float(getattr(policy, "visit_floor_s", 0) or 0)
        overdue_mult = float(getattr(policy, "overdue_mult", 1.25) or 1.25)
        age = (now - last_visit).total_seconds()
        dry_waves = int((entry.admission_decision or {}).get("dry_waves") or 0)
        dry_cap = int(getattr(policy, "dry_defer_waves", 0) or 0)
        if dry_cap > 0 and dry_waves >= dry_cap:
            return 3
        if floor_s > 0 and age >= floor_s * overdue_mult:
            return 1
        if floor_s > 0 and age >= floor_s:
            return 1
        return 2

    def _visit_timebox_expired(
        self, entry: SchedulerQueueEntry, now: datetime
    ) -> bool:
        last_visit = self._last_visit_at(entry)
        timebox = float(
            (entry.admission_decision or {}).get("visit_timebox_s") or 0
        )
        if last_visit is None or timebox <= 0:
            return False
        return (now - last_visit).total_seconds() >= timebox

    @staticmethod
    def _policy_challenge_ids(
        policy: CompetitionPolicy, field: str
    ) -> frozenset[str]:
        return frozenset(
            str(item).strip()
            for item in (getattr(policy, field, None) or [])
            if str(item).strip()
        )

    def _is_persistent_challenge(
        self,
        challenge: CompetitionChallenge,
        policy: CompetitionPolicy,
    ) -> bool:
        return challenge.external_challenge_id in self._policy_challenge_ids(
            policy, "persistent_challenge_ids"
        )

    def _fill_idle_revisit(
        self,
        competition: Competition,
        policy: CompetitionPolicy,
        capacity: SchedulerCapacity,
        challenge: CompetitionChallenge,
    ) -> bool:
        """平台允许时，用已访问题填满真实空槽。

        只有所有开放的非最终阶段题都至少访问过一次后才生效，避免复访题
        抢占首轮题目。最终阶段题仍由 terminal_phase gate 单独控制。
        """
        if not bool(getattr(policy, "fill_idle_revisits", False)):
            return False
        if self._is_persistent_challenge(challenge, policy):
            return False
        active_runs = (
            capacity.active_runs
            if capacity.active_runs is not None
            else self._active_run_count(competition.competition_id)
        )
        if policy.max_concurrent_runs > 0 and active_runs >= policy.max_concurrent_runs:
            return False
        active_instances = (
            capacity.active_instances
            if capacity.active_instances is not None
            else self._active_instance_count(competition.competition_id)
        )
        if policy.max_instances > 0 and active_instances >= policy.max_instances:
            return False
        terminal_ids = self._policy_challenge_ids(
            policy, "terminal_phase_challenge_ids"
        )
        for other in self._store.list(
            CompetitionChallenge, competition_id=competition.competition_id
        ):
            if other.external_challenge_id in terminal_ids or not self._challenge_open(other):
                continue
            binding = self._store.active_binding_for_challenge(other.challenge_id)
            if self._binding_occupies(binding):
                continue
            entry = self._store.get(
                SchedulerQueueEntry, competition.competition_id, other.challenge_id
            )
            if entry is None or int(
                (entry.admission_decision or {}).get("visit_count") or 0
            ) <= 0:
                return False
        return True

    def _terminal_phase_blockers(
        self,
        challenge: CompetitionChallenge,
        policy: CompetitionPolicy,
    ) -> list[str]:
        """返回阻止最终阶段题准入的非最终阶段题目。"""
        terminal_ids = self._policy_challenge_ids(
            policy, "terminal_phase_challenge_ids"
        )
        if challenge.external_challenge_id not in terminal_ids:
            return []
        blockers: list[str] = []
        for other in self._store.list(
            CompetitionChallenge, competition_id=challenge.competition_id
        ):
            if other.external_challenge_id in terminal_ids:
                continue
            if other.tombstoned or other.remote_state in {
                "closed", "solved_remote", "hidden",
            }:
                continue
            if other.challenge_state() in _TERMINAL_CHALLENGE_STATES:
                continue
            if bool(getattr(policy, "terminal_phase_fill_idle", False)):
                entry = self._store.get(
                    SchedulerQueueEntry,
                    challenge.competition_id,
                    other.challenge_id,
                )
                if entry is not None and int(
                    (entry.admission_decision or {}).get("visit_count") or 0
                ) > 0:
                    continue
            blockers.append(other.external_challenge_id or other.challenge_id)
        return sorted(blockers)

    async def _enforce_terminal_phase_gate(
        self,
        competition: Competition,
        policy: CompetitionPolicy,
        now: datetime,
    ) -> None:
        """策略变更后收回提前启动的最终阶段题目。"""
        terminal_ids = self._policy_challenge_ids(
            policy, "terminal_phase_challenge_ids"
        )
        if not terminal_ids:
            return
        failures: list[str] = []
        for challenge in self._store.list(
            CompetitionChallenge, competition_id=competition.competition_id
        ):
            if challenge.external_challenge_id not in terminal_ids:
                continue
            if not self._terminal_phase_blockers(challenge, policy):
                continue
            binding = self._store.active_binding_for_challenge(
                challenge.challenge_id)
            if (
                binding is not None
                and binding.binding_state() is BindingState.ACTIVE
            ):
                if self._binding is None:
                    failures.append(
                        f"{challenge.external_challenge_id}: binding service unavailable"
                    )
                    continue
                try:
                    await self._binding.pause(
                        challenge.challenge_id,
                        reason="terminal_phase_wait",
                    )
                except Exception as exc:
                    failures.append(
                        f"{challenge.external_challenge_id}: pause: "
                        f"{type(exc).__name__}: {exc}"
                    )
                    continue
            lease = self._store.active_lease_for_challenge(
                challenge.challenge_id)
            if lease is not None:
                if self._leases is None:
                    failures.append(
                        f"{challenge.external_challenge_id}: lease manager unavailable"
                    )
                    continue
                try:
                    await self._leases.release(
                        challenge_id=challenge.challenge_id,
                        reason="terminal_phase_wait",
                    )
                except Exception as exc:
                    failures.append(
                        f"{challenge.external_challenge_id}: release: "
                        f"{type(exc).__name__}: {exc}"
                    )
                    continue
            entry = self._store.get(
                SchedulerQueueEntry,
                competition.competition_id,
                challenge.challenge_id,
            )
            if entry is not None and entry.state == QueueEntryState.DONE.value:
                self._requeue_visit(entry, challenge, now)
        if failures:
            raise RuntimeError(
                "terminal phase enforcement failed: " + "; ".join(failures)
            )

    def _binding_occupies(self, binding: Optional[RunBinding]) -> bool:
        return (
            binding is not None
            and binding.binding_state() in _RUN_SLOT_STATES
        )

    def _challenge_open(self, challenge: CompetitionChallenge) -> bool:
        return (
            not challenge.tombstoned
            and challenge.remote_state == "open"
            and challenge.challenge_state() not in _TERMINAL_CHALLENGE_STATES
        )

    def _has_open_single_flag(self, competition_id: str) -> bool:
        for challenge in self._store.list(
            CompetitionChallenge, competition_id=competition_id
        ):
            if not self._challenge_open(challenge):
                continue
            if not self._is_multi_flag(challenge):
                return True
        return False

    def _occupying_multi_flag(
        self,
        competition_id: str,
        policy: Optional[CompetitionPolicy] = None,
    ) -> list[RunBinding]:
        found: list[RunBinding] = []
        for binding in self._store.list(RunBinding, competition_id=competition_id):
            if not self._binding_occupies(binding):
                continue
            challenge = self._store.get(
                CompetitionChallenge, binding.competition_challenge_id)
            if challenge is None or not self._is_multi_flag(challenge):
                continue
            if policy is not None and self._is_persistent_challenge(
                challenge, policy
            ):
                continue
            found.append(binding)
        return found

    def _deepchain_allowance(self, policy: CompetitionPolicy, competition_id: str) -> int:
        slots = int(getattr(policy, "deepchain_slots", 0) or 0)
        if slots <= 0:
            return 0
        if self._has_open_single_flag(competition_id):
            return slots
        return max(slots, 2)

    def _deepchain_hold(
        self,
        challenge: CompetitionChallenge,
        revision: Optional[ChallengeRevision],
        policy: CompetitionPolicy,
    ) -> bool:
        if not self._is_tsec(policy) or not self._is_multi_flag(challenge, revision):
            return False
        if self._is_persistent_challenge(challenge, policy):
            return False
        allowed = self._deepchain_allowance(policy, challenge.competition_id)
        if allowed <= 0:
            return False
        occupying = self._occupying_multi_flag(
            challenge.competition_id, policy)
        return len(occupying) >= allowed

    def _requeue_visit(
        self,
        entry: SchedulerQueueEntry,
        challenge: CompetitionChallenge,
        now: datetime,
    ) -> SchedulerQueueEntry:
        decision = dict(entry.admission_decision or {})
        current = len(self._store.list(
            SubmissionCandidate,
            competition_challenge_id=challenge.challenge_id,
        ))
        previous = int(decision.get("candidate_count_at_visit") or 0)
        dry = int(decision.get("dry_waves") or 0)
        decision["dry_waves"] = dry + 1 if current <= previous else 0
        decision["harvested_at"] = now.isoformat()
        updated = entry.model_copy(update={
            "state": QueueEntryState.QUEUED.value,
            "admission_decision": decision,
        })
        return self._store.save(updated)

    async def _harvest_finished_visits(
        self,
        competition: Competition,
        policy: CompetitionPolicy,
        now: datetime,
    ) -> None:
        """时间盒到期或 visit 已结束：暂停占用席位并重新入队。"""
        if not self._is_tsec(policy):
            return
        store = self._store
        failures: list[str] = []
        for entry in store.list(
            SchedulerQueueEntry, competition_id=competition.competition_id
        ):
            if entry.state != QueueEntryState.DONE.value:
                continue
            challenge = store.get(
                CompetitionChallenge, entry.competition_challenge_id)
            if challenge is None or not self._challenge_open(challenge):
                continue
            if self._is_persistent_challenge(challenge, policy):
                continue
            binding = store.active_binding_for_challenge(challenge.challenge_id)
            occupying = self._binding_occupies(binding)
            expired = self._visit_timebox_expired(entry, now)
            if occupying and not expired:
                continue
            if occupying and expired:
                if (
                    self._binding is None
                    or binding is None
                    or binding.binding_state() is not BindingState.ACTIVE
                ):
                    continue
                try:
                    await self._binding.pause(
                        challenge.challenge_id, reason="visit_timebox")
                except Exception as exc:
                    failures.append(
                        f"{challenge.external_challenge_id}: pause: "
                        f"{type(exc).__name__}: {exc}")
                    continue
                binding = store.active_binding_for_challenge(
                    challenge.challenge_id)
                if self._binding_occupies(binding):
                    continue
            lease = store.active_lease_for_challenge(challenge.challenge_id)
            if lease is not None:
                if self._leases is None:
                    failures.append(
                        f"{challenge.external_challenge_id}: lease manager unavailable")
                    continue
                try:
                    await self._leases.release(
                        challenge_id=challenge.challenge_id,
                        reason="visit_timebox",
                    )
                except Exception as exc:
                    failures.append(
                        f"{challenge.external_challenge_id}: release: "
                        f"{type(exc).__name__}: {exc}")
                    continue
            self._requeue_visit(entry, challenge, now)
        if failures:
            raise RuntimeError("visit harvest failed: " + "; ".join(failures))

    async def _enforce_deepchain(
        self,
        competition: Competition,
        policy: CompetitionPolicy,
    ) -> None:
        """多 flag 深链超额时暂停最不宜占席的 ACTIVE binding。"""
        if not self._is_tsec(policy) or self._binding is None:
            return
        allowed = self._deepchain_allowance(policy, competition.competition_id)
        if allowed <= 0:
            return
        occupying = self._occupying_multi_flag(
            competition.competition_id, policy)
        extras = len(occupying) - allowed
        if extras <= 0:
            return
        ranked = sorted(
            occupying,
            key=lambda binding: self._deepchain_evict_key(binding),
        )
        now = utcnow()
        failures: list[str] = []
        for binding in ranked[:extras]:
            if binding.binding_state() is not BindingState.ACTIVE:
                continue
            challenge = self._store.get(
                CompetitionChallenge, binding.competition_challenge_id)
            if challenge is None:
                continue
            try:
                await self._binding.pause(
                    challenge.challenge_id, reason="deepchain_quota")
                if (
                    self._leases is not None
                    and self._store.active_lease_for_challenge(
                        challenge.challenge_id) is not None
                ):
                    await self._leases.release(
                        challenge_id=challenge.challenge_id,
                        reason="deepchain_quota",
                    )
            except Exception as exc:
                failures.append(
                    f"{challenge.external_challenge_id}: {type(exc).__name__}: {exc}")
                continue
            entry = self._store.get(
                SchedulerQueueEntry,
                competition.competition_id,
                challenge.challenge_id,
            )
            if entry is not None and entry.state == QueueEntryState.DONE.value:
                self._requeue_visit(entry, challenge, now)
        if failures:
            raise RuntimeError("deepchain enforcement failed: " + "; ".join(failures))

    def _deepchain_evict_key(self, binding: RunBinding) -> tuple:
        """先清空转 hard，临门题最后。"""
        challenge = self._store.get(
            CompetitionChallenge, binding.competition_challenge_id)
        revision = (
            self._store.get(ChallengeRevision, challenge.current_revision_id)
            if challenge and challenge.current_revision_id else None
        )
        near = 1
        if challenge is not None and challenge.state in (
            ChallengeState.CANDIDATE_FOUND.value,
            ChallengeState.SUBMITTING.value,
        ):
            near = 0
        candidates = 0
        if challenge is not None:
            candidates = len(self._store.list(
                SubmissionCandidate,
                competition_challenge_id=challenge.challenge_id,
            ))
            if candidates > 0:
                near = 0
        diff = difficulty_rank(
            difficulty_of(
                challenge.category if challenge else "",
                challenge.name if challenge else "",
            )
        )
        return (near, -diff, candidates, binding.updated_at.isoformat())

    def _advance_challenge_to_running(
        self,
        challenge: CompetitionChallenge,
        binding: RunBinding,
    ) -> None:
        """首次准入推进到 running；续访保留 candidate_found / submitting。"""
        store = self._store
        current = challenge.challenge_state()
        if current in {
            ChallengeState.RUNNING,
            ChallengeState.CANDIDATE_FOUND,
            ChallengeState.SUBMITTING,
        }:
            return
        for target in (
            ChallengeState.PROVISIONING,
            ChallengeState.DISPATCHING,
            ChallengeState.RUNNING,
        ):
            if current is target:
                continue
            ensure_challenge_transition(current, target)
            challenge = challenge.model_copy(
                update={"state": target.value, "paused_from": None})
            store.save(challenge)
            store.append_events([ev.make_event(
                competition_id=challenge.competition_id,
                aggregate_type=ev.AGG_CHALLENGE,
                aggregate_id=challenge.challenge_id,
                event_type=ev.CHALLENGE_STATE_CHANGED,
                payload={
                    "challenge_id": challenge.challenge_id,
                    "from": current.value,
                    "to": target.value,
                    "reason": "scheduler.admit",
                    "run_id": binding.run_id,
                },
            )])
            current = target

    # -- Operator 手工排队（COMP-01 命令之外的辅助入口） --------------------------------

    def enqueue_manual(
        self,
        competition_id: str,
        challenge_id: str,
        *,
        priority: float = 0.0,
        not_before: Optional[datetime] = None,
    ) -> SchedulerQueueEntry:
        """Operator 手工排队 / 固定优先级设置（三档均最高优先级）。

        正式路径是 ``challenge.queue`` 命令（COMP-01 Handler，带 receipt /
        事件）；本方法供调度层内部与测试直接操作队列条目，状态机推进仍由
        命令 Handler 或 ``_auto_enqueue`` 完成。
        """
        store = self._store
        if store.get(CompetitionChallenge, challenge_id) is None:
            raise NotFoundError(f"challenge not found: {challenge_id}")
        entry = store.get(SchedulerQueueEntry, competition_id, challenge_id)
        entry = (entry or SchedulerQueueEntry(
            competition_id=competition_id,
            competition_challenge_id=challenge_id,
        )).model_copy(update={
            "state": QueueEntryState.QUEUED.value,
            "priority": float(priority),
            "not_before": not_before,
        })
        return store.save(entry)


__all__ = [
    "ACTION_ADMIT",
    "ACTION_DEFER",
    "ACTION_HOLD",
    "ACTION_OBSERVE",
    "ADMISSION_DECIDED",
    "AUTOMATION_CHANGED",
    "BUDGET_EXHAUSTED",
    "BUDGET_KINDS",
    "DISPATCH_PAUSED",
    "AdmissionDecision",
    "CompetitionScheduler",
    "RuleOutcome",
    "ScheduleReport",
    "SchedulerCapacity",
]
