mod cronus;
mod ipc;
pub(crate) mod settings;
use crate::AppState;
use axum::{
    Router,
    routing::{get, post},
};
pub(in crate::runs) use ipc::WorkerGuardHandle;
use std::{path::PathBuf, sync::Arc};

pub(super) fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/runs/{id}/guard/state", get(state))
        .route("/api/runs/{id}/guard/approvals/{aid}/decide", post(decide))
        .route(
            "/api/settings/execution-guardrail",
            get(settings::get).put(settings::put),
        )
        .route(
            "/api/settings/execution-guardrail/probe",
            post(settings::probe),
        )
}

pub(in crate::runs) mod store;
use crate::{chat, control};
use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use rusqlite::params;
use serde_json::{Value, json};

pub(in crate::runs) fn protected(run: &Value) -> bool {
    run["pentest_contract"]["production_guard"]["enabled"] == true
}

pub(super) fn init_run(state: &AppState, id: &str, generation: u64) -> Result<(), String> {
    let db = super::open_control_db(state, id)?;
    store::ensure_open(&db, generation).map_err(|e| e.to_string())
}

pub(in crate::runs) fn worker_pause_budget(
    state: &AppState,
    id: &str,
    worker: &str,
) -> (f64, bool) {
    let Some(run) = state.store.get("runs", id).ok().flatten() else {
        return (0.0, false);
    };
    if !protected(&run) {
        return (0.0, false);
    }
    let Some(generation) = run["execution_generation"].as_u64() else {
        return (0.0, false);
    };
    super::open_control_db(state, id)
        .ok()
        .and_then(|db| store::pause_seconds(&db, generation, worker).ok())
        .unwrap_or((0.0, false))
}

pub(in crate::runs) fn api_key_secret(state: &AppState) -> Option<String> {
    settings::load(state)
        .ok()
        .map(|cfg| cfg.api_key)
        .filter(|key| !key.is_empty())
}

pub(in crate::runs) fn spawn_worker_guard(
    state: Arc<AppState>,
    run_id: String,
    solver_id: String,
    worker_role: String,
    step_goal: String,
    generation: u64,
    guard_ipc_dir: PathBuf,
    contract: Value,
    operator_secrets: Vec<String>,
) -> ipc::WorkerGuardHandle {
    ipc::spawn_worker_guard(
        state,
        run_id,
        solver_id,
        worker_role,
        step_goal,
        generation,
        guard_ipc_dir,
        contract,
        operator_secrets,
    )
}

fn run(state: &AppState, id: &str) -> Result<Value, chat::Failure> {
    state
        .store
        .get("runs", id)
        .map_err(control::storage)?
        .ok_or_else(|| control::failure(StatusCode::NOT_FOUND, "run.not_found", "Run not found"))
}

