"""CompetitionSyncService：增量同步、diff、revision 与 tombstone（任务书 10.4
/ 设计 7.2、8.1，COMP-05）。

职责边界：

- 平台 Adapter（COMP-03/04）只产出 ``SyncResult``，``detail["snapshots"]``
  携带 ``RemoteChallengeSnapshot`` 列表；本服务消费快照、落库 diff。
- 增量标识：每轮读取/保存 ``SyncCursor``（kind="challenges"），把上一轮的
  cursor 传回 Adapter 的 ``SyncRequest.cursor``（GZCTF 的 ETag 条件请求、
  browser 的内容指纹游标也走这里）；Adapter 不支持增量时 cursor 为
  None，全量快照仍然由内容 hash 保证幂等。
- diff 分类：added / changed / unchanged / removed(tombstone) /
  unlocked / locked / solved（任务书 10.4「新增/变化/删除/解锁/已解决」，
  locked 是解锁的反向，供调度层区分）。
- revision：远端字段规范化（去首尾空白、换行归一、附件清单排序由
  ``revision_content_hash`` 内部保证）后计算内容 hash；hash 命中已有
  revision 行则复用（内容寻址去重），否则 ``revision_seq`` 单调递增落新行。
  hash 不含短期签名 URL / Cookie / 令牌 / 实例续租时间（COMP-01 契约）。
- 远端删除：``store.tombstone_challenge`` 形成 tombstone，历史 revision
  与 RunBinding 行全部保留（任务书 10.4）。
- 附件：经 ``CompetitionArtifactStore`` 进比赛级 CAS，sha256 去重；
  单个附件终态失败不中断整轮同步，记入 ``SyncReport.artifact_failures``
  并跳过该题本轮 revision 更新。
"""

from __future__ import annotations

import asyncio
from datetime import datetime, timezone
from typing import Any, Callable, Optional

from pydantic import Field

from muteki.competition import events as ev
from muteki.competition.artifacts import (
    ArtifactDownloadError,
    CompetitionArtifactStore,
)
from muteki.competition.models import (
    ChallengeArtifact,
    ChallengeRevision,
    Competition,
    CompetitionChallenge,
    RevisionArtifact,
    SyncCursor,
    revision_content_hash,
)
from muteki.competition.platforms.base import RemoteChallengeSnapshot
from muteki.competition.store import CompetitionStore, NotFoundError
from muteki.platform.contracts.base import ContractModel
from muteki.platform.contracts.commands import CommandEnvelope
from muteki.platform.contracts.modules import (
    RemoteArtifactRef,
    SyncRequest,
    SyncResult,
)

#: 同步落库完成事件（competition.* 命名空间内的新事件类型，局部定义避免
#: 改动 COMP-01 的 events 模块）。
SYNC_APPLIED = "competition.sync.applied"

#: 附件下载地址解析器：从快照（raw_payload 里的签名 URL / location）找到
#: 与附件同名的下载地址。签名 URL 只用于下载，绝不写入 revision 或 CAS
#: 元数据（设计 13.1）。
ArtifactUrlResolver = Callable[[RemoteChallengeSnapshot, str], str]


def _default_url_resolver(snapshot: RemoteChallengeSnapshot, name: str) -> str:
    """在 ``raw_payload`` 中递归查找与附件同名的 URL / location。

    覆盖已交付 Adapter 的三种 raw 形态：CTFd ``detail.files`` 的签名字符串、
    GZCTF ``detail.files`` 的 ``{"url", "name"}`` 对象、RCTF 顶层 ``files``
    的 ``{"name", ...}`` 对象。
    """
    found: list[str] = []

    def _basename(text: str) -> str:
        return text.split("?", 1)[0].rstrip("/").rsplit("/", 1)[-1]

    def _walk(node: Any) -> None:
        if isinstance(node, dict):
            url = node.get("url") or node.get("location")
            if isinstance(url, str) and url:
                entry_name = str(node.get("name") or "") or _basename(url)
                if entry_name == name:
                    found.append(url)
            for value in node.values():
                _walk(value)
        elif isinstance(node, (list, tuple)):
            for value in node:
                _walk(value)
        elif isinstance(node, str):
            if "/" in node and _basename(node) == name:
                found.append(node)

    _walk(snapshot.raw_payload)
    return found[0] if found else ""


