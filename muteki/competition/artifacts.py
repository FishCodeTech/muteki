"""比赛级 Artifact CAS（任务书 10.4 / 设计 13.1，COMP-05）。

布局与设计 13.1 逐字对齐::

    <artifact_root>/sha256/<digest>

语义：

- 按 sha256 内容寻址去重：同一内容全比赛只下载、只存一份；重复附件命中
  CAS 后直接复用 ``ArtifactObject`` 元数据，不再发起下载。
- ``ArtifactObject``（COMP-01 契约）保存 sha256、大小、MIME、来源与本地
  对象路径；revision 侧的关联行是 ``ChallengeArtifact``（由 SyncService
  在 revision 落库时写入）。短期签名 URL 只用于下载，绝不进 CAS 元数据
  或 revision。
- 下载失败按 ``PlatformErrorCategory`` 分类；仅 ``retryable`` 的错误做
  有界重试（次数与退避可注入，测试用零退避），终态失败抛
  ``ArtifactDownloadError``，由 SyncService 记入同步报告并跳过该题本轮
  revision 更新。
- ``materialize_into_run`` 复用现有 ``materialize_input`` 把 CAS 对象
  复制进每 Run 的输入 CAS（设计 13.1 末段）。
"""

from __future__ import annotations

import hashlib
import mimetypes
import os
from pathlib import Path
from typing import Any, Awaitable, Callable, Optional

from muteki.competition.models import ArtifactObject
from muteki.competition.platforms.base import (
    PlatformErrorCategory,
    PlatformTransportError,
)
from muteki.competition.store import CompetitionStore
from muteki.solver.workspace import materialize_input

#: 下载结果：（内容字节, MIME 或空串）。
FetchResult = tuple[bytes, str]
#: 单次下载尝试的回调；由 SyncService 用平台 Adapter 的 fetch_artifact 构造。
FetchFn = Callable[[], Awaitable[FetchResult]]


class ArtifactDownloadError(PlatformTransportError):
    """附件下载终态失败（重试耗尽或不可重试错误）。

    继承 ``PlatformTransportError`` 保留 typed 分类，SyncService 据此把
    失败记入报告而不是中断整轮同步。
    """

    def __init__(
        self,
        message: str,
        *,
        category: PlatformErrorCategory = PlatformErrorCategory.PLATFORM,
        **kwargs: Any,
    ) -> None:
        super().__init__(message, **kwargs)
        # 基类 category 是类属性；失败分类按实例携带。
        self.category = category


def guess_media_type(name: str) -> str:
    """按文件名猜测 MIME；未知时归 ``application/octet-stream``。"""
    return mimetypes.guess_type(name)[0] or "application/octet-stream"