async fn state(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> chat::Api {
    let run = run(&state, &id)?;
    if !protected(&run) {
        return Ok(Json(json!({
            "enabled": false,
            "mode": "off",
            "egress_state": "open",
            "pending_approvals": [],
            "recent_receipts": [],
            "emergency_stop_receipt": null,
            "metrics": {
                "total_evaluated": 0,
                "allowed": 0,
                "escalated": 0,
                "denied": 0,
                "cronus_errors": 0,
                "avg_cronus_ms": 0
            }
        })));
    }
    let db = super::open_control_db(&state, &id).map_err(control::storage)?;
    Ok(Json(
        store::snapshot(&db, run["execution_generation"].as_u64().unwrap_or(0))
            .map_err(control::storage)?,
    ))
}

async fn decide(
    State(state): State<Arc<AppState>>,
    Path((id, aid)): Path<(String, String)>,
    Json(body): Json<Value>,
) -> Result<Response, chat::Failure> {
    let run = run(&state, &id)?;
    if !protected(&run) {
        return Err(control::failure(
            StatusCode::CONFLICT,
            "guard.disabled",
            "Run has no production guard",
        ));
    }
    let db = super::open_control_db(&state, &id).map_err(control::storage)?;
    store::init(&db).map_err(control::storage)?;
    let action = body["action"].as_str().unwrap_or("");
    let generation = run["execution_generation"]
        .as_u64()
        .ok_or_else(|| control::storage("Missing execution generation"))?;
    if !matches!(action, "allow_once" | "deny_once" | "emergency_stop")
        || body.get("note").is_some_and(|note| !note.is_string())
    {
        return Err(control::failure(
            StatusCode::UNPROCESSABLE_ENTITY,
            "guard.approval.invalid",
            "Invalid approval decision",
        ));
    }
    let note = body["note"].as_str();
    if action == "emergency_stop" {
        return Ok(super::control_run(
            State(state),
            Path(id),
            Json(json!({
                "action": "emergency_stop",
                "expected_generation": run["control_generation"].as_u64().unwrap_or(0),
                "approval_id": aid,
                "text": note.unwrap_or("")
            })),
        )
        .await);
    }
    let (resolved, opened) = {
        let tx = db.unchecked_transaction().map_err(control::storage)?;
        let changed = tx
            .execute(
                "UPDATE guard_actions SET approval_state=?1,note=?2 WHERE generation=?3 AND approval_id=?4
             AND approval_state='pending' AND receipt_state='not_sent' AND EXISTS(
             SELECT 1 FROM guard_state WHERE generation=?3 AND egress_state='held')",
                params![action, note, generation, aid],
            )
            .map_err(control::storage)?;
        if changed != 1 {
            return Err(control::failure(
                StatusCode::CONFLICT,
                "guard.approval.stale",
                "Approval is not pending for this execution",
            ));
        }
        let opened = tx
            .execute(
                "UPDATE guard_state SET egress_state='open' WHERE generation=?1 AND egress_state='held'
             AND NOT EXISTS(SELECT 1 FROM guard_actions WHERE generation=?1 AND approval_state='pending')",
                [generation],
            )
            .map_err(control::storage)?
            == 1;
        tx.commit().map_err(control::storage)?;
        (
            json!({"approval_id": aid, "action": action, "note": note, "execution_generation": generation}),
            opened,
        )
    };
    super::emit_event(
        &state,
        &id,
        None,
        "guard.approval.resolved",
        resolved.clone(),
    );
    if opened {
        super::emit_event(
            &state,
            &id,
            None,
            "guard.egress.state_changed",
            json!({"egress_state": "open", "execution_generation": generation}),
        );
    }
    state.runs.handle(&state, &id).guard_notify.notify_waiters();
    Ok(Json(json!({"ok": true, "approval": resolved})).into_response())
}

pub(super) fn preflight(
    state: &AppState,
    specs: &[super::worker::WorkerSpec],
    degraded: &[Value],
    run_config: Option<&Value>,
) -> Option<Response> {
    if specs.is_empty()
        || specs.iter().any(|seat| seat.norm_engine != "pi")
        || degraded.iter().any(|seat| {
            seat["engine"]
                .as_str()
                .is_some_and(|engine| super::settings::normalize_engine(engine) != "pi")
        })
    {
        return Some(
            (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(json!({
                    "code": "guard.preflight.only_muteki_pi_allowed",
                    "detail": "Each protected Worker seat must resolve to Pi"
                })),
            )
                .into_response(),
        );
    }
    if !degraded.is_empty() {
        return Some(
            (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(json!({
                    "code": "guard.preflight.seat_unavailable",
                    "detail": "Protected Worker seat unavailable",
                    "profile_failures": degraded
                })),
            )
                .into_response(),
        );
    }
    let global_settings = state
        .store
        .get("settings", "global")
        .ok()
        .flatten()
        .unwrap_or(Value::Null);
    let remote_selected = run_config
        .and_then(crate::runner_remote::controller::selected)
        .is_some()
        || crate::runner_remote::controller::selected(&global_settings).is_some();
    if specs.iter().any(|seat| seat.backend == "container") && remote_selected {
        return Some(
            (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(json!({
                    "code": "guard.preflight.remote_runner_unsupported",
                    "detail": "Protected Pentest mode does not support remote container runners; use local host or local Docker container"
                })),
            )
                .into_response(),
        );
    }
    match settings::load(state) {
        Ok(config) if config.enabled && !config.endpoint.is_empty() => None,
        Ok(_) => Some(
            (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(json!({
                    "code": "guard.preflight.not_configured",
                    "detail": "Execution guard is not configured"
                })),
            )
                .into_response(),
        ),
        Err(error) => Some(error.into_response()),
    }
}

pub(super) fn apply_contract(
    contract: &mut Value,
    requested: Option<&Value>,
) -> Result<(), String> {
    let Some(value) = requested else {
        return Ok(());
    };
    let enabled = value["enabled"]
        .as_bool()
        .ok_or("production_guard.enabled must be a boolean")?;
    let mode = if enabled { "protected" } else { "off" };
    if value
        .get("mode")
        .is_some_and(|field| field.as_str() != Some(mode))
    {
        return Err("production_guard.mode conflicts with enabled".into());
    }
    if enabled {
        contract["version"] = json!(3);
        contract["production_guard"] = json!({"enabled": true, "mode": mode});
    } else if contract["production_guard"]["enabled"] == true {
        return Err("The Run contract requires production guard".into());
    }
    Ok(())
}

pub(super) fn dispatch_challenge(body: &Value, mut challenge: Value) -> Result<Value, String> {
    if let Some(guard) = body.get("production_guard") {
        if !challenge.is_object()
            || challenge
                .get("production_guard")
                .is_some_and(|prior| prior != guard)
        {
            return Err("Conflicting production guard configuration".into());
        }
        challenge["production_guard"] = guard.clone();
    }
    Ok(challenge)
}

pub(super) fn is_revoked(state: &AppState, id: &str, run: &Value) -> Result<bool, String> {
    if !protected(run) {
        return Ok(false);
    }
    let db = super::open_control_db(state, id)?;
    store::revoked(&db, run["execution_generation"].as_u64().unwrap_or(0))
        .map_err(|e| e.to_string())
}

pub(super) fn admit_stop(
    db: &rusqlite::Connection,
    run: &Value,
    body: &Value,
    command: &str,
    pending: &Value,
) -> Result<(Vec<String>, Vec<String>), chat::Failure> {
    let generation = run["execution_generation"]
        .as_u64()
        .ok_or_else(|| control::storage("Missing execution generation"))?;
    let tx = db.unchecked_transaction().map_err(control::storage)?;
    store::init(&tx).map_err(control::storage)?;
    if let Some(aid) = body.get("approval_id") {
        let aid = aid.as_str().ok_or_else(|| {
            control::failure(
                StatusCode::UNPROCESSABLE_ENTITY,
                "guard.approval.invalid",
                "Invalid approval ID",
            )
        })?;
        let pending: bool = tx
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM guard_actions WHERE generation=?1
            AND approval_id=?2 AND approval_state='pending' AND receipt_state='not_sent')",
                params![generation, aid],
                |r| r.get(0),
            )
            .map_err(control::storage)?;
        if !pending {
            return Err(control::failure(
                StatusCode::CONFLICT,
                "guard.approval.stale",
                "Approval is no longer pending",
            ));
        }
    }
    tx.execute(
        "INSERT INTO control_commands(command_id,action,target,status,generation,payload_json,response_json,created_at)
        VALUES(?1,'emergency_stop',?2,'unknown',?3,?4,?5,?6)",
        params![
            command,
            body["scope"].as_str(),
            pending["generation"].as_u64(),
            body.to_string(),
            pending.to_string(),
            super::now_secs()
        ],
    )
    .map_err(control::storage)?;
    let revoked = store::revoke(&tx, generation, command).map_err(control::storage)?;
    tx.commit().map_err(control::storage)?;
    Ok(revoked)
}

