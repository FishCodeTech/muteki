"""Shared, evidence-bearing, event-sourced solve graph.

A shared, evolving solve graph WITH a provenance gate on every fact: each fact
carries the evidence (and the event) that produced it, so the graph is not just
a scratchpad but an auditable record of what was actually proven.

Design (A+B+C+D):
- (D) Local direct SQLite file, ONE per challenge. No HTTP server: same-host
  sub-process workers open the same `.db`; WAL natively supports multi-process
  concurrent read/write. A `SharedGraph` Protocol keeps the backend swappable
  (a cross-container HTTP backend can be added later without touching callers).
- (A) One long-lived connection per instance + one-time PRAGMA, incl.
  `busy_timeout` (avoids lost writes: SQLITE_BUSY → auto-queue, not drop) +
  `synchronous=NORMAL` (safe & fast under WAL).
- (C) The source of truth is an append-only `events` table (INSERT only, never
  UPDATE/DELETE). `facts`/`intents` are MATERIALIZED views folded from events —
  droppable & rebuildable. Provenance is free (every fact's origin is its event);
  the analytics flywheel reads the raw event log; time-travel replay is possible.
- (B) Intent claiming is a single atomic UPDATE guarded by `changes()` — zero
  TOCTOU window (used once the reasoner dispatches intents).

Invariant: the flag-acceptance gate stays a separate, hardcoded `_flag_ok` — it
is NEVER reachable as a pluggable verifier here.
"""

from __future__ import annotations

import json
import hashlib
import re
import sqlite3
import threading
import time
from urllib.parse import urlparse
from difflib import SequenceMatcher
from pathlib import Path
from typing import Any, Optional, Protocol, runtime_checkable

from muteki.models.solve_graph import Challenge, Evidence, SolveGraph
from muteki.solver.result_codes import is_genuine_giveup


# ── event types (C: append-only log) ─────────────────────────────────────────
EV_FACT_ADDED = "fact_added"
EV_HYP_PROPOSED = "hyp_proposed"
EV_HYP_REFUTED = "hyp_refuted"
EV_DEAD_END = "dead_end"
EV_INTENT_PROPOSED = "intent_proposed"
EV_INTENT_CLAIMED = "intent_claimed"
EV_INTENT_CONCLUDED = "intent_concluded"
EV_FLAG_FOUND = "flag_found"
EV_FLAG_INVALIDATED = "flag_invalidated"  # multi-flag: a false-positive flag is removed
EV_POC_SAVED = "poc_saved"
EV_POC_CLAIMED = "poc_claimed"
EV_POC_CONCLUDED = "poc_concluded"
EV_REVIEW_FINDING = "review_finding"
EV_FACT_CHALLENGED = "fact_challenged"
EV_FACT_REVALIDATED = "fact_revalidated"
EV_ROUTE_SUPPRESSED = "route_suppressed"
EV_ROUTE_REOPENED = "route_reopened"
EV_BRANCH_SPLIT = "branch_split"
EV_BRANCH_RESOLVED = "branch_resolved"
EV_COORDINATOR_DIRECTIVE = "coordinator_directive"
EV_REVIEW_PROPOSAL = "review_proposal"
EV_REVIEW_PROPOSAL_DECISION = "review_proposal_decision"
EV_LANE_LOCKED = "lane_locked"
EV_LANE_RELEASED = "lane_released"
EV_INTENT_LANE_DEFERRED = "intent_lane_deferred"
# A+J: fact lifecycle (reject/merge/supersede) + intent dispatch_state transitions.
EV_FACT_REJECTED = "fact_rejected"
EV_FACT_MERGED = "fact_merged"
EV_FACT_SUPERSEDED = "fact_superseded"
EV_FACT_PINNED = "fact_pinned"
EV_INTENT_STATE_CHANGED = "intent_state_changed"
# B/F: operator directive lifecycle + classified HITL request.
EV_OPERATOR_DIRECTIVE = "operator_directive"
EV_OPERATOR_DIRECTIVE_STATUS = "operator_directive_status"
EV_HITL_CLASSIFIED = "hitl_classified"
# E: unified resource lock (coexists with lane_locks via the adapter).
EV_RESOURCE_LOCKED = "resource_locked"
EV_RESOURCE_RELEASED = "resource_released"
# H: long-run graph compaction.
EV_GRAPH_COMPACTED = "graph_compacted"


# A: fact lifecycle states. unresolved/challenged/revalidated keep the legacy
# fact_reviews semantics; rejected/merged/superseded are the new terminal states.
FACT_STATE_UNRESOLVED = "unresolved"
FACT_STATE_CHALLENGED = "challenged"
FACT_STATE_REVALIDATED = "revalidated"
FACT_STATE_REJECTED = "rejected"
FACT_STATE_MERGED = "merged"
FACT_STATE_SUPERSEDED = "superseded"
_FACT_TERMINAL_STATES = {FACT_STATE_REJECTED, FACT_STATE_MERGED, FACT_STATE_SUPERSEDED}
_FACT_STATES = {
    FACT_STATE_UNRESOLVED, FACT_STATE_CHALLENGED, FACT_STATE_REVALIDATED,
    FACT_STATE_REJECTED, FACT_STATE_MERGED, FACT_STATE_SUPERSEDED,
}

# A/J: intent dispatch_state — orthogonal to status (open/claimed/done).
# active  → claimable + visible to planner/workers (the default)
# resume  → held back from dispatch (paused/deferred), kept for audit/revival
# retired → permanently dropped (compacted/stale); never re-dispatched
# closed  → terminal-by-conclusion (solved/route_suppressed/etc.)
INTENT_DISPATCH_ACTIVE = "active"
INTENT_DISPATCH_RESUME = "resume"
INTENT_DISPATCH_RETIRED = "retired"
INTENT_DISPATCH_CLOSED = "closed"
_INTENT_DISPATCH_STATES = {
    INTENT_DISPATCH_ACTIVE, INTENT_DISPATCH_RESUME,
    INTENT_DISPATCH_RETIRED, INTENT_DISPATCH_CLOSED,
}


_SERVICE_DEFAULT_PORTS = {
    "smb": 445,
    "microsoft-ds": 445,
    "http": 80,
    "https": 443,
    "rdp": 3389,
    "winrm": 5985,
    "winrm-http": 5985,
    "winrm-https": 5986,
    "ssh": 22,
    "redis": 6379,
    "mysql": 3306,
    "mssql": 1433,
    "ldap": 389,
    "ldaps": 636,
    "kerberos": 88,
    "postgres": 5432,
}
_LANE_RISK_CLASSES = {
    "destructive",
    "exclusive_shell",
    "listener_port",
    "relay_service",
    "rate_limited",
}


# A worker records the SAME finding through two entrances: the blackboard skill
# (write_fact → bare text, verified) AND its CLI stream's VERIFIED_FACT= marker
# (_record_fact → "[codex] <text>", often witness-downgraded to a candidate). The
# old dedupe key `fact::{actor}::{artifact_id}::{text}` treated these as two facts
# (engine prefix + artifact differ), so one finding became 1 verified + 1 candidate
# echo — the dominant source of candidate inflation (run-75377: 97 candidates, most
# of them prefixed marker echoes of 33 bare verified skill facts). The fact's
# IDENTITY is who-said-what, not which entrance or which artifact carried it: strip
# the leading "[engine] " tag and normalize whitespace so both entrances collide on
# one key. artifact_id is provenance, not identity — it is excluded from the key.
_FACT_ENGINE_PREFIX_RE = re.compile(r"^\[[a-z0-9 _.-]{1,40}\]\s*", re.IGNORECASE)


def _normalize_fact_identity(fact: str) -> str:
    s = _FACT_ENGINE_PREFIX_RE.sub("", str(fact or ""))
    return " ".join(s.split()).lower()


def _clean_lane_risk(risk_class: str) -> str:
    risk = re.sub(r"[^a-z0-9_]+", "_", (risk_class or "").strip().lower()).strip("_")
    return risk if risk in _LANE_RISK_CLASSES else "destructive"


def _clean_lane_host(host: str) -> tuple[str, float, str]:
    raw = (host or "").strip()
    if not raw:
        return "", 0.0, "missing_host"
    parsed = urlparse(raw if "://" in raw else f"//{raw}")
    candidate = parsed.hostname or raw
    candidate = candidate.strip().strip("[]").lower()
    candidate = re.sub(r"^https?://", "", candidate)
    candidate = candidate.split("/", 1)[0].split("?", 1)[0].split("#", 1)[0]
    if "@" in candidate:
        candidate = candidate.rsplit("@", 1)[-1]
    candidate = candidate.strip().strip("[]")
    if not candidate:
        if raw:
            bucket = re.sub(r"[^a-z0-9_.-]+", "-", raw.lower()).strip("-")[:120]
            return f"unknown-host:{bucket or hashlib.sha1(raw.encode()).hexdigest()[:10]}", 0.30, "host_unparsed"
        return "", 0.0, "missing_host"
    if re.fullmatch(r"[0-9a-f:.]+", candidate) and ":" in candidate:
        # Keep IPv6 usable without DNS. We do not resolve names here.
        return candidate, 0.95, ""
    if re.fullmatch(r"(?:\d{1,3}\.){3}\d{1,3}", candidate):
        return candidate, 1.0, ""
    if re.fullmatch(r"[a-z0-9][a-z0-9.-]{0,252}", candidate):
        return candidate.rstrip("."), 0.85, "host_not_verified"
    bucket = re.sub(r"[^a-z0-9_.-]+", "-", raw.lower()).strip("-")[:120]
    return f"unknown-host:{bucket or hashlib.sha1(raw.encode()).hexdigest()[:10]}", 0.30, "host_unparsed"


def canonicalize_lane(
    host: str = "",
    port: str | int | None = None,
    service: str = "",
    risk_class: str = "destructive",
) -> tuple[str, float, str]:
    """Return a stable lane key for dangerous/exclusive work.

    The key is intentionally resource-only: technique text never participates, so
    "MS17-010 on SMB" and "EternalBlue against 445" collide on the same lane.
    """
    risk = _clean_lane_risk(risk_class)
    clean_host, host_conf, host_reason = _clean_lane_host(host)
    if not clean_host:
        return "", 0.0, host_reason

    clean_service = re.sub(r"[^a-z0-9_-]+", "-", (service or "").strip().lower()).strip("-")
    clean_port = ""
    if port not in (None, ""):
        try:
            p = int(str(port).strip())
            if 0 < p <= 65535:
                clean_port = str(p)
        except (TypeError, ValueError):
            clean_port = ""
    if not clean_port and clean_service in _SERVICE_DEFAULT_PORTS:
        clean_port = str(_SERVICE_DEFAULT_PORTS[clean_service])

    if not clean_port:
        if risk == "listener_port":
            return "", min(host_conf, 0.40), "listener_port_unknown"
        if risk in {"destructive", "exclusive_shell", "relay_service"}:
            clean_port = "*"
        else:
            return "", min(host_conf, 0.50), "port_unknown_fail_open"

    reason = host_reason if host_reason else ""
    conf = min(1.0, host_conf if clean_port != "*" else min(host_conf, 0.65))
    return f"{risk}:tcp:{clean_port}@{clean_host}", conf, reason


@runtime_checkable
class SharedGraph(Protocol):
    """Backend-swappable shared graph. Local = SQLite file; (future) cross-
    container = HTTP. Callers depend only on this surface."""

    def add_evidence(self, *, actor: str, source: str, fact: str,
                     artifact_id: Optional[str] = None, verified: bool = False,
                     confidence: float = 1.0, witness: Optional[str] = None,
                     verifier: str = "", route_hash: str = "",
                     intent_id: Optional[str] = None) -> int: ...

    def add_dead_end(self, *, actor: str, reason: str) -> int: ...

    def flag_found(self, *, actor: str, flag: str,
                   artifact_id: Optional[str] = None,
                   intent_id: Optional[str] = None) -> int: ...

    def propose_intent(self, *, actor: str, intent_id: str, goal: str,
                       payload: Optional[dict] = None,
                       from_fact_seqs: Optional[list[int]] = None) -> int: ...

    def claim_intent(self, *, worker: str, intent_id: str,
                     lease_s: float = 300.0) -> bool: ...

    def conclude_intent(self, *, actor: str, intent_id: str,
                        result: str = "",
                        to_fact_seq: Optional[int] = None,
                        result_detail: str = "") -> int: ...

    def save_poc(self, *, actor: str, poc_id: str, path: str,
                 entry_command: str, status: str = "available",
                 note: str = "", artifact_id: Optional[str] = None,
                 intent_id: Optional[str] = None, name: str = "") -> int: ...

    def claim_poc(self, *, worker: str, poc_id: str,
                  lease_s: float = 300.0) -> bool: ...

    def conclude_poc(self, *, actor: str, poc_id: str,
                     status: str = "spent", note: str = "") -> int: ...

    def supersede_open_intents(self, *, actor: str, match: str,
                               reason: str = "") -> list[str]: ...

    def add_review_finding(self, *, actor: str, kind: str, severity: str,
                           summary: str, evidence_seqs: Optional[list[int]] = None,
                           intent_ids: Optional[list[str]] = None,
                           route_hash: str = "", branch_id: str = "",
                           recommended_actions: Optional[list[str]] = None) -> int: ...

    def add_review_proposal(self, *, actor: str, marker: str, payload: dict,
                            tier: str = "tier1") -> int: ...

    def decide_review_proposal(self, *, actor: str, proposal_seq: int,
                               decision: str, reason: str = "",
                               applied_seq: Optional[int] = None) -> int: ...

    def challenge_fact(self, *, actor: str, fact_seq: int, reason: str,
                       verification_goal: str) -> dict: ...

    def revalidate_fact(self, *, actor: str, fact_seq: int, reason: str = "") -> int: ...

    def reject_fact(self, *, actor: str, fact_seq: int, reason: str = "") -> int: ...

    def merge_fact(self, *, actor: str, from_fact_seq: int, to_fact_seq: int,
                   reason: str = "") -> int: ...

    def supersede_fact(self, *, actor: str, fact_seq: int, reason: str = "",
                       by_fact_seq: Optional[int] = None) -> int: ...

    def review_fact(self, *, actor: str, fact_seq: int, action: str,
                    reason: str = "", verification_goal: str = "",
                    to_fact_seq: Optional[int] = None) -> dict: ...

    def active_candidates(self) -> list[dict]: ...

    def verified_evidence(self) -> list[dict]: ...

    def suppress_route(self, *, actor: str, route_hash: str, label: str = "",
                       reason: str = "", until: str = "new_evidence",
                       matching_intents: Optional[list[str]] = None) -> dict: ...

    def reopen_route(self, *, actor: str, route_hash: str, reason: str = "",
                     intent_goal: str = "") -> dict: ...

    def split_branch(self, *, actor: str, title: str,
                     branches: list[dict[str, Any]]) -> dict: ...

    def resolve_branch(self, *, actor: str, branch_id: str, reason: str = "",
                       status: str = "resolved") -> dict: ...

    def add_coordinator_directive(self, *, actor: str, action: str,
                                  directive: str, priority: str = "normal",
                                  route_hash: str = "") -> int: ...

    def add_operator_directive(self, *, actor: str = "operator", action: str,
                               text: str, scope: str = "global",
                               standing: bool = False,
                               preempt_policy: str = "soft_rebind",
                               priority: Optional[int] = None) -> dict: ...

    def update_directive_status(self, *, directive_id: str, status: str,
                                actor: str = "coordinator",
                                generated_fact_seq: Optional[int] = None,
                                generated_intent_id: Optional[str] = None,
                                bound_worker: Optional[str] = None,
                                conflicts: Optional[list[str]] = None) -> int: ...

    def operator_directives(self, *, active_only: bool = True) -> list[dict]: ...

    def add_hitl_request(self, *, worker: str, need: str, need_kind: str,
                         classification_confidence: float = 1.0,
                         status: str = "classified",
                         directive_id: Optional[str] = None,
                         resource_lock_id: Optional[str] = None,
                         auto_action_seq: Optional[int] = None) -> dict: ...

    def lock_lane(self, *, actor: str, lane_key: str, risk_class: str,
                  owner_worker: str, owner_intent: str,
                  lease_s: float = 900.0) -> dict: ...

    def release_lane(self, *, actor: str, lane_key: str,
                     by_worker: str = "") -> dict: ...

    def defer_intent_for_lane(self, *, actor: str, intent_id: str,
                              lane_key: str, against_locked_seq: int = 0) -> int: ...

    def active_lanes(self) -> list[dict]: ...

    def request_resource_lock(self, *, actor: str, resource_key: str,
                              scope: str = "activity", risk_class: str = "",
                              owner_worker: str = "", owner_intent: str = "",
                              conflict_policy: str = "exclusive",
                              lease_s: float = 600.0, cooldown_s: float = 0.0) -> dict: ...

    def release_resource_lock(self, *, actor: str, resource_key: str = "",
                              lock_id: str = "", by_worker: str = "") -> dict: ...

    def active_resource_locks(self) -> list[dict]: ...

    def check_resource_conflicts(self, *, resource_key: str = "", lane_key: str = "",
                                 by_worker: str = "") -> dict: ...

    def is_lane_held_by_other(self, lane_key: str, by_worker: str) -> bool: ...

    def in_lane_cooldown(self, lane_key: str, worker: str) -> bool: ...

    def release_claims_for_finalize(self, *, reason: str) -> dict: ...

    def compact_graph(self, *, actor: str = "coordinator",
                      trigger: str = "no_progress_time", summary: str = "") -> dict: ...

    def compact_epochs(self) -> list[dict]: ...

    def revive_resume_intents(self, *, actor: str = "coordinator") -> list[str]: ...

    def prior_intent_count(self) -> int: ...

    def to_review_summary(self) -> str: ...

    def suppressed_routes(self) -> list[dict]: ...

    def challenged_facts(self) -> list[dict]: ...

    def branches(self) -> list[dict]: ...

    def coordinator_directives(self) -> list[dict]: ...

    def snapshot(self) -> SolveGraph: ...

    def invalidated_flags(self) -> set[str]: ...

    def events(self) -> list[dict]: ...
    def events_since(self, after_seq: int, kinds: Optional[list[str]] = None) -> list[dict]: ...

    def to_summary(self, max_evidence: int = 16,
                   max_dead_ends: Optional[int] = None) -> str: ...

    def to_reason_summary(self, standing_guidance: Optional[list[str]] = None) -> str: ...

    def to_board_markdown(self) -> str: ...

    def open_goal_texts(self) -> list[str]: ...

    def dispatchable_goal_texts(self) -> list[str]: ...

    def open_route_hashes(self) -> list[str]: ...

    def barren_concluded_goal_texts(self) -> list[str]: ...

    def pin_facts(self, *, actor: str, fact_seqs: list[int],
                  reason: str = "") -> list[int]: ...

    def pinned_fact_seqs(self) -> list[int]: ...

    def fact_pin_context(self, limit: int = 240) -> str: ...

    def try_claim_activity(self, *, worker: str, key: str,
                           lease_s: float = 600.0) -> bool: ...

    def release_activity(self, *, worker: str, key: str) -> None: ...

    def active_activities(self) -> list[dict]: ...

    def canonical_credentials(self) -> list[dict]: ...


