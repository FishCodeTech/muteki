//! Shared Solve Graph (`shared_graph.db`) — Slice 7.3.
//!
//! Shared event storage and projections corresponding to `muteki/swarm/shared_graph.py`, `graph_schema.py`,
//! `graph_facts.py`, `graph_intents.py`, `graph_routes.py`, `graph_locks.py`,
//! `graph_views.py`, and `graph_render.py`.

use crate::{
    AppState, native_chat,
    runs::{encode_run_id, now_secs},
};
use regex::Regex;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    fs,
    path::PathBuf,
    sync::OnceLock,
};

#[path = "graph_result.rs"]
mod result;
#[path = "graph_dispatch.rs"]
mod dispatch;
#[path = "graph_evidence.rs"]
mod evidence;
#[path = "graph_review_context.rs"]
mod review_context;
#[path = "graph_review.rs"]
mod review;
pub use review::{AutoReviewRequest, ReviewTriggerState, normalize_review_marker};
#[path = "graph_snapshot.rs"]
mod snapshot;
#[path = "graph_control.rs"]
mod control;
pub use snapshot::read_snapshot;

pub const EV_FACT_ORIGIN: &str = "fact_origin";
pub const EV_OBSERVATION_ADDED: &str = "observation_added";
pub const EV_FACT_ADDED: &str = "fact_added";
pub const EV_FACT_CHALLENGED: &str = "fact_challenged";
pub const EV_FACT_REVALIDATED: &str = "fact_revalidated";
pub const EV_FACT_REJECTED: &str = "fact_rejected";
pub const EV_FACT_MERGED: &str = "fact_merged";
pub const EV_FACT_SUPERSEDED: &str = "fact_superseded";
pub const EV_FACT_REVIEWED: &str = "fact_reviewed";
pub const EV_FACT_PINNED: &str = "fact_pinned";
pub const EV_REVIEW_FINDING: &str = "review_finding";
pub const EV_REVIEW_PROPOSAL: &str = "review_proposal";
pub const EV_REVIEW_PROPOSAL_DECISION: &str = "review_proposal_decision";
pub const EV_DEAD_END: &str = "dead_end";
pub const EV_FLAG_FOUND: &str = "flag_found";
pub const EV_FLAG_SUBMISSION: &str = "flag_submission";
pub const EV_FLAG_INVALIDATED: &str = "flag_invalidated";
pub const EV_PLATFORM_VERDICT: &str = "platform_verdict";
pub const EV_INTENT_PROPOSED: &str = "intent_proposed";
pub const EV_INTENT_CLAIMED: &str = "intent_claimed";
pub const EV_INTENT_RELEASED: &str = "intent_released";
pub const EV_INTENT_CONCLUDED: &str = "intent_concluded";
pub const EV_INTENT_STATE_CHANGED: &str = "intent_state_changed";
pub const EV_INTENT_SUPERSEDED: &str = "intent_superseded";
pub const EV_INTENT_REPRIORITIZED: &str = "intent_reprioritized";
pub const EV_INTENT_BLOCKED: &str = "intent_blocked";
pub const EV_INTENT_REOPENED: &str = "intent_reopened";
pub const EV_POC_SAVED: &str = "poc_saved";
pub const EV_POC_CLAIMED: &str = "poc_claimed";
pub const EV_POC_CONCLUDED: &str = "poc_concluded";
pub const EV_WORKER_RESULT_COMMITTED: &str = "worker_result_committed";
pub const EV_ROUTE_SUPPRESSED: &str = "route_suppressed";
pub const EV_ROUTE_REOPENED: &str = "route_reopened";
pub const EV_BRANCH_SPLIT: &str = "branch_split";
pub const EV_BRANCH_RESOLVED: &str = "branch_resolved";
pub const EV_LANE_LOCKED: &str = "lane_locked";
pub const EV_LANE_RELEASED: &str = "lane_released";
pub const EV_RESOURCE_LOCKED: &str = "resource_locked";
pub const EV_RESOURCE_RELEASED: &str = "resource_released";
pub const EV_ACTIVITY_CLAIMED: &str = "activity_claimed";
pub const EV_ACTIVITY_RELEASED: &str = "activity_released";
pub const EV_OPERATOR_DIRECTIVE: &str = "operator_directive";
pub const EV_HITL_REQUEST: &str = "hitl_request";
pub const EV_HITL_RESOLVED: &str = "hitl_resolved";
pub const EV_GRAPH_COMPACTED: &str = "graph_compacted";
pub const EV_SUB_GOAL_UPDATED: &str = "sub_goal_updated";
pub const EV_SUB_GOAL_ADDED: &str = "sub_goal_added";
pub const EV_SUB_GOAL_SATISFIED: &str = "sub_goal_satisfied";

pub const SEMANTIC_GRAPH_KINDS: &[&str] = &[
    EV_FACT_ORIGIN,
    EV_FACT_ADDED,
    EV_FACT_CHALLENGED,
    EV_FACT_REVALIDATED,
    EV_FACT_REJECTED,
    EV_FACT_MERGED,
    EV_FACT_SUPERSEDED,
    EV_DEAD_END,
    EV_FLAG_FOUND,
    EV_FLAG_INVALIDATED,
    EV_PLATFORM_VERDICT,
    EV_INTENT_CONCLUDED,
    EV_INTENT_STATE_CHANGED,
    EV_POC_SAVED,
    EV_ROUTE_SUPPRESSED,
    EV_ROUTE_REOPENED,
    EV_BRANCH_SPLIT,
    EV_BRANCH_RESOLVED,
    EV_OPERATOR_DIRECTIVE,
    EV_REVIEW_PROPOSAL_DECISION,
    EV_HITL_RESOLVED,
    EV_SUB_GOAL_UPDATED,
    EV_SUB_GOAL_ADDED,
    EV_SUB_GOAL_SATISFIED,
];

pub const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS events (
    seq         INTEGER PRIMARY KEY AUTOINCREMENT,
    ts          REAL    NOT NULL,
    kind        TEXT    NOT NULL,
    actor       TEXT    NOT NULL,
    payload     TEXT    NOT NULL,
    artifact_id TEXT,
    verified    INTEGER NOT NULL DEFAULT 0,
    confidence  REAL    NOT NULL DEFAULT 0.5,
    dedupe_key  TEXT    UNIQUE
);
CREATE INDEX IF NOT EXISTS idx_events_kind ON events(kind);
CREATE INDEX IF NOT EXISTS idx_events_actor ON events(actor);

CREATE TABLE IF NOT EXISTS intents (
    intent_id        TEXT    PRIMARY KEY,
    goal             TEXT    NOT NULL,
    worker_class     TEXT    NOT NULL DEFAULT 'code',
    status           TEXT    NOT NULL DEFAULT 'open',
    dispatch_state   TEXT    NOT NULL DEFAULT 'active',
    worker           TEXT,
    route_hash       TEXT    NOT NULL DEFAULT '',
    branch_id        TEXT    NOT NULL DEFAULT '',
    expected_observable TEXT NOT NULL DEFAULT '',
    stop_condition   TEXT    NOT NULL DEFAULT '',
    coverage_key     TEXT    NOT NULL DEFAULT '',
    lane_key         TEXT    NOT NULL DEFAULT '',
    risk_class       TEXT    NOT NULL DEFAULT '',
    resource_key     TEXT    NOT NULL DEFAULT '',
    priority         TEXT    NOT NULL DEFAULT 'normal',
    requested_priority TEXT  NOT NULL DEFAULT 'normal',
    priority_reason  TEXT    NOT NULL DEFAULT '',
    novelty_key      TEXT    NOT NULL DEFAULT '',
    value_claim_json TEXT    NOT NULL DEFAULT '{}',
    requires_capabilities_json TEXT NOT NULL DEFAULT '[]',
    required_pocs_json TEXT  NOT NULL DEFAULT '[]',
    block_reason     TEXT    NOT NULL DEFAULT '',
    dup_of           TEXT    NOT NULL DEFAULT '',
    reopen_because   TEXT    NOT NULL DEFAULT '',
    created_seq      INTEGER NOT NULL,
    claimed_seq      INTEGER,
    claimed_ts       REAL,
    lease_expires_at REAL,
    lease_suspended_remaining_s REAL,
    concluded_seq    INTEGER,
    result           TEXT
);
CREATE INDEX IF NOT EXISTS idx_intents_status ON intents(status);
CREATE INDEX IF NOT EXISTS idx_intents_dispatch_state ON intents(status, dispatch_state);
CREATE INDEX IF NOT EXISTS idx_intents_priority ON intents(status, priority, created_seq);

CREATE TABLE IF NOT EXISTS intent_sources (
    intent_id TEXT    NOT NULL,
    fact_seq  INTEGER NOT NULL,
    role      TEXT    NOT NULL DEFAULT 'prerequisite',
    PRIMARY KEY (intent_id, fact_seq)
);
CREATE INDEX IF NOT EXISTS idx_intent_sources_fact ON intent_sources(fact_seq);

CREATE TABLE IF NOT EXISTS intent_dependencies (
    intent_id            TEXT NOT NULL,
    depends_on_intent_id TEXT NOT NULL,
    PRIMARY KEY (intent_id, depends_on_intent_id)
);
CREATE INDEX IF NOT EXISTS idx_intent_deps_dep ON intent_dependencies(depends_on_intent_id);

CREATE TABLE IF NOT EXISTS intent_products (
    intent_id TEXT    NOT NULL,
    fact_seq  INTEGER NOT NULL,
    PRIMARY KEY (intent_id, fact_seq)
);
CREATE INDEX IF NOT EXISTS idx_intent_products_fact ON intent_products(fact_seq);

