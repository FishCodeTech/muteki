"""ctf.shared_graph.v1：现有 CTF SharedGraph 的 GraphService 薄 Adapter（GRAPH-01）。

对应任务书 8.3 与设计文档 11.3：

- 不复制数据、不改 schema：append / snapshot / claim / lease 直接映射到
  ``SQLiteSharedGraph`` 的现有 API，底层仍是同一个 per-challenge SQLite
  文件；历史数据库无迁移即可继续读写，blackboard 与 Swarm Worker 的
  行为完全不变。
- append 对已知 CTF 事件类型走对应的公开方法；未知类型走底层 append-only
  ``_append`` 落入同一事件日志（保持可回放、可审计）。
- claim / lease 分别映射到现有原子 ``claim_intent`` 与
  ``request_resource_lock``；CTF SharedGraph 没有独立 fencing token 字段，
  Adapter 用底层事件日志的全局 seq 作为 fencing token（单调、可核查）。
"""

from __future__ import annotations

from datetime import datetime, timedelta, timezone
from pathlib import Path
from typing import Any, Optional

from muteki.models.solve_graph import Challenge
from muteki.platform.contracts.graphs import (
    ClaimReceipt,
    ClaimRequest,
    GraphEvent,
    GraphReceipt,
    GraphScope,
    GraphSnapshot,
    LeaseReceipt,
    LeaseRequest,
)
from muteki.swarm.graph_defs import (
    EV_DEAD_END,
    EV_FACT_ADDED,
    EV_FLAG_FOUND,
    EV_INTENT_PROPOSED,
)
from muteki.swarm.shared_graph import SQLiteSharedGraph

from muteki.graphs.base import GraphError


