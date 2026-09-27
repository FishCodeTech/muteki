"""InstanceLeaseManager：动态实例租约的生命周期、配额、fencing 与重启 reconcile
（任务书 10.6 / 设计 9.2、9.3 规则 7、9.5，COMP-07）。

职责与边界：

- **状态机**：``requested → provisioning → active → renewing → active``，
  分支 ``releasing → released``、``failed`` / ``expired`` / ``lost``；转移校验
  复用 ``models.ensure_lease_transition``（COMP-01 冻结的转移表）。
- **TTL / renew deadline**：租约携带 ``expires_at`` 与
  ``renew_deadline_at``（= expires_at − renew_skew）；``maintenance`` 在
  deadline 前自动续租，到期未续上的做幂等释放并落 ``expired``。
- **fencing**：同一 (connection_id, platform_instance_id) 的 fencing token
  由 ``CompetitionStore.save_lease`` 强制单调（COMP-01）；本层在
  ``assert_fencing`` / ``renew`` / ``release`` 上再做旧执行代拒绝——地址或
  实例硬变化后 lease token（实例更换时）与 binding execution_generation
  同时前进，旧 generation 的 Worker 不能继续提交或写新实例状态。
- **地址硬变化**：续租/探测发现地址或平台实例 id 变化且题目未解出时，经
  ``RunBindingService.resolve_execution`` 触发同一 Run 的新 execution
  generation（RESOLVING→ACTIVE，generation+1，重新编译 lease 投影）。
- **重启 reconcile**：对所有活动租约做平台探测（``probe_instance``，缺失时
  退化为 ``renew_instance``）；结果未知时经 ``RunBindingService.pause``
  把 binding 降级为 paused 并暂停新操作（设计 9.2）。
- **释放幂等**：平台返回 NOT_FOUND 视为已释放；回执落在
  ``LEASE_STATE_CHANGED`` 事件 payload 的 ``release_receipt`` 字段。
- **完整输出**：实例地址与凭据引用会出现在 ``InstanceLease`` 与 Run 执行上下文
  （编译进 ``Challenge.target`` / gateway 载荷）；事件 payload 一律不含
  ``address`` / ``credential_ref``（SSE / 公开事件只读事件日志）。

配额：每场 ``CompetitionPolicy.max_instances`` 与连接能力
``capabilities[detail].max_instances`` 两道闸；超限拒绝并写
``competition.instance.quota_rejected`` 事件，不落租约行。
"""

from __future__ import annotations

import asyncio
from dataclasses import dataclass, field
from datetime import datetime, timedelta
from typing import Any, Callable, Mapping, Optional

from muteki.competition import events as ev
from muteki.competition.binding import RunBindingService
from muteki.competition.models import (
    LEASE_ACTIVE_STATES,
    BindingState,
    ChallengeState,
    Competition,
    CompetitionChallenge,
    CompetitionPolicy,
    InstanceLease,
    LeaseState,
    PlatformConnection,
    RunBinding,
    ensure_lease_transition,
)
from muteki.competition.platforms.base import (
    PlatformNotFoundError,
    PlatformTransportError,
)
from muteki.competition.store import (
    CompetitionStore,
    NotFoundError,
    StateConflictError,
)
from muteki.platform.contracts.base import utcnow
from muteki.platform.contracts.modules import (
    InstanceLeaseRef,
    InstanceResult,
    PlatformChallengeRef,
)


class LeaseQuotaExceededError(RuntimeError):
    """实例配额超限：获取被拒绝（已写 quota_rejected 事件，未落租约行）。"""

    def __init__(self, scope: str, limit: int, active: int) -> None:
        super().__init__(
            f"instance quota exceeded ({scope}): active={active} limit={limit}"
        )
        self.scope = scope
        self.limit = limit
        self.active = active


@dataclass(frozen=True)
class LeaseManagerConfig:
    """租约管理参数。"""

    default_ttl_seconds: int = 3600   # 平台未给 expires_at 时的兜底 TTL
    renew_skew_seconds: float = 60.0  # renew deadline = expires_at - skew


@dataclass
class ReconcileReport:
    """一次启动 reconcile 的结果汇总（设计 9.5 的租约步骤）。"""

    confirmed: list[str] = field(default_factory=list)   # lease_id：探测确认 active
    adopted: list[str] = field(default_factory=list)     # lease_id：provisioning 被收养
    lost: list[str] = field(default_factory=list)        # lease_id：平台侧已不存在
    released: list[str] = field(default_factory=list)    # lease_id：releasing 收尾完成
    failed: list[str] = field(default_factory=list)      # lease_id：从未发出的请求收尾
    degraded: list[str] = field(default_factory=list)    # challenge_id：未知 → 降级
    paused: list[str] = field(default_factory=list)      # challenge_id：终态租约释放槽位


