"""Lanes, resource locks, cooldowns, and claim finalization.

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
import math
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


# These control-plane events are deliberately colocated with the lease
# materialization.  The generic event spine accepts them without a schema change.
EV_LEASES_SHIFTED = "leases_shifted"
EV_LEASES_SUSPENDED = "leases_suspended"
EV_LEASES_RESUMED = "leases_resumed"


# All SQL identifiers are fixed here; no caller-controlled string is interpolated.
_LEASE_TARGETS = (
    {"kind": "intent", "table": "intents", "key": "intent_id",
     "owner": "worker", "intent": "intent_id", "lane": "lane_key",
     "active": "status='claimed'"},
    {"kind": "poc", "table": "pocs", "key": "poc_id",
     "owner": "worker", "intent": "intent_id",
     # A PoC inherits lane scope through its owning intent when available.
     "lane": ("(SELECT i.lane_key FROM intents i "
              "WHERE i.intent_id=pocs.intent_id "
              "AND i.challenge_id=pocs.challenge_id)"),
     "active": "status='wip'"},
    {"kind": "activity", "table": "activity_locks", "key": "activity_key",
     "owner": "worker", "intent": None, "lane": "activity_key",
     "active": "1=1"},
    {"kind": "lane", "table": "lane_locks", "key": "lane_key",
     "owner": "owner_worker", "intent": "owner_intent", "lane": "lane_key",
     "active": "owner_worker IS NOT NULL"},
    {"kind": "resource", "table": "resource_locks", "key": "lock_id",
     "owner": "owner_worker", "intent": "owner_intent", "lane": "resource_key",
     "active": "status='active' AND owner_worker IS NOT NULL"},
)


class _LanesLocksMixin:
    @staticmethod
    def _lease_engine_matches(worker: str, engine: str) -> bool:
        """Match the stable ``cli-<engine>`` worker-id convention exactly."""
        owner = (worker or "").strip().lower()
        wanted = (engine or "").strip().lower()
        if not owner or not wanted:
            return False
        prefix = f"cli-{wanted}"
        if owner == prefix:
            return True
        return owner.startswith(prefix) and owner[len(prefix):len(prefix) + 1] in {
            "-", "#", ":", "/",
        }

    @staticmethod
    def _validate_lease_scope(scope_kind: str, scope_id: str) -> tuple[str, str]:
        kind = (scope_kind or "challenge").strip().lower()
        if kind not in {"global", "run", "challenge", "worker", "intent", "engine", "lane"}:
            raise ValueError(f"unsupported lease scope: {scope_kind!r}")
        value = (scope_id or "").strip()
        if kind in {"worker", "intent", "engine", "lane"} and not value:
            raise ValueError(f"lease scope {kind!r} requires scope_id")
        return kind, value

    def _lease_scope_matches(self, row: dict, scope_kind: str, scope_id: str) -> bool:
        if scope_kind in {"global", "run"}:
            # A SQLiteSharedGraph is already a run/challenge-local boundary.  There
            # is no run id on a lease row, so run scope means this graph database.
            return True
        if scope_kind == "challenge":
            return not scope_id or scope_id == self.challenge.id
        if scope_kind == "worker":
            return row["owner_worker"] == scope_id
        if scope_kind == "intent":
            return row.get("owner_intent", "") == scope_id
        if scope_kind == "engine":
            return self._lease_engine_matches(row["owner_worker"], scope_id)
        if scope_kind == "lane":
            return row.get("lane_key", "") == self.normalize_lane_key(scope_id)
        return False

    def _select_active_lease_rows(self, *, active_at: float,
                                  scope_kind: str, scope_id: str) -> list[dict]:
        rows: list[dict] = []
        for target in _LEASE_TARGETS:
            intent_expr = target["intent"] or "NULL"
            lane_expr = target["lane"] or "NULL"
            selected = self._conn.execute(
                f"SELECT {target['key']}, {target['owner']}, {intent_expr}, "
                f"{lane_expr}, lease_until FROM {target['table']} "
                f"WHERE challenge_id=? AND {target['active']} "
                f"AND {target['owner']} IS NOT NULL "
                f"AND lease_until IS NOT NULL AND lease_until > ?",
                (self.challenge.id, active_at),
            ).fetchall()
            for key, owner, owner_intent, lane_key, lease_until in selected:
                row = {
                    "kind": target["kind"],
                    "key": str(key),
                    "owner_worker": str(owner or ""),
                    "owner_intent": str(owner_intent or ""),
                    "lane_key": str(lane_key or ""),
                    "lease_until": float(lease_until),
                }
                if self._lease_scope_matches(row, scope_kind, scope_id):
                    rows.append(row)
        return rows

    @staticmethod
    def _lease_target(kind: str) -> dict:
        for target in _LEASE_TARGETS:
            if target["kind"] == kind:
                return target
        raise ValueError(f"unknown lease target kind: {kind!r}")

    def _update_lease_row(self, row: dict, *, expected_until: float,
                          new_until: float) -> bool:
        target = self._lease_target(str(row.get("kind") or ""))
        cur = self._conn.execute(
            f"UPDATE {target['table']} SET lease_until=? "
            f"WHERE challenge_id=? AND {target['key']}=? "
            f"AND {target['owner']}=? AND lease_until=? AND {target['active']}",
            (new_until, self.challenge.id, row["key"], row["owner_worker"],
             expected_until),
        )
        return cur.rowcount == 1

    def _append_lease_event_in_transaction(self, *, kind: str, actor: str,
                                           payload: dict, dedupe_key: str,
                                           event_ts: float) -> int:
        cur = self._conn.execute(
            "INSERT INTO events "
            "(ts, challenge_id, actor, kind, payload, artifact_id, verified, "
            " confidence, dedupe_key) VALUES (?,?,?,?,?,NULL,0,1.0,?)",
            (event_ts, self.challenge.id, actor, kind,
             json.dumps(payload, default=str), dedupe_key),
        )
        return int(cur.lastrowid or 0)

    def shift_active_leases(self, *, actor: str, delta_s: float,
                            scope_kind: str = "challenge", scope_id: str = "",
                            operation_id: str = "", reason: str = "freeze_resume",
                            observed_at: Optional[float] = None) -> dict:
        """Atomically move live lease deadlines and append their audit record.

        ``operation_id`` makes control-command retries idempotent.  Event insertion
        and all materialized-table updates share one ``BEGIN IMMEDIATE`` transaction,
        so a lease deadline can never move silently.
        """
        delta = float(delta_s)
        at = float(time.time() if observed_at is None else observed_at)
        if not math.isfinite(delta) or delta < 0:
            raise ValueError("delta_s must be a finite non-negative number")
        if not math.isfinite(at):
            raise ValueError("observed_at must be finite")
        scope, value = self._validate_lease_scope(scope_kind, scope_id)
        op_id = (operation_id or f"lease-shift-{time.time_ns()}").strip()
        dedupe = f"lease-shift::{self.challenge.id}::{op_id}"
        with self._lock:
            self._conn.execute("BEGIN IMMEDIATE")
            try:
                prior = self._conn.execute(
                    "SELECT seq, payload FROM events WHERE dedupe_key=?", (dedupe,)
                ).fetchone()
                if prior:
                    self._conn.rollback()
                    out = json.loads(prior[1])
                    return {**out, "seq": int(prior[0]), "idempotent": True}
                rows = self._select_active_lease_rows(
                    active_at=at, scope_kind=scope, scope_id=value)
                targets = [
                    {**row, "lease_before": row["lease_until"],
                     "lease_after": row["lease_until"] + delta}
                    for row in rows
                ]
                payload = {
                    "operation_id": op_id,
                    "reason": (reason or "freeze_resume")[:200],
                    "scope": {"kind": scope, "id": value},
                    "observed_at": at,
                    "delta_s": delta,
                    "affected": len(targets),
                    "targets": targets,
                }
                seq = self._append_lease_event_in_transaction(
                    kind=EV_LEASES_SHIFTED, actor=actor, payload=payload,
                    dedupe_key=dedupe, event_ts=time.time())
                for row in targets:
                    if not self._update_lease_row(
                        row, expected_until=row["lease_before"],
                        new_until=row["lease_after"]):
                        raise RuntimeError(
                            f"lease changed during atomic shift: {row['kind']}:{row['key']}")
                self._conn.commit()
            except Exception:
                self._conn.rollback()
                raise
        return {**payload, "seq": seq, "idempotent": False}

    def suspend_active_leases(self, *, actor: str, suspension_id: str,
                              scope_kind: str = "challenge", scope_id: str = "",
                              guard_s: float = 3600.0, reason: str = "freeze",
                              observed_at: Optional[float] = None) -> dict:
        """Guard live leases during freeze and snapshot their exact deadlines.

        A finite provisional extension prevents another dispatcher from reclaiming
        a lease while its owner process is frozen.  ``resume_suspended_leases``
        later replaces that guard with the actual frozen duration.  The snapshot is
        itself the append-only event, avoiding a second mutable source of truth.
        """
        sid = (suspension_id or "").strip()
        if not sid:
            raise ValueError("suspension_id is required")
        guard = float(guard_s)
        at = float(time.time() if observed_at is None else observed_at)
        if not math.isfinite(guard) or guard <= 0:
            raise ValueError("guard_s must be a finite positive number")
        if not math.isfinite(at):
            raise ValueError("observed_at must be finite")
        scope, value = self._validate_lease_scope(scope_kind, scope_id)
        dedupe = f"lease-suspend::{self.challenge.id}::{sid}"
        with self._lock:
            self._conn.execute("BEGIN IMMEDIATE")
            try:
                prior = self._conn.execute(
                    "SELECT seq, payload FROM events WHERE dedupe_key=?", (dedupe,)
                ).fetchone()
                if prior:
                    self._conn.rollback()
                    out = json.loads(prior[1])
                    return {**out, "seq": int(prior[0]), "idempotent": True}
                rows = self._select_active_lease_rows(
                    active_at=at, scope_kind=scope, scope_id=value)
                targets = [
                    {**row, "lease_before": row["lease_until"],
                     "lease_guarded": row["lease_until"] + guard}
                    for row in rows
                ]
                payload = {
                    "suspension_id": sid,
                    "reason": (reason or "freeze")[:200],
                    "scope": {"kind": scope, "id": value},
                    "started_at": at,
                    "guard_s": guard,
                    "affected": len(targets),
                    "targets": targets,
                }
                seq = self._append_lease_event_in_transaction(
                    kind=EV_LEASES_SUSPENDED, actor=actor, payload=payload,
                    dedupe_key=dedupe, event_ts=at)
                for row in targets:
                    if not self._update_lease_row(
                        row, expected_until=row["lease_before"],
                        new_until=row["lease_guarded"]):
                        raise RuntimeError(
                            f"lease changed during atomic suspend: {row['kind']}:{row['key']}")
                self._conn.commit()
            except Exception:
                self._conn.rollback()
                raise
        return {**payload, "seq": seq, "idempotent": False}

    def resume_suspended_leases(self, *, actor: str, suspension_id: str,
                                resumed_at: Optional[float] = None,
                                duration_s: Optional[float] = None,
                                reason: str = "thaw") -> dict:
        """Replace a provisional freeze guard with the measured freeze duration.

        Every row is owner- and deadline-fenced against the suspension event.  If a
        lease was released or re-acquired while frozen it is never clobbered; the
        resume event reports it under ``skipped``.  Retries are idempotent.
        """
        sid = (suspension_id or "").strip()
        if not sid:
            raise ValueError("suspension_id is required")
        at = float(time.time() if resumed_at is None else resumed_at)
        if not math.isfinite(at):
            raise ValueError("resumed_at must be finite")
        start_key = f"lease-suspend::{self.challenge.id}::{sid}"
        resume_key = f"lease-resume::{self.challenge.id}::{sid}"
        with self._lock:
            self._conn.execute("BEGIN IMMEDIATE")
            try:
                prior = self._conn.execute(
                    "SELECT seq, payload FROM events WHERE dedupe_key=?", (resume_key,)
                ).fetchone()
                if prior:
                    self._conn.rollback()
                    out = json.loads(prior[1])
                    return {**out, "seq": int(prior[0]), "idempotent": True}
                started = self._conn.execute(
                    "SELECT seq, payload FROM events WHERE dedupe_key=? AND kind=?",
                    (start_key, EV_LEASES_SUSPENDED),
                ).fetchone()
                if not started:
                    self._conn.rollback()
                    return {"suspension_id": sid, "resumed": False,
                            "status": "not_found", "affected": 0, "skipped": []}
                start_payload = json.loads(started[1])
                started_at = float(start_payload["started_at"])
                duration = float(at - started_at if duration_s is None else duration_s)
                if not math.isfinite(duration) or duration < 0:
                    raise ValueError("duration_s must be a finite non-negative number")

                applicable: list[dict] = []
                skipped: list[dict] = []
                for target_row in start_payload.get("targets", []):
                    row = dict(target_row)
                    descriptor = self._lease_target(str(row.get("kind") or ""))
                    current = self._conn.execute(
                        f"SELECT {descriptor['owner']}, lease_until, "
                        f"CASE WHEN {descriptor['active']} THEN 1 ELSE 0 END "
                        f"FROM {descriptor['table']} WHERE challenge_id=? "
                        f"AND {descriptor['key']}=?",
                        (self.challenge.id, row["key"]),
                    ).fetchone()
                    expected = float(row["lease_guarded"])
                    if (not current or str(current[0] or "") != row["owner_worker"]
                            or float(current[1] or 0.0) != expected or not int(current[2])):
                        skipped.append({"kind": row["kind"], "key": row["key"],
                                        "reason": "owner_or_lease_changed"})
                        continue
                    row["lease_after"] = float(row["lease_before"]) + duration
                    applicable.append(row)

                payload = {
                    "suspension_id": sid,
                    "reason": (reason or "thaw")[:200],
                    "suspended_seq": int(started[0]),
                    "started_at": started_at,
                    "resumed_at": at,
                    "duration_s": duration,
                    "resumed": True,
                    "affected": len(applicable),
                    "skipped": skipped,
                    "targets": [
                        {"kind": row["kind"], "key": row["key"],
                         "owner_worker": row["owner_worker"],
                         "lease_before": row["lease_before"],
                         "lease_after": row["lease_after"]}
                        for row in applicable
                    ],
                }
                seq = self._append_lease_event_in_transaction(
                    kind=EV_LEASES_RESUMED, actor=actor, payload=payload,
                    dedupe_key=resume_key, event_ts=at)
                for row in applicable:
                    if not self._update_lease_row(
                        row, expected_until=float(row["lease_guarded"]),
                        new_until=float(row["lease_after"])):
                        raise RuntimeError(
                            f"lease changed during atomic resume: {row['kind']}:{row['key']}")
                self._conn.commit()
            except Exception:
                self._conn.rollback()
                raise
        return {**payload, "seq": seq, "idempotent": False}

    def outstanding_lease_suspensions(self) -> list[str]:
        """Return suspension ids that have no matching append-only resume event."""
        with self._lock:
            rows = self._conn.execute(
                "SELECT kind, payload FROM events WHERE challenge_id=? "
                "AND kind IN (?,?) ORDER BY seq",
                (self.challenge.id, EV_LEASES_SUSPENDED, EV_LEASES_RESUMED),
            ).fetchall()
        suspended: list[str] = []
        resumed: set[str] = set()
        for kind, raw in rows:
            try:
                sid = str(json.loads(raw).get("suspension_id") or "")
            except Exception:
                sid = ""
            if not sid:
                continue
            if kind == EV_LEASES_SUSPENDED and sid not in suspended:
                suspended.append(sid)
            elif kind == EV_LEASES_RESUMED:
                resumed.add(sid)
        return [sid for sid in suspended if sid not in resumed]

    def recover_suspended_leases(self, *, actor: str = "control-recovery",
                                 resumed_at: Optional[float] = None) -> list[dict]:
        """Close stale freeze guards before an explicitly ACTIVE restart."""
        at = time.time() if resumed_at is None else float(resumed_at)
        return [
            self.resume_suspended_leases(
                actor=actor, suspension_id=sid, resumed_at=at,
                reason="active control epoch recovery")
            for sid in self.outstanding_lease_suspensions()
        ]

    def lock_lane(self, *, actor: str, lane_key: str, risk_class: str,
                  owner_worker: str, owner_intent: str,
                  lease_s: float = 900.0) -> dict:
        lane = self.normalize_lane_key(lane_key)
        risk_seed = risk_class or (lane.split(":", 1)[0] if lane else "")
        risk = _clean_lane_risk(risk_seed)
        if not lane:
            return {"lane_key": "", "seq": 0, "acquired": True}
        now = time.time()
        owner = (owner_worker or actor or "coordinator").strip()
        intent = (owner_intent or "").strip()
        with self._lock:
            row = self._conn.execute(
                "SELECT owner_worker, owner_intent, lease_until, locked_seq, "
                "released_at, released_worker, cooldown_s FROM lane_locks "
                "WHERE challenge_id=? AND lane_key=?",
                (self.challenge.id, lane),
            ).fetchone()
            if row:
                held_by = str(row[0] or "")
                lease_until = float(row[2] or 0.0)
                # Review/coordinator lane locks are reservations: they should stop
                # unstructured fan-out, but the first concrete worker assigned to
                # that same lane must be able to take ownership. After that worker
                # owns it, all other workers are blocked by the normal lease check.
                coordinator_reservation = held_by == "coordinator" and owner != "coordinator"
                if held_by and held_by != owner and lease_until > now and not coordinator_reservation:
                    return {
                        "lane_key": lane,
                        "seq": 0,
                        "acquired": False,
                        "held_by": held_by,
                        "held_intent": str(row[1] or ""),
                        "held_seq": int(row[3] or 0),
                        "lease_until": lease_until,
                    }
                released_at = float(row[4] or 0.0)
                released_worker = str(row[5] or "")
                cooldown_s = float(row[6] or 120.0)
                if (not held_by and released_worker == owner
                        and released_at + cooldown_s > now):
                    return {
                        "lane_key": lane,
                        "seq": 0,
                        "acquired": False,
                        "held_by": "",
                        "held_intent": "",
                        "held_seq": int(row[3] or 0),
                        "cooldown_until": released_at + cooldown_s,
                    }
            self._conn.execute(
                "INSERT INTO lane_locks "
                "(lane_key, challenge_id, risk_class, owner_worker, owner_intent, "
                " lease_until, cooldown_s) VALUES (?,?,?,?,?,?,120) "
                "ON CONFLICT(lane_key) DO UPDATE SET "
                " challenge_id=excluded.challenge_id, risk_class=excluded.risk_class, "
                " owner_worker=excluded.owner_worker, owner_intent=excluded.owner_intent, "
                " lease_until=excluded.lease_until",
                (lane, self.challenge.id, risk, owner, intent, now + float(lease_s)),
            )
            self._conn.commit()
        seq = self._append(
            EV_LANE_LOCKED,
            actor,
            {
                "lane_key": lane,
                "risk_class": risk,
                "owner_worker": owner,
                "owner_intent": intent,
                "lease_until": now + float(lease_s),
            },
        )
        with self._lock:
            self._conn.execute(
                "UPDATE lane_locks SET locked_seq=? "
                "WHERE challenge_id=? AND lane_key=? AND owner_worker=?",
                (seq if seq > 0 else None, self.challenge.id, lane, owner),
            )
            self._conn.commit()
        return {"lane_key": lane, "seq": seq, "acquired": True,
                "owner_worker": owner, "owner_intent": intent,
                "lease_until": now + float(lease_s)}

    def defer_intent_for_lane(self, *, actor: str, intent_id: str,
                              lane_key: str, against_locked_seq: int = 0) -> int:
        lane = self.normalize_lane_key(lane_key)
        if not lane or not intent_id:
            return 0
        with self._lock:
            row = self._conn.execute(
                "SELECT lane_deferrals, deferred_against_locked_seq, risk_class "
                "FROM intents WHERE challenge_id=? AND intent_id=?",
                (self.challenge.id, intent_id),
            ).fetchone()
        if not row:
            return 0
        prev_count = int(row[0] or 0)
        prev_epoch = int(row[1] or 0)
        epoch = int(against_locked_seq or 0)
        should_count = epoch <= 0 or epoch != prev_epoch
        new_count = prev_count + 1 if should_count else prev_count
        seq = self._append(
            EV_INTENT_LANE_DEFERRED,
            actor,
            {
                "intent_id": intent_id,
                "lane_key": lane,
                "against_locked_seq": epoch,
                "lane_deferrals": new_count,
            },
        )
        result_seq = self._append(
            EV_INTENT_CONCLUDED,
            actor,
            {"intent_id": intent_id, "result": "lane_deferred", "lane_key": lane},
        )
        with self._lock:
            self._conn.execute(
                "UPDATE intents SET status='done', worker=NULL, lease_until=NULL, "
                "result_seq=?, lane_key=COALESCE(lane_key, ?), "
                "lane_deferrals=?, deferred_against_locked_seq=? "
                "WHERE challenge_id=? AND intent_id=?",
                (
                    result_seq if result_seq > 0 else None,
                    lane,
                    new_count,
                    epoch if epoch > 0 else None,
                    self.challenge.id,
                    intent_id,
                ),
            )
            self._conn.commit()
        return seq

    def release_lane(self, *, actor: str, lane_key: str,
                     by_worker: str = "") -> dict:
        lane = self.normalize_lane_key(lane_key)
        if not lane:
            return {"lane_key": "", "seq": 0, "released": False,
                    "revived": [], "escalated": []}
        now = time.time()
        by = (by_worker or "").strip()
        with self._lock:
            row = self._conn.execute(
                "SELECT owner_worker, owner_intent, lease_until, risk_class "
                "FROM lane_locks WHERE challenge_id=? AND lane_key=?",
                (self.challenge.id, lane),
            ).fetchone()
        if not row:
            return {"lane_key": lane, "seq": 0, "released": False,
                    "revived": [], "escalated": []}
        owner = str(row[0] or "")
        lease_until = float(row[2] or 0.0)
        if owner and by and owner != by and lease_until > now:
            return {"lane_key": lane, "seq": 0, "released": False,
                    "revived": [], "escalated": [], "held_by": owner}
        risk = str(row[3] or "").strip()
        release_dedupe_key = (
            f"lane-release::{self.challenge.id}::{lane}::"
            f"{str(row[1] or '')}::{owner}::{float(row[2] or 0.0):.9f}"
        )
        seq = self._append(
            EV_LANE_RELEASED,
            actor,
            {"lane_key": lane, "risk_class": risk, "released_worker": owner,
             "owner_intent": str(row[1] or ""), "released_by": by or actor},
            # One lane-lock epoch has one release event.  If the event append
            # commits but the materialized release does not, retry repairs the row
            # without duplicating the append-only audit edge.
            dedupe_key=release_dedupe_key,
        )
        if seq <= 0:
            with self._lock:
                prior = self._conn.execute(
                    "SELECT seq FROM events WHERE challenge_id=? AND dedupe_key=?",
                    (self.challenge.id, release_dedupe_key),
                ).fetchone()
            seq = int(prior[0]) if prior else 0
        with self._lock:
            self._conn.execute("BEGIN IMMEDIATE")
            try:
                self._conn.execute(
                    "UPDATE lane_locks SET owner_worker=NULL, owner_intent=NULL, "
                    "lease_until=NULL, released_at=?, released_worker=?, released_seq=? "
                    "WHERE challenge_id=? AND lane_key=?",
                    (now, owner, seq if seq > 0 else None, self.challenge.id, lane),
                )
                rows = self._conn.execute(
                    "SELECT i.intent_id, i.lane_deferrals FROM intents i "
                    "LEFT JOIN events e ON e.seq=i.result_seq "
                    "WHERE i.challenge_id=? AND i.lane_key=? AND i.status='done' "
                    "AND json_extract(e.payload,'$.result')='lane_deferred' "
                    "ORDER BY i.created_seq",
                    (self.challenge.id, lane),
                ).fetchall()
                revived = [
                    str(r[0]) for r in rows
                    if int(r[1] or 0) < self.MAX_LANE_DEFERRALS
                ]
                escalated = [
                    str(r[0]) for r in rows
                    if int(r[1] or 0) >= self.MAX_LANE_DEFERRALS
                ]
                if revived:
                    q = ",".join("?" for _ in revived)
                    self._conn.execute(
                        f"UPDATE intents SET status='open', dispatch_state='active', "
                        f"close_reason=NULL, worker=NULL, lease_until=NULL, "
                        f"result_seq=NULL, to_fact_seq=NULL, deferred_against_locked_seq=NULL "
                        f"WHERE challenge_id=? AND intent_id IN ({q})",
                        (self.challenge.id, *revived),
                    )
                self._conn.commit()
            except BaseException:
                # Without an explicit rollback, this connection would observe its
                # own uncommitted owner=NULL and let retirement drop the only lane
                # owner even though a fresh process still sees it locked.
                self._conn.rollback()
                raise
        for iid in escalated:
            result_seq = self._append(
                EV_INTENT_CONCLUDED,
                actor,
                {"intent_id": iid, "result": "lane_blocked", "lane_key": lane},
            )
            with self._lock:
                self._conn.execute(
                    "UPDATE intents SET result_seq=? "
                    "WHERE challenge_id=? AND intent_id=?",
                    (result_seq if result_seq > 0 else None, self.challenge.id, iid),
                )
                self._conn.commit()
        return {"lane_key": lane, "seq": seq, "released": True,
                "revived": revived, "escalated": escalated}

    def active_lanes(self) -> list[dict]:
        now = time.time()
        with self._lock:
            rows = self._conn.execute(
                "SELECT lane_key, risk_class, owner_worker, owner_intent, "
                "lease_until, locked_seq FROM lane_locks "
                "WHERE challenge_id=? AND owner_worker IS NOT NULL "
                "AND lease_until IS NOT NULL AND lease_until > ? "
                "ORDER BY locked_seq",
                (self.challenge.id, now),
            ).fetchall()
        return [
            {"lane_key": r[0], "risk_class": r[1] or "", "owner_worker": r[2] or "",
             "owner_intent": r[3] or "", "lease_until": float(r[4] or 0.0),
             "locked_seq": int(r[5] or 0)}
            for r in rows
        ]

    # ── E: unified resource locks (adapter over lane_locks) ──────────────
    @staticmethod
    def normalize_resource_key(resource_key: str) -> str:
        raw = (resource_key or "").strip().lower()
        raw = re.sub(r"\s+", "", raw)
        raw = re.sub(r"[^a-z0-9_:@.*/-]+", "-", raw).strip("-")
        return raw[:180]

    def request_resource_lock(self, *, actor: str, resource_key: str,
                              scope: str = "activity", risk_class: str = "",
                              owner_worker: str = "", owner_intent: str = "",
                              conflict_policy: str = "exclusive",
                              lease_s: float = 600.0, cooldown_s: float = 0.0) -> dict:
        """E: acquire an exclusive (or serialize/cooldown/dedupe) resource lock.
        Returns {lock_id, acquired, held_by?}. Self-heals on lease expiry."""
        rkey = self.normalize_resource_key(resource_key)
        if not rkey:
            return {"lock_id": "", "acquired": True, "resource_key": ""}
        now = time.time()
        owner = (owner_worker or actor or "worker").strip()
        lock_id = f"rl-{rkey}"
        policy = conflict_policy if conflict_policy in {
            "dedupe", "exclusive", "serialize", "cooldown"} else "exclusive"
        with self._lock:
            row = self._conn.execute(
                "SELECT owner_worker, lease_until, status FROM resource_locks "
                "WHERE challenge_id=? AND lock_id=?",
                (self.challenge.id, lock_id),
            ).fetchone()
            if row:
                held_by = str(row[0] or "")
                lease_until = float(row[1] or 0.0)
                if held_by and held_by != owner and lease_until > now:
                    return {"lock_id": lock_id, "acquired": False,
                            "held_by": held_by, "resource_key": rkey,
                            "lease_until": lease_until}
            self._conn.execute(
                "INSERT INTO resource_locks "
                "(lock_id, challenge_id, resource_key, scope, risk_class, status, "
                " owner_worker, owner_intent, lease_until, conflict_policy, cooldown_s) "
                "VALUES (?,?,?,?,?,'active',?,?,?,?,?) "
                "ON CONFLICT(lock_id) DO UPDATE SET "
                " status='active', owner_worker=excluded.owner_worker, "
                " owner_intent=excluded.owner_intent, scope=excluded.scope, "
                " risk_class=excluded.risk_class, lease_until=excluded.lease_until, "
                " conflict_policy=excluded.conflict_policy, cooldown_s=excluded.cooldown_s",
                (lock_id, self.challenge.id, rkey, scope or "activity",
                 risk_class or None, owner, owner_intent or None, now + float(lease_s),
                 policy, float(cooldown_s)),
            )
            self._conn.commit()
        seq = self._append(EV_RESOURCE_LOCKED, actor,
                           {"lock_id": lock_id, "resource_key": rkey, "scope": scope,
                            "risk_class": risk_class, "owner_worker": owner,
                            "owner_intent": owner_intent})
        with self._lock:
            self._conn.execute(
                "UPDATE resource_locks SET created_seq=COALESCE(created_seq,?) "
                "WHERE challenge_id=? AND lock_id=?",
                (seq if seq > 0 else None, self.challenge.id, lock_id),
            )
            self._conn.commit()
        return {"lock_id": lock_id, "acquired": True, "resource_key": rkey,
                "owner_worker": owner, "seq": seq}

    def release_resource_lock(self, *, actor: str, resource_key: str = "",
                              lock_id: str = "", by_worker: str = "") -> dict:
        """E: release a resource lock (owner-fenced). Pass resource_key or lock_id."""
        lid = (lock_id or "").strip()
        if not lid and resource_key:
            lid = f"rl-{self.normalize_resource_key(resource_key)}"
        if not lid:
            return {"lock_id": "", "released": False}
        by = (by_worker or actor or "").strip()
        now = time.time()
        with self._lock:
            row = self._conn.execute(
                "SELECT owner_worker, lease_until, resource_key FROM resource_locks "
                "WHERE challenge_id=? AND lock_id=?",
                (self.challenge.id, lid),
            ).fetchone()
            if not row:
                return {"lock_id": lid, "released": False}
            owner = str(row[0] or "")
            lease_until = float(row[1] or 0.0)
            rkey = str(row[2] or "")
            if owner and by and owner != by and lease_until > now:
                return {"lock_id": lid, "released": False, "held_by": owner}
            self._conn.execute(
                "UPDATE resource_locks SET status='released', owner_worker=NULL, "
                "lease_until=NULL WHERE challenge_id=? AND lock_id=?",
                (self.challenge.id, lid),
            )
            self._conn.commit()
        seq = self._append(EV_RESOURCE_RELEASED, actor,
                           {"lock_id": lid, "resource_key": rkey, "released_by": by})
        with self._lock:
            self._conn.execute(
                "UPDATE resource_locks SET released_seq=? "
                "WHERE challenge_id=? AND lock_id=?",
                (seq if seq > 0 else None, self.challenge.id, lid),
            )
            self._conn.commit()
        return {"lock_id": lid, "released": True, "resource_key": rkey, "seq": seq}

    def active_resource_locks(self) -> list[dict]:
        if not self._table_exists("resource_locks"):
            return []
        now = time.time()
        with self._lock:
            rows = self._conn.execute(
                "SELECT lock_id, resource_key, scope, risk_class, owner_worker, "
                "owner_intent, lease_until FROM resource_locks "
                "WHERE challenge_id=? AND status='active' AND owner_worker IS NOT NULL "
                "AND (lease_until IS NULL OR lease_until > ?) ORDER BY created_seq",
                (self.challenge.id, now),
            ).fetchall()
        return [
            {"lock_id": r[0], "resource_key": r[1], "scope": r[2] or "",
             "risk_class": r[3] or "", "owner_worker": r[4] or "",
             "owner_intent": r[5] or "", "lease_until": float(r[6] or 0.0)}
            for r in rows
        ]

    def check_resource_conflicts(self, *, resource_key: str = "", lane_key: str = "",
                                 by_worker: str = "") -> dict:
        """E: unified conflict check across lane_locks AND resource_locks. The
        scheduler calls THIS one method before dispatch. Returns
        {conflict: bool, blockers: [{kind, key, owner}]}."""
        blockers: list[dict] = []
        if lane_key and self.is_lane_held_by_other(lane_key, by_worker):
            lane = self.normalize_lane_key(lane_key)
            owner = ""
            for l in self.active_lanes():
                if l["lane_key"] == lane:
                    owner = l["owner_worker"]
                    break
            blockers.append({"kind": "lane", "key": lane, "owner": owner})
        if resource_key:
            rkey = self.normalize_resource_key(resource_key)
            for rl in self.active_resource_locks():
                if rl["resource_key"] == rkey and rl["owner_worker"] != (by_worker or ""):
                    blockers.append({"kind": "resource", "key": rkey,
                                     "owner": rl["owner_worker"]})
                    break
        return {"conflict": bool(blockers), "blockers": blockers}

    def is_lane_held_by_other(self, lane_key: str, by_worker: str) -> bool:
        lane = self.normalize_lane_key(lane_key)
        if not lane:
            return False
        now = time.time()
        with self._lock:
            row = self._conn.execute(
                "SELECT owner_worker, lease_until FROM lane_locks "
                "WHERE challenge_id=? AND lane_key=?",
                (self.challenge.id, lane),
            ).fetchone()
        if not row:
            return False
        owner = str(row[0] or "")
        return bool(owner and owner != (by_worker or "") and float(row[1] or 0.0) > now)

    def in_lane_cooldown(self, lane_key: str, worker: str) -> bool:
        lane = self.normalize_lane_key(lane_key)
        if not lane or not worker:
            return False
        now = time.time()
        with self._lock:
            row = self._conn.execute(
                "SELECT released_at, released_worker, cooldown_s FROM lane_locks "
                "WHERE challenge_id=? AND lane_key=?",
                (self.challenge.id, lane),
            ).fetchone()
        if not row:
            return False
        return (
            str(row[1] or "") == worker
            and float(row[0] or 0.0) + float(row[2] or 120.0) > now
        )

    def release_claims_for_finalize(self, *, reason: str) -> dict:
        """J: clean up the graph at run finish, branching on the stop reason (§4).

        - solved / goal_met → close active+claimed intents; close open branches.
        - operator_stop   → claimed/active → resume (dispatch held; kept for revival).
        - budget_exhausted/runtime_failure → claimed → resume, active left open.
        - compacted       → handled by compact_graph (not here).

        Returns the affected intent/branch ids so the caller can emit deltas."""
        terminal_reason = (reason or "runtime_failure").strip() or "runtime_failure"
        # 1) Always free the lease on claimed intents (a finalized run owns nothing).
        with self._lock:
            claimed_rows = self._conn.execute(
                "SELECT intent_id FROM intents WHERE challenge_id=? AND status='claimed'",
                (self.challenge.id,),
            ).fetchall()
            claimed = [str(r[0]) for r in claimed_rows]
            self._conn.execute(
                "UPDATE intents SET status='open', worker=NULL, lease_until=NULL, "
                "result_seq=NULL WHERE challenge_id=? AND status='claimed'",
                (self.challenge.id,),
            )
            self._conn.commit()
        closed_intents: list[str] = []
        resumed_intents: list[str] = []
        closed_branches: list[str] = []
        if terminal_reason in {"solved", "goal_met"}:
            with self._lock:
                rows = self._conn.execute(
                    "SELECT intent_id FROM intents WHERE challenge_id=? AND status='open' "
                    "AND dispatch_state='active'",
                    (self.challenge.id,),
                ).fetchall()
                closed_intents = [str(r[0]) for r in rows]
            if closed_intents:
                result_seq = self._append(
                    EV_INTENT_CONCLUDED,
                    "coordinator",
                    {"intent_id": ",".join(closed_intents),
                     "result": "closed_by_solve" if terminal_reason == "solved"
                     else "closed_by_goal_met"},
                )
                with self._lock:
                    q = ",".join("?" for _ in closed_intents)
                    close_reason = (
                        "closed_by_solve" if terminal_reason == "solved"
                        else "closed_by_goal_met")
                    self._conn.execute(
                        f"UPDATE intents SET status='done', dispatch_state='closed', "
                        f"close_reason=?, stop_reason=?, "
                        f"result_seq=? WHERE challenge_id=? AND intent_id IN ({q})",
                        (close_reason, terminal_reason,
                         result_seq if result_seq > 0 else None,
                         self.challenge.id, *closed_intents),
                    )
                    self._conn.commit()
            with self._lock:
                rows = self._conn.execute(
                    "SELECT branch_id FROM branches WHERE challenge_id=? AND status='open'",
                    (self.challenge.id,),
                ).fetchall()
                closed_branches = [str(r[0]) for r in rows]
            for bid in closed_branches:
                self.resolve_branch(
                    actor="coordinator", branch_id=bid,
                    reason="closed by solved run", status="closed_by_solve")
        elif terminal_reason == "operator_stop":
            # ⑤ operator_stop is the user DELIBERATELY ending the run — close the
            # active intents like a solved run, do NOT park them as resume. Parking
            # them stranded a pile of verify/review intents as "resume" noise that no
            # running coordinator ever revives (revive only runs at next launch), and
            # it polluted the backlog the operator was complaining about (run-75377: 53
            # stranded). budget/runtime_failure still resume (a crash may be retried).
            with self._lock:
                rows = self._conn.execute(
                    "SELECT intent_id FROM intents WHERE challenge_id=? AND status='open' "
                    "AND dispatch_state='active'",
                    (self.challenge.id,),
                ).fetchall()
                closed_intents = [str(r[0]) for r in rows]
            if closed_intents:
                result_seq = self._append(
                    EV_INTENT_CONCLUDED, "coordinator",
                    {"intent_id": ",".join(closed_intents),
                     "result": "operator_stop"})
                with self._lock:
                    q = ",".join("?" for _ in closed_intents)
                    self._conn.execute(
                        f"UPDATE intents SET status='done', dispatch_state='closed', "
                        f"close_reason='operator_stop', stop_reason='operator_stop', "
                        f"result_seq=? WHERE challenge_id=? AND intent_id IN ({q})",
                        (result_seq if result_seq > 0 else None,
                         self.challenge.id, *closed_intents),
                    )
                    self._conn.commit()
                self._append(
                    EV_INTENT_STATE_CHANGED, "coordinator",
                    {"intent_id": ",".join(closed_intents),
                     "dispatch_state": "closed",
                     "stop_reason": "operator_stop"})
        else:
            # budget_exhausted / runtime_failure: hold the run's intents back from a
            # future dispatch (resume) so a re-opened / standby run doesn't immediately
            # re-hurl workers at directions the prior run left mid-flight, while keeping
            # them auditable + revivable. Released claims + still-active opens become
            # resume; stop_reason records which terminal caused it.
            with self._lock:
                rows = self._conn.execute(
                    "SELECT intent_id FROM intents WHERE challenge_id=? AND status='open' "
                    "AND dispatch_state='active'",
                    (self.challenge.id,),
                ).fetchall()
                resumed_intents = [str(r[0]) for r in rows]
                if resumed_intents:
                    q = ",".join("?" for _ in resumed_intents)
                    self._conn.execute(
                        f"UPDATE intents SET dispatch_state='resume', stop_reason=? "
                        f"WHERE challenge_id=? AND intent_id IN ({q})",
                        (terminal_reason, self.challenge.id, *resumed_intents),
                    )
                    self._conn.commit()
            if resumed_intents:
                self._append(
                    EV_INTENT_STATE_CHANGED, "coordinator",
                    {"intent_id": ",".join(resumed_intents),
                     "dispatch_state": INTENT_DISPATCH_RESUME,
                     "stop_reason": terminal_reason})
        return {"reason": terminal_reason, "released_claims": claimed,
                "closed_intents": closed_intents, "resumed_intents": resumed_intents,
                "closed_branches": closed_branches}
