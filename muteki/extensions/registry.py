"""扩展注册表与生命周期编排（任务书 11.2/11.3，EXT-01）。

``ExtensionService`` 是扩展生命周期的唯一入口，负责：

- install / enable / disable / upgrade / rollback / uninstall / invoke /
  read_projection / health / logs；
- 启用路径：依赖与 core version 检查 → config schema 校验 → 子进程握手
  → activate → health check → mark active；
- 升级保留旧版本目录，状态目录按版本迁移并写 migration receipt；
  健康检查失败时自动恢复上一活动版本（auto-rollback）；
- 停用后已有 Domain Event 保持可读（事件在 PlatformStore，不随扩展删除），
  公开 projection 返回 unavailable；
- 从根 ``plugin.json`` 解析出的 Muteki 客户端扩展视图经
  ``PlatformStore.save_extension``（extensions /
  extension_versions 两表）持久化，运行期状态（启用标志、活动版本、健康）
  记录在每个扩展状态目录的 ``record.json``。

事件与命令一律经 MutekiCommandAPI（``handlers.py``）；本模块被 Handler 的
副作用闭包调用，不直接写核心投影。
"""

from __future__ import annotations

import asyncio
import json
import logging
import shutil
from dataclasses import asdict
from datetime import datetime, timedelta
from enum import Enum
from pathlib import Path
from typing import Any, Awaitable, Callable, Optional

from pydantic import Field

from muteki.extensions.host import (
    DEFAULT_HEALTH_FAIL_THRESHOLD,
    ExtensionProcess,
)
from muteki.extensions.installer import (
    ExtensionInstaller,
    Source,
)
from muteki.extensions.manifest import (
    CORE_VERSION,
    SchemaValidationError,
    load_manifest,
    load_schema,
    parse_semver,
    validate_against_schema,
    validate_manifest,
)
from muteki.extensions.permissions import (
    PermissionChecker,
    PermissionDenied,
    SecretResolver,
)
from muteki.platform.contracts.base import ContractModel, utcnow
from muteki.platform.contracts.base import new_id
from muteki.platform.contracts.extensions import ExtensionManifest
from muteki.platform.store import PlatformStore

LOG = logging.getLogger(__name__)

#: 后台健康巡检默认间隔（秒）；None 表示不开巡检（测试用显式 check_health）。
DEFAULT_MONITOR_INTERVAL: Optional[float] = None


class ExtensionState(str, Enum):
    """扩展运行期状态。

    - ``installed``：已安装未启用；
    - ``ready``：活动版本握手与健康检查通过；
    - ``degraded``：健康检查失败但进程仍可重启恢复；
    - ``unavailable``：启用 / 握手 / 校验失败，或上一活动版本也不可用；
    - ``disabled``：Operator 停用；历史事件仍可读，projection 报 unavailable。
    """

    INSTALLED = "installed"
    READY = "ready"
    DEGRADED = "degraded"
    UNAVAILABLE = "unavailable"
    DISABLED = "disabled"


class ExtensionError(RuntimeError):
    """生命周期操作失败；code 供统一错误 envelope 使用。"""

    def __init__(self, code: str, message: str) -> None:
        super().__init__(f"{code}: {message}")
        self.code = code
        self.message = message


class ExtensionRecord(ContractModel):
    """一个扩展的运行期状态记录（持久化在状态目录 record.json）。"""

    extension_id: str = ""
    origin: str = "installed"
    state: ExtensionState = ExtensionState.INSTALLED
    enabled: bool = False
    active_version: str = ""
    installed_versions: list[str] = Field(default_factory=list)
    # 版本号 -> 安装目录（升级后旧目录保留）
    install_dirs: dict[str, str] = Field(default_factory=dict)
    config: dict[str, Any] = Field(default_factory=dict)
    health: str = ""
    last_error: str = ""
    capabilities: dict[str, Any] = Field(default_factory=dict)
    # version -> 可核查的来源、摘要、签名与安装阶段。
    installations: dict[str, dict[str, Any]] = Field(default_factory=dict)
    registry_entries: list[dict[str, Any]] = Field(default_factory=list)
    isolation: dict[str, Any] = Field(default_factory=dict)
    secret_usage: list[dict[str, Any]] = Field(default_factory=list)
    start_count: int = 0
    last_exit_code: Optional[int] = None
    last_health_at: Optional[datetime] = None
    restart_backoff_seconds: float = 0.0
    manual_stop_reason: str = ""
    created_at: datetime = Field(default_factory=utcnow)
    updated_at: datetime = Field(default_factory=utcnow)


#: 扩展事件提案的宿主回调签名：(extension_id, event_type, payload)。
ProposalHandler = Callable[[str, str, dict[str, Any]], Awaitable[dict[str, Any]]]