class InstanceLeaseManager:
    """动态实例租约管理器（任务书 10.6）。

    - ``store``：CompetitionStore（唯一持久化入口）。
    - ``adapters``：platform_kind → PlatformAdapter 映射，或用
      ``adapter_for`` 按连接解析；Adapter 只需实现契约的
      acquire/renew/release_instance，可选 ``probe_instance``。
    - ``binding_service``：RunBindingService；提供时地址硬变化触发新
      execution generation，reconcile 未知时降级 binding。为 None 时只
      维护租约自身状态（单测 / 无 Run 场景）。
    """

    def __init__(
        self,
        store: CompetitionStore,
        adapters: Optional[Mapping[str, Any]] = None,
        binding_service: Optional[RunBindingService] = None,
        *,
        adapter_for: Optional[Callable[[PlatformConnection], Any]] = None,
        config: Optional[LeaseManagerConfig] = None,
        now: Optional[Callable[[], datetime]] = None,
    ) -> None:
        self._store = store
        self._adapters = dict(adapters or {})
        self._binding = binding_service
        self._adapter_for = adapter_for
        self._config = config or LeaseManagerConfig()
        self._now = now or utcnow
        self._release_locks: dict[str, asyncio.Lock] = {}

    # ------------------------------------------------------------------
    # 获取（requested → provisioning → active；配额闸 + fencing token）
    # ------------------------------------------------------------------

    async def acquire(self, challenge_id: str, *, owner: str = "") -> InstanceLease:
        """为题目获取动态实例；已有活动租约时幂等返回。

        配额超限抛 ``LeaseQuotaExceededError``（事件已记录）；平台错误落
        ``failed`` 后原样抛出。成功后若活动 binding 已在可 resolve 状态
        （运行中的旧输入不含本实例地址），触发新 execution generation。
        """
        challenge = self._challenge(challenge_id)
        existing = self._store.active_lease_for_challenge(challenge_id)
        if existing is not None:
            self._link_binding(challenge_id, existing)
            return existing
        competition_id = challenge.competition_id
        connection = self._connection(challenge)
        adapter = self._adapter(connection)
        self._check_quota(challenge, connection)

        lease = InstanceLease(
            connection_id=connection.connection_id,
            competition_id=competition_id,
            competition_challenge_id=challenge.challenge_id,
            owner=owner,
        )
        self._store.save(lease)
        self._append_lease_event(lease, ev.LEASE_REQUESTED, {})
        lease = self._transition(lease, LeaseState.PROVISIONING)
        self._store.save(lease)
        try:
            result = await adapter.acquire_instance(PlatformChallengeRef(
                connection_id=connection.connection_id,
                challenge_key=challenge.external_challenge_id,
            ))
        except PlatformTransportError as exc:
            lease = self._transition(
                lease, LeaseState.FAILED, {"error": str(exc)}
            )
            self._store.save(lease.model_copy(update={"last_error": str(exc)}))
            raise
        lease = self._apply_result(lease, result, adopt=True)
        # 平台效果已经发生。先保存远端实例身份和 fencing 信息，使进程在
        # active 状态落盘前终止时，重启 reconcile 仍可探测并收养该实例。
        lease = self._store.save_lease(lease)
        lease = self._transition(lease, LeaseState.ACTIVE)
        lease = self._store.save_lease(lease)
        binding = self._link_binding(challenge_id, lease)
        await self._maybe_resolve(challenge_id, binding, reason="instance_acquired")
        return lease

    async def provision_requested(self, lease_id: str) -> InstanceLease:
        """消费 instance.ensure 命令已持久化的 requested 租约。

        Command Handler 必须先把租约与 outbox 原子落库；consumer 随后调用
        本方法执行真实平台请求。它与 ``acquire`` 共用状态机和地址硬变化
        处理，避免看到已有 requested 租约后直接幂等返回、遗漏远端效果。
        """
        lease = self._lease(lease_id)
        if lease.lease_state() is not LeaseState.REQUESTED:
            return lease
        challenge = self._challenge(lease.competition_challenge_id)
        connection = self._connection(challenge)
        adapter = self._adapter(connection)
        try:
            self._check_quota(
                challenge, connection, exclude_lease_id=lease.lease_id)
        except LeaseQuotaExceededError as exc:
            # instance.ensure 已经先落 requested。配额拒绝属于本次申请的
            # 终态，必须释放它对后续申请的占位，否则并发 ensure 会互相
            # 计数并留下永久 requested 僵尸。
            lease = self._transition(
                lease, LeaseState.FAILED, {"error": str(exc)})
            self._store.save_lease(
                lease.model_copy(update={"last_error": str(exc)}))
            raise
        lease = self._transition(lease, LeaseState.PROVISIONING)
        self._store.save_lease(lease)
        try:
            result = await adapter.acquire_instance(PlatformChallengeRef(
                connection_id=connection.connection_id,
                challenge_key=challenge.external_challenge_id,
            ))
        except PlatformTransportError as exc:
            lease = self._transition(
                lease, LeaseState.FAILED, {"error": str(exc)})
            self._store.save_lease(lease.model_copy(update={"last_error": str(exc)}))
            raise
        lease = self._apply_result(lease, result, adopt=True)
        # 与 acquire 保持相同的恢复边界：平台回执中的实例身份先落盘。
        lease = self._store.save_lease(lease)
        lease = self._transition(lease, LeaseState.ACTIVE)
        lease = self._store.save_lease(lease)
        binding = self._link_binding(challenge.challenge_id, lease)
        await self._maybe_resolve(
            challenge.challenge_id, binding, reason="instance_acquired")
        return lease

    # ------------------------------------------------------------------
    # 续租（renewing → active；地址硬变化 → 新执行代）
    # ------------------------------------------------------------------

    async def renew(
        self,
        lease_id: str,
        *,
        expected_fencing_token: Optional[int] = None,
    ) -> InstanceLease:
        """续租一次。实例不存在落 ``lost``；其他平台错误回到 ``active``
        并记录 ``last_error`` 后抛出（由 maintenance 下 tick 重试）。

        ``expected_fencing_token`` 非空且与当前不一致时抛
        ``StateConflictError``：旧执行代的 Worker 不能写新实例状态。
        """
        lease = self._lease(lease_id)
        self._check_fencing_token(lease, expected_fencing_token)
        state = lease.lease_state()
        if state not in (LeaseState.ACTIVE, LeaseState.RENEWING):
            raise StateConflictError(
                f"lease {lease_id} is not renewable in state {state.value}"
            )
        if state is LeaseState.ACTIVE:
            lease = self._transition(lease, LeaseState.RENEWING)
            self._store.save_lease(lease)
        adapter = self._adapter(self._connection_by_id(lease.connection_id))
        try:
            result = await adapter.renew_instance(self._ref_for(lease))
        except PlatformNotFoundError:
            # 平台侧实例丢失（设计 9.2 的 lost 分支）。
            lease = self._transition(lease, LeaseState.LOST)
            return self._store.save_lease(lease)
        except PlatformTransportError as exc:
            lease = lease.model_copy(update={"last_error": str(exc)})
            lease = self._transition(lease, LeaseState.ACTIVE, {"error": str(exc)})
            self._store.save_lease(lease)
            raise
        hard = self._is_hard_change(lease, result)
        previous_expires_at = lease.expires_at
        lease = self._apply_result(lease, result, adopt=False)
        # Tsec 等平台的 renew_instance 实际是存活探测，固定 visit 的到期
        # 时间不会被延长。若回执没有把 expires_at 向后推进，就把下一次维护
        # 点放到到期时刻，避免 renew_deadline 过后每秒写
        # active→renewing→active 两条重复事件。
        if (
            previous_expires_at is not None
            and lease.expires_at is not None
            and lease.expires_at <= previous_expires_at
        ):
            lease = lease.model_copy(update={
                "renew_deadline_at": lease.expires_at,
            })
        lease = self._transition(lease, LeaseState.ACTIVE,
                                 {"hard_change": hard} if hard else {})
        lease = self._store.save_lease(lease)
        if hard:
            await self._maybe_resolve(
                lease.competition_challenge_id,
                self._store.active_binding_for_challenge(
                    lease.competition_challenge_id),
                reason="instance_hard_change",
            )
        return lease

    # ------------------------------------------------------------------
    # 释放（幂等；平台 404 视为已释放并保存回执）
    # ------------------------------------------------------------------

    async def release(
        self,
        lease_id: Optional[str] = None,
        *,
        challenge_id: Optional[str] = None,
        reason: str = "release",
        expected_fencing_token: Optional[int] = None,
    ) -> InstanceLease:
        """释放租约。终态幂等返回；平台 NOT_FOUND 视为已释放。

        回执（confirmed / platform_not_found / no_remote_instance）写入
        ``released`` 事件 payload 的 ``release_receipt`` 字段。
        """
        if lease_id is not None:
            lease = self._lease(lease_id)
        elif challenge_id is not None:
            found = self._store.active_lease_for_challenge(challenge_id)
            if found is None:
                raise NotFoundError(
                    f"no active lease for challenge {challenge_id}"
                )
            lease = found
        else:
            raise ValueError("release requires lease_id or challenge_id")
        lock = self._release_locks.setdefault(lease.lease_id, asyncio.Lock())
        async with lock:
            # Scheduler harvest and lease maintenance can observe the same
            # expiry concurrently. Re-read under one per-lease lock so only
            # one caller emits release_requested and contacts the platform.
            lease = self._lease(lease.lease_id)
            self._check_fencing_token(lease, expected_fencing_token)
            state = lease.lease_state()
            if state in (LeaseState.RELEASED, LeaseState.FAILED,
                         LeaseState.EXPIRED, LeaseState.LOST):
                return lease  # 终态幂等
            self._append_lease_event(lease, ev.LEASE_RELEASE_REQUESTED,
                                     {"reason": reason})
            if state is LeaseState.REQUESTED:
                # 平台尚未交付实例：无需外部释放调用（模型转移表允许）。
                lease = self._transition(
                    lease, LeaseState.RELEASED,
                    {"release_receipt": "no_remote_instance"})
                return self._store.save(lease)
            if state is not LeaseState.RELEASING:
                lease = self._transition(lease, LeaseState.RELEASING)
                self._store.save_lease(lease)
            adapter = self._adapter(self._connection_by_id(lease.connection_id))
            try:
                await adapter.release_instance(self._ref_for(lease))
                receipt = "confirmed"
            except PlatformNotFoundError:
                receipt = "platform_not_found"  # 幂等：不存在视为已释放
            except PlatformTransportError as exc:
                lease = self._transition(
                    lease, LeaseState.FAILED, {"error": str(exc)}
                )
                self._store.save_lease(lease.model_copy(
                    update={"last_error": str(exc)}))
                raise
            lease = self._transition(
                lease, LeaseState.RELEASED, {"release_receipt": receipt})
            return self._store.save_lease(lease)

    # ------------------------------------------------------------------
    # 到期自动续租与到期释放（renew deadline 前续租；到期未续上则释放）
    # ------------------------------------------------------------------

    async def maintenance(
        self,
        *,
        competition_id: Optional[str] = None,
    ) -> list[InstanceLease]:
        """一轮租约维护：到期续租（renew deadline 前）与到期释放。

        返回本轮状态发生变化的租约。续租的平台错误（非 lost）只记录
        ``last_error`` 并留待下一轮；越过 ``expires_at`` 仍未续上的做
        幂等释放并落 ``expired``。
        """
        now = self._now()
        changed: list[InstanceLease] = []
        for lease in self._active_leases(competition_id):
            state = lease.lease_state()
            if state not in (LeaseState.ACTIVE, LeaseState.RENEWING):
                continue
            if (lease.renew_deadline_at is not None
                    and now >= lease.renew_deadline_at):
                try:
                    lease = await self.renew(lease.lease_id)
                except PlatformTransportError:
                    lease = self._lease(lease.lease_id)  # last_error 已落库
                if lease.lease_state() is not state:
                    changed.append(lease)
            if (lease.lease_state() in (LeaseState.ACTIVE, LeaseState.RENEWING)
                    and lease.expires_at is not None
                    and now >= lease.expires_at):
                changed.append(await self._expire(lease))
        await self._pause_bindings_without_lease(competition_id=competition_id)
        return changed

    async def _expire(self, lease: InstanceLease) -> InstanceLease:
        """到期释放：平台侧实例已失效，幂等释放后落 ``expired``。"""
        adapter = self._adapter(self._connection_by_id(lease.connection_id))
        error = ""
        try:
            await adapter.release_instance(self._ref_for(lease))
        except PlatformNotFoundError:
            pass  # 实例已不存在：与到期语义一致
        except PlatformTransportError as exc:
            error = str(exc)  # 释放失败不阻塞到期收尾，留 last_error 供审计
        lease = self._transition(
            lease, LeaseState.EXPIRED,
            {"release_receipt": "expired", **({"error": error} if error else {})})
        return self._store.save_lease(
            lease.model_copy(update={"last_error": error or lease.last_error}))

    # ------------------------------------------------------------------
    # 重启 reconcile（设计 9.5：先探测所有活动租约，再恢复调度）
    # ------------------------------------------------------------------

    async def reconcile(
        self,
        *,
        competition_id: Optional[str] = None,
    ) -> ReconcileReport:
        """启动恢复：探测所有活动租约的平台真实状态。

        - ``requested``：崩溃发生在平台调用之前（provisioning 才会发出
          调用），直接收尾为 ``failed``；
        - ``provisioning``：调用可能已发出、结果未知 → 探测；确认存在则
          收养为 ``active``，不存在落 ``failed``，未知则降级；
        - ``active`` / ``renewing``：探测确认归一化为 ``active``（地址硬
          变化触发新执行代），不存在落 ``lost``，未知则降级；
        - ``releasing``：重放幂等释放（404 → released + 回执），失败降级。
        降级 = 写 ``reconcile_degraded`` 事件并把活动 binding 暂停
        （paused），恢复调度前不再对其发起新操作。
        """
        report = ReconcileReport()
        for lease in self._active_leases(competition_id):
            state = lease.lease_state()
            if state is LeaseState.REQUESTED:
                lease = self._transition(
                    lease, LeaseState.FAILED,
                    {"error": "restart reconcile: acquire never dispatched"})
                self._store.save(lease.model_copy(update={
                    "last_error": "restart reconcile: acquire never dispatched",
                }))
                report.failed.append(lease.lease_id)
            elif state is LeaseState.PROVISIONING:
                await self._reconcile_provisioning(lease, report)
            elif state in (LeaseState.ACTIVE, LeaseState.RENEWING):
                await self._reconcile_active(lease, report)
            elif state is LeaseState.RELEASING:
                try:
                    await self.release(lease.lease_id, reason="restart_reconcile")
                    report.released.append(lease.lease_id)
                except PlatformTransportError:
                    await self._mark_degraded(lease, "release_replay_failed")
                    report.degraded.append(lease.competition_challenge_id)
        report.paused.extend(await self._pause_bindings_without_lease(
            competition_id=competition_id))
        return report

    async def _pause_bindings_without_lease(
        self,
        *,
        competition_id: Optional[str] = None,
    ) -> list[str]:
        """终态租约不能留下继续占求解席位的 ACTIVE binding。"""
        if self._binding is None:
            return []
        paused: list[str] = []
        failures: list[str] = []
        for binding in self._store.list(RunBinding):
            if competition_id and binding.competition_id != competition_id:
                continue
            if binding.binding_state() is not BindingState.ACTIVE:
                continue
            challenge_id = binding.competition_challenge_id
            if self._store.active_lease_for_challenge(challenge_id) is not None:
                continue
            # 只有曾经持有动态实例的 binding 才由租约管理器收敛；静态题不受
            # 影响。requested/provisioning 仍属于活动租约，不会走到这里。
            if not self._store.list(
                InstanceLease, competition_challenge_id=challenge_id
            ):
                continue
            try:
                updated = await self._binding.pause(
                    challenge_id, reason="instance_lease_terminal")
            except Exception as exc:
                failures.append(
                    f"{challenge_id}: {type(exc).__name__}: {exc}")
                continue
            if updated is not None:
                paused.append(challenge_id)
        if failures:
            raise RuntimeError(
                "failed to pause bindings after terminal lease: "
                + "; ".join(failures)
            )
        return paused

    async def _reconcile_provisioning(
        self, lease: InstanceLease, report: ReconcileReport
    ) -> None:
        outcome, result = await self._probe(lease)
        if outcome == "active" and result is not None:
            lease = self._apply_result(lease, result, adopt=True)
            lease = self._transition(lease, LeaseState.ACTIVE,
                                     {"reconciled": "adopted"})
            lease = self._store.save_lease(lease)
            binding = self._link_binding(
                lease.competition_challenge_id, lease)
            await self._maybe_resolve(
                lease.competition_challenge_id, binding,
                reason="instance_reconcile_adopted")
            report.adopted.append(lease.lease_id)
            await self._release_after_terminal_challenge(lease, report)
        elif outcome == "lost":
            lease = self._transition(lease, LeaseState.FAILED,
                                     {"reconciled": "not_found"})
            self._store.save(lease)
            report.failed.append(lease.lease_id)
        else:
            await self._mark_degraded(lease, "probe_unknown")
            report.degraded.append(lease.competition_challenge_id)

    async def _reconcile_active(
        self, lease: InstanceLease, report: ReconcileReport
    ) -> None:
        if lease.platform_instance_id:
            newest_token = self._store.next_fencing_token(
                lease.connection_id, lease.platform_instance_id) - 1
            if newest_token > lease.fencing_token:
                lease = self._transition(
                    lease,
                    LeaseState.LOST,
                    {
                        "reconciled": "superseded_fencing",
                        "superseded_by_fencing_token": newest_token,
                    },
                )
                self._store.save_lease(lease)
                report.lost.append(lease.lease_id)
                return
        outcome, result = await self._probe(lease)
        if outcome == "active" and result is not None:
            hard = self._is_hard_change(lease, result)
            lease = self._apply_result(lease, result, adopt=False)
            if lease.lease_state() is LeaseState.RENEWING:
                lease = self._transition(lease, LeaseState.ACTIVE,
                                         {"reconciled": "confirmed"})
            lease = self._store.save_lease(lease)
            if hard:
                await self._maybe_resolve(
                    lease.competition_challenge_id,
                    self._store.active_binding_for_challenge(
                        lease.competition_challenge_id),
                    reason="instance_hard_change")
            report.confirmed.append(lease.lease_id)
            await self._release_after_terminal_challenge(lease, report)
        elif outcome == "lost":
            lease = self._transition(lease, LeaseState.LOST,
                                     {"reconciled": "not_found"})
            self._store.save_lease(lease)
            report.lost.append(lease.lease_id)
        else:
            await self._mark_degraded(lease, "probe_unknown")
            report.degraded.append(lease.competition_challenge_id)

    async def _probe(
        self, lease: InstanceLease
    ) -> tuple[str, Optional[InstanceResult]]:
        """探测平台真实状态：("active", result) / ("lost", None) / ("unknown", None)。

        Adapter 暴露 ``probe_instance`` 时用之；否则退化为
        ``renew_instance``（顺带续期，语义与「TTL 成功续期只更新租约」一致）。
        """
        adapter = self._adapter(self._connection_by_id(lease.connection_id))
        probe = getattr(adapter, "probe_instance", None)
        ref = self._ref_for(lease)
        try:
            if probe is not None:
                result = await probe(ref)
            else:
                result = await adapter.renew_instance(ref)
        except PlatformNotFoundError:
            return "lost", None
        except PlatformTransportError:
            return "unknown", None
        if not isinstance(result, InstanceResult):
            return "unknown", None
        return "active", result

    async def _mark_degraded(self, lease: InstanceLease, reason: str) -> None:
        """结果未知：写降级事件；活动 binding 处于 active 时暂停新操作。"""
        self._append_lease_event(lease, ev.LEASE_RECONCILE_DEGRADED,
                                 {"reason": reason})
        if self._binding is None:
            return
        binding = self._store.active_binding_for_challenge(
            lease.competition_challenge_id)
        if binding is not None and binding.binding_state() is BindingState.ACTIVE:
            await self._binding.pause(
                lease.competition_challenge_id, reason=f"lease_{reason}")

    async def _release_after_terminal_challenge(
        self,
        lease: InstanceLease,
        report: ReconcileReport,
    ) -> None:
        """恢复确认远端实例后，释放已经结束题目的遗留实例。"""
        challenge = self._store.get(
            CompetitionChallenge, lease.competition_challenge_id)
        if challenge is None or challenge.state not in {
            ChallengeState.SOLVED.value,
            ChallengeState.SKIPPED.value,
            ChallengeState.EXHAUSTED.value,
            ChallengeState.FAILED.value,
            ChallengeState.RETIRED.value,
        }:
            return
        try:
            released = await self.release(
                lease.lease_id,
                reason=f"restart_reconcile_{challenge.state}",
            )
        except PlatformTransportError:
            await self._mark_degraded(lease, "terminal_release_failed")
            report.degraded.append(lease.competition_challenge_id)
            return
        if released.lease_state() is LeaseState.RELEASED:
            report.released.append(lease.lease_id)

    # ------------------------------------------------------------------
    # fencing：旧执行代拒绝写新实例状态 / 提交
    # ------------------------------------------------------------------

    def assert_fencing(
        self,
        challenge_id: str,
        *,
        lease_id: str,
        fencing_token: int,
        execution_generation: int,
    ) -> InstanceLease:
        """校验调用方仍持有当前租约与当前执行代；否则抛 ``StateConflictError``。

        地址/实例硬变化后：binding execution_generation 递增；平台实例
        更换时 fencing token 亦递增。旧 generation 的 Worker 携带旧
        (lease_id, fencing_token, execution_generation) 在此被拒绝，
        不能继续提交或写入新实例状态。
        """
        lease = self._store.active_lease_for_challenge(challenge_id)
        if lease is None:
            raise StateConflictError(
                f"no active lease for challenge {challenge_id}"
            )
        if lease.lease_id != lease_id or lease.fencing_token != fencing_token:
            raise StateConflictError(
                f"stale lease fencing: ({lease_id}, token={fencing_token}) vs "
                f"current ({lease.lease_id}, token={lease.fencing_token})"
            )
        binding = self._store.active_binding_for_challenge(challenge_id)
        current_gen = binding.execution_generation if binding is not None else None
        if current_gen != execution_generation:
            raise StateConflictError(
                f"stale execution generation: {execution_generation} vs "
                f"current {current_gen}"
            )
        return lease

    def _check_fencing_token(
        self, lease: InstanceLease, expected: Optional[int]
    ) -> None:
        if expected is not None and expected != lease.fencing_token:
            raise StateConflictError(
                f"stale fencing token for lease {lease.lease_id}: "
                f"{expected} != {lease.fencing_token}"
            )

    # ------------------------------------------------------------------
    # 配额（场次 policy + 连接能力两道闸；超限拒绝并记录）
    # ------------------------------------------------------------------

    def _check_quota(
        self,
        challenge: CompetitionChallenge,
        connection: PlatformConnection,
        *,
        exclude_lease_id: str = "",
    ) -> None:
        competition_id = challenge.competition_id
        policy = self._store.get(CompetitionPolicy, competition_id)
        if policy is not None and policy.max_instances > 0:
            active = sum(
                1 for lease in self._store.list(
                    InstanceLease, competition_id=competition_id)
                if lease.lease_state() in LEASE_ACTIVE_STATES
                and lease.lease_id != exclude_lease_id
            )
            if active >= policy.max_instances:
                self._reject_quota(challenge, "competition",
                                   policy.max_instances, active)
        max_conn = self._connection_max_instances(connection)
        if max_conn is not None and max_conn > 0:
            active = sum(
                1 for lease in self._store.list(
                    InstanceLease, connection_id=connection.connection_id)
                if lease.lease_state() in LEASE_ACTIVE_STATES
                and lease.lease_id != exclude_lease_id
            )
            if active >= max_conn:
                self._reject_quota(challenge, "connection", max_conn, active)

    def _reject_quota(
        self,
        challenge: CompetitionChallenge,
        scope: str,
        limit: int,
        active: int,
    ) -> None:
        # 配额证据只含计数与来源，不含地址/凭据（事件进 SSE 也安全）。
        self._store.append_events([ev.make_event(
            competition_id=challenge.competition_id,
            aggregate_type=ev.AGG_COMPETITION,
            aggregate_id=challenge.competition_id,
            event_type=ev.LEASE_QUOTA_REJECTED,
            payload={
                "challenge_id": challenge.challenge_id,
                "scope": scope,
                "limit": limit,
                "active": active,
            },
        )])
        raise LeaseQuotaExceededError(scope, limit, active)

    @staticmethod
    def _connection_max_instances(
        connection: PlatformConnection,
    ) -> Optional[int]:
        """连接能力里的平台实例配额（probe 写入 detail.max_instances）。"""
        caps = connection.capabilities or {}
        value = caps.get("max_instances")
        if value is None and isinstance(caps.get("detail"), dict):
            value = caps["detail"].get("max_instances")
        try:
            return int(value) if value is not None else None
        except (TypeError, ValueError):
            return None

    # ------------------------------------------------------------------
    # 内部：结果落租约 / 硬变化判定 / binding 联动
    # ------------------------------------------------------------------

    def _apply_result(
        self, lease: InstanceLease, result: InstanceResult, *, adopt: bool
    ) -> InstanceLease:
        """把平台结果写进租约字段（地址、TTL、deadline、token、generation）。

        ``adopt=True``（首次获取 / reconcile 收养）时 fencing token 取该
        平台实例的下一个值；实例 id 变化视为平台侧换了实例，token 重新取
        值且 ``generation`` 递增（同实例续租 token 不变）。
        """
        now = self._now()
        new_instance_id = result.lease.lease_id or lease.platform_instance_id
        address = self._pick_address(result.endpoints) or lease.address
        persistent = self._is_persistent_challenge(
            lease.competition_challenge_id)
        expires_at = None if persistent else (
            result.lease.expires_at
            or now + timedelta(seconds=self._config.default_ttl_seconds)
        )
        ttl = (
            0
            if expires_at is None
            else max(0, int((expires_at - now).total_seconds()))
        )
        skew = min(self._config.renew_skew_seconds, max(ttl / 2, 0))
        updates: dict[str, Any] = {
            "platform_instance_id": new_instance_id,
            "address": address,
            "ttl_seconds": (
                0 if persistent else ttl or self._config.default_ttl_seconds
            ),
            "expires_at": expires_at,
            "renew_deadline_at": (
                None
                if expires_at is None
                else expires_at - timedelta(seconds=skew)
            ),
            "last_error": "",
        }
        if adopt or new_instance_id != lease.platform_instance_id:
            updates["fencing_token"] = self._store.next_fencing_token(
                lease.connection_id, new_instance_id)
            next_generation = self._store.next_instance_generation(
                lease.connection_id,
                new_instance_id,
                exclude_lease_id=lease.lease_id,
            )
            updates["generation"] = (
                next_generation
                if adopt
                else max(lease.generation + 1, next_generation)
            )
        return lease.model_copy(update=updates)

    def _is_persistent_challenge(self, challenge_id: str) -> bool:
        challenge = self._store.get(CompetitionChallenge, challenge_id)
        if challenge is None:
            return False
        policy = self._store.get(CompetitionPolicy, challenge.competition_id)
        if policy is None:
            return False
        persistent_ids = {
            str(item).strip()
            for item in (getattr(policy, "persistent_challenge_ids", None) or [])
            if str(item).strip()
        }
        return challenge.external_challenge_id in persistent_ids

    @staticmethod
    def _is_hard_change(lease: InstanceLease, result: InstanceResult) -> bool:
        """地址或平台实例 id 变化 = 连接信息硬变化（设计 9.2）。"""
        if result.lease.lease_id and (
                result.lease.lease_id != lease.platform_instance_id):
            return True
        new_address = InstanceLeaseManager._pick_address(result.endpoints)
        return bool(new_address) and new_address != lease.address

    @staticmethod
    def _pick_address(endpoints: Mapping[str, str]) -> str:
        """endpoints → 单一地址投影；优先 http/https，其次按 key 序取第一。"""
        for key in ("http", "https", "tcp"):
            value = str(endpoints.get(key) or "").strip()
            if value:
                return value
        for key in sorted(endpoints):
            value = str(endpoints[key] or "").strip()
            if value:
                return value
        return ""

    def _link_binding(
        self, challenge_id: str, lease: InstanceLease
    ) -> Optional[Any]:
        """把活动 binding 的 lease_id 对齐到当前租约（不做状态转移）。"""
        binding = self._store.active_binding_for_challenge(challenge_id)
        if binding is not None and binding.lease_id != lease.lease_id:
            binding = self._store.save(
                binding.model_copy(update={"lease_id": lease.lease_id}))
        return binding

    async def _maybe_resolve(
        self,
        challenge_id: str,
        binding: Optional[Any],
        *,
        reason: str,
    ) -> None:
        """地址硬变化后触发同一 Run 的新 execution generation。

        仅当活动 binding 处于可 resolve 状态（active/rejected/paused）时
        触发；创建在途（planned/creating/starting）由 start 编译自然带上
        新地址，无需 resolve；无 binding / 终态则不动作。
        """
        if self._binding is None or binding is None:
            return
        if binding.binding_state() not in (
            BindingState.ACTIVE, BindingState.REJECTED, BindingState.PAUSED,
        ):
            return
        await self._binding.resolve_execution(challenge_id, reason=reason)

    # ------------------------------------------------------------------
    # 内部：读取 / 转移 / 事件（payload 不含地址与凭据）
    # ------------------------------------------------------------------

    def _challenge(self, challenge_id: str) -> CompetitionChallenge:
        challenge = self._store.get(CompetitionChallenge, challenge_id)
        if challenge is None:
            raise NotFoundError(f"challenge not found: {challenge_id}")
        if challenge.tombstoned:
            raise StateConflictError(
                f"challenge {challenge_id} is tombstoned; no instance ops"
            )
        return challenge

    def _lease(self, lease_id: str) -> InstanceLease:
        lease = self._store.get(InstanceLease, lease_id)
        if lease is None:
            raise NotFoundError(f"lease not found: {lease_id}")
        return lease

    def _connection(self, challenge: CompetitionChallenge) -> PlatformConnection:
        competition = self._store.get(Competition, challenge.competition_id)
        connection_id = competition.connection_id if competition else ""
        return self._connection_by_id(connection_id)

    def _connection_by_id(self, connection_id: str) -> PlatformConnection:
        connection = self._store.get(PlatformConnection, connection_id)
        if connection is None:
            raise NotFoundError(f"connection not found: {connection_id}")
        return connection

    def _adapter(self, connection: PlatformConnection) -> Any:
        if self._adapter_for is not None:
            return self._adapter_for(connection)
        adapter = self._adapters.get(connection.platform_kind)
        if adapter is None:
            raise NotFoundError(
                f"no platform adapter for kind {connection.platform_kind!r}"
            )
        return adapter

    def _ref_for(self, lease: InstanceLease) -> InstanceLeaseRef:
        """租约 → Adapter 的 InstanceLeaseRef（challenge_key 取远端题目 id）。"""
        challenge = self._store.get(
            CompetitionChallenge, lease.competition_challenge_id)
        return InstanceLeaseRef(
            lease_id=lease.platform_instance_id,
            connection_id=lease.connection_id,
            challenge_key=challenge.external_challenge_id if challenge else "",
            fencing_token=lease.fencing_token,
            expires_at=lease.expires_at,
        )

    def _active_leases(
        self, competition_id: Optional[str]
    ) -> list[InstanceLease]:
        leases = (self._store.list(InstanceLease, competition_id=competition_id)
                  if competition_id else self._store.list(InstanceLease))
        return [
            lease for lease in leases
            if lease.lease_state() in LEASE_ACTIVE_STATES
        ]

    def _transition(
        self,
        lease: InstanceLease,
        target: LeaseState,
        extra: Optional[dict[str, Any]] = None,
    ) -> InstanceLease:
        current = lease.lease_state()
        ensure_lease_transition(current, target)
        updated = lease.model_copy(update={"state": target.value})
        self._append_lease_event(updated, ev.LEASE_STATE_CHANGED, {
            "from": current.value,
            "to": target.value,
            **(extra or {}),
        })
        return updated

    def _append_lease_event(
        self,
        lease: InstanceLease,
        event_type: str,
        extra: dict[str, Any],
    ) -> None:
        """租约事件：payload 只含身份/状态/时间，绝不含地址与凭据引用。"""
        self._store.append_events([ev.make_event(
            competition_id=lease.competition_id,
            aggregate_type=ev.AGG_LEASE,
            aggregate_id=lease.lease_id,
            event_type=event_type,
            payload={
                "lease_id": lease.lease_id,
                "challenge_id": lease.competition_challenge_id,
                "state": lease.state,
                "platform_instance_id": lease.platform_instance_id,
                "generation": lease.generation,
                "fencing_token": lease.fencing_token,
                "owner": lease.owner,
                "expires_at": (lease.expires_at.isoformat()
                               if lease.expires_at else None),
                **extra,
            },
        )])


__all__ = [
    "InstanceLeaseManager",
    "LeaseManagerConfig",
    "LeaseQuotaExceededError",
    "ReconcileReport",
]
