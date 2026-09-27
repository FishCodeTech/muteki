"""SubmissionService：提交候选准入、远端提交执行与结果分支处理
（任务书 10.7 / 设计 9.4、12，COMP-08）。

语义边界：

- **候选准入**：核心 Run 已接受的模型 Skill 提交直接登记。这里只保留
  题目状态、来源 Run 和来源类别等业务约束，不再复核 Flag 内容。
- **状态机**：candidate →（assisted：awaiting_approval →
  ``platform_submission.approve`` 命令 / autonomous：来源、预算、冷却、
  策略全部满足后自动批准）→ PlatformSubmission queued → submitting →
  终态。幂等键 (competition_id, challenge, answer_slot, digest, attempt)
  由 COMP-01 的 ``next_submission_attempt`` 与唯一索引保证。
- **observe**：只登记候选，绝不创建提交动作（不生成 outbox）。
- **结果分支**（设计 9.4 / 任务书 10.7）：
  - ``correct`` / ``duplicate_or_solved``：更新远端解题投影
    （challenge → solved，remote_state=solved_remote），结束仍在运行的
    绑定（gateway stop + binding → solved/stopped），释放动态实例；
    ``duplicate_or_solved`` 一律按 correct 处理。
  - ``wrong``：写 ``competition.platform_submission.state_changed``
    （to=wrong）事件，随后把候选值经 SharedGraph 的 flag invalidation
    通道（``reopen_after_false_positive``）投影为该 Run 的持久否决，
    再按策略 resolve 同一 Run（``resolve_execution``，递增
    execution_generation）；远端回执原文保留在 ``remote_receipt``。
  - ``unknown``（含异步平台 pending）：禁止直接重复提交；由
    ``reconcile_unknown`` 查询远端状态（Adapter 暴露
    ``poll_submission`` / ``reconcile_submission`` 时），否则等 Operator。
  - ``rate_limited``：保存 ``retry_after_at`` 并把连接级冷却写进
    ``connection.capabilities["submission_cooldown_until"]``；到点后由
    ``pump`` 自动回到 queued。
  - ``auth_failed``（模型层 ``auth_required``）：连接转
    ``ConnectionStatus.AUTH_REQUIRED``，暂停该连接的外部动作；已有本地
    Run 不做任何停止 / 暂停。
  - ``transient_failure``：有界重试，次数取连接级保守配置
    （``capabilities["detail"]["max_transient_retries"]``，缺省
    ``SubmissionServiceConfig.max_transient_retries_default``）。
- **数据暴露**（设计 12.4）：比赛事件 payload 只含候选 digest、来源
  Run 与回执状态；候选原文只留在 ``SubmissionCandidate`` 行（控制面
  competition.db）与来源 Run 的私有状态，绝不进事件 / SSE / outbox
  payload。
- **崩溃安全**：``_execute`` 先把 submission 落为 submitting 再发出平台
  请求；重启后 reconciler 把遗留 submitting 统一归 unknown（先查远端，
  禁止直接重发），outbox 记录对非 queued 的 submission 幂等收尾。
"""

from __future__ import annotations

import hashlib
from dataclasses import dataclass
from datetime import datetime, timedelta
from typing import Any, Callable, Mapping, Optional

from muteki.competition import events as ev
from muteki.competition.models import (
    BINDING_ACTIVE_STATES,
    AutomationMode,
    BindingState,
    CandidateState,
    ChallengeRevision,
    ChallengeState,
    Competition,
    CompetitionChallenge,
    CompetitionPolicy,
    ConnectionStatus,
    PlatformConnection,
    PlatformSubmission,
    ResourceBudget,
    SubmissionCandidate,
    SubmissionState,
    ensure_binding_transition,
    ensure_candidate_transition,
    ensure_challenge_transition,
    ensure_submission_transition,
    flag_digest,
)
from muteki.competition.platforms.base import (
    PlatformAuthRequiredError,
    PlatformRateLimitedError,
    PlatformTimeoutError,
    PlatformTransientError,
    PlatformTransportError,
    PlatformUnknownResultError,
)
from muteki.competition.store import (
    CompetitionStore,
    NotFoundError,
    StateConflictError,
)
from muteki.platform.contracts.base import utcnow
from muteki.platform.contracts.commands import CommandEnvelope
from muteki.platform.contracts.modules import SubmissionRequest
from muteki.platform.contracts.receipts import OutboxRecord
from muteki.platform.contracts.runs import RunCommand

# ---------------------------------------------------------------------------
# 事件类型（COMP-08 新增；与 scheduler.py 同样局部定义，不改 COMP-01 的
# events 模块）。候选事件聚合类型用 ``submission_candidate``。
# ---------------------------------------------------------------------------

AGG_CANDIDATE = "submission_candidate"

#: 候选登记（payload 只有 digest / 来源 Run / 来源类别，绝无候选原文）。
CANDIDATE_REGISTERED = "competition.submission.candidate_registered"
#: 候选被准入拒绝（无 Gate witness / 来源类别不允许 / 题目已解出等）。
CANDIDATE_REJECTED = "competition.submission.candidate_rejected"
#: 候选状态转移（candidate → awaiting_approval → approved → submitted …）。
CANDIDATE_STATE_CHANGED = "competition.submission.candidate_state_changed"
#: rate_limited 到点 / transient_failure 有界重试后回到 queued。
SUBMISSION_REQUEUED = "competition.platform_submission.requeued"
#: unknown 经远端核对落终态（reconciler 路径）。
SUBMISSION_RECONCILED = "competition.platform_submission.reconciled"
#: wrong 否决已投影到来源 Run 的 SharedGraph（payload 只有 digest）。
FLAG_INVALIDATION_PROJECTED = (
    "competition.platform_submission.invalidation_projected"
)
#: 连接状态变化（auth_failed 暂停外部动作等）。
CONNECTION_STATUS_CHANGED = "competition.connection.status_changed"
MANUAL_OVERRIDE_APPLIED = "competition.submission.manual_override_applied"

# ---------------------------------------------------------------------------
# 候选来源类别（任务书 10.7：只有真实执行来源可以成为提交候选来源）
# ---------------------------------------------------------------------------

SOURCE_EXECUTION_OUTPUT = "execution_output"  # 命令 stdout/stderr 原文
SOURCE_ARTIFACT = "artifact"                  # 产物内容（路径 + 内容）
SOURCE_RUN_EVENT = "run_event"                # Run 内事件指针（witness 原文随指针）
SOURCE_OPERATOR_OVERRIDE = "operator_override"  # 独立人工覆盖命令

#: 允许成为提交来源的类别。
ALLOWED_SOURCE_KINDS: frozenset[str] = frozenset({
    SOURCE_EXECUTION_OUTPUT, SOURCE_ARTIFACT, SOURCE_RUN_EVENT,
})

#: 显式禁止的来源类别：Operator / Review / Advisor / 知识库 / 普通聊天。
REJECTED_SOURCE_KINDS: frozenset[str] = frozenset({
    "operator", "review", "advisor", "knowledge", "chat",
})

#: 未提供的平台格式保持未知，由平台判定候选；不猜测前缀或大小写。
DEFAULT_FLAG_FORMAT = ""

#: 连接级冷却写入 capabilities 的键（ISO8601 时间戳）。
CONNECTION_COOLDOWN_KEY = "submission_cooldown_until"


class CandidateRejectedError(ValueError):
    """候选准入被拒绝；``reason`` 是稳定机器可读原因。"""

    def __init__(self, reason: str, detail: str = "") -> None:
        super().__init__(detail or reason)
        self.reason = reason


@dataclass(frozen=True)
class SubmissionServiceConfig:
    """提交服务参数（连接级保守缺省）。"""

    max_transient_retries_default: int = 3   # transient_failure 有界重试上限
    default_retry_after_seconds: float = 60.0  # 平台未给 Retry-After 时的兜底


def _parse_iso(value: Any) -> Optional[datetime]:
    if not value:
        return None
    try:
        return datetime.fromisoformat(str(value))
    except (TypeError, ValueError):
        return None


def open_run_shared_graph(
    db_path: str,
    *,
    challenge_id: str = "",
    flag_format: str = "",
) -> Any:
    """打开某个 Run 的 SharedGraph（flag invalidation 通道的投影目标）。

    延迟导入 swarm 层，避免 competition 包在 import 期拉入 solver/swarm
    依赖链。Challenge 只承载图的事件 schema 元数据；否决走
    ``reopen_after_false_positive`` 的 dedupe_key，重复投影不重复写。
    """
    from muteki.models.solve_graph import Challenge
    from muteki.swarm.shared_graph import SQLiteSharedGraph

    challenge = Challenge(
        id=challenge_id or "competition",
        name="competition-projection",
        category="misc",
        flag_format=str(flag_format or "").strip(),
    )
    return SQLiteSharedGraph.open(db_path=db_path, challenge=challenge)


