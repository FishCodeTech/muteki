"""CTFd 平台 Adapter（任务书 10.3 / 设计 8.4 阶段 1，COMP-03）。

端点与语义以 docs/research/third_party_verification.md §CTFD 为准
（核验基线：CTFd/CTFd@master，release 3.8.7；
https://docs.ctfd.io/docs/api/getting-started/）：

- 认证：``Authorization: Token <access token>``；Token 在用户 Settings →
  Access Tokens 生成。probe 额外探测管理端点 ``GET /api/v1/configs``
  区分 token 权限级别（admin / user；附件 sha1sum 等管理字段仅 admin 可见）。
- 列表 ``GET /api/v1/challenges`` 一次返回全部可见题目，**不分页**；
  未满足 prerequisites 的题目以 ``type: "hidden"`` 占位（``name: "???"``）
  返回——同步映射为 hidden 快照，不产生真实 revision。
- 详情 ``GET /api/v1/challenges/<id>`` 的 ``files`` 是 itsdangerous 签名的
  短时效 URL；快照只保留 ``location``，下载前重新拉详情刷新签名 URL。
- 提交 ``POST /api/v1/challenges/attempt``：失败语义以 ``data.status``
  为准而非 HTTP code（200/403 混合）：

  - ``correct`` / ``incorrect`` / ``already_solved`` / ``partial`` → 提交回执；
  - ``ratelimited``（429 或并发锁 403）→ ``PlatformRateLimitedError``，
    退避秒数从 message ``Try again in {N} seconds`` 解析；
  - ``authentication_required``（403）→ ``PlatformAuthRequiredError``；
  - ``paused``（403）→ 不可重试平台错误；
  - 403 无 body → max_attempts lockout（不可重试，
    ``detail["max_attempts_exhausted"]=True``）。

凭据只以 ``secret://`` 引用保存；真实 token 在 ``_api()`` 构造请求头时
短暂解引用，不进入日志、事件或错误消息。
"""

from __future__ import annotations

import hashlib
import re
from typing import Any, Optional

from muteki.competition.models import (
    PlatformConnection,
    PlatformKind,
    RevisionArtifact,
)
from muteki.competition.platforms.base import (
    CONSERVATIVE_RATE_LIMIT,
    CapabilityProbe,
    PlatformAuthRequiredError,
    PlatformPermissionError,
    PlatformRateLimitedError,
    PlatformTransportError,
    RateLimitSource,
    RemoteChallengeSnapshot,
)
from muteki.competition.secrets import PlatformSecretStore
from muteki.competition.transports import RestResponse, RestTransport
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

#: CTFd 提交回执 message 中的退避提示（核验 §CTFD：两种 429 形态都是这句）。
_TRY_AGAIN_RE = re.compile(r"Try again in (\d+)\s*seconds?", re.IGNORECASE)

#: data.status → 本地提交回执（correct/incorrect 之外的形态单独处理）。
_VERDICT_MAP = {
    "correct": "correct",
    "incorrect": "incorrect",
    # already_solved：flag 正确但无新增得分；SubmissionState 机有
    # duplicate_or_solved 分支，SubmissionResult.status 是非约束 str，取同义值。
    "already_solved": "duplicate",
    # partial：多答案题部分命中（multi-flag 插件）。
    "partial": "partial",
}


def parse_try_again_seconds(message: str) -> Optional[float]:
    """从 CTFd 限速 message 解析退避秒数；没有该提示时返回 None。"""
    match = _TRY_AGAIN_RE.search(message or "")
    return float(match.group(1)) if match else None


