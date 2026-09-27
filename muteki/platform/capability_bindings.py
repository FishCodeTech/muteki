"""Thread 级 CapabilityBinding 服务与 AgentSession 级 Grant 管理（CAP-01）。

职责（任务书 6.6、设计 9.5）：

- 按 Thread 模式签发 CapabilityBinding：mode / tool_set 在一个 version 内不可变；
  切换模式在同一 binding_id 下创建新版本，并撤销旧版本与其全部 Grant。
- 面向具体 AgentSession 签发 CapabilityGrant：runtime instance、audience、
  injection kind、短期 credential reference、有效期、touch、rotation、撤销。
- credential reference 只是 ``secret://`` 风格的引用，真实 secret 不落库。

本服务是 CapabilityBinding 的唯一签发入口；不存在从 Worker Profile 或
ExecutionBinding 推导 / 提权 CapabilityBinding 的代码路径（两者分表保存、
语义互不复用，见 ``contracts/capabilities.py`` 模块 docstring）。
"""

from __future__ import annotations

import secrets
from datetime import datetime, timedelta
from typing import Optional

from muteki.platform.contracts.base import new_id, utcnow
from muteki.platform.contracts.capabilities import (
    CapabilityBinding,
    CapabilityGrant,
    InjectionKind,
    ThreadMode,
)
from muteki.platform.contracts.objects import AgentSession
from muteki.platform.store import PlatformStore

from .capability_catalog import (
    DEFAULT_CATALOG,
    CapabilityCatalog,
    default_resource_scopes,
    default_tool_set,
    effective_tool_set,
)

#: Grant 默认有效期（短期授权；活动 Session 由服务端 lease 续期）。
DEFAULT_GRANT_TTL_SECONDS = 3600.0

#: credential_ref 引用前缀（不是凭据本体；本体只存在于 Runtime 进程内）。
CREDENTIAL_REF_PREFIX = "secret://capability-grant/"


def new_credential_ref(grant_id: str) -> str:
    """生成短期 credential reference（引用，不含任何 secret 材料）。"""
    return f"{CREDENTIAL_REF_PREFIX}{grant_id}/{secrets.token_urlsafe(24)}"


#: 常见凭据本体的值前缀；credential_ref 只能是引用，不能是本体。
_SECRET_VALUE_PREFIXES = ("sk-", "ghp_", "gho_", "xox", "-----BEGIN")


def _validate_credential_ref(value: str) -> None:
    """拒绝把凭据本体当作 reference 落库。"""
    text = value.strip()
    if not text or " " in text:
        raise BindingServiceError("credential_ref must be a non-empty reference")
    if any(text.startswith(prefix) for prefix in _SECRET_VALUE_PREFIXES):
        raise BindingServiceError(
            "credential_ref must be a reference (secret://...), "
            "not raw credential material")


class BindingServiceError(ValueError):
    """Binding 服务的输入 / 状态错误（未知对象、模式非法等）。"""


