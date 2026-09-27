"""ChallengeCompiler：revision + 活动实例连接信息 → 现有 ``Challenge``
（任务书 10.4 / 设计 7.3，COMP-05）。

映射规则（设计 7.3 逐字对齐）::

    name/category/points/description
    attachments → 已物化的本地路径
    target      → 当前实例或静态目标
    flag_format/expected_flags/multi_flag

约束：

- 只写选手可见字段；平台身份（external id、revision_id 之外的平台字段）
  由 ``RunBinding.revision_id`` 保存，绝不写入核心 ``Challenge``。
- 动态实例地址来自活动 ``InstanceLease`` 的投影（lease.address），
  不改写历史 ChallengeRevision（任务书 10.4 末条）。
- 附件可选地经 ``materialize_input`` 复制进 Run workspace（设计 13.1），
  ``attachments`` 落 Run 内 by-name 路径；不给 workspace 时直接引用
  比赛级 CAS 对象路径。
"""

from __future__ import annotations

from pathlib import Path
from typing import Optional

from muteki.competition.artifacts import CompetitionArtifactStore
from muteki.competition.models import (
    ArtifactObject,
    ChallengeArtifact,
    ChallengeRevision,
    InstanceLease,
    LeaseState,
)
from muteki.competition.store import CompetitionStore, NotFoundError
from muteki.models.solve_graph import Challenge

#: 核心 ``Challenge.category`` 的合法值（solve_graph.Category 的 Literal）。
_KNOWN_CATEGORIES = {"web", "pwn", "reverse", "crypto", "forensics", "misc"}

#: 平台侧常见类别别名 → 核心类别。未命中的一律归 ``misc``，平台原值仍可由
#: revision 行查询，不进核心 Challenge。
_CATEGORY_ALIASES = {
    "rev": "reverse",
    "re": "reverse",
    "reversing": "reverse",
    "pwnable": "pwn",
    "binary": "pwn",
    "exploit": "pwn",
    "exploitation": "pwn",
    "cryptography": "crypto",
    "stego": "forensics",
    "steganography": "forensics",
    "forensic": "forensics",
    "network": "forensics",
    "osint": "misc",
    "ppc": "misc",
    "programming": "misc",
    "web3": "web",
}


def map_category(raw: str) -> str:
    """平台类别 → 核心 ``Challenge.category``；未知归 ``misc``。"""
    key = raw.strip().lower()
    if key in _KNOWN_CATEGORIES:
        return key
    return _CATEGORY_ALIASES.get(key, "misc")


class ChallengeCompiler:
    """把不可变 revision 编译为一次派发用的核心 ``Challenge``。"""

    def __init__(
        self,
        store: CompetitionStore,
        artifacts: Optional[CompetitionArtifactStore] = None,
    ) -> None:
        self._store = store
        self._artifacts = artifacts or CompetitionArtifactStore(store)

    def compile(
        self,
        revision: ChallengeRevision,
        *,
        lease: Optional[InstanceLease] = None,
        workspace_root: str | Path | None = None,
    ) -> Challenge:
        """revision → 核心 ``Challenge``。

        - ``lease``：活动实例租约；``active``/``renewing`` 状态下其地址
          覆盖 revision 的静态 target（lease 投影进入执行上下文）。
        - ``workspace_root`` 非空时把 revision 附件物化进该 Run 的输入
          CAS，``attachments`` 为 Run 内 by-name 路径；否则引用比赛级
          CAS 的对象路径。
        """
        target = revision.target.strip() or None
        if lease is not None and lease.state in (
            LeaseState.ACTIVE.value, LeaseState.RENEWING.value,
        ) and lease.address.strip():
            target = lease.address.strip()
        description = revision.description
        if revision.hints:
            # 提示是选手可见信息；核心 Challenge 没有独立 hints 字段，
            # 追加在描述尾部。
            hints = "\n".join(f"- {h}" for h in revision.hints)
            description = f"{description}\n\nHints:\n{hints}".strip()
        attachments = [
            str(self._attachment_path(revision, link, workspace_root))
            for link in self._store.list(
                ChallengeArtifact, revision_id=revision.revision_id
            )
        ]
        fields: dict = {
            "id": revision.revision_id,  # 派发快照身份；平台身份见 RunBinding
            "name": revision.name,
            "category": map_category(revision.category),
            "points": int(round(revision.points)),
            "description": description,
            "attachments": attachments,
            "target": target,
            "expected_flags": max(1, int(revision.expected_flags or 1)),
            "multi_flag": bool(revision.multi_flag),
            "platform_confirmation_required": True,
        }
        if revision.flag_format.strip():
            fields["flag_format"] = revision.flag_format.strip()
        return Challenge(**fields)

    def _attachment_path(
        self,
        revision: ChallengeRevision,
        link: ChallengeArtifact,
        workspace_root: str | Path | None,
    ) -> Path:
        obj = self._store.get(ArtifactObject, link.sha256)
        if obj is None:
            raise NotFoundError(
                f"artifact object not found: {link.sha256} "
                f"(revision {revision.revision_id})"
            )
        if workspace_root is None:
            return self._artifacts.object_path(obj.sha256)
        result = self._artifacts.materialize_into_run(
            workspace_root, obj, name=link.name or None
        )
        return result["by_name"]


__all__ = ["ChallengeCompiler", "map_category"]
