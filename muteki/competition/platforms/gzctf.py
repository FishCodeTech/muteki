"""GZCTF 平台 Adapter（任务书 10.3 / 设计 8.4 阶段 3，COMP-03）。

端点与语义以 docs/research/third_party_verification.md §GZCTF 为准
（核验基线：GZTimeWalker/GZCTF@develop，最新 release v1.8.1）。**GZCTF
没有选手端 API 稳定性承诺**（路由无版本前缀、无 deprecation 策略），因此
本 Adapter 不把请求路径当作固定常量盲信：probe 时拉取目标实例的
``GET /openapi/v1.json``，校验所需 path 存在，保存平台版本与 schema
hash 快照；快照 hash 变化或必需 path 缺失即报告为 capability 变化。

- 认证：ASP.NET Identity cookie 会话。``POST /api/account/login``
  （body ``{userName, password}``）拿 cookie；任何请求 401 时强制重新
  登录并重试一次，仍 401 则上抛 ``PlatformAuthRequiredError``。
  ``account_key`` 为空时把凭据当作已导入的会话 cookie 值使用。
- 比赛：``GET /api/game``（``count/skip`` 分页，带 ETag/304）；
  选手侧题目列表 ``GET /api/game/{id}/details``；题目详情
  ``GET /api/game/{id}/challenges/{challengeId}``。
- 附件：``GET /Assets/{hash}/{filename}``（**不在 /api 前缀下**，无鉴权）。
- 提交（**异步判定**）：``POST /api/Game/{id}/Challenges/{challengeId}``
  body ``FlagSubmitModel{flag}``，返回 200 + submission id（int）；随后
  ``GET .../Status/{submitId}`` 轮询 ``AnswerResult``（``CheatDetected``
  对外映射为 ``WrongAnswer``）。每题 ``SubmissionLimit`` 超限返回 400
  ``SubmissionLimitExceeded``（不可重试）；操作过频 429。
- 动态容器：``POST /api/Game/{id}/Container/{challengeId}``（创建）、
  ``POST .../Extend``（续租）、``DELETE``（释放），挂 LimitPolicy.Container。

``challenge_key`` 采用 ``<gameId>/<challengeId>`` 复合形态（GZCTF 的题目
身份二元组）。凭据只以 ``secret://`` 引用保存，登录密码只在 ``_login``
 边界短暂解引用。
"""

from __future__ import annotations

import hashlib
import json
from datetime import datetime
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
    PlatformTransportError,
    RemoteChallengeSnapshot,
)
from muteki.competition.secrets import PlatformSecretStore
from muteki.competition.transports import RestResponse, RestTransport
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

#: probe 校验的必需 path（核验 §GZCTF 已确认端点；缺失即 capability 降级）。
REQUIRED_OPENAPI_PATHS: tuple[str, ...] = (
    "/api/game",
    "/api/game/{id}",
    "/api/game/{id}/details",
    "/api/game/{id}/challenges/{challengeid}",
    "/api/game/{id}/challenges/{challengeid}/status/{submitid}",
    "/api/game/{id}/container/{challengeid}",
    "/api/game/{id}/container/{challengeid}/extend",
    "/api/account/login",
)

#: 容器能力对应的 OpenAPI path（缺失时 dynamic_instances 降级为 False）。
_CONTAINER_PATH = "/api/game/{id}/container/{challengeid}"

#: ASP.NET Identity 默认会话 cookie 名（直接导入 cookie 凭据时使用；
#: 目标实例若自定义需在登录流程获取，见模块 docstring）。
GZCTF_SESSION_COOKIE = "GZCTF_Token"

#: /api/Game 分页步长（核验：count/skip 分页 + 60s 响应缓存）。
_GAMES_PAGE_SIZE = 50

#: AnswerResult → 本地提交状态（CheatDetected 对外映射 WrongAnswer，核验 §GZCTF）。
_ANSWER_RESULT_MAP = {
    "Accepted": "correct",
    "WrongAnswer": "incorrect",
    "CheatDetected": "incorrect",
}

#: 400 ProblemDetails title → 不可重试的每题限制（其余 400 按平台错误上抛）。
_PERMISSION_TITLES = frozenset({"SubmissionLimitExceeded"})


