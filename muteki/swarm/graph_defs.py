"""Shared-graph constants + pure lane/fact helpers.

Split out of ``shared_graph.py`` (code-health G1) so the event-type vocabulary,
lifecycle-state sets, and the stateless lane-canonicalization helpers live in one
small, dependency-free module. ``shared_graph`` re-imports every name here, so the
public surface (``from muteki.swarm.shared_graph import EV_*, canonicalize_lane,
_normalize_fact_identity, …``) is unchanged.

These are pure definitions/functions: no SQLite, no I/O, no graph state.
"""

from __future__ import annotations

import hashlib
import re
from urllib.parse import urlparse


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
EV_FLAG_SUBMISSION = "flag_submission"
EV_FLAG_SUBMISSION_DECISION = "flag_submission_decision"
EV_FINDING_FOUND = "finding_found"
EV_FINDING_INVALIDATED = "finding_invalidated"
EV_REPORT_SUBMITTED = "report_submitted"
EV_REPORT_REJECTED = "report_rejected"
EV_REPORT_REPRO_DECISION = "report_repro_decision"
EV_REPORT_VALUE_DECISION = "report_value_decision"
EV_REPORT_ACCEPTED = "report_accepted"
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
EV_CONTROL_STANDING_CLEAR_APPLIED = "control_standing_clear_applied"
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