_SCHEMA = """
CREATE TABLE IF NOT EXISTS events (
    seq          INTEGER PRIMARY KEY AUTOINCREMENT,
    ts           REAL    NOT NULL,
    challenge_id TEXT    NOT NULL,
    actor        TEXT    NOT NULL,
    kind         TEXT    NOT NULL,
    payload      TEXT    NOT NULL,          -- JSON
    artifact_id  TEXT,
    verified     INTEGER NOT NULL DEFAULT 0,
    confidence   REAL    NOT NULL DEFAULT 1.0,
    dedupe_key   TEXT    UNIQUE             -- NULL allowed; same key not re-appended
);
CREATE TABLE IF NOT EXISTS intents (
    intent_id     TEXT PRIMARY KEY,
    challenge_id  TEXT NOT NULL,
    goal          TEXT NOT NULL,
    worker_class  TEXT NOT NULL DEFAULT 'code',
    route_hash    TEXT,
    branch_id     TEXT,
    lane_key      TEXT,
    risk_class    TEXT,
    lane_deferrals INTEGER NOT NULL DEFAULT 0,
    deferred_against_locked_seq INTEGER,
    priority      INTEGER NOT NULL DEFAULT 0,
    status        TEXT NOT NULL DEFAULT 'open',  -- open|claimed|done
    worker        TEXT,
    lease_until   REAL,
    created_seq   INTEGER NOT NULL,
    result_seq    INTEGER,
    result_detail TEXT
);
CREATE TABLE IF NOT EXISTS intent_sources (
    intent_id  TEXT NOT NULL,
    fact_seq   INTEGER NOT NULL,
    PRIMARY KEY (intent_id, fact_seq)
);
CREATE TABLE IF NOT EXISTS intent_products (
    intent_id  TEXT NOT NULL,
    fact_seq   INTEGER NOT NULL,
    PRIMARY KEY (intent_id, fact_seq)
);
CREATE TABLE IF NOT EXISTS pocs (
    poc_id        TEXT PRIMARY KEY,
    challenge_id  TEXT NOT NULL,
    intent_id     TEXT,
    name          TEXT NOT NULL,
    path          TEXT NOT NULL,
    artifact_id   TEXT,
    entry_command TEXT NOT NULL,
    status        TEXT NOT NULL DEFAULT 'available',
    note          TEXT,
    worker        TEXT,
    lease_until   REAL,
    created_seq   INTEGER NOT NULL,
    result_seq    INTEGER
);
-- P4 action-level dedup: a worker claims a high-cost ACTIVITY (e.g.
-- "nmap:8.130.96.176", "shiro-key-brute:8080") before doing it; a parallel worker
-- that finds the activity already claimed (lease not expired) avoids redoing it.
-- This is the "two workers nmap the same target" fix that intent-level claim can't
-- reach (whole-challenge workers don't claim per-action). Lease-expiry self-heals.
CREATE TABLE IF NOT EXISTS activity_locks (
    activity_key  TEXT PRIMARY KEY,           -- normalized "verb:target"
    challenge_id  TEXT NOT NULL,
    worker        TEXT NOT NULL,
    lease_until   REAL NOT NULL,
    claimed_ts    REAL NOT NULL
);
CREATE TABLE IF NOT EXISTS lane_locks (
    lane_key      TEXT PRIMARY KEY,
    challenge_id  TEXT NOT NULL,
    risk_class    TEXT NOT NULL,
    owner_worker  TEXT,
    owner_intent  TEXT,
    lease_until   REAL,
    released_at   REAL,
    released_worker TEXT,
    cooldown_s    REAL NOT NULL DEFAULT 120,
    locked_seq    INTEGER,
    released_seq  INTEGER
);
CREATE TABLE IF NOT EXISTS routes (
    route_hash     TEXT PRIMARY KEY,
    challenge_id   TEXT NOT NULL,
    label          TEXT NOT NULL,
    status         TEXT NOT NULL DEFAULT 'open',
    suppressed_seq INTEGER,
    reopened_seq   INTEGER,
    reason         TEXT,
    until_policy   TEXT
);
CREATE TABLE IF NOT EXISTS fact_reviews (
    fact_seq        INTEGER PRIMARY KEY,
    challenge_id    TEXT NOT NULL,
    status          TEXT NOT NULL,
    challenged_seq  INTEGER,
    revalidated_seq INTEGER,
    reason          TEXT,
    verification_intent_id TEXT
);
CREATE TABLE IF NOT EXISTS branches (
    branch_id     TEXT PRIMARY KEY,
    challenge_id  TEXT NOT NULL,
    parent_id     TEXT,
    title         TEXT NOT NULL,
    assumption    TEXT NOT NULL,
    prove_or_disprove TEXT,
    status        TEXT NOT NULL DEFAULT 'open',
    created_seq   INTEGER NOT NULL,
    resolved_seq  INTEGER
);
-- A: current lifecycle state per fact (fact_reviews stays the action history).
CREATE TABLE IF NOT EXISTS fact_states (
    fact_seq      INTEGER PRIMARY KEY,
    challenge_id  TEXT NOT NULL,
    state         TEXT NOT NULL DEFAULT 'unresolved',
    verified_effective   INTEGER,
    confidence_effective REAL,
    reason               TEXT,
    challenged_seq       INTEGER,
    revalidated_seq      INTEGER,
    rejected_seq         INTEGER,
    merged_seq           INTEGER,
    superseded_seq       INTEGER,
    retired_seq          INTEGER,
    verification_intent_id TEXT,
    updated_seq          INTEGER
);
-- Reason-selected retention pins. The model, not summary heuristics, decides
-- which older facts stay globally visible after the recency frontier clips noise.
CREATE TABLE IF NOT EXISTS fact_pins (
    fact_seq      INTEGER PRIMARY KEY,
    challenge_id  TEXT NOT NULL,
    actor         TEXT NOT NULL,
    reason        TEXT,
    pinned_seq    INTEGER NOT NULL
);
-- A: fact merge edges (from_fact folded into to_fact).
CREATE TABLE IF NOT EXISTS fact_merges (
    from_fact_seq INTEGER NOT NULL,
    to_fact_seq   INTEGER NOT NULL,
    challenge_id  TEXT NOT NULL,
    merge_seq     INTEGER NOT NULL,
    reason        TEXT,
    PRIMARY KEY (from_fact_seq, to_fact_seq)
);
-- B/F: operator directives (replaces the legacy operator_hint fact+intent path).
CREATE TABLE IF NOT EXISTS operator_directives (
    directive_id     TEXT PRIMARY KEY,
    challenge_id     TEXT NOT NULL,
    action           TEXT NOT NULL,
    text             TEXT NOT NULL,
    scope            TEXT,
    priority         INTEGER NOT NULL DEFAULT 50,
    standing         INTEGER NOT NULL DEFAULT 0,
    status           TEXT NOT NULL DEFAULT 'received',
    preempt_policy   TEXT NOT NULL DEFAULT 'soft_rebind',
    generated_fact_seq    INTEGER,
    generated_intent_id   TEXT,
    bound_worker          TEXT,
    conflicts_json        TEXT,
    received_seq   INTEGER,
    queued_seq     INTEGER,
    bound_seq      INTEGER,
    acted_seq      INTEGER,
    superseded_seq INTEGER
);
-- F: classified HITL requests (need_kind drives auto-resolution vs operator pause).
CREATE TABLE IF NOT EXISTS hitl_requests (
    request_id       TEXT PRIMARY KEY,
    challenge_id     TEXT NOT NULL,
    worker           TEXT NOT NULL,
    need             TEXT NOT NULL,
    need_kind        TEXT NOT NULL,
    classification_confidence REAL,
    status           TEXT NOT NULL DEFAULT 'classified',
    auto_action_seq  INTEGER,
    directive_id     TEXT,
    resource_lock_id TEXT,
    created_seq      INTEGER
);
-- E: unified resource locks (coexist with lane_locks via the adapter).
CREATE TABLE IF NOT EXISTS resource_locks (
    lock_id         TEXT PRIMARY KEY,
    challenge_id    TEXT NOT NULL,
    resource_key    TEXT NOT NULL,
    scope           TEXT NOT NULL,
    risk_class      TEXT,
    status          TEXT NOT NULL DEFAULT 'requested',
    owner_worker    TEXT,
    owner_intent    TEXT,
    lease_until     REAL,
    created_seq     INTEGER,
    released_seq    INTEGER,
    conflict_policy TEXT NOT NULL DEFAULT 'exclusive',
    cooldown_s      REAL NOT NULL DEFAULT 0
);
-- H: compaction epochs (audit trail of long-run graph compactions).
CREATE TABLE IF NOT EXISTS compact_epochs (
    compact_id        TEXT PRIMARY KEY,
    challenge_id      TEXT NOT NULL,
    trigger           TEXT NOT NULL,
    cutoff_seq        INTEGER NOT NULL,
    summary           TEXT NOT NULL,
    retained_fact_seqs TEXT,
    retired_intent_ids TEXT,
    stale_route_hashes TEXT,
    created_seq       INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_events_kind ON events(kind);
CREATE INDEX IF NOT EXISTS idx_intent_products_fact_seq ON intent_products(fact_seq);
"""


