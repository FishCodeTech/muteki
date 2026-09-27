"""Competition Command Handler 与 CompetitionCommandApi（设计 10，COMP-01）。

命令类型（设计第 10 章，逐字对齐）：

    connection.create / connection.test
    competition.sync
    challenge.select / challenge.queue / challenge.skip
    policy.update
    scheduler.start / scheduler.pause / scheduler.resume
    instance.ensure / instance.stop
    run_binding.redirect / run_binding.pause / run_binding.resolve
    platform_submission.approve / platform_submission.retry
    competition.message

COMP-09 增补（能力目录 ``competition.*`` 前缀命令与查询，见
``capability_catalog`` 的比赛工具声明）：

    competition.connection.create（connection.create 别名）
    competition.policy.update（policy.update 别名）
    competition.challenge.queue（challenge.queue 别名）
    competition.submission.submit（登记候选并批准，一次命令完成提交入队）
    competition.list / competition.snapshot / competition.challenges /
    competition.submissions / competition.leases / connection.list（查询）

run.* 命名冲突的解决（COMP-09）：COMMAND-01 内置 ``RunCommandHandler``
已占有 ``run.pause`` / ``run.resolve``（聚合 run），而
``HandlerRegistry`` 只按 command_type 路由、重复注册即报错。比赛命令
操作的是 run_binding 聚合，因此把 COMP-01 的 ``run.redirect`` /
``run.pause`` / ``run.resolve`` 改名到 ``run_binding.*`` 命名空间，而不
在共享框架里引入 aggregate_type 二级路由：命令名如实反映目标聚合，
两个领域的 Handler 可以共存于同一 Registry，Web / Gateway / 各协议
Binding 也无需区分同名命令的两个语义。

实现口径：

- Handler 注册进共享 ``MutekiCommandAPI`` 的 ``HandlerRegistry``（COMMAND-01
  框架不改动；``CompetitionCommandApi`` 直接子类化 ``MutekiCommandApiImpl``，
  把存储鸭子类型换成 ``CompetitionStore``，因此 receipt / 事件 / outbox /
  水位全部落在 competition.db，满足设计 10「同一个 competition.db 事务」的
  归属要求；dispatch 的判定、幂等、expected_version 顺序完全复用共享实现）。
- 本包只实现纯状态机部分：不调用真实平台 Adapter 或 RunManager。需要外部
  副作用的命令（connection.test / competition.sync / instance.* / run.* /
  platform_submission.approve）在事务提交后的 side effect 里把实体状态落库，
  并向 ``competition_outbox`` 写一条 pending 记录（幂等键 = command_id，
  destination 标明消费方），供 COMP-03+ 的传输层 / SubmissionService 消费；
  这里不投递，故不使用共享 dispatch 的 ``outbox_destination`` 自动投递路径。
- 非法状态转移在 plan 阶段经 ``models.ensure_*_transition`` 拒绝，返回
  category=state 的准确错误（from/to 状态齐全）。
- 比赛命令 operator/system 全权；COMP-09 起 ``CompetitionCommandPolicy``
  可注入 ``binding_store``（CAP-01 的 PlatformStore）解析 agent 主体的
  CapabilityBinding，判定逻辑完全复用共享 ``CommandPolicy``。
"""

from __future__ import annotations

from datetime import datetime
from typing import Any, Optional
from urllib.parse import urlsplit

from muteki.platform.command_api import MutekiCommandApiImpl
from muteki.platform.command_handlers.base import (
    CommandFailed,
    CommandPlan,
    CommandPolicy,
    HandlerContext,
    SideEffectResult,
    correlation_id_of,
    make_error,
)
from muteki.platform.contracts.commands import (
    CommandEnvelope,
    QueryEnvelope,
    QueryResult,
)
from muteki.platform.contracts.base import utcnow
from muteki.platform.contracts.errors import ErrorCategory
from muteki.platform.contracts.receipts import (
    AggregateRef,
    CommandReceipt,
    EffectReceipt,
    EffectState,
    OutboxRecord,
    OutboxStatus,
    ReceiptState,
)
from muteki.competition import events as ev
from muteki.competition.models import (
    AutomationMode,
    BindingState,
    CandidateState,
    ChallengeRevision,
    ChallengeState,
    Competition,
    CompetitionChallenge,
    CompetitionPolicy,
    ConnectionStatus,
    IllegalTransitionError,
    InstanceLease,
    LeaseState,
    PlatformConnection,
    PlatformKind,
    PlatformSubmission,
    QueueEntryState,
    RunBinding,
    SchedulerQueueEntry,
    SchedulerState,
    SubmissionCandidate,
    SubmissionState,
    ensure_binding_transition,
    ensure_candidate_transition,
    ensure_challenge_transition,
    ensure_lease_transition,
    ensure_submission_transition,
    flag_digest,
)
from muteki.competition.store import CompetitionStore
from muteki.competition.submission import (
    AGG_CANDIDATE,
    CANDIDATE_REGISTERED,
    CandidateRejectedError,
)
from muteki.platform.contracts.events import EventEnvelope


# ---------------------------------------------------------------------------
# 错误与参数工具
# ---------------------------------------------------------------------------


#: 能力目录（任务书 10.11）声明的 ``competition.*`` 前缀命令 → COMP-01 命令
#: 的别名映射。两个名字共用同一个 Handler 与状态机；Handler 在 plan 入口
#: 归一化后分发，事件 payload / 错误里的 command_type 仍保留信封原值。
COMMAND_ALIASES: dict[str, str] = {
    "competition.connection.create": "connection.create",
    "competition.policy.update": "policy.update",
    "competition.challenge.queue": "challenge.queue",
}

#: 别名命令类型集合（注册与 /commands 端点的白名单共用）。
ALIAS_COMMAND_TYPES: frozenset[str] = frozenset(COMMAND_ALIASES)


def _canonical_type(command: CommandEnvelope) -> str:
    """命令类型的归一化视图（别名 → COMP-01 名）。"""
    return COMMAND_ALIASES.get(command.command_type, command.command_type)


def _fail(
    code: str,
    message: str,
    category: ErrorCategory,
    command: CommandEnvelope,
    *,
    recovery_hint: str = "",
) -> CommandFailed:
    return CommandFailed(make_error(
        code, message, category,
        correlation_id=correlation_id_of(command),
        recovery_hint=recovery_hint))


def _not_found(kind: str, ident: str, command: CommandEnvelope) -> CommandFailed:
    return _fail(
        f"competition.{kind}.not_found",
        f"unknown {kind} {ident!r}",
        ErrorCategory.NOT_FOUND, command)


def _illegal(command: CommandEnvelope, exc: IllegalTransitionError) -> CommandFailed:
    return _fail(
        f"competition.{exc.machine}.illegal_transition",
        f"illegal {exc.machine} transition: {exc.current} -> {exc.target}",
        ErrorCategory.STATE, command,
        recovery_hint="refresh snapshot and issue a command valid for the "
                      "current state")


def _require_str(payload: dict[str, Any], key: str, command: CommandEnvelope,
                 code_prefix: str) -> str:
    value = str(payload.get(key) or "").strip()
    if not value:
        raise _fail(
            f"{code_prefix}.{key}_required",
            f"{command.command_type} requires payload.{key}",
            ErrorCategory.VALIDATION, command)
    return value


def canonicalize_base_url(raw: str) -> str:
    """规范化平台地址：小写 scheme/host、保留端口、去路径尾斜杠。"""
    text = str(raw or "").strip()
    if not text:
        raise ValueError("base_url cannot be empty")
    parts = urlsplit(text if "://" in text else f"https://{text}")
    scheme = (parts.scheme or "https").lower()
    host = (parts.hostname or "").lower()
    if not host:
        raise ValueError(f"base_url has no host: {raw!r}")
    port = f":{parts.port}" if parts.port else ""
    return f"{scheme}://{host}{port}{parts.path.rstrip('/')}"


def _store_of(ctx: HandlerContext) -> CompetitionStore:
    store = ctx.store
    if not isinstance(store, CompetitionStore):
        raise CommandFailed(make_error(
            "competition.store.misconfigured",
            "competition handlers require a CompetitionStore-bound Command API",
            ErrorCategory.INTERNAL,
            correlation_id=ctx.correlation_id))
    return store


def _parse_dt(value: Any) -> Optional[datetime]:
    if value in (None, ""):
        return None
    if isinstance(value, datetime):
        return value
    return datetime.fromisoformat(str(value))


def _accepted(command: CommandEnvelope, aggregate_type: str, aggregate_id: str,
              *, run_id: Optional[str] = None) -> CommandReceipt:
    return CommandReceipt(
        command_id=command.command_id,
        state=ReceiptState.ACCEPTED,
        run_id=run_id,
        aggregate=AggregateRef(type=aggregate_type, id=aggregate_id),
    )


def _outbox_record(
    command: CommandEnvelope,
    *,
    aggregate_type: str,
    aggregate_id: str,
    event_type: str,
    destination: str,
    payload: dict[str, Any],
) -> OutboxRecord:
    """外部副作用的 outbox 记录（幂等键 = command_id，由 enqueue 默认规则保证）。"""
    return OutboxRecord(
        command_id=command.command_id,
        aggregate_type=aggregate_type,
        aggregate_id=aggregate_id,
        event_type=event_type,
        destination=destination,
        payload=payload,
    )


# ---------------------------------------------------------------------------
# connection.*：平台连接（真实 probe 属 COMP-02，这里落状态 + outbox）
# ---------------------------------------------------------------------------