class SubmissionService:
    """提交候选准入与远端提交执行（任务书 10.7）。

    - ``adapters`` / ``adapter_for``：platform_kind → PlatformAdapter
      （契约 ``submit``；可选 ``poll_submission`` / ``reconcile_submission``
      用于 unknown 核对）。
    - ``binding_service``：wrong 后 resolve 同一 Run（新 execution
      generation）；为 None 时只维护提交自身状态。
    - ``lease_manager``：correct 后释放动态实例；为 None 时跳过。
    - ``gateway``：correct 后向仍在运行的 Run 下发 stop（尽力而为）。
    - ``shared_graph_for``：run_id → 已打开的 SharedGraph（鸭子类型，
      需暴露 ``reopen_after_false_positive``）；返回 None 表示该 Run 的
      图不可用，否决投影留给 reconciler 的水位投影器补齐。
    """

    def __init__(
        self,
        store: CompetitionStore,
        adapters: Optional[Mapping[str, Any]] = None,
        *,
        adapter_for: Optional[Callable[[PlatformConnection], Any]] = None,
        binding_service: Any = None,
        lease_manager: Any = None,
        gateway: Any = None,
        shared_graph_for: Optional[Callable[[str], Optional[Any]]] = None,
        source_witness_resolver: Optional[Callable[..., str]] = None,
        config: Optional[SubmissionServiceConfig] = None,
        now: Optional[Callable[[], datetime]] = None,
    ) -> None:
        self._store = store
        self._adapters = dict(adapters or {})
        self._adapter_for = adapter_for
        self._binding = binding_service
        self._leases = lease_manager
        self._gateway = gateway
        self._shared_graph_for = shared_graph_for
        self._source_witness_resolver = source_witness_resolver
        self._config = config or SubmissionServiceConfig()
        self._now = now or utcnow

    # ------------------------------------------------------------------
    # 候选准入
    # ------------------------------------------------------------------

    def register_candidate(
        self,
        challenge_id: str,
        value: str,
        *,
        source_run_id: str,
        source_kind: str,
        witness: str,
        gate_verdict: str = "",
        answer_slot: int = 1,
        command: Optional[CommandEnvelope] = None,
        source_execution_generation: int = 0,
        source_worker_id: str = "",
        source_session_id: str = "",
        shared_graph_fact_id: str = "",
        witness_artifact_path: str = "",
    ) -> SubmissionCandidate:
        """登记一个提交候选；幂等（同题同槽同值返回已有候选）。"""
        challenge = self._challenge(challenge_id)
        value = str(value)
        witness = str(witness or "")
        source_kind = str(source_kind or "").strip()
        digest = flag_digest(value)

        def _reject(reason: str, detail: str) -> None:
            self._append_candidate_event(
                challenge, digest, CANDIDATE_REJECTED,
                {"reason": reason, "source_kind": source_kind,
                 "source_run_id": source_run_id, "answer_slot": answer_slot},
                command=command,
            )
            raise CandidateRejectedError(reason, detail)

        if challenge.tombstoned:
            _reject("challenge_tombstoned",
                    f"challenge {challenge_id} is tombstoned")
        if challenge.state == ChallengeState.SOLVED.value:
            _reject("challenge_solved",
                    f"challenge {challenge_id} is already solved")
        if source_kind not in ALLOWED_SOURCE_KINDS:
            _reject(
                "source_kind_not_allowed",
                f"source_kind {source_kind!r} is not an execution provenance "
                f"(operator/review/advisor/knowledge/chat can never be a "
                "submission source)",
            )
        if not source_run_id.strip():
            _reject("missing_source_run", "source_run_id is required")
        # 幂等：唯一键 (challenge, answer_slot, digest)（设计 7.1）。
        for existing in self._store.list(
            SubmissionCandidate,
            competition_challenge_id=challenge.challenge_id,
            answer_slot=int(answer_slot),
            digest=digest,
        ):
            return existing

        candidate = SubmissionCandidate(
            competition_id=challenge.competition_id,
            competition_challenge_id=challenge.challenge_id,
            answer_slot=int(answer_slot),
            value=value,
            digest=digest,
            source_run_id=source_run_id.strip(),
            source_ref=source_kind,
            gate_verdict=str(gate_verdict or ""),
            source_execution_generation=max(
                0, int(source_execution_generation or 0)),
            source_worker_id=str(source_worker_id or ""),
            source_session_id=str(source_session_id or ""),
            shared_graph_fact_id=str(shared_graph_fact_id or ""),
            witness_digest=hashlib.sha256(
                witness.encode("utf-8")).hexdigest(),
            witness_artifact_path=(
                str(witness_artifact_path or "")
                if source_kind == SOURCE_ARTIFACT else ""
            ),
        )
        with self._store.lock, self._store.conn:
            self._store.save(candidate)
            self._append_candidate_event(
                challenge, digest, CANDIDATE_REGISTERED,
                {"candidate_id": candidate.candidate_id,
                 "source_run_id": candidate.source_run_id,
                 "source_kind": source_kind,
                 "gate_verdict": candidate.gate_verdict,
                 "answer_slot": candidate.answer_slot,
                 "execution_generation": (
                     candidate.source_execution_generation),
                 "worker_id": candidate.source_worker_id,
                 "session_id": candidate.source_session_id,
                 "shared_graph_fact_id": candidate.shared_graph_fact_id,
                 "witness_digest": candidate.witness_digest},
                command=command,
            )
            # 候选出现后题目进入 candidate_found（设计 12.1）；其它状态
            # （如同步后尚未调度的 discovered）不在此强行推进。
            if challenge.state == ChallengeState.RUNNING.value:
                self._transition_challenge(
                    challenge, ChallengeState.CANDIDATE_FOUND)
                self._store.save(challenge.model_copy(update={
                    "state": ChallengeState.CANDIDATE_FOUND.value}))

        competition = self._store.get(Competition, challenge.competition_id)
        policy = (
            self._store.get(CompetitionPolicy, challenge.competition_id)
        ) or CompetitionPolicy(competition_id=challenge.competition_id)
        connection = (
            self._store.get(PlatformConnection, competition.connection_id)
            if competition is not None else None
        )
        mode = AutomationMode(policy.automation_mode)
        if mode is AutomationMode.OBSERVE:
            # observe：只登记，绝不创建提交动作（任务书 10.7）。
            return candidate
        if mode is AutomationMode.ASSISTED:
            # assisted：等待 Operator 的 platform_submission.approve 命令。
            updated = self._transition_candidate(
                candidate, CandidateState.AWAITING_APPROVAL)
            with self._store.lock, self._store.conn:
                self._store.save(updated)
                self._append_candidate_event(
                    challenge, digest, CANDIDATE_STATE_CHANGED,
                    {"candidate_id": candidate.candidate_id,
                     "from": CandidateState.CANDIDATE.value,
                     "to": CandidateState.AWAITING_APPROVAL.value},
                    command=command,
                )
            return updated
        # autonomous：来源（已过 Gate）、预算、冷却、策略全部满足后自动提交。
        allowed, reason = self._auto_submit_allowed(challenge, connection)
        if not allowed:
            self._append_candidate_event(
                challenge, digest, CANDIDATE_STATE_CHANGED,
                {"candidate_id": candidate.candidate_id,
                 "from": candidate.state, "to": candidate.state,
                 "hold_reason": reason},
            )
            return candidate
        self.approve(
            candidate.candidate_id, actor="system", command=command)
        return self._store.get(SubmissionCandidate, candidate.candidate_id)

    # ------------------------------------------------------------------
    # 批准与取消（assisted 的 approve 命令与 autonomous 共用同一落库路径）
    # ------------------------------------------------------------------

    def approve(
        self,
        candidate_id: str,
        *,
        actor: str = "operator",
        command: Optional[CommandEnvelope] = None,
    ) -> PlatformSubmission:
        """批准候选并生成 queued 提交 + outbox（幂等 attempt 序号）。

        状态机：candidate awaiting_approval/candidate → approved；
        PlatformSubmission queued；题目 candidate_found → submitting
        （submitting 自循环允许 rate_limited 等待期间再次批准）。
        """
        candidate = self._store.get(SubmissionCandidate, candidate_id)
        if candidate is None:
            raise NotFoundError(f"submission_candidate not found: {candidate_id}")
        challenge = self._challenge(candidate.competition_challenge_id)
        self._transition_candidate(candidate, CandidateState.APPROVED)  # 校验
        attempt = self._store.next_submission_attempt(
            challenge.challenge_id, candidate.answer_slot, candidate.digest)
        submission = PlatformSubmission(
            competition_id=challenge.competition_id,
            competition_challenge_id=challenge.challenge_id,
            candidate_id=candidate.candidate_id,
            answer_slot=candidate.answer_slot,
            digest=candidate.digest,
            attempt=attempt,
        )
        connection = self._connection_for(challenge)
        record = OutboxRecord(
            command_id=command.command_id if command is not None else None,
            correlation_id=(
                str(command.payload.get("correlation_id") or command.command_id)
                if command is not None else ""
            ),
            aggregate_type=ev.AGG_SUBMISSION,
            aggregate_id=submission.submission_id,
            event_type=ev.SUBMISSION_QUEUED,
            destination=(
                f"platform.{connection.platform_kind}" if connection else ""),
            payload={
                "op": "submit",
                "submission_id": submission.submission_id,
                "challenge_id": challenge.challenge_id,
                "competition_id": challenge.competition_id,
            },
        )
        outbox_key = (
            f"platform_submission.submit:{submission.submission_id}:attempt:{attempt}"
        )
        with self._store.lock, self._store.conn:
            self._store.save(candidate.model_copy(
                update={"state": CandidateState.APPROVED.value}))
            self._store.save(submission)
            self._append_candidate_event(
                challenge, candidate.digest, CANDIDATE_STATE_CHANGED,
                {"candidate_id": candidate.candidate_id,
                 "from": candidate.state, "to": CandidateState.APPROVED.value,
                 "actor": actor},
                command=command,
            )
            event_payload = {
                "submission_id": submission.submission_id,
                "candidate_id": candidate.candidate_id,
                "challenge_id": challenge.challenge_id,
                "answer_slot": candidate.answer_slot,
                "attempt": attempt,
            }
            self._store.append_events([
                ev.make_event(
                    competition_id=challenge.competition_id,
                    aggregate_type=ev.AGG_SUBMISSION,
                    aggregate_id=submission.submission_id,
                    event_type=ev.SUBMISSION_APPROVED,
                    command=command,
                    payload=event_payload,
                ),
                ev.make_event(
                    competition_id=challenge.competition_id,
                    aggregate_type=ev.AGG_SUBMISSION,
                    aggregate_id=submission.submission_id,
                    event_type=ev.SUBMISSION_QUEUED,
                    command=command,
                    payload={
                        **event_payload,
                        "state": SubmissionState.QUEUED.value,
                    },
                ),
            ])
            current = ChallengeState(challenge.state)
            ensure_challenge_transition(
                current, ChallengeState.SUBMITTING,
                paused_from=ChallengeState(challenge.paused_from)
                if challenge.paused_from else None)
            if current is not ChallengeState.SUBMITTING:
                self._transition_challenge(
                    challenge, ChallengeState.SUBMITTING, command=command)
                self._store.save(challenge.model_copy(update={
                    "state": ChallengeState.SUBMITTING.value,
                    "paused_from": None,
                }))
            self._store.outbox.enqueue(record, idempotency_key=outbox_key)
        return submission

    def approve_candidate(
        self,
        candidate_id: str,
        *,
        actor: str = "operator",
        command: Optional[CommandEnvelope] = None,
    ) -> PlatformSubmission:
        """产品命令使用的明确入口；与既有 ``approve`` 保持同一状态机。"""
        return self.approve(candidate_id, actor=actor, command=command)

    def manual_override(
        self,
        challenge_id: str,
        value: str,
        *,
        answer_slot: int = 1,
        actor: str,
        command: CommandEnvelope,
    ) -> tuple[SubmissionCandidate, PlatformSubmission]:
        """经独立 Operator 命令登记未核验答案并生成一次提交。

        该入口不调用 ``register_candidate``，也不会把 Operator 输入标记为
        Gate 已确认。调用方必须先完成权限和二次确认校验。事件只保存摘要、
        操作人和确认事实，答案原文仅保存在候选实体并在远端请求时读取。
        """
        challenge = self._challenge(challenge_id)
        value = str(value or "").strip()
        if challenge.tombstoned:
            raise CandidateRejectedError(
                "challenge_tombstoned", f"challenge {challenge_id} is tombstoned")
        if challenge.state == ChallengeState.SOLVED.value:
            raise CandidateRejectedError(
                "challenge_solved", f"challenge {challenge_id} is already solved")
        if not value:
            raise CandidateRejectedError("empty_value", "manual answer is empty")
        digest = flag_digest(value)
        existing = self._store.list(
            SubmissionCandidate,
            competition_challenge_id=challenge.challenge_id,
            answer_slot=int(answer_slot),
            digest=digest,
        )
        if existing:
            candidate = existing[0]
            submissions = self._store.list(
                PlatformSubmission, candidate_id=candidate.candidate_id)
            if submissions:
                return candidate, submissions[-1]
            candidate = candidate.model_copy(update={
                "state": CandidateState.APPROVED.value,
            })
        else:
            candidate = SubmissionCandidate(
                competition_id=challenge.competition_id,
                competition_challenge_id=challenge.challenge_id,
                answer_slot=int(answer_slot),
                value=value,
                digest=digest,
                source_ref=SOURCE_OPERATOR_OVERRIDE,
                gate_verdict="manual_override:unverified",
                state=CandidateState.APPROVED.value,
            )
        attempt = self._store.next_submission_attempt(
            challenge.challenge_id, candidate.answer_slot, candidate.digest)
        submission = PlatformSubmission(
            competition_id=challenge.competition_id,
            competition_challenge_id=challenge.challenge_id,
            candidate_id=candidate.candidate_id,
            answer_slot=candidate.answer_slot,
            digest=candidate.digest,
            attempt=attempt,
        )
        connection = self._connection_for(challenge)
        record = OutboxRecord(
            command_id=command.command_id,
            correlation_id=str(
                command.payload.get("correlation_id") or command.command_id),
            aggregate_type=ev.AGG_SUBMISSION,
            aggregate_id=submission.submission_id,
            event_type=ev.SUBMISSION_QUEUED,
            destination=(
                f"platform.{connection.platform_kind}" if connection else ""),
            payload={
                "op": "submit",
                "submission_id": submission.submission_id,
                "challenge_id": challenge.challenge_id,
                "competition_id": challenge.competition_id,
            },
        )
        previous_state = challenge.state
        updated_challenge = challenge.model_copy(update={
            "state": ChallengeState.SUBMITTING.value,
            "paused_from": None,
        })
        events = [
            ev.make_event(
                competition_id=challenge.competition_id,
                aggregate_type=AGG_CANDIDATE,
                aggregate_id=candidate.candidate_id,
                event_type=MANUAL_OVERRIDE_APPLIED,
                command=command,
                payload={
                    "candidate_id": candidate.candidate_id,
                    "challenge_id": challenge.challenge_id,
                    "digest": digest,
                    "answer_slot": candidate.answer_slot,
                    "actor": actor,
                    "source_kind": candidate.source_ref,
                    "gate_verdict": candidate.gate_verdict,
                    "acknowledged_unverified": True,
                },
            ),
            ev.make_event(
                competition_id=challenge.competition_id,
                aggregate_type=ev.AGG_SUBMISSION,
                aggregate_id=submission.submission_id,
                event_type=ev.SUBMISSION_APPROVED,
                command=command,
                payload={
                    "submission_id": submission.submission_id,
                    "candidate_id": candidate.candidate_id,
                    "challenge_id": challenge.challenge_id,
                    "answer_slot": candidate.answer_slot,
                    "attempt": attempt,
                    "approval_kind": "manual_override",
                    "actor": actor,
                },
            ),
            ev.make_event(
                competition_id=challenge.competition_id,
                aggregate_type=ev.AGG_SUBMISSION,
                aggregate_id=submission.submission_id,
                event_type=ev.SUBMISSION_QUEUED,
                command=command,
                payload={
                    "submission_id": submission.submission_id,
                    "state": SubmissionState.QUEUED.value,
                    "approval_kind": "manual_override",
                },
            ),
            ev.make_event(
                competition_id=challenge.competition_id,
                aggregate_type=ev.AGG_CHALLENGE,
                aggregate_id=challenge.challenge_id,
                event_type=ev.CHALLENGE_STATE_CHANGED,
                command=command,
                payload={
                    "challenge_id": challenge.challenge_id,
                    "from": previous_state,
                    "to": ChallengeState.SUBMITTING.value,
                    "reason": "manual_override",
                },
            ),
        ]
        with self._store.lock, self._store.conn:
            self._store.save(candidate)
            self._store.save(submission)
            self._store.save(updated_challenge)
            self._store.append_events(events)
            self._store.outbox.enqueue(
                record,
                idempotency_key=(
                    f"platform_submission.manual_override:"
                    f"{challenge.challenge_id}:{candidate.answer_slot}:{digest}"
                ),
            )
        return candidate, submission

    def cancel_submission(
        self, submission_id: str, *, reason: str = "operator"
    ) -> PlatformSubmission:
        """Operator 取消：queued/rate_limited/transient_failure/
        auth_required/unknown → cancelled（unknown 禁止重提，只允许取消或
        由 reconciler 核对，设计 9.4）。"""
        submission = self._submission(submission_id)
        submission = self._transition_submission(
            submission, SubmissionState.CANCELLED, {"reason": reason})
        return self._store.save(submission)

    # ------------------------------------------------------------------
    # outbox 泵：自动回队 → 补齐 queued 的 outbox → 执行 submit
    # ------------------------------------------------------------------

    async def pump(
        self,
        *,
        competition_id: Optional[str] = None,
        limit: int = 200,
    ) -> list[PlatformSubmission]:
        """一轮提交泵。返回本轮实际向平台发出的提交（终态已落库）。

        顺序：重新评估 autonomous 档被冷却暂存的候选并淘汰旧执行代候选 →
        rate_limited 到点 / transient_failure 界内自动回 queued →
        为没有待投递 outbox 记录的 queued 提交补记录（retry 命令路径）→
        逐条执行。连接冷却 / 预算 / 认证暂停 / 策略不满足时跳过，记录留
        pending 等下一轮。
        """
        moment = self._now()
        self.reconcile_held_candidates(competition_id=competition_id)
        self._auto_requeue(moment, competition_id=competition_id)
        pending = [
            r for r in self._store.outbox.pending(now=moment, limit=1000)
            if r.payload.get("op") == "submit"
        ]
        covered = {r.aggregate_id for r in pending}
        for submission in self._queued_submissions(competition_id):
            if submission.submission_id not in covered:
                record = self._enqueue_submit_record(submission)
                pending.append(record)
                covered.add(submission.submission_id)

        executed: list[PlatformSubmission] = []
        for record in pending[:limit]:
            submission = self._store.get(
                PlatformSubmission, record.aggregate_id)
            if submission is None:
                self._store.outbox.mark_failed(
                    record.outbox_id, "platform_submission row missing")
                continue
            state = submission.submission_state()
            if state is not SubmissionState.QUEUED:
                # 已发出过（submitting/unknown/终态）：副作用已完成，幂等收尾；
                # submitting 遗留由 reconciler 归 unknown，绝不在这里重发。
                if state is not SubmissionState.SUBMITTING:
                    self._store.outbox.mark_delivered(record.outbox_id)
                continue
            if competition_id and submission.competition_id != competition_id:
                continue
            challenge = self._store.get(
                CompetitionChallenge, submission.competition_challenge_id)
            connection = self._connection_for(challenge) if challenge else None
            if connection is None:
                self._store.outbox.mark_failed(
                    record.outbox_id, "platform connection missing")
                continue
            if self._cancel_stale_generation(submission, challenge):
                # The instance lifecycle has already resolved the same Run
                # against the replacement environment.  Never send a Flag
                # produced by an older execution generation to the platform.
                self._store.outbox.mark_delivered(record.outbox_id)
                continue
            if connection.status != ConnectionStatus.ACTIVE.value:
                continue  # auth_failed 等：暂停该连接的外部动作
            blocked, _reason = self._submit_gates(
                submission, connection, moment)
            if blocked:
                continue  # 冷却 / 预算：留 pending 等下一轮
            try:
                adapter = self._adapter(connection)
            except (NotFoundError, ValueError):
                # Extension upgrades temporarily withdraw the live adapter.
                # Keep the record pending until the replacement is ready; no
                # submission state or budget has changed yet.
                continue
            # 真实请求发出前原子领取；进程终止会留下 processing，启动恢复
            # 将其转为待核对状态，不能把尚未确认的提交误记为 completed。
            record = self._store.outbox.mark_processing(record.outbox_id)
            try:
                submission = await self._execute(
                    submission, challenge, connection, adapter=adapter)
            except Exception:
                # 平台回执可能已经落成终态，而后续的本地投影
                # （例如 wrong 后续做同一 Run）失败。此时外部副作用
                # 已有确定结果，必须收口 outbox；只有仍停在
                # submitting 才保留 processing，交给启动恢复核对。
                refreshed = self._store.get(
                    PlatformSubmission, record.aggregate_id)
                if (
                    refreshed is not None
                    and refreshed.submission_state()
                    not in {SubmissionState.QUEUED, SubmissionState.SUBMITTING}
                ):
                    self._store.outbox.mark_delivered(record.outbox_id)
                raise
            self._store.outbox.mark_delivered(record.outbox_id)
            executed.append(submission)
        return executed

    def reconcile_held_candidates(
        self,
        *,
        competition_id: Optional[str] = None,
    ) -> dict[str, list[str]]:
        """重新评估 autonomous 档暂存在候选态的 Flag。

        候选可能因为比赛级冷却或连接级冷却而暂时不能批准。冷却是瞬时
        条件，不能把候选永久留在 ``candidate``。若等待期间实例已更换，
        候选属于旧执行代，必须先从来源 SharedGraph 否决并标记为过期，
        让同一个 Run 在新地址上继续求解，绝不能再发往平台。
        """
        report: dict[str, list[str]] = {
            "approved": [], "stale": [], "held": [],
        }
        approved_competitions: set[str] = set()
        candidates = sorted(
            (
                candidate for candidate in self._store.list(SubmissionCandidate)
                if candidate.candidate_state() in {
                    CandidateState.CANDIDATE,
                    CandidateState.AWAITING_APPROVAL,
                }
                and (
                    not competition_id
                    or candidate.competition_id == competition_id
                )
            ),
            key=lambda candidate: (candidate.created_at, candidate.candidate_id),
        )
        for candidate in candidates:
            challenge = self._store.get(
                CompetitionChallenge, candidate.competition_challenge_id)
            if challenge is None:
                report["held"].append(candidate.candidate_id)
                continue
            policy = self._store.get(
                CompetitionPolicy, candidate.competition_id
            ) or CompetitionPolicy(competition_id=candidate.competition_id)
            if policy.automation_mode != AutomationMode.AUTONOMOUS.value:
                continue
            binding = self._store.active_binding_for_challenge(
                challenge.challenge_id)
            source_generation = int(
                candidate.source_execution_generation or 0)
            current_generation = int(
                binding.execution_generation or 0) if binding is not None else 0
            if (
                source_generation > 0
                and current_generation > source_generation
            ):
                target = (
                    CandidateState.SUPERSEDED
                    if candidate.candidate_state() is CandidateState.CANDIDATE
                    else CandidateState.CANCELLED
                )
                reason = "stale_execution_generation"
                updated = self._transition_candidate(candidate, target)
                with self._store.lock, self._store.conn:
                    self._store.save(updated)
                    self._store.append_events([ev.make_event(
                        competition_id=candidate.competition_id,
                        aggregate_type=AGG_CANDIDATE,
                        aggregate_id=candidate.candidate_id,
                        event_type=CANDIDATE_STATE_CHANGED,
                        payload={
                            "candidate_id": candidate.candidate_id,
                            "challenge_id": challenge.challenge_id,
                            "from": candidate.state,
                            "to": target.value,
                            "reason": reason,
                            "source_execution_generation": source_generation,
                            "current_execution_generation": current_generation,
                        },
                    )])
                    other_current_candidate = any(
                        other.candidate_id != candidate.candidate_id
                        and other.candidate_state() in {
                            CandidateState.CANDIDATE,
                            CandidateState.AWAITING_APPROVAL,
                            CandidateState.APPROVED,
                        }
                        and (
                            int(other.source_execution_generation or 0) <= 0
                            or int(other.source_execution_generation or 0)
                            >= current_generation
                        )
                        for other in self._store.list(
                            SubmissionCandidate,
                            competition_challenge_id=challenge.challenge_id,
                        )
                    )
                    if (
                        not other_current_candidate
                        and challenge.state
                        == ChallengeState.CANDIDATE_FOUND.value
                    ):
                        self._transition_challenge(
                            challenge,
                            ChallengeState.RUNNING,
                            {
                                "reason": reason,
                                "source_execution_generation": source_generation,
                                "current_execution_generation": current_generation,
                            },
                        )
                        self._store.save(challenge.model_copy(update={
                            "state": ChallengeState.RUNNING.value,
                            "paused_from": None,
                        }))
                self.project_invalidation(
                    candidate,
                    receipt=(
                        "candidate expired after dynamic instance replacement "
                        f"generation {source_generation}->{current_generation}"
                    ),
                )
                report["stale"].append(candidate.candidate_id)
                continue
            if candidate.competition_id in approved_competitions:
                report["held"].append(candidate.candidate_id)
                continue
            connection = self._connection_for(challenge)
            allowed, _reason = self._auto_submit_allowed(challenge, connection)
            if not allowed:
                report["held"].append(candidate.candidate_id)
                continue
            self.approve(candidate.candidate_id, actor="system")
            approved_competitions.add(candidate.competition_id)
            report["approved"].append(candidate.candidate_id)
        return report

    def _cancel_stale_generation(
        self,
        submission: PlatformSubmission,
        challenge: CompetitionChallenge,
    ) -> bool:
        """取消已跨过实例/执行代边界的排队候选。

        动态实例重新申请或地址硬变化时，InstanceLeaseService 会先 resolve
        同一 Run 并递增 binding.execution_generation。旧代候选可能因为平台
        插件不可用或限流仍留在 outbox；这里在平台边界前做最后一道校验，
        防止把旧环境 Flag 发到新环境。
        """
        candidate = self._store.get(
            SubmissionCandidate, submission.candidate_id)
        binding = self._store.active_binding_for_challenge(
            challenge.challenge_id)
        if candidate is None or binding is None:
            return False
        source_generation = int(candidate.source_execution_generation or 0)
        current_generation = int(binding.execution_generation or 0)
        if source_generation <= 0 or source_generation >= current_generation:
            return False

        reason = "stale_execution_generation"
        cancelled = self._transition_submission(
            submission,
            SubmissionState.CANCELLED,
            {
                "reason": reason,
                "source_execution_generation": source_generation,
                "current_execution_generation": current_generation,
            },
        )
        with self._store.lock, self._store.conn:
            self._store.save(cancelled.model_copy(update={"last_error": ""}))
            if candidate.candidate_state() is CandidateState.APPROVED:
                updated_candidate = self._transition_candidate(
                    candidate, CandidateState.CANCELLED)
                self._store.save(updated_candidate)
                self._store.append_events([ev.make_event(
                    competition_id=candidate.competition_id,
                    aggregate_type=AGG_CANDIDATE,
                    aggregate_id=candidate.candidate_id,
                    event_type=CANDIDATE_STATE_CHANGED,
                    payload={
                        "candidate_id": candidate.candidate_id,
                        "challenge_id": challenge.challenge_id,
                        "from": CandidateState.APPROVED.value,
                        "to": CandidateState.CANCELLED.value,
                        "reason": reason,
                        "source_execution_generation": source_generation,
                        "current_execution_generation": current_generation,
                    },
                )])
            other_inflight = any(
                other.submission_id != submission.submission_id
                and other.state in {
                    SubmissionState.QUEUED.value,
                    SubmissionState.SUBMITTING.value,
                    SubmissionState.RATE_LIMITED.value,
                    SubmissionState.TRANSIENT_FAILURE.value,
                    SubmissionState.AUTH_REQUIRED.value,
                    SubmissionState.UNKNOWN.value,
                }
                for other in self._store.list(
                    PlatformSubmission,
                    competition_challenge_id=challenge.challenge_id,
                )
            )
            if (not other_inflight
                    and challenge.state == ChallengeState.SUBMITTING.value):
                self._transition_challenge(
                    challenge,
                    ChallengeState.RUNNING,
                    {
                        "reason": reason,
                        "source_execution_generation": source_generation,
                        "current_execution_generation": current_generation,
                    },
                )
                self._store.save(challenge.model_copy(update={
                    "state": ChallengeState.RUNNING.value,
                    "paused_from": None,
                }))
        self.project_invalidation(
            candidate,
            receipt=(
                "queued candidate expired after dynamic instance replacement "
                f"generation {source_generation}->{current_generation}"
            ),
        )
        return True

    def _auto_requeue(
        self, moment: datetime, *, competition_id: Optional[str]
    ) -> None:
        """rate_limited 到点回 queued；transient_failure 界内自动重试。"""
        for submission in self._store.list(PlatformSubmission):
            if competition_id and submission.competition_id != competition_id:
                continue
            state = submission.submission_state()
            if state is SubmissionState.RATE_LIMITED:
                until = submission.retry_after_at
                if until is not None and moment >= until:
                    self._requeue(submission, reason="retry_after_reached")
            elif state is SubmissionState.TRANSIENT_FAILURE:
                challenge = self._store.get(
                    CompetitionChallenge,
                    submission.competition_challenge_id)
                connection = (
                    self._connection_for(challenge)
                    if challenge is not None else None
                )
                # 次数语义为「重试」（不含首提）：已失败 N 次 = 已用 N-1
                # 次重试；N <= max 时还可以再回队一次。
                if self._transient_retry_count(submission) <= (
                        self._max_transient_retries(connection)):
                    self._requeue(submission, reason="transient_retry")

    def _requeue(self, submission: PlatformSubmission, *, reason: str) -> None:
        submission = self._transition_submission(
            submission, SubmissionState.QUEUED, {"reason": reason})
        with self._store.lock, self._store.conn:
            self._store.save(submission)
            self._store.append_events([ev.make_event(
                competition_id=submission.competition_id,
                aggregate_type=ev.AGG_SUBMISSION,
                aggregate_id=submission.submission_id,
                event_type=SUBMISSION_REQUEUED,
                payload={
                    "submission_id": submission.submission_id,
                    "reason": reason,
                },
            )])

    def _enqueue_submit_record(
        self, submission: PlatformSubmission
    ) -> OutboxRecord:
        """为 queued 提交补一条 outbox 记录（retry 命令只改状态不入队）。

        幂等键取该提交第 N 条 submit 记录（N = 既有记录数 + 1），重启后
        重复补记得到同一键，不会重复入队。
        """
        challenge = self._store.get(
            CompetitionChallenge, submission.competition_challenge_id)
        connection = self._connection_for(challenge) if challenge else None
        row = self._store.conn.execute(
            "SELECT COUNT(*) FROM competition_outbox "
            "WHERE aggregate_type = ? AND aggregate_id = ?",
            (ev.AGG_SUBMISSION, submission.submission_id),
        ).fetchone()
        seq = int(row[0]) + 1
        record = OutboxRecord(
            aggregate_type=ev.AGG_SUBMISSION,
            aggregate_id=submission.submission_id,
            event_type=ev.SUBMISSION_QUEUED,
            destination=(
                f"platform.{connection.platform_kind}" if connection else ""),
            payload={
                "op": "submit",
                "submission_id": submission.submission_id,
                "challenge_id": submission.competition_challenge_id,
                "competition_id": submission.competition_id,
            },
        )
        record, _created = self._store.outbox.enqueue(
            record,
            idempotency_key=(
                f"platform_submission.submit:{submission.submission_id}:{seq}"
            ),
        )
        return record

    def _queued_submissions(
        self, competition_id: Optional[str]
    ) -> list[PlatformSubmission]:
        submissions = self._store.list(
            PlatformSubmission, state=SubmissionState.QUEUED.value)
        if competition_id:
            submissions = [
                s for s in submissions if s.competition_id == competition_id]
        return submissions

    # ------------------------------------------------------------------
    # 执行一次远端提交（先落 submitting 再发请求；结果分支落终态）
    # ------------------------------------------------------------------

    async def _execute(
        self,
        submission: PlatformSubmission,
        challenge: CompetitionChallenge,
        connection: PlatformConnection,
        *,
        adapter: Any = None,
    ) -> PlatformSubmission:
        candidate = self._store.get(SubmissionCandidate, submission.candidate_id)
        if candidate is None:
            submission = self._transition_submission(
                submission, SubmissionState.UNKNOWN,
                {"error": "candidate row missing"})
            return self._store.save(submission)
        adapter = adapter or self._adapter(connection)
        submission = self._transition_submission(
            submission, SubmissionState.SUBMITTING)
        with self._store.lock, self._store.conn:
            self._store.save(submission)
            self._consume_budget(submission.competition_id)

        request = SubmissionRequest(
            connection_id=connection.connection_id,
            challenge_key=challenge.external_challenge_id,
            flag=candidate.value,  # 候选原文只在提交请求中使用，不进事件
            idempotency_key=submission.submission_id,
        )
        try:
            result = await adapter.submit(request)
        except PlatformRateLimitedError as exc:
            return self._handle_rate_limited(submission, connection, exc)
        except PlatformAuthRequiredError as exc:
            return self._handle_auth_required(submission, connection, exc)
        except PlatformUnknownResultError as exc:
            return self._to_unknown(submission, str(exc))
        except (PlatformTimeoutError, PlatformTransientError) as exc:
            submission = self._transition_submission(
                submission, SubmissionState.TRANSIENT_FAILURE,
                {"error": str(exc), "category": exc.category.value})
            return self._store.save(submission.model_copy(
                update={"last_error": str(exc)}))
        except PlatformTransportError as exc:
            # 其它平台错误（permission/not_found/invalid_response…）：
            # 结果不确定，禁止盲目重试，归 unknown 等核对（任务书 10.7）。
            return self._to_unknown(
                submission, f"{exc.category.value}: {exc}")
        except Exception as exc:  # 请求是否发出不可知 → unknown，绝不重发
            return self._to_unknown(submission, f"unexpected: {exc}")

        status = str(result.status or "").strip()
        receipt = str(result.detail.get("remote_receipt") or "")
        if not receipt:
            receipt = str(
                result.detail.get("remote_status")
                or result.detail.get("kind")
                or result.detail.get("answer_result")
                or status
            )
        if status == "correct":
            return await self._handle_correct(submission, receipt=receipt)
        if status == "incorrect":
            return await self._handle_wrong(submission, receipt=receipt)
        if status in ("duplicate", "duplicate_or_solved"):
            # duplicate_or_solved 一律按 correct 处理（任务书 10.7）。
            submission = self._transition_submission(
                submission, SubmissionState.DUPLICATE_OR_SOLVED,
                {"receipt": receipt})
            submission = self._store.save(submission.model_copy(
                update={"remote_receipt": receipt, "last_error": ""}))
            return await self._apply_correct_effects(
                submission, receipt=receipt)
        if status == "partial":
            # 多答案题部分命中：本槽按 correct 计，题目是否解出看槽位数。
            return await self._handle_correct(submission, receipt=receipt)
        if status == "pending":
            # 异步判定平台（GZCTF 形态）：回执已拿到、判定未出 →
            # unknown，remote_receipt 存远端提交 id 供 reconciler 轮询。
            remote_id = str(result.submission_id or "")
            submission = self._transition_submission(
                submission, SubmissionState.UNKNOWN,
                {"async_verdict": True, "remote_submission_id": remote_id})
            return self._store.save(submission.model_copy(
                update={"remote_receipt": remote_id}))
        return self._to_unknown(submission, f"unrecognized verdict: {status}")

    def _to_unknown(
        self, submission: PlatformSubmission, error: str
    ) -> PlatformSubmission:
        submission = self._transition_submission(
            submission, SubmissionState.UNKNOWN, {"error": error})
        return self._store.save(
            submission.model_copy(update={"last_error": error}))

    # -- correct / duplicate_or_solved -------------------------------------

    async def _handle_correct(
        self, submission: PlatformSubmission, *, receipt: str
    ) -> PlatformSubmission:
        submission = self._transition_submission(
            submission, SubmissionState.CORRECT, {"receipt": receipt})
        submission = self._store.save(
            submission.model_copy(update={
                "remote_receipt": receipt,
                "last_error": "",
            }))
        return await self._apply_correct_effects(submission, receipt=receipt)

    async def _apply_correct_effects(
        self, submission: PlatformSubmission, *, receipt: str
    ) -> PlatformSubmission:
        """远端判对投影：候选 submitted；够槽位则题目 solved、结束绑定、
        释放动态实例（设计 12.3）。"""
        candidate = self._store.get(SubmissionCandidate, submission.candidate_id)
        if candidate is not None and candidate.candidate_state() not in (
                CandidateState.SUBMITTED,):
            self._transition_candidate(candidate, CandidateState.SUBMITTED)
            self._store.save(candidate.model_copy(
                update={"state": CandidateState.SUBMITTED.value}))
        challenge = self._store.get(
            CompetitionChallenge, submission.competition_challenge_id)
        if challenge is None:
            return submission
        if challenge.state == ChallengeState.SOLVED.value:
            await self._settle_binding_solved(challenge)
            await self._release_lease(challenge)
            return submission
        revision = (
            self._store.get(ChallengeRevision, challenge.current_revision_id)
            if challenge.current_revision_id else None
        )
        expected = max(1, int(revision.expected_flags)) if revision else 1
        # 多 Flag 题中 duplicate 只说明某个答案已提交，无法证明它是新的
        # 答案槽位；把不同的重复候选按 digest 累加会提前把题目判为解出。
        # 平台明确判对的候选才增加多 Flag 进度。单 Flag 题收到 duplicate
        # 仍可直接视为远端已解。
        progress_states = (
            (SubmissionState.CORRECT.value,)
            if expected > 1
            else (
                SubmissionState.CORRECT.value,
                SubmissionState.DUPLICATE_OR_SOLVED.value,
            )
        )
        solved_digests = {
            s.digest for s in self._store.list(
                PlatformSubmission,
                competition_challenge_id=challenge.challenge_id)
            if s.state in progress_states
        }
        if len(solved_digests) < expected:
            # This particular submission has a terminal verdict.  If there is
            # no other request queued or crossing the platform boundary, the
            # challenge must be available for continued multi-flag work.
            active_submission = any(
                other.submission_id != submission.submission_id
                and other.state in (
                    SubmissionState.QUEUED.value,
                    SubmissionState.SUBMITTING.value,
                )
                for other in self._store.list(
                    PlatformSubmission,
                    competition_challenge_id=challenge.challenge_id,
                )
            )
            if (not active_submission
                    and challenge.state == ChallengeState.SUBMITTING.value):
                self._transition_challenge(
                    challenge,
                    ChallengeState.RUNNING,
                    {"reason": "multi_flag_partial_verdict",
                     "confirmed_flags": len(solved_digests),
                     "expected_flags": expected},
                )
                self._store.save(challenge.model_copy(update={
                    "state": ChallengeState.RUNNING.value,
                    "paused_from": None,
                }))
            return submission
        current = ChallengeState(challenge.state)
        if current is not ChallengeState.SOLVED:
            ensure_challenge_transition(current, ChallengeState.SOLVED)
            self._transition_challenge(
                challenge, ChallengeState.SOLVED,
                {"receipt": receipt,
                 "remote_state": "solved_remote"})
            self._store.save(challenge.model_copy(update={
                "state": ChallengeState.SOLVED.value,
                "remote_state": "solved_remote",
                "paused_from": None,
            }))
        await self._settle_binding_solved(challenge)
        await self._release_lease(challenge)
        return submission

    async def _settle_binding_solved(
        self, challenge: CompetitionChallenge
    ) -> None:
        """结束仍在运行的绑定；已正常终态的 Run 保持原记录（设计 12.3）。"""
        binding = self._store.active_binding_for_challenge(
            challenge.challenge_id)
        if binding is None:
            return
        state = binding.binding_state()
        gateway_stop_failed = ""
        if self._gateway is not None and state in (
                BINDING_ACTIVE_STATES - {BindingState.PLANNED}):
            try:
                snapshot = await self._gateway.snapshot(binding.run_id)
                if int(snapshot.generation) != int(binding.execution_generation):
                    raise RuntimeError(
                        "stale run binding while settling solved challenge: "
                        f"binding generation {binding.execution_generation}, "
                        f"run generation {snapshot.generation}"
                    )
                if snapshot.state == "running":
                    control_generation = int(
                        (snapshot.detail or {}).get("control_generation") or 0
                    )
                    material = (
                        f"{binding.binding_id}:{binding.execution_generation}:"
                        f"{control_generation}:stop"
                    )
                    command_id = (
                        "cmd-competition-"
                        + hashlib.sha256(material.encode("utf-8")).hexdigest()[:32]
                    )
                    receipt = await self._gateway.command(
                        binding.run_id,
                        RunCommand(
                            command_type="stop",
                            command_id=command_id,
                            expected_generation=control_generation,
                        ),
                    )
                    if receipt.error is not None:
                        raise RuntimeError(
                            f"{receipt.error.code}: {receipt.error.message}"
                        )
            except Exception as exc:  # stop 失败不阻塞判对投影，留痕
                gateway_stop_failed = str(exc)
        extra = (
            {"gateway_stop_error": gateway_stop_failed}
            if gateway_stop_failed else {}
        )
        # ACTIVE/LOCAL_FINISHED/REMOTE_PENDING 走 solved；创建在途与
        # paused/rejected/resolving 走 stopped（状态机不允许直接 solved）。
        solved_path = {
            BindingState.ACTIVE: (
                BindingState.LOCAL_FINISHED, BindingState.REMOTE_PENDING,
                BindingState.SOLVED),
            BindingState.LOCAL_FINISHED: (
                BindingState.REMOTE_PENDING, BindingState.SOLVED),
            BindingState.REMOTE_PENDING: (BindingState.SOLVED,),
        }
        path = solved_path.get(state, (BindingState.STOPPED,))
        for target in path:
            binding = self._transition_binding(binding, target, extra)
            self._store.save(binding)

    async def _release_lease(self, challenge: CompetitionChallenge) -> None:
        if self._leases is None:
            return
        try:
            await self._leases.release(
                challenge_id=challenge.challenge_id, reason="challenge_solved")
        except (NotFoundError, StateConflictError):
            pass  # 无活动租约 / 已释放：幂等

    # -- wrong --------------------------------------------------------------

    async def _handle_wrong(
        self, submission: PlatformSubmission, *, receipt: str
    ) -> PlatformSubmission:
        """远端判错：写 wrong 事件 → 投影 Run 持久否决 → resolve 新执行代
        （设计 12.2，逐字顺序）。"""
        submission = self._transition_submission(
            submission, SubmissionState.WRONG, {"receipt": receipt})
        submission = self._store.save(
            submission.model_copy(update={
                "remote_receipt": receipt,
                "last_error": "",
            }))
        candidate = self._store.get(SubmissionCandidate, submission.candidate_id)
        if candidate is not None and candidate.candidate_state() in (
                CandidateState.APPROVED,):
            self._transition_candidate(candidate, CandidateState.SUBMITTED)
            self._store.save(candidate.model_copy(
                update={"state": CandidateState.SUBMITTED.value}))
        challenge = self._store.get(
            CompetitionChallenge, submission.competition_challenge_id)
        # 持久否决投影：经 SharedGraph flag invalidation 通道（dedupe_key
        # 幂等）；图不可用时由 reconciler 的水位投影器补齐。
        if candidate is not None:
            self.project_invalidation(candidate, receipt=receipt)
        if challenge is None:
            return submission
        # 题目随 resolve 回到 running（设计 9.1 注释）。
        if challenge.state == ChallengeState.SUBMITTING.value:
            self._transition_challenge(challenge, ChallengeState.RUNNING)
            self._store.save(challenge.model_copy(
                update={"state": ChallengeState.RUNNING.value}))
        # 远端判错后按策略 resolve 同一 Run、递增 execution generation：
        # local_finished/remote_pending 先落 rejected，再经
        # resolve_execution 进入新执行代（设计 9.3 规则 7）。
        binding = self._store.active_binding_for_challenge(
            challenge.challenge_id)
        if binding is None or self._binding is None:
            return submission
        source_generation = (
            int(candidate.source_execution_generation or 0)
            if candidate is not None else 0
        )
        if (source_generation > 0
                and source_generation < int(binding.execution_generation or 0)):
            # This verdict belongs to an environment that has already been
            # replaced.  The instance lifecycle already dispatched the newer
            # execution generation, so do not interrupt it with another
            # resolve.  The old candidate invalidation above is still kept.
            return submission
        state = binding.binding_state()
        if state is BindingState.LOCAL_FINISHED:
            binding = self._transition_binding(
                binding, BindingState.REMOTE_PENDING)
            self._store.save(binding)
            state = binding.binding_state()
        if state is BindingState.REMOTE_PENDING:
            binding = self._transition_binding(binding, BindingState.REJECTED)
            self._store.save(binding)
            state = binding.binding_state()
        if state is BindingState.ACTIVE:
            # 本地 Run 在产出候选 Flag 后会进入结束收尾，远端
            # wrong 回执可能恰好早于 asyncio task 完全退出。直接
            # resolve 会因旧任务仍存活而被拒绝。先 park 并等待
            # stop effect，再在同一 run_id/工作区上启动新执行代。
            binding = await self._binding.pause(
                challenge.challenge_id, reason="submission_wrong") or binding
            state = binding.binding_state()
        if state in (BindingState.REJECTED, BindingState.PAUSED):
            await self._binding.resolve_execution(
                challenge.challenge_id, reason="submission_wrong")
        return submission

    def project_invalidation(
        self,
        candidate: SubmissionCandidate,
        *,
        receipt: str = "",
    ) -> bool:
        """把候选值投影为来源 Run 的持久 Flag 否决。返回是否已写入。

        经 ``reopen_after_false_positive``（内部 ``flaginvalid::<flag>``
        dedupe_key）：重复投影不重复写否决。事件 payload 只含 digest。
        """
        if self._shared_graph_for is None:
            return False
        graph = self._shared_graph_for(candidate.source_run_id)
        if graph is None:
            return False
        graph.reopen_after_false_positive(
            actor="competition",
            flag=candidate.value,
            reason=(
                f"remote verdict wrong (submission of digest "
                f"{candidate.digest[:12]}): {receipt[:200]}"
            ),
        )
        self._store.append_events([ev.make_event(
            competition_id=candidate.competition_id,
            aggregate_type=ev.AGG_SUBMISSION,
            aggregate_id=candidate.candidate_id,
            event_type=FLAG_INVALIDATION_PROJECTED,
            payload={
                "candidate_id": candidate.candidate_id,
                "digest": candidate.digest,
                "source_run_id": candidate.source_run_id,
                "challenge_id": candidate.competition_challenge_id,
            },
        )])
        return True

    # -- rate_limited / auth_failed ------------------------------------------

    def _handle_rate_limited(
        self,
        submission: PlatformSubmission,
        connection: PlatformConnection,
        exc: PlatformRateLimitedError,
    ) -> PlatformSubmission:
        moment = self._now()
        delay = exc.retry_after_seconds
        if delay is None:
            delay = self._connection_rate_cooldown(connection)
        if delay is None:
            delay = self._config.default_retry_after_seconds
        retry_at = moment + timedelta(seconds=float(delay))
        submission = self._transition_submission(
            submission, SubmissionState.RATE_LIMITED,
            {"retry_after_at": retry_at.isoformat(),
             "retry_after_source": (
                 "response_header" if exc.retry_after_seconds is not None
                 else "connection_default")},
        )
        submission.retry_after_at = retry_at
        # 连接级平台冷却：保存到 capabilities，暂停期间该连接的提交全跳过。
        self._write_connection_cooldown(connection, retry_at)
        return self._store.save(submission)

    def _handle_auth_required(
        self,
        submission: PlatformSubmission,
        connection: PlatformConnection,
        exc: PlatformAuthRequiredError,
    ) -> PlatformSubmission:
        """auth_failed：连接暂停外部动作；已有本地 Run 不做任何停止。"""
        submission = self._transition_submission(
            submission, SubmissionState.AUTH_REQUIRED,
            {"error": str(exc)})
        submission = self._store.save(
            submission.model_copy(update={"last_error": str(exc)}))
        if connection.status != ConnectionStatus.AUTH_REQUIRED.value:
            with self._store.lock, self._store.conn:
                self._store.save(connection.model_copy(update={
                    "status": ConnectionStatus.AUTH_REQUIRED.value,
                    "last_error": str(exc),
                }))
                self._store.append_events([ev.make_event(
                    competition_id=submission.competition_id,
                    aggregate_type=ev.AGG_CONNECTION,
                    aggregate_id=connection.connection_id,
                    event_type=CONNECTION_STATUS_CHANGED,
                    payload={
                        "connection_id": connection.connection_id,
                        "from": connection.status,
                        "to": ConnectionStatus.AUTH_REQUIRED.value,
                        "reason": "submission_auth_failed",
                    },
                )])
        return submission

    # ------------------------------------------------------------------
    # unknown 核对（reconciler 步骤 9；禁止直接重发，先查远端状态）
    # ------------------------------------------------------------------

    async def reconcile_unknown(
        self,
        *,
        competition_id: Optional[str] = None,
    ) -> dict[str, list[str]]:
        """核对所有 unknown 提交的远端状态。

        Adapter 暴露 ``poll_submission``（GZCTF 异步判定形态，远端提交 id
        在 ``remote_receipt``，game/challenge 由题目 external id 拆出）或
        ``reconcile_submission`` 时查询；判对/判错/duplicate 落对应终态并
        执行各自的投影；仍 pending 或无核对能力的保持 unknown 等 Operator。
        """
        report: dict[str, list[str]] = {
            "correct": [], "wrong": [], "duplicate": [],
            "still_unknown": [], "awaiting_operator": [], "requeued": [],
            "error": [],
        }
        for submission in self._store.list(
                PlatformSubmission, state=SubmissionState.UNKNOWN.value):
            if competition_id and submission.competition_id != competition_id:
                continue
            sid = submission.submission_id
            # Legacy rows created while an extension was disabled are known to
            # have failed inside the host before the adapter process received
            # the request.  They are safe to move into the bounded retry path.
            if submission.last_error.startswith(
                    "unexpected: extension capability is disabled:"):
                recovered = self._transition_submission(
                    submission,
                    SubmissionState.TRANSIENT_FAILURE,
                    {"reason": "legacy_pre_dispatch_extension_unavailable"},
                )
                self._store.save(recovered.model_copy(update={
                    "last_error": "extension unavailable before dispatch",
                }))
                report["requeued"].append(sid)
                continue
            challenge = self._store.get(
                CompetitionChallenge, submission.competition_challenge_id)
            connection = (
                self._connection_for(challenge) if challenge is not None
                else None
            )
            if challenge is None or connection is None:
                report["awaiting_operator"].append(sid)
                continue
            try:
                adapter = self._adapter(connection)
            except NotFoundError:
                report["awaiting_operator"].append(sid)
                continue
            try:
                result = await self._probe_remote(
                    adapter, challenge, submission)
            except PlatformTransportError as exc:
                submission = self._store.save(submission.model_copy(
                    update={"last_error": str(exc)}))
                report["error"].append(sid)
                continue
            if result is None:
                report["awaiting_operator"].append(sid)
                continue
            status = str(result.status or "")
            receipt = str(
                result.detail.get("remote_receipt")
                or result.detail.get("answer_result") or status)
            if status == "correct":
                submission = self._transition_submission(
                    submission, SubmissionState.CORRECT,
                    {"reconciled": True, "receipt": receipt})
                submission = self._store.save(submission.model_copy(
                    update={"remote_receipt": receipt, "last_error": ""}))
                self._mark_reconciled(submission, "correct")
                await self._apply_correct_effects(submission, receipt=receipt)
                report["correct"].append(sid)
            elif status == "incorrect":
                self._mark_reconciled(submission, "wrong")
                await self._handle_wrong(submission, receipt=receipt)
                report["wrong"].append(sid)
            elif status in ("duplicate", "duplicate_or_solved"):
                submission = self._transition_submission(
                    submission, SubmissionState.DUPLICATE_OR_SOLVED,
                    {"reconciled": True, "receipt": receipt})
                submission = self._store.save(submission.model_copy(
                    update={"remote_receipt": receipt, "last_error": ""}))
                self._mark_reconciled(submission, "duplicate")
                await self._apply_correct_effects(submission, receipt=receipt)
                report["duplicate"].append(sid)
            else:
                report["still_unknown"].append(sid)  # 远端仍未判定
        return report

    async def _probe_remote(
        self,
        adapter: Any,
        challenge: CompetitionChallenge,
        submission: PlatformSubmission,
    ) -> Optional[Any]:
        """查询远端提交状态；Adapter 无核对能力时返回 None（等 Operator）。"""
        poll = getattr(adapter, "poll_submission", None)
        remote_id = (submission.remote_receipt or "").strip()
        if poll is not None and remote_id:
            parts = str(challenge.external_challenge_id).split("/", 1)
            if len(parts) == 2 and all(parts):
                return await poll(parts[0], parts[1], remote_id)
        reconcile = getattr(adapter, "reconcile_submission", None)
        if reconcile is not None:
            return await reconcile({
                "connection_id": connection_id_of(challenge, self._store),
                "challenge_key": challenge.external_challenge_id,
                "submission_id": submission.submission_id,
                "remote_submission_id": remote_id,
                "digest": submission.digest,
            })
        return None

    def _mark_reconciled(
        self, submission: PlatformSubmission, verdict: str
    ) -> None:
        self._store.append_events([ev.make_event(
            competition_id=submission.competition_id,
            aggregate_type=ev.AGG_SUBMISSION,
            aggregate_id=submission.submission_id,
            event_type=SUBMISSION_RECONCILED,
            payload={
                "submission_id": submission.submission_id,
                "verdict": verdict,
            },
        )])

    # ------------------------------------------------------------------
    # 崩溃补齐：判对/判错实体对齐（reconciler 步骤 7 的 competition.db 侧）
    # ------------------------------------------------------------------

    async def align_verdicts(self) -> dict[str, int]:
        """补齐 wrong/correct 的实体投影（事件已落库但实体对齐被崩溃打断）。

        - correct/duplicate_or_solved 而题目未 solved：重放判对投影
          （幂等：已 solved 直接跳过）；
        - wrong 而题目还停在 submitting：题目回 running。
        """
        aligned = {"correct": 0, "wrong": 0}
        for submission in self._store.list(PlatformSubmission):
            state = submission.submission_state()
            if state in (SubmissionState.CORRECT,
                         SubmissionState.DUPLICATE_OR_SOLVED):
                challenge = self._store.get(
                    CompetitionChallenge,
                    submission.competition_challenge_id)
                if (challenge is not None
                        and challenge.state != ChallengeState.SOLVED.value):
                    await self._apply_correct_effects(
                        submission, receipt=submission.remote_receipt)
                    aligned["correct"] += 1
            elif state is SubmissionState.WRONG:
                challenge = self._store.get(
                    CompetitionChallenge,
                    submission.competition_challenge_id)
                if (challenge is not None
                        and challenge.state == ChallengeState.SUBMITTING.value):
                    self._transition_challenge(
                        challenge, ChallengeState.RUNNING)
                    self._store.save(challenge.model_copy(
                        update={"state": ChallengeState.RUNNING.value}))
                    aligned["wrong"] += 1
        return aligned

    # ------------------------------------------------------------------
    # 自动提交闸（autonomous）：来源已由 Gate 保证，这里查策略/预算/冷却
    # ------------------------------------------------------------------

    def _auto_submit_allowed(
        self,
        challenge: CompetitionChallenge,
        connection: Optional[PlatformConnection],
    ) -> tuple[bool, str]:
        if connection is None:
            return False, "connection_missing"
        if connection.status != ConnectionStatus.ACTIVE.value:
            return False, f"connection_{connection.status}"
        caps = connection.capabilities or {}
        detail = caps.get("detail") if isinstance(caps.get("detail"), dict) else {}
        if caps.get("automation_allowed") is False or (
                detail.get("automation_allowed") is False):
            return False, "automation_not_allowed"
        blocked, reason = self._submit_gates(None, connection, self._now(),
                                             competition_id=challenge.competition_id)
        if blocked:
            return False, reason
        return True, ""

    def _submit_gates(
        self,
        submission: Optional[PlatformSubmission],
        connection: PlatformConnection,
        moment: datetime,
        *,
        competition_id: Optional[str] = None,
    ) -> tuple[bool, str]:
        """冷却与预算闸；返回 (是否阻止, 原因)。"""
        competition_id = competition_id or (
            submission.competition_id if submission else "")
        cooldown_until = _parse_iso(
            (connection.capabilities or {}).get(CONNECTION_COOLDOWN_KEY))
        if cooldown_until is not None and moment < cooldown_until:
            return True, "connection_cooldown"
        policy = (
            self._store.get(CompetitionPolicy, competition_id)
            if competition_id else None
        )
        cooldown_s = (
            float(policy.submission_cooldown_seconds)
            if policy is not None else 0.0
        )
        if cooldown_s > 0 and competition_id:
            last = self._last_attempt_at(competition_id)
            if (last is not None
                    and (moment - last).total_seconds() < cooldown_s):
                return True, "policy_cooldown"
        if competition_id and self._budget_exhausted(competition_id):
            return True, "submission_budget_exhausted"
        return False, ""

    def _last_attempt_at(self, competition_id: str) -> Optional[datetime]:
        """该比赛最近一次实际提交时间（queued 不算尝试）。"""
        moments = [
            s.updated_at for s in self._store.list(
                PlatformSubmission, competition_id=competition_id)
            if s.state != SubmissionState.QUEUED.value
        ]
        return max(moments) if moments else None

    def _budget_exhausted(self, competition_id: str) -> bool:
        budget = self._store.get(ResourceBudget, competition_id, "submissions")
        return bool(
            budget is not None and budget.limit > 0
            and budget.used >= budget.limit
        )

    def _consume_budget(self, competition_id: str) -> None:
        """每次实际向平台发出提交消耗一格 submissions 预算（投影缓存）。"""
        budget = self._store.get(ResourceBudget, competition_id, "submissions")
        if budget is None:
            budget = ResourceBudget(
                competition_id=competition_id, kind="submissions")
        self._store.save(budget.model_copy(update={"used": budget.used + 1}))

    # ------------------------------------------------------------------
    # 连接 / 适配器 / 冷却参数
    # ------------------------------------------------------------------

    def _write_connection_cooldown(
        self, connection: PlatformConnection, until: datetime
    ) -> None:
        caps = dict(connection.capabilities or {})
        existing = _parse_iso(caps.get(CONNECTION_COOLDOWN_KEY))
        if existing is not None and existing >= until:
            return  # 已有更长的冷却：不缩短
        caps[CONNECTION_COOLDOWN_KEY] = until.isoformat()
        self._store.save(connection.model_copy(update={"capabilities": caps}))

    @staticmethod
    def _connection_rate_cooldown(
        connection: PlatformConnection
    ) -> Optional[float]:
        caps = connection.capabilities or {}
        detail = caps.get("detail") if isinstance(caps.get("detail"), dict) else {}
        rate = detail.get("rate_limit") if isinstance(
            detail.get("rate_limit"), dict) else {}
        try:
            value = float(rate.get("cooldown_seconds"))
            return value if value > 0 else None
        except (TypeError, ValueError):
            return None

    def _max_transient_retries(
        self, connection: Optional[PlatformConnection]
    ) -> int:
        if connection is not None:
            caps = connection.capabilities or {}
            detail = (
                caps.get("detail") if isinstance(caps.get("detail"), dict)
                else {}
            )
            for source in (caps, detail):
                try:
                    value = int(source.get("max_transient_retries"))
                    if value >= 0:
                        return value
                except (TypeError, ValueError):
                    continue
        return self._config.max_transient_retries_default

    def _transient_retry_count(self, submission: PlatformSubmission) -> int:
        """已发生的 transient_failure 次数（从事件流推导，不另存计数器）。"""
        return sum(
            1 for e in self._store.read_events(
                ev.AGG_SUBMISSION, submission.submission_id, limit=1000)
            if e.event_type == ev.SUBMISSION_STATE_CHANGED
            and e.payload.get("to") == SubmissionState.TRANSIENT_FAILURE.value
        )

    def _adapter(self, connection: PlatformConnection) -> Any:
        if self._adapter_for is not None:
            return self._adapter_for(connection)
        adapter = self._adapters.get(connection.platform_kind)
        if adapter is None:
            raise NotFoundError(
                f"no platform adapter for kind {connection.platform_kind!r}"
            )
        return adapter

    def _connection_for(
        self, challenge: Optional[CompetitionChallenge]
    ) -> Optional[PlatformConnection]:
        if challenge is None:
            return None
        competition = self._store.get(Competition, challenge.competition_id)
        if competition is None:
            return None
        return self._store.get(PlatformConnection, competition.connection_id)

    # ------------------------------------------------------------------
    # 读取 / 状态转移 / 事件（payload 一律不含候选原文）
    # ------------------------------------------------------------------

    def _challenge(self, challenge_id: str) -> CompetitionChallenge:
        challenge = self._store.get(CompetitionChallenge, challenge_id)
        if challenge is None:
            raise NotFoundError(f"challenge not found: {challenge_id}")
        return challenge

    def _submission(self, submission_id: str) -> PlatformSubmission:
        submission = self._store.get(PlatformSubmission, submission_id)
        if submission is None:
            raise NotFoundError(f"platform_submission not found: {submission_id}")
        return submission

    def _transition_candidate(
        self, candidate: SubmissionCandidate, target: CandidateState
    ) -> SubmissionCandidate:
        current = candidate.candidate_state()
        ensure_candidate_transition(current, target)
        return candidate.model_copy(update={"state": target.value})

    def _transition_submission(
        self,
        submission: PlatformSubmission,
        target: SubmissionState,
        extra: Optional[dict[str, Any]] = None,
    ) -> PlatformSubmission:
        current = submission.submission_state()
        ensure_submission_transition(current, target)
        updated = submission.model_copy(update={"state": target.value})
        self._store.append_events([ev.make_event(
            competition_id=submission.competition_id,
            aggregate_type=ev.AGG_SUBMISSION,
            aggregate_id=submission.submission_id,
            event_type=ev.SUBMISSION_STATE_CHANGED,
            payload={
                "submission_id": submission.submission_id,
                "candidate_id": submission.candidate_id,
                "challenge_id": submission.competition_challenge_id,
                "from": current.value,
                "to": target.value,
                "attempt": submission.attempt,
                **(extra or {}),
            },
        )])
        return updated

    def _transition_challenge(
        self,
        challenge: CompetitionChallenge,
        target: ChallengeState,
        extra: Optional[dict[str, Any]] = None,
        *,
        command: Optional[CommandEnvelope] = None,
    ) -> None:
        current = challenge.challenge_state()
        ensure_challenge_transition(
            current, target,
            paused_from=ChallengeState(challenge.paused_from)
            if challenge.paused_from else None)
        self._store.append_events([ev.make_event(
            competition_id=challenge.competition_id,
            aggregate_type=ev.AGG_CHALLENGE,
            aggregate_id=challenge.challenge_id,
            event_type=ev.CHALLENGE_STATE_CHANGED,
            command=command,
            payload={
                "challenge_id": challenge.challenge_id,
                "from": current.value,
                "to": target.value,
                **(extra or {}),
            },
        )])

    def _transition_binding(
        self,
        binding: Any,
        target: BindingState,
        extra: Optional[dict[str, Any]] = None,
    ) -> Any:
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
                **(extra or {}),
            },
        )])
        return updated

    def _append_candidate_event(
        self,
        challenge: CompetitionChallenge,
        digest: str,
        event_type: str,
        extra: dict[str, Any],
        *,
        command: Optional[CommandEnvelope] = None,
    ) -> None:
        self._store.append_events([ev.make_event(
            competition_id=challenge.competition_id,
            aggregate_type=AGG_CANDIDATE,
            aggregate_id=challenge.challenge_id,
            event_type=event_type,
            command=command,
            payload={
                "challenge_id": challenge.challenge_id,
                "digest": digest,
                **extra,
            },
        )])


def connection_id_of(
    challenge: CompetitionChallenge, store: CompetitionStore
) -> str:
    """题目 → 连接 id（reconcile 提示载荷用；缺失返回空串）。"""
    competition = store.get(Competition, challenge.competition_id)
    return competition.connection_id if competition is not None else ""


__all__ = [
    "AGG_CANDIDATE",
    "ALLOWED_SOURCE_KINDS",
    "CANDIDATE_REGISTERED",
    "CANDIDATE_REJECTED",
    "CANDIDATE_STATE_CHANGED",
    "CONNECTION_COOLDOWN_KEY",
    "CONNECTION_STATUS_CHANGED",
    "CandidateRejectedError",
    "DEFAULT_FLAG_FORMAT",
    "FLAG_INVALIDATION_PROJECTED",
    "REJECTED_SOURCE_KINDS",
    "SOURCE_ARTIFACT",
    "SOURCE_EXECUTION_OUTPUT",
    "SOURCE_RUN_EVENT",
    "SUBMISSION_RECONCILED",
    "SUBMISSION_REQUEUED",
    "SubmissionService",
    "SubmissionServiceConfig",
    "open_run_shared_graph",
]
