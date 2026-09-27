"""通用授权浏览器平台 Adapter（任务书 10.3 / COMP-04，设计 8.3 第 2 顺位、13.3）。

面向没有稳定 REST 接口的平台，经用户明确授权的
Playwright 会话工作。边界与降级规则：

- **最后手段**：构造时经 ``select_transport_kind`` 落实官方接口优先
  规则——调用方声明的可用传输里只要 REST 可用，本 Adapter
  拒绝构造；``automation_allowed=False`` 时同样拒绝（浏览器不得绕过
  组织者自动化策略，设计 8.3）。
- **可选依赖**：Playwright 未安装时所有操作抛
  ``TransportUnavailableError``（明确降级），进程不崩溃。
- **授权登录**：``login`` 只接受 ``secret://platform/...`` 引用，在
  调用边界短暂物化后传给 ``PlaywrightTransport.login_with_credentials``
  （APIRequestContext API 登录，核验 §PW 官方推荐路径）；带验证码/2FA
  的平台不适用，应走人工辅助登录流程（本 Adapter 不实现）。
- **会话生命周期**：官方无自动续期（核验 §PW）；``ensure_session``
  探测会话有效性，过期即删除 storage state 并抛
  ``PlatformAuthRequiredError``——连接转 ``auth_required``，等用户
  重新授权，不自动重登（Adapter 不持有密码）。
- **页面变化诊断**：页面内 JSON API 响应形态与 ``BrowserSiteProfile``
  配置不符时抛 ``PlatformTransportError(INVALID_RESPONSE)``，detail
  里带实际响应的键集合，供排障定位站点改版。
- **提交**：提交结果无法识别时返回 ``status="unknown"``（禁止上层
  自动重试，设计 9.4）；401/403 由 transport 映射为
  ``PlatformAuthRequiredError``。
- **动态实例**：通用浏览器形态不支持，相关方法明确抛错。

凭据与 storage state 不进入 Worker、SSE、日志或事件：本模块的返回值、
错误消息与能力输出只含 ``secret://`` 引用与安全元数据。
"""

from __future__ import annotations

import hashlib
import json
import os
import re
from typing import Any, Callable, Iterable, Optional

from pydantic import Field

from muteki.competition.models import PlatformKind, RevisionArtifact
from muteki.competition.platforms.base import (
    CapabilityProbe,
    PlatformAuthRequiredError,
    PlatformPermissionError,
    PlatformRateLimitedError,
    PlatformTransportError,
    RemoteChallengeSnapshot,
    TRANSPORT_BROWSER,
    select_transport_kind,
)
from muteki.competition.secrets import PlatformSecretStore
from muteki.competition.transports.playwright import PlaywrightTransport
from muteki.platform.contracts.base import ContractModel
from muteki.platform.contracts.modules import (
    ArtifactObject,
    InstanceLeaseRef,
    InstanceResult,
    PlatformCapabilities,
    PlatformChallengeRef,
    PlatformConnectionRef,
    RemoteArtifactRef,
    SubmissionRequest,
    SubmissionResult,
    SyncRequest,
    SyncResult,
)


class BrowserSiteProfile(ContractModel):
    """站点级页面/API 形态配置；站点改版时只需调整本配置。

    所有路径相对连接的 ``endpoint``（同源 JSON API，核验 §PW 的
    APIRequestContext 形态）。
    """

    # 登录表单端点（API 登录）；空串表示站点不支持 API 登录。
    login_path: str = "/login"
    # 需登录的轻量端点，用于会话有效性探测。
    probe_path: str = "/"
    # 页面内 JSON API：题目列表；空串表示未知（probe 不做形态校验）。
    challenges_path: str = ""
    # 题目数组在响应体中的键；空串表示响应体本身即数组。
    challenges_data_key: str = "data"
    # 可选题目详情端点，支持 ``{challenge_key}`` 占位。配置后同步会把
    # 列表摘要与详情合并，用于取得描述、提示和附件路径。
    challenge_detail_path: str = ""
    # 题目详情对象在响应体中的键；空串表示响应体本身即详情对象。
    challenge_detail_data_key: str = "data"
    # 提交端点，支持 ``{challenge_key}`` 占位；空串表示不支持提交。
    submit_path: str = ""
    # 提交 body's flag 字段名。
    flag_field: str = "flag"
    # 部分站点在固定提交端点中还要求题目 ID；空串表示无需该字段。
    challenge_field: str = ""
    # 可选 CSRF 页面和正则。正则的第一个捕获组作为请求头值。
    csrf_path: str = ""
    csrf_pattern: str = ""
    csrf_header: str = "CSRF-Token"
    # 判对/判错关键字（小写子串匹配，先判负后判正）。
    verdict_correct: list[str] = Field(
        default_factory=lambda: ["correct", "success", "accepted"]
    )
    verdict_incorrect: list[str] = Field(
        default_factory=lambda: ["incorrect", "wrong", "invalid"]
    )