class CtfSharedGraphService:
    """以 GraphService 接口包装一个现有 SQLiteSharedGraph 实例。"""

    id = "ctf.shared_graph.v1"

    def __init__(self, graph: SQLiteSharedGraph) -> None:
        self._graph = graph

    # -- 构造 ----------------------------------------------------------------

    @classmethod
    def open(
        cls,
        *,
        db_path: str | Path,
        challenge: Challenge,
        artifacts: Any = None,
    ) -> "CtfSharedGraphService":
        """打开（或创建）本 Challenge 的 SharedGraph 并以 GraphService 返回。"""
        return cls(SQLiteSharedGraph.open(
            db_path=db_path, challenge=challenge, artifacts=artifacts))

    @property
    def graph(self) -> SQLiteSharedGraph:
        """底层 SharedGraph（需要完整 CTF 词汇的调用方使用）。"""
        return self._graph

    def close(self) -> None:
        self._graph.close()

    def __enter__(self) -> "CtfSharedGraphService":
        return self

    def __exit__(self, *exc: Any) -> None:
        self.close()

    def _check_graph_id(self, graph_id: str) -> None:
        if graph_id and graph_id != self.id:
            raise GraphError(
                f"graph_id mismatch: request targets {graph_id!r}, "
                f"this is {self.id!r}"
            )

    def _watermark(self) -> int:
        """事件日志水位（最大 seq）。只读查询，不改既有行为。"""
        row = self._graph._conn.execute(
            "SELECT MAX(seq) FROM events"
        ).fetchone()
        return int(row[0]) if row and row[0] is not None else 0

    # -- GraphService.append -------------------------------------------------

    def _append_known(self, event: GraphEvent) -> Optional[int]:
        """已知 CTF 事件类型 → 现有公开方法；未知类型返回 None 走底层日志。"""
        p = event.payload
        actor = event.actor_id or "system"
        kind = event.event_type
        if kind == EV_FACT_ADDED:
            return self._graph.add_evidence(
                actor=actor,
                source=str(p.get("source") or "graph_service"),
                fact=str(p.get("fact") or p.get("text") or ""),
                artifact_id=p.get("artifact_id"),
                verified=bool(p.get("verified", False)),
                confidence=float(p.get("confidence", 1.0)),
                witness=p.get("witness"),
                verifier=str(p.get("verifier") or ""),
                route_hash=str(p.get("route_hash") or ""),
                intent_id=p.get("intent_id"),
                provenance=(
                    dict(p.get("evidence_provenance"))
                    if isinstance(p.get("evidence_provenance"), dict) else None
                ),
            )
        if kind == EV_DEAD_END:
            return self._graph.add_dead_end(
                actor=actor,
                reason=str(p.get("reason") or ""),
                intent_id=str(p.get("intent_id") or ""),
                route_hash=str(p.get("route_hash") or ""),
                coverage_key=str(p.get("coverage_key") or ""),
                target_epoch=str(p.get("target_epoch") or ""),
                tested_scope=str(p.get("tested_scope") or ""),
                observed_result=str(p.get("observed_result") or ""),
            )
        if kind == EV_FLAG_FOUND:
            return self._graph.flag_found(
                actor=actor,
                flag=str(p.get("flag") or ""),
                artifact_id=p.get("artifact_id"),
                intent_id=p.get("intent_id"),
            )
        if kind == EV_INTENT_PROPOSED:
            return self._graph.propose_intent(
                actor=actor,
                intent_id=str(p.get("intent_id") or ""),
                goal=str(p.get("goal") or ""),
                payload=p.get("payload") if isinstance(p.get("payload"), dict) else None,
                from_fact_seqs=p.get("from_fact_seqs"),
            )
        return None

    async def append(self, event: GraphEvent) -> GraphReceipt:
        """追加一条 CTF 领域事件；返回事件日志 seq 作为 stream_seq。

        底层去重（dedupe_key 冲突或 fact 级去重）时 seq=-1，回执标记
        ``deduplicated=True``。
        """
        self._check_graph_id(event.graph_id)
        if not event.event_type:
            raise GraphError("event_type is required")
        seq = self._append_known(event)
        if seq is None:
            # 未知类型：落入同一 append-only 事件日志；idempotency_key 映射为
            # 底层 dedupe_key，重放时返回 -1 并在回执上标记 deduplicated。
            seq = self._graph._append(
                event.event_type,
                event.actor_id or "system",
                dict(event.payload),
                dedupe_key=event.idempotency_key,
            )
        return GraphReceipt(
            graph_id=self.id,
            stream_seq=max(0, int(seq)),
            deduplicated=(int(seq) == -1),
        )

    # -- GraphService.snapshot -------------------------------------------------

    async def snapshot(self, scope: GraphScope) -> GraphSnapshot:
        """CTF 图状态快照。

        默认返回 SolveGraph 投影（evidence / dead_ends / flags / findings /
        intents）；scope 里给 ``after_seq`` / ``kinds`` 时附带增量事件页
        ``state["events"]``。
        """
        self._check_graph_id(scope.graph_id)
        solve = self._graph.snapshot()
        state: dict[str, Any] = {
            "challenge_id": self._graph.challenge.id,
            "evidence": [e.model_dump() for e in solve.evidence],
            "dead_ends": list(solve.dead_ends),
            "flag": solve.flag,
            "flags": list(solve.flags),
            "rejected_flags": list(solve.rejected_flags),
            "findings": list(solve.findings),
            "intents": self._graph.coverage_intent_rows(),
        }
        after_seq = scope.scope.get("after_seq")
        kinds = scope.scope.get("kinds")
        if after_seq is not None or kinds is not None:
            state["events"] = self._graph.events_since(
                int(after_seq or 0),
                [str(k) for k in kinds] if kinds else None,
            )
        return GraphSnapshot(
            graph_id=self.id, watermark=self._watermark(), state=state
        )

    # -- GraphService.claim ----------------------------------------------------

    async def claim(self, request: ClaimRequest) -> ClaimReceipt:
        """原子领取开放 Intent：映射到底层单 UPDATE 的 ``claim_intent``。"""
        self._check_graph_id(request.graph_id)
        if not request.intent_id:
            raise GraphError("intent_id is required")
        won = self._graph.claim_intent(
            worker=request.claimant or "worker",
            intent_id=request.intent_id,
        )
        if not won:
            return ClaimReceipt(
                granted=False,
                graph_id=self.id,
                intent_id=request.intent_id,
            )
        # fencing token = claim 事件的全局 seq（底层事件日志单调递增）
        return ClaimReceipt(
            granted=True,
            graph_id=self.id,
            intent_id=request.intent_id,
            claimant=request.claimant,
            fencing_token=self._watermark(),
            expires_at=None,
        )

    # -- GraphService.lease ----------------------------------------------------

    async def lease(self, request: LeaseRequest) -> LeaseReceipt:
        """图内独占资源租约：映射到现有 ``request_resource_lock``（自愈过期）。"""
        self._check_graph_id(request.graph_id)
        if not request.resource_kind or not request.resource_key:
            raise GraphError("resource_kind and resource_key are required")
        ttl = float(request.ttl_seconds or 600.0)
        owner = request.owner or "graph-service"
        res = self._graph.request_resource_lock(
            actor=owner,
            resource_key=f"{request.resource_kind}:{request.resource_key}",
            scope="activity",
            owner_worker=owner,
            lease_s=ttl,
        )
        granted = bool(res.get("acquired"))
        expires_at: Optional[datetime] = None
        if granted:
            expires_at = datetime.now(timezone.utc) + timedelta(seconds=ttl)
        elif res.get("lease_until"):
            expires_at = datetime.fromtimestamp(
                float(res["lease_until"]), tz=timezone.utc)
        return LeaseReceipt(
            granted=granted,
            graph_id=self.id,
            resource_kind=request.resource_kind,
            resource_key=request.resource_key,
            owner=owner if granted else str(res.get("held_by") or ""),
            fencing_token=int(res.get("seq") or 0),
            expires_at=expires_at,
        )