class ConnectionCommandHandler:
    """connection.create / connection.test 命名空间属主。

    同时接受能力目录别名 ``competition.connection.create``（10.11）。
    """

    command_types = {
        "connection.create", "connection.test", "connection.unregister",
        "competition.connection.create",
        "connection.credential.set", "connection.credential.revoke",
        "connection.browser_session.import",
        "connection.browser_session.renew",
        "connection.browser_session.revoke",
    }

    async def plan(self, command: CommandEnvelope, ctx: HandlerContext) -> CommandPlan:
        ctype = _canonical_type(command)
        if ctype == "connection.create":
            return self._plan_create(command, ctx)
        if ctype == "connection.unregister":
            return self._plan_unregister(command, ctx)
        if command.command_type.startswith("connection.credential."):
            return self._plan_credential(command, ctx)
        if command.command_type.startswith("connection.browser_session."):
            return self._plan_browser_session(command, ctx)
        return self._plan_test(command, ctx)

    def _plan_browser_session(
        self, command: CommandEnvelope, ctx: HandlerContext
    ) -> CommandPlan:
        store = _store_of(ctx)
        payload = dict(command.payload)
        connection_id = _require_str(
            payload, "connection_id", command, "competition.connection")
        connection = store.get(PlatformConnection, connection_id)
        if connection is None:
            raise _not_found("connection", connection_id, command)
        if connection.platform_kind != PlatformKind.GENERIC_BROWSER.value:
            raise _fail(
                "competition.connection.browser_session_unsupported",
                "browser storage state is only valid for generic_browser connections",
                ErrorCategory.VALIDATION, command)
        revoke = command.command_type.endswith(".revoke")
        renew = command.command_type.endswith(".renew")
        state_ref = str(payload.get("state_ref") or "").strip()
        if not revoke and not renew and not state_ref.startswith("secret://platform/"):
            raise _fail(
                "competition.connection.browser_state_ref_invalid",
                "state_ref must be a one-time secret://platform reference",
                ErrorCategory.VALIDATION, command)
        factory = ctx.services.get("platform_adapter_factory")
        secrets = ctx.services.get("platform_secret_store")
        if factory is None or (not revoke and not renew and secrets is None):
            raise _fail(
                "competition.connection.browser_service_unavailable",
                "browser Adapter or SecretStore is unavailable",
                ErrorCategory.INTERNAL, command)
        event = ev.make_event(
            competition_id="",
            aggregate_type=ev.AGG_CONNECTION,
            aggregate_id=connection_id,
            event_type=(
                ev.CONNECTION_BROWSER_SESSION_REVOKED
                if revoke else ev.CONNECTION_BROWSER_SESSION_UPDATED
            ),
            command=command,
            payload={
                "connection_id": connection_id,
                "expires_at": str(payload.get("expires_at") or ""),
                "operation": command.command_type.rsplit(".", 1)[-1],
            },
        )

        async def _apply() -> SideEffectResult:
            adapter = factory.for_connection(connection)
            reference = factory.connection_ref(connection)
            if revoke:
                removed = adapter.revoke_storage_state(reference)
                status = {"present": False, "revoked": removed}
                updated_status = ConnectionStatus.AUTH_REQUIRED.value
            elif renew:
                status = adapter.renew_storage_state(
                    reference,
                    expires_at=str(payload.get("expires_at") or ""),
                )
                updated_status = ConnectionStatus.ACTIVE.value
            else:
                try:
                    serialized = secrets.resolve(state_ref)
                    status = adapter.import_storage_state(
                        reference, serialized,
                        expires_at=str(payload.get("expires_at") or ""),
                    )
                finally:
                    try:
                        secrets.delete(state_ref)
                    except Exception:
                        pass
                updated_status = ConnectionStatus.ACTIVE.value
            store.save(connection.model_copy(update={
                "status": updated_status,
                "last_error": "" if not revoke else "browser session revoked",
                "updated_at": utcnow(),
            }))
            return SideEffectResult(output={
                "connection_id": connection_id,
                "browser_session": status,
            })

        return CommandPlan(
            events=[event],
            receipt=_accepted(command, ev.AGG_CONNECTION, connection_id),
            side_effect=_apply,
        )

    def _plan_credential(
        self, command: CommandEnvelope, ctx: HandlerContext
    ) -> CommandPlan:
        """更新连接保存的 opaque 引用；Secret 本体由产品 SecretStore 管理。"""
        store = _store_of(ctx)
        payload = dict(command.payload)
        connection_id = _require_str(
            payload, "connection_id", command, "competition.connection")
        connection = store.get(PlatformConnection, connection_id)
        if connection is None:
            raise _not_found("connection", connection_id, command)
        revoke = command.command_type == "connection.credential.revoke"
        reference = str(payload.get("credential_ref") or "").strip()
        if not revoke and not reference.startswith("secret://platform/"):
            raise _fail(
                "competition.connection.credential_ref_invalid",
                "credential_ref must use the secret://platform namespace",
                ErrorCategory.VALIDATION, command)
        if revoke and reference and reference != connection.credential_ref:
            raise _fail(
                "competition.connection.credential_ref_conflict",
                "the credential reference no longer matches this connection",
                ErrorCategory.CONFLICT, command)
        updated = connection.model_copy(update={
            "credential_ref": "" if revoke else reference,
            "status": (
                ConnectionStatus.AUTH_REQUIRED.value
                if revoke else ConnectionStatus.ACTIVE.value
            ),
            "last_error": (
                "credential revoked" if revoke else ""
            ),
            "updated_at": utcnow(),
        })
        event = ev.make_event(
            competition_id="",
            aggregate_type=ev.AGG_CONNECTION,
            aggregate_id=connection_id,
            event_type=(
                ev.CONNECTION_CREDENTIAL_REVOKED
                if revoke else ev.CONNECTION_CREDENTIAL_UPDATED
            ),
            command=command,
            payload={
                "connection_id": connection_id,
                "credential_configured": not revoke,
            },
        )

        async def _apply() -> SideEffectResult:
            with store.lock, store.conn:
                store.save(updated)
            factory = ctx.services.get("platform_adapter_factory")
            if factory is not None:
                factory.invalidate(connection_id)
            return SideEffectResult(output={
                "connection_id": connection_id,
                "credential_configured": not revoke,
            })

        return CommandPlan(
            events=[event],
            receipt=_accepted(command, ev.AGG_CONNECTION, connection_id),
            side_effect=_apply,
        )

    def _plan_create(self, command: CommandEnvelope, ctx: HandlerContext) -> CommandPlan:
        store = _store_of(ctx)
        payload = dict(command.payload)
        kind = _require_str(payload, "platform_kind", command, "competition.connection")
        builtin = {k.value for k in PlatformKind}
        factory = ctx.services.get("platform_adapter_factory")
        if kind not in builtin:
            supported = False
            if factory is not None and hasattr(factory, "is_supported_kind"):
                supported = bool(factory.is_supported_kind(kind))
            if not supported:
                known = sorted(builtin)
                raise _fail(
                    "competition.connection.kind_unsupported",
                    f"platform_kind must be a builtin kind {known} or an "
                    f"enabled extension platform-adapter id: {kind!r}",
                    ErrorCategory.VALIDATION, command)
        try:
            base_url = canonicalize_base_url(
                _require_str(payload, "base_url", command, "competition.connection"))
        except ValueError as exc:
            raise _fail("competition.connection.base_url_invalid", str(exc),
                        ErrorCategory.VALIDATION, command) from exc
        account_key = _require_str(
            payload, "account_key", command, "competition.connection")
        # 凭据只保存 secret:// 引用（真实 secret 存储属 COMP-02）。
        credential_ref = str(payload.get("credential_ref") or "").strip()
        if credential_ref and not credential_ref.startswith("secret://"):
            raise _fail(
                "competition.connection.credential_ref_invalid",
                "credential_ref must be a secret:// reference; raw credentials "
                "are never persisted",
                ErrorCategory.VALIDATION, command)

        # 唯一身份 (platform_kind, canonical_base_url, account_key)：重复创建
        # 归一到已有连接（业务幂等），不生成第二行。
        existing = store.connection_by_identity(kind, base_url, account_key)
        connection = existing or PlatformConnection(
            platform_kind=kind,
            canonical_base_url=base_url,
            account_key=account_key,
            credential_ref=credential_ref,
        )
        if existing is not None:
            updates: dict[str, Any] = {}
            if credential_ref:
                updates["credential_ref"] = credential_ref
            if existing.archived:
                updates["archived"] = False
                updates["status"] = ConnectionStatus.ACTIVE.value
                updates["last_error"] = ""
            if updates:
                connection = existing.model_copy(update=updates)
            else:
                connection = existing

        event = ev.make_event(
            competition_id="",
            aggregate_type=ev.AGG_CONNECTION,
            aggregate_id=connection.connection_id,
            event_type=ev.CONNECTION_CREATED,
            command=command,
            payload={
                "connection_id": connection.connection_id,
                "platform_kind": kind,
                "canonical_base_url": base_url,
                "account_key": account_key,
                "credential_ref": credential_ref,
                "already_existed": existing is not None,
            },
        )

        async def _apply() -> SideEffectResult:
            with store.lock, store.conn:
                store.save(connection)
            return SideEffectResult()

        return CommandPlan(
            events=[event],
            receipt=_accepted(command, ev.AGG_CONNECTION, connection.connection_id),
            side_effect=_apply,
        )

    def _plan_unregister(
        self, command: CommandEnvelope, ctx: HandlerContext
    ) -> CommandPlan:
        store = _store_of(ctx)
        payload = dict(command.payload)
        connection_id = _require_str(
            payload, "connection_id", command, "connection.unregister")
        connection = store.get(PlatformConnection, connection_id)
        if connection is None:
            raise _not_found("connection", connection_id, command)
        if connection.archived:
            async def _noop() -> SideEffectResult:
                return SideEffectResult()

            return CommandPlan(
                events=[],
                receipt=_accepted(command, ev.AGG_CONNECTION, connection_id),
                side_effect=_noop,
            )

        bound = [
            row for row in store.list(Competition, connection_id=connection_id)
            if not row.archived
        ]
        if bound:
            raise _fail(
                "competition.connection.has_bound_competitions",
                f"connection {connection_id} still has {len(bound)} registered "
                "competition(s); remove them from the inventory first",
                ErrorCategory.STATE, command)

        secrets = ctx.services.get("platform_secret_store")
        factory = ctx.services.get("platform_adapter_factory")
        credential_ref = connection.credential_ref
        updated = connection.model_copy(update={
            "archived": True,
            "status": ConnectionStatus.DISABLED.value,
            "credential_ref": "",
            "last_error": "",
        })
        event = ev.make_event(
            competition_id="",
            aggregate_type=ev.AGG_CONNECTION,
            aggregate_id=connection_id,
            event_type=ev.CONNECTION_UNREGISTERED,
            command=command,
            payload={"connection_id": connection_id},
        )

        async def _apply() -> SideEffectResult:
            with store.lock, store.conn:
                store.save(updated)
            if secrets is not None:
                if credential_ref:
                    try:
                        secrets.delete(credential_ref)
                    except Exception:
                        pass
                try:
                    secrets.delete_connection(connection_id)
                except Exception:
                    pass
            if factory is not None and (
                connection.platform_kind == PlatformKind.GENERIC_BROWSER.value
            ):
                try:
                    adapter = factory.for_connection(connection)
                    adapter.revoke_storage_state(factory.connection_ref(connection))
                except Exception:
                    pass
            return SideEffectResult()

        return CommandPlan(
            events=[event],
            receipt=_accepted(command, ev.AGG_CONNECTION, connection_id),
            side_effect=_apply,
        )

    def _plan_test(self, command: CommandEnvelope, ctx: HandlerContext) -> CommandPlan:
        store = _store_of(ctx)
        connection_id = _require_str(
            dict(command.payload), "connection_id", command, "competition.connection")
        connection = store.get(PlatformConnection, connection_id)
        if connection is None:
            raise _not_found("connection", connection_id, command)
        event = ev.make_event(
            competition_id="",
            aggregate_type=ev.AGG_CONNECTION,
            aggregate_id=connection_id,
            event_type=ev.CONNECTION_TEST_REQUESTED,
            command=command,
            payload={"connection_id": connection_id},
        )
        record = _outbox_record(
            command,
            aggregate_type=ev.AGG_CONNECTION,
            aggregate_id=connection_id,
            event_type=ev.CONNECTION_TEST_REQUESTED,
            destination=f"platform.{connection.platform_kind}",
            payload={"op": "probe_connection", "connection_id": connection_id},
        )

        async def _apply() -> SideEffectResult:
            # 真实探测属 COMP-02；这里只留 outbox 记录供传输层消费。
            with store.lock, store.conn:
                store.outbox.enqueue(record)
            return SideEffectResult()

        return CommandPlan(
            events=[event],
            receipt=_accepted(command, ev.AGG_CONNECTION, connection_id),
            side_effect=_apply,
        )


