"""rCTF 平台 Adapter（任务书 10.3 / 设计 8.4 阶段 3，COMP-03）。

端点与语义以 docs/research/third_party_verification.md §RCTF 为准
（核验基线：otter-sec/rctf@main——原 redpwn/rCTF 已停维并迁移；
官方 API 文档 https://rctf.osec.io/api/）：

- API 挂在 ``/api`` 下，V1（``/api/v1``，兼容）与 V2（``/api/v2``，推荐）
  并存；响应包裹为 ``{kind, message, data?}``。错误处理按 ``kind``
  字符串分发而非 HTTP code（401 可能是 badToken / badNotStarted / badEnded）。
- 认证：``Authorization: Bearer <auth-token>``；401 ``badToken``、
  403 ``badPerms``。token 校验用 ``GET /api/v1/users/me``。
- 题目列表 ``GET /api/v2/challs``（``goodChallengesV2``，含 tags 与
  ``instancerLifetime/instancerExtendable/instancerStoppable`` 能力位）；
  V2 不可用（404）时回退 ``GET /api/v1/challs``。附件内嵌在题目对象
  ``files: [{name, url, size?}]``，URL 可直接下载，无独立附件端点。
- 提交 ``POST /api/v1/challs/:id/submit``，body ``{flag}``；``goodFlag``
  判对、``badFlag`` 判错、``badAlreadySolvedChallenge`` 归 duplicate。
  限速按「用户×题目」桶（burst 5 / 25000ms refill，核验已确认），超限
  429 ``badRateLimit`` 且 ``data.timeLeft``（毫秒）是精确退避来源。
- 动态实例（V2 integrations）：``PUT/PATCH/DELETE
  /api/v2/integrations/challs/:id/instance`` 对应租约的获取/续租/释放。
- 无显式解锁端点：题目依赖在服务端判定，列表只返回可见题目。

凭据只以 ``secret://`` 引用保存；真实 token 在 ``_api()`` 构造请求头时
短暂解引用，不进入日志、事件或错误消息。
"""

from __future__ import annotations

import hashlib
from typing import Any, Optional

from muteki.competition.models import (
    PlatformConnection,
    PlatformKind,
    RevisionArtifact,
)
from muteki.competition.platforms.base import (
    CapabilityProbe,
    PlatformAuthRequiredError,
    PlatformNotFoundError,
    PlatformPermissionError,
    PlatformRateLimitedError,
    PlatformTransportError,
    RateLimitSource,
    RateLimitSpec,
    RemoteChallengeSnapshot,
)
from muteki.competition.secrets import PlatformSecretStore
from muteki.competition.transports import RestTransport
from muteki.platform.contracts.base import utcnow
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

#: kind → typed 错误分发表（核验 §RCTF：按 kind 字符串而非 HTTP code）。
#: badFlag / badAlreadySolvedChallenge 是提交回执而非错误，不在此表。
_KIND_ERROR_MAP = {
    "badToken": PlatformAuthRequiredError,
    "badPerms": PlatformPermissionError,
    "badChallenge": PlatformNotFoundError,
    "badNotFound": PlatformNotFoundError,
}

#: 比赛状态类 kind（401 badNotStarted / badEnded）：平台可达但比赛未开放。
_GATE_KINDS = frozenset({"badNotStarted", "badEnded"})

#: rCTF 官方限速参数（核验 §RCTF：burst 5 / 25000ms refill，用户×题目桶）。
RCTF_RATE_LIMIT = RateLimitSpec(
    max_attempts=5,
    window_seconds=25.0,
    cooldown_seconds=5.0,
    source=RateLimitSource.PLATFORM.value,
)


