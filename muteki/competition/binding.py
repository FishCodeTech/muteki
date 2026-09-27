"""RunBindingService：competition.db 侧的 Run 绑定与 revision 冲突策略
（任务书 10.4 / 设计 7.2、16.3，COMP-05）。

语义：

- **稳定 run_id**：``crun_`` 前缀 + sha256(challenge_id, revision_id) 截断，
  在调用 RunGateway 之前由 competition.db 预先确定并写进 ``RunBinding``
  行（设计 16.3：``BoundRunRequest.run_id`` 由 competition.db 预先确定）；
  RunGateway/RunManager 侧的幂等索引与 ``crun_`` 命名空间冲突拒绝由
  CORE-04 保证，本服务不绕过。
- **binding key**：``comp:<competition_id>:cch:<challenge_id>:rev:<revision_id>``
  稳定唯一；``task_revision`` 取 ``ChallengeRevision.revision_seq``。同一
  binding key 不同 revision_seq 由 RunGateway 以 typed CONFLICT 拒绝
  （binding key 与 revision 冲突语义）。
- **同步变化后的确定策略表**（设计 7.2）：

  +--------------------------------+----------------------------------------+
  | 情形                           | 动作                                   |
  +================================+========================================+
  | 无活动 binding                 | 新 Run 一律用 current revision         |
  +--------------------------------+----------------------------------------+
  | 活动 binding 同 revision       | 幂等返回（不重复建 Run）               |
  +--------------------------------+----------------------------------------+
  | 不同 revision + continue(默认) | 活动 Run 保持原输入，不静默修改        |
  +--------------------------------+----------------------------------------+
  | 不同 revision + pause          | ACTIVE→PAUSED，下发 gateway pause      |
  +--------------------------------+----------------------------------------+
  | 不同 revision + resolve        | →RESOLVING→ACTIVE，generation+1 并切   |
  |                                | 到新 revision，下发 gateway resolve    |
  +--------------------------------+----------------------------------------+

- 下发路径绝不发送 ``swarm_class``（实验臂不接产品默认；RunGateway 侧
  另有显式拒绝兜底，CORE-04）。
- 实例硬变化（COMP-07）与 Operator resolve 经 ``resolve_execution`` 走
  与 revision RESOLVE 相同的迁移（设计 9.3 规则 7）；reconcile 降级经
  ``pause`` 暂停新操作（设计 9.2）。
"""

from __future__ import annotations

import asyncio
import hashlib
from enum import Enum
from typing import Any, Callable, Optional

from muteki.competition import events as ev
from muteki.competition.compiler import ChallengeCompiler
from muteki.competition.models import (
    BindingState,
    ChallengeRevision,
    CompetitionChallenge,
    IllegalTransitionError,
    RunBinding,
    ensure_binding_transition,
)
from muteki.competition.store import CompetitionStore, NotFoundError
from muteki.models.solve_graph import Challenge
from muteki.platform.contracts.receipts import ReceiptState
from muteki.platform.contracts.runs import BoundRunRequest, RunCommand

#: 与 apps.web.run_manager.BOUND_RUN_ID_PREFIX 保持一致（CORE-04 的命名空间）。
BOUND_RUN_ID_PREFIX = "crun_"


class RevisionChangePolicy(str, Enum):
    """同步产生新 revision 后对活动 Run 的处理策略（确定策略表）。"""

    CONTINUE = "continue"  # 活动 Run 保持原输入（默认）
    PAUSE = "pause"        # 暂停活动 Run，交 Operator 决策
    RESOLVE = "resolve"    # 进入新执行代并切到新 revision


#: 策略 → 允许执行的活动 binding 状态。planned/creating/starting 是创建在途
#: 的瞬态：不允许 pause/resolve，由 continue 等待落地后交 Operator。
_POLICY_ALLOWED_STATES: dict[RevisionChangePolicy, frozenset[BindingState]] = {
    RevisionChangePolicy.CONTINUE: frozenset(),  # 任何状态都不动
    RevisionChangePolicy.PAUSE: frozenset({BindingState.ACTIVE}),
    RevisionChangePolicy.RESOLVE: frozenset({
        BindingState.ACTIVE, BindingState.REJECTED, BindingState.PAUSED,
    }),
}


class BindingConflictError(ValueError):
    """binding 与 revision 冲突（同题活动 binding 指向不同 revision 等）。"""