# ---------------------------------------------------------------------------
# competition.sync / policy.update / scheduler.*
# ---------------------------------------------------------------------------


class CompetitionCommandHandler:
    """比赛登记 / 同步请求 / 策略 / 调度器开关。"""

    command_types = {
        "competition.sync",
        "competition.unregister",
        "policy.update",
        "competition.policy.update",
        "scheduler.start",
        "scheduler.pause",
        "scheduler.resume",
    }

    async def plan(self, command: CommandEnvelope, ctx: HandlerContext) -> CommandPlan:
        ctype = _canonical_type(command)
        if ctype == "competition.sync":
            return self._plan_sync(command, ctx)
        if ctype == "competition.unregister":
            return self._plan_unregister(command, ctx)
        if ctype == "policy.update":
            return self._plan_policy(command, ctx)
        return self._plan_scheduler(command, ctx)

    def _plan_sync(self, command: CommandEnvelope, ctx: HandlerContext) -> CommandPlan:
        store = _store_of(ctx)
        payload = dict(command.payload)
        connection_id = _require_str(
            payload, "connection_id", command, "competition.sync")
        external_id = _require_str(
            payload, "external_competition_id", command, "competition.sync")
        connection = store.get(PlatformConnection, connection_id)
        if connection is None:
            raise _not_found("connection", connection_id, command)

        # 唯一身份 (connection_id, external_competition_id)：重复 sync 归一。
        competition = store.competition_by_external(connection_id, external_id)
        is_new = competition is None
        if is_new:
            competition = Competition(
                connection_id=connection_id,
                external_competition_id=external_id,
                title=str(payload.get("title") or ""),
                description=str(payload.get("description") or ""),
                starts_at=_parse_dt(payload.get("starts_at")),
                ends_at=_parse_dt(payload.get("ends_at")),
            )
        elif competition.archived:
            competition = competition.model_copy(update={"archived": False})

        events = []
        if is_new:
            events.append(ev.make_event(
                competition_id=competition.competition_id,
                aggregate_type=ev.AGG_COMPETITION,
                aggregate_id=competition.competition_id,
                event_type=ev.COMPETITION_REGISTERED,
                command=command,
                payload={
                    "competition_id": competition.competition_id,
                    "connection_id": connection_id,
                    "external_competition_id": external_id,
                    "title": competition.title,
                },
            ))
        events.append(ev.make_event(
            competition_id=competition.competition_id,
            aggregate_type=ev.AGG_COMPETITION,
            aggregate_id=competition.competition_id,
            event_type=ev.SYNC_REQUESTED,
            command=command,
            payload={
                "competition_id": competition.competition_id,
                "connection_id": connection_id,
            },
        ))
        record = _outbox_record(
            command,
            aggregate_type=ev.AGG_COMPETITION,
            aggregate_id=competition.competition_id,
            event_type=ev.SYNC_REQUESTED,
            destination=f"platform.{connection.platform_kind}",
            payload={
                "op": "sync",
                "competition_id": competition.competition_id,
                "connection_id": connection_id,
            },
        )
        competition_id = competition.competition_id

        async def _apply() -> SideEffectResult:
            with store.lock, store.conn:
                store.save(competition)
                if store.get(CompetitionPolicy, competition_id) is None:
                    from muteki.competition.policy_profiles import merge_policy_hints
                    hints = {}
                    caps = connection.capabilities or {}
                    detail = caps.get("detail") if isinstance(caps, dict) else {}
                    if isinstance(detail, dict):
                        hints = dict(detail.get("policy_hints") or {})
                    profile = str(hints.get("policy_profile") or "").strip()
                    if not profile and connection.platform_kind not in {
                            k.value for k in PlatformKind}:
                        # 扩展平台默认建议测评档；builtin 保持 default。
                        profile = str(
                            hints.get("suggested_policy_profile") or "tsec_eval"
                        )
                    policy = CompetitionPolicy(competition_id=competition_id)
                    policy = merge_policy_hints(
                        policy, profile=profile or None, hints=hints)
                    store.save(policy)
                store.outbox.enqueue(record)
            return SideEffectResult()

        return CommandPlan(
            events=events,
            receipt=_accepted(command, ev.AGG_COMPETITION, competition_id),
            side_effect=_apply,
        )

    def _plan_unregister(
        self, command: CommandEnvelope, ctx: HandlerContext
    ) -> CommandPlan:
        store = _store_of(ctx)
        payload = dict(command.payload)
        competition_id = _require_str(
            payload, "competition_id", command, "competition.unregister")
        competition = store.get(Competition, competition_id)
        if competition is None:
            raise _not_found("competition", competition_id, command)
        if competition.archived:
            async def _noop() -> SideEffectResult:
                return SideEffectResult()

            return CommandPlan(
                events=[],
                receipt=_accepted(command, ev.AGG_COMPETITION, competition_id),
                side_effect=_noop,
            )

        previous_scheduler = competition.scheduler_state
        updated = competition.model_copy(update={
            "archived": True,
            "scheduler_state": SchedulerState.STOPPED.value,
        })
        event = ev.make_event(
            competition_id=competition_id,
            aggregate_type=ev.AGG_COMPETITION,
            aggregate_id=competition_id,
            event_type=ev.COMPETITION_UNREGISTERED,
            command=command,
            payload={
                "competition_id": competition_id,
                "previous_scheduler_state": previous_scheduler,
            },
        )

        async def _apply() -> SideEffectResult:
            with store.lock, store.conn:
                store.save(updated)
            return SideEffectResult()

        return CommandPlan(
            events=[event],
            receipt=_accepted(command, ev.AGG_COMPETITION, competition_id),
            side_effect=_apply,
        )

    def _plan_policy(self, command: CommandEnvelope, ctx: HandlerContext) -> CommandPlan:
        store = _store_of(ctx)
        payload = dict(command.payload)
        competition_id = _require_str(
            payload, "competition_id", command, "competition.policy")
        if store.get(Competition, competition_id) is None:
            raise _not_found("competition", competition_id, command)
        mode = str(payload.get("automation_mode") or "").strip()
        if mode and mode not in {m.value for m in AutomationMode}:
            raise _fail(
                "competition.policy.mode_unsupported",
                f"automation_mode must be one of "
                f"{sorted(m.value for m in AutomationMode)}: {mode!r}",
                ErrorCategory.VALIDATION, command)

        current = store.get(CompetitionPolicy, competition_id) or CompetitionPolicy(
            competition_id=competition_id)
        changes: dict[str, Any] = {}
        if mode:
            changes["automation_mode"] = mode
        for key in ("max_concurrent_runs", "max_instances", "working_set",
                    "keepalive_max", "dry_defer_waves", "deepchain_slots",
                    "stuck_waves_cap"):
            if payload.get(key) is not None:
                changes[key] = int(payload[key])
        if payload.get("fill_idle_revisits") is not None:
            changes["fill_idle_revisits"] = bool(payload["fill_idle_revisits"])
        if payload.get("terminal_phase_fill_idle") is not None:
            changes["terminal_phase_fill_idle"] = bool(
                payload["terminal_phase_fill_idle"]
            )
        for key in ("submission_cooldown_seconds", "visit_floor_s", "overdue_mult",
                    "keepalive_tail_s", "total_budget_s", "per_challenge_seconds"):
            if payload.get(key) is not None:
                changes[key] = float(payload[key])
        if payload.get("policy_profile") is not None:
            from muteki.competition.policy_profiles import (
                KNOWN_POLICY_PROFILES,
                resolve_policy_profile,
            )
            profile = str(payload.get("policy_profile") or "").strip() or "default"
            if profile not in KNOWN_POLICY_PROFILES:
                raise _fail(
                    "competition.policy.profile_unsupported",
                    f"policy_profile must be one of {sorted(KNOWN_POLICY_PROFILES)}: "
                    f"{profile!r}",
                    ErrorCategory.VALIDATION, command)
            changes["policy_profile"] = profile
            # 选用策略档时合并缺省字段（调用方可再覆盖单项）。
            for key, value in resolve_policy_profile(profile).items():
                if key == "policy_profile":
                    continue
                if key not in changes and payload.get(key) is None:
                    changes[key] = value
        if payload.get("round_timeboxes_s") is not None:
            changes["round_timeboxes_s"] = [
                int(x) for x in payload["round_timeboxes_s"]
            ]
        if payload.get("category_allow") is not None:
            changes["category_allow"] = [str(c) for c in payload["category_allow"]]
        if payload.get("category_deny") is not None:
            changes["category_deny"] = [str(c) for c in payload["category_deny"]]
        for key in (
            "challenge_order",
            "terminal_phase_challenge_ids",
            "persistent_challenge_ids",
        ):
            if payload.get(key) is not None:
                changes[key] = [
                    str(item).strip()
                    for item in payload[key]
                    if str(item).strip()
                ]
        if payload.get("budget_limits") is not None:
            changes["budget_limits"] = {
                str(k): float(v) for k, v in dict(payload["budget_limits"]).items()
            }
        policy = current.model_copy(update=changes)

        event = ev.make_event(
            competition_id=competition_id,
            aggregate_type=ev.AGG_COMPETITION,
            aggregate_id=competition_id,
            event_type=ev.POLICY_UPDATED,
            command=command,
            payload={"competition_id": competition_id, "changes": {
                k: (v if not isinstance(v, (list, dict)) else v)
                for k, v in changes.items()
            }},
        )

        async def _apply() -> SideEffectResult:
            with store.lock, store.conn:
                store.save(policy)
            return SideEffectResult()

        return CommandPlan(
            events=[event],
            receipt=_accepted(command, ev.AGG_COMPETITION, competition_id),
            side_effect=_apply,
        )

    def _plan_scheduler(self, command: CommandEnvelope, ctx: HandlerContext) -> CommandPlan:
        store = _store_of(ctx)
        payload = dict(command.payload)
        competition_id = _require_str(
            payload, "competition_id", command, "competition.scheduler")
        competition = store.get(Competition, competition_id)
        if competition is None:
            raise _not_found("competition", competition_id, command)
        current = SchedulerState(competition.scheduler_state)
        transitions = {
            "scheduler.start": (SchedulerState.STOPPED, SchedulerState.RUNNING,
                                ev.SCHEDULER_STARTED),
            "scheduler.pause": (SchedulerState.RUNNING, SchedulerState.PAUSED,
                                ev.SCHEDULER_PAUSED),
            "scheduler.resume": (SchedulerState.PAUSED, SchedulerState.RUNNING,
                                 ev.SCHEDULER_RESUMED),
        }
        expected, target, event_type = transitions[command.command_type]
        if current is not expected:
            raise _fail(
                "competition.scheduler.illegal_transition",
                f"{command.command_type} requires scheduler_state "
                f"{expected.value!r}, current is {current.value!r}",
                ErrorCategory.STATE, command)
        updated = competition.model_copy(
            update={"scheduler_state": target.value})
        event = ev.make_event(
            competition_id=competition_id,
            aggregate_type=ev.AGG_COMPETITION,
            aggregate_id=competition_id,
            event_type=event_type,
            command=command,
            payload={
                "competition_id": competition_id,
                "from": current.value,
                "to": target.value,
            },
        )

        async def _apply() -> SideEffectResult:
            with store.lock, store.conn:
                store.save(updated)
            return SideEffectResult()

        return CommandPlan(
            events=[event],
            receipt=_accepted(command, ev.AGG_COMPETITION, competition_id),
            side_effect=_apply,
        )