class SyncReport(ContractModel):
    """一轮同步的确定性 diff 报告（供调度 / 审计 / 测试断言）。"""

    competition_id: str = ""
    added: list[str] = Field(default_factory=list)        # challenge_id
    changed: list[str] = Field(default_factory=list)      # 新 current revision
    unchanged: list[str] = Field(default_factory=list)
    removed: list[str] = Field(default_factory=list)      # tombstone
    unlocked: list[str] = Field(default_factory=list)     # hidden → 可见
    locked: list[str] = Field(default_factory=list)       # 可见 → hidden
    solved: list[str] = Field(default_factory=list)       # 远端标记已解决
    artifacts_downloaded: int = 0
    artifacts_deduped: int = 0
    artifact_failures: list[dict[str, Any]] = Field(default_factory=list)
    platform_status_changed: bool = False
    cursor: str = ""


def normalize_snapshot(snapshot: RemoteChallengeSnapshot) -> dict[str, Any]:
    """远端字段规范化：hash 输入必须与同步轮次、签名 URL 等瞬态无关。"""
    description = (
        snapshot.description.replace("\r\n", "\n").replace("\r", "\n")
    )
    return {
        "name": snapshot.name.strip(),
        "category": snapshot.category.strip(),
        "points": float(snapshot.points),
        "description": description.strip("\n"),
        "target": snapshot.target.strip(),
        "flag_format": snapshot.flag_format.strip(),
        "hints": [str(h).strip() for h in snapshot.hints],
        "prerequisites": [str(p).strip() for p in snapshot.prerequisites],
        "multi_flag": bool(snapshot.multi_flag),
        "expected_flags": max(1, int(snapshot.expected_flags or 1)),
    }