class CapabilityBindingService:
    """CapabilityBinding / CapabilityGrant 的签发与生命周期服务。

    无业务状态：所有持久化都落在 PlatformStore 的 capability_bindings /
    capability_grants 两表，与 execution_bindings 表完全分离。
    """

    def __init__(
        self,
        store: PlatformStore,
        *,
        catalog: CapabilityCatalog = DEFAULT_CATALOG,
    ) -> None:
        self._store = store
        self._catalog = catalog

    # -- Binding 签发 ----------------------------------------------------------

    def get_binding(
        self, binding_id: str, version: Optional[int] = None
    ) -> Optional[CapabilityBinding]:
        """读取 binding；version 为空时取最新版本（含已撤销，供判定）。"""
        if version is not None:
            return self._store.get(CapabilityBinding, binding_id, version)
        versions = self._store.list(CapabilityBinding, binding_id=binding_id)
        if not versions:
            return None
        return max(versions, key=lambda b: b.binding_version)

    def active_binding_for_thread(
        self, thread_id: str, principal_id: str
    ) -> Optional[CapabilityBinding]:
        """该 (thread, principal) 当前有效（未撤销）的最新版本 binding。"""
        candidates = self._store.list(
            CapabilityBinding, thread_id=thread_id, principal_id=principal_id)
        active = [b for b in candidates if b.revoked_at is None]
        if not active:
            return None
        return max(active, key=lambda b: (b.binding_version, b.created_at))

    def issue_binding(
        self,
        thread_id: str,
        principal_id: str,
        mode: ThreadMode | str,
        *,
        resource_scopes: Optional[list[str]] = None,
        policy_version: int = 1,
    ) -> CapabilityBinding:
        """按模式签发（或复用）该 Thread 的 CapabilityBinding。

        已存在同模式的有效 binding 时直接返回；模式切换时在同一 binding_id
        下创建新版本（mode / tool_set 版本内不可变），撤销旧版本及其 Grant。
        """
        if not isinstance(mode, ThreadMode):
            mode = ThreadMode(str(mode))
        if not thread_id or not principal_id:
            raise BindingServiceError("thread_id and principal_id are required")

        existing = self.active_binding_for_thread(thread_id, principal_id)
        if (
            existing is not None
            and existing.mode is mode
            and effective_tool_set(mode, existing.tool_set) == existing.tool_set
        ):
            return existing

        tool_set = default_tool_set(mode)
        binding = CapabilityBinding(
            binding_id=(existing.binding_id if existing is not None
                        else new_id("cbind")),
            binding_version=(existing.binding_version + 1)
            if existing is not None else 1,
            thread_id=thread_id,
            principal_id=principal_id,
            mode=mode,
            tool_set=tool_set,
            allowed_commands=self._catalog.command_types_of(tool_set),
            allowed_queries=self._catalog.query_types_of(tool_set),
            resource_scopes=(
                list(resource_scopes)
                if resource_scopes is not None
                else default_resource_scopes(mode, thread_id)
            ),
            policy_version=policy_version,
        )
        saved = self._store.save(binding)
        if existing is not None:
            # 切模式：撤销旧版本，并撤销旧版本签出的全部 Grant（设计 9.5）。
            self._store.revoke_capability_binding(
                existing.binding_id, existing.binding_version)
            self.revoke_grants_for_binding(existing.binding_id)
        return saved

    def replace_tool_set(
        self,
        thread_id: str,
        principal_id: str,
        tool_set: list[str],
        *,
        mode: ThreadMode | str,
        resource_scopes: Optional[list[str]] = None,
        policy_version: Optional[int] = None,
    ) -> tuple[CapabilityBinding, bool, int]:
        """替换一个 Thread 的工具集合，并保留完整版本与撤销记录。

        工具名只接受 ``CapabilityCatalog`` 中真实注册的条目。已有 Binding
        发生变化时沿用 ``binding_id`` 新建版本，随后撤销旧版本以及它签发
        的全部 Grant；相同配置幂等返回当前版本。尚无 Binding 时创建版本 1。

        返回 ``(binding, changed, revoked_grants)``。
        """
        if not isinstance(mode, ThreadMode):
            mode = ThreadMode(str(mode))
        if not thread_id or not principal_id:
            raise BindingServiceError("thread_id and principal_id are required")

        normalized: list[str] = []
        seen: set[str] = set()
        for raw_name in tool_set:
            name = str(raw_name or "").strip()
            if not name or name in seen:
                continue
            if self._catalog.get(name) is None:
                raise BindingServiceError(f"unknown capability tool: {name}")
            normalized.append(name)
            seen.add(name)

        existing = self.active_binding_for_thread(thread_id, principal_id)
        scopes = (
            list(resource_scopes)
            if resource_scopes is not None
            else (
                list(existing.resource_scopes)
                if existing is not None and existing.mode is mode
                else default_resource_scopes(mode, thread_id)
            )
        )
        effective_policy = int(
            policy_version
            if policy_version is not None
            else (existing.policy_version if existing is not None else 1)
        )
        if (
            existing is not None
            and existing.mode is mode
            and existing.tool_set == normalized
            and existing.resource_scopes == scopes
            and existing.policy_version == effective_policy
        ):
            return existing, False, 0

        binding = CapabilityBinding(
            binding_id=(
                existing.binding_id if existing is not None else new_id("cbind")
            ),
            binding_version=(
                existing.binding_version + 1 if existing is not None else 1
            ),
            thread_id=thread_id,
            principal_id=principal_id,
            mode=mode,
            tool_set=normalized,
            allowed_commands=self._catalog.command_types_of(normalized),
            allowed_queries=self._catalog.query_types_of(normalized),
            resource_scopes=scopes,
            policy_version=effective_policy,
        )
        saved = self._store.save(binding)
        revoked_grants = 0
        if existing is not None:
            self._store.revoke_capability_binding(
                existing.binding_id, existing.binding_version
            )
            revoked_grants = self.revoke_grants_for_binding(existing.binding_id)
        return saved, True, revoked_grants

    def revoke_binding(
        self, binding_id: str, version: Optional[int] = None
    ) -> CapabilityBinding:
        """撤销 binding（默认最新版本）并撤销其全部 Grant。"""
        binding = self.get_binding(binding_id, version)
        if binding is None:
            raise BindingServiceError(f"unknown capability binding: {binding_id}")
        revoked = self._store.revoke_capability_binding(
            binding.binding_id, binding.binding_version)
        self.revoke_grants_for_binding(binding.binding_id)
        return revoked

    # -- Grant 签发 ------------------------------------------------------------

    def issue_grant(
        self,
        binding: CapabilityBinding,
        agent_session_id: str,
        *,
        runtime_instance_id: Optional[str] = None,
        injection_kind: InjectionKind = InjectionKind.MCP,
        audience: str = "",
        ttl_seconds: float = DEFAULT_GRANT_TTL_SECONDS,
        credential_ref: Optional[str] = None,
    ) -> CapabilityGrant:
        """面向一个 AgentSession 签发短期 Grant。

        ``credential_ref`` 只允许 ``secret://`` 风格引用；缺省时生成随机引用。
        """
        if binding.revoked_at is not None:
            raise BindingServiceError(
                f"capability binding revoked: {binding.binding_id}")
        if not agent_session_id:
            raise BindingServiceError("agent_session_id is required")
        if credential_ref is not None:
            _validate_credential_ref(credential_ref)
        grant = CapabilityGrant(
            binding_id=binding.binding_id,
            agent_session_id=agent_session_id,
            runtime_instance_id=runtime_instance_id,
            injection_kind=injection_kind,
            audience=audience,
            expires_at=utcnow() + timedelta(seconds=float(ttl_seconds)),
        )
        grant = grant.model_copy(update={
            "credential_ref": credential_ref or new_credential_ref(grant.grant_id),
        })
        return self._store.save(grant)

    def get_grant(self, grant_id: str) -> Optional[CapabilityGrant]:
        return self._store.get(CapabilityGrant, grant_id)

    def touch_grant(self, grant_id: str) -> CapabilityGrant:
        """活跃心跳：刷新 last_touched_at。"""
        return self._store.touch_capability_grant(grant_id)

    def renew_grant(
        self,
        grant_id: str,
        *,
        ttl_seconds: float = DEFAULT_GRANT_TTL_SECONDS,
    ) -> CapabilityGrant:
        """延长同一个活动 Grant 的租期，不更换 Runtime 中的 bearer token。

        续期是服务端受信操作：只有 Binding 仍有效且 AgentSession 未关闭时
        才能执行。允许仍在运行的会话在下一次用户交互前恢复已到期租期；
        已撤销 Grant、已撤销 Binding 和已关闭 Session 都不会被恢复。
        """
        ttl = float(ttl_seconds)
        if ttl <= 0:
            raise BindingServiceError("grant renewal ttl_seconds must be positive")
        grant = self.get_grant(grant_id)
        if grant is None:
            raise BindingServiceError(f"unknown capability grant: {grant_id}")
        if grant.revoked_at is not None:
            raise BindingServiceError(f"capability grant revoked: {grant_id}")
        binding = self.get_binding(grant.binding_id)
        if binding is None:
            raise BindingServiceError(
                f"grant references unknown binding: {grant.binding_id}")
        if binding.revoked_at is not None:
            raise BindingServiceError(
                f"capability binding revoked: {binding.binding_id}")
        session = self._store.get(AgentSession, grant.agent_session_id)
        if session is None:
            raise BindingServiceError(
                f"grant references unknown agent session: {grant.agent_session_id}")
        if session.closed_at is not None:
            raise BindingServiceError(
                f"agent session closed: {grant.agent_session_id}")
        now = utcnow()
        return self._store.renew_capability_grant(
            grant_id,
            expires_at=now + timedelta(seconds=ttl),
            at=now,
        )

    def revoke_grant(self, grant_id: str) -> CapabilityGrant:
        return self._store.revoke_capability_grant(grant_id)

    def revoke_grants_for_binding(self, binding_id: str) -> int:
        """撤销该 binding 签出的所有未撤销 Grant，返回撤销数量。"""
        count = 0
        for grant in self._store.list(CapabilityGrant, binding_id=binding_id):
            if grant.revoked_at is None:
                self._store.revoke_capability_grant(grant.grant_id)
                count += 1
        return count

    def revoke_all_grants(self) -> int:
        """策略或 Secret 变化时撤销全部活动 Grant。"""
        count = 0
        for grant in self._store.list(CapabilityGrant):
            if grant.revoked_at is None:
                self._store.revoke_capability_grant(grant.grant_id)
                count += 1
        return count

    def rotate_grant(
        self,
        grant_id: str,
        *,
        ttl_seconds: float = DEFAULT_GRANT_TTL_SECONDS,
    ) -> CapabilityGrant:
        """轮换 Grant：撤销旧 Grant，按同一 binding / session / audience
        签发带新 credential reference 的新 Grant。"""
        old = self.get_grant(grant_id)
        if old is None:
            raise BindingServiceError(f"unknown capability grant: {grant_id}")
        binding = self.get_binding(old.binding_id)
        if binding is None:
            raise BindingServiceError(
                f"grant references unknown binding: {old.binding_id}")
        self._store.revoke_capability_grant(grant_id)
        return self.issue_grant(
            binding,
            old.agent_session_id,
            runtime_instance_id=old.runtime_instance_id,
            injection_kind=old.injection_kind,
            audience=old.audience,
            ttl_seconds=ttl_seconds,
        )

    # -- Gateway 校验辅助 --------------------------------------------------------

    @staticmethod
    def grant_expired(grant: CapabilityGrant, *, at: Optional[datetime] = None) -> bool:
        """Grant 是否已过有效期。"""
        if grant.expires_at is None:
            return False
        return grant.expires_at <= (at or utcnow())


__all__ = [
    "BindingServiceError",
    "CREDENTIAL_REF_PREFIX",
    "CapabilityBindingService",
    "DEFAULT_GRANT_TTL_SECONDS",
    "new_credential_ref",
]