# ---------------------------------------------------------------------------
# challenge.select / challenge.queue / challenge.skip
# ---------------------------------------------------------------------------


def _load_challenge(
    store: CompetitionStore, payload: dict[str, Any], command: CommandEnvelope
) -> CompetitionChallenge:
    """按 challenge_id 或 (competition_id, external_challenge_id) 定位题目。"""
    challenge_id = str(payload.get("challenge_id") or "").strip()
    challenge: Optional[CompetitionChallenge] = None
    if challenge_id:
        challenge = store.get(CompetitionChallenge, challenge_id)
    else:
        competition_id = _require_str(
            payload, "competition_id", command, "competition.challenge")
        external_id = _require_str(
            payload, "external_challenge_id", command, "competition.challenge")
        challenge = store.challenge_by_external(competition_id, external_id)
    if challenge is None:
        raise _not_found(
            "challenge",
            challenge_id or str(payload.get("external_challenge_id") or ""),
            command)
    return challenge


class ChallengeCommandHandler:
    """challenge.select / challenge.queue / challenge.skip 命名空间属主。

    同时接受能力目录别名 ``competition.challenge.queue``（10.11）。
    """

    command_types = {
        "challenge.select", "challenge.queue", "challenge.skip",
        "competition.challenge.queue",
    }

    async def plan(self, command: CommandEnvelope, ctx: HandlerContext) -> CommandPlan:
        store = _store_of(ctx)
        payload = dict(command.payload)
        ctype = _canonical_type(command)
        challenge = _load_challenge(store, payload, command)
        if challenge.tombstoned:
            raise _fail(
                "competition.challenge.tombstoned",
                f"challenge {challenge.challenge_id} is tombstoned "
                "(removed upstream); historical revisions and bindings are kept",
                ErrorCategory.STATE, command)
        current = challenge.challenge_state()
        target = {
            "challenge.select": ChallengeState.SELECTED,
            "challenge.queue": ChallengeState.QUEUED,
            "challenge.skip": ChallengeState.SKIPPED,
        }[ctype]
        try:
            ensure_challenge_transition(
                current, target,
                paused_from=ChallengeState(challenge.paused_from)
                if challenge.paused_from else None)
        except IllegalTransitionError as exc:
            raise _illegal(command, exc) from exc

        updated = challenge.model_copy(update={
            "state": target.value,
            "paused_from": None,
        })
        event_payload: dict[str, Any] = {
            "challenge_id": challenge.challenge_id,
            "from": current.value,
            "to": target.value,
        }
        event_type = {
            "challenge.select": ev.CHALLENGE_STATE_CHANGED,
            "challenge.queue": ev.CHALLENGE_STATE_CHANGED,
            "challenge.skip": ev.CHALLENGE_STATE_CHANGED,
        }[ctype]
        events = [ev.make_event(
            competition_id=challenge.competition_id,
            aggregate_type=ev.AGG_CHALLENGE,
            aggregate_id=challenge.challenge_id,
            event_type=event_type,
            command=command,
            payload=event_payload,
        )]

        queue_entry: Optional[SchedulerQueueEntry] = None
        if ctype == "challenge.queue":
            existing_entry = store.get(
                SchedulerQueueEntry, challenge.competition_id,
                challenge.challenge_id)
            queue_entry = (existing_entry or SchedulerQueueEntry(
                competition_id=challenge.competition_id,
                competition_challenge_id=challenge.challenge_id,
            )).model_copy(update={
                "state": QueueEntryState.QUEUED.value,
                "priority": float(payload.get("priority") or 0.0),
            })
            events.append(ev.make_event(
                competition_id=challenge.competition_id,
                aggregate_type=ev.AGG_CHALLENGE,
                aggregate_id=challenge.challenge_id,
                event_type=ev.QUEUE_ENQUEUED,
                command=command,
                payload={
                    "challenge_id": challenge.challenge_id,
                    "priority": queue_entry.priority,
                },
            ))
        drop_queue = ctype == "challenge.skip"

        async def _apply() -> SideEffectResult:
            with store.lock, store.conn:
                store.save(updated)
                if queue_entry is not None:
                    store.save(queue_entry)
                if drop_queue:
                    entry = store.get(
                        SchedulerQueueEntry, updated.competition_id,
                        updated.challenge_id)
                    if entry is not None and entry.state not in (
                            QueueEntryState.DONE.value,
                            QueueEntryState.DROPPED.value):
                        store.save(entry.model_copy(
                            update={"state": QueueEntryState.DROPPED.value}))
            return SideEffectResult()

        return CommandPlan(
            events=events,
            receipt=_accepted(command, ev.AGG_CHALLENGE, challenge.challenge_id),
            side_effect=_apply,
        )