CREATE TABLE IF NOT EXISTS observations (
    obs_seq           INTEGER PRIMARY KEY,
    actor             TEXT    NOT NULL,
    summary           TEXT    NOT NULL,
    detail            TEXT    NOT NULL DEFAULT '',
    artifact_id       TEXT,
    evidence_json     TEXT    NOT NULL DEFAULT '{}',
    confidence        REAL    NOT NULL DEFAULT 0.5,
    intent_id         TEXT    NOT NULL DEFAULT '',
    route_hash        TEXT    NOT NULL DEFAULT '',
    branch_id         TEXT    NOT NULL DEFAULT '',
    scope_json        TEXT    NOT NULL DEFAULT '{}',
    target_epoch      INTEGER NOT NULL DEFAULT 1,
    status            TEXT    NOT NULL DEFAULT 'recorded',
    admitted_fact_seq INTEGER,
    rejected_reason   TEXT    NOT NULL DEFAULT '',
    created_ts        REAL    NOT NULL,
    updated_ts        REAL    NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_observations_status ON observations(status, obs_seq);
CREATE INDEX IF NOT EXISTS idx_observations_intent ON observations(intent_id, obs_seq);

CREATE TABLE IF NOT EXISTS fact_evidence (
    fact_seq    INTEGER NOT NULL,
    obs_seq     INTEGER NOT NULL,
    relation    TEXT    NOT NULL DEFAULT 'supports',
    created_seq INTEGER NOT NULL,
    PRIMARY KEY (fact_seq, obs_seq, relation)
);
CREATE INDEX IF NOT EXISTS idx_fact_evidence_obs ON fact_evidence(obs_seq);

CREATE TABLE IF NOT EXISTS capabilities (
    capability_id   TEXT    PRIMARY KEY,
    kind            TEXT    NOT NULL,
    scope           TEXT    NOT NULL DEFAULT '',
    summary         TEXT    NOT NULL DEFAULT '',
    status          TEXT    NOT NULL DEFAULT 'active',
    source_fact_seq INTEGER,
    source_obs_seq  INTEGER,
    intent_id       TEXT    NOT NULL DEFAULT '',
    actor           TEXT    NOT NULL,
    metadata_json   TEXT    NOT NULL DEFAULT '{}',
    created_seq     INTEGER NOT NULL,
    updated_seq     INTEGER NOT NULL,
    updated_ts      REAL    NOT NULL
);

CREATE TABLE IF NOT EXISTS runtime_resources (
    resource_id     TEXT    PRIMARY KEY,
    kind            TEXT    NOT NULL,
    location        TEXT    NOT NULL,
    summary         TEXT    NOT NULL DEFAULT '',
    status          TEXT    NOT NULL DEFAULT 'available',
    source_fact_seq INTEGER,
    source_obs_seq  INTEGER,
    intent_id       TEXT    NOT NULL DEFAULT '',
    actor           TEXT    NOT NULL,
    metadata_json   TEXT    NOT NULL DEFAULT '{}',
    created_seq     INTEGER NOT NULL,
    updated_seq     INTEGER NOT NULL,
    updated_ts      REAL    NOT NULL
);

CREATE TABLE IF NOT EXISTS access_paths (
    path_id            TEXT    PRIMARY KEY,
    from_capability_id TEXT    NOT NULL DEFAULT '',
    to_capability_id   TEXT    NOT NULL,
    via_resource_id    TEXT    NOT NULL DEFAULT '',
    summary            TEXT    NOT NULL DEFAULT '',
    status             TEXT    NOT NULL DEFAULT 'open',
    cost_class         TEXT    NOT NULL DEFAULT 'medium',
    risk_class         TEXT    NOT NULL DEFAULT 'read',
    source_fact_seq    INTEGER,
    source_obs_seq     INTEGER,
    intent_id          TEXT    NOT NULL DEFAULT '',
    actor              TEXT    NOT NULL,
    created_seq        INTEGER NOT NULL,
    updated_seq        INTEGER NOT NULL,
    updated_ts         REAL    NOT NULL
);

CREATE TABLE IF NOT EXISTS capability_gaps (
    gap_key            TEXT    PRIMARY KEY,
    capability_kind    TEXT    NOT NULL,
    scope              TEXT    NOT NULL DEFAULT '',
    needed_for         TEXT    NOT NULL DEFAULT '',
    blocker            TEXT    NOT NULL DEFAULT '',
    status             TEXT    NOT NULL DEFAULT 'open',
    intent_id          TEXT    NOT NULL DEFAULT '',
    route_hash         TEXT    NOT NULL DEFAULT '',
    branch_id          TEXT    NOT NULL DEFAULT '',
    source_fact_seq    INTEGER,
    source_obs_seq     INTEGER,
    resolved_by_capability_id TEXT NOT NULL DEFAULT '',
    actor              TEXT    NOT NULL,
    created_seq        INTEGER NOT NULL,
    updated_seq        INTEGER NOT NULL,
    updated_ts         REAL    NOT NULL
);

CREATE TABLE IF NOT EXISTS value_receipts (
    intent_id             TEXT    PRIMARY KEY,
    worker                TEXT    NOT NULL,
    conclude_result       TEXT    NOT NULL,
    claimed_effect        TEXT    NOT NULL DEFAULT '',
    novelty_key           TEXT    NOT NULL DEFAULT '',
    value_claim_json      TEXT    NOT NULL DEFAULT '{}',
    observations_added    INTEGER NOT NULL DEFAULT 0,
    verified_facts_added  INTEGER NOT NULL DEFAULT 0,
    capabilities_added    INTEGER NOT NULL DEFAULT 0,
    resources_added       INTEGER NOT NULL DEFAULT 0,
    access_paths_added    INTEGER NOT NULL DEFAULT 0,
    gaps_opened           INTEGER NOT NULL DEFAULT 0,
    gaps_resolved         INTEGER NOT NULL DEFAULT 0,
    dead_ends_added       INTEGER NOT NULL DEFAULT 0,
    pocs_saved            INTEGER NOT NULL DEFAULT 0,
    realized_value        TEXT    NOT NULL DEFAULT 'none',
    concluded_seq         INTEGER NOT NULL,
    updated_ts            REAL    NOT NULL
);

CREATE TABLE IF NOT EXISTS pocs (
    poc_id               TEXT    PRIMARY KEY,
    name                 TEXT    NOT NULL,
    path                 TEXT    NOT NULL,
    entry_command        TEXT    NOT NULL,
    target               TEXT    NOT NULL DEFAULT '',
    parameters_json      TEXT    NOT NULL DEFAULT '{}',
    expected_signal      TEXT    NOT NULL DEFAULT '',
    output_format        TEXT    NOT NULL DEFAULT '',
    requires_json        TEXT    NOT NULL DEFAULT '[]',
    produces_json        TEXT    NOT NULL DEFAULT '[]',
    verified_artifact_id TEXT,
    status               TEXT    NOT NULL DEFAULT 'draft',
    owner                TEXT,
    claimed_intent_id    TEXT,
    claim_seq            INTEGER,
    claim_ts             REAL,
    lease_expires_at     REAL,
    lease_suspended_remaining_s REAL,
    created_by           TEXT    NOT NULL,
    created_seq          INTEGER NOT NULL,
    updated_seq          INTEGER NOT NULL,
    note                 TEXT    NOT NULL DEFAULT '',
    content_sha256       TEXT    NOT NULL DEFAULT '',
    size_bytes           INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_pocs_status ON pocs(status);

CREATE TABLE IF NOT EXISTS activity_locks (
    activity_key     TEXT    PRIMARY KEY,
    owner            TEXT    NOT NULL,
    intent_id        TEXT    NOT NULL DEFAULT '',
    claimed_seq      INTEGER NOT NULL,
    claimed_ts       REAL    NOT NULL,
    lease_expires_at REAL    NOT NULL,
    lease_suspended_remaining_s REAL
);

CREATE TABLE IF NOT EXISTS lane_locks (
    lane_key         TEXT    PRIMARY KEY,
    risk_class       TEXT    NOT NULL,
    owner            TEXT    NOT NULL,
    intent_id        TEXT    NOT NULL DEFAULT '',
    branch_id        TEXT    NOT NULL DEFAULT '',
    reason           TEXT    NOT NULL DEFAULT '',
    claimed_seq      INTEGER NOT NULL,
    claimed_ts       REAL    NOT NULL,
    lease_expires_at REAL    NOT NULL,
    lease_suspended_remaining_s REAL
);

CREATE TABLE IF NOT EXISTS resource_locks (
    resource_key     TEXT    PRIMARY KEY,
    holder_worker_id TEXT    NOT NULL,
    intent_id        TEXT    NOT NULL DEFAULT '',
    acquired_seq     INTEGER NOT NULL,
    acquired_at      REAL    NOT NULL,
    lease_expires_at REAL    NOT NULL,
    lease_suspended_remaining_s REAL,
    released_seq     INTEGER,
    released_at      REAL,
    release_reason   TEXT    NOT NULL DEFAULT ''
);

CREATE TABLE IF NOT EXISTS routes (
    route_hash        TEXT    PRIMARY KEY,
    status            TEXT    NOT NULL DEFAULT 'open',
    reason            TEXT    NOT NULL DEFAULT '',
    suppressed_by     TEXT,
    suppressed_seq    INTEGER,
    reopen_condition  TEXT    NOT NULL DEFAULT '',
    reopened_seq      INTEGER,
    reopened_fact_seq INTEGER,
    updated_ts        REAL    NOT NULL
);

CREATE TABLE IF NOT EXISTS fact_reviews (
    fact_seq               INTEGER PRIMARY KEY,
    verdict                TEXT    NOT NULL,
    reviewer               TEXT    NOT NULL,
    rationale              TEXT    NOT NULL DEFAULT '',
    reopen_condition       TEXT    NOT NULL DEFAULT '',
    verification_intent_id TEXT    NOT NULL DEFAULT '',
    reviewed_seq           INTEGER NOT NULL,
    updated_ts             REAL    NOT NULL
);

CREATE TABLE IF NOT EXISTS branches (
    branch_id        TEXT    PRIMARY KEY,
    route_hash       TEXT    NOT NULL DEFAULT '',
    hypothesis       TEXT    NOT NULL,
    discriminator    TEXT    NOT NULL DEFAULT '',
    lane_key         TEXT    NOT NULL DEFAULT '',
    risk_class       TEXT    NOT NULL DEFAULT '',
    resource_key     TEXT    NOT NULL DEFAULT '',
    coverage_key     TEXT    NOT NULL DEFAULT '',
    source_fact_seqs TEXT    NOT NULL DEFAULT '[]',
    status           TEXT    NOT NULL DEFAULT 'open',
    created_by       TEXT    NOT NULL,
    created_seq      INTEGER NOT NULL,
    resolved_seq     INTEGER,
    resolution       TEXT    NOT NULL DEFAULT '',
    updated_ts       REAL    NOT NULL
);

CREATE TABLE IF NOT EXISTS fact_states (
    fact_seq               INTEGER PRIMARY KEY,
    status                 TEXT    NOT NULL DEFAULT 'active',
    canonical_fact_seq     INTEGER,
    superseded_by_seq      INTEGER,
    rationale              TEXT    NOT NULL DEFAULT '',
    verification_intent_id TEXT    NOT NULL DEFAULT '',
    updated_by             TEXT    NOT NULL DEFAULT '',
    updated_seq            INTEGER NOT NULL,
    updated_ts             REAL    NOT NULL
);

CREATE TABLE IF NOT EXISTS fact_pins (
    fact_seq    INTEGER PRIMARY KEY,
    pinned_by   TEXT    NOT NULL,
    reason      TEXT    NOT NULL DEFAULT '',
    pinned_seq  INTEGER NOT NULL,
    updated_ts  REAL    NOT NULL
);

CREATE TABLE IF NOT EXISTS fact_merges (
    alias_fact_seq     INTEGER PRIMARY KEY,
    canonical_fact_seq INTEGER NOT NULL,
    merged_by          TEXT    NOT NULL,
    rationale          TEXT    NOT NULL DEFAULT '',
    merged_seq         INTEGER NOT NULL,
    updated_ts         REAL    NOT NULL
);

CREATE TABLE IF NOT EXISTS operator_directives (
    directive_id    TEXT    PRIMARY KEY,
    challenge_id    TEXT    NOT NULL DEFAULT '',
    actor           TEXT    NOT NULL,
    target          TEXT    NOT NULL DEFAULT 'coordinator',
    action          TEXT    NOT NULL DEFAULT 'directive',
    text            TEXT    NOT NULL,
    route_hash      TEXT    NOT NULL DEFAULT '',
    branch_id       TEXT    NOT NULL DEFAULT '',
    standing        INTEGER NOT NULL DEFAULT 1,
    status          TEXT    NOT NULL DEFAULT 'pending',
    status_reason   TEXT    NOT NULL DEFAULT '',
    preempt_policy  TEXT    NOT NULL DEFAULT 'soft_rebind',
    priority        INTEGER NOT NULL DEFAULT 50,
    received_seq    INTEGER NOT NULL DEFAULT 0,
    bound_worker    TEXT    NOT NULL DEFAULT '',
    created_seq     INTEGER NOT NULL,
    updated_seq     INTEGER NOT NULL,
    updated_ts      REAL    NOT NULL
);

CREATE TABLE IF NOT EXISTS hitl_requests (
    request_id      TEXT    PRIMARY KEY,
    worker          TEXT    NOT NULL,
    kind            TEXT    NOT NULL DEFAULT 'need_input',
    need_kind       TEXT    NOT NULL DEFAULT 'external_blocker',
    need            TEXT    NOT NULL,
    why             TEXT    NOT NULL DEFAULT '',
     tried           TEXT    NOT NULL DEFAULT '',
    unblocks        TEXT    NOT NULL DEFAULT '',
    artifact_id     TEXT,
    intent_id       TEXT    NOT NULL DEFAULT '',
    route_hash      TEXT    NOT NULL DEFAULT '',
    branch_id       TEXT    NOT NULL DEFAULT '',
    dedupe_key      TEXT    NOT NULL DEFAULT '',
    status          TEXT    NOT NULL DEFAULT 'open',
    resolution      TEXT    NOT NULL DEFAULT '',
    resolved_by     TEXT    NOT NULL DEFAULT '',
    created_seq     INTEGER NOT NULL,
    updated_seq     INTEGER NOT NULL,
    created_ts      REAL    NOT NULL,
    updated_ts      REAL    NOT NULL
);

CREATE TABLE IF NOT EXISTS compact_epochs (
    epoch_id     INTEGER PRIMARY KEY AUTOINCREMENT,
    before_seq   INTEGER NOT NULL,
    summary_seq  INTEGER NOT NULL,
    summary_text TEXT    NOT NULL,
    actor        TEXT    NOT NULL,
    created_ts   REAL    NOT NULL
);

CREATE TABLE IF NOT EXISTS sub_goals (
    sub_goal_id   TEXT    PRIMARY KEY,
    goal          TEXT    NOT NULL,
    status        TEXT    NOT NULL DEFAULT 'open',
    depends_on    TEXT    NOT NULL DEFAULT '[]',
    evidence_seqs TEXT    NOT NULL DEFAULT '[]',
    note          TEXT    NOT NULL DEFAULT '',
    created_seq   INTEGER NOT NULL,
    updated_seq   INTEGER NOT NULL,
    updated_ts    REAL    NOT NULL
);
"#;

pub fn graph_dir(state: &AppState, run_id: &str) -> PathBuf {
    native_chat::state_dir(state)
        .join("graphs")
        .join(encode_run_id(run_id))
}

#[cfg(test)]
mod false_positive_tests {
    use super::*;

    #[test]
    fn platform_verdict_replay_preserves_partial_progress_and_rejection() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("graph.db");
        let mut first = SharedGraph::open_at(path.clone()).unwrap();
        let partial = first.record_platform_verdict("sub-one","flag{one}","accepted",
            false,Some(1),Some(2),"").unwrap();
        let mut second = SharedGraph::open_at(path).unwrap();
        assert_eq!(second.platform_verdicts(),vec![partial.clone()]);
        assert_eq!(second.record_platform_verdict("sub-one","flag{one}","accepted",
            false,Some(1),Some(2),"").unwrap(),partial);
        assert!(second.record_platform_verdict("sub-one","flag{other}","accepted",
            false,Some(1),Some(2),"").is_err());
        second.record_platform_verdict("sub-wrong","flag{wrong}","rejected",
            false,Some(1),Some(2),"wrong answer").unwrap();
        let events = second.events();
        assert_eq!(events.iter().filter(|event|event["kind"] == EV_PLATFORM_VERDICT).count(),2);
        assert_eq!(events.iter().filter(|event|event["kind"] == EV_FLAG_INVALIDATED).count(),1);
        assert_eq!(events.iter().filter(|event|event["kind"] == EV_DEAD_END).count(),1);
    }

    #[test]
    fn reopen_only_solved_intent_linked_to_false_flag_once() {
        let directory = tempfile::tempdir().unwrap();
        let mut graph = SharedGraph::open_at(directory.path().join("graph.db")).unwrap();
        for id in ["linked", "unrelated", "superseded"] {
            assert!(graph.propose_intent("worker",id,id,&json!({}),None) > 0);
            assert!(graph.claim_intent("worker",id,60.));
        }
        graph.conclude_intent("worker","linked","solved","flag found");
        graph.conclude_intent("worker","unrelated","solved","another route");
        graph.conclude_intent("worker","superseded","superseded","retired ask");
        graph.append_event(EV_FLAG_FOUND,"worker",
            &json!({"flag":"flag{wrong}","intent_id":"linked"}),None,true,1.0,None);
        let reopened = graph.reopen_after_false_positive("competition","flag{wrong}","remote wrong").unwrap();
        assert_eq!(reopened, vec!["linked"]);
        let intents = graph.all_intents();
        assert_eq!(intents.iter().find(|row| row["intent_id"] == "linked").unwrap()["status"],"open");
        assert_eq!(intents.iter().find(|row| row["intent_id"] == "unrelated").unwrap()["status"],"done");
        assert_eq!(intents.iter().find(|row| row["intent_id"] == "superseded").unwrap()["status"],"superseded");
        assert!(graph.reopen_after_false_positive("competition","flag{wrong}","remote wrong").unwrap().is_empty());
        assert_eq!(graph.events().iter().filter(|row| row["kind"] == EV_FLAG_INVALIDATED).count(),1);
    }
}

#[cfg(test)]
mod pentest_decide_atomic_tests {
    use super::*;

    #[test]
    fn pentest_decide_drop_steps_closes_exact_active_ids_with_one_event() {
        let directory = tempfile::tempdir().unwrap();
        let mut graph = SharedGraph::open_at(directory.path().join("graph.db")).unwrap();
        for id in ["queued", "running", "finished"] {
            assert!(graph.propose_intent("reason", id, id, &json!({}), None) > 0);
        }
        assert!(graph.claim_intent("worker", "running", 60.0));
        assert!(graph.claim_intent("worker", "finished", 60.0));
        graph.conclude_intent("worker", "finished", "done", "already finished");
        let before = graph.events().len();
        let dropped = graph
            .pentest_decide_drop_steps(
                "reason",
                &["queued".into(), "running".into(), "queued".into(),
                  "finished".into(), "missing".into()],
                "new evidence",
            )
            .unwrap();
        assert_eq!(dropped, vec!["queued", "running"]);
        let events = graph.events();
        assert_eq!(events.len(), before + 1);
        let event = events.last().unwrap();
        assert_eq!(event["kind"], EV_INTENT_CONCLUDED);
        assert_eq!(event["actor"], "reason");
        assert_eq!(event["payload"]["intent_ids"], json!(["queued", "running"]));
        assert_eq!(event["payload"]["result"], "superseded");
        assert_eq!(event["payload"]["reason"], "new evidence");
        for id in &dropped {
            let row = graph.all_intents().into_iter()
                .find(|row| row["intent_id"] == *id).unwrap();
            assert_eq!(row["status"], "done");
            assert_eq!(row["dispatch_state"], "closed");
            assert_eq!(row["result"], "superseded");
            assert_eq!(row["concluded_seq"], event["seq"]);
        }
        assert!(graph.pentest_decide_drop_steps("reason", &dropped, "again").unwrap().is_empty());
        assert_eq!(graph.events().len(), events.len());
    }

    #[test]
    fn pentest_decide_reprioritize_steps_updates_only_queued_with_one_event() {
        let directory = tempfile::tempdir().unwrap();
        let mut graph = SharedGraph::open_at(directory.path().join("graph.db")).unwrap();
        for id in ["high_step", "low_step", "claimed_step"] {
            assert!(graph.propose_intent("reason", id, id, &json!({}), None) > 0);
        }
        assert!(graph.claim_intent("worker", "claimed_step", 60.0));
        let mut priorities = HashMap::new();
        priorities.insert("high_step".to_owned(), "high".to_owned());
        priorities.insert("low_step".to_owned(), "low".to_owned());
        priorities.insert("claimed_step".to_owned(), "high".to_owned());
        priorities.insert("invalid".to_owned(), "urgent".to_owned());
        let before = graph.events().len();
        let changed = graph.pentest_decide_reprioritize_steps("reason", &priorities).unwrap();
        assert_eq!(changed, vec!["high_step", "low_step"]);
        let events = graph.events();
        assert_eq!(events.len(), before + 1);
        let event = events.last().unwrap();
        assert_eq!(event["kind"], EV_INTENT_STATE_CHANGED);
        assert_eq!(event["payload"]["intent_ids"], json!(["high_step", "low_step"]));
        assert_eq!(event["payload"]["priorities"], json!({"high_step":50,"low_step":-10}));
        assert_eq!(event["payload"]["reason"], "decide_reprioritized");
        for (id, expected) in [("high_step", "high"), ("low_step", "low"),
            ("claimed_step", "normal")] {
            let row: (String, String, String) = graph.conn.query_row(
                "SELECT priority,requested_priority,priority_reason FROM intents WHERE intent_id=?1",
                [id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            ).unwrap();
            assert_eq!(row.0, expected);
            assert_eq!(row.1, expected);
            assert_eq!(row.2, if id == "claimed_step" { "" } else { "decide_reprioritized" });
        }
        assert!(graph.pentest_decide_reprioritize_steps("reason", &priorities).unwrap().is_empty());
        assert_eq!(graph.events().len(), events.len());
    }

    #[test]
    fn pentest_decide_reprioritize_steps_rolls_back_prior_rows_on_failure() {
        let directory = tempfile::tempdir().unwrap();
        let mut graph = SharedGraph::open_at(directory.path().join("graph.db")).unwrap();
        for id in ["a", "b"] {
            assert!(graph.propose_intent("reason", id, id, &json!({}), None) > 0);
        }
        graph.conn.execute_batch(
            "CREATE TRIGGER reject_b_priority BEFORE UPDATE OF priority ON intents
             WHEN NEW.intent_id='b' BEGIN SELECT RAISE(FAIL,'priority rejected'); END;",
        ).unwrap();
        let before = graph.events().len();
        let priorities = HashMap::from([
            ("a".to_owned(), "high".to_owned()),
            ("b".to_owned(), "low".to_owned()),
        ]);
        assert!(graph.pentest_decide_reprioritize_steps("reason", &priorities).is_err());
        let a_priority: String = graph.conn.query_row(
            "SELECT priority FROM intents WHERE intent_id='a'", [], |row| row.get(0)
        ).unwrap();
        assert_eq!(a_priority, "normal");
        assert_eq!(graph.events().len(), before);
    }

    #[test]
    fn pentest_decide_supersede_fact_retires_only_queued_dependents_and_reopens_goal() {
        let directory = tempfile::tempdir().unwrap();
        let mut graph = SharedGraph::open_at(directory.path().join("graph.db")).unwrap();
        let origin = graph.seed_origin_fact(&json!({"name":"DVWA","target":"http://dvwa/"}));
        let old_fact = graph.append_event(EV_FACT_ADDED, "worker", &json!({"fact":"old"}),
            None, true, 1.0, None);
        let replacement = graph.append_event(EV_FACT_ADDED, "worker", &json!({"fact":"new"}),
            None, true, 1.0, None);
        assert!(origin > 0 && old_fact > 0 && replacement > 0);
        for id in ["queued", "running"] {
            assert!(graph.propose_intent("reason", id, id, &json!({}), Some(&[old_fact])) > 0);
        }
        assert!(graph.claim_intent("worker", "running", 60.0));
        assert_eq!(graph.pentest_decide_add_sub_goal("reason", "Verify login", "goal_001").unwrap(),
            "goal_001");
        assert!(graph.pentest_decide_satisfy_sub_goal("reason", "goal_001", "old evidence",
            &[old_fact]).unwrap() > 0);
        assert_eq!(graph.sub_goals()[0]["state"], "satisfied");

        let before = graph.events().len();
        let seq = graph.pentest_decide_supersede_fact("reason", old_fact,
            "new observation contradicted old", Some(replacement)).unwrap();
        assert!(seq > 0);
        let events = graph.events();
        assert_eq!(events.len(), before + 2);
        let supersede = events.iter().find(|event| event["seq"] == seq).unwrap();
        assert_eq!(supersede["kind"], EV_FACT_SUPERSEDED);
        assert_eq!(supersede["payload"]["by_fact_seq"], replacement);
        let retirement = events.last().unwrap();
        assert_eq!(retirement["kind"], EV_INTENT_STATE_CHANGED);
        assert_eq!(retirement["payload"]["intent_id"], "queued");
        assert_eq!(retirement["payload"]["dispatch_state"], "retired");
        assert!(!graph.active_facts().iter().any(|fact| fact["fact_seq"] == old_fact));
        assert!(graph.active_facts().iter().any(|fact| fact["fact_seq"] == replacement));
        let intents = graph.all_intents();
        let queued = intents.iter().find(|intent| intent["intent_id"] == "queued").unwrap();
        assert_eq!(queued["status"], "done");
        assert_eq!(queued["dispatch_state"], "retired");
        assert_eq!(queued["concluded_seq"], retirement["seq"]);
        let running = intents.iter().find(|intent| intent["intent_id"] == "running").unwrap();
        assert_eq!(running["status"], "claimed");
        assert_eq!(running["dispatch_state"], "active");
        assert_eq!(graph.sub_goals()[0]["state"], "open");
        assert_eq!(graph.pentest_decide_supersede_fact("reason", old_fact,
            "new observation contradicted old", Some(replacement)).unwrap(), -1);
        assert_eq!(graph.events().len(), events.len());
        assert!(graph.pentest_decide_supersede_fact("reason", old_fact,
            "different stale reason", Some(replacement)).unwrap() > 0);
        assert!(graph.pentest_decide_supersede_fact("reason", origin, "not allowed", None).is_err());
    }

    #[test]
    fn pentest_decide_sub_goal_ids_and_satisfaction_follow_active_fact_evidence() {
        let directory = tempfile::tempdir().unwrap();
        let mut graph = SharedGraph::open_at(directory.path().join("graph.db")).unwrap();
        let fact = graph.append_event(EV_FACT_ADDED, "worker", &json!({"fact":"response"}),
            None, true, 1.0, None);
        assert_eq!(graph.pentest_decide_add_sub_goal("reason", "Check response", "").unwrap(),
            "goal_001");
        assert_eq!(graph.pentest_decide_add_sub_goal("reason", "Check response", "goal_001").unwrap(),
            "goal_001");
        assert!(graph.pentest_decide_add_sub_goal("reason", "Different criterion", "goal_001").is_err());
        assert!(graph.pentest_decide_add_sub_goal("reason", "Final", "goal_final").is_err());
        assert_eq!(graph.pentest_decide_add_sub_goal("reason", "Check session", "").unwrap(),
            "goal_002");
        assert!(graph.pentest_decide_satisfy_sub_goal("reason", "goal_missing", "why", &[fact]).is_err());
        assert!(graph.pentest_decide_satisfy_sub_goal("reason", "goal_002", "why", &[999]).is_err());
        let seq = graph.pentest_decide_satisfy_sub_goal("reason", "goal_001", "observed", &[fact,fact])
            .unwrap();
        assert!(seq > 0);
        assert_eq!(graph.pentest_decide_satisfy_sub_goal("reason", "goal_001", "observed", &[fact])
            .unwrap(), 0);
        let goals = graph.sub_goals();
        assert_eq!(goals.len(), 2);
        assert_eq!(goals[0]["goal_id"], "goal_001");
        assert_eq!(goals[0]["state"], "satisfied");
        assert_eq!(goals[0]["evidence_fact_seqs"], json!([fact]));
        assert_eq!(goals[1]["state"], "open");

        let yaml = graph.to_ctf_graph_yaml(&json!({
            "mode": "pentest", "description": "Audit DVWA", "target": "http://dvwa/"
        }));
        let view: serde_yaml::Value = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(view["goals"][0]["id"], "goal_final");
        assert_eq!(view["goals"][0]["kind"], "pentest_objective");
        assert_eq!(view["goals"][0]["criterion"], "Audit DVWA");
        assert_eq!(view["goals"][1]["id"], "goal_001");
        assert_eq!(view["goals"][1]["kind"], "sub");
        assert_eq!(view["goals"][1]["state"], "satisfied");
        assert_eq!(view["goals"][1]["criterion"], "Check response");
        assert_eq!(view["goals"][2]["id"], "goal_002");
        assert_eq!(view["goals"][2]["state"], "open");
    }

    #[test]
    fn pentest_decide_supersede_fact_rolls_back_when_retirement_fails() {
        let directory = tempfile::tempdir().unwrap();
        let graph = SharedGraph::open_at(directory.path().join("graph.db")).unwrap();
        let fact = graph.append_event(EV_FACT_ADDED, "worker", &json!({"fact":"old"}),
            None, true, 1.0, None);
        assert!(graph.propose_intent("reason", "queued", "queued", &json!({}), Some(&[fact])) > 0);
        graph.conn.execute_batch(
            "CREATE TRIGGER reject_retire BEFORE UPDATE OF dispatch_state ON intents
             WHEN NEW.intent_id='queued' BEGIN SELECT RAISE(FAIL,'retire rejected'); END;",
        ).unwrap();
        let before = graph.events().len();
        assert!(graph.pentest_decide_supersede_fact("reason", fact, "contradicted", None).is_err());
        assert_eq!(graph.events().len(), before);
        assert!(graph.active_facts().iter().any(|row| row["fact_seq"] == fact));
        assert_eq!(graph.all_intents()[0]["status"], "open");
    }
}

pub fn graph_db_path(state: &AppState, run_id: &str) -> PathBuf {
    graph_dir(state, run_id).join("shared_graph.db")
}

pub fn canonical_fact_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn active_fact_seqs_from(conn: &Connection) -> Result<HashSet<i64>, String> {
    let mut stmt = conn.prepare(
        "SELECT e.seq FROM events e LEFT JOIN fact_states fs ON fs.fact_seq=e.seq
         WHERE e.kind IN (?1,?2) AND e.verified=1
         AND COALESCE(fs.status,'active') IN ('active','verified')",
    ).map_err(|error| error.to_string())?;
    stmt.query_map(params![EV_FACT_ORIGIN, EV_FACT_ADDED], |row| row.get::<_, i64>(0))
        .map_err(|error| error.to_string())?
        .collect::<Result<HashSet<_>, _>>()
        .map_err(|error| error.to_string())
}

fn sub_goal_rows_from(conn: &Connection) -> Result<Vec<Value>, String> {
    let active = active_fact_seqs_from(conn)?;
    let mut stmt = conn.prepare(
        "SELECT seq,ts,kind,payload FROM events
         WHERE kind IN (?1,?2) ORDER BY seq",
    ).map_err(|error| error.to_string())?;
    let rows = stmt.query_map(params![EV_SUB_GOAL_ADDED, EV_SUB_GOAL_SATISFIED], |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, f64>(1)?,
            row.get::<_, String>(2)?, row.get::<_, String>(3)?))
    }).map_err(|error| error.to_string())?;
    let mut order = Vec::new();
    let mut goals: HashMap<String, Value> = HashMap::new();
    for row in rows {
        let (seq, ts, kind, raw) = row.map_err(|error| error.to_string())?;
        let payload: Value = serde_json::from_str(&raw).map_err(|error| error.to_string())?;
        let Some(goal_id) = payload["goal_id"].as_str().filter(|id| !id.is_empty()) else {
            continue;
        };
        if kind == EV_SUB_GOAL_ADDED {
            if !goals.contains_key(goal_id) {
                order.push(goal_id.to_owned());
            }
            goals.insert(goal_id.to_owned(), json!({
                "goal_id":goal_id,"criterion":payload["criterion"],"state":"open",
                "reason":"","evidence_fact_seqs":[],"created_at":ts,
                "satisfied_at":null,"created_seq":seq,"updated_seq":seq,
            }));
        } else if let Some(goal) = goals.get_mut(goal_id) {
            goal["state"] = json!("satisfied");
            goal["reason"] = payload["reason"].clone();
            goal["evidence_fact_seqs"] = payload["evidence_fact_seqs"].clone();
            goal["satisfied_at"] = json!(ts);
            goal["updated_seq"] = json!(seq);
        }
    }
    Ok(order.into_iter().filter_map(|id| goals.remove(&id)).map(|mut goal| {
        if goal["state"] == "satisfied" {
            let evidence: Vec<i64> = goal["evidence_fact_seqs"]
                .as_array().into_iter().flatten().filter_map(Value::as_i64).collect();
            if evidence.is_empty() || evidence.iter().any(|seq| !active.contains(seq)) {
                goal["state"] = json!("open");
            }
        }
        goal
    }).collect())
}

