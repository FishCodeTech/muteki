"""Compaction, intents, PoCs, activity locks, and false-positive reopen.

Split out of ``shared_graph.py`` (code-health G1) as a mixin of
``SQLiteSharedGraph``. Every method body is byte-for-byte the original; the mixin
is combined back into ``SQLiteSharedGraph`` so behavior and the public surface are
unchanged. Instance state (``self._conn``, ``self._lock``, ``self.challenge``,
``self._append``, the class-level caps, the ``normalize_*`` helpers, …) is
resolved through the composed class at runtime.
"""

from __future__ import annotations

import json  # noqa: F401
import hashlib  # noqa: F401
import re  # noqa: F401
import time  # noqa: F401
from difflib import SequenceMatcher  # noqa: F401
from pathlib import Path  # noqa: F401
from typing import Any, Optional  # noqa: F401

from muteki.models.solve_graph import Challenge, Evidence, SolveGraph  # noqa: F401
from muteki.solver.result_codes import is_genuine_giveup  # noqa: F401
from muteki.swarm.graph_defs import (  # noqa: F401
    EV_FACT_ADDED, EV_HYP_PROPOSED, EV_HYP_REFUTED, EV_DEAD_END,
    EV_INTENT_PROPOSED, EV_INTENT_CLAIMED, EV_INTENT_CONCLUDED, EV_FLAG_FOUND,
    EV_FLAG_INVALIDATED, EV_POC_SAVED, EV_POC_CLAIMED, EV_POC_CONCLUDED,
    EV_REVIEW_FINDING, EV_FACT_CHALLENGED, EV_FACT_REVALIDATED,
    EV_ROUTE_SUPPRESSED, EV_ROUTE_REOPENED, EV_BRANCH_SPLIT, EV_BRANCH_RESOLVED,
    EV_COORDINATOR_DIRECTIVE, EV_REVIEW_PROPOSAL, EV_REVIEW_PROPOSAL_DECISION,
    EV_LANE_LOCKED, EV_LANE_RELEASED, EV_INTENT_LANE_DEFERRED, EV_FACT_REJECTED,
    EV_FACT_MERGED, EV_FACT_SUPERSEDED, EV_FACT_PINNED, EV_INTENT_STATE_CHANGED,
    EV_OPERATOR_DIRECTIVE, EV_OPERATOR_DIRECTIVE_STATUS, EV_HITL_CLASSIFIED,
    EV_RESOURCE_LOCKED, EV_RESOURCE_RELEASED, EV_GRAPH_COMPACTED,
    FACT_STATE_UNRESOLVED, FACT_STATE_CHALLENGED, FACT_STATE_REVALIDATED,
    FACT_STATE_REJECTED, FACT_STATE_MERGED, FACT_STATE_SUPERSEDED,
    _FACT_TERMINAL_STATES, _FACT_STATES,
    INTENT_DISPATCH_ACTIVE, INTENT_DISPATCH_RESUME, INTENT_DISPATCH_RETIRED,
    INTENT_DISPATCH_CLOSED, _INTENT_DISPATCH_STATES,
    _SERVICE_DEFAULT_PORTS, _LANE_RISK_CLASSES, _FACT_ENGINE_PREFIX_RE,
    _normalize_fact_identity, _clean_lane_risk, _clean_lane_host, canonicalize_lane,
)