class CompetitionArtifactStore:
    """比赛级不可变对象缓存 + ``ArtifactObject`` 元数据落库。"""

    def __init__(
        self,
        store: CompetitionStore,
        root: str | Path | None = None,
        *,
        max_download_attempts: int = 2,
        retry_delay_seconds: float = 0.5,
    ) -> None:
        self._store = store
        # 默认与 competition.db 同目录下的 competition-artifacts/。
        self.root = Path(root) if root is not None else (
            Path(store.db_path).parent / "competition-artifacts"
        )
        self.max_download_attempts = max(1, int(max_download_attempts))
        self.retry_delay_seconds = float(retry_delay_seconds)

    # -- 路径与查询 -----------------------------------------------------------

    def object_path(self, sha256: str) -> Path:
        return self.root / "sha256" / sha256.lower()

    def get(self, sha256: str) -> Optional[ArtifactObject]:
        """元数据与对象文件都在才视为有效 CAS 命中。"""
        obj = self._store.get(ArtifactObject, sha256.lower())
        if obj is None:
            return None
        if not self.object_path(obj.sha256).exists():
            return None
        return obj

    # -- 写入 -----------------------------------------------------------------

    async def ensure(
        self,
        *,
        name: str,
        fetch: FetchFn,
        sha256_hint: str = "",
        origin: str = "",
        media_type: str = "",
    ) -> tuple[ArtifactObject, bool]:
        """确保附件内容在 CAS 中。返回 (对象, 是否本轮新下载)。

        去重两级判定：

        1. ``sha256_hint`` 非空且 CAS 已有该对象 → 直接命中，不下载；
        2. 下载完成后按真实 digest 再查一次——不同来源的相同内容仍然只存
           一份（元数据保留首次来源）。
        """
        hint = sha256_hint.lower()
        if hint:
            hit = self.get(hint)
            if hit is not None:
                return hit, False
        content, fetched_media = await self._download_with_retry(name, fetch)
        digest = hashlib.sha256(content).hexdigest()
        if hint and digest != hint:
            raise ArtifactDownloadError(
                f"artifact {name!r}: sha256 mismatch after download",
                category=PlatformErrorCategory.INVALID_RESPONSE,
                detail={"expected": hint, "actual": digest},
            )
        hit = self.get(digest)
        if hit is not None:
            return hit, False
        obj = self._put(
            digest, content,
            media_type=media_type or fetched_media or guess_media_type(name),
            origin=origin,
        )
        return obj, True

    def _put(
        self,
        sha256: str,
        content: bytes,
        *,
        media_type: str,
        origin: str,
    ) -> ArtifactObject:
        """原子写入对象文件并落 ``ArtifactObject`` 元数据。"""
        path = self.object_path(sha256)
        path.parent.mkdir(parents=True, exist_ok=True)
        tmp = path.with_name(f".{path.name}.{os.getpid()}.tmp")
        tmp.write_bytes(content)
        if path.exists():
            # 并发/重入下的同级写入：内容寻址保证等价，丢弃本份临时文件。
            tmp.unlink(missing_ok=True)
        else:
            tmp.replace(path)  # POSIX 原子替换
        obj = ArtifactObject(
            sha256=sha256,
            size=len(content),
            media_type=media_type,
            origin=origin,
            local_path=str(path),
        )
        self._store.save(obj)
        return obj

    async def _download_with_retry(self, name: str, fetch: FetchFn) -> FetchResult:
        """有界重试：仅 ``retryable`` 的平台错误重试，其余立即失败。"""
        import asyncio

        last: Optional[PlatformTransportError] = None
        for attempt in range(1, self.max_download_attempts + 1):
            try:
                content, media_type = await fetch()
                if not isinstance(content, (bytes, bytearray)):
                    raise ArtifactDownloadError(
                        f"artifact {name!r}: fetch returned non-bytes content",
                    )
                return bytes(content), str(media_type or "")
            except ArtifactDownloadError:
                raise
            except PlatformTransportError as exc:
                last = exc
                if not exc.retryable or attempt >= self.max_download_attempts:
                    break
                if self.retry_delay_seconds > 0:
                    await asyncio.sleep(self.retry_delay_seconds)
            except Exception as exc:  # 非平台错误归为 platform 类，不猜测细节
                raise ArtifactDownloadError(
                    f"artifact {name!r}: download failed: "
                    f"{type(exc).__name__}: {exc}",
                    category=PlatformErrorCategory.PLATFORM,
                ) from exc
        assert last is not None
        raise ArtifactDownloadError(
            f"artifact {name!r}: download failed after "
            f"{self.max_download_attempts} attempt(s): {last}",
            category=last.category,
            status_code=last.status_code,
            retry_after_seconds=last.retry_after_seconds,
        ) from last

    # -- Run 物化（设计 13.1：派发前进每 Run 输入 CAS） -------------------------

    def materialize_into_run(
        self,
        workspace_root: str | Path,
        artifact: ArtifactObject,
        *,
        name: str | None = None,
    ) -> dict[str, Any]:
        """把 CAS 对象物化到 Run workspace，返回 ``materialize_input`` 的结果。"""
        src = self.object_path(artifact.sha256)
        if not src.exists():
            raise FileNotFoundError(
                f"artifact object missing from CAS: {artifact.sha256}"
            )
        return materialize_input(workspace_root, src, name=name or artifact.sha256)


__all__ = [
    "ArtifactDownloadError",
    "CompetitionArtifactStore",
    "FetchFn",
    "FetchResult",
    "guess_media_type",
]