def binding_key_for(
    competition_id: str, challenge_id: str, revision_id: str
) -> str:
    """稳定 binding key：身份随 revision 变化，天然区分新旧执行输入。"""
    return f"comp:{competition_id}:cch:{challenge_id}:rev:{revision_id}"


def mint_run_id(challenge_id: str, revision_id: str) -> str:
    """competition.db 预先确定的稳定 run_id（``crun_`` 命名空间）。

    同一 (challenge, revision) 永远得到同一 run_id：ensure_bound_run 重试
    与重启恢复都回到同一 Run；resolve 沿用同一 run_id 递增
    execution_generation（设计 9.3 规则 7）。
    """
    digest = hashlib.sha256(
        f"{challenge_id}:{revision_id}".encode("utf-8")
    ).hexdigest()
    return f"{BOUND_RUN_ID_PREFIX}{digest[:16]}"


class RunBindingService:
    """题目 ↔ Run ↔ 执行代的绑定服务（RunGateway 的 competition.db 侧）。"""

    def __init__(
        self,
        store: CompetitionStore,
        gateway: Any = None,
        compiler: Optional[ChallengeCompiler] = None,
        *,
        workspace_root_for: Optional[Callable[[str], Any]] = None,
    ) -> None:
        """
        - ``gateway``：``RunGateway`` 协议实现（ensure_bound_run /
          command）；为 None 时只维护 competition.db 侧的 binding 状态。
        - ``workspace_root_for``：run_id → Run workspace 根的解析钩子；
          提供时 ``start`` 会把附件物化进 Run 输入 CAS，否则 attachments
          引用比赛级 CAS 对象路径。
        """
        self._store = store
        self._gateway = gateway
        self._compiler = compiler or ChallengeCompiler(store)
        self._workspace_root_for = workspace_root_for
        # scheduler 与 lease maintenance 是两个独立循环。租约到期时它们可能
        # 同时请求暂停同一 binding；按 binding 串行化，避免重复状态迁移。
        self._pause_locks: dict[str, asyncio.Lock] = {}

    # -- ensure_bound_run -------------------------------------------------------

    async def ensure_bound_run(
        self,
        challenge_id: str,
        *,
        revision_id: Optional[str] = None,
        executor_id: Optional[str] = None,
    ) -> RunBinding:
        """幂等绑定：同题同 revision 永远返回同一 RunBinding / Run。

        - 无活动 binding：创建 PLANNED binding（run_id 预铸落库），再调
          ``gateway.ensure_bound_run``（幂等），成功后转 CREATING；
        - 活动 binding 同 revision：幂等返回（gateway 侧同 key 同
          revision 也幂等）；
        - 活动 binding 不同 revision：抛 ``BindingConflictError``，由
          调用方按 ``on_revision_change`` 策略表显式处理，绝不静默改
          活动 Run 的输入。
        """
        challenge = self._store.get(CompetitionChallenge, challenge_id)
        if challenge is None:
            raise NotFoundError(f"challenge not found: {challenge_id}")
        if challenge.tombstoned:
            raise BindingConflictError(
                f"challenge {challenge_id} is tombstoned; cannot bind a run"
            )
        revision_id = revision_id or challenge.current_revision_id or ""
        revision = self._store.get(ChallengeRevision, revision_id) if revision_id else None
        if revision is None:
            raise NotFoundError(
                f"no revision to bind for challenge {challenge_id} "
                f"(revision_id={revision_id!r})"
            )

        existing = self._store.active_binding_for_challenge(challenge_id)
        if existing is not None:
            if existing.revision_id != revision.revision_id:
                raise BindingConflictError(
                    f"challenge {challenge_id} has an active binding "
                    f"{existing.binding_id} on revision "
                    f"{existing.revision_id}; requested {revision.revision_id}. "
                    "Use on_revision_change() with an explicit policy "
                    "(continue/pause/resolve)."
                )
            # 幂等路径：gateway 侧同 binding_key + 同 task_revision 返回同一 Run。
            if self._gateway is not None:
                await self._gateway.ensure_bound_run(
                    self._request_for(existing, revision, executor_id)
                )
            return existing

        binding = RunBinding(
            competition_id=challenge.competition_id,
            competition_challenge_id=challenge.challenge_id,
            revision_id=revision.revision_id,
            run_id=mint_run_id(challenge.challenge_id, revision.revision_id),
        )
        self._store.save(binding)
        self._store.append_events([ev.make_event(
            competition_id=binding.competition_id,
            aggregate_type=ev.AGG_BINDING,
            aggregate_id=binding.binding_id,
            event_type=ev.BINDING_CREATED,
            payload={
                "binding_id": binding.binding_id,
                "run_id": binding.run_id,
                "challenge_id": challenge.challenge_id,
                "revision_id": revision.revision_id,
                "execution_generation": binding.execution_generation,
            },
        )])
        if self._gateway is not None:
            try:
                await self._gateway.ensure_bound_run(
                    self._request_for(binding, revision, executor_id)
                )
            except Exception:
                failed = self._transition(binding, BindingState.FAILED)
                self._store.save(failed)
                raise
            binding = self._transition(binding, BindingState.CREATING)
            self._store.save(binding)
        return binding

    def _request_for(
        self,
        binding: RunBinding,
        revision: ChallengeRevision,
        executor_id: Optional[str],
    ) -> BoundRunRequest:
        return BoundRunRequest(
            binding_key=binding_key_for(
                binding.competition_id,
                binding.competition_challenge_id,
                binding.revision_id,
            ),
            task_id=binding.competition_challenge_id,
            task_kind="ctf",
            task_revision=revision.revision_seq,
            run_id=binding.run_id,
            executor_id=executor_id,
        )

    # -- revision 变化策略表 ------------------------------------------------------

    async def on_revision_change(
        self,
        challenge_id: str,
        new_revision_id: str,
        *,
        policy: RevisionChangePolicy | str = RevisionChangePolicy.CONTINUE,
    ) -> Optional[RunBinding]:
        """按确定策略表处理「同步后 current revision 变了」的活动 binding。

        返回处理后的活动 binding；无活动 binding 返回 None（下一个
        ``ensure_bound_run`` 自然用新 revision）。策略不允许当前状态时抛
        ``IllegalTransitionError``。
        """
        policy = (
            policy if isinstance(policy, RevisionChangePolicy)
            else RevisionChangePolicy(str(policy))
        )
        binding = self._store.active_binding_for_challenge(challenge_id)
        if binding is None or binding.revision_id == new_revision_id:
            return binding
        revision = self._store.get(ChallengeRevision, new_revision_id)
        if revision is None:
            raise NotFoundError(f"revision not found: {new_revision_id}")
        current = binding.binding_state()
        if policy is RevisionChangePolicy.CONTINUE:
            # 活动 Run 保持原输入，不静默修改（设计 7.2）。
            return binding
        if current not in _POLICY_ALLOWED_STATES[policy]:
            raise IllegalTransitionError(
                "run_binding", current.value, f"{policy.value}(revision_change)"
            )
        if policy is RevisionChangePolicy.PAUSE:
            return await self._pause_binding(binding)
        # RESOLVE：resolving → active，generation+1，切到新 revision（新执行代）。
        return await self._resolve_migration(binding, revision)

    # -- 实例硬变化 / Operator resolve 的新执行代（设计 9.3 规则 7 的同一条迁移） -----

    async def resolve_execution(
        self,
        challenge_id: str,
        *,
        reason: str = "resolve",
        extra_payload: Optional[dict[str, Any]] = None,
    ) -> Optional[RunBinding]:
        """revision 不变的新执行代迁移：RESOLVING → ACTIVE，generation+1。

        实例地址/凭据硬变化（COMP-07）与 Operator resolve 走同一条迁移；
        重新编译时活动 lease 的新地址经 ``compile_for`` 投影进 target。
        无活动 binding 返回 None；状态不允许时抛 ``IllegalTransitionError``
        （允许集合与 revision RESOLVE 策略相同）。
        ``extra_payload`` 并入 gateway resolve（如 visit 时间盒），不携带
        ``swarm_class``。
        """
        binding = self._store.active_binding_for_challenge(challenge_id)
        if binding is None:
            return None
        current = binding.binding_state()
        if current not in _POLICY_ALLOWED_STATES[RevisionChangePolicy.RESOLVE]:
            raise IllegalTransitionError(
                "run_binding", current.value, f"resolve_execution({reason})"
            )
        revision = self._store.get(ChallengeRevision, binding.revision_id)
        if revision is None:
            raise NotFoundError(f"revision not found: {binding.revision_id}")
        return await self._resolve_migration(
            binding, revision, extra=extra_payload)

    async def pause(
        self,
        challenge_id: str,
        *,
        reason: str = "operator",
    ) -> Optional[RunBinding]:
        """暂停活动 binding（ACTIVE→PAUSED 并下发 gateway pause）。

        实例 reconcile 降级（设计 9.2「结果未知时暂停新操作」）与 Operator
        暂停共用本入口；无活动 binding 返回 None，已 paused 幂等返回。
        """
        binding = self._store.active_binding_for_challenge(challenge_id)
        if binding is None:
            return None
        current = binding.binding_state()
        if current is BindingState.PAUSED:
            return binding
        if current is not BindingState.ACTIVE:
            raise IllegalTransitionError(
                "run_binding", current.value, f"pause({reason})"
            )
        return await self._pause_binding(binding)

    async def _pause_binding(self, binding: RunBinding) -> RunBinding:
        lock = self._pause_locks.setdefault(binding.binding_id, asyncio.Lock())
        async with lock:
            # 另一个维护循环可能已经完成暂停。重新读取后再决定是否产生副作用。
            current = self._store.get(RunBinding, binding.binding_id)
            if current is None:
                raise NotFoundError(f"run binding not found: {binding.binding_id}")
            if current.binding_state() is BindingState.PAUSED:
                return current
            if current.binding_state() is not BindingState.ACTIVE:
                raise IllegalTransitionError(
                    "run_binding", current.state, "pause"
                )
            binding = current

            if self._gateway is not None:
                snapshot = await self._gateway.snapshot(binding.run_id)
                # RunSnapshot.generation 是求解执行代；控制 journal 的 generation
                # 是独立的控制状态 CAS 代。先围栏执行代，再把 control_generation
                # 作为 pause 命令的 expected_generation。
                if int(snapshot.generation) != int(binding.execution_generation):
                    raise BindingConflictError(
                        "pause refused for stale run binding "
                        f"{binding.binding_id}: binding execution generation "
                        f"{binding.execution_generation}, run generation "
                        f"{snapshot.generation}"
                    )
                if snapshot.state in {"running", "paused"}:
                    control_generation = int(
                        (snapshot.detail or {}).get("control_generation") or 0
                    )
                    command_id = self._control_command_id(
                        binding, "park", control_generation
                    )
                    receipt = await self._gateway.command(
                        binding.run_id,
                        RunCommand(
                            # A competition visit must release both the remote
                            # instance and the local execution slot.  A plain
                            # Run pause keeps the coordinator task alive and a
                            # later resolve is rejected as "already live".
                            # Stop only the current execution generation; the
                            # stable run_id, workspace and event history remain
                            # available for the next resolve generation.
                            command_type="stop",
                            command_id=command_id,
                            expected_generation=control_generation,
                        ),
                    )
                    self._raise_for_failed_receipt(
                        receipt, operation="pause", run_id=binding.run_id)
                elif snapshot.state not in {"paused", "finished", "solved"}:
                    raise BindingConflictError(
                        f"pause refused for run {binding.run_id} in unexpected "
                        f"state {snapshot.state!r}"
                    )
            binding = self._transition(binding, BindingState.PAUSED)
            return self._store.save(binding)

    async def _resolve_migration(
        self,
        binding: RunBinding,
        revision: ChallengeRevision,
        extra: Optional[dict[str, Any]] = None,
    ) -> RunBinding:
        """RESOLVING →（gateway resolve + 重新编译）→ generation+1 → ACTIVE。"""
        if self._gateway is not None:
            challenge_payload = self.compile_for(binding, revision)
            payload: dict[str, Any] = {
                "challenge": challenge_payload.model_dump(mode="json"),
            }
            if extra:
                payload.update(extra)
                payload.pop("swarm_class", None)
            receipt = await self._gateway.command(binding.run_id, RunCommand(
                command_type="resolve",
                expected_generation=binding.execution_generation,
                payload=payload,
            ))
            self._raise_for_failed_receipt(
                receipt, operation="resolve", run_id=binding.run_id)
        # 只有 RunGateway 确认接收了 resolve，才把控制面推进到 resolving。
        # 失败回执会保留原状态，调度器可在下一轮重试，不会卡成假 resolving。
        binding = self._transition(binding, BindingState.RESOLVING)
        self._store.save(binding)
        changes: dict[str, Any] = {
            "execution_generation": binding.execution_generation + 1,
        }
        if revision.revision_id != binding.revision_id:
            changes["revision_id"] = revision.revision_id
        binding = binding.model_copy(update=changes)
        binding = self._transition(binding, BindingState.ACTIVE)
        return self._store.save(binding)

    # -- 编译与下发 ----------------------------------------------------------------

    def compile_for(
        self,
        binding: RunBinding,
        revision: Optional[ChallengeRevision] = None,
    ) -> Challenge:
        """编译 binding 当前 revision 为派发用核心 ``Challenge``。

        活动实例地址由 lease 投影进入 target；提供 workspace 钩子时附件
        经 ``materialize_input`` 物化进 Run 输入 CAS。
        """
        revision = revision or self._store.get(
            ChallengeRevision, binding.revision_id
        )
        if revision is None:
            raise NotFoundError(
                f"revision not found: {binding.revision_id}"
            )
        lease = self._store.active_lease_for_challenge(
            binding.competition_challenge_id
        )
        workspace = (
            self._workspace_root_for(binding.run_id)
            if self._workspace_root_for is not None else None
        )
        return self._compiler.compile(
            revision, lease=lease, workspace_root=workspace
        )

    def start_payload(
        self,
        binding: RunBinding,
        *,
        extra: Optional[dict[str, Any]] = None,
    ) -> dict[str, Any]:
        """构造 gateway start 命令载荷；绝不携带 ``swarm_class``。"""
        body: dict[str, Any] = {
            "challenge": self.compile_for(binding).model_dump(mode="json"),
        }
        body.update(extra or {})
        body.pop("swarm_class", None)  # 产品下发不发送 swarm_class（锁定）
        return body

    async def start(
        self,
        binding: RunBinding,
        *,
        extra_payload: Optional[dict[str, Any]] = None,
    ) -> RunBinding:
        """经 gateway 下发 start 并把 binding 推进到 ACTIVE。"""
        if self._gateway is None:
            raise BindingConflictError(
                "start requires a RunGateway-bound service"
            )
        receipt = await self._gateway.command(binding.run_id, RunCommand(
            command_type="start",
            expected_generation=binding.execution_generation,
            payload=self.start_payload(binding, extra=extra_payload),
        ))
        self._raise_for_failed_receipt(
            receipt, operation="start", run_id=binding.run_id)
        # planned → creating → starting → active；已在 active（重入）则不推进。
        for target in (BindingState.CREATING, BindingState.STARTING,
                       BindingState.ACTIVE):
            state = binding.binding_state()
            if state is BindingState.ACTIVE:
                break
            if state is target:
                continue
            binding = self._transition(binding, target)
            binding = self._store.save(binding)
        return binding

    # -- 内部 ----------------------------------------------------------------------

    @staticmethod
    def _control_command_id(
        binding: RunBinding,
        operation: str,
        control_generation: int,
    ) -> str:
        """同一 binding/执行代/控制代的控制动作只产生一个 journal 命令。"""
        material = (
            f"{binding.binding_id}:{binding.execution_generation}:"
            f"{control_generation}:{operation}"
        )
        digest = hashlib.sha256(material.encode("utf-8")).hexdigest()
        return f"cmd-competition-{digest[:32]}"

    @staticmethod
    def _raise_for_failed_receipt(
        receipt: Any,
        *,
        operation: str,
        run_id: str,
    ) -> None:
        """RunGateway 用失败回执表达领域错误；调用方不能把它当成功。"""
        state = getattr(receipt, "state", None)
        if state not in {
            ReceiptState.FAILED,
            ReceiptState.CONFLICT,
            ReceiptState.CANCELLED,
        }:
            return
        error = getattr(receipt, "error", None)
        code = str(getattr(error, "code", "") or state.value)
        message = str(
            getattr(error, "message", "")
            or f"gateway returned {state.value}"
        )
        detail = f"{operation} failed for run {run_id}: {code}: {message}"
        if state is ReceiptState.CONFLICT:
            raise BindingConflictError(detail)
        raise RuntimeError(detail)

    def _transition(self, binding: RunBinding, target: BindingState) -> RunBinding:
        current = binding.binding_state()
        ensure_binding_transition(current, target)
        updated = binding.model_copy(update={"state": target.value})
        self._store.append_events([ev.make_event(
            competition_id=binding.competition_id,
            aggregate_type=ev.AGG_BINDING,
            aggregate_id=binding.binding_id,
            event_type=ev.BINDING_STATE_CHANGED,
            payload={
                "binding_id": binding.binding_id,
                "run_id": binding.run_id,
                "challenge_id": binding.competition_challenge_id,
                "from": current.value,
                "to": target.value,
                "execution_generation": binding.execution_generation,
            },
        )])
        return updated


__all__ = [
    "BOUND_RUN_ID_PREFIX",
    "BindingConflictError",
    "RevisionChangePolicy",
    "RunBindingService",
    "binding_key_for",
    "mint_run_id",
]
