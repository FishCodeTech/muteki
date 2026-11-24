//! The session ledger: which engine session each thread is running, and what
//! happened to the ones a previous process left behind.
//!
//! Process ownership and recovery are maintained by `runtime_guardian`; this
//! ledger retains provider session identities and resume cursors.

use crate::store::Store;
use serde_json::{Value, json};

pub const COLLECTION: &str = "agent_runtime_sessions";

#[derive(Debug, Clone)]
pub struct Record {
    pub thread_id: String,
    pub engine: String,
    pub adapter_id: String,
    pub instance_id: String,
    pub model: Option<String>,
    pub access_mode: String,
    pub cwd: String,
    pub binary_path: String,
    pub binary_source: String,
    pub epoch: String,
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}

/// Record a session that has just started. One row per thread; a new session
/// replaces the previous row for that thread.
pub fn started(store: &Store, record: &Record) -> rusqlite::Result<()> {
    store.put(
        COLLECTION,
        &record.thread_id,
        &json!({
            "thread_id": record.thread_id,
            "engine": record.engine,
            "adapter_id": record.adapter_id,
            "instance_id": record.instance_id,
            "model": record.model,
            "access_mode": record.access_mode,
            "cwd": record.cwd,
            "binary_path": record.binary_path,
            "binary_source": record.binary_source,
            "epoch": record.epoch,
            "status": "running",
            "provider_session_id": Value::Null,
            "resume": Value::Null,
            "started_at": now(),
            "ended_at": Value::Null,
            "end_reason": Value::Null,
        }),
    )
}

/// Save the resume cursor the engine reported, so a later process can continue
/// this thread instead of starting a cold session.
pub fn session_established(
    store: &Store,
    thread_id: &str,
    provider_session_id: &str,
    resume: &Value,
) -> rusqlite::Result<()> {
    let Some(mut row) = store.get(COLLECTION, thread_id)? else {
        return Ok(());
    };
    row["provider_session_id"] = json!(provider_session_id);
    row["resume"] = resume.clone();
    store.put(COLLECTION, thread_id, &row)
}

/// Record that a session ended, keeping its resume cursor for a later restart.
pub fn ended(store: &Store, thread_id: &str, reason: &str) -> rusqlite::Result<()> {
    let Some(mut row) = store.get(COLLECTION, thread_id)? else {
        return Ok(());
    };
    row["status"] = json!("ended");
    row["ended_at"] = json!(now());
    row["end_reason"] = json!(reason);
    store.put(COLLECTION, thread_id, &row)
}

/// The stored resume cursor for a thread, when the engine reported one.
pub fn resume_cursor(store: &Store, thread_id: &str) -> rusqlite::Result<Option<Value>> {
    Ok(store
        .get(COLLECTION, thread_id)?
        .and_then(|row| row.get("resume").cloned())
        .filter(|resume| !resume.is_null()))
}

/// Settle every row left `running` by an earlier process.
///
/// Those sessions cannot be reattached to, so they are closed with a reason the
/// UI can show rather than left looking live. Returns how many were settled.
pub fn settle_previous_epochs(store: &Store, epoch: &str) -> rusqlite::Result<usize> {
    let mut settled = 0;
    for mut row in store.list(COLLECTION)? {
        let stale = row["status"] == "running" && row["epoch"] != epoch;
        if !stale {
            continue;
        }
        let Some(thread_id) = row["thread_id"].as_str().map(str::to_owned) else {
            continue;
        };
        row["status"] = json!("ended");
        row["ended_at"] = json!(now());
        row["end_reason"] = json!("server_restarted");
        store.put(COLLECTION, &thread_id, &row)?;
        settled += 1;
    }
    Ok(settled)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = Store::open(dir.path().join("muteki.sqlite")).expect("open");
        (dir, store)
    }

    fn record(thread: &str, epoch: &str) -> Record {
        Record {
            thread_id: thread.into(),
            engine: "codex".into(),
            adapter_id: "codex.app_server".into(),
            instance_id: "default".into(),
            model: Some("gpt-5".into()),
            access_mode: "supervised".into(),
            cwd: "/tmp".into(),
            binary_path: "/usr/local/bin/codex".into(),
            binary_source: "path".into(),
            epoch: epoch.into(),
        }
    }

    #[test]
    fn a_resume_cursor_survives_the_session_that_reported_it() {
        let (_dir, store) = store();
        started(&store, &record("t1", "e1")).unwrap();
        assert_eq!(resume_cursor(&store, "t1").unwrap(), None);
        session_established(&store, "t1", "sess-1", &json!({"thread_id": "abc"})).unwrap();
        ended(&store, "t1", "closed").unwrap();
        assert_eq!(
            resume_cursor(&store, "t1").unwrap(),
            Some(json!({"thread_id": "abc"}))
        );
    }

    #[test]
    fn sessions_from_an_earlier_process_are_settled_not_left_running() {
        let (_dir, store) = store();
        started(&store, &record("old", "e1")).unwrap();
        started(&store, &record("new", "e2")).unwrap();
        assert_eq!(settle_previous_epochs(&store, "e2").unwrap(), 1);
        let old = store.get(COLLECTION, "old").unwrap().unwrap();
        assert_eq!(old["status"], "ended");
        assert_eq!(old["end_reason"], "server_restarted");
        let new = store.get(COLLECTION, "new").unwrap().unwrap();
        assert_eq!(new["status"], "running");
        assert_eq!(settle_previous_epochs(&store, "e2").unwrap(), 0);
    }
}
