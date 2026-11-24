//! A deleted credential account must not keep serving Workers that already hold a projected copy.
//! Each projection is recorded when it is made so deletion can find and stop exactly those Workers.
use super::*;

const TABLE: &str = "runner_worker_credentials";

fn account(credential: &str) -> &str {
    credential.strip_prefix("account:").or_else(|| credential.strip_prefix("endpoint:")).unwrap_or(credential)
}

pub(crate) fn record(state: &AppState, run: &str, solver: &str, credential: &str) {
    let key = format!("{run}:{solver}");
    let row = json!({"run_id": run, "solver_id": solver, "account_id": account(credential), "recorded_at": now_secs()});
    if let Err(error) = state.store.put(TABLE, &key, &row) { tracing::warn!(%error, "Credential projection could not be recorded"); }
}

/// Stop the live Workers that were given `account_id` and report which ones could not be asked to stop.
pub(crate) fn revoke(state: &AppState, account_id: &str) -> Value {
    let account_id = account(account_id);
    let (mut requested, mut unreachable) = (Vec::new(), Vec::new());
    for row in state.store.list(TABLE).unwrap_or_default() {
        if row["account_id"] != account_id { continue; }
        let (Some(run), Some(solver)) = (row["run_id"].as_str(), row["solver_id"].as_str()) else { continue };
        if state.store.get("runs", run).ok().flatten().is_none_or(|doc| doc["running"] != true) {
            let _ = state.store.delete(TABLE, &format!("{run}:{solver}"));
            continue;
        }
        let outcome = state.runs.request_worker_cancel(run, "worker", solver);
        let stopped = !outcome.requested.is_empty();
        emit_event(state, run, Some(solver), "worker.credential_revoked", json!({"account_id": account_id, "stop_requested": stopped}));
        if stopped { requested.push(format!("{run}:{solver}")); let _ = state.store.delete(TABLE, &format!("{run}:{solver}")); }
        else { unreachable.push(format!("{run}:{solver}")); }
    }
    json!({"stop_requested": requested, "unconfirmed": unreachable})
}