#: transport 工厂签名（测试注入 stub；真实路径用 PlaywrightTransport）。
BrowserTransportFactory = Callable[[PlatformConnectionRef, BrowserSiteProfile], Any]


class GenericBrowserAdapter:
    """``generic_browser`` 平台形态的 PlatformAdapter 实现。

    生命周期：先 ``login``（用户授权）或确认已有 storage state，再
    ``probe``；会话过期后 ``ensure_session`` 抛
    ``PlatformAuthRequiredError``，连接转 ``auth_required`` 等用户重新
    授权后再次 ``login``。
    """

    id = PlatformKind.GENERIC_BROWSER.value

    def __init__(
        self,
        secret_store: PlatformSecretStore,
        profile_root: str | os.PathLike[str],
        *,
        site: Optional[BrowserSiteProfile] = None,
        available_transports: Optional[Iterable[str]] = None,
        automation_allowed: bool = True,
        transport_factory: Optional[BrowserTransportFactory] = None,
    ) -> None:
        # 官方接口优先（设计 8.3）：调用方声明可用传输时，浏览器应为
        # 唯一选择；组织者禁止自动化时 select_transport_kind
        # 直接抛 PlatformPermissionError，浏览器不得绕过。
        declared = list(available_transports or (TRANSPORT_BROWSER,))
        chosen = select_transport_kind(
            declared, automation_allowed=automation_allowed
        )
        if chosen != TRANSPORT_BROWSER:
            raise PlatformPermissionError(
                f"generic browser adapter refused: machine transport {chosen!r} "
                "is available and takes precedence"
            )
        self._secrets = secret_store
        self._profile_root = profile_root
        self._site = site or BrowserSiteProfile()
        self._transport_factory = transport_factory
        self._connections: dict[str, PlatformConnectionRef] = {}

    # ------------------------------------------------------------------
    # 授权登录与会话生命周期
    # ------------------------------------------------------------------

    async def login(
        self,
        connection: PlatformConnectionRef,
        *,
        username_ref: str,
        password_ref: str,
        extra_fields: Optional[dict[str, str]] = None,
    ) -> None:
        """API 登录并保存 storage state（凭据只在调用边界短暂物化）。"""
        username = self._secrets.resolve(username_ref)
        password = self._secrets.resolve(password_ref)
        transport = self._transport(connection)
        await transport.login_with_credentials(
            username, password, extra_fields=extra_fields
        )
        self._connections[connection.connection_id] = connection

    async def ensure_session(self, connection: PlatformConnectionRef) -> None:
        """探测会话有效性；过期即删除 state 并抛 auth_required。

        官方无自动续期（核验 §PW：过期后删除 state 重新登录）；本
        Adapter 不持有密码，不能自动重登，由用户重新授权后再 ``login``。
        """
        transport = self._transport(connection)
        try:
            await transport.probe_session()
        except PlatformAuthRequiredError:
            transport.clear_storage_state()
            raise

    def import_storage_state(
        self,
        connection: PlatformConnectionRef,
        serialized_state: str,
        *,
        expires_at: str = "",
    ) -> dict[str, Any]:
        transport = self._transport(connection)
        status = transport.import_storage_state(
            serialized_state, expires_at=expires_at)
        self._connections[connection.connection_id] = connection
        return status

    def storage_state_status(
        self, connection: PlatformConnectionRef
    ) -> dict[str, Any]:
        return self._transport(connection).storage_state_status()

    def restore_persisted_session(
        self, connection: PlatformConnectionRef
    ) -> bool:
        """重建 Adapter 后恢复已持久化且仍存在的浏览器会话绑定。"""
        status = self.storage_state_status(connection)
        if not bool(status.get("present")) or bool(status.get("expired")):
            return False
        self._connections[connection.connection_id] = connection
        return True

    def renew_storage_state(
        self, connection: PlatformConnectionRef, *, expires_at: str = ""
    ) -> dict[str, Any]:
        return self._transport(connection).renew_storage_state(
            expires_at=expires_at)

    def revoke_storage_state(self, connection: PlatformConnectionRef) -> bool:
        self._connections.pop(connection.connection_id, None)
        return bool(self._transport(connection).clear_storage_state())

    # ------------------------------------------------------------------
    # PlatformAdapter：probe 与同步
    # ------------------------------------------------------------------

    async def probe(self, connection: PlatformConnectionRef) -> PlatformCapabilities:
        transport = self._transport(connection)
        # 未装 Playwright → TransportUnavailableError；无会话/会话过期
        # → PlatformAuthRequiredError，均由上层映射连接状态。
        await transport.probe_session()
        probe = CapabilityProbe(
            platform_kind=PlatformKind.GENERIC_BROWSER.value,
            auth_methods=["browser_session"],
            sync_modes=["full"],
            attachments=True,
            submit=bool(self._site.submit_path),
            dynamic_instances=False,
            evidence={
                "endpoint": connection.endpoint,
                "site": self._site.model_dump(mode="json"),
            },
        )
        if self._site.challenges_path:
            # 形态校验：页面内 JSON API 是否仍符合配置（页面变化诊断）。
            _, body = await transport.api_request("GET", self._site.challenges_path)
            items = self._extract_challenges(body)
            probe.evidence["challenges_seen"] = len(items)
        self._connections[connection.connection_id] = connection
        return probe.build_capabilities()

    async def sync_competition(self, request: SyncRequest) -> SyncResult:
        connection = self._require_connection(request.connection_id)
        if not self._site.challenges_path:
            raise PlatformTransportError(
                "browser sync: site profile has no challenges_path"
            )
        transport = self._transport(connection)
        status, body = await transport.api_request(
            "GET", self._site.challenges_path
        )
        if status == 429:
            raise PlatformRateLimitedError("browser sync: rate limited (HTTP 429)")
        if not 200 <= status < 300:
            raise PlatformTransportError(
                f"browser sync: unexpected status (HTTP {status})",
                status_code=status,
            )
        items = self._extract_challenges(body)
        if self._site.challenge_detail_path:
            items = await self._fetch_challenge_details(transport, items)
        snapshots = [self._to_snapshot(item) for item in items]
        cursor = hashlib.sha256(
            json.dumps(items, ensure_ascii=False, sort_keys=True).encode("utf-8")
        ).hexdigest()
        return SyncResult(
            connection_id=request.connection_id,
            cursor=cursor,
            synced_challenges=len(items),
            detail={
                "challenges": items,
                "snapshots": [item.model_dump(mode="json") for item in snapshots],
                "snapshot_complete": True,
            },
        )

    @staticmethod
    def _to_snapshot(item: dict[str, Any]) -> RemoteChallengeSnapshot:
        """把常见页面 JSON 字段归一为正式同步服务使用的快照。"""
        external_id = item.get("id") or item.get("key") or item.get("slug")
        raw_points = item.get("value", item.get("points", item.get("score", 0)))
        try:
            points = float(raw_points or 0)
        except (TypeError, ValueError):
            points = 0.0
        state = str(item.get("state") or item.get("status") or "open").lower()
        artifacts: list[RevisionArtifact] = []
        for raw in item.get("files") or item.get("attachments") or []:
            if isinstance(raw, str):
                name = raw.split("?", 1)[0].rstrip("/").rsplit("/", 1)[-1]
                artifacts.append(RevisionArtifact(name=name))
            elif isinstance(raw, dict):
                url = str(raw.get("url") or raw.get("location") or "")
                name = str(raw.get("name") or "") or (
                    url.split("?", 1)[0].rstrip("/").rsplit("/", 1)[-1]
                )
                if name:
                    artifacts.append(RevisionArtifact(
                        name=name,
                        size=int(raw.get("size") or 0),
                    ))
        hints = item.get("hints") or []
        if not isinstance(hints, list):
            hints = [hints]
        return RemoteChallengeSnapshot(
            external_challenge_id=str(external_id or ""),
            name=str(item.get("name") or item.get("title") or external_id or ""),
            category=str(item.get("category") or ""),
            remote_state=(
                "hidden" if state in {"hidden", "locked", "disabled"} else state
            ),
            points=points,
            description=str(item.get("description") or item.get("content") or ""),
            target=str(item.get("target") or ""),
            flag_format=str(item.get("flag_format") or item.get("flagFormat") or ""),
            hints=[str(value) for value in hints],
            artifacts=artifacts,
            raw_payload=dict(item),
        )

    def _extract_challenges(self, body: Any) -> list[dict[str, Any]]:
        """按 site profile 取题目数组；形态不符即页面变化诊断错误。"""
        items: Any = body
        if self._site.challenges_data_key:
            if not isinstance(body, dict):
                raise self._shape_error(body)
            items = body.get(self._site.challenges_data_key)
        if not isinstance(items, list):
            raise self._shape_error(body)
        return [item for item in items if isinstance(item, dict)]

    async def _fetch_challenge_details(
        self, transport: Any, items: list[dict[str, Any]]
    ) -> list[dict[str, Any]]:
        """读取题目详情并与列表摘要合并。"""
        detailed: list[dict[str, Any]] = []
        for item in items:
            challenge_key = item.get("id") or item.get("key") or item.get("slug")
            if challenge_key is None:
                raise self._shape_error(item)
            path = self._site.challenge_detail_path.format(
                challenge_key=challenge_key
            )
            status, body = await transport.api_request("GET", path)
            if status == 429:
                raise PlatformRateLimitedError(
                    "browser challenge detail: rate limited (HTTP 429)"
                )
            if not 200 <= status < 300:
                raise PlatformTransportError(
                    f"browser challenge detail: unexpected status (HTTP {status})",
                    status_code=status,
                )
            detail: Any = body
            if self._site.challenge_detail_data_key:
                if not isinstance(body, dict):
                    raise self._shape_error(body)
                detail = body.get(self._site.challenge_detail_data_key)
            if not isinstance(detail, dict):
                raise self._shape_error(body)
            detailed.append({**item, **detail})
        return detailed

    @staticmethod
    def _shape_error(body: Any) -> PlatformTransportError:
        keys = sorted(body.keys()) if isinstance(body, dict) else []
        return PlatformTransportError(
            "browser api: response shape no longer matches site profile "
            "(page structure changed)",
            detail={"body_type": type(body).__name__, "body_keys": keys},
        )

    # ------------------------------------------------------------------
    # PlatformAdapter：附件
    # ------------------------------------------------------------------

    async def fetch_artifact(self, artifact: RemoteArtifactRef) -> ArtifactObject:
        connection = self._require_connection(artifact.connection_id)
        if not artifact.url:
            raise PlatformTransportError("browser artifact: url is required")
        transport = self._transport(connection)
        content = await transport.download(artifact.url)
        digest = hashlib.sha256(content).hexdigest()
        if artifact.sha256 and artifact.sha256 != digest:
            raise PlatformTransportError(
                "browser artifact: sha256 mismatch after download",
                detail={"expected": artifact.sha256, "actual": digest},
            )
        return ArtifactObject(
            sha256=digest,
            media_type="application/octet-stream",
            content=content,
        )

    # ------------------------------------------------------------------
    # PlatformAdapter：提交
    # ------------------------------------------------------------------

    async def submit(self, request: SubmissionRequest) -> SubmissionResult:
        connection = self._require_connection(request.connection_id)
        if not self._site.submit_path:
            raise PlatformTransportError(
                "browser submit: site profile has no submit_path"
            )
        if request.flag is None:
            raise PlatformTransportError("browser submit: flag is required")
        path = self._site.submit_path.format(challenge_key=request.challenge_key)
        transport = self._transport(connection)
        # 401/403 已由 transport 映射为 PlatformAuthRequiredError；
        # 非 JSON 响应（页面改版）映射为 INVALID_RESPONSE 诊断错误。
        payload: dict[str, Any] = {self._site.flag_field: request.flag}
        if self._site.challenge_field:
            payload[self._site.challenge_field] = request.challenge_key
        headers: dict[str, str] = {}
        if self._site.csrf_path and self._site.csrf_pattern:
            csrf_status, page = await transport.text_request(
                "GET", self._site.csrf_path
            )
            if not 200 <= csrf_status < 300:
                raise PlatformTransportError(
                    f"browser csrf page: unexpected status (HTTP {csrf_status})",
                    status_code=csrf_status,
                )
            match = re.search(self._site.csrf_pattern, page)
            if match is None or not match.groups() or not match.group(1):
                raise PlatformTransportError(
                    "browser csrf token was not found; page structure changed"
                )
            headers[self._site.csrf_header] = match.group(1)
        status, body = await transport.api_request(
            "POST", path, json_body=payload, headers=headers
        )
        if status == 429:
            raise PlatformRateLimitedError("browser submit: rate limited (HTTP 429)")
        if not 200 <= status < 300:
            raise PlatformTransportError(
                f"browser submit: unexpected status (HTTP {status})",
                status_code=status,
            )
        return self._parse_verdict(body)

    def _parse_verdict(self, body: Any) -> SubmissionResult:
        """按 site profile 关键字解释提交回执；无法识别 → unknown。"""
        blob = json.dumps(body, ensure_ascii=False).lower() if body is not None else ""
        submission_id: Optional[str] = None
        if isinstance(body, dict):
            raw_id = body.get("submission_id") or body.get("id")
            submission_id = str(raw_id) if raw_id is not None else None
        for word in self._site.verdict_incorrect:
            if word in blob:
                return SubmissionResult(
                    submission_id=submission_id, status="incorrect"
                )
        for word in self._site.verdict_correct:
            if word in blob:
                return SubmissionResult(
                    submission_id=submission_id, status="correct"
                )
        keys = sorted(body.keys()) if isinstance(body, dict) else []
        return SubmissionResult(
            submission_id=submission_id,
            status="unknown",
            detail={
                "reason": "unrecognized verdict shape",
                "body_keys": keys,
            },
        )

    # ------------------------------------------------------------------
    # PlatformAdapter：动态实例（通用浏览器形态不支持）
    # ------------------------------------------------------------------

    async def acquire_instance(self, challenge: PlatformChallengeRef) -> InstanceResult:
        raise PlatformTransportError(
            "generic browser adapter does not manage dynamic instances"
        )

    async def renew_instance(self, lease: InstanceLeaseRef) -> InstanceResult:
        raise PlatformTransportError(
            "generic browser adapter does not manage dynamic instances"
        )

    async def release_instance(self, lease: InstanceLeaseRef) -> None:
        raise PlatformTransportError(
            "generic browser adapter does not manage dynamic instances"
        )

    # ------------------------------------------------------------------
    # 内部
    # ------------------------------------------------------------------

    def _require_connection(self, connection_id: str) -> PlatformConnectionRef:
        connection = self._connections.get(connection_id)
        if connection is None:
            raise PlatformTransportError(
                f"browser connection {connection_id}: login/probe required before use"
            )
        return connection

    def _transport(self, connection: PlatformConnectionRef) -> Any:
        if self._transport_factory is not None:
            return self._transport_factory(connection, self._site)
        return PlaywrightTransport(
            self._profile_root,
            connection.connection_id,
            connection.endpoint,
            probe_path=self._site.probe_path,
            login_path=self._site.login_path,
        )


__all__ = [
    "BrowserSiteProfile",
    "GenericBrowserAdapter",
]