class _IntentsPocsMixin:
    def query_legacy_candidates(self, *, now: float) -> list[dict]:
        """Compatibility read for the narrow Protocol 1 SearchStatePort."""
        with self._lock:
            rows = self._conn.execute(
                "SELECT i.intent_id, i.goal, i.worker_class, i.route_hash, "
                "i.branch_id, i.priority, i.lane_key, i.risk_class, "
                "i.resource_key FROM intents i "
                "WHERE i.dispatch_state='active' AND (i.status='open' "
                "   OR (i.status='claimed' AND i.lease_until IS NOT NULL "
                "       AND i.lease_until < ?)) "
                "AND NOT EXISTS ("
                "  SELECT 1 FROM intent_dependencies d "
                "  LEFT JOIN intents p ON p.challenge_id=i.challenge_id "
                "   AND p.intent_id=d.depends_on_intent_id "
                "  WHERE d.challenge_id=i.challenge_id "
                "   AND d.intent_id=i.intent_id "
                "   AND (p.intent_id IS NULL OR p.status!='done' "
                "        OR COALESCE(p.dispatch_state,'active')!='closed')"
                ") "
                "ORDER BY CASE WHEN worker_class IN ('verifier','review') "
                "         THEN 0 ELSE 1 END, priority DESC, created_seq",
                (float(now),),
            ).fetchall()
            ids = [str(row[0]) for row in rows]
            source_rows = []
            dep_rows = []
            if ids:
                q = ",".join("?" for _ in ids)
                source_rows = self._conn.execute(
                    f"SELECT intent_id, fact_seq FROM intent_sources "
                    f"WHERE intent_id IN ({q}) ORDER BY fact_seq",
                    tuple(ids),
                ).fetchall()
                dep_rows = self._conn.execute(
                    f"SELECT intent_id, depends_on_intent_id "
                    f"FROM intent_dependencies WHERE challenge_id=? "
                    f"AND intent_id IN ({q}) ORDER BY depends_on_intent_id",
                    (self.challenge.id, *ids),
                ).fetchall()
        sources: dict[str, list[int]] = {}
        for intent_id, fact_seq in source_rows:
            sources.setdefault(str(intent_id), []).append(int(fact_seq))
        deps: dict[str, list[str]] = {}
        for intent_id, dep_id in dep_rows:
            deps.setdefault(str(intent_id), []).append(str(dep_id))
        return [
            {
                "intent_id": row[0], "goal": row[1],
                "worker_class": row[2] or "code", "route_hash": row[3] or "",
                "branch_id": row[4] or "", "priority": int(row[5] or 0),
                "lane_key": row[6] or "", "risk_class": row[7] or "",
                "resource_key": row[8] or "",
                # Enables cluster planner long-chain continuity scoring without
                # a new schema — intent_sources already exists.
                "from_facts": sources.get(str(row[0]), []),
                "depends_on": deps.get(str(row[0]), []),
            }
            for row in rows
        ]

    def apply_legacy_lane_inferences(
        self, *, inferences: list[tuple[str, str, str]],
    ) -> None:
        if not inferences:
            return
        with self._lock:
            self._conn.execute("BEGIN IMMEDIATE")
            try:
                for lane_key, risk_class, intent_id in inferences:
                    self._conn.execute(
                        "UPDATE intents SET lane_key=?, risk_class=? "
                        "WHERE challenge_id=? AND intent_id=? "
                        "AND (lane_key IS NULL OR lane_key='')",
                        (lane_key, risk_class or lane_key.split(":", 1)[0],
                         self.challenge.id, intent_id),
                    )
                self._conn.commit()
            except Exception:
                self._conn.rollback()
                raise

    # ── H: long-run compaction ──────────────────────────────────────────
    def compact_graph(self, *, actor: str = "coordinator",
                      trigger: str = "no_progress_time", summary: str = "") -> dict:
        """H: compact a long-running graph. RETIRES stale concluded/closed intents
        (dispatch_state → retired) and records an audit epoch. It does NOT touch
        verified/active candidate FACTS — compaction must never collapse an
        unverified candidate into a fact or drop evidence (design §12). Returns
        {compact_id, retired_intent_ids, cutoff_seq, summary}."""
        now_seq = 0
        with self._lock:
            row = self._conn.execute(
                "SELECT MAX(seq) FROM events WHERE challenge_id=?",
                (self.challenge.id,),
            ).fetchone()
            now_seq = int((row[0] if row and row[0] is not None else 0))
            # stale = fact-less intents (to_fact_seq IS NULL) that are ALREADY
            # non-dispatchable, so retiring them can never steal queued work:
            #   • status='done' AND dispatch_state='closed'  — concluded barren attempts
            #   • dispatch_state='resume'                    — stranded by a prior
            #     finalize; no production revival re-activates them mid-run, so without
            #     this they accumulate forever (the run-75375 "34 open/resume" leak).
            # HARD GUARD (Codex trap #1): dispatch_state='active' is the live dispatch
            # queue (_open_intents / claim_intent only take 'active'); it is NEVER
            # compacted here. 'claimed' rows are also excluded — a claimed intent is
            # owned by a live worker; lease-expiry reclaim is _open_intents' job, not
            # the compactor's, so we never retire a row a worker might still be on.
            rows = self._conn.execute(
                "SELECT intent_id FROM intents WHERE challenge_id=? "
                "AND to_fact_seq IS NULL AND ("
                "  (status='done' AND dispatch_state='closed') "
                "  OR dispatch_state='resume'"
                ")",
                (self.challenge.id,),
            ).fetchall()
            retired = [str(r[0]) for r in rows]
        compact_id = f"C-{hashlib.sha1(f'{trigger}:{now_seq}'.encode()).hexdigest()[:10]}"
        clean_summary = (summary or f"compacted at seq {now_seq} ({trigger})").strip()[:4000]
        seq = self._append(
            EV_GRAPH_COMPACTED, actor,
            {"compact_id": compact_id, "trigger": trigger, "cutoff_seq": now_seq,
             "summary": clean_summary, "retired_intent_ids": retired})
        with self._lock:
            self._conn.execute(
                "INSERT OR IGNORE INTO compact_epochs "
                "(compact_id, challenge_id, trigger, cutoff_seq, summary, "
                " retained_fact_seqs, retired_intent_ids, stale_route_hashes, created_seq) "
                "VALUES (?,?,?,?,?,?,?,?,?)",
                (compact_id, self.challenge.id, trigger, now_seq, clean_summary,
                 None, json.dumps(retired), None, seq if seq > 0 else 0),
            )
            if retired:
                q = ",".join("?" for _ in retired)
                self._conn.execute(
                    f"UPDATE intents SET dispatch_state='retired', compact_id=? "
                    f"WHERE challenge_id=? AND intent_id IN ({q})",
                    (compact_id, self.challenge.id, *retired),
                )
            self._conn.commit()
        if retired:
            self._append(EV_INTENT_STATE_CHANGED, actor,
                         {"intent_id": ",".join(retired),
                          "dispatch_state": INTENT_DISPATCH_RETIRED,
                          "compact_id": compact_id})
        return {"compact_id": compact_id, "trigger": trigger, "cutoff_seq": now_seq,
                "summary": clean_summary, "retired_intent_ids": retired}

    def compact_epochs(self) -> list[dict]:
        if not self._table_exists("compact_epochs"):
            return []
        with self._lock:
            rows = self._conn.execute(
                "SELECT compact_id, trigger, cutoff_seq, summary, created_seq "
                "FROM compact_epochs WHERE challenge_id=? ORDER BY created_seq",
                (self.challenge.id,),
            ).fetchall()
        return [
            {"compact_id": r[0], "trigger": r[1], "cutoff_seq": int(r[2] or 0),
             "summary": r[3] or "", "created_seq": int(r[4] or 0)}
            for r in rows
        ]

    def revive_resume_intents(self, *, actor: str = "coordinator") -> list[str]:
        """J: flip dispatch_state='resume' intents back to 'active' (e.g. a standby
        run continues, or operator resumes). Only re-activates rows still status=open."""
        with self._lock:
            rows = self._conn.execute(
                "SELECT intent_id FROM intents WHERE challenge_id=? "
                "AND dispatch_state='resume' AND status='open'",
                (self.challenge.id,),
            ).fetchall()
            revived = [str(r[0]) for r in rows]
            if revived:
                q = ",".join("?" for _ in revived)
                self._conn.execute(
                    f"UPDATE intents SET dispatch_state='active', stop_reason=NULL "
                    f"WHERE challenge_id=? AND intent_id IN ({q})",
                    (self.challenge.id, *revived),
                )
                self._conn.commit()
        if revived:
            self._append(EV_INTENT_STATE_CHANGED, actor,
                         {"intent_id": ",".join(revived),
                          "dispatch_state": INTENT_DISPATCH_ACTIVE})
        return revived

    def prior_intent_count(self) -> int:
        """How many intents this challenge's graph has EVER held (any status).

        This is the durable "has a prior solve touched this graph?" signal used by
        the coordinator's cold-start guard. Intents are written only by the
        reasoner/coordinator dispatching real work — operator pre-seeding adds
        *facts*, never intents — so a non-zero count means a previous run already
        planned and dispatched here, i.e. this launch is a resume/reopen, not a
        cold start. Queried off the materialized `intents` table so it survives a
        process restart (a fresh Swarm has empty in-memory state but the DB carries
        the history)."""
        with self._lock:
            row = self._conn.execute(
                "SELECT COUNT(*) FROM intents WHERE challenge_id=?",
                (self.challenge.id,),
            ).fetchone()
        return int(row[0]) if row else 0

    # ── intents (B: atomic claim) ───────────────────────────────────────
    def propose_intent(self, *, actor: str, intent_id: str, goal: str,
                       payload: Optional[dict] = None,
                       from_fact_seqs: Optional[list[int]] = None) -> int:
        payload = dict(payload or {})
        worker_class = str(payload.get("worker_class") or "code").strip()
        if worker_class not in {"code", "shell_agent", "verifier", "review"}:
            worker_class = "code"
        route_hash = self.normalize_route_hash(str(payload.get("route_hash") or "")) if payload.get("route_hash") else ""
        branch_id = str(payload.get("branch_id") or "").strip()
        lane_key = self.normalize_lane_key(str(payload.get("lane_key") or "")) if payload.get("lane_key") else ""
        risk_class = (
            _clean_lane_risk(str(payload.get("risk_class") or lane_key.split(":", 1)[0]))
            if lane_key else ""
        )
        raw_priority = payload.get("priority")
        if raw_priority is None and payload.get("source") == "operator_hint":
            raw_priority = "operator"
        if isinstance(raw_priority, str):
            priority = {"operator": 100, "high": 50, "normal": 0, "low": -10}.get(
                raw_priority.strip().lower(), 0)
        else:
            try:
                priority = int(raw_priority or 0)
            except (TypeError, ValueError):
                priority = 0
        resource_key = str(payload.get("resource_key") or "").strip()
        directive_id = str(payload.get("directive_id") or "").strip()
        depends_on: list[str] = []
        for raw_dep in payload.get("depends_on") or []:
            dep = str(raw_dep or "").strip()
            if dep and dep != intent_id and dep not in depends_on:
                depends_on.append(dep)
        seq = self._append(EV_INTENT_PROPOSED, actor,
                          {"intent_id": intent_id, "goal": goal,
                           **payload, "worker_class": worker_class,
                           "route_hash": route_hash, "branch_id": branch_id,
                           "lane_key": lane_key, "risk_class": risk_class,
                           "resource_key": resource_key, "directive_id": directive_id,
                           "priority": priority},
                          dedupe_key=f"intent::{intent_id}")
        # Round-14 declaration seam (default-off, additive): persist the
        # proposer's typed declaration alongside the intent. It rides the
        # EV_INTENT_PROPOSED payload above for free; the column makes it
        # queryable by research-side consumers (never read by dispatch).
        declares = payload.get("declares")
        declares_json = (
            json.dumps(declares, ensure_ascii=False, sort_keys=True)
            if isinstance(declares, dict) and declares
            else None
        )
        with self._lock:
            self._conn.execute(
                "INSERT OR IGNORE INTO intents "
                "(intent_id, challenge_id, goal, worker_class, route_hash, branch_id, "
                " lane_key, risk_class, priority, status, dispatch_state, created_seq, "
                " resource_key, directive_id, declares_json) "
                "VALUES (?,?,?,?,?,?,?,?,?,'open','active',?,?,?,?)",
                (intent_id, self.challenge.id, goal, worker_class,
                 route_hash or None, branch_id or None,
                 lane_key or None, risk_class if lane_key else None, priority,
                 seq if seq > 0 else 0, resource_key or None, directive_id or None,
                 declares_json),
            )
            if from_fact_seqs:
                for fs in from_fact_seqs:
                    self._conn.execute(
                        "INSERT OR IGNORE INTO intent_sources "
                        "(intent_id, fact_seq) VALUES (?,?)",
                        (intent_id, fs),
                    )
            self._conn.commit()
            for dep in depends_on:
                self._conn.execute(
                    "INSERT OR IGNORE INTO intent_dependencies "
                    "(intent_id, depends_on_intent_id, challenge_id, created_seq) "
                    "VALUES (?,?,?,?)",
                    (intent_id, dep, self.challenge.id, seq if seq > 0 else 0),
                )
            self._conn.commit()
        return seq

    # ── summaries (zh gist, written back once after deepseek-flash) ──────
    def record_fact_summary(self, *, fact_seq: int, summary: str) -> bool:
        """Patch events.payload["summary"] for the fact at `fact_seq`.

        events is append-only by design, but a gist is derived metadata, not a
        new fact — so we read-modify-write the one row's JSON payload in place.
        Returns True if the row was found and updated."""
        if fact_seq is None or fact_seq <= 0 or not summary:
            return False
        with self._lock:
            row = self._conn.execute(
                "SELECT payload FROM events WHERE seq=?", (fact_seq,)
            ).fetchone()
            if not row:
                return False
            try:
                payload = json.loads(row[0]) if row[0] else {}
            except (json.JSONDecodeError, TypeError):
                payload = {}
            payload["summary"] = summary
            self._conn.execute(
                "UPDATE events SET payload=? WHERE seq=?",
                (json.dumps(payload, default=str), fact_seq),
            )
            self._conn.commit()
            return True

    def record_intent_summary(self, *, intent_id: str, summary: str) -> bool:
        """Store the zh gist for an intent in intents.summary. Idempotent."""
        if not intent_id or not summary:
            return False
        with self._lock:
            cur = self._conn.execute(
                "UPDATE intents SET summary=? WHERE intent_id=?",
                (summary, intent_id),
            )
            self._conn.commit()
            return cur.rowcount > 0

    def claim_intent(self, *, worker: str, intent_id: str,
                     lease_s: float = 300.0) -> bool:
        """Single atomic UPDATE (B). True iff THIS worker won the claim.

        A/J: only a dispatch_state='active' intent is claimable — resume/retired/
        closed rows are held back even if their status is still 'open'."""
        now = time.time()
        with self._lock:
            cur = self._conn.execute(
                "UPDATE intents SET worker=?, status='claimed', lease_until=? "
                "WHERE intent_id=? AND challenge_id=? "
                "  AND dispatch_state='active' "
                "  AND (status='open' OR (status='claimed' AND lease_until < ?)) "
                "  AND NOT EXISTS ("
                "    SELECT 1 FROM intent_dependencies d "
                "    LEFT JOIN intents p ON p.challenge_id=intents.challenge_id "
                "     AND p.intent_id=d.depends_on_intent_id "
                "    WHERE d.challenge_id=intents.challenge_id "
                "     AND d.intent_id=intents.intent_id "
                "     AND (p.intent_id IS NULL OR p.status!='done' "
                "          OR COALESCE(p.dispatch_state,'active')!='closed')"
                "  )",
                (worker, now + lease_s, intent_id, self.challenge.id, now),
            )
            self._conn.commit()
            won = cur.rowcount == 1
        if won:
            self._append(EV_INTENT_CLAIMED, worker, {"intent_id": intent_id})
        return won

    def release_intent_claim(
        self, *, worker: str, intent_id: str, reason: str = "",
    ) -> bool:
        """Owner-fenced return of a not-yet-started intent to the open queue."""
        with self._lock:
            self._conn.execute("BEGIN IMMEDIATE")
            try:
                cur = self._conn.execute(
                    "UPDATE intents SET worker=NULL, status='open', lease_until=NULL "
                    "WHERE intent_id=? AND challenge_id=? AND status='claimed' "
                    "AND worker=?",
                    (intent_id, self.challenge.id, worker),
                )
                released = cur.rowcount == 1
                self._conn.commit()
            except BaseException:
                # A failed commit leaves the uncommitted UPDATE visible through
                # this same long-lived connection.  Retirement's verification read
                # must never mistake that local view for a durable release.
                self._conn.rollback()
                raise
        if released:
            self._append(
                EV_INTENT_STATE_CHANGED, worker,
                {"intent_id": intent_id, "status": "open",
                 "reason": str(reason or "claim released")[:500]},
            )
        return released

    def intent_claim_state(self, intent_id: str) -> dict[str, str]:
        """Read the materialized owner/status used to verify idempotent retirement."""
        with self._lock:
            row = self._conn.execute(
                "SELECT status, worker, dispatch_state FROM intents "
                "WHERE intent_id=? AND challenge_id=?",
                (intent_id, self.challenge.id),
            ).fetchone()
        if row is None:
            return {}
        return {
            "status": str(row[0] or ""),
            "worker": str(row[1] or ""),
            "dispatch_state": str(row[2] or ""),
        }

    def reopen_intent(self, *, actor: str, intent_id: str, reason: str = "") -> bool:
        """Return a concluded intent to the open dispatch queue."""
        iid = str(intent_id or "").strip()
        if not iid:
            return False
        with self._lock:
            cur = self._conn.execute(
                "UPDATE intents SET status='open', dispatch_state='active', "
                "worker=NULL, lease_until=NULL "
                "WHERE intent_id=? AND challenge_id=? AND status='done'",
                (iid, self.challenge.id),
            )
            self._conn.commit()
            n = int(cur.rowcount or 0)
        if n == 1:
            self._append(
                EV_INTENT_STATE_CHANGED, actor,
                {"intent_id": iid, "status": "open",
                 "reason": str(reason or "retry")[:500]},
            )
        return n == 1

    def terminalize_intent_claim(
        self, *, worker: str, intent_id: str, reason: str = "",
    ) -> bool:
        """Idempotently close one owner's possibly-executed intent.

        The deterministic event key lets a retry repair the materialized row after
        a crash between event append and owner-fenced UPDATE without duplicating
        terminal events.  True is returned only after the postcondition read proves
        the intent is terminal.
        """
        state = self.intent_claim_state(intent_id)
        if state.get("status") == "done" or state.get("dispatch_state") in {
                "closed", "retired"}:
            return True
        if (state.get("status") != "claimed"
                or state.get("worker") != worker):
            return False
        detail = str(reason or "worker runtime ended after process start")[:500]
        dedupe_key = f"runtime-retire::{intent_id}::{worker}"
        seq = self._append(
            EV_INTENT_CONCLUDED, worker,
            {"intent_id": intent_id, "result": "cancelled",
             "result_detail": detail},
            dedupe_key=dedupe_key,
        )
        if seq <= 0:
            # _append returns -1 on a dedupe collision.  A prior event may have
            # committed just before the materialized UPDATE failed; recover that
            # exact sequence so the repaired row keeps its audit lineage.
            with self._lock:
                prior = self._conn.execute(
                    "SELECT seq FROM events WHERE challenge_id=? AND dedupe_key=?",
                    (self.challenge.id, dedupe_key),
                ).fetchone()
            seq = int(prior[0]) if prior else 0
        with self._lock:
            self._conn.execute("BEGIN IMMEDIATE")
            try:
                self._conn.execute(
                    "UPDATE intents SET status='done', dispatch_state='closed', "
                    "close_reason='cancelled', result_seq=COALESCE(result_seq, ?), "
                    "result_detail=? WHERE intent_id=? AND challenge_id=? "
                    "AND status='claimed' AND worker=?",
                    (seq if seq > 0 else None, detail, intent_id,
                     self.challenge.id, worker),
                )
                self._conn.commit()
            except BaseException:
                # Do not expose an uncommitted terminal state to the retrying
                # runtime reaper.  The append is deduped, so the next attempt can
                # safely repair the materialized row after this rollback.
                self._conn.rollback()
                raise
        after = self.intent_claim_state(intent_id)
        return (
            after.get("status") == "done"
            or after.get("dispatch_state") in {"closed", "retired"}
        )

    @staticmethod
    def _norm_activity_key(key: str) -> str:
        """Normalize an activity key so 'nmap 8.130.96.176' and 'NMAP:8.130.96.176'
        collide. Lowercase, collapse whitespace/separators to ':'."""
        import re as _re
        k = (key or "").strip().lower()
        k = _re.sub(r"[\s/]+", ":", k)
        k = _re.sub(r":+", ":", k).strip(":")
        return k

    def try_claim_activity(self, *, worker: str, key: str,
                           lease_s: float = 600.0) -> bool:
        """P4: atomically claim a high-cost activity. True iff THIS worker won (no
        live claim existed). A parallel worker that gets False should AVOID redoing
        the activity (a teammate is on it). Lease-expiry lets an abandoned activity
        be re-claimed. INSERT-or-take-expired in one atomic step."""
        nkey = self._norm_activity_key(key)
        if not nkey:
            return True  # nothing to lock on → don't block
        now = time.time()
        with self._lock:
            # take over only if no row, or the existing lease expired.
            cur = self._conn.execute(
                "INSERT INTO activity_locks "
                "(activity_key, challenge_id, worker, lease_until, claimed_ts) "
                "VALUES (?,?,?,?,?) "
                "ON CONFLICT(activity_key) DO UPDATE SET "
                "  worker=excluded.worker, lease_until=excluded.lease_until, "
                "  claimed_ts=excluded.claimed_ts "
                "WHERE activity_locks.lease_until < ?",
                (nkey, self.challenge.id, worker, now + lease_s, now, now),
            )
            self._conn.commit()
            return cur.rowcount == 1

    def release_activity(self, *, worker: str, key: str) -> None:
        """Release an activity lock this worker holds (best-effort; owner-fenced)."""
        nkey = self._norm_activity_key(key)
        if not nkey:
            return
        with self._lock:
            self._conn.execute(
                "DELETE FROM activity_locks WHERE activity_key=? AND worker=?",
                (nkey, worker))
            self._conn.commit()

    def active_activities(self) -> list[dict]:
        """Currently-held activity locks (lease not expired) — for the board so a
        worker's prompt can show 'teammates are already doing X' and avoid it."""
        now = time.time()
        with self._lock:
            rows = self._conn.execute(
                "SELECT activity_key, worker FROM activity_locks "
                "WHERE challenge_id=? AND lease_until > ? ORDER BY claimed_ts",
                (self.challenge.id, now)).fetchall()
        return [{"activity": r[0], "worker": r[1]} for r in rows]

    def _activity_locks_block(self) -> str:
        """Render in-progress activities for the board, so workers avoid redoing a
        nmap/brute a teammate already started. Empty when none."""
        acts = self.active_activities()
        if not acts:
            return ""
        lines = ["\n## In progress (a teammate is already doing these — do NOT redo)"]
        for a in acts[:30]:
            lines.append(f"- {a['activity']} [{a['worker']}]")
        return "\n".join(lines)

    def _lane_locks_block(self) -> str:
        lanes = self.active_lanes()
        if not lanes:
            return ""
        lines = ["\n## Exclusive lanes (do NOT duplicate dangerous work)"]
        for lane in lanes[:30]:
            lines.append(
                f"- {lane['lane_key']} [{lane['owner_worker']}] "
                f"intent={lane['owner_intent']}")
        return "\n".join(lines)

    def _resource_locks_block(self) -> str:
        """E: active resource locks (site/account/listener) a teammate holds — a
        worker must not run conflicting destructive/exclusive work on these."""
        locks = self.active_resource_locks()
        if not locks:
            return ""
        lines = ["\n## Held resource locks (do NOT run conflicting work)"]
        for rl in locks[:30]:
            risk = f" risk={rl['risk_class']}" if rl.get("risk_class") else ""
            lines.append(
                f"- {rl['resource_key']} (scope={rl['scope']}{risk}) "
                f"[{rl['owner_worker']}]")
        return "\n".join(lines)

    def conclude_intent(self, *, actor: str, intent_id: str,
                        result: str = "",
                        to_fact_seq: Optional[int] = None,
                        result_detail: str = "") -> int:
        """Mark an intent done — but ONLY if `actor` still OWNS the claim (owner
        fencing). The coordinator claims an explore intent under the worker's own
        solver_id, so the worker that concludes is the owner. If the worker's lease
        lapsed and the coordinator re-dispatched the intent to a NEW worker (owner
        changes to that new solver_id via _open_intents → claim_intent), then a
        slow/late ORIGINAL worker concluding now is NO LONGER the owner and must NOT
        clobber the fresh claim. The EV_INTENT_CONCLUDED event is still appended
        (provenance of what the late worker reported); only the intents-table state
        row is fenced. Exceptions that always win: a 'solved' conclusion (a real flag
        ends the run regardless), and actor 'coordinator' (legacy/admin path)."""
        detail = (result_detail or "").strip()
        payload = {"intent_id": intent_id, "result": result}
        if detail:
            payload["result_detail"] = detail
        if to_fact_seq is not None:
            payload["to_fact_seq"] = to_fact_seq
        seq = self._append(EV_INTENT_CONCLUDED, actor, payload)
        # owner fence: only the current owner (or coordinator, or a solved result)
        # may flip the row to done. worker IS NULL handles never-claimed intents
        # some paths conclude as a no-op.
        fence = "" if (result == "solved" or actor == "coordinator") else (
            " AND (worker=? OR worker IS NULL)")
        # A/J: a concluded intent also leaves the dispatch pool (closed), with the
        # conclusion text as its close_reason — distinguishes it from resume/retired.
        close_reason = (result or "concluded").strip()[:200]
        with self._lock:
            if to_fact_seq is not None:
                sql = ("UPDATE intents SET status='done', dispatch_state='closed', "
                       "close_reason=?, result_seq=?, result_detail=?, to_fact_seq=? "
                       "WHERE intent_id=? AND challenge_id=?" + fence)
                params: list = [close_reason, seq if seq > 0 else None, detail or None,
                                to_fact_seq,
                                intent_id, self.challenge.id]
            else:
                sql = ("UPDATE intents SET status='done', dispatch_state='closed', "
                       "close_reason=?, result_seq=?, result_detail=? "
                       "WHERE intent_id=? AND challenge_id=?" + fence)
                params = [close_reason, seq if seq > 0 else None, detail or None, intent_id,
                          self.challenge.id]
            if fence:
                params.append(actor)
            self._conn.execute(sql, tuple(params))
            if self._intent_result_marks_poc_spent(result):
                self._conn.execute(
                    "UPDATE pocs SET status='spent', result_seq=? "
                    "WHERE challenge_id=? AND intent_id=? "
                    "AND status IN ('available','wip','directional')",
                    (seq if seq > 0 else None, self.challenge.id, intent_id),
                )
            self._conn.commit()
        return seq

    @staticmethod
    def _intent_result_marks_poc_spent(result: str) -> bool:
        return is_genuine_giveup(result)

    def save_poc(self, *, actor: str, poc_id: str, path: str,
                 entry_command: str, status: str = "available",
                 note: str = "", artifact_id: Optional[str] = None,
                 intent_id: Optional[str] = None, name: str = "") -> int:
        """Register a PoC as metadata for a shared artifact body.

        The body lives in workspace/shared CAS; this graph is the source of truth
        for inheritance state.
        """
        status = status if status in {"available", "wip", "directional", "spent", "quarantined"} else "available"
        payload = {
            "poc_id": poc_id,
            "intent_id": intent_id,
            "name": name or Path(path).name,
            "path": path,
            "entry_command": entry_command,
            "status": status,
            "note": note,
        }
        seq = self._append(EV_POC_SAVED, actor, payload,
                           artifact_id=artifact_id,
                           dedupe_key=f"poc::{poc_id}::{status}::{entry_command}::{note}")
        with self._lock:
            self._conn.execute(
                "INSERT INTO pocs "
                "(poc_id, challenge_id, intent_id, name, path, artifact_id, "
                " entry_command, status, note, created_seq) "
                "VALUES (?,?,?,?,?,?,?,?,?,?) "
                "ON CONFLICT(poc_id) DO UPDATE SET "
                " intent_id=excluded.intent_id, name=excluded.name, path=excluded.path, "
                " artifact_id=excluded.artifact_id, entry_command=excluded.entry_command, "
                " status=excluded.status, note=excluded.note",
                (poc_id, self.challenge.id, intent_id, payload["name"], path,
                 artifact_id, entry_command, status, note, seq if seq > 0 else 0),
            )
            self._conn.commit()
        return seq

    def claim_poc(self, *, worker: str, poc_id: str,
                  lease_s: float = 300.0) -> bool:
        now = time.time()
        with self._lock:
            cur = self._conn.execute(
                "UPDATE pocs SET worker=?, status='wip', lease_until=? "
                "WHERE poc_id=? AND challenge_id=? "
                "AND status IN ('available','directional','wip') "
                "AND (worker IS NULL OR lease_until IS NULL OR lease_until < ?)",
                (worker, now + lease_s, poc_id, self.challenge.id, now),
            )
            self._conn.commit()
            won = cur.rowcount == 1
        if won:
            self._append(EV_POC_CLAIMED, worker, {"poc_id": poc_id})
        return won

    def conclude_poc(self, *, actor: str, poc_id: str,
                     status: str = "spent", note: str = "") -> int:
        status = status if status in {"available", "directional", "spent", "quarantined"} else "spent"
        seq = self._append(EV_POC_CONCLUDED, actor,
                           {"poc_id": poc_id, "status": status, "note": note})
        fence = " AND (worker=? OR worker IS NULL)"
        with self._lock:
            self._conn.execute(
                "UPDATE pocs SET status=?, result_seq=? "
                "WHERE poc_id=? AND challenge_id=?" + fence,
                (status, seq if seq > 0 else None, poc_id, self.challenge.id, actor),
            )
            self._conn.commit()
        return seq

    def pocs(self, *, inheritable_only: bool = False) -> list[dict]:
        sql = ("SELECT poc_id, intent_id, name, path, artifact_id, entry_command, "
               "status, note, worker FROM pocs WHERE challenge_id=?")
        params: list[Any] = [self.challenge.id]
        if inheritable_only:
            # A PoC is inheritable if it's available/directional, OR it was claimed
            # ('wip') but the claiming worker's lease has EXPIRED (#9). claim_poc
            # flips status→'wip' to mark "in use by the current worker"; without the
            # expired-lease clause a wip PoC would vanish from the pool forever the
            # moment any worker claimed it (single-use inheritance — nothing ever
            # resets wip→available). Mirrors how _open_intents re-offers an
            # expired-lease 'claimed' intent. now() bound below.
            sql += (" AND (status IN ('available','directional') OR "
                    "(status='wip' AND (lease_until IS NULL OR lease_until < ?)))")
            params.append(time.time())
        sql += " ORDER BY created_seq"
        with self._lock:
            rows = self._conn.execute(sql, tuple(params)).fetchall()
        return [
            {"poc_id": r[0], "intent_id": r[1], "name": r[2], "path": r[3],
             "artifact_id": r[4], "entry_command": r[5], "status": r[6],
             "note": r[7], "worker": r[8]}
            for r in rows
        ]

    def reopen_after_false_positive(self, *, actor: str, flag: str,
                                    reason: str = "") -> dict:
        """A human marked ONE flag as a FALSE POSITIVE. Record it as a dead-end (so
        nobody retries it), DROP it from the flag set (other collected flags are
        kept — multi-flag), and re-open the concluded intent(s) so a standby worker
        re-finds the missing flag from the verified facts.

        Returns {dead_end_seq, dead_end_reason, reopened: [intent_id, ...]} so the
        caller can emit the matching blackboard/graph deltas (fact-graph + board
        grow a dead-end node; the reopened intents flip back to 'open')."""
        why = reason or f"false positive: {flag}"
        dead_seq = self._append(EV_DEAD_END, actor, {"reason": why},
                                dedupe_key=f"deadend::fp::{flag}")
        # remove just this flag from the run's set (snapshot replays this).
        self._append(EV_FLAG_INVALIDATED, actor, {"flag": flag},
                     dedupe_key=f"flaginvalid::{flag}")
        reopened: list[str] = []
        with self._lock:
            # reopen every intent that was concluded with result 'solved' — the solve
            # they led to is now invalid. Clear the produced-fact link too. (Intent→
            # flag linkage isn't stored, so we reopen the SOLVED set and let the
            # worker, seeded with the still-valid flags, re-find only the missing
            # one — the worker prompt's already-found list keeps it from re-hunting
            # the good ones.)
            #
            # #11: DON'T reopen non-solved 'done' intents. supersede_open_intents
            # also flips intents to status='done' (result 'superseded') when the
            # operator supplies a resource that obsoletes an "ask the operator for X"
            # intent. Blindly reopening every 'done' row resurrected those retired
            # asks on a false-positive (run-11190's 238-worker "request the password"
            # loop came back). Fence on the concluding event's result text via the
            # result_seq → events.payload pattern (LEFT JOIN, used elsewhere).
            linked_intents: set[str] = set()
            for (payload,) in self._conn.execute(
                "SELECT payload FROM events WHERE challenge_id=? AND kind=?",
                (self.challenge.id, EV_FLAG_FOUND),
            ).fetchall():
                try:
                    p = json.loads(payload or "{}") or {}
                except (json.JSONDecodeError, TypeError):
                    continue
                if p.get("flag") == flag and p.get("intent_id"):
                    linked_intents.add(str(p["intent_id"]))

            rows = self._conn.execute(
                "SELECT i.intent_id, e.payload FROM intents i "
                "LEFT JOIN events e ON e.seq = i.result_seq "
                "WHERE i.challenge_id=? AND i.status='done'",
                (self.challenge.id,),
            ).fetchall()
            for intent_id, payload in rows:
                result = ""
                if payload:
                    try:
                        result = str((json.loads(payload) or {}).get("result", "")).lower()
                    except (json.JSONDecodeError, TypeError):
                        result = ""
                if result == "solved" and (not linked_intents or intent_id in linked_intents):
                    reopened.append(intent_id)
            if reopened:
                qmarks = ",".join("?" for _ in reopened)
                self._conn.execute(
                    f"UPDATE intents SET status='open', dispatch_state='active', "
                    f"close_reason=NULL, to_fact_seq=NULL, "
                    f"result_seq=NULL WHERE challenge_id=? AND intent_id IN ({qmarks})",
                    (self.challenge.id, *reopened),
                )
            self._conn.commit()
        return {"dead_end_seq": dead_seq, "dead_end_reason": why,
                "reopened": reopened}

    def supersede_open_intents(self, *, actor: str, match: str,
                               reason: str = "") -> list[str]:
        """Retire every OPEN/claimed-lease-expired intent whose goal contains the
        `match` substring (case-insensitive) — they've been made obsolete by an
        operator action. run-11190: a worker proposes "Request the operator for the
        L2 SSH password", the operator then SUPPLIES it as a standing hint, but the
        old "ask for the password" intents stayed status='open' forever, so fresh
        explore workers kept claiming them and re-asking for a password they already
        had → 238-worker dead loop. Flipping them to status='done' (result=
        'superseded') stops _open_intents from re-dispatching them. Returns the list
        of superseded intent_ids (for a blackboard delta). Only OPEN or expired-lease
        rows are touched — a live claim a worker is actively working is left alone.

        #11: a marker EV_INTENT_CONCLUDED event with result='superseded' is appended
        and its seq stored in each row's result_seq, so the rows are DISTINGUISHABLE
        from a genuinely solved 'done' intent. reopen_after_false_positive uses that
        result text to reopen ONLY solved intents and leave these superseded asks
        retired (run-11190 regression)."""
        import time as _time
        now = _time.time()
        like = f"%{match.lower()}%"
        with self._lock:
            rows = self._conn.execute(
                "SELECT intent_id FROM intents WHERE challenge_id=? "
                "  AND (status='open' OR (status='claimed' AND lease_until IS NOT NULL "
                "       AND lease_until < ?)) "
                "  AND lower(goal) LIKE ?",
                (self.challenge.id, now, like),
            ).fetchall()
            ids = [r[0] for r in rows]
        marker_seq = 0
        if ids:
            # append the provenance marker OUTSIDE the lock (._append takes the lock),
            # then stamp result_seq under the lock.
            marker_seq = self._append(
                EV_INTENT_CONCLUDED, actor,
                {"intent_id": ",".join(ids), "result": "superseded",
                 "match": match})
            with self._lock:
                qmarks = ",".join("?" for _ in ids)
                self._conn.execute(
                    f"UPDATE intents SET status='done', result_seq=? "
                    f"WHERE challenge_id=? AND intent_id IN ({qmarks})",
                    (marker_seq if marker_seq > 0 else None, self.challenge.id, *ids),
                )
                self._conn.commit()
            self._append(EV_DEAD_END, actor,
                         {"reason": reason or f"superseded by operator: {match}"},
                         dedupe_key=f"supersede::{match}::{len(ids)}")
        return ids