pub fn normalize_route_hash(raw: &str) -> String {
    raw.trim().to_ascii_lowercase()
}

pub fn normalize_lane_key(raw: &str) -> String {
    raw.trim().to_ascii_lowercase()
}

pub struct SharedGraph {
    pub db_path: PathBuf,
    pub conn: Connection,
}

impl SharedGraph {
    pub fn open(state: &AppState, run_id: &str) -> Result<Self, String> {
        let dir = graph_dir(state, run_id);
        fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&dir, fs::Permissions::from_mode(0o700));
        }
        let db_path = dir.join("shared_graph.db");
        Self::open_at(db_path)
    }

    pub fn open_at(db_path: PathBuf) -> Result<Self, String> {
        if let Some(parent) = db_path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let conn = Connection::open(&db_path).map_err(|e| e.to_string())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&db_path, fs::Permissions::from_mode(0o600));
        }
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA busy_timeout=5000;
             PRAGMA synchronous=NORMAL;
             PRAGMA foreign_keys=ON;",
        )
        .map_err(|e| e.to_string())?;
        conn.execute_batch(SCHEMA).map_err(|e| e.to_string())?;
        let has_consumed=conn.query_row("SELECT COUNT(*) FROM pragma_table_info('operator_directives') WHERE name='consumed_revision'",[],|r|r.get::<_,i64>(0)).map_err(|e|e.to_string())?>0;
        if !has_consumed{conn.execute("ALTER TABLE operator_directives ADD COLUMN consumed_revision INTEGER",[]).map_err(|e|e.to_string())?;}
        let has_compact=conn.query_row("SELECT COUNT(*) FROM pragma_table_info('intents') WHERE name='compact_id'",[],|row|row.get::<_,i64>(0)).map_err(|e|e.to_string())?>0;
        if !has_compact{conn.execute("ALTER TABLE intents ADD COLUMN compact_id INTEGER",[]).map_err(|e|e.to_string())?;}

        let _ = conn.execute(
            "ALTER TABLE fact_reviews ADD COLUMN verification_intent_id TEXT NOT NULL DEFAULT ''",
            [],
        );
        let _ = conn.execute(
            "ALTER TABLE fact_states ADD COLUMN verification_intent_id TEXT NOT NULL DEFAULT ''",
            [],
        );
        Ok(Self { db_path, conn })
    }

    pub fn append_event(
        &self,
        kind: &str,
        actor: &str,
        payload: &Value,
        artifact_id: Option<&str>,
        verified: bool,
        confidence: f64,
        dedupe_key: Option<&str>,
    ) -> i64 {
        let ts = now_secs();
        let payload_str = serde_json::to_string(payload).unwrap_or_else(|_| "{}".to_owned());
        match self.conn.execute(
            "INSERT INTO events (ts, kind, actor, payload, artifact_id, verified, confidence, dedupe_key)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                ts,
                kind,
                actor,
                payload_str,
                artifact_id.filter(|s| !s.is_empty()),
                if verified { 1 } else { 0 },
                confidence,
                dedupe_key.filter(|s| !s.is_empty()),
            ],
        ) {
            Ok(_) => self.conn.last_insert_rowid(),
            Err(rusqlite::Error::SqliteFailure(err, _))
                if err.code == rusqlite::ErrorCode::ConstraintViolation =>
            {
                -1
            }
            Err(_) => -1,
        }
    }

    /// Seeds the initial `fact_origin` event for a run if not already present.
    #[cfg(test)]
    pub fn seed_origin_fact(&self, challenge: &Value) -> i64 {
        self.seed_origin_fact_checked(challenge).expect("test origin admission")
    }

    pub fn seed_origin_fact_checked(&self, challenge: &Value) -> Result<i64,String> {
        self.with_immediate_tx(|| {
            if let Some(seq)=self.conn.query_row("SELECT seq FROM events WHERE kind=?1 ORDER BY seq LIMIT 1",[EV_FACT_ORIGIN],|row|row.get::<_,i64>(0)).optional().map_err(|e|e.to_string())?{return Ok(seq);}
            let description=challenge["description"].as_str().unwrap_or("");
            let mode=challenge["mode"].as_str().unwrap_or("ctf");
            let text=if mode=="pentest"{description.to_owned()}else{format!("题目信息:\n{description}")};
            let epoch=challenge["target_epoch"].as_u64().or_else(||challenge["execution_generation"].as_u64()).unwrap_or(1);
            let (_,fact)=self.add_evidence_checked("origin",text.lines().next().unwrap_or(""),&text,None,true,1.,None,None,Some(&json!({"mode":mode,"target_epoch":epoch})))?;
            let seq=fact.ok_or("Origin observation was not admitted")?;
            let raw:String=self.conn.query_row("SELECT payload FROM events WHERE seq=?1",[seq],|row|row.get(0)).map_err(|e|e.to_string())?;
            let mut payload:Value=serde_json::from_str(&raw).map_err(|e|e.to_string())?;
            payload["origin"]=json!(true);payload["source"]=json!("origin");
            payload["description"]=json!(description);payload["target"]=challenge["target"].clone();payload["category"]=challenge["category"].clone();payload["flag_format"]=challenge["flag_format"].clone();
            self.conn.execute("UPDATE events SET kind=?1,payload=?2,dedupe_key='origin:challenge-root' WHERE seq=?3",params![EV_FACT_ORIGIN,payload.to_string(),seq]).map_err(|e|e.to_string())?;
            Ok(seq)
        })
    }

    /// Adds an observation and (if verified) promotes it to a deduplicated `fact_added` event.
    /// Returns `(obs_seq, admitted_fact_seq)`.
    pub fn add_evidence(
        &self,
        actor: &str,
        title: &str,
        fact: &str,
        artifact_id: Option<&str>,
        verified: bool,
        confidence: f64,
        intent_id: Option<&str>,
        route_hash: Option<&str>,
        evidence_provenance: Option<&Value>,
    ) -> (i64, Option<i64>) {
        self.add_evidence_checked(actor,title,fact,artifact_id,verified,confidence,intent_id,route_hash,evidence_provenance).unwrap_or((-1,None))
    }

    pub fn add_dead_end(
        &self,
        actor: &str,
        reason: &str,
        tested_scope: &str,
        observed_result: &str,
        intent_id: Option<&str>,
        route_hash: Option<&str>,
        coverage_key: Option<&str>,
    ) -> i64 {
        let canon_reason = canonical_fact_whitespace(reason);
        let canon_scope = canonical_fact_whitespace(tested_scope);
        if self.dead_ends().iter().any(|row| {
            canonical_fact_whitespace(row["tested_scope"].as_str().unwrap_or("")) == canon_scope
                && canonical_fact_whitespace(row["reason"].as_str().unwrap_or("")) == canon_reason
        }) {
            return -1;
        }
        let dedupe = format!("dead_end:{}", uuid::Uuid::new_v4().simple());

        let payload = json!({
            "reason": reason.trim(),
            "tested_scope": tested_scope.trim(),
            "observed_result": observed_result.trim(),
            "intent_id": intent_id.unwrap_or("").trim(),
            "route_hash": normalize_route_hash(route_hash.unwrap_or("")),
            "coverage_key": coverage_key.unwrap_or("").trim(),
        });
        self.append_event(EV_DEAD_END, actor, &payload, None, true, 1.0, Some(&dedupe))
    }

    pub fn flag_found(&self, actor: &str, flag: &str) -> i64 {
        let trimmed = flag.trim();
        let dedupe = format!("flag:{trimmed}");
        let intent=self.conn.query_row("SELECT intent_id FROM intents WHERE worker=?1 AND status='claimed' ORDER BY created_seq DESC LIMIT 1",[actor],|row|row.get::<_,String>(0)).optional().ok().flatten();
        let payload = json!({
            "intent_id":intent,
            "flag": trimmed,
            "source": actor,
            "verified_by": actor,
        });
        self.append_event(
            EV_FLAG_FOUND,
            actor,
            &payload,
            None,
            true,
            1.0,
            Some(&dedupe),
        )
    }

    pub fn invalidate_flag(&self, actor: &str, flag: &str, reason: &str) -> i64 {
        let trimmed = flag.trim();
        let payload = json!({
            "flag": trimmed,
            "reason": reason,
        });
        self.append_event(EV_FLAG_INVALIDATED, actor, &payload, None, true, 1.0, None)
    }

    pub fn record_platform_verdict(
        &mut self, submission_id: &str, flag: &str, status: &str,
        completed: bool, correct_flag_count: Option<u64>, total_flag_count: Option<u64>,
        detail: &str,
    ) -> Result<Value, String> {
        if submission_id.is_empty()
            || !matches!(status, "accepted" | "duplicate" | "rejected" | "unknown" | "rate_limited") {
            return Err("invalid platform verdict".to_owned());
        }
        let payload = json!({"submission_id":submission_id,"flag":flag,"status":status,
            "completed":completed,"correct_flag_count":correct_flag_count,
            "total_flag_count":total_flag_count,"detail":detail});
        let key = format!("platform-verdict::{submission_id}");
        let tx = self.conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(|error| error.to_string())?;
        let existing: Option<String> = tx.query_row(
            "SELECT payload FROM events WHERE kind=?1 AND dedupe_key=?2",
            params![EV_PLATFORM_VERDICT,key], |row| row.get(0),
        ).optional().map_err(|error| error.to_string())?;
        if let Some(raw) = existing {
            let prior: Value = serde_json::from_str(&raw).map_err(|error| error.to_string())?;
            if prior != payload { return Err("conflicting platform verdict for submission".to_owned()); }
            tx.commit().map_err(|error| error.to_string())?;
            return Ok(prior);
        }
        let now = now_secs();
        tx.execute(
            "INSERT INTO events (ts,kind,actor,payload,verified,confidence,dedupe_key)
             VALUES (?1,?2,'competition',?3,?4,1.0,?5)",
            params![now,EV_PLATFORM_VERDICT,payload.to_string(),
                i64::from(matches!(status,"accepted" | "duplicate" | "rejected")),key],
        ).map_err(|error| error.to_string())?;
        if status == "rejected" {
            tx.execute(
                "INSERT OR IGNORE INTO events (ts,kind,actor,payload,verified,confidence,dedupe_key)
                 VALUES (?1,?2,'competition',?3,1,1.0,?4)",
                params![now,EV_FLAG_INVALIDATED,json!({"flag":flag}).to_string(),
                    format!("flaginvalid::{flag}")],
            ).map_err(|error| error.to_string())?;
            tx.execute(
                "INSERT OR IGNORE INTO events (ts,kind,actor,payload,verified,confidence,dedupe_key)
                 VALUES (?1,?2,'competition',?3,1,1.0,?4)",
                params![now,EV_DEAD_END,json!({
                    "reason":format!("Platform rejected candidate {flag} (decoy or invalid flag)"),
                    "tested_scope":format!("submit-flag candidate {flag}"),
                    "observed_result":format!("platform verdict=rejected ({})",
                        if detail.is_empty() {"invalid flag"} else {detail}),
                    "coverage_key":format!("rejected-flag:{submission_id}")}).to_string(),
                    format!("deadend::platform::{submission_id}")],
            ).map_err(|error| error.to_string())?;
        }
        tx.commit().map_err(|error| error.to_string())?;
        Ok(payload)
    }

    pub fn platform_verdicts(&self) -> Vec<Value> {
        let Ok(mut stmt) = self.conn.prepare(
            "SELECT payload FROM events WHERE kind=?1 ORDER BY seq",
        ) else { return Vec::new(); };
        stmt.query_map([EV_PLATFORM_VERDICT], |row| row.get::<_, String>(0))
            .ok().into_iter().flatten().filter_map(Result::ok)
            .filter_map(|raw| serde_json::from_str(&raw).ok()).collect()
    }

    /// Undo one false-positive flag and reopen only intents concluded as solved.
    /// The flag and dead-end events use the original graph dedupe identities.
    pub fn reopen_after_false_positive(
        &mut self, actor: &str, flag: &str, reason: &str,
    ) -> Result<Vec<String>, String> {
        let tx = self.conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(|error| error.to_string())?;
        let timestamp = now_secs();
        let dead_end_key = format!("deadend::fp::{flag}");
        let invalidation_key = format!("flaginvalid::{flag}");
        let dead_end_payload = json!({"reason":reason});
        let invalidation_payload = json!({"flag":flag});
        for (kind,payload,key) in [
            (EV_DEAD_END,&dead_end_payload,&dead_end_key),
            (EV_FLAG_INVALIDATED,&invalidation_payload,&invalidation_key),
        ] {
            tx.execute("INSERT OR IGNORE INTO events
                (ts,kind,actor,payload,artifact_id,verified,confidence,dedupe_key)
                VALUES (?1,?2,?3,?4,NULL,1,1.0,?5)",
                params![timestamp,kind,actor,payload.to_string(),key])
                .map_err(|error| error.to_string())?;
        }
        let linked = {
            let mut stmt = tx.prepare("SELECT payload FROM events WHERE kind=?1")
                .map_err(|error| error.to_string())?;
            stmt.query_map([EV_FLAG_FOUND], |row| row.get::<_,String>(0))
                .map_err(|error| error.to_string())?
                .filter_map(Result::ok)
                .filter_map(|raw| serde_json::from_str::<Value>(&raw).ok())
                .filter(|payload| payload["flag"] == flag)
                .filter_map(|payload| payload["intent_id"].as_str().map(str::to_owned))
                .collect::<HashSet<_>>()
        };
        let candidates = {
            let mut stmt = tx.prepare("SELECT intent_id FROM intents
                WHERE status='done' AND result='solved' ORDER BY created_seq,intent_id")
                .map_err(|error| error.to_string())?;
            stmt.query_map([], |row| row.get::<_,String>(0))
                .map_err(|error| error.to_string())?
                .collect::<Result<Vec<_>,_>>().map_err(|error| error.to_string())?
        };
        let mut reopened = Vec::new();
        for intent_id in candidates {
            if !linked.is_empty() && !linked.contains(&intent_id) { continue; }
            let changed = tx.execute("UPDATE intents SET status='open',dispatch_state='active',
                worker=NULL,claimed_seq=NULL,claimed_ts=NULL,lease_expires_at=NULL,
                concluded_seq=NULL,result=NULL,reopen_because=?1
                WHERE intent_id=?2 AND status='done' AND result='solved'",
                params![reason,intent_id]).map_err(|error| error.to_string())?;
            if changed > 0 { reopened.push(intent_id); }
        }
        tx.commit().map_err(|error| error.to_string())?;
        Ok(reopened)
    }

    pub fn propose_intent(
        &self,
        actor: &str,
        intent_id: &str,
        goal: &str,
        payload: &Value,
        from_fact_seqs: Option<&[i64]>,
    ) -> i64 {
        let iid = intent_id.trim();
        if iid.is_empty() || goal.trim().is_empty() {
            return -1;
        }
        if self
            .conn
            .query_row(
                "SELECT 1 FROM intents WHERE intent_id = ?1",
                params![iid],
                |_| Ok(()),
            )
            .optional()
            .ok()
            .flatten()
            .is_some()
        {
            return -1;
        }

        let worker_class = payload["worker_class"].as_str().unwrap_or("code").trim();
        let priority = payload["priority"]
            .as_str()
            .unwrap_or("normal")
            .trim()
            .to_ascii_lowercase();
        let requested_priority = payload["requested_priority"]
            .as_str()
            .unwrap_or(&priority)
            .trim()
            .to_ascii_lowercase();
        let priority_reason = payload["priority_reason"].as_str().unwrap_or("").trim();
        let route_hash = normalize_route_hash(payload["route_hash"].as_str().unwrap_or(""));
        let branch_id = payload["branch_id"].as_str().unwrap_or("").trim();
        let expected_observable = payload["expected_observable"].as_str().unwrap_or("").trim();
        let stop_condition = payload["stop_condition"].as_str().unwrap_or("").trim();
        let coverage_key = payload["coverage_key"].as_str().unwrap_or("").trim();
        let lane_key = normalize_lane_key(payload["lane_key"].as_str().unwrap_or(""));
        let risk_class = payload["risk_class"].as_str().unwrap_or("").trim();
        let resource_key = payload["resource_key"].as_str().unwrap_or("").trim();
        let dup_of = payload["dup_of"].as_str().unwrap_or("").trim();
        let reopen_because = payload["reopen_because"].as_str().unwrap_or("").trim();
        let novelty_key = payload["novelty_key"].as_str().unwrap_or("").trim();
        let value_claim_json = payload
            .get("value_claim")
            .map(Value::to_string)
            .unwrap_or_else(|| "{}".to_owned());
        let requires_caps_json = payload
            .get("requires_capabilities")
            .map(Value::to_string)
            .unwrap_or_else(|| "[]".to_owned());
        let required_pocs_json = payload
            .get("required_pocs")
            .map(Value::to_string)
            .unwrap_or_else(|| "[]".to_owned());

        let mut ev_payload = payload.clone();
        if let Some(obj) = ev_payload.as_object_mut() {
            obj.insert("intent_id".to_owned(), json!(iid));
            obj.insert("goal".to_owned(), json!(goal.trim()));
            obj.insert("worker_class".to_owned(), json!(worker_class));
            obj.insert("priority".to_owned(), json!(priority));
            if let Some(srcs) = from_fact_seqs {
                obj.insert("from_facts".to_owned(), json!(srcs));
            }
        }

        let seq = self.append_event(
            EV_INTENT_PROPOSED,
            actor,
            &ev_payload,
            None,
            true,
            1.0,
            Some(&format!("intent:{iid}")),
        );
        if seq <= 0 {
            return -1;
        }

        let _ = self.conn.execute(
            "INSERT INTO intents (
                intent_id, goal, worker_class, status, dispatch_state,
                route_hash, branch_id, expected_observable, stop_condition, coverage_key,
                lane_key, risk_class, resource_key, priority, requested_priority,
                priority_reason, novelty_key, value_claim_json, requires_capabilities_json,
                required_pocs_json, dup_of, reopen_because, created_seq
             ) VALUES (
                ?1, ?2, ?3, 'open', 'active',
                ?4, ?5, ?6, ?7, ?8,
                ?9, ?10, ?11, ?12, ?13,
                ?14, ?15, ?16, ?17,
                ?18, ?19, ?20, ?21
             )",
            params![
                iid,
                goal.trim(),
                worker_class,
                route_hash,
                branch_id,
                expected_observable,
                stop_condition,
                coverage_key,
                lane_key,
                risk_class,
                resource_key,
                priority,
                requested_priority,
                priority_reason,
                novelty_key,
                value_claim_json,
                requires_caps_json,
                required_pocs_json,
                dup_of,
                reopen_because,
                seq,
            ],
        );

        if let Some(srcs) = from_fact_seqs {
            for &fseq in srcs {
                if fseq > 0 {
                    let _ = self.conn.execute(
                        "INSERT OR IGNORE INTO intent_sources (intent_id, fact_seq, role)
                         VALUES (?1, ?2, 'prerequisite')",
                        params![iid, fseq],
                    );
                }
            }
        }

        if let Some(deps) = payload["depends_on"].as_array() {
            for dep in deps.iter().filter_map(Value::as_str) {
                let d = dep.trim();
                if !d.is_empty() {
                    let _ = self.conn.execute(
                        "INSERT OR IGNORE INTO intent_dependencies (intent_id, depends_on_intent_id)
                         VALUES (?1, ?2)",
                        params![iid, d],
                    );
                }
            }
        }

        seq
    }

    pub fn claim_intent(&self, solver_id: &str, intent_id: &str, lease_s: f64) -> bool {
        self.claim_intent_checked(solver_id,intent_id,lease_s).unwrap_or(false)
    }

    pub fn release_intent_claim(&self, solver_id: &str, intent_id: &str, reason: &str) -> bool {
        let ev_payload = json!({
            "intent_id": intent_id,
            "worker": solver_id,
            "reason": reason,
        });
        let seq = self.append_event(
            EV_INTENT_RELEASED,
            solver_id,
            &ev_payload,
            None,
            true,
            1.0,
            None,
        );
        let changed = self
            .conn
            .execute(
                "UPDATE intents
                 SET status = 'open', worker = NULL, claimed_seq = NULL, claimed_ts = NULL, lease_expires_at = NULL
                 WHERE intent_id = ?1 AND status = 'claimed' AND (worker = ?2 OR ?2 = 'coordinator')",
                params![intent_id, solver_id],
            )
            .unwrap_or(0);
        let _ = seq;
        changed > 0
    }

    pub fn conclude_intent(
        &self,
        solver_id: &str,
        intent_id: &str,
        result: &str,
        reason: &str,
    ) -> i64 {
        self.conclude_intent_checked(solver_id,intent_id,result,reason).unwrap_or(-1)
    }

    /// Idempotent reconciliation pass for historical direct Steps left in `claimed`/`active`
    /// state whose owning worker/generation is no longer active.
    /// - If matching terminal evidence exists (`intent_concluded`, `worker_result_committed`,
    ///   `flag_found`, or produced facts/dead-ends on a finished run), concludes the Step
    ///   with its recorded terminal result.
    /// - If no terminal result exists, reopens the Step (`status = 'open'`) and records an
    ///   audit event.
    pub fn reconcile_historical_direct_steps(
        &self,
        active_workers: &HashSet<String>,
        current_generation: Option<u64>,
        run_outcome: Option<&Value>,
    ) -> Result<Vec<Value>, String> {
        self.with_immediate_tx(|| {
            let claimed_rows: Vec<(String, String, i64)> = {
                let mut stmt = self
                    .conn
                    .prepare(
                        "SELECT intent_id, COALESCE(worker, ''), COALESCE(claimed_seq, created_seq)
                         FROM intents
                         WHERE status = 'claimed' AND dispatch_state = 'active'
                         ORDER BY created_seq ASC",
                    )
                    .map_err(|error| error.to_string())?;
                stmt.query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                })
                .map_err(|error| error.to_string())?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| error.to_string())?
            };

            if claimed_rows.is_empty() {
                return Ok(Vec::new());
            }

            let events = self.events();
            let run_solved = run_outcome.is_some_and(|r| r["solved"] == true);
            let run_finished = run_outcome.is_some_and(|r| {
                matches!(r["status"].as_str(), Some("finished" | "solved"))
                    || r["running"].as_bool() == Some(false)
            });

            let mut reconciled = Vec::new();
            for (intent_id, worker, claimed_seq) in claimed_rows {
                if !worker.is_empty() && active_workers.contains(&worker) {
                    continue;
                }
                let worker_gen = super::worker::worker_execution_generation(&worker);
                if let Some(cur_gen) = current_generation
                    && worker_gen >= cur_gen
                {
                    let has_concluded_ev = events.iter().any(|ev| {
                        matches!(
                            ev["kind"].as_str(),
                            Some(EV_INTENT_CONCLUDED | EV_WORKER_RESULT_COMMITTED)
                        ) && ev["payload"]["intent_id"] == intent_id
                    });
                    if !has_concluded_ev {
                        continue;
                    }
                }

                let concluded_ev = events.iter().rev().find(|ev| {
                    ev["kind"] == EV_INTENT_CONCLUDED && ev["payload"]["intent_id"] == intent_id
                });
                let committed_ev = events.iter().rev().find(|ev| {
                    ev["kind"] == EV_WORKER_RESULT_COMMITTED
                        && ev["payload"]["intent_id"] == intent_id
                        && ev["payload"]["checkpoint"] != true
                });
                let flag_ev = events.iter().rev().find(|ev| {
                    ev["kind"] == EV_FLAG_FOUND
                        && (ev["payload"]["intent_id"] == intent_id
                            || (!worker.is_empty() && ev["actor"] == worker))
                });
                let produced_fact = events.iter().any(|ev| {
                    ev["kind"] == EV_FACT_ADDED
                        && ev["verified"] == true
                        && (ev["payload"]["intent_id"] == intent_id
                            || (!worker.is_empty() && ev["actor"] == worker))
                }) || self
                    .conn
                    .query_row(
                        "SELECT EXISTS(SELECT 1 FROM intent_products WHERE intent_id = ?1)",
                        params![intent_id],
                        |r| r.get::<_, bool>(0),
                    )
                    .unwrap_or(false);
                let produced_dead_end = events.iter().any(|ev| {
                    ev["kind"] == EV_DEAD_END && ev["payload"]["intent_id"] == intent_id
                });

                let terminal = if let Some(ev) = concluded_ev {
                    Some((
                        ev["payload"]["result"]
                            .as_str()
                            .unwrap_or("done")
                            .to_owned(),
                        ev["payload"]["reason"]
                            .as_str()
                            .unwrap_or("historical_backfill")
                            .to_owned(),
                    ))
                } else if flag_ev.is_some() || (run_solved && intent_id.starts_with("step_")) {
                    Some(("solved".to_owned(), "historical_flag_evidence".to_owned()))
                } else if let Some(ev) = committed_ev {
                    Some((
                        ev["payload"]["result"]
                            .as_str()
                            .unwrap_or("done")
                            .to_owned(),
                        ev["payload"]["summary"]
                            .as_str()
                            .filter(|s| !s.is_empty())
                            .unwrap_or("historical_step_committed")
                            .to_owned(),
                    ))
                } else if (produced_fact || produced_dead_end) && (run_finished || current_generation.is_none()) {
                    Some(("done".to_owned(), "historical_evidence_backfill".to_owned()))
                } else {
                    None
                };

                let actor = if worker.is_empty() {
                    "coordinator"
                } else {
                    worker.as_str()
                };

                if let Some((result_str, reason_str)) = terminal {
                    let seq = self.conclude_intent_checked(actor, &intent_id, &result_str, &reason_str)?;
                    reconciled.push(json!({
                        "intent_id": intent_id,
                        "worker": worker,
                        "action": "concluded",
                        "result": result_str,
                        "reason": reason_str,
                        "seq": seq,
                    }));
                } else {
                    let dedupe = format!("reconcile-reopen::{intent_id}::{claimed_seq}");
                    let seq = self.append_event(
                        EV_INTENT_REOPENED,
                        "coordinator",
                        &json!({
                            "intent_id": intent_id,
                            "previous_worker": worker,
                            "action": "reopened",
                            "reason": "historical_direct_step_reconciled",
                        }),
                        None,
                        true,
                        1.0,
                        Some(&dedupe),
                    );
                    self.conn
                        .execute(
                            "UPDATE intents
                             SET status = 'open', dispatch_state = 'active', worker = NULL,
                                 claimed_seq = NULL, claimed_ts = NULL, lease_expires_at = NULL,
                                 reopen_because = 'historical_direct_step_reconciled'
                             WHERE intent_id = ?1 AND status = 'claimed'",
                            params![intent_id],
                        )
                        .map_err(|error| error.to_string())?;
                    reconciled.push(json!({
                        "intent_id": intent_id,
                        "worker": worker,
                        "action": "reopened",
                        "reason": "historical_direct_step_reconciled",
                        "seq": seq,
                    }));
                }
            }
            Ok(reconciled)
        })
    }

    pub fn supersede_open_intent_ids(
        &self,
        actor: &str,
        intent_ids: &[String],
        reason: &str,
    ) -> Vec<String> {
        let mut superseded = Vec::new();
        for iid in intent_ids {
            let trimmed = iid.trim();
            if trimmed.is_empty() {
                continue;
            }
            let seq = self.append_event(
                EV_INTENT_SUPERSEDED,
                actor,
                &json!({"intent_id": trimmed, "reason": reason}),
                None,
                true,
                1.0,
                None,
            );
            let changed = self
                .conn
                .execute(
                    "UPDATE intents
                     SET status = 'superseded', dispatch_state = 'closed', concluded_seq = ?1, result = 'superseded'
                     WHERE intent_id = ?2 AND status IN ('open', 'claimed') AND dispatch_state = 'active'",
                    params![seq, trimmed],
                )
                .unwrap_or(0);
            if changed > 0 {
                superseded.push(trimmed.to_owned());
            }
        }
        superseded
    }

    /// Apply one Pentest Decide pass's exact Step drops as one graph transaction.
    /// The event precedes the row updates so every concluded row points to that
    /// single audit event, matching the source graph's batch operation.
    pub fn pentest_decide_drop_steps(
        &mut self,
        actor: &str,
        intent_ids: &[String],
        reason: &str,
    ) -> Result<Vec<String>, String> {
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(|error| error.to_string())?;
        let mut seen = HashSet::new();
        let mut accepted = Vec::new();
        for raw_id in intent_ids {
            let intent_id = raw_id.trim();
            if intent_id.is_empty() || !seen.insert(intent_id.to_owned()) {
                continue;
            }
            let eligible = tx
                .query_row(
                    "SELECT 1 FROM intents WHERE intent_id=?1
                     AND status IN ('open','claimed') AND dispatch_state='active'",
                    [intent_id],
                    |_| Ok(()),
                )
                .optional()
                .map_err(|error| error.to_string())?
                .is_some();
            if eligible {
                accepted.push(intent_id.to_owned());
            }
        }
        if accepted.is_empty() {
            return Ok(accepted);
        }
        let payload = json!({
            "intent_id": accepted.join(","),
            "intent_ids": accepted,
            "result": "superseded",
            "reason": if reason.is_empty() { "new evidence superseded queued work" } else { reason },
        });
        tx.execute(
            "INSERT INTO events (ts,kind,actor,payload,verified,confidence)
             VALUES (?1,?2,?3,?4,0,1.0)",
            params![now_secs(), EV_INTENT_CONCLUDED, actor, payload.to_string()],
        )
        .map_err(|error| error.to_string())?;
        let seq = tx.last_insert_rowid();
        for intent_id in &accepted {
            let changed = tx
                .execute(
                    "UPDATE intents SET status='done',dispatch_state='closed',
                     concluded_seq=?1,result='superseded'
                     WHERE intent_id=?2 AND status IN ('open','claimed')
                     AND dispatch_state='active'",
                    params![seq, intent_id],
                )
                .map_err(|error| error.to_string())?;
            if changed != 1 {
                return Err(format!("Pentest Decide drop lost active Step {intent_id}"));
            }
        }
        tx.commit().map_err(|error| error.to_string())?;
        Ok(accepted)
    }

    /// Apply one Pentest Decide pass's queued Step priorities atomically.
    /// Claimed Steps keep their current dispatch priority.
    pub fn pentest_decide_reprioritize_steps(
        &mut self,
        actor: &str,
        priorities: &HashMap<String, String>,
    ) -> Result<Vec<String>, String> {
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(|error| error.to_string())?;
        let mut requested: Vec<(&str, &str, i64)> = priorities
            .iter()
            .filter_map(|(id, priority)| {
                let id = id.trim();
                let priority = priority.trim();
                let value = match priority {
                    "high" => 50,
                    "normal" => 0,
                    "low" => -10,
                    _ => return None,
                };
                (!id.is_empty()).then_some((id, priority, value))
            })
            .collect();
        requested.sort_unstable_by(|left, right| left.0.cmp(right.0));
        let mut changed = Vec::new();
        let mut effective = serde_json::Map::new();
        let mut previous_id = "";
        for (intent_id, priority, value) in requested {
            if intent_id == previous_id {
                continue;
            }
            previous_id = intent_id;
            let current = tx
                .query_row(
                    "SELECT priority,requested_priority,priority_reason FROM intents
                     WHERE intent_id=?1 AND status='open' AND dispatch_state='active'",
                    [intent_id],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                        ))
                    },
                )
                .optional()
                .map_err(|error| error.to_string())?;
            let Some((current_priority, requested_priority, priority_reason)) = current else {
                continue;
            };
            if current_priority == priority
                && requested_priority == priority
                && priority_reason == "decide_reprioritized"
            {
                continue;
            }
            let updated = tx
                .execute(
                    "UPDATE intents SET priority=?1,requested_priority=?1,
                     priority_reason='decide_reprioritized'
                     WHERE intent_id=?2 AND status='open' AND dispatch_state='active'",
                    params![priority, intent_id],
                )
                .map_err(|error| error.to_string())?;
            if updated != 1 {
                return Err(format!("Pentest Decide priority lost queued Step {intent_id}"));
            }
            changed.push(intent_id.to_owned());
            effective.insert(intent_id.to_owned(), json!(value));
        }
        if changed.is_empty() {
            return Ok(changed);
        }
        tx.execute(
            "INSERT INTO events (ts,kind,actor,payload,verified,confidence)
             VALUES (?1,?2,?3,?4,0,1.0)",
            params![
                now_secs(),
                EV_INTENT_STATE_CHANGED,
                actor,
                json!({"intent_ids": changed, "priorities": effective,
                    "reason": "decide_reprioritized"}).to_string(),
            ],
        )
        .map_err(|error| error.to_string())?;
        tx.commit().map_err(|error| error.to_string())?;
        Ok(changed)
    }

    /// Project the current intermediate Goals from their durable events.
    /// A satisfied Goal becomes open again when any cited Fact leaves the
    /// active evidence set; its original satisfaction remains in the log.
    pub fn sub_goals(&self) -> Vec<Value> {
        sub_goal_rows_from(&self.conn).unwrap_or_default()
    }

    /// Retire a Fact and only the queued Steps that cite it. A claimed Step
    /// keeps its owner so its Worker can finish or hand off independently.
    pub fn pentest_decide_supersede_fact(
        &self,
        actor: &str,
        fact_seq: i64,
        reason: &str,
        by_fact_seq: Option<i64>,
    ) -> Result<i64, String> {
        self.with_immediate_tx(|| {
        let tx = &self.conn;
        let exists = tx.query_row(
            "SELECT 1 FROM events WHERE seq=?1 AND kind=?2",
            params![fact_seq, EV_FACT_ADDED], |_| Ok(()),
        ).optional().map_err(|error| error.to_string())?.is_some();
        if !exists {
            return Err(format!("Unknown Fact event: {fact_seq}"));
        }
        let reason = reason.trim();
        let dedupe_key = format!("fact-superseded::{fact_seq}::{reason}");
        if tx.query_row(
            "SELECT 1 FROM events WHERE dedupe_key=?1", [&dedupe_key], |_| Ok(()),
        ).optional().map_err(|error| error.to_string())?.is_some() {
            return Ok(-1);
        }
        let active = active_fact_seqs_from(tx)?;
        if let Some(replacement) = by_fact_seq {
            if replacement == fact_seq {
                return Err("A Fact cannot supersede itself".to_owned());
            }
            if !active.contains(&replacement) {
                return Err("Replacement Fact must be active".to_owned());
            }
        }
        let mut payload = json!({"fact_seq":fact_seq,"status":"superseded",
            "reason":reason,"superseded_by":actor});
        if let Some(replacement) = by_fact_seq {
            payload["by_fact_seq"] = json!(replacement);
        }
        let now = now_secs();
        tx.execute(
            "INSERT INTO events (ts,kind,actor,payload,verified,confidence,dedupe_key)
             VALUES (?1,?2,?3,?4,0,1.0,?5)",
            params![now, EV_FACT_SUPERSEDED, actor, payload.to_string(), dedupe_key],
        ).map_err(|error| error.to_string())?;
        let seq = tx.last_insert_rowid();
        tx.execute(
            "INSERT INTO fact_reviews
             (fact_seq,verdict,reviewer,rationale,reviewed_seq,updated_ts)
             VALUES (?1,'superseded',?2,?3,?4,?5)
             ON CONFLICT(fact_seq) DO UPDATE SET verdict='superseded',
             reviewer=excluded.reviewer,rationale=excluded.rationale,
             reviewed_seq=excluded.reviewed_seq,updated_ts=excluded.updated_ts",
            params![fact_seq, actor, reason, seq, now],
        ).map_err(|error| error.to_string())?;
        tx.execute(
            "INSERT INTO fact_states
             (fact_seq,status,superseded_by_seq,rationale,updated_by,updated_seq,updated_ts)
             VALUES (?1,'superseded',?2,?3,?4,?5,?6)
             ON CONFLICT(fact_seq) DO UPDATE SET status='superseded',
             superseded_by_seq=excluded.superseded_by_seq,rationale=excluded.rationale,
             updated_by=excluded.updated_by,updated_seq=excluded.updated_seq,
             updated_ts=excluded.updated_ts",
            params![fact_seq, by_fact_seq, reason, actor, seq, now],
        ).map_err(|error| error.to_string())?;

        let dependents = {
            let mut stmt = tx.prepare(
                "SELECT DISTINCT i.intent_id FROM intents i JOIN intent_sources s
                 ON s.intent_id=i.intent_id WHERE s.fact_seq=?1
                 AND i.status='open' AND i.dispatch_state='active'
                 ORDER BY i.created_seq,i.intent_id",
            ).map_err(|error| error.to_string())?;
            stmt.query_map([fact_seq], |row| row.get::<_, String>(0))
                .map_err(|error| error.to_string())?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| error.to_string())?
        };
        let detail = format!("Prerequisite fact_{fact_seq} was superseded");
        for intent_id in dependents {
            tx.execute(
                "INSERT INTO events (ts,kind,actor,payload,verified,confidence)
                 VALUES (?1,?2,?3,?4,0,1.0)",
                params![now, EV_INTENT_STATE_CHANGED, actor,
                    json!({"intent_id":intent_id,"status":"done",
                        "dispatch_state":"retired","reason":detail,
                        "fact_seq":fact_seq}).to_string()],
            ).map_err(|error| error.to_string())?;
            let retirement_seq = tx.last_insert_rowid();
            let changed = tx.execute(
                "UPDATE intents SET status='done',dispatch_state='retired',
                 concluded_seq=?1,result='retired'
                 WHERE intent_id=?2 AND status='open' AND dispatch_state='active'",
                params![retirement_seq, intent_id],
            ).map_err(|error| error.to_string())?;
            if changed != 1 {
                return Err(format!("Fact dependent Step changed while retiring {intent_id}"));
            }
        }
        Ok(seq)
        })
    }

    pub fn pentest_decide_add_sub_goal(
        &mut self,
        actor: &str,
        criterion: &str,
        goal_id: &str,
    ) -> Result<String, String> {
        let criterion = criterion.trim();
        if criterion.is_empty() {
            return Err("Sub-goal criterion must not be empty".to_owned());
        }
        let tx = self.conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(|error| error.to_string())?;
        let reserved = {
            let mut stmt = tx.prepare("SELECT payload FROM events WHERE kind=?1 ORDER BY seq")
                .map_err(|error| error.to_string())?;
            let mut reserved = HashMap::new();
            for row in stmt.query_map([EV_SUB_GOAL_ADDED], |row| row.get::<_, String>(0))
                .map_err(|error| error.to_string())? {
                let payload: Value = serde_json::from_str(&row.map_err(|error| error.to_string())?)
                    .map_err(|error| error.to_string())?;
                if let Some(id) = payload["goal_id"].as_str() {
                    reserved.insert(id.to_owned(), payload["criterion"].as_str().unwrap_or("").to_owned());
                }
            }
            reserved
        };
        let goal_id = if goal_id.trim().is_empty() {
            let mut number = reserved.len() + 1;
            loop {
                let candidate = format!("goal_{number:03}");
                if !reserved.contains_key(&candidate) { break candidate; }
                number += 1;
            }
        } else {
            goal_id.trim().to_owned()
        };
        let suffix = goal_id.strip_prefix("goal_").unwrap_or("");
        if suffix.len() < 3 || !suffix.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err("Sub-goal ID must be goal_NNN".to_owned());
        }
        if let Some(prior) = reserved.get(&goal_id) {
            if prior == criterion {
                return Ok(goal_id);
            }
            return Err(format!("Sub-goal ID already exists: {goal_id}"));
        }
        tx.execute(
            "INSERT INTO events (ts,kind,actor,payload,verified,confidence,dedupe_key)
             VALUES (?1,?2,?3,?4,0,1.0,?5)",
            params![now_secs(), EV_SUB_GOAL_ADDED, actor,
                json!({"goal_id":goal_id,"criterion":criterion}).to_string(),
                format!("sub-goal::{goal_id}")],
        ).map_err(|error| error.to_string())?;
        tx.commit().map_err(|error| error.to_string())?;
        Ok(goal_id)
    }

    pub fn pentest_decide_satisfy_sub_goal(
        &mut self,
        actor: &str,
        goal_id: &str,
        reason: &str,
        evidence_fact_seqs: &[i64],
    ) -> Result<i64, String> {
        let reason = reason.trim();
        let mut evidence = Vec::new();
        for seq in evidence_fact_seqs {
            if !evidence.contains(seq) {
                evidence.push(*seq);
            }
        }
        if evidence.is_empty() || reason.is_empty() {
            return Err("Sub-goal satisfaction needs active evidence and a reason".to_owned());
        }
        let tx = self.conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(|error| error.to_string())?;
        let goal = sub_goal_rows_from(&tx)?.into_iter()
            .find(|goal| goal["goal_id"] == goal_id)
            .ok_or_else(|| format!("Unknown sub-goal: {goal_id}"))?;
        let active = active_fact_seqs_from(&tx)?;
        if evidence.iter().any(|seq| !active.contains(seq)) {
            return Err("Sub-goal evidence references an inactive Fact".to_owned());
        }
        if goal["state"] == "satisfied" {
            return Ok(0);
        }
        tx.execute(
            "INSERT INTO events (ts,kind,actor,payload,verified,confidence)
             VALUES (?1,?2,?3,?4,0,1.0)",
            params![now_secs(), EV_SUB_GOAL_SATISFIED, actor,
                json!({"goal_id":goal_id,"reason":reason,
                    "evidence_fact_seqs":evidence}).to_string()],
        ).map_err(|error| error.to_string())?;
        let seq = tx.last_insert_rowid();
        tx.commit().map_err(|error| error.to_string())?;
        Ok(seq)
    }

    pub fn reprioritize_open_intents(
        &self,
        actor: &str,
        priorities: &HashMap<String, String>,
    ) -> Vec<String> {
        let mut updated = Vec::new();
        for (iid, prio) in priorities {
            let p = prio.trim().to_ascii_lowercase();
            if !matches!(p.as_str(), "high" | "normal" | "low") {
                continue;
            }
            let changed = self
                .conn
                .execute(
                    "UPDATE intents SET priority = ?1, priority_reason = 'reprioritized'
                     WHERE intent_id = ?2 AND status = 'open' AND dispatch_state = 'active'",
                    params![p, iid.trim()],
                )
                .unwrap_or(0);
            if changed > 0 {
                let _ = self.append_event(
                    EV_INTENT_REPRIORITIZED,
                    actor,
                    &json!({"intent_id": iid.trim(), "priority": p}),
                    None,
                    true,
                    1.0,
                    None,
                );
                updated.push(iid.trim().to_owned());
            }
        }
        updated
    }

    pub fn lock_lane(
        &self,
        lane_key: &str,
        risk_class: &str,
        owner: &str,
        intent_id: &str,
        lease_s: f64,
    ) -> bool {
        let lane = normalize_lane_key(lane_key);
        if lane.is_empty() {
            return true;
        }
        self.with_immediate_tx(|| {
            let now = now_secs();
            self.conn.execute(
                "DELETE FROM lane_locks WHERE lane_key = ?1 AND lease_expires_at < ?2
                 AND lease_suspended_remaining_s IS NULL",
                params![lane, now],
            ).map_err(|error| error.to_string())?;
            let current = self.conn.query_row(
                "SELECT owner, lease_suspended_remaining_s IS NOT NULL FROM lane_locks WHERE lane_key = ?1",
                params![lane],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, bool>(1)?)),
            ).optional().map_err(|error| error.to_string())?;
            if let Some((holder, suspended)) = current
                && (suspended || holder != owner)
            {
                return Ok(false);
            }
            let seq = self.append_event(
                EV_LANE_LOCKED,
                owner,
                &json!({"lane_key": lane, "risk_class": risk_class, "intent_id": intent_id}),
                None,
                true,
                1.0,
                None,
            );
            let exp = now + lease_s.max(30.0);
            if seq <= 0 {
                return Err("Could not record lease acquisition".into());
            }
            self.conn.execute(
                "INSERT OR REPLACE INTO lane_locks
                 (lane_key, risk_class, owner, intent_id, claimed_seq, claimed_ts, lease_expires_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![lane, risk_class, owner, intent_id, seq.max(1), now, exp],
            ).map_err(|error| error.to_string())?;
            Ok(true)
        }).unwrap_or(false)
    }

    pub fn release_lane(&self, lane_key: &str, owner: &str) {
        let lane = normalize_lane_key(lane_key);
        if lane.is_empty() {
            return;
        }
        let changed = self
            .conn
            .execute(
                "DELETE FROM lane_locks WHERE lane_key = ?1 AND (owner = ?2 OR ?2 = 'coordinator')",
                params![lane, owner],
            )
            .unwrap_or(0);
        if changed > 0 {
            let _ = self.append_event(
                EV_LANE_RELEASED,
                owner,
                &json!({"lane_key": lane}),
                None,
                true,
                1.0,
                None,
            );
        }
    }

    pub fn request_resource_lock(
        &self,
        resource_key: &str,
        worker_id: &str,
        intent_id: &str,
        lease_s: f64,
    ) -> bool {
        let rk = resource_key.trim().to_ascii_lowercase();
        if rk.is_empty() {
            return true;
        }
        self.with_immediate_tx(|| {
            let now = now_secs();
            let current = self.conn.query_row(
                "SELECT holder_worker_id, lease_expires_at, released_seq, lease_suspended_remaining_s IS NOT NULL
                 FROM resource_locks WHERE resource_key = ?1",
                params![rk],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, f64>(1)?,
                           row.get::<_, Option<i64>>(2)?, row.get::<_, bool>(3)?)),
            ).optional().map_err(|error| error.to_string())?;
            if let Some((holder, deadline, released, suspended)) = current
                && released.is_none()
                && (suspended || (deadline >= now && holder != worker_id))
            {
                return Ok(false);
            }
            let seq = self.append_event(
                EV_RESOURCE_LOCKED,
                worker_id,
                &json!({"resource_key": rk, "intent_id": intent_id}),
                None,
                true,
                1.0,
                None,
            );
            let exp = now + lease_s.max(30.0);
            if seq <= 0 {
                return Err("Could not record lease acquisition".into());
            }
            self.conn.execute(
                "INSERT OR REPLACE INTO resource_locks
                 (resource_key, holder_worker_id, intent_id, acquired_seq, acquired_at, lease_expires_at, released_seq, released_at, release_reason)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL, NULL, '')",
                params![rk, worker_id, intent_id, seq.max(1), now, exp],
            ).map_err(|error| error.to_string())?;
            Ok(true)
        }).unwrap_or(false)
    }

    pub fn release_resource_lock(&self, resource_key: &str, worker_id: &str, reason: &str) {
        let rk = resource_key.trim().to_ascii_lowercase();
        if rk.is_empty() {
            return;
        }
        let now = now_secs();
        let seq = self.append_event(
            EV_RESOURCE_RELEASED,
            worker_id,
            &json!({"resource_key": rk, "reason": reason}),
            None,
            true,
            1.0,
            None,
        );
        let _ = self.conn.execute(
            "UPDATE resource_locks
             SET released_seq = ?1, released_at = ?2, release_reason = ?3
             WHERE resource_key = ?4 AND (holder_worker_id = ?5 OR ?5 = 'coordinator')",
            params![seq.max(1), now, reason, rk, worker_id],
        );
    }

    pub fn try_claim_activity(
        &self,
        activity_key: &str,
        owner: &str,
        intent_id: &str,
        lease_s: f64,
    ) -> bool {
        let ak = activity_key.trim();
        if ak.is_empty() {
            return true;
        }
        self.with_immediate_tx(|| {
            let now = now_secs();
            self.conn.execute(
                "DELETE FROM activity_locks WHERE activity_key = ?1 AND lease_expires_at < ?2
                 AND lease_suspended_remaining_s IS NULL",
                params![ak, now],
            ).map_err(|error| error.to_string())?;
            let current = self.conn.query_row(
                "SELECT owner, lease_suspended_remaining_s IS NOT NULL FROM activity_locks WHERE activity_key = ?1",
                params![ak],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, bool>(1)?)),
            ).optional().map_err(|error| error.to_string())?;
            if let Some((holder, suspended)) = current
                && (suspended || holder != owner)
            {
                return Ok(false);
            }
            let seq = self.append_event(
                EV_ACTIVITY_CLAIMED,
                owner,
                &json!({"activity_key": ak, "intent_id": intent_id}),
                None,
                true,
                1.0,
                None,
            );
            if seq <= 0 {
                return Err("Could not record lease acquisition".into());
            }
            self.conn.execute(
                "INSERT OR REPLACE INTO activity_locks
                 (activity_key, owner, intent_id, claimed_seq, claimed_ts, lease_expires_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    ak,
                    owner,
                    intent_id,
                    seq.max(1),
                    now,
                    now + lease_s.max(30.0)
                ],
            ).map_err(|error| error.to_string())?;
            Ok(true)
        }).unwrap_or(false)
    }

    pub fn add_operator_directive(
        &self,
        directive_id: &str,
        actor: &str,
        target: &str,
        action: &str,
        text: &str,
        standing: bool,
        preempt_policy: &str,
    ) -> i64 {
        let now = now_secs();
        let payload = json!({
            "directive_id": directive_id,
            "target": target,
            "action": action,
            "text": text,
            "standing": standing,
            "preempt_policy": preempt_policy,
        });
        let seq = self.append_event(
            EV_OPERATOR_DIRECTIVE,
            actor,
            &payload,
            None,
            true,
            1.0,
            None,
        );
        let _ = self.conn.execute(
            "INSERT OR REPLACE INTO operator_directives
             (directive_id, challenge_id, actor, target, action, text, standing, status, preempt_policy, priority, received_seq, bound_worker, created_seq, updated_seq, updated_ts)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'applied', ?8, 50, ?9, ?10, ?9, ?9, ?11)",
            params![
                directive_id,
                "",
                actor,
                target,
                action,
                text,
                if standing { 1 } else { 0 },
                preempt_policy,
                seq.max(1),
                if target != "coordinator" { target } else { "" },
                now,
            ],
        );
        seq
    }

    pub fn archive_directive(&self,state:&AppState,run_id:&str,id:&str,reason:&str,actor:&str)->Result<(),String>{
        if reason.trim().is_empty(){return Err("archive_directive requires a reason".into());}
        let changed=self.with_immediate_tx(||{
            let status:Option<String>=self.conn.query_row("SELECT status FROM operator_directives WHERE directive_id=?1",[id],|r|r.get(0)).optional().map_err(|e|e.to_string())?;
            if status.as_deref()==Some("consumed"){return Ok(false);}
            if status.as_deref()!=Some("applied"){return Err("archive_directive target is not active".into());}
            let seq=self.append_event("operator_directive_status",actor,&json!({"directive_id":id,"status":"consumed","reason":reason}),None,true,1.0,None);
            if seq<=0{return Err("Could not persist directive archive".into());}
            self.conn.execute("UPDATE operator_directives SET status='consumed',status_reason=?1,consumed_revision=?2,updated_seq=?2,updated_ts=?3 WHERE directive_id=?4",params![reason,seq,now_secs(),id]).map_err(|e|e.to_string())?;Ok(true)
        })?;
        let conn=super::open_control_db(state,run_id)?;
        conn.execute("UPDATE context_resources SET expired_at=?1 WHERE context_id=?2 AND expired_at IS NULL",params![now_secs(),id]).map_err(|e|e.to_string())?;
        state.runs.handle(state,run_id).directives.lock().unwrap().retain(|d|d["directive_id"]!=id);
        if changed{super::directive_changed(state,run_id,id,"consumed",reason)?;}
        Ok(())
    }

    pub fn expire_standing_directives(
        &self, exact_text: &str, command_id: &str,
    ) -> Result<Vec<String>, String> {
        self.conn.execute_batch("BEGIN IMMEDIATE").map_err(|error| error.to_string())?;
        let result = (|| {
            let mut stmt = self.conn.prepare(
                "SELECT directive_id,text FROM operator_directives
                 WHERE standing=1 AND status='applied' ORDER BY created_seq",
            ).map_err(|error| error.to_string())?;
            let matched: Vec<String> = stmt.query_map([], |row| {
                Ok((row.get::<_, String>(0)?,row.get::<_, String>(1)?))
            }).map_err(|error| error.to_string())?
              .filter_map(Result::ok)
              .filter(|(_,text)| exact_text.is_empty() || text == exact_text)
              .map(|(id,_)| id).collect();
            drop(stmt);
            for id in &matched {
                let now = now_secs();
                self.conn.execute(
                    "INSERT INTO events (ts,kind,actor,payload,verified,confidence,dedupe_key)
                     VALUES (?1,?2,'operator',?3,1,1.0,?4)",
                    params![now,EV_OPERATOR_DIRECTIVE,
                        json!({"directive_id":id,"status":"expired",
                            "source_command_id":command_id}).to_string(),
                        format!("directive-expire::{command_id}::{id}")],
                ).map_err(|error| error.to_string())?;
                let seq = self.conn.last_insert_rowid();
                self.conn.execute(
                    "UPDATE operator_directives SET status='expired',updated_seq=?1,updated_ts=?2
                     WHERE directive_id=?3 AND status='applied'",
                    params![seq,now,id],
                ).map_err(|error| error.to_string())?;
            }
            Ok::<_,String>(matched)
        })();
        match result {
            Ok(matched) => {
                self.conn.execute_batch("COMMIT").map_err(|error| error.to_string())?;
                Ok(matched)
            }
            Err(error) => { let _ = self.conn.execute_batch("ROLLBACK"); Err(error) }
        }
    }

    pub fn expire_directive(
        &self, directive_id: &str, command_id: &str,
    ) -> Result<bool, String> {
        self.conn.execute_batch("BEGIN IMMEDIATE").map_err(|error| error.to_string())?;
        let result = (|| {
            let active: bool = self.conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM operator_directives
                 WHERE directive_id=?1 AND status='applied')",
                [directive_id], |row| row.get(0),
            ).map_err(|error| error.to_string())?;
            if active {
                let at = now_secs();
                self.conn.execute(
                    "INSERT INTO events (ts,kind,actor,payload,verified,confidence,dedupe_key)
                     VALUES (?1,?2,'operator',?3,1,1.0,?4)",
                    params![at,EV_OPERATOR_DIRECTIVE,
                        json!({"directive_id":directive_id,"status":"expired",
                            "source_command_id":command_id}).to_string(),
                        format!("directive-expire::{command_id}::{directive_id}")],
                ).map_err(|error| error.to_string())?;
                let seq = self.conn.last_insert_rowid();
                self.conn.execute(
                    "UPDATE operator_directives SET status='expired',updated_seq=?1,updated_ts=?2
                     WHERE directive_id=?3 AND status='applied'",
                    params![seq,at,directive_id],
                ).map_err(|error| error.to_string())?;
            }
            Ok::<_,String>(active)
        })();
        match result {
            Ok(active) => {
                self.conn.execute_batch("COMMIT").map_err(|error| error.to_string())?;
                Ok(active)
            }
            Err(error) => { let _ = self.conn.execute_batch("ROLLBACK"); Err(error) }
        }
    }

    pub fn add_hitl_request(&self, request_id: &str, worker: &str, need: &str) -> i64 {
        let now = now_secs();
        let payload = json!({
            "request_id": request_id,
            "worker": worker,
            "need": need,
        });
        let seq = self.append_event(EV_HITL_REQUEST, worker, &payload, None, true, 1.0, None);
        let _ = self.conn.execute(
            "INSERT OR REPLACE INTO hitl_requests
             (request_id, worker, need, status, created_seq, updated_seq, created_ts, updated_ts)
             VALUES (?1, ?2, ?3, 'open', ?4, ?4, ?5, ?5)",
            params![request_id, worker, need, seq.max(1), now],
        );
        seq
    }

    pub fn release_claims_for_finalize(&self) {
        let now = now_secs();
        let _ = self.conn.execute(
            "UPDATE intents
             SET status = 'open', worker = NULL, claimed_seq = NULL, claimed_ts = NULL, lease_expires_at = NULL
             WHERE status = 'claimed'",
            [],
        );
        let _ = self.conn.execute("DELETE FROM lane_locks", []);
        let _ = self.conn.execute("DELETE FROM activity_locks", []);
        let _ = self.conn.execute(
            "UPDATE resource_locks SET released_at = ?1, release_reason = 'run_finalized' WHERE released_at IS NULL",
            params![now],
        );
    }

    pub fn compact_graph(&self, actor: &str, summary_text: &str) -> i64 {
        self.compact_graph_checked(actor,summary_text).map(|result|result.0).unwrap_or(-1)
    }

    pub fn semantic_graph_watermark(&self) -> i64 {
        self.conn
            .query_row("SELECT COALESCE(MAX(seq), 0) FROM events", [], |r| r.get(0))
            .unwrap_or(0)
    }

    pub fn prior_intent_count(&self) -> usize {
        self.conn
            .query_row("SELECT COUNT(*) FROM intents", [], |r| r.get::<_, i64>(0))
            .unwrap_or(0) as usize
    }

    pub fn open_dispatchable_intents(&self) -> Vec<Value> {
        let mut stmt = match self.conn.prepare(
            "SELECT i.intent_id, i.goal, i.worker_class, i.priority, i.expected_observable,
                    i.stop_condition, i.coverage_key, i.route_hash, i.branch_id,
                    i.lane_key, i.risk_class, i.resource_key, i.created_seq
             FROM intents i
             WHERE i.status = 'open' AND i.dispatch_state = 'active'
               AND NOT EXISTS (
                   SELECT 1 FROM intent_dependencies d
                   LEFT JOIN intents dep ON dep.intent_id = d.depends_on_intent_id
                   WHERE d.intent_id=i.intent_id AND ((dep.intent_id IS NULL AND d.depends_on_intent_id GLOB '*[^0-9]*') OR (dep.intent_id IS NOT NULL AND (dep.status<>'done' OR dep.dispatch_state<>'closed')))
               )
               AND NOT EXISTS(SELECT 1 FROM json_each(i.required_pocs_json) r LEFT JOIN pocs p ON p.poc_id=r.value WHERE p.poc_id IS NULL OR p.status NOT IN('available','directional','wip'))
             ORDER BY
               CASE i.priority WHEN 'high' THEN 0 WHEN 'normal' THEN 1 ELSE 2 END ASC,
               i.created_seq ASC",
        ) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };

        let rows = stmt
            .query_map([], |row| {
                let iid: String = row.get(0)?;
                Ok(json!({
                    "intent_id": iid,
                    "goal": row.get::<_, String>(1)?,
                    "worker_class": row.get::<_, String>(2)?,
                    "priority": row.get::<_, String>(3)?,
                    "expected_observable": row.get::<_, String>(4)?,
                    "stop_condition": row.get::<_, String>(5)?,
                    "coverage_key": row.get::<_, String>(6)?,
                    "route_hash": row.get::<_, String>(7)?,
                    "branch_id": row.get::<_, String>(8)?,
                    "lane_key": row.get::<_, String>(9)?,
                    "risk_class": row.get::<_, String>(10)?,
                    "resource_key": row.get::<_, String>(11)?,
                    "created_seq": row.get::<_, i64>(12)?,
                }))
            })
            .ok();

        let mut list: Vec<Value> = rows.into_iter().flatten().filter_map(Result::ok).collect();

        for item in &mut list {
            if let Some(iid) = item["intent_id"].as_str() {
                let from_facts = self.intent_source_seqs(iid);
                item["from_facts"] = json!(from_facts);
            }
        }
        list
    }

    pub fn intent_source_seqs(&self, intent_id: &str) -> Vec<i64> {
        let Ok(mut stmt) = self.conn.prepare(
            "SELECT fact_seq FROM intent_sources WHERE intent_id = ?1 ORDER BY fact_seq ASC",
        ) else {
            return Vec::new();
        };
        stmt.query_map(params![intent_id], |r| r.get::<_, i64>(0))
            .ok()
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .collect()
    }

    pub fn all_intents(&self) -> Vec<Value> {
        let Ok(mut stmt) = self.conn.prepare(
            "SELECT intent_id, goal, worker_class, status, dispatch_state, worker,
                    priority, expected_observable, stop_condition, coverage_key,
                    route_hash, branch_id, lane_key, created_seq, concluded_seq, result, requires_capabilities_json, required_pocs_json, requested_priority, value_claim_json
             FROM intents ORDER BY created_seq ASC",
        ) else {
            return Vec::new();
        };
        let mut items: Vec<Value> = stmt
            .query_map([], |row| {
                Ok(json!({
                    "intent_id": row.get::<_, String>(0)?,
                    "goal": row.get::<_, String>(1)?,
                    "worker_class": row.get::<_, String>(2)?,
                    "status": row.get::<_, String>(3)?,
                    "dispatch_state": row.get::<_, String>(4)?,
                    "worker": row.get::<_, Option<String>>(5)?,
                    "priority": row.get::<_, String>(6)?,
                    "expected_observable": row.get::<_, String>(7)?,
                    "stop_condition": row.get::<_, String>(8)?,
                    "coverage_key": row.get::<_, String>(9)?,
                    "route_hash": row.get::<_, String>(10)?,
                    "branch_id": row.get::<_, String>(11)?,
                    "lane_key": row.get::<_, String>(12)?,
                    "created_seq": row.get::<_, i64>(13)?,
                    "concluded_seq": row.get::<_, Option<i64>>(14)?,
                    "result": row.get::<_, Option<String>>(15)?,
                    "requires_capabilities": serde_json::from_str::<Value>(&row.get::<_,String>(16)?).map_err(|error|rusqlite::Error::FromSqlConversionFailure(16,rusqlite::types::Type::Text,Box::new(error)))?,
                    "required_pocs": serde_json::from_str::<Value>(&row.get::<_,String>(17)?).map_err(|error|rusqlite::Error::FromSqlConversionFailure(17,rusqlite::types::Type::Text,Box::new(error)))?,
                    "requested_priority":row.get::<_,String>(18)?,
                    "value_claim":serde_json::from_str::<Value>(&row.get::<_,String>(19)?).map_err(|error|rusqlite::Error::FromSqlConversionFailure(19,rusqlite::types::Type::Text,Box::new(error)))?,
                }))
            })
            .ok()
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .collect();
        for item in &mut items {
            if let Some(iid) = item["intent_id"].as_str() {
                item["from_facts"] = json!(self.intent_source_seqs(iid));
            }
        }
        items
    }

    pub fn active_facts(&self) -> Vec<Value> {
        let Ok(mut stmt) = self.conn.prepare(
            "SELECT e.seq, e.ts, e.kind, e.actor, e.payload, e.artifact_id, e.confidence
             FROM events e
             LEFT JOIN fact_states fs ON fs.fact_seq = e.seq
             WHERE e.kind IN (?1, ?2)
               AND e.verified = 1
               AND COALESCE(fs.status, 'active') IN ('active', 'verified', 'revalidated')
             ORDER BY e.seq ASC",
        ) else {
            return Vec::new();
        };
        stmt.query_map(params![EV_FACT_ORIGIN, EV_FACT_ADDED], |row| {
            let seq: i64 = row.get(0)?;
            let ts: f64 = row.get(1)?;
            let kind: String = row.get(2)?;
            let actor: String = row.get(3)?;
            let raw_payload: String = row.get(4)?;
            let artifact_id: Option<String> = row.get(5)?;
            let confidence: f64 = row.get(6)?;
            let payload: Value = serde_json::from_str(&raw_payload).unwrap_or_else(|_| json!({}));
            let fact = payload["fact"]
                .as_str()
                .or_else(|| payload["detail"].as_str())
                .or_else(|| payload["summary"].as_str())
                .unwrap_or("")
                .to_owned();
            let summary = payload["summary"].as_str().unwrap_or(&fact).to_owned();
            Ok(json!({
                "fact_seq": seq,
                "seq": seq,
                "ts": ts,
                "kind": kind,
                "actor": actor,
                "fact": fact,
                "summary": summary,
                "artifact_id": artifact_id,
                "confidence": confidence,
                "origin": kind == EV_FACT_ORIGIN,
            }))
        })
        .ok()
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .collect()
    }

    pub fn fact_state(&self, fact_seq: i64) -> Option<Value> {
        self.conn
            .query_row(
                "SELECT fact_seq, status, canonical_fact_seq, superseded_by_seq, rationale,
                        verification_intent_id, updated_by, updated_seq, updated_ts
                 FROM fact_states WHERE fact_seq = ?1",
                params![fact_seq],
                |row| {
                    Ok(json!({
                        "fact_seq": row.get::<_, i64>(0)?,
                        "status": row.get::<_, String>(1)?,
                        "canonical_fact_seq": row.get::<_, Option<i64>>(2)?,
                        "superseded_by_seq": row.get::<_, Option<i64>>(3)?,
                        "rationale": row.get::<_, String>(4)?,
                        "verification_intent_id": row.get::<_, String>(5)?,
                        "updated_by": row.get::<_, String>(6)?,
                        "updated_seq": row.get::<_, i64>(7)?,
                        "updated_ts": row.get::<_, f64>(8)?,
                    }))
                },
            )
            .optional()
            .ok()
            .flatten()
    }

    pub fn fact_review(&self, fact_seq: i64) -> Option<Value> {
        self.conn
            .query_row(
                "SELECT fact_seq, verdict, reviewer, rationale, reopen_condition,
                        verification_intent_id, reviewed_seq, updated_ts
                 FROM fact_reviews WHERE fact_seq = ?1",
                params![fact_seq],
                |row| {
                    Ok(json!({
                        "fact_seq": row.get::<_, i64>(0)?,
                        "verdict": row.get::<_, String>(1)?,
                        "reviewer": row.get::<_, String>(2)?,
                        "rationale": row.get::<_, String>(3)?,
                        "reopen_condition": row.get::<_, String>(4)?,
                        "verification_intent_id": row.get::<_, String>(5)?,
                        "reviewed_seq": row.get::<_, i64>(6)?,
                        "updated_ts": row.get::<_, f64>(7)?,
                    }))
                },
            )
            .optional()
            .ok()
            .flatten()
    }

    pub fn fact_states_map(&self) -> HashMap<i64, String> {
        let Ok(mut stmt) = self
            .conn
            .prepare("SELECT fact_seq, status FROM fact_states")
        else {
            return HashMap::new();
        };
        stmt.query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)))
            .ok()
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .collect()
    }

    pub fn dead_ends(&self) -> Vec<Value> {
        let Ok(mut stmt) = self
            .conn
            .prepare("SELECT seq, ts, actor, payload FROM events WHERE kind = ?1 ORDER BY seq ASC")
        else {
            return Vec::new();
        };
        stmt.query_map(params![EV_DEAD_END], |row| {
            let seq: i64 = row.get(0)?;
            let ts: f64 = row.get(1)?;
            let actor: String = row.get(2)?;
            let raw: String = row.get(3)?;
            let p: Value = serde_json::from_str(&raw).unwrap_or_else(|_| json!({}));
            Ok(json!({
                "seq": seq,
                "ts": ts,
                "actor": actor,
                "reason": p["reason"].as_str().unwrap_or(""),
                "tested_scope": p["tested_scope"].as_str().unwrap_or(""),
                "observed_result": p["observed_result"].as_str().unwrap_or(""),
                "coverage_key": p["coverage_key"].as_str().unwrap_or(""),
                "intent_id": p["intent_id"].as_str().unwrap_or(""),
            }))
        })
        .ok()
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .collect()
    }

    pub fn pocs(&self) -> Vec<Value> {
        let Ok(mut stmt) = self.conn.prepare(
            "SELECT poc_id, name, path, entry_command, status, created_by, created_seq, note, (SELECT payload FROM events WHERE seq=pocs.updated_seq)
             FROM pocs ORDER BY created_seq ASC",
        ) else {
            return Vec::new();
        };
        stmt.query_map([], |row| {
            let body=row.get::<_,Option<String>>(8)?.and_then(|raw|serde_json::from_str::<Value>(&raw).ok()).unwrap_or(Value::Null);
            Ok(json!({
                "poc_id": row.get::<_, String>(0)?,
                "name": row.get::<_, String>(1)?,
                "path": row.get::<_, String>(2)?,
                "entry_command": row.get::<_, String>(3)?,
                "status": row.get::<_, String>(4)?,
                "actor": row.get::<_, String>(5)?,
                "created_seq": row.get::<_, i64>(6)?,
                "note": row.get::<_, String>(7)?,
                "image_ref":body["image_ref"],"intent_id":body["intent_id"],"target_epoch":body["target_epoch"],
            }))
        })
        .ok()
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .collect()
    }

    pub fn directives(&self) -> Vec<Value> {
        let Ok(mut stmt) = self.conn.prepare(
            "SELECT directive_id, actor, target, action, text, standing, status, created_seq, updated_ts
             FROM operator_directives ORDER BY created_seq ASC",
        ) else {
            return Vec::new();
        };
        stmt.query_map([], |row| {
            Ok(json!({
                "directive_id": row.get::<_, String>(0)?,
                "actor": row.get::<_, String>(1)?,
                "target": row.get::<_, String>(2)?,
                "action": row.get::<_, String>(3)?,
                "text": row.get::<_, String>(4)?,
                "standing": row.get::<_, i64>(5)? != 0,
                "status": row.get::<_, String>(6)?,
                "created_seq": row.get::<_, i64>(7)?,
                "ts": row.get::<_, f64>(8)?,
            }))
        })
        .ok()
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .collect()
    }

    pub fn events(&self) -> Vec<Value> {
        let Ok(mut stmt) = self.conn.prepare(
            "SELECT seq, ts, kind, actor, payload, artifact_id, verified, confidence
             FROM events ORDER BY seq ASC",
        ) else {
            return Vec::new();
        };
        stmt.query_map([], |row| {
            let raw_payload: String = row.get(4)?;
            let payload: Value = serde_json::from_str(&raw_payload).unwrap_or_else(|_| json!({}));
            Ok(json!({
                "seq": row.get::<_, i64>(0)?,
                "ts": row.get::<_, f64>(1)?,
                "kind": row.get::<_, String>(2)?,
                "actor": row.get::<_, String>(3)?,
                "payload": payload,
                "artifact_id": row.get::<_, Option<String>>(5)?,
                "verified": row.get::<_, i64>(6)? != 0,
                "confidence": row.get::<_, f64>(7)?,
            }))
        })
        .ok()
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .collect()
    }

    /// Behavioral port of `muteki/swarm/graph_views.py::canonical_credentials`.
    /// Extracts verified `(entity, value, seq)` credentials in unlock order.
    pub fn canonical_credentials(&self) -> Vec<Value> {
        static RE_SUCCESS: OnceLock<Regex> = OnceLock::new();
        static RE_FAILURE: OnceLock<Regex> = OnceLock::new();
        static RE_ENTITY: OnceLock<Regex> = OnceLock::new();
        static RE_TARGET: OnceLock<Regex> = OnceLock::new();
        static RE_PAIR: OnceLock<Regex> = OnceLock::new();
        static RE_VALUE_KW: OnceLock<Regex> = OnceLock::new();
        static RE_BARE: OnceLock<Regex> = OnceLock::new();
        static RE_REJECT: OnceLock<Regex> = OnceLock::new();

        let re_success = RE_SUCCESS.get_or_init(|| {
            Regex::new(r"(?i)\b(?:verified|confirm(?:ed)?|authenticat(?:ed|ion\s+succeeded)|logged\s+in|login\s+succeeded|accepted|valid|works|working|unlocked|decrypted|cracked|recovered|found\s+password| credentials?\s+for)\b").unwrap()
        });
        let re_failure = RE_FAILURE.get_or_init(|| {
            Regex::new(r"(?i)\b(?:failed|invalid|rejected|wrong|denied|unauthorized|incorrect|expired|not\s+working|placeholder|example|todo|dummy)\b").unwrap()
        });
        let re_entity = RE_ENTITY.get_or_init(|| {
            Regex::new(r"(?i)\b(?:user(?:name)?|account|login|role|admin|service|ssh|ftp|mysql|postgres|redis|db)\s*[:=]\s*([A-Za-z0-9_.\-@]{2,64})").unwrap()
        });
        let re_target =
            RE_TARGET.get_or_init(|| Regex::new(r"(?i)\bfor\s+([A-Za-z0-9_.\-@/]{2,64})").unwrap());
        let re_pair = RE_PAIR.get_or_init(|| {
            Regex::new(r"\b([A-Za-z0-9_.\-]{2,32})\s*/\s*([^\s,;'\x22`]{3,64})\b").unwrap()
        });
        let re_value_kw = RE_VALUE_KW.get_or_init(|| {
            Regex::new(r"(?i)\b(?:password|passwd|pass|secret|token|api[_\-]?key|private[_\-]?key|pin)\s*(?:is|[:=])\s*[`'\x22]?([^\s,;'\x22`]{3,128})[`'\x22]?").unwrap()
        });
        let re_bare = RE_BARE.get_or_init(|| {
            Regex::new(r"(?i)\b([A-Za-z0-9_.\-]{2,32}):([^\s:,'\x22`]{4,64})\b").unwrap()
        });
        let re_reject = RE_REJECT.get_or_init(|| {
            Regex::new(r"(?i)^(?:http|https|ftp|ssh|ws|wss|true|false|null|none|unknown|redacted|\*+|\.+)$").unwrap()
        });

        let mut by_entity: HashMap<String, (String, i64)> = HashMap::new();
        let mut order: Vec<String> = Vec::new();

        for row in self.active_facts() {
            if row["origin"] == true {
                continue;
            }
            let seq = row["fact_seq"].as_i64().unwrap_or(0);
            let text = row["fact"].as_str().unwrap_or("").trim();
            if text.is_empty() || re_failure.is_match(text) {
                continue;
            }

            let mut entity = String::new();
            let mut value = String::new();

            if let Some(cap) = re_value_kw.captures(text) {
                value = cap.get(1).map(|m| m.as_str()).unwrap_or("").to_owned();
                if let Some(ec) = re_entity.captures(text) {
                    entity = ec.get(1).map(|m| m.as_str()).unwrap_or("").to_owned();
                } else if let Some(tc) = re_target.captures(text) {
                    entity = tc.get(1).map(|m| m.as_str()).unwrap_or("").to_owned();
                }
            } else if re_success.is_match(text) {
                if let Some(cap) = re_pair.captures(text) {
                    entity = cap.get(1).map(|m| m.as_str()).unwrap_or("").to_owned();
                    value = cap.get(2).map(|m| m.as_str()).unwrap_or("").to_owned();
                } else if let Some(cap) = re_bare.captures(text) {
                    let lhs = cap.get(1).map(|m| m.as_str()).unwrap_or("");
                    let rhs = cap.get(2).map(|m| m.as_str()).unwrap_or("");
                    if !re_reject.is_match(lhs) && rhs.parse::<u16>().is_err() {
                        entity = lhs.to_owned();
                        value = rhs.to_owned();
                    }
                }
            }

            let value_trimmed = value.trim_matches(|c: char| c == '.' || c == ',' || c == ';');
            if value_trimmed.len() < 3 || re_reject.is_match(value_trimmed) {
                continue;
            }
            let entity_norm = if entity.trim().is_empty() {
                "default".to_owned()
            } else {
                entity.trim().to_owned()
            };

            if !by_entity.contains_key(&entity_norm) {
                order.push(entity_norm.clone());
            }
            by_entity.insert(entity_norm, (value_trimmed.to_owned(), seq));
        }

        order
            .into_iter()
            .filter_map(|ent| {
                let (val, seq) = by_entity.remove(&ent)?;
                Some(json!({
                    "entity": ent,
                    "value": val,
                    "seq": seq,
                }))
            })
            .collect()
    }

    /// Renders the `Fact-Goal-Step` YAML view consumed by the Reason planner (`to_ctf_graph_yaml`).
    pub fn to_ctf_graph_yaml(&self, challenge: &Value) -> String {
        let name = challenge["name"].as_str().unwrap_or("challenge");
        let mode = challenge["mode"].as_str().unwrap_or("ctf");
        let cat = challenge["category"].as_str().unwrap_or("misc");
        let ff = challenge["flag_format"].as_str().unwrap_or("flag{...}");
        let target = challenge["target"].as_str().unwrap_or("");
        let desc = challenge["description"].as_str().unwrap_or("");

        let mut out = String::new();
        out.push_str("Goal:\n");
        out.push_str(&format!("  name: {:?}\n", name));
        out.push_str(&format!("  mode: {:?}\n", mode));
        out.push_str(&format!("  category: {:?}\n", cat));
        out.push_str(&format!("  flag_format: {:?}\n", ff));
        if !target.is_empty() {
            out.push_str(&format!("  target: {:?}\n", target));
        }
        if !desc.is_empty() {
            out.push_str(&format!("  description: {:?}\n", desc));
        }

        // The Reason/Goal Decide graph view includes the durable intermediate
        // Goals beside the final objective. Their current state is projected
        // from events, so a Goal whose evidence was superseded reopens here.
        out.push_str("\ngoals:\n  - id: goal_final\n");
        if mode == "pentest" {
            out.push_str("    kind: pentest_objective\n");
            out.push_str(&format!("    criterion: {:?}\n", desc));
        } else {
            out.push_str("    kind: final\n");
            out.push_str("    criterion: 解决这道 CTF 题目，并提交全部 FLAG，多 FLAG 题目要找到全部 FLAG 并有效提交\n");
        }
        for goal in self.sub_goals() {
            out.push_str(&format!(
                "  - id: {:?}\n    kind: sub\n",
                goal["goal_id"].as_str().unwrap_or("")
            ));
            if mode == "pentest" {
                out.push_str(&format!(
                    "    state: {:?}\n",
                    goal["state"].as_str().unwrap_or("open")
                ));
            }
            out.push_str(&format!(
                "    criterion: {:?}\n",
                goal["criterion"].as_str().unwrap_or("")
            ));
        }

        // The planner never receives ContextResources, so operator guidance mirrored
        // as a CTX- directive must stay visible here or hints are silently ignored.
        let directives: Vec<_> = self.directives().into_iter()
            .filter(|directive| directive["status"] == "applied")
            .collect();
        if !directives.is_empty() {
            out.push_str("\nOperatorDirectives:\n");
            for d in directives {
                out.push_str(&format!(
                    "  - id: {:?}\n    text: {:?}\n",
                    d["directive_id"].as_str().unwrap_or(""),
                    d["text"].as_str().unwrap_or("")
                ));
            }
        }

        let facts = self.active_facts();
        out.push_str("\nFacts:\n");
        if facts.is_empty() {
            out.push_str("  []\n");
        } else {
            for f in facts {
                out.push_str(&format!(
                    "  - seq: {}\n    actor: {:?}\n    fact: {:?}\n",
                    f["fact_seq"].as_i64().unwrap_or(0),
                    f["actor"].as_str().unwrap_or(""),
                    f["fact"].as_str().unwrap_or("")
                ));
            }
        }

        let intents = self.all_intents();
        out.push_str("\nSteps:\n");
        if intents.is_empty() {
            out.push_str("  []\n");
        } else {
            for it in intents {
                out.push_str(&format!(
                    "  - id: {:?}\n    status: {:?}\n    priority: {:?}\n    goal: {:?}\n    coverage_key: {:?}\n    expected_observable: {:?}\n    stop_condition: {:?}\n",
                    it["intent_id"].as_str().unwrap_or(""),
                    it["status"].as_str().unwrap_or(""),
                    it["priority"].as_str().unwrap_or("normal"),
                    it["goal"].as_str().unwrap_or(""),
                    it["coverage_key"].as_str().unwrap_or(""),
                    it["expected_observable"].as_str().unwrap_or(""),
                    it["stop_condition"].as_str().unwrap_or(""),
                ));
            }
        }

        let dead_ends = self.dead_ends();
        out.push_str("\nDeadEnds:\n");
        if dead_ends.is_empty() {
            out.push_str("  []\n");
        } else {
            for d in dead_ends {
                out.push_str(&format!(
                    "  - seq: {}\n    tested_scope: {:?}\n    reason: {:?}\n    observed_result: {:?}\n",
                    d["seq"].as_i64().unwrap_or(0),
                    d["tested_scope"].as_str().unwrap_or(""),
                    d["reason"].as_str().unwrap_or(""),
                    d["observed_result"].as_str().unwrap_or(""),
                ));
            }
        }

        let pocs = self.pocs();
        if !pocs.is_empty() {
            out.push_str("\nSavedPoCs:\n");
            for p in pocs {
                out.push_str(&format!(
                    "  - id: {:?}\n    name: {:?}\n    path: {:?}\n    entry_command: {:?}\n    status: {:?}\n",
                    p["poc_id"].as_str().unwrap_or(""),
                    p["name"].as_str().unwrap_or(""),
                    p["path"].as_str().unwrap_or(""),
                    p["entry_command"].as_str().unwrap_or(""),
                    p["status"].as_str().unwrap_or(""),
                ));
            }
        }

        let verdicts = self.platform_verdicts();
        if !verdicts.is_empty() {
            out.push_str("\nPlatformVerdicts:\n");
            for v in verdicts {
                out.push_str(&format!(
                    "  - submission_id: {:?}\n    flag: {:?}\n    status: {:?}\n    completed: {}\n    detail: {:?}\n",
                    v["submission_id"].as_str().unwrap_or(""),
                    v["flag"].as_str().unwrap_or(""),
                    v["status"].as_str().unwrap_or(""),
                    v["completed"].as_bool().unwrap_or(false),
                    v["detail"].as_str().unwrap_or(""),
                ));
            }
        }

        out
    }

    pub fn to_board_markdown(&self, run_id: &str, run_doc: &Value) -> String {
        let mut md = format!(
            "# Blackboard Snapshot ({run_id})\n\
             - Challenge: {}\n\
             - Category: {}\n\
             - Flag Format: `{}`\n\
             - Solved: {}\n",
            run_doc["name"].as_str().unwrap_or(run_id),
            run_doc["category"].as_str().unwrap_or("misc"),
            run_doc["flag_format"].as_str().unwrap_or("flag{...}"),
            run_doc["solved"].as_bool().unwrap_or(false),
        );
        if let Some(flags) = run_doc["flags"].as_array()
            && !flags.is_empty()
        {
            let list: Vec<&str> = flags.iter().filter_map(Value::as_str).collect();
            md.push_str(&format!("- Flags: {}\n", list.join(", ")));
        }

        let verdicts = self.platform_verdicts();
        if !verdicts.is_empty() {
            md.push_str("\n## Platform Verdicts\n");
            for v in &verdicts {
                let sub_id = v["submission_id"].as_str().unwrap_or("");
                let flag = v["flag"].as_str().unwrap_or("");
                let status = v["status"].as_str().unwrap_or("unknown");
                let detail = v["detail"].as_str().unwrap_or("");
                let progress = match (
                    v["correct_flag_count"].as_u64(),
                    v["total_flag_count"].as_u64(),
                ) {
                    (Some(cur), Some(tot)) => format!(" ({cur}/{tot})"),
                    _ => String::new(),
                };
                if detail.is_empty() {
                    md.push_str(&format!(
                        "- [{status}]{progress} `{flag}` (submission `{sub_id}`)\n"
                    ));
                } else {
                    md.push_str(&format!(
                        "- [{status}]{progress} `{flag}` (submission `{sub_id}`): {detail}\n"
                    ));
                }
            }
        }

        let facts = self.active_facts();
        md.push_str("\n## Verified Facts\n");
        if facts.is_empty() {
            md.push_str("- (none yet)\n");
        } else {
            for f in facts {
                md.push_str(&format!(
                    "- [fact#{}] {}\n",
                    f["fact_seq"].as_i64().unwrap_or(0),
                    f["fact"].as_str().unwrap_or("")
                ));
            }
        }

        let intents = self.all_intents();
        if !intents.is_empty() {
            md.push_str("\n## Steps (Intents)\n");
            for it in intents {
                md.push_str(&format!(
                    "- [{}] `{}` ({}): {}\n",
                    it["status"].as_str().unwrap_or("open"),
                    it["intent_id"].as_str().unwrap_or(""),
                    it["priority"].as_str().unwrap_or("normal"),
                    it["goal"].as_str().unwrap_or("")
                ));
            }
        }

        let pocs = self.pocs();
        if !pocs.is_empty() {
            md.push_str("\n## Saved PoCs\n");
            for p in pocs {
                md.push_str(&format!(
                    "- `{}` ({}): `{}` — {}\n",
                    p["poc_id"].as_str().unwrap_or(""),
                    p["status"].as_str().unwrap_or(""),
                    p["path"].as_str().unwrap_or(""),
                    p["entry_command"].as_str().unwrap_or("")
                ));
            }
        }

        let dead_ends = self.dead_ends();
        md.push_str("\n## Dead Ends\n");
        if dead_ends.is_empty() {
            md.push_str("- (none)\n");
        } else {
            for d in dead_ends {
                let scope = d["tested_scope"].as_str().unwrap_or("");
                let reason = d["reason"].as_str().unwrap_or("");
                if scope.is_empty() {
                    md.push_str(&format!("- {reason}\n"));
                } else {
                    md.push_str(&format!("- [{scope}] {reason}\n"));
                }
            }
        }

        let directives: Vec<_> = self.directives().into_iter()
            .filter(|directive| directive["status"] == "applied"
                && !directive["directive_id"].as_str().unwrap_or("").starts_with("CTX-"))
            .collect();
        if !directives.is_empty() {
            md.push_str("\n## Operator Directives\n");
            for dir in directives {
                md.push_str(&format!("- {}\n", dir["text"].as_str().unwrap_or("")));
            }
        }

        let creds = self.canonical_credentials();
        if !creds.is_empty() {
            md.push_str("\n## Discovered Credentials\n");
            for c in creds {
                md.push_str(&format!(
                    "- `{}`: `{}` (fact#{})\n",
                    c["entity"].as_str().unwrap_or("default"),
                    c["value"].as_str().unwrap_or(""),
                    c["seq"].as_i64().unwrap_or(0)
                ));
            }
        }

        md
    }

    pub fn export_sqlite_bytes(&self) -> Result<Vec<u8>, String> {
        export_sqlite_snapshot(&self.conn, false)
    }

    pub fn coverage_keys(&self) -> HashSet<String> {
        let Ok(mut stmt) = self
            .conn
            .prepare("SELECT coverage_key FROM intents WHERE coverage_key != ''")
        else {
            return HashSet::new();
        };
        stmt.query_map([], |r| r.get::<_, String>(0))
            .ok()
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .map(|s| canonical_fact_whitespace(&s).to_ascii_lowercase())
            .collect()
    }
}

