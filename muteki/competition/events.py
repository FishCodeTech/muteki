"""比赛领域事件类型与 envelope 工厂（设计 10，COMP-01）。

所有事件使用统一 ``EventEnvelope``（CORE-01）。命名空间约定：

- 事件类型一律以 ``competition.`` 前缀落在 CORE-01 已知命名空间内
  （``EventEnvelope`` 校验器只接受 core.*/run.*/competition.*/ctf.*/
  pentest.*/ext.<id>.*）。
- 平台提交事件统一使用 ``competition.platform_submission.*`` 中段
  （设计 10 的 ``platform_submission.*`` 在统一 envelope 下的落法），
  与 SharedGraph 已有的 ``flag_submission`` / ``flag_submission_decision``
  明确区分，互不混用。

payload 必须携带 ``competition_id``：``CompetitionStore.append_events``
会把它写入 ``competition_events.competition_id`` 独立列，供按比赛分页。
"""

from __future__ import annotations

from typing import Any, Optional

from muteki.platform.contracts.commands import CommandEnvelope
from muteki.platform.contracts.events import EventEnvelope

#: 比赛领域事件的生产者标识。
PRODUCER = "builtin.competition"

# -- 连接 / 比赛 / 同步 -------------------------------------------------------
CONNECTION_CREATED = "competition.connection.created"
CONNECTION_UNREGISTERED = "competition.connection.unregistered"
CONNECTION_TEST_REQUESTED = "competition.connection.test_requested"
CONNECTION_CREDENTIAL_UPDATED = "competition.connection.credential_updated"
CONNECTION_CREDENTIAL_REVOKED = "competition.connection.credential_revoked"
CONNECTION_BROWSER_SESSION_UPDATED = "competition.connection.browser_session_updated"
CONNECTION_BROWSER_SESSION_REVOKED = "competition.connection.browser_session_revoked"
COMPETITION_REGISTERED = "competition.registered"
COMPETITION_UNREGISTERED = "competition.unregistered"
SYNC_REQUESTED = "competition.sync.requested"

# -- 策略 / 调度器 -------------------------------------------------------------
POLICY_UPDATED = "competition.policy.updated"
SCHEDULER_STARTED = "competition.scheduler.started"
SCHEDULER_PAUSED = "competition.scheduler.paused"
SCHEDULER_RESUMED = "competition.scheduler.resumed"

# -- 题目 / revision -----------------------------------------------------------
CHALLENGE_DISCOVERED = "competition.challenge.discovered"
CHALLENGE_STATE_CHANGED = "competition.challenge.state_changed"
REVISION_CREATED = "competition.revision.created"

# -- 队列 ----------------------------------------------------------------------
QUEUE_ENQUEUED = "competition.queue.enqueued"

# -- 实例租约 -------------------------------------------------------------------
LEASE_REQUESTED = "competition.instance.lease_requested"
LEASE_RELEASE_REQUESTED = "competition.instance.release_requested"
LEASE_STATE_CHANGED = "competition.instance.state_changed"
# 配额超限拒绝（COMP-07；payload 不含地址/凭据，只含配额证据）。
LEASE_QUOTA_REJECTED = "competition.instance.quota_rejected"
# 重启 reconcile 结果未知：binding 降级（paused）并暂停新操作（设计 9.2/9.5）。
LEASE_RECONCILE_DEGRADED = "competition.instance.reconcile_degraded"

# -- RunBinding -----------------------------------------------------------------
BINDING_CREATED = "competition.run_binding.created"
BINDING_STATE_CHANGED = "competition.run_binding.state_changed"

# -- 远端提交（competition.platform_submission.*，见模块 docstring） -------------
SUBMISSION_APPROVED = "competition.platform_submission.approved"
SUBMISSION_QUEUED = "competition.platform_submission.queued"
SUBMISSION_RETRY_REQUESTED = "competition.platform_submission.retry_requested"
SUBMISSION_STATE_CHANGED = "competition.platform_submission.state_changed"

# -- 比赛聊天（COMP-09）：自然语言消息只记录为事件，不直接改状态 -----------------
MESSAGE_POSTED = "competition.message.posted"

#: 聚合类型名（competition_events.aggregate_type）。
AGG_COMPETITION = "competition"
AGG_CONNECTION = "platform_connection"
AGG_CHALLENGE = "competition_challenge"
AGG_BINDING = "run_binding"
AGG_LEASE = "instance_lease"
AGG_SUBMISSION = "platform_submission"


def make_event(
    *,
    competition_id: str,
    aggregate_type: str,
    aggregate_id: str,
    event_type: str,
    command: Optional[CommandEnvelope] = None,
    payload: Optional[dict[str, Any]] = None,
) -> EventEnvelope:
    """构造统一 EventEnvelope：命令上下文（command_id / correlation /
    idempotency_key / actor）自动贯穿，payload 强制带 competition_id。"""
    body = dict(payload or {})
    body.setdefault("competition_id", competition_id)
    return EventEnvelope(
        aggregate_type=aggregate_type,
        aggregate_id=aggregate_id,
        event_type=event_type,
        producer=PRODUCER,
        actor_id=(command.actor.id or "system") if command else "system",
        command_id=command.command_id if command else None,
        causation_id=command.command_id if command else None,
        correlation_id=(
            str(command.payload.get("correlation_id") or "").strip()
            or command.command_id
        ) if command else None,
        idempotency_key=command.idempotency_key if command else None,
        payload=body,
    )


__all__ = [
    "AGG_BINDING",
    "AGG_CHALLENGE",
    "AGG_COMPETITION",
    "AGG_CONNECTION",
    "AGG_LEASE",
    "AGG_SUBMISSION",
    "BINDING_CREATED",
    "BINDING_STATE_CHANGED",
    "CHALLENGE_DISCOVERED",
    "CHALLENGE_STATE_CHANGED",
    "COMPETITION_REGISTERED",
    "COMPETITION_UNREGISTERED",
    "CONNECTION_CREATED",
    "CONNECTION_UNREGISTERED",
    "CONNECTION_BROWSER_SESSION_REVOKED",
    "CONNECTION_BROWSER_SESSION_UPDATED",
    "CONNECTION_CREDENTIAL_REVOKED",
    "CONNECTION_CREDENTIAL_UPDATED",
    "CONNECTION_TEST_REQUESTED",
    "LEASE_QUOTA_REJECTED",
    "LEASE_RECONCILE_DEGRADED",
    "LEASE_RELEASE_REQUESTED",
    "LEASE_REQUESTED",
    "LEASE_STATE_CHANGED",
    "MESSAGE_POSTED",
    "POLICY_UPDATED",
    "PRODUCER",
    "QUEUE_ENQUEUED",
    "REVISION_CREATED",
    "SCHEDULER_PAUSED",
    "SCHEDULER_RESUMED",
    "SCHEDULER_STARTED",
    "SUBMISSION_APPROVED",
    "SUBMISSION_QUEUED",
    "SUBMISSION_RETRY_REQUESTED",
    "SUBMISSION_STATE_CHANGED",
    "SYNC_REQUESTED",
    "make_event",
]