# ---------------------------------------------------------------------------
# instance.ensure / instance.stop
# ---------------------------------------------------------------------------


class InstanceCommandHandler:
    """动态实例租约命令；真实平台调用经 outbox 留给 COMP-03+。"""

    command_types = {"instance.ensure", "instance.stop"}

    async def plan(self, command: CommandEnvelope, ctx: HandlerContext) -> CommandPlan:
        if command.command_type == "instance.ensure":
            return self._plan_ensure(command, ctx)
        return self._plan_stop(command, ctx)

    def _plan_ensure(self, command: CommandEnvelope, ctx: HandlerContext) -> CommandPlan:
        store = _store_of(ctx)
        payload = dict(command.payload)
        challenge = _load_challenge(store, payload, command)
        competition = store.get(Competition, challenge.competition_id)
        if competition is None:
            raise _not_found("competition", challenge.competition_id, command)

        # 设计 7.1：同一道题最多一个活动租约。
        if store.active_lease_for_challenge(challenge.challenge_id) is not None:
            raise _fail(
                "competition.lease.active_exists",
                f"challenge {challenge.challenge_id} already has an active "
                "lease; stop it before ensuring a new one",
                ErrorCategory.CONFLICT, command)

        lease = InstanceLease(
            connection_id=competition.connection_id,
            competition_id=challenge.competition_id,
            competition_challenge_id=challenge.challenge_id,
            owner=str(payload.get("owner") or ""),
            ttl_seconds=int(payload.get("ttl_seconds") or 0),
        )
        # 题目随实例申请进入 provisioning（设计 9.1：queued → provisioning）。
        current = challenge.challenge_state()
        updated_challenge: Optional[CompetitionChallenge] = None
        if current is ChallengeState.QUEUED:
            updated_challenge = challenge.model_copy(
                update={"state": ChallengeState.PROVISIONING.value,
                        "paused_from": None})
        events = [ev.make_event(
            competition_id=challenge.competition_id,
            aggregate_type=ev.AGG_LEASE,
            aggregate_id=lease.lease_id,
            event_type=ev.LEASE_REQUESTED,
            command=command,
            payload={
                "lease_id": lease.lease_id,
                "challenge_id": challenge.challenge_id,
                "competition_id": challenge.competition_id,
            },
        )]
        if updated_challenge is not None:
            events.append(ev.make_event(
                competition_id=challenge.competition_id,
                aggregate_type=ev.AGG_CHALLENGE,
                aggregate_id=challenge.challenge_id,
                event_type=ev.CHALLENGE_STATE_CHANGED,
                command=command,
                payload={
                    "challenge_id": challenge.challenge_id,
                    "from": current.value,
                    "to": ChallengeState.PROVISIONING.value,
                },
            ))
        connection = store.get(PlatformConnection, competition.connection_id)
        record = _outbox_record(
            command,
            aggregate_type=ev.AGG_LEASE,
            aggregate_id=lease.lease_id,
            event_type=ev.LEASE_REQUESTED,
            destination=(
                f"platform.{connection.platform_kind}" if connection else ""),
            payload={
                "op": "ensure_instance",
                "lease_id": lease.lease_id,
                "challenge_id": challenge.challenge_id,
                "competition_id": challenge.competition_id,
                "connection_id": competition.connection_id,
            },
        )

        async def _apply() -> SideEffectResult:
            with store.lock, store.conn:
                store.save(lease)
                if updated_challenge is not None:
                    store.save(updated_challenge)
                store.outbox.enqueue(record)
            return SideEffectResult()

        return CommandPlan(
            events=events,
            receipt=_accepted(command, ev.AGG_LEASE, lease.lease_id),
            side_effect=_apply,
        )

    def _plan_stop(self, command: CommandEnvelope, ctx: HandlerContext) -> CommandPlan:
        store = _store_of(ctx)
        payload = dict(command.payload)
        lease_id = str(payload.get("lease_id") or "").strip()
        lease: Optional[InstanceLease] = None
        if lease_id:
            lease = store.get(InstanceLease, lease_id)
        else:
            challenge = _load_challenge(store, payload, command)
            lease = store.active_lease_for_challenge(challenge.challenge_id)
        if lease is None:
            raise _not_found("lease", lease_id or str(payload.get("challenge_id") or ""),
                             command)
        current = lease.lease_state()
        if current is LeaseState.RELEASING:
            target = LeaseState.RELEASING  # 幂等重入：重复 stop 不产生新动作
        elif current in (LeaseState.ACTIVE, LeaseState.RENEWING):
            # 释放经平台调用：releasing + outbox（任务书 10.6：释放幂等）。
            target = LeaseState.RELEASING
        elif current is LeaseState.REQUESTED:
            # 平台尚未交付实例：直接落 released，无需外部调用。
            target = LeaseState.RELEASED
        else:
            # provisioning 中平台调用在途，或已处终态：不允许 stop。
            raise _fail(
                "competition.lease.illegal_transition",
                f"instance.stop on lease {lease.lease_id} is not allowed "
                f"from state {current.value!r}",
                ErrorCategory.STATE, command)
        if target is not current:
            try:
                ensure_lease_transition(current, target)
            except IllegalTransitionError as exc:
                raise _illegal(command, exc) from exc

        updated = lease if target is current else lease.model_copy(
            update={"state": target.value})
        outbox_needed = target is LeaseState.RELEASING
        connection = store.get(PlatformConnection, lease.connection_id)
        events = [ev.make_event(
            competition_id=lease.competition_id,
            aggregate_type=ev.AGG_LEASE,
            aggregate_id=lease.lease_id,
            event_type=ev.LEASE_RELEASE_REQUESTED,
            command=command,
            payload={
                "lease_id": lease.lease_id,
                "from": current.value,
                "to": target.value,
            },
        )]
        record = _outbox_record(
            command,
            aggregate_type=ev.AGG_LEASE,
            aggregate_id=lease.lease_id,
            event_type=ev.LEASE_RELEASE_REQUESTED,
            destination=(
                f"platform.{connection.platform_kind}" if connection else ""),
            payload={"op": "stop_instance", "lease_id": lease.lease_id},
        )

        async def _apply() -> SideEffectResult:
            with store.lock, store.conn:
                store.save(updated)
                if outbox_needed:
                    store.outbox.enqueue(record)
            return SideEffectResult()

        return CommandPlan(
            events=events,
            receipt=_accepted(command, ev.AGG_LEASE, lease.lease_id),
            side_effect=_apply,
        )


# ---------------------------------------------------------------------------
# run_binding.redirect / run_binding.pause / run_binding.resolve
# （RunBinding；RunManager 调用留给 COMP-05）
# ---------------------------------------------------------------------------


def _load_binding(
    store: CompetitionStore, payload: dict[str, Any], command: CommandEnvelope
) -> RunBinding:
    binding_id = str(payload.get("binding_id") or "").strip()
    binding: Optional[RunBinding] = None
    if binding_id:
        binding = store.get(RunBinding, binding_id)
    else:
        challenge = _load_challenge(store, payload, command)
        binding = store.active_binding_for_challenge(challenge.challenge_id)
    if binding is None:
        raise _not_found(
            "run_binding", binding_id or str(payload.get("challenge_id") or ""),
            command)
    return binding


class RunBindingCommandHandler:
    """run_binding.redirect / run_binding.pause / run_binding.resolve 属主。

    只推进 RunBinding 状态机并写 outbox（destination=run_gateway，op 区分
    动作）；对 RunManager / RunGateway 的真实调用属 COMP-05。

    命名说明：COMP-01 曾使用 ``run.*`` 前缀，与 COMMAND-01 内置 Run 命令
    （聚合 run）的 command_type 冲突；COMP-09 起统一为 ``run_binding.*``
    （见模块 docstring）。
    """

    command_types = {
        "run_binding.redirect", "run_binding.pause", "run_binding.resolve"}

    async def plan(self, command: CommandEnvelope, ctx: HandlerContext) -> CommandPlan:
        store = _store_of(ctx)
        binding = _load_binding(store, dict(command.payload), command)
        current = binding.binding_state()
        target = {
            "run_binding.redirect": BindingState.RESOLVING,
            "run_binding.pause": BindingState.PAUSED,
            "run_binding.resolve": BindingState.RESOLVING,
        }[command.command_type]
        try:
            ensure_binding_transition(current, target)
        except IllegalTransitionError as exc:
            raise _illegal(command, exc) from exc

        updated = binding.model_copy(update={"state": target.value})
        event = ev.make_event(
            competition_id=binding.competition_id,
            aggregate_type=ev.AGG_BINDING,
            aggregate_id=binding.binding_id,
            event_type=ev.BINDING_STATE_CHANGED,
            command=command,
            payload={
                "binding_id": binding.binding_id,
                "run_id": binding.run_id,
                "challenge_id": binding.competition_challenge_id,
                "from": current.value,
                "to": target.value,
                "reason": command.command_type,
                "execution_generation": binding.execution_generation,
            },
        )
        record = _outbox_record(
            command,
            aggregate_type=ev.AGG_BINDING,
            aggregate_id=binding.binding_id,
            event_type=ev.BINDING_STATE_CHANGED,
            destination="run_gateway",
            payload={
                "op": command.command_type,
                "binding_id": binding.binding_id,
                "run_id": binding.run_id,
                "execution_generation": binding.execution_generation,
            },
        )

        async def _apply() -> SideEffectResult:
            with store.lock, store.conn:
                store.save(updated)
                store.outbox.enqueue(record)
            return SideEffectResult()

        return CommandPlan(
            events=[event],
            receipt=_accepted(command, ev.AGG_BINDING, binding.binding_id,
                              run_id=binding.run_id or None),
            side_effect=_apply,
        )