class CTFdAdapter:
    """绑定单个 ``PlatformConnection`` 的 CTFd REST Adapter。

    构造时只保存连接与 secret 存储引用；``client_transport`` 供测试注入
    ``httpx.MockTransport``，生产为 None（真实网络）。
    """

    id = PlatformKind.CTFD.value

    def __init__(
        self,
        connection: PlatformConnection,
        secrets: PlatformSecretStore,
        *,
        client_transport: Optional[Any] = None,
    ) -> None:
        if connection.platform_kind != self.id:
            raise ValueError(
                f"CTFdAdapter requires platform_kind={self.id!r}, "
                f"got {connection.platform_kind!r}"
            )
        self._connection = connection
        self._secrets = secrets
        self._client_transport = client_transport
        self._transport: Optional[RestTransport] = None

    @property
    def connection(self) -> PlatformConnection:
        return self._connection

    # ------------------------------------------------------------------
    # 传输与凭据
    # ------------------------------------------------------------------

    def _api(self) -> RestTransport:
        """按需构造 REST 传输；token 只在这里短暂解引用为请求头。"""
        if self._transport is None:
            token = self._secrets.resolve(self._connection.credential_ref)
            self._transport = RestTransport(
                self._connection.canonical_base_url,
                default_headers={
                    "Authorization": f"Token {token}",
                    "Content-Type": "application/json",
                },
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
    # 响应包裹解析
    # ------------------------------------------------------------------

    @staticmethod
    def _envelope(response: RestResponse, context: str) -> Any:
        """解析 ``{success, data}`` 包裹；success=false 视为平台错误。"""
        body = response.json_body
        if not isinstance(body, dict) or "success" not in body:
            raise PlatformTransportError(
                f"{context}: invalid CTFd envelope (HTTP {response.status_code})",
                status_code=response.status_code,
            )
        if not body.get("success"):
            raise PlatformTransportError(
                f"{context}: CTFd reported failure (HTTP {response.status_code})",
                status_code=response.status_code,
                detail={"response_body": body},
            )
        return body.get("data")

    # ------------------------------------------------------------------
    # 能力探测（设计 8.2）
    # ------------------------------------------------------------------

    async def probe(
        self, connection: Optional[PlatformConnectionRef] = None
    ) -> PlatformCapabilities:
        self._check_ref(connection)
        api = self._api()
        probe = CapabilityProbe(
            platform_kind=self.id,
            auth_methods=["token_header"],
            sync_modes=["full"],          # 列表端点不分页、无增量游标
            attachments=True,
            hints=True,
            prerequisites=True,           # requirements.prerequisites 服务端判定
            submit=True,
            scoreboard=False,             # 核验文档未覆盖 scoreboard，保守关闭
            rate_limit=CONSERVATIVE_RATE_LIMIT.model_copy(),
        )
        # 选手端列表确认 token 可用（401 由 transport 抛 PlatformAuthRequiredError）。
        data = self._envelope(
            await api.get("/api/v1/challenges", context="ctfd probe challenges"),
            "ctfd probe challenges",
        )
        probe.evidence["visible_challenges"] = len(data) if isinstance(data, list) else 0
        # 管理端点探测 token 权限级别（核验 §CTFD 影响第 4 条）。
        try:
            await api.get("/api/v1/configs", context="ctfd probe configs")
            probe.auth_detail["token_scope"] = "admin"
        except PlatformPermissionError:
            probe.auth_detail["token_scope"] = "user"
        probe.evidence.update(
            {
                "platform": "ctfd",
                "api_prefix": "/api/v1",
                # kpm / max_attempts_behavior 是部署配置，探测不到；
                # 429 时由提交路径从 message 解析真实退避。
                "rate_limit_note": "deployment-configured; backoff parsed from "
                "'Try again in N seconds' on 429",
            }
        )
        probe.rate_limit.source = RateLimitSource.CONNECTION_DEFAULT.value
        return probe.build_capabilities()

    # ------------------------------------------------------------------
    # 题目同步（设计 8.1 sync；保真映射到 RemoteChallengeSnapshot）
    # ------------------------------------------------------------------

    async def list_challenges(self) -> list[RemoteChallengeSnapshot]:
        """全量同步：列表 + 逐题详情。hidden 占位题映射为 hidden 快照。"""
        api = self._api()
        items = self._envelope(
            await api.get("/api/v1/challenges", context="ctfd list challenges"),
            "ctfd list challenges",
        )
        if not isinstance(items, list):
            raise PlatformTransportError("ctfd list challenges: data is not a list")
        snapshots: list[RemoteChallengeSnapshot] = []
        for item in items:
            if not isinstance(item, dict):
                continue
            if item.get("type") == "hidden":
                # 未解锁占位（核验 §CTFD）：不当作真实题目，不产生 revision。
                snapshots.append(self._hidden_snapshot(item))
                continue
            detail = await self.get_challenge_detail(str(item.get("id", "")))
            snapshots.append(self._to_snapshot(item, detail))
        return snapshots

    async def get_challenge_detail(self, external_id: str) -> dict[str, Any]:
        """拉取题目详情原文（含签名 files / hints / view / attempts）。"""
        api = self._api()
        data = self._envelope(
            await api.get(
                f"/api/v1/challenges/{external_id}",
                context=f"ctfd challenge detail {external_id}",
            ),
            f"ctfd challenge detail {external_id}",
        )
        if not isinstance(data, dict):
            raise PlatformTransportError(
                f"ctfd challenge detail {external_id}: data is not an object"
            )
        return data

    @staticmethod
    def _hidden_snapshot(item: dict[str, Any]) -> RemoteChallengeSnapshot:
        return RemoteChallengeSnapshot(
            external_challenge_id=str(item.get("id", "")),
            name=str(item.get("name") or ""),
            category=str(item.get("category") or ""),
            remote_state="hidden",
            hidden=True,
            raw_payload=dict(item),
        )

    @staticmethod
    def _to_snapshot(
        item: dict[str, Any], detail: dict[str, Any]
    ) -> RemoteChallengeSnapshot:
        hints = [
            str(h.get("content", ""))
            for h in detail.get("hints") or []
            if isinstance(h, dict)
        ]
        # 附件只保留 location（/files/<location>?token=… 的签名有时效，
        # 核验 §CTFD 影响第 3 条）；sha256 选手侧不可得，下载落 CAS 后回填。
        artifacts: list[RevisionArtifact] = []
        for raw_url in detail.get("files") or []:
            location = str(raw_url).split("?", 1)[0]
            name = location.rstrip("/").rsplit("/", 1)[-1]
            artifacts.append(RevisionArtifact(name=name))
        solved = bool(detail.get("solved_by_me") or item.get("solved_by_me"))
        return RemoteChallengeSnapshot(
            external_challenge_id=str(item.get("id", "")),
            name=str(item.get("name") or detail.get("name") or ""),
            category=str(item.get("category") or detail.get("category") or ""),
            remote_state="solved_remote" if solved else "open",
            points=float(detail.get("value") or item.get("value") or 0),
            description=str(detail.get("description") or ""),
            hints=hints,
            artifacts=artifacts,
            revision_extensions={
                # 自定义 challenge type（dynamic_iac / ctfd-whale 等插件）
                # 的原始字段全部保留，不进冻结的契约模型。
                "type": detail.get("type") or item.get("type") or "",
                "max_attempts": detail.get("max_attempts"),
                "attempts": detail.get("attempts"),
                "solves": detail.get("solves", item.get("solves")),
                "tags": detail.get("tags") or item.get("tags") or [],
                "module_id": item.get("module_id"),
            },
            raw_payload={"list": dict(item), "detail": dict(detail)},
        )

    async def sync_competition(self, request: SyncRequest) -> SyncResult:
        """CTFd 单实例即一场比赛：全量同步，无增量游标。"""
        self._check_ref(
            PlatformConnectionRef(connection_id=request.connection_id)
        )
        snapshots = await self.list_challenges()
        hidden = sum(1 for s in snapshots if s.hidden)
        return SyncResult(
            connection_id=request.connection_id,
            cursor=None,
            synced_challenges=len(snapshots) - hidden,
            detail={
                "external_competition_id": "default",
                "hidden_challenges": hidden,
                "snapshots": [s.model_dump(mode="json") for s in snapshots],
            },
        )

    # ------------------------------------------------------------------
    # 附件下载（签名 URL 用时刷新 + sha256 校验）
    # ------------------------------------------------------------------

    async def fetch_artifact(self, artifact: RemoteArtifactRef) -> ArtifactObject:
        """下载附件并校验 sha256。

        ``artifact.url`` 允许两种形态：完整签名 URL（含 ``?token=``）直接
        下载；只有 location（无查询串）时先重新拉详情刷新签名 URL
        （核验 §CTFD：token 短时效，revision 不存完整 URL）。
        """
        url = artifact.url
        if "?" not in url:
            url = await self._refresh_signed_url(artifact.remote_id, url)
        content = await self._api().download(
            url, context=f"ctfd download {artifact.remote_id}"
        )
        digest = hashlib.sha256(content).hexdigest()
        if artifact.sha256 and artifact.sha256.lower() != digest:
            raise PlatformTransportError(
                f"ctfd download {artifact.remote_id}: sha256 mismatch",
                detail={"expected": artifact.sha256, "actual": digest},
            )
        return ArtifactObject(sha256=digest, content=content)

    async def _refresh_signed_url(self, external_id: str, location: str) -> str:
        """按 location 从题目详情刷新签名下载 URL。"""
        detail = await self.get_challenge_detail(external_id)
        for raw_url in detail.get("files") or []:
            if str(raw_url).split("?", 1)[0] == location.split("?", 1)[0]:
                return str(raw_url)
        raise PlatformTransportError(
            f"ctfd download {external_id}: file location not found in detail",
            detail={"location": location},
        )

    # ------------------------------------------------------------------
    # 提交（data.status 语义优先于 HTTP code）
    # ------------------------------------------------------------------

    async def submit(self, request: SubmissionRequest) -> SubmissionResult:
        api = self._api()
        try:
            response = await api.post(
                "/api/v1/challenges/attempt",
                json_body={
                    "challenge_id": int(request.challenge_key),
                    "submission": request.flag or "",
                },
                context=f"ctfd attempt {request.challenge_key}",
            )
        except PlatformTransportError as exc:
            self._reclassify_attempt_error(exc)
            raise  # pragma: no cover - _reclassify_attempt_error 必抛
        data = self._envelope(response, f"ctfd attempt {request.challenge_key}")
        if not isinstance(data, dict):
            raise PlatformTransportError(
                f"ctfd attempt {request.challenge_key}: data is not an object"
            )
        status = str(data.get("status") or "")
        message = str(data.get("message") or "")
        if status == "ratelimited":
            raise PlatformRateLimitedError(
                f"ctfd attempt {request.challenge_key}: ratelimited",
                retry_after_seconds=parse_try_again_seconds(message),
                detail={"remote_status": status},
            )
        if status == "authentication_required":
            raise PlatformAuthRequiredError(
                f"ctfd attempt {request.challenge_key}: authentication required",
                detail={"remote_status": status},
            )
        if status == "paused":
            raise PlatformTransportError(
                f"ctfd attempt {request.challenge_key}: competition paused",
                detail={"remote_status": status},
            )
        verdict = _VERDICT_MAP.get(status)
        if verdict is None:
            raise PlatformTransportError(
                f"ctfd attempt {request.challenge_key}: unknown status",
                detail={"remote_status": status},
            )
        return SubmissionResult(
            status=verdict,
            detail={"remote_status": status, "remote_receipt": message},
        )

    @staticmethod
    def _reclassify_attempt_error(exc: PlatformTransportError) -> None:
        """按错误体 ``data.status`` 改写 403/429 的分类（200/403 混合语义）。

        - 429 / 403 + ``status: "ratelimited"``（限速或并发提交锁）→ 可重试限速；
        - 403 + ``authentication_required`` → 认证失效；
        - 403 + ``paused`` → 比赛暂停（不可重试平台错误）；
        - 403 无 body → max_attempts lockout（不可重试）。
        """
        body = exc.detail.get("response_body")
        status = ""
        message = ""
        if isinstance(body, dict):
            data = body.get("data")
            if isinstance(data, dict):
                status = str(data.get("status") or "")
                message = str(data.get("message") or "")
        retry_after = parse_try_again_seconds(message) or exc.retry_after_seconds
        if status == "ratelimited":
            raise PlatformRateLimitedError(
                f"ctfd attempt: ratelimited (HTTP {exc.status_code})",
                status_code=exc.status_code,
                retry_after_seconds=retry_after,
                detail={"remote_status": status},
            ) from exc
        if status == "authentication_required":
            raise PlatformAuthRequiredError(
                "ctfd attempt: authentication required",
                status_code=exc.status_code,
                detail={"remote_status": status},
            ) from exc
        if status == "paused":
            raise PlatformTransportError(
                "ctfd attempt: competition paused",
                status_code=exc.status_code,
                detail={"remote_status": status},
            ) from exc
        if isinstance(exc, PlatformPermissionError) and not isinstance(body, dict):
            # 挑战锁定 / max_attempts_behavior=lockout：403 无 body（核验 §CTFD）。
            exc.detail["max_attempts_exhausted"] = True
        raise exc

    # ------------------------------------------------------------------
    # 动态实例：核验的 REST 面不提供（插件私有），能力关闭、调用显式失败
    # ------------------------------------------------------------------

    async def acquire_instance(self, challenge: PlatformChallengeRef) -> InstanceResult:
        raise PlatformTransportError(
            "ctfd: dynamic instances are plugin-specific and not part of the "
            "verified REST surface",
            detail={"capability": "dynamic_instances", "available": False},
        )

    async def renew_instance(self, lease: InstanceLeaseRef) -> InstanceResult:
        raise PlatformTransportError(
            "ctfd: dynamic instances not available",
            detail={"capability": "dynamic_instances", "available": False},
        )

    async def release_instance(self, lease: InstanceLeaseRef) -> None:
        raise PlatformTransportError(
            "ctfd: dynamic instances not available",
            detail={"capability": "dynamic_instances", "available": False},
        )


__all__ = ["CTFdAdapter", "parse_try_again_seconds"]
