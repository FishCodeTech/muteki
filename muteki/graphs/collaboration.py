"""collaboration.graph.v1：通用协作图（GRAPH-01，任务书 8.3）。

通用协作语义：fact（含 challenge / revalidate）、intent（原子 claim）、
branch、review、directive。独立 SQLite 库，append-only 事件 + 投影；
原子 claim 与 fencing 租约由 ``muteki.graphs.base`` 提供。

边界：flag / finding / PoC / report 属于安全领域投影，不进入本图
（CTF 场景继续使用 ``ctf.shared_graph.v1``）。
"""

from __future__ import annotations

import json
from typing import Any

from muteki.platform.contracts.graphs import GraphEvent, GraphScope, GraphSnapshot

from muteki.graphs.base import GraphError, SQLiteGraphBase

# ---------------------------------------------------------------------------
# 领域事件词汇（"collab." 前缀，与其他图的事件不混用）
# ---------------------------------------------------------------------------

EV_FACT_ADDED = "collab.fact_added"
EV_FACT_CHALLENGED = "collab.fact_challenged"
EV_FACT_REVALIDATED = "collab.fact_revalidated"
EV_INTENT_PROPOSED = "collab.intent_proposed"
EV_INTENT_CONCLUDED = "collab.intent_concluded"
EV_INTENT_CLAIMED = "collab.intent_claimed"
EV_BRANCH_SPLIT = "collab.branch_split"
EV_BRANCH_RESOLVED = "collab.branch_resolved"
EV_REVIEW_RECORDED = "collab.review_recorded"
EV_REVIEW_DECISION = "collab.review_decision"
EV_DIRECTIVE_ISSUED = "collab.directive_issued"

#: fact 状态集合
FACT_STATES = ("unresolved", "challenged", "revalidated")


