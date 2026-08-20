"""Routes, branches, coordinator/operator directives, and HITL requests.

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
    EV_OPERATOR_DIRECTIVE, EV_OPERATOR_DIRECTIVE_STATUS,
    EV_CONTROL_STANDING_CLEAR_APPLIED, EV_HITL_CLASSIFIED,
    EV_RESOURCE_LOCKED, EV_RESOURCE_RELEASED, EV_GRAPH_COMPACTED,
    FACT_STATE_UNRESOLVED, FACT_STATE_CHALLENGED, FACT_STATE_REVALIDATED,
    FACT_STATE_REJECTED, FACT_STATE_MERGED, FACT_STATE_SUPERSEDED,
    _FACT_TERMINAL_STATES, _FACT_STATES,
    INTENT_DISPATCH_ACTIVE, INTENT_DISPATCH_RESUME, INTENT_DISPATCH_RETIRED,
    INTENT_DISPATCH_CLOSED, _INTENT_DISPATCH_STATES,
    _SERVICE_DEFAULT_PORTS, _LANE_RISK_CLASSES, _FACT_ENGINE_PREFIX_RE,
    _normalize_fact_identity, _clean_lane_risk, _clean_lane_host, canonicalize_lane,
)


class _RoutesDirectivesMixin:
    def suppress_route(self, *, actor: str, route_hash: str, label: str = "",
                       reason: str = "", until: str = "new_evidence",
                       matching_intents: Optional[list[str]] = None) -> dict:
        route = self.normalize_route_hash(route_hash, label=label)
        clean_label = (label or route).strip()
        payload = {
            "route_hash": route,
            "label": clean_label,
            "reason": (reason or "").strip()[:1000],
            "until": (until or "new_evidence").strip(),
            "matching_intents": [str(i) for i in (matching_intents or []) if i],
            "suppressed_by": actor,
        }
        seq = self._append(EV_ROUTE_SUPPRESSED, actor, payload,
                           dedupe_key=f"route-suppressed::{route}::{payload['reason']}")
        superseded: list[str] = []
        with self._lock:
            self._conn.execute(
                "INSERT INTO routes "
                "(route_hash, challenge_id, label, status, suppressed_seq, reason, until_policy) "
                "VALUES (?,?,?,?,?,?,?) "
                "ON CONFLICT(route_hash) DO UPDATE SET "
                " status='suppressed', suppressed_seq=excluded.suppressed_seq, "
                " label=excluded.label, reason=excluded.reason, until_policy=excluded.until_policy",
                (route, self.challenge.id, clean_label, "suppressed",
                 seq if seq > 0 else None, payload["reason"], payload["until"]),
            )
            where = ["challenge_id=? AND status='open' AND worker IS NULL"]
            params: list[Any] = [self.challenge.id]
            if payload["matching_intents"]:
                q = ",".join("?" for _ in payload["matching_intents"])
                where.append(f"intent_id IN ({q})")
                params.extend(payload["matching_intents"])
            else:
                where.append("route_hash=?")
                params.append(route)
            rows = self._conn.execute(
                "SELECT intent_id FROM intents WHERE " + " AND ".join(where),
                tuple(params),
            ).fetchall()
            superseded = [str(r[0]) for r in rows]
        marker_seq = 0
        if superseded:
            marker_seq = self._append(
                EV_INTENT_CONCLUDED, actor,
                {"intent_id": ",".join(superseded),
                 "result": "route_suppressed", "route_hash": route})
            with self._lock:
                q = ",".join("?" for _ in superseded)
                # 刀5: also flip dispatch_state='closed' (not just status='done'),
                # else these land in a done/active limbo the compactor's
                # done/closed filter can never reach (and reopen_route restores
                # them to open/active). close_reason records the cause.
                self._conn.execute(
                    f"UPDATE intents SET status='done', dispatch_state='closed', "
                    f"close_reason='route_suppressed', result_seq=? "
                    f"WHERE challenge_id=? AND intent_id IN ({q})",
                    (marker_seq if marker_seq > 0 else None, self.challenge.id, *superseded),
                )
                self._conn.commit()
            # mirror the dispatch transition so the deck dims them immediately.
            self._append(
                EV_INTENT_STATE_CHANGED, actor,
                {"intent_id": ",".join(superseded),
                 "dispatch_state": INTENT_DISPATCH_CLOSED,
                 "close_reason": "route_suppressed", "route_hash": route})
        else:
            with self._lock:
                self._conn.commit()
        return {"route_hash": route, "seq": seq, "superseded": superseded}

    def reopen_route(self, *, actor: str, route_hash: str, reason: str = "",
                     intent_goal: str = "") -> dict:
        route = self.normalize_route_hash(route_hash)
        payload = {
            "route_hash": route,
            "reason": (reason or "").strip()[:1000],
            "reopened_by": actor,
        }
        seq = self._append(EV_ROUTE_REOPENED, actor, payload,
                           dedupe_key=f"route-reopened::{route}::{payload['reason']}")
        with self._lock:
            self._conn.execute(
                "INSERT INTO routes "
                "(route_hash, challenge_id, label, status, reopened_seq, reason) "
                "VALUES (?,?,?,?,?,?) "
                "ON CONFLICT(route_hash) DO UPDATE SET "
                " status='open', reopened_seq=excluded.reopened_seq, reason=excluded.reason",
                (route, self.challenge.id, route, "open",
                 seq if seq > 0 else None, payload["reason"]),
            )
            rows = self._conn.execute(
                "SELECT i.intent_id FROM intents i "
                "LEFT JOIN events e ON e.seq = i.result_seq "
                "WHERE i.challenge_id=? AND i.route_hash=? AND i.status='done' "
                "AND json_extract(e.payload,'$.result')='route_suppressed' "
                "ORDER BY i.created_seq",
                (self.challenge.id, route),
            ).fetchall()
            reopened = [str(r[0]) for r in rows]
            if reopened:
                q = ",".join("?" for _ in reopened)
                self._conn.execute(
                    f"UPDATE intents SET status='open', dispatch_state='active', "
                    f"close_reason=NULL, worker=NULL, lease_until=NULL, "
                    f"result_seq=NULL, to_fact_seq=NULL WHERE challenge_id=? "
                    f"AND intent_id IN ({q})",
                    (self.challenge.id, *reopened),
                )
            self._conn.commit()
        intent_id = ""
        if intent_goal:
            h = hashlib.sha1(f"{route}:{intent_goal}".encode("utf-8", "ignore")).hexdigest()[:8]
            intent_id = f"I-reopen-{h}"
            self.propose_intent(
                actor=actor, intent_id=intent_id, goal=intent_goal,
                payload={"worker_class": "code", "route_hash": route,
                         "rationale": f"Route reopened by review: {reason}"},
            )
        return {"route_hash": route, "seq": seq, "intent_id": intent_id,
                "reopened": reopened}

    def split_branch(self, *, actor: str, title: str,
                     branches: list[dict[str, Any]]) -> dict:
        parent = f"branch-{hashlib.sha1((title or str(time.time())).encode()).hexdigest()[:10]}"
        payload = {"branch_id": parent, "title": title, "branches": branches}
        seq = self._append(EV_BRANCH_SPLIT, actor, payload,
                           dedupe_key=f"branch-split::{title}::{len(branches)}")
        with self._lock:
            for raw in branches:
                bid = str(raw.get("id") or "").strip() or (
                    f"{parent}-{hashlib.sha1(str(raw).encode()).hexdigest()[:6]}")
                self._conn.execute(
                    "INSERT INTO branches "
                    "(branch_id, challenge_id, parent_id, title, assumption, "
                    " prove_or_disprove, status, created_seq) "
                    "VALUES (?,?,?,?,?,?,?,?) "
                    "ON CONFLICT(branch_id) DO UPDATE SET "
                    " title=excluded.title, assumption=excluded.assumption, "
                    " prove_or_disprove=excluded.prove_or_disprove, status='open'",
                    (bid, self.challenge.id, parent, title,
                     str(raw.get("assumption") or "").strip(),
                     str(raw.get("prove_or_disprove") or "").strip(),
                     "open", seq if seq > 0 else 0),
                )
            self._conn.commit()
        return {"branch_id": parent, "seq": seq}

    def resolve_branch(self, *, actor: str, branch_id: str, reason: str = "",
                       status: str = "resolved") -> dict:
        bid = (branch_id or "").strip()
        clean_status = (status or "resolved").strip().lower()
        if clean_status not in {"resolved", "closed", "superseded", "closed_by_solve"}:
            clean_status = "resolved"
        payload = {
            "branch_id": bid,
            "status": clean_status,
            "reason": (reason or "").strip()[:1000],
            "resolved_by": actor,
        }
        seq = self._append(EV_BRANCH_RESOLVED, actor, payload,
                           dedupe_key=f"branch-resolved::{bid}::{clean_status}::{payload['reason']}")
        with self._lock:
            self._conn.execute(
                "UPDATE branches SET status=?, resolved_seq=? "
                "WHERE challenge_id=? AND branch_id=?",
                (clean_status, seq if seq > 0 else None, self.challenge.id, bid),
            )
            self._conn.commit()
        return {"branch_id": bid, "status": clean_status, "seq": seq,
                "reason": payload["reason"]}

    def add_coordinator_directive(self, *, actor: str, action: str,
                                  directive: str, priority: str = "normal",
                                  route_hash: str = "") -> int:
        route = self.normalize_route_hash(route_hash) if route_hash else ""
        payload = {
            "action": (action or "").strip() or "note",
            "priority": (priority or "normal").strip(),
            "directive": (directive or "").strip()[:2000],
            "route_hash": route,
        }
        return self._append(EV_COORDINATOR_DIRECTIVE, actor, payload,
                            dedupe_key=f"directive::{payload['action']}::{payload['directive']}::{route}")

    # ── B: operator directives (first-class steering, not a fake candidate) ──
    _DIRECTIVE_PRIORITY = {"correction": 100, "redirect": 70, "focus": 60,
                           "hint": 50, "standing": 30, "note": 20}
    _PREEMPT_POLICIES = {"none", "soft_rebind", "graceful_drain", "force_cancel"}

    def add_operator_directive(self, *, actor: str = "operator", action: str,
                               text: str, scope: str = "global",
                               standing: bool = False,
                               preempt_policy: str = "soft_rebind",
                               priority: Optional[int] = None,
                               source_command_id: str = "") -> dict:
        """B: record an operator directive as a first-class steering object. Returns
        {directive_id, seq}. The caller binds it to intents/workers per preemption."""
        act = (action or "hint").strip().lower() or "hint"
        clean_text = (text or "").strip()[:2000]
        scope = (scope or "global").strip() or "global"
        policy = preempt_policy if preempt_policy in self._PREEMPT_POLICIES else "soft_rebind"
        prio = priority if priority is not None else self._DIRECTIVE_PRIORITY.get(
            act, 30 if standing else 50)
        digest = hashlib.sha1(
            f"{act}:{scope}:{clean_text}:{time.time()}".encode("utf-8", "ignore")
        ).hexdigest()[:10]
        directive_id = f"D-{digest}"
        payload = {
            "directive_id": directive_id, "action": act, "text": clean_text,
            "scope": scope, "standing": bool(standing), "priority": prio,
            "preempt_policy": policy, "status": "received",
        }
        source_command_id = str(source_command_id or "").strip()
        if source_command_id:
            payload["source_command_id"] = source_command_id
        seq = self._append(EV_OPERATOR_DIRECTIVE, actor, payload,
                           dedupe_key=f"opdirective::{directive_id}")
        with self._lock:
            self._conn.execute(
                "INSERT OR IGNORE INTO operator_directives "
                "(directive_id, challenge_id, action, text, scope, priority, standing, "
                " status, preempt_policy, received_seq) "
                "VALUES (?,?,?,?,?,?,?,?,?,?)",
                (directive_id, self.challenge.id, act, clean_text, scope, prio,
                 int(bool(standing)), "received", policy, seq if seq > 0 else 0),
            )
            self._conn.commit()
        return {"directive_id": directive_id, "seq": seq, "priority": prio,
                "preempt_policy": policy, "action": act}

    def update_directive_status(self, *, directive_id: str, status: str,
                                actor: str = "coordinator",
                                generated_fact_seq: Optional[int] = None,
                                generated_intent_id: Optional[str] = None,
                                bound_worker: Optional[str] = None,
                                conflicts: Optional[list[str]] = None) -> int:
        """B: advance a directive through received→queued→bound→acted (or
        superseded/expired/rejected). Stamps the per-status seq column + payload."""
        valid = {"received", "queued", "bound", "acted", "superseded",
                 "expired", "rejected"}
        st = (status or "").strip().lower()
        if st not in valid:
            st = "queued"
        payload = {"directive_id": directive_id, "status": st}
        if generated_fact_seq is not None:
            payload["generated_fact_seq"] = int(generated_fact_seq)
        if generated_intent_id:
            payload["generated_intent_id"] = generated_intent_id
        if bound_worker:
            payload["bound_worker"] = bound_worker
        if conflicts:
            payload["conflicts"] = list(conflicts)
        seq = self._append(EV_OPERATOR_DIRECTIVE_STATUS, actor, payload)
        col = {"queued": "queued_seq", "bound": "bound_seq", "acted": "acted_seq",
               "superseded": "superseded_seq"}.get(st)
        sets = ["status=?"]
        params: list[Any] = [st]
        if col:
            sets.append(f"{col}=?")
            params.append(seq if seq > 0 else None)
        if generated_fact_seq is not None:
            sets.append("generated_fact_seq=?")
            params.append(int(generated_fact_seq))
        if generated_intent_id:
            sets.append("generated_intent_id=?")
            params.append(generated_intent_id)
        if bound_worker:
            sets.append("bound_worker=?")
            params.append(bound_worker)
        if conflicts:
            sets.append("conflicts_json=?")
            params.append(json.dumps(list(conflicts)))
        params.extend([self.challenge.id, directive_id])
        with self._lock:
            self._conn.execute(
                f"UPDATE operator_directives SET {', '.join(sets)} "
                f"WHERE challenge_id=? AND directive_id=?",
                tuple(params),
            )
            self._conn.commit()
        return seq

    def operator_directives(self, *, active_only: bool = True) -> list[dict]:
        if not self._table_exists("operator_directives"):
            return []
        where = "challenge_id=?"
        if active_only:
            # ``acted`` means a one-shot directive was actually bound/routed to a
            # worker.  It is history, not active context, and must never be injected
            # into every future worker.  Standing directives stay active by remaining
            # ``bound`` until an explicit clear marks them expired.
            where += " AND status NOT IN ('acted','superseded','expired','rejected')"
        with self._lock:
            rows = self._conn.execute(
                "SELECT directive_id, action, text, scope, priority, standing, status, "
                "preempt_policy, generated_intent_id, bound_worker FROM operator_directives "
                f"WHERE {where} ORDER BY priority DESC, received_seq",
                (self.challenge.id,),
            ).fetchall()
        return [
            {"directive_id": r[0], "action": r[1], "text": r[2], "scope": r[3],
             "priority": int(r[4] or 0), "standing": bool(r[5]), "status": r[6],
             "preempt_policy": r[7], "generated_intent_id": r[8] or "",
             "bound_worker": r[9] or ""}
            for r in rows
        ]

    def expire_standing_directives(self, *, actor: str = "operator",
                                   text: str = "") -> list[str]:
        """Atomically expire matching standing directives and append receipts.

        Clearing guidance is an absence guarantee.  Updating rows one-by-one via
        ``update_directive_status`` could leave a half-cleared graph if a later
        write failed, while the runtime still ACKed success.  This transaction
        keeps the append-only event log and its materialized projection aligned.
        """
        exact = str(text or "").strip()
        with self._lock:
            try:
                self._conn.execute("BEGIN IMMEDIATE")
                where = (
                    "challenge_id=? AND standing=1 AND status NOT IN "
                    "('acted','superseded','expired','rejected')"
                )
                params: list[Any] = [self.challenge.id]
                if exact:
                    where += " AND text=?"
                    params.append(exact)
                rows = self._conn.execute(
                    f"SELECT directive_id FROM operator_directives WHERE {where} "
                    "ORDER BY received_seq",
                    tuple(params),
                ).fetchall()
                directive_ids = [str(row[0]) for row in rows]
                for directive_id in directive_ids:
                    payload = {"directive_id": directive_id, "status": "expired"}
                    self._conn.execute(
                        "INSERT INTO events "
                        "(ts, challenge_id, actor, kind, payload, artifact_id, "
                        " verified, confidence, dedupe_key) "
                        "VALUES (?,?,?,?,?,?,?,?,?)",
                        (time.time(), self.challenge.id, actor,
                         EV_OPERATOR_DIRECTIVE_STATUS, json.dumps(payload),
                         None, 0, 1.0, None),
                    )
                    self._conn.execute(
                        "UPDATE operator_directives SET status='expired' "
                        "WHERE challenge_id=? AND directive_id=?",
                        (self.challenge.id, directive_id),
                    )
                self._conn.commit()
                return directive_ids
            except Exception:
                self._conn.rollback()
                raise

    def apply_standing_clear(
        self,
        *,
        command_id: str,
        actor: str = "operator",
        text: str = "",
        cutoff_before: Optional[float] = None,
        eligible_command_ids: Optional[list[str]] = None,
        match_by_source_ids: bool = False,
    ) -> dict:
        """Apply one journaled standing-guidance clear exactly once.

        The control journal and evidence graph cannot participate in one SQLite
        transaction. This graph-side marker is therefore the durable inbox half
        of that outbox edge: directive tombstones and the marker commit together.
        A restart can safely replay the control command until this marker exists.

        Recovery passes ``eligible_command_ids`` as a *closed* causal set: only
        source ids durably proven to precede the clear may be retracted. A source
        id absent from that set may have been committed after the recovery
        snapshot and is preserved. ``cutoff_before`` is the clear command's own
        persistence fence and is used only for legacy directives without source
        metadata. Live application passes ``None`` and clears every matching
        directive visible at its linearization point. ``match_by_source_ids`` is
        the secret-safe exact selector: the graph never materialises plaintext and
        therefore retracts only the source ids already matched inside the private
        control journal boundary; source-less legacy rows are conservatively kept.
        """
        source_command_id = str(command_id or "").strip()
        if not source_command_id:
            raise ValueError("standing clear command_id cannot be empty")
        exact = str(text or "").strip()
        eligible_ids = (
            None if eligible_command_ids is None else {
                str(value or "").strip()
                for value in eligible_command_ids
                if str(value or "").strip()
            }
        )
        marker_key = (
            f"control-standing-clear::{self.challenge.id}::{source_command_id}"
        )
        with self._lock:
            try:
                self._conn.execute("BEGIN IMMEDIATE")
                marker = self._conn.execute(
                    "SELECT seq, payload FROM events WHERE challenge_id=? "
                    "AND dedupe_key=? LIMIT 1",
                    (self.challenge.id, marker_key),
                ).fetchone()
                if marker is not None:
                    try:
                        payload = dict(json.loads(marker[1]) or {})
                    except Exception:
                        payload = {}
                    if str(payload.get("text") or "").strip() != exact:
                        raise ValueError(
                            "standing clear command_id was reused with different text"
                        )
                    if bool(payload.get("match_by_source_ids", False)) != bool(
                            match_by_source_ids):
                        raise ValueError(
                            "standing clear command_id was reused with a different selector"
                        )
                    self._conn.commit()
                    return {
                        "command_id": source_command_id,
                        "marker_seq": int(marker[0]),
                        "expired_directives": list(
                            payload.get("expired_directives") or []),
                        "already_applied": True,
                    }

                where = (
                    "d.challenge_id=? AND d.standing=1 AND d.status NOT IN "
                    "('acted','superseded','expired','rejected')"
                )
                params: list[Any] = [self.challenge.id]
                if exact and not match_by_source_ids:
                    where += " AND d.text=?"
                    params.append(exact)
                rows = self._conn.execute(
                    "SELECT d.directive_id, e.ts, e.payload "
                    "FROM operator_directives d "
                    "LEFT JOIN events e ON e.seq=d.received_seq "
                    f"WHERE {where} ORDER BY d.received_seq, d.directive_id",
                    tuple(params),
                ).fetchall()
                directive_ids: list[str] = []
                for directive_id, received_at, raw_event_payload in rows:
                    try:
                        event_payload = dict(json.loads(raw_event_payload) or {})
                    except Exception:
                        event_payload = {}
                    source_id = str(
                        event_payload.get("source_command_id") or "").strip()
                    if match_by_source_ids:
                        if (not source_id or eligible_ids is None
                                or source_id not in eligible_ids):
                            continue
                    elif (source_id and eligible_ids is not None
                          and source_id not in eligible_ids):
                        continue
                    # Source-tagged typed commands use journal order above. Only
                    # legacy directives need the wall-clock compatibility fence.
                    if (not source_id and cutoff_before is not None
                            and received_at is not None
                            and float(received_at) >= float(cutoff_before)):
                        continue
                    directive_ids.append(str(directive_id))
                now = time.time()
                for directive_id in directive_ids:
                    status_payload = {
                        "directive_id": directive_id,
                        "status": "expired",
                        "source_command_id": source_command_id,
                    }
                    self._conn.execute(
                        "INSERT INTO events "
                        "(ts, challenge_id, actor, kind, payload, artifact_id, "
                        " verified, confidence, dedupe_key) "
                        "VALUES (?,?,?,?,?,?,?,?,?)",
                        (now, self.challenge.id, actor,
                         EV_OPERATOR_DIRECTIVE_STATUS,
                         json.dumps(status_payload), None, 0, 1.0,
                         f"{marker_key}::directive::{directive_id}"),
                    )
                    self._conn.execute(
                        "UPDATE operator_directives SET status='expired' "
                        "WHERE challenge_id=? AND directive_id=?",
                        (self.challenge.id, directive_id),
                    )

                marker_payload = {
                    "source_command_id": source_command_id,
                    "text": exact,
                    "cutoff_before": (
                        float(cutoff_before)
                        if cutoff_before is not None else None
                    ),
                    "eligible_command_ids": (
                        sorted(eligible_ids) if eligible_ids is not None else None),
                    "match_by_source_ids": bool(match_by_source_ids),
                    "expired_directives": directive_ids,
                }
                marker_cursor = self._conn.execute(
                    "INSERT INTO events "
                    "(ts, challenge_id, actor, kind, payload, artifact_id, "
                    " verified, confidence, dedupe_key) "
                    "VALUES (?,?,?,?,?,?,?,?,?)",
                    (now, self.challenge.id, actor,
                     EV_CONTROL_STANDING_CLEAR_APPLIED,
                     json.dumps(marker_payload), None, 0, 1.0, marker_key),
                )
                self._conn.commit()
                return {
                    "command_id": source_command_id,
                    "marker_seq": int(marker_cursor.lastrowid or 0),
                    "expired_directives": directive_ids,
                    "already_applied": False,
                }
            except Exception:
                self._conn.rollback()
                raise

    def active_operator_directive_texts(self) -> list[str]:
        """The directive texts the planner must prioritize (highest-priority first)."""
        return [d["text"] for d in self.operator_directives(active_only=True) if d.get("text")]

    # ── F: classified HITL requests (need_kind drives auto vs operator pause) ──
    def add_hitl_request(self, *, worker: str, need: str, need_kind: str,
                         classification_confidence: float = 1.0,
                         status: str = "classified",
                         request_id: Optional[str] = None,
                         directive_id: Optional[str] = None,
                         resource_lock_id: Optional[str] = None,
                         auto_action_seq: Optional[int] = None) -> dict:
        """F: record a classified worker hand-raise. need_kind decides downstream
        handling (external_blocker pauses; the others auto-resolve)."""
        nk = (need_kind or "external_blocker").strip()
        rid = str(request_id or "").strip()[:128]
        if not rid:
            digest = hashlib.sha1(
                f"{worker}:{need}:{nk}".encode("utf-8", "ignore")).hexdigest()[:10]
            rid = f"H-{digest}"
        payload = {"request_id": rid, "worker": worker, "need": (need or "")[:1000],
                   "need_kind": nk, "status": status,
                   "classification_confidence": float(classification_confidence)}
        seq = self._append(EV_HITL_CLASSIFIED, worker or "worker", payload,
                           dedupe_key=f"hitl::{rid}")
        with self._lock:
            self._conn.execute(
                "INSERT OR IGNORE INTO hitl_requests "
                "(request_id, challenge_id, worker, need, need_kind, "
                " classification_confidence, status, auto_action_seq, directive_id, "
                " resource_lock_id, created_seq) "
                "VALUES (?,?,?,?,?,?,?,?,?,?,?)",
                (rid, self.challenge.id, worker or "worker", (need or "")[:1000],
                 nk, float(classification_confidence), status,
                 auto_action_seq, directive_id, resource_lock_id, seq if seq > 0 else 0),
            )
            self._conn.commit()
        return {"request_id": rid, "seq": seq, "need_kind": nk}