pub(crate) fn preflight_graph_export(conn: &Connection, source_graph: bool) -> Result<(), String> {
    let query = if source_graph {
        "SELECT seq, challenge_id, ts, actor, kind, payload, artifact_id, verified, confidence FROM events LIMIT 0"
    } else {
        "SELECT seq, ts, actor, kind, payload, artifact_id, verified, confidence FROM events LIMIT 0"
    };
    conn.prepare(query).map_err(|error| format!("run graph unreadable: {error}"))?;
    Ok(())
}

pub(crate) fn export_sqlite_snapshot(conn: &Connection, source_graph: bool) -> Result<Vec<u8>, String> {
    preflight_graph_export(conn, source_graph)?;
    let tmp_dir = tempfile::tempdir().map_err(|error| format!("run graph unreadable: {error}"))?;
    let dst_path = tmp_dir.path().join("export_graph.sqlite");
    conn.backup(rusqlite::DatabaseName::Main, &dst_path, None)
        .map_err(|error| format!("run graph unreadable: {error}"))?;
    fs::read(dst_path).map_err(|error| format!("run graph unreadable: {error}"))
}

pub(crate) fn export_filename(run_id: &str, suffix: &str) -> String {
    let safe: String = run_id.chars().map(|ch| {
        if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' { ch } else { '_' }
    }).take(80).collect();
    format!("muteki-graph-{}.{}", if safe.is_empty() { "run" } else { &safe }, suffix)
}

