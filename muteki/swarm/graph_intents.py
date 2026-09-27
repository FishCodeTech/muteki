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
    EV_WORKER_RESULT_COMMITTED, EV_VALUE_RECEIPT,
    FACT_STATE_UNRESOLVED, FACT_STATE_CHALLENGED, FACT_STATE_REVALIDATED,
    FACT_STATE_REJECTED, FACT_STATE_MERGED, FACT_STATE_SUPERSEDED,
    _FACT_TERMINAL_STATES, _FACT_STATES,
    INTENT_DISPATCH_ACTIVE, INTENT_DISPATCH_RESUME, INTENT_DISPATCH_RETIRED,
    INTENT_DISPATCH_CLOSED, INTENT_DISPATCH_BLOCKED, _INTENT_DISPATCH_STATES,
    _SERVICE_DEFAULT_PORTS, _LANE_RISK_CLASSES, _FACT_ENGINE_PREFIX_RE,
    _normalize_fact_identity, _clean_lane_risk, _clean_lane_host, canonicalize_lane,
    WORKER_RUNTIME_CAPABILITY_KEYS,
)


class _IntentsPocsMixin:
    def query_legacy_candidates(self) -> list[dict]:
        """Compatibility read for the narrow Protocol 1 SearchStatePort."""
        if getattr(self.challenge, "mode", "ctf") == "ctf":
            # A newly proved, high-priority exploit Step must not wait behind a
            # normal-priority Review/Verifier row merely because that role was
            # inserted first.  Pentest keeps the report-pipeline-first order.
            dispatch_order = (
                "priority DESC, "
                "CASE WHEN worker_class IN ('code','shell_agent') THEN 0 "
                "WHEN worker_class='verifier' THEN 1 ELSE 2 END, created_seq"
            )
        else:
            dispatch_order = (
                "CASE WHEN worker_class IN ('verifier','review') THEN 0 ELSE 1 END, "
                "priority DESC, created_seq"
            )
        step_columns = ", ".join(
            f"i.{column}" if self._column_exists("intents", column) else "''"
            for column in (
                "expected_observable", "stop_condition", "coverage_key",
            )
        )
        with self._lock:
            rows = self._conn.execute(
                "SELECT i.intent_id, i.goal, i.worker_class, i.route_hash, "
                "i.branch_id, i.priority, i.lane_key, i.risk_class, "
                f"i.resource_key, {step_columns}, i.value_claim_json, "
                "i.priority_reason, i.novelty_key, i.requires_capabilities_json, "
                "i.required_pocs_json "
                "FROM intents i "
                "WHERE i.dispatch_state='active' AND i.status='open' "
                "AND NOT EXISTS ("
                "  SELECT 1 FROM intent_dependencies d "
                "  LEFT JOIN intents p ON p.challenge_id=i.challenge_id "
                "   AND p.intent_id=d.depends_on_intent_id "
                "  WHERE d.challenge_id=i.challenge_id "
                "   AND d.intent_id=i.intent_id "
                "   AND ((p.intent_id IS NULL "
                "         AND d.depends_on_intent_id GLOB '*[^0-9]*') "
                "        OR (p.intent_id IS NOT NULL "
                "            AND (p.status!='done' "
                "                 OR COALESCE(p.dispatch_state,'active')!='closed')))"
                ") "
                f"ORDER BY {dispatch_order}",
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
        available_pocs = {
            str(poc.get("poc_id") or "")
            for poc in self.pocs()
            if str(poc.get("status") or "") in {"available", "directional", "wip"}
        }
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
                "expected_observable": row[9] or "",
                "stop_condition": row[10] or "",
                "coverage_key": row[11] or "",
                "value_claim": json.loads(row[12] or "{}"),
                "priority_reason": row[13] or "",
                "novelty_key": row[14] or "",
                "requires_capabilities": json.loads(row[15] or "[]"),
                "required_pocs": json.loads(row[16] or "[]"),
            }
            for row in rows
            if set(json.loads(row[16] or "[]")) <= available_pocs
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
            # stale = product-less intents that are ALREADY
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
                "SELECT i.intent_id FROM intents i WHERE i.challenge_id=? "
                "AND NOT EXISTS (SELECT 1 FROM intent_products p "
                "                WHERE p.intent_id=i.intent_id) AND ("
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

    def record_reason_context_compaction(
        self, *, actor: str, cutoff_seq: int, summary: str,
        tokens_before: int,
    ) -> dict:
        cutoff = max(0, int(cutoff_seq))
        text = str(summary or "").strip()
        if not text:
            raise ValueError("reason context compaction requires a summary")
        digest = hashlib.sha1(
            f"reason_context:{cutoff}:{text}".encode("utf-8", "ignore")
        ).hexdigest()[:10]
        compact_id = f"C-reason-{digest}"
        seq = self._append(
            EV_GRAPH_COMPACTED, actor,
            {"compact_id": compact_id, "trigger": "reason_context",
             "cutoff_seq": cutoff, "summary": text,
             "tokens_before": max(0, int(tokens_before)),
             "retired_intent_ids": []},
            dedupe_key=f"reason-context-compact::{cutoff}::{digest}",
        )
        with self._lock:
            self._conn.execute(
                "INSERT OR IGNORE INTO compact_epochs "
                "(compact_id, challenge_id, trigger, cutoff_seq, summary, "
                " retained_fact_seqs, retired_intent_ids, stale_route_hashes, created_seq) "
                "VALUES (?,?,?,?,?,?,?,?,?)",
                (compact_id, self.challenge.id, "reason_context", cutoff, text,
                 None, "[]", None, seq if seq > 0 else 0),
            )
            self._conn.commit()
        return {"compact_id": compact_id, "trigger": "reason_context",
                "cutoff_seq": cutoff, "summary": text,
                "tokens_before": max(0, int(tokens_before))}

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
        if getattr(self.challenge, "mode", "ctf") == "ctf":
            intent_id = str(intent_id or "").strip()
            goal = str(goal or "").strip()
            if not intent_id or not goal:
                return -1
            expected_observable = str(payload.get("expected_observable") or "").strip()
            stop_condition = str(payload.get("stop_condition") or "").strip()
            coverage_key = str(payload.get("coverage_key") or "").strip()
            required_pocs = list(dict.fromkeys(
                str(item).strip() for item in (payload.get("required_pocs") or [])
                if str(item).strip()
            ))
            worker_class = str(payload.get("worker_class") or "code").strip()
            if worker_class not in {"code", "shell_agent"}:
                worker_class = "code"
            raw_priority = payload.get("priority")
            if isinstance(raw_priority, str):
                priority = {
                    "high": 50,
                    "normal": 0,
                    "low": -10,
                }.get(raw_priority.strip().lower(), 0)
                requested_priority = raw_priority.strip().lower()
            else:
                try:
                    priority = int(raw_priority or 0)
                except (TypeError, ValueError):
                    priority = 0
                requested_priority = "normal"
            source_fact_seqs: list[int] = []
            for raw_seq in from_fact_seqs or []:
                try:
                    fact_seq = int(raw_seq)
                except (TypeError, ValueError):
                    continue
                if fact_seq > 0 and fact_seq not in source_fact_seqs:
                    source_fact_seqs.append(fact_seq)
            source_fact_seqs.sort()
            with self._lock:
                self._conn.execute("BEGIN IMMEDIATE")
                try:
                    if source_fact_seqs:
                        marks = ",".join("?" for _ in source_fact_seqs)
                        known = {
                            int(row[0]) for row in self._conn.execute(
                                f"SELECT seq FROM events WHERE challenge_id=? "
                                f"AND kind=? AND seq IN ({marks})",
                                (self.challenge.id, EV_FACT_ADDED, *source_fact_seqs),
                            ).fetchall()
                        }
                        if known != set(source_fact_seqs):
                            self._conn.rollback()
                            return -1
                    if required_pocs:
                        marks = ",".join("?" for _ in required_pocs)
                        known_pocs = {
                            str(row[0]) for row in self._conn.execute(
                                f"SELECT poc_id FROM pocs WHERE challenge_id=? "
                                f"AND poc_id IN ({marks}) AND status IN "
                                "('available','directional','wip')",
                                (self.challenge.id, *required_pocs),
                            ).fetchall()
                        }
                        if known_pocs != set(required_pocs):
                            self._conn.rollback()
                            return -1
                    seq = self._append_locked(
                        EV_INTENT_PROPOSED,
                        actor,
                        {
                            "intent_id": intent_id,
                            "goal": goal,
                            "worker_class": worker_class,
                            "priority": priority,
                            "requested_priority": requested_priority,
                            "from_facts": source_fact_seqs,
                            "expected_observable": expected_observable,
                            "stop_condition": stop_condition,
                            "coverage_key": coverage_key,
                            "required_pocs": required_pocs,
                        },
                        dedupe_key=f"intent::{intent_id}",
                    )
                    if seq < 0:
                        self._conn.rollback()
                        return -1
                    cur = self._conn.execute(
                        "INSERT OR IGNORE INTO intents "
                        "(intent_id,challenge_id,goal,worker_class,priority,status,"
                        "dispatch_state,created_seq,requested_priority,"
                        "expected_observable,stop_condition,coverage_key,required_pocs_json) "
                        "VALUES (?,?,?,?,?,'open','active',?,?,?,?,?,?)",
                        (
                            intent_id,
                            self.challenge.id,
                            goal,
                            worker_class,
                            priority,
                            seq,
                            requested_priority,
                            expected_observable or None,
                            stop_condition or None,
                            coverage_key or None,
                            json.dumps(required_pocs, ensure_ascii=False),
                        ),
                    )
                    if int(cur.rowcount or 0) != 1:
                        self._conn.rollback()
                        return -1
                    for fact_seq in source_fact_seqs:
                        self._conn.execute(
                            "INSERT OR IGNORE INTO intent_sources "
                            "(intent_id, fact_seq) VALUES (?,?)",
                            (intent_id, fact_seq),
                        )
                    self._conn.commit()
                    return seq
                except BaseException:
                    self._conn.rollback()
                    raise
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
        requested_priority = str(payload.get("requested_priority") or raw_priority or "normal")[:20]
        priority_reason = str(payload.get("priority_reason") or "")[:1000]
        value_claim = payload.get("value_claim")
        value_claim_json = (
            json.dumps(value_claim, ensure_ascii=False, sort_keys=True)
            if isinstance(value_claim, dict) and value_claim else None
        )
        novelty_key = str(payload.get("novelty_key") or (
            value_claim.get("novelty_key") if isinstance(value_claim, dict) else ""
        ) or "").strip().casefold()[:300]
        requires_capabilities = list(dict.fromkeys(
            str(item).strip().casefold()
            for item in (payload.get("requires_capabilities") or [])
            if str(item).strip()
        ))[:32]
        requires_capabilities_json = json.dumps(requires_capabilities)
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
        if depends_on:
            with self._lock:
                qmarks = ",".join("?" for _ in depends_on)
                known = {
                    str(row[0])
                    for row in self._conn.execute(
                        f"SELECT intent_id FROM intents WHERE challenge_id=? "
                        f"AND intent_id IN ({qmarks})",
                        (self.challenge.id, *depends_on),
                    ).fetchall()
                }
            # Persist only real Intent edges. Reason dispatch orders same-batch
            # parents before children; legacy numeric Fact seq edges remain only
            # in old databases and are tolerated by the read path above.
            depends_on = [dep for dep in depends_on if dep in known]
        expected_observable = str(payload.get("expected_observable") or "").strip()
        stop_condition = str(payload.get("stop_condition") or "").strip()
        coverage_key = str(payload.get("coverage_key") or "").strip()
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
        source_fact_seqs: list[int] = []
        for raw_seq in from_fact_seqs or []:
            try:
                fact_seq = int(raw_seq)
            except (TypeError, ValueError):
                continue
            if fact_seq > 0 and fact_seq not in source_fact_seqs:
                source_fact_seqs.append(fact_seq)
        source_fact_seqs.sort()
        if (getattr(self.challenge, "mode", "ctf") != "ctf"
                and worker_class in {"code", "shell_agent", "review"}
                and source_fact_seqs
                and any(seq not in self._active_fact_seq_set()
                        for seq in source_fact_seqs)):
            return -1
        with self._lock:
            self._conn.execute("BEGIN IMMEDIATE")
            try:
                if branch_id:
                    owner = self._conn.execute(
                        "SELECT intent_id FROM intents WHERE challenge_id=? "
                        "AND branch_id=? AND status IN ('open','claimed') "
                        "AND dispatch_state='active' AND intent_id<>? LIMIT 1",
                        (self.challenge.id, branch_id, intent_id),
                    ).fetchone()
                    # A Branch owns one live Step. Its next Step may be queued
                    # behind that exact owner and remains unclaimable until the
                    # predecessor checkpoint closes. Unrelated same-Branch work
                    # is still rejected here.
                    if (owner is not None
                            and str(owner[0] or "") not in depends_on):
                        self._conn.rollback()
                        return -1
                seq = self._append_locked(
                    EV_INTENT_PROPOSED,
                    actor,
                    {"intent_id": intent_id, "goal": goal,
                     **payload, "worker_class": worker_class,
                     "route_hash": route_hash, "branch_id": branch_id,
                     "lane_key": lane_key, "risk_class": risk_class,
                     "resource_key": resource_key, "directive_id": directive_id,
                     "priority": priority,
                     "requested_priority": requested_priority,
                     "priority_reason": priority_reason,
                     "value_claim": value_claim or {},
                     "novelty_key": novelty_key,
                     "requires_capabilities": requires_capabilities,
                     "expected_observable": expected_observable,
                     "stop_condition": stop_condition,
                     "coverage_key": coverage_key},
                    dedupe_key=f"intent::{intent_id}",
                )
                if seq < 0:
                    self._conn.rollback()
                    return -1
                cur = self._conn.execute(
                    "INSERT OR IGNORE INTO intents "
                    "(intent_id, challenge_id, goal, worker_class, route_hash, branch_id, "
                    " lane_key, risk_class, priority, status, dispatch_state, created_seq, "
                    " resource_key, directive_id, declares_json, "
                    " expected_observable, stop_condition, coverage_key, requested_priority,"
                    " priority_reason,value_claim_json,novelty_key,requires_capabilities_json) "
                    "VALUES (?,?,?,?,?,?,?,?,?,'open','active',?,?,?,?,?,?,?,?,?,?,?,?)",
                    (intent_id, self.challenge.id, goal, worker_class,
                     route_hash or None, branch_id or None,
                     lane_key or None, risk_class if lane_key else None, priority,
                     seq, resource_key or None, directive_id or None,
                     declares_json,
                     expected_observable or None, stop_condition or None,
                     coverage_key or None, requested_priority or None,
                     priority_reason or None, value_claim_json, novelty_key or None,
                     requires_capabilities_json),
                )
                if int(cur.rowcount or 0) != 1:
                    self._conn.rollback()
                    return -1
                if source_fact_seqs:
                    for fs in source_fact_seqs:
                        self._conn.execute(
                            "INSERT OR IGNORE INTO intent_sources "
                            "(intent_id, fact_seq) VALUES (?,?)",
                            (intent_id, fs),
                        )
                for dep in depends_on:
                    self._conn.execute(
                        "INSERT OR IGNORE INTO intent_dependencies "
                        "(intent_id, depends_on_intent_id, challenge_id, created_seq) "
                        "VALUES (?,?,?,?)",
                        (intent_id, dep, self.challenge.id, seq),
                    )
                self._conn.commit()
                return seq
            except BaseException:
                self._conn.rollback()
                raise

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

    def start_intent(self, *, actor: str, worker: str, intent_id: str,
                     goal: str, worker_class: str = "code") -> bool:
        """Create and claim one Worker-owned Intent in one transaction.

        Bootstrap Workers create their own whole-challenge Intent.  Publishing an
        open row and claiming it in a later transaction exposes an ownerless row to
        the Scheduler.  This transition inserts both audit events and materializes
        the claimed owner under the same SQLite write lock.  An existing claim is
        accepted only when it already belongs to this Worker.
        """
        iid = str(intent_id or "").strip()
        owner = str(worker or "").strip()
        text = str(goal or "").strip()
        if not iid or not owner or not text:
            return False
        klass = str(worker_class or "code").strip()
        if klass not in {"code", "shell_agent", "verifier", "review"}:
            klass = "code"
        with self._lock:
            self._conn.execute("BEGIN IMMEDIATE")
            try:
                current = self._conn.execute(
                    "SELECT status, worker, dispatch_state FROM intents "
                    "WHERE intent_id=? AND challenge_id=?",
                    (iid, self.challenge.id),
                ).fetchone()
                if current is not None:
                    status = str(current[0] or "")
                    current_owner = str(current[1] or "")
                    dispatch_state = str(current[2] or "active")
                    if (status == "claimed" and current_owner == owner
                            and dispatch_state == INTENT_DISPATCH_ACTIVE):
                        self._conn.execute(
                            "UPDATE intents SET lease_until=NULL "
                            "WHERE intent_id=? AND challenge_id=?",
                            (iid, self.challenge.id),
                        )
                        self._conn.commit()
                        return True
                    if (status != "open"
                            or dispatch_state != INTENT_DISPATCH_ACTIVE):
                        self._conn.commit()
                        return False
                    cur = self._conn.execute(
                        "UPDATE intents SET worker=?, status='claimed', "
                        "lease_until=NULL WHERE intent_id=? AND challenge_id=? "
                        "AND status='open' AND dispatch_state='active'",
                        (owner, iid, self.challenge.id),
                    )
                    if int(cur.rowcount or 0) != 1:
                        self._conn.rollback()
                        return False
                else:
                    proposed_seq = self._append_locked(
                        EV_INTENT_PROPOSED,
                        actor,
                        {
                            "intent_id": iid,
                            "goal": text,
                            "worker_class": klass,
                            "route_hash": "",
                            "branch_id": "",
                            "lane_key": "",
                            "risk_class": "",
                            "resource_key": "",
                            "directive_id": "",
                            "priority": 0,
                            "expected_observable": "",
                            "stop_condition": "",
                            "coverage_key": "",
                        },
                        dedupe_key=f"intent::{iid}",
                    )
                    if proposed_seq < 0:
                        prior = self._conn.execute(
                            "SELECT seq FROM events WHERE challenge_id=? "
                            "AND dedupe_key=?",
                            (self.challenge.id, f"intent::{iid}"),
                        ).fetchone()
                        proposed_seq = int(prior[0] or 0) if prior else 0
                    if proposed_seq <= 0:
                        raise RuntimeError(
                            "owned intent start missing proposal audit event")
                    self._conn.execute(
                        "INSERT INTO intents "
                        "(intent_id, challenge_id, goal, worker_class, status, "
                        " worker, lease_until, dispatch_state, created_seq) "
                        "VALUES (?,?,?,?,'claimed',?,NULL,'active',?)",
                        (iid, self.challenge.id, text, klass, owner, proposed_seq),
                    )
                claimed_seq = self._append_locked(
                    EV_INTENT_CLAIMED, owner, {"intent_id": iid})
                if claimed_seq <= 0:
                    raise RuntimeError(
                        "owned intent start missing claim audit event")
                self._conn.commit()
                return True
            except BaseException:
                self._conn.rollback()
                raise

    def claim_intent(self, *, worker: str, intent_id: str) -> bool:
        """Atomically claim an open Intent for one Worker.

        Ownership changes only through explicit release/reopen transitions.  A
        wall-clock lease no longer authorizes another Worker to overwrite a live
        owner; runtime retirement is the authority that returns work to the queue.
        """
        with self._lock:
            self._conn.execute("BEGIN IMMEDIATE")
            try:
                cur = self._conn.execute(
                    "UPDATE intents SET worker=?, status='claimed', "
                    "lease_until=NULL WHERE intent_id=? AND challenge_id=? "
                    "  AND dispatch_state='active' "
                    "  AND status='open' "
                    "  AND NOT EXISTS ("
                    "    SELECT 1 FROM intent_dependencies d "
                    "    LEFT JOIN intents p ON p.challenge_id=intents.challenge_id "
                    "     AND p.intent_id=d.depends_on_intent_id "
                    "    WHERE d.challenge_id=intents.challenge_id "
                    "     AND d.intent_id=intents.intent_id "
                    "     AND ((p.intent_id IS NULL "
                    "           AND d.depends_on_intent_id GLOB '*[^0-9]*') "
                    "          OR (p.intent_id IS NOT NULL "
                    "              AND (p.status!='done' "
                    "                   OR COALESCE(p.dispatch_state,'active')"
                    "!='closed')))"
                    "  )",
                    (worker, intent_id, self.challenge.id),
                )
                won = int(cur.rowcount or 0) == 1
                if won:
                    event_seq = self._append_locked(
                        EV_INTENT_CLAIMED, worker, {"intent_id": intent_id})
                    if event_seq <= 0:
                        raise RuntimeError(
                            "intent claim missing audit event")
                self._conn.commit()
                return won
            except BaseException:
                self._conn.rollback()
                raise

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
                if released:
                    event_seq = self._append_locked(
                        EV_INTENT_STATE_CHANGED,
                        worker,
                        {
                            "intent_id": intent_id,
                            "status": "open",
                            "reason": str(reason or "claim released")[:500],
                        },
                    )
                    if event_seq <= 0:
                        raise RuntimeError(
                            "intent release missing audit event")
                self._conn.commit()
            except BaseException:
                # A failed commit leaves the uncommitted UPDATE visible through
                # this same long-lived connection.  Retirement's verification read
                # must never mistake that local view for a durable release.
                self._conn.rollback()
                raise
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

    def claimed_intent_successor(
        self, *, worker: str, intent_id: str,
    ) -> dict[str, Any]:
        """Return the highest-priority queued child Step for this exact owner.

        The child remains open and dependency-blocked until the caller commits the
        predecessor checkpoint. This read grants no ownership by itself.
        """
        owner = str(worker or "").strip()
        current_id = str(intent_id or "").strip()
        if not owner or not current_id:
            return {}
        with self._lock:
            current = self._conn.execute(
                "SELECT 1 FROM intents WHERE challenge_id=? AND intent_id=? "
                "AND status='claimed' AND dispatch_state='active' AND worker=?",
                (self.challenge.id, current_id, owner),
            ).fetchone()
            if current is None:
                return {}
            row = self._conn.execute(
                "SELECT i.intent_id, i.goal, i.worker_class, i.route_hash, "
                "i.branch_id, i.lane_key, i.risk_class, i.resource_key, "
                "i.expected_observable, i.stop_condition, i.coverage_key, "
                "i.requires_capabilities_json, i.value_claim_json, "
                "i.required_pocs_json "
                "FROM intents i JOIN intent_dependencies d "
                "ON d.intent_id=i.intent_id AND d.challenge_id=i.challenge_id "
                "WHERE i.challenge_id=? AND d.depends_on_intent_id=? "
                "AND i.status='open' AND i.dispatch_state='active' "
                "ORDER BY i.priority DESC, i.created_seq LIMIT 1",
                (self.challenge.id, current_id),
            ).fetchone()
            if row is None:
                return {}
            source_rows = self._conn.execute(
                "SELECT fact_seq FROM intent_sources WHERE intent_id=? "
                "ORDER BY fact_seq",
                (str(row[0]),),
            ).fetchall()
        try:
            requires_capabilities = list(json.loads(row[11] or "[]"))
        except (TypeError, ValueError, json.JSONDecodeError):
            requires_capabilities = []
        required = {
            str(item).strip().casefold()
            for item in requires_capabilities if str(item).strip()
        }
        try:
            available = (
                self.active_capability_keys() | WORKER_RUNTIME_CAPABILITY_KEYS
            )
        except Exception:
            available = set(WORKER_RUNTIME_CAPABILITY_KEYS)
        if required - available:
            return {}
        required_pocs = list(json.loads(row[13] or "[]"))
        inheritable_pocs = {
            str(poc.get("poc_id") or "")
            for poc in self.pocs()
            if str(poc.get("status") or "") in {"available", "directional", "wip"}
        }
        if set(required_pocs) - inheritable_pocs:
            return {}
        try:
            value_claim = dict(json.loads(row[12] or "{}"))
        except (TypeError, ValueError, json.JSONDecodeError):
            value_claim = {}
        return {
            "intent_id": str(row[0]),
            "goal": str(row[1] or ""),
            "worker_class": str(row[2] or "code"),
            "route_hash": str(row[3] or ""),
            "branch_id": str(row[4] or ""),
            "lane_key": str(row[5] or ""),
            "risk_class": str(row[6] or ""),
            "resource_key": str(row[7] or ""),
            "expected_observable": str(row[8] or ""),
            "stop_condition": str(row[9] or ""),
            "coverage_key": str(row[10] or ""),
            "requires_capabilities": requires_capabilities,
            "required_pocs": required_pocs,
            "value_claim": value_claim,
            "from_facts": [int(source[0]) for source in source_rows],
        }

    def block_intent_context(
        self, *, actor: str, intent_id: str, missing: list[str],
        reason: str = "",
    ) -> bool:
        """Hold an open intent out of dispatch on a context/capability gap.

        The row keeps status='open' for audit, but dispatch_state='blocked'
        drops it out of every dispatchable/claimable query (they whitelist
        'active'), so the coordinator stops re-offering it instead of looping
        spawn/reject; ``reopen_intent`` reverses the block.  The state change
        and its audit event share one transaction.  A repeat while already
        blocked updates no row and therefore writes no duplicate event; a
        later explicit reopen followed by a new block gets a new audit event.
        True only when THIS call performed the transition.
        """
        iid = str(intent_id or "").strip()
        if not iid:
            return False
        caps = [str(m) for m in (missing or []) if str(m or "").strip()][:8]
        detail = str(reason or "")[:500]
        with self._lock:
            self._conn.execute("BEGIN IMMEDIATE")
            try:
                cur = self._conn.execute(
                    "UPDATE intents SET dispatch_state=?, close_reason=? "
                    "WHERE intent_id=? AND challenge_id=? AND status='open' "
                    "AND dispatch_state=?",
                    (INTENT_DISPATCH_BLOCKED, "blocked_context_capability",
                     iid, self.challenge.id, INTENT_DISPATCH_ACTIVE),
                )
                transitioned = int(cur.rowcount or 0) == 1
                if transitioned:
                    event_seq = self._append_locked(
                        EV_INTENT_STATE_CHANGED, actor,
                        {"intent_id": iid, "status": "open",
                         "dispatch_state": INTENT_DISPATCH_BLOCKED,
                         "missing": caps,
                         "reason": detail or "blocked_context_capability"},
                    )
                    if event_seq <= 0:
                        raise RuntimeError(
                            "blocked intent transition missing audit event")
                self._conn.commit()
            except BaseException:
                self._conn.rollback()
                raise
        return transitioned

    def reopen_intent(self, *, actor: str, intent_id: str, reason: str = "") -> bool:
        """Return a concluded OR context-blocked intent to the open dispatch queue.

        A blocked row (dispatch_state='blocked', close_reason set by
        block_intent_context) is reset to active with the close marker cleared —
        same reopen idiom as reopen_route — so an explicit operator retry after
        repairing the context/capability gap makes the intent dispatchable again."""
        iid = str(intent_id or "").strip()
        if not iid:
            return False
        with self._lock:
            cur = self._conn.execute(
                "UPDATE intents SET status='open', dispatch_state='active', "
                "close_reason=NULL, worker=NULL, lease_until=NULL "
                "WHERE intent_id=? AND challenge_id=? "
                "AND (status='done' OR dispatch_state='blocked')",
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
        append a terminal event or clobber the fresh claim. Its WorkerResult remains
        the audit record. Exceptions that always win: a 'solved' conclusion (a real
        flag ends the run regardless), and actor 'coordinator' (legacy/admin path)."""
        with self._lock:
            try:
                self._conn.execute("BEGIN IMMEDIATE")
                seq, _applied = self._conclude_intent_locked(
                    actor=actor, intent_id=intent_id, result=result,
                    to_fact_seq=to_fact_seq, result_detail=result_detail,
                )
            except BaseException:
                self._conn.rollback()
                raise
            self._conn.commit()
            return seq

    def _conclude_intent_locked(self, *, actor: str, intent_id: str,
                                result: str = "",
                                to_fact_seq: Optional[int] = None,
                                result_detail: str = "") -> tuple[int, bool]:
        """``conclude_intent`` body with ``self._lock`` held and no commit
        (transactional reuse from ``commit_worker_result``). The owner check runs
        before the terminal event; PoCs are spent only after the fenced row update
        succeeds."""
        current = self._conn.execute(
            "SELECT status, worker FROM intents "
            "WHERE intent_id=? AND challenge_id=?",
            (intent_id, self.challenge.id),
        ).fetchone()
        if current is None or str(current[0] or "") == "done":
            return 0, False
        current_owner = str(current[1] or "")
        if (result != "solved" and actor != "coordinator"
                and current_owner not in {"", actor}):
            return 0, False

        detail = str(result_detail or "")
        payload = {"intent_id": intent_id, "result": result}
        if detail:
            payload["result_detail"] = detail
        if to_fact_seq is not None:
            payload["to_fact_seq"] = to_fact_seq
        seq = self._append_locked(EV_INTENT_CONCLUDED, actor, payload)
        if seq <= 0:
            raise RuntimeError("intent conclusion missing audit event")
        # owner fence: only the current owner (or coordinator, or a solved result)
        # may flip the row to done. worker IS NULL handles never-claimed intents
        # some paths conclude as a no-op.
        fence = "" if (result == "solved" or actor == "coordinator") else (
            " AND (worker=? OR worker IS NULL)")
        # A/J: a concluded intent also leaves the dispatch pool (closed), with the
        # conclusion text as its close_reason — distinguishes it from resume/retired.
        close_reason = (result or "concluded").strip()[:200]
        if to_fact_seq is not None:
            sql = ("UPDATE intents SET status='done', dispatch_state='closed', "
                   "close_reason=?, result_seq=?, result_detail=?, to_fact_seq=? "
                   "WHERE intent_id=? AND challenge_id=? AND status!='done'" + fence)
            params: list = [close_reason, seq if seq > 0 else None, detail or None,
                            to_fact_seq,
                            intent_id, self.challenge.id]
        else:
            sql = ("UPDATE intents SET status='done', dispatch_state='closed', "
                   "close_reason=?, result_seq=?, result_detail=? "
                   "WHERE intent_id=? AND challenge_id=? AND status!='done'" + fence)
            params = [close_reason, seq if seq > 0 else None, detail or None, intent_id,
                      self.challenge.id]
        if fence:
            params.append(actor)
        cur = self._conn.execute(sql, tuple(params))
        applied = int(cur.rowcount or 0) == 1
        if applied and self._intent_result_marks_poc_spent(result):
            self._conn.execute(
                "UPDATE pocs SET status='spent', result_seq=? "
                "WHERE challenge_id=? AND intent_id=? "
                "AND status IN ('available','wip','directional')",
                (seq if seq > 0 else None, self.challenge.id, intent_id),
            )
        if applied:
            self._record_value_receipt_locked(
                actor=actor, intent_id=intent_id, result=result,
                conclusion_seq=seq,
            )
        return seq, applied

    def _record_value_receipt_locked(
        self, *, actor: str, intent_id: str, result: str, conclusion_seq: int,
    ) -> int:
        row = self._conn.execute(
            "SELECT value_claim_json,requested_priority,priority,created_seq FROM intents "
            "WHERE challenge_id=? AND intent_id=?",
            (self.challenge.id, intent_id),
        ).fetchone()
        if not row:
            return -1
        try:
            claim = json.loads(row[0] or "{}")
        except (TypeError, ValueError, json.JSONDecodeError):
            claim = {}
        if not isinstance(claim, dict) or not claim:
            return -1
        after = {
            str(item).strip().casefold()
            for item in claim.get("capability_after", [])
            if str(item).strip()
        }
        epoch = str(claim.get("target_epoch") or "")
        args: list[Any] = [self.challenge.id]
        sql = ("SELECT capability_key FROM capabilities WHERE challenge_id=? "
               "AND state='active'")
        if epoch:
            sql += " AND target_epoch=?"
            args.append(epoch)
        present = {
            str(r[0]) for r in self._conn.execute(sql, tuple(args)).fetchall()
        }
        effect = str(claim.get("effect") or "")
        achieved = bool(
            (effect == "terminal" and result == "solved")
            or (after and after.issubset(present))
            or (effect == "branch_resolution" and result not in {"", "failed", "error"})
        )
        created = self._conn.execute(
            "SELECT ts FROM events WHERE seq=?", (int(row[3] or 0),)
        ).fetchone()
        elapsed = max(0.0, time.time() - float(created[0])) if created else None
        receipt_id = f"VR-{hashlib.sha256(intent_id.encode()).hexdigest()[:16]}"
        payload = {"receipt_id": receipt_id, "intent_id": intent_id,
                   "effect": effect, "requested_priority": row[1] or "",
                   "effective_priority": int(row[2] or 0), "achieved": achieved,
                   "capabilities_added": sorted(after & present),
                   "unblocked_count": len(claim.get("unblocks") or []),
                   "elapsed_seconds": elapsed, "result": result}
        receipt_seq = self._append_locked(
            EV_VALUE_RECEIPT, actor, payload,
            dedupe_key=f"value-receipt::{self.challenge.id}::{intent_id}",
        )
        if receipt_seq < 0:
            return -1
        self._conn.execute(
            "INSERT OR IGNORE INTO value_receipts (receipt_id,challenge_id,intent_id,effect,"
            "requested_priority,effective_priority,achieved,capabilities_added_json,"
            "unblocked_count,elapsed_seconds,detail,created_seq) VALUES (?,?,?,?,?,?,?,?,?,?,?,?)",
            (receipt_id, self.challenge.id, intent_id, effect, row[1] or "",
             int(row[2] or 0), int(achieved), json.dumps(sorted(after & present)),
             len(claim.get("unblocks") or []), elapsed, str(result or ""), receipt_seq),
        )
        return receipt_seq

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
        with self._lock:
            try:
                seq = self._save_poc_locked(
                    actor=actor, poc_id=poc_id, path=path,
                    entry_command=entry_command, status=status, note=note,
                    artifact_id=artifact_id, intent_id=intent_id, name=name,
                )
            except BaseException:
                self._conn.rollback()
                raise
            self._conn.commit()
            return seq

    def _save_poc_locked(self, *, actor: str, poc_id: str, path: str,
                         entry_command: str, status: str = "available",
                         note: str = "", artifact_id: Optional[str] = None,
                         intent_id: Optional[str] = None, name: str = "") -> int:
        """``save_poc`` body with ``self._lock`` held and no commit
        (transactional reuse from ``commit_worker_result``)."""
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
        seq = self._append_locked(EV_POC_SAVED, actor, payload,
                                  artifact_id=artifact_id,
                                  dedupe_key=f"poc::{poc_id}::{status}::{entry_command}::{note}")
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

    def commit_worker_result(self, *, actor: str, worker_id: str,
                             intent_id: Optional[str], target_epoch: str = "",
                             run_id: str = "", status: str,
                             checkpoint_id: str = "",
                             result_detail: str = "",
                             produced_new_info: bool = False,
                             stop_condition_result: str = "",
                             handoff_missing: bool = False,
                             observations: Optional[list[dict]] = None,
                             dead_ends: Optional[list[dict]] = None,
                             pocs: Optional[list[dict]] = None,
                             artifacts: Optional[list[str]] = None,
                             need_input: Optional[dict] = None,
                             conclude: bool = True,
                             successor_intent_id: str = "") -> dict:
        """Atomically commit one worker's result: observations (each through the
        existing provenance gate, so verified-vs-candidate and canonical folding
        are unchanged), dead ends, PoCs, one audit event, and — LAST — the intent
        conclusion. ONE lock, ONE commit; any exception rolls everything back.

        Observations: {text, claim_verified, provenance}. Dead ends: {reason,
        tested_scope, observed_result, route_hash, coverage_key}. PoCs: {poc_id,
        path, entry_command, status,
        note, artifact_id, name}. need_input is NOT written here (HITL requests
        already flow live); it is only summarized into the audit event.
        """
        iid = (intent_id or "").strip()
        next_iid = str(successor_intent_id or "").strip()
        # handoff_missing always wins over the caller-passed status.
        final_result = "handoff_missing" if handoff_missing else (status or "")
        detail = (result_detail or "").strip()
        obs_list = [o for o in (observations or []) if isinstance(o, dict)]
        dead_list = [d for d in (dead_ends or []) if isinstance(d, dict)]
        rejected_dead_ends: list[dict[str, str]] = []
        if getattr(self.challenge, "mode", "ctf") == "ctf":
            bounded_dead_ends: list[dict] = []
            for dead in dead_list:
                reason = str(dead.get("reason") or "").strip()
                tested_scope = str(dead.get("tested_scope") or "").strip()
                observed_result = str(dead.get("observed_result") or "").strip()
                if reason and tested_scope and observed_result:
                    bounded_dead_ends.append(dead)
                    continue
                rejected_dead_ends.append({
                    "reason": reason[:300],
                    "missing": ",".join(
                        name for name, value in (
                            ("reason", reason),
                            ("tested_scope", tested_scope),
                            ("observed_result", observed_result),
                        ) if not value
                    ),
                })
            dead_list = bounded_dead_ends
        poc_list = [p for p in (pocs or []) if isinstance(p, dict)]
        artifact_refs = [str(a).strip() for a in (artifacts or []) if str(a).strip()]
        checkpoint = str(checkpoint_id or "").strip()
        commit_identity = json.dumps(
            [self.challenge.id, run_id or "", worker_id, iid,
             target_epoch or "", checkpoint],
            ensure_ascii=False, separators=(",", ":"),
        )
        commit_id = hashlib.sha256(
            commit_identity.encode("utf-8")
        ).hexdigest()
        commit_key = f"worker-result::{commit_id}"

        def _prior_receipt_locked() -> Optional[dict]:
            row = self._conn.execute(
                "SELECT seq, payload FROM events WHERE challenge_id=? "
                "AND dedupe_key=? AND kind=? LIMIT 1",
                (self.challenge.id, commit_key, EV_WORKER_RESULT_COMMITTED),
            ).fetchone()
            if row is None:
                return None
            try:
                prior = json.loads(str(row[1] or "{}"))
            except (TypeError, ValueError, json.JSONDecodeError):
                prior = {}
            facts = [int(x) for x in prior.get("verified_fact_seqs", [])]
            candidates: list[int] = []
            observation_ids = [int(x) for x in prior.get("observation_seqs", [])]
            dead_end_ids = [int(x) for x in prior.get("dead_end_seqs", [])]
            poc_ids = [int(x) for x in prior.get("poc_seqs", [])]
            conclude_seq: Optional[int] = None
            concluded = False
            if iid:
                state = self._conn.execute(
                    "SELECT i.status, i.result_seq, e.actor FROM intents i "
                    "LEFT JOIN events e ON e.seq=i.result_seq "
                    "WHERE i.intent_id=? AND i.challenge_id=?",
                    (iid, self.challenge.id),
                ).fetchone()
                if (state is not None and str(state[0] or "") == "done"
                        and str(state[2] or "") == str(actor or "")):
                    concluded = True
                    conclude_seq = (
                        int(state[1]) if state[1] is not None else None
                    )
            return {
                "seqs": {
                    "facts": facts,
                    "candidates": candidates,
                    "observations": observation_ids,
                    "dead_ends": dead_end_ids,
                    "pocs": poc_ids,
                    "summary": int(row[0]),
                    "concluded": conclude_seq,
                },
                "facts": facts,
                "candidates": candidates,
                "dead_ends": dead_end_ids,
                "dead_end_rejections": list(
                    prior.get("dead_end_rejections") or []),
                "pocs": poc_ids,
                "concluded": concluded,
                "result": str(prior.get("final_result") or prior.get("status") or ""),
                "successor_intent_id": str(
                    prior.get("successor_intent_id") or ""),
                "successor_claimed": bool(prior.get("successor_claimed")),
            }

        if handoff_missing:
            # The conclusion detail must name the artifact references this commit
            # carried, so the evidence trail survives the missing handoff.
            refs: list[str] = []
            refs.extend(f"artifact:{aid}" for aid in artifact_refs)
            for obs in obs_list:
                prov = obs.get("provenance")
                aid = (str(prov.get("artifact_id") or "")
                       if isinstance(prov, dict) else "")
                if aid:
                    refs.append(f"artifact:{aid}")
            for poc in poc_list:
                label = str(poc.get("poc_id") or "") or str(poc.get("path") or "")
                if not label:
                    continue
                ref = f"poc:{label}"
                aid = str(poc.get("artifact_id") or "")
                if aid:
                    ref += f" artifact:{aid}"
                refs.append(ref)
            if refs:
                refs_line = "evidence_refs: " + ", ".join(refs)
                detail = f"{detail}\n{refs_line}" if detail else refs_line
        with self._lock:
            try:
                # Acquire the SQLite writer boundary before checking idempotency
                # or reading the current Intent owner. This makes the receipt,
                # evidence writes, and owner-fenced conclusion one serializable
                # transaction even when another graph connection is active.
                self._conn.execute("BEGIN IMMEDIATE")
                prior_receipt = _prior_receipt_locked()
                if prior_receipt is not None:
                    self._conn.rollback()
                    return prior_receipt
                fact_seqs: list[int] = []
                candidate_seqs: list[int] = []
                observation_seqs: list[int] = []
                for obs in obs_list:
                    text = str(obs.get("text") or "")
                    if not text.strip():
                        continue
                    provenance = dict(obs.get("provenance") or {})
                    provenance["committed_at"] = time.time()
                    artifact_id = str(provenance.get("artifact_id") or "") or None
                    seq = self._add_evidence_locked(
                        actor=worker_id, source="worker_result", fact=text,
                        artifact_id=artifact_id,
                        verified=bool(obs.get("claim_verified")),
                        intent_id=iid or None, provenance=provenance,
                        subject=str(obs.get("subject") or ""),
                        predicate=str(obs.get("predicate") or ""),
                        object_value=obs.get("object"),
                        scope=str(obs.get("scope") or ""),
                        canonical_key=str(obs.get("canonical_key") or ""),
                    )
                    observed_seq = int(getattr(self, "_last_observation_seq", 0) or 0)
                    if observed_seq > 0 and observed_seq not in observation_seqs:
                        observation_seqs.append(observed_seq)
                    if seq <= 0:
                        continue
                    fact_seqs.append(int(seq))
                dead_end_seqs: list[int] = []
                for dead in dead_list:
                    reason = str(dead.get("reason") or "").strip()
                    if not reason:
                        continue
                    seq = self._add_dead_end_locked(
                        actor=worker_id, reason=reason, intent_id=iid,
                        route_hash=str(dead.get("route_hash") or ""),
                        coverage_key=str(dead.get("coverage_key") or ""),
                        target_epoch=target_epoch,
                        tested_scope=str(dead.get("tested_scope") or ""),
                        observed_result=str(dead.get("observed_result") or ""),
                    )
                    if seq > 0:
                        dead_end_seqs.append(int(seq))
                poc_seqs: list[int] = []
                for poc in poc_list:
                    poc_id = str(poc.get("poc_id") or "").strip()
                    path = str(poc.get("path") or "").strip()
                    if not poc_id or not path:
                        continue
                    seq = self._save_poc_locked(
                        actor=worker_id, poc_id=poc_id, path=path,
                        entry_command=str(poc.get("entry_command") or ""),
                        status=str(poc.get("status") or "available"),
                        note=str(poc.get("note") or ""),
                        artifact_id=(str(poc["artifact_id"])
                                     if poc.get("artifact_id") else None),
                        intent_id=iid or None, name=str(poc.get("name") or ""),
                    )
                    if seq > 0:
                        poc_seqs.append(int(seq))
                payload = {
                    "commit_id": commit_id,
                    "worker_id": worker_id,
                    "intent_id": iid,
                    "target_epoch": target_epoch or "",
                    "run_id": run_id or "",
                    "checkpoint_id": checkpoint,
                    "status": status or "",
                    "result_detail": detail,
                    "produced_new_info": bool(produced_new_info),
                    "stop_condition_result": stop_condition_result or "",
                    "handoff_missing": bool(handoff_missing),
                    "final_result": final_result,
                    "counts": {
                        "observations": len(observation_seqs),
                        "verified": len(fact_seqs),
                        "candidates": len(candidate_seqs),
                        "dead_ends": len(dead_end_seqs),
                        "dead_ends_rejected": len(rejected_dead_ends),
                        "pocs": len(poc_seqs),
                    },
                    "fact_seqs": fact_seqs,
                    "verified_fact_seqs": fact_seqs,
                    "candidate_fact_seqs": candidate_seqs,
                    "observation_seqs": observation_seqs,
                    "dead_end_seqs": dead_end_seqs,
                    "dead_end_rejections": rejected_dead_ends[:20],
                    "poc_seqs": poc_seqs,
                    "artifact_ids": artifact_refs,
                    "successor_intent_id": next_iid,
                    "successor_claimed": False,
                }
                if need_input:
                    payload["need_input"] = need_input
                summary_seq = self._append_locked(
                    EV_WORKER_RESULT_COMMITTED, actor, payload,
                    dedupe_key=commit_key)
                if summary_seq < 0:
                    # Another graph owner committed this exact Worker result
                    # between our initial read and insert. Discard this whole
                    # duplicate transaction, then return the durable receipt.
                    self._conn.rollback()
                    prior_receipt = _prior_receipt_locked()
                    if prior_receipt is None:
                        raise RuntimeError(
                            "worker-result dedupe collision without durable receipt")
                    return prior_receipt
                concluded = False
                conclude_seq: Optional[int] = None
                successor_claimed = False
                if conclude and iid:
                    # Conclusion is LAST in the transaction: facts/dead-ends/pocs
                    # must already be written when the intent closes.
                    primary_fact_seq = (
                        fact_seqs[-1] if fact_seqs
                        else None
                    )
                    conclude_seq, concluded = self._conclude_intent_locked(
                        actor=actor, intent_id=iid, result=final_result,
                        to_fact_seq=primary_fact_seq,
                        result_detail=detail)
                if next_iid:
                    if not concluded:
                        raise RuntimeError(
                            "successor claim requires a concluded predecessor")
                    cur = self._conn.execute(
                        "UPDATE intents SET worker=?, status='claimed', "
                        "lease_until=NULL WHERE intent_id=? AND challenge_id=? "
                        "AND status='open' AND dispatch_state='active' "
                        "AND EXISTS (SELECT 1 FROM intent_dependencies d "
                        "WHERE d.challenge_id=? AND d.intent_id=? "
                        "AND d.depends_on_intent_id=?)",
                        (worker_id, next_iid, self.challenge.id,
                         self.challenge.id, next_iid, iid),
                    )
                    successor_claimed = int(cur.rowcount or 0) == 1
                    if not successor_claimed:
                        raise RuntimeError(
                            "planned successor was not claimable at checkpoint")
                    claimed_seq = self._append_locked(
                        EV_INTENT_CLAIMED, worker_id,
                        {"intent_id": next_iid, "predecessor_intent_id": iid},
                    )
                    if claimed_seq <= 0:
                        raise RuntimeError(
                            "successor claim missing audit event")
                    self._conn.execute(
                        "UPDATE lane_locks SET owner_intent=? "
                        "WHERE challenge_id=? AND owner_worker=? "
                        "AND owner_intent=?",
                        (next_iid, self.challenge.id, worker_id, iid),
                    )
                    self._conn.execute(
                        "UPDATE resource_locks SET owner_intent=? "
                        "WHERE challenge_id=? AND owner_worker=? "
                        "AND owner_intent=?",
                        (next_iid, self.challenge.id, worker_id, iid),
                    )
                    payload["successor_claimed"] = True
                    self._conn.execute(
                        "UPDATE events SET payload=? WHERE seq=?",
                        (json.dumps(payload, default=str), summary_seq),
                    )
            except BaseException:
                self._conn.rollback()
                raise
            self._conn.commit()
        return {
            "seqs": {
                "facts": fact_seqs,
                "candidates": candidate_seqs,
                "observations": observation_seqs,
                "dead_ends": dead_end_seqs,
                "pocs": poc_seqs,
                "summary": int(summary_seq),
                "concluded": (int(conclude_seq)
                              if conclude_seq is not None else None),
            },
            "facts": fact_seqs,
            "candidates": candidate_seqs,
            "observations": observation_seqs,
            "dead_ends": dead_end_seqs,
            "dead_end_rejections": rejected_dead_ends,
            "pocs": poc_seqs,
            "concluded": concluded,
            "result": final_result,
            "successor_intent_id": next_iid,
            "successor_claimed": successor_claimed,
        }

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
                               reason: str = "",
                               exclude_ids: Optional[list[str]] = None) -> list[str]:
        """Retire every OPEN intent whose goal contains the
        `match` substring (case-insensitive) — they've been made obsolete by an
        operator action. run-11190: a worker proposes "Request the operator for the
        L2 SSH password", the operator then SUPPLIES it as a standing hint, but the
        old "ask for the password" intents stayed status='open' forever, so fresh
        explore workers kept claiming them and re-asking for a password they already
        had → 238-worker dead loop. Flipping them to status='done' (result=
        'superseded') stops _open_intents from re-dispatching them. Returns the list
        of superseded intent_ids (for a blackboard delta). Only OPEN rows are
        touched; a claimed Intent remains under its active owner's control.
        ``exclude_ids`` keeps operator-assigned steps (I-D-*) from being
        retired by a later hint that happens to share a substring.

        #11: a marker EV_INTENT_CONCLUDED event with result='superseded' is appended
        and its seq stored in each row's result_seq, so the rows are DISTINGUISHABLE
        from a genuinely solved 'done' intent. reopen_after_false_positive uses that
        result text to reopen ONLY solved intents and leave these superseded asks
        retired (run-11190 regression)."""
        blocked = {
            str(item).strip()
            for item in (exclude_ids or [])
            if str(item).strip()
        }
        like = f"%{match.lower()}%"
        with self._lock:
            rows = self._conn.execute(
                "SELECT intent_id FROM intents WHERE challenge_id=? "
                "  AND status='open' "
                "  AND lower(goal) LIKE ?",
                (self.challenge.id, like),
            ).fetchall()
            ids = [r[0] for r in rows if r[0] not in blocked]
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

    def supersede_open_intent_ids(self, *, actor: str,
                                  intent_ids: list[str],
                                  reason: str = "") -> list[str]:
        """Close exact open or claimed intents explicitly dropped by Decide.

        The coordinator stops a matching live Worker after this transaction.
        Exact ids keep this narrower than the substring-based operator helper.
        """
        requested: list[str] = []
        for raw in intent_ids:
            intent_id = str(raw or "").strip()
            if intent_id and intent_id not in requested:
                requested.append(intent_id)
        if not requested:
            return []
        with self._lock:
            self._conn.execute("BEGIN IMMEDIATE")
            try:
                qmarks = ",".join("?" for _ in requested)
                rows = self._conn.execute(
                    f"SELECT intent_id FROM intents WHERE challenge_id=? "
                    f"AND status IN ('open','claimed') AND dispatch_state='active' "
                    f"AND intent_id IN ({qmarks})",
                    (self.challenge.id, *requested),
                ).fetchall()
                accepted = [str(row[0]) for row in rows]
                if not accepted:
                    self._conn.rollback()
                    return []
                marker_seq = self._append_locked(
                    EV_INTENT_CONCLUDED,
                    actor,
                    {
                        "intent_id": ",".join(accepted),
                        "intent_ids": accepted,
                        "result": "superseded",
                        "reason": str(reason or "new evidence superseded queued work")[:1000],
                    },
                )
                accepted_marks = ",".join("?" for _ in accepted)
                self._conn.execute(
                    f"UPDATE intents SET status='done', dispatch_state='closed', "
                    f"close_reason='superseded', result_seq=?, result_detail=? "
                    f"WHERE challenge_id=? AND status IN ('open','claimed') "
                    f"AND dispatch_state='active' "
                    f"AND intent_id IN ({accepted_marks})",
                    (
                        marker_seq if marker_seq > 0 else None,
                        str(reason or "")[:1000] or None,
                        self.challenge.id,
                        *accepted,
                    ),
                )
                self._conn.commit()
            except BaseException:
                self._conn.rollback()
                raise
        return accepted

    def reprioritize_open_intents(
        self, *, actor: str, priorities: dict[str, str],
    ) -> list[str]:
        """Apply one Decide pass's priority changes to queued active Steps."""
        values = {"high": 50, "normal": 0, "low": -10}
        requested = {
            str(intent_id or "").strip(): values[str(priority).strip().lower()]
            for intent_id, priority in dict(priorities or {}).items()
            if (
                str(intent_id or "").strip()
                and str(priority).strip().lower() in values
            )
        }
        if not requested:
            return []
        active_fact_seqs = self._active_fact_seq_set()
        active_capability_keys = (
            self.active_capability_keys() | WORKER_RUNTIME_CAPABILITY_KEYS
        )
        changed: list[str] = []
        effective_priorities: dict[str, int] = {}
        with self._lock:
            self._conn.execute("BEGIN IMMEDIATE")
            try:
                for intent_id, priority in requested.items():
                    requested_name = next(
                        (name for name, value in values.items()
                         if value == requested[intent_id]), "normal")
                    reason = "decide_reprioritized"
                    row = self._conn.execute(
                        "SELECT value_claim_json,priority,requested_priority,priority_reason,"
                        "requires_capabilities_json "
                        "FROM intents WHERE challenge_id=? AND intent_id=?",
                        (self.challenge.id, intent_id),
                    ).fetchone()
                    if priority == values["high"]:
                        try:
                            claim = json.loads(row[0] or "{}") if row else {}
                        except (TypeError, ValueError, json.JSONDecodeError):
                            claim = {}
                        sources = {
                            int(r[0]) for r in self._conn.execute(
                                "SELECT fact_seq FROM intent_sources WHERE intent_id=?",
                                (intent_id,),
                            ).fetchall()
                        }
                        active_sources = sources & active_fact_seqs
                        effect = str(claim.get("effect") or "")
                        novelty = str(claim.get("novelty_key") or "").strip()
                        after = {
                            str(item).strip().casefold()
                            for item in claim.get("capability_after", [])
                            if str(item).strip()
                        }
                        consumers = {
                            str(item).strip()
                            for key in ("unblocks", "consumer_capabilities")
                            for item in claim.get(key, []) if str(item).strip()
                        }
                        try:
                            required = {
                                str(item).strip().casefold()
                                for item in json.loads(row[4] or "[]")
                                if str(item).strip()
                            }
                        except (AttributeError, IndexError, TypeError, ValueError,
                                json.JSONDecodeError):
                            required = set()
                        valid_high = bool(
                            active_sources and novelty
                            and not (required - active_capability_keys)
                            and effect in {"terminal", "capability_advance", "shared_enablement"}
                            and (effect == "terminal" or (
                                after and not after.issubset(active_capability_keys)))
                            and (effect != "shared_enablement" or len(consumers) >= 2)
                        )
                        if not valid_high:
                            priority = values["normal"]
                            reason = "high_rejected_by_value_claim"
                    if (row and int(row[1] or 0) == priority
                            and str(row[2] or "") == requested_name
                            and str(row[3] or "") == reason):
                        continue
                    cur = self._conn.execute(
                        "UPDATE intents SET priority=?,requested_priority=?,priority_reason=? "
                        "WHERE challenge_id=? "
                        "AND intent_id=? AND status='open' AND dispatch_state='active'",
                        (priority, requested_name, reason,
                         self.challenge.id, intent_id),
                    )
                    if int(cur.rowcount or 0) == 1:
                        changed.append(intent_id)
                        effective_priorities[intent_id] = priority
                if changed:
                    seq = self._append_locked(
                        EV_INTENT_STATE_CHANGED,
                        actor,
                        {
                            "intent_ids": changed,
                            "priorities": effective_priorities,
                            "reason": "decide_reprioritized",
                        },
                    )
                    if seq <= 0:
                        raise RuntimeError("priority update missing audit event")
                self._conn.commit()
            except BaseException:
                self._conn.rollback()
                raise
        return changed