def openapi_schema_hash(document: dict[str, Any]) -> str:
    """OpenAPI 文档的稳定快照 hash：paths（含方法集）+ schema 名清单。

    只对结构敏感（路由增删、方法变化、schema 增删），对描述文案不敏感，
    避免文档措辞调整误报 API 变化。
    """
    paths = document.get("paths") or {}
    canonical_paths = {
        path: sorted(m for m in methods if isinstance(methods, dict) and m != "parameters")
        if isinstance(methods, dict)
        else []
        for path, methods in sorted(paths.items())
    }
    schemas = sorted(
        ((document.get("components") or {}).get("schemas") or {}).keys()
    )
    raw = json.dumps(
        {"paths": canonical_paths, "schemas": schemas},
        ensure_ascii=False,
        sort_keys=True,
        separators=(",", ":"),
    )
    return hashlib.sha256(raw.encode("utf-8")).hexdigest()


class GZCTFAdapter:
    """绑定单个 ``PlatformConnection`` 的 GZCTF REST Adapter（cookie 会话）。"""

    id = PlatformKind.GZCTF.value

    def __init__(
        self,
        connection: PlatformConnection,
        secrets: PlatformSecretStore,
        *,
        client_transport: Optional[Any] = None,
    ) -> None:
        if connection.platform_kind != self.id:
            raise ValueError(
                f"GZCTFAdapter requires platform_kind={self.id!r}, "
                f"got {connection.platform_kind!r}"
            )
        self._connection = connection
        self._secrets = secrets
        self._client_transport = client_transport
        self._transport: Optional[RestTransport] = None
        self._logged_in = False

    @property
    def connection(self) -> PlatformConnection:
        return self._connection

    # ------------------------------------------------------------------
    # 传输、凭据与会话
    # ------------------------------------------------------------------

    def _api(self) -> RestTransport:
        if self._transport is None:
            self._transport = RestTransport(
                self._connection.canonical_base_url,
                transport=self._client_transport,
            )
        return self._transport

    async def aclose(self) -> None:
        if self._transport is not None:
            await self._transport.aclose()
            self._transport = None
        self._logged_in = False

    def _check_ref(self, connection: Optional[PlatformConnectionRef]) -> None:
        if connection is not None and connection.connection_id != self._connection.connection_id:
            raise ValueError(
                f"connection ref {connection.connection_id} does not match "
                f"bound connection {self._connection.connection_id}"
            )

    async def _ensure_session(self) -> None:
        """确保会话可用：用户名+密码登录，或导入 cookie 凭据。

        密码 / cookie 值只在本边界短暂解引用，绝不进入日志或错误消息。
        """
        if self._logged_in:
            return
        if not self._connection.account_key:
            # 直接导入 cookie 形态：凭据值即会话 cookie。
            cookie_value = self._secrets.resolve(self._connection.credential_ref)
            self._api().cookies.set(
                GZCTF_SESSION_COOKIE, cookie_value
            )
            self._logged_in = True
            return
        await self._login()

    async def _login(self) -> None:
        """POST /api/account/login；401/403 上抛认证/权限错误。"""
        password = self._secrets.resolve(self._connection.credential_ref)
        await self._api().post(
            "/api/account/login",
            json_body={
                "userName": self._connection.account_key,
                "password": password,
            },
            context="gzctf login",
        )
        self._logged_in = True

    # ------------------------------------------------------------------
    # 请求包装：401 重登录一次 + ProblemDetails title 分发
    # ------------------------------------------------------------------

    async def _request(
        self, method: str, path: str, *, retry_auth: bool = True, **kwargs: Any
    ) -> RestResponse:
        await self._ensure_session()
        try:
            return await self._api().request(method, path, **kwargs)
        except PlatformAuthRequiredError:
            if not retry_auth or not self._connection.account_key:
                raise
            # 会话过期：强制重新登录后重试一次；仍 401 → auth_required。
            self._logged_in = False
            await self._login()
            return await self._api().request(method, path, **kwargs)
        except PlatformTransportError as exc:
            self._annotate_problem(exc)
            raise

    @staticmethod
    def _annotate_problem(exc: PlatformTransportError) -> None:
        """读取 ASP.NET ProblemDetails 的 title，做 400 细分。

        ``SubmissionLimitExceeded``（每题提交上限）映射为不可重试的
        ``PlatformPermissionError``；``GameNotStarted`` / ``GameEnded`` /
        ``ContainerAlreadyCreated`` 等保留原标题上抛供调度分支。
        """
        body = exc.detail.get("response_body")
        title = ""
        if isinstance(body, dict):
            title = str(body.get("title") or body.get("detail") or "")
        if not title:
            return
        exc.detail["title"] = title
        if exc.status_code == 400 and title in _PERMISSION_TITLES:
            raise PlatformPermissionError(
                f"gzctf: {title}",
                status_code=exc.status_code,
                detail=dict(exc.detail),
            ) from exc

    # ------------------------------------------------------------------
    # 能力探测：OpenAPI 快照与 schema 变化检测（设计 8.2 / 核验 §GZCTF）
    # ------------------------------------------------------------------

    async def fetch_openapi(self) -> dict[str, Any]:
        """拉取目标实例的 OpenAPI 文档（能力发现的唯一依据）。"""
        response = await self._request(
            "GET", "/openapi/v1.json", context="gzctf openapi"
        )
        document = response.json_body
        if not isinstance(document, dict) or "paths" not in document:
            raise PlatformTransportError(
                "gzctf openapi: invalid OpenAPI document",
                status_code=response.status_code,
            )
        return document

    async def probe(
        self, connection: Optional[PlatformConnectionRef] = None
    ) -> PlatformCapabilities:
        self._check_ref(connection)
        document = await self.fetch_openapi()
        info = document.get("info") or {}
        version = str(info.get("version") or "")
        paths = {
            str(path).lower(): methods
            for path, methods in (document.get("paths") or {}).items()
        }
        schema_hash = openapi_schema_hash(document)
        missing = [p for p in REQUIRED_OPENAPI_PATHS if p not in paths]

        # 与连接上保存的上一次快照比对：hash 变化 / 必需 path 缺失
        # 都报告为 capability 变化（核验 §GZCTF：无 API 稳定性承诺）。
        previous = self._previous_evidence()
        previous_hash = str(previous.get("openapi_schema_hash") or "")
        schema_changed = bool(previous_hash) and previous_hash != schema_hash

        probe = CapabilityProbe(
            platform_kind=self.id,
            auth_methods=["cookie_session"],
            sync_modes=["full", "etag"],    # Details / Game 广泛支持 ETag/304
            attachments=True,
            hints=True,
            submit=True,
            submit_async=True,              # 提交 → submission id → 轮询 Status
            dynamic_instances=_CONTAINER_PATH in paths,
            instance_renewable=True,
            instance_stoppable=True,
            scoreboard=True,
            rate_limit=CONSERVATIVE_RATE_LIMIT.model_copy(),
            # LimitPolicy.Submit/Container 桶参数未逐行核验（核验 §GZCTF
            # 待确认第 2 条）：保持连接级保守默认并标注来源。
        )
        probe.evidence.update(
            {
                "platform": "gzctf",
                "platform_version": version,
                "openapi_url": "/openapi/v1.json",
                "openapi_schema_hash": schema_hash,
                "previous_schema_hash": previous_hash,
                "schema_changed": schema_changed,
                "openapi_required_missing": missing,
                "rate_limit_note": "LimitPolicy bucket parameters not verified; "
                "conservative connection default",
            }
        )
        return probe.build_capabilities()

    def _previous_evidence(self) -> dict[str, Any]:
        """读取连接上保存的上一次 probe 证据（安全元数据）。"""
        capabilities = self._connection.capabilities or {}
        detail = capabilities.get("detail")
        if isinstance(detail, dict) and isinstance(detail.get("evidence"), dict):
            return dict(detail["evidence"])
        # 兼容直接以 evidence 平铺保存的形态。
        if isinstance(capabilities.get("evidence"), dict):
            return dict(capabilities["evidence"])
        return {}

    # ------------------------------------------------------------------
    # 比赛与题目同步
    # ------------------------------------------------------------------

    async def list_competitions(self) -> list[dict[str, Any]]:
        """分页拉取比赛列表（count/skip + data/length/total 包裹）。"""
        games: list[dict[str, Any]] = []
        skip = 0
        for _ in range(100):
            response = await self._request(
                "GET",
                "/api/game",
                params={"count": _GAMES_PAGE_SIZE, "skip": skip},
                context="gzctf list games",
            )
            body = response.json_body
            if isinstance(body, list):
                items = body
                total = len(body)
            elif isinstance(body, dict):
                items = body.get("data") or []
                total = int(body.get("total") or body.get("length") or 0)
            else:
                items = []
                total = 0
            if not isinstance(items, list):
                raise PlatformTransportError(
                    "gzctf list games: data is not a list"
                )
            games.extend(
                self._to_competition(item)
                for item in items
                if isinstance(item, dict)
            )
            if len(items) < _GAMES_PAGE_SIZE or skip + len(items) >= total:
                break
            skip += len(items)
        else:
            raise PlatformTransportError(
                "gzctf list games: pagination exceeded 100 pages"
            )
        return games

    @staticmethod
    def _to_competition(item: dict[str, Any]) -> dict[str, Any]:
        return {
            "external_competition_id": str(item.get("id", "")),
            "title": str(item.get("title") or ""),
            "description": str(item.get("summary") or ""),
            # 原始 ISO 字符串，由 store 层解析为 datetime。
            "starts_at": item.get("start"),
            "ends_at": item.get("end"),
            "raw_payload": dict(item),
        }

    async def list_challenges(
        self, game_id: str, *, etag: str = ""
    ) -> tuple[list[RemoteChallengeSnapshot], str, bool]:
        """拉取一场比赛的题目快照。

        返回 ``(snapshots, etag, not_modified)``：携带 ``etag`` 时走条件
        请求，304 命中返回 ``not_modified=True`` 与空列表（增量同步游标）。
        题目内容（描述/提示/附件）逐题经详情端点补全。
        """
        headers = {"If-None-Match": etag} if etag else None
        response = await self._request(
            "GET",
            f"/api/game/{game_id}/details",
            headers=headers,
            context=f"gzctf game details {game_id}",
        )
        if response.not_modified:
            return [], response.etag or etag, True
        detail = response.json_body
        if not isinstance(detail, dict):
            raise PlatformTransportError(
                f"gzctf game details {game_id}: invalid GameDetailModel"
            )
        snapshots: list[RemoteChallengeSnapshot] = []
        for category, item in self._iter_challenge_items(detail):
            full = await self.get_challenge_detail(game_id, str(item.get("id", "")))
            snapshots.append(self._to_snapshot(game_id, category, item, full))
        return snapshots, response.etag, False

    @staticmethod
    def _iter_challenge_items(
        detail: dict[str, Any],
    ) -> list[tuple[str, dict[str, Any]]]:
        """GameDetailModel.Challenges 宽容展开：按类别分组的 dict 或平铺 list。"""
        challenges = detail.get("challenges") or detail.get("Challenges") or {}
        items: list[tuple[str, dict[str, Any]]] = []
        if isinstance(challenges, dict):
            for category, group in challenges.items():
                for entry in group or []:
                    if isinstance(entry, dict):
                        items.append((str(category), entry))
        elif isinstance(challenges, list):
            for entry in challenges:
                if isinstance(entry, dict):
                    items.append((str(entry.get("category") or ""), entry))
        return items

    async def get_challenge_detail(
        self, game_id: str, challenge_id: str
    ) -> dict[str, Any]:
        response = await self._request(
            "GET",
            f"/api/game/{game_id}/challenges/{challenge_id}",
            context=f"gzctf challenge detail {game_id}/{challenge_id}",
        )
        data = response.json_body
        if not isinstance(data, dict):
            raise PlatformTransportError(
                f"gzctf challenge detail {game_id}/{challenge_id}: invalid model"
            )
        return data

    @staticmethod
    def _to_snapshot(
        game_id: str,
        category: str,
        item: dict[str, Any],
        detail: dict[str, Any],
    ) -> RemoteChallengeSnapshot:
        hints = [str(h) for h in detail.get("hints") or item.get("hints") or []]
        # 附件：详情以 hash 链接给出（/Assets/<hash>/<filename>，核验 §GZCTF）；
        # 字段名以目标实例 OpenAPI 为准，宽容尝试 files / attachment 两种形态。
        artifacts: list[RevisionArtifact] = []
        for entry in detail.get("files") or []:
            if isinstance(entry, dict) and entry.get("url"):
                name = str(entry.get("name") or str(entry["url"]).rsplit("/", 1)[-1])
                artifacts.append(RevisionArtifact(name=name))
        attachment = detail.get("attachment")
        if isinstance(attachment, dict) and attachment.get("url"):
            name = str(
                attachment.get("name") or str(attachment["url"]).rsplit("/", 1)[-1]
            )
            artifacts.append(RevisionArtifact(name=name))
        context = detail.get("context")
        if isinstance(context, dict) and context.get("url"):
            url = str(context["url"])
            name = url.split("?", 1)[0].rstrip("/").rsplit("/", 1)[-1]
            artifacts.append(RevisionArtifact(
                name=name,
                size=int(context.get("fileSize") or 0),
            ))
        accepted = bool(detail.get("accepted") or item.get("accepted"))
        return RemoteChallengeSnapshot(
            external_challenge_id=f"{game_id}/{item.get('id', '')}",
            name=str(item.get("title") or detail.get("title") or ""),
            category=category or str(item.get("category") or ""),
            remote_state="solved_remote" if accepted else "open",
            points=float(detail.get("score") or item.get("score") or 0),
            description=str(detail.get("content") or detail.get("description") or ""),
            hints=hints,
            artifacts=artifacts,
            revision_extensions={
                # 自定义 type（StaticAttachment / DynamicAttachment /
                # DynamicContainer 及后续新增）原始字段全部保留。
                "type": detail.get("type") or item.get("type") or "",
                "solved": item.get("solved"),
                "attempts": detail.get("attempts"),
            },
            raw_payload={"list": dict(item), "detail": dict(detail)},
        )

    async def sync_competition(self, request: SyncRequest) -> SyncResult:
        """全量同步连接下所有比赛的题目。

        契约 ``SyncRequest`` 没有 external competition 字段（本地↔外部身份
        映射属 COMP-05 store 职责），因此 GZCTF 的 sync 以连接为粒度展开
        所有比赛；``request.cursor`` 传上一轮的 ETag 时各比赛走条件请求。
        """
        self._check_ref(PlatformConnectionRef(connection_id=request.connection_id))
        previous_etags: dict[str, str] = {}
        legacy_etag = ""
        if request.cursor:
            try:
                cursor_data = json.loads(request.cursor)
            except (TypeError, ValueError):
                legacy_etag = request.cursor
            else:
                if isinstance(cursor_data, dict) and isinstance(
                    cursor_data.get("etags"), dict
                ):
                    previous_etags = {
                        str(key): str(value)
                        for key, value in cursor_data["etags"].items()
                    }
                else:
                    legacy_etag = request.cursor
        games = await self.list_competitions()
        if request.competition_id:
            games = [
                game for game in games
                if game["external_competition_id"] == str(request.competition_id)
            ]
            if not games:
                raise PlatformTransportError(
                    f"gzctf competition {request.competition_id} was not found",
                    status_code=404,
                )
        all_snapshots: list[RemoteChallengeSnapshot] = []
        etags: dict[str, str] = {}
        unchanged = 0
        for game in games:
            game_id = game["external_competition_id"]
            snapshots, new_etag, not_modified = await self.list_challenges(
                game_id, etag=previous_etags.get(game_id, legacy_etag)
            )
            etags[game_id] = new_etag
            if not_modified:
                unchanged += 1
                continue
            all_snapshots.extend(snapshots)
        cursor = json.dumps({"etags": etags}, sort_keys=True)
        return SyncResult(
            connection_id=request.connection_id,
            cursor=cursor,
            synced_challenges=len(all_snapshots),
            detail={
                "games": [g["external_competition_id"] for g in games],
                "unchanged_games": unchanged,
                "snapshot_complete": unchanged == 0,
                "snapshots": [s.model_dump(mode="json") for s in all_snapshots],
            },
        )

    # ------------------------------------------------------------------
    # 附件下载（/Assets/ 根路径 + sha256 校验）
    # ------------------------------------------------------------------

    async def fetch_artifact(self, artifact: RemoteArtifactRef) -> ArtifactObject:
        content = await self._api().download(
            artifact.url, context=f"gzctf download {artifact.remote_id}"
        )
        digest = hashlib.sha256(content).hexdigest()
        if artifact.sha256 and artifact.sha256.lower() != digest:
            raise PlatformTransportError(
                f"gzctf download {artifact.remote_id}: sha256 mismatch",
                detail={"expected": artifact.sha256, "actual": digest},
            )
        return ArtifactObject(sha256=digest, content=content)

    # ------------------------------------------------------------------
    # 提交（异步判定：提交 → submission id → 轮询 Status）
    # ------------------------------------------------------------------

    @staticmethod
    def _split_key(challenge_key: str) -> tuple[str, str]:
        parts = str(challenge_key).split("/", 1)
        if len(parts) != 2 or not all(parts):
            raise PlatformTransportError(
                "gzctf: challenge_key must be '<gameId>/<challengeId>'",
                detail={"challenge_key": challenge_key},
            )
        return parts[0], parts[1]

    async def submit(self, request: SubmissionRequest) -> SubmissionResult:
        """提交 flag，返回 ``pending`` 回执（submission id 在 submission_id）。

        判定结果异步产生，调用方用 ``poll_submission`` 核对远端状态
        （SubmissionState 机的 remote_pending 分支）。
        """
        game_id, challenge_id = self._split_key(request.challenge_key)
        response = await self._request(
            "POST",
            f"/api/game/{game_id}/challenges/{challenge_id}",
            json_body={"flag": request.flag or ""},
            context=f"gzctf submit {request.challenge_key}",
        )
        body = response.json_body
        if isinstance(body, bool) or not isinstance(body, (int, str)):
            raise PlatformTransportError(
                f"gzctf submit {request.challenge_key}: invalid submission id",
                status_code=response.status_code,
            )
        return SubmissionResult(
            submission_id=str(body),
            status="pending",
            detail={"async": True, "game_id": game_id},
        )

    async def poll_submission(
        self, game_id: str, challenge_id: str, submit_id: str
    ) -> SubmissionResult:
        """轮询异步判定结果（``GET .../Status/{submitId}``，AnswerResult）。"""
        response = await self._request(
            "GET",
            f"/api/game/{game_id}/challenges/{challenge_id}/status/{submit_id}",
            context=f"gzctf submission status {submit_id}",
        )
        body = response.json_body
        answer = ""
        if isinstance(body, str):
            answer = body
        elif isinstance(body, dict):
            answer = str(body.get("status") or body.get("result") or "")
        verdict = _ANSWER_RESULT_MAP.get(answer)
        if verdict is None:
            # 未识别的枚举值（含仍在判定中的形态）：保持 pending。
            return SubmissionResult(
                submission_id=str(submit_id),
                status="pending",
                detail={"answer_result": answer},
            )
        return SubmissionResult(
            submission_id=str(submit_id),
            status=verdict,
            detail={"answer_result": answer},
        )

    # ------------------------------------------------------------------
    # 动态容器实例（创建 / 续租 / 释放）
    # ------------------------------------------------------------------

    async def acquire_instance(self, challenge: PlatformChallengeRef) -> InstanceResult:
        game_id, challenge_id = self._split_key(challenge.challenge_key)
        response = await self._request(
            "POST",
            f"/api/game/{game_id}/container/{challenge_id}",
            context=f"gzctf container create {challenge.challenge_key}",
        )
        return self._instance_result(challenge.challenge_key, response.json_body)

    async def renew_instance(self, lease: InstanceLeaseRef) -> InstanceResult:
        game_id, challenge_id = self._split_key(lease.challenge_key)
        response = await self._request(
            "POST",
            f"/api/game/{game_id}/container/{challenge_id}/extend",
            context=f"gzctf container extend {lease.challenge_key}",
        )
        return self._instance_result(lease.challenge_key, response.json_body)

    async def release_instance(self, lease: InstanceLeaseRef) -> None:
        game_id, challenge_id = self._split_key(lease.challenge_key)
        await self._request(
            "DELETE",
            f"/api/game/{game_id}/container/{challenge_id}",
            context=f"gzctf container delete {lease.challenge_key}",
        )

    def _instance_result(
        self, challenge_key: str, model: Any
    ) -> InstanceResult:
        """ContainerInfoModel 宽容解析：entry / port / expectStopAt。"""
        data = model if isinstance(model, dict) else {}
        endpoints: dict[str, str] = {}
        entry = data.get("entry")
        port = data.get("port")
        if entry and port:
            endpoints["tcp"] = f"{entry}:{port}"
        elif entry:
            endpoints["target"] = str(entry)
        expires_at = self._parse_dt(data.get("expectStopAt") or data.get("expiresAt"))
        return InstanceResult(
            lease=InstanceLeaseRef(
                # GZCTF 对一个队伍/题目只保留一个活动容器，响应模型不提供
                # 独立实例 id。使用题目键形成稳定远端身份，避免续租时由
                # InstanceLeaseRef 的随机缺省值误判为实例更换。
                lease_id=f"gzctf:{challenge_key}",
                connection_id=self._connection.connection_id,
                challenge_key=challenge_key,
                expires_at=expires_at,
            ),
            endpoints=endpoints,
            acquired_at=utcnow(),
        )

    @staticmethod
    def _parse_dt(value: Any) -> Optional[datetime]:
        if value in (None, ""):
            return None
        if isinstance(value, datetime):
            return value
        try:
            return datetime.fromisoformat(str(value).replace("Z", "+00:00"))
        except ValueError:
            return None


__all__ = [
    "GZCTF_SESSION_COOKIE",
    "GZCTFAdapter",
    "REQUIRED_OPENAPI_PATHS",
    "openapi_schema_hash",
]