pub(super) async fn stop(
    state: &Arc<AppState>,
    id: &str,
    db: rusqlite::Connection,
    before: &Value,
    command: &str,
    next: u64,
) -> Result<Value, String> {
    state.runs.handle(state, id).guard_notify.notify_waiters();
    let generation = before["execution_generation"]
        .as_u64()
        .ok_or("Missing execution generation")?;
    let handle = state.runs.handle(state, id);
    let mut tasks = handle.worker_tasks.lock().unwrap().clone();
    tasks.extend(handle.coordinator_tasks.lock().unwrap().iter().cloned());
    let cancelled = state.runs.abort_all_workers(id);
    let close_agents = async {
        for tid in cancelled {
            state.agents.close(&tid).await;
        }
    };
    let (teardown, ()) = tokio::join!(
        super::container_exec::teardown_run_container_recorded(state, id),
        close_agents
    );
    let tasks_stopped = super::wait_for_tasks(&tasks, super::main_runtime_cancel_timeout()).await;
    let (runner_all_exited, runner_some_exited, has_runner_records) =
        store::exit_evidence(state, id, generation)?;
    let host_exited = tasks_stopped && state.runs.active_worker_count(id) == 0;
    let (all_exited, some_exited) = if has_runner_records {
        (runner_all_exited, runner_some_exited)
    } else {
        (host_exited, host_exited)
    };
    let applied = tasks_stopped && all_exited && teardown.is_ok();
    let mut after = before.clone();
    let mut detail = teardown.err();
    if applied {
        super::worker_result::settle_claims(state, id, "canceled", "emergency_stop")?;
        super::control_freeze::settle_stopped(state, id, &mut after)?;
        super::terminal::finish(state, id, generation, None, false, "emergency_stop", None)?;
        let terminal = state
            .store
            .get("runs", id)
            .map_err(|e| e.to_string())?
            .ok_or("Stopped Run missing")?;
        for key in [
            "status",
            "running",
            "finished",
            "solved",
            "terminal_at",
            "terminal_generation",
            "terminal_sequence",
            "terminal_receipt_id",
            "outcome_reason",
            "outcome_detail",
            "awaiting_operator",
            "updated",
        ] {
            if let Some(value) = terminal.get(key) {
                after[key] = value.clone();
            }
        }
        after["control_mode"] = json!("terminated");
    } else {
        after["status"] = json!("stopping");
        after["awaiting_operator"] = json!("stop_confirmation");
        if detail.is_none() {
            detail = Some("Complete process exit evidence is unavailable".into());
        }
    }
    after["control_generation"] = json!(next);
    let status = if applied {
        "applied"
    } else if some_exited {
        "partial"
    } else {
        "unknown"
    };
    let response = json!({
        "ok": applied,
        "action": "emergency_stop",
        "command_id": command,
        "generation": next,
        "status": status,
        "process_exit_confirmed": all_exited && tasks_stopped,
        "detail": detail
    });
    super::control_journal::finish(state, id, &db, command, before, &after, &response)?;
    let receipt = store::stop_receipt(&db, command)?;
    super::emit_event(state, id, None, "guard.emergency_stop.receipt", receipt);
    super::emit_event(state, id, None, "control.command", response.clone());
    Ok(response)
}