# ---------------------------------------------------------------------------
# platform_submission.approve / platform_submission.retry
# ---------------------------------------------------------------------------


class SubmissionCommandHandler:
    """远端提交命令；真实提交经 outbox 留给 COMP-08 SubmissionService。

    ``competition.submission.submit``（10.11 能力目录）：一次命令完成
    候选登记（幂等唯一键 challenge/slot/digest）+ 批准入队，与
    ``platform_submission.approve`` 共用同一条落库路径。
    """

    command_types = {
        "platform_submission.approve", "platform_submission.retry",
        "competition.submission.submit",
        "competition.submission.manual_override",
    }

    async def plan(self, command: CommandEnvelope, ctx: HandlerContext) -> CommandPlan:
        if command.command_type == "platform_submission.approve":
            return self._plan_approve(command, ctx)
        if command.command_type == "competition.submission.submit":
            return self._plan_submit(command, ctx)
        if command.command_type == "competition.submission.manual_override":
            return self._plan_manual_override(command, ctx)
        return self._plan_retry(command, ctx)

    def _plan_manual_override(
        self, command: CommandEnvelope, ctx: HandlerContext
    ) -> CommandPlan:
        """Operator 独立人工覆盖；要求重复答案和显式风险确认。"""
        if command.actor.kind != "operator" or not command.actor.id.strip():
            raise _fail(
                "competition.submission.manual_override_operator_required",
                "manual submission override requires an authenticated operator",
                ErrorCategory.PERMISSION,
                command,
            )
        store = _store_of(ctx)
        payload = dict(command.payload)
        challenge = _load_challenge(store, payload, command)
        answer = _require_str(
            payload, "answer", command, "competition.submission.manual_override")
        confirmation = _require_str(
            payload,
            "confirm_answer",
            command,
            "competition.submission.manual_override",
        )
        if confirmation != answer:
            raise _fail(
                "competition.submission.manual_override_confirmation_mismatch",
                "confirm_answer must exactly match answer",
                ErrorCategory.VALIDATION,
                command,
            )
        if payload.get("acknowledge_unverified") is not True:
            raise _fail(
                "competition.submission.manual_override_ack_required",
                "acknowledge_unverified must be true",
                ErrorCategory.VALIDATION,
                command,
            )
        service = ctx.services.get("submission_service")
        if service is None:
            raise _fail(
                "competition.submission.service_unavailable",
                "SubmissionService is not assembled",
                ErrorCategory.INTERNAL,
                command,
            )
        requested = ev.make_event(
            competition_id=challenge.competition_id,
            aggregate_type=AGG_CANDIDATE,
            aggregate_id=challenge.challenge_id,
            event_type="competition.submission.manual_override_requested",
            command=command,
            payload={
                "challenge_id": challenge.challenge_id,
                "answer_slot": int(payload.get("answer_slot") or 1),
                "digest": flag_digest(answer),
                "actor": command.actor.id,
                "acknowledged_unverified": True,
            },
        )

        async def _apply() -> SideEffectResult:
            try:
                candidate, submission = service.manual_override(
                    challenge.challenge_id,
                    answer,
                    answer_slot=int(payload.get("answer_slot") or 1),
                    actor=command.actor.id,
                    command=command,
                )
            except CandidateRejectedError as exc:
                return SideEffectResult(
                    state=ReceiptState.FAILED,
                    error=make_error(
                        f"competition.submission.{exc.reason}",
                        str(exc),
                        ErrorCategory.VALIDATION,
                        correlation_id=correlation_id_of(command),
                    ),
                )
            return SideEffectResult(output={
                "candidate_id": candidate.candidate_id,
                "submission_id": submission.submission_id,
                "manual_override": True,
            })

        return CommandPlan(
            events=[requested],
            receipt=_accepted(
                command, AGG_CANDIDATE, challenge.challenge_id),
            side_effect=_apply,
        )

    def _plan_approve(self, command: CommandEnvelope, ctx: HandlerContext) -> CommandPlan:
        store = _store_of(ctx)
        payload = dict(command.payload)
        candidate_id = _require_str(
            payload, "candidate_id", command, "competition.submission")
        candidate = store.get(SubmissionCandidate, candidate_id)
        if candidate is None:
            raise _not_found("submission_candidate", candidate_id, command)
        service = ctx.services.get("submission_service")
        if service is None:
            raise _fail(
                "competition.submission.service_unavailable",
                "SubmissionService is not assembled",
                ErrorCategory.INTERNAL, command)
        event = ev.make_event(
            competition_id=candidate.competition_id,
            aggregate_type=AGG_CANDIDATE,
            aggregate_id=candidate.candidate_id,
            event_type="competition.submission.approval_requested",
            command=command,
            payload={"candidate_id": candidate.candidate_id},
        )

        async def _apply() -> SideEffectResult:
            try:
                submission = service.approve_candidate(
                    candidate.candidate_id,
                    actor=command.actor.id or "operator",
                    command=command,
                )
            except Exception as exc:
                return SideEffectResult(
                    state=ReceiptState.FAILED,
                    error=make_error(
                        "competition.submission.approval_failed",
                        f"{type(exc).__name__}: {exc}",
                        ErrorCategory.STATE,
                        correlation_id=correlation_id_of(command),
                    ),
                )
            return SideEffectResult(output={
                "candidate_id": candidate.candidate_id,
                "submission_id": submission.submission_id,
            })

        return CommandPlan(
            events=[event],
            receipt=_accepted(command, AGG_CANDIDATE, candidate.candidate_id),
            side_effect=_apply,
        )

    def _plan_submit(self, command: CommandEnvelope, ctx: HandlerContext) -> CommandPlan:
        """登记经过独立 Gate 复核的候选；是否提交由策略与审批决定。"""
        store = _store_of(ctx)
        payload = dict(command.payload)
        challenge = _load_challenge(store, payload, command)
        if challenge.tombstoned:
            raise _fail(
                "competition.challenge.tombstoned",
                f"challenge {challenge.challenge_id} is tombstoned",
                ErrorCategory.STATE, command)
        answer = _require_str(payload, "answer", command, "competition.submission")
        answer_slot = int(payload.get("answer_slot") or 1)
        witness = _require_str(
            payload, "witness", command, "competition.submission")
        source_run_id = _require_str(
            payload, "source_run_id", command, "competition.submission")
        source_kind = _require_str(
            payload, "source_kind", command, "competition.submission")
        service = ctx.services.get("submission_service")
        if service is None:
            raise _fail(
                "competition.submission.service_unavailable",
                "SubmissionService is not assembled",
                ErrorCategory.INTERNAL, command)
        event = ev.make_event(
            competition_id=challenge.competition_id,
            aggregate_type=AGG_CANDIDATE,
            aggregate_id=challenge.challenge_id,
            event_type="competition.submission.registration_requested",
            command=command,
            payload={
                "challenge_id": challenge.challenge_id,
                "source_run_id": source_run_id,
                "source_kind": source_kind,
                "answer_slot": answer_slot,
                "digest": flag_digest(answer),
            },
        )

        async def _apply() -> SideEffectResult:
            try:
                candidate = service.register_candidate(
                    challenge.challenge_id,
                    answer,
                    source_run_id=source_run_id,
                    source_kind=source_kind,
                    witness=witness,
                    gate_verdict="confirmed",
                    answer_slot=answer_slot,
                    command=command,
                    source_execution_generation=int(
                        payload.get("execution_generation") or 0),
                    source_worker_id=str(payload.get("worker_id") or ""),
                    source_session_id=str(payload.get("session_id") or ""),
                    shared_graph_fact_id=str(
                        payload.get("shared_graph_fact_id") or ""),
                    witness_artifact_path=str(
                        payload.get("artifact_path") or ""),
                )
            except CandidateRejectedError as exc:
                return SideEffectResult(
                    state=ReceiptState.FAILED,
                    error=make_error(
                        f"competition.submission.{exc.reason}",
                        str(exc),
                        ErrorCategory.VALIDATION,
                        correlation_id=correlation_id_of(command),
                    ),
                )
            submissions = store.list(
                PlatformSubmission, candidate_id=candidate.candidate_id)
            submission = submissions[-1] if submissions else None
            return SideEffectResult(output={
                "candidate_id": candidate.candidate_id,
                "candidate_state": candidate.state,
                "submission_id": submission.submission_id if submission else "",
                "approval_required": (
                    candidate.state == CandidateState.AWAITING_APPROVAL.value),
            })

        return CommandPlan(
            events=[event],
            receipt=_accepted(
                command, AGG_CANDIDATE, challenge.challenge_id),
            side_effect=_apply,
        )

    def _approve_plan(
        self,
        command: CommandEnvelope,
        store: CompetitionStore,
        candidate: SubmissionCandidate,
        challenge: CompetitionChallenge,
        *,
        register_candidate: bool = False,
    ) -> CommandPlan:
        """approve / submit 共用的批准计划：候选 → approved，生成 queued
        提交 + 事件 + outbox，题目进入 submitting。"""
        if register_candidate:
            raise RuntimeError(
                "direct unverified candidate registration is disabled; "
                "use SubmissionService.register_candidate")
        cand_state = candidate.candidate_state()
        try:
            # assisted 默认等待确认（awaiting_approval）；observe/autonomous
            # 进入批准路径前的候选也允许经本命令批准。
            ensure_candidate_transition(cand_state, CandidateState.APPROVED)
        except IllegalTransitionError as exc:
            raise _illegal(command, exc) from exc

        # 幂等键 (competition_id, challenge, answer_slot, digest, attempt)：
        # attempt 取该候选值的下一个序号（设计 7.1）。
        attempt = store.next_submission_attempt(
            challenge.challenge_id, candidate.answer_slot, candidate.digest)
        submission = PlatformSubmission(
            competition_id=challenge.competition_id,
            competition_challenge_id=challenge.challenge_id,
            candidate_id=candidate.candidate_id,
            answer_slot=candidate.answer_slot,
            digest=candidate.digest,
            attempt=attempt,
        )

        # 题目：candidate_found → submitting（已在 submitting 时为允许的
        # 自循环：rate_limited 等待期间的再次批准）。
        current = challenge.challenge_state()
        try:
            ensure_challenge_transition(
                current, ChallengeState.SUBMITTING,
                paused_from=ChallengeState(challenge.paused_from)
                if challenge.paused_from else None)
        except IllegalTransitionError as exc:
            raise _illegal(command, exc) from exc
        updated_challenge = challenge.model_copy(update={
            "state": ChallengeState.SUBMITTING.value, "paused_from": None})
        updated_candidate = candidate.model_copy(
            update={"state": CandidateState.APPROVED.value})

        competition = store.get(Competition, challenge.competition_id)
        connection = (
            store.get(PlatformConnection, competition.connection_id)
            if competition is not None else None
        )
        events: list[EventEnvelope] = []
        if register_candidate:
            # submit 一次命令完成候选登记：事件只带 digest / 来源，绝无原文。
            events.append(ev.make_event(
                competition_id=challenge.competition_id,
                aggregate_type=AGG_CANDIDATE,
                aggregate_id=candidate.candidate_id,
                event_type=CANDIDATE_REGISTERED,
                command=command,
                payload={
                    "candidate_id": candidate.candidate_id,
                    "challenge_id": challenge.challenge_id,
                    "digest": candidate.digest,
                    "answer_slot": candidate.answer_slot,
                    "source_run_id": candidate.source_run_id,
                    "source_kind": candidate.source_ref,
                    "gate_verdict": candidate.gate_verdict,
                },
            ))
        events += [
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
                    "from": current.value,
                    "to": ChallengeState.SUBMITTING.value,
                },
            ),
        ]
        record = _outbox_record(
            command,
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

        async def _apply() -> SideEffectResult:
            with store.lock, store.conn:
                store.save(updated_candidate)
                store.save(submission)
                store.save(updated_challenge)
                store.outbox.enqueue(record)
            return SideEffectResult()

        return CommandPlan(
            events=events,
            receipt=_accepted(
                command, ev.AGG_SUBMISSION, submission.submission_id),
            side_effect=_apply,
        )

    def _plan_retry(self, command: CommandEnvelope, ctx: HandlerContext) -> CommandPlan:
        store = _store_of(ctx)
        payload = dict(command.payload)
        submission_id = _require_str(
            payload, "submission_id", command, "competition.submission")
        submission = store.get(PlatformSubmission, submission_id)
        if submission is None:
            raise _not_found("platform_submission", submission_id, command)
        current = submission.submission_state()
        try:
            # 设计 9.4：rate_limited / transient_failure / auth_required →
            # queued；unknown 禁止直接重提（由 reconciler 核对远端状态）。
            ensure_submission_transition(current, SubmissionState.QUEUED)
        except IllegalTransitionError as exc:
            raise _illegal(command, exc) from exc
        updated = submission.model_copy(
            update={"state": SubmissionState.QUEUED.value})
        event = ev.make_event(
            competition_id=submission.competition_id,
            aggregate_type=ev.AGG_SUBMISSION,
            aggregate_id=submission_id,
            event_type=ev.SUBMISSION_RETRY_REQUESTED,
            command=command,
            payload={
                "submission_id": submission_id,
                "from": current.value,
                "to": SubmissionState.QUEUED.value,
            },
        )

        async def _apply() -> SideEffectResult:
            with store.lock, store.conn:
                store.save(updated)
            return SideEffectResult()

        return CommandPlan(
            events=[event],
            receipt=_accepted(command, ev.AGG_SUBMISSION, submission_id),
            side_effect=_apply,
        )


# ---------------------------------------------------------------------------
# competition.message：比赛聊天（设计 14.4：自然语言消息本身不直接修改状态）
# ---------------------------------------------------------------------------


class MessageCommandHandler:
    """比赛聊天消息：只追加 ``competition.message.posted`` 事件并回执。

    消息不触碰任何实体状态机；附带的结构化命令由调用方（Web
    /messages 端点）另行 dispatch，receipt 随消息响应返回。
    """

    command_types = {"competition.message"}

    async def plan(self, command: CommandEnvelope, ctx: HandlerContext) -> CommandPlan:
        store = _store_of(ctx)
        payload = dict(command.payload)
        competition_id = _require_str(
            payload, "competition_id", command, "competition.message")
        if store.get(Competition, competition_id) is None:
            raise _not_found("competition", competition_id, command)
        text = str(payload.get("text") or "").strip()
        # 纯命令卡片允许空文本（消息即结构化 receipt 的载体）；两者皆空
        # 才视为校验失败。
        if not text and not payload.get("command_id"):
            raise _fail(
                "competition.message.text_required",
                "competition.message requires payload.text or an attached "
                "command receipt reference",
                ErrorCategory.VALIDATION, command)
        author = str(payload.get("author") or command.actor.id or "operator")
        event_payload: dict[str, Any] = {
            "competition_id": competition_id,
            "author": author,
            "text": text,
        }
        # 附带结构化命令的回执引用（设计 14.4：receipt 是实际操作记录）。
        if payload.get("command_id"):
            event_payload["command_id"] = str(payload["command_id"])
        if payload.get("receipt_state"):
            event_payload["receipt_state"] = str(payload["receipt_state"])
        event = ev.make_event(
            competition_id=competition_id,
            aggregate_type=ev.AGG_COMPETITION,
            aggregate_id=competition_id,
            event_type=ev.MESSAGE_POSTED,
            command=command,
            payload=event_payload,
        )
        return CommandPlan(
            events=[event],
            receipt=_accepted(command, ev.AGG_COMPETITION, competition_id),
        )


# ---------------------------------------------------------------------------
# 只读查询 Handler（10.9 查询端点与 10.11 能力工具共用同一授权边界）
# ---------------------------------------------------------------------------


class CompetitionQueryHandler:
    """比赛读模型查询：连接 / 比赛清单、快照、题目、提交、租约。

    全部只读 ``CompetitionStore`` 的投影数据，不产生事件；权限由 Command
    API 的 ``binding_for_query`` 统一判定（与能力工具同一边界）。
    """

    query_types = {
        "connection.list",
        "competition.list",
        "competition.snapshot",
        "competition.challenges",
        "competition.submissions",
        "competition.leases",
    }

    async def handle(self, query: QueryEnvelope, ctx: HandlerContext) -> QueryResult:
        store = _store_of(ctx)
        competition_id = str(
            query.params.get("competition_id") or query.aggregate_id or ""
        ).strip()

        if query.query_type == "connection.list":
            return self._result(query, [
                self._connection_view(c)
                for c in store.list(PlatformConnection)
                if not c.archived
            ])
        if query.query_type == "competition.list":
            return self._result(query, [
                c.model_dump(mode="json")
                for c in store.list(Competition)
                if not c.archived
            ])
        self._require_competition(store, competition_id, query)
        if query.query_type == "competition.snapshot":
            snapshot = store.snapshot(competition_id)
            return self._result(query, self._snapshot_view(snapshot))
        if query.query_type == "competition.challenges":
            return self._result(query, self._challenges(store, competition_id))
        if query.query_type == "competition.submissions":
            submissions = sorted(
                store.list(PlatformSubmission, competition_id=competition_id),
                key=lambda s: (s.created_at, s.submission_id))
            candidates = sorted(
                store.list(SubmissionCandidate, competition_id=competition_id),
                key=lambda c: (c.created_at, c.candidate_id))
            return self._result(query, {
                "competition_id": competition_id,
                "submissions": [
                    self._submission_view(s) for s in submissions],
                "candidates": [
                    self._candidate_view(c) for c in candidates],
            })
        # competition.leases
        leases = sorted(
            store.list(InstanceLease, competition_id=competition_id),
            key=lambda lease: (lease.created_at, lease.lease_id))
        return self._result(query, [self._lease_view(x) for x in leases])

    # -- 内部 -----------------------------------------------------------------

    @staticmethod
    def _result(query: QueryEnvelope, result: Any) -> QueryResult:
        return QueryResult(
            query_id=query.query_id, query_type=query.query_type, result=result)

    @staticmethod
    def _require_competition(
        store: CompetitionStore, competition_id: str, query: QueryEnvelope
    ) -> Competition:
        if not competition_id:
            raise CommandFailed(make_error(
                "competition.id_required",
                f"{query.query_type} requires params.competition_id",
                ErrorCategory.VALIDATION, correlation_id=query.query_id))
        competition = store.get(Competition, competition_id)
        if competition is None:
            raise CommandFailed(make_error(
                "competition.competition.not_found",
                f"unknown competition {competition_id!r}",
                ErrorCategory.NOT_FOUND, correlation_id=query.query_id))
        if competition.archived:
            raise CommandFailed(make_error(
                "competition.competition.archived",
                f"competition {competition_id!r} was removed from the inventory",
                ErrorCategory.NOT_FOUND, correlation_id=query.query_id))
        return competition

    @staticmethod
    def _challenges(store: CompetitionStore, competition_id: str) -> list[dict[str, Any]]:
        """题目清单：题目 + 当前 revision 摘要 + 活动 binding（run 链接）。"""
        rows: list[dict[str, Any]] = []
        challenges = sorted(
            store.list(CompetitionChallenge, competition_id=competition_id),
            key=lambda c: (c.created_at, c.challenge_id))
        for challenge in challenges:
            revision = (
                store.get(ChallengeRevision, challenge.current_revision_id)
                if challenge.current_revision_id else None)
            binding = store.active_binding_for_challenge(challenge.challenge_id)
            lease = store.active_lease_for_challenge(challenge.challenge_id)
            entry = store.get(
                SchedulerQueueEntry, competition_id, challenge.challenge_id)
            rows.append({
                "challenge": challenge.model_dump(mode="json"),
                "current_revision": (
                    revision.model_dump(mode="json")
                    if revision is not None else None),
                "active_binding": (
                    binding.model_dump(mode="json")
                    if binding is not None else None),
                "run_id": binding.run_id if binding is not None else None,
                "active_lease": (
                    CompetitionQueryHandler._lease_view(lease)
                    if lease is not None else None),
                "queue_entry": (
                    entry.model_dump(mode="json") if entry is not None else None),
            })
        return rows

    @staticmethod
    def _lease_view(lease: InstanceLease) -> dict[str, Any]:
        """租约完整视图。"""
        data = lease.model_dump(mode="json")
        data["has_credential"] = bool(lease.credential_ref)
        return data

    @staticmethod
    def _connection_view(connection: PlatformConnection) -> dict[str, Any]:
        """连接完整视图。"""
        data = connection.model_dump(mode="json")
        data["has_credential"] = bool(connection.credential_ref)
        return data

    @classmethod
    def _snapshot_view(cls, snapshot: Any) -> dict[str, Any]:
        """快照视图：连接与租约去凭据引用，候选去原文（与 SSE 同口径）。"""
        data = snapshot.model_dump(mode="json")
        if snapshot.connection is not None:
            data["connection"] = cls._connection_view(snapshot.connection)
        for row, challenge_view in zip(
                snapshot.challenges, data.get("challenges", [])):
            if row.active_lease is not None:
                challenge_view["active_lease"] = cls._lease_view(
                    row.active_lease)
            for candidate_view in challenge_view.get("candidates", []):
                candidate_view.pop("value", None)
        return data

    @staticmethod
    def _candidate_view(candidate: SubmissionCandidate) -> dict[str, Any]:
        """候选视图：只有 digest / 来源，绝无候选原文（任务书 10.7）。"""
        data = candidate.model_dump(mode="json")
        data.pop("value", None)
        return data

    @staticmethod
    def _submission_view(submission: PlatformSubmission) -> dict[str, Any]:
        return submission.model_dump(mode="json")


# ---------------------------------------------------------------------------
# 注册与 API 绑定
# ---------------------------------------------------------------------------


class CompetitionCommandPolicy(CommandPolicy):
    """比赛命令权限：operator/system 全权；agent 主体需 CapabilityBinding。

    COMP-01 阶段恒不解析 Binding（仅 operator 可发起）；COMP-09 起可注入
    ``binding_store``（存有 CapabilityBinding 的 PlatformStore）：agent
    主体的命令 / 查询 / 事件读取经共享判定层校验 Binding 的
    allowed_commands / allowed_queries / resource scope / 撤销状态，
    与平台侧 Run 命令完全同一判定逻辑。
    """

    def __init__(self, binding_store: Any = None) -> None:
        self._binding_store = binding_store

    def resolve_binding(self, store: Any, principal_id: str) -> Any:
        if self._binding_store is None:
            return None
        # Binding 实体持久化在 platform.db（CAP-01），比赛存储不复制。
        return CommandPolicy.resolve_binding(
            self, self._binding_store, principal_id)


def register_competition_handlers(api: MutekiCommandApiImpl) -> None:
    """把比赛命令 / 查询 Handler 注册进共享 HandlerRegistry。"""
    api.register_command(ConnectionCommandHandler())
    api.register_command(CompetitionCommandHandler())
    api.register_command(ChallengeCommandHandler())
    api.register_command(InstanceCommandHandler())
    api.register_command(RunBindingCommandHandler())
    api.register_command(SubmissionCommandHandler())
    api.register_command(MessageCommandHandler())
    api.register_query(CompetitionQueryHandler())


COMPETITION_COMMAND_TYPES: frozenset[str] = frozenset({
    "connection.create", "connection.test", "connection.unregister",
    "connection.credential.set", "connection.credential.revoke",
    "connection.browser_session.import", "connection.browser_session.renew",
    "connection.browser_session.revoke",
    "competition.sync",
    "competition.unregister",
    "challenge.select", "challenge.queue", "challenge.skip",
    "policy.update",
    "scheduler.start", "scheduler.pause", "scheduler.resume",
    "instance.ensure", "instance.stop",
    "run_binding.redirect", "run_binding.pause", "run_binding.resolve",
    "platform_submission.approve", "platform_submission.retry",
    "competition.message",
    # 能力目录（10.11）别名与一次完成提交命令。
    *ALIAS_COMMAND_TYPES,
    "competition.submission.submit",
    "competition.submission.manual_override",
})


class CompetitionCommandApi(MutekiCommandApiImpl):
    """绑定 CompetitionStore 的共享 Command API。

    dispatch / 幂等 / expected_version / 游标 / wait 全部复用
    ``MutekiCommandApiImpl``；receipt、事件、outbox 与水位经鸭子类型落在
    competition.db。构造后自动注册全部比赛命令 Handler。
    """

    def __init__(
        self,
        store: CompetitionStore,
        *,
        registry: Any = None,
        cursor_key: Optional[bytes] = None,
        max_wait_seconds: float = 30.0,
        services: Optional[dict[str, Any]] = None,
        binding_store: Any = None,
    ) -> None:
        super().__init__(
            store,  # type: ignore[arg-type] 鸭子类型：与 PlatformStore 同名同语义
            registry=registry,
            policy=CompetitionCommandPolicy(binding_store),
            cursor_key=cursor_key,
            max_wait_seconds=max_wait_seconds,
            services=services,
        )
        # 共享 dispatch 的自动 outbox 路径指向 platform 语义（投递即
        # delivered）；比赛外部副作用由 Handler 在 side effect 里自行
        # enqueue 为 pending，供 COMP-03+ 消费，故这里替换为比赛 outbox。
        self._outbox = store.outbox
        self._effect_store = dict(services or {}).get("effect_store")
        register_competition_handlers(self)

    async def dispatch(self, command: CommandEnvelope) -> CommandReceipt:
        """执行命令，并把 pending competition_outbox 表达为真实长操作。

        Handler 的本地状态变更完成后，远端效果仍需 consumer 投递。共享
        Command API 此时会得到一个同步 side-effect 的 completed；这里根据
        同库 outbox 事实将接受回执推进为 waiting/running，并在 platform.db
        建立独立 EffectReceipt。consumer 确认实际效果后再写 completed。
        """
        receipt = await super().dispatch(command)
        if receipt.state in {
            ReceiptState.FAILED, ReceiptState.CONFLICT,
            ReceiptState.CANCELLED,
        }:
            return receipt
        outboxes = self.competition_store.outbox.for_command(command.command_id)
        active = [
            record for record in outboxes
            if record.status not in {
                OutboxStatus.DELIVERED, OutboxStatus.CANCELLED,
            }
        ]
        if not active:
            return receipt

        effect_ids: list[str] = []
        if self._effect_store is not None:
            for record in active:
                prior = self._effect_store.effect_for_outbox(record.outbox_id)
                if prior is None:
                    state = (
                        EffectState.RUNNING
                        if record.status is OutboxStatus.PROCESSING
                        else EffectState.WAITING
                    )
                    prior = self._effect_store.save_effect_receipt(EffectReceipt(
                        command_id=command.command_id,
                        acceptance_receipt_id=receipt.receipt_id,
                        domain="competition",
                        state=state,
                        aggregate=AggregateRef(
                            type=record.aggregate_type,
                            id=record.aggregate_id,
                        ),
                        correlation_id=correlation_id_of(command),
                        outbox_id=record.outbox_id,
                        destination=record.destination,
                        idempotency_key=(
                            self.competition_store.outbox.idempotency_key(
                                record.outbox_id)
                        ),
                        attempts=record.attempts,
                        object_links={
                            "aggregate": (
                                f"{record.aggregate_type}:{record.aggregate_id}"
                            ),
                        },
                    ))
                effect_ids.append(prior.effect_id)
        state = (
            ReceiptState.RUNNING
            if any(r.status is OutboxStatus.PROCESSING for r in active)
            else ReceiptState.WAITING
        )
        updated = self.competition_store.update_receipt_state(
            command.command_id,
            state,
            event_cursor=receipt.event_cursor,
            run_id=receipt.run_id,
            effect_ids=effect_ids,
            output={
                **dict(receipt.output or {}),
                "effect_ids": effect_ids,
                "outbox_ids": [record.outbox_id for record in active],
                "effect_state": state.value,
            },
        )
        if receipt.deduplicated:
            updated = updated.model_copy(update={"deduplicated": True})
        return updated

    @property
    def competition_store(self) -> CompetitionStore:
        store = self._store
        if not isinstance(store, CompetitionStore):
            raise TypeError("CompetitionCommandApi requires CompetitionStore")
        return store


__all__ = [
    "ALIAS_COMMAND_TYPES",
    "COMMAND_ALIASES",
    "COMPETITION_COMMAND_TYPES",
    "ChallengeCommandHandler",
    "CompetitionCommandApi",
    "CompetitionCommandHandler",
    "CompetitionCommandPolicy",
    "CompetitionQueryHandler",
    "ConnectionCommandHandler",
    "InstanceCommandHandler",
    "MessageCommandHandler",
    "RunBindingCommandHandler",
    "SubmissionCommandHandler",
    "canonicalize_base_url",
    "register_competition_handlers",
]