fn export_event_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Value> {
    let raw: Option<String> = row.get(4)?;
    let parsed = raw.as_deref().map(serde_json::from_str::<Value>);
    let (payload, error) = match parsed {
        Some(Ok(payload)) => (payload, None),
        Some(Err(error)) => (Value::Null, Some(format!("JSONDecodeError: {error}"))),
        None => (Value::Null, Some("TypeError: payload is null".to_owned())),
    };
    let mut event = json!({
        "record":"event",
        "seq":row.get::<_, i64>(0)?,
        "ts":row.get::<_, f64>(1)?,
        "actor":row.get::<_, String>(2)?,
        "kind":row.get::<_, String>(3)?,
        "payload":payload,
        "artifact_id":row.get::<_, Option<String>>(5)?,
        "verified":row.get::<_, i64>(6)? != 0,
        "confidence":row.get::<_, f64>(7)?,
    });
    if let Some(error) = error {
        event["payload_raw"] = json!(raw);
        event["payload_error"] = json!(error);
    }
    Ok(event)
}

pub(crate) fn export_graph_text(conn: &Connection, run_id: &str, run: &Value,
    format: &str, source_graph: bool) -> Result<String, String> {
    if !matches!(format, "jsonl" | "timeline") {
        return Err("format must be jsonl, timeline, or sqlite".to_owned());
    }
    let tx = conn.unchecked_transaction().map_err(|error| format!("run graph unreadable: {error}"))?;
    preflight_graph_export(&tx, source_graph)?;
    let (count, watermark): (i64, i64) = if source_graph {
        tx.query_row("SELECT COUNT(*), COALESCE(MAX(seq),0) FROM events WHERE challenge_id=?1",
            params![run_id], |row| Ok((row.get(0)?, row.get(1)?)))
    } else {
        tx.query_row("SELECT COUNT(*), COALESCE(MAX(seq),0) FROM events", [],
            |row| Ok((row.get(0)?, row.get(1)?)))
    }.map_err(|error| format!("run graph unreadable: {error}"))?;
    let events = {
        let query = if source_graph {
            "SELECT seq, ts, actor, kind, payload, artifact_id, verified, confidence FROM events WHERE challenge_id=?1 ORDER BY seq"
        } else {
            "SELECT seq, ts, actor, kind, payload, artifact_id, verified, confidence FROM events ORDER BY seq"
        };
        let mut statement = tx.prepare(query).map_err(|error| format!("run graph unreadable: {error}"))?;
        if source_graph {
            statement.query_map(params![run_id], export_event_row)
                .map_err(|error| format!("run graph unreadable: {error}"))?
                .collect::<rusqlite::Result<Vec<_>>>()
        } else {
            statement.query_map([], export_event_row)
                .map_err(|error| format!("run graph unreadable: {error}"))?
                .collect::<rusqlite::Result<Vec<_>>>()
        }.map_err(|error| format!("run graph unreadable: {error}"))?
    };
    tx.commit().map_err(|error| format!("run graph unreadable: {error}"))?;

    if format == "jsonl" {
        let metadata = json!({"record":"graph_export","schema_version":1,
            "run_id":run_id,"name":run["name"],"category":run["category"],
            "event_count":count,"watermark":watermark});
        let mut output = format!("{metadata}\n");
        for event in events { output.push_str(&format!("{event}\n")); }
        return Ok(output);
    }
    let name = run["name"].as_str().filter(|name| !name.is_empty()).unwrap_or(run_id);
    let category = run["category"].as_str().filter(|name| !name.is_empty()).unwrap_or("unknown");
    let mut output = format!("Run: {name} ({run_id})\nCategory: {category}\nEvents: {count} | Watermark: #{watermark}\n\n");
    for event in events {
        output.push_str(&format!("[{}] #{} {} by {}\n",
            event["ts"], event["seq"], event["kind"].as_str().unwrap_or(""),
            event["actor"].as_str().unwrap_or("")));
        if let Some(error) = event["payload_error"].as_str() {
            output.push_str(&format!("invalid JSON payload ({error}):\n{}",
                event["payload_raw"].as_str().unwrap_or("None")));
        } else {
            output.push_str(&serde_json::to_string_pretty(&event["payload"])
                .map_err(|error| error.to_string())?);
        }
        if let Some(artifact) = event["artifact_id"].as_str().filter(|id| !id.is_empty()) {
            output.push_str(&format!("\nartifact: {artifact}"));
        }
        output.push_str(&format!("\nverified: {} | confidence: {}\n\n",
            if event["verified"] == true { "True" } else { "False" }, event["confidence"]));
    }
    Ok(output)
}