class SQLiteSharedGraph:
    """Local direct-SQLite implementation of SharedGraph (D)."""

    CANDIDATE_CAP_PER_SOURCE_ROUTE = 20
    # 刀7: route-LESS candidates (no route_hash) all land in one per-actor catch-all
    # bucket, so it gets a larger ceiling than a single route — but it is still
    # bounded, closing the old "route_hash IS NULL bypasses the cap entirely" leak
    # (run-75375's hottest candidate buckets were all route-less). Generous enough
    # that a productive worker emitting many distinct findings isn't starved.
    CANDIDATE_CAP_PER_SOURCE_NOROUTE = 60
    MAX_LANE_DEFERRALS = 5

    def __init__(self, db_path: str | Path, challenge: Challenge,
                 artifacts: Any = None) -> None:
        self.db_path = str(db_path)
        self.challenge = challenge
        self.artifacts = artifacts  # ArtifactStore, for the P-B gate
        Path(self.db_path).parent.mkdir(parents=True, exist_ok=True)
        # (A) one connection + one-time PRAGMA. check_same_thread=False so the
        # async solver tasks (same loop, possibly different threads) can share it;
        # we guard writes with a lock since sqlite3 module objects aren't
        # thread-safe for concurrent use on one connection.
        self._conn = sqlite3.connect(self.db_path, check_same_thread=False)
        self._lock = threading.Lock()
        cur = self._conn.cursor()
        cur.execute("PRAGMA journal_mode=WAL")
        cur.execute("PRAGMA busy_timeout=5000")      # fixes lost-write: auto-queue
        cur.execute("PRAGMA synchronous=NORMAL")     # safe + fast under WAL
        cur.execute("PRAGMA foreign_keys=ON")
        self._conn.executescript(_SCHEMA)
        self._conn.commit()
        try:
            self._conn.execute("ALTER TABLE intents ADD COLUMN to_fact_seq INTEGER")
            self._conn.commit()
        except sqlite3.OperationalError:
            pass
        # zh gist of the intent goal (deepseek-flash, written back once). Facts
        # carry their gist inside events.payload["summary"] instead (events is
        # append-only, so we patch the JSON in place — see record_fact_summary).
        try:
            self._conn.execute("ALTER TABLE intents ADD COLUMN summary TEXT")
            self._conn.commit()
        except sqlite3.OperationalError:
            pass
        for ddl in (
            "ALTER TABLE intents ADD COLUMN worker_class TEXT NOT NULL DEFAULT 'code'",
            "ALTER TABLE intents ADD COLUMN route_hash TEXT",
            "ALTER TABLE intents ADD COLUMN branch_id TEXT",
            "ALTER TABLE intents ADD COLUMN lane_key TEXT",
            "ALTER TABLE intents ADD COLUMN risk_class TEXT",
            "ALTER TABLE intents ADD COLUMN lane_deferrals INTEGER NOT NULL DEFAULT 0",
            "ALTER TABLE intents ADD COLUMN deferred_against_locked_seq INTEGER",
            "ALTER TABLE intents ADD COLUMN priority INTEGER NOT NULL DEFAULT 0",
        ):
            try:
                self._conn.execute(ddl)
                self._conn.commit()
            except sqlite3.OperationalError:
                pass
        try:
            self._conn.execute("ALTER TABLE lane_locks ADD COLUMN released_worker TEXT")
            self._conn.commit()
        except sqlite3.OperationalError:
            pass
        # A/J: dispatch_state lifecycle columns on intents (idempotent for old DBs).
        for ddl in (
            "ALTER TABLE intents ADD COLUMN dispatch_state TEXT NOT NULL DEFAULT 'active'",
            "ALTER TABLE intents ADD COLUMN close_reason TEXT",
            "ALTER TABLE intents ADD COLUMN stop_reason TEXT",
            "ALTER TABLE intents ADD COLUMN superseded_by_intent_id TEXT",
            "ALTER TABLE intents ADD COLUMN superseded_by_directive_id TEXT",
            "ALTER TABLE intents ADD COLUMN resource_key TEXT",
            "ALTER TABLE intents ADD COLUMN resource_lock_id TEXT",
            "ALTER TABLE intents ADD COLUMN compact_id TEXT",
            "ALTER TABLE intents ADD COLUMN directive_id TEXT",
            "ALTER TABLE intents ADD COLUMN result_detail TEXT",
        ):
            try:
                self._conn.execute(ddl)
                self._conn.commit()
            except sqlite3.OperationalError:
                pass
        try:
            self._conn.execute(
                "CREATE INDEX IF NOT EXISTS idx_intents_dispatch "
                "ON intents(challenge_id, dispatch_state, status, priority, created_seq)"
            )
            self._conn.commit()
        except sqlite3.OperationalError:
            pass

    def _table_exists(self, name: str) -> bool:
        with self._lock:
            row = self._conn.execute(
                "SELECT 1 FROM sqlite_master WHERE type='table' AND name=?",
                (name,),
            ).fetchone()
        return row is not None

    # ── classmethod ctor ────────────────────────────────────────────────
    @classmethod
    def open(cls, *, db_path: str | Path, challenge: Challenge,
             artifacts: Any = None) -> "SQLiteSharedGraph":
        return cls(db_path, challenge, artifacts)

    @classmethod
    def open_readonly(cls, *, db_path: str | Path, challenge: Challenge) -> "SQLiteSharedGraph":
        """TRUE read-only open for observer paths (btw side-query, replay QA).

        Unlike `open()`, this NEVER: creates the parent dir, sets WAL, runs the
        schema/migration script, or commits. It opens the existing DB file in
        SQLite `mode=ro` + `query_only=ON` so even a buggy caller cannot write.

        Assumes the DB was already initialised by a prior `open()` (true for any
        run that has reached the coordination phase). If the file is missing or
        the schema is absent/stale, raises sqlite3.OperationalError — callers
        must catch and degrade to a minimal context, NOT attempt migration.

        WAL sidecar note: `mode=ro` reads an active WAL DB fine when `-wal`/`-shm`
        exist; if they are missing on a live run the read may fail or miss
        un-checkpointed rows. Callers should treat any OperationalError as
        "graph temporarily unreadable" and degrade, never as a reason to open RW.
        """
        p = str(db_path)
        uri = f"file:{p}?mode=ro"
        inst = cls.__new__(cls)
        inst.db_path = p
        inst.challenge = challenge
        inst.artifacts = None
        inst._lock = threading.Lock()
        # URI mode=ro: open the file read-only at the SQLite VFS layer.
        inst._conn = sqlite3.connect(uri, uri=True, check_same_thread=False)
        cur = inst._conn.cursor()
        # query_only blocks ANY write DDL/DML even if a caller tried; belt+suspenders
        # on top of mode=ro. These PRAGMAs are read-only-safe.
        cur.execute("PRAGMA query_only=ON")
        cur.execute("PRAGMA busy_timeout=3000")
        return inst

    def close(self) -> None:
        with self._lock:
            self._conn.close()

    # ── append (C: INSERT only) ─────────────────────────────────────────
    def _append(self, kind: str, actor: str, payload: dict, *,
                artifact_id: Optional[str] = None, verified: bool = False,
                confidence: float = 1.0, dedupe_key: Optional[str] = None) -> int:
        with self._lock:
            try:
                cur = self._conn.execute(
                    "INSERT INTO events "
                    "(ts, challenge_id, actor, kind, payload, artifact_id, "
                    " verified, confidence, dedupe_key) "
                    "VALUES (?,?,?,?,?,?,?,?,?)",
                    (time.time(), self.challenge.id, actor, kind,
                     json.dumps(payload, default=str), artifact_id,
                     int(verified), float(confidence), dedupe_key),
                )
                self._conn.commit()
                return int(cur.lastrowid or 0)
            except sqlite3.IntegrityError:
                # dedupe_key collision → same event already appended; no-op.
                self._conn.rollback()
                return -1

    def add_evidence(self, *, actor: str, source: str, fact: str,
                     artifact_id: Optional[str] = None, verified: bool = False,
                     confidence: float = 1.0, witness: Optional[str] = None,
                     verifier: str = "", route_hash: str = "",
                     intent_id: Optional[str] = None) -> int:
        route = self.normalize_route_hash(route_hash) if route_hash else ""
        if not verified:
            if route:
                with self._lock:
                    row = self._conn.execute(
                        "SELECT COUNT(*) FROM events "
                        "WHERE challenge_id=? AND kind=? AND actor=? AND verified=0 "
                        "AND json_extract(payload,'$.route_hash')=?",
                        (self.challenge.id, EV_FACT_ADDED, actor, route),
                    ).fetchone()
                if int(row[0] if row else 0) >= self.CANDIDATE_CAP_PER_SOURCE_ROUTE:
                    return -1
            else:
                # 刀7: route-less candidates used to skip the cap entirely. Bound the
                # per-actor catch-all bucket (route_hash absent/NULL) too.
                with self._lock:
                    row = self._conn.execute(
                        "SELECT COUNT(*) FROM events "
                        "WHERE challenge_id=? AND kind=? AND actor=? AND verified=0 "
                        "AND (json_extract(payload,'$.route_hash') IS NULL "
                        "     OR json_extract(payload,'$.route_hash')='')",
                        (self.challenge.id, EV_FACT_ADDED, actor),
                    ).fetchone()
                if int(row[0] if row else 0) >= self.CANDIDATE_CAP_PER_SOURCE_NOROUTE:
                    return -1
        payload = {"source": source, "fact": fact, "source_solver": actor,
                   "witness": witness, "verifier": verifier}
        if route:
            payload["route_hash"] = route
        iid = (intent_id or "").strip()
        if iid:
            payload["intent_id"] = iid
        # dedupe on fact IDENTITY (who-said-what), normalized to collapse the
        # skill/marker double-write: strip the "[engine]" tag, fold whitespace, drop
        # case; artifact_id is NOT part of identity. So a worker's bare verified skill
        # fact and its prefixed VERIFIED_FACT marker echo land on ONE key.
        dk = f"fact::{actor}::{_normalize_fact_identity(fact)}"
        seq = self._append(EV_FACT_ADDED, actor, payload,
                           artifact_id=artifact_id, verified=verified,
                           confidence=confidence, dedupe_key=dk)
        if seq <= 0:
            # collided with an existing fact of the same identity. The echo is dropped
            # — but if THIS write is verified and the row already there is a candidate,
            # promote it (guards the rare case where the marker candidate raced in
            # before the skill's verified copy). Then attach the product edge below.
            with self._lock:
                row = self._conn.execute(
                    "SELECT seq, verified FROM events WHERE challenge_id=? AND kind=? "
                    "AND dedupe_key=? ORDER BY seq LIMIT 1",
                    (self.challenge.id, EV_FACT_ADDED, dk),
                ).fetchone()
            if row and verified and not int(row[1] or 0):
                with self._lock:
                    self._conn.execute(
                        "UPDATE events SET verified=1, confidence=? WHERE seq=?",
                        (float(confidence), int(row[0])),
                    )
                    self._conn.commit()
        product_seq = seq
        if product_seq <= 0 and iid:
            with self._lock:
                row = self._conn.execute(
                    "SELECT seq FROM events WHERE challenge_id=? AND kind=? "
                    "AND dedupe_key=? ORDER BY seq LIMIT 1",
                    (self.challenge.id, EV_FACT_ADDED, dk),
                ).fetchone()
            product_seq = int(row[0]) if row else -1
        if product_seq > 0 and iid:
            with self._lock:
                self._conn.execute(
                    "INSERT OR IGNORE INTO intent_products "
                    "(intent_id, fact_seq) VALUES (?,?)",
                    (iid, product_seq),
                )
                self._conn.commit()
        return seq

    def add_dead_end(self, *, actor: str, reason: str) -> int:
        if self._has_near_duplicate_dead_end(reason):
            return -1
        return self._append(EV_DEAD_END, actor, {"reason": reason},
                            dedupe_key=f"deadend::{reason}")

    @staticmethod
    def _norm_dead_end_text(text: str) -> str:
        s = (text or "").strip().lower()
        s = re.sub(r"\bthree\b", "3", s)
        s = re.sub(r"\btwo\b", "2", s)
        s = re.sub(r"\bone\b", "1", s)
        s = re.sub(r"[^a-z0-9]+", " ", s)
        return re.sub(r"\s+", " ", s).strip()

    def _has_near_duplicate_dead_end(self, reason: str, *, threshold: float = 0.92) -> bool:
        target = self._norm_dead_end_text(reason)
        if not target:
            return False
        target_nums = set(re.findall(r"\b\d+\b", target))
        with self._lock:
            rows = self._conn.execute(
                "SELECT json_extract(payload,'$.reason') FROM events "
                "WHERE challenge_id=? AND kind=? ORDER BY seq DESC LIMIT 200",
                (self.challenge.id, EV_DEAD_END),
            ).fetchall()
        for (old_reason,) in rows:
            old = self._norm_dead_end_text(str(old_reason or ""))
            if not old:
                continue
            old_nums = set(re.findall(r"\b\d+\b", old))
            if target_nums != old_nums:
                continue
            if SequenceMatcher(None, target, old).ratio() >= threshold:
                return True
        return False

    def flag_found(self, *, actor: str, flag: str,
                   artifact_id: Optional[str] = None,
                   intent_id: Optional[str] = None) -> int:
        payload = {"flag": flag}
        if intent_id:
            payload["intent_id"] = intent_id
        return self._append(EV_FLAG_FOUND, actor, payload,
                            artifact_id=artifact_id, verified=True,
                            dedupe_key=f"flag::{flag}")

    # ── review-arbiter events/state ────────────────────────────────────
    _ROUTE_STOPWORDS = {
        "the", "a", "an", "to", "of", "for", "on", "in", "at", "and",
        "or", "with", "via", "try", "test", "probe", "inspect", "attack",
        "exploit", "route", "path", "endpoint", "issue",
    }
    _ROUTE_ALIAS = (
        (re.compile(r"\bsql\s+injection\b|\bunion\s+(?:select\s+)?(?:payload|sqli)\b", re.I), "sqli"),
        (re.compile(r"\bcross\s+site\s+scripting\b|\bxss\b", re.I), "xss"),
        (re.compile(r"\bserver\s+side\s+request\s+forgery\b|\bssrf\b", re.I), "ssrf"),
        (re.compile(r"\bserver\s+side\s+template\s+injection\b|\bssti\b", re.I), "ssti"),
        (re.compile(r"\bpath\s+traversal\b|\bdirectory\s+traversal\b", re.I), "traversal"),
        (re.compile(r"\bfile\s+upload\b|\bupload\b", re.I), "upload"),
        (re.compile(r"\bjson\s+web\s+token\b|\bjwts?\b", re.I), "jwt"),
        (re.compile(r"\bcommand\s+injection\b|\bcmdi\b", re.I), "cmdi"),
    )

    @classmethod
    def normalize_route_hash(cls, route_hash: str, *, label: str = "") -> str:
        raw = (route_hash or label or "").strip().lower()
        for rx, repl in cls._ROUTE_ALIAS:
            raw = rx.sub(repl, raw)
        parts = [
            p for p in re.findall(r"[a-z0-9]+", raw)
            if p and p not in cls._ROUTE_STOPWORDS
        ]
        if not parts:
            h = hashlib.sha1(raw.encode("utf-8", "ignore")).hexdigest()[:10]
            return f"route:{h}"
        return ":".join(parts[:6])

    @staticmethod
    def normalize_lane_key(lane_key: str) -> str:
        raw = (lane_key or "").strip().lower()
        raw = re.sub(r"\s+", "", raw)
        raw = raw.replace("://", ":")
        raw = re.sub(r"[^a-z0-9_:@.*-]+", "-", raw).strip("-")
        if not raw:
            return ""
        m = re.match(r"^(?P<risk>[a-z0-9_]+):(?P<proto>[a-z0-9_]+):(?P<port>[0-9*]+)@(?P<host>.+)$", raw)
        if not m:
            return raw[:180]
        risk = _clean_lane_risk(m.group("risk"))
        proto = m.group("proto") or "tcp"
        port = m.group("port") or "*"
        host = m.group("host").strip("[]")
        return f"{risk}:{proto}:{port}@{host}"[:180]

    @staticmethod
    def _safe_review_severity(value: str) -> str:
        v = (value or "info").strip().lower()
        return v if v in {"info", "warn", "blocker"} else "warn"

    def add_review_finding(self, *, actor: str, kind: str, severity: str,
                           summary: str, evidence_seqs: Optional[list[int]] = None,
                           intent_ids: Optional[list[str]] = None,
                           route_hash: str = "", branch_id: str = "",
                           recommended_actions: Optional[list[str]] = None) -> int:
        route = self.normalize_route_hash(route_hash) if route_hash else ""
        fid_seed = f"{kind}:{summary}:{route}:{time.time()}"
        payload = {
            "finding_id": f"rvw-{hashlib.sha1(fid_seed.encode()).hexdigest()[:10]}",
            "kind": (kind or "no_action").strip() or "no_action",
            "severity": self._safe_review_severity(severity),
            "summary": (summary or "").strip()[:1000],
            "evidence_seqs": [int(x) for x in (evidence_seqs or []) if isinstance(x, int)],
            "intent_ids": [str(x) for x in (intent_ids or []) if x],
            "route_hash": route,
            "branch_id": (branch_id or "").strip(),
            "recommended_actions": [str(x) for x in (recommended_actions or []) if x],
        }
        return self._append(EV_REVIEW_FINDING, actor, payload,
                            dedupe_key=f"review::{payload['kind']}::{payload['summary']}::{route}")

    @staticmethod
    def _review_proposal_tier(marker: str) -> str:
        m = (marker or "").strip().upper()
        if m in {"ROUTE_SUPPRESS", "COORDINATOR_DIRECTIVE", "LANE_LOCK", "LANE_UNLOCK"}:
            return "tier2"
        return "tier1"

    def add_review_proposal(self, *, actor: str, marker: str, payload: dict,
                            tier: str = "tier1") -> int:
        marker = (marker or "").strip().upper()
        clean_payload = dict(payload or {})
        route_hash = str(clean_payload.get("route_hash") or "").strip()
        if route_hash:
            clean_payload["route_hash"] = self.normalize_route_hash(route_hash)
        lane_key = str(clean_payload.get("lane_key") or "").strip()
        if lane_key:
            clean_payload["lane_key"] = self.normalize_lane_key(lane_key)
        confidence = clean_payload.get("confidence", 1.0)
        try:
            confidence = float(confidence)
        except (TypeError, ValueError):
            confidence = 1.0
        clean_payload["confidence"] = max(0.0, min(1.0, confidence))
        clean_tier = tier if tier in {"tier1", "tier2"} else self._review_proposal_tier(marker)
        payload_out = {
            "marker": marker,
            "tier": clean_tier,
            "payload": clean_payload,
            "status": "pending",
        }
        fp = json.dumps(clean_payload, sort_keys=True, ensure_ascii=False, default=str)
        return self._append(EV_REVIEW_PROPOSAL, actor, payload_out,
                            dedupe_key=f"review-proposal::{marker}::{hashlib.sha1(fp.encode()).hexdigest()}")

    def decide_review_proposal(self, *, actor: str, proposal_seq: int,
                               decision: str, reason: str = "",
                               applied_seq: Optional[int] = None) -> int:
        clean_decision = (decision or "deferred").strip().lower()
        if clean_decision not in {"accepted", "deferred", "rejected"}:
            clean_decision = "deferred"
        payload = {
            "proposal_seq": int(proposal_seq),
            "decision": clean_decision,
            "reason": (reason or "").strip()[:1000],
        }
        if applied_seq is not None:
            payload["applied_seq"] = int(applied_seq)
        return self._append(
            EV_REVIEW_PROPOSAL_DECISION, actor, payload,
            dedupe_key=f"review-proposal-decision::{proposal_seq}::{clean_decision}",
        )

    def challenge_fact(self, *, actor: str, fact_seq: int, reason: str,
                       verification_goal: str) -> dict:
        fact_seq = int(fact_seq)
        goal = (verification_goal or f"Verify fact #{fact_seq}: {reason}").strip()
        h = hashlib.sha1(f"{fact_seq}:{goal}".encode("utf-8", "ignore")).hexdigest()[:8]
        intent_id = f"I-verify-{fact_seq}-{h}"
        payload = {
            "fact_seq": fact_seq,
            "status": "challenged",
            "reason": (reason or "").strip()[:1000],
            "challenged_by": actor,
            "verification_intent_id": intent_id,
        }
        seq = self._append(EV_FACT_CHALLENGED, actor, payload,
                           dedupe_key=f"fact-challenged::{fact_seq}::{payload['reason']}")
        with self._lock:
            self._conn.execute(
                "INSERT INTO fact_reviews "
                "(fact_seq, challenge_id, status, challenged_seq, reason, verification_intent_id) "
                "VALUES (?,?,?,?,?,?) "
                "ON CONFLICT(fact_seq) DO UPDATE SET "
                " status='challenged', challenged_seq=excluded.challenged_seq, "
                " reason=excluded.reason, verification_intent_id=excluded.verification_intent_id",
                (fact_seq, self.challenge.id, "challenged",
                 seq if seq > 0 else None, payload["reason"], intent_id),
            )
            self._upsert_fact_state(
                fact_seq, FACT_STATE_CHALLENGED,
                reason=payload["reason"], challenged_seq=seq if seq > 0 else None,
                verification_intent_id=intent_id,
                verified_effective=0, confidence_effective=0.4, updated_seq=seq)
            self._conn.commit()
        self.propose_intent(
            actor=actor, intent_id=intent_id, goal=goal,
            payload={"worker_class": "verifier", "depends_on": [str(fact_seq)],
                     "rationale": f"Review challenged fact #{fact_seq}: {reason}"},
            from_fact_seqs=[fact_seq],
        )
        return {"fact_seq": fact_seq, "verification_intent_id": intent_id,
                "seq": seq, "reason": payload["reason"]}

    def revalidate_fact(self, *, actor: str, fact_seq: int, reason: str = "") -> int:
        fact_seq = int(fact_seq)
        payload = {
            "fact_seq": fact_seq,
            "status": "revalidated",
            "reason": (reason or "").strip()[:1000],
            "revalidated_by": actor,
        }
        seq = self._append(EV_FACT_REVALIDATED, actor, payload,
                           dedupe_key=f"fact-revalidated::{fact_seq}::{payload['reason']}")
        # revalidate effectively restores the fact's verified verdict (defect-4: the
        # legacy path wrote status but the snapshot still leaned on events.verified).
        orig_verified, orig_conf = self._fact_origin_verdict(fact_seq)
        with self._lock:
            self._conn.execute(
                "INSERT INTO fact_reviews "
                "(fact_seq, challenge_id, status, revalidated_seq, reason) "
                "VALUES (?,?,?,?,?) "
                "ON CONFLICT(fact_seq) DO UPDATE SET "
                " status='revalidated', revalidated_seq=excluded.revalidated_seq, "
                " reason=excluded.reason",
                (fact_seq, self.challenge.id, "revalidated",
                 seq if seq > 0 else None, payload["reason"]),
            )
            self._upsert_fact_state(
                fact_seq, FACT_STATE_REVALIDATED, reason=payload["reason"],
                revalidated_seq=seq if seq > 0 else None,
                verified_effective=1 if orig_verified else 0,
                confidence_effective=orig_conf, updated_seq=seq)
            self._conn.commit()
        return seq

    # ── A: fact lifecycle (reject / merge / supersede) ──────────────────
    def _fact_origin_verdict(self, fact_seq: int) -> tuple[bool, float]:
        """The fact's original verified/confidence from the append-only event."""
        with self._lock:
            row = self._conn.execute(
                "SELECT verified, confidence FROM events WHERE seq=? AND kind=?",
                (int(fact_seq), EV_FACT_ADDED),
            ).fetchone()
        if not row:
            return (False, 0.0)
        return (bool(row[0]), float(row[1] if row[1] is not None else 1.0))

    def _upsert_fact_state(self, fact_seq: int, state: str, *,
                           reason: str = "", verified_effective: Optional[int] = None,
                           confidence_effective: Optional[float] = None,
                           challenged_seq: Optional[int] = None,
                           revalidated_seq: Optional[int] = None,
                           rejected_seq: Optional[int] = None,
                           merged_seq: Optional[int] = None,
                           superseded_seq: Optional[int] = None,
                           retired_seq: Optional[int] = None,
                           verification_intent_id: Optional[str] = None,
                           updated_seq: Optional[int] = None) -> None:
        """Write the current lifecycle state for a fact. Caller holds self._lock.

        Only non-None transition seqs / effective verdicts overwrite existing
        columns (COALESCE), so a later reject doesn't blank an earlier challenge's
        challenged_seq. `state`, `reason`, and effective verdicts always win."""
        state = state if state in _FACT_STATES else FACT_STATE_UNRESOLVED
        self._conn.execute(
            "INSERT INTO fact_states "
            "(fact_seq, challenge_id, state, verified_effective, confidence_effective, "
            " reason, challenged_seq, revalidated_seq, rejected_seq, merged_seq, "
            " superseded_seq, retired_seq, verification_intent_id, updated_seq) "
            "VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?) "
            "ON CONFLICT(fact_seq) DO UPDATE SET "
            " state=excluded.state, reason=excluded.reason, "
            " verified_effective=COALESCE(excluded.verified_effective, fact_states.verified_effective), "
            " confidence_effective=COALESCE(excluded.confidence_effective, fact_states.confidence_effective), "
            " challenged_seq=COALESCE(excluded.challenged_seq, fact_states.challenged_seq), "
            " revalidated_seq=COALESCE(excluded.revalidated_seq, fact_states.revalidated_seq), "
            " rejected_seq=COALESCE(excluded.rejected_seq, fact_states.rejected_seq), "
            " merged_seq=COALESCE(excluded.merged_seq, fact_states.merged_seq), "
            " superseded_seq=COALESCE(excluded.superseded_seq, fact_states.superseded_seq), "
            " retired_seq=COALESCE(excluded.retired_seq, fact_states.retired_seq), "
            " verification_intent_id=COALESCE(excluded.verification_intent_id, fact_states.verification_intent_id), "
            " updated_seq=COALESCE(excluded.updated_seq, fact_states.updated_seq)",
            (int(fact_seq), self.challenge.id, state, verified_effective,
             confidence_effective, reason or None, challenged_seq, revalidated_seq,
             rejected_seq, merged_seq, superseded_seq, retired_seq,
             verification_intent_id, updated_seq),
        )

    def reject_fact(self, *, actor: str, fact_seq: int, reason: str = "") -> int:
        """Mark a fact REJECTED — review proved it false. It is retired from the
        active candidate set and excluded from snapshots / Reason summaries, but the
        originating event stays (audit trail)."""
        fact_seq = int(fact_seq)
        payload = {"fact_seq": fact_seq, "status": FACT_STATE_REJECTED,
                   "reason": (reason or "").strip()[:1000], "rejected_by": actor}
        seq = self._append(EV_FACT_REJECTED, actor, payload,
                           dedupe_key=f"fact-rejected::{fact_seq}::{payload['reason']}")
        with self._lock:
            self._conn.execute(
                "INSERT INTO fact_reviews (fact_seq, challenge_id, status, reason) "
                "VALUES (?,?,?,?) ON CONFLICT(fact_seq) DO UPDATE SET "
                " status='rejected', reason=excluded.reason",
                (fact_seq, self.challenge.id, FACT_STATE_REJECTED, payload["reason"]),
            )
            self._upsert_fact_state(
                fact_seq, FACT_STATE_REJECTED, reason=payload["reason"],
                rejected_seq=seq if seq > 0 else None, retired_seq=seq if seq > 0 else None,
                verified_effective=0, confidence_effective=0.0, updated_seq=seq)
            self._conn.commit()
        return seq

    def merge_fact(self, *, actor: str, from_fact_seq: int, to_fact_seq: int,
                   reason: str = "") -> int:
        """Fold `from_fact_seq` into `to_fact_seq` — they describe the same finding.
        The from-fact is retired (merged) and the merge edge recorded."""
        from_seq, to_seq = int(from_fact_seq), int(to_fact_seq)
        if from_seq == to_seq:
            return -1
        payload = {"from_fact_seq": from_seq, "to_fact_seq": to_seq,
                   "status": FACT_STATE_MERGED, "reason": (reason or "").strip()[:1000],
                   "merged_by": actor}
        seq = self._append(EV_FACT_MERGED, actor, payload,
                           dedupe_key=f"fact-merged::{from_seq}::{to_seq}")
        with self._lock:
            self._conn.execute(
                "INSERT OR IGNORE INTO fact_merges "
                "(from_fact_seq, to_fact_seq, challenge_id, merge_seq, reason) "
                "VALUES (?,?,?,?,?)",
                (from_seq, to_seq, self.challenge.id, seq if seq > 0 else 0,
                 payload["reason"]),
            )
            self._conn.execute(
                "INSERT INTO fact_reviews (fact_seq, challenge_id, status, reason) "
                "VALUES (?,?,?,?) ON CONFLICT(fact_seq) DO UPDATE SET "
                " status='merged', reason=excluded.reason",
                (from_seq, self.challenge.id, FACT_STATE_MERGED, payload["reason"]),
            )
            self._upsert_fact_state(
                from_seq, FACT_STATE_MERGED, reason=payload["reason"],
                merged_seq=seq if seq > 0 else None, retired_seq=seq if seq > 0 else None,
                verified_effective=0, updated_seq=seq)
            self._conn.commit()
        return seq

    def supersede_fact(self, *, actor: str, fact_seq: int, reason: str = "",
                       by_fact_seq: Optional[int] = None) -> int:
        """Mark a fact SUPERSEDED — a newer fact replaces it. Retired from the
        active set; kept for audit."""
        fact_seq = int(fact_seq)
        payload = {"fact_seq": fact_seq, "status": FACT_STATE_SUPERSEDED,
                   "reason": (reason or "").strip()[:1000], "superseded_by": actor}
        if by_fact_seq is not None:
            payload["by_fact_seq"] = int(by_fact_seq)
        seq = self._append(EV_FACT_SUPERSEDED, actor, payload,
                           dedupe_key=f"fact-superseded::{fact_seq}::{payload['reason']}")
        with self._lock:
            self._conn.execute(
                "INSERT INTO fact_reviews (fact_seq, challenge_id, status, reason) "
                "VALUES (?,?,?,?) ON CONFLICT(fact_seq) DO UPDATE SET "
                " status='superseded', reason=excluded.reason",
                (fact_seq, self.challenge.id, FACT_STATE_SUPERSEDED, payload["reason"]),
            )
            self._upsert_fact_state(
                fact_seq, FACT_STATE_SUPERSEDED, reason=payload["reason"],
                superseded_seq=seq if seq > 0 else None, retired_seq=seq if seq > 0 else None,
                verified_effective=0, updated_seq=seq)
            self._conn.commit()
        return seq

    def review_fact(self, *, actor: str, fact_seq: int, action: str,
                    reason: str = "", verification_goal: str = "",
                    to_fact_seq: Optional[int] = None) -> dict:
        """Unified fact review dispatcher (challenge/revalidate/reject/merge/supersede).
        Returns {action, fact_seq, seq}."""
        act = (action or "").strip().lower()
        if act in ("challenge", "challenged"):
            res = self.challenge_fact(actor=actor, fact_seq=fact_seq, reason=reason,
                                      verification_goal=verification_goal)
            return {"action": "challenge", "fact_seq": int(fact_seq),
                    "seq": int(res.get("seq") or 0)}
        if act in ("revalidate", "revalidated"):
            seq = self.revalidate_fact(actor=actor, fact_seq=fact_seq, reason=reason)
            return {"action": "revalidate", "fact_seq": int(fact_seq), "seq": seq}
        if act in ("reject", "rejected"):
            seq = self.reject_fact(actor=actor, fact_seq=fact_seq, reason=reason)
            return {"action": "reject", "fact_seq": int(fact_seq), "seq": seq}
        if act in ("merge", "merged"):
            seq = self.merge_fact(actor=actor, from_fact_seq=fact_seq,
                                  to_fact_seq=int(to_fact_seq or 0), reason=reason)
            return {"action": "merge", "fact_seq": int(fact_seq), "seq": seq}
        if act in ("supersede", "superseded"):
            seq = self.supersede_fact(actor=actor, fact_seq=fact_seq, reason=reason,
                                      by_fact_seq=to_fact_seq)
            return {"action": "supersede", "fact_seq": int(fact_seq), "seq": seq}
        return {"action": act, "fact_seq": int(fact_seq), "seq": -1}

    def _fact_state_map(self) -> dict[int, dict]:
        """fact_seq → {state, verified_effective, confidence_effective, retired}."""
        if not self._table_exists("fact_states"):
            return {}
        with self._lock:
            rows = self._conn.execute(
                "SELECT fact_seq, state, verified_effective, confidence_effective, "
                "retired_seq FROM fact_states WHERE challenge_id=?",
                (self.challenge.id,),
            ).fetchall()
        return {
            int(r[0]): {
                "state": str(r[1] or FACT_STATE_UNRESOLVED),
                "verified_effective": (None if r[2] is None else bool(r[2])),
                "confidence_effective": (None if r[3] is None else float(r[3])),
                "retired": r[4] is not None,
            }
            for r in rows
        }

    def active_candidates(self) -> list[dict]:
        """Active (non-retired) UNRESOLVED/CHALLENGED candidate facts — the set the
        planner should still weigh. Excludes verified, rejected, merged, superseded."""
        texts = self._fact_text_by_seq()
        states = self._fact_state_map()
        with self._lock:
            rows = self._conn.execute(
                "SELECT seq, verified FROM events WHERE challenge_id=? AND kind=? "
                "ORDER BY seq",
                (self.challenge.id, EV_FACT_ADDED),
            ).fetchall()
        out: list[dict] = []
        for seq, verified in rows:
            seq = int(seq)
            st = states.get(seq, {})
            state = st.get("state", FACT_STATE_UNRESOLVED)
            if st.get("retired") or state in _FACT_TERMINAL_STATES:
                continue
            eff = st.get("verified_effective")
            is_verified = bool(verified) if eff is None else eff
            if is_verified and state != FACT_STATE_CHALLENGED:
                continue
            out.append({"fact_seq": seq, "fact": texts.get(seq, ""), "state": state})
        return out

    def verified_evidence(self) -> list[dict]:
        """Facts that are verified (origin or revalidated) AND not retired."""
        texts = self._fact_text_by_seq()
        states = self._fact_state_map()
        with self._lock:
            rows = self._conn.execute(
                "SELECT seq, verified FROM events WHERE challenge_id=? AND kind=? "
                "ORDER BY seq",
                (self.challenge.id, EV_FACT_ADDED),
            ).fetchall()
        out: list[dict] = []
        for seq, verified in rows:
            seq = int(seq)
            st = states.get(seq, {})
            if st.get("retired") or st.get("state") in _FACT_TERMINAL_STATES:
                continue
            eff = st.get("verified_effective")
            is_verified = bool(verified) if eff is None else eff
            if is_verified:
                out.append({"fact_seq": seq, "fact": texts.get(seq, "")})
        return out

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
                               priority: Optional[int] = None) -> dict:
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
            where += " AND status NOT IN ('superseded','expired','rejected')"
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

    def active_operator_directive_texts(self) -> list[str]:
        """The directive texts the planner must prioritize (highest-priority first)."""
        return [d["text"] for d in self.operator_directives(active_only=True) if d.get("text")]

    # ── F: classified HITL requests (need_kind drives auto vs operator pause) ──
    def add_hitl_request(self, *, worker: str, need: str, need_kind: str,
                         classification_confidence: float = 1.0,
                         status: str = "classified",
                         directive_id: Optional[str] = None,
                         resource_lock_id: Optional[str] = None,
                         auto_action_seq: Optional[int] = None) -> dict:
        """F: record a classified worker hand-raise. need_kind decides downstream
        handling (external_blocker pauses; the others auto-resolve)."""
        nk = (need_kind or "external_blocker").strip()
        digest = hashlib.sha1(
            f"{worker}:{need}:{nk}".encode("utf-8", "ignore")).hexdigest()[:10]
        request_id = f"H-{digest}"
        payload = {"request_id": request_id, "worker": worker, "need": (need or "")[:1000],
                   "need_kind": nk, "status": status,
                   "classification_confidence": float(classification_confidence)}
        seq = self._append(EV_HITL_CLASSIFIED, worker or "worker", payload,
                           dedupe_key=f"hitl::{request_id}")
        with self._lock:
            self._conn.execute(
                "INSERT OR IGNORE INTO hitl_requests "
                "(request_id, challenge_id, worker, need, need_kind, "
                " classification_confidence, status, auto_action_seq, directive_id, "
                " resource_lock_id, created_seq) "
                "VALUES (?,?,?,?,?,?,?,?,?,?,?)",
                (request_id, self.challenge.id, worker or "worker", (need or "")[:1000],
                 nk, float(classification_confidence), status,
                 auto_action_seq, directive_id, resource_lock_id, seq if seq > 0 else 0),
            )
            self._conn.commit()
        return {"request_id": request_id, "seq": seq, "need_kind": nk}

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
        seq = self._append(
            EV_LANE_RELEASED,
            actor,
            {"lane_key": lane, "risk_class": risk, "released_worker": owner,
             "owner_intent": str(row[1] or ""), "released_by": by or actor},
        )
        with self._lock:
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

        - solved          → close active+claimed intents; close open branches.
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
        if terminal_reason == "solved":
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
                     "result": "closed_by_solve"},
                )
                with self._lock:
                    q = ",".join("?" for _ in closed_intents)
                    self._conn.execute(
                        f"UPDATE intents SET status='done', dispatch_state='closed', "
                        f"close_reason='closed_by_solve', stop_reason='solved', "
                        f"result_seq=? WHERE challenge_id=? AND intent_id IN ({q})",
                        (result_seq if result_seq > 0 else None,
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
        seq = self._append(EV_INTENT_PROPOSED, actor,
                          {"intent_id": intent_id, "goal": goal,
                           **payload, "worker_class": worker_class,
                           "route_hash": route_hash, "branch_id": branch_id,
                           "lane_key": lane_key, "risk_class": risk_class,
                           "resource_key": resource_key, "directive_id": directive_id,
                           "priority": priority},
                          dedupe_key=f"intent::{intent_id}")
        with self._lock:
            self._conn.execute(
                "INSERT OR IGNORE INTO intents "
                "(intent_id, challenge_id, goal, worker_class, route_hash, branch_id, "
                " lane_key, risk_class, priority, status, dispatch_state, created_seq, "
                " resource_key, directive_id) "
                "VALUES (?,?,?,?,?,?,?,?,?,'open','active',?,?,?)",
                (intent_id, self.challenge.id, goal, worker_class,
                 route_hash or None, branch_id or None,
                 lane_key or None, risk_class if lane_key else None, priority,
                 seq if seq > 0 else 0, resource_key or None, directive_id or None),
            )
            if from_fact_seqs:
                for fs in from_fact_seqs:
                    self._conn.execute(
                        "INSERT OR IGNORE INTO intent_sources "
                        "(intent_id, fact_seq) VALUES (?,?)",
                        (intent_id, fs),
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
                "  AND (status='open' OR (status='claimed' AND lease_until < ?))",
                (worker, now + lease_s, intent_id, self.challenge.id, now),
            )
            self._conn.commit()
            won = cur.rowcount == 1
        if won:
            self._append(EV_INTENT_CLAIMED, worker, {"intent_id": intent_id})
        return won

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

    # ── read paths ──────────────────────────────────────────────────────
    def invalidated_flags(self) -> set[str]:
        """Every flag the operator marked false (an EV_FLAG_INVALIDATED event).

        snapshot().flags already excludes these; this accessor lets the coordinator
        DROP a stale in-memory flag during reconciliation so a blacklisted flag can
        never count toward expected_flags (BUG③ cross-check). Cheap, read-only."""
        out: set[str] = set()
        with self._lock:
            rows = self._conn.execute(
                "SELECT payload FROM events WHERE challenge_id=? AND kind=?",
                (self.challenge.id, EV_FLAG_INVALIDATED),
            ).fetchall()
        for (payload,) in rows:
            try:
                bad = (json.loads(payload) or {}).get("flag")
            except Exception:
                bad = None
            if bad:
                out.add(bad)
        return out

    def events(self) -> list[dict]:
        with self._lock:
            rows = self._conn.execute(
                "SELECT seq, ts, actor, kind, payload, artifact_id, verified, "
                "confidence FROM events ORDER BY seq"
            ).fetchall()
        out = []
        for seq, ts, actor, kind, payload, aid, verified, conf in rows:
            out.append({"seq": seq, "ts": ts, "actor": actor, "kind": kind,
                        "payload": json.loads(payload), "artifact_id": aid,
                        "verified": bool(verified), "confidence": conf})
        return out

    def recent_events(self, limit: int = 40) -> list[dict]:
        """Last `limit` events, oldest-first, filtered to this challenge.

        Unlike `events()[-limit:]` this is bounded at the SQL layer (no full-table
        scan) and scopes to challenge_id so a shared sessions DB stays correct.
        Used by the read-only btw observer to build a recent timeline snapshot.
        """
        if limit <= 0:
            return []
        with self._lock:
            rows = self._conn.execute(
                "SELECT seq, ts, actor, kind, payload, artifact_id, verified, "
                "confidence FROM events WHERE challenge_id=? "
                "ORDER BY seq DESC LIMIT ?",
                (self.challenge.id, int(limit)),
            ).fetchall()
        out = []
        for seq, ts, actor, kind, payload, aid, verified, conf in reversed(rows):
            try:
                p = json.loads(payload)
            except Exception:
                p = {}
            out.append({"seq": seq, "ts": ts, "actor": actor, "kind": kind,
                        "payload": p, "artifact_id": aid,
                        "verified": bool(verified), "confidence": conf})
        return out

    def events_since(self, after_seq: int, kinds: Optional[list[str]] = None) -> list[dict]:
        after = int(after_seq or 0)
        params: list[Any] = [after]
        kind_list = [str(k) for k in (kinds or []) if str(k)]
        where = "WHERE seq > ?"
        if kind_list:
            where += " AND kind IN (" + ",".join("?" for _ in kind_list) + ")"
            params.extend(kind_list)
        with self._lock:
            rows = self._conn.execute(
                "SELECT seq, ts, actor, kind, payload, artifact_id, verified, "
                f"confidence FROM events {where} ORDER BY seq",
                tuple(params),
            ).fetchall()
        out = []
        for seq, ts, actor, kind, payload, aid, verified, conf in rows:
            try:
                parsed = json.loads(payload)
            except (json.JSONDecodeError, TypeError):
                parsed = {}
            out.append({"seq": seq, "ts": ts, "actor": actor, "kind": kind,
                        "payload": parsed, "artifact_id": aid,
                        "verified": bool(verified), "confidence": conf})
        return out

    def intent_products(self, intent_id: str) -> list[int]:
        with self._lock:
            rows = self._conn.execute(
                "SELECT fact_seq FROM intent_products WHERE intent_id=? ORDER BY fact_seq",
                (intent_id,),
            ).fetchall()
        return [int(r[0]) for r in rows]

    def snapshot(self) -> SolveGraph:
        """Materialize (C) the event log into a read-only SolveGraph view.

        Facts in a TERMINAL lifecycle state (rejected/merged/superseded) are
        dropped — they failed review and must not pollute the planner/worker view
        or downstream writeups. challenged stays (shown, but de-verified)."""
        g = SolveGraph(challenge=self.challenge)
        fact_reviews = self._fact_review_map()
        fact_states = self._fact_state_map()
        for e in self.events():
            p = e["payload"]
            if e["kind"] == EV_FACT_ADDED:
                seq = int(e["seq"])
                st = fact_states.get(seq, {})
                if st.get("retired") or st.get("state") in _FACT_TERMINAL_STATES:
                    continue  # A: rejected/merged/superseded facts leave the view
                status = fact_reviews.get(seq)
                verified = bool(e["verified"])
                confidence = e["confidence"]
                if status == "challenged":
                    verified = False
                    confidence = min(float(confidence or 0.4), 0.4)
                elif status == "revalidated":
                    eff = st.get("verified_effective")
                    verified = bool(e["verified"]) if eff is None else eff
                g.add_evidence(
                    source=p.get("source", ""), fact=p.get("fact", ""),
                    artifact_id=e["artifact_id"],
                    verified=verified, confidence=confidence,
                    source_solver=p.get("source_solver", ""),
                    witness=p.get("witness"), verifier=p.get("verifier", ""),
                )
            elif e["kind"] == EV_DEAD_END:
                g.mark_dead_end(p.get("reason", ""))
            elif e["kind"] == EV_FLAG_FOUND:
                # multi-flag: ACCUMULATE (was a last-wins overwrite that lost every
                # flag but the last). add_flag dedups + keeps flag==flags[0].
                g.add_flag(p.get("flag"))
            elif e["kind"] == EV_FLAG_INVALIDATED:
                # a false-positive flag was marked by the operator — drop just that
                # one from the set (preserving any other collected flags) AND record
                # it as permanently rejected. reject_flag is UNCONDITIONAL on the
                # rejected set: a flag invalidated here stays rejected even if its
                # EV_FLAG_FOUND has not yet replayed, or is re-emitted later by a
                # reopened worker (add_flag refuses anything in rejected_flags). This
                # closes the run-75379 invalidate→reopen→re-find→re-accept loop at the
                # durable layer — survivable across worker respawn.
                g.reject_flag(p.get("flag"))
        return g

    def to_summary(self, max_evidence: int = 16,
                   max_dead_ends: Optional[int] = None) -> str:
        """Like SolveGraph.to_summary but with [seq] labels on each fact so the
        Reason model can reference specific facts by number in its `from` field."""
        base = self.snapshot().to_summary(max_evidence=max_evidence,
                                          max_dead_ends=max_dead_ends)
        seq_map = self._fact_seq_map()
        if not seq_map:
            return base
        for fact_text, seq in seq_map.items():
            short = fact_text[:80]
            old_marker = f") {short}"
            new_marker = f") [#{seq}] {short}"
            base = base.replace(old_marker, new_marker, 1)
        return base

    def _fact_seq_map(self) -> dict[str, int]:
        """Map fact text → event seq for the most recent facts."""
        with self._lock:
            rows = self._conn.execute(
                "SELECT seq, json_extract(payload, '$.fact') "
                "FROM events WHERE kind=? ORDER BY seq",
                (EV_FACT_ADDED,),
            ).fetchall()
        return {text: seq for seq, text in rows if text}

    def _fact_text_by_seq(self, *, include_retired: bool = False) -> dict[int, str]:
        with self._lock:
            rows = self._conn.execute(
                "SELECT seq, json_extract(payload, '$.fact') "
                "FROM events WHERE kind=? ORDER BY seq",
                (EV_FACT_ADDED,),
            ).fetchall()
        active = None if include_retired else self._active_fact_seq_set()
        return {
            int(seq): str(text)
            for seq, text in rows
            if text and (active is None or int(seq) in active)
        }

    def _fact_review_map(self) -> dict[int, str]:
        with self._lock:
            rows = self._conn.execute(
                "SELECT fact_seq, status FROM fact_reviews WHERE challenge_id=?",
                (self.challenge.id,),
            ).fetchall()
        return {int(seq): str(status) for seq, status in rows}

    def _active_fact_seq_set(self) -> set[int]:
        """Fact seqs still usable as graph evidence.

        Terminal lifecycle states (rejected/merged/superseded) are audit history only:
        they must not participate in graph reachability, lineage display, or worker
        neighborhood prompts. Challenged facts remain active candidates, but their
        effective verified status is downgraded elsewhere.
        """
        states = self._fact_state_map()
        with self._lock:
            rows = self._conn.execute(
                "SELECT seq FROM events WHERE challenge_id=? AND kind=?",
                (self.challenge.id, EV_FACT_ADDED),
            ).fetchall()
        out: set[int] = set()
        for (seq_raw,) in rows:
            seq = int(seq_raw)
            st = states.get(seq, {})
            if st.get("retired") or st.get("state") in _FACT_TERMINAL_STATES:
                continue
            out.add(seq)
        return out

    def fact_seqs_for_texts(self, texts: list[str]) -> list[int]:
        """Resolve fact description strings to their event seq numbers."""
        m = self._fact_seq_map()
        return [m[t] for t in texts if t in m]

    def per_flag_evidence_chains(self) -> dict[str, list[str]]:
        """G: per-flag evidence chains for multi-flag writeups. For each captured
        flag, build the ordered VERIFIED-fact trail that led to it:

          1. INTENT-LINKED (preferred): the flag_found event carries intent_id →
             use that intent's source facts (intent_sources) + produced fact
             (to_fact_seq), in seq order. This is the precise per-flag path.
          2. TEMPORAL FALLBACK (no intent_id): every verified fact with seq < the
             flag's seq (the breadcrumb trail up to that flag's discovery).

        Single-flag runs return {flag: chain} too — the caller can fall back to the
        flat evidence_chain when only one flag exists (byte-identical behavior)."""
        # collect flag_found events (flag, seq, intent_id) and flag invalidations
        flag_events: list[tuple[str, int, str]] = []
        invalidated: set[str] = set()
        for e in self.events():
            if e["kind"] == EV_FLAG_FOUND:
                p = e["payload"] or {}
                fl = p.get("flag")
                if fl:
                    flag_events.append((fl, int(e["seq"]), str(p.get("intent_id") or "")))
            elif e["kind"] == EV_FLAG_INVALIDATED:
                bad = (e["payload"] or {}).get("flag")
                if bad:
                    invalidated.add(bad)
        if not flag_events:
            return {}
        texts = self._fact_text_by_seq()
        states = self._fact_state_map()

        def _live_verified(seq: int) -> bool:
            st = states.get(seq, {})
            if st.get("retired") or st.get("state") in _FACT_TERMINAL_STATES:
                return False
            return True

        # verified fact seqs in order (origin verified OR revalidated, not retired)
        verified_seqs = [d["fact_seq"] for d in self.verified_evidence()]
        out: dict[str, list[str]] = {}
        for flag, fseq, intent_id in flag_events:
            if flag in invalidated:
                continue
            chain_seqs: list[int] = []
            if intent_id:
                # intent-linked: source facts + produced fact for this flag's intent
                with self._lock:
                    src_rows = self._conn.execute(
                        "SELECT fact_seq FROM intent_sources WHERE intent_id=? ORDER BY fact_seq",
                        (intent_id,),
                    ).fetchall()
                    to_row = self._conn.execute(
                        "SELECT to_fact_seq FROM intents WHERE intent_id=? AND challenge_id=?",
                        (intent_id, self.challenge.id),
                    ).fetchone()
                for (s,) in src_rows:
                    if s is not None and _live_verified(int(s)):
                        chain_seqs.append(int(s))
                if to_row and to_row[0] is not None and _live_verified(int(to_row[0])):
                    chain_seqs.append(int(to_row[0]))
            if not chain_seqs:
                # temporal fallback: verified facts discovered before this flag
                chain_seqs = [s for s in verified_seqs if s <= fseq]
            # de-dup preserve order, resolve to text
            seen: set[int] = set()
            chain: list[str] = []
            for s in chain_seqs:
                if s in seen:
                    continue
                seen.add(s)
                t = texts.get(s)
                if t:
                    chain.append(t)
            out[flag] = chain[:12]
        return out

    # ── P2A: canonical credential / unlock chain (read-side, text-derived) ────
    # A long unlock-chain challenge (run-10067: 22-level SSH ladder) buries the
    # reusable passwords in 90+ free-text facts; truncation then drops them and
    # workers re-walk from ghost0. We surface the chain as a first-class section
    # derived from the fact TEXT (where the password literally appears in a
    # verified fact) — NOT from the stored from/to edges, which are untrustworthy
    # (`from` = a truncation-blinded planner's self-report; `to` = the worker's
    # closing stdout-tail summary). See DESIGN_board_file_handoff §9.
    #
    # HARD false-positive guard (DESIGN §4-P2A): a free-text board mixes "password
    # is X" with "tried X but it FAILED". Promoting a failed guess into a section
    # labelled "reuse, do NOT re-derive" is STRICTLY WORSE than truncation. So we
    # promote ONLY verified facts that carry an explicit success cue and lack a
    # failure cue, and we label the section "verify before trusting".
    _CRED_SUCCESS_CUE = re.compile(
        r"(unlock|logg?ed in|logs? in|whoami|authenticat|succe|login (?:to|succeed)|"
        r"password (?:for|is|works|valid)|cred(?:ential)?s? (?:for|is)|"
        r"pass(?:word)? (?:works|valid))", re.I)
    _CRED_FAILURE_CUE = re.compile(
        r"(fail|denied|wrong|incorrect|invalid|rejected|tried but|does ?n'?t "
        r"work|decoy|red herring|not (?:a )?(?:valid|the right) )", re.I)
    # The ENTITY being unlocked: a level/user-ish token (ghost3, level4, bandit5,
    # user1, root, admin). Anchored to known CTF-ladder prefixes OR a bare
    # well-known account, to avoid lifting random words.
    _CRED_ENTITY = re.compile(
        r"\b((?:ghost|level|bandit|krypton|natas|user|stage|node|flag|box)\s?\d{1,3}"
        r"|root|admin|administrator)\b", re.I)
    # The VALUE: the credential token introduced by a password keyword
    # ("password X", "with X", "is X", zh 密码 X) OR an explicit entity:value pair.
    _CRED_VALUE_KW = re.compile(
        r"(?:password|passwd|pass|cred(?:ential)?s?|secret|key|密码|凭据)\s*"
        r"(?:for\s+\S+\s+)?(?:is|=|:|of|was|为|->|→)?\s*"
        r"[`'\"]?([A-Za-z0-9_+/=.\-]{6,64})[`'\"]?", re.I)
    # A bare credential-shaped token: a MIXED-CASE alphanumeric ≥8 chars (looks like
    # a real password, not an English word / hex blob / decimal). Used only as a
    # last resort when a fact has a success cue + an entity but no keyword-introduced
    # value — and only if EXACTLY ONE such token exists (ambiguity → emit nothing).
    _CRED_BARE = re.compile(r"\b([A-Za-z0-9_+/=.\-]{8,64})\b")
    _CRED_PAIR = re.compile(
        r"\b((?:ghost|level|bandit|krypton|natas|user|stage)\s?\d{1,3})\s*[:=]\s*"
        r"[`'\"]?([A-Za-z0-9_+/=.\-]{6,64})[`'\"]?", re.I)
    # tokens that look credential-shaped but are noise we must never emit as a value
    _CRED_VALUE_STOP = {"password", "passwd", "secret", "succeeds", "succeeded",
                        "returns", "returned", "whoami", "authenticates", "unlocked",
                        "credential", "credentials", "logged", "login"}
    # SSH/config option assignments (PubkeyAuthentication=no, StrictHostKeyChecking=yes,
    # IdentitiesOnly=yes) look like entity:value pairs but are flags, not passwords —
    # observed as a false-positive `ghost0:Authentication=no` row in run-10070.
    _CRED_VALUE_REJECT = re.compile(
        r"(?:^|=)(?:no|yes)$|authentication|hostkey|identit|knownhosts|stricthost|"
        r"pubkey|forwarding|batchmode|connecttimeout", re.I)

    # The credential belongs to the entity it UNLOCKS, not the one whose home it was
    # found in. "X authenticates as ghost3" / "is the ghost3 password" / "unlocks
    # ghost3" / "认证 ghost3" → the TARGET entity is ghost3, even if the fact opens
    # with "ghost2 hidden lead contains X". Prefer this target over a leading entity.
    _CRED_TARGET = re.compile(
        r"(?:authenticat\w*|logs? in|logg?ed in|unlock\w*|is the|为|认证|登录)\s+"
        r"(?:as\s+|password\s+(?:for|of)\s+|to\s+)?"
        r"((?:ghost|level|bandit|krypton|natas|user|stage)\s?\d{1,3})\b", re.I)

    @staticmethod
    def _norm_entity(ent: str) -> str:
        """ghost 3 / Ghost3 / GHOST3 → ghost3 (canonical dedup key)."""
        return re.sub(r"\s+", "", ent).lower()

    def canonical_credentials(self) -> list[dict]:
        """Deduped recovered-credential rows derived from VERIFIED fact text, in
        unlock order. Each row: {entity, value, seq}. Newest verified fact per
        entity wins. Returns [] when nothing qualifies (graceful — the raw facts
        are still rendered in the verified-facts section).

        Extraction is conservative by design (DESIGN §4-P2A false-positive guard):
        a row is emitted ONLY from a verified fact that (a) carries a success cue,
        (b) lacks a failure cue, AND (c) yields BOTH a level/user entity and a
        password-shaped value via a keyword/pair pattern. A miss is fine — the raw
        fact still appears in the verified-facts section; a wrong row would mislead
        workers told to reuse it, so we prefer to emit nothing when unsure."""
        with self._lock:
            rows = self._conn.execute(
                "SELECT seq, json_extract(payload,'$.fact'), verified "
                "FROM events WHERE kind=? ORDER BY seq",
                (EV_FACT_ADDED,),
            ).fetchall()
        by_entity: dict[str, dict] = {}
        for seq, fact, verified in rows:
            if not verified or not fact:
                continue                              # guard 1: verified only
            if self._CRED_FAILURE_CUE.search(fact):
                continue                              # guard 3: skip explicit failures
            if not self._CRED_SUCCESS_CUE.search(fact):
                continue                              # guard 2: require a success cue
            ent_m = self._CRED_ENTITY.search(fact)
            if not ent_m:
                continue                              # need a concrete entity
            # the entity the credential UNLOCKS (authenticates-as / is-the-X-password)
            # wins over a leading "found in X's home" entity — fixes mis-attributing
            # a cred to the box it was discovered on rather than the box it opens.
            tgt_m = self._CRED_TARGET.search(fact)
            entity = tgt_m.group(1) if tgt_m else ent_m.group(1)
            # value: prefer an explicit entity:value pair, else a keyword-introduced token
            value = None
            pair = self._CRED_PAIR.search(fact)
            if pair and not tgt_m:
                entity, value = pair.group(1), pair.group(2)
            elif pair:
                value = pair.group(2)                 # keep the target entity
            else:
                for cand in self._CRED_VALUE_KW.findall(fact):
                    if cand.lower() not in self._CRED_VALUE_STOP and not cand.isdigit():
                        value = cand
                        break
            if not value:
                # last resort: a value-first / no-keyword fact ("X authenticates as
                # ghostN"). Accept ONLY a strong, UNAMBIGUOUS password-shaped token:
                # mixed letters+digits, ≥8 chars, and EXACTLY ONE such token in the
                # fact (more than one → can't tell which is the cred → emit nothing).
                strong = [t for t in self._CRED_BARE.findall(fact)
                          if t.lower() not in self._CRED_VALUE_STOP
                          and self._norm_entity(t) != self._norm_entity(entity)
                          and re.search(r"[A-Za-z]", t) and re.search(r"\d", t)
                          and not re.fullmatch(r"[0-9a-f]{8,}", t.lower())]  # not pure hex
                uniq = list(dict.fromkeys(strong))
                if len(uniq) == 1:
                    value = uniq[0]
            if not value or value.lower() in self._CRED_VALUE_STOP:
                continue
            if self._CRED_VALUE_REJECT.search(value):
                continue                              # SSH/config flag, not a password
            key = self._norm_entity(entity)
            by_entity[key] = {"entity": self._norm_entity(entity), "value": value,
                              "seq": seq}
        # order by the seq each entity was (last) confirmed at → unlock order
        return sorted(by_entity.values(), key=lambda r: r["seq"])

    def _open_intents_block(self, limit: int = 24) -> str:
        """Render open/claimed intents (not in the SolveGraph snapshot — they live
        only in the intents table). Empty string when none."""
        with self._lock:
            rows = self._conn.execute(
                "SELECT goal, status, worker, worker_class, route_hash, branch_id, "
                "priority, lane_key, risk_class FROM intents "
                "WHERE status IN ('open','claimed') AND dispatch_state='active' "
                "ORDER BY priority DESC, created_seq",
            ).fetchall()
        if not rows:
            return ""
        omitted = max(0, len(rows) - limit)
        rows = rows[-limit:]
        lines = ["\n## Open intents (directions in flight)"]
        if omitted:
            lines.append(f"  (... {omitted} earlier open intents omitted)")
        for goal, status, worker, worker_class, route_hash, branch_id, priority, lane_key, risk_class in rows:
            who = f" [{worker}]" if worker else ""
            meta = []
            if worker_class and worker_class != "code":
                meta.append(str(worker_class))
            if route_hash:
                meta.append(f"route={route_hash}")
            if branch_id:
                meta.append(f"branch={branch_id}")
            if lane_key:
                meta.append(f"lane={lane_key}")
            if risk_class:
                meta.append(f"risk={risk_class}")
            if int(priority or 0):
                meta.append(f"priority={int(priority or 0)}")
            suffix = f" ({', '.join(meta)})" if meta else ""
            lines.append(f"- ({status}){who} {str(goal)[:160]}{suffix}")
        return "\n".join(lines)

    def _intent_sources_map(self, intent_ids: Optional[set[str]] = None, *,
                            include_retired: bool = False) -> dict[str, list[int]]:
        params: list[Any] = []
        where = ""
        if intent_ids:
            where = "WHERE intent_id IN (" + ",".join("?" for _ in intent_ids) + ")"
            params.extend(sorted(intent_ids))
        with self._lock:
            rows = self._conn.execute(
                f"SELECT intent_id, fact_seq FROM intent_sources {where} ORDER BY fact_seq",
                tuple(params),
            ).fetchall()
        active = None if include_retired else self._active_fact_seq_set()
        out: dict[str, list[int]] = {}
        for iid, seq in rows:
            fact_seq = int(seq)
            if active is not None and fact_seq not in active:
                continue
            out.setdefault(str(iid), []).append(fact_seq)
        return out

    def _intent_products_map(self, intent_ids: Optional[set[str]] = None, *,
                             include_retired: bool = False) -> dict[str, list[int]]:
        params: list[Any] = []
        where = ""
        if intent_ids:
            where = "WHERE intent_id IN (" + ",".join("?" for _ in intent_ids) + ")"
            params.extend(sorted(intent_ids))
        with self._lock:
            rows = self._conn.execute(
                f"SELECT intent_id, fact_seq FROM intent_products {where} ORDER BY fact_seq",
                tuple(params),
            ).fetchall()
        active = None if include_retired else self._active_fact_seq_set()
        out: dict[str, list[int]] = {}
        for iid, seq in rows:
            fact_seq = int(seq)
            if active is not None and fact_seq not in active:
                continue
            out.setdefault(str(iid), []).append(fact_seq)
        return out

    def _active_intent_rows(self) -> list[dict]:
        with self._lock:
            rows = self._conn.execute(
                "SELECT intent_id, goal, status, worker, worker_class, route_hash, branch_id "
                "FROM intents WHERE status IN ('open','claimed') AND dispatch_state='active' "
                "ORDER BY priority DESC, created_seq",
            ).fetchall()
        return [
            {"intent_id": r[0], "goal": r[1], "status": r[2], "worker": r[3] or "",
             "worker_class": r[4] or "code", "route_hash": r[5] or "", "branch_id": r[6] or ""}
            for r in rows
        ]

    def _giveup_product_fact_seqs(self) -> set[int]:
        giveup = self._intent_giveup_map()
        if not giveup:
            return set()
        products = self._intent_products_map(include_retired=True)
        out: set[int] = set()
        for iid, seqs in products.items():
            if giveup.get(iid, False):
                out.update(seqs)
        return out

    def _active_fact_seqs_by_verified(self, *, verified: bool,
                                      limit: Optional[int] = None,
                                      exclude_giveup_products: bool = False) -> list[int]:
        states = self._fact_state_map()
        blocked = self._giveup_product_fact_seqs() if exclude_giveup_products else set()
        sql = "SELECT seq, verified FROM events WHERE challenge_id=? AND kind=? ORDER BY seq DESC"
        params: list[Any] = [self.challenge.id, EV_FACT_ADDED]
        if limit is not None:
            sql += " LIMIT ?"
            params.append(int(limit))
        with self._lock:
            rows = self._conn.execute(sql, tuple(params)).fetchall()
        out: list[int] = []
        for seq_raw, raw_verified in rows:
            seq = int(seq_raw)
            if seq in blocked:
                continue
            st = states.get(seq, {})
            state = st.get("state", FACT_STATE_UNRESOLVED)
            if st.get("retired") or state in _FACT_TERMINAL_STATES:
                continue
            eff = st.get("verified_effective")
            is_verified = bool(raw_verified) if eff is None else bool(eff)
            if state == FACT_STATE_CHALLENGED:
                is_verified = False
            if is_verified == bool(verified):
                out.append(seq)
        return list(reversed(out))

    def _latest_verified_fact_seqs(self, limit: Optional[int] = None, *,
                                   exclude_giveup_products: bool = False) -> list[int]:
        return self._active_fact_seqs_by_verified(
            verified=True, limit=limit,
            exclude_giveup_products=exclude_giveup_products)

    def _latest_candidate_fact_seqs(self, limit: Optional[int] = None, *,
                                    exclude_giveup_products: bool = False) -> list[int]:
        return self._active_fact_seqs_by_verified(
            verified=False, limit=limit,
            exclude_giveup_products=exclude_giveup_products)

    def pin_facts(self, *, actor: str, fact_seqs: list[int],
                  reason: str = "") -> list[int]:
        active = self._active_fact_seq_set()
        clean: list[int] = []
        seen: set[int] = set()
        for raw in fact_seqs or []:
            try:
                seq = int(raw)
            except (TypeError, ValueError):
                continue
            if seq <= 0 or seq in seen or seq not in active:
                continue
            seen.add(seq)
            clean.append(seq)
        if not clean:
            return []
        pinned: list[int] = []
        detail = (reason or "").strip()[:500]
        for seq in clean:
            ev_seq = self._append(
                EV_FACT_PINNED,
                actor,
                {"fact_seq": seq, "reason": detail},
                dedupe_key=f"fact-pinned::{self.challenge.id}::{seq}",
            )
            with self._lock:
                self._conn.execute(
                    "INSERT OR IGNORE INTO fact_pins "
                    "(fact_seq, challenge_id, actor, reason, pinned_seq) "
                    "VALUES (?,?,?,?,?)",
                    (seq, self.challenge.id, actor, detail,
                     ev_seq if ev_seq > 0 else 0),
                )
                self._conn.commit()
            pinned.append(seq)
        return pinned

    def pinned_fact_seqs(self, *, exclude_giveup_products: bool = False) -> list[int]:
        if not self._table_exists("fact_pins"):
            return []
        active = self._active_fact_seq_set()
        blocked = self._giveup_product_fact_seqs() if exclude_giveup_products else set()
        with self._lock:
            rows = self._conn.execute(
                "SELECT fact_seq FROM fact_pins WHERE challenge_id=? ORDER BY pinned_seq",
                (self.challenge.id,),
            ).fetchall()
        out: list[int] = []
        for (raw_seq,) in rows:
            seq = int(raw_seq)
            if seq in active and seq not in blocked:
                out.append(seq)
        return out

    def fact_pin_context(self, limit: int = 240) -> str:
        active = self._active_fact_seq_set() - self._giveup_product_fact_seqs()
        if not active:
            return ""
        states = self._fact_state_map()
        with self._lock:
            rows = self._conn.execute(
                "SELECT seq, json_extract(payload,'$.source'), "
                "json_extract(payload,'$.fact'), verified, confidence "
                "FROM events WHERE challenge_id=? AND kind=? ORDER BY seq DESC LIMIT ?",
                (self.challenge.id, EV_FACT_ADDED, int(limit)),
            ).fetchall()
        lines: list[str] = []
        for seq_raw, source, fact, raw_verified, confidence in reversed(rows):
            seq = int(seq_raw)
            if seq not in active or not fact:
                continue
            st = states.get(seq, {})
            eff = st.get("verified_effective")
            is_verified = bool(raw_verified) if eff is None else bool(eff)
            if st.get("state") == FACT_STATE_CHALLENGED:
                is_verified = False
            verdict = "verified" if is_verified else "candidate"
            lines.append(
                f"- [#{seq}] {verdict} ({source or 'unknown'}, "
                f"confidence={float(confidence or 0):.2f}) {str(fact)[:220]}"
            )
        if not lines:
            return ""
        return "## Fact retention index (model decides pinned_facts)\n" + "\n".join(lines)

    def _intent_giveup_map(self) -> dict[str, bool]:
        with self._lock:
            rows = self._conn.execute(
                "SELECT intent_id, close_reason FROM intents WHERE challenge_id=?",
                (self.challenge.id,),
            ).fetchall()
        return {str(iid): is_genuine_giveup(str(reason or "")) for iid, reason in rows}

    def _reason_relevant_fact_seqs(self) -> set[int]:
        active = self._active_intent_rows()
        active_ids = {str(i["intent_id"]) for i in active}
        sources = self._intent_sources_map()
        products = self._intent_products_map()
        giveup = self._intent_giveup_map()
        producer_by_fact: dict[int, set[str]] = {}
        for iid, seqs in products.items():
            for seq in seqs:
                producer_by_fact.setdefault(seq, set()).add(iid)
        facts: set[int] = set()
        seen_intents: set[str] = set()
        stack = list(active_ids)
        while stack:
            iid = stack.pop()
            if iid in seen_intents:
                continue
            seen_intents.add(iid)
            for seq in sources.get(iid, []):
                facts.add(seq)
                for producer in producer_by_fact.get(seq, set()) - seen_intents:
                    if not giveup.get(producer, False):
                        stack.append(producer)
            for seq in products.get(iid, []):
                facts.add(seq)
        facts.update(self._latest_verified_fact_seqs(
            limit=8, exclude_giveup_products=True))
        facts.update(self.pinned_fact_seqs(exclude_giveup_products=True))
        facts.update(self._latest_candidate_fact_seqs(
            limit=16, exclude_giveup_products=True))
        return facts

    def _summary_for_fact_seqs(self, fact_seqs: set[int],
                               max_dead_ends: Optional[int] = None) -> str:
        c = self.challenge
        lines = [f"# Challenge: {c.name} [{c.category}] ({c.points} pts)"]
        fact_states = self._fact_state_map()
        want = sorted(fact_seqs)
        if want:
            q = ",".join("?" for _ in want)
            with self._lock:
                rows = self._conn.execute(
                    "SELECT seq, json_extract(payload,'$.source'), "
                    "json_extract(payload,'$.fact'), verified, confidence "
                    f"FROM events WHERE kind=? AND seq IN ({q}) ORDER BY seq",
                    (EV_FACT_ADDED, *want),
                ).fetchall()
            verified_lines: list[str] = []
            candidate_lines: list[str] = []
            for seq, source, fact, verified, confidence in rows:
                st = fact_states.get(int(seq), {})
                if st.get("retired") or st.get("state") in _FACT_TERMINAL_STATES:
                    continue
                line = f"- ({source or 'unknown'}) [#{int(seq)}] {str(fact)[:240]}"
                if bool(verified):
                    verified_lines.append(line)
                else:
                    candidate_lines.append(f"{line} [UNVERIFIED] confidence={float(confidence or 0):.2f}")
            if verified_lines:
                lines.append("\n## Confirmed evidence")
                lines.extend(verified_lines)
            if candidate_lines:
                lines.append("\n## Candidates / needs verification")
                lines.extend(candidate_lines)
        with self._lock:
            rows = self._conn.execute(
                "SELECT json_extract(payload,'$.reason') FROM events "
                "WHERE kind=? ORDER BY seq",
                (EV_DEAD_END,),
            ).fetchall()
        reasons = [str(r[0]) for r in rows if r[0]]
        if max_dead_ends is not None:
            reasons = reasons[-int(max_dead_ends):]
        if reasons:
            lines.append("\n## Dead ends")
            lines.extend(f"- {r}" for r in reasons)
        return "\n".join(lines)

    def _active_intent_lineage_block(self, limit: int = 24) -> str:
        active = self._active_intent_rows()[-limit:]
        if not active:
            return ""
        texts = self._fact_text_by_seq()
        ids = {str(i["intent_id"]) for i in active}
        sources = self._intent_sources_map(ids)
        products = self._intent_products_map(ids)
        lines = ["\n## Active intent lineage"]
        for row in active:
            iid = str(row["intent_id"])
            src = sources.get(iid, [])
            prod = products.get(iid, [])
            src_txt = ", ".join(f"#{seq} {texts.get(seq, '')[:80]}" for seq in src) or "no source facts"
            prod_txt = ", ".join(f"#{seq}" for seq in prod) or "no products yet"
            lines.append(
                f"- {iid} ({row['status']}): {str(row['goal'])[:140]} <= {src_txt}; "
                f"products: {prod_txt}")
        return "\n".join(lines)

    def intent_neighborhood_block(self, intent_id: str, sibling_limit: int = 8) -> str:
        iid = (intent_id or "").strip()
        if not iid:
            return ""
        texts = self._fact_text_by_seq()
        sources = self._intent_sources_map({iid}).get(iid, [])
        if not sources:
            return ""
        source_set = set(sources)
        with self._lock:
            rows = self._conn.execute(
                "SELECT i.intent_id, i.goal, i.status FROM intents i "
                "JOIN intent_sources s ON s.intent_id=i.intent_id "
                "WHERE s.fact_seq IN (" + ",".join("?" for _ in source_set) + ") "
                "AND i.intent_id<>? AND i.status IN ('open','claimed') "
                "AND i.dispatch_state='active' ORDER BY i.created_seq LIMIT ?",
                (*sorted(source_set), iid, int(sibling_limit)),
            ).fetchall()
        lines = ["\n## Intent graph neighborhood"]
        lines.append("Source facts:")
        for seq in sources[:12]:
            lines.append(f"- [#{seq}] {texts.get(seq, '')[:240]}")
        if rows:
            lines.append("Sibling intents sharing those facts:")
            for sid, goal, status in rows:
                lines.append(f"- {sid} ({status}): {str(goal)[:180]}")
        return "\n".join(lines)

    def open_goal_texts(self) -> list[str]:
        """Goal texts of every open/claimed intent — the dedup reference set for
        dispatch_intents' near-duplicate filter (reason.py). Claimed included:
        a direction someone is actively working must not be re-proposed either."""
        with self._lock:
            rows = self._conn.execute(
                "SELECT goal FROM intents WHERE status IN ('open','claimed') "
                "AND dispatch_state='active' ORDER BY created_seq",
            ).fetchall()
        return [str(r[0]) for r in rows if r[0]]

    def dispatchable_goal_texts(self) -> list[str]:
        """Goal texts that can be claimed right now.

        This intentionally differs from open_goal_texts(): a live claimed intent is
        active for dedupe, but it is not dispatchable until its lease expires. Reason's
        starvation valve needs this narrower view to avoid treating a stale live claim
        as available work.
        """
        now = time.time()
        with self._lock:
            rows = self._conn.execute(
                "SELECT goal FROM intents WHERE dispatch_state='active' "
                "AND (status='open' OR (status='claimed' AND lease_until IS NOT NULL "
                "AND lease_until < ?)) ORDER BY priority DESC, created_seq",
                (now,),
            ).fetchall()
        return [str(r[0]) for r in rows if r[0]]

    def open_route_hashes(self) -> list[str]:
        with self._lock:
            rows = self._conn.execute(
                "SELECT DISTINCT route_hash FROM intents "
                "WHERE status IN ('open','claimed') AND dispatch_state='active' "
                "AND route_hash IS NOT NULL AND route_hash<>'' "
                "AND worker_class NOT IN ('verifier','review') "
                "ORDER BY route_hash",
            ).fetchall()
        return [str(r[0]) for r in rows if r[0]]

    def barren_concluded_goal_texts(self) -> list[str]:
        """P1 escape-valve dedup set: goals of CONCLUDED intents that yielded NOTHING
        — result is a barren 'explored'/dead-end/no-flag AND no fact was attached
        (result_seq → to_fact_seq is NULL). These are safe to suppress re-proposal of
        (a tried-and-empty direction). Intents that DID produce a fact (to_fact_seq
        set) are EXCLUDED, so re-proposing a productive direction under new evidence
        stays a planner judgment call — avoiding the run-7349 starvation where a
        blanket concluded-dedup proposed 0 intents and Explore starved."""
        with self._lock:
            rows = self._conn.execute(
                "SELECT i.goal, e.payload FROM intents i "
                "LEFT JOIN events e ON e.seq = i.result_seq "
                "WHERE i.status='done' AND i.to_fact_seq IS NULL "
                "AND NOT EXISTS ("
                "  SELECT 1 FROM pocs p WHERE p.intent_id=i.intent_id "
                "  AND p.status IN ('available','wip','directional')"
                ") "
                "ORDER BY i.created_seq",
            ).fetchall()
        out: list[str] = []
        for goal, payload in rows:
            if not goal:
                continue
            result = ""
            if payload:
                try:
                    result = str((json.loads(payload) or {}).get("result", "")).lower()
                except (json.JSONDecodeError, TypeError):
                    result = ""
            # only barren outcomes — never 'solved' (that has a flag) or anything
            # that produced evidence. 'explored'/'dead_end'/'no verified flag'/''.
            if "solved" in result:
                continue
            out.append(str(goal))
        return out

    def _attempted_intents_block(self, limit: int = 40) -> str:
        """Render CONCLUDED intents with each one's conclusion text, so the Reason
        planner sees what was already tried AND what came of it (run-11190: the
        planner kept re-proposing paraphrases of concluded directions because the
        summary never showed them). The result comes from the EV_INTENT_CONCLUDED
        event the row's result_seq points at; superseded/no-result rows render a
        placeholder. Most recent `limit` shown (oldest→newest); earlier ones are
        collapsed into a count line — a goal is one line, so 40 stays cheap."""
        with self._lock:
            rows = self._conn.execute(
                "SELECT i.goal, e.payload, i.worker_class, i.route_hash, i.branch_id, "
                "i.result_detail FROM intents i "
                "LEFT JOIN events e ON e.seq = i.result_seq "
                "WHERE i.status='done' ORDER BY i.created_seq",
            ).fetchall()
        if not rows:
            return ""
        omitted = max(0, len(rows) - limit)
        lines = ["\n## Already attempted (concluded intents — do NOT re-propose; "
                 "build on their results)"]
        if omitted:
            lines.append(f"  (… {omitted} earlier attempted intents omitted)")
        for goal, payload, worker_class, route_hash, branch_id, row_detail in rows[-limit:]:
            result = ""
            detail = str(row_detail or "")
            if payload:
                try:
                    p = json.loads(payload) or {}
                    result = str(p.get("result", ""))
                    detail = detail or str(p.get("result_detail", ""))
                except (json.JSONDecodeError, TypeError):
                    result = ""
            tail = result.strip()[:80] if result.strip() else "(superseded / no result recorded)"
            if detail.strip():
                tail = f"{tail}: {detail.strip()[:220]}"
            meta = []
            if worker_class and worker_class != "code":
                meta.append(str(worker_class))
            if route_hash:
                meta.append(f"route={route_hash}")
            if branch_id:
                meta.append(f"branch={branch_id}")
            suffix = f" ({', '.join(meta)})" if meta else ""
            lines.append(f"- {str(goal)[:160]}{suffix} → {tail}")
        return "\n".join(lines)

    def challenged_facts(self) -> list[dict]:
        texts = self._fact_text_by_seq()
        with self._lock:
            rows = self._conn.execute(
                "SELECT fact_seq, status, reason, verification_intent_id "
                "FROM fact_reviews WHERE challenge_id=? AND status='challenged' "
                "ORDER BY challenged_seq",
                (self.challenge.id,),
            ).fetchall()
        return [
            {"fact_seq": int(r[0]), "status": r[1], "reason": r[2] or "",
             "verification_intent_id": r[3] or "", "fact": texts.get(int(r[0]), "")}
            for r in rows
        ]

    def revalidated_facts(self) -> list[dict]:
        texts = self._fact_text_by_seq()
        with self._lock:
            rows = self._conn.execute(
                "SELECT fact_seq, status, reason FROM fact_reviews "
                "WHERE challenge_id=? AND status='revalidated' ORDER BY revalidated_seq",
                (self.challenge.id,),
            ).fetchall()
        return [
            {"fact_seq": int(r[0]), "status": r[1], "reason": r[2] or "",
             "fact": texts.get(int(r[0]), "")}
            for r in rows
        ]

    def retired_facts(self, *, states: Optional[tuple[str, ...]] = None) -> list[dict]:
        """Facts in a terminal lifecycle state (rejected/merged/superseded) — for the
        review/audit board (kept visible but de-verified)."""
        if not self._table_exists("fact_states"):
            return []
        want = states or (FACT_STATE_REJECTED, FACT_STATE_MERGED, FACT_STATE_SUPERSEDED)
        texts = self._fact_text_by_seq(include_retired=True)
        q = ",".join("?" for _ in want)
        with self._lock:
            rows = self._conn.execute(
                f"SELECT fact_seq, state, reason, merged_seq FROM fact_states "
                f"WHERE challenge_id=? AND state IN ({q}) ORDER BY updated_seq",
                (self.challenge.id, *want),
            ).fetchall()
        merges: dict[int, int] = {}
        if self._table_exists("fact_merges"):
            with self._lock:
                mrows = self._conn.execute(
                    "SELECT from_fact_seq, to_fact_seq FROM fact_merges WHERE challenge_id=?",
                    (self.challenge.id,),
                ).fetchall()
            merges = {int(m[0]): int(m[1]) for m in mrows}
        return [
            {"fact_seq": int(r[0]), "state": r[1], "reason": r[2] or "",
             "fact": texts.get(int(r[0]), ""),
             "merged_into": merges.get(int(r[0]))}
            for r in rows
        ]

    def suppressed_routes(self) -> list[dict]:
        with self._lock:
            rows = self._conn.execute(
                "SELECT route_hash, label, reason, until_policy, suppressed_seq "
                "FROM routes WHERE challenge_id=? AND status='suppressed' "
                "ORDER BY suppressed_seq",
                (self.challenge.id,),
            ).fetchall()
        return [
            {"route_hash": r[0], "label": r[1], "reason": r[2] or "",
             "until": r[3] or "new_evidence", "suppressed_seq": r[4]}
            for r in rows
        ]

    def is_route_suppressed(self, route_hash: str) -> bool:
        route = self.normalize_route_hash(route_hash)
        with self._lock:
            row = self._conn.execute(
                "SELECT status FROM routes WHERE challenge_id=? AND route_hash=?",
                (self.challenge.id, route),
            ).fetchone()
        return bool(row and row[0] == "suppressed")

    def branches(self) -> list[dict]:
        with self._lock:
            rows = self._conn.execute(
                "SELECT branch_id, parent_id, title, assumption, prove_or_disprove, status "
                "FROM branches WHERE challenge_id=? ORDER BY created_seq, branch_id",
                (self.challenge.id,),
            ).fetchall()
        return [
            {"branch_id": r[0], "parent_id": r[1] or "", "title": r[2] or "",
             "assumption": r[3] or "", "prove_or_disprove": r[4] or "",
             "status": r[5] or "open"}
            for r in rows
        ]

    def coordinator_directives(self) -> list[dict]:
        out: list[dict] = []
        for e in self.events():
            if e.get("kind") == EV_COORDINATOR_DIRECTIVE:
                p = dict(e.get("payload") or {})
                p["seq"] = e.get("seq")
                p["actor"] = e.get("actor")
                out.append(p)
        return out

    def latest_unconsumed_directive_seq(self, *, after_seq: int = 0,
                                        action: str = "") -> Optional[dict]:
        directives = [
            d for d in self.coordinator_directives()
            if int(d.get("seq") or 0) > int(after_seq or 0)
            and (not action or d.get("action") == action)
        ]
        return directives[-1] if directives else None

    def genuine_failures_for_route(self, route_hash: str) -> int:
        route = self.normalize_route_hash(route_hash)
        with self._lock:
            rows = self._conn.execute(
                "SELECT e.payload FROM intents i "
                "LEFT JOIN events e ON e.seq = i.result_seq "
                "WHERE i.challenge_id=? AND i.route_hash=? AND i.status='done'",
                (self.challenge.id, route),
            ).fetchall()
        count = 0
        for (payload,) in rows:
            result = ""
            if payload:
                try:
                    result = str((json.loads(payload) or {}).get("result", "")).lower()
                except (json.JSONDecodeError, TypeError):
                    result = ""
            if not result:
                continue
            if any(skip in result for skip in (
                "timeout", "timed out", "cancelled", "canceled", "steered",
                "oom", "killed", "route_suppressed", "superseded",
                "lane_deferred", "lane_blocked", "closed_by_solve",
            )):
                continue
            if any(tok in result for tok in (
                "dead", "failed", "no flag", "no verified flag", "gave up",
                "exhausted", "not exploitable",
            )):
                count += 1
        return count

    def _review_state_block(self) -> str:
        parts: list[str] = []
        challenged = self.challenged_facts()
        if challenged:
            parts.append("\n## Challenged facts (DO NOT treat as verified until revalidated)")
            for f in challenged[-30:]:
                parts.append(
                    f"- [#{f['fact_seq']}] {f['fact'][:160]} :: {f['reason'][:160]} "
                    f"(verify via {f['verification_intent_id']})")
        revalidated = self.revalidated_facts()
        if revalidated:
            parts.append("\n## Revalidated facts")
            for f in revalidated[-20:]:
                parts.append(f"- [#{f['fact_seq']}] {f['fact'][:160]} :: {f['reason'][:160]}")
        retired = self.retired_facts()
        if retired:
            parts.append("\n## Retired facts (rejected/merged/superseded — do NOT use as evidence)")
            for f in retired[-30:]:
                tag = f['state']
                if f['state'] == FACT_STATE_MERGED and f.get('merged_into'):
                    tag = f"merged→#{f['merged_into']}"
                parts.append(f"- [#{f['fact_seq']}] ({tag}) {f['fact'][:160]} :: {f['reason'][:160]}")
        suppressed = self.suppressed_routes()
        if suppressed:
            parts.append("\n## Suppressed routes (ordinary workers must not retry)")
            for r in suppressed[-30:]:
                parts.append(
                    f"- {r['route_hash']} ({r['label']}): {r['reason'][:180]} "
                    f"until={r['until']}")
        branches = self.branches()
        if branches:
            parts.append("\n## Open branches (do not mix incompatible assumptions)")
            for b in branches[-30:]:
                parts.append(
                    f"- {b['branch_id']} [{b['status']}]: {b['assumption'][:180]} "
                    f"→ {b['prove_or_disprove'][:180]}")
        directives = self.coordinator_directives()
        if directives:
            parts.append("\n## Review directives")
            for d in directives[-20:]:
                parts.append(
                    f"- #{d.get('seq')} {d.get('action')}[{d.get('priority','normal')}]: "
                    f"{str(d.get('directive',''))[:220]}")
        return "\n".join(parts)

    def _poc_block(self, *, limit: int = 30) -> str:
        rows = self.pocs(inheritable_only=False)
        if not rows:
            return ""
        visible = [p for p in rows if p.get("status") != "quarantined"]
        if not visible:
            return "\n## Shared PoCs\n- all saved PoCs are quarantined; do not inherit them"
        # Only the PoCs the linker actually mounts get the "./inherited/<poc_id>/"
        # promise (#10). A 'wip' PoC under a live lease, or a 'spent' one, is NOT
        # linked into any worker cwd, so advertising that path for it points at a
        # folder that doesn't exist. Split: inheritable (path promised) vs historical
        # (metadata only, no path). The inheritable set is exactly pocs(inheritable
        # _only=True) so the board and the linker never disagree.
        inheritable_ids = {p["poc_id"] for p in self.pocs(inheritable_only=True)}

        def _render(items: list[dict]) -> list[str]:
            out: list[str] = []
            for p in items:
                iid = f" intent={p['intent_id']}" if p.get("intent_id") else ""
                note = f" — {str(p.get('note') or '')[:100]}" if p.get("note") else ""
                out.append(f"- {p['poc_id']} ({p['status']}){iid}: "
                           f"{p['entry_command']}{note}")
            return out

        inheritable = [p for p in visible if p["poc_id"] in inheritable_ids]
        historical = [p for p in visible if p["poc_id"] not in inheritable_ids]
        lines: list[str] = []
        if inheritable:
            lines.append("\n## Inheritable PoCs (run/copy under ./inherited/<poc_id>/)")
            omitted = max(0, len(inheritable) - limit)
            if omitted:
                lines.append(f"  (... {omitted} older inheritable PoCs omitted)")
            lines.extend(_render(inheritable[-limit:]))
        if historical:
            # in-use (wip, currently leased) or spent — listed for context, but NOT
            # mounted; don't tell a worker to run them from ./inherited/.
            lines.append("\n## Historical PoCs (in-use or spent; metadata only, not mounted)")
            omitted = max(0, len(historical) - limit)
            if omitted:
                lines.append(f"  (... {omitted} older historical PoCs omitted)")
            lines.extend(_render(historical[-limit:]))
        return "\n".join(lines)

    def _standing_guidance_block(self, standing_guidance: Optional[list[str]]) -> str:
        items = [
            str(x).strip()
            for x in (standing_guidance or [])
            if str(x).strip()
        ]
        if not items:
            return ""
        lines = ["\n## Operator standing guidance (highest priority; guidance, not evidence)"]
        for item in items[-12:]:
            lines.append(f"- {item[:500]}")
        return "\n".join(lines)

    def _operator_directives_block(self) -> str:
        """B: active operator directives the planner MUST prioritize (highest
        priority; guidance, not proven evidence)."""
        directives = self.operator_directives(active_only=True)
        if not directives:
            return ""
        lines = ["\n## Operator directives (MUST prioritize — guidance, not evidence)"]
        for d in directives[:12]:
            lines.append(f"- [{d['action']}/{d['status']}] {d['text'][:400]}")
        return "\n".join(lines)

    def _forbidden_zones_block(self) -> str:
        """D/E: the exclusive lanes + held resource locks the planner must route
        AROUND (don't propose intents that collide with an active lock)."""
        parts: list[str] = []
        lanes = self.active_lanes()
        locks = self.active_resource_locks()
        if not lanes and not locks:
            return ""
        parts.append("\n## Forbidden zones (locked — do NOT propose conflicting work)")
        for lane in lanes[:20]:
            parts.append(f"- lane {lane['lane_key']} [{lane['owner_worker']}]")
        for rl in locks[:20]:
            parts.append(f"- resource {rl['resource_key']} (scope={rl['scope']}) "
                         f"[{rl['owner_worker']}]")
        return "\n".join(parts)

    def to_reason_summary(self, standing_guidance: Optional[list[str]] = None) -> str:
        """The PLANNER's board view: the uncapped [#seq]-labelled summary (all
        facts AND all dead-ends — the P1.5 un-blinding lifted only the evidence
        cap; dead-ends stayed clipped to the last 8, so long-run planners forgot
        old dead directions) plus the two intent sections the snapshot can't
        carry: in-flight (open/claimed) and attempted-with-results. REASON_SYSTEM
        references both section titles in its no-re-proposal rule.

        Phase 4: now also carries active operator directives (B, must-prioritize)
        and forbidden zones (D/E, locked lanes/resources to route around). Retired
        facts (rejected/merged/superseded) are already dropped by snapshot(); only
        dispatch_state='active' intents appear in the open-intents block."""
        relevant = self._reason_relevant_fact_seqs()
        parts = [self._summary_for_fact_seqs(relevant, max_dead_ends=10**9),
                 self._captured_flags_block(),   # defect-9: already-solved directions
                 self._standing_guidance_block(standing_guidance),
                 self._operator_directives_block(),
                 self._forbidden_zones_block(),
                 self._review_state_block(),
                 self._active_intent_lineage_block(),
                 self._open_intents_block(limit=24),
                 self._poc_block(),
                 self._attempted_intents_block()]
        return "\n".join(p for p in parts if p and p.strip())

    def _captured_flags_block(self) -> str:
        """defect-9: the flags the run already holds. Surfaced to the planner so it
        does NOT re-propose intents aiming at an already-captured flag (the ezrop-ROP
        re-do: a worker re-running a direction that already yielded flag1). Empty when
        no flags yet — single/zero-flag runs are byte-identical."""
        flags = self.snapshot().flags
        if not flags:
            return ""
        lines = "\n".join(f"  - {f}" for f in flags)
        return ("\n## Flags already captured (do NOT propose any intent to re-recover "
                "these — those directions are DONE):\n"
                f"{lines}\n")

    def _credential_block(self, creds: "Optional[list[dict]]" = None) -> str:
        """The canonical credential / unlock-chain section (also used standalone as
        the inline prompt digest). Empty string when no creds qualify."""
        creds = self.canonical_credentials() if creds is None else creds
        if not creds:
            return ""
        chain = " → ".join(f"{c['entity']}:{c['value']}" for c in creds)
        return ("\n## Recovered credentials / unlock chain "
                "(heuristically derived — verify before trusting)\n"
                f"{chain}\n")

    def _brief_block(self) -> str:
        """The FULL, untruncated challenge brief for the board file. SolveGraph's
        to_summary caps the description at 300 chars — but the brief is exactly where
        target/connection blocks live (e.g. an `SSH Access` host/port/creds section,
        run-10070), so capping it forces workers to dig the target out of session
        files. The file has no budget, so carry the whole thing here.

        Target/attachments come from the prompt builder (not the graph), so we render
        what the SolveGraph snapshot has: the challenge description verbatim."""
        c = self.challenge
        desc = (getattr(c, "description", "") or "").strip()
        if not desc:
            return ""
        return ("\n## Challenge brief (full — read for target/connection details)\n"
                f"{desc}\n")

    def to_board_markdown(self) -> str:
        """The FULL board rendered for the workdir file (no truncation): the
        canonical credential chain on top, the FULL challenge brief (target/SSH
        block lives here), then the untruncated [#seq]-labelled fact summary, then
        open intents. The credential section is also the inline prompt digest
        (rendered alone via _credential_block).

        Uses to_summary(max_evidence=10**9, max_dead_ends=10**9) so stage-1 [-16]
        is defeated, ALL dead-ends are shown (a worker re-walking a long-ruled-out
        path is the same waste the planner suffers — see to_reason_summary), and
        the [#seq] labels (the stable fact ids that Reason cites via `from`)
        are preserved; the caller drops the stage-2 [:2000] clip by using this
        method instead of the inline path."""
        creds = self.canonical_credentials()
        parts = [self._credential_block(creds), self._brief_block(),
                 self.to_summary(max_evidence=10**9, max_dead_ends=10**9),
                 self._review_state_block(),
                 self._open_intents_block(),
                 self._poc_block(),
                 # P4: in-progress activities a teammate is doing right now (avoid
                 # redoing a nmap/brute already underway).
                 self._activity_locks_block(),
                 self._lane_locks_block(),
                 self._resource_locks_block(),
                 # P1-A: also show CONCLUDED directions (+ results) to WORKERS, not
                 # just the Reason planner. Without this the board was asymmetric
                 # (to_reason_summary had it, to_board_markdown didn't), so a new
                 # worker re-walked directions already attempted-and-concluded — the
                 # "重走老路" report. A goal is one line; the file has no budget.
                 self._attempted_intents_block()]
        return "\n".join(p for p in parts if p and p.strip())

    def to_review_summary(self) -> str:
        """Review-Arbiter's full audit view. It intentionally includes more than
        Reason's compact planner view: raw event tails, all fact classes, route
        state, branch state, intent lifecycle, PoCs, flags, and operator/review
        directives. It is still derived from append-only events/materialized views."""
        parts = [
            "# Review-Arbiter audit board",
            self._brief_block(),
            self.to_summary(max_evidence=10**9, max_dead_ends=10**9),
            self._captured_flags_block(),
            self._review_state_block(),
            self._open_intents_block(),
            self._poc_block(limit=80),
            self._activity_locks_block(),
            self._lane_locks_block(),
            self._resource_locks_block(),
            self._attempted_intents_block(limit=120),
            "\n## Recent raw events",
        ]
        for e in self.events()[-80:]:
            payload = e.get("payload") or {}
            preview = json.dumps(payload, ensure_ascii=False, default=str)[:500]
            parts.append(
                f"- #{e.get('seq')} {e.get('kind')} actor={e.get('actor')} "
                f"verified={e.get('verified')} {preview}")
        return "\n".join(p for p in parts if p and str(p).strip())