class ExtensionService:
    """扩展生命周期编排。

    ``proposal_handler`` 由 handlers.py 注入：把权限校验过的事件提案经
    MutekiCommandAPI 的 extension.propose_event 命令落入 Domain Event。
    """

    def __init__(
        self,
        store: PlatformStore,
        *,
        install_root: str | Path,
        state_root: str | Path,
        workspace_root: str | Path | None = None,
        core_version: str = CORE_VERSION,
        capabilities: Optional[dict[str, int]] = None,
        secret_resolver: Optional[SecretResolver] = None,
        proposal_handler: Optional[ProposalHandler] = None,
        monitor_interval: Optional[float] = DEFAULT_MONITOR_INTERVAL,
        health_fail_threshold: int = DEFAULT_HEALTH_FAIL_THRESHOLD,
        install_policy: Optional[dict[str, Any]] = None,
        registration_bridge: Any = None,
        permission_change_callback: Any = None,
    ) -> None:
        self._store = store
        # 根目录一律解析为绝对路径：扩展子进程的 cwd 是安装目录，相对路径的
        # state_dir 注入环境后会在错误的目录下解析（扩展状态写不进状态目录）。
        self.install_root = Path(install_root).resolve()
        self.state_root = Path(state_root).resolve()
        self.workspace_root = Path(workspace_root).resolve() if workspace_root else None
        self.core_version = core_version
        self.capabilities = capabilities
        self._secret_resolver = secret_resolver
        self.proposal_handler = proposal_handler
        self._monitor_interval = monitor_interval
        self._health_fail_threshold = int(health_fail_threshold)
        self.install_policy = dict(install_policy or {})
        self.registration_bridge = registration_bridge
        self.permission_change_callback = permission_change_callback
        self._processes: dict[str, ExtensionProcess] = {}
        self._monitor_task: Optional[asyncio.Task] = None

    # ------------------------------------------------------------------
    # 状态目录与记录
    # ------------------------------------------------------------------

    def state_dir(self, extension_id: str, version: str = "") -> Path:
        base = self.state_root / extension_id
        return base / "state" / version if version else base

    def _record_path(self, extension_id: str) -> Path:
        return self.state_dir(extension_id) / "record.json"

    def get_record(self, extension_id: str) -> Optional[ExtensionRecord]:
        path = self._record_path(extension_id)
        if not path.is_file():
            return None
        return ExtensionRecord.model_validate_json(
            path.read_text(encoding="utf-8")
        )

    def _save_record(self, record: ExtensionRecord) -> ExtensionRecord:
        record = record.model_copy(update={"updated_at": utcnow()})
        path = self._record_path(record.extension_id)
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(record.model_dump_json(indent=2), encoding="utf-8")
        return record

    def list_records(self) -> list[ExtensionRecord]:
        records: list[ExtensionRecord] = []
        if not self.state_root.is_dir():
            return records
        for path in sorted(self.state_root.glob("*/record.json")):
            try:
                records.append(
                    ExtensionRecord.model_validate_json(
                        path.read_text(encoding="utf-8")
                    )
                )
            except ValueError:
                LOG.warning("unreadable extension record: %s", path)
        return records

    def _require_record(self, extension_id: str) -> ExtensionRecord:
        record = self.get_record(extension_id)
        if record is None:
            raise ExtensionError(
                "extension.not_found", f"extension not installed: {extension_id}"
            )
        return record

    # ------------------------------------------------------------------
    # install / uninstall
    # ------------------------------------------------------------------

    def _available_capabilities(self) -> Optional[dict[str, int]]:
        if self.capabilities is None:
            return None
        catalog = dict(self.capabilities)
        for record in self.list_records():
            for version in record.installed_versions:
                try:
                    manifest = self.manifest_of(record.extension_id, version)
                except Exception:
                    continue
                for provide in manifest.provides:
                    catalog[provide.id] = provide.api_version
        return catalog

    def preview_install(
        self,
        source: Source,
        *,
        extension_id: str = "",
    ) -> dict[str, Any]:
        """执行安装前完整检查并持久化短期待确认预览。"""
        installer = ExtensionInstaller(
            self.install_root,
            core_version=self.core_version,
            capabilities=self._available_capabilities(),
        )
        package = installer.preview(source)
        manifest = package.manifest
        if extension_id and manifest.id != extension_id:
            raise ExtensionError(
                "extension.preview_id_mismatch",
                f"source provides {manifest.id}, expected {extension_id}",
            )
        previous = self.get_record(manifest.id)
        required: list[str] = []
        if package.verification.get("signature") == "unsigned":
            required.append("unsigned")
        changes = self._preview_changes(previous, manifest, package.verification)
        if changes["source_changed"]:
            required.append("source_change")
        if changes["publisher_changed"]:
            required.append("publisher_change")
        if changes["permission_expansion"]:
            required.append("permission_expansion")
        self._enforce_install_policy(manifest, package.verification)
        preview_id = new_id("ext-preview")
        created = utcnow()
        preview = {
            "preview_id": preview_id,
            "extension_id": manifest.id,
            "version": manifest.version,
            "source": asdict(source),
            "manifest": manifest.model_dump(mode="json"),
            "verification": package.verification,
            "changes": changes,
            "confirmations_required": sorted(set(required)),
            "created_at": created.isoformat(),
            "expires_at": (created + timedelta(minutes=30)).isoformat(),
            "state": "pending_activation",
        }
        root = self.state_root / "_previews"
        root.mkdir(parents=True, exist_ok=True)
        (root / f"{preview_id}.json").write_text(
            json.dumps(preview, ensure_ascii=False, indent=2), encoding="utf-8")
        return preview

    def _preview_changes(
        self,
        previous: Optional[ExtensionRecord],
        manifest: ExtensionManifest,
        verification: dict[str, Any],
    ) -> dict[str, Any]:
        if previous is None or not previous.installed_versions:
            return {
                "source_changed": False,
                "publisher_changed": False,
                "permission_expansion": [],
                "ui_changed": False,
                "dependencies_changed": False,
            }
        prior_version = previous.active_version or previous.installed_versions[-1]
        prior_manifest = self.manifest_of(previous.extension_id, prior_version)
        prior_install = previous.installations.get(prior_version, {})
        old_verify = dict(prior_install.get("verification") or {})
        old_source = old_verify.get("source") or {}
        new_source = verification.get("source") or {}
        old_permissions = prior_manifest.permissions.model_dump(
            mode="json", exclude={"schema_version"}
        )
        new_permissions = manifest.permissions.model_dump(
            mode="json", exclude={"schema_version"}
        )
        expansion: list[str] = []
        for group, values in new_permissions.items():
            added = sorted(set(values or []) - set(old_permissions.get(group) or []))
            expansion.extend(f"{group}:{item}" for item in added)
        return {
            "source_changed": old_source != new_source,
            "publisher_changed": (
                old_verify.get("publisher", "unknown")
                != verification.get("publisher", "unknown")
            ),
            "permission_expansion": expansion,
            "ui_changed": prior_manifest.ui != manifest.ui,
            "dependencies_changed": (
                [item.model_dump(mode="json") for item in prior_manifest.requires]
                != [item.model_dump(mode="json") for item in manifest.requires]
            ),
        }

    def _enforce_install_policy(
        self, manifest: ExtensionManifest, verification: dict[str, Any]
    ) -> None:
        denied_extensions = {
            str(item) for item in self.install_policy.get("denied_extensions", [])
        }
        if manifest.id in denied_extensions:
            raise ExtensionError(
                "extension.policy_extension_denied",
                f"administrator policy rejects extension {manifest.id}",
            )
        publisher = str(verification.get("publisher") or "unknown")
        denied_publishers = {
            str(item) for item in self.install_policy.get("denied_publishers", [])
        }
        if publisher in denied_publishers:
            raise ExtensionError(
                "extension.policy_publisher_denied",
                f"administrator policy rejects publisher {publisher}",
            )
        if (self.install_policy.get("deny_untrusted_publisher")
                and verification.get("trust_status") != "verified"):
            raise ExtensionError(
                "extension.policy_untrusted_publisher_denied",
                "administrator policy requires a trusted Ed25519 publisher signature",
            )
        if verification.get("revoked"):
            raise ExtensionError(
                "extension.policy_revoked_source_denied",
                "revoked extension sources cannot be installed",
            )
        if (self.install_policy.get("deny_unsigned")
                and verification.get("signature") == "unsigned"):
            raise ExtensionError(
                "extension.policy_unsigned_denied",
                "administrator policy rejects unsigned extensions",
            )
        if (self.install_policy.get("deny_mutable_source")
                and not verification.get("immutable")):
            raise ExtensionError(
                "extension.policy_mutable_source_denied",
                "administrator policy requires an immutable source digest or commit",
            )
        high = set(manifest.permissions.filesystem) & {"workspace-write"}
        if self.install_policy.get("deny_high_permissions") and high:
            raise ExtensionError(
                "extension.policy_permission_denied",
                f"administrator policy rejects permissions: {sorted(high)}",
            )

    def _consume_preview(
        self,
        preview_id: str,
        package: Any,
        confirmations: list[str],
    ) -> dict[str, Any]:
        path = self.state_root / "_previews" / f"{preview_id}.json"
        if not path.is_file():
            raise ExtensionError(
                "extension.preview_not_found", f"unknown install preview: {preview_id}")
        preview = json.loads(path.read_text(encoding="utf-8"))
        if datetime.fromisoformat(preview["expires_at"]) <= utcnow():
            raise ExtensionError(
                "extension.preview_expired", f"install preview expired: {preview_id}")
        if (preview.get("extension_id") != package.manifest.id
                or preview.get("version") != package.manifest.version
                or (preview.get("verification") or {}).get("content_sha256")
                != package.verification.get("content_sha256")):
            raise ExtensionError(
                "extension.preview_source_changed",
                "extension source changed after preview; create a new preview",
            )
        missing = sorted(
            set(preview.get("confirmations_required") or [])
            - set(confirmations or []))
        if missing:
            raise ExtensionError(
                "extension.confirmation_required",
                "separate confirmation is required for: " + ", ".join(missing),
            )
        path.unlink()
        return preview

    def install(
        self,
        source: Source,
        *,
        preview_id: str = "",
        confirmations: Optional[list[str]] = None,
    ) -> ExtensionRecord:
        """安装一个来源包：fetch → verify → unpack → validate → 登记。"""
        installer = ExtensionInstaller(
            self.install_root,
            core_version=self.core_version,
            capabilities=self._available_capabilities(),
        )
        preview: dict[str, Any] = {}
        if preview_id:
            candidate = installer.preview(source)
            preview = self._consume_preview(
                preview_id, candidate, list(confirmations or []))
        package = installer.install(source)
        if (preview
                and (preview.get("verification") or {}).get("content_sha256")
                != package.verification.get("content_sha256")):
            shutil.rmtree(package.install_dir, ignore_errors=True)
            raise ExtensionError(
                "extension.preview_source_changed",
                "extension source changed while activation was in progress",
            )
        self._enforce_install_policy(package.manifest, package.verification)
        manifest = package.manifest
        self._store.save_extension(manifest)
        record = self.get_record(manifest.id) or ExtensionRecord(
            extension_id=manifest.id, origin=manifest.origin
        )
        versions = sorted(
            set(record.installed_versions) | {manifest.version},
            key=lambda v: parse_semver(v),
        )
        install_dirs = dict(record.install_dirs)
        install_dirs[manifest.version] = package.install_dir
        installations = dict(record.installations)
        installations[manifest.version] = {
            "source_kind": package.source_kind,
            "verification": package.verification,
            "installed_at": utcnow().isoformat(),
            "phases": list(package.verification.get("phases") or []),
            "preview_id": preview.get("preview_id", "compatibility-direct"),
            "confirmations": sorted(set(confirmations or [])),
        }
        record = record.model_copy(update={
            "origin": manifest.origin,
            "installed_versions": versions,
            "install_dirs": install_dirs,
            "installations": installations,
            # 首个版本装上后默认不启用；升级场景下保持 active_version 不变
            "state": record.state if record.enabled else ExtensionState.INSTALLED,
        })
        return self._save_record(record)

    async def uninstall(
        self,
        extension_id: str,
        *,
        preserve_state: bool = True,
        preserve_logs: bool = True,
        preserve_artifacts: bool = True,
    ) -> ExtensionRecord:
        """卸载：先停用（停子进程），删除版本目录与运行记录。

        Domain Event 在 PlatformStore 中，不随卸载删除；日志归档目录保留。
        """
        record = self._require_record(extension_id)
        if record.enabled:
            await self.disable(extension_id)
            record = self._require_record(extension_id)
        await self._stop_process(extension_id)
        if self.registration_bridge is not None:
            self.registration_bridge.uninstall(extension_id)
        ext_root = self.install_root / extension_id
        if ext_root.is_dir():
            shutil.rmtree(ext_root)
        archived: list[str] = []
        base = self.state_dir(extension_id)
        archive = self.state_root / "_uninstalled" / extension_id
        archive.mkdir(parents=True, exist_ok=True)
        stamp = utcnow().strftime("%Y%m%dT%H%M%S%fZ")
        if preserve_state:
            for item in sorted((base / "state").rglob("*")) if (base / "state").is_dir() else []:
                if not item.is_file() or (not preserve_logs and "logs" in item.parts):
                    continue
                target = archive / stamp / "state" / item.relative_to(base / "state")
                target.parent.mkdir(parents=True, exist_ok=True)
                shutil.copy2(item, target)
                archived.append(str(target.relative_to(archive)))
        elif preserve_logs:
            for item in sorted(base.rglob("*.log")) if base.is_dir() else []:
                target = archive / stamp / "logs" / item.name
                target.parent.mkdir(parents=True, exist_ok=True)
                shutil.copy2(item, target)
                archived.append(str(target.relative_to(archive)))
        receipt = {
            "extension_id": extension_id,
            "uninstalled_at": utcnow().isoformat(),
            "preserve_state": preserve_state,
            "preserve_logs": preserve_logs,
            "preserve_artifacts": preserve_artifacts,
            "archived_files": archived,
            "artifact_count": 0,
        }
        (archive / f"{stamp}.json").write_text(
            json.dumps(receipt, ensure_ascii=False, indent=2), encoding="utf-8")
        path = self._record_path(extension_id)
        if path.is_file():
            path.unlink()
        if base.is_dir():
            shutil.rmtree(base)
        return record

    # ------------------------------------------------------------------
    # enable / disable
    # ------------------------------------------------------------------

    async def enable(
        self,
        extension_id: str,
        *,
        version: Optional[str] = None,
        config: Optional[dict[str, Any]] = None,
        allow_rollback: bool = True,
    ) -> ExtensionRecord:
        """启用指定版本（默认最新已装版本）。

        流程：依赖与 core version 复核 → config schema 校验 → 子进程握手
        → activate → health check → mark active。健康检查失败且存在上一
        活动版本时自动回滚（``allow_rollback``）。
        """
        record = self._require_record(extension_id)
        target = version or record.installed_versions[-1]
        if target not in record.install_dirs:
            raise ExtensionError(
                "extension.version_not_installed",
                f"{extension_id}@{target} is not installed "
                f"(installed: {record.installed_versions})",
            )
        previous = record.active_version or None
        # config 只在启用成功后写入记录：失败回滚时必须沿用旧配置，
        # 否则「失败的新配置」会让旧版本也起不来。
        effective_config = dict(config) if config is not None else record.config
        prior_registry_entries = (
            self.registration_bridge.disable(extension_id)
            if self.registration_bridge is not None else record.registry_entries
        )
        await self._stop_process(extension_id)
        record = self._save_record(record.model_copy(update={
            "start_count": record.start_count + 1,
            "manual_stop_reason": "",
        }))
        try:
            process = await self._start_version(record, target, effective_config)
            health = await self._health_or_raise(process)
            registry_entries = (
                self.registration_bridge.enable(
                    process.manifest, process=process)
                if self.registration_bridge is not None else [])
        except Exception as exc:
            # 启动或健康检查失败：停掉失败进程，记录不可用原因，
            # 有上一活动版本时自动恢复（auto-rollback）。
            await self._stop_process(extension_id)
            if self.registration_bridge is not None:
                prior_registry_entries = (
                    self.registration_bridge.disable(extension_id)
                    or prior_registry_entries
                )
            record = record.model_copy(update={
                "state": ExtensionState.UNAVAILABLE,
                "enabled": False,
                "health": "unhealthy",
                "last_error": str(exc),
                "registry_entries": prior_registry_entries,
            })
            self._save_record(record)
            if allow_rollback and previous and previous != target:
                LOG.warning(
                    "ext %s@%s failed to start (%s); rolling back to %s",
                    extension_id, target, exc, previous,
                )
                return await self.rollback(extension_id, version=previous)
            raise
        record = record.model_copy(update={
            "state": ExtensionState.READY,
            "enabled": True,
            "active_version": target,
            "config": effective_config,
            "health": str(health.get("status") or "healthy"),
            "last_error": "",
            "capabilities": process.capabilities,
            "isolation": process.isolation,
            "secret_usage": process.secret_usage,
            "registry_entries": registry_entries,
            "last_health_at": utcnow(),
            "restart_backoff_seconds": 0.0,
        })
        self._save_record(record)
        self._ensure_monitor()
        return record

    async def disable(
        self, extension_id: str, *, reason: str = "operator_disabled"
    ) -> ExtensionRecord:
        """停用：优雅停止子进程；历史 Domain Event 保持可读。"""
        record = self._require_record(extension_id)
        registry_entries = (
            self.registration_bridge.disable(extension_id)
            if self.registration_bridge is not None else record.registry_entries)
        exit_code = await self._stop_process(extension_id)
        record = record.model_copy(update={
            "state": ExtensionState.DISABLED,
            "enabled": False,
            "health": "",
            "last_exit_code": exit_code,
            "manual_stop_reason": reason,
            "secret_usage": [],
            "registry_entries": registry_entries,
        })
        return self._save_record(record)

    async def handle_secret_change(
        self, old_reference: str, new_reference: str = ""
    ) -> dict[str, Any]:
        """刷新或停止使用已变化 Secret 的扩展进程。"""
        restarted: list[str] = []
        stopped: list[str] = []
        for record in self.list_records():
            if not record.enabled:
                continue
            manifest = self.manifest_of(record.extension_id)
            if old_reference not in manifest.permissions.secrets:
                continue
            config = dict(record.config)
            version = record.active_version
            await self.disable(record.extension_id, reason="secret_rotated")
            if new_reference == old_reference:
                await self.enable(
                    record.extension_id, version=version, config=config,
                    allow_rollback=False)
                restarted.append(record.extension_id)
            else:
                stopped.append(record.extension_id)
        return {"restarted": restarted, "stopped": stopped}

    # ------------------------------------------------------------------
    # upgrade / rollback / migration
    # ------------------------------------------------------------------

    async def upgrade(
        self,
        extension_id: str,
        source: Source,
        *,
        config: Optional[dict[str, Any]] = None,
        preview_id: str = "",
        confirmations: Optional[list[str]] = None,
    ) -> ExtensionRecord:
        """升级、迁移状态并按插件声明决定是否保留旧版本。"""
        record = self._require_record(extension_id)
        previous = record.active_version or None
        previous_manifest = (
            self.manifest_of(extension_id, previous) if previous else None)
        was_enabled = record.enabled
        effective_config = config if config is not None else record.config
        record = self.install(
            source,
            preview_id=preview_id,
            confirmations=confirmations,
        )
        target = record.installed_versions[-1]
        if target == previous:
            raise ExtensionError(
                "extension.upgrade_noop",
                f"upgrade source resolved to the active version {target}",
            )
        receipt_path = self._migrate_state(extension_id, previous, target)
        target_manifest = self.manifest_of(extension_id, target)
        if (previous_manifest is not None
                and previous_manifest.permissions != target_manifest.permissions
                and self.permission_change_callback is not None):
            await self.permission_change_callback(
                extension_id,
                previous_manifest.permissions.model_dump(mode="json"),
                target_manifest.permissions.model_dump(mode="json"),
            )
        LOG.info("ext %s state migration receipt: %s", extension_id, receipt_path)
        retain_previous = target_manifest.retain_previous_versions
        if not was_enabled:
            result = self._require_record(extension_id).model_copy(update={
                "active_version": target,
            })
            result = self._save_record(result)
        else:
            try:
                result = await self.enable(
                    extension_id,
                    version=target,
                    config=effective_config,
                    allow_rollback=retain_previous,
                )
            except Exception:
                if not retain_previous:
                    self._prune_versions(extension_id, target, active=False)
                raise
        if not retain_previous:
            result = self._prune_versions(extension_id, target, active=was_enabled)
        return result

    def _prune_versions(
        self, extension_id: str, target: str, *, active: bool
    ) -> ExtensionRecord:
        """Delete every non-target package and version state for replace upgrades."""
        record = self._require_record(extension_id)
        for version, raw_dir in list(record.install_dirs.items()):
            if version == target:
                continue
            path = Path(raw_dir).resolve()
            root = self.install_root.resolve()
            if root in path.parents:
                shutil.rmtree(path, ignore_errors=True)
            state_dir = self.state_dir(extension_id) / "state" / version
            shutil.rmtree(state_dir, ignore_errors=True)
        state = record.state
        enabled = record.enabled
        active_version = target
        if not active:
            enabled = False
            if state is not ExtensionState.UNAVAILABLE:
                state = ExtensionState.INSTALLED
        return self._save_record(record.model_copy(update={
            "installed_versions": [target],
            "install_dirs": {
                target: record.install_dirs[target],
            },
            "installations": {
                target: record.installations[target],
            },
            "active_version": active_version,
            "enabled": enabled,
            "state": state,
        }))

    async def rollback(
        self, extension_id: str, *, version: Optional[str] = None
    ) -> ExtensionRecord:
        """回滚到上一已装版本（默认取低于 active 的最新版本）。"""
        record = self._require_record(extension_id)
        current = record.active_version
        if version is None:
            candidates = [
                v for v in record.installed_versions if v != current
            ]
            if not candidates:
                raise ExtensionError(
                    "extension.no_rollback_target",
                    f"{extension_id} has no other installed version to roll back to",
                )
            version = candidates[-1]
        if version not in record.install_dirs:
            raise ExtensionError(
                "extension.version_not_installed",
                f"{extension_id}@{version} is not installed",
            )
        self._migrate_state(extension_id, current, version)
        # 回滚目标本身启动失败时不再二次回滚，如实落 unavailable
        return await self.enable(
            extension_id, version=version, allow_rollback=False
        )

    def _migrate_state(
        self, extension_id: str, from_version: Optional[str], to_version: str
    ) -> Path:
        """把状态目录从旧版本复制到新版本并写 migration receipt。

        receipt 记录 from/to、时间与文件清单，作为状态迁移的可审计证据；
        不删除旧版本状态目录。
        """
        base = self.state_dir(extension_id)
        migrations = base / "migrations"
        migrations.mkdir(parents=True, exist_ok=True)
        copied: list[str] = []
        if from_version and from_version != to_version:
            src = base / "state" / from_version
            dst = base / "state" / to_version
            if src.is_dir():
                dst.mkdir(parents=True, exist_ok=True)
                for item in sorted(src.rglob("*")):
                    if not item.is_file():
                        continue
                    rel = item.relative_to(src)
                    # 日志归档不随状态迁移（各版本独立留存）
                    if rel.parts and rel.parts[0] == "logs":
                        continue
                    target = dst / rel
                    target.parent.mkdir(parents=True, exist_ok=True)
                    shutil.copy2(item, target)
                    copied.append(str(rel))
        else:
            (base / "state" / to_version).mkdir(parents=True, exist_ok=True)
        receipt = {
            "extension_id": extension_id,
            "from_version": from_version,
            "to_version": to_version,
            "migrated_at": utcnow().isoformat(),
            "files": copied,
        }
        path = migrations / f"{from_version or 'none'}__{to_version}.json"
        path.write_text(
            json.dumps(receipt, ensure_ascii=False, indent=2), encoding="utf-8"
        )
        return path

    # ------------------------------------------------------------------
    # 调用 / 查询
    # ------------------------------------------------------------------

    async def invoke(
        self,
        extension_id: str,
        command_type: str,
        payload: Optional[dict[str, Any]] = None,
    ) -> dict[str, Any]:
        """调用扩展业务命令（command/handle）。扩展必须处于 ready。"""
        process = self._require_process(extension_id)
        try:
            return await process.handle_command(command_type, payload)
        except PermissionDenied:
            raise

    async def read_projection(
        self, extension_id: str, name: str = ""
    ) -> dict[str, Any]:
        """读取扩展公开 projection；停用 / 不可用时返回 unavailable 状态。"""
        record = self._require_record(extension_id)
        if not record.enabled or record.state is not ExtensionState.READY:
            return {
                "status": "unavailable",
                "extension_id": extension_id,
                "state": record.state.value,
                "name": name,
            }
        process = self._require_process(extension_id)
        result = await process.read_projection(name)
        return {"status": "ok", **result}

    async def health(self, extension_id: str) -> dict[str, Any]:
        """主动健康检查；不触发自动回滚（监管判定在 check_health）。"""
        record = self._require_record(extension_id)
        process = self._processes.get(extension_id)
        if process is None or not process.running:
            return {
                "status": "unavailable",
                "extension_id": extension_id,
                "state": record.state.value,
            }
        result = await process.health()
        return {"extension_id": extension_id, **result}

    async def check_health(self, extension_id: str) -> ExtensionRecord:
        """监管健康检查：连续失败达到阈值时自动恢复上一活动版本。"""
        record = self._require_record(extension_id)
        if not record.enabled:
            return record
        process = self._processes.get(extension_id)
        healthy = False
        detail = ""
        if process is not None and process.running:
            try:
                result = await process.health()
                healthy = str(result.get("status")) == "healthy"
                detail = str(result.get("status") or "")
            except Exception as exc:
                detail = str(exc)
        if healthy:
            return self._save_record(record.model_copy(update={
                "health": str(detail or "healthy"),
                "last_health_at": utcnow(),
                "restart_backoff_seconds": 0.0,
            }))
        LOG.warning("ext %s health check failed: %s", extension_id, detail)
        previous = self._previous_version(record)
        if previous is not None:
            return await self.rollback(extension_id, version=previous)
        registry_entries = (
            self.registration_bridge.disable(extension_id)
            if self.registration_bridge is not None else record.registry_entries
        )
        record = record.model_copy(update={
            "state": ExtensionState.UNAVAILABLE,
            "enabled": False,
            "health": "unhealthy",
            "last_error": detail or "health check failed",
            "last_exit_code": process.return_code if process is not None else None,
            "last_health_at": utcnow(),
            "restart_backoff_seconds": min(
                max(record.restart_backoff_seconds * 2, 1.0), 300.0),
            "registry_entries": registry_entries,
        })
        return self._save_record(record)

    async def recover_enabled(self) -> dict[str, Any]:
        """按 capability 依赖顺序恢复 enabled 扩展，单项失败不阻断其它项。"""
        desired = {
            item.extension_id: item
            for item in self.list_records() if item.enabled
        }
        manifests: dict[str, ExtensionManifest] = {}
        failed: dict[str, str] = {}
        for extension_id, record in desired.items():
            try:
                manifests[extension_id] = self.manifest_of(
                    extension_id, record.active_version or None)
            except Exception as exc:
                failed[extension_id] = f"{type(exc).__name__}: {exc}"

        providers: dict[str, str] = {}
        for extension_id, manifest in manifests.items():
            for provide in manifest.provides:
                providers.setdefault(provide.id, extension_id)
        dependencies: dict[str, set[str]] = {}
        host_capabilities = set((self.capabilities or {}).keys())
        for extension_id, manifest in manifests.items():
            deps: set[str] = set()
            for requirement in manifest.requires:
                provider = providers.get(requirement.capability)
                if provider and provider != extension_id:
                    deps.add(provider)
                elif requirement.capability not in host_capabilities:
                    failed.setdefault(
                        extension_id,
                        f"missing capability: {requirement.capability}",
                    )
            dependencies[extension_id] = deps

        order: list[str] = []
        remaining = set(manifests) - set(failed)
        while remaining:
            ready = sorted(
                item for item in remaining
                if not (dependencies.get(item, set()) & remaining)
            )
            if not ready:
                for extension_id in sorted(remaining):
                    failed[extension_id] = "cyclic extension dependency"
                break
            order.extend(ready)
            remaining.difference_update(ready)

        recovered: list[str] = []
        for extension_id in order:
            blocked = [
                dep for dep in dependencies.get(extension_id, set())
                if dep in failed
            ]
            if blocked:
                failed[extension_id] = (
                    "dependency unavailable: " + ", ".join(sorted(blocked)))
                continue
            record = desired[extension_id]
            try:
                await self.enable(
                    extension_id,
                    version=record.active_version or None,
                    config=record.config,
                    allow_rollback=False,
                )
            except Exception as exc:
                failed[extension_id] = f"{type(exc).__name__}: {exc}"
            else:
                recovered.append(extension_id)

        for extension_id, detail in failed.items():
            record = self.get_record(extension_id)
            if record is None:
                continue
            registry_entries = (
                self.registration_bridge.disable(extension_id)
                if self.registration_bridge is not None
                else record.registry_entries
            )
            self._save_record(record.model_copy(update={
                "enabled": True,
                "state": ExtensionState.UNAVAILABLE,
                "health": "unavailable",
                "last_error": detail,
                "registry_entries": registry_entries,
            }))
        return {
            "requested": sorted(desired),
            "order": order,
            "recovered": recovered,
            "failed": failed,
        }

    @staticmethod
    def _previous_version(record: ExtensionRecord) -> Optional[str]:
        candidates = [
            v for v in record.installed_versions if v != record.active_version
        ]
        return candidates[-1] if candidates else None

    def logs(self, extension_id: str, *, limit: int = 200) -> list[str]:
        """读取完整的扩展日志尾部（跨版本，按文件名排序）。"""
        base = self.state_dir(extension_id)
        if not base.is_dir():
            return []
        files = sorted(base.rglob("*.log"))
        lines: list[str] = []
        for path in files:
            lines.extend(path.read_text(encoding="utf-8").splitlines())
        return lines[-int(limit):]

    def migration_receipts(self, extension_id: str) -> list[dict[str, Any]]:
        """该扩展全部状态迁移 receipt。"""
        migrations = self.state_dir(extension_id) / "migrations"
        if not migrations.is_dir():
            return []
        return [
            json.loads(path.read_text(encoding="utf-8"))
            for path in sorted(migrations.glob("*.json"))
        ]

    def uninstall_receipts(self, extension_id: str) -> list[dict[str, Any]]:
        root = self.state_root / "_uninstalled" / extension_id
        if not root.is_dir():
            return []
        return [
            json.loads(path.read_text(encoding="utf-8"))
            for path in sorted(root.glob("*.json"))
        ]

    def manifest_of(self, extension_id: str, version: Optional[str] = None) -> ExtensionManifest:
        """从某版本根 plugin.json 读取 Muteki 客户端扩展视图。"""
        record = self._require_record(extension_id)
        target = version or record.active_version or record.installed_versions[-1]
        install_dir = record.install_dirs.get(target)
        if install_dir is None:
            raise ExtensionError(
                "extension.version_not_installed",
                f"{extension_id}@{target} is not installed",
            )
        return load_manifest(install_dir)

    # ------------------------------------------------------------------
    # 内部：进程启停
    # ------------------------------------------------------------------

    async def _start_version(
        self, record: ExtensionRecord, version: str, config: dict[str, Any]
    ) -> ExtensionProcess:
        install_dir = Path(record.install_dirs[version])
        manifest = load_manifest(install_dir)
        # 启用时复核 manifest、依赖与 core version（安装后目录可能被改动）。
        validate_manifest(
            manifest,
            package_dir=install_dir,
            core_version=self.core_version,
            capabilities=self._available_capabilities(),
        )
        config = dict(config)
        schema = load_schema(install_dir, manifest.config_schema)
        if schema is not None:
            try:
                validate_against_schema(config, schema)
            except SchemaValidationError as exc:
                raise ExtensionError(
                    "extension.invalid_config", f"config validation failed: {exc}"
                ) from exc
        state_dir = self.state_dir(record.extension_id, version)
        checker = PermissionChecker(
            manifest,
            workspace_root=self.workspace_root,
            state_dir=state_dir,
            secret_resolver=self._secret_resolver,
        )
        process = ExtensionProcess(
            manifest,
            package_dir=install_dir,
            state_dir=state_dir,
            checker=checker,
            config=config,
            proposal_handler=self._make_proposal_handler(record.extension_id),
            # Platform sync / probe can exceed the default 10s RPC budget when
            # listing dozens of challenges over the network.
            request_timeout=60.0,
        )
        await process.start()
        self._processes[record.extension_id] = process
        return process

    async def _health_or_raise(self, process: ExtensionProcess) -> dict[str, Any]:
        health = await process.health()
        status = str(health.get("status") or "")
        if status != "healthy":
            raise ExtensionError(
                "extension.unhealthy",
                f"health check failed for {process.manifest.id}: "
                f"status={status or 'missing'}",
            )
        return health

    async def _stop_process(self, extension_id: str) -> Optional[int]:
        process = self._processes.pop(extension_id, None)
        if process is not None:
            await process.stop()
            return process.return_code
        return None

    def _require_process(self, extension_id: str) -> ExtensionProcess:
        record = self._require_record(extension_id)
        process = self._processes.get(extension_id)
        if process is None or not process.running:
            raise ExtensionError(
                "extension.not_running",
                f"{extension_id} is not active (state={record.state.value}); "
                "enable it first",
            )
        return process

    def _make_proposal_handler(self, extension_id: str) -> ProposalHandler:
        async def _handle(event_type: str, payload: dict[str, Any]) -> dict[str, Any]:
            if self.proposal_handler is None:
                raise ExtensionError(
                    "extension.proposals_disabled",
                    "no proposal handler is wired (MutekiCommandAPI required)",
                )
            return await self.proposal_handler(extension_id, event_type, payload)

        return _handle

    # ------------------------------------------------------------------
    # 后台巡检与关闭
    # ------------------------------------------------------------------

    def _ensure_monitor(self) -> None:
        if self._monitor_interval is None or self._monitor_task is not None:
            return
        self._monitor_task = asyncio.create_task(self._monitor_loop())

    async def _monitor_loop(self) -> None:
        assert self._monitor_interval is not None
        try:
            while True:
                await asyncio.sleep(self._monitor_interval)
                for record in self.list_records():
                    if record.enabled:
                        try:
                            await self.check_health(record.extension_id)
                        except Exception:
                            LOG.exception(
                                "health monitor failed for %s", record.extension_id
                            )
        except asyncio.CancelledError:
            raise

    async def shutdown(self) -> None:
        """停止巡检与全部扩展子进程（服务关闭顺序的一部分）。"""
        if self._monitor_task is not None:
            self._monitor_task.cancel()
            try:
                await self._monitor_task
            except asyncio.CancelledError:
                pass
            self._monitor_task = None
        for extension_id in list(self._processes):
            if self.registration_bridge is not None:
                self.registration_bridge.disable(extension_id)
            await self._stop_process(extension_id)


__all__ = [
    "ExtensionError",
    "ExtensionRecord",
    "ExtensionService",
    "ExtensionState",
    "Source",
]