class CompetitionSyncService:
    """把一轮 Adapter 同步结果落进 competition.db 的服务。"""

    def __init__(
        self,
        store: CompetitionStore,
        artifacts: Optional[CompetitionArtifactStore] = None,
        *,
        url_resolver: Optional[ArtifactUrlResolver] = None,
    ) -> None:
        self._store = store
        self._artifacts = artifacts or CompetitionArtifactStore(store)
        self._resolve_url = url_resolver or _default_url_resolver
        self._locks: dict[str, asyncio.Lock] = {}

    # -- 主入口 ----------------------------------------------------------------

    async def sync(
        self,
        competition_id: str,
        adapter: Any,
        *,
        command: Optional[CommandEnvelope] = None,
    ) -> SyncReport:
        lock = self._locks.setdefault(competition_id, asyncio.Lock())
        async with lock:
            return await self._sync_once(
                competition_id, adapter, command=command
            )

    async def _sync_once(
        self,
        competition_id: str,
        adapter: Any,
        *,
        command: Optional[CommandEnvelope] = None,
    ) -> SyncReport:
        """同步一场比赛：拉快照 → diff → revision/tombstone → 存游标。"""
        competition = self._store.get(Competition, competition_id)
        if competition is None:
            raise NotFoundError(f"competition not found: {competition_id}")
        if competition.tombstoned:
            raise NotFoundError(
                f"competition {competition_id} is tombstoned"
            )
        cursor_row = self._store.get(SyncCursor, competition_id, "challenges")
        request = SyncRequest(
            connection_id=competition.connection_id,
            competition_id=competition.external_competition_id or None,
            cursor=(cursor_row.cursor or None) if cursor_row else None,
        )
        result: SyncResult = await adapter.sync_competition(request)
        competition, platform_status_changed = self._apply_competition_status(
            competition, result.detail
        )
        snapshots = [
            RemoteChallengeSnapshot.model_validate(raw)
            for raw in (result.detail.get("snapshots") or [])
        ]

        report = SyncReport(
            competition_id=competition_id,
            platform_status_changed=platform_status_changed,
        )
        seen_external: set[str] = set()
        for snapshot in snapshots:
            if not snapshot.external_challenge_id:
                continue
            seen_external.add(snapshot.external_challenge_id)
            await self._apply_snapshot(competition, snapshot, adapter, report, command)

        # 远端删除 → tombstone（历史 revision / RunBinding 保留）。条件请求
        # 命中 304 时 Adapter 返回空增量，不能把空增量解释成远端全量删除。
        if result.detail.get("snapshot_complete", True):
            for challenge in self._store.list(
                CompetitionChallenge, competition_id=competition_id
            ):
                if challenge.external_challenge_id in seen_external:
                    continue
                if challenge.tombstoned:
                    continue
                before = challenge.state
                self._store.tombstone_challenge(challenge.challenge_id)
                self._store.append_events([ev.make_event(
                    competition_id=competition_id,
                    aggregate_type=ev.AGG_CHALLENGE,
                    aggregate_id=challenge.challenge_id,
                    event_type=ev.CHALLENGE_STATE_CHANGED,
                    command=command,
                    payload={
                        "challenge_id": challenge.challenge_id,
                        "from": before,
                        "to": "retired",
                        "reason": "remote_removed",
                    },
                )])
                report.removed.append(challenge.challenge_id)

        # 增量游标：Adapter 不给新 cursor 时保留旧值（全量平台幂等靠 hash）。
        new_cursor = result.cursor or (cursor_row.cursor if cursor_row else "")
        self._store.save(SyncCursor(
            competition_id=competition_id,
            kind="challenges",
            cursor=new_cursor or "",
            etag=(cursor_row.etag if cursor_row else ""),
        ))
        report.cursor = new_cursor or ""
        # 定时刷新大多是无变化快照。只在最终状态确实变化时记事件，避免
        # 以固定频率把整场比赛的观察历史重新堆进数据库。
        if any((
            report.added,
            report.changed,
            report.removed,
            report.unlocked,
            report.locked,
            report.solved,
            report.artifact_failures,
            report.platform_status_changed,
        )):
            self._store.append_events([ev.make_event(
                competition_id=competition_id,
                aggregate_type=ev.AGG_COMPETITION,
                aggregate_id=competition_id,
                event_type=SYNC_APPLIED,
                command=command,
                payload={
                    "competition_id": competition_id,
                    "added": len(report.added),
                    "changed": len(report.changed),
                    "unchanged": len(report.unchanged),
                    "removed": len(report.removed),
                    "unlocked": len(report.unlocked),
                    "locked": len(report.locked),
                    "solved": len(report.solved),
                    "artifacts_downloaded": report.artifacts_downloaded,
                    "artifacts_deduped": report.artifacts_deduped,
                    "artifact_failures": len(report.artifact_failures),
                    "platform_status_changed": report.platform_status_changed,
                    "cursor": report.cursor,
                },
            )])
        return report

    def _apply_competition_status(
        self, competition: Competition, detail: dict[str, Any]
    ) -> tuple[Competition, bool]:
        """Persist stable adapter status while rejecting secret-shaped fields."""
        clock = detail.get("competition_clock")
        clock = clock if isinstance(clock, dict) else {}
        platform_status = detail.get("platform_status")
        platform_status = platform_status if isinstance(platform_status, dict) else {}
        forbidden = {"token", "credential", "cookie", "vpn_config", "private_key"}

        def _safe(node: Any) -> Any:
            if isinstance(node, dict):
                return {
                    str(key): _safe(value)
                    for key, value in node.items()
                    if str(key).lower() not in forbidden
                }
            if isinstance(node, list):
                return [_safe(value) for value in node[:100]]
            if isinstance(node, (str, int, float, bool)) or node is None:
                return node
            return str(node)

        updates: dict[str, Any] = {"platform_status": _safe(platform_status)}
        for source, target in (("started_at", "starts_at"), ("ends_at", "ends_at")):
            value = str(clock.get(source) or "").strip()
            if not value:
                continue
            try:
                parsed = datetime.fromisoformat(value.replace("Z", "+00:00"))
            except ValueError:
                continue
            if parsed.tzinfo is None:
                parsed = parsed.replace(tzinfo=timezone.utc)
            updates[target] = parsed
        before = competition.model_dump(
            mode="json", exclude={"updated_at"}
        )
        candidate = competition.model_copy(update=updates)
        after = candidate.model_dump(mode="json", exclude={"updated_at"})
        if before == after:
            return competition, False
        return self._store.save(candidate), True

    # -- 单题落库 ----------------------------------------------------------------

    async def _apply_snapshot(
        self,
        competition: Competition,
        snapshot: RemoteChallengeSnapshot,
        adapter: Any,
        report: SyncReport,
        command: Optional[CommandEnvelope],
    ) -> None:
        challenge = self._store.challenge_by_external(
            competition.competition_id, snapshot.external_challenge_id
        )
        was_hidden = challenge is not None and challenge.remote_state == "hidden"
        if challenge is not None and challenge.tombstoned:
            # 已 tombstone 的题目复活属 Operator 决策，同步不隐式恢复。
            report.unchanged.append(challenge.challenge_id)
            return

        if snapshot.hidden:
            # 未解锁占位题：只落身份与 hidden 状态，不产生 revision。
            state_events: list[dict[str, str]] = []
            if challenge is None:
                challenge = CompetitionChallenge(
                    competition_id=competition.competition_id,
                    external_challenge_id=snapshot.external_challenge_id,
                    name=snapshot.name.strip(),
                    category=snapshot.category.strip(),
                    remote_state="hidden",
                )
                self._store.save(challenge)
                state_events.append({"from": "", "to": "hidden"})
                report.locked.append(challenge.challenge_id)
            elif challenge.remote_state != "hidden":
                challenge = challenge.model_copy(update={
                    "remote_state": "hidden",
                    "name": snapshot.name.strip() or challenge.name,
                    "category": snapshot.category.strip() or challenge.category,
                })
                self._store.save(challenge)
                state_events.append({
                    "from": "open", "to": "hidden", "reason": "remote_locked",
                })
                report.locked.append(challenge.challenge_id)
            else:
                report.unchanged.append(challenge.challenge_id)
            for extra in state_events:
                self._store.append_events([ev.make_event(
                    competition_id=competition.competition_id,
                    aggregate_type=ev.AGG_CHALLENGE,
                    aggregate_id=challenge.challenge_id,
                    event_type=ev.CHALLENGE_STATE_CHANGED,
                    command=command,
                    payload={"challenge_id": challenge.challenge_id, **extra},
                )])
            return

        fields = normalize_snapshot(snapshot)
        try:
            artifacts = await self._resolve_artifacts(
                competition, challenge, snapshot, adapter, report
            )
        except ArtifactDownloadError as exc:
            # 附件终态失败：跳过该题本轮 revision 更新，不中断整轮同步。
            report.artifact_failures.append({
                "external_challenge_id": snapshot.external_challenge_id,
                "challenge_id": challenge.challenge_id if challenge else "",
                "category": exc.category.value,
                "message": str(exc),
            })
            return

        content_hash = revision_content_hash(artifacts=artifacts, **fields)
        revision: Optional[ChallengeRevision] = None
        is_new_revision = False
        if challenge is not None:
            revision = self._store.revision_by_hash(
                challenge.challenge_id, content_hash
            )
        if challenge is None:
            challenge = CompetitionChallenge(
                competition_id=competition.competition_id,
                external_challenge_id=snapshot.external_challenge_id,
                name=fields["name"],
                category=fields["category"],
                remote_state=snapshot.remote_state,
            )
            self._store.save(challenge)
            self._store.append_events([ev.make_event(
                competition_id=competition.competition_id,
                aggregate_type=ev.AGG_CHALLENGE,
                aggregate_id=challenge.challenge_id,
                event_type=ev.CHALLENGE_DISCOVERED,
                command=command,
                payload={
                    "challenge_id": challenge.challenge_id,
                    "external_challenge_id": snapshot.external_challenge_id,
                    "name": fields["name"],
                    "category": fields["category"],
                },
            )])
            report.added.append(challenge.challenge_id)
        elif was_hidden:
            report.unlocked.append(challenge.challenge_id)

        if revision is None:
            revision = ChallengeRevision(
                competition_challenge_id=challenge.challenge_id,
                content_hash=content_hash,
                revision_seq=self._store.next_revision_seq(challenge.challenge_id),
                artifacts=artifacts,
                **fields,
            )
            self._store.save(revision)
            for position, artifact in enumerate(artifacts):
                self._store.save(ChallengeArtifact(
                    revision_id=revision.revision_id,
                    sha256=artifact.sha256,
                    name=artifact.name,
                    position=position,
                ))
            self._store.append_events([ev.make_event(
                competition_id=competition.competition_id,
                aggregate_type=ev.AGG_CHALLENGE,
                aggregate_id=challenge.challenge_id,
                event_type=ev.REVISION_CREATED,
                command=command,
                payload={
                    "challenge_id": challenge.challenge_id,
                    "revision_id": revision.revision_id,
                    "revision_seq": revision.revision_seq,
                    "content_hash": content_hash,
                },
            )])
            is_new_revision = True

        previous_revision_id = challenge.current_revision_id
        previous_remote_state = challenge.remote_state
        updated = challenge.model_copy(update={
            "name": fields["name"],
            "category": fields["category"],
            "current_revision_id": revision.revision_id,
            "remote_state": snapshot.remote_state,
        })
        if updated.model_dump() != challenge.model_dump():
            self._store.save(updated)
        challenge = updated

        if challenge.challenge_id not in report.added:
            if previous_revision_id != revision.revision_id:
                report.changed.append(challenge.challenge_id)
            elif not was_hidden:
                report.unchanged.append(challenge.challenge_id)
        if (previous_remote_state != "solved_remote"
                and snapshot.remote_state == "solved_remote"):
            report.solved.append(challenge.challenge_id)
            self._store.append_events([ev.make_event(
                competition_id=competition.competition_id,
                aggregate_type=ev.AGG_CHALLENGE,
                aggregate_id=challenge.challenge_id,
                event_type=ev.CHALLENGE_STATE_CHANGED,
                command=command,
                payload={
                    "challenge_id": challenge.challenge_id,
                    "from": previous_remote_state,
                    "to": "solved_remote",
                    "reason": "remote_solved",
                    "revision_id": revision.revision_id,
                },
            )])
        elif is_new_revision and previous_revision_id is not None:
            # revision 切换对活动 Run 的处理由 binding 服务的策略表决定
            # （COMP-05 设计 7.2：活动 Run 保持原输入，不静默修改）。
            self._store.append_events([ev.make_event(
                competition_id=competition.competition_id,
                aggregate_type=ev.AGG_CHALLENGE,
                aggregate_id=challenge.challenge_id,
                event_type=ev.CHALLENGE_STATE_CHANGED,
                command=command,
                payload={
                    "challenge_id": challenge.challenge_id,
                    "from": previous_revision_id,
                    "to": revision.revision_id,
                    "reason": "revision_changed",
                },
            )])

    # -- 附件解析与下载 ------------------------------------------------------------

    async def _resolve_artifacts(
        self,
        competition: Competition,
        challenge: Optional[CompetitionChallenge],
        snapshot: RemoteChallengeSnapshot,
        adapter: Any,
        report: SyncReport,
    ) -> list[RevisionArtifact]:
        """把快照附件清单补全为带 sha256/size/MIME 的 revision 清单。

        不重复下载的两条路径（任务书 10.4）：

        1. 快照给 sha256 且 CAS 已命中 → 直接复用；
        2. 快照不给 sha256（选手侧 API 常态）且与当前 revision 的附件
           按 (name, size) 完全一致 → 复用当前 revision 的 sha256，连同
           hash 一起保持不变，本轮不发起任何下载。
        """
        current: Optional[ChallengeRevision] = None
        if challenge is not None and challenge.current_revision_id:
            current = self._store.get(
                ChallengeRevision, challenge.current_revision_id
            )
        resolved: list[RevisionArtifact] = []
        for artifact in snapshot.artifacts:
            name = artifact.name.strip()
            sha256 = artifact.sha256.lower()
            size = int(artifact.size or 0)
            media_type = artifact.media_type
            if not sha256 and current is not None:
                match = next(
                    (a for a in current.artifacts
                     if a.name == name and (not size or a.size == size)),
                    None,
                )
                if match is not None:
                    sha256, size, media_type = (
                        match.sha256, match.size, match.media_type
                    )
            if sha256:
                cached = self._artifacts.get(sha256)
                if cached is not None:
                    report.artifacts_deduped += 1
                    resolved.append(RevisionArtifact(
                        sha256=sha256, name=name,
                        size=cached.size, media_type=cached.media_type,
                    ))
                    continue
            resolved.append(await self._download(
                competition, snapshot, adapter, name,
                sha256_hint=sha256, size=size, media_type=media_type,
                report=report,
            ))
        return resolved

    async def _download(
        self,
        competition: Competition,
        snapshot: RemoteChallengeSnapshot,
        adapter: Any,
        name: str,
        *,
        sha256_hint: str,
        size: int,
        media_type: str,
        report: SyncReport,
    ) -> RevisionArtifact:
        url = self._resolve_url(snapshot, name)
        if not url:
            raise ArtifactDownloadError(
                f"artifact {name!r} of challenge "
                f"{snapshot.external_challenge_id!r}: no download url in "
                "snapshot raw_payload"
            )
        ref = RemoteArtifactRef(
            connection_id=competition.connection_id,
            remote_id=snapshot.external_challenge_id,
            url=url,
            sha256=sha256_hint or None,
        )

        async def _fetch() -> tuple[bytes, str]:
            fetched = await adapter.fetch_artifact(ref)
            return fetched.content, fetched.media_type

        obj, downloaded = await self._artifacts.ensure(
            name=name,
            fetch=_fetch,
            sha256_hint=sha256_hint,
            # 来源只记连接与题目/附件身份，不记签名 URL（token 不落库）。
            origin=(f"{competition.connection_id}/"
                    f"{snapshot.external_challenge_id}/{name}"),
            media_type=media_type,
        )
        if downloaded:
            report.artifacts_downloaded += 1
        else:
            report.artifacts_deduped += 1
        return RevisionArtifact(
            sha256=obj.sha256, name=name,
            size=obj.size or size, media_type=obj.media_type or media_type,
        )


__all__ = [
    "ArtifactUrlResolver",
    "CompetitionSyncService",
    "SYNC_APPLIED",
    "SyncReport",
    "normalize_snapshot",
]