pub(super) async fn recover_stop(
    state: &Arc<AppState>,
    id: &str,
    run: &Value,
) -> Result<bool, String> {
    if !protected(run) {
        return Ok(false);
    }
    use rusqlite::OptionalExtension;
    let db = super::open_control_db(state, id)?;
    let generation = run["execution_generation"]
        .as_u64()
        .ok_or("Missing execution generation")?;
    if !store::revoked(&db, generation).map_err(|e| e.to_string())? {
        return Ok(false);
    }
    let prior: Option<(String, u64, bool)> = db
        .query_row(
            "SELECT c.command_id,c.generation,
        EXISTS(SELECT 1 FROM control_results r WHERE r.command_id=c.command_id)
        FROM guard_state g JOIN control_commands c ON c.command_id=g.stop_id WHERE g.generation=?1
        AND c.status IN ('unknown','partial')",
            [generation],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()
        .map_err(|e| e.to_string())?;
    if let Some((id_stop, next, attempted)) = prior {
        let (runner_all_exited, _, has_runner_records) =
            store::exit_evidence(state, id, generation)?;
        let ready_to_finalize = if has_runner_records {
            runner_all_exited
        } else {
            state.runs.active_worker_count(id) == 0
        };
        if !attempted || ready_to_finalize {
            stop(state, id, db, run, &id_stop, next).await?;
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    #[test]
    fn emergency_admission_revokes_the_current_generation_atomically() {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch(
            "CREATE TABLE control_commands(command_id TEXT PRIMARY KEY,action TEXT,target TEXT,status TEXT,
            generation INTEGER,payload_json TEXT,response_json TEXT,created_at REAL)",
        )
        .unwrap();
        store::init(&db).unwrap();
        db.execute_batch(
            "INSERT INTO guard_state(generation,egress_state) VALUES(1,'held'),(2,'held');
            INSERT INTO guard_actions(generation,call_id,request_json,evaluation_json,decision,receipt_state,approval_id,approval_state)
            VALUES(1,'a','{}','{}','ask','not_sent','p1','pending'),
                  (1,'b','{}','{}','ask','not_sent','p2','allow_once'),
                  (1,'c','{}','{}','allow','sent',null,null),
                  (2,'d','{}','{}','ask','not_sent','p3','pending')",
        )
        .unwrap();
        let run = json!({"execution_generation": 1});
        let body = json!({"scope": "global", "approval_id": "p1"});
        let receipt = json!({"command_id": "stop-1", "generation": 2, "status": "unknown"});
        db.execute_batch("CREATE TRIGGER interrupt_fence BEFORE UPDATE ON guard_state BEGIN SELECT RAISE(ABORT,'interrupted'); END;").unwrap();
        assert!(admit_stop(&db, &run, &body, "stop-1", &receipt).is_err());
        assert_eq!(
            db.query_row("SELECT count(*) FROM control_commands", [], |r| r
                .get::<_, u64>(0))
                .unwrap(),
            0
        );
        assert_eq!(store::snapshot(&db, 1).unwrap()["egress_state"], "held");
        db.execute_batch("DROP TRIGGER interrupt_fence").unwrap();
        let (mut approvals, unknown) = admit_stop(&db, &run, &body, "stop-1", &receipt).unwrap();
        approvals.sort();
        assert_eq!(approvals, ["p1", "p2"]);
        assert_eq!(unknown, ["c"]);
        let revoked = store::snapshot(&db, 1).unwrap();
        assert_eq!(revoked["egress_state"], "revoked");
        assert_eq!(revoked["emergency_stop_receipt"]["status"], "UNKNOWN");
        assert_eq!(revoked["pending_approvals"], json!([]));
        assert_eq!(revoked["recent_receipts"][0]["status"], "outcome_unknown");
        assert_eq!(
            store::snapshot(&db, 2).unwrap()["pending_approvals"][0]["approval_id"],
            "p3"
        );
    }

    #[test]
    fn allow_once_cannot_be_consumed_twice_or_after_revocation() {
        let db = Connection::open_in_memory().unwrap();
        store::init(&db).unwrap();
        store::ensure_open(&db, 1).unwrap();
        let eval = cronus::GuardEvaluation {
            decision: "ask".into(),
            reason: "Check".into(),
            risk_level: "medium".into(),
            available: true,
            error: None,
            fail_closed_reason: None,
            decision_label: Some("ask".into()),
            decision_probs: None,
            proceed_prob: None,
            ask_prob: None,
            reject_prob: None,
            scope_p_true: None,
            latency_ms: 5.0,
            model_name: None,
            tier0_rule: None,
            action_fingerprint: "fp1".into(),
        };
        let (decision, aid, moved, inserted) =
            store::record_evaluation(&db, 1, "c1", &json!({"tool_name":"http_request"}), &eval)
                .unwrap();
        assert!(inserted && moved && decision == "ask");
        let aid = aid.unwrap();
        db.execute(
            "UPDATE guard_actions SET approval_state='allow_once' WHERE approval_id=?1",
            [&aid],
        )
        .unwrap();
        db.execute(
            "UPDATE guard_state SET egress_state='open' WHERE generation=1",
            [],
        )
        .unwrap();
        assert!(store::consume_for_dispatch(&db, 1, "c1", Some(&aid), true).unwrap());
        assert!(!store::consume_for_dispatch(&db, 1, "c1", Some(&aid), true).unwrap());
    }

    #[test]
    fn exit_evidence_distinguishes_host_local_and_runner_backed_runs() {
        let state = crate::AppState::new(crate::store::Store::open(":memory:").unwrap());
        assert_eq!(
            store::exit_evidence(&state, "run-host", 1).unwrap(),
            (false, false, false)
        );
        state
            .store
            .put(
                "runner_runs",
                "rr-1",
                &json!({"run_id": "run-ctr", "generation": 1, "state": "running"}),
            )
            .unwrap();
        assert_eq!(
            store::exit_evidence(&state, "run-ctr", 1).unwrap(),
            (false, false, true)
        );
        state
            .store
            .put(
                "runner_runs",
                "rr-1",
                &json!({"run_id": "run-ctr", "generation": 1, "state": "stopped"}),
            )
            .unwrap();
        assert_eq!(
            store::exit_evidence(&state, "run-ctr", 1).unwrap(),
            (true, true, true)
        );
    }
}