class CollaborationGraph(SQLiteGraphBase):
    """通用协作图的 SQLite 实现。"""

    GRAPH_ID = "collaboration.graph.v1"
    CLAIM_EVENT_TYPE = EV_INTENT_CLAIMED

    EXTRA_SCHEMA = """
    CREATE TABLE IF NOT EXISTS collab_facts (
        fact_id TEXT PRIMARY KEY,
        actor TEXT NOT NULL DEFAULT '',
        text TEXT NOT NULL DEFAULT '',
        status TEXT NOT NULL DEFAULT 'unresolved',
        branch_id TEXT,
        payload TEXT NOT NULL DEFAULT '{}',
        created_seq INTEGER NOT NULL,
        updated_seq INTEGER NOT NULL
    );
    CREATE TABLE IF NOT EXISTS collab_branches (
        branch_id TEXT PRIMARY KEY,
        title TEXT NOT NULL DEFAULT '',
        status TEXT NOT NULL DEFAULT 'open',
        reason TEXT,
        created_seq INTEGER NOT NULL,
        updated_seq INTEGER NOT NULL
    );
    CREATE TABLE IF NOT EXISTS collab_reviews (
        review_id TEXT PRIMARY KEY,
        kind TEXT NOT NULL DEFAULT '',
        summary TEXT NOT NULL DEFAULT '',
        decision TEXT,
        payload TEXT NOT NULL DEFAULT '{}',
        created_seq INTEGER NOT NULL,
        updated_seq INTEGER NOT NULL
    );
    CREATE TABLE IF NOT EXISTS collab_directives (
        directive_id TEXT PRIMARY KEY,
        actor TEXT NOT NULL DEFAULT '',
        action TEXT NOT NULL DEFAULT '',
        text TEXT NOT NULL DEFAULT '',
        priority TEXT NOT NULL DEFAULT 'normal',
        payload TEXT NOT NULL DEFAULT '{}',
        created_seq INTEGER NOT NULL
    );
    """

    # -- 领域校验与投影 ---------------------------------------------------------

    def _validate_event(self, event: GraphEvent) -> None:
        p = event.payload
        if event.event_type == EV_INTENT_PROPOSED:
            if not p.get("intent_id") or not p.get("goal"):
                raise GraphError("intent_proposed requires intent_id and goal")
        elif event.event_type in (EV_INTENT_CONCLUDED,):
            if not p.get("intent_id"):
                raise GraphError(f"{event.event_type} requires intent_id")
        elif event.event_type == EV_FACT_ADDED:
            if not p.get("text"):
                raise GraphError("fact_added requires text")
        elif event.event_type in (EV_FACT_CHALLENGED, EV_FACT_REVALIDATED):
            if not p.get("fact_id"):
                raise GraphError(f"{event.event_type} requires fact_id")
        elif event.event_type == EV_BRANCH_SPLIT:
            if not p.get("branch_id") and not p.get("branches"):
                raise GraphError("branch_split requires branch_id or branches")
        elif event.event_type == EV_BRANCH_RESOLVED:
            if not p.get("branch_id"):
                raise GraphError("branch_resolved requires branch_id")
        elif event.event_type == EV_REVIEW_DECISION:
            if not p.get("review_id"):
                raise GraphError("review_decision requires review_id")

    def _apply_event(self, event: GraphEvent, seq: int) -> None:
        p = event.payload
        kind = event.event_type
        if kind == EV_INTENT_PROPOSED:
            self._conn.execute(
                "INSERT INTO graph_intents (intent_id, goal, status, payload, "
                "created_seq, updated_seq) VALUES (?,?,'open',?,?,?) "
                "ON CONFLICT(intent_id) DO NOTHING",
                (
                    str(p["intent_id"]),
                    str(p["goal"]),
                    json.dumps(p, ensure_ascii=False, default=str),
                    seq,
                    seq,
                ),
            )
        elif kind == EV_INTENT_CONCLUDED:
            self._conn.execute(
                "UPDATE graph_intents SET status='done', result=?, "
                "updated_seq=? WHERE intent_id=?",
                (str(p.get("result") or ""), seq, str(p["intent_id"])),
            )
        elif kind == EV_FACT_ADDED:
            fact_id = str(p.get("fact_id") or f"cfact-{seq}")
            self._conn.execute(
                "INSERT INTO collab_facts (fact_id, actor, text, status, "
                "branch_id, payload, created_seq, updated_seq) "
                "VALUES (?,?,?,'unresolved',?,?,?,?) "
                "ON CONFLICT(fact_id) DO UPDATE SET text=excluded.text, "
                "payload=excluded.payload, updated_seq=excluded.updated_seq",
                (
                    fact_id,
                    event.actor_id,
                    str(p["text"]),
                    p.get("branch_id"),
                    json.dumps(p, ensure_ascii=False, default=str),
                    seq,
                    seq,
                ),
            )
        elif kind in (EV_FACT_CHALLENGED, EV_FACT_REVALIDATED):
            status = (
                "challenged" if kind == EV_FACT_CHALLENGED else "revalidated"
            )
            self._conn.execute(
                "UPDATE collab_facts SET status=?, updated_seq=? WHERE fact_id=?",
                (status, seq, str(p["fact_id"])),
            )
        elif kind == EV_BRANCH_SPLIT:
            branches = p.get("branches")
            if isinstance(branches, list) and branches:
                items = [
                    (str(b.get("branch_id") or f"cbranch-{seq}-{i}"),
                     str(b.get("title") or ""))
                    for i, b in enumerate(branches)
                ]
            else:
                items = [(str(p["branch_id"]), str(p.get("title") or ""))]
            for branch_id, title in items:
                self._conn.execute(
                    "INSERT INTO collab_branches (branch_id, title, status, "
                    "created_seq, updated_seq) VALUES (?,?,'open',?,?) "
                    "ON CONFLICT(branch_id) DO NOTHING",
                    (branch_id, title, seq, seq),
                )
        elif kind == EV_BRANCH_RESOLVED:
            self._conn.execute(
                "UPDATE collab_branches SET status=?, reason=?, updated_seq=? "
                "WHERE branch_id=?",
                (
                    str(p.get("status") or "resolved"),
                    str(p.get("reason") or ""),
                    seq,
                    str(p["branch_id"]),
                ),
            )
        elif kind == EV_REVIEW_RECORDED:
            review_id = str(p.get("review_id") or f"crev-{seq}")
            self._conn.execute(
                "INSERT INTO collab_reviews (review_id, kind, summary, payload, "
                "created_seq, updated_seq) VALUES (?,?,?,?,?,?) "
                "ON CONFLICT(review_id) DO UPDATE SET "
                "summary=excluded.summary, payload=excluded.payload, "
                "updated_seq=excluded.updated_seq",
                (
                    review_id,
                    str(p.get("kind") or ""),
                    str(p.get("summary") or ""),
                    json.dumps(p, ensure_ascii=False, default=str),
                    seq,
                    seq,
                ),
            )
        elif kind == EV_REVIEW_DECISION:
            self._conn.execute(
                "UPDATE collab_reviews SET decision=?, updated_seq=? "
                "WHERE review_id=?",
                (str(p.get("decision") or ""), seq, str(p["review_id"])),
            )
        elif kind == EV_DIRECTIVE_ISSUED:
            directive_id = str(p.get("directive_id") or f"cdir-{seq}")
            self._conn.execute(
                "INSERT INTO collab_directives (directive_id, actor, action, "
                "text, priority, payload, created_seq) VALUES (?,?,?,?,?,?,?) "
                "ON CONFLICT(directive_id) DO NOTHING",
                (
                    directive_id,
                    event.actor_id,
                    str(p.get("action") or ""),
                    str(p.get("text") or ""),
                    str(p.get("priority") or "normal"),
                    json.dumps(p, ensure_ascii=False, default=str),
                    seq,
                ),
            )
        # 其他事件类型：只进事件日志，无投影（基类机制允许领域外扩展）

    # -- GraphService.snapshot ---------------------------------------------------

    async def snapshot(self, scope: GraphScope) -> GraphSnapshot:
        """投影读取模型；scope 支持 branch_id / fact_status / intent_status 过滤。"""
        self._check_graph_id(scope.graph_id)
        branch_id = scope.scope.get("branch_id")
        fact_status = scope.scope.get("fact_status")
        intent_status = scope.scope.get("intent_status")
        with self._lock:
            facts = self._load_facts(branch_id=branch_id, status=fact_status)
            intents = self._load_intents(
                status=str(intent_status) if intent_status else None
            )
            branches = self._load_branches()
            reviews = self._load_reviews()
            directives = self._load_directives()
            leases = self._load_leases()
            watermark = self.watermark()
        return GraphSnapshot(
            graph_id=self.GRAPH_ID,
            watermark=watermark,
            state={
                "facts": facts,
                "intents": intents,
                "branches": branches,
                "reviews": reviews,
                "directives": directives,
                "leases": leases,
            },
        )

    # -- 投影读取辅助 ------------------------------------------------------------

    def _load_facts(
        self,
        *,
        branch_id: Any = None,
        status: Any = None,
    ) -> list[dict[str, Any]]:
        sql = (
            "SELECT fact_id, actor, text, status, branch_id, created_seq, "
            "updated_seq FROM collab_facts"
        )
        clauses: list[str] = []
        params: list[Any] = []
        if branch_id:
            clauses.append("branch_id=?")
            params.append(str(branch_id))
        if status:
            clauses.append("status=?")
            params.append(str(status))
        if clauses:
            sql += " WHERE " + " AND ".join(clauses)
        sql += " ORDER BY created_seq"
        rows = self._conn.execute(sql, tuple(params)).fetchall()
        return [
            {
                "fact_id": r[0],
                "actor": r[1],
                "text": r[2],
                "status": r[3],
                "branch_id": r[4] or "",
                "created_seq": int(r[5]),
                "updated_seq": int(r[6]),
            }
            for r in rows
        ]

    def _load_branches(self) -> list[dict[str, Any]]:
        rows = self._conn.execute(
            "SELECT branch_id, title, status, reason, created_seq, updated_seq "
            "FROM collab_branches ORDER BY created_seq"
        ).fetchall()
        return [
            {
                "branch_id": r[0],
                "title": r[1],
                "status": r[2],
                "reason": r[3] or "",
                "created_seq": int(r[4]),
                "updated_seq": int(r[5]),
            }
            for r in rows
        ]

    def _load_reviews(self) -> list[dict[str, Any]]:
        rows = self._conn.execute(
            "SELECT review_id, kind, summary, decision, created_seq, "
            "updated_seq FROM collab_reviews ORDER BY created_seq"
        ).fetchall()
        return [
            {
                "review_id": r[0],
                "kind": r[1],
                "summary": r[2],
                "decision": r[3] or "",
                "created_seq": int(r[4]),
                "updated_seq": int(r[5]),
            }
            for r in rows
        ]

    def _load_directives(self) -> list[dict[str, Any]]:
        rows = self._conn.execute(
            "SELECT directive_id, actor, action, text, priority, created_seq "
            "FROM collab_directives ORDER BY created_seq"
        ).fetchall()
        return [
            {
                "directive_id": r[0],
                "actor": r[1],
                "action": r[2],
                "text": r[3],
                "priority": r[4],
                "created_seq": int(r[5]),
            }
            for r in rows
        ]
