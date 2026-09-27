"""SQLite DDL for the shared evidence graph.

Split out of ``shared_graph.py`` (code-health G1). The append-only ``events``
table is the source of truth; every other table is a materialized view folded
from events (droppable & rebuildable). ``shared_graph`` imports ``SCHEMA`` from
here; the DDL is byte-for-byte the same as the former inline ``_SCHEMA``.
"""

from __future__ import annotations

SCHEMA = """
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
    requested_priority TEXT,
    priority_reason TEXT,
    value_claim_json TEXT,
    novelty_key TEXT,
    requires_capabilities_json TEXT,
    required_pocs_json TEXT,
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
CREATE TABLE IF NOT EXISTS intent_dependencies (
    intent_id            TEXT NOT NULL,
    depends_on_intent_id TEXT NOT NULL,
    challenge_id         TEXT NOT NULL,
    created_seq          INTEGER NOT NULL,
    PRIMARY KEY (intent_id, depends_on_intent_id)
);
CREATE TABLE IF NOT EXISTS intent_products (
    intent_id  TEXT NOT NULL,
    fact_seq   INTEGER NOT NULL,
    PRIMARY KEY (intent_id, fact_seq)
);
-- Raw claims and tool evidence.  These rows are audit/search input and never
-- enter the default Fact-Goal-Step planning frontier directly.
CREATE TABLE IF NOT EXISTS observations (
    observation_seq INTEGER PRIMARY KEY,
    challenge_id TEXT NOT NULL,
    actor TEXT NOT NULL,
    intent_id TEXT,
    target_epoch TEXT NOT NULL,
    text TEXT NOT NULL,
    witness TEXT,
    artifact_id TEXT,
    provenance_json TEXT,
    canonical_key TEXT,
    admitted_fact_seq INTEGER,
    created_at REAL NOT NULL
);
CREATE TABLE IF NOT EXISTS fact_evidence (
    fact_seq INTEGER NOT NULL,
    observation_seq INTEGER NOT NULL,
    challenge_id TEXT NOT NULL,
    PRIMARY KEY (fact_seq, observation_seq)
);
-- Explicit executable state used by Decide and the dispatcher.
CREATE TABLE IF NOT EXISTS capabilities (
    capability_key TEXT NOT NULL,
    challenge_id TEXT NOT NULL,
    target_epoch TEXT NOT NULL,
    kind TEXT NOT NULL,
    quality TEXT,
    sharing TEXT NOT NULL DEFAULT 'run-shared',
    state TEXT NOT NULL DEFAULT 'active',
    source_intent TEXT,
    access_path_id TEXT,
    evidence_facts_json TEXT,
    metadata_json TEXT,
    created_seq INTEGER NOT NULL,
    updated_seq INTEGER NOT NULL,
    PRIMARY KEY (challenge_id, target_epoch, capability_key)
);
CREATE TABLE IF NOT EXISTS runtime_resources (
    resource_id TEXT PRIMARY KEY,
    challenge_id TEXT NOT NULL,
    target_epoch TEXT NOT NULL,
    owner_worker TEXT NOT NULL,
    owner_intent TEXT,
    backend TEXT NOT NULL,
    pid INTEGER,
    container_name TEXT,
    log_path TEXT,
    cwd TEXT,
    env_json TEXT,
    cleanup_command TEXT,
    health_json TEXT,
    state TEXT NOT NULL DEFAULT 'running',
    created_seq INTEGER NOT NULL,
    updated_seq INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS access_paths (
    access_path_id TEXT PRIMARY KEY,
    challenge_id TEXT NOT NULL,
    target_epoch TEXT NOT NULL,
    runtime_resource_id TEXT NOT NULL,
    owner_worker TEXT NOT NULL,
    source_intent TEXT,
    reach_json TEXT NOT NULL,
    operations_json TEXT NOT NULL,
    quality TEXT NOT NULL,
    endpoint TEXT,
    use_spec_json TEXT NOT NULL,
    health_json TEXT,
    dependency_facts_json TEXT,
    capability_keys_json TEXT,
    state TEXT NOT NULL DEFAULT 'starting',
    consecutive_failures INTEGER NOT NULL DEFAULT 0,
    created_seq INTEGER NOT NULL,
    updated_seq INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS capability_gaps (
    gap_id TEXT PRIMARY KEY,
    challenge_id TEXT NOT NULL,
    target_epoch TEXT NOT NULL,
    worker TEXT NOT NULL,
    intent_id TEXT,
    description TEXT NOT NULL,
    required_capabilities_json TEXT,
    consumers_json TEXT,
    state TEXT NOT NULL DEFAULT 'open',
    created_seq INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS value_receipts (
    receipt_id TEXT PRIMARY KEY,
    challenge_id TEXT NOT NULL,
    intent_id TEXT NOT NULL,
    effect TEXT,
    requested_priority TEXT,
    effective_priority INTEGER,
    achieved INTEGER NOT NULL,
    capabilities_added_json TEXT,
    unblocked_count INTEGER NOT NULL DEFAULT 0,
    elapsed_seconds REAL,
    detail TEXT,
    created_seq INTEGER NOT NULL
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
    source_intent TEXT,
    from_facts_json TEXT,
    expected_observable TEXT,
    stop_condition TEXT,
    coverage_key TEXT,
    route_hash TEXT,
    lane_key TEXT,
    risk_class TEXT,
    resource_key TEXT,
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
-- Durable HITL requests. need_kind is retained for historical rows; every new
-- NEED_INPUT request waits for the operator.
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
CREATE INDEX IF NOT EXISTS idx_intent_dependencies_parent
    ON intent_dependencies(challenge_id, depends_on_intent_id);
CREATE INDEX IF NOT EXISTS idx_intent_products_fact_seq ON intent_products(fact_seq);
CREATE INDEX IF NOT EXISTS idx_observations_challenge
    ON observations(challenge_id, target_epoch, observation_seq);
CREATE INDEX IF NOT EXISTS idx_capabilities_active
    ON capabilities(challenge_id, target_epoch, state, capability_key);
CREATE INDEX IF NOT EXISTS idx_access_paths_active
    ON access_paths(challenge_id, target_epoch, state, access_path_id);
"""