class RCtfAdapter:
    """绑定单个 ``PlatformConnection`` 的 rCTF REST Adapter（V2 优先）。"""

    id = PlatformKind.RCTF.value

    def __init__(
        self,
        connection: PlatformConnection,
        secrets: PlatformSecretStore,
        *,
        client_transport: Optional[Any] = None,
    ) -> None:
        if connection.platform_kind != self.id:
            raise ValueError(
                f"RCtfAdapter requires platform_kind={self.id!r}, "
                f"got {connection.platform_kind!r}"
            )
        self._connection = connection
        self._secrets = secrets
        self._client_transport = client_transport
        self._transport: Optional[RestTransport] = None
        self._api_version: str = ""  # probe 时实测：v2 / v1

    @property
    def connection(self) -> PlatformConnection:
        return self._connection

    @property
    def api_version(self) -> str:
        """probe 实测的 API 版本（"v2" / "v1"）；未 probe 时为空串。"""
        return self._api_version

    # ------------------------------------------------------------------
    # 传输与凭据
    # ------------------------------------------------------------------

    def _api(self) -> RestTransport:
        if self._transport is None:
            token = self._secrets.resolve(self._connection.credential_ref)
            self._transport = RestTransport(
                self._connection.canonical_base_url,
                default_headers={"Authorization": f"Bearer {token}"},
                transport=self._client_transport,
            )
        return self._transport

    async def aclose(self) -> None:
        if self._transport is not None:
            await self._transport.aclose()
            self._transport = None

    def _check_ref(self, connection: Optional[PlatformConnectionRef]) -> None:
        if connection is not None and connection.connection_id != self._connection.connection_id:
            raise ValueError(
                f"connection ref {connection.connection_id} does not match "
                f"bound connection {self._connection.connection_id}"
            )

    # ------------------------------------------------------------------
    # {kind, message, data} 包裹解析与 kind 分发
    # ------------------------------------------------------------------

    async def _call(self, method: str, path: str, **kwargs: Any) -> Any:
        """发送请求并展开包裹；错误 kind 分发为 typed 错误。

        返回 ``data``（可能为 None）。提交回执类 kind（badFlag 等）也在这里
        抛出为 ``PlatformTransportError``（detail 带 kind），由 ``submit``
        捕获后映射为回执。
        """
        context = kwargs.pop("context", "") or f"rctf {method} {path}"
        try:
            response = await self._api().request(
                method, path, context=context, **kwargs
            )
        except PlatformTransportError as exc:
            self._rethrow_kind(exc)
            raise  # pragma: no cover - _rethrow_kind 对已识别 kind 必抛
        body = response.json_body
        if not isinstance(body, dict) or "kind" not in body:
            raise PlatformTransportError(
                f"{context}: invalid rCTF envelope (HTTP {response.status_code})",
                status_code=response.status_code,
            )
        kind = str(body.get("kind") or "")
        if kind.startswith("good"):
            return body.get("data")
        self._raise_kind(
            kind,
            str(body.get("message") or ""),
            body.get("data"),
            response.status_code,
            context,
        )
        raise PlatformTransportError(f"{context}: unreachable")  # pragma: no cover

    @staticmethod
    def _raise_kind(
        kind: str,
        message: str,
        data: Any,
        status_code: Optional[int],
        context: str,
    ) -> None:
        """kind 字符串 → typed 错误。消息只含 kind 与状态码，不含平台原文细节。"""
        detail: dict[str, Any] = {"kind": kind}
        error_cls = _KIND_ERROR_MAP.get(kind)
        if error_cls is not None:
            raise error_cls(
                f"{context}: {kind} (HTTP {status_code})",
                status_code=status_code,
                detail=detail,
            )
        if kind == "badRateLimit":
            # data.timeLeft（毫秒）是精确退避来源（核验 §RCTF）。
            retry_after = None
            if isinstance(data, dict) and data.get("timeLeft") is not None:
                retry_after = max(0.0, float(data["timeLeft"]) / 1000.0)
            raise PlatformRateLimitedError(
                f"{context}: badRateLimit (HTTP {status_code})",
                status_code=status_code,
                retry_after_seconds=retry_after,
                detail=detail,
            )
        if kind in _GATE_KINDS:
            # 比赛未开始 / 已结束：平台可达，作为门控信息上抛。
            raise PlatformTransportError(
                f"{context}: {kind} (HTTP {status_code})",
                status_code=status_code,
                detail=detail,
            )
        # badFlag / badAlreadySolvedChallenge / 其他：保留 kind 供 submit 判定。
        raise PlatformTransportError(
            f"{context}: {kind} (HTTP {status_code})",
            status_code=status_code,
            detail=detail,
        )

    def _rethrow_kind(self, exc: PlatformTransportError) -> None:
        """transport 按 HTTP code 分类后，用错误体 kind 改写（401 三分支等）。"""
        body = exc.detail.get("response_body")
        if not isinstance(body, dict) or "kind" not in body:
            raise exc
        kind = str(body.get("kind") or "")
        if kind == "badRateLimit" and exc.retry_after_seconds is not None:
            # 保留 transport 解析的 Retry-After 作为兜底。
            try:
                self._raise_kind(
                    kind, "", body.get("data"), exc.status_code, "rctf request"
                )
            except PlatformRateLimitedError as rate_exc:
                if rate_exc.retry_after_seconds is None:
                    rate_exc.retry_after_seconds = exc.retry_after_seconds
                raise rate_exc from exc
            return
        self._raise_kind(
            kind,
            str(body.get("message") or ""),
            body.get("data"),
            exc.status_code,
            "rctf request",
        )

    # ------------------------------------------------------------------
    # 能力探测（设计 8.2）
    # ------------------------------------------------------------------

    async def probe(
        self, connection: Optional[PlatformConnectionRef] = None
    ) -> PlatformCapabilities:
        self._check_ref(connection)
        probe = CapabilityProbe(
            platform_kind=self.id,
            auth_methods=["bearer"],
            sync_modes=["full"],
            attachments=True,
            submit=True,
            scoreboard=True,               # GET /api/v1/leaderboard（核验 §RCTF）
            rate_limit=RCTF_RATE_LIMIT.model_copy(),
        )
        # token 校验（challs 端点无需认证，必须走认证端点）。
        await self._call("GET", "/api/v1/users/me", context="rctf probe users/me")
        # V2 探测：404 → 回退 V1（核验 §RCTF 修正：V2 优先，V1 兼容回退）。
        try:
            await self._call("GET", "/api/v2/challs", context="rctf probe v2 challs")
            self._api_version = "v2"
            probe.dynamic_instances = True
            probe.instance_renewable = True
            probe.instance_stoppable = True
        except PlatformNotFoundError:
            await self._call("GET", "/api/v1/challs", context="rctf probe v1 challs")
            self._api_version = "v1"
        probe.auth_detail["token_type"] = "auth-token"
        probe.evidence.update(
            {
                "platform": "rctf",
                "api_version": self._api_version,
                "rate_limit": "5 submissions / 25s per user×challenge "
                "(verified; badRateLimit.data.timeLeft is authoritative backoff)",
            }
        )
        return probe.build_capabilities()

    # ------------------------------------------------------------------
    # 题目同步
    # ------------------------------------------------------------------

    def _challs_path(self) -> str:
        return f"/api/{self._api_version or 'v2'}/challs"

    async def list_challenges(self) -> list[RemoteChallengeSnapshot]:
        if not self._api_version:
            # 未 probe 时先实测 V2 可用性（404 回退 V1）。
            try:
                data = await self._call("GET", "/api/v2/challs")
                self._api_version = "v2"
            except PlatformNotFoundError:
                data = await self._call("GET", "/api/v1/challs")
                self._api_version = "v1"
        else:
            data = await self._call("GET", self._challs_path())
        if not isinstance(data, list):
            raise PlatformTransportError("rctf list challenges: data is not a list")
        return [self._to_snapshot(item) for item in data if isinstance(item, dict)]

    @staticmethod
    def _to_snapshot(item: dict[str, Any]) -> RemoteChallengeSnapshot:
        # points 容忍数值或动态分值对象 {min, max}。
        raw_points = item.get("points")
        if isinstance(raw_points, dict):
            points = float(raw_points.get("max") or raw_points.get("min") or 0)
        else:
            points = float(raw_points or 0)
        artifacts = [
            RevisionArtifact(
                name=str(f.get("name") or ""),
                size=int(f.get("size") or 0),
            )
            for f in item.get("files") or []
            if isinstance(f, dict)
        ]
        return RemoteChallengeSnapshot(
            external_challenge_id=str(item.get("id", "")),
            name=str(item.get("name") or ""),
            category=str(item.get("category") or ""),
            remote_state="open",
            points=points,
            description=str(item.get("description") or ""),
            artifacts=artifacts,
            revision_extensions={
                "author": item.get("author"),
                "solves": item.get("solves"),
                "sort_weight": item.get("sortWeight"),
                "tags": item.get("tags") or [],
                # V2 instancer 能力位（核验 §RCTF）。
                "instancer_lifetime": item.get("instancerLifetime"),
                "instancer_extendable": item.get("instancerExtendable"),
                "instancer_stoppable": item.get("instancerStoppable"),
            },
            raw_payload=dict(item),
        )

    async def sync_competition(self, request: SyncRequest) -> SyncResult:
        """rCTF 单实例即一场比赛：全量同步，无增量游标。"""
        self._check_ref(PlatformConnectionRef(connection_id=request.connection_id))
        snapshots = await self.list_challenges()
        return SyncResult(
            connection_id=request.connection_id,
            cursor=None,
            synced_challenges=len(snapshots),
            detail={
                "external_competition_id": "default",
                "api_version": self._api_version,
                "snapshots": [s.model_dump(mode="json") for s in snapshots],
            },
        )

    # ------------------------------------------------------------------
    # 附件下载（URL 直接下载 + sha256 校验）
    # ------------------------------------------------------------------

    async def fetch_artifact(self, artifact: RemoteArtifactRef) -> ArtifactObject:
        content = await self._api().download(
            artifact.url, context=f"rctf download {artifact.remote_id}"
        )
        digest = hashlib.sha256(content).hexdigest()
        if artifact.sha256 and artifact.sha256.lower() != digest:
            raise PlatformTransportError(
                f"rctf download {artifact.remote_id}: sha256 mismatch",
                detail={"expected": artifact.sha256, "actual": digest},
            )
        return ArtifactObject(sha256=digest, content=content)

    # ------------------------------------------------------------------
    # 提交（同步判定；回执类 kind 映射为 SubmissionResult）
    # ------------------------------------------------------------------

    async def submit(self, request: SubmissionRequest) -> SubmissionResult:
        try:
            await self._call(
                "POST",
                f"/api/v1/challs/{request.challenge_key}/submit",
                json_body={"flag": request.flag or ""},
                context=f"rctf submit {request.challenge_key}",
            )
        except PlatformTransportError as exc:
            kind = str(exc.detail.get("kind") or "")
            if kind == "badFlag":
                return SubmissionResult(
                    status="incorrect",
                    detail={"kind": kind},
                )
            if kind == "badAlreadySolvedChallenge":
                return SubmissionResult(
                    status="duplicate",
                    detail={"kind": kind},
                )
            raise
        return SubmissionResult(status="correct", detail={"kind": "goodFlag"})

    # ------------------------------------------------------------------
    # 动态实例（V2 integrations：PUT/PATCH/DELETE）
    # ------------------------------------------------------------------

    def _require_v2(self, operation: str) -> None:
        if self._api_version == "v1":
            raise PlatformTransportError(
                f"rctf {operation}: instancer requires API v2",
                detail={"capability": "dynamic_instances", "available": False},
            )

    async def acquire_instance(self, challenge: PlatformChallengeRef) -> InstanceResult:
        self._require_v2("acquire_instance")
        data = await self._call(
            "PUT",
            f"/api/v2/integrations/challs/{challenge.challenge_key}/instance",
            context=f"rctf instance create {challenge.challenge_key}",
        )
        return InstanceResult(
            lease=InstanceLeaseRef(
                connection_id=self._connection.connection_id,
                challenge_key=challenge.challenge_key,
            ),
            endpoints=self._instance_endpoints(data),
            acquired_at=utcnow(),
        )

    async def renew_instance(self, lease: InstanceLeaseRef) -> InstanceResult:
        self._require_v2("renew_instance")
        data = await self._call(
            "PATCH",
            f"/api/v2/integrations/challs/{lease.challenge_key}/instance",
            context=f"rctf instance extend {lease.challenge_key}",
        )
        return InstanceResult(
            lease=lease,
            endpoints=self._instance_endpoints(data),
            acquired_at=utcnow(),
        )

    async def release_instance(self, lease: InstanceLeaseRef) -> None:
        self._require_v2("release_instance")
        await self._call(
            "DELETE",
            f"/api/v2/integrations/challs/{lease.challenge_key}/instance",
            context=f"rctf instance stop {lease.challenge_key}",
        )

    @staticmethod
    def _instance_endpoints(data: Any) -> dict[str, str]:
        """instancer 返回形态按 provider 而定，宽容解析 host/port/url。"""
        if isinstance(data, str):
            return {"target": data}
        if isinstance(data, dict):
            if data.get("url"):
                return {"http": str(data["url"])}
            host = data.get("host")
            port = data.get("port")
            if host and port:
                return {"tcp": f"{host}:{port}"}
            if host:
                return {"host": str(host)}
        return {}


__all__ = ["RCTF_RATE_LIMIT", "RCtfAdapter"]
